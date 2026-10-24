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
    Content, Control, DeviceRevokePayload, EditPayload, GenesisPayload, InvitePayload, KeyGrant, KeyUpdatePayload, MemberAddPayload, MemberRemovePayload,
    MessagePayload, MessageRole, ReactionOp, ReactionPayload, RoleChangePayload, ScopeKind, TombstonePayload,
};
use crate::types::{Change, ImportedMessage, NewMessage};

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
    fn my_other_devices(&self) -> Result<Vec<DeviceCert>> {
        let mut stmt =
            self.db.prepare_cached("SELECT cert FROM known_devices WHERE user = ? AND device != ? AND device NOT IN (SELECT device FROM revoked_devices)")?;
        let rows: Vec<Vec<u8>> =
            stmt.query_map(params![&self.user_key().0[..], &self.device_key().0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows.into_iter().filter_map(|b| crate::payload::decode::<DeviceCert>(&b).ok()).filter(|c| c.verify()).collect())
    }

    fn grants_for(&self, devices: &[DeviceKey], keys: &[(u64, [u8; 32])]) -> Result<Vec<KeyGrant>> {
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
        self.tx(|r| r.create_scope_inner(kind_, title, None))
    }

    fn create_scope_inner(&mut self, kind_: ScopeKind, title: &str, created_ms: Option<u64>) -> Result<ScopeId> {
        let id = self.new_event_id();
        let scope = ScopeId::from(id);
        self.join_scope(scope)?;
        // A scope created here is complete by definition.
        self.db.execute("UPDATE scopes SET synced_from_hlc = 0 WHERE scope = ?", [&scope.0[..]])?;
        let genesis = Control::Genesis(GenesisPayload {
            owner: self.user_key(),
            owner_device: self.cert.clone(),
            scope_kind: kind_,
            created_ms: created_ms.unwrap_or_else(|| self.env.now_ms()),
        });
        self.append(scope, kind::GENESIS, 0, genesis.encode(), Some(id), None)?;
        let others = self.my_other_devices()?;
        for c in &others {
            self.append_control(scope, &Control::DeviceCert(c.clone()))?;
        }
        let key: [u8; 32] = random_array(self.env.as_ref());
        let mut devices = vec![self.device_key()];
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
        self.tx(|r| {
            let scope = r.create_scope_inner(ScopeKind::Conversation, title, Some(created_ms))?;
            for (i, m) in messages.iter().enumerate() {
                let hlc = crate::hlc::pack(m.created_ms, (i % 0xffff) as u16);
                let payload = MessagePayload {
                    role: m.role,
                    content: m.content.clone(),
                    blobs: vec![],
                    reply_to: None,
                    as_agent: None,
                    tool_call_id: None,
                    tool_name: None,
                    imported_at_ms: Some(m.created_ms),
                };
                r.append_content(scope, &Content::Message(payload), None, Some(hlc))?;
            }
            Ok(scope)
        })
    }

    pub fn send_message(&mut self, scope: ScopeId, msg: NewMessage) -> Result<EventId> {
        if let Some(agent) = msg.as_agent
            && self.agent_host(scope, &agent)? != Some(self.device_key())
        {
            return Err(Error::PermissionDenied("this device does not host the agent".into()));
        }
        self.tx(|r| {
            let mut blobs = Vec::new();
            for a in &msg.attachments {
                blobs.push(r.blobs.import(r.env.as_ref(), &a.path, &a.media_type, &a.name)?);
            }
            let mut content = msg.content.clone();
            if msg.role == MessageRole::ToolResult && content.len() > r.cfg.tool_output_blob_threshold {
                let b = r.blobs.import_bytes(r.env.as_ref(), content.as_bytes(), "text/plain", "tool-output.txt")?;
                let mut cut = 512.min(content.len());
                while !content.is_char_boundary(cut) {
                    cut -= 1;
                }
                content = format!("{}…", &content[..cut]);
                blobs.push(b);
            }
            let payload = MessagePayload {
                role: msg.role,
                content,
                blobs,
                reply_to: msg.reply_to,
                as_agent: msg.as_agent,
                tool_call_id: msg.tool_call_id.clone(),
                tool_name: msg.tool_name.clone(),
                imported_at_ms: None,
            };
            r.append_content(scope, &Content::Message(payload), msg.id, None)
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

    fn max_hlc_of(&self, scope: ScopeId, device: &DeviceKey) -> Result<Option<u64>> {
        let v: Option<i64> =
            self.db.query_row("SELECT MAX(hlc) FROM events WHERE scope = ? AND author = ?", params![&scope.0[..], &device.0[..]], |r| r.get(0))?;
        Ok(v.map(|v| v as u64))
    }

    /// Removes a member. They keep what they already received and lose access to anything new.
    pub fn remove_member(&mut self, scope: ScopeId, subject: Principal) -> Result<()> {
        self.tx(|r| {
            let st = r.auth_mut(scope)?.full_state();
            let devices: Vec<DeviceKey> = match subject {
                Principal::User(u) => st.devices_of(&u),
                Principal::Agent(a) => st.agent_hosts.get(&a).copied().into_iter().collect(),
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
        let devices: Vec<DeviceKey> = st.certs.keys().copied().filter(|d| !st.acting(d).is_empty()).collect();
        let next = self.current_key(scope)?.map(|(e, _)| e + 1).unwrap_or(1);
        let key: [u8; 32] = random_array(self.env.as_ref());
        let grants = self.grants_for(&devices, &[(next, key)])?;
        self.append_control(scope, &Control::KeyUpdate(KeyUpdatePayload { grants }))?;
        Ok(())
    }

    /// Revokes a lost or retired device in every scope where it is certified, and rotates keys.
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
        Ok(mine.is_subset(&theirs))
    }

    // ---- invites ----

    pub fn create_invite(&mut self, scope: ScopeId, role: Role, ttl_ms: u64) -> Result<InviteToken> {
        let token: [u8; 32] = random_array(self.env.as_ref());
        let expires_ms = self.env.now_ms() + ttl_ms;
        let token_hash = crypto::hash(&token);
        self.tx(|r| {
            r.append_control(scope, &Control::Invite(InvitePayload { token_hash, role, expires_ms }))?;
            r.db.execute(
                "INSERT INTO invites_issued(token_hash, scope, token, role, expires_ms) VALUES (?, ?, ?, ?, ?)",
                params![&token_hash[..], &scope.0[..], &token[..], role as i64, expires_ms as i64],
            )?;
            Ok(())
        })?;
        Ok(InviteToken { scope, token, inviter: self.device_key(), role })
    }

    /// Admits an invitee presenting a valid, unused token. Runs on an Admin or Owner device.
    pub fn admit_invitee(&mut self, scope: ScopeId, token: [u8; 32], cert: &DeviceCert) -> Result<()> {
        if !cert.verify() {
            return Err(Error::InvalidInput("bad certificate".into()));
        }
        let hash = crypto::hash(&token);
        let st = self.auth_mut(scope)?.full_state();
        let invite = st.invites.get(&hash).cloned().ok_or_else(|| Error::PermissionDenied("unknown invite".into()))?;
        if st.used_invites.contains(&hash) {
            return Err(Error::PermissionDenied("invite already used".into()));
        }
        if invite.expires_ms < self.env.now_ms() {
            return Err(Error::PermissionDenied("invite expired".into()));
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
        Ok(self.auth_mut(scope)?.full_state().agent_hosts.get(agent).copied())
    }

    // ---- identity ----

    /// Switches this device to another user identity (after pairing or recovery). Local scopes
    /// owned by the previous identity add the new user as Owner so nothing is orphaned.
    pub fn adopt_user(&mut self, user: UserSecret) -> Result<()> {
        if user.public() == self.user_key() {
            return Ok(());
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
        self.changes.push(Change::IdentityChanged);
        Ok(())
    }

    pub fn scope_ids(&self) -> Result<Vec<ScopeId>> {
        let mut stmt = self.db.prepare_cached("SELECT scope FROM scopes ORDER BY scope")?;
        let rows: Vec<Vec<u8>> = stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows.into_iter().filter_map(|b| ScopeId::from_slice(&b)).collect())
    }
}
