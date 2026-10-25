//! SQLite schema. `events` is the source of truth; every view table is a cache rebuilt from it
//! when `VIEW_VERSION` changes.
//!
//! Base tables can't be rebuilt, so they change only through numbered migrations: version 1 is
//! `BASE_SCHEMA_V1`, and `MIGRATIONS[i]` takes a database from version `i + 1` to `i + 2`. A
//! new database runs the same steps as an upgraded one. Never edit a shipped step; append one,
//! bump `STORE_VERSION`, and freeze a fixture of the old version for `tests/upgrade.rs`.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

pub const STORE_VERSION: i64 = 1 + MIGRATIONS.len() as i64;
pub const VIEW_VERSION: i64 = 2;

/// `scopes.replication`.
pub mod replication {
    pub const SYNCED: i64 = 0;
    /// Never offered to another device; its key is sealed only to this device and the recovery key.
    pub const DEVICE_LOCAL: i64 = 1;
}

pub mod status {
    pub const VALID: i64 = 0;
    pub const REJECTED: i64 = 1;
    /// Unknown kind or envelope version: stored and forwarded, never applied.
    pub const UNSUPPORTED: i64 = 2;
}

/// `events.purged`: why an event's payload was removed. The envelope still verifies.
pub mod purged {
    pub const NONE: i64 = 0;
    /// Tombstoned, or received without a payload.
    pub const DELETED: i64 = 1;
    /// Superseded and dropped once every storing peer had it.
    pub const COMPACTED: i64 = 2;
}

pub mod delivery {
    pub const DELIVERED: i64 = 0;
    pub const PENDING: i64 = 1;
    pub const REJECTED: i64 = 2;
}

const BASE_SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);

CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    hash BLOB NOT NULL UNIQUE,
    id BLOB NOT NULL,
    scope BLOB NOT NULL,
    author BLOB,
    hlc INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    version INTEGER NOT NULL,
    epoch INTEGER NOT NULL,
    control INTEGER NOT NULL,
    status INTEGER NOT NULL,
    reason INTEGER,
    purged INTEGER NOT NULL DEFAULT 0,
    bytes BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS events_scope_order ON events(scope, control, hlc, id);
CREATE INDEX IF NOT EXISTS events_id ON events(id);
CREATE INDEX IF NOT EXISTS events_author ON events(scope, author, hlc);

