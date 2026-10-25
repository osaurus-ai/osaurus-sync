//! Local operations. Each runs in one transaction and maps to one or more signed events.

use std::collections::BTreeSet;

use loro::LoroValue;
use rusqlite::{OptionalExtension, params};

use super::Replica;
use crate::crypto;
use crate::env::random_array;
use crate::envelope::kind;
use crate::error::{Error, Result};
use crate::identity::{AgentBinding, DeviceCert};
use crate::ids::{AgentId, DeviceKey, EventId, Principal, Role, ScopeId, UserKey};
use crate::keys::UserSecret;
use crate::payload::{
    AppDataPayload, BlobRef, Content, Control, DeviceRevokePayload, EditPayload, GenesisPayload, InvitePayload, KeyGrant, KeyUpdatePayload, MemberAddPayload,
    MemberRemovePayload, MessagePayload, MessageRole, ReactionOp, ReactionPayload, RoleChangePayload, ScopeKind, TombstonePayload,
};
use crate::store::replication;
use crate::types::{AppDataWrite, Attachment, Change, ImportOptions, ImportedMessage, NewMessage, Replication};

/// Longest app data namespace, in bytes.
const MAX_NAMESPACE_BYTES: usize = 128;
/// Longest conversation tag key or value, in bytes.
const MAX_TAG_BYTES: usize = 256;
const MAX_TAGS: usize = 64;

/// A one-time invitation to a scope. Encoded into a link or QR code by the app.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InviteToken {
    pub scope: ScopeId,
    #[serde(with = "crate::ids::fixed")]
    pub token: [u8; 32],
    pub inviter: DeviceKey,
    pub role: Role,
}

impl InviteToken {
    pub fn encode(&self) -> String {
        format!("osaurus-invite:1:{}", hex::encode(crate::payload::encode(self)))
    }

    pub fn decode(s: &str) -> Result<Self> {
        let hexed = s.trim().strip_prefix("osaurus-invite:1:").ok_or_else(|| Error::InvalidInput("not an invite".into()))?;
        let bytes = hex::decode(hexed).map_err(|_| Error::InvalidInput("bad invite encoding".into()))?;
        crate::payload::decode(&bytes)
    }

    pub fn token_hash(&self) -> [u8; 32] {
        crypto::hash(&self.token)
    }
}

impl Replica {
    fn new_event_id(&self) -> EventId {
        EventId::new_v7(self.env.now_ms(), random_array(self.env.as_ref()))
    }

