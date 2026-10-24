//! Membership, roles, revocation, agents and malformed input across separate users.

use std::sync::Arc;

use osaurus_sync_core::env::TestEnv;
use osaurus_sync_core::error::Error;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::{Principal, Role, ScopeId};
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::{IngestOutcome, InviteToken, MemorySecretStore};
use osaurus_sync_core::types::{DeliveryStatus, NewMessage};
use osaurus_sync_core::{Replica, ReplicaConfig};

fn open(name: &str, env: &Arc<TestEnv>) -> (Replica, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let r = Replica::open(ReplicaConfig::new(dir.path(), name, Platform::MacOs), env.clone(), Arc::new(MemorySecretStore::default())).unwrap();
    (r, dir)
}

fn text(s: &str) -> NewMessage {
    NewMessage::text(MessageRole::User, s)
}

fn copy(a: &Replica, b: &mut Replica, scope: ScopeId) {
    let mut events = a.events_by_hashes(&a.all_hashes(scope).unwrap()).unwrap();
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

/// Alice owns a workspace; Bob joins through an invite as a Member.
fn alice_and_bob(env: &Arc<TestEnv>) -> ((Replica, tempfile::TempDir), (Replica, tempfile::TempDir), ScopeId) {
    let (mut alice, ad) = open("Alice Mac", env);
    let (mut bob, bd) = open("Bob PC", env);
    let scope = alice.create_scope(ScopeKind::Workspace, "Team").unwrap();
    alice.send_message(scope, text("welcome")).unwrap();
    let invite = alice.create_invite(scope, Role::Member, 86_400_000).unwrap();
    let decoded = InviteToken::decode(&invite.encode()).unwrap();
    assert_eq!(decoded.scope, scope);
    env.advance(10);
    alice.admit_invitee(scope, decoded.token, bob.cert()).unwrap();
    assert!(matches!(alice.admit_invitee(scope, decoded.token, bob.cert()), Err(Error::PermissionDenied(_))));
    copy(&alice, &mut bob, scope);
    ((alice, ad), (bob, bd), scope)
}

#[test]
fn invitee_reads_and_writes() {
    let env = Arc::new(TestEnv::new(11, 1_700_000_000_000));
    let ((mut alice, _a), (mut bob, _b), scope) = alice_and_bob(&env);
    assert_eq!(contents(&bob, scope), vec!["welcome"]);
    assert_eq!(bob.conversation(scope).unwrap().unwrap().my_role, Some(Role::Member));
    env.advance(10);
    bob.send_message(scope, text("hi alice")).unwrap();
    copy(&bob, &mut alice, scope);
    assert_eq!(contents(&alice, scope), vec!["welcome", "hi alice"]);
    // Members cannot manage membership; the local write is refused and nothing is stored.
    let before = bob.all_hashes(scope).unwrap().len();
    assert!(matches!(bob.add_member(scope, alice.user_key(), Role::Viewer), Err(Error::PermissionDenied(_))));
    assert_eq!(bob.all_hashes(scope).unwrap().len(), before);
    // Members cannot delete other people's messages.
    let welcome = alice.messages(scope, None, 10).unwrap().messages[0].id;
    assert!(matches!(bob.delete_message(welcome), Err(Error::PermissionDenied(_))));
}

#[test]
fn post_concurrent_with_removal_is_rejected_everywhere() {
    let env = Arc::new(TestEnv::new(12, 1_700_000_000_000));
    let ((mut alice, _a), (mut bob, _b), scope) = alice_and_bob(&env);
    env.advance(10);
    bob.send_message(scope, text("before removal")).unwrap();
    copy(&bob, &mut alice, scope);
    // Alice removes Bob. Bob, offline, posts again.
    env.advance(10);
    alice.remove_member(scope, Principal::User(bob.user_key())).unwrap();
    env.advance(10);
    let late = bob.send_message(scope, text("after removal")).unwrap();
    copy(&bob, &mut alice, scope);
    copy(&alice, &mut bob, scope);
    assert_eq!(contents(&alice, scope), vec!["welcome", "before removal"]);
    // Bob still sees his own message, marked as not delivered.
    assert!(matches!(bob.message_view(&late).unwrap().unwrap().status, DeliveryStatus::Rejected(_)));
    assert_eq!(bob.conversation(scope).unwrap().unwrap().my_role, None);
    // New content uses a key Bob does not receive.
    env.advance(10);
    alice.send_message(scope, text("after bob left")).unwrap();
    copy(&alice, &mut bob, scope);
    assert!(!contents(&bob, scope).contains(&"after bob left".to_string()));
}

#[test]
fn admin_moderation_and_role_limits() {
    let env = Arc::new(TestEnv::new(13, 1_700_000_000_000));
    let ((mut alice, _a), (mut bob, _b), scope) = alice_and_bob(&env);
    let (mut carol, _c) = open("Carol", &env);
    alice.change_role(scope, Principal::User(bob.user_key()), Role::Admin).unwrap();
    copy(&alice, &mut bob, scope);
    assert_eq!(bob.conversation(scope).unwrap().unwrap().my_role, Some(Role::Admin));
    // An Admin can add Members but cannot create another Admin or demote the Owner.
    bob.add_member(scope, carol.user_key(), Role::Member).unwrap();
    assert!(bob.change_role(scope, Principal::User(carol.user_key()), Role::Admin).is_err());
    assert!(bob.change_role(scope, Principal::User(alice.user_key()), Role::Member).is_err());
    // An Admin can delete anyone's message.
    let welcome = bob.messages(scope, None, 10).unwrap().messages[0].id;
    bob.delete_message(welcome).unwrap();
    copy(&bob, &mut alice, scope);
    assert!(alice.message_view(&welcome).unwrap().is_none());
    // Carol was added directly; her device is admitted when an existing member sees her cert.
    copy(&alice, &mut carol, scope);
    let admitted = alice.admit_device(carol.cert()).unwrap();
    assert_eq!(admitted, vec![scope]);
    copy(&alice, &mut carol, scope);
    env.advance(10);
    carol.send_message(scope, text("carol here")).unwrap();
    copy(&carol, &mut alice, scope);
    assert!(contents(&alice, scope).contains(&"carol here".to_string()));
}

#[test]
fn agent_messages_only_from_host() {
    let env = Arc::new(TestEnv::new(14, 1_700_000_000_000));
    let ((mut alice, _a), (mut bob, _b), scope) = alice_and_bob(&env);
    let agent = alice.assign_agent(scope, &[7u8; 32], None, Role::Member, 30 * 86_400_000).unwrap();
    assert_eq!(alice.agent_host(scope, &agent).unwrap(), Some(alice.device_key()));
    let mut m = NewMessage::text(MessageRole::Assistant, "I am the agent");
    m.as_agent = Some(agent);
    let id = alice.send_message(scope, m.clone()).unwrap();
    let v = alice.message_view(&id).unwrap().unwrap();
    assert!(v.is_agent);
    assert_eq!(v.author, Principal::Agent(agent));
    copy(&alice, &mut bob, scope);
    assert!(bob.message_view(&id).unwrap().unwrap().is_agent);
    // Bob's device is not the host: speaking as the agent is refused.
    assert!(bob.send_message(scope, m).is_err());
}

#[test]
fn held_until_parents_arrive_and_garbage_rejected() {
    let env = Arc::new(TestEnv::new(15, 1_700_000_000_000));
    let (mut a, _ad) = open("A", &env);
    let (mut b, _bd) = open("B", &env);
    let scope = a.create_scope(ScopeKind::Conversation, "t").unwrap();
    a.send_message(scope, text("x")).unwrap();
    let hashes = a.all_hashes(scope).unwrap();
    let ids = |control| a.recon_keys(scope, control, 0, u64::MAX).unwrap().into_iter().map(|k| k.1).collect::<Vec<_>>();
    // Content first: it waits for the control events it depends on.
    let content = a.events_by_ids(scope, &ids(false)).unwrap();
    let control = a.events_by_ids(scope, &ids(true)).unwrap();
    assert!(matches!(b.ingest(&content[0]).unwrap(), IngestOutcome::Invalid(_)), "events for unjoined scopes are refused");
    b.join_scope(scope).unwrap();
    for e in &content {
        assert!(matches!(b.ingest(e).unwrap(), IngestOutcome::Held { .. }));
    }
    for e in control.iter().rev() {
        b.ingest(e).unwrap();
    }
    let events = a.events_by_hashes(&hashes).unwrap();
    assert!(b.held_missing(scope).unwrap().is_empty());
    assert_eq!(b.all_hashes(scope).unwrap(), hashes);

    assert!(matches!(b.ingest(b"garbage").unwrap(), IngestOutcome::Invalid(_)));
    let mut tampered = events[0].clone();
    let n = tampered.len();
    tampered[n - 1] ^= 1;
    assert!(matches!(b.ingest(&tampered).unwrap(), IngestOutcome::Invalid(_)));
    assert!(matches!(b.ingest(&events[0]).unwrap(), IngestOutcome::Duplicate { .. }));
}

#[test]
fn tentative_scopes_are_kept_only_for_members() {
    let env = Arc::new(TestEnv::new(17, 1_700_000_000_000));
    let ((alice, _a), (mut bob, _b), scope) = alice_and_bob(&env);
    let (mut carol, _c) = open("Carol", &env);
    let events = alice.events_by_hashes(&alice.all_hashes(scope).unwrap()).unwrap();
    for r in [&mut bob, &mut carol] {
        r.purge_scope(scope).unwrap();
        r.join_scope_tentatively(scope).unwrap();
        for e in &events {
            r.ingest(e).unwrap();
        }
    }
    assert!(carol.conversations().unwrap().is_empty(), "tentative scopes are not listed");
    assert!(!carol.settle_scope(scope).unwrap());
    assert!(!carol.scope_ids().unwrap().contains(&scope));
    assert!(carol.all_hashes(scope).unwrap().is_empty());
    assert!(bob.settle_scope(scope).unwrap());
    assert_eq!(contents(&bob, scope), vec!["welcome"]);
    assert_eq!(bob.conversations().unwrap().len(), 1);
}

#[test]
fn delivery_order_does_not_matter() {
    let env = Arc::new(TestEnv::new(16, 1_700_000_000_000));
    let ((mut alice, _a), (mut bob, _b), scope) = alice_and_bob(&env);
    for i in 0..5 {
        env.advance(3);
        alice.send_message(scope, text(&format!("a{i}"))).unwrap();
        bob.send_message(scope, text(&format!("b{i}"))).unwrap();
    }
    alice.rename_conversation(scope, "A title").unwrap();
    copy(&bob, &mut alice, scope);
    let (mut c1, _c1) = open("observer1", &env);
    let (mut c2, _c2) = open("observer2", &env);
    c1.join_scope(scope).unwrap();
    c2.join_scope(scope).unwrap();
    let mut events = alice.events_by_hashes(&alice.all_hashes(scope).unwrap()).unwrap();
    for e in &events {
        c1.ingest(e).unwrap();
    }
    events.reverse();
    for e in &events {
        c2.ingest(e).unwrap();
    }
    assert_eq!(c1.all_hashes(scope).unwrap(), c2.all_hashes(scope).unwrap());
    let v1: Vec<_> = c1.scope_heads(scope).unwrap().0;
    let v2: Vec<_> = c2.scope_heads(scope).unwrap().0;
    assert_eq!(v1, v2);
}
