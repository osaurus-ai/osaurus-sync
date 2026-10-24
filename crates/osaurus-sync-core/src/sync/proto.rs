//! Sync protocol messages. Frames are postcard-encoded and length-prefixed by the transport.

use serde::{Deserialize, Serialize};

use super::recon::{Key, Range};
use crate::error::{Error, Result};
use crate::identity::DeviceCert;
use crate::ids::{AgentId, EventHash, EventId, ScopeId, UserKey};

/// Bumped on incompatible changes. Peers with a different major version refuse to sync.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame accepted from a peer.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Layer {
    Control,
    Content,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeOffer {
    pub scope: ScopeId,
    pub control_heads: Vec<EventHash>,
    pub heads: Vec<EventHash>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    Hello {
        version: u32,
        cert: DeviceCert,
        storage_role: bool,
    },
    /// Scopes the sender believes the receiver may read, with the sender's heads.
    Offer {
        scopes: Vec<ScopeOffer>,
    },
    /// One reconciliation round. `from_initiator` tells the receiver which side it plays.
    Recon {
        scope: ScopeId,
        layer: Layer,
        tag: u32,
        lo: Key,
        ranges: Vec<Range>,
        from_initiator: bool,
    },
    Want {
        scope: ScopeId,
        tag: u32,
        ids: Vec<EventId>,
        hashes: Vec<EventHash>,
    },
    /// `tag` 0 means a push. `last` closes the reply to a `Want` with the same tag.
    Events {
        scope: ScopeId,
        tag: u32,
        events: Vec<Vec<u8>>,
        last: bool,
    },
    Ack {
        hashes: Vec<EventHash>,
    },
    /// The receiver will not sync this scope with the sender.
    Denied {
        scope: ScopeId,
    },
    PairRequest {
        cert: DeviceCert,
        proof: [u8; 32],
    },
    PairingGrant {
        sealed_user: Vec<u8>,
        cert: DeviceCert,
    },
    PairDenied {
        reason: String,
    },
    InviteRedeem {
        scope: ScopeId,
        token: [u8; 32],
        cert: DeviceCert,
    },
    InviteResult {
        scope: ScopeId,
        error: Option<String>,
    },
    Bye {
        reason: String,
    },
    /// Sent by a storing peer after reconciling the whole scope against the offer with these
    /// heads: it now holds every event the receiver had when it sent that offer.
    Seen {
        scope: ScopeId,
        control_heads: Vec<EventHash>,
        heads: Vec<EventHash>,
    },
    /// The user secret this user rotated to, sealed to the receiver's device key, and the
    /// receiver's certificate under it. Sent to a device still presenting `retired`.
    IdentityHandoff {
        sealed_user: Vec<u8>,
        cert: DeviceCert,
        retired: UserKey,
    },
}

impl Msg {
    /// Splits an `Events` message into messages of at most `n` events (other messages come
    /// back whole), so a receiver can apply a large chunk in steps and let other work in
    /// between. Only the final slice keeps `last`.
    pub fn into_slices(self, n: usize) -> Vec<Msg> {
        match self {
            Msg::Events { scope, tag, events, last } if events.len() > n.max(1) => {
                let mut events = events.into_iter().peekable();
                let mut out = Vec::new();
                while events.peek().is_some() {
                    let slice: Vec<Vec<u8>> = events.by_ref().take(n.max(1)).collect();
                    out.push(Msg::Events { scope, tag, events: slice, last: false });
                }
                if let Some(Msg::Events { last: l, .. }) = out.last_mut() {
                    *l = last;
                }
                out
            }
            m => vec![m],
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("encoding to a Vec cannot fail")
    }

    pub fn decode(b: &[u8]) -> Result<Msg> {
        if b.len() > MAX_FRAME {
            return Err(Error::Malformed("frame too large".into()));
        }
        Ok(postcard::from_bytes(b)?)
    }
}

/// Ephemeral streaming text for an in-progress agent response. Sent on its own ALPN, never
/// stored. The committed message later replaces it under the same `message_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFrame {
    pub scope: ScopeId,
    pub message_id: EventId,
    pub as_agent: Option<AgentId>,
    pub seq: u32,
    /// The full text so far. Frames are small and idempotent this way.
    pub text: String,
    pub state: u8,
}

impl StreamFrame {
    pub const STREAMING: u8 = 0;
    pub const FINISHED: u8 = 1;
    pub const INTERRUPTED: u8 = 2;

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("encoding to a Vec cannot fail")
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(b)?)
    }
}

/// Request on the blob ALPN: send the encrypted blob from `offset`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRequest {
    pub scope: ScopeId,
    pub hash: [u8; 32],
    pub offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobResponse {
    Ok { size: u64 },
    NotFound,
    Denied,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_keep_order_and_last() {
        let scope = ScopeId([1; 16]);
        let events: Vec<Vec<u8>> = (0..10u8).map(|i| vec![i]).collect();
        let slices = Msg::Events { scope, tag: 7, events: events.clone(), last: true }.into_slices(4);
        let mut seen = Vec::new();
        let mut lasts = Vec::new();
        for m in &slices {
            let Msg::Events { tag, events, last, .. } = m else { panic!("not events") };
            assert_eq!(*tag, 7);
            assert!(events.len() <= 4);
            seen.extend(events.iter().cloned());
            lasts.push(*last);
        }
        assert_eq!(seen, events);
        assert_eq!(lasts, [false, false, true]);
        assert_eq!(Msg::Ack { hashes: vec![] }.into_slices(4).len(), 1);
        assert_eq!(Msg::Events { scope, tag: 0, events: vec![vec![1]], last: false }.into_slices(4).len(), 1);
    }
}
