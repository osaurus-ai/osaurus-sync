//! Canonical, signed event envelope.
//!
//! Wire layout (little endian):
//!
//! ```text
//! signed_len u32 | signed bytes | has_payload u8 | [payload bytes] | sig 64
//! ```
//!
//! Signed bytes for version 1:
//!
//! ```text
//! version u16 | id 16 | scope 16 | author 32 | hlc u64 |
//! n_control u16 | control_parents 32*n | n_parents u16 | parents 32*n |
//! kind u16 | epoch u64 | payload_len u32 | payload_hash 32
//! ```
//!
//! `version`, `id` and `scope` lead the signed bytes in every version, so peers can store and
//! forward events from newer versions without understanding them. The signature covers the
//! payload's BLAKE3 hash rather than the payload itself, so a peer that purged a deleted
//! payload can still prove the event's integrity and serve it as a parent.
//!
//! `parents` is split in two: `control_parents` are the author's heads of the scope's control
//! events (membership, keys, Loro updates) and `parents` are the remaining DAG heads. Permission
//! checks only need the control sub-DAG, which syncs in full first, so a message can be validated
//! while its message ancestors are still backfilling.

use crate::crypto;
use crate::error::{Error, Result};
use crate::ids::{DeviceKey, EventHash, EventId, ScopeId};
use crate::keys::DeviceSecret;

pub const ENVELOPE_VERSION: u16 = 1;
const MAX_PARENTS: usize = 256;
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;

pub mod kind {
    pub const MESSAGE: u16 = 1;
    pub const MESSAGE_EDIT: u16 = 2;
    pub const TOMBSTONE: u16 = 3;
    pub const REACTION: u16 = 4;
    pub const LORO_UPDATE: u16 = 5;

    pub const GENESIS: u16 = 16;
    pub const MEMBER_ADD: u16 = 17;
    pub const MEMBER_REMOVE: u16 = 18;
    pub const ROLE_CHANGE: u16 = 19;
    pub const DEVICE_CERT: u16 = 20;
    pub const DEVICE_REVOKE: u16 = 21;
    pub const AGENT_ASSIGN: u16 = 22;
    pub const KEY_UPDATE: u16 = 23;
    pub const INVITE: u16 = 24;

    /// Kinds 16..64 are reserved for control events; unknown kinds in this range still sync in the
    /// control phase.
    pub fn is_control(kind: u16) -> bool {
        (16..64).contains(&kind) || kind == LORO_UPDATE
    }

    /// Control events whose payload is plaintext (public keys, roles, sealed keys), so peers
    /// without scope keys can still validate membership.
    pub fn is_plaintext(kind: u16) -> bool {
        (16..64).contains(&kind)
    }

    pub fn is_known(kind: u16) -> bool {
        matches!(kind, 1..=5 | 16..=24)
    }

    /// Content whose effect gets purged for good: a tombstone removes its target and compaction
    /// drops the edits a newer edit superseded. These count by their author's causal past, so a
    /// removal that arrives later can't bring back what they replaced.
    pub fn causal_only(kind: u16) -> bool {
        matches!(kind, MESSAGE_EDIT | TOMBSTONE)
    }

    pub fn name(kind: u16) -> &'static str {
        match kind {
            MESSAGE => "message",
            MESSAGE_EDIT => "message_edit",
            TOMBSTONE => "tombstone",
            REACTION => "reaction",
            LORO_UPDATE => "loro_update",
            GENESIS => "genesis",
            MEMBER_ADD => "member_add",
            MEMBER_REMOVE => "member_remove",
            ROLE_CHANGE => "role_change",
            DEVICE_CERT => "device_cert",
            DEVICE_REVOKE => "device_revoke",
            AGENT_ASSIGN => "agent_assign",
            KEY_UPDATE => "key_update",
            INVITE => "invite",
            _ => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayloadBytes {
    Full(Vec<u8>),
    /// Payload removed after a tombstone. The hash and length still verify against the signature.
    Purged {
        hash: [u8; 32],
        len: u32,
    },
}

impl PayloadBytes {
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            PayloadBytes::Full(b) => Some(b),
            PayloadBytes::Purged { .. } => None,
        }
    }

