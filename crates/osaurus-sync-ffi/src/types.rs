//! Binding-facing records. Identifiers cross the boundary as lowercase hex strings.

use osaurus_sync_core::identity::Platform as CorePlatform;
use osaurus_sync_core::ids::{AgentId, DeviceKey, EventId, Principal, Role as CoreRole, ScopeId, UserKey};
use osaurus_sync_core::payload::{MessageRole as CoreMessageRole, ScopeKind as CoreScopeKind};
use osaurus_sync_core::sync::Notice;
use osaurus_sync_core::types as ct;
use osaurus_sync_net as net;

use crate::error::SyncError;

pub(crate) fn scope_id(s: &str) -> Result<ScopeId, SyncError> {
    ScopeId::from_hex(s).ok_or_else(|| SyncError::invalid("bad scope id"))
}
pub(crate) fn event_id(s: &str) -> Result<EventId, SyncError> {
    EventId::from_hex(s).ok_or_else(|| SyncError::invalid("bad message id"))
}
pub(crate) fn device_key(s: &str) -> Result<DeviceKey, SyncError> {
    DeviceKey::from_hex(s).ok_or_else(|| SyncError::invalid("bad device id"))
}
pub(crate) fn user_key(s: &str) -> Result<UserKey, SyncError> {
    UserKey::from_hex(s).ok_or_else(|| SyncError::invalid("bad user key"))
}
pub(crate) fn agent_id(s: &str) -> Result<AgentId, SyncError> {
    AgentId::from_hex(s).ok_or_else(|| SyncError::invalid("bad agent id"))
}
pub(crate) fn principal(s: &str) -> Result<Principal, SyncError> {
    Principal::from_hex(s).ok_or_else(|| SyncError::invalid("bad principal; expected u:<hex> or a:<hex>"))
}
pub(crate) fn hash32(s: &str) -> Result<[u8; 32], SyncError> {
    let v = hex_decode(s).ok_or_else(|| SyncError::invalid("bad hash"))?;
    v.try_into().map_err(|_| SyncError::invalid("bad hash"))
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Platform {
    MacOs,
    Ios,
    Windows,
    Server,
    Other,
}

impl From<Platform> for CorePlatform {
    fn from(p: Platform) -> Self {
        match p {
            Platform::MacOs => CorePlatform::MacOs,
            Platform::Ios => CorePlatform::Ios,
            Platform::Windows => CorePlatform::Windows,
            Platform::Server => CorePlatform::Server,
            Platform::Other => CorePlatform::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum HistoryMode {
    Full,
    Recent { days: u32 },
}

impl From<HistoryMode> for ct::HistoryMode {
    fn from(h: HistoryMode) -> Self {
        match h {
            HistoryMode::Full => ct::HistoryMode::Full,
            HistoryMode::Recent { days } => ct::HistoryMode::Recent { days },
        }
    }
}

/// Whether background backfill of older history may run. The platform helpers map Low Power
/// Mode and expensive or constrained networks to `Paused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum BackfillPolicy {
    Run,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MemberRole {
    Viewer,
    Member,
    Admin,
    Owner,
}

impl From<MemberRole> for CoreRole {
    fn from(r: MemberRole) -> Self {
        match r {
            MemberRole::Viewer => CoreRole::Viewer,
            MemberRole::Member => CoreRole::Member,
            MemberRole::Admin => CoreRole::Admin,
            MemberRole::Owner => CoreRole::Owner,
        }
    }
}

impl From<CoreRole> for MemberRole {
    fn from(r: CoreRole) -> Self {
        match r {
            CoreRole::Viewer => MemberRole::Viewer,
            CoreRole::Member => MemberRole::Member,
            CoreRole::Admin => MemberRole::Admin,
            CoreRole::Owner => MemberRole::Owner,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    ToolCall,
    ToolResult,
}

impl From<MessageRole> for CoreMessageRole {
    fn from(r: MessageRole) -> Self {
        match r {
            MessageRole::User => CoreMessageRole::User,
            MessageRole::Assistant => CoreMessageRole::Assistant,
            MessageRole::System => CoreMessageRole::System,
            MessageRole::ToolCall => CoreMessageRole::ToolCall,
            MessageRole::ToolResult => CoreMessageRole::ToolResult,
        }
    }
}

impl From<CoreMessageRole> for MessageRole {
    fn from(r: CoreMessageRole) -> Self {
        match r {
            CoreMessageRole::User => MessageRole::User,
            CoreMessageRole::Assistant => MessageRole::Assistant,
            CoreMessageRole::System => MessageRole::System,
            CoreMessageRole::ToolCall => MessageRole::ToolCall,
            CoreMessageRole::ToolResult => MessageRole::ToolResult,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ScopeKind {
    Conversation,
    Workspace,
}

impl From<CoreScopeKind> for ScopeKind {
    fn from(k: CoreScopeKind) -> Self {
        match k {
            CoreScopeKind::Conversation => ScopeKind::Conversation,
            CoreScopeKind::Workspace => ScopeKind::Workspace,
        }
    }
}

/// What this device publishes to the discovery server. See the client guide for what each
/// mode discloses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AddressPublishing {
    /// Only the home relay URL.
    RelayOnly,
    /// The relay URL and direct IP addresses. For servers with a public address.
    DirectAddresses,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NetworkConfig {
    /// Bring the endpoint up at open. When false, call `resume` to go online.
    pub enabled: bool,
    /// Relay server URLs. Empty means no relays: devices connect only directly.
    pub relays: Vec<String>,
    /// The discovery server's pkarr relay URL. None means no internet discovery.
    pub discovery_server: Option<String>,
    /// The DNS origin domain the discovery server serves records under.
    pub discovery_origin: Option<String>,
    pub publish: AddressPublishing,
    /// Local-network discovery. On iOS this needs the local network permission.
    pub mdns: bool,
    /// 0 picks any port.
    pub bind_port: u16,
}

impl NetworkConfig {
    pub(crate) fn to_net(&self) -> Result<net::NetConfig, SyncError> {
        let discovery = match (&self.discovery_server, &self.discovery_origin) {
            (None, None) => None,
            (Some(s), Some(o)) => Some(net::Discovery { pkarr_relay: s.clone(), origin_domain: o.clone() }),
            _ => return Err(SyncError::invalid("discovery_server and discovery_origin must be set together")),
        };
        let publish = match self.publish {
            AddressPublishing::RelayOnly => net::Publish::RelayOnly,
            AddressPublishing::DirectAddresses => net::Publish::DirectAddresses,
        };
        Ok(net::NetConfig { relays: self.relays.clone(), discovery, publish, mdns: self.mdns, bind_port: self.bind_port, ..net::NetConfig::default() })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CoreConfig {
    pub data_dir: String,
    pub device_name: String,
    pub platform: Platform,
    /// None uses the platform default (recent 90 days on iOS, full elsewhere).
    pub history: Option<HistoryMode>,
    pub network: NetworkConfig,
    /// The API version the bindings were generated for. Must equal `api_version()`.
    pub api_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Blob {
    pub hash: String,
    pub size: u64,
    pub media_type: String,
    pub name: String,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Reaction {
    pub value: String,
    pub count: u32,
    pub mine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum Delivery {
    Delivered,
    Pending,
    /// The event was refused by membership rules. `reason_code` is stable.
    Rejected {
        reason_code: u32,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Message {
    pub id: String,
    pub scope: String,
    pub hlc: u64,
    /// Milliseconds since the Unix epoch, from the HLC.
    pub timestamp_ms: u64,
    pub author_device: String,
    /// `u:<hex>` for a user, `a:<hex>` for an agent.
    pub author: String,
    pub is_agent: bool,
    pub role: MessageRole,
    pub content: String,
    pub blobs: Vec<Blob>,
    pub reply_to: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub edited: bool,
    pub delivery: Delivery,
    pub reactions: Vec<Reaction>,
}

impl From<ct::MessageView> for Message {
    fn from(m: ct::MessageView) -> Self {
        Message {
            id: m.id.to_hex(),
            scope: m.scope.to_hex(),
            hlc: m.hlc,
            timestamp_ms: osaurus_sync_core::hlc::physical_ms(m.hlc),
            author_device: m.author_device.to_hex(),
            author: m.author.to_hex(),
            is_agent: m.is_agent,
            role: m.role.into(),
            content: m.content,
            blobs: m
                .blobs
                .into_iter()
                .map(|b| Blob { hash: hex(&b.hash), size: b.size, media_type: b.media_type, name: b.name, available: b.available })
                .collect(),
            reply_to: m.reply_to.map(|i| i.to_hex()),
            tool_call_id: m.tool_call_id,
            tool_name: m.tool_name,
            edited: m.edited,
            delivery: match m.status {
                ct::DeliveryStatus::Delivered => Delivery::Delivered,
                ct::DeliveryStatus::Pending => Delivery::Pending,
                ct::DeliveryStatus::Rejected(r) => Delivery::Rejected { reason_code: r as u32, reason: format!("{r:?}") },
            },
            reactions: m.reactions.into_iter().map(|r| Reaction { value: r.reaction, count: r.count, mine: r.mine }).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PageCursor {
    pub hlc: u64,
    pub author: String,
    pub id: String,
}

impl PageCursor {
    pub(crate) fn to_core(&self) -> Result<ct::Cursor, SyncError> {
        Ok(ct::Cursor { hlc: self.hlc, author: device_key(&self.author)?, id: event_id(&self.id)? })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MessagePage {
    /// Oldest first.
    pub messages: Vec<Message>,
    pub next_cursor: Option<PageCursor>,
    /// Older history may exist on peers; call `request_history` to fetch it.
    pub at_sync_boundary: bool,
}

impl From<ct::MessagePage> for MessagePage {
    fn from(p: ct::MessagePage) -> Self {
        MessagePage {
            messages: p.messages.into_iter().map(Into::into).collect(),
            next_cursor: p.next_cursor.map(|c| PageCursor { hlc: c.hlc, author: c.author.to_hex(), id: c.id.to_hex() }),
            at_sync_boundary: p.at_sync_boundary,
        }
    }
}

/// A conversation rebuilt as it was at a past moment. Read-only: to revert, write new messages.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PointInTimeView {
    pub timestamp_ms: u64,
    pub conversation: Option<Conversation>,
    /// The newest messages as of `timestamp_ms`, oldest first.
    pub messages: Vec<Message>,
    /// False when this device compacted part of that history or keeps only recent history.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Member {
    pub principal: String,
    pub role: MemberRole,
}

/// An agent member of a scope and its current assignment.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AgentInfo {
    /// The agent id (hex of its compressed secp256k1 key), as in `a:<agent>` principals.
    pub agent: String,
    /// The agent's `0x` address, as osaurus-relay names it.
    pub address: String,
    pub role: MemberRole,
    /// The device that hosts the agent; None if the agent was never assigned.
    pub host: Option<String>,
    /// True when this device is the host.
    pub hosted_here: bool,
    /// When the assignment's binding expires.
    pub expires_ms: Option<u64>,
}

/// A verified agent card: who hosts an agent, and the user that host acts for.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AgentCardInfo {
    pub agent: String,
    pub address: String,
    /// The host's device id, for dialing and admission.
    pub host: String,
    /// The host's user key, for `invite`.
    pub user: String,
    pub expires_ms: u64,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Conversation {
    pub scope: String,
    pub kind: ScopeKind,
    pub title: String,
    pub pinned: bool,
    pub sort_order: Option<f64>,
    pub archived: bool,
    pub last_activity_hlc: u64,
    pub last_preview: Option<String>,
    pub unread: u32,
    pub my_role: Option<MemberRole>,
    pub members: Vec<Member>,
}

impl From<ct::ConversationSummary> for Conversation {
    fn from(c: ct::ConversationSummary) -> Self {
        Conversation {
            scope: c.scope.to_hex(),
            kind: c.kind.into(),
            title: c.title,
            pinned: c.pinned,
            sort_order: c.sort_order,
            archived: c.archived,
            last_activity_hlc: c.last_activity_hlc,
            last_preview: c.last_preview,
            unread: c.unread,
            my_role: c.my_role.map(Into::into),
            members: c.members.into_iter().map(|m| Member { principal: m.principal.to_hex(), role: m.role.into() }).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchResult {
    pub scope: String,
    pub id: String,
    pub snippet: String,
    pub hlc: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Device {
    pub id: String,
    pub user: String,
    pub name: String,
    pub platform: String,
    pub last_seen_ms: Option<u64>,
    pub revoked: bool,
    pub is_self: bool,
}

impl From<ct::DeviceInfo> for Device {
    fn from(d: ct::DeviceInfo) -> Self {
        Device {
            id: d.device.to_hex(),
            user: d.user.to_hex(),
            name: d.name,
            platform: d.platform,
            last_seen_ms: d.last_seen_ms,
            revoked: d.revoked,
            is_self: d.is_self,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AttachmentFile {
    pub path: String,
    pub media_type: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OutgoingMessage {
    pub role: MessageRole,
    pub content: String,
    pub attachments: Vec<AttachmentFile>,
    pub reply_to: Option<String>,
    /// Send as an agent this device hosts.
    pub as_agent: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
}

impl OutgoingMessage {
    pub(crate) fn to_core(&self) -> Result<ct::NewMessage, SyncError> {
        Ok(ct::NewMessage {
            role: self.role.into(),
            content: self.content.clone(),
            attachments: self
                .attachments
                .iter()
                .map(|a| ct::Attachment { path: a.path.clone().into(), media_type: a.media_type.clone(), name: a.name.clone() })
                .collect(),
            reply_to: self.reply_to.as_deref().map(event_id).transpose()?,
            as_agent: self.as_agent.as_deref().map(agent_id).transpose()?,
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            id: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LegacyMessage {
    pub role: MessageRole,
    pub content: String,
    pub created_ms: u64,
}

/// Result of `revoke_and_rotate`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct IdentityRotation {
    /// The new user key. Show the new recovery phrase; the old one no longer works.
    pub user: String,
    /// Conversations this user owned, now owned by the new identity.
    pub rotated: Vec<String>,
    /// Conversations owned by others that the old identity left. Their owners have to invite
    /// the new user key.
    pub left: Vec<String>,
}

impl From<ct::IdentityRotation> for IdentityRotation {
    fn from(r: ct::IdentityRotation) -> Self {
        IdentityRotation { user: r.user.to_hex(), rotated: r.rotated.iter().map(ScopeId::to_hex).collect(), left: r.left.iter().map(ScopeId::to_hex).collect() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PairingOffer {
    /// Encode as a QR code. Contains the code.
    pub payload: String,
    /// Shown for typing on the new device.
    pub code: String,
    pub device: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PeerState {
    pub device: String,
    pub name: String,
    pub connected: bool,
    pub relayed: bool,
    pub last_contact_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SyncStatus {
    pub scope: String,
    pub pending_outbox: u32,
    pub last_sync_ms: Option<u64>,
    pub synced_from_hlc: Option<u64>,
    pub backfill_complete: bool,
    pub peers: Vec<PeerState>,
}

impl From<ct::SyncStatus> for SyncStatus {
    fn from(s: ct::SyncStatus) -> Self {
        SyncStatus {
            scope: s.scope.to_hex(),
            pending_outbox: s.pending_outbox,
            last_sync_ms: s.last_sync_ms,
            synced_from_hlc: s.synced_from_hlc,
            backfill_complete: s.backfill_complete,
            peers: s
                .peers
                .into_iter()
                .map(|p| PeerState { device: p.device.to_hex(), name: p.name, connected: p.connected, relayed: p.relayed, last_contact_ms: p.last_contact_ms })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RelayState {
    pub url: String,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PeerConnection {
    pub device: String,
    pub name: String,
    pub connected: bool,
    pub established: bool,
    /// True when traffic goes through a relay rather than a direct path.
    pub relayed: bool,
    pub rtt_ms: Option<u64>,
    pub syncing: bool,
    pub storage_role: bool,
}

/// Disk space used by the core, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct StorageUsage {
    /// The database with its write-ahead log.
    pub database_bytes: u64,
    /// Encrypted attachments, including partial downloads.
    pub attachment_bytes: u64,
    /// Decrypted copies of opened attachments, evicted automatically past the cap.
    pub attachment_cache_bytes: u64,
}

impl From<osaurus_sync_core::replica::StorageUsage> for StorageUsage {
    fn from(u: osaurus_sync_core::replica::StorageUsage) -> Self {
        StorageUsage { database_bytes: u.database_bytes, attachment_bytes: u.blob_bytes, attachment_cache_bytes: u.attachment_cache_bytes }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ExportSummary {
    pub messages: u32,
    pub attachments: u32,
    /// Attachments this device hasn't downloaded: listed in the export without a file.
    pub missing_attachments: u32,
}

impl From<osaurus_sync_core::replica::ExportSummary> for ExportSummary {
    fn from(s: osaurus_sync_core::replica::ExportSummary) -> Self {
        ExportSummary { messages: s.messages, attachments: s.attachments, missing_attachments: s.missing_attachments }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Diagnostics {
    pub device: String,
    pub running: bool,
    pub home_relays: Vec<RelayState>,
    pub local_addrs: Vec<String>,
    pub backfill_paused: bool,
    pub peers: Vec<PeerConnection>,
    pub scopes: Vec<SyncStatus>,
    pub counters: SyncCounters,
}

/// Running totals since the core opened.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SyncCounters {
    pub sessions_established: u64,
    pub sessions_ended: u64,
    /// Peers turned away: not members, revoked, or with a bad certificate.
    pub peers_rejected: u64,
    pub events_received: u64,
    pub event_bytes_received: u64,
    pub applied: u64,
    /// Stored but rejected by the membership rules.
    pub rejected: u64,
    /// Waiting for control events they depend on.
    pub held: u64,
    pub duplicates: u64,
    /// Failed to verify or decode.
    pub invalid: u64,
    /// Peers disconnected for exceeding a limit.
    pub limit_hits: u64,
    pub pairings: u64,
    pub pairing_failures: u64,
    /// Sync protocol bytes.
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl From<net::Diagnostics> for Diagnostics {
    fn from(d: net::Diagnostics) -> Self {
        Diagnostics {
            device: d.device.to_hex(),
            running: d.running,
            home_relays: d.home_relays.into_iter().map(|(url, connected)| RelayState { url, connected }).collect(),
            local_addrs: d.local_addrs,
            backfill_paused: d.backfill_paused,
            peers: d
                .peers
                .into_iter()
                .map(|p| PeerConnection {
                    device: p.device.to_hex(),
                    name: p.name,
                    connected: p.connected,
                    established: p.established,
                    relayed: p.relayed,
                    rtt_ms: p.rtt_ms,
                    syncing: p.syncing,
                    storage_role: p.storage_role,
                })
                .collect(),
            scopes: d.scopes.into_iter().map(Into::into).collect(),
            counters: SyncCounters {
                sessions_established: d.counters.sessions_established,
                sessions_ended: d.counters.sessions_ended,
                peers_rejected: d.counters.peers_rejected,
                events_received: d.counters.events_received,
                event_bytes_received: d.counters.event_bytes_received,
                applied: d.counters.applied,
                rejected: d.counters.rejected,
                held: d.counters.held,
                duplicates: d.counters.duplicates,
                invalid: d.counters.invalid,
                limit_hits: d.counters.limit_hits,
                pairings: d.counters.pairings,
                pairing_failures: d.counters.pairing_failures,
                bytes_sent: d.bytes_sent,
                bytes_received: d.bytes_received,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StreamState {
    Streaming,
    Finished,
    Interrupted,
}

impl From<ct::StreamState> for StreamState {
    fn from(s: ct::StreamState) -> Self {
        match s {
            ct::StreamState::Streaming => StreamState::Streaming,
            ct::StreamState::Finished => StreamState::Finished,
            ct::StreamState::Interrupted => StreamState::Interrupted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StreamUpdate {
    pub message_id: String,
    pub host: String,
    pub as_agent: Option<String>,
    /// The full text so far.
    pub text: String,
    pub state: StreamState,
}

/// Changes for one subscribed scope.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ScopeChange {
    MessageAdded {
        message: Message,
    },
    MessageChanged {
        message: Message,
    },
    MessageRemoved {
        id: String,
    },
    /// Membership changed in a way that invalidated earlier events; reload the scope.
    Reset,
    Stream {
        update: StreamUpdate,
    },
    SyncStatusChanged,
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum ConversationChange {
    Changed { conversation: Conversation },
    Removed { scope: String },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CoreEvent {
    DevicesChanged,
    /// This device was revoked; sync has stopped.
    SelfRevoked,
    /// Pairing or recovery replaced the user identity; reload everything.
    IdentityChanged,
    /// A restored database for another identity was found. Prompt for its recovery phrase and
    /// call `recover_from_phrase` to unlock its history.
    RecoveryAvailable,
    Paired {
        device: String,
    },
    PairingFailed {
        reason: String,
    },
    InviteResult {
        scope: String,
        error: Option<String>,
    },
    PeerRevoked {
        device: String,
    },
    /// A device that isn't this user's or a member's tried to sync and was turned away.
    PeerRejected {
        device: String,
    },
    IncompatiblePeer {
        device: String,
        version: u32,
    },
}

impl From<Notice> for CoreEvent {
    fn from(n: Notice) -> Self {
        match n {
            Notice::Paired { device } => CoreEvent::Paired { device: device.to_hex() },
            Notice::PairingFailed(reason) => CoreEvent::PairingFailed { reason },
            Notice::InviteResult { scope, error } => CoreEvent::InviteResult { scope: scope.to_hex(), error },
            Notice::PeerRevoked(d) => CoreEvent::PeerRevoked { device: d.to_hex() },
            Notice::PeerRejected(d) => CoreEvent::PeerRejected { device: d.to_hex() },
            Notice::Incompatible { device, version } => CoreEvent::IncompatiblePeer { device: device.to_hex(), version },
        }
    }
}

pub(crate) fn stream_update(u: ct::StreamUpdate) -> StreamUpdate {
    StreamUpdate { message_id: u.message_id.to_hex(), host: u.host.to_hex(), as_agent: u.as_agent.map(|a| a.to_hex()), text: u.text, state: u.state.into() }
}
