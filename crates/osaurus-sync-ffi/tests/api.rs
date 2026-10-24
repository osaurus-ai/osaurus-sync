//! Exercises the exported surface from Rust, the way the bindings call it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::executor::block_on;
use osaurus_sync_ffi::*;

#[derive(Default, Clone)]
struct Keys(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl KeyStore for Keys {
    fn load(&self, name: String) -> Result<Option<Vec<u8>>, SyncError> {
        Ok(self.0.lock().unwrap().get(&name).cloned())
    }
    fn store(&self, name: String, value: Vec<u8>) -> Result<(), SyncError> {
        self.0.lock().unwrap().insert(name, value);
        Ok(())
    }
    fn delete(&self, name: String) -> Result<(), SyncError> {
        self.0.lock().unwrap().remove(&name);
        Ok(())
    }
}

#[derive(Default, Clone)]
struct Events(Arc<Mutex<Vec<CoreEvent>>>);
impl CoreListener for Events {
    fn on_event(&self, e: CoreEvent) {
        self.0.lock().unwrap().push(e);
    }
}

#[derive(Default, Clone)]
struct ScopeLog(Arc<Mutex<Vec<ScopeChange>>>, Arc<Mutex<Vec<String>>>);
impl ScopeListener for ScopeLog {
    fn on_changes(&self, c: Vec<ScopeChange>) {
        self.1.lock().unwrap().push(std::thread::current().name().unwrap_or("").to_string());
        self.0.lock().unwrap().extend(c);
    }
}

#[derive(Default, Clone)]
struct ConvLog(Arc<Mutex<Vec<ConversationChange>>>);
impl ConversationListener for ConvLog {
    fn on_changes(&self, c: Vec<ConversationChange>) {
        self.0.lock().unwrap().extend(c);
    }
}

fn config(dir: &std::path::Path, name: &str, platform: Platform, network: bool) -> CoreConfig {
    CoreConfig {
        data_dir: dir.join(name).to_string_lossy().into_owned(),
        device_name: name.into(),
        platform,
        history: None,
        network: NetworkConfig { enabled: network, ..local_network_config() },
        api_version: api_version(),
    }
}

fn until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn text(s: &str) -> OutgoingMessage {
    OutgoingMessage { role: MessageRole::User, content: s.into(), attachments: vec![], reply_to: None, as_agent: None, tool_call_id: None, tool_name: None }
}

fn contents(c: &SyncCore, scope: &str) -> Vec<String> {
    c.messages(scope.into(), None, 100).unwrap().messages.into_iter().map(|m| m.content).collect()
}

#[test]
fn version_and_lock_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), "a", Platform::MacOs, false);
    cfg.api_version = 999;
    let err = SyncCore::open(cfg.clone(), Box::new(Keys::default())).err().unwrap();
    assert!(matches!(err, SyncError::IncompatibleVersion { code: 110, .. }));
    cfg.api_version = api_version();
    let a = SyncCore::open(cfg.clone(), Box::new(Keys::default())).unwrap();
    let err = SyncCore::open(cfg.clone(), Box::new(Keys::default())).err().unwrap();
    assert!(matches!(err, SyncError::AlreadyOpen { code: 106, .. }));
    block_on(a.close()).unwrap();
    assert!(matches!(a.conversations(), Err(SyncError::Closed { code: 112, .. })));
    let b = SyncCore::open(cfg, Box::new(Keys::default())).unwrap();
    block_on(b.close()).unwrap();
}

