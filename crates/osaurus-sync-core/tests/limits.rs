//! Bounded local storage: held-event limits and expiry, and the periodic cleanup pass.

use std::sync::Arc;

use osaurus_sync_core::env::TestEnv;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::{Role, ScopeId};
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::{IngestOutcome, MemorySecretStore};
use osaurus_sync_core::types::{Attachment, NewMessage};
use osaurus_sync_core::{Replica, ReplicaConfig};

const DAY: u64 = 86_400_000;

fn open_with(name: &str, env: &Arc<TestEnv>, f: impl FnOnce(&mut ReplicaConfig)) -> (Replica, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ReplicaConfig::new(dir.path(), name, Platform::MacOs);
    f(&mut cfg);
    let r = Replica::open(cfg, env.clone(), Arc::new(MemorySecretStore::default())).unwrap();
    (r, dir)
}

fn split(r: &Replica, scope: ScopeId) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let ids = |control| r.recon_keys(scope, control, 0, u64::MAX).unwrap().into_iter().map(|k| k.1).collect::<Vec<_>>();
    (r.events_by_ids(scope, &ids(true)).unwrap(), r.events_by_ids(scope, &ids(false)).unwrap())
}

#[test]
fn held_events_are_capped_released_and_expired() {
    let env = Arc::new(TestEnv::new(21, 1_700_000_000_000));
    let (mut a, _ad) = open_with("A", &env, |_| {});
    let (mut b, _bd) = open_with("B", &env, |c| c.held_max_events = 3);
    let scope = a.create_scope(ScopeKind::Conversation, "t").unwrap();
    for i in 0..5 {
        a.send_message(scope, NewMessage::text(MessageRole::User, format!("m{i}"))).unwrap();
    }
    let (control, content) = split(&a, scope);
    b.join_scope(scope).unwrap();
    let outcomes: Vec<_> = content.iter().map(|e| b.ingest(e).unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|o| matches!(o, IngestOutcome::Held { .. })).count(), 3);
    assert_eq!(outcomes.iter().filter(|o| matches!(o, IngestOutcome::Invalid(_))).count(), 2);
    assert!(!b.held_missing(scope).unwrap().is_empty());

    // The control layer releases exactly the held events; the dropped ones arrive again later.
    for e in &control {
        b.ingest(e).unwrap();
    }
    assert!(b.held_missing(scope).unwrap().is_empty());
    assert_eq!(b.all_hashes(scope).unwrap().len(), control.len() + 3);
    for e in &content {
        b.ingest(e).unwrap();
    }
    assert_eq!(b.all_hashes(scope).unwrap().len(), control.len() + content.len());

    // Events whose parents never come are dropped after a week.
    let (mut c, _cd) = open_with("C", &env, |_| {});
    c.join_scope(scope).unwrap();
    assert!(matches!(c.ingest(&content[0]).unwrap(), IngestOutcome::Held { .. }));
    env.advance(8 * DAY);
    assert_eq!(c.maintain().unwrap().held_expired, 1);
    assert!(c.held_missing(scope).unwrap().is_empty());
}

#[test]
fn maintenance_prunes_invites_scopes_and_attachments() {
    let env = Arc::new(TestEnv::new(22, 1_700_000_000_000));
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _ad) = open_with("A", &env, |c| c.plain_cache_bytes = 1_500);
    let scope = a.create_scope(ScopeKind::Workspace, "t").unwrap();

    a.create_invite(scope, Role::Member, 1_000).unwrap();
    assert!(a.has_open_invites().unwrap());

    let stranger = ScopeId([9; 16]);
    a.join_scope_tentatively(stranger).unwrap();

    // Two 1 KB attachments against a 1.5 KB cache: opening the second evicts the first.
    let mut paths = Vec::new();
    for i in 0..2 {
        let f = dir.path().join(format!("f{i}.txt"));
        std::fs::write(&f, vec![b'a' + i as u8; 1_000]).unwrap();
        let mut m = NewMessage::text(MessageRole::User, "file");
        m.attachments.push(Attachment { path: f, media_type: "text/plain".into(), name: format!("f{i}.txt") });
        a.send_message(scope, m).unwrap();
    }
    for m in a.messages(scope, None, 10).unwrap().messages {
        paths.push(a.blob_path(scope, &m.blobs[0].hash).unwrap().unwrap());
    }
    assert!(!paths[0].exists());
    assert!(paths[1].exists());

    env.advance(2 * DAY);
    let m = a.maintain().unwrap();
    assert_eq!(m.invites_pruned, 1);
    assert_eq!(m.scopes_purged, 1);
    assert!(!a.has_open_invites().unwrap());
    assert_eq!(a.scope_ids().unwrap(), vec![scope]);
    // Nothing is left to do on a second pass.
    assert_eq!(a.maintain().unwrap(), Default::default());
}
