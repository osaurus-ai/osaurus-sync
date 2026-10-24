use std::sync::Arc;

use osaurus_sync_core::env::TestEnv;
use osaurus_sync_core::error::Error;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::ScopeId;
use osaurus_sync_core::keys::UserSecret;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::{IngestOutcome, MemorySecretStore};
use osaurus_sync_core::types::{Attachment, Change, DeliveryStatus, NewMessage};
use osaurus_sync_core::{Replica, ReplicaConfig};

struct Dev {
    replica: Replica,
    _dir: Option<tempfile::TempDir>,
    secrets: Arc<MemorySecretStore>,
}

fn open(name: &str, env: Arc<TestEnv>) -> Dev {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let cfg = ReplicaConfig::new(dir.path(), name, Platform::MacOs);
    let replica = Replica::open(cfg, env, secrets.clone()).unwrap();
    Dev { replica, _dir: Some(dir), secrets }
}

fn text(s: &str) -> NewMessage {
    NewMessage::text(MessageRole::User, s)
}

/// Copies every event of `scope` from `a` to `b`, in reverse order to exercise holding.
fn copy_scope(a: &Replica, b: &mut Replica, scope: ScopeId) {
    let hashes = a.all_hashes(scope).unwrap();
    let mut events = a.events_by_hashes(&hashes).unwrap();
    events.reverse();
    b.join_scope(scope).unwrap();
    for e in events {
        if let IngestOutcome::Invalid(m) = b.ingest(&e).unwrap() {
            panic!("invalid: {m}");
        }
    }
}

fn contents(r: &Replica, scope: ScopeId) -> Vec<String> {
    r.messages(scope, None, 100).unwrap().messages.into_iter().map(|m| m.content).collect()
}

#[test]
fn send_read_search() {
    let env = Arc::new(TestEnv::new(1, 1_700_000_000_000));
    let mut d = open("Mac", env.clone());
    let r = &mut d.replica;
    let scope = r.create_scope(ScopeKind::Conversation, "Trip planning").unwrap();
    env.advance(10);
    r.send_message(scope, text("Book the ferry to Naxos")).unwrap();
    env.advance(10);
    r.send_message(scope, text("Pack sunscreen")).unwrap();
    assert_eq!(contents(r, scope), vec!["Book the ferry to Naxos", "Pack sunscreen"]);
    let convs = r.conversations().unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title, "Trip planning");
    assert_eq!(convs[0].last_preview.as_deref(), Some("Pack sunscreen"));
    let hits = r.search("ferr", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("[ferry]"));
    // Single device: nothing to wait for, so nothing is pending.
    assert!(r.messages(scope, None, 10).unwrap().messages.iter().all(|m| m.status == DeliveryStatus::Delivered));
    let changes = r.take_changes();
    assert!(changes.iter().any(|c| matches!(c, Change::MessageAdded(m) if m.content == "Pack sunscreen")));
}

#[test]
fn paging_with_cursor() {
    let env = Arc::new(TestEnv::new(2, 1_700_000_000_000));
    let mut d = open("Mac", env.clone());
    let r = &mut d.replica;
    let scope = r.create_scope(ScopeKind::Conversation, "").unwrap();
    for i in 0..25 {
        env.advance(1);
        r.send_message(scope, text(&format!("m{i}"))).unwrap();
    }
    let p1 = r.messages(scope, None, 10).unwrap();
    assert_eq!(p1.messages.first().unwrap().content, "m15");
    let p2 = r.messages(scope, p1.next_cursor, 10).unwrap();
    assert_eq!(p2.messages.last().unwrap().content, "m14");
    let p3 = r.messages(scope, p2.next_cursor, 10).unwrap();
    assert_eq!(p3.messages.len(), 5);
    assert!(p3.next_cursor.is_none());
    assert!(!p3.at_sync_boundary);
}

