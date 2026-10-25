//! Data management: leaving a scope, storage usage, export, and wiping a device.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use rusqlite::Connection;
use serde_json::json;

use super::{DEVICE_SECRET, Replica, USER_SECRET, USER_SECRET_NEXT, VIEW_KEY};
use crate::error::{Error, Result};
use crate::hlc;
use crate::ids::{DeviceKey, Principal, Role, ScopeId};
use crate::payload::{Control, MemberRemovePayload};
use crate::store::replication;
use crate::types::DeliveryStatus;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageUsage {
    /// The SQLite database with its write-ahead log.
    pub database_bytes: u64,
    /// Stored events of synced scopes, a share of `database_bytes`.
    pub synced_event_bytes: u64,
    /// Stored events of device-local scopes, a share of `database_bytes`.
    pub local_event_bytes: u64,
    /// Encrypted attachments, including partial downloads.
    pub blob_bytes: u64,
    /// Decrypted copies of opened attachments. Evicted automatically past the configured cap.
    pub attachment_cache_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportSummary {
    pub messages: u32,
    pub attachments: u32,
    /// Attachments not downloaded to this device, listed in the export without a file.
    pub missing_attachments: u32,
}

/// App data for an export: JSON as it is, anything else as hex.
fn app_data_json<'a>(items: impl Iterator<Item = (&'a String, &'a Vec<u8>)>) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (ns, bytes) in items {
        let v = serde_json::from_slice::<serde_json::Value>(bytes).unwrap_or_else(|_| json!({ "hex": hex::encode(bytes) }));
        out.insert(ns.clone(), v);
    }
    serde_json::Value::Object(out)
}

impl Replica {
    /// Leaves a scope: removes this user as a member and deletes the local copy. Other members
    /// keep theirs. The copy stays, hidden, until another device has acknowledged the removal,
    /// so it isn't lost in transit. The last owner of a scope with other members has to make
    /// someone else an owner first. In a scope this user no longer belongs to, only the local
    /// copy is deleted.
    ///
    /// This user's other devices stop listing the scope but keep their copies until they leave
    /// it too.
    pub fn leave_scope(&mut self, scope: ScopeId) -> Result<()> {
        if !self.scope_exists(scope)? {
            return Err(Error::NotFound("scope".into()));
        }
        let me = self.me();
        let device = self.device_key();
        let st = self.auth_mut(scope)?.full_state();
        let member = !self.is_self_revoked() && st.max_role(&device).is_some() && st.members.contains_key(&me);
        let mut tell = false;
        if member {
            let others = st.members.keys().any(|p| *p != me && matches!(p, Principal::User(_)));
            let other_owner = st.members.iter().any(|(p, r)| *p != me && *r == Role::Owner && matches!(p, Principal::User(_)));
            if st.members.get(&me) == Some(&Role::Owner) && others && !other_owner {
                return Err(Error::PermissionDenied("the last owner can't leave; make another member an owner first".into()));
            }
            tell = st.certs.keys().any(|d| *d != device);
            let mine: HashSet<DeviceKey> = st.devices_of(&self.user_key()).into_iter().collect();
            let remaining: Vec<DeviceKey> = st.certs.keys().copied().filter(|d| !mine.contains(d) && !st.acting(d).is_empty()).collect();
            let mine: Vec<DeviceKey> = mine.into_iter().collect();
            self.tx(|r| {
                // Remaining members get a key this user's devices don't have.
                if !remaining.is_empty() {
                    r.rotate_key_to(scope, &remaining)?;
                }
                let cutoffs = r.cutoffs_of(scope, &mine)?;
                let c = Control::MemberRemove(MemberRemovePayload { subject: me, cutoffs });
                let (id, hash) = r.append(scope, c.kind(), 0, c.encode(), None, None)?;
                // Control events aren't tracked for delivery; this one is, so the purge waits for it.
                if tell {
                    r.db.execute(
                        "INSERT OR IGNORE INTO outbox(hash, scope, id, created_ms) VALUES (?, ?, ?, ?)",
                        rusqlite::params![&hash.0[..], &scope.0[..], &id.0[..], r.env.now_ms() as i64],
                    )?;
                }
                Ok(())
            })?;
        }
        self.db.execute("INSERT OR REPLACE INTO left_scopes(scope, left_ms) VALUES (?, ?)", rusqlite::params![&scope.0[..], self.env.now_ms() as i64])?;
        if tell {
            self.leaving = true;
            self.finish_leaving()?;
        } else {
            self.purge_scope(scope)?;
        }
        Ok(())
    }

