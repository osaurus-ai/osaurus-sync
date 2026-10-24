//! A device's replica: the SQLite event log, the validator, Loro documents and the views.
//!
//! Every local write and every received event goes through `accept`, so local and remote events
//! are validated and applied by exactly the same code.

mod apply;
mod checkpoint;
mod compact;
mod data;
mod loro_docs;
mod maintain;
mod ops;
mod read;
mod rotate;
mod sync;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::auth::{CtlEvent, Reason, ScopeAuth, Verdict};
use crate::blobs::BlobStore;
use crate::crypto;
use crate::env::{Env, random_u64};
use crate::envelope::{Decoded, Draft, Envelope, kind};
use crate::error::{Error, Result};
use crate::hlc::HlcClock;
use crate::identity::{DeviceCert, Platform};
use crate::ids::{DeviceKey, EventHash, EventId, Principal, Role, ScopeId, UserKey};
use crate::keys::{DeviceSecret, UserSecret};
use crate::payload::{self, Content, Control};
use crate::store::{self, status};
use crate::types::{Change, HistoryMode};

pub use checkpoint::Checkpointer;
pub use compact::{Compaction, PointInTimeView};
pub use data::{ExportSummary, StorageUsage};
pub use loro_docs::LoroCache;
pub use maintain::{Maintenance, PEER_TTL_MS, TENTATIVE_TTL_MS};
pub use ops::InviteToken;
pub use rotate::SUCCESSION_GRACE_MS;
pub use sync::{HELD_MAX_BYTES, HELD_MAX_EVENTS, HELD_TTL_MS, IngestOutcome, ReconKey};

/// Where device and user secrets live: the Keychain on Apple platforms, DPAPI on Windows.
pub trait SecretStore: Send + Sync {
    fn load(&self, name: &str) -> Option<Vec<u8>>;
    fn store(&self, name: &str, value: &[u8]) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

#[derive(Default)]
pub struct MemorySecretStore(Mutex<HashMap<String, Vec<u8>>>);

impl SecretStore for MemorySecretStore {
    fn load(&self, name: &str) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(name).cloned()
    }
    fn store(&self, name: &str, value: &[u8]) -> Result<()> {
        self.0.lock().unwrap().insert(name.into(), value.to_vec());
        Ok(())
    }
    fn delete(&self, name: &str) -> Result<()> {
        self.0.lock().unwrap().remove(name);
        Ok(())
    }
}

/// Present while the replica may have unflushed writes. Found at open, it means the last run
/// didn't shut down cleanly, and the database is checked before use.
const UNCLEAN: &str = ".unclean";
pub const DEVICE_SECRET: &str = "device-key";
pub const USER_SECRET: &str = "user-secret";
/// A rotated user secret, written before the rotation commits and promoted after.
pub const USER_SECRET_NEXT: &str = "user-secret-next";

#[derive(Clone, Debug)]
pub struct ReplicaConfig {
    pub data_dir: PathBuf,
    pub device_name: String,
    pub platform: Platform,
    /// Default history mode for newly joined scopes. `None` picks the platform default.
    pub history_mode: Option<HistoryMode>,
    /// Tool results larger than this are stored as blobs.
    pub tool_output_blob_threshold: usize,
    /// Write a Loro snapshot after this many updates.
    pub snapshot_every: u32,
    /// Keep every scope in full and never compact (Osaurus Server, mailbox peers).
    pub storage_role: bool,
    /// Fold Loro updates into a shallow snapshot once this many can be compacted.
    pub compact_min_updates: u32,
    /// Size cap for decrypted attachments under `blobs/plain`; least recently opened go first.
    pub plain_cache_bytes: u64,
    /// Limits on events waiting for their control parents, per scope and per sending peer.
    pub held_max_events: usize,
    pub held_max_bytes: usize,
}

impl ReplicaConfig {
    pub fn new(data_dir: impl Into<PathBuf>, device_name: impl Into<String>, platform: Platform) -> Self {
        Self {
            data_dir: data_dir.into(),
            device_name: device_name.into(),
            platform,
            history_mode: None,
            tool_output_blob_threshold: 16 * 1024,
            snapshot_every: 500,
            storage_role: false,
            compact_min_updates: 64,
            plain_cache_bytes: 512 << 20,
            held_max_events: sync::HELD_MAX_EVENTS,
            held_max_bytes: sync::HELD_MAX_BYTES,
        }
    }

    pub fn default_history(&self) -> HistoryMode {
        if self.storage_role {
            return HistoryMode::Full;
        }
        self.history_mode.unwrap_or_else(|| HistoryMode::default_for(self.platform))
    }
}

