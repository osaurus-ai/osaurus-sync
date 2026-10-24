//! Opening databases written by earlier releases.
//!
//! `tests/fixtures/store-v<N>` holds a data directory written by the last commit at store
//! version N, with the key store entries it needs under `secrets/`. To freeze one before
//! bumping `STORE_VERSION`, run the generator on the old commit:
//!
//! ```text
//! OSAURUS_FIXTURE_OUT=/tmp/store-vN cargo test -p osaurus-sync-core --test upgrade write_fixture -- --ignored
//! ```

use std::path::Path;
use std::sync::Arc;

use osaurus_sync_core::env::SystemEnv;
use osaurus_sync_core::error::Error;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::Role;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::MemorySecretStore;
use osaurus_sync_core::store;
use osaurus_sync_core::types::{Attachment, NewMessage};
use osaurus_sync_core::{Replica, ReplicaConfig, SecretStore};

const SECRETS: &[&str] = &["device-key", "user-secret"];
const ATTACHMENT: &[u8] = b"fixture attachment bytes";

fn text(s: &str) -> NewMessage {
    NewMessage::text(MessageRole::User, s)
}

#[test]
#[ignore = "run on an old commit to freeze a fixture"]
fn write_fixture() {
    let out = std::path::PathBuf::from(std::env::var("OSAURUS_FIXTURE_OUT").expect("set OSAURUS_FIXTURE_OUT"));
    let secrets = Arc::new(MemorySecretStore::default());
    let mut r = Replica::open(ReplicaConfig::new(out.join("data"), "Fixture Mac", Platform::MacOs), Arc::new(SystemEnv), secrets.clone()).unwrap();
    let scope = r.create_scope(ScopeKind::Conversation, "Trip").unwrap();
    r.send_message(scope, text("first")).unwrap();
    let second = r.send_message(scope, text("second, before editing")).unwrap();
    r.edit_message(second, "second").unwrap();
    r.add_reaction(second, "👍").unwrap();
    let gone = r.send_message(scope, text("deleted")).unwrap();
    r.delete_message(gone).unwrap();
    let file = out.join("upload.bin");
    std::fs::write(&file, ATTACHMENT).unwrap();
    let mut m = text("with a file");
    m.attachments.push(Attachment { path: file.clone(), media_type: "application/octet-stream".into(), name: "notes.bin".into() });
    r.send_message(scope, m).unwrap();
    r.rename_conversation(scope, "Trip to Lisbon").unwrap();
    r.set_pinned(scope, true).unwrap();
    let ws = r.create_scope(ScopeKind::Workspace, "Projects").unwrap();
    r.send_message(ws, text("workspace note")).unwrap();
    r.close().unwrap();
    std::fs::remove_file(file).unwrap();
    std::fs::create_dir_all(out.join("secrets")).unwrap();
    for name in SECRETS {
        std::fs::write(out.join("secrets").join(name), secrets.get(name).unwrap()).unwrap();
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

fn open_fixture(version: u32) -> (Replica, tempfile::TempDir) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/store-v{version}"));
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&src.join("data"), &tmp.path().join("data"));
    let secrets = Arc::new(MemorySecretStore::default());
    for name in SECRETS {
        secrets.store(name, &std::fs::read(src.join("secrets").join(name)).unwrap()).unwrap();
    }
    let r = Replica::open(ReplicaConfig::new(tmp.path().join("data"), "Fixture Mac", Platform::MacOs), Arc::new(SystemEnv), secrets).unwrap();
    (r, tmp)
}

#[test]
fn opens_a_version_1_database() {
    opens_fixture(1);
}

#[test]
fn opens_a_version_2_database() {
    opens_fixture(2);
}

