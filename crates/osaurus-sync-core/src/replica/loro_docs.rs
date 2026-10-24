//! Loro documents for concurrently mutable scope state (title, pinned state, order, agent state).
//! Documents load from their latest snapshot plus later `loro_update` events, and are evicted
//! when their scope goes inactive.

use std::collections::{HashMap, VecDeque};

use loro::{ExportMode, LoroDoc, LoroValue, ValueOrContainer};
use rusqlite::{OptionalExtension, params};

use super::Replica;
use crate::envelope::{Decoded, Envelope, kind};
use crate::error::{Error, Result};
use crate::ids::{Role, ScopeId};
use crate::payload::{Content, LoroUpdatePayload};
use crate::store::{purged, status};

const MAX_LOADED: usize = 32;

pub struct LoroEntry {
    pub doc: LoroDoc,
    pub updates_since_snapshot: u32,
}

pub struct LoroCache {
    peer_id: u64,
    docs: HashMap<ScopeId, LoroEntry>,
    lru: VecDeque<ScopeId>,
}

impl LoroCache {
    pub fn new(peer_id: u64) -> Self {
        Self { peer_id, docs: HashMap::new(), lru: VecDeque::new() }
    }

    pub fn clear(&mut self) {
        self.docs.clear();
        self.lru.clear();
    }

    pub fn remove(&mut self, scope: &ScopeId) {
        self.docs.remove(scope);
        self.lru.retain(|s| s != scope);
    }

    fn touch(&mut self, scope: ScopeId) {
        self.lru.retain(|s| *s != scope);
        self.lru.push_back(scope);
    }
}

/// Metadata read from a scope's Loro document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScopeMeta {
    pub title: String,
    pub pinned: bool,
    pub sort_order: Option<f64>,
    pub archived: bool,
}

/// Imports one update. A shallow snapshot only takes effect in an empty document, so when the
/// document already has state the snapshot becomes the base and the document's own changes are
/// replayed onto it. If they cannot be (they depend on history the snapshot trimmed), the
/// document is kept as it is.
pub(crate) fn import_payload(doc: &mut LoroDoc, peer_id: u64, p: &LoroUpdatePayload) {
    if !p.shallow {
        let _ = doc.import(&p.bytes);
        return;
    }
    let base = LoroDoc::new();
    if base.import(&p.bytes).is_err() || doc.oplog_vv().includes_vv(&base.oplog_vv()) {
        return;
    }
    if !doc.oplog_vv().is_empty() {
        match doc.export(ExportMode::all_updates()) {
            Ok(ours) if base.import(&ours).is_ok() => {}
            _ => return,
        }
    }
    let _ = base.set_peer_id(peer_id);
    *doc = base;
}

fn value(m: &loro::LoroMap, key: &str) -> Option<LoroValue> {
    match m.get(key)? {
        ValueOrContainer::Value(v) => Some(v),
        _ => None,
    }
}

