use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::Role;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::sync::Notice;
use osaurus_sync_core::types::{DeliveryStatus, HistoryMode, ImportedMessage, NewMessage};
use osaurus_sync_sim::{Sim, snapshot};

const DAY: u64 = 86_400_000;

fn text(s: &str) -> NewMessage {
    NewMessage::text(MessageRole::User, s)
}

fn contents(sim: &mut Sim, i: usize, scope: osaurus_sync_core::ids::ScopeId) -> Vec<String> {
    sim.r(i).messages(scope, None, 10_000).unwrap().messages.into_iter().map(|m| m.content).collect()
}

#[test]
fn pairing_brings_over_existing_history() {
    let mut sim = Sim::new(1);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let pc = sim.add_node("PC", Platform::Windows);
    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "Before pairing").unwrap();
    sim.r(mac).send_message(scope, text("old message")).unwrap();
    sim.pair(mac, pc);
    sim.run(10_000);
    assert!(sim.nodes[pc].notices.iter().any(|n| matches!(n, Notice::Paired { .. })));
    assert_eq!(sim.r(pc).user_key(), sim.r(mac).user_key());
    assert_eq!(contents(&mut sim, pc, scope), vec!["old message"]);
    assert_eq!(snapshot(sim.r(pc), scope), snapshot(sim.r(mac), scope));
    // Both devices list each other.
    assert_eq!(sim.r(pc).devices().unwrap().len(), 2);
}

#[test]
fn wrong_pairing_code_is_refused() {
    let mut sim = Sim::new(2);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let pc = sim.add_node("PC", Platform::Windows);
    let mut invite = {
        let n = &mut sim.nodes[mac];
        n.engine.start_pairing(n.replica.as_ref().unwrap(), 300_000)
    };
    invite.code = "AAAA-BBBB-CCCC".into();
    let n = &mut sim.nodes[pc];
    let outs = n.engine.connect_pairing(n.replica.as_mut().unwrap(), &invite).unwrap();
    assert_eq!(outs.len(), 1);
    // Deliver by hand through the harness link.
    sim.link(pc, mac);
    sim.process(pc, outs);
    sim.run(100);
    assert!(sim.nodes[pc].notices.iter().any(|n| matches!(n, Notice::PairingFailed(_))));
    assert_ne!(sim.r(pc).user_key(), sim.r(mac).user_key());
}

#[test]
fn live_push_and_acknowledgement() {
    let mut sim = Sim::new(3);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let pc = sim.add_node("PC", Platform::Windows);
    sim.pair(mac, pc);
    sim.run(10_000);
    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "Live").unwrap();
    sim.poll(mac);
    sim.run(10_000);
    let id = sim.r(mac).send_message(scope, text("ping")).unwrap();
    assert_eq!(sim.r(mac).message_view(&id).unwrap().unwrap().status, DeliveryStatus::Pending);
    sim.poll(mac);
    sim.run(10_000);
    assert_eq!(contents(&mut sim, pc, scope), vec!["ping"]);
    assert_eq!(sim.r(mac).message_view(&id).unwrap().unwrap().status, DeliveryStatus::Delivered);
    sim.r(pc).rename_conversation(scope, "Renamed").unwrap();
    sim.poll(pc);
    sim.run(10_000);
    assert_eq!(sim.r(mac).conversation(scope).unwrap().unwrap().title, "Renamed");
}

#[test]
fn invite_over_the_network() {
    let mut sim = Sim::new(4);
    let alice = sim.add_node("Alice", Platform::MacOs);
    let bob = sim.add_node("Bob", Platform::Windows);
    let scope = sim.r(alice).create_scope(ScopeKind::Workspace, "Team").unwrap();
    sim.r(alice).send_message(scope, text("hello team")).unwrap();
    sim.invite(alice, bob, scope, Role::Member);
    sim.run(10_000);
    assert!(sim.nodes[bob].notices.iter().any(|n| matches!(n, Notice::InviteResult { error: None, .. })), "{:?}", sim.nodes[bob].notices);
    assert_eq!(contents(&mut sim, bob, scope), vec!["hello team"]);
    sim.r(bob).send_message(scope, text("hi")).unwrap();
    sim.poll(bob);
    sim.run(10_000);
    assert_eq!(contents(&mut sim, alice, scope), vec!["hello team", "hi"]);
}