fn opens_fixture(version: u32) {
    let (mut r, _tmp) = open_fixture(version);
    assert_eq!(store::store_version(r.db_for_tests()).unwrap(), Some(store::STORE_VERSION));
    let list = r.conversations().unwrap();
    assert_eq!(list.len(), 2);
    let trip = list.iter().find(|c| c.title == "Trip to Lisbon").expect("renamed conversation");
    assert!(trip.pinned);
    let page = r.messages(trip.scope, None, 100).unwrap();
    let contents: Vec<&str> = page.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["first", "second", "with a file"]);
    assert!(page.messages[1].edited);
    assert_eq!(page.messages[1].reactions[0].reaction, "👍");
    let blob = &page.messages[2].blobs[0];
    let path = r.blob_path(trip.scope, &blob.hash).unwrap().expect("attachment survives");
    assert_eq!(std::fs::read(path).unwrap(), ATTACHMENT);
    // New writes, and features added since, work on the upgraded database.
    r.send_message(trip.scope, text("after the upgrade")).unwrap();
    let ws = list.iter().find(|c| c.title == "Projects").unwrap().scope;
    r.create_invite_for(ws, Role::Member, r.user_key(), 60_000).unwrap();
    r.leave_scope(ws).unwrap();
    assert!(r.has_left(ws).unwrap());
    r.close().unwrap();
}

#[test]
fn refuses_a_database_from_a_newer_release() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ReplicaConfig::new(tmp.path(), "Mac", Platform::MacOs);
    let secrets = Arc::new(MemorySecretStore::default());
    let mut r = Replica::open(cfg.clone(), Arc::new(SystemEnv), secrets.clone()).unwrap();
    store::set_meta(r.db_for_tests(), "store_version", &(store::STORE_VERSION + 1).to_le_bytes()).unwrap();
    r.close().unwrap();
    drop(r);
    let err = Replica::open(cfg, Arc::new(SystemEnv), secrets).err().expect("newer database is refused");
    assert_eq!(err, Error::NewerDatabase { found: store::STORE_VERSION + 1, supported: store::STORE_VERSION });
}

#[test]
fn a_damaged_view_is_rebuilt_after_a_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ReplicaConfig::new(tmp.path(), "Mac", Platform::MacOs);
    let secrets = Arc::new(MemorySecretStore::default());
    let mut r = Replica::open(cfg.clone(), Arc::new(SystemEnv), secrets.clone()).unwrap();
    let scope = r.create_scope(ScopeKind::Conversation, "Notes").unwrap();
    for i in 0..300 {
        r.send_message(scope, text(&format!("message number {i} with enough text to fill several pages"))).unwrap();
    }
    let root: i64 = r.db_for_tests().query_row("SELECT rootpage FROM sqlite_master WHERE type = 'table' AND name = 'messages_view'", [], |x| x.get(0)).unwrap();
    let page_size: i64 = r.db_for_tests().query_row("PRAGMA page_size", [], |x| x.get(0)).unwrap();
    r.close().unwrap();
    drop(r);

    // Scribble over the cells (not the header) of the view's root page, as a torn write might.
    let db = tmp.path().join("sync.db");
    let mut bytes = std::fs::read(&db).unwrap();
    let start = ((root - 1) * page_size) as usize;
    for b in &mut bytes[start + page_size as usize - 512..start + page_size as usize] {
        *b = 0xA5;
    }
    std::fs::write(&db, bytes).unwrap();
    // A run that didn't shut down cleanly leaves the marker behind.
    std::fs::write(tmp.path().join(".unclean"), b"").unwrap();

    let r = Replica::open(cfg, Arc::new(SystemEnv), secrets).unwrap();
    assert_eq!(r.messages(scope, None, 1_000).unwrap().messages.len(), 300);
}

#[test]
fn an_unreadable_file_is_reported_as_corrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ReplicaConfig::new(tmp.path(), "Mac", Platform::MacOs);
    let secrets = Arc::new(MemorySecretStore::default());
    Replica::open(cfg.clone(), Arc::new(SystemEnv), secrets.clone()).unwrap().close().unwrap();
    std::fs::write(tmp.path().join("sync.db"), vec![0x5A; 8192]).unwrap();
    let err = Replica::open(cfg, Arc::new(SystemEnv), secrets).err().unwrap();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
}
