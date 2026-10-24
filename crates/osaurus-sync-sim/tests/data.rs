use osaurus_sync_core::error::Error;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::{Principal, Role};
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::Replica;
use osaurus_sync_core::types::{Attachment, NewMessage};
use osaurus_sync_sim::Sim;

fn text(s: &str) -> NewMessage {
    NewMessage::text(MessageRole::User, s)
}

fn attachment(dir: &std::path::Path, name: &str, bytes: &[u8]) -> Attachment {
    let path = dir.join("upload.bin");
    std::fs::write(&path, bytes).unwrap();
    Attachment { path, media_type: "application/octet-stream".into(), name: name.into() }
}

fn contents(sim: &mut Sim, i: usize, scope: osaurus_sync_core::ids::ScopeId) -> Vec<String> {
    sim.r(i).messages(scope, None, 10_000).unwrap().messages.into_iter().map(|m| m.content).collect()
}

#[test]
fn leaving_removes_me_and_deletes_my_copy() {
    let mut sim = Sim::new(41);
    let alice = sim.add_node("Alice", Platform::MacOs);
    let bob = sim.add_node("Bob", Platform::MacOs);
    let scope = sim.r(alice).create_scope(ScopeKind::Conversation, "Plans").unwrap();
    sim.r(alice).send_message(scope, text("hello")).unwrap();
    sim.poll(alice);
    sim.invite(alice, bob, scope, Role::Member);
    sim.run(10_000);
    assert_eq!(contents(&mut sim, bob, scope), vec!["hello"]);

    sim.r(bob).leave_scope(scope).unwrap();
    // Hidden right away, deleted once Alice has the removal.
    assert!(sim.r(bob).conversations().unwrap().iter().all(|c| c.scope != scope));
    sim.poll(bob);
    sim.run(10_000);
    assert!(!sim.r(bob).scope_ids().unwrap().contains(&scope));
    let bob_user = Principal::User(sim.r(bob).user_key());
    let members = sim.r(alice).conversation(scope).unwrap().unwrap().members;
    assert!(members.iter().all(|m| m.principal != bob_user));

    // Alice's next offers don't bring the scope back.
    sim.r(alice).send_message(scope, text("after")).unwrap();
    sim.poll(alice);
    sim.disconnect(alice, bob);
    sim.connect(alice, bob);
    sim.run(10_000);
    assert!(!sim.r(bob).scope_ids().unwrap().contains(&scope));

    // A new invite does.
    sim.invite(alice, bob, scope, Role::Member);
    sim.run(10_000);
    assert!(sim.r(bob).scope_ids().unwrap().contains(&scope));
    assert!(!sim.r(bob).has_left(scope).unwrap());
}

#[test]
fn the_last_owner_cannot_leave_others_behind() {
    let mut sim = Sim::new(42);
    let alice = sim.add_node("Alice", Platform::MacOs);
    let bob = sim.add_node("Bob", Platform::MacOs);
    let scope = sim.r(alice).create_scope(ScopeKind::Conversation, "Plans").unwrap();
    sim.poll(alice);
    sim.invite(alice, bob, scope, Role::Member);
    sim.run(10_000);

    assert!(matches!(sim.r(alice).leave_scope(scope), Err(Error::PermissionDenied(_))));
    let bob_user = Principal::User(sim.r(bob).user_key());
    sim.r(alice).change_role(scope, bob_user, Role::Owner).unwrap();
    sim.poll(alice);
    sim.r(alice).leave_scope(scope).unwrap();
    sim.poll(alice);
    sim.run(10_000);
    assert!(!sim.r(alice).scope_ids().unwrap().contains(&scope));
    let alice_user = Principal::User(sim.r(alice).user_key());
    let members = sim.r(bob).conversation(scope).unwrap().unwrap().members;
    assert!(members.iter().all(|m| m.principal != alice_user));
    // Bob can carry on alone.
    sim.r(bob).send_message(scope, text("still here")).unwrap();
}

