//! Compaction and point-in-time views.
//!
//! History up to a version can be compacted once every known storing peer has reported holding
//! it: superseded edits lose their payloads and Loro updates fold into a shallow snapshot.
//! Compacted events keep their signed headers, so reconciliation still sees them and peers never
//! send them back. Devices with the storage role never compact.

use std::sync::Arc;

use rusqlite::params;

use super::{DEVICE_SECRET, MemorySecretStore, Replica, ReplicaConfig, SecretStore, USER_SECRET};
use crate::envelope::kind;
use crate::error::Result;
use crate::ids::{DeviceKey, EventHash, ScopeId};
use crate::store::{purged, status};
use crate::types::{ConversationSummary, HistoryMode, MessageView};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Compaction {
    /// Local seq up to which history was compacted; 0 when nothing could be.
    pub upto_seq: i64,
    pub edits_dropped: u32,
    pub loro_folded: u32,
}

/// A scope rebuilt from its events up to a past version. Read-only: reverting means writing new
/// events.
#[derive(Clone, Debug, PartialEq)]
pub struct PointInTimeView {
    pub at_hlc: u64,
    pub conversation: Option<ConversationSummary>,
    /// The newest messages as of `at_hlc`, oldest first.
    pub messages: Vec<MessageView>,
    /// False when this device compacted part of that history or keeps only recent history, so
    /// the view may lack edits, deleted-later content or older messages. A storing peer has it all.
    pub complete: bool,
}

impl Replica {
    pub fn max_seq(&self, scope: ScopeId) -> Result<i64> {
        Ok(self.db.query_row("SELECT COALESCE(MAX(seq), 0) FROM events WHERE scope = ?", [&scope.0[..]], |r| r.get(0))?)
    }

    /// Notes a peer with the storage role that shares this scope.
    pub fn record_storage_peer(&mut self, device: &DeviceKey, scope: ScopeId) -> Result<()> {
        self.db.execute("INSERT OR IGNORE INTO storage_peers(device, scope) VALUES (?, ?)", params![&device.0[..], &scope.0[..]])?;
        Ok(())
    }

