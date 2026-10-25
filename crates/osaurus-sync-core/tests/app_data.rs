//! App data, tags and filters, device-local scopes, encryption at rest and sign-out.

use std::sync::Arc;

use osaurus_sync_core::env::TestEnv;
use osaurus_sync_core::error::Error;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::{Role, ScopeId};
use osaurus_sync_core::keys::UserSecret;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::{IngestOutcome, MemorySecretStore};
use osaurus_sync_core::store;
use osaurus_sync_core::types::{AppDataWrite, Attachment, Change, ConversationFilter, ImportOptions, ImportedMessage, NewMessage, Replication};
use osaurus_sync_core::{Replica, ReplicaConfig, SecretStore};

const TURN: &str = "ai.osaurus.turn/1";

struct Dev {
    replica: Replica,
    dir: tempfile::TempDir,
    secrets: Arc<MemorySecretStore>,
}

fn open(name: &str, env: Arc<TestEnv>) -> Dev {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let replica = Replica::open(ReplicaConfig::new(dir.path(), name, Platform::MacOs), env, secrets.clone()).unwrap();
    Dev { replica, dir, secrets }
}

fn text(s: &str) -> NewMessage {
    NewMessage::text(MessageRole::User, s)
}

fn data(namespace: &str, bytes: &[u8]) -> AppDataWrite {
    AppDataWrite { namespace: namespace.into(), bytes: bytes.to_vec(), attachments: vec![] }
}

fn user_of(d: &Dev) -> UserSecret {
    UserSecret::from_entropy(d.secrets.get("user-secret").unwrap().try_into().unwrap())
}

/// Pairs `b` into `a`'s identity and admits it to `a`'s synced scopes.
fn pair(a: &mut Dev, b: &mut Dev) -> Vec<ScopeId> {
    b.replica.adopt_user(user_of(a)).unwrap();
    a.replica.admit_device(b.replica.cert()).unwrap()
}

fn copy_scope(a: &Replica, b: &mut Replica, scope: ScopeId) {
    let hashes = a.all_hashes(scope).unwrap();
    let events = a.events_by_hashes(&hashes).unwrap();
    b.join_scope(scope).unwrap();
    for e in events {
        if let IngestOutcome::Invalid(m) = b.ingest(&e).unwrap() {
            panic!("invalid: {m}");
        }
    }
}

fn app_data(r: &Replica, id: &osaurus_sync_core::ids::EventId, namespace: &str) -> Option<Vec<u8>> {
    r.message_view(id).unwrap().unwrap().app_data.into_iter().find(|a| a.namespace == namespace).map(|a| a.bytes)
}