/// Decrypted group keys for one scope, by epoch. Kept in memory only and re-derived from
/// `key_update` events with the device key.
#[derive(Default, Clone)]
pub(crate) struct ScopeKeys {
    by_epoch: BTreeMap<u64, Vec<[u8; 32]>>,
}

impl ScopeKeys {
    fn add(&mut self, epoch: u64, key: [u8; 32]) -> bool {
        let v = self.by_epoch.entry(epoch).or_default();
        if v.contains(&key) {
            return false;
        }
        v.push(key);
        v.sort_by_key(crypto::key_check);
        true
    }

    fn current(&self) -> Option<(u64, [u8; 32])> {
        self.by_epoch.iter().next_back().and_then(|(e, ks)| ks.first().map(|k| (*e, *k)))
    }

    fn find(&self, epoch: u64, check: [u8; 8]) -> Option<[u8; 32]> {
        self.by_epoch.get(&epoch)?.iter().find(|k| crypto::key_check(k) == check).copied()
    }

    fn all(&self) -> impl Iterator<Item = (u64, [u8; 32])> + '_ {
        self.by_epoch.iter().flat_map(|(e, ks)| ks.iter().map(move |k| (*e, *k)))
    }
}

pub struct Replica {
    pub(crate) db: Connection,
    pub(crate) env: Arc<dyn Env>,
    pub(crate) cfg: ReplicaConfig,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) device: DeviceSecret,
    pub(crate) user: UserSecret,
    pub(crate) cert: DeviceCert,
    pub(crate) clock: HlcClock,
    pub(crate) auth: HashMap<ScopeId, ScopeAuth>,
    pub(crate) keys: HashMap<ScopeId, ScopeKeys>,
    /// Keys known from outside the log (a point-in-time view borrows the live replica's keys).
    pub(crate) seeded_keys: HashMap<ScopeId, ScopeKeys>,
    pub(crate) loro: LoroCache,
    pub(crate) blobs: BlobStore,
    /// Number of keys per scope when recovery grants were last checked.
    pub(crate) recovery_checked: HashMap<ScopeId, usize>,
    in_tx: bool,
    changes: Vec<Change>,
    pending_changes: Vec<Change>,
    new_events: Vec<(ScopeId, EventHash, Vec<u8>)>,
    pending_events: Vec<(ScopeId, EventHash, Vec<u8>)>,
    checkpointer: Checkpointer,
    /// Mirrors the `self_revoked` meta row, which every poll checks.
    self_revoked: bool,
    /// Whether the unclean-shutdown marker is on disk.
    unclean_marked: bool,
    /// Whether a scope this device left may still be waiting to be purged.
    pub(crate) leaving: bool,
    _lock: Option<File>,
}

/// Outcome of running an event through validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Accepted {
    Stored(Verdict),
    Held(Vec<EventHash>),
    Duplicate,
}

impl Replica {
    /// Opens (or creates) the replica in `cfg.data_dir`. Only one instance may hold a data
    /// directory: two would share a Loro peer id and corrupt state.
    pub fn open(cfg: ReplicaConfig, env: Arc<dyn Env>, secrets: Arc<dyn SecretStore>) -> Result<Self> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let lock = File::create(cfg.data_dir.join(".lock"))?;
        if lock.try_lock().is_err() {
            return Err(Error::AlreadyOpen);
        }
        let db_path = cfg.data_dir.join("sync.db");

        let device = match secrets.load(DEVICE_SECRET) {
            Some(b) => DeviceSecret::from_bytes(&b)?,
            None => {
                let d = DeviceSecret::generate(env.as_ref());
                secrets.store(DEVICE_SECRET, &d.to_bytes())?;
                d
            }
        };

        let mut db = store::open(Some(&db_path))?;
        let unclean = cfg.data_dir.join(UNCLEAN);
        if unclean.exists() {
            db = store::check_and_repair(db, &db_path)?;
        }
        File::create(&unclean)?;
        let initialized: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')", [], |r| r.get(0))?;
        let restored = initialized && store::meta(&db, "device_pub")?.is_some_and(|stored| stored != device.public().0);
        let rebuild = store::migrate(&db)?;
        // A database whose device key is not ours was restored from a backup or copied from
        // another device. Its events and blobs are kept; the per-install identifiers (device
        // certificate, Loro peer id, outbox, peer records) must never be reused, so they reset.
        if restored {
            if let Some(c) = store::meta(&db, "device_cert")?.and_then(|b| payload::decode::<DeviceCert>(&b).ok()) {
                store::set_meta(&db, "restored_user", &c.user.0)?;
            }
            db.execute_batch(
                "DELETE FROM meta WHERE key IN ('device_cert', 'loro_peer', 'self_revoked');
                 DELETE FROM outbox; DELETE FROM peers; DELETE FROM storage_peers;",
            )?;
        }