#[test]
fn local_api_and_callbacks() {
    let dir = tempfile::tempdir().unwrap();
    let c = SyncCore::open(config(dir.path(), "mac", Platform::MacOs, false), Box::new(Keys::default())).unwrap();
    let events = Events::default();
    c.set_listener(Some(Box::new(events.clone())));
    let convs = ConvLog::default();
    let _cs = c.subscribe_conversations(Box::new(convs.clone()));

    let scope = c.create_conversation("Plans".into()).unwrap();
    let log = ScopeLog::default();
    let sub = c.subscribe(scope.clone(), Box::new(log.clone())).unwrap();
    let id = c.send_message(scope.clone(), text("hello world")).unwrap();
    c.edit_message(id.clone(), "hello there".into()).unwrap();
    c.add_reaction(id.clone(), "👍".into()).unwrap();
    c.rename_conversation(scope.clone(), "Trip plans".into()).unwrap();
    c.set_pinned(scope.clone(), true).unwrap();
    c.set_sort_order(scope.clone(), 1.5).unwrap();

    until("callbacks", || log.0.lock().unwrap().len() >= 3);
    assert!(log.1.lock().unwrap().iter().all(|t| t == "osaurus-sync-callbacks"));
    let m = c.message(id.clone()).unwrap().unwrap();
    assert_eq!(m.content, "hello there");
    assert!(m.edited);
    assert_eq!(m.reactions[0].value, "👍");
    // Nothing else stores this scope yet, so there is nobody to wait for.
    assert_eq!(m.delivery, Delivery::Delivered);
    let conv = c.conversation(scope.clone()).unwrap().unwrap();
    assert_eq!(conv.title, "Trip plans");
    assert!(conv.pinned);
    assert_eq!(conv.my_role, Some(MemberRole::Owner));
    until("conversation feed", || !convs.0.lock().unwrap().is_empty());

    assert_eq!(c.search("there".into(), 10).unwrap().len(), 1);
    c.remove_reaction(id.clone(), "👍".into()).unwrap();
    c.delete_message(id.clone()).unwrap();
    assert!(contents(&c, &scope).is_empty());

    let legacy = c
        .import_legacy_conversation(
            "Old chat".into(),
            1_600_000_000_000,
            vec![LegacyMessage { role: MessageRole::User, content: "from 2020".into(), created_ms: 1_600_000_000_000 }],
        )
        .unwrap();
    let page = c.messages(legacy, None, 10).unwrap();
    assert_eq!(page.messages[0].timestamp_ms, 1_600_000_000_000);

    let notes = c.create_conversation("Notes".into()).unwrap();
    let first = c.send_message(notes.clone(), text("first")).unwrap();
    let then = c.message(first).unwrap().unwrap().timestamp_ms;
    std::thread::sleep(std::time::Duration::from_millis(5));
    c.rename_conversation(notes.clone(), "Renamed notes".into()).unwrap();
    c.send_message(notes.clone(), text("second")).unwrap();
    let past = c.view_at(notes.clone(), then, 50).unwrap();
    assert!(past.complete);
    assert_eq!(past.conversation.unwrap().title, "Notes");
    assert_eq!(past.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["first"]);
    assert_eq!(contents(&c, &notes), ["first", "second"]);

    assert_eq!(c.recovery_phrase().unwrap().len(), 24);
    assert_eq!(c.devices().unwrap().len(), 1);
    c.set_history_mode(None, HistoryMode::Recent { days: 30 }).unwrap();
    c.set_backfill_policy(BackfillPolicy::Paused).unwrap();
    assert!(c.diagnostics().backfill_paused);
    assert_eq!(c.sync_status(scope.clone()).unwrap().scope, scope);
    assert!(matches!(c.send_message("zz".into(), text("x")), Err(SyncError::InvalidInput { code: 101, .. })));

    // Streams end with an interruption when dropped unfinished; finishing commits.
    let s = c.open_stream(scope.clone(), MessageRole::Assistant, None).unwrap();
    s.push("Hel".into()).unwrap();
    s.push("lo".into()).unwrap();
    let mid = s.finish(None).unwrap();
    assert_eq!(c.message(mid).unwrap().unwrap().content, "Hello");
    assert!(s.push("more".into()).is_err());

    sub.cancel();
    block_on(c.close()).unwrap();
}