#[test]
fn message_app_data_travels_with_the_message_and_the_newest_write_wins() {
    let env = Arc::new(TestEnv::new(1, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let mut b = open("PC", env.clone());
    let scope = a.replica.create_scope(ScopeKind::Conversation, "Chat").unwrap();
    pair(&mut a, &mut b);

    let mut m = NewMessage::text(MessageRole::Assistant, "Here is the plan");
    m.app_data.push(data(TURN, br#"{"thinking":"first"}"#));
    let id = a.replica.send_message(scope, m).unwrap();
    assert_eq!(app_data(&a.replica, &id, TURN).unwrap(), br#"{"thinking":"first"}"#);

    env.advance(5);
    a.replica.set_message_app_data(id, &[data(TURN, br#"{"thinking":"second"}"#), data("other.app/1", b"x")]).unwrap();
    assert_eq!(app_data(&a.replica, &id, TURN).unwrap(), br#"{"thinking":"second"}"#);
    assert!(a.replica.take_changes().iter().any(|c| matches!(c, Change::MessageChanged(v) if v.id == id)));

    copy_scope(&a.replica, &mut b.replica, scope);
    let v = b.replica.message_view(&id).unwrap().unwrap();
    assert_eq!(v.content, "Here is the plan");
    assert_eq!(app_data(&b.replica, &id, TURN).unwrap(), br#"{"thinking":"second"}"#);
    assert_eq!(app_data(&b.replica, &id, "other.app/1").unwrap(), b"x");

    // Empty bytes clear the namespace.
    env.advance(5);
    a.replica.set_message_app_data(id, &[data("other.app/1", b"")]).unwrap();
    assert!(app_data(&a.replica, &id, "other.app/1").is_none());

    // The same result after rebuilding views from events.
    a.replica.rebuild_all_views().unwrap();
    assert_eq!(app_data(&a.replica, &id, TURN).unwrap(), br#"{"thinking":"second"}"#);
    assert!(app_data(&a.replica, &id, "other.app/1").is_none());
}

#[test]
fn app_data_is_bounded_and_only_the_author_writes_it() {
    let env = Arc::new(TestEnv::new(2, 1_700_000_000_000));
    let mut alice = open("Alice", env.clone());
    let mut bob = open("Bob", env.clone());
    let scope = alice.replica.create_scope(ScopeKind::Workspace, "Team").unwrap();
    let invite = alice.replica.create_invite(scope, Role::Member, 60_000).unwrap();
    alice.replica.admit_invitee(scope, invite.token, bob.replica.cert()).unwrap();
    let id = alice.replica.send_message(scope, text("hi")).unwrap();

    let too_big = vec![0u8; 64 * 1024 + 1];
    assert!(matches!(alice.replica.set_message_app_data(id, &[data(TURN, &too_big)]), Err(Error::InvalidInput(_))));
    assert!(matches!(alice.replica.set_message_app_data(id, &[data("", b"x")]), Err(Error::InvalidInput(_))));
    let mut m = text("too much");
    m.app_data.push(data(TURN, &too_big));
    assert!(alice.replica.send_message(scope, m).is_err());
    assert_eq!(alice.replica.messages(scope, None, 10).unwrap().messages.len(), 1, "a refused write commits nothing");

    copy_scope(&alice.replica, &mut bob.replica, scope);
    assert_eq!(bob.replica.message_view(&id).unwrap().unwrap().content, "hi");
    assert!(matches!(bob.replica.set_message_app_data(id, &[data(TURN, b"mine")]), Err(Error::PermissionDenied(_))));
}

#[test]
fn received_app_data_over_the_local_cap_is_accepted() {
    let env = Arc::new(TestEnv::new(10, 1_700_000_000_000));
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ReplicaConfig::new(dir.path(), "Server", Platform::Server);
    cfg.max_app_data_bytes = 256 * 1024;
    let secrets = Arc::new(MemorySecretStore::default());
    let mut big = Dev { replica: Replica::open(cfg, env.clone(), secrets.clone()).unwrap(), dir, secrets };
    let mut small = open("Mac", env.clone());
    let scope = big.replica.create_scope(ScopeKind::Conversation, "").unwrap();
    pair(&mut big, &mut small);
    let mut m = text("q");
    m.app_data.push(data(TURN, &vec![1u8; 200_000]));
    let id = big.replica.send_message(scope, m).unwrap();
    copy_scope(&big.replica, &mut small.replica, scope);
    assert_eq!(app_data(&small.replica, &id, TURN).unwrap().len(), 200_000);
}

#[test]
fn app_data_attachments_are_blobs_and_go_with_the_message() {
    let env = Arc::new(TestEnv::new(3, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let scope = a.replica.create_scope(ScopeKind::Conversation, "").unwrap();
    let file = a.dir.path().join("trace.json");
    std::fs::write(&file, vec![7u8; 100_000]).unwrap();
    let mut m = NewMessage::text(MessageRole::Assistant, "done");
    m.app_data.push(AppDataWrite {
        namespace: TURN.into(),
        bytes: b"{}".to_vec(),
        attachments: vec![Attachment { path: file, media_type: "application/json".into(), name: "trace.json".into() }],
    });
    let id = a.replica.send_message(scope, m).unwrap();
    let v = a.replica.message_view(&id).unwrap().unwrap();
    let blob = &v.app_data[0].blobs[0];
    assert_eq!(blob.size, 100_000);
    assert_eq!(a.replica.blob_bytes(scope, &blob.hash).unwrap().unwrap(), vec![7u8; 100_000]);
    let path = a.replica.blob_path(scope, &blob.hash).unwrap().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 100_000]);

    // Decrypted copies don't outlive the session.
    a.replica.close().unwrap();
    assert!(!path.exists());
    let mut a = Dev { replica: Replica::open(ReplicaConfig::new(a.dir.path(), "Mac", Platform::MacOs), env.clone(), a.secrets.clone()).unwrap(), ..a };

    // Deleting the message removes its app data and the blobs it referenced.
    let hash = blob.hash;
    a.replica.delete_message(id).unwrap();
    assert!(a.replica.message_view(&id).unwrap().is_none());
    assert!(matches!(a.replica.blob_path(scope, &hash), Err(Error::NotFound(_))));
}

#[test]
fn compaction_drops_superseded_app_data() {
    let env = Arc::new(TestEnv::new(4, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let scope = a.replica.create_scope_with(ScopeKind::Conversation, "", Replication::DeviceLocal).unwrap();
    let id = a.replica.send_message(scope, text("q")).unwrap();
    for i in 0..5 {
        env.advance(5);
        a.replica.set_message_app_data(id, &[data(TURN, format!("v{i}").as_bytes())]).unwrap();
    }
    let c = a.replica.compact(scope).unwrap();
    assert_eq!(c.app_data_dropped, 4);
    assert_eq!(app_data(&a.replica, &id, TURN).unwrap(), b"v4");
    a.replica.rebuild_all_views().unwrap();
    assert_eq!(app_data(&a.replica, &id, TURN).unwrap(), b"v4");
}

#[test]
fn tags_conversation_app_data_and_filters() {
    let env = Arc::new(TestEnv::new(5, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let mut b = open("PC", env.clone());
    pair(&mut a, &mut b);
    let r = &mut a.replica;
    let work = r.create_scope(ScopeKind::Conversation, "Work").unwrap();
    let home = r.create_scope(ScopeKind::Conversation, "Home").unwrap();
    r.create_scope_with(ScopeKind::Conversation, "Scratch", Replication::DeviceLocal).unwrap();
    r.set_conversation_tags(work, &[("agent".into(), "coder".into()), ("source".into(), "chat".into())]).unwrap();
    r.set_conversation_tags(home, &[("agent".into(), "chef".into())]).unwrap();
    r.set_conversation_app_data(work, "ai.osaurus.session/1", Some(br#"{"model":"m"}"#)).unwrap();
    r.send_message(work, text("standup notes")).unwrap();
    r.send_message(home, text("dinner notes")).unwrap();
    r.set_archived(home, true).unwrap();

    let titles = |v: Vec<osaurus_sync_core::types::ConversationSummary>| v.into_iter().map(|c| c.title).collect::<Vec<_>>();
    let by_agent = ConversationFilter { tags: vec![("agent".into(), "coder".into())], ..Default::default() };
    assert_eq!(titles(r.conversations_filtered(&by_agent).unwrap()), ["Work"]);
    let tagged = ConversationFilter { has_tags: vec!["agent".into()], ..Default::default() };
    assert_eq!(titles(r.conversations_filtered(&tagged).unwrap()).len(), 2);
    let unarchived = ConversationFilter { has_tags: vec!["agent".into()], archived: Some(false), ..Default::default() };
    assert_eq!(titles(r.conversations_filtered(&unarchived).unwrap()), ["Work"]);
    let locals = ConversationFilter { replication: Some(Replication::DeviceLocal), ..Default::default() };
    assert_eq!(titles(r.conversations_filtered(&locals).unwrap()), ["Scratch"]);
    let page = ConversationFilter { limit: 1, offset: 1, ..Default::default() };
    assert_eq!(r.conversations_filtered(&page).unwrap().len(), 1);
    assert_eq!(r.search_filtered("notes", &by_agent, 10).unwrap().len(), 1);
    assert_eq!(r.search("notes", 10).unwrap().len(), 2);

    let c = r.conversation(work).unwrap().unwrap();
    assert_eq!(c.message_count, 1);
    assert_eq!(c.replication, Replication::Synced);
    assert_eq!(c.app_data, vec![("ai.osaurus.session/1".to_string(), br#"{"model":"m"}"#.to_vec())]);
    assert!(r.set_conversation_tags(work, &[("k".into(), "1".into()), ("k".into(), "2".into())]).is_err());

    // Tags and conversation app data sync; replacing tags removes the missing ones.
    r.set_conversation_tags(work, &[("agent".into(), "coder".into())]).unwrap();
    r.set_conversation_app_data(work, "ai.osaurus.session/1", None).unwrap();
    copy_scope(&a.replica, &mut b.replica, work);
    let c = b.replica.conversation(work).unwrap().unwrap();
    assert_eq!(c.tags, vec![("agent".to_string(), "coder".to_string())]);
    assert!(c.app_data.is_empty());
}

#[test]
fn import_keeps_metadata_tool_fields_and_app_data() {
    let env = Arc::new(TestEnv::new(6, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let opts = ImportOptions {
        title: "Old".into(),
        created_ms: 1_600_000_000_000,
        replication: Replication::DeviceLocal,
        pinned: true,
        archived: false,
        tags: vec![("source".into(), "http".into())],
        app_data: vec![("ai.osaurus.session/1".into(), b"{}".to_vec())],
    };
    let mut call = ImportedMessage::text(MessageRole::ToolCall, "{\"q\":1}", 1_600_000_000_100);
    call.tool_call_id = Some("call_1".into());
    call.tool_name = Some("search".into());
    let mut answer = ImportedMessage::text(MessageRole::Assistant, "found it", 1_600_000_000_200);
    answer.app_data.push(data(TURN, b"{\"t\":1}"));
    let msgs = [ImportedMessage::text(MessageRole::User, "look it up", 1_600_000_000_000), call, answer];
    let scope = a.replica.import_conversation_with(&opts, &msgs).unwrap();

    let c = a.replica.conversation(scope).unwrap().unwrap();
    assert_eq!((c.title.as_str(), c.pinned, c.created_ms, c.message_count), ("Old", true, 1_600_000_000_000, 3));
    assert_eq!(c.replication, Replication::DeviceLocal);
    assert_eq!(c.tags, vec![("source".to_string(), "http".to_string())]);
    let page = a.replica.messages(scope, None, 10).unwrap().messages;
    assert_eq!(page[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(page[1].tool_name.as_deref(), Some("search"));
    assert_eq!(app_data(&a.replica, &page[2].id, TURN).unwrap(), b"{\"t\":1}");
    assert_eq!(osaurus_sync_core::hlc::physical_ms(page[2].hlc), 1_600_000_000_200);
}

#[test]
fn edits_after_a_future_dated_import_still_win() {
    let env = Arc::new(TestEnv::new(9, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let opts = ImportOptions { title: "Skewed".into(), created_ms: 1_800_000_000_000, ..Default::default() };
    let mut m = ImportedMessage::text(MessageRole::Assistant, "from a fast clock", 1_800_000_000_000);
    m.app_data.push(data(TURN, b"{\"t\":1}"));
    let scope = a.replica.import_conversation_with(&opts, &[m]).unwrap();
    let page = a.replica.messages(scope, None, 10).unwrap().messages;
    assert_eq!(osaurus_sync_core::hlc::physical_ms(page[0].hlc), 1_700_000_000_000);

    a.replica.set_message_app_data(page[0].id, &[data(TURN, b"{\"t\":2}")]).unwrap();
    assert_eq!(app_data(&a.replica, &page[0].id, TURN).unwrap(), b"{\"t\":2}");
    a.replica.edit_message(page[0].id, "edited").unwrap();
    assert_eq!(a.replica.messages(scope, None, 10).unwrap().messages[0].content, "edited");
}

#[test]
fn device_local_scopes_never_leave_the_device() {
    let env = Arc::new(TestEnv::new(7, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let mut b = open("PC", env.clone());
    let local = a.replica.create_scope_with(ScopeKind::Conversation, "Automation run", Replication::DeviceLocal).unwrap();
    let synced = a.replica.create_scope(ScopeKind::Conversation, "Chat").unwrap();
    a.replica.send_message(local, text("headless")).unwrap();
    assert_eq!(pair(&mut a, &mut b), vec![synced]);

    a.replica.send_message(local, text("still headless")).unwrap();
    assert_eq!(a.replica.pending_outbox(local).unwrap(), 0);
    assert!(a.replica.known_peer_devices().unwrap().iter().all(|d| *d == b.replica.device_key()));
    assert!(matches!(a.replica.add_member(local, b.replica.user_key(), Role::Member), Err(Error::PermissionDenied(_))));
    assert!(matches!(a.replica.create_invite(local, Role::Member, 60_000), Err(Error::PermissionDenied(_))));
    // Another device of the same user isn't authorized for it even with every event in hand.
    assert!(!a.replica.is_authorized(local, &b.replica.device_key()).unwrap());

    // Promoting shares it, history included.
    a.replica.promote_to_synced(local).unwrap();
    assert!(a.replica.is_authorized(local, &b.replica.device_key()).unwrap());
    copy_scope(&a.replica, &mut b.replica, local);
    let got: Vec<String> = b.replica.messages(local, None, 10).unwrap().messages.into_iter().map(|m| m.content).collect();
    assert_eq!(got, ["headless", "still headless"]);

    let gone = a.replica.create_scope_with(ScopeKind::Conversation, "Gone", Replication::DeviceLocal).unwrap();
    assert!(a.replica.delete_local_scope(synced).is_err());
    a.replica.delete_local_scope(gone).unwrap();
    assert!(a.replica.conversation(gone).unwrap().is_none());
}

#[test]
fn the_database_is_encrypted_at_rest() {
    let env = Arc::new(TestEnv::new(8, 1_700_000_000_000));
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let cfg = ReplicaConfig::new(dir.path(), "Mac", Platform::MacOs);
    let mut r = Replica::open(cfg.clone(), env.clone(), secrets.clone()).unwrap();
    let scope = r.create_scope(ScopeKind::Conversation, "Secret title").unwrap();
    r.send_message(scope, text("a very recognisable sentence")).unwrap();
    r.close().unwrap();
    drop(r);
    let db = dir.path().join("sync.db");
    assert!(!store::is_plaintext(&db));
    let bytes = std::fs::read(&db).unwrap();
    assert!(!bytes.windows(10).any(|w| w == b"recognisab"));
    assert!(!bytes.windows(6).any(|w| w == b"Secret"));
    assert!(secrets.get("view-key").is_some());
    assert!(dir.path().join("view-key.sealed").exists());
    // A database written under another key is set aside, then unlocked through the copy of
    // its key sealed to the recovery key, which this device's user secret opens.
    secrets.store("view-key", &[9u8; 32]).unwrap();
    let r = Replica::open(cfg, env, secrets).unwrap();
    assert!(!r.recovery_available().unwrap());
    assert_eq!(r.messages(scope, None, 10).unwrap().messages[0].content, "a very recognisable sentence");
    assert!(!dir.path().join("locked").exists());
}

#[test]
fn signing_out_keeps_owned_conversations_on_this_device() {
    let env = Arc::new(TestEnv::new(9, 1_700_000_000_000));
    let mut a = open("Mac", env.clone());
    let mut b = open("PC", env.clone());
    let mut carol = open("Carol", env.clone());
    let mine = a.replica.create_scope(ScopeKind::Conversation, "Mine").unwrap();
    a.replica.send_message(mine, text("keep me")).unwrap();
    pair(&mut a, &mut b);
    copy_scope(&a.replica, &mut b.replica, mine);
    let theirs = carol.replica.create_scope(ScopeKind::Workspace, "Carol's").unwrap();
    let invite = carol.replica.create_invite(theirs, Role::Member, 60_000).unwrap();
    carol.replica.admit_invitee(theirs, invite.token, a.replica.cert()).unwrap();
    copy_scope(&carol.replica, &mut a.replica, theirs);
    let old_user = a.replica.user_key();
    assert!(a.replica.conversation(theirs).unwrap().is_some());

    a.replica.sign_out_keeping_data().unwrap();
    assert_ne!(a.replica.user_key(), old_user);
    assert!(a.replica.is_device_revoked(&b.replica.device_key()).unwrap());
    let c = a.replica.conversation(mine).unwrap().unwrap();
    assert_eq!(c.replication, Replication::DeviceLocal);
    assert_eq!(c.my_role, Some(Role::Owner));
    assert_eq!(a.replica.pending_outbox(mine).unwrap(), 0);
    assert!(a.replica.conversation(theirs).unwrap().is_none());
    env.advance(5);
    a.replica.send_message(mine, text("after sign-out")).unwrap();
    let got: Vec<String> = a.replica.messages(mine, None, 10).unwrap().messages.into_iter().map(|m| m.content).collect();
    assert_eq!(got, ["keep me", "after sign-out"]);
    assert!(!a.replica.is_authorized(mine, &b.replica.device_key()).unwrap());
}