#[test]
fn edit_delete_and_reactions() {
    let env = Arc::new(TestEnv::new(3, 1_700_000_000_000));
    let mut d = open("Mac", env.clone());
    let r = &mut d.replica;
    let scope = r.create_scope(ScopeKind::Conversation, "").unwrap();
    let id = r.send_message(scope, text("helo")).unwrap();
    env.advance(5);
    r.edit_message(id, "hello").unwrap();
    let m = r.message_view(&id).unwrap().unwrap();
    assert_eq!(m.content, "hello");
    assert!(m.edited);
    r.add_reaction(id, "👍").unwrap();
    let m = r.message_view(&id).unwrap().unwrap();
    assert_eq!(m.reactions.len(), 1);
    assert!(m.reactions[0].mine);
    r.remove_reaction(id, "👍").unwrap();
    assert!(r.message_view(&id).unwrap().unwrap().reactions.is_empty());

    r.delete_message(id).unwrap();
    assert!(r.message_view(&id).unwrap().is_none());
    assert!(r.search("hello", 10).unwrap().is_empty());
}

#[test]
fn second_instance_is_refused() {
    let env = Arc::new(TestEnv::new(4, 0));
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let _a = Replica::open(ReplicaConfig::new(dir.path(), "Mac", Platform::MacOs), env.clone(), secrets.clone()).unwrap();
    let b = Replica::open(ReplicaConfig::new(dir.path(), "Mac", Platform::MacOs), env, secrets);
    assert!(matches!(b, Err(Error::AlreadyOpen)));
}

#[test]
fn reopen_keeps_state_and_rebuilds_views() {
    let env = Arc::new(TestEnv::new(5, 1_700_000_000_000));
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let cfg = || {
        let mut c = ReplicaConfig::new(dir.path(), "Mac", Platform::MacOs);
        c.snapshot_every = 2;
        c
    };
    let scope;
    {
        let mut r = Replica::open(cfg(), env.clone(), secrets.clone()).unwrap();
        scope = r.create_scope(ScopeKind::Conversation, "One").unwrap();
        r.rename_conversation(scope, "Two").unwrap();
        r.rename_conversation(scope, "Three").unwrap();
        r.set_pinned(scope, true).unwrap();
        r.send_message(scope, text("persist me")).unwrap();
        r.flush().unwrap();
    }
    {
        let r = Replica::open(cfg(), env.clone(), secrets.clone()).unwrap();
        let c = r.conversation(scope).unwrap().unwrap();
        assert_eq!(c.title, "Three");
        assert!(c.pinned);
        assert_eq!(contents(&r, scope), vec!["persist me"]);
    }
    // Force a view schema change: views are rebuilt from events.
    {
        let conn = rusqlite::Connection::open(dir.path().join("sync.db")).unwrap();
        conn.execute("UPDATE meta SET value = ? WHERE key = 'view_version'", [0i64.to_le_bytes().to_vec()]).unwrap();
    }
    let mut r = Replica::open(cfg(), env, secrets).unwrap();
    assert_eq!(contents(&r, scope), vec!["persist me"]);
    assert_eq!(r.conversation(scope).unwrap().unwrap().title, "Three");
    r.rename_conversation(scope, "Four").unwrap();
    assert_eq!(r.conversation(scope).unwrap().unwrap().title, "Four");
}

#[test]
fn copied_database_is_discarded() {
    let env = Arc::new(TestEnv::new(6, 1_700_000_000_000));
    let dir = tempfile::tempdir().unwrap();
    {
        let mut r = Replica::open(ReplicaConfig::new(dir.path(), "Mac", Platform::MacOs), env.clone(), Arc::new(MemorySecretStore::default())).unwrap();
        r.create_scope(ScopeKind::Conversation, "old").unwrap();
    }
    // Same files, different key store: a disk image restored on another machine.
    let r = Replica::open(ReplicaConfig::new(dir.path(), "Mac", Platform::MacOs), env, Arc::new(MemorySecretStore::default())).unwrap();
    assert!(r.conversations().unwrap().is_empty());
}

