//! Applying valid events to the views. Every rule here is order independent: messages form a
//! grow-only set, edits are last-writer-wins by (HLC, author), tombstones win over their target
//! and its edits, reactions are an observed-remove set and Loro merges itself. Any delivery order
//! therefore produces the same views.

use rusqlite::{OptionalExtension, params};

use super::{Replica, ctl_event};
use crate::auth::{CtlEvent, Reason, Verdict};
use crate::envelope::{Decoded, Envelope, kind};
use crate::error::Result;
use crate::ids::{EventHash, EventId, Principal, Role, ScopeId};
use crate::payload::{self, BlobRef, Content, Control, MessagePayload, ReactionOp};
use crate::store::{delivery, purged, status};
use crate::types::Change;

impl Replica {
    pub(crate) fn apply_control(&mut self, e: &Envelope, ctl: &CtlEvent) -> Result<()> {
        let scope = e.scope;
        match &ctl.body {
            Some(Control::Genesis(g)) => {
                self.upsert_known_device(&g.owner_device)?;
                self.db.execute(
                    "INSERT OR IGNORE INTO conversations_view(scope, kind, created_hlc) VALUES (?, ?, ?)",
                    params![&scope.0[..], g.scope_kind as i64, e.hlc as i64],
                )?;
                self.refresh_conversation(scope)?;
            }
            Some(Control::MemberAdd(_)) | Some(Control::MemberRemove(_)) | Some(Control::RoleChange(_)) | Some(Control::AgentAssign(_)) => {
                self.refresh_conversation(scope)?;
            }
            Some(Control::DeviceCert(c)) => {
                self.upsert_known_device(c)?;
                self.emit(Change::DevicesChanged);
                self.refresh_conversation(scope)?;
            }
            Some(Control::DeviceRevoke(p)) => {
                if p.device == self.device_key() {
                    crate::store::set_meta(&self.db, "self_revoked", &[1])?;
                    self.emit(Change::SelfRevoked);
                }
                let mine = self.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM known_devices WHERE device = ? AND user = ?)",
                    params![&p.device.0[..], &self.user_key().0[..]],
                    |r| r.get::<_, bool>(0),
                )?;
                if mine {
                    self.db.execute(
                        "INSERT OR IGNORE INTO revoked_devices(device, revoked_ms) VALUES (?, ?)",
                        params![&p.device.0[..], self.env.now_ms() as i64],
                    )?;
                }
                self.emit(Change::DevicesChanged);
                self.refresh_conversation(scope)?;
            }
            Some(Control::KeyUpdate(_)) => {
                let mut sk = self.scope_keys(scope)?.clone();
                if self.unseal_grants(e, &mut sk) {
                    self.keys.insert(scope, sk);
                    self.drain_pending_decrypt(scope)?;
                }
            }
            Some(Control::Invite(_)) => {}
            None if e.kind == kind::LORO_UPDATE => {
                let acting = self.auth_mut(scope)?.state_at(&e.control_parents).acting(&e.author);
                self.apply_content(e, ctl.hash, &acting)?;
            }
            None => {}
        }
        Ok(())
    }

    fn drain_pending_decrypt(&mut self, scope: ScopeId) -> Result<()> {
        let hashes: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT hash FROM pending_decrypt WHERE scope = ?")?;
            stmt.query_map([&scope.0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        for h in hashes {
            let Some(hash) = EventHash::from_slice(&h) else { continue };
            let Some(e) = self.load_event(&hash)? else { continue };
            if self.decrypt(&e)?.is_some() {
                self.db.execute("DELETE FROM pending_decrypt WHERE hash = ?", [&h])?;
                let acting = self.auth_mut(scope)?.state_at(&e.control_parents).acting(&e.author);
                self.apply_content(&e, hash, &acting)?;
            }
        }
        Ok(())
    }

    /// Applies a valid content event (or a valid Loro update).
    pub(crate) fn apply_content(&mut self, e: &Envelope, hash: EventHash, acting: &[(Principal, Role)]) -> Result<()> {
        let Some(pt) = self.decrypt(e)? else {
            if e.payload_bytes().is_some() {
                self.db.execute(
                    "INSERT OR IGNORE INTO pending_decrypt(hash, scope, epoch) VALUES (?, ?, ?)",
                    params![&hash.0[..], &e.scope.0[..], e.epoch as i64],
                )?;
            }
            return Ok(());
        };
        let content = match Content::decode(e.kind, &pt) {
            Ok(Some(c)) => c,
            _ => return Ok(()),
        };
        let user = acting.iter().filter_map(|(p, _)| matches!(p, Principal::User(_)).then_some(*p)).min();
        let principal = user.or_else(|| acting.first().map(|(p, _)| *p));
        let Some(principal) = principal else { return Ok(()) };
        let is_admin = acting.iter().any(|(_, r)| *r >= Role::Admin);
        match content {
            Content::Message(m) => self.apply_message(e, hash, principal, acting, m, delivery::DELIVERED, None)?,
            Content::Edit(p) => {
                if self.tombstoned(&p.target)? {
                    self.purge_event(&hash)?;
                    return Ok(());
                }
                self.db.execute(
                    "INSERT OR IGNORE INTO edits(hash, scope, target, hlc, author_device, principal, content) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![&hash.0[..], &e.scope.0[..], &p.target.0[..], e.hlc as i64, &e.author.0[..], principal.to_hex(), p.content],
                )?;
                if self.reapply_best_edit(&p.target)? {
                    self.emit_message_changed(&p.target)?;
                }
            }
            Content::Tombstone(p) => {
                self.db.execute(
                    "INSERT OR IGNORE INTO tombstones(target, hash, scope, principal, admin) VALUES (?, ?, ?, ?, ?)",
                    params![&p.target.0[..], &hash.0[..], &e.scope.0[..], principal.to_hex(), is_admin as i64],
                )?;
                let target: Option<String> =
                    self.db.query_row("SELECT principal FROM messages_view WHERE id = ?", [&p.target.0[..]], |r| r.get(0)).optional()?;
                if let Some(tp) = target
                    && (is_admin || tp == principal.to_hex())
                {
                    self.purge_message(&p.target)?;
                    self.emit(Change::MessageRemoved { scope: e.scope, id: p.target });
                }
            }
            Content::Reaction(p) => {
                let ph = principal.to_hex();
                match &p.op {
                    ReactionOp::Add => {
                        let removed: bool = self.db.query_row(
                            "SELECT EXISTS(SELECT 1 FROM reaction_removed WHERE add_id = ? AND principal = ?)",
                            params![&e.id.0[..], ph],
                            |r| r.get(0),
                        )?;
                        if !removed {
                            self.db.execute(
                                "INSERT OR IGNORE INTO reactions(add_id, scope, target, reaction, principal) VALUES (?, ?, ?, ?, ?)",
                                params![&e.id.0[..], &e.scope.0[..], &p.target.0[..], p.reaction, ph],
                            )?;
                        }
                    }
                    ReactionOp::Remove { observed } => {
                        for o in observed {
                            self.db.execute(
                                "INSERT OR IGNORE INTO reaction_removed(add_id, principal, scope) VALUES (?, ?, ?)",
                                params![&o.0[..], ph, &e.scope.0[..]],
                            )?;
                            self.db.execute("DELETE FROM reactions WHERE add_id = ? AND principal = ?", params![&o.0[..], ph])?;
                        }
                    }
                }
                self.emit_message_changed(&p.target)?;
            }
            Content::Loro(p) => {
                self.apply_loro(e.scope, &p)?;
                self.refresh_conversation(e.scope)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_message(
        &mut self,
        e: &Envelope,
        hash: EventHash,
        mut principal: Principal,
        acting: &[(Principal, Role)],
        m: MessagePayload,
        st: i64,
        reason: Option<Reason>,
    ) -> Result<()> {
        let mut st = st;
        let mut reason = reason;
        if let Some(agent) = m.as_agent {
            let hosted = acting.iter().any(|(p, _)| *p == Principal::Agent(agent))
                && !self.auth_mut(e.scope)?.agent_removed_outside(&agent, &e.control_parents, e.author, e.hlc);
            if hosted {
                principal = Principal::Agent(agent);
            } else if st != delivery::REJECTED {
                self.set_status(&hash, Verdict::Rejected(Reason::AgentNotHosted))?;
                if e.author != self.device_key() {
                    return Ok(());
                }
                st = delivery::REJECTED;
                reason = Some(Reason::AgentNotHosted);
                principal = Principal::Agent(agent);
            }
        }
        // A tombstone that arrived first wins if its author may delete this message.
        let ph = principal.to_hex();
        let stones: Vec<(String, bool)> = {
            let mut stmt = self.db.prepare_cached("SELECT principal, admin FROM tombstones WHERE target = ?")?;
            stmt.query_map([&e.id.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        if stones.iter().any(|(p, admin)| *admin || *p == ph) {
            self.purge_event(&hash)?;
            self.delete_blobs(&m.blobs)?;
            return Ok(());
        }
        let pending: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE hash = ?)", [&hash.0[..]], |r| r.get(0))?;
        if st == delivery::DELIVERED && pending {
            st = delivery::PENDING;
        }
        for b in &m.blobs {
            self.db.execute(
                "INSERT OR IGNORE INTO blobs(hash, size, media_type, available) VALUES (?, ?, ?, ?)",
                params![&b.hash[..], b.size as i64, b.media_type, self.blobs.has(&b.hash) as i64],
            )?;
            self.db.execute("INSERT OR IGNORE INTO blob_refs(hash, scope, event) VALUES (?, ?, ?)", params![&b.hash[..], &e.scope.0[..], &e.id.0[..]])?;
        }
        let hlc = m.imported_at_ms.map(|_| e.hlc).unwrap_or(e.hlc);
        self.db.execute(
            "INSERT OR REPLACE INTO messages_view(id, scope, hash, hlc, author_device, principal, is_agent, role, content, blobs, reply_to, tool_call_id, tool_name, status, reason)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                &e.id.0[..],
                &e.scope.0[..],
                &hash.0[..],
                hlc as i64,
                &e.author.0[..],
                ph,
                matches!(principal, Principal::Agent(_)) as i64,
                m.role as i64,
                m.content,
                payload::encode(&m.blobs),
                m.reply_to.map(|r| r.0.to_vec()),
                m.tool_call_id,
                m.tool_name,
                st,
                reason.map(|r| r.code()),
            ],
        )?;
        let rowid = self.db.last_insert_rowid();
        self.db.execute("INSERT INTO messages_fts(rowid, content) VALUES (?, ?)", params![rowid, m.content])?;
        self.reapply_best_edit(&e.id)?;
        if let Some(v) = self.message_view(&e.id)? {
            self.emit(Change::MessageAdded(v));
        }
        Ok(())
    }

    /// Records the author's own rejected message so the app can show it as "Not delivered".
    pub(crate) fn apply_rejected_own(&mut self, e: &Envelope, hash: EventHash, reason: Reason) -> Result<()> {
        if e.author != self.device_key() || e.kind != kind::MESSAGE {
            return Ok(());
        }
        let Some(pt) = self.decrypt(e)? else { return Ok(()) };
        if let Ok(Some(Content::Message(m))) = Content::decode(kind::MESSAGE, &pt) {
            let me = self.me();
            self.apply_message(e, hash, me, &[], m, delivery::REJECTED, Some(reason))?;
        }
        Ok(())
    }

    fn tombstoned(&self, target: &EventId) -> Result<bool> {
        let row: Option<String> = self.db.query_row("SELECT principal FROM messages_view WHERE id = ?", [&target.0[..]], |r| r.get(0)).optional()?;
        let stones: Vec<(String, bool)> = {
            let mut stmt = self.db.prepare_cached("SELECT principal, admin FROM tombstones WHERE target = ?")?;
            stmt.query_map([&target.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        Ok(match row {
            Some(p) => stones.iter().any(|(sp, admin)| *admin || *sp == p),
            // Target not here: either never arrived or already purged. A purged target leaves its
            // tombstone, which is what matters for edits arriving late.
            None => !stones.is_empty() && self.purged_target(target)?,
        })
    }

    fn purged_target(&self, target: &EventId) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE id = ? AND purged = ?)", params![&target.0[..], purged::DELETED], |r| r.get(0))?)
    }

    /// Sets the message content to its latest valid edit. Returns true if the view changed.
    fn reapply_best_edit(&mut self, target: &EventId) -> Result<bool> {
        let row: Option<(String, Option<i64>, Option<Vec<u8>>, i64)> = self
            .db
            .query_row("SELECT principal, edit_hlc, edit_author, rowid FROM messages_view WHERE id = ?", [&target.0[..]], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let Some((principal, cur_hlc, cur_author, rowid)) = row else { return Ok(false) };
        let best: Option<(String, i64, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT content, hlc, author_device FROM edits WHERE target = ? AND principal = ? ORDER BY hlc DESC, author_device DESC LIMIT 1",
                params![&target.0[..], principal],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((content, hlc, author)) = best else { return Ok(false) };
        if cur_hlc == Some(hlc) && cur_author.as_deref() == Some(&author[..]) {
            return Ok(false);
        }
        self.db.execute(
            "UPDATE messages_view SET content = ?, edited = 1, edit_hlc = ?, edit_author = ? WHERE id = ?",
            params![content, hlc, author, &target.0[..]],
        )?;
        self.db.execute("DELETE FROM messages_fts WHERE rowid = ?", [rowid])?;
        self.db.execute("INSERT INTO messages_fts(rowid, content) VALUES (?, ?)", params![rowid, content])?;
        Ok(true)
    }

    pub(crate) fn emit_message_changed(&mut self, id: &EventId) -> Result<()> {
        if let Some(v) = self.message_view(id)? {
            self.emit(Change::MessageChanged(v));
        }
        Ok(())
    }

    /// Removes the target's plaintext and payload, its edits' payloads, its reactions and its blobs.
    fn purge_message(&mut self, target: &EventId) -> Result<()> {
        let row: Option<(i64, Vec<u8>, Vec<u8>)> = self
            .db
            .query_row("SELECT rowid, hash, blobs FROM messages_view WHERE id = ?", [&target.0[..]], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        if let Some((rowid, hash, blobs)) = row {
            self.db.execute("DELETE FROM messages_fts WHERE rowid = ?", [rowid])?;
            self.db.execute("DELETE FROM messages_view WHERE id = ?", [&target.0[..]])?;
            if let Some(h) = EventHash::from_slice(&hash) {
                self.purge_event(&h)?;
            }
            if let Ok(refs) = payload::decode::<Vec<BlobRef>>(&blobs) {
                self.delete_blobs(&refs)?;
            }
        }
        let edit_hashes: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT hash FROM edits WHERE target = ?")?;
            stmt.query_map([&target.0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        for h in edit_hashes {
            if let Some(h) = EventHash::from_slice(&h) {
                self.purge_event(&h)?;
            }
        }
        self.db.execute("DELETE FROM edits WHERE target = ?", [&target.0[..]])?;
        self.db.execute("DELETE FROM reactions WHERE target = ?", [&target.0[..]])?;
        Ok(())
    }

    fn delete_blobs(&mut self, refs: &[BlobRef]) -> Result<()> {
        for b in refs {
            self.blobs.delete(&b.hash);
            self.db.execute("UPDATE blobs SET available = 0 WHERE hash = ?", [&b.hash[..]])?;
        }
        Ok(())
    }

    /// Replaces an event's stored bytes with its payload-free form. It still verifies.
    pub(crate) fn purge_event(&mut self, hash: &EventHash) -> Result<()> {
        if let Some(e) = self.load_event(hash)?
            && e.payload_bytes().is_some()
        {
            self.db.execute("UPDATE events SET bytes = ?, purged = ? WHERE hash = ?", params![e.purged().encode(), purged::DELETED, &hash.0[..]])?;
        }
        Ok(())
    }

    /// Recomputes the verdict of every content event in a scope and rebuilds its views. Runs only
    /// when membership changed in a way that can invalidate earlier events.
    pub(crate) fn revalidate_scope(&mut self, scope: ScopeId) -> Result<()> {
        let rows: Vec<(Vec<u8>, i64)> = {
            let mut stmt = self.db.prepare_cached("SELECT bytes, status FROM events WHERE scope = ? AND control = 0 AND status != ?")?;
            stmt.query_map(params![&scope.0[..], status::UNSUPPORTED], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        for (b, old) in rows {
            let Ok(Decoded::Known(e)) = crate::envelope::Envelope::decode(&b) else { continue };
            if !kind::is_known(e.kind) {
                continue;
            }
            let (v, _) = self.auth_mut(scope)?.validate_content(e.author, e.hlc, &e.control_parents, e.epoch);
            let new = if v.is_valid() { status::VALID } else { status::REJECTED };
            if new != old {
                self.set_status(&e.hash(), v)?;
            }
        }
        self.rebuild_scope_views(scope)?;
        self.emit(Change::ScopeReset(scope));
        Ok(())
    }

    pub(crate) fn rebuild_scope_views(&mut self, scope: ScopeId) -> Result<()> {
        let s = &scope.0[..];
        self.db.execute("DELETE FROM messages_fts WHERE rowid IN (SELECT rowid FROM messages_view WHERE scope = ?)", [s])?;
        for t in ["messages_view", "edits", "tombstones", "reactions", "reaction_removed", "pending_decrypt", "loro_snapshots"] {
            self.db.execute(&format!("DELETE FROM {t} WHERE scope = ?"), [s])?;
        }
        self.loro.remove(&scope);
        self.keys.remove(&scope);
        self.auth.remove(&scope);

        let ctl: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT bytes FROM events WHERE scope = ? AND control = 1 AND status = ? ORDER BY seq")?;
            stmt.query_map(params![s, status::VALID], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        for b in ctl {
            if let Ok(Decoded::Known(e)) = Envelope::decode(&b) {
                let c = ctl_event(&e);
                if e.kind == kind::LORO_UPDATE {
                    continue;
                }
                self.apply_control(&e, &c)?;
            }
        }
        // Loro documents load their valid updates on first use.
        let _ = self.loro_doc(scope)?;

        let content: Vec<(Vec<u8>, i64, Option<i64>)> = {
            let mut stmt =
                self.db.prepare_cached("SELECT bytes, status, reason FROM events WHERE scope = ? AND control = 0 AND status != ? ORDER BY hlc, id")?;
            stmt.query_map(params![s, status::UNSUPPORTED], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?
        };
        for (b, st, reason) in content {
            let Ok(Decoded::Known(e)) = Envelope::decode(&b) else { continue };
            let hash = e.hash();
            if st == status::VALID {
                let acting = self.auth_mut(scope)?.state_at(&e.control_parents).acting(&e.author);
                self.apply_content(&e, hash, &acting)?;
            } else if e.kind == kind::TOMBSTONE {
                let acting = self.auth_mut(scope)?.causal_acting(e.author, &e.control_parents, e.epoch);
                if !acting.is_empty() {
                    self.apply_content(&e, hash, &acting)?;
                }
            } else if let Some(r) = reason.and_then(Reason::from_code) {
                self.apply_rejected_own(&e, hash, r)?;
            }
        }
        self.refresh_conversation(scope)?;
        Ok(())
    }

    pub(crate) fn rebuild_all_views(&mut self) -> Result<()> {
        let scopes: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare("SELECT scope FROM scopes")?;
            stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        self.tx(|r| {
            for s in scopes {
                if let Some(scope) = ScopeId::from_slice(&s) {
                    r.rebuild_scope_views(scope)?;
                }
            }
            Ok(())
        })?;
        self.take_changes();
        Ok(())
    }

    pub(crate) fn refresh_conversation(&mut self, scope: ScopeId) -> Result<()> {
        let exists: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM conversations_view WHERE scope = ?)", [&scope.0[..]], |r| r.get(0))?;
        if !exists {
            // Genesis may arrive later during gradual sync; create the row when it does.
            let st = self.auth_mut(scope)?.full_state();
            if st.genesis.is_none() {
                return Ok(());
            }
            self.db.execute("INSERT OR IGNORE INTO conversations_view(scope, kind) VALUES (?, 0)", [&scope.0[..]])?;
        }
        let meta = self.scope_meta(scope)?;
        let st = self.auth_mut(scope)?.full_state();
        let members: Vec<(String, i64)> = st.members.iter().map(|(p, r)| (p.to_hex(), *r as i64)).collect();
        let my_role = st.max_role(&self.device_key()).or_else(|| st.members.get(&self.me()).copied());
        self.db.execute(
            "UPDATE conversations_view SET title = ?, pinned = ?, sort_order = ?, archived = ?, my_role = ?, members = ? WHERE scope = ?",
            params![meta.title, meta.pinned as i64, meta.sort_order, meta.archived as i64, my_role.map(|r| r as i64), payload::encode(&members), &scope.0[..]],
        )?;
        match self.conversation(scope)? {
            Some(c) if c.my_role.is_some() => self.emit(Change::ConversationChanged(c)),
            Some(_) => self.emit(Change::ConversationRemoved(scope)),
            None => {}
        }
        Ok(())
    }
}
