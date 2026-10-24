use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::{Role, ScopeId};
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::sync::engine::{MAX_INVALID_PER_SESSION, MAX_UNTRUSTED_MSGS};
use osaurus_sync_core::sync::proto::{PROTOCOL_VERSION, ScopeOffer};
use osaurus_sync_core::sync::{Msg, Notice, Out};
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

/// Delivers `msg` from `from` to `to` as if `from` had sent it, session or not.
fn inject(sim: &mut Sim, from: usize, to: usize, msg: Msg) {
    let dev = sim.nodes[to].device();
    sim.link(from, to);
    sim.process(from, vec![Out::Send(dev, msg)]);
}

fn all_events(sim: &mut Sim, i: usize, scope: ScopeId) -> Vec<Vec<u8>> {
    let hashes = sim.r(i).all_hashes(scope).unwrap();
    sim.r(i).events_by_hashes(&hashes).unwrap()
}

fn offer(sim: &mut Sim, i: usize, scope: ScopeId) -> Msg {
    let (control_heads, heads) = sim.r(i).scope_heads(scope).unwrap();
    Msg::Offer { scopes: vec![ScopeOffer { scope, control_heads, heads }] }
}

#[test]
fn strangers_are_turned_away() {
    let mut sim = Sim::new(9);
    let alice = sim.add_node("Alice", Platform::MacOs);
    let mallory = sim.add_node("Mallory", Platform::Windows);
    let scope = sim.r(alice).create_scope(ScopeKind::Conversation, "Private").unwrap();
    sim.r(alice).send_message(scope, text("private")).unwrap();
    let spam = sim.r(mallory).create_scope(ScopeKind::Conversation, "Spam").unwrap();
    sim.r(mallory).send_message(spam, text("buy now")).unwrap();

    sim.connect(mallory, alice);
    sim.run(10_000);
    assert!(!sim.is_linked(mallory, alice));
    assert!(sim.nodes[alice].notices.iter().any(|n| matches!(n, Notice::PeerRejected(_))));
    assert!(!sim.r(mallory).scope_ids().unwrap().contains(&scope));

    // Unsolicited pushes and offers outside a session are dropped without storing anything.
    let events = all_events(&mut sim, mallory, spam);
    inject(&mut sim, mallory, alice, Msg::Events { scope: spam, tag: 0, events, last: true });
    let o = offer(&mut sim, mallory, spam);
    inject(&mut sim, mallory, alice, o);
    sim.run(10_000);
    assert!(!sim.r(alice).scope_ids().unwrap().contains(&spam));
    assert_eq!(sim.r(alice).conversations().unwrap().len(), 1);
    assert!(sim.r(alice).devices().unwrap().iter().all(|d| d.user == sim.nodes[alice].replica.as_ref().unwrap().user_key()));
}

#[test]
fn open_invite_lets_a_stranger_redeem_but_not_sync() {
    let mut sim = Sim::new(10);
    let alice = sim.add_node("Alice", Platform::MacOs);
    let mallory = sim.add_node("Mallory", Platform::Windows);
    let scope = sim.r(alice).create_scope(ScopeKind::Workspace, "Team").unwrap();
    sim.r(alice).send_message(scope, text("team only")).unwrap();
    sim.r(alice).create_invite(scope, Role::Member, 3_600_000).unwrap();
    let spam = sim.r(mallory).create_scope(ScopeKind::Conversation, "Spam").unwrap();
    sim.r(mallory).send_message(spam, text("buy now")).unwrap();

    // Alice keeps the session while the invite is open but offers and accepts nothing. (Mallory,
    // holding no invite from Alice, hangs up on its side.)
    sim.connect(mallory, alice);
    sim.run(10_000);
    assert!(!sim.nodes[alice].notices.iter().any(|n| matches!(n, Notice::PeerRejected(_))));
    assert!(!sim.r(mallory).scope_ids().unwrap().contains(&scope));
    let events = all_events(&mut sim, mallory, spam);
    inject(&mut sim, mallory, alice, Msg::Events { scope: spam, tag: 0, events, last: true });
    let o = offer(&mut sim, mallory, spam);
    inject(&mut sim, mallory, alice, o);
    sim.run(10_000);
    assert!(!sim.r(alice).scope_ids().unwrap().contains(&spam));
    assert_eq!(sim.r(alice).conversations().unwrap().len(), 1);
}

