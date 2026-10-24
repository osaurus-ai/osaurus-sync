//! Identity rotation: replacing a user secret that a stolen device still holds.
//!
//! A revoked device that kept the user secret can certify a fresh device key and be admitted
//! again. Rotation retires the old user: a new secret is generated, this user's remaining devices
//! are certified for it, and in every scope the old user is replaced (in scopes it owned) or
//! leaves (elsewhere), with cutoffs and a new epoch. The new secret reaches the remaining devices
//! sealed to their device keys, like pairing. Certificates of a retired user are refused.

use rusqlite::{OptionalExtension, params};

use super::{Replica, USER_SECRET, USER_SECRET_NEXT};
use crate::error::{Error, Result};
use crate::identity::DeviceCert;
use crate::ids::{DeviceKey, Principal, Role, ScopeId, UserKey};
use crate::keys::UserSecret;
use crate::payload::{Control, DeviceRevokePayload, MemberAddPayload, MemberRemovePayload};
use crate::types::{Change, IdentityRotation};

/// A scope still listing a retired user is taken over by this device only after this long, so
/// the rotating device's own takeover has time to arrive.
pub const SUCCESSION_GRACE_MS: u64 = 3_600_000;

impl Replica {
    /// Revokes `revoke` and replaces this user's secret with a new one. Use it when a device was
    /// stolen: a plain revoke leaves the thief with the user secret. Every other known device of
    /// this user stays and receives the new secret when it next connects to one that has it.
    pub fn rotate_identity(&mut self, revoke: &[DeviceKey]) -> Result<IdentityRotation> {
        if revoke.contains(&self.device_key()) {
            return Err(Error::InvalidInput("cannot revoke this device from itself".into()));
        }
        if self.is_self_revoked() {
            return Err(Error::PermissionDenied("this device is revoked".into()));
        }
        let new = UserSecret::generate(self.env.as_ref());
        self.secrets.store(USER_SECRET_NEXT, &new.entropy())?;
        let (old, old_cert) = (self.user.clone(), self.cert.clone());
        match self.tx(|r| r.rotate_inner(&old, &new, revoke)) {
            Ok(report) => {
                self.secrets.store(USER_SECRET, &new.entropy())?;
                self.secrets.delete(USER_SECRET_NEXT)?;
                Ok(report)
            }
            Err(e) => {
                self.user = old;
                self.cert = old_cert;
                let _ = self.secrets.delete(USER_SECRET_NEXT);
                Err(e)
            }
        }
    }

    fn rotate_inner(&mut self, old: &UserSecret, new: &UserSecret, revoke: &[DeviceKey]) -> Result<IdentityRotation> {
        let now = self.env.now_ms();
        let me = self.device_key();
        let scopes = self.scope_ids()?;
        for &device in revoke {
            for &scope in &scopes {
                let st = self.auth_mut(scope)?.full_state();
                if st.certs.contains_key(&device) && !st.revoked.contains(&device) && st.max_role(&me).is_some() {
                    let cutoff = self.max_hlc_of(scope, &device)?;
                    self.append_control(scope, &Control::DeviceRevoke(DeviceRevokePayload { device, cutoff }))?;
                }
            }
            self.db.execute("INSERT OR IGNORE INTO revoked_devices(device, revoked_ms) VALUES (?, ?)", params![&device.0[..], now as i64])?;
        }

        let mut certs = vec![DeviceCert::issue(new, me, &self.cfg.device_name, self.cfg.platform, now)];
        for c in self.my_other_devices()? {
            certs.push(DeviceCert::issue(new, c.device, &c.name, c.platform, now));
        }
        self.user = new.clone();
        self.cert = certs[0].clone();
        crate::store::set_meta(&self.db, "device_cert", &crate::payload::encode(&self.cert))?;
        for c in &certs {
            self.upsert_known_device(c)?;
        }
        self.db.execute(
            "INSERT OR REPLACE INTO retired_users(user, successor, retired_ms) VALUES (?, ?, ?)",
            params![&old.public().0[..], &new.public().0[..], now as i64],
        )?;

        let mut report = IdentityRotation { user: new.public(), rotated: vec![], left: vec![] };
        for scope in scopes {
            match self.succeed_in_scope(scope, old.public(), &certs)? {
                Some(true) => report.rotated.push(scope),
                Some(false) => report.left.push(scope),
                None => {}
            }
        }
        self.recovery_checked.clear();
        self.emit(Change::IdentityChanged);
        self.emit(Change::DevicesChanged);
        Ok(report)
    }

    /// Replaces a retired user in one scope. Where it was Owner, the current user becomes Owner
    /// and `certs` (this user's devices) are published; elsewhere it can't add anyone, so it
    /// rotates the key to the other members and leaves. Returns whether the scope was kept, or
    /// `None` if there was nothing to do.
    fn succeed_in_scope(&mut self, scope: ScopeId, old: UserKey, certs: &[DeviceCert]) -> Result<Option<bool>> {
        let me = self.device_key();
        let st = self.auth_mut(scope)?.full_state();
        let Some(&role) = st.members.get(&Principal::User(old)) else { return Ok(None) };
        let acts_for_old = st.certs.get(&me).is_some_and(|u| u.contains_key(&old));
        if self.is_tentative(scope)? || st.revoked.contains(&me) || !acts_for_old {
            return Ok(None);
        }
        let old_p = Principal::User(old);
        if role == Role::Owner {
            let user = self.user_key();
            if !st.members.contains_key(&Principal::User(user)) {
                self.append_control(scope, &Control::MemberAdd(MemberAddPayload { subject: Principal::User(user), role: Role::Owner, invite: None }))?;
            }
            for c in certs {
                let in_scope = c.device == me || st.certs.get(&c.device).is_some_and(|u| u.contains_key(&old));
                let has = st.certs.get(&c.device).is_some_and(|u| u.contains_key(&user));
                if in_scope && !has && !st.revoked.contains(&c.device) {
                    self.append_control(scope, &Control::DeviceCert(c.clone()))?;
                }
            }
            let cutoffs = self.cutoffs_of(scope, &st.devices_of(&old))?;
            self.append_control(scope, &Control::MemberRemove(MemberRemovePayload { subject: old_p, cutoffs }))?;
            self.rotate_key(scope)?;
            self.refresh_conversation(scope)?;
            return Ok(Some(true));
        }
        let others: Vec<DeviceKey> = st.certs.keys().copied().filter(|d| st.acting(d).iter().any(|(p, _)| *p != old_p)).collect();
        self.rotate_key_to(scope, &others)?;
        let cutoffs = self.cutoffs_of(scope, &st.devices_of(&old))?;
        self.append_control(scope, &Control::MemberRemove(MemberRemovePayload { subject: old_p, cutoffs }))?;
        self.refresh_conversation(scope)?;
        Ok(Some(false))
    }

