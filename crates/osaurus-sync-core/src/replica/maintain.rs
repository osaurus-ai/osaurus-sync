//! Periodic cleanup of local tables that would otherwise grow without bound.

use std::collections::HashSet;

use rusqlite::params;

use super::Replica;
use crate::error::Result;
use crate::ids::{DeviceKey, ScopeId};

/// Contact records for peers not seen in this long are dropped.
pub const PEER_TTL_MS: u64 = 90 * 86_400_000;
/// A tentatively joined scope still unproven after this long is purged.
pub const TENTATIVE_TTL_MS: u64 = 86_400_000;

/// What one `maintain` pass removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Maintenance {
    pub held_expired: usize,
    pub invites_pruned: usize,
    pub peers_pruned: usize,
    pub devices_pruned: usize,
    pub scopes_purged: usize,
    pub attachments_evicted: usize,
}

impl Replica {
    /// Expires held events and prunes invites, peers, other users' devices, unproven scopes
    /// and the decrypted attachment cache. Safe to call at any time; the network node runs it
    /// hourly.
    pub fn maintain(&mut self) -> Result<Maintenance> {
        let now = self.env.now_ms();
        let mut m = Maintenance { held_expired: self.expire_held()?, ..Default::default() };

        for scope in self.tentative_scopes(now.saturating_sub(TENTATIVE_TTL_MS))? {
            if self.is_member(scope)? {
                self.db.execute("UPDATE scopes SET tentative = 0 WHERE scope = ?", [&scope.0[..]])?;
            } else {
                self.purge_scope(scope)?;
                m.scopes_purged += 1;
            }
        }

        // Expired or used invites can't admit anyone.
        let mut used: Vec<Vec<u8>> = Vec::new();
        for scope in self.scope_ids()? {
            used.extend(self.auth_mut(scope)?.full_state().used_invites.iter().map(|h| h.to_vec()));
        }
        m.invites_pruned = self.db.execute("DELETE FROM invites_issued WHERE expires_ms < ?", [now as i64])?;
        for h in used {
            m.invites_pruned += self.db.execute("DELETE FROM invites_issued WHERE token_hash = ?", [h])?;
        }

        // Devices still certified somewhere, and devices revoked anywhere.
        let mut live: HashSet<DeviceKey> = HashSet::new();
        let mut revoked: HashSet<DeviceKey> = HashSet::new();
        for scope in self.scope_ids()? {
            let st = self.auth_mut(scope)?.full_state();
            revoked.extend(st.revoked.iter().copied());
            live.extend(st.certs.keys().copied());
        }
        live.retain(|d| !revoked.contains(d));

        let peer_cutoff = now.saturating_sub(PEER_TTL_MS) as i64;
        m.peers_pruned = self.db.execute("DELETE FROM peers WHERE last_contact_ms < ?", [peer_cutoff])?;
        for d in &revoked {
            m.peers_pruned += self.db.execute("DELETE FROM peers WHERE device = ?", [&d.0[..]])?;
        }

        // This user's own devices stay listed (revoked ones included); other users' devices
        // are kept only while certified in a scope held here.
        let others: Vec<Vec<u8>> = {
            let mut stmt = self.db.prepare_cached("SELECT device FROM known_devices WHERE user != ?")?;
            stmt.query_map([&self.user_key().0[..]], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        for d in others {
            if DeviceKey::from_slice(&d).is_some_and(|k| !live.contains(&k)) {
                m.devices_pruned += self.db.execute("DELETE FROM known_devices WHERE device = ? AND user != ?", params![d, &self.user_key().0[..]])?;
            }
        }

        m.attachments_evicted = self.blobs.evict_plain(self.cfg.plain_cache_bytes, None);
        Ok(m)
    }

    fn tentative_scopes(&self, joined_before_ms: u64) -> Result<Vec<ScopeId>> {
        let mut stmt = self.db.prepare_cached("SELECT scope FROM scopes WHERE tentative = 1 AND joined_ms < ?")?;
        let rows: Vec<Vec<u8>> = stmt.query_map([joined_before_ms as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows.into_iter().filter_map(|b| ScopeId::from_slice(&b)).collect())
    }
}