CREATE TABLE IF NOT EXISTS event_parents (
    child BLOB NOT NULL,
    parent BLOB NOT NULL,
    control INTEGER NOT NULL,
    PRIMARY KEY (child, parent)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS event_parents_parent ON event_parents(parent);

CREATE TABLE IF NOT EXISTS heads (
    scope BLOB NOT NULL,
    layer INTEGER NOT NULL,
    hash BLOB NOT NULL,
    PRIMARY KEY (scope, layer, hash)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS held (
    hash BLOB PRIMARY KEY,
    scope BLOB NOT NULL,
    bytes BLOB NOT NULL,
    received_ms INTEGER NOT NULL,
    -- The peer that sent it, for per-peer limits. NULL when unknown.
    peer BLOB
);
CREATE INDEX IF NOT EXISTS held_scope ON held(scope);
CREATE INDEX IF NOT EXISTS held_peer ON held(peer);

-- The control parents each held event is waiting for. A stored event releases only its waiters.
CREATE TABLE IF NOT EXISTS held_needs (
    missing BLOB NOT NULL,
    hash BLOB NOT NULL,
    PRIMARY KEY (missing, hash)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS held_needs_hash ON held_needs(hash);

CREATE TABLE IF NOT EXISTS scopes (
    scope BLOB PRIMARY KEY,
    history_mode INTEGER NOT NULL,
    window_days INTEGER NOT NULL,
    storage_role INTEGER NOT NULL DEFAULT 0,
    synced_from_hlc INTEGER,
    last_sync_ms INTEGER,
    joined_ms INTEGER NOT NULL,
    -- Joined from a peer's offer and kept only if its control layer lists this user.
    tentative INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS outbox (
    hash BLOB PRIMARY KEY,
    scope BLOB NOT NULL,
    id BLOB NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS outbox_scope ON outbox(scope);

CREATE TABLE IF NOT EXISTS peers (
    device BLOB NOT NULL,
    scope BLOB NOT NULL,
    last_contact_ms INTEGER NOT NULL,
    PRIMARY KEY (device, scope)
) WITHOUT ROWID;

-- Peers with the storage role, and the local seq up to which each holds this scope's events.
CREATE TABLE IF NOT EXISTS storage_peers (
    device BLOB NOT NULL,
    scope BLOB NOT NULL,
    seen_seq INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (device, scope)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS known_devices (
    device BLOB NOT NULL,
    user BLOB NOT NULL,
    name TEXT NOT NULL,
    platform TEXT NOT NULL,
    cert BLOB NOT NULL,
    last_seen_ms INTEGER,
    PRIMARY KEY (device, user)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS revoked_devices (device BLOB PRIMARY KEY, revoked_ms INTEGER NOT NULL);

-- User identities this device has rotated away from. Their certificates are no longer trusted.
CREATE TABLE IF NOT EXISTS retired_users (user BLOB PRIMARY KEY, successor BLOB NOT NULL, retired_ms INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS blobs (
    hash BLOB PRIMARY KEY,
    size INTEGER NOT NULL,
    media_type TEXT NOT NULL,
    available INTEGER NOT NULL,
    fetch_state INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS blob_refs (
    hash BLOB NOT NULL,
    scope BLOB NOT NULL,
    event BLOB NOT NULL,
    PRIMARY KEY (hash, event)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS loro_snapshots (
    scope BLOB PRIMARY KEY,
    snapshot BLOB NOT NULL,
    upto_seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS read_state (scope BLOB PRIMARY KEY, last_read_hlc INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS invites_issued (
    token_hash BLOB PRIMARY KEY,
    scope BLOB NOT NULL,
    token BLOB NOT NULL,
    role INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL
);
"#;

const MIGRATIONS: &[&str] = &[
    // 2: scopes this device left. Kept after the scope is purged so offers don't bring it back;
    // an invite clears it.
    "CREATE TABLE left_scopes (scope BLOB PRIMARY KEY, left_ms INTEGER NOT NULL);",
    // 3: the only user an issued invite admits, when it was created for one.
    "ALTER TABLE invites_issued ADD COLUMN for_user BLOB;",
    // 4: device-local scopes.
    "ALTER TABLE scopes ADD COLUMN replication INTEGER NOT NULL DEFAULT 0;",
];

const VIEW_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS messages_view (
    id BLOB PRIMARY KEY,
    scope BLOB NOT NULL,
    hash BLOB NOT NULL,
    hlc INTEGER NOT NULL,
    author_device BLOB NOT NULL,
    principal TEXT NOT NULL,
    is_agent INTEGER NOT NULL,
    role INTEGER NOT NULL,
    content TEXT NOT NULL,
    blobs BLOB NOT NULL,
    reply_to BLOB,
    tool_call_id TEXT,
    tool_name TEXT,
    edited INTEGER NOT NULL DEFAULT 0,
    edit_hlc INTEGER,
    edit_author BLOB,
    status INTEGER NOT NULL,
    reason INTEGER
);
CREATE INDEX IF NOT EXISTS messages_view_order ON messages_view(scope, hlc, author_device, id);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content, tokenize = 'unicode61 remove_diacritics 2');

CREATE TABLE IF NOT EXISTS edits (
    hash BLOB PRIMARY KEY,
    scope BLOB NOT NULL,
    target BLOB NOT NULL,
    hlc INTEGER NOT NULL,
    author_device BLOB NOT NULL,
    principal TEXT NOT NULL,
    content TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS edits_target ON edits(target);

CREATE TABLE IF NOT EXISTS tombstones (
    target BLOB NOT NULL,
    hash BLOB NOT NULL,
    scope BLOB NOT NULL,
    principal TEXT NOT NULL,
    admin INTEGER NOT NULL,
    PRIMARY KEY (target, hash)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS reactions (
    add_id BLOB PRIMARY KEY,
    scope BLOB NOT NULL,
    target BLOB NOT NULL,
    reaction TEXT NOT NULL,
    principal TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS reactions_target ON reactions(target);
CREATE TABLE IF NOT EXISTS reaction_removed (
    add_id BLOB NOT NULL,
    principal TEXT NOT NULL,
    scope BLOB NOT NULL,
    PRIMARY KEY (add_id, principal)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS conversations_view (
    scope BLOB PRIMARY KEY,
    kind INTEGER NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    pinned INTEGER NOT NULL DEFAULT 0,
    sort_order REAL,
    archived INTEGER NOT NULL DEFAULT 0,
    created_hlc INTEGER NOT NULL DEFAULT 0,
    created_ms INTEGER NOT NULL DEFAULT 0,
    my_role INTEGER,
    members BLOB,
    app_data BLOB
);

CREATE TABLE IF NOT EXISTS conversation_tags (
    scope BLOB NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (scope, key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS conversation_tags_value ON conversation_tags(key, value);

CREATE TABLE IF NOT EXISTS app_data_view (
    target BLOB NOT NULL,
    namespace TEXT NOT NULL,
    principal TEXT NOT NULL,
    scope BLOB NOT NULL,
    hash BLOB NOT NULL,
    hlc INTEGER NOT NULL,
    author_device BLOB NOT NULL,
    bytes BLOB NOT NULL,
    blobs BLOB NOT NULL,
    PRIMARY KEY (target, namespace, principal)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS app_data_scope ON app_data_view(scope);
CREATE TABLE IF NOT EXISTS app_data_events (
    hash BLOB PRIMARY KEY,
    target BLOB NOT NULL,
    scope BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS app_data_events_target ON app_data_events(target);
CREATE INDEX IF NOT EXISTS app_data_events_scope ON app_data_events(scope);

CREATE TABLE IF NOT EXISTS pending_decrypt (
    hash BLOB PRIMARY KEY,
    scope BLOB NOT NULL,
    epoch INTEGER NOT NULL
);
"#;

const VIEW_TABLES: &[&str] = &[
    "messages_view",
    "messages_fts",
    "edits",
    "tombstones",
    "reactions",
    "reaction_removed",
    "conversations_view",
    "conversation_tags",
    "app_data_view",
    "app_data_events",
    "pending_decrypt",
];

/// The key a database file is encrypted with (SQLCipher, raw 256-bit key).
#[derive(Clone)]
pub struct DbKey(pub [u8; 32]);

impl std::fmt::Debug for DbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbKey(..)")
    }
}

impl DbKey {
    fn pragma(&self) -> String {
        let hex: String = self.0.iter().map(|b| format!("{b:02x}")).collect();
        format!("\"x'{hex}'\"")
    }
}

/// Opens a database. With a key, the file is encrypted at rest and every page is authenticated:
/// a wrong key fails here with `Crypto` rather than on first use.
pub fn open(path: Option<&std::path::Path>, key: Option<&DbKey>) -> Result<Connection> {
    let conn = match path {
        Some(p) => Connection::open(p)?,
        None => Connection::open_in_memory()?,
    };
    if let Some(k) = key {
        apply_key(&conn, k)?;
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// Keys a fresh connection and checks the key against the file.
pub fn apply_key(conn: &Connection, key: &DbKey) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA key = {};", key.pragma()))?;
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::NotADatabase => {
            Err(Error::Crypto("the database key doesn't match this database".into()))
        }
        Err(e) => Err(e.into()),
    }
}

/// Whether `path` holds an unencrypted SQLite database (one from before encryption at rest).
pub fn is_plaintext(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 16];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"SQLite format 3\0"
}

/// Encrypts an unencrypted database in place. The encrypted copy is written next to it and
/// renamed over it, so an interruption leaves the plaintext original intact.
pub fn encrypt_in_place(path: &std::path::Path, key: &DbKey) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    export_over(conn, path, "db.encrypting", key)
}

/// Copies the tables in `conn`'s schema into a fresh file keyed with `key`, then renames it
/// over `path`. Pages no table reaches are never read.
fn export_over(conn: Connection, path: &std::path::Path, ext: &str, key: &DbKey) -> Result<()> {
    let tmp = path.with_extension(ext);
    let _ = std::fs::remove_file(&tmp);
    conn.execute("ATTACH DATABASE ?1 AS fresh KEY ?2", params![tmp.to_string_lossy(), format!("x'{}'", hex_of(&key.0))])?;
    conn.query_row("SELECT sqlcipher_export('fresh')", [], |_| Ok(()))?;
    conn.execute_batch("DETACH DATABASE fresh;")?;
    drop(conn);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(sidecar(path, suffix));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Re-encrypts a database under a new key, in place.
pub fn rekey(conn: &Connection, key: &DbKey) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA rekey = {};", key.pragma()))?;
    Ok(())
}

fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn sidecar(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// The base-table version of a database, if it has one.
pub fn store_version(conn: &Connection) -> Result<Option<i64>> {
    if !has_table(conn, "meta")? {
        return Ok(None);
    }
    meta_i64(conn, "store_version")
}

/// Creates or upgrades tables. Returns true when view tables were (re)created and must be
/// rebuilt.
pub fn migrate(conn: &Connection) -> Result<bool> {
    let found = store_version(conn)?.unwrap_or(0);
    if found > STORE_VERSION {
        return Err(Error::NewerDatabase { found, supported: STORE_VERSION });
    }
    if found <= 1 {
        let had_held = has_table(conn, "held")?;
        let had_needs = has_table(conn, "held_needs")?;
        // Development databases from before version 1 was frozen.
        if had_held && !has_column(conn, "held", "peer")? {
            conn.execute_batch("ALTER TABLE held ADD COLUMN peer BLOB;")?;
        }
        conn.execute_batch(BASE_SCHEMA_V1)?;
        if !has_column(conn, "scopes", "tentative")? {
            conn.execute_batch("ALTER TABLE scopes ADD COLUMN tentative INTEGER NOT NULL DEFAULT 0;")?;
        }
        if !had_needs {
            // Held rows from before the index can't be released; peers send them again.
            conn.execute_batch("DELETE FROM held;")?;
        }
        set_meta(conn, "store_version", &1i64.to_le_bytes())?;
    }
    for (i, step) in MIGRATIONS.iter().enumerate() {
        let to = i as i64 + 2;
        if found >= to {
            continue;
        }
        // Each step commits with its version, so an interrupted upgrade resumes where it stopped.
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let res = conn.execute_batch(step).and_then(|()| {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES ('store_version', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [&to.to_le_bytes()[..]],
            )
        });
        match res {
            Ok(_) => conn.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
    }
    let view_version: Option<i64> = meta_i64(conn, "view_version")?;
    let rebuild = view_version != Some(VIEW_VERSION);
    if rebuild {
        for t in VIEW_TABLES {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {t};"))?;
        }
    }
    conn.execute_batch(VIEW_SCHEMA)?;
    set_meta(conn, "view_version", &VIEW_VERSION.to_le_bytes())?;
    let had_events: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM events)", [], |r| r.get(0))?;
    Ok(rebuild && had_events)
}

/// Checks a database that may have been left mid-write. A damaged one is repaired when the
/// damage is confined to view tables, which are caches: they're unlinked from the schema
/// without reading their pages, the base tables are exported into a fresh file, and
/// `migrate` rebuilds the views from `events`. Damage anywhere else is `Corrupt`. Returns the
/// connection to use from then on.
pub fn check_and_repair(conn: Connection, path: &std::path::Path, key: &DbKey) -> Result<Connection> {
    // SQLCipher fails the check outright (as out of memory) on a page that doesn't authenticate.
    let problems = integrity_problems(&conn).unwrap_or_else(|e| vec![e.to_string()]);
    if problems.is_empty() {
        return Ok(conn);
    }
    tracing::warn!(?problems, "database failed its integrity check; rebuilding views");
    // A page that failed to decrypt leaves the connection's pager in an error state.
    drop(conn);
    let conn = open(Some(path), Some(key))?;
    let mut names: Vec<String> = VIEW_TABLES.iter().map(|t| format!("'{t}'")).collect();
    names.push("'messages_fts_data'".into());
    names.extend(["idx", "content", "docsize", "config"].iter().map(|s| format!("'messages_fts_{s}'")));
    conn.execute_batch(&format!(
        "PRAGMA writable_schema = ON; DELETE FROM sqlite_master WHERE tbl_name IN ({}); PRAGMA writable_schema = OFF;",
        names.join(", ")
    ))?;
    drop(conn);
    // A fresh connection, so the schema without the views is the one in use. (SQLCipher's
    // VACUUM refuses a file with a page that doesn't authenticate, even an unreachable one.)
    let conn = open(Some(path), Some(key))?;
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    export_over(conn, path, "db.repairing", key)?;
    let conn = open(Some(path), Some(key))?;
    let left = integrity_problems(&conn)?;
    if !left.is_empty() {
        return Err(Error::Corrupt(left.join("; ")));
    }
    set_meta(&conn, "view_version", &0i64.to_le_bytes())?;
    Ok(conn)
}

fn integrity_problems(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA integrity_check(20)")?;
    let rows: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
    Ok(if rows.len() == 1 && rows[0] == "ok" { Vec::new() } else { rows })
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)", [table], |r| r.get(0))?)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt.query_map([], |r| r.get(1))?.collect::<std::result::Result<_, _>>()?;
    Ok(names.iter().any(|n| n == column))
}

pub fn meta(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0)).optional()?)
}

pub fn set_meta(conn: &Connection, key: &str, value: &[u8]) -> Result<()> {
    conn.execute("INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
    Ok(())
}

pub fn meta_i64(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(meta(conn, key)?.and_then(|v| v.try_into().ok()).map(i64::from_le_bytes))
}