#[test]
fn two_devices_converge_and_outbox_clears() {
    let env = Arc::new(TestEnv::new(7, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let mut b = open("Windows", env.clone());
    // Pair: B adopts A's user; A admits B.
    let user = UserSecret::from_entropy(a.secrets.load_entropy());
    b.replica.adopt_user(user).unwrap();
    let scope = a.replica.create_scope(ScopeKind::Conversation, "Shared").unwrap();
    let admitted = a.replica.admit_device(b.replica.cert()).unwrap();
    assert_eq!(admitted, vec![scope]);
    let id = a.replica.send_message(scope, text("from mac")).unwrap();
    assert_eq!(a.replica.message_view(&id).unwrap().unwrap().status, DeliveryStatus::Pending);

    copy_scope(&a.replica, &mut b.replica, scope);
    assert_eq!(contents(&b.replica, scope), vec!["from mac"]);
    assert_eq!(b.replica.conversation(scope).unwrap().unwrap().title, "Shared");

    env.advance(10);
    b.replica.send_message(scope, text("from windows")).unwrap();
    b.replica.rename_conversation(scope, "Renamed on Windows").unwrap();
    copy_scope(&b.replica, &mut a.replica, scope);
    assert_eq!(contents(&a.replica, scope), contents(&b.replica, scope));
    assert_eq!(a.replica.conversation(scope).unwrap().unwrap().title, "Renamed on Windows");

    let hash = a.replica.all_hashes(scope).unwrap();
    a.replica.ack(&hash).unwrap();
    assert_eq!(a.replica.message_view(&id).unwrap().unwrap().status, DeliveryStatus::Delivered);
    assert_eq!(a.replica.pending_outbox(scope).unwrap(), 0);
}

#[test]
fn revoked_device_events_rejected_and_keys_rotate() {
    let env = Arc::new(TestEnv::new(8, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let mut b = open("Old laptop", env.clone());
    b.replica.adopt_user(UserSecret::from_entropy(a.secrets.load_entropy())).unwrap();
    let scope = a.replica.create_scope(ScopeKind::Conversation, "").unwrap();
    a.replica.admit_device(b.replica.cert()).unwrap();
    copy_scope(&a.replica, &mut b.replica, scope);
    env.advance(5);
    a.replica.revoke_device(b.replica.device_key()).unwrap();
    // B, offline and unaware, keeps posting.
    env.advance(5);
    b.replica.send_message(scope, text("written after revocation")).unwrap();
    copy_scope(&b.replica, &mut a.replica, scope);
    assert!(contents(&a.replica, scope).is_empty());
    // New messages from A use an epoch B cannot read.
    env.advance(5);
    a.replica.send_message(scope, text("secret")).unwrap();
    copy_scope(&a.replica, &mut b.replica, scope);
    assert!(!contents(&b.replica, scope).contains(&"secret".to_string()));
    assert!(b.replica.is_self_revoked());
}

#[test]
fn attachments_and_large_tool_output_become_blobs() {
    let env = Arc::new(TestEnv::new(9, 1_700_000_000_000));
    let mut d = open("Mac", env.clone());
    let r = &mut d.replica;
    let scope = r.create_scope(ScopeKind::Conversation, "").unwrap();
    let file = d._dir.as_ref().unwrap().path().join("photo.jpg");
    std::fs::write(&file, vec![42u8; 200_000]).unwrap();
    let mut m = text("look");
    m.attachments.push(Attachment { path: file, media_type: "image/jpeg".into(), name: "photo.jpg".into() });
    let id = r.send_message(scope, m).unwrap();
    let v = r.message_view(&id).unwrap().unwrap();
    assert_eq!(v.blobs.len(), 1);
    assert_eq!(v.blobs[0].size, 200_000);
    let path = r.blob_path(scope, &v.blobs[0].hash).unwrap().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), vec![42u8; 200_000]);

    let big = "x".repeat(40_000);
    let id = r.send_message(scope, NewMessage::text(MessageRole::ToolResult, big.clone())).unwrap();
    let v = r.message_view(&id).unwrap().unwrap();
    assert!(v.content.len() < 1_000);
    let path = r.blob_path(scope, &v.blobs[0].hash).unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), big);
}

trait LoadEntropy {
    fn load_entropy(&self) -> [u8; 32];
}

impl LoadEntropy for MemorySecretStore {
    fn load_entropy(&self) -> [u8; 32] {
        use osaurus_sync_core::SecretStore;
        self.load("user-secret").unwrap().try_into().unwrap()
    }
}
