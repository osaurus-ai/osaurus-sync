use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::ScopeId;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::types::NewMessage;
use osaurus_sync_sim::{Sim, snapshot};

fn title(sim: &mut Sim, i: usize, scope: ScopeId) -> String {
    sim.r(i).conversation(scope).unwrap().unwrap().title
}

fn contents(sim: &mut Sim, i: usize, scope: ScopeId) -> Vec<String> {
    sim.r(i).messages(scope, None, 1000).unwrap().messages.into_iter().map(|m| m.content).collect()
}

#[test]
fn storing_peer_lets_others_compact_and_keeps_full_history() {
    let mut sim = Sim::new(21);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let server = sim.add_storage_node("Server");

    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "Draft 0").unwrap();
    let id = sim.r(mac).send_message(scope, NewMessage::text(MessageRole::User, "v0")).unwrap();
    let sent_hlc = sim.r(mac).message_view(&id).unwrap().unwrap().hlc;
    sim.advance(1000);
    for i in 1..=3 {
        sim.r(mac).edit_message(id, &format!("v{i}")).unwrap();
        sim.advance(1000);
    }
    for i in 1..=6 {
        sim.r(mac).rename_conversation(scope, &format!("Draft {i}")).unwrap();
        sim.advance(1000);
    }
    // Nothing is compacted without a storing peer.
    assert_eq!(sim.r(mac).compact(scope).unwrap().upto_seq, 0);

    sim.pair(mac, server);
    sim.run(50_000);
    assert_eq!(snapshot(sim.r(server), scope), snapshot(sim.r(mac), scope));

    // The server reported a full pass, so the Mac dropped two superseded edits and folded its
    // Loro updates into one snapshot.
    let dropped = sim.r(mac).compacted_events(scope).unwrap();
    assert!(dropped >= 2 + 4, "compacted {dropped}");
    assert_eq!(sim.r(server).compacted_events(scope).unwrap(), 0);
    assert_eq!(contents(&mut sim, mac, scope), vec!["v3"]);
    assert_eq!(title(&mut sim, mac, scope), "Draft 6");

    // The snapshot reaches the server and the documents still agree.
    sim.run(50_000);
    assert_eq!(title(&mut sim, server, scope), "Draft 6");

    // The Mac reloads its document from the snapshot.
    sim.restart(mac);
    sim.connect(mac, server);
    sim.run(50_000);
    assert_eq!(title(&mut sim, mac, scope), "Draft 6");
    sim.r(mac).set_pinned(scope, true).unwrap();
    sim.poll(mac);
    sim.run(50_000);
    assert!(sim.r(server).conversation(scope).unwrap().unwrap().pinned);

    // A new device that syncs only with the compacted Mac still gets the current state.
    let pc = sim.add_node("PC", Platform::Windows);
    sim.disconnect_all(server);
    sim.pair(mac, pc);
    sim.run(50_000);
    assert_eq!(contents(&mut sim, pc, scope), vec!["v3"]);
    assert_eq!(title(&mut sim, pc, scope), "Draft 6");
    assert!(sim.r(pc).conversation(scope).unwrap().unwrap().pinned);
    sim.r(pc).rename_conversation(scope, "From PC").unwrap();
    sim.poll(pc);
    sim.run(50_000);
    sim.heal(&[]);
    sim.run(50_000);
    for i in [mac, server, pc] {
        assert_eq!(title(&mut sim, i, scope), "From PC");
    }

    // The server rebuilds the conversation as it was when the message was first sent.
    let past = sim.r(server).view_at(scope, sent_hlc, 100).unwrap();
    assert!(past.complete);
    assert_eq!(past.conversation.unwrap().title, "Draft 0");
    assert_eq!(past.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), vec!["v0"]);
    let mid = sim.r(server).view_at(scope, sent_hlc + osaurus_sync_core::hlc::pack(2000, 0), 100).unwrap();
    assert_eq!(mid.messages[0].content, "v2");
    assert_eq!(mid.conversation.unwrap().title, "Draft 0");
    // The view is read-only: the live state is unchanged.
    assert_eq!(contents(&mut sim, server, scope), vec!["v3"]);

    // The compacted Mac knows its own view of the past is partial.
    let partial = sim.r(mac).view_at(scope, sent_hlc + osaurus_sync_core::hlc::pack(2000, 0), 100).unwrap();
    assert!(!partial.complete);
    assert_eq!(partial.messages.len(), 1);
}

#[test]
fn offline_change_merges_after_others_fold_their_history() {
    let mut sim = Sim::new(23);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let pc = sim.add_node("PC", Platform::Windows);
    let server = sim.add_storage_node("Server");
    sim.pair(mac, pc);
    sim.run(50_000);
    sim.pair(mac, server);
    sim.run(50_000);
    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "Start").unwrap();
    sim.r(mac).set_pinned(scope, true).unwrap();
    sim.poll(mac);
    sim.run(50_000);

    // The PC changes the title offline, on top of history the Mac is about to fold.
    sim.disconnect_all(pc);
    sim.disconnect_all(server);
    sim.r(pc).rename_conversation(scope, "From PC").unwrap();
    for i in 0..6 {
        sim.advance(1000);
        sim.r(mac).rename_conversation(scope, &format!("Mac {i}")).unwrap();
    }
    sim.connect(mac, server);
    sim.run(50_000);
    assert!(sim.r(mac).compacted_events(scope).unwrap() > 0);

    sim.heal(&[]);
    sim.run(200_000);
    let want = title(&mut sim, server, scope);
    for i in [mac, pc, server] {
        assert_eq!(title(&mut sim, i, scope), want, "node {i}");
        sim.r(i).rebuild_all_views().unwrap();
        assert_eq!(title(&mut sim, i, scope), want, "node {i} after rebuild");
    }
}

#[test]
fn compaction_waits_for_every_storing_peer() {
    let mut sim = Sim::new(22);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let s1 = sim.add_storage_node("Server 1");
    let s2 = sim.add_storage_node("Server 2");
    let scope = sim.r(mac).create_scope(ScopeKind::Conversation, "t").unwrap();
    let id = sim.r(mac).send_message(scope, NewMessage::text(MessageRole::User, "a")).unwrap();
    let edit = |sim: &mut Sim, s: &str| {
        sim.advance(1000);
        sim.r(mac).edit_message(id, s).unwrap();
        sim.poll(mac);
        sim.run(50_000);
    };
    edit(&mut sim, "b");
    edit(&mut sim, "c");
    sim.pair(mac, s1);
    sim.run(50_000);
    sim.pair(mac, s2);
    sim.run(50_000);
    // Both servers reported holding "b" and "c"; "b" lost to "c".
    assert_eq!(sim.r(mac).compacted_events(scope).unwrap(), 1);

    // While the second server is away, the first one sees two more edits.
    sim.disconnect_all(s2);
    edit(&mut sim, "d");
    edit(&mut sim, "e");
    sim.disconnect_all(s1);
    sim.connect(mac, s1);
    sim.run(50_000);
    // "c" is superseded and both servers hold it; "d" is held only by the first server.
    assert_eq!(sim.r(mac).compacted_events(scope).unwrap(), 2);

    sim.connect(mac, s2);
    sim.run(50_000);
    assert_eq!(sim.r(mac).compacted_events(scope).unwrap(), 3);
    assert_eq!(contents(&mut sim, mac, scope), vec!["e"]);
    assert_eq!(snapshot(sim.r(s2), scope).unwrap().messages, snapshot(sim.r(mac), scope).unwrap().messages);
}