#[test]
fn pairing_sync_and_streams_over_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let mac = SyncCore::open(config(dir.path(), "mac", Platform::MacOs, true), Box::new(Keys::default())).unwrap();
    let phone_keys = Keys::default();
    let phone = SyncCore::open(config(dir.path(), "phone", Platform::Ios, true), Box::new(phone_keys.clone())).unwrap();
    let phone_events = Events::default();
    phone.set_listener(Some(Box::new(phone_events.clone())));

    let scope = mac.create_conversation("Shared".into()).unwrap();
    mac.send_message(scope.clone(), text("before pairing")).unwrap();
    mac.add_peer_address(phone.local_address().unwrap()).unwrap();
    phone.add_peer_address(mac.local_address().unwrap()).unwrap();

    let offer = mac.pairing_offer(300_000).unwrap();
    assert_eq!(offer.code.len(), 14);
    block_on(phone.pair_with_offer(offer.payload)).unwrap();
    until("paired", || phone_events.0.lock().unwrap().iter().any(|e| matches!(e, CoreEvent::Paired { .. })));
    until("history", || contents(&phone, &scope) == vec!["before pairing"]);
    assert_eq!(phone.user_key().unwrap(), mac.user_key().unwrap());

    let log = ScopeLog::default();
    let _sub = phone.subscribe(scope.clone(), Box::new(log.clone())).unwrap();
    let s = mac.open_stream(scope.clone(), MessageRole::Assistant, None).unwrap();
    s.push("Thinking".into()).unwrap();
    until("stream tokens", || log.0.lock().unwrap().iter().any(|c| matches!(c, ScopeChange::Stream { update } if update.text == "Thinking")));
    s.finish(Some("Done thinking".into())).unwrap();
    until("committed", || contents(&phone, &scope).contains(&"Done thinking".to_string()));

    let d = mac.diagnostics();
    assert!(d.peers.iter().any(|p| p.device == phone.device_key() && p.connected && !p.relayed));
    until("acknowledged", || mac.sync_status(scope.clone()).unwrap().pending_outbox == 0);

    // iOS lifecycle: writes while suspended survive a suspend, a kill and a reopen, and sync
    // once the app is foregrounded again.
    block_on(phone.suspend()).unwrap();
    assert!(!phone.is_online());
    let id = phone.send_message(scope.clone(), text("offline on the phone")).unwrap();
    phone.add_reaction(id.clone(), "🔥".into()).unwrap();
    block_on(phone.suspend()).unwrap();
    let killed = dir.path().join("phone-after-kill");
    copy_dir(&dir.path().join("phone"), &killed);
    let mut cfg = config(dir.path(), "phone-after-kill", Platform::Ios, false);
    cfg.device_name = "phone".into();
    let reopened = SyncCore::open(cfg, Box::new(phone_keys.clone())).unwrap();
    let m = reopened.message(id.clone()).unwrap().unwrap();
    assert_eq!(m.delivery, Delivery::Pending);
    assert_eq!(m.reactions[0].value, "🔥");
    assert!(reopened.sync_status(scope.clone()).unwrap().pending_outbox >= 2);
    block_on(reopened.close()).unwrap();

    block_on(phone.resume()).unwrap();
    phone.add_peer_address(mac.local_address().unwrap()).unwrap();
    mac.add_peer_address(phone.local_address().unwrap()).unwrap();
    phone.sync_now();
    until("catch-up after resume", || contents(&mac, &scope).contains(&"offline on the phone".to_string()));

    block_on(mac.close()).unwrap();
    block_on(phone.close()).unwrap();
}

#[test]
fn rotated_identity_reaches_the_other_device() {
    let dir = tempfile::tempdir().unwrap();
    let mac = SyncCore::open(config(dir.path(), "mac", Platform::MacOs, true), Box::new(Keys::default())).unwrap();
    let pc = SyncCore::open(config(dir.path(), "pc", Platform::Windows, true), Box::new(Keys::default())).unwrap();
    let events = Events::default();
    pc.set_listener(Some(Box::new(events.clone())));
    let scope = mac.create_conversation("Notes".into()).unwrap();
    mac.send_message(scope.clone(), text("before")).unwrap();
    mac.add_peer_address(pc.local_address().unwrap()).unwrap();
    pc.add_peer_address(mac.local_address().unwrap()).unwrap();
    let offer = mac.pairing_offer(300_000).unwrap();
    block_on(pc.pair_with_offer(offer.payload)).unwrap();
    until("history", || contents(&pc, &scope) == vec!["before"]);
    let (old_user, old_phrase) = (mac.user_key().unwrap(), mac.recovery_phrase().unwrap());
    events.0.lock().unwrap().clear();

    let report = mac.revoke_and_rotate(vec![]).unwrap();
    assert_eq!(report.rotated, vec![scope.clone()]);
    assert_ne!(report.user, old_user);
    assert_eq!(mac.user_key().unwrap(), report.user);
    assert_ne!(mac.recovery_phrase().unwrap(), old_phrase);
    until("handoff", || pc.user_key().unwrap() == report.user);
    until("identity event", || events.0.lock().unwrap().iter().any(|e| matches!(e, CoreEvent::IdentityChanged)));
    mac.send_message(scope.clone(), text("after")).unwrap();
    until("new epoch", || contents(&pc, &scope) == vec!["before", "after"]);
    assert!(matches!(pc.recover_from_phrase(old_phrase.join(" ")), Err(SyncError::InvalidInput { .. })));

    block_on(mac.close()).unwrap();
    block_on(pc.close()).unwrap();
}

