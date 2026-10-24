//! Replica operations used by the sync engine.

use rusqlite::{OptionalExtension, params};

use super::{Accepted, Replica};
use crate::auth::Verdict;
use crate::envelope::{Decoded, Envelope};
use crate::error::Result;
use crate::identity::DeviceCert;
use crate::ids::{DeviceKey, EventHash, EventId, ScopeId};
use crate::store::{delivery, status};
use crate::types::{Change, HistoryMode};

/// A reconciliation key: display order without the author.
pub type ReconKey = (u64, EventId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Stored {
        scope: ScopeId,
        hash: EventHash,
        author: Option<DeviceKey>,
        valid: bool,
    },
    Duplicate {
        hash: EventHash,
    },
    /// Waiting for these control parents.
    Held {
        scope: ScopeId,
        hash: EventHash,
        missing: Vec<EventHash>,
    },
    /// The event belongs to a scope this device hasn't joined; it was not stored.
    NotJoined {
        scope: ScopeId,
    },
    Invalid(String),
}

/// Held events allowed per scope and per sending peer.
pub const HELD_MAX_EVENTS: usize = 10_000;
pub const HELD_MAX_BYTES: usize = 64 << 20;
/// Held events whose parents never arrive are dropped after a week.
pub const HELD_TTL_MS: u64 = 7 * 86_400_000;

impl Replica {
    /// Receives one encoded event from an unknown source.
    pub fn ingest(&mut self, bytes: &[u8]) -> Result<IngestOutcome> {
        self.ingest_from(bytes, None)
    }

