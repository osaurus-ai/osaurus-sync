//! View-level types handed to the apps. Event bytes, Loro containers and keys never appear here.

use std::path::PathBuf;

use crate::auth::Reason;
use crate::identity::Platform;
use crate::ids::{AgentId, DeviceKey, EventId, Principal, Role, ScopeId, UserKey};
use crate::payload::{MessageRole, ScopeKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryMode {
    Full,
    /// Keep a time window and fetch older ranges on demand.
    Recent {
        days: u32,
    },
}

impl HistoryMode {
    /// Platform default: phones keep 90 days, desktops keep everything.
    pub fn default_for(platform: Platform) -> HistoryMode {
        match platform {
            Platform::Ios => HistoryMode::Recent { days: 90 },
            _ => HistoryMode::Full,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryStatus {
    Delivered,
    /// In the outbox: not yet acknowledged by any peer that stores the scope.
    Pending,
    Rejected(Reason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobInfo {
    pub hash: [u8; 32],
    pub size: u64,
    pub media_type: String,
    pub name: String,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionSummary {
    pub reaction: String,
    pub count: u32,
    pub mine: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageView {
    pub id: EventId,
    pub scope: ScopeId,
    pub hlc: u64,
    pub author_device: DeviceKey,
    pub author: Principal,
    pub is_agent: bool,
    pub role: MessageRole,
    pub content: String,
    pub blobs: Vec<BlobInfo>,
    pub reply_to: Option<EventId>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub edited: bool,
    pub status: DeliveryStatus,
    pub reactions: Vec<ReactionSummary>,
}

/// Position in display order `(hlc, author, id)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor {
    pub hlc: u64,
    pub author: DeviceKey,
    pub id: EventId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessagePage {
    /// Oldest first.
    pub messages: Vec<MessageView>,
    /// Pass as `before` to load the next older page.
    pub next_cursor: Option<Cursor>,
    /// The page reached the oldest synced message while older history may exist on peers.
    pub at_sync_boundary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberInfo {
    pub principal: Principal,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConversationSummary {
    pub scope: ScopeId,
    pub kind: ScopeKind,
    pub title: String,
    pub pinned: bool,
    pub sort_order: Option<f64>,
    pub archived: bool,
    pub last_activity_hlc: u64,
    pub last_preview: Option<String>,
    pub unread: u32,
    pub my_role: Option<Role>,
    pub members: Vec<MemberInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub scope: ScopeId,
    pub id: EventId,
    pub snippet: String,
    pub hlc: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device: DeviceKey,
    pub user: UserKey,
    pub name: String,
    pub platform: String,
    pub last_seen_ms: Option<u64>,
    pub revoked: bool,
    pub is_self: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub media_type: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    pub role: MessageRole,
    pub content: String,
    pub attachments: Vec<Attachment>,
    pub reply_to: Option<EventId>,
    pub as_agent: Option<AgentId>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    /// Pre-allocated id, used to commit a streamed response under the stream's message id.
    pub id: Option<EventId>,
}

impl NewMessage {
    pub fn text(role: MessageRole, content: impl Into<String>) -> Self {
        NewMessage { role, content: content.into(), attachments: vec![], reply_to: None, as_agent: None, tool_call_id: None, tool_name: None, id: None }
    }
}

/// Result of `rotate_identity`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityRotation {
    /// The new user key.
    pub user: UserKey,
    /// Scopes this user owned: the new identity took over as Owner.
    pub rotated: Vec<ScopeId>,
    /// Scopes owned by someone else: the old identity left them, and an owner or admin has to
    /// invite the new one.
    pub left: Vec<ScopeId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedMessage {
    pub role: MessageRole,
    pub content: String,
    pub created_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamState {
    Streaming,
    Finished,
    /// The host went away mid-stream. Partial text is never committed.
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamUpdate {
    pub scope: ScopeId,
    pub message_id: EventId,
    pub host: DeviceKey,
    pub as_agent: Option<AgentId>,
    pub text: String,
    pub state: StreamState,
}

/// View-level diffs delivered to listeners after each commit.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    MessageAdded(MessageView),
    MessageChanged(MessageView),
    MessageRemoved {
        scope: ScopeId,
        id: EventId,
    },
    /// Membership changed in a way that invalidated earlier events; reload the scope.
    ScopeReset(ScopeId),
    ConversationChanged(ConversationSummary),
    ConversationRemoved(ScopeId),
    Stream(StreamUpdate),
    SyncStatusChanged(ScopeId),
    DevicesChanged,
    /// This device was revoked. Sync stops.
    SelfRevoked,
    /// The user identity changed after pairing or recovery; the app should persist the new
    /// user secret (already written through the secret store) and refresh everything.
    IdentityChanged,
    /// This device holds a restored database for another identity. Entering that identity's
    /// recovery phrase unlocks its history.
    RecoveryAvailable,
}

impl Change {
    pub fn scope(&self) -> Option<ScopeId> {
        match self {
            Change::MessageAdded(m) | Change::MessageChanged(m) => Some(m.scope),
            Change::MessageRemoved { scope, .. } => Some(*scope),
            Change::ScopeReset(s) | Change::ConversationRemoved(s) | Change::SyncStatusChanged(s) => Some(*s),
            Change::ConversationChanged(c) => Some(c.scope),
            Change::Stream(s) => Some(s.scope),
            Change::DevicesChanged | Change::SelfRevoked | Change::IdentityChanged | Change::RecoveryAvailable => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerStatus {
    pub device: DeviceKey,
    pub name: String,
    pub connected: bool,
    pub relayed: bool,
    pub last_contact_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub scope: ScopeId,
    pub pending_outbox: u32,
    pub last_sync_ms: Option<u64>,
    pub synced_from_hlc: Option<u64>,
    pub backfill_complete: bool,
    pub peers: Vec<PeerStatus>,
}