impl Replica {
    pub(crate) fn loro_doc(&mut self, scope: ScopeId) -> Result<&mut LoroEntry> {
        if !self.loro.docs.contains_key(&scope) {
            let doc = LoroDoc::new();
            doc.set_peer_id(self.loro.peer_id).map_err(|e| Error::Internal(e.to_string()))?;
            let snap: Option<(Vec<u8>, i64)> = self
                .db
                .query_row("SELECT snapshot, upto_seq FROM loro_snapshots WHERE scope = ?", [&scope.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let mut upto = 0i64;
            if let Some((s, u)) = snap
                && doc.import(&s).is_ok()
            {
                upto = u;
            }
            let rows: Vec<(Vec<u8>, i64)> = {
                let mut stmt =
                    self.db.prepare_cached("SELECT bytes, status FROM events WHERE scope = ? AND kind = ? AND status != ? AND seq > ? ORDER BY seq")?;
                stmt.query_map(params![&scope.0[..], kind::LORO_UPDATE as i64, status::UNSUPPORTED, upto], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?
            };
            let mut doc = doc;
            let mut n = 0;
            for (b, st) in rows {
                if let Some(p) = self.loro_payload(&b, st)? {
                    import_payload(&mut doc, self.loro.peer_id, &p);
                    n += 1;
                }
            }
            if self.loro.docs.len() >= MAX_LOADED
                && let Some(old) = self.loro.lru.front().copied()
            {
                self.snapshot_loro(old)?;
                self.loro.remove(&old);
            }
            self.loro.docs.insert(scope, LoroEntry { doc, updates_since_snapshot: n });
        }
        self.loro.touch(scope);
        Ok(self.loro.docs.get_mut(&scope).unwrap())
    }

    /// The decrypted payload of a stored `loro_update` event, if it counts toward the document.
    fn loro_payload(&mut self, bytes: &[u8], st: i64) -> Result<Option<LoroUpdatePayload>> {
        if let Ok(Decoded::Known(e)) = Envelope::decode(bytes)
            && (st == status::VALID || self.loro_included(&e)?)
            && let Some(pt) = self.decrypt(&e)?
            && let Ok(Some(Content::Loro(p))) = Content::decode(kind::LORO_UPDATE, &pt)
        {
            return Ok(Some(p));
        }
        Ok(None)
    }

    /// Folds the scope's Loro updates up to `upto_seq` into one snapshot, published as a new
    /// `loro_update` event, and drops their payloads. Returns the number of updates folded.
    ///
    /// The snapshot keeps the folded history. A shallow one would not merge with an update a
    /// partitioned device made on top of the trimmed history, nor with another device's
    /// concurrent fold, and devices would settle on different states.
    pub(crate) fn fold_loro(&mut self, scope: ScopeId, upto_seq: i64) -> Result<u32> {
        if self.my_role(scope)? < Some(Role::Member) {
            return Ok(0);
        }
        let rows: Vec<(Vec<u8>, Vec<u8>, i64)> = {
            let mut stmt = self.db.prepare_cached(
                "SELECT hash, bytes, status FROM events WHERE scope = ? AND kind = ? AND status != ? AND purged = ? AND seq <= ? ORDER BY seq",
            )?;
            stmt.query_map(params![&scope.0[..], kind::LORO_UPDATE as i64, status::UNSUPPORTED, purged::NONE, upto_seq], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?
        };
        if rows.len() < self.cfg.compact_min_updates.max(1) as usize {
            return Ok(0);
        }
        // The version every storing peer holds: the folded updates replayed on their own.
        let mut base = LoroDoc::new();
        let mut folded = Vec::new();
        for (h, b, st) in rows {
            if let Some(p) = self.loro_payload(&b, st)? {
                import_payload(&mut base, self.loro.peer_id, &p);
                folded.push(h);
            }
        }
        let bytes = match base.export(ExportMode::Snapshot) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(error = %e, "snapshot failed; not folding");
                return Ok(0);
            }
        };
        // Only a fold over an earlier shallow snapshot, from before folds kept history, is shallow.
        let shallow = base.is_shallow();
        self.append_content(scope, &Content::Loro(LoroUpdatePayload { doc: 0, shallow, bytes }), None, None)?;
        for h in &folded {
            self.compact_event(h)?;
        }
        self.loro_doc(scope)?.updates_since_snapshot += 1;
        self.snapshot_loro(scope)?;
        Ok(folded.len() as u32)
    }

    /// Loro updates depend on each other, so excluding one would strand every later update built
    /// on it. An update counts if its author could write given its own causal past, even when a
    /// concurrent removal later rejects the event.
    pub(crate) fn loro_included(&mut self, e: &Envelope) -> Result<bool> {
        Ok(!self.auth_mut(e.scope)?.causal_acting(e.author, &e.control_parents, e.epoch).is_empty())
    }

    pub(crate) fn scope_meta(&mut self, scope: ScopeId) -> Result<ScopeMeta> {
        let entry = self.loro_doc(scope)?;
        let m = entry.doc.get_map("meta");
        Ok(ScopeMeta {
            title: match value(&m, "title") {
                Some(LoroValue::String(s)) => s.to_string(),
                _ => String::new(),
            },
            pinned: matches!(value(&m, "pinned"), Some(LoroValue::Bool(true))),
            sort_order: match value(&m, "order") {
                Some(LoroValue::Double(d)) => Some(d),
                Some(LoroValue::I64(i)) => Some(i as f64),
                _ => None,
            },
            archived: matches!(value(&m, "archived"), Some(LoroValue::Bool(true))),
        })
    }

    /// Applies a local change to the scope's Loro document and records it as a `loro_update` event.
    pub(crate) fn loro_write(&mut self, scope: ScopeId, f: impl FnOnce(&LoroDoc) -> std::result::Result<(), loro::LoroError>) -> Result<()> {
        let bytes = {
            let entry = self.loro_doc(scope)?;
            let vv = entry.doc.oplog_vv();
            f(&entry.doc).map_err(|e| Error::Internal(e.to_string()))?;
            entry.doc.commit();
            entry.doc.export(ExportMode::updates(&vv)).map_err(|e| Error::Internal(e.to_string()))?
        };
        self.append_content(scope, &Content::Loro(LoroUpdatePayload { doc: 0, shallow: false, bytes }), None, None)?;
        Ok(())
    }

    pub(crate) fn apply_loro(&mut self, scope: ScopeId, p: &LoroUpdatePayload) -> Result<()> {
        let every = self.cfg.snapshot_every;
        let peer_id = self.loro.peer_id;
        let entry = self.loro_doc(scope)?;
        import_payload(&mut entry.doc, peer_id, p);
        entry.updates_since_snapshot += 1;
        if entry.updates_since_snapshot >= every {
            self.snapshot_loro(scope)?;
        }
        Ok(())
    }

    pub(crate) fn snapshot_loro(&mut self, scope: ScopeId) -> Result<()> {
        let Some(entry) = self.loro.docs.get_mut(&scope) else { return Ok(()) };
        if entry.updates_since_snapshot == 0 {
            return Ok(());
        }
        let snap = entry.doc.export(ExportMode::Snapshot).map_err(|e| Error::Internal(e.to_string()))?;
        entry.updates_since_snapshot = 0;
        let upto: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE scope = ? AND kind = ?",
            params![&scope.0[..], kind::LORO_UPDATE as i64],
            |r| r.get(0),
        )?;
        self.db.execute(
            "INSERT INTO loro_snapshots(scope, snapshot, upto_seq) VALUES (?, ?, ?)
             ON CONFLICT(scope) DO UPDATE SET snapshot = excluded.snapshot, upto_seq = excluded.upto_seq",
            params![&scope.0[..], snap, upto],
        )?;
        Ok(())
    }

    pub(crate) fn snapshot_all_loro(&mut self) -> Result<()> {
        let scopes: Vec<ScopeId> = self.loro.docs.keys().copied().collect();
        for s in scopes {
            self.snapshot_loro(s)?;
        }
        Ok(())
    }

    /// Encoded Loro state for an agent: an app-defined string per agent id.
    pub fn agent_state(&mut self, scope: ScopeId, agent_key: &str) -> Result<Option<String>> {
        let entry = self.loro_doc(scope)?;
        let m = entry.doc.get_map("agent_state");
        Ok(match value(&m, agent_key) {
            Some(LoroValue::String(s)) => Some(s.to_string()),
            _ => None,
        })
    }
}