#[test]
fn restored_backup_unlocks_with_the_phrase() {
    let dir = tempfile::tempdir().unwrap();
    let old = SyncCore::open(config(dir.path(), "old", Platform::MacOs, false), Box::new(Keys::default())).unwrap();
    let scope = old.create_conversation("Kept".into()).unwrap();
    old.send_message(scope.clone(), text("from the old Mac")).unwrap();
    let phrase = old.recovery_phrase().unwrap().join(" ");
    let user = old.user_key().unwrap();
    block_on(old.close()).unwrap();

    // The backup lands on new hardware: same files, empty key store.
    copy_dir(&dir.path().join("old"), &dir.path().join("new"));
    let new = SyncCore::open(config(dir.path(), "new", Platform::MacOs, false), Box::new(Keys::default())).unwrap();
    let events = Events::default();
    new.set_listener(Some(Box::new(events.clone())));
    assert!(new.recovery_available().unwrap());
    assert!(new.conversations().unwrap().is_empty());

    new.recover_from_phrase(phrase).unwrap();
    until("identity", || events.0.lock().unwrap().iter().any(|e| matches!(e, CoreEvent::IdentityChanged)));
    assert!(!new.recovery_available().unwrap());
    assert_eq!(new.user_key().unwrap(), user);
    assert_eq!(new.conversations().unwrap().len(), 1);
    new.send_message(scope.clone(), text("from the new Mac")).unwrap();
    assert_eq!(contents(&new, &scope), ["from the old Mac", "from the new Mac"]);
    block_on(new.close()).unwrap();
}

#[test]
fn network_configs() {
    let d = default_network_config();
    assert!(d.relays.is_empty() && d.discovery_server.is_none() && d.mdns);
    assert_eq!(d.publish, AddressPublishing::RelayOnly);
    let n0 = n0_public_network_config();
    assert!(!n0.relays.is_empty() && n0.discovery_server.is_some() && n0.discovery_origin.is_some());

    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = config(tmp.path(), "Mac", Platform::MacOs, false);
    cfg.network.discovery_server = Some("https://dns.example.com/pkarr".into());
    let err = SyncCore::open(cfg, Box::new(Keys::default())).err().expect("half a discovery config is refused");
    assert!(matches!(err, SyncError::InvalidInput { .. }));
}

#[test]
fn data_management() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = Keys::default();
    let c = SyncCore::open(config(tmp.path(), "Mac", Platform::MacOs, false), Box::new(keys.clone())).unwrap();
    let keep = c.create_conversation("Keep".into()).unwrap();
    let file = tmp.path().join("notes.txt");
    std::fs::write(&file, b"attached").unwrap();
    let mut m = text("hello");
    m.attachments.push(AttachmentFile { path: file.to_string_lossy().into_owned(), media_type: "text/plain".into(), name: "notes.txt".into() });
    c.send_message(keep.clone(), m).unwrap();

    let usage = c.storage_usage().unwrap();
    assert!(usage.database_bytes > 0 && usage.attachment_bytes >= 8);

    let out = tmp.path().join("export");
    let summary = block_on(c.export_conversation(keep.clone(), out.to_string_lossy().into_owned())).unwrap();
    assert_eq!(summary, ExportSummary { messages: 1, attachments: 1, missing_attachments: 0 });
    assert!(out.join("conversation.json").exists());

    let gone = c.create_conversation("Leave".into()).unwrap();
    c.leave_conversation(gone.clone()).unwrap();
    assert_eq!(c.conversations().unwrap().iter().map(|x| x.scope.clone()).collect::<Vec<_>>(), [keep]);
    assert!(c.support_bundle().contains("\"database_bytes\""));

    assert!(!keys.0.lock().unwrap().is_empty());
    block_on(c.reset()).unwrap();
    assert!(keys.0.lock().unwrap().is_empty());
    assert!(!tmp.path().join("Mac").exists());
    assert!(matches!(c.conversations(), Err(SyncError::Closed { .. })));
    // A fresh core starts empty.
    let c = SyncCore::open(config(tmp.path(), "Mac", Platform::MacOs, false), Box::new(keys)).unwrap();
    assert!(c.conversations().unwrap().is_empty());
    block_on(c.close()).unwrap();
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        if name == ".lock" {
            continue;
        }
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(name));
        } else {
            std::fs::copy(e.path(), to.join(name)).unwrap();
        }
    }
}
