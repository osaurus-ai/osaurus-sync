//! Event payload bodies. Content payloads are encrypted with the scope's epoch key; control
//! payloads are plaintext and carry only public keys, roles and sealed keys.

use serde::{Deserialize, Serialize};

use crate::envelope::kind;
use crate::error::{Error, Result};
use crate::identity::{AgentBinding, DeviceCert};
use crate::ids::{AgentId, DeviceKey, EventId, Principal, Role, UserKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    ToolCall,
    ToolResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// BLAKE3 of the encrypted blob file.
    #[serde(with = "crate::ids::fixed")]
    pub hash: [u8; 32],
    pub size: u64,
    pub plain_size: u64,
    pub media_type: String,
    pub name: String,
    /// Per-blob key. Lives only inside this encrypted payload, so purging the payload makes the
    /// blob unreadable even where its bytes remain.
    #[serde(with = "crate::ids::fixed")]
    pub key: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePayload {
    pub role: MessageRole,
    pub content: String,
    pub blobs: Vec<BlobRef>,
    pub reply_to: Option<EventId>,
    pub as_agent: Option<AgentId>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    /// Original creation time for imported history, in Unix milliseconds.
    pub imported_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditPayload {
    pub target: EventId,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TombstonePayload {
    pub target: EventId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReactionOp {
    Add,
    /// Removes the add events the author had observed.
    Remove {
        observed: Vec<EventId>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionPayload {
    pub target: EventId,
    pub reaction: String,
    pub op: ReactionOp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoroUpdatePayload {
    pub doc: u8,
    pub shallow: bool,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScopeKind {
    Conversation,
    Workspace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenesisPayload {
    pub owner: UserKey,
    pub owner_device: DeviceCert,
    pub scope_kind: ScopeKind,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberAddPayload {
    pub subject: Principal,
    pub role: Role,
    #[serde(with = "crate::ids::fixed::option")]
    pub invite: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRemovePayload {
    pub subject: Principal,
    /// For each of the subject's devices, the highest HLC the remover had seen from it. Events
    /// from those devices above the cutoff are concurrent with or after the removal.
    pub cutoffs: Vec<(DeviceKey, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleChangePayload {
    pub subject: Principal,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevokePayload {
    pub device: DeviceKey,
    pub cutoff: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyGrant {
    pub epoch: u64,
    #[serde(with = "crate::ids::fixed")]
    pub key_check: [u8; 8],
    pub device: DeviceKey,
    pub sealed: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyUpdatePayload {
    pub grants: Vec<KeyGrant>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitePayload {
    #[serde(with = "crate::ids::fixed")]
    pub token_hash: [u8; 32],
    pub role: Role,
    pub expires_ms: u64,
}

/// A parsed control payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Genesis(GenesisPayload),
    MemberAdd(MemberAddPayload),
    MemberRemove(MemberRemovePayload),
    RoleChange(RoleChangePayload),
    DeviceCert(DeviceCert),
    DeviceRevoke(DeviceRevokePayload),
    AgentAssign(AgentBinding),
    KeyUpdate(KeyUpdatePayload),
    Invite(InvitePayload),
}

impl Control {
    pub fn kind(&self) -> u16 {
        match self {
            Control::Genesis(_) => kind::GENESIS,
            Control::MemberAdd(_) => kind::MEMBER_ADD,
            Control::MemberRemove(_) => kind::MEMBER_REMOVE,
            Control::RoleChange(_) => kind::ROLE_CHANGE,
            Control::DeviceCert(_) => kind::DEVICE_CERT,
            Control::DeviceRevoke(_) => kind::DEVICE_REVOKE,
            Control::AgentAssign(_) => kind::AGENT_ASSIGN,
            Control::KeyUpdate(_) => kind::KEY_UPDATE,
            Control::Invite(_) => kind::INVITE,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            Control::Genesis(p) => encode(p),
            Control::MemberAdd(p) => encode(p),
            Control::MemberRemove(p) => encode(p),
            Control::RoleChange(p) => encode(p),
            Control::DeviceCert(p) => encode(p),
            Control::DeviceRevoke(p) => encode(p),
            Control::AgentAssign(p) => encode(p),
            Control::KeyUpdate(p) => encode(p),
            Control::Invite(p) => encode(p),
        }
    }

    /// Returns `Ok(None)` for kinds this version does not know.
    pub fn decode(kind: u16, bytes: &[u8]) -> Result<Option<Control>> {
        Ok(Some(match kind {
            kind::GENESIS => Control::Genesis(decode(bytes)?),
            kind::MEMBER_ADD => Control::MemberAdd(decode(bytes)?),
            kind::MEMBER_REMOVE => Control::MemberRemove(decode(bytes)?),
            kind::ROLE_CHANGE => Control::RoleChange(decode(bytes)?),
            kind::DEVICE_CERT => Control::DeviceCert(decode(bytes)?),
            kind::DEVICE_REVOKE => Control::DeviceRevoke(decode(bytes)?),
            kind::AGENT_ASSIGN => Control::AgentAssign(decode(bytes)?),
            kind::KEY_UPDATE => Control::KeyUpdate(decode(bytes)?),
            kind::INVITE => Control::Invite(decode(bytes)?),
            _ => return Ok(None),
        }))
    }
}

/// A parsed, decrypted content payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Message(MessagePayload),
    Edit(EditPayload),
    Tombstone(TombstonePayload),
    Reaction(ReactionPayload),
    Loro(LoroUpdatePayload),
}

impl Content {
    pub fn kind(&self) -> u16 {
        match self {
            Content::Message(_) => kind::MESSAGE,
            Content::Edit(_) => kind::MESSAGE_EDIT,
            Content::Tombstone(_) => kind::TOMBSTONE,
            Content::Reaction(_) => kind::REACTION,
            Content::Loro(_) => kind::LORO_UPDATE,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            Content::Message(p) => encode(p),
            Content::Edit(p) => encode(p),
            Content::Tombstone(p) => encode(p),
            Content::Reaction(p) => encode(p),
            Content::Loro(p) => encode(p),
        }
    }

    pub fn decode(kind: u16, bytes: &[u8]) -> Result<Option<Content>> {
        Ok(Some(match kind {
            kind::MESSAGE => Content::Message(decode(bytes)?),
            kind::MESSAGE_EDIT => Content::Edit(decode(bytes)?),
            kind::TOMBSTONE => Content::Tombstone(decode(bytes)?),
            kind::REACTION => Content::Reaction(decode(bytes)?),
            kind::LORO_UPDATE => Content::Loro(decode(bytes)?),
            _ => return Ok(None),
        }))
    }
}

pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_allocvec(v).expect("payloads are serializable")
}

pub fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Result<T> {
    postcard::from_bytes(b).map_err(|e| Error::Malformed(format!("payload: {e}")))
}