    /// Devices of my user that I know and that are not revoked, excluding this one.
    pub(super) fn my_other_devices(&self) -> Result<Vec<DeviceCert>> {
        let mut stmt =
            self.db.prepare_cached("SELECT cert FROM known_devices WHERE user = ? AND device != ? AND device NOT IN (SELECT device FROM revoked_devices)")?;
        let rows: Vec<Vec<u8>> =
            stmt.query_map(params![&self.user_key().0[..], &self.device_key().0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows.into_iter().filter_map(|b| crate::payload::decode::<DeviceCert>(&b).ok()).filter(|c| c.verify()).collect())
    }

    pub(super) fn grants_for(&self, devices: &[DeviceKey], keys: &[(u64, [u8; 32])]) -> Result<Vec<KeyGrant>> {
        let mut out = Vec::new();
        for d in devices {
            for (epoch, k) in keys {
                out.push(KeyGrant { epoch: *epoch, key_check: crypto::key_check(k), device: *d, sealed: crypto::seal_to_device(self.env.as_ref(), &d.0, k)? });
            }
        }
        Ok(out)
    }

    /// Creates a conversation or workspace scope owned by this user.
    pub fn create_scope(&mut self, kind_: ScopeKind, title: &str) -> Result<ScopeId> {
        self.create_scope_with(kind_, title, Replication::Synced)
    }

    /// Creates a scope that syncs, or one that never leaves this device.
    pub fn create_scope_with(&mut self, kind_: ScopeKind, title: &str, replication: Replication) -> Result<ScopeId> {
        self.tx(|r| r.create_scope_inner(kind_, title, None, replication))
    }

    fn create_scope_inner(&mut self, kind_: ScopeKind, title: &str, created_ms: Option<u64>, repl: Replication) -> Result<ScopeId> {
        let id = self.new_event_id();
        let scope = ScopeId::from(id);
        self.join_scope(scope)?;
        let local = repl == Replication::DeviceLocal;
        // A scope created here is complete by definition.
        self.db.execute(
            "UPDATE scopes SET synced_from_hlc = 0, replication = ? WHERE scope = ?",
            params![if local { replication::DEVICE_LOCAL } else { replication::SYNCED }, &scope.0[..]],
        )?;
        let genesis = Control::Genesis(GenesisPayload {
            owner: self.user_key(),
            owner_device: self.cert.clone(),
            scope_kind: kind_,
            created_ms: created_ms.unwrap_or_else(|| self.env.now_ms()),
        });
        self.append(scope, kind::GENESIS, 0, genesis.encode(), Some(id), None)?;
        let others = if local { vec![] } else { self.my_other_devices()? };
        for c in &others {
            self.append_control(scope, &Control::DeviceCert(c.clone()))?;
        }
        let key: [u8; 32] = random_array(self.env.as_ref());
        let mut devices = vec![self.device_key(), self.user.recovery_public()];
        devices.extend(others.iter().map(|c| c.device));
        let grants = self.grants_for(&devices, &[(1, key)])?;
        self.append_control(scope, &Control::KeyUpdate(KeyUpdatePayload { grants }))?;
        if !title.is_empty() {
            self.loro_write(scope, |doc| doc.get_map("meta").insert("title", title).map(|_| ()))?;
        }
        self.refresh_conversation(scope)?;
        Ok(scope)
    }

    /// Imports an existing local conversation, keeping original timestamps for display order.
    pub fn import_conversation(&mut self, title: &str, created_ms: u64, messages: &[ImportedMessage]) -> Result<ScopeId> {
        self.import_conversation_with(&ImportOptions { title: title.into(), created_ms, ..Default::default() }, messages)
    }

    /// Imports a legacy conversation with its metadata, attachments and app data in one
    /// transaction, so an interrupted import leaves nothing behind.
    pub fn import_conversation_with(&mut self, opts: &ImportOptions, messages: &[ImportedMessage]) -> Result<ScopeId> {
        for m in messages {
            for w in &m.app_data {
                self.check_app_data(&w.namespace, &w.bytes)?;
            }
        }
        for (ns, b) in &opts.app_data {
            self.check_app_data(ns, b)?;
        }
        check_tags(&opts.tags)?;
        self.tx(|r| {
            let scope = r.create_scope_inner(ScopeKind::Conversation, &opts.title, Some(opts.created_ms), opts.replication)?;
            if opts.pinned || opts.archived || !opts.tags.is_empty() || !opts.app_data.is_empty() {
                r.loro_write(scope, |doc| {
                    let meta = doc.get_map("meta");
                    if opts.pinned {
                        meta.insert("pinned", true)?;
                    }
                    if opts.archived {
                        meta.insert("archived", true)?;
                    }
                    let tags = doc.get_map("tags");
                    for (k, v) in &opts.tags {
                        tags.insert(k, v.as_str())?;
                    }
                    let data = doc.get_map("app_data");
                    for (ns, b) in &opts.app_data {
                        data.insert(ns, LoroValue::Binary(b.clone().into()))?;
                    }
                    Ok(())
                })?;
                r.refresh_conversation(scope)?;
            }
            // Order by the original times, but never past now: a later edit must sort after
            // the message it changes, even if the legacy clock ran fast.
            let now = r.env.now_ms();
            for (i, m) in messages.iter().enumerate() {
                let hlc = crate::hlc::pack(m.created_ms.min(now), (i % 0xffff) as u16);
                let mut payload = r.message_payload(m.role, &m.content, &m.attachments)?;
                payload.tool_call_id = m.tool_call_id.clone();
                payload.tool_name = m.tool_name.clone();
                payload.imported_at_ms = Some(m.created_ms);
                let id = r.append_content(scope, &Content::Message(payload), None, Some(hlc))?;
                for w in &m.app_data {
                    r.append_app_data(scope, id, w, Some(hlc))?;
                }
            }
            Ok(scope)
        })
    }

    /// Encrypts attachments into the blob store and spills a long tool result into a blob.
    fn message_payload(&mut self, role: MessageRole, content: &str, attachments: &[Attachment]) -> Result<MessagePayload> {
        let mut blobs = Vec::new();
        for a in attachments {
            blobs.push(self.blobs.import(self.env.as_ref(), &a.path, &a.media_type, &a.name)?);
        }
        let mut content = content.to_owned();
        if role == MessageRole::ToolResult && content.len() > self.cfg.tool_output_blob_threshold {
            let b = self.blobs.import_bytes(self.env.as_ref(), content.as_bytes(), "text/plain", "tool-output.txt")?;
            let mut cut = 512.min(content.len());
            while !content.is_char_boundary(cut) {
                cut -= 1;
            }
            content = format!("{}…", &content[..cut]);
            blobs.push(b);
        }
        Ok(MessagePayload { role, content, blobs, reply_to: None, as_agent: None, tool_call_id: None, tool_name: None, imported_at_ms: None })
    }

    pub fn send_message(&mut self, scope: ScopeId, msg: NewMessage) -> Result<EventId> {
        if let Some(agent) = msg.as_agent
            && self.agent_host(scope, &agent)? != Some(self.device_key())
        {
            return Err(Error::PermissionDenied("this device does not host the agent".into()));
        }
        for w in &msg.app_data {
            self.check_app_data(&w.namespace, &w.bytes)?;
        }
        self.tx(|r| {
            let mut payload = r.message_payload(msg.role, &msg.content, &msg.attachments)?;
            payload.reply_to = msg.reply_to;
            payload.as_agent = msg.as_agent;
            payload.tool_call_id = msg.tool_call_id.clone();
            payload.tool_name = msg.tool_name.clone();
            let id = r.append_content(scope, &Content::Message(payload), msg.id, None)?;
            for w in &msg.app_data {
                r.append_app_data(scope, id, w, None)?;
            }
            Ok(id)
        })
    }

    // ---- app data ----

    fn check_app_data(&self, namespace: &str, bytes: &[u8]) -> Result<()> {
        if namespace.is_empty() || namespace.len() > MAX_NAMESPACE_BYTES {
            return Err(Error::InvalidInput(format!("app data namespace must be 1 to {MAX_NAMESPACE_BYTES} bytes")));
        }
        if bytes.len() > self.cfg.max_app_data_bytes {
            return Err(Error::InvalidInput(format!(
                "app data for {namespace} is {} bytes; the limit is {}. Store large values as app data attachments",
                bytes.len(),
                self.cfg.max_app_data_bytes
            )));
        }
        Ok(())
    }

    fn append_app_data(&mut self, scope: ScopeId, target: EventId, w: &AppDataWrite, hlc: Option<u64>) -> Result<()> {
        let mut blobs: Vec<BlobRef> = Vec::new();
        for a in &w.attachments {
            blobs.push(self.blobs.import(self.env.as_ref(), &a.path, &a.media_type, &a.name)?);
        }
        let p = AppDataPayload { target, namespace: w.namespace.clone(), bytes: w.bytes.clone(), blobs };
        self.append_content(scope, &Content::AppData(p), None, hlc)?;
        Ok(())
    }

    /// Replaces app data on one of this user's own messages, one namespace per write. Empty
    /// bytes without attachments clear the namespace.
    pub fn set_message_app_data(&mut self, target: EventId, writes: &[AppDataWrite]) -> Result<()> {
        let (scope, principal) = self.message_scope_and_principal(&target)?;
        if principal != self.me().to_hex() {
            return Err(Error::PermissionDenied("only the author can set a message's app data".into()));
        }
        for w in writes {
            self.check_app_data(&w.namespace, &w.bytes)?;
        }
        self.tx(|r| {
            for w in writes {
                r.append_app_data(scope, target, w, None)?;
            }
            Ok(())
        })
    }

    /// Sets (or with `None`, removes) one namespace of conversation-level app data.
    pub fn set_conversation_app_data(&mut self, scope: ScopeId, namespace: &str, bytes: Option<&[u8]>) -> Result<()> {
        self.check_app_data(namespace, bytes.unwrap_or_default())?;
        let ns = namespace.to_owned();
        let value = bytes.map(|b| LoroValue::Binary(b.to_vec().into()));
        self.tx(|r| {
            r.loro_write(scope, |doc| {
                let m = doc.get_map("app_data");
                match value {
                    Some(v) => m.insert(&ns, v),
                    None => m.delete(&ns),
                }
            })
        })
    }

    /// Replaces the conversation's tags. Tags are indexed for `conversations_filtered` and sync
    /// like the title.
    pub fn set_conversation_tags(&mut self, scope: ScopeId, tags: &[(String, String)]) -> Result<()> {
        check_tags(tags)?;
        let current = self.scope_meta(scope)?.tags;
        let removed: Vec<String> = current.iter().filter(|(k, _)| !tags.iter().any(|(t, _)| t == k)).map(|(k, _)| k.clone()).collect();
        let changed: Vec<(String, String)> = tags.iter().filter(|t| !current.contains(t)).cloned().collect();
        if removed.is_empty() && changed.is_empty() {
            return Ok(());
        }
        self.tx(|r| {
            r.loro_write(scope, |doc| {
                let m = doc.get_map("tags");
                for k in &removed {
                    m.delete(k)?;
                }
                for (k, v) in &changed {
                    m.insert(k, v.as_str())?;
                }
                Ok(())
            })
        })
    }

    // ---- device-local scopes ----

    fn require_synced(&self, scope: ScopeId) -> Result<()> {
        if self.is_device_local(scope)? {
            return Err(Error::PermissionDenied("a device-local conversation can't be shared".into()));
        }
        Ok(())
    }

    /// Deletes a device-local conversation outright. Synced ones are left with `leave_scope`.
    pub fn delete_local_scope(&mut self, scope: ScopeId) -> Result<()> {
        if !self.scope_exists(scope)? {
            return Err(Error::NotFound("scope".into()));
        }
        if !self.is_device_local(scope)? {
            return Err(Error::InvalidInput("only a device-local conversation can be deleted; leave a synced one".into()));
        }
        self.purge_scope(scope)
    }

    /// Starts syncing a device-local scope with this user's other devices. Its history goes
    /// with it.
    pub fn promote_to_synced(&mut self, scope: ScopeId) -> Result<()> {
        if !self.is_device_local(scope)? {
            return Ok(());
        }
        if self.my_role(scope)? != Some(Role::Owner) {
            return Err(Error::PermissionDenied("only the owner can sync a conversation".into()));
        }
        self.tx(|r| {
            r.db.execute("UPDATE scopes SET replication = ? WHERE scope = ?", params![replication::SYNCED, &scope.0[..]])?;
            let others = r.my_other_devices()?;
            let keys = r.all_keys(scope)?;
            for c in &others {
                r.append_control(scope, &Control::DeviceCert(c.clone()))?;
            }
            if !others.is_empty() && !keys.is_empty() {
                let devices: Vec<DeviceKey> = others.iter().map(|c| c.device).collect();
                let grants = r.grants_for(&devices, &keys)?;
                r.append_control(scope, &Control::KeyUpdate(KeyUpdatePayload { grants }))?;
            }
            r.refresh_conversation(scope)
        })
    }

    fn message_scope_and_principal(&self, target: &EventId) -> Result<(ScopeId, String)> {
        let row: Option<(Vec<u8>, String)> =
            self.db.query_row("SELECT scope, principal FROM messages_view WHERE id = ?", [&target.0[..]], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (s, p) = row.ok_or_else(|| Error::NotFound("message".into()))?;
        Ok((ScopeId::from_slice(&s).ok_or_else(|| Error::Storage("bad scope".into()))?, p))
    }

    /// Edits one of this user's own messages.
    pub fn edit_message(&mut self, target: EventId, content: &str) -> Result<()> {
        let (scope, principal) = self.message_scope_and_principal(&target)?;
        if principal != self.me().to_hex() {
            return Err(Error::PermissionDenied("only the author can edit a message".into()));
        }
        self.tx(|r| {
            r.append_content(scope, &Content::Edit(EditPayload { target, content: content.into() }), None, None)?;
            Ok(())
        })
    }

    /// Deletes a message on all conforming peers. Authors may delete their own; Admins and Owners
    /// may delete any.
    pub fn delete_message(&mut self, target: EventId) -> Result<()> {
        let (scope, principal) = self.message_scope_and_principal(&target)?;
        let role = self.my_role(scope)?;
        if principal != self.me().to_hex() && role < Some(Role::Admin) {
            return Err(Error::PermissionDenied("only the author or an admin can delete a message".into()));
        }
        self.tx(|r| {
            r.append_content(scope, &Content::Tombstone(TombstonePayload { target }), None, None)?;
            Ok(())
        })
    }

    pub fn rename_conversation(&mut self, scope: ScopeId, title: &str) -> Result<()> {
        let title = title.to_owned();
        self.tx(|r| r.loro_write(scope, |doc| doc.get_map("meta").insert("title", title.as_str()).map(|_| ())))
    }

    pub fn set_pinned(&mut self, scope: ScopeId, pinned: bool) -> Result<()> {
        self.tx(|r| r.loro_write(scope, |doc| doc.get_map("meta").insert("pinned", pinned).map(|_| ())))
    }

    pub fn set_archived(&mut self, scope: ScopeId, archived: bool) -> Result<()> {
        self.tx(|r| r.loro_write(scope, |doc| doc.get_map("meta").insert("archived", archived).map(|_| ())))
    }

    /// Sets the conversation's position. Apps use fractional values between neighbours.
    pub fn set_sort_order(&mut self, scope: ScopeId, order: f64) -> Result<()> {
        self.tx(|r| r.loro_write(scope, |doc| doc.get_map("meta").insert("order", LoroValue::Double(order)).map(|_| ())))
    }

    pub fn set_agent_state(&mut self, scope: ScopeId, agent: AgentId, state: &str) -> Result<()> {
        let key = agent.to_hex();
        let state = state.to_owned();
        self.tx(|r| r.loro_write(scope, |doc| doc.get_map("agent_state").insert(&key, state.as_str()).map(|_| ())))
    }

    pub fn add_reaction(&mut self, target: EventId, reaction: &str) -> Result<()> {
        let (scope, _) = self.message_scope_and_principal(&target)?;
        self.tx(|r| {
            r.append_content(scope, &Content::Reaction(ReactionPayload { target, reaction: reaction.into(), op: ReactionOp::Add }), None, None)?;
            Ok(())
        })
    }

    pub fn remove_reaction(&mut self, target: EventId, reaction: &str) -> Result<()> {
        let (scope, _) = self.message_scope_and_principal(&target)?;
        let observed: Vec<EventId> = {
            let mut stmt = self.db.prepare_cached("SELECT add_id FROM reactions WHERE target = ? AND reaction = ? AND principal = ?")?;
            stmt.query_map(params![&target.0[..], reaction, self.me().to_hex()], |r| r.get::<_, Vec<u8>>(0))?
                .filter_map(|b| b.ok().and_then(|b| EventId::from_slice(&b)))
                .collect()
        };
        if observed.is_empty() {
            return Ok(());
        }
        self.tx(|r| {
            r.append_content(
                scope,
                &Content::Reaction(ReactionPayload { target, reaction: reaction.into(), op: ReactionOp::Remove { observed } }),
                None,
                None,
            )?;
            Ok(())
        })
    }

    pub fn mark_read(&mut self, scope: ScopeId, hlc: u64) -> Result<()> {
        self.db.execute(
            "INSERT INTO read_state(scope, last_read_hlc) VALUES (?, ?) ON CONFLICT(scope) DO UPDATE SET last_read_hlc = MAX(last_read_hlc, excluded.last_read_hlc)",
            params![&scope.0[..], hlc as i64],
        )?;
        if let Some(c) = self.conversation(scope)? {
            self.changes.push(Change::ConversationChanged(c));
        }
        Ok(())
    }

    // ---- membership ----

    /// Adds a user to a scope directly. Their devices are admitted when they connect.
    pub fn add_member(&mut self, scope: ScopeId, user: UserKey, role: Role) -> Result<()> {
        self.require_synced(scope)?;
        self.tx(|r| {
            r.append_control(scope, &Control::MemberAdd(MemberAddPayload { subject: Principal::User(user), role, invite: None }))?;
            Ok(())
        })
    }

    pub fn change_role(&mut self, scope: ScopeId, subject: Principal, role: Role) -> Result<()> {
        self.tx(|r| {
            r.append_control(scope, &Control::RoleChange(RoleChangePayload { subject, role }))?;
            Ok(())
        })
    }

    pub(super) fn max_hlc_of(&self, scope: ScopeId, device: &DeviceKey) -> Result<Option<u64>> {
        let v: Option<i64> =
            self.db.query_row("SELECT MAX(hlc) FROM events WHERE scope = ? AND author = ?", params![&scope.0[..], &device.0[..]], |r| r.get(0))?;
        Ok(v.map(|v| v as u64))
    }

    /// Removes a member. They keep what they already received and lose access to anything new.
    pub fn remove_member(&mut self, scope: ScopeId, subject: Principal) -> Result<()> {
        self.tx(|r| {
            let st = r.auth_mut(scope)?.full_state();
            let devices: Vec<DeviceKey> = match subject {
                Principal::User(u) => st.devices_ever_of(&u),
                Principal::Agent(a) => st.agent_hosts.get(&a).map(|b| b.host_device).into_iter().collect(),
            };
            let mut cutoffs = Vec::new();
            for d in devices {
                if let Some(h) = r.max_hlc_of(scope, &d)? {
                    cutoffs.push((d, h));
                }
            }
            r.append_control(scope, &Control::MemberRemove(MemberRemovePayload { subject, cutoffs }))?;
            if matches!(subject, Principal::User(_)) {
                r.rotate_key(scope)?;
            }
            Ok(())
        })
    }

    /// Starts a new epoch whose key only remaining member devices receive.
    pub(crate) fn rotate_key(&mut self, scope: ScopeId) -> Result<()> {
        let st = self.auth_mut(scope)?.full_state();
        let mut devices: Vec<DeviceKey> = st.certs.keys().copied().filter(|d| !st.acting(d).is_empty()).collect();
        // Other members' recovery grants are added by their own devices (`repair_recovery_grants`).
        if st.members.contains_key(&self.me()) {
            devices.push(self.user.recovery_public());
        }
        self.rotate_key_to(scope, &devices)
    }

    /// Starts a new epoch whose key only `devices` receive.
    pub(super) fn rotate_key_to(&mut self, scope: ScopeId, devices: &[DeviceKey]) -> Result<()> {
        let next = self.current_key(scope)?.map(|(e, _)| e + 1).unwrap_or(1);
        let key: [u8; 32] = random_array(self.env.as_ref());
        let grants = self.grants_for(devices, &[(next, key)])?;
        self.append_control(scope, &Control::KeyUpdate(KeyUpdatePayload { grants }))?;
        Ok(())
    }

    /// Revokes a lost or retired device in every scope where it is certified, and rotates keys.
    /// The device keeps the user secret, so this only stops a device that cooperates; a stolen
    /// device needs `rotate_identity`.
    pub fn revoke_device(&mut self, device: DeviceKey) -> Result<()> {
        if device == self.device_key() {
            return Err(Error::InvalidInput("cannot revoke this device from itself".into()));
        }
        let scopes = self.scope_ids()?;
        self.tx(|r| {
            for scope in scopes {
                let st = r.auth_mut(scope)?.full_state();
                if !st.certs.contains_key(&device) || st.revoked.contains(&device) {
                    continue;
                }
                let cutoff = r.max_hlc_of(scope, &device)?;
                r.append_control(scope, &Control::DeviceRevoke(DeviceRevokePayload { device, cutoff }))?;
                r.rotate_key(scope)?;
            }
            r.db.execute("INSERT OR IGNORE INTO revoked_devices(device, revoked_ms) VALUES (?, ?)", params![&device.0[..], r.env.now_ms() as i64])?;
            r.emit(Change::DevicesChanged);
            Ok(())
        })
    }

    /// Publishes a valid certificate for a member's device and shares every key this device holds.
    /// Returns the scopes the device was admitted to.
    pub fn admit_device(&mut self, cert: &DeviceCert) -> Result<Vec<ScopeId>> {
        if !cert.verify() || cert.device == self.device_key() {
            return Ok(vec![]);
        }
        let revoked: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM revoked_devices WHERE device = ?)", [&cert.device.0[..]], |r| r.get(0))?;
        if revoked {
            return Ok(vec![]);
        }
        let scopes = self.scope_ids()?;
        let me = self.device_key();
        self.tx(|r| {
            r.upsert_known_device(cert)?;
            let mut admitted = Vec::new();
            for scope in scopes {
                if r.is_device_local(scope)? {
                    continue;
                }
                let st = r.auth_mut(scope)?.full_state();
                if st.revoked.contains(&cert.device) || st.max_role(&me).is_none() || !st.members.contains_key(&Principal::User(cert.user)) {
                    continue;
                }
                let has_cert = st.certs.get(&cert.device).is_some_and(|u| u.contains_key(&cert.user));
                if !has_cert {
                    r.append_control(scope, &Control::DeviceCert(cert.clone()))?;
                }
                if !r.device_has_all_keys(scope, &cert.device)? {
                    let keys = r.all_keys(scope)?;
                    if !keys.is_empty() {
                        let grants = r.grants_for(&[cert.device], &keys)?;
                        r.append_control(scope, &Control::KeyUpdate(KeyUpdatePayload { grants }))?;
                    }
                }
                if !has_cert {
                    admitted.push(scope);
                }
            }
            Ok(admitted)
        })
    }

    /// Admits a known device of this user, such as one recovered from the phrase, into every
    /// scope this device can reach.
    pub fn approve_device(&mut self, device: DeviceKey) -> Result<Vec<ScopeId>> {
        let cert: Option<Vec<u8>> = self
            .db
            .query_row("SELECT cert FROM known_devices WHERE device = ? AND user = ?", params![&device.0[..], &self.user_key().0[..]], |r| r.get(0))
            .optional()?;
        let cert: DeviceCert = crate::payload::decode(&cert.ok_or_else(|| Error::NotFound("device".into()))?)?;
        self.admit_device(&cert)
    }

    /// A fresh UUIDv7 for a message whose id must be known before it is committed, such as a
    /// streamed response.
    pub fn allocate_message_id(&self) -> EventId {
        self.new_event_id()
    }

    fn device_has_all_keys(&mut self, scope: ScopeId, device: &DeviceKey) -> Result<bool> {
        let mine: BTreeSet<[u8; 8]> = self.all_keys(scope)?.iter().map(|(_, k)| crypto::key_check(k)).collect();
        Ok(mine.is_subset(&self.granted_checks(scope, device)?))
    }

    /// Key checks of every key granted to `device` (or a recovery key) in the scope.
    fn granted_checks(&self, scope: ScopeId, device: &DeviceKey) -> Result<BTreeSet<[u8; 8]>> {
        let mut theirs = BTreeSet::new();
        let rows: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT bytes FROM events WHERE scope = ? AND kind = ? AND status = 0")?;
            stmt.query_map(params![&scope.0[..], kind::KEY_UPDATE as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        for b in rows {
            if let Ok(crate::envelope::Decoded::Known(e)) = crate::envelope::Envelope::decode(&b)
                && let Some(Ok(Some(Control::KeyUpdate(ku)))) = e.payload_bytes().map(|p| Control::decode(kind::KEY_UPDATE, p))
            {
                theirs.extend(ku.grants.iter().filter(|g| g.device == *device).map(|g| g.key_check));
            }
        }
        Ok(theirs)
    }

    /// Seals every key this device holds to its user's recovery key where that grant is missing,
    /// so the recovery phrase alone decrypts the scope. Returns whether a `key_update` was added.
    pub fn repair_recovery_grants(&mut self, scope: ScopeId) -> Result<bool> {
        let n = self.all_keys(scope)?.len();
        if n == 0 || self.recovery_checked.get(&scope) == Some(&n) || self.is_self_revoked() {
            return Ok(false);
        }
        let me = self.device_key();
        let st = self.auth_mut(scope)?.full_state();
        if !st.members.contains_key(&self.me()) || st.max_role(&me).is_none() {
            return Ok(false);
        }
        let recovery = self.user.recovery_public();
        let have = self.granted_checks(scope, &recovery)?;
        let missing: Vec<(u64, [u8; 32])> = self.all_keys(scope)?.into_iter().filter(|(_, k)| !have.contains(&crypto::key_check(k))).collect();
        let added = !missing.is_empty();
        if added {
            let grants = self.grants_for(&[recovery], &missing)?;
            self.tx(|r| r.append_control(scope, &Control::KeyUpdate(KeyUpdatePayload { grants })))?;
        }
        self.recovery_checked.insert(scope, n);
        Ok(added)
    }

    /// Takes on the identity of a restored database: certifies this device in the scopes its
    /// user belongs to, decrypts history with the recovery grants, and repairs missing grants.
    pub(crate) fn finish_recovery(&mut self) -> Result<()> {
        let me = self.device_key();
        let user = self.user_key();
        let cert = self.cert.clone();
        let scopes = self.scope_ids()?;
        self.keys.clear();
        self.recovery_checked.clear();
        self.tx(|r| {
            for &scope in &scopes {
                let st = r.auth_mut(scope)?.full_state();
                let member = st.members.contains_key(&Principal::User(user));
                let certified = st.certs.get(&me).is_some_and(|u| u.contains_key(&user));
                if member && !certified && !st.revoked.contains(&me) {
                    r.append_control(scope, &Control::DeviceCert(cert.clone()))?;
                }
                r.drain_pending_decrypt(scope)?;
                r.refresh_conversation(scope)?;
            }
            r.db.execute("DELETE FROM meta WHERE key = 'restored_user' AND value = ?", [&user.0[..]])?;
            Ok(())
        })?;
        for scope in scopes {
            self.repair_recovery_grants(scope)?;
        }
        Ok(())
    }

    // ---- invites ----

    pub fn create_invite(&mut self, scope: ScopeId, role: Role, ttl_ms: u64) -> Result<InviteToken> {
        self.issue_invite(scope, role, ttl_ms, None)
    }

    /// An invite only `user` can redeem, for tokens sent over channels that others can read.
    pub fn create_invite_for(&mut self, scope: ScopeId, role: Role, user: UserKey, ttl_ms: u64) -> Result<InviteToken> {
        self.issue_invite(scope, role, ttl_ms, Some(user))
    }

    fn issue_invite(&mut self, scope: ScopeId, role: Role, ttl_ms: u64, for_user: Option<UserKey>) -> Result<InviteToken> {
        self.require_synced(scope)?;
        let token: [u8; 32] = random_array(self.env.as_ref());
        let expires_ms = self.env.now_ms() + ttl_ms;
        let token_hash = crypto::hash(&token);
        self.tx(|r| {
            r.append_control(scope, &Control::Invite(InvitePayload { token_hash, role, expires_ms }))?;
            r.db.execute(
                "INSERT INTO invites_issued(token_hash, scope, token, role, expires_ms, for_user) VALUES (?, ?, ?, ?, ?, ?)",
                params![&token_hash[..], &scope.0[..], &token[..], role as i64, expires_ms as i64, for_user.as_ref().map(|u| &u.0[..])],
            )?;
            Ok(())
        })?;
        Ok(InviteToken { scope, token, inviter: self.device_key(), role })
    }

    /// Admits an invitee presenting a valid, unused token. Runs on the Admin or Owner device
    /// that issued it, since only that device knows which user a bound invite is for.
    pub fn admit_invitee(&mut self, scope: ScopeId, token: [u8; 32], cert: &DeviceCert) -> Result<()> {
        if !cert.verify() {
            return Err(Error::InvalidInput("bad certificate".into()));
        }
        self.require_synced(scope)?;
        let hash = crypto::hash(&token);
        let st = self.auth_mut(scope)?.full_state();
        let invite = st.invites.get(&hash).cloned().ok_or_else(|| Error::PermissionDenied("unknown invite".into()))?;
        if st.used_invites.contains(&hash) {
            return Err(Error::PermissionDenied("invite already used".into()));
        }
        if invite.expires_ms < self.env.now_ms() {
            return Err(Error::PermissionDenied("invite expired".into()));
        }
        let issued: Option<Option<Vec<u8>>> =
            self.db.query_row("SELECT for_user FROM invites_issued WHERE token_hash = ?", [&hash[..]], |r| r.get(0)).optional()?;
        match issued {
            None => return Err(Error::PermissionDenied("invite not issued by this device".into())),
            Some(Some(user)) if user != cert.user.0[..] => return Err(Error::PermissionDenied("invite is for another user".into())),
            Some(_) => {}
        }
        if st.max_role(&self.device_key()) < Some(Role::Admin) {
            return Err(Error::PermissionDenied("only an admin can admit".into()));
        }
        self.tx(|r| {
            r.append_control(scope, &Control::MemberAdd(MemberAddPayload { subject: Principal::User(cert.user), role: invite.role, invite: Some(hash) }))?;
            r.upsert_known_device(cert)?;
            r.append_control(scope, &Control::DeviceCert(cert.clone()))?;
            let keys = r.all_keys(scope)?;
            let grants = r.grants_for(&[cert.device], &keys)?;
            r.append_control(scope, &Control::KeyUpdate(KeyUpdatePayload { grants }))?;
            Ok(())
        })
    }

    // ---- agents ----

    /// Adds an agent to a scope (if needed) and binds it to a host device. Only the host runs it.
    pub fn assign_agent(&mut self, scope: ScopeId, agent_secret: &[u8], host: Option<DeviceKey>, role: Role, ttl_ms: u64) -> Result<AgentId> {
        let host = host.unwrap_or_else(|| self.device_key());
        if host != self.device_key() {
            self.require_synced(scope)?;
        }
        let binding = AgentBinding::sign(agent_secret, host, self.env.now_ms() + ttl_ms)?;
        let agent = binding.agent;
        self.tx(|r| {
            let st = r.auth_mut(scope)?.full_state();
            if !st.members.contains_key(&Principal::Agent(agent)) {
                r.append_control(scope, &Control::MemberAdd(MemberAddPayload { subject: Principal::Agent(agent), role, invite: None }))?;
            }
            r.append_control(scope, &Control::AgentAssign(binding))?;
            Ok(())
        })?;
        Ok(agent)
    }

    pub fn agent_host(&mut self, scope: ScopeId, agent: &AgentId) -> Result<Option<DeviceKey>> {
        Ok(self.auth_mut(scope)?.full_state().agent_hosts.get(agent).map(|b| b.host_device))
    }

    /// The scope's agent members with their roles and current assignments. An agent added as a
    /// member but never assigned has no binding.
    pub fn agents(&mut self, scope: ScopeId) -> Result<Vec<(AgentId, Role, Option<AgentBinding>)>> {
        let st = self.auth_mut(scope)?.full_state();
        Ok(st
            .members
            .iter()
            .filter_map(|(p, role)| match p {
                Principal::Agent(a) => Some((*a, *role, st.agent_hosts.get(a).cloned())),
                Principal::User(_) => None,
            })
            .collect())
    }

    // ---- identity ----

    /// Switches this device to another user identity (after pairing or recovery). Local scopes
    /// owned by the previous identity add the new user as Owner so nothing is orphaned.
    pub fn adopt_user(&mut self, user: UserSecret) -> Result<()> {
        let unlocked = self.unlock_with(&user)?;
        if user.public() == self.user_key() {
            if unlocked && self.restored_user()? == Some(user.public()) {
                self.finish_recovery()?;
            }
            return Ok(());
        }
        if self.is_retired(&user.public())? {
            return Err(Error::InvalidInput("this identity was rotated; use the current recovery phrase".into()));
        }
        let old = self.user_key();
        let cert = DeviceCert::issue(&user, self.device_key(), &self.cfg.device_name, self.cfg.platform, self.env.now_ms());
        let scopes = self.scope_ids()?;
        self.tx(|r| {
            crate::store::set_meta(&r.db, "device_cert", &crate::payload::encode(&cert))?;
            for scope in scopes {
                let st = r.auth_mut(scope)?.full_state();
                if st.members.get(&Principal::User(old)) != Some(&Role::Owner) {
                    continue;
                }
                if !st.members.contains_key(&Principal::User(user.public())) {
                    r.append_control(
                        scope,
                        &Control::MemberAdd(MemberAddPayload { subject: Principal::User(user.public()), role: Role::Owner, invite: None }),
                    )?;
                }
                r.append_control(scope, &Control::DeviceCert(cert.clone()))?;
            }
            Ok(())
        })?;
        self.secrets.store(super::USER_SECRET, &user.entropy())?;
        self.user = user;
        self.cert = cert.clone();
        self.upsert_known_device(&cert)?;
        self.finish_recovery()?;
        self.seal_view_key()?;
        self.changes.push(Change::IdentityChanged);
        Ok(())
    }

    pub fn scope_ids(&self) -> Result<Vec<ScopeId>> {
        let mut stmt = self.db.prepare_cached("SELECT scope FROM scopes ORDER BY scope")?;
        let rows: Vec<Vec<u8>> = stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows.into_iter().filter_map(|b| ScopeId::from_slice(&b)).collect())
    }
}

fn check_tags(tags: &[(String, String)]) -> Result<()> {
    if tags.len() > MAX_TAGS {
        return Err(Error::InvalidInput(format!("at most {MAX_TAGS} tags")));
    }
    for (i, (k, v)) in tags.iter().enumerate() {
        if k.is_empty() || k.len() > MAX_TAG_BYTES || v.len() > MAX_TAG_BYTES {
            return Err(Error::InvalidInput(format!("tag keys must be 1 to {MAX_TAG_BYTES} bytes, values up to {MAX_TAG_BYTES}")));
        }
        if tags[..i].iter().any(|(o, _)| o == k) {
            return Err(Error::InvalidInput(format!("duplicate tag {k}")));
        }
    }
    Ok(())
}