        let mut user = match secrets.load(USER_SECRET) {
            Some(b) => UserSecret::from_entropy(b.try_into().map_err(|_| Error::Crypto("bad user secret".into()))?),
            None => {
                let u = UserSecret::generate(env.as_ref());
                secrets.store(USER_SECRET, &u.entropy())?;
                u
            }
        };
        // An identity rotation that stopped between committing and storing its secret.
        if let Some(b) = secrets.load(USER_SECRET_NEXT) {
            let next = UserSecret::from_entropy(b.try_into().map_err(|_| Error::Crypto("bad user secret".into()))?);
            let committed: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM retired_users WHERE user = ? AND successor = ?)",
                rusqlite::params![&user.public().0[..], &next.public().0[..]],
                |r| r.get(0),
            )?;
            if committed {
                secrets.store(USER_SECRET, &next.entropy())?;
                user = next;
            }
            secrets.delete(USER_SECRET_NEXT)?;
        }

        store::set_meta(&db, "device_pub", &device.public().0)?;
        let cert = Self::load_or_issue_cert(&db, &cfg, &user, &device, env.as_ref())?;
        let peer_id = match store::meta(&db, "loro_peer")? {
            Some(b) => u64::from_le_bytes(b.try_into().map_err(|_| Error::Storage("bad loro peer".into()))?),
            None => {
                let p = random_u64(env.as_ref()) >> 1;
                store::set_meta(&db, "loro_peer", &p.to_le_bytes())?;
                p
            }
        };
        let max_hlc: i64 = db.query_row("SELECT COALESCE(MAX(hlc), 0) FROM events", [], |r| r.get(0))?;
        let blobs = BlobStore::new(cfg.data_dir.join("blobs"))?;

        let mut r = Replica {
            db,
            env,
            cfg,
            secrets,
            device,
            user,
            cert,
            clock: HlcClock::new(max_hlc as u64),
            auth: HashMap::new(),
            keys: HashMap::new(),
            seeded_keys: HashMap::new(),
            loro: LoroCache::new(peer_id),
            blobs,
            recovery_checked: HashMap::new(),
            in_tx: false,
            changes: Vec::new(),
            pending_changes: Vec::new(),
            new_events: Vec::new(),
            pending_events: Vec::new(),
            checkpointer: Checkpointer::new(db_path),
            self_revoked: false,
            unclean_marked: true,
            leaving: true,
            _lock: Some(lock),
        };
        r.self_revoked = store::meta(&r.db, "self_revoked")?.is_some();
        r.upsert_known_device(&r.cert.clone())?;
        if rebuild {
            r.rebuild_all_views()?;
        }
        if restored {
            // Roles were computed for the old device; this one isn't certified anywhere yet.
            for scope in r.scope_ids()? {
                r.refresh_conversation(scope)?;
            }
        }
        match r.restored_user()? {
            Some(u) if u == r.user_key() => r.finish_recovery()?,
            Some(_) => r.changes.push(Change::RecoveryAvailable),
            None => {}
        }
        Ok(r)
    }

    /// The user a restored database belonged to, until this device takes on that identity.
    pub fn restored_user(&self) -> Result<Option<UserKey>> {
        Ok(store::meta(&self.db, "restored_user")?.and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()).map(UserKey))
    }

    /// Whether this device holds a restored database for another identity, so entering that
    /// identity's recovery phrase would unlock its history.
    pub fn recovery_available(&self) -> Result<bool> {
        Ok(self.restored_user()?.is_some_and(|u| u != self.user_key()))
    }

    fn load_or_issue_cert(db: &Connection, cfg: &ReplicaConfig, user: &UserSecret, device: &DeviceSecret, env: &dyn Env) -> Result<DeviceCert> {
        if let Some(b) = store::meta(db, "device_cert")?
            && let Ok(c) = payload::decode::<DeviceCert>(&b)
            && c.user == user.public()
            && c.device == device.public()
            && c.name == cfg.device_name
            && c.platform == cfg.platform
        {
            return Ok(c);
        }
        let c = DeviceCert::issue(user, device.public(), &cfg.device_name, cfg.platform, env.now_ms());
        store::set_meta(db, "device_cert", &payload::encode(&c))?;
        Ok(c)
    }

    pub fn device_key(&self) -> DeviceKey {
        self.device.public()
    }

    pub fn device_secret(&self) -> &DeviceSecret {
        &self.device
    }

    pub fn user_key(&self) -> UserKey {
        self.user.public()
    }

    pub fn me(&self) -> Principal {
        Principal::User(self.user.public())
    }

    pub fn cert(&self) -> &DeviceCert {
        &self.cert
    }

    pub fn config(&self) -> &ReplicaConfig {
        &self.cfg
    }

    pub fn env(&self) -> &Arc<dyn Env> {
        &self.env
    }

    pub fn recovery_phrase(&self) -> Vec<String> {
        self.user.phrase()
    }

    /// The database connection, for tests that inspect or damage it.
    #[doc(hidden)]
    pub fn db_for_tests(&self) -> &Connection {
        &self.db
    }

    pub fn is_self_revoked(&self) -> bool {
        self.self_revoked
    }

    pub(crate) fn mark_self_revoked(&mut self) -> Result<()> {
        store::set_meta(&self.db, "self_revoked", &[1])?;
        self.self_revoked = true;
        Ok(())
    }

    /// Drains view diffs produced by committed transactions.
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    /// Drains newly stored events (local and received) for gossip.
    pub fn take_new_events(&mut self) -> Vec<(ScopeId, EventHash, Vec<u8>)> {
        std::mem::take(&mut self.new_events)
    }

    pub(crate) fn emit(&mut self, c: Change) {
        self.pending_changes.push(c);
    }

    /// Runs `f` in one SQLite transaction. Diffs and new events are published only on commit;
    /// on failure, in-memory caches derived from the database are dropped.
    pub(crate) fn tx<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.in_tx {
            return f(self);
        }
        if !self.unclean_marked {
            File::create(self.cfg.data_dir.join(UNCLEAN))?;
            self.unclean_marked = true;
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        self.in_tx = true;
        let res = f(self);
        self.in_tx = false;
        match res {
            Ok(v) => match self.db.execute_batch("COMMIT") {
                Ok(()) => {
                    self.changes.append(&mut self.pending_changes);
                    self.new_events.append(&mut self.pending_events);
                    Ok(v)
                }
                Err(e) => {
                    let _ = self.db.execute_batch("ROLLBACK");
                    self.discard_caches();
                    Err(e.into())
                }
            },
            Err(e) => {
                let _ = self.db.execute_batch("ROLLBACK");
                self.discard_caches();
                Err(e)
            }
        }
    }

    fn discard_caches(&mut self) {
        self.pending_changes.clear();
        self.pending_events.clear();
        self.auth.clear();
        self.keys.clear();
        self.loro.clear();
        self.recovery_checked.clear();
        self.self_revoked = store::meta(&self.db, "self_revoked").ok().flatten().is_some();
    }

    /// Flushes Loro snapshots and checkpoints the WAL. Called on clean shutdown and when an iOS
    /// app moves to the background.
    pub fn flush(&mut self) -> Result<()> {
        self.snapshot_all_loro()?;
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        match std::fs::remove_file(self.cfg.data_dir.join(UNCLEAN)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => self.unclean_marked = false,
        }
        Ok(())
    }

    /// Flushes and releases the data directory lock so another instance may open it.
    pub fn close(&mut self) -> Result<()> {
        self.checkpointer.close();
        self.flush()?;
        self._lock = None;
        Ok(())
    }

    // ---- scope bookkeeping ----

    pub(crate) fn scope_exists(&self, scope: ScopeId) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM scopes WHERE scope = ?)", [&scope.0[..]], |r| r.get(0))?)
    }

    /// Subscribes to a scope so its events are accepted and synced. Events for scopes that were
    /// never joined are refused.
    pub fn join_scope(&mut self, scope: ScopeId) -> Result<()> {
        self.insert_scope(scope, false)?;
        self.db.execute("UPDATE scopes SET tentative = 0 WHERE scope = ?", [&scope.0[..]])?;
        Ok(())
    }

    /// Joins a scope a peer offered, pending proof that this user is a member. `settle_scope`
    /// keeps or drops it once the control layer has synced.
    pub fn join_scope_tentatively(&mut self, scope: ScopeId) -> Result<()> {
        self.insert_scope(scope, true)
    }

    fn insert_scope(&mut self, scope: ScopeId, tentative: bool) -> Result<()> {
        let mode = self.cfg.default_history();
        let (m, days) = match mode {
            HistoryMode::Full => (0, 0),
            HistoryMode::Recent { days } => (1, days as i64),
        };
        self.db.execute(
            "INSERT OR IGNORE INTO scopes(scope, history_mode, window_days, storage_role, joined_ms, tentative) VALUES (?, ?, ?, ?, ?, ?)",
            params![&scope.0[..], m, days, self.cfg.storage_role as i64, self.env.now_ms() as i64, tentative as i64],
        )?;
        self.db.execute("DELETE FROM left_scopes WHERE scope = ?", [&scope.0[..]])?;
        Ok(())
    }

    pub(crate) fn is_tentative(&self, scope: ScopeId) -> Result<bool> {
        let t: Option<bool> = self.db.query_row("SELECT tentative FROM scopes WHERE scope = ?", [&scope.0[..]], |r| r.get(0)).optional()?;
        Ok(t.unwrap_or(false))
    }

    /// Whether this user (or this device) appears in the scope's membership.
    pub fn is_member(&mut self, scope: ScopeId) -> Result<bool> {
        let me = self.me();
        let device = self.device_key();
        let st = self.auth_mut(scope)?.full_state();
        Ok(st.members.contains_key(&me) || st.certs.contains_key(&device))
    }

    /// Keeps a tentatively joined scope if this user is a member, and purges it otherwise.
    /// Returns whether the scope is kept.
    pub fn settle_scope(&mut self, scope: ScopeId) -> Result<bool> {
        if !self.is_tentative(scope)? {
            return self.scope_exists(scope);
        }
        if self.is_member(scope)? {
            self.db.execute("UPDATE scopes SET tentative = 0 WHERE scope = ?", [&scope.0[..]])?;
            return Ok(true);
        }
        // Control events still waiting for parents may yet list this user.
        let held: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM held WHERE scope = ?)", [&scope.0[..]], |r| r.get(0))?;
        if held {
            return Ok(true);
        }
        self.purge_scope(scope)?;
        Ok(false)
    }

    /// Deletes everything stored for a scope on this device. Peers are unaffected.
    pub fn purge_scope(&mut self, scope: ScopeId) -> Result<()> {
        let had_view: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM conversations_view WHERE scope = ?)", [&scope.0[..]], |r| r.get(0))?;
        let blobs: Vec<Vec<u8>> = {
            let mut stmt = self
                .db
                .prepare_cached("SELECT DISTINCT hash FROM blob_refs WHERE scope = ?1 AND hash NOT IN (SELECT hash FROM blob_refs WHERE scope != ?1)")?;
            stmt.query_map([&scope.0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        self.tx(|r| {
            let s = &scope.0[..];
            r.db.execute("DELETE FROM messages_fts WHERE rowid IN (SELECT rowid FROM messages_view WHERE scope = ?)", [s])?;
            r.db.execute("DELETE FROM event_parents WHERE child IN (SELECT hash FROM events WHERE scope = ?)", [s])?;
            r.db.execute("DELETE FROM held_needs WHERE hash IN (SELECT hash FROM held WHERE scope = ?)", [s])?;
            for table in [
                "events",
                "heads",
                "held",
                "outbox",
                "peers",
                "storage_peers",
                "blob_refs",
                "loro_snapshots",
                "read_state",
                "invites_issued",
                "messages_view",
                "edits",
                "tombstones",
                "reactions",
                "reaction_removed",
                "conversations_view",
                "pending_decrypt",
                "scopes",
            ] {
                r.db.execute(&format!("DELETE FROM {table} WHERE scope = ?"), [s])?;
            }
            for h in &blobs {
                r.db.execute("DELETE FROM blobs WHERE hash = ?", [h])?;
            }
            Ok(())
        })?;
        for h in &blobs {
            if let Ok(h) = <[u8; 32]>::try_from(h.as_slice()) {
                self.blobs.delete(&h);
            }
        }
        self.auth.remove(&scope);
        self.keys.remove(&scope);
        self.loro.remove(&scope);
        if had_view {
            self.changes.push(Change::ConversationRemoved(scope));
        }
        Ok(())
    }

    pub(crate) fn auth_mut(&mut self, scope: ScopeId) -> Result<&mut ScopeAuth> {
        if !self.auth.contains_key(&scope) {
            let mut a = ScopeAuth::new(scope);
            let mut stmt = self.db.prepare_cached("SELECT bytes FROM events WHERE scope = ? AND control = 1 AND version = ? ORDER BY seq")?;
            let rows: Vec<Vec<u8>> =
                stmt.query_map(params![&scope.0[..], crate::envelope::ENVELOPE_VERSION as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            for b in rows {
                if let Ok(Decoded::Known(e)) = Envelope::decode(&b) {
                    a.load(ctl_event(&e));
                }
            }
            a.recompute();
            self.auth.insert(scope, a);
        }
        Ok(self.auth.get_mut(&scope).unwrap())
    }

    pub(crate) fn heads(&self, scope: ScopeId) -> Result<(Vec<EventHash>, Vec<EventHash>)> {
        let mut stmt = self.db.prepare_cached("SELECT layer, hash FROM heads WHERE scope = ? ORDER BY layer, hash")?;
        let mut control = Vec::new();
        let mut all = Vec::new();
        for row in stmt.query_map([&scope.0[..]], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))? {
            let (layer, h) = row?;
            let h = EventHash::from_slice(&h).ok_or_else(|| Error::Storage("bad head".into()))?;
            if layer == 1 { control.push(h) } else { all.push(h) }
        }
        all.retain(|h| !control.contains(h));
        Ok((control, all))
    }

    fn store_event(&mut self, e: &Envelope, hash: EventHash, st: i64, reason: Option<Reason>) -> Result<()> {
        let control = kind::is_control(e.kind);
        self.db.execute(
            "INSERT INTO events(hash, id, scope, author, hlc, kind, version, epoch, control, status, reason, purged, bytes)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                &hash.0[..],
                &e.id.0[..],
                &e.scope.0[..],
                &e.author.0[..],
                e.hlc as i64,
                e.kind as i64,
                e.version as i64,
                e.epoch as i64,
                control as i64,
                st,
                reason.map(|r| r.code()),
                e.payload_bytes().is_none() as i64,
                e.encode()
            ],
        )?;
        for p in &e.control_parents {
            self.db.execute("INSERT OR IGNORE INTO event_parents(child, parent, control) VALUES (?, ?, 1)", params![&hash.0[..], &p.0[..]])?;
        }
        for p in &e.parents {
            self.db.execute("INSERT OR IGNORE INTO event_parents(child, parent, control) VALUES (?, ?, 0)", params![&hash.0[..], &p.0[..]])?;
        }
        for p in e.all_parents() {
            self.db.execute("DELETE FROM heads WHERE scope = ? AND layer = 0 AND hash = ?", params![&e.scope.0[..], &p.0[..]])?;
        }
        let has_child: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM event_parents WHERE parent = ?)", [&hash.0[..]], |r| r.get(0))?;
        if !has_child {
            self.db.execute("INSERT OR IGNORE INTO heads(scope, layer, hash) VALUES (?, 0, ?)", params![&e.scope.0[..], &hash.0[..]])?;
        }
        if control {
            for p in &e.control_parents {
                self.db.execute("DELETE FROM heads WHERE scope = ? AND layer = 1 AND hash = ?", params![&e.scope.0[..], &p.0[..]])?;
            }
            let has_ctl_child: bool =
                self.db.query_row("SELECT EXISTS(SELECT 1 FROM event_parents WHERE parent = ? AND control = 1)", [&hash.0[..]], |r| r.get(0))?;
            if !has_ctl_child {
                self.db.execute("INSERT OR IGNORE INTO heads(scope, layer, hash) VALUES (?, 1, ?)", params![&e.scope.0[..], &hash.0[..]])?;
            }
        }
        Ok(())
    }

    pub(crate) fn set_status(&mut self, hash: &EventHash, v: Verdict) -> Result<()> {
        let (s, r) = match v {
            Verdict::Valid => (status::VALID, None),
            Verdict::Rejected(r) => (status::REJECTED, Some(r.code())),
        };
        self.db.execute("UPDATE events SET status = ?, reason = ? WHERE hash = ? AND status != ?", params![s, r, &hash.0[..], status::UNSUPPORTED])?;
        Ok(())
    }

    pub(crate) fn event_exists(&self, hash: &EventHash) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE hash = ?)", [&hash.0[..]], |r| r.get(0))?)
    }

    pub(crate) fn load_event(&self, hash: &EventHash) -> Result<Option<Envelope>> {
        let b: Option<Vec<u8>> = self.db.query_row("SELECT bytes FROM events WHERE hash = ?", [&hash.0[..]], |r| r.get(0)).optional()?;
        Ok(match b.map(|b| Envelope::decode(&b)).transpose()? {
            Some(Decoded::Known(e)) => Some(e),
            _ => None,
        })
    }

    /// Validates, stores and applies one event. The caller holds the transaction.
    pub(crate) fn accept(&mut self, e: Envelope) -> Result<Accepted> {
        let hash = e.hash();
        if self.event_exists(&hash)? {
            return Ok(Accepted::Duplicate);
        }
        if !self.scope_exists(e.scope)? {
            return Err(Error::NotFound("scope not joined".into()));
        }
        let scope = e.scope;
        let missing = self.auth_mut(scope)?.missing_parents(&e.control_parents);
        if !missing.is_empty() {
            return Ok(Accepted::Held(missing));
        }

        let verdict = if kind::is_control(e.kind) {
            let ctl = ctl_event(&e);
            let unknown = ctl.body.is_none() && e.kind != kind::LORO_UPDATE;
            let out = self.auth_mut(scope)?.add(ctl.clone());
            let v = out.verdict.unwrap_or(Verdict::Rejected(Reason::InvalidPayload));
            let st = match v {
                Verdict::Valid if unknown => status::UNSUPPORTED,
                Verdict::Valid => status::VALID,
                Verdict::Rejected(_) => status::REJECTED,
            };
            self.store_event(
                &e,
                hash,
                st,
                match v {
                    Verdict::Rejected(r) => Some(r),
                    _ => None,
                },
            )?;
            for (h, nv) in &out.changed {
                self.set_status(h, *nv)?;
            }
            if out.revalidate_content || !out.changed.is_empty() {
                self.revalidate_scope(scope)?;
            } else if v.is_valid() && !unknown {
                self.apply_control(&e, &ctl)?;
            } else if e.kind == kind::LORO_UPDATE && self.loro_included(&e)? {
                let acting = self.auth_mut(scope)?.causal_acting(e.author, &e.control_parents, e.epoch);
                self.apply_content(&e, hash, &acting)?;
            }
            v
        } else if !kind::is_known(e.kind) {
            self.store_event(&e, hash, status::UNSUPPORTED, None)?;
            Verdict::Valid
        } else {
            let (v, acting) = self.auth_mut(scope)?.validate_content(e.author, e.hlc, &e.control_parents, e.epoch);
            self.store_event(
                &e,
                hash,
                if v.is_valid() { status::VALID } else { status::REJECTED },
                match v {
                    Verdict::Rejected(r) => Some(r),
                    _ => None,
                },
            )?;
            match v {
                Verdict::Valid => self.apply_content(&e, hash, &acting)?,
                Verdict::Rejected(_) if e.kind == kind::TOMBSTONE => {
                    let acting = self.auth_mut(scope)?.causal_acting(e.author, &e.control_parents, e.epoch);
                    if !acting.is_empty() {
                        self.apply_content(&e, hash, &acting)?;
                    }
                }
                Verdict::Rejected(r) => self.apply_rejected_own(&e, hash, r)?,
            }
            v
        };
        // Rejected events must not move this device's clock.
        if verdict.is_valid() {
            self.clock.observe(e.hlc, self.env.now_ms());
        }
        self.pending_events.push((scope, hash, e.encode()));
        Ok(Accepted::Stored(verdict))
    }

    /// Creates, signs and accepts a local event. Rejected local writes fail with
    /// `PermissionDenied` and roll back.
    pub(crate) fn append(
        &mut self,
        scope: ScopeId,
        kind_: u16,
        epoch: u64,
        payload: Vec<u8>,
        id: Option<EventId>,
        hlc: Option<u64>,
    ) -> Result<(EventId, EventHash)> {
        if self.is_self_revoked() {
            return Err(Error::DeviceRevoked);
        }
        let now = self.env.now_ms();
        let hlc = hlc.unwrap_or_else(|| self.clock.tick(now));
        let id = id.unwrap_or_else(|| EventId::new_v7(now, crate::env::random_array(self.env.as_ref())));
        let (control_parents, parents) = self.heads(scope)?;
        let e = Envelope::sign(Draft { id, scope, hlc, control_parents, parents, kind: kind_, epoch, payload }, &self.device);
        let hash = e.hash();
        if !kind::is_control(kind_) || kind_ == kind::LORO_UPDATE {
            // Pending until a peer that stores the scope acknowledges it.
            if self.has_other_devices(scope)? {
                self.db.execute(
                    "INSERT OR IGNORE INTO outbox(hash, scope, id, created_ms) VALUES (?, ?, ?, ?)",
                    params![&hash.0[..], &scope.0[..], &id.0[..], now as i64],
                )?;
            }
        }
        match self.accept(e)? {
            Accepted::Stored(Verdict::Valid) => Ok((id, hash)),
            Accepted::Stored(Verdict::Rejected(r)) => Err(Error::PermissionDenied(format!("{r:?}"))),
            Accepted::Held(_) => Err(Error::Internal("local event held".into())),
            Accepted::Duplicate => Err(Error::Internal("duplicate local event".into())),
        }
    }

    pub(crate) fn append_control(&mut self, scope: ScopeId, c: &Control) -> Result<EventHash> {
        Ok(self.append(scope, c.kind(), 0, c.encode(), None, None)?.1)
    }

    pub(crate) fn append_content(&mut self, scope: ScopeId, c: &Content, id: Option<EventId>, hlc: Option<u64>) -> Result<EventId> {
        let (epoch, key) = self.current_key(scope)?.ok_or(Error::NoKey)?;
        let now = self.env.now_ms();
        let id = id.unwrap_or_else(|| EventId::new_v7(now, crate::env::random_array(self.env.as_ref())));
        let ct = crypto::encrypt_payload(self.env.as_ref(), &key, &id.0, &c.encode());
        Ok(self.append(scope, c.kind(), epoch, ct, Some(id), hlc)?.0)
    }

    pub(crate) fn has_other_devices(&mut self, scope: ScopeId) -> Result<bool> {
        let me = self.device_key();
        let st = self.auth_mut(scope)?.full_state();
        Ok(st.certs.keys().any(|d| *d != me && !st.revoked.contains(d)))
    }

    // ---- keys ----

    pub(crate) fn scope_keys(&mut self, scope: ScopeId) -> Result<&ScopeKeys> {
        if !self.keys.contains_key(&scope) {
            let mut sk = ScopeKeys::default();
            let mut stmt = self.db.prepare_cached("SELECT bytes FROM events WHERE scope = ? AND kind = ? AND status = ?")?;
            let rows: Vec<Vec<u8>> =
                stmt.query_map(params![&scope.0[..], kind::KEY_UPDATE as i64, status::VALID], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            for b in rows {
                if let Ok(Decoded::Known(e)) = Envelope::decode(&b) {
                    self.unseal_grants(&e, &mut sk);
                }
            }
            if let Some(seed) = self.seeded_keys.get(&scope) {
                for (epoch, k) in seed.all() {
                    sk.add(epoch, k);
                }
            }
            self.keys.insert(scope, sk);
        }
        Ok(self.keys.get(&scope).unwrap())
    }

    fn unseal_grants(&self, e: &Envelope, sk: &mut ScopeKeys) -> bool {
        let Some(Ok(Some(Control::KeyUpdate(ku)))) = e.payload_bytes().map(|b| Control::decode(kind::KEY_UPDATE, b)) else { return false };
        let me = self.device_key();
        let recovery = self.user.recovery_public();
        let mut added = false;
        for g in &ku.grants {
            let opener = if g.device == me {
                self.device.signing_key()
            } else if g.device == recovery {
                self.user.recovery_key()
            } else {
                continue;
            };
            if let Ok(k) = crypto::unseal(opener, &g.sealed)
                && let Ok(k) = <[u8; 32]>::try_from(k.as_slice())
                && crypto::key_check(&k) == g.key_check
            {
                added |= sk.add(g.epoch, k);
            }
        }
        added
    }

    pub(crate) fn current_key(&mut self, scope: ScopeId) -> Result<Option<(u64, [u8; 32])>> {
        Ok(self.scope_keys(scope)?.current())
    }

    pub(crate) fn all_keys(&mut self, scope: ScopeId) -> Result<Vec<(u64, [u8; 32])>> {
        Ok(self.scope_keys(scope)?.all().collect())
    }

    pub(crate) fn decrypt(&mut self, e: &Envelope) -> Result<Option<Vec<u8>>> {
        let Some(ct) = e.payload_bytes() else { return Ok(None) };
        let Some(check) = crypto::payload_key_check(ct) else { return Ok(None) };
        let key = self.scope_keys(e.scope)?.find(e.epoch, check);
        match key {
            Some(k) => Ok(crypto::decrypt_payload(&k, &e.id.0, ct).ok()),
            None => Ok(None),
        }
    }

    pub(crate) fn upsert_known_device(&mut self, c: &DeviceCert) -> Result<()> {
        if !c.verify() {
            return Ok(());
        }
        self.db.execute(
            "INSERT INTO known_devices(device, user, name, platform, cert) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(device, user) DO UPDATE SET name = excluded.name, platform = excluded.platform, cert = excluded.cert",
            params![&c.device.0[..], &c.user.0[..], c.name, c.platform.name(), payload::encode(c)],
        )?;
        Ok(())
    }

    pub(crate) fn my_role(&mut self, scope: ScopeId) -> Result<Option<Role>> {
        let me = self.device_key();
        Ok(self.auth_mut(scope)?.full_state().max_role(&me))
    }
}

pub(crate) fn ctl_event(e: &Envelope) -> CtlEvent {
    let (body, malformed) = if kind::is_plaintext(e.kind) {
        match e.payload_bytes().map(|b| Control::decode(e.kind, b)) {
            Some(Ok(b)) => (b, false),
            _ => (None, kind::is_known(e.kind)),
        }
    } else {
        (None, false)
    };
    CtlEvent {
        hash: e.hash(),
        id: e.id,
        author: e.author,
        hlc: e.hlc,
        kind: e.kind,
        epoch: e.epoch,
        control_parents: e.control_parents.clone(),
        body,
        malformed,
    }
}