#[test]
fn leaving_a_solo_conversation_deletes_it_at_once() {
    let mut sim = Sim::new(43);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "Notes").unwrap();
    sim.r(mac).send_message(scope, text("one")).unwrap();
    sim.r(mac).leave_scope(scope).unwrap();
    assert!(!sim.r(mac).scope_ids().unwrap().contains(&scope));
    assert!(sim.r(mac).has_left(scope).unwrap());
    assert!(matches!(sim.r(mac).leave_scope(scope), Err(Error::NotFound(_))));
}

#[test]
fn storage_usage_and_export() {
    let mut sim = Sim::new(44);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "Trip").unwrap();
    sim.r(mac).send_message(scope, text("first")).unwrap();
    let src = tempfile::tempdir().unwrap();
    let mut msg = text("with a photo");
    msg.attachments.push(attachment(src.path(), "beach/1.jpg", &[7; 50_000]));
    sim.r(mac).send_message(scope, msg).unwrap();

    let usage = sim.r(mac).storage_usage().unwrap();
    assert!(usage.database_bytes > 0);
    assert!(usage.blob_bytes >= 50_000);

    let out = tempfile::tempdir().unwrap();
    let summary = sim.r(mac).export_conversation(scope, out.path()).unwrap();
    assert_eq!((summary.messages, summary.attachments, summary.missing_attachments), (2, 1, 0));
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(out.path().join("conversation.json")).unwrap()).unwrap();
    assert_eq!(doc["title"], "Trip");
    assert_eq!(doc["format"], "osaurus-sync-conversation");
    let messages = doc["messages"].as_array().unwrap();
    assert_eq!(messages[0]["content"], "first");
    let file = messages[1]["attachments"][0]["file"].as_str().unwrap();
    assert!(file.starts_with("attachments/") && !file["attachments/".len()..].contains('/'));
    assert_eq!(std::fs::read(out.path().join(file)).unwrap(), vec![7; 50_000]);
}

#[test]
fn wipe_deletes_data_and_keys() {
    let mut sim = Sim::new(45);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "Secret").unwrap();
    let src = tempfile::tempdir().unwrap();
    let mut msg = text("x");
    msg.attachments.push(attachment(src.path(), "a.bin", &[1; 1000]));
    sim.r(mac).send_message(scope, msg).unwrap();
    let old_device = sim.r(mac).device_key();
    sim.r(mac).wipe().unwrap();
    let n = &mut sim.nodes[mac];
    for name in Replica::secret_names() {
        assert!(n.secrets.get(name).is_none(), "{name} survived");
    }
    let dir = n.dir.path();
    assert!(!dir.join("sync.db").exists() && !dir.join("blobs").exists());
    // Opening again starts a new device with nothing in it.
    n.replica = None;
    let r = Replica::open(n.config(), n.env.clone(), n.secrets.clone()).unwrap();
    assert_ne!(r.device_key(), old_device);
    assert!(r.scope_ids().unwrap().is_empty());
}

#[test]
fn my_other_devices_stop_listing_a_scope_i_left() {
    let mut sim = Sim::new(46);
    let alice = sim.add_node("Alice", Platform::MacOs);
    let mac = sim.add_node("Bob Mac", Platform::MacOs);
    let pc = sim.add_node("Bob PC", Platform::Windows);
    sim.pair(mac, pc);
    sim.run(10_000);
    let scope = sim.r(alice).create_scope(ScopeKind::Conversation, "Plans").unwrap();
    sim.poll(alice);
    sim.invite(alice, mac, scope, Role::Member);
    sim.run(10_000);
    sim.heal(&[]);
    sim.run(10_000);
    assert!(sim.r(pc).conversations().unwrap().iter().any(|c| c.scope == scope));

    sim.r(mac).leave_scope(scope).unwrap();
    sim.poll(mac);
    sim.run(10_000);
    assert!(!sim.r(mac).scope_ids().unwrap().contains(&scope));
    assert!(sim.r(pc).conversations().unwrap().iter().all(|c| c.scope != scope));
    // The PC's copy doesn't bring it back to the Mac.
    sim.disconnect(mac, pc);
    sim.connect(mac, pc);
    sim.run(10_000);
    assert!(!sim.r(mac).scope_ids().unwrap().contains(&scope));
}