    fn hash_and_len(&self) -> ([u8; 32], u32) {
        match self {
            PayloadBytes::Full(b) => (crypto::hash(b), b.len() as u32),
            PayloadBytes::Purged { hash, len } => (*hash, *len),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub version: u16,
    pub id: EventId,
    pub scope: ScopeId,
    pub author: DeviceKey,
    pub hlc: u64,
    pub control_parents: Vec<EventHash>,
    pub parents: Vec<EventHash>,
    pub kind: u16,
    pub epoch: u64,
    pub payload: PayloadBytes,
    pub sig: [u8; 64],
}

/// The fields an author fills in before signing.
pub struct Draft {
    pub id: EventId,
    pub scope: ScopeId,
    pub hlc: u64,
    pub control_parents: Vec<EventHash>,
    pub parents: Vec<EventHash>,
    pub kind: u16,
    pub epoch: u64,
    pub payload: Vec<u8>,
}

/// A decoded event. Newer envelope versions are kept as opaque bytes.
#[derive(Clone, Debug)]
pub enum Decoded {
    Known(Envelope),
    Unknown { version: u16, id: EventId, scope: ScopeId, hash: EventHash, bytes: Vec<u8> },
}

impl Decoded {
    pub fn hash(&self) -> EventHash {
        match self {
            Decoded::Known(e) => e.hash(),
            Decoded::Unknown { hash, .. } => *hash,
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.b.len() < n {
            return Err(Error::Malformed("truncated envelope".into()));
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    fn hashes(&mut self) -> Result<Vec<EventHash>> {
        let n = self.u16()? as usize;
        if n > MAX_PARENTS {
            return Err(Error::Malformed("too many parents".into()));
        }
        (0..n).map(|_| self.arr().map(EventHash)).collect()
    }
}

impl Envelope {
    pub fn sign(draft: Draft, author: &DeviceSecret) -> Envelope {
        let mut e = Envelope {
            version: ENVELOPE_VERSION,
            id: draft.id,
            scope: draft.scope,
            author: author.public(),
            hlc: draft.hlc,
            control_parents: draft.control_parents,
            parents: draft.parents,
            kind: draft.kind,
            epoch: draft.epoch,
            payload: PayloadBytes::Full(draft.payload),
            sig: [0; 64],
        };
        e.sig = crypto::sign(author.signing_key(), &e.signed_bytes());
        e
    }

    pub fn signed_bytes(&self) -> Vec<u8> {
        let (payload_hash, payload_len) = self.payload.hash_and_len();
        let mut b = Vec::with_capacity(128 + 32 * (self.parents.len() + self.control_parents.len()));
        b.extend_from_slice(&self.version.to_le_bytes());
        b.extend_from_slice(&self.id.0);
        b.extend_from_slice(&self.scope.0);
        b.extend_from_slice(&self.author.0);
        b.extend_from_slice(&self.hlc.to_le_bytes());
        b.extend_from_slice(&(self.control_parents.len() as u16).to_le_bytes());
        for p in &self.control_parents {
            b.extend_from_slice(&p.0);
        }
        b.extend_from_slice(&(self.parents.len() as u16).to_le_bytes());
        for p in &self.parents {
            b.extend_from_slice(&p.0);
        }
        b.extend_from_slice(&self.kind.to_le_bytes());
        b.extend_from_slice(&self.epoch.to_le_bytes());
        b.extend_from_slice(&payload_len.to_le_bytes());
        b.extend_from_slice(&payload_hash);
        b
    }

    pub fn hash(&self) -> EventHash {
        EventHash(crypto::hash(&self.signed_bytes()))
    }

    pub fn verify_signature(&self) -> bool {
        crypto::verify(&self.author.0, &self.signed_bytes(), &self.sig)
    }

    pub fn encode(&self) -> Vec<u8> {
        let signed = self.signed_bytes();
        let mut out = Vec::with_capacity(signed.len() + 70 + self.payload.bytes().map_or(0, |p| p.len()));
        out.extend_from_slice(&(signed.len() as u32).to_le_bytes());
        out.extend_from_slice(&signed);
        match &self.payload {
            PayloadBytes::Full(p) => {
                out.push(1);
                out.extend_from_slice(p);
            }
            PayloadBytes::Purged { .. } => out.push(0),
        }
        out.extend_from_slice(&self.sig);
        out
    }

    /// Returns a copy with the payload removed. The event still verifies.
    pub fn purged(&self) -> Envelope {
        let (hash, len) = self.payload.hash_and_len();
        Envelope { payload: PayloadBytes::Purged { hash, len }, ..self.clone() }
    }

    pub fn all_parents(&self) -> impl Iterator<Item = &EventHash> {
        self.control_parents.iter().chain(self.parents.iter())
    }

    pub fn payload_bytes(&self) -> Option<&[u8]> {
        self.payload.bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Decoded> {
        let mut r = Reader { b: bytes };
        let signed_len = r.u32()? as usize;
        let signed = r.take(signed_len)?;
        let mut s = Reader { b: signed };
        let version = s.u16()?;
        let id = EventId(s.arr()?);
        let scope = ScopeId(s.arr()?);
        if version != ENVELOPE_VERSION {
            return Ok(Decoded::Unknown { version, id, scope, hash: EventHash(crypto::hash(signed)), bytes: bytes.to_vec() });
        }
        let author = DeviceKey(s.arr()?);
        let hlc = s.u64()?;
        let control_parents = s.hashes()?;
        let parents = s.hashes()?;
        let kind = s.u16()?;
        let epoch = s.u64()?;
        let payload_len = s.u32()?;
        let payload_hash: [u8; 32] = s.arr()?;
        if !s.b.is_empty() {
            return Err(Error::Malformed("trailing signed bytes".into()));
        }
        if payload_len as usize > MAX_PAYLOAD {
            return Err(Error::Malformed("payload too large".into()));
        }
        let payload = match r.u8()? {
            1 => {
                let p = r.take(payload_len as usize)?;
                if crypto::hash(p) != payload_hash {
                    return Err(Error::Malformed("payload hash mismatch".into()));
                }
                PayloadBytes::Full(p.to_vec())
            }
            0 => PayloadBytes::Purged { hash: payload_hash, len: payload_len },
            _ => return Err(Error::Malformed("bad payload flag".into())),
        };
        let sig: [u8; 64] = r.arr()?;
        if !r.b.is_empty() {
            return Err(Error::Malformed("trailing bytes".into()));
        }
        Ok(Decoded::Known(Envelope { version, id, scope, author, hlc, control_parents, parents, kind, epoch, payload, sig }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::TestEnv;
    use proptest::prelude::*;

    fn sample(env: &TestEnv, payload: Vec<u8>, parents: usize) -> Envelope {
        let dev = DeviceSecret::generate(env);
        Envelope::sign(
            Draft {
                id: EventId([1; 16]),
                scope: ScopeId([2; 16]),
                hlc: 99,
                control_parents: vec![EventHash([3; 32])],
                parents: (0..parents).map(|i| EventHash([i as u8; 32])).collect(),
                kind: kind::MESSAGE,
                epoch: 1,
                payload,
            },
            &dev,
        )
    }

    #[test]
    fn roundtrip_and_signature() {
        let env = TestEnv::new(1, 0);
        let e = sample(&env, b"ciphertext".to_vec(), 2);
        assert!(e.verify_signature());
        let Decoded::Known(d) = Envelope::decode(&e.encode()).unwrap() else { panic!() };
        assert_eq!(d, e);
        assert_eq!(d.hash(), e.hash());
    }

    #[test]
    fn purged_event_still_verifies_with_same_hash() {
        let env = TestEnv::new(2, 0);
        let e = sample(&env, b"secret".to_vec(), 1);
        let p = e.purged();
        assert!(p.verify_signature());
        assert_eq!(p.hash(), e.hash());
        let Decoded::Known(d) = Envelope::decode(&p.encode()).unwrap() else { panic!() };
        assert!(d.payload_bytes().is_none());
        assert_eq!(d.hash(), e.hash());
    }

    #[test]
    fn tamper_breaks_signature() {
        let env = TestEnv::new(3, 0);
        let mut e = sample(&env, b"x".to_vec(), 0);
        e.hlc += 1;
        assert!(!e.verify_signature());
    }

    #[test]
    fn unknown_version_kept_as_bytes() {
        let env = TestEnv::new(4, 0);
        let mut e = sample(&env, b"x".to_vec(), 0);
        e.version = 2;
        let bytes = e.encode();
        match Envelope::decode(&bytes).unwrap() {
            Decoded::Unknown { version, id, scope, bytes: kept, .. } => {
                assert_eq!(version, 2);
                assert_eq!(id, e.id);
                assert_eq!(scope, e.scope);
                assert_eq!(kept, bytes);
            }
            _ => panic!("expected unknown"),
        }
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
            let _ = Envelope::decode(&bytes);
        }

        #[test]
        fn roundtrip_any_payload(payload in proptest::collection::vec(any::<u8>(), 0..2000), parents in 0usize..8) {
            let env = TestEnv::new(5, 0);
            let e = sample(&env, payload, parents);
            let Decoded::Known(d) = Envelope::decode(&e.encode()).unwrap() else { panic!() };
            prop_assert!(d.verify_signature());
            prop_assert_eq!(d, e);
        }
    }
}
