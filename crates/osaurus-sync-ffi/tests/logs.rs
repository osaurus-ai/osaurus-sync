//! The log listener and the support bundle. In its own test binary because logging is
//! process-wide.

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
struct Logs(Arc<Mutex<Vec<(LogRecord, String)>>>);

impl LogListener for Logs {
    fn on_log(&self, record: LogRecord) {
        let thread = std::thread::current().name().unwrap_or("").to_string();
        self.0.lock().unwrap().push((record, thread));
    }
}

impl Logs {
    fn has(&self, message: &str) -> bool {
        self.0.lock().unwrap().iter().any(|(r, _)| r.message == message)
    }
}

fn config(dir: &std::path::Path, name: &str) -> CoreConfig {
    CoreConfig {
        data_dir: dir.join(name).to_string_lossy().into_owned(),
        device_name: name.into(),
        platform: Platform::MacOs,
        history: None,
        network: NetworkConfig { enabled: true, ..local_network_config() },
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

#[test]
fn logs_reach_the_listener_and_the_support_bundle_is_redacted() {
    let logs = Logs::default();
    set_log_listener(LogLevel::Debug, Some(Box::new(logs.clone())));
    let dir = tempfile::tempdir().unwrap();
    let mac = SyncCore::open(config(dir.path(), "Alice's Mac"), Box::new(Keys::default())).unwrap();
    let pc = SyncCore::open(config(dir.path(), "Alice's PC"), Box::new(Keys::default())).unwrap();
    let scope = mac.create_conversation("Medical notes".into()).unwrap();
    mac.send_message(
        scope.clone(),
        OutgoingMessage {
            role: MessageRole::User,
            content: "private words".into(),
            attachments: vec![],
            reply_to: None,
            as_agent: None,
            tool_call_id: None,
            tool_name: None,
        },
    )
    .unwrap();
    mac.add_peer_address(pc.local_address().unwrap()).unwrap();
    pc.add_peer_address(mac.local_address().unwrap()).unwrap();
    let offer = mac.pairing_offer(300_000).unwrap();
    block_on(pc.pair_with_offer(offer.payload)).unwrap();
    until("history", || pc.messages(scope.clone(), None, 10).unwrap().messages.len() == 1);
    until("session log", || logs.has("session established"));
    until("pairing log", || logs.has("paired a new device"));

    {
        let got = logs.0.lock().unwrap();
        assert!(got.iter().all(|(_, t)| t == "osaurus-sync-logs"));
        let (paired, _) = got.iter().find(|(r, _)| r.message == "paired a new device").unwrap();
        assert_eq!(paired.level, LogLevel::Info);
        assert!(paired.target.starts_with("osaurus_sync_core"));
        assert!(paired.fields.contains_key("peer"));
        // Debug records come only from the sync crates.
        assert!(got.iter().filter(|(r, _)| r.level > LogLevel::Warn).all(|(r, _)| r.target.starts_with("osaurus_sync")));
    }

    let d = mac.diagnostics();
    assert!(d.counters.sessions_established >= 1);
    assert!(d.counters.pairings >= 1);
    assert!(d.counters.bytes_sent > 0 && d.counters.bytes_received > 0);

    let bundle = mac.support_bundle();
    let v: serde_json::Value = serde_json::from_str(&bundle).unwrap();
    assert!(v["counters"]["sessions_established"].as_u64().unwrap() >= 1);
    assert!(v["counters"]["bytes_sent"].as_u64().unwrap() > 0);
    assert!(v["recent_logs"].as_array().unwrap().iter().any(|l| l["message"] == "session established"));
    assert_eq!(v["peers"].as_array().unwrap().len(), 1);
    for secret in [&mac.device_key()[..12], &pc.device_key()[..12], &scope[..12], "private words", "Medical notes", "Alice", "127.0.0.1"] {
        assert!(!bundle.contains(secret), "support bundle contains {secret}");
    }

    // Without a listener nothing is delivered, but recent records are still kept.
    set_log_listener(LogLevel::Info, None);
    let before = logs.0.lock().unwrap().len();
    block_on(pc.close()).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(logs.0.lock().unwrap().len(), before);
    block_on(mac.close()).unwrap();
}
