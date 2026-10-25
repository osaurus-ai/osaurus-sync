//! Attachment transfer. Blobs are already encrypted at rest; peers serve ciphertext only to
//! devices that can read a scope referencing the blob. Downloads resume from the partial file.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use osaurus_sync_core::blobs::BlobStore;
use osaurus_sync_core::ids::{DeviceKey, ScopeId};
use osaurus_sync_core::sync::proto::{BlobRequest, BlobResponse};
use tracing::debug;

use crate::{ALPN_BLOBS, Inner, device_of, frame, lock};

const CHUNK: usize = 256 * 1024;

impl Inner {
    fn blob_store(&self) -> BlobStore {
        lock(&self.state).replica.blob_store().clone()
    }

    pub(crate) async fn fetch_blob(self: Arc<Self>, scope: ScopeId, hash: [u8; 32]) -> Result<PathBuf> {
        self.clone().ensure_blob(scope, hash).await?;
        lock(&self.state).replica.blob_path(scope, &hash)?.ok_or_else(|| anyhow!("blob vanished"))
    }

    pub(crate) async fn fetch_blob_bytes(self: Arc<Self>, scope: ScopeId, hash: [u8; 32]) -> Result<Vec<u8>> {
        self.clone().ensure_blob(scope, hash).await?;
        lock(&self.state).replica.blob_bytes(scope, &hash)?.ok_or_else(|| anyhow!("blob vanished"))
    }

    /// Downloads a blob from a device that can serve it, unless it's already here.
    async fn ensure_blob(self: Arc<Self>, scope: ScopeId, hash: [u8; 32]) -> Result<()> {
        {
            let st = lock(&self.state);
            if st.replica.blob_ref(scope, &hash)?.is_none() {
                return Err(osaurus_sync_core::Error::NotFound("blob".into()).into());
            }
            if st.replica.blob_store().has(&hash) {
                return Ok(());
            }
        }
        let connected: Vec<DeviceKey> = lock(&self.peers).keys().copied().collect();
        let mut candidates: Vec<DeviceKey> = {
            let mut st = lock(&self.state);
            let mut c: Vec<DeviceKey> = connected.into_iter().filter(|d| st.replica.is_authorized(scope, d).unwrap_or(false)).collect();
            for (d, _) in st.replica.peer_contacts(scope).unwrap_or_default() {
                if !c.contains(&d) && d != self.me {
                    c.push(d);
                }
            }
            c
        };
        candidates.truncate(8);
        let mut last_err = anyhow!("no device that can serve this attachment is reachable");
        for peer in candidates {
            match self.download_from(peer, scope, hash).await {
                Ok(()) => {
                    let mut st = lock(&self.state);
                    st.replica.mark_blob_available(&hash)?;
                    let changes = st.replica.take_changes();
                    drop(st);
                    if !changes.is_empty() {
                        self.listener.changes(changes);
                    }
                    return Ok(());
                }
                Err(e) => {
                    debug!(?peer, "blob fetch failed: {e:#}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn download_from(&self, peer: DeviceKey, scope: ScopeId, hash: [u8; 32]) -> Result<()> {
        let store = self.blob_store();
        let conn = self.connect(&peer, ALPN_BLOBS).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let mut offset = store.partial_len(&hash);
        let req = BlobRequest { scope, hash, offset };
        frame::write(&mut send, &postcard::to_allocvec(&req)?).await?;
        send.finish()?;
        let resp = frame::read(&mut recv).await?.ok_or_else(|| anyhow!("no response"))?;
        let size = match postcard::from_bytes::<BlobResponse>(&resp)? {
            BlobResponse::Ok { size } => size,
            BlobResponse::NotFound => bail!("peer does not have the blob"),
            BlobResponse::Denied => bail!("peer refused"),
        };
        if offset > size {
            offset = 0;
        }
        let mut buf = vec![0u8; CHUNK];
        while offset < size {
            let n = match recv.read(&mut buf).await? {
                Some(n) => n,
                None => break,
            };
            let s = store.clone();
            let data = buf[..n].to_vec();
            let at = offset;
            tokio::task::spawn_blocking(move || s.append_partial(&hash, at, &data)).await??;
            offset += n as u64;
        }
        conn.close(0u32.into(), b"done");
        if offset != size {
            bail!("transfer ended at {offset} of {size} bytes");
        }
        let s = store.clone();
        tokio::task::spawn_blocking(move || s.finalize(&hash)).await??;
        Ok(())
    }

    async fn serve_blob(&self, peer: DeviceKey, conn: &Connection) -> Result<()> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let req = frame::read(&mut recv).await?.ok_or_else(|| anyhow!("no request"))?;
        let req: BlobRequest = postcard::from_bytes(&req)?;
        let (allowed, store) = {
            let mut st = lock(&self.state);
            let r = &mut st.replica;
            let ok = r.blob_scopes(&req.hash)?.contains(&req.scope) && r.is_authorized(req.scope, &peer)?;
            (ok, r.blob_store().clone())
        };
        let resp = match (allowed, store.size(&req.hash)) {
            (false, _) => BlobResponse::Denied,
            (true, None) => BlobResponse::NotFound,
            (true, Some(size)) => BlobResponse::Ok { size },
        };
        frame::write(&mut send, &postcard::to_allocvec(&resp)?).await?;
        if let BlobResponse::Ok { size } = resp {
            let mut offset = req.offset.min(size);
            while offset < size {
                let s = store.clone();
                let chunk = tokio::task::spawn_blocking(move || s.read_range(&req.hash, offset, CHUNK)).await??;
                if chunk.is_empty() {
                    break;
                }
                send.write_all(&chunk).await?;
                offset += chunk.len() as u64;
            }
        }
        send.finish()?;
        let _ = send.stopped().await;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BlobProtocol(pub Arc<Inner>);

impl ProtocolHandler for BlobProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = device_of(conn.remote_id());
        if let Err(e) = self.0.serve_blob(peer, &conn).await {
            debug!(?peer, "blob serve ended: {e:#}");
        }
        Ok(())
    }
}