    /// Whether this device left the scope (it may still be waiting to purge it).
    pub fn has_left(&self, scope: ScopeId) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM left_scopes WHERE scope = ?)", [&scope.0[..]], |r| r.get(0))?)
    }

    /// Purges left scopes whose removal another device has acknowledged. Returns how many.
    pub(crate) fn finish_leaving(&mut self) -> Result<u32> {
        if !self.leaving {
            return Ok(0);
        }
        let scopes: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT scope FROM left_scopes WHERE scope IN (SELECT scope FROM scopes)")?;
            stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        let mut n = 0;
        let mut waiting = false;
        for s in scopes {
            let Some(scope) = ScopeId::from_slice(&s) else { continue };
            if self.pending_outbox(scope)? == 0 {
                self.purge_scope(scope)?;
                n += 1;
            } else {
                waiting = true;
            }
        }
        self.leaving = waiting;
        Ok(n)
    }

    pub fn storage_usage(&self) -> Result<StorageUsage> {
        let dir = &self.cfg.data_dir;
        let database_bytes = ["sync.db", "sync.db-wal", "sync.db-shm"].iter().filter_map(|f| fs::metadata(dir.join(f)).ok()).map(|m| m.len()).sum();
        let (blobs, cache) = self.blobs.usage();
        let (synced, local): (i64, i64) = self.db.query_row(
            "SELECT COALESCE(SUM(CASE WHEN s.replication = ?1 THEN 0 ELSE length(e.bytes) END), 0),
                    COALESCE(SUM(CASE WHEN s.replication = ?1 THEN length(e.bytes) ELSE 0 END), 0)
             FROM events e JOIN scopes s ON s.scope = e.scope",
            [replication::DEVICE_LOCAL],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(StorageUsage {
            database_bytes,
            synced_event_bytes: synced as u64,
            local_event_bytes: local as u64,
            blob_bytes: blobs,
            attachment_cache_bytes: cache,
        })
    }

    /// Writes a scope to `dir` as `conversation.json`, with its attachments under
    /// `attachments/`. The directory is created; existing files with the same names are
    /// overwritten. Only what this device holds is exported: history outside its window, or
    /// compacted away, isn't included.
    pub fn export_conversation(&mut self, scope: ScopeId, dir: &Path) -> Result<ExportSummary> {
        let c = self.conversation(scope)?.ok_or_else(|| Error::NotFound("conversation".into()))?;
        let page = self.messages(scope, None, u32::MAX)?;
        let files = dir.join("attachments");
        fs::create_dir_all(&files)?;
        let mut summary = ExportSummary::default();
        let mut messages = Vec::new();
        for m in &page.messages {
            let mut attachments = Vec::new();
            for (i, b) in m.blobs.iter().enumerate() {
                let path = if b.available { self.blob_path(scope, &b.hash)? } else { None };
                let file = match path {
                    Some(src) => {
                        let safe: String = b.name.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect();
                        let name = format!("{}-{i}-{safe}", &m.id.to_hex()[..12]);
                        fs::copy(&src, files.join(&name))?;
                        summary.attachments += 1;
                        Some(format!("attachments/{name}"))
                    }
                    None => {
                        summary.missing_attachments += 1;
                        None
                    }
                };
                attachments.push(json!({ "name": b.name, "media_type": b.media_type, "size": b.size, "file": file }));
            }
            let status = match &m.status {
                DeliveryStatus::Delivered => "delivered".to_string(),
                DeliveryStatus::Pending => "pending".to_string(),
                DeliveryStatus::Rejected(r) => format!("rejected: {r:?}"),
            };
            messages.push(json!({
                "id": m.id.to_hex(),
                "time_ms": hlc::physical_ms(m.hlc),
                "hlc": m.hlc,
                "author": m.author.to_hex(),
                "author_device": self.device_name(&m.author_device)?.unwrap_or_default(),
                "is_agent": m.is_agent,
                "role": format!("{:?}", m.role).to_lowercase(),
                "content": m.content,
                "edited": m.edited,
                "reply_to": m.reply_to.map(|r| r.to_hex()),
                "tool_call_id": m.tool_call_id,
                "tool_name": m.tool_name,
                "status": status,
                "reactions": m.reactions.iter().map(|r| json!({ "reaction": r.reaction, "count": r.count })).collect::<Vec<_>>(),
                "attachments": attachments,
                "app_data": app_data_json(m.app_data.iter().map(|a| (&a.namespace, &a.bytes))),
            }));
            summary.messages += 1;
        }
        let doc = json!({
            "format": "osaurus-sync-conversation",
            "version": 1,
            "exported_ms": self.env.now_ms(),
            "scope": scope.to_hex(),
            "kind": format!("{:?}", c.kind).to_lowercase(),
            "title": c.title,
            "pinned": c.pinned,
            "archived": c.archived,
            "members": c.members.iter().map(|m| json!({ "principal": m.principal.to_hex(), "role": format!("{:?}", m.role).to_lowercase() })).collect::<Vec<_>>(),
            "complete": !page.at_sync_boundary && self.compacted_events(scope)? == 0,
            "tags": c.tags.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<String, serde_json::Value>>(),
            "app_data": app_data_json(c.app_data.iter().map(|(n, b)| (n, b))),
            "messages": messages,
        });
        fs::write(dir.join("conversation.json"), serde_json::to_vec_pretty(&doc).map_err(|e| Error::Internal(e.to_string()))?)?;
        Ok(summary)
    }

    /// Key store entries a replica may create.
    pub fn secret_names() -> &'static [&'static str] {
        &[DEVICE_SECRET, USER_SECRET, USER_SECRET_NEXT, VIEW_KEY]
    }

    /// Signs out of the current identity but keeps this device's conversations. The user
    /// identity is replaced (as when a device is stolen), this user's other devices are revoked
    /// here, and every conversation this device owned becomes device-local under the new
    /// identity. Conversations owned by others are left. The other devices keep their own
    /// copies; pairing again shares the conversations that are synced from then on.
    pub fn sign_out_keeping_data(&mut self) -> Result<()> {
        if self.is_self_revoked() {
            return Err(Error::DeviceRevoked);
        }
        let others: Vec<DeviceKey> = self.my_other_devices()?.into_iter().map(|c| c.device).collect();
        let report = self.rotate_identity(&others)?;
        self.tx(|r| {
            for scope in &report.rotated {
                r.db.execute("UPDATE scopes SET replication = ? WHERE scope = ?", rusqlite::params![replication::DEVICE_LOCAL, &scope.0[..]])?;
                r.db.execute("DELETE FROM outbox WHERE scope = ?", [&scope.0[..]])?;
                r.refresh_conversation(*scope)?;
            }
            r.db.execute_batch("DELETE FROM peers; DELETE FROM storage_peers;")?;
            Ok(())
        })?;
        for scope in report.left {
            self.purge_scope(scope)?;
        }
        Ok(())
    }

    /// Signs this device out for good: deletes its key store entries and its data directory.
    /// The replica is unusable afterwards. Other devices keep their data.
    pub fn wipe(&mut self) -> Result<()> {
        self.checkpointer.close();
        let _ = self.flush();
        // Release the database files so they can be deleted (Windows refuses open files).
        self.db = Connection::open_in_memory()?;
        self.discard_caches();
        self._lock = None;
        for name in Self::secret_names() {
            self.secrets.delete(name)?;
        }
        let dir = self.cfg.data_dir.clone();
        for f in ["sync.db", "sync.db-wal", "sync.db-shm", super::VIEW_KEY_SEALED, super::UNCLEAN, ".lock"] {
            match fs::remove_file(dir.join(f)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        for d in ["blobs", super::LOCKED_DIR] {
            match fs::remove_dir_all(dir.join(d)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        // Remove the directory itself if nothing else lives there.
        let _ = fs::remove_dir(&dir);
        Ok(())
    }
}