#[test]
fn recent_only_device_fetches_older_history_on_demand() {
    let mut sim = Sim::new(5);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let phone = sim.add_node_with("iPhone", Platform::Ios, None, 0);
    let now = sim.clock;
    let old: Vec<ImportedMessage> = [400u64, 200, 100, 60, 10, 2]
        .iter()
        .map(|d| ImportedMessage { role: MessageRole::User, content: format!("{d} days ago"), created_ms: now - d * DAY })
        .collect();
    let scope = sim.r(mac).import_conversation("Imported", now - 400 * DAY, &old).unwrap();
    sim.pair(mac, phone);
    sim.run(10_000);
    assert_eq!(sim.r(phone).history_mode(scope).unwrap(), HistoryMode::Recent { days: 90 });
    assert_eq!(contents(&mut sim, phone, scope), vec!["60 days ago", "10 days ago", "2 days ago"]);
    let page = sim.r(phone).messages(scope, None, 50).unwrap();
    assert!(page.at_sync_boundary);
    assert!(!sim.r(phone).backfill_complete(scope).unwrap());
    let before = sim.r(phone).synced_from_hlc(scope).unwrap().unwrap();
    {
        let n = &mut sim.nodes[phone];
        let outs = n.engine.request_history(n.replica.as_mut().unwrap(), scope, before, 200).unwrap();
        sim.process(phone, outs);
    }
    sim.run(10_000);
    assert_eq!(contents(&mut sim, phone, scope), vec!["200 days ago", "100 days ago", "60 days ago", "10 days ago", "2 days ago"]);
}

#[test]
fn paused_backfill_keeps_only_the_last_week() {
    let mut sim = Sim::new(6);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let pc = sim.add_node("PC", Platform::Windows);
    let now = sim.clock;
    let old: Vec<ImportedMessage> =
        [30u64, 3, 0].iter().map(|d| ImportedMessage { role: MessageRole::User, content: format!("{d}d"), created_ms: now - d * DAY - 1000 }).collect();
    let scope = sim.r(mac).import_conversation("x", now - 31 * DAY, &old).unwrap();
    {
        let n = &mut sim.nodes[pc];
        n.engine.set_backfill_paused(n.replica.as_mut().unwrap(), true).unwrap();
    }
    sim.pair(mac, pc);
    sim.run(10_000);
    assert_eq!(contents(&mut sim, pc, scope), vec!["3d", "0d"]);
    {
        let n = &mut sim.nodes[pc];
        let outs = n.engine.set_backfill_paused(n.replica.as_mut().unwrap(), false).unwrap();
        sim.process(pc, outs);
    }
    sim.run(10_000);
    assert_eq!(contents(&mut sim, pc, scope), vec!["30d", "3d", "0d"]);
    assert!(sim.r(pc).backfill_complete(scope).unwrap());
}

#[test]
fn events_are_forwarded_through_a_middle_peer() {
    let mut sim = Sim::new(7);
    let a = sim.add_node("A", Platform::MacOs);
    let b = sim.add_node("B", Platform::Server);
    let c = sim.add_node("C", Platform::Windows);
    sim.pair(a, b);
    sim.run(10_000);
    sim.pair(b, c);
    sim.run(10_000);
    let scope = sim.r(a).create_scope(ScopeKind::Conversation, "relay").unwrap();
    sim.poll(a);
    sim.run(10_000);
    sim.r(a).send_message(scope, text("via b")).unwrap();
    sim.poll(a);
    sim.run(10_000);
    assert!(!sim.is_linked(a, c));
    assert_eq!(contents(&mut sim, c, scope), vec!["via b"]);
}

#[test]
fn revoked_device_is_refused() {
    let mut sim = Sim::new(8);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let lost = sim.add_node("Lost laptop", Platform::MacOs);
    sim.pair(mac, lost);
    sim.run(10_000);
    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "c").unwrap();
    sim.poll(mac);
    sim.run(10_000);
    sim.disconnect(mac, lost);
    let dev = sim.nodes[lost].device();
    sim.r(mac).revoke_device(dev).unwrap();
    sim.r(mac).send_message(scope, text("secret")).unwrap();
    sim.connect(lost, mac);
    sim.run(10_000);
    assert!(!sim.is_linked(lost, mac));
    assert!(contents(&mut sim, lost, scope).is_empty());
    assert!(sim.r(mac).devices().unwrap().iter().any(|d| d.device == dev && d.revoked));
}
