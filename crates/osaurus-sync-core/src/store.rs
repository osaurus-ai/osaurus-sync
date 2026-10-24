//! SQLite schema. `events` is the source of truth; every view table is a cache rebuilt from it
//! when `VIEW_VERSION` changes.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

pub const STORE_VERSION: i64 = 1;
pub const VIEW_VERSION: i64 = 1;

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

const BASE_SCHEMA: &str = r#"
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

-- Scopes this device left. Kept after the scope is purged so offers don't bring it back; an
-- invite clears it.
CREATE TABLE IF NOT EXISTS left_scopes (scope BLOB PRIMARY KEY, left_ms INTEGER NOT NULL);

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
    my_role INTEGER,
    members BLOB
);

CREATE TABLE IF NOT EXISTS pending_decrypt (
    hash BLOB PRIMARY KEY,
    scope BLOB NOT NULL,
    epoch INTEGER NOT NULL
);
"#;

const VIEW_TABLES: &[&str] =
    &["messages_view", "messages_fts", "edits", "tombstones", "reactions", "reaction_removed", "conversations_view", "pending_decrypt"];

pub fn open(path: Option<&std::path::Path>) -> Result<Connection> {
    let conn = match path {
        Some(p) => Connection::open(p)?,
        None => Connection::open_in_memory()?,
    };
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// Creates tables. Returns true when view tables were (re)created and must be rebuilt.
pub fn migrate(conn: &Connection) -> Result<bool> {
    let had_held = has_table(conn, "held")?;
    let had_needs = has_table(conn, "held_needs")?;
    if had_held && !has_column(conn, "held", "peer")? {
        conn.execute_batch("ALTER TABLE held ADD COLUMN peer BLOB;")?;
    }
    conn.execute_batch(BASE_SCHEMA)?;
    if !has_column(conn, "scopes", "tentative")? {
        conn.execute_batch("ALTER TABLE scopes ADD COLUMN tentative INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !had_needs {
        // Held rows from before the index can't be released; peers send them again.
        conn.execute_batch("DELETE FROM held;")?;
    }
    let view_version: Option<i64> = meta_i64(conn, "view_version")?;
    let rebuild = view_version != Some(VIEW_VERSION);
    if rebuild {
        for t in VIEW_TABLES {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {t};"))?;
        }
    }
    conn.execute_batch(VIEW_SCHEMA)?;
    set_meta(conn, "store_version", &STORE_VERSION.to_le_bytes())?;
    set_meta(conn, "view_version", &VIEW_VERSION.to_le_bytes())?;
    let had_events: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM events)", [], |r| r.get(0))?;
    Ok(rebuild && had_events)
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