    /// A storing peer holds every event of the scope up to local `seq`.
    pub fn record_storage_seen(&mut self, device: &DeviceKey, scope: ScopeId, seq: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO storage_peers(device, scope, seen_seq) VALUES (?1, ?2, ?3)
             ON CONFLICT(device, scope) DO UPDATE SET seen_seq = MAX(seen_seq, ?3)",
            params![&device.0[..], &scope.0[..], seq],
        )?;
        Ok(())
    }

    /// The local seq every known, unrevoked storing peer holds, or `None` without one.
    pub fn compactable_seq(&mut self, scope: ScopeId) -> Result<Option<i64>> {
        let rows: Vec<(Vec<u8>, i64)> = {
            let mut stmt = self.db.prepare_cached("SELECT device, seen_seq FROM storage_peers WHERE scope = ?")?;
            stmt.query_map([&scope.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        let st = self.auth_mut(scope)?.full_state();
        let seen = rows.into_iter().filter(|(d, _)| DeviceKey::from_slice(d).is_some_and(|d| !st.revoked.contains(&d))).map(|(_, s)| s).min();
        Ok(seen.filter(|s| *s > 0))
    }

    /// Compacts what every storing peer holds. A no-op on storing peers and without one.
    pub fn compact(&mut self, scope: ScopeId) -> Result<Compaction> {
        if self.cfg.storage_role {
            return Ok(Compaction::default());
        }
        let Some(upto) = self.compactable_seq(scope)? else { return Ok(Compaction::default()) };
        self.tx(|r| {
            let edits_dropped = r.drop_superseded_edits(scope, upto)?;
            let loro_folded = r.fold_loro(scope, upto)?;
            Ok(Compaction { upto_seq: upto, edits_dropped, loro_folded })
        })
    }

    /// Edits that lost to a later edit of the same message. Later edits can only win over the
    /// current winner, so these never become visible again.
    fn drop_superseded_edits(&mut self, scope: ScopeId, upto_seq: i64) -> Result<u32> {
        let hashes: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached(
                "SELECT e.hash FROM edits e
                 JOIN events ev ON ev.hash = e.hash
                 JOIN messages_view m ON m.id = e.target
                 WHERE e.scope = ? AND ev.seq <= ? AND ev.purged = ?
                   AND NOT (m.edit_hlc IS e.hlc AND m.edit_author IS e.author_device)",
            )?;
            stmt.query_map(params![&scope.0[..], upto_seq, purged::NONE], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        let mut n = 0;
        for h in hashes {
            if self.compact_event(&h)? {
                self.db.execute("DELETE FROM edits WHERE hash = ?", [&h])?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Events in the scope whose payload was compacted away.
    pub fn compacted_events(&self, scope: ScopeId) -> Result<u32> {
        Ok(self.db.query_row("SELECT COUNT(*) FROM events WHERE scope = ? AND purged = ?", params![&scope.0[..], purged::COMPACTED], |r| r.get::<_, i64>(0))?
            as u32)
    }

    /// Replaces an event's bytes with its payload-free form.
    pub(crate) fn compact_event(&mut self, hash: &[u8]) -> Result<bool> {
        let Some(h) = EventHash::from_slice(hash) else { return Ok(false) };
        let Some(e) = self.load_event(&h)? else { return Ok(false) };
        if e.payload_bytes().is_none() {
            return Ok(false);
        }
        self.db.execute("UPDATE events SET bytes = ?, purged = ? WHERE hash = ?", params![e.purged().encode(), purged::COMPACTED, hash])?;
        Ok(true)
    }

    /// Rebuilds the scope from its events with HLC at or before `at_hlc` in a throwaway replica
    /// and returns the newest `limit` messages and the conversation as they were then.
    pub fn view_at(&mut self, scope: ScopeId, at_hlc: u64, limit: u32) -> Result<PointInTimeView> {
        let rows: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT bytes FROM events WHERE scope = ? AND hlc <= ? AND status != ? ORDER BY seq")?;
            stmt.query_map(params![&scope.0[..], at_hlc.min(i64::MAX as u64) as i64, status::UNSUPPORTED], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        let compacted: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE scope = ? AND hlc <= ? AND purged = ? AND kind != ?)",
            params![&scope.0[..], at_hlc.min(i64::MAX as u64) as i64, purged::COMPACTED, kind::MESSAGE as i64],
            |r| r.get(0),
        )?;
        let complete = !compacted && self.history_mode(scope)? == HistoryMode::Full;

        let dir = self.cfg.data_dir.join(format!("view-at-{:016x}", crate::env::random_u64(self.env.as_ref())));
        let secrets = Arc::new(MemorySecretStore::default());
        secrets.store(DEVICE_SECRET, &self.device.to_bytes())?;
        secrets.store(USER_SECRET, &self.user.entropy())?;
        let mut cfg = ReplicaConfig::new(&dir, self.cfg.device_name.clone(), self.cfg.platform);
        cfg.storage_role = true;
        let res = (|| -> Result<PointInTimeView> {
            let mut past = Replica::open(cfg, self.env.clone(), secrets)?;
            // Keys granted to this device after `at_hlc` still decrypt what was written before it.
            past.seeded_keys.insert(scope, self.scope_keys(scope)?.clone());
            past.join_scope(scope)?;
            past.tx(|p| {
                for b in &rows {
                    p.ingest(b)?;
                }
                Ok(())
            })?;
            let conversation = past.conversation(scope)?;
            let mut messages = past.messages(scope, None, limit)?.messages;
            for m in &mut messages {
                for b in &mut m.blobs {
                    b.available = self.blobs.has(&b.hash);
                }
            }
            past.close()?;
            Ok(PointInTimeView { at_hlc, conversation, messages, complete })
        })();
        let _ = std::fs::remove_dir_all(&dir);
        res
    }
}