#[test]
fn restored_backup_recovers_with_every_device_lost() {
    let mut sim = Sim::new(13);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let pc = sim.add_node("PC", Platform::Windows);
    let bob = sim.add_node("Bob", Platform::Windows);
    sim.pair(mac, pc);
    sim.run(10_000);
    let scope = sim.r(mac).create_scope(ScopeKind::Workspace, "Team").unwrap();
    sim.r(mac).send_message(scope, text("before the loss")).unwrap();
    sim.invite(mac, bob, scope, Role::Member);
    sim.run(10_000);
    let phrase = sim.r(mac).recovery_phrase().join(" ");
    let user = sim.r(mac).user_key();

    // Both of Alice's devices are gone. Bob keeps talking; a backup of the Mac is restored.
    sim.disconnect_all(pc);
    sim.nodes[pc].replica = None;
    sim.r(bob).send_message(scope, text("anyone there?")).unwrap();
    let new = sim.restore_onto_new_device(mac, "New Mac");
    assert!(sim.r(new).recovery_available().unwrap());
    assert!(sim.r(new).conversations().unwrap().is_empty());

    sim.r(new).adopt_user(osaurus_sync_core::keys::UserSecret::from_phrase(&phrase).unwrap()).unwrap();
    assert_eq!(sim.r(new).user_key(), user);
    assert_eq!(contents(&mut sim, new, scope), vec!["before the loss"]);
    sim.connect(new, bob);
    sim.run(10_000);
    assert_eq!(contents(&mut sim, new, scope), vec!["before the loss", "anyone there?"]);
    sim.r(new).send_message(scope, text("back")).unwrap();
    sim.poll(new);
    sim.run(10_000);
    assert_eq!(contents(&mut sim, bob, scope), vec!["before the loss", "anyone there?", "back"]);
}

#[test]
fn peers_over_budget_are_disconnected() {
    let mut sim = Sim::new(12);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let pc = sim.add_node("PC", Platform::Windows);
    sim.pair(mac, pc);
    sim.run(10_000);
    assert!(sim.is_linked(mac, pc));
    // A trusted peer that floods garbage is cut off.
    let garbage = vec![vec![0u8; 8]; MAX_INVALID_PER_SESSION + 1];
    inject(&mut sim, pc, mac, Msg::Events { scope: ScopeId([1; 16]), tag: 0, events: garbage, last: true });
    sim.run(10_000);
    assert!(!sim.is_linked(mac, pc));

    // An untrusted peer gets a handful of messages to redeem an invite, then is cut off.
    let mallory = sim.add_node("Mallory", Platform::Windows);
    let scope = sim.r(mac).create_scope(ScopeKind::Workspace, "Team").unwrap();
    sim.r(mac).create_invite(scope, Role::Member, 3_600_000).unwrap();
    let cert = sim.r(mallory).cert().clone();
    let peer = sim.nodes[mallory].device();
    let n = &mut sim.nodes[mac];
    let r = n.replica.as_mut().unwrap();
    let outs = n.engine.handle(r, peer, Msg::Hello { version: PROTOCOL_VERSION, cert, storage_role: false }).unwrap();
    assert!(!outs.iter().any(|o| matches!(o, Out::Close(..))));
    let mut closed = false;
    for _ in 0..=MAX_UNTRUSTED_MSGS {
        let outs = n.engine.handle(r, peer, Msg::Denied { scope }).unwrap();
        closed |= outs.iter().any(|o| matches!(o, Out::Close(..)));
    }
    assert!(closed);
}

#[test]
fn member_cannot_plant_a_foreign_scope() {
    let mut sim = Sim::new(11);
    let alice = sim.add_node("Alice", Platform::MacOs);
    let bob = sim.add_node("Bob", Platform::Windows);
    let shared = sim.r(alice).create_scope(ScopeKind::Workspace, "Team").unwrap();
    sim.invite(alice, bob, shared, Role::Member);
    sim.run(10_000);
    assert_eq!(sim.r(bob).conversations().unwrap().len(), 1);

    // Bob offers and pushes a scope Alice was never added to.
    let other = sim.r(bob).create_scope(ScopeKind::Conversation, "Not for Alice").unwrap();
    sim.r(bob).send_message(other, text("planted")).unwrap();
    let events = all_events(&mut sim, bob, other);
    inject(&mut sim, bob, alice, Msg::Events { scope: other, tag: 0, events, last: true });
    let o = offer(&mut sim, bob, other);
    inject(&mut sim, bob, alice, o);
    sim.run(10_000);
    assert!(!sim.r(alice).scope_ids().unwrap().contains(&other));
    let listed: Vec<_> = sim.r(alice).conversations().unwrap().into_iter().map(|c| c.scope).collect();
    assert_eq!(listed, vec![shared]);
}
