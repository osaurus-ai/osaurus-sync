//! Compatibility fixtures: signed events checked into `tests/fixtures`. Every build must keep
//! decoding, verifying and applying them, including unknown kinds and newer envelope versions.
//!
//! Regenerate with `REGENERATE_FIXTURES=1 cargo test -p osaurus-sync-sim --test fixtures`. Only do
//! that for a deliberate format change; old fixtures should normally stay as they are.

use std::path::PathBuf;
use std::sync::Arc;

use osaurus_sync_core::env::TestEnv;
use osaurus_sync_core::envelope::{Decoded, Draft, Envelope};
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::{EventId, ScopeId};
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::{IngestOutcome, MemorySecretStore};
use osaurus_sync_core::types::NewMessage;
use osaurus_sync_core::{Replica, ReplicaConfig, SecretStore};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

const T0: u64 = 1_750_000_000_000;

fn generate() {
    let env = Arc::new(TestEnv::new(2024, T0));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let mut r = Replica::open(ReplicaConfig::new(tmp.path(), "Fixture Mac", Platform::MacOs), env.clone(), secrets.clone()).unwrap();
    let scope = r.create_scope(ScopeKind::Conversation, "Fixtures").unwrap();
    env.advance(1000);
    let hello = r.send_message(scope, NewMessage::text(MessageRole::User, "hello from v1")).unwrap();
    env.advance(1000);
    r.send_message(scope, NewMessage::text(MessageRole::Assistant, "an answer")).unwrap();
    env.advance(1000);
    r.edit_message(hello, "hello from v1, edited").unwrap();
    r.add_reaction(hello, "👍").unwrap();
    env.advance(1000);
    let gone = r.send_message(scope, NewMessage::text(MessageRole::User, "to be deleted")).unwrap();
    r.delete_message(gone).unwrap();
    r.set_pinned(scope, true).unwrap();
    env.advance(1000);

    let ids = |control| r.recon_keys(scope, control, 0, u64::MAX).unwrap().into_iter().map(|k| k.1).collect::<Vec<_>>();
    let mut events = r.events_by_ids(scope, &ids(true)).unwrap();
    events.extend(r.events_by_ids(scope, &ids(false)).unwrap());

    // Kinds this version does not know, signed like any other event.
    let (control_parents, parents) = r.scope_heads(scope).unwrap();
    let unknown = |kind: u16, n: u8| {
        let e = Envelope::sign(
            Draft {
                id: EventId::new_v7(env.now_ms_pub(), [n; 10]),
                scope,
                hlc: osaurus_sync_core::hlc::pack(T0 + 10_000 + n as u64, 0),
                control_parents: control_parents.clone(),
                parents: parents.clone(),
                kind,
                epoch: 1,
                payload: vec![n; 40],
            },
            r.device_secret(),
        );
        e.encode()
    };
    let unknown_content = unknown(900, 1);
    let unknown_control = unknown(40, 2);

    // A newer envelope version: same leading fields, then fields this version cannot parse.
    let mut signed = Vec::new();
    signed.extend_from_slice(&2u16.to_le_bytes());
    signed.extend_from_slice(&EventId::new_v7(T0 + 20_000, [9; 10]).0);
    signed.extend_from_slice(&scope.0);
    signed.extend_from_slice(b"fields added by envelope version 2");
    let mut future = (signed.len() as u32).to_le_bytes().to_vec();
    future.extend_from_slice(&signed);
    future.push(0);
    future.extend_from_slice(&[0u8; 64]);

    let d = dir();
    std::fs::create_dir_all(&d).unwrap();
    for f in std::fs::read_dir(&d).unwrap() {
        let f = f.unwrap().path();
        if f.extension().is_some_and(|e| e == "bin") {
            std::fs::remove_file(f).unwrap();
        }
    }
    let mut manifest = String::from("# file hash\n");
    let mut write = |name: String, bytes: &[u8]| {
        std::fs::write(d.join(&name), bytes).unwrap();
        manifest.push_str(&format!("{name} {}\n", Envelope::decode(bytes).unwrap().hash().to_hex()));
    };
    for (i, e) in events.iter().enumerate() {
        write(format!("v1-{i:03}.bin"), e);
    }
    write("unknown-content-kind.bin".into(), &unknown_content);
    write("unknown-control-kind.bin".into(), &unknown_control);
    write("future-envelope-version.bin".into(), &future);
    std::fs::write(d.join("manifest.txt"), manifest).unwrap();
    let mut identity = String::new();
    for k in ["device-key", "user-secret"] {
        identity.push_str(&format!("{k} {}\n", hex(&secrets.load(k).unwrap())));
    }
    identity.push_str(&format!("scope {}\n", scope.to_hex()));
    std::fs::write(d.join("identity.txt"), identity).unwrap();
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

trait NowPub {
    fn now_ms_pub(&self) -> u64;
}

impl NowPub for TestEnv {
    fn now_ms_pub(&self) -> u64 {
        osaurus_sync_core::env::Env::now_ms(self)
    }
}

#[test]
fn fixtures_still_decode_and_apply() {
    if std::env::var("REGENERATE_FIXTURES").is_ok() {
        generate();
    }
    let d = dir();
    let manifest = std::fs::read_to_string(d.join("manifest.txt")).expect("fixtures exist; run with REGENERATE_FIXTURES=1 once");
    let identity = std::fs::read_to_string(d.join("identity.txt")).unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let mut scope = None;
    for line in identity.lines() {
        let (k, v) = line.split_once(' ').unwrap();
        if k == "scope" {
            scope = ScopeId::from_hex(v);
        } else {
            secrets.store(k, &unhex(v)).unwrap();
        }
    }
    let scope = scope.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let env = Arc::new(TestEnv::new(1, T0 + 100_000));
    let mut r = Replica::open(ReplicaConfig::new(tmp.path(), "Reader", Platform::MacOs), env, secrets).unwrap();
    let mut files = Vec::new();
    for line in manifest.lines().filter(|l| !l.starts_with('#')) {
        let (name, hash) = line.split_once(' ').unwrap();
        let bytes = std::fs::read(d.join(name)).unwrap();
        let decoded = Envelope::decode(&bytes).unwrap_or_else(|e| panic!("{name} no longer decodes: {e}"));
        assert_eq!(decoded.hash().to_hex(), hash, "{name}: hash changed");
        if let Decoded::Known(e) = &decoded {
            assert!(e.verify_signature(), "{name}: signature no longer verifies");
            assert_eq!(e.encode(), bytes, "{name}: re-encoding changed the bytes");
        }
        files.push((name.to_string(), bytes));
    }
    // Reverse order: everything must still apply once its parents arrive.
    r.join_scope(scope).unwrap();
    for (name, bytes) in files.iter().rev() {
        let out = r.ingest(bytes).unwrap();
        assert!(!matches!(out, IngestOutcome::Invalid(_)), "{name}: {out:?}");
    }
    assert!(r.held_missing(scope).unwrap().is_empty());
    assert_eq!(r.all_hashes(scope).unwrap().len(), files.len());
    let c = r.conversation(scope).unwrap().unwrap();
    assert_eq!(c.title, "Fixtures");
    assert!(c.pinned);
    let msgs: Vec<String> = r.messages(scope, None, 100).unwrap().messages.into_iter().map(|m| m.content).collect();
    assert_eq!(msgs, vec!["hello from v1, edited", "an answer"]);
    let first = &r.messages(scope, None, 100).unwrap().messages[0];
    assert_eq!(first.reactions.len(), 1);
    // Unknown and future events are kept and offered to peers.
    let keys = r.recon_keys(scope, false, 0, u64::MAX).unwrap().len() + r.recon_keys(scope, true, 0, u64::MAX).unwrap().len();
    assert_eq!(keys, files.len());
}