    /// The highest HLC seen from each device, for a removal. Computed last, so the remover's
    /// own events before the removal stay valid.
    pub(super) fn cutoffs_of(&self, scope: ScopeId, devices: &[DeviceKey]) -> Result<Vec<(DeviceKey, u64)>> {
        let mut out = Vec::new();
        for d in devices {
            if let Some(h) = self.max_hlc_of(scope, d)? {
                out.push((*d, h));
            }
        }
        Ok(out)
    }

    /// Takes over scopes that still list a user this device retired, once the device that
    /// rotated has had time to do it. Returns how many scopes changed.
    pub(crate) fn complete_successions(&mut self) -> Result<usize> {
        let before = self.env.now_ms().saturating_sub(SUCCESSION_GRACE_MS) as i64;
        let retired: Vec<UserKey> = {
            let mut stmt = self.db.prepare_cached("SELECT user FROM retired_users WHERE retired_ms < ?")?;
            let rows: Vec<Vec<u8>> = stmt.query_map([before], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
            rows.into_iter().filter_map(|b| <[u8; 32]>::try_from(b.as_slice()).ok()).map(UserKey).collect()
        };
        if retired.is_empty() || self.is_self_revoked() {
            return Ok(0);
        }
        let mut certs = vec![self.cert.clone()];
        certs.extend(self.my_other_devices()?);
        let scopes = self.scope_ids()?;
        let mut n = 0;
        for old in retired {
            for &scope in &scopes {
                n += self.tx(|r| r.succeed_in_scope(scope, old, &certs))?.is_some() as usize;
            }
        }
        Ok(n)
    }

    /// Takes on the identity another device of this user rotated to. `retired` must be this
    /// device's current user, and `cert` this device's certificate under the new user.
    pub(crate) fn accept_identity_handoff(&mut self, new: UserSecret, cert: DeviceCert, retired: UserKey) -> Result<bool> {
        let fits = retired == self.user_key() && cert.user == new.public() && cert.device == self.device_key() && cert.verify();
        if !fits || self.is_self_revoked() || self.is_retired(&new.public())? {
            return Ok(false);
        }
        self.secrets.store(USER_SECRET_NEXT, &new.entropy())?;
        let (old, old_cert) = (self.user.clone(), self.cert.clone());
        let res = self.tx(|r| {
            r.user = new.clone();
            r.cert = cert.clone();
            crate::store::set_meta(&r.db, "device_cert", &crate::payload::encode(&cert))?;
            r.upsert_known_device(&cert)?;
            r.db.execute(
                "INSERT OR REPLACE INTO retired_users(user, successor, retired_ms) VALUES (?, ?, ?)",
                params![&retired.0[..], &new.public().0[..], r.env.now_ms() as i64],
            )?;
            for scope in r.scope_ids()? {
                r.refresh_conversation(scope)?;
            }
            r.emit(Change::IdentityChanged);
            r.emit(Change::DevicesChanged);
            Ok(())
        });
        match res {
            Ok(()) => {
                self.secrets.store(USER_SECRET, &new.entropy())?;
                self.secrets.delete(USER_SECRET_NEXT)?;
                self.recovery_checked.clear();
                Ok(true)
            }
            Err(e) => {
                self.user = old;
                self.cert = old_cert;
                let _ = self.secrets.delete(USER_SECRET_NEXT);
                Err(e)
            }
        }
    }

    pub fn is_retired(&self, user: &UserKey) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM retired_users WHERE user = ?)", [&user.0[..]], |r| r.get(0))?)
    }

    /// For a peer presenting a retired user's certificate: its certificate under the current
    /// user, if this user certified it. Such a peer is owed the new secret.
    pub fn successor_cert(&self, cert: &DeviceCert) -> Result<Option<DeviceCert>> {
        if !self.is_retired(&cert.user)? || self.is_device_revoked(&cert.device)? {
            return Ok(None);
        }
        let row: Option<Vec<u8>> = self
            .db
            .query_row("SELECT cert FROM known_devices WHERE device = ? AND user = ?", params![&cert.device.0[..], &self.user_key().0[..]], |r| r.get(0))
            .optional()?;
        Ok(row.and_then(|b| crate::payload::decode::<DeviceCert>(&b).ok()).filter(|c| c.verify() && c.user == self.user_key()))
    }

    /// Whether `device` is a known, unrevoked device of this user.
    pub fn is_own_device(&self, device: &DeviceKey) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM known_devices WHERE device = ? AND user = ?) AND NOT EXISTS(SELECT 1 FROM revoked_devices WHERE device = ?)",
            params![&device.0[..], &self.user_key().0[..], &device.0[..]],
            |r| r.get(0),
        )?)
    }
}