    /// Receives one encoded event from `peer`, which per-peer limits are charged to.
    pub fn ingest_from(&mut self, bytes: &[u8], peer: Option<DeviceKey>) -> Result<IngestOutcome> {
        let decoded = match Envelope::decode(bytes) {
            Ok(d) => d,
            Err(e) => return Ok(IngestOutcome::Invalid(e.to_string())),
        };
        let e = match decoded {
            Decoded::Unknown { version, id, scope, hash, bytes } => {
                if !self.scope_exists(scope)? {
                    return Ok(IngestOutcome::NotJoined { scope });
                }
                return self.tx(|r| {
                    if r.event_exists(&hash)? {
                        return Ok(IngestOutcome::Duplicate { hash });
                    }
                    r.db.execute(
                        "INSERT INTO events(hash, id, scope, author, hlc, kind, version, epoch, control, status, bytes) VALUES (?, ?, ?, NULL, 0, 0, ?, 0, 0, ?, ?)",
                        params![&hash.0[..], &id.0[..], &scope.0[..], version as i64, status::UNSUPPORTED, bytes],
                    )?;
                    r.pending_events.push((scope, hash, bytes));
                    Ok(IngestOutcome::Stored { scope, hash, author: None, valid: false })
                });
            }
            Decoded::Known(e) => e,
        };
        let hash = e.hash();
        let scope = e.scope;
        let author = e.author;
        if !self.scope_exists(scope)? {
            return Ok(IngestOutcome::NotJoined { scope });
        }
        // The hash covers the signed bytes, so a stored hash is this event, already verified.
        // Peers resend what they have to every neighbour; skipping the check here is most of
        // the cost of a catch-up among several devices.
        if self.event_exists(&hash)? {
            return Ok(IngestOutcome::Duplicate { hash });
        }
        if !e.verify_signature() {
            return Ok(IngestOutcome::Invalid("bad signature".into()));
        }
        let held: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM held WHERE hash = ?)", [&hash.0[..]], |r| r.get(0))?;
        if held {
            let missing = self.auth_mut(scope)?.missing_parents(&e.control_parents);
            return Ok(IngestOutcome::Held { scope, hash, missing });
        }
        self.tx(|r| {
            let out = match r.accept(e)? {
                Accepted::Duplicate => IngestOutcome::Duplicate { hash },
                Accepted::Held(missing) => {
                    if !r.hold(scope, hash, bytes, &missing, peer)? {
                        return Ok(IngestOutcome::Invalid("held limit reached".into()));
                    }
                    IngestOutcome::Held { scope, hash, missing }
                }
                Accepted::Stored(v) => {
                    r.release_held(hash)?;
                    IngestOutcome::Stored { scope, hash, author: Some(author), valid: v.is_valid() }
                }
            };
            Ok(out)
        })
    }

    /// Parks an event until its control parents arrive. Returns false when the scope or the
    /// sending peer is over its limit; the event is dropped and can be fetched again later.
    fn hold(&mut self, scope: ScopeId, hash: EventHash, bytes: &[u8], missing: &[EventHash], peer: Option<DeviceKey>) -> Result<bool> {
        let (max_n, max_bytes) = (self.cfg.held_max_events, self.cfg.held_max_bytes);
        let (n, size): (i64, i64) =
            self.db.query_row("SELECT COUNT(*), COALESCE(SUM(length(bytes)), 0) FROM held WHERE scope = ?", [&scope.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if n as usize >= max_n || size as usize + bytes.len() > max_bytes {
            return Ok(false);
        }
        if let Some(p) = peer {
            let (n, size): (i64, i64) =
                self.db.query_row("SELECT COUNT(*), COALESCE(SUM(length(bytes)), 0) FROM held WHERE peer = ?", [&p.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if n as usize >= max_n || size as usize + bytes.len() > max_bytes {
                return Ok(false);
            }
        }
        self.db.execute(
            "INSERT OR IGNORE INTO held(hash, scope, bytes, received_ms, peer) VALUES (?, ?, ?, ?, ?)",
            params![&hash.0[..], &scope.0[..], bytes, self.env.now_ms() as i64, peer.map(|p| p.0.to_vec())],
        )?;
        let mut stmt = self.db.prepare_cached("INSERT OR IGNORE INTO held_needs(missing, hash) VALUES (?, ?)")?;
        for m in missing {
            stmt.execute([&m.0[..], &hash.0[..]])?;
        }
        Ok(true)
    }

    /// Accepts the held events that were waiting only for `stored`, then those waiting for them.
    fn release_held(&mut self, stored: EventHash) -> Result<()> {
        let mut ready = vec![stored];
        while let Some(parent) = ready.pop() {
            let waiting: Vec<Vec<u8>> = {
                let mut stmt = self.db.prepare_cached("SELECT hash FROM held_needs WHERE missing = ?")?;
                stmt.query_map([&parent.0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
            };
            if waiting.is_empty() {
                continue;
            }
            self.db.execute("DELETE FROM held_needs WHERE missing = ?", [&parent.0[..]])?;
            for h in waiting {
                let still: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM held_needs WHERE hash = ?)", [&h], |r| r.get(0))?;
                if still {
                    continue;
                }
                let row: Option<(Vec<u8>, Option<Vec<u8>>)> =
                    self.db.query_row("SELECT bytes, peer FROM held WHERE hash = ?", [&h], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                let Some((b, peer)) = row else { continue };
                self.db.execute("DELETE FROM held WHERE hash = ?", [&h])?;
                let Ok(Decoded::Known(e)) = Envelope::decode(&b) else { continue };
                let (scope, hash) = (e.scope, e.hash());
                match self.accept(e)? {
                    Accepted::Stored(_) => ready.push(hash),
                    Accepted::Held(missing) => {
                        let peer = peer.and_then(|p| <[u8; 32]>::try_from(p.as_slice()).ok()).map(DeviceKey);
                        self.hold(scope, hash, &b, &missing, peer)?;
                    }
                    Accepted::Duplicate => {}
                }
            }
        }
        Ok(())
    }

    /// Drops held events older than `HELD_TTL_MS`. Returns how many were dropped.
    pub(crate) fn expire_held(&mut self) -> Result<usize> {
        let cutoff = self.env.now_ms().saturating_sub(HELD_TTL_MS) as i64;
        let n = self.db.execute("DELETE FROM held WHERE received_ms < ?", [cutoff])?;
        if n > 0 {
            self.db.execute("DELETE FROM held_needs WHERE hash NOT IN (SELECT hash FROM held)", [])?;
        }
        Ok(n)
    }

    pub fn held_missing(&mut self, scope: ScopeId) -> Result<Vec<EventHash>> {
        let rows: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT DISTINCT n.missing FROM held_needs n JOIN held h ON h.hash = n.hash WHERE h.scope = ?")?;
            stmt.query_map([&scope.0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        Ok(rows.into_iter().filter_map(|b| <[u8; 32]>::try_from(b.as_slice()).ok()).map(EventHash).filter(|h| !self.event_exists(h).unwrap_or(false)).collect())
    }

    /// Current DAG heads: (control heads, other heads).
    pub fn scope_heads(&self, scope: ScopeId) -> Result<(Vec<EventHash>, Vec<EventHash>)> {
        self.heads(scope)
    }

    /// Reconciliation keys in `[lo, hi)` for the control or content layer.
    pub fn recon_keys(&self, scope: ScopeId, control: bool, lo: u64, hi: u64) -> Result<Vec<ReconKey>> {
        let mut stmt = self.db.prepare_cached("SELECT hlc, id FROM events WHERE scope = ? AND control = ? AND hlc >= ? AND hlc < ? ORDER BY hlc, id")?;
        let rows = stmt.query_map(params![&scope.0[..], control as i64, lo as i64, hi.min(i64::MAX as u64) as i64], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (h, id) = row?;
            if let Some(id) = EventId::from_slice(&id) {
                out.push((h, id));
            }
        }
        Ok(out)
    }

    /// Encoded events by id, control events first, then by (HLC, id).
    pub fn events_by_ids(&self, scope: ScopeId, ids: &[EventId]) -> Result<Vec<Vec<u8>>> {
        let mut rows: Vec<(i64, i64, Vec<u8>, Vec<u8>)> = Vec::new();
        let mut stmt = self.db.prepare_cached("SELECT control, hlc, id, bytes FROM events WHERE scope = ? AND id = ?")?;
        for id in ids {
            for r in stmt.query_map(params![&scope.0[..], &id.0[..]], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))? {
                rows.push(r?);
            }
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        Ok(rows.into_iter().map(|r| r.3).collect())
    }

    pub fn events_by_hashes(&self, hashes: &[EventHash]) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut stmt = self.db.prepare_cached("SELECT bytes FROM events WHERE hash = ?")?;
        for h in hashes {
            if let Some(b) = stmt.query_row([&h.0[..]], |r| r.get::<_, Vec<u8>>(0)).optional()? {
                out.push(b);
            }
        }
        Ok(out)
    }

    /// Whether a device may receive this scope's events: a certified, unrevoked device acting for
    /// a member.
    pub fn is_authorized(&mut self, scope: ScopeId, device: &DeviceKey) -> Result<bool> {
        let st = self.auth_mut(scope)?.full_state();
        Ok(!st.acting(device).is_empty())
    }

    /// Whether a device may receive the scope's control events: any device ever certified in it
    /// and not revoked. Removed members learn of their removal this way; content after the
    /// removal is encrypted with keys they never receive.
    pub fn may_read_control(&mut self, scope: ScopeId, device: &DeviceKey) -> Result<bool> {
        let st = self.auth_mut(scope)?.full_state();
        Ok(st.certs.contains_key(device) && !st.revoked.contains(device))
    }

    /// Whether a peer may sync with this device at all: one of this user's devices, or a device
    /// of a user that is a member of (or was certified in) a scope held here. Anyone else may
    /// only pair or redeem an invite.
    pub fn is_trusted_peer(&mut self, cert: &DeviceCert) -> Result<bool> {
        if !cert.verify() || cert.device == self.device_key() || self.is_device_revoked(&cert.device)? || self.is_retired(&cert.user)? {
            return Ok(false);
        }
        if cert.user == self.user_key() {
            return Ok(true);
        }
        let user = crate::ids::Principal::User(cert.user);
        for scope in self.scope_ids()? {
            let st = self.auth_mut(scope)?.full_state();
            if st.revoked.contains(&cert.device) {
                continue;
            }
            if st.members.contains_key(&user) || st.certs.contains_key(&cert.device) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether an issued invite is still open, so an unknown device may connect to redeem it.
    pub fn has_open_invites(&self) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM invites_issued WHERE expires_ms > ?)", [self.env.now_ms() as i64], |r| r.get(0))?)
    }

    /// Whether a peer presenting this certificate would be admitted automatically.
    pub fn would_admit(&mut self, scope: ScopeId, cert: &DeviceCert) -> Result<bool> {
        let st = self.auth_mut(scope)?.full_state();
        Ok(cert.verify() && !st.revoked.contains(&cert.device) && st.members.contains_key(&crate::ids::Principal::User(cert.user)))
    }

    /// A peer stored these events; clear them from the outbox.
    pub fn ack(&mut self, hashes: &[EventHash]) -> Result<()> {
        let mut changed_scopes = Vec::new();
        self.tx(|r| {
            for h in hashes {
                let row: Option<(Vec<u8>, Vec<u8>)> =
                    r.db.query_row("SELECT scope, id FROM outbox WHERE hash = ?", [&h.0[..]], |x| Ok((x.get(0)?, x.get(1)?))).optional()?;
                let Some((scope, id)) = row else { continue };
                r.db.execute("DELETE FROM outbox WHERE hash = ?", [&h.0[..]])?;
                let n =
                    r.db.execute("UPDATE messages_view SET status = ? WHERE id = ? AND status = ?", params![delivery::DELIVERED, &id, delivery::PENDING])?;
                if n > 0
                    && let Some(id) = EventId::from_slice(&id)
                {
                    r.emit_message_changed(&id)?;
                }
                if let Some(s) = ScopeId::from_slice(&scope)
                    && !changed_scopes.contains(&s)
                {
                    changed_scopes.push(s);
                }
            }
            for s in &changed_scopes {
                r.emit(Change::SyncStatusChanged(*s));
            }
            Ok(())
        })
    }

    pub fn record_contact(&mut self, device: &DeviceKey, scope: Option<ScopeId>) -> Result<()> {
        let now = self.env.now_ms() as i64;
        if let Some(scope) = scope {
            self.db.execute(
                "INSERT INTO peers(device, scope, last_contact_ms) VALUES (?, ?, ?) ON CONFLICT(device, scope) DO UPDATE SET last_contact_ms = excluded.last_contact_ms",
                params![&device.0[..], &scope.0[..], now],
            )?;
        }
        self.db.execute("UPDATE known_devices SET last_seen_ms = ? WHERE device = ?", params![now, &device.0[..]])?;
        Ok(())
    }

    pub fn peer_contacts(&self, scope: ScopeId) -> Result<Vec<(DeviceKey, u64)>> {
        let mut stmt = self.db.prepare_cached("SELECT device, last_contact_ms FROM peers WHERE scope = ?")?;
        let rows = stmt.query_map([&scope.0[..]], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (d, t) = row?;
            if let Some(d) = DeviceKey::from_slice(&d) {
                out.push((d, t as u64));
            }
        }
        Ok(out)
    }

    /// Devices this replica should try to reach: certified devices in any of its scopes.
    pub fn known_peer_devices(&mut self) -> Result<Vec<DeviceKey>> {
        let me = self.device_key();
        let mut out = Vec::new();
        for scope in self.scope_ids()? {
            let st = self.auth_mut(scope)?.full_state();
            for d in st.certs.keys() {
                if *d != me && !st.revoked.contains(d) && !out.contains(d) {
                    out.push(*d);
                }
            }
        }
        Ok(out)
    }

    pub fn history_mode(&self, scope: ScopeId) -> Result<HistoryMode> {
        let row: Option<(i64, i64, i64)> = self
            .db
            .query_row("SELECT history_mode, window_days, storage_role FROM scopes WHERE scope = ?", [&scope.0[..]], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        Ok(match row {
            Some((_, _, 1)) => HistoryMode::Full,
            Some((1, days, _)) => HistoryMode::Recent { days: days as u32 },
            Some(_) => HistoryMode::Full,
            None => self.cfg.default_history(),
        })
    }

    pub fn set_history_mode(&mut self, scope: Option<ScopeId>, mode: HistoryMode) -> Result<()> {
        let (m, d) = match mode {
            HistoryMode::Full => (0i64, 0i64),
            HistoryMode::Recent { days } => (1, days as i64),
        };
        match scope {
            Some(s) => self.db.execute("UPDATE scopes SET history_mode = ?, window_days = ? WHERE scope = ?", params![m, d, &s.0[..]])?,
            None => {
                self.cfg.history_mode = Some(mode);
                self.db.execute("UPDATE scopes SET history_mode = ?, window_days = ?", params![m, d])?
            }
        };
        Ok(())
    }

    /// Records that every event at or above `hlc` has been reconciled with some peer.
    pub fn mark_synced_from(&mut self, scope: ScopeId, hlc: u64) -> Result<()> {
        self.db.execute(
            "UPDATE scopes SET synced_from_hlc = CASE WHEN synced_from_hlc IS NULL OR synced_from_hlc > ?1 THEN ?1 ELSE synced_from_hlc END, last_sync_ms = ?2 WHERE scope = ?3",
            params![hlc as i64, self.env.now_ms() as i64, &scope.0[..]],
        )?;
        self.changes.push(Change::SyncStatusChanged(scope));
        Ok(())
    }

    pub fn mark_synced(&mut self, scope: ScopeId) -> Result<()> {
        self.db.execute("UPDATE scopes SET last_sync_ms = ? WHERE scope = ?", params![self.env.now_ms() as i64, &scope.0[..]])?;
        self.changes.push(Change::SyncStatusChanged(scope));
        Ok(())
    }

    /// Lowest HLC among locally stored messages, or `None` when the scope has none.
    pub fn oldest_message_hlc(&self, scope: ScopeId) -> Result<Option<u64>> {
        let v: Option<i64> = self.db.query_row("SELECT MIN(hlc) FROM events WHERE scope = ? AND control = 0", [&scope.0[..]], |r| r.get(0))?;
        Ok(v.map(|v| v as u64))
    }

    pub fn verdict_of(&mut self, hash: &EventHash) -> Result<Option<Verdict>> {
        let row: Option<(i64, Option<i64>)> =
            self.db.query_row("SELECT status, reason FROM events WHERE hash = ?", [&hash.0[..]], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        Ok(row.map(|(s, r)| {
            if s == status::REJECTED {
                Verdict::Rejected(r.and_then(crate::auth::Reason::from_code).unwrap_or(crate::auth::Reason::NotMember))
            } else {
                Verdict::Valid
            }
        }))
    }

    pub fn is_device_revoked(&self, device: &DeviceKey) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM revoked_devices WHERE device = ?)", [&device.0[..]], |r| r.get(0))?)
    }

    pub(crate) fn user_secret(&self) -> &crate::keys::UserSecret {
        &self.user
    }

    /// Scopes that reference a blob.
    pub fn blob_scopes(&self, hash: &[u8; 32]) -> Result<Vec<ScopeId>> {
        let mut stmt = self.db.prepare_cached("SELECT DISTINCT scope FROM blob_refs WHERE hash = ?")?;
        let rows: Vec<Vec<u8>> = stmt.query_map([&hash[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows.into_iter().filter_map(|b| ScopeId::from_slice(&b)).collect())
    }

    /// Hashes of every stored event in a scope (for tests and diagnostics).
    pub fn all_hashes(&self, scope: ScopeId) -> Result<Vec<EventHash>> {
        let mut stmt = self.db.prepare_cached("SELECT hash FROM events WHERE scope = ? ORDER BY hash")?;
        let rows: Vec<Vec<u8>> = stmt.query_map([&scope.0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows.into_iter().filter_map(|b| EventHash::from_slice(&b)).collect())
    }
}
