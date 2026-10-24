//! Two real endpoints on loopback: pairing, catch-up, live push, streaming, blob transfer,
//! suspend and resume.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use osaurus_sync_core::env::SystemEnv;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::ScopeId;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::MemorySecretStore;
use osaurus_sync_core::sync::Notice;
use osaurus_sync_core::types::{Attachment, Change, NewMessage, StreamState};
use osaurus_sync_core::{Replica, ReplicaConfig};
use osaurus_sync_net::{Listener, NetConfig, SyncNode};

#[derive(Default)]
struct Recorder {
    changes: Mutex<Vec<Change>>,
    notices: Mutex<Vec<Notice>>,
}

impl Listener for Recorder {
    fn changes(&self, c: Vec<Change>) {
        self.changes.lock().unwrap().extend(c);
    }
    fn notice(&self, n: Notice) {
        self.notices.lock().unwrap().push(n);
    }
}

async fn node(dir: &std::path::Path, name: &str, platform: Platform) -> (SyncNode, Arc<Recorder>) {
    let cfg = ReplicaConfig::new(dir.join(name), name, platform);
    let r = Replica::open(cfg, Arc::new(SystemEnv), Arc::new(MemorySecretStore::default())).unwrap();
    let rec = Arc::new(Recorder::default());
    let n = SyncNode::start(r, NetConfig::local(), rec.clone()).await.unwrap();
    (n, rec)
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn contents(n: &SyncNode, scope: ScopeId) -> Vec<String> {
    n.read(|r| r.messages(scope, None, 1000).map(|p| p.messages.into_iter().map(|m| m.content).collect()).unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_sync_stream_blob_suspend() {
    let dir = tempfile::tempdir().unwrap();
    let (mac, mac_rec) = node(dir.path(), "mac", Platform::MacOs).await;
    let (pc, pc_rec) = node(dir.path(), "pc", Platform::Windows).await;

    let file = dir.path().join("notes.txt");
    std::fs::write(&file, b"attachment body ".repeat(50_000)).unwrap();
    let scope = mac.write(|r| r.create_scope(ScopeKind::Conversation, "Trip")).unwrap();
    mac.write(|r| {
        let mut m = NewMessage::text(MessageRole::User, "see attached");
        m.attachments.push(Attachment { path: file.clone(), media_type: "text/plain".into(), name: "notes.txt".into() });
        r.send_message(scope, m)
    })
    .unwrap();

    mac.add_peer_addr(pc.addr().unwrap());
    pc.add_peer_addr(mac.addr().unwrap());
    let invite = mac.start_pairing(300_000);
    pc.pair(&invite.encode()).await.unwrap();

    until("pairing", || pc_rec.notices.lock().unwrap().iter().any(|n| matches!(n, Notice::Paired { .. }))).await;
    until("catch-up", || contents(&pc, scope) == vec!["see attached"]).await;
    assert_eq!(pc.read(|r| r.user_key()), mac.read(|r| r.user_key()));

    // Live push in the other direction.
    pc.write(|r| r.send_message(scope, NewMessage::text(MessageRole::User, "from pc"))).unwrap();
    until("live push", || contents(&mac, scope).len() == 2).await;

    // Blob transfer, verified against its hash.
    let blob = pc.read(|r| r.messages(scope, None, 10).unwrap().messages[0].blobs[0].clone());
    assert!(!blob.available);
    let path = pc.fetch_blob(scope, blob.hash).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), std::fs::read(&file).unwrap());

    // Ephemeral streaming tokens.
    let msg_id = osaurus_sync_core::ids::EventId([9; 16]);
    mac.stream_update(scope, msg_id, None, 1, "Hel", StreamState::Streaming);
    mac.stream_update(scope, msg_id, None, 2, "Hello", StreamState::Finished);
    until("stream", || pc_rec.changes.lock().unwrap().iter().any(|c| matches!(c, Change::Stream(u) if u.text == "Hello" && u.state == StreamState::Finished)))
        .await;

    // Suspend: writes keep working locally and sync after resume.
    mac.suspend().await.unwrap();
    assert!(!mac.is_running());
    until("disconnect", || !pc.is_connected(&mac.device_key())).await;
    mac.write(|r| r.send_message(scope, NewMessage::text(MessageRole::User, "while suspended"))).unwrap();
    mac.resume().await.unwrap();
    pc.add_peer_addr(mac.addr().unwrap());
    mac.add_peer_addr(pc.addr().unwrap());
    mac.sync_now();
    until("catch-up after resume", || contents(&pc, scope).len() == 3).await;

    let d = mac.diagnostics();
    assert!(d.running);
    assert!(d.peers.iter().any(|p| p.device == pc.device_key() && p.connected));
    assert!(mac_rec.changes.lock().unwrap().iter().any(|c| matches!(c, Change::MessageAdded(_))));

    mac.shutdown().await.unwrap();
    pc.shutdown().await.unwrap();
}
