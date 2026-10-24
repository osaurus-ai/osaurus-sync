//! Length-prefixed frames on QUIC streams.

use anyhow::{Result, bail};
use iroh::endpoint::{RecvStream, SendStream};
use osaurus_sync_core::sync::proto::MAX_FRAME;

pub async fn write(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_FRAME {
        bail!("frame too large");
    }
    send.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    send.write_all(bytes).await?;
    Ok(())
}

/// Reads one frame. `None` when the stream finished cleanly between frames.
pub async fn read(recv: &mut RecvStream) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match recv.read(&mut len[got..]).await? {
            Some(n) => got += n,
            None if got == 0 => return Ok(None),
            None => bail!("stream ended inside a frame header"),
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("frame too large");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(Some(buf))
}
