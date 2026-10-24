//! UniFFI surface for the Swift and C# packages. The only crate that knows about bindings.
//!
//! The core owns a multi-threaded tokio runtime. Listener callbacks are delivered on one
//! dedicated core thread, in commit order, never while internal locks are held.

mod error;
mod hub;
mod types;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use osaurus_sync_core::env::{SystemEnv, random_array};
use osaurus_sync_core::ids::{EventId, ScopeId};
use osaurus_sync_core::keys::UserSecret;
use osaurus_sync_core::payload::ScopeKind as CoreScopeKind;
use osaurus_sync_core::types::{ImportedMessage, NewMessage, StreamState as CoreStreamState};
use osaurus_sync_core::{Replica, ReplicaConfig, SecretStore};
use osaurus_sync_net::SyncNode;
use tokio::runtime::{Handle, Runtime};

pub use error::SyncError;
pub use hub::Subscription;
pub use types::*;

uniffi::setup_scaffolding!();

/// Bumped whenever the exported surface changes incompatibly.
pub const API_VERSION: u32 = 1;

#[uniffi::export]
pub fn api_version() -> u32 {
    API_VERSION
}

/// Defaults for `CoreConfig.network`: Osaurus relays, default discovery, mDNS on.
#[uniffi::export]
pub fn default_network_config() -> NetworkConfig {
    NetworkConfig { enabled: true, relays: vec![], relays_disabled: false, dns_server: None, dns_origin: None, dns_disabled: false, mdns: true, bind_port: 0 }
}

/// No relays, discovery or mDNS. For tests and air-gapped setups.
#[uniffi::export]
pub fn local_network_config() -> NetworkConfig {
    NetworkConfig { relays_disabled: true, dns_disabled: true, mdns: false, ..default_network_config() }
}

/// Stores device and user secrets in the OS key store (Keychain, DPAPI). Secrets never touch
/// the database.
#[uniffi::export(callback_interface)]
pub trait KeyStore: Send + Sync {
    fn load(&self, name: String) -> Option<Vec<u8>>;
    fn store(&self, name: String, value: Vec<u8>) -> Result<(), SyncError>;
    fn delete(&self, name: String) -> Result<(), SyncError>;
}

#[uniffi::export(callback_interface)]
pub trait CoreListener: Send + Sync {
    fn on_event(&self, event: CoreEvent);
}

#[uniffi::export(callback_interface)]
pub trait ScopeListener: Send + Sync {
    fn on_changes(&self, changes: Vec<ScopeChange>);
}

#[uniffi::export(callback_interface)]
pub trait ConversationListener: Send + Sync {
    fn on_changes(&self, changes: Vec<ConversationChange>);
}

struct KeyStoreAdapter(Box<dyn KeyStore>);

impl SecretStore for KeyStoreAdapter {
    fn load(&self, name: &str) -> Option<Vec<u8>> {
        self.0.load(name.to_string())
    }
    fn store(&self, name: &str, value: &[u8]) -> osaurus_sync_core::Result<()> {
        self.0.store(name.to_string(), value.to_vec()).map_err(|e| osaurus_sync_core::Error::Storage(format!("key store: {e}")))
    }
    fn delete(&self, name: &str) -> osaurus_sync_core::Result<()> {
        self.0.delete(name.to_string()).map_err(|e| osaurus_sync_core::Error::Storage(format!("key store: {e}")))
    }
}

#[derive(uniffi::Object)]
pub struct SyncCore {
    rt: Mutex<Option<Runtime>>,
    handle: Handle,
    node: SyncNode,
    hub: Arc<hub::Hub>,
    closed: AtomicBool,
}

impl Drop for SyncCore {
    fn drop(&mut self) {
        self.hub.stop();
        if let Some(rt) = self.rt.lock().unwrap_or_else(|e| e.into_inner()).take() {
            rt.shutdown_background();
        }
    }
}

type R<T> = Result<T, SyncError>;

impl SyncCore {
    fn live(&self) -> R<()> {
        if self.closed.load(Ordering::Acquire) { Err(SyncError::closed()) } else { Ok(()) }
    }

    fn write<T>(&self, f: impl FnOnce(&mut Replica) -> osaurus_sync_core::Result<T>) -> R<T> {
        self.live()?;
        Ok(self.node.write(f)?)
    }

    fn read<T>(&self, f: impl FnOnce(&mut Replica) -> osaurus_sync_core::Result<T>) -> R<T> {
        self.live()?;
        Ok(self.node.read(f)?)
    }

    async fn on_runtime<T: Send + 'static>(&self, f: impl Future<Output = R<T>> + Send + 'static) -> R<T> {
        self.live()?;
        self.handle.spawn(f).await.map_err(|e| SyncError::from(osaurus_sync_core::Error::Internal(e.to_string())))?
    }
}

#[uniffi::export]
impl SyncCore {
    /// Opens the core for a data directory. Fails with `AlreadyOpen` if another instance holds
    /// it and with `IncompatibleVersion` if the bindings were built for another API version.
    #[uniffi::constructor]
    pub fn open(config: CoreConfig, key_store: Box<dyn KeyStore>) -> R<Arc<Self>> {
        if config.api_version != API_VERSION {
            return Err(osaurus_sync_core::Error::IncompatibleVersion { core: API_VERSION, bindings: config.api_version }.into());
        }
        let net_cfg = config.network.to_net()?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("osaurus-sync")
            .enable_all()
            .build()
            .map_err(|e| osaurus_sync_core::Error::Internal(e.to_string()))?;
        let mut rcfg = ReplicaConfig::new(&config.data_dir, &config.device_name, config.platform.into());
        rcfg.history_mode = config.history.map(Into::into);
        let replica = Replica::open(rcfg, Arc::new(SystemEnv), Arc::new(KeyStoreAdapter(key_store)))?;
        let hub = hub::Hub::start();
        let node = SyncNode::offline(replica, net_cfg, hub.clone(), rt.handle().clone());
        if config.network.enabled {
            let n = node.clone();
            rt.block_on(async move { n.resume().await })?;
        }
        Ok(Arc::new(SyncCore { handle: rt.handle().clone(), rt: Mutex::new(Some(rt)), node, hub, closed: AtomicBool::new(false) }))
    }

    pub fn api_version(&self) -> u32 {
        API_VERSION
    }

    /// Receives device, identity and pairing events.
    pub fn set_listener(&self, listener: Option<Box<dyn CoreListener>>) {
        self.hub.set_core(listener);
    }

    // ---- lifecycle ----

    /// Call when the app moves to the background (required on iOS). Flushes Loro snapshots,
    /// checkpoints the WAL and closes the endpoint. Local reads and writes keep working; the
    /// outbox waits for `resume`.
    pub async fn suspend(&self) -> R<()> {
        let node = self.node.clone();
        self.on_runtime(async move { Ok(node.suspend().await?) }).await
    }

    /// Rebinds the endpoint, reconnects and catches up.
    pub async fn resume(&self) -> R<()> {
        let node = self.node.clone();
        self.on_runtime(async move { Ok(node.resume().await?) }).await
    }

    /// Stops networking, flushes and releases the data directory. Further calls fail with `Closed`.
    pub async fn close(&self) -> R<()> {
        let node = self.node.clone();
        let res = self.on_runtime(async move { Ok(node.shutdown().await?) }).await;
        self.closed.store(true, Ordering::Release);
        self.hub.stop();
        res
    }

    pub fn is_online(&self) -> bool {
        self.node.is_running()
    }

    /// Dial known devices now instead of waiting for the retry interval.
    pub fn sync_now(&self) {
        self.node.sync_now();
    }

    /// This device's direct addresses, for QR payloads or setups without discovery.
    pub fn local_address(&self) -> Option<String> {
        self.node.addr_string()
    }

    /// Teaches the endpoint another device's address (from `local_address`).
    pub fn add_peer_address(&self, address: String) -> R<()> {
        self.node.add_peer_addr_string(&address).map_err(|e| SyncError::invalid(format!("{e:#}")))
    }

    // ---- identity ----

    pub fn device_key(&self) -> String {
        self.node.device_key().to_hex()
    }

    pub fn user_key(&self) -> R<String> {
        self.read(|r| Ok(r.user_key().to_hex()))
    }

    pub fn recovery_phrase(&self) -> R<Vec<String>> {
        self.read(|r| Ok(r.recovery_phrase()))
    }

    /// Whether this data directory was restored from another device's backup and its history
    /// is waiting for that identity's recovery phrase. Also signalled by `RecoveryAvailable`.
    pub fn recovery_available(&self) -> R<bool> {
        self.read(|r| r.recovery_available())
    }

    /// Replaces this device's user identity with the one derived from the phrase. Existing
    /// local conversations are kept and shared with the recovered identity, and history sealed
    /// to that identity's recovery key (including a restored database's) becomes readable.
    pub fn recover_from_phrase(&self, phrase: String) -> R<()> {
        let user = UserSecret::from_phrase(&phrase)?;
        self.write(|r| r.adopt_user(user))?;
        self.node.sync_now();
        Ok(())
    }

    // ---- conversations and messages ----

    pub fn create_conversation(&self, title: String) -> R<String> {
        self.write(|r| r.create_scope(CoreScopeKind::Conversation, &title)).map(|s| s.to_hex())
    }

    pub fn create_workspace(&self, title: String) -> R<String> {
        self.write(|r| r.create_scope(CoreScopeKind::Workspace, &title)).map(|s| s.to_hex())
    }

    /// First run: imports a legacy conversation, keeping its timestamps in the HLC.
    pub fn import_legacy_conversation(&self, title: String, created_ms: u64, messages: Vec<LegacyMessage>) -> R<String> {
        let msgs: Vec<ImportedMessage> =
            messages.into_iter().map(|m| ImportedMessage { role: m.role.into(), content: m.content, created_ms: m.created_ms }).collect();
        self.write(|r| r.import_conversation(&title, created_ms, &msgs)).map(|s| s.to_hex())
    }

    pub fn send_message(&self, scope: String, message: OutgoingMessage) -> R<String> {
        let scope = scope_id(&scope)?;
        let m = message.to_core()?;
        self.write(|r| r.send_message(scope, m)).map(|id| id.to_hex())
    }

    pub fn edit_message(&self, id: String, content: String) -> R<()> {
        let id = event_id(&id)?;
        self.write(|r| r.edit_message(id, &content))
    }

    pub fn delete_message(&self, id: String) -> R<()> {
        let id = event_id(&id)?;
        self.write(|r| r.delete_message(id))
    }

    /// One page of messages, oldest first. Pass `next_cursor` as `before` to page back.
    pub fn messages(&self, scope: String, before: Option<PageCursor>, limit: u32) -> R<MessagePage> {
        let scope = scope_id(&scope)?;
        let before = before.as_ref().map(PageCursor::to_core).transpose()?;
        self.read(|r| r.messages(scope, before, limit)).map(Into::into)
    }

    pub fn message(&self, id: String) -> R<Option<Message>> {
        let id = event_id(&id)?;
        self.read(|r| r.message_view(&id)).map(|m| m.map(Into::into))
    }

    pub fn conversations(&self) -> R<Vec<Conversation>> {
        self.read(|r| r.conversations()).map(|v| v.into_iter().map(Into::into).collect())
    }

    pub fn conversation(&self, scope: String) -> R<Option<Conversation>> {
        let scope = scope_id(&scope)?;
        self.read(|r| r.conversation(scope)).map(|c| c.map(Into::into))
    }

    /// Rebuilds a conversation as of `timestamp_ms` by replaying its events up to then.
    pub fn view_at(&self, scope: String, timestamp_ms: u64, limit: u32) -> R<PointInTimeView> {
        let scope = scope_id(&scope)?;
        let at = osaurus_sync_core::hlc::pack(timestamp_ms, u16::MAX);
        let v = self.read(|r| r.view_at(scope, at, limit))?;
        Ok(PointInTimeView {
            timestamp_ms,
            conversation: v.conversation.map(Into::into),
            messages: v.messages.into_iter().map(Into::into).collect(),
            complete: v.complete,
        })
    }

    pub fn search(&self, query: String, limit: u32) -> R<Vec<SearchResult>> {
        self.read(|r| r.search(&query, limit))
            .map(|v| v.into_iter().map(|h| SearchResult { scope: h.scope.to_hex(), id: h.id.to_hex(), snippet: h.snippet, hlc: h.hlc }).collect())
    }

    pub fn rename_conversation(&self, scope: String, title: String) -> R<()> {
        let scope = scope_id(&scope)?;
        self.write(|r| r.rename_conversation(scope, &title))
    }

    pub fn set_pinned(&self, scope: String, pinned: bool) -> R<()> {
        let scope = scope_id(&scope)?;
        self.write(|r| r.set_pinned(scope, pinned))
    }

    pub fn set_archived(&self, scope: String, archived: bool) -> R<()> {
        let scope = scope_id(&scope)?;
        self.write(|r| r.set_archived(scope, archived))
    }

    /// Fractional ordering for manual reorder.
    pub fn set_sort_order(&self, scope: String, order: f64) -> R<()> {
        let scope = scope_id(&scope)?;
        self.write(|r| r.set_sort_order(scope, order))
    }

    pub fn mark_read(&self, scope: String, hlc: u64) -> R<()> {
        let scope = scope_id(&scope)?;
        self.write(|r| r.mark_read(scope, hlc))
    }

    pub fn add_reaction(&self, id: String, reaction: String) -> R<()> {
        let id = event_id(&id)?;
        self.write(|r| r.add_reaction(id, &reaction))
    }

    pub fn remove_reaction(&self, id: String, reaction: String) -> R<()> {
        let id = event_id(&id)?;
        self.write(|r| r.remove_reaction(id, &reaction))
    }

    /// Path to the decrypted attachment, downloading it from a peer if needed.
    pub async fn fetch_attachment(&self, scope: String, hash: String) -> R<String> {
        let scope = scope_id(&scope)?;
        let hash = hash32(&hash)?;
        let node = self.node.clone();
        self.on_runtime(async move { Ok(node.fetch_blob(scope, hash).await?.to_string_lossy().into_owned()) }).await
    }

    // ---- live updates ----

    /// Delivers message, stream and sync-status changes for one scope until cancelled.
    pub fn subscribe(&self, scope: String, listener: Box<dyn ScopeListener>) -> R<Arc<Subscription>> {
        let scope = scope_id(&scope)?;
        Ok(self.hub.subscribe_scope(scope, listener))
    }

    /// Delivers conversation list changes until cancelled.
    pub fn subscribe_conversations(&self, listener: Box<dyn ConversationListener>) -> Arc<Subscription> {
        self.hub.subscribe_conversations(listener)
    }

    /// Starts streaming a response. Tokens go to connected devices directly and are never
    /// stored; `finish` commits the final message under the stream's id.
    pub fn open_stream(&self, scope: String, role: MessageRole, as_agent: Option<String>) -> R<Arc<OutgoingStream>> {
        let scope_v = scope_id(&scope)?;
        let agent = as_agent.as_deref().map(agent_id).transpose()?;
        let id = self.read(|r| Ok(r.allocate_message_id()))?;
        Ok(Arc::new(OutgoingStream {
            node: self.node.clone(),
            scope: scope_v,
            id,
            role,
            as_agent: agent,
            state: Mutex::new(StreamBuf { seq: 0, text: String::new(), done: false }),
        }))
    }

    // ---- membership ----

    /// A one-time invite link for `scope`.
    pub fn create_invite(&self, scope: String, role: MemberRole, ttl_ms: u64) -> R<String> {
        let scope = scope_id(&scope)?;
        self.write(|r| r.create_invite(scope, role.into(), ttl_ms)).map(|t| t.encode())
    }

    /// Redeems an invite. The result arrives as `CoreEvent::InviteResult`.
    pub fn redeem_invite(&self, token: String) -> R<()> {
        self.live()?;
        Ok(self.node.redeem_invite(&token)?)
    }

    /// Adds a user by key.
    pub fn invite(&self, scope: String, user: String, role: MemberRole) -> R<()> {
        let scope = scope_id(&scope)?;
        let user = user_key(&user)?;
        self.write(|r| r.add_member(scope, user, role.into()))
    }

    pub fn remove(&self, scope: String, principal: String) -> R<()> {
        let scope = scope_id(&scope)?;
        let p = types::principal(&principal)?;
        self.write(|r| r.remove_member(scope, p))
    }

    pub fn change_role(&self, scope: String, principal: String, role: MemberRole) -> R<()> {
        let scope = scope_id(&scope)?;
        let p = types::principal(&principal)?;
        self.write(|r| r.change_role(scope, p, role.into()))
    }

    // ---- devices ----

    pub fn devices(&self) -> R<Vec<Device>> {
        self.read(|r| r.devices()).map(|v| v.into_iter().map(Into::into).collect())
    }

    pub fn revoke_device(&self, device: String) -> R<()> {
        let d = types::device_key(&device)?;
        self.write(|r| r.revoke_device(d))
    }

    /// Starts pairing on this (existing) device. Show `payload` as a QR code or `code` for
    /// typing. Valid for `ttl_ms`.
    pub fn pairing_offer(&self, ttl_ms: u64) -> R<PairingOffer> {
        self.live()?;
        let inv = self.node.start_pairing(ttl_ms);
        Ok(PairingOffer { payload: inv.encode(), code: inv.code.clone(), device: inv.device.to_hex() })
    }

    pub fn cancel_pairing(&self) {
        self.node.cancel_pairing();
    }

    /// On the new device: joins using a scanned payload, or `osaurus-pair:1:<device>:<code>`
    /// built from a typed code. Completion arrives as `CoreEvent::Paired` or `PairingFailed`.
    pub async fn pair_with_offer(&self, payload: String) -> R<()> {
        let node = self.node.clone();
        self.on_runtime(async move { Ok(node.pair(&payload).await?) }).await
    }

    /// Admits a device of this user that is known but not yet in every scope (for example
    /// one recovered from the phrase). Returns the scopes it was added to.
    pub fn approve_device(&self, device: String) -> R<Vec<String>> {
        let d = types::device_key(&device)?;
        self.write(|r| r.approve_device(d)).map(|v| v.into_iter().map(|s| s.to_hex()).collect())
    }

    // ---- agents ----

    /// Assigns an agent (by its secp256k1 secret) to a scope, hosted by `host` or this device.
    pub fn assign_agent(&self, scope: String, agent_secret: Vec<u8>, host: Option<String>, role: MemberRole, ttl_ms: u64) -> R<String> {
        let scope = scope_id(&scope)?;
        let host = host.as_deref().map(types::device_key).transpose()?;
        self.write(|r| r.assign_agent(scope, &agent_secret, host, role.into(), ttl_ms)).map(|a| a.to_hex())
    }

    /// Publishes a signed record in this device's discovery entry naming it as the agent's
    /// host. Pass None to withdraw.
    pub fn publish_agent_record(&self, agent_secret: Option<Vec<u8>>, ttl_ms: u64) -> R<Option<String>> {
        self.live()?;
        let binding = match agent_secret {
            Some(secret) => {
                let expires = osaurus_sync_core::env::Env::now_ms(&SystemEnv) + ttl_ms;
                Some(osaurus_sync_core::identity::AgentBinding::sign(&secret, self.node.device_key(), expires)?)
            }
            None => None,
        };
        let id = binding.as_ref().map(|b| b.agent.to_hex());
        self.node.publish_agent(binding).map_err(|e| SyncError::invalid(format!("{e:#}")))?;
        Ok(id)
    }

    /// Looks up the agent a device publishes as host. Returns the verified agent id.
    pub async fn resolve_agent(&self, host: String) -> R<Option<String>> {
        let host = types::device_key(&host)?;
        let node = self.node.clone();
        self.on_runtime(async move { Ok(node.resolve_agent(host).await?.map(|b| b.agent.to_hex())) }).await
    }

    // ---- gradual sync ----

    pub fn set_backfill_policy(&self, policy: BackfillPolicy) -> R<()> {
        self.live()?;
        Ok(self.node.set_backfill_paused(policy == BackfillPolicy::Paused)?)
    }

    /// Sets the history mode for one scope, or the default for new scopes when `scope` is None.
    pub fn set_history_mode(&self, scope: Option<String>, mode: HistoryMode) -> R<()> {
        let scope = scope.as_deref().map(scope_id).transpose()?;
        self.write(|r| r.set_history_mode(scope, mode.into()))
    }

    /// Fetches `days` of history older than `before_hlc` at high priority.
    pub fn request_history(&self, scope: String, before_hlc: u64, days: u32) -> R<()> {
        let scope = scope_id(&scope)?;
        self.live()?;
        Ok(self.node.request_history(scope, before_hlc, days)?)
    }

    // ---- status ----

    pub fn sync_status(&self, scope: String) -> R<SyncStatus> {
        let scope = scope_id(&scope)?;
        self.live()?;
        Ok(self.node.sync_status(scope)?.into())
    }

    pub fn diagnostics(&self) -> Diagnostics {
        self.node.diagnostics().into()
    }
}

struct StreamBuf {
    seq: u32,
    text: String,
    done: bool,
}

/// A response being streamed. Dropping it without `finish` aborts.
#[derive(uniffi::Object)]
pub struct OutgoingStream {
    node: SyncNode,
    scope: ScopeId,
    id: EventId,
    role: MessageRole,
    as_agent: Option<osaurus_sync_core::ids::AgentId>,
    state: Mutex<StreamBuf>,
}

impl OutgoingStream {
    fn emit(&self, b: &mut StreamBuf, state: CoreStreamState) {
        b.seq += 1;
        self.node.stream_update(self.scope, self.id, self.as_agent, b.seq, &b.text, state);
    }
}

impl Drop for OutgoingStream {
    fn drop(&mut self) {
        let mut b = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !b.done {
            b.done = true;
            b.seq += 1;
            self.node.stream_update(self.scope, self.id, self.as_agent, b.seq, &b.text, CoreStreamState::Interrupted);
        }
    }
}

#[uniffi::export]
impl OutgoingStream {
    /// The id the committed message will have.
    pub fn message_id(&self) -> String {
        self.id.to_hex()
    }

    /// Appends tokens.
    pub fn push(&self, delta: String) -> R<()> {
        let mut b = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if b.done {
            return Err(SyncError::invalid("stream already ended"));
        }
        b.text.push_str(&delta);
        self.emit(&mut b, CoreStreamState::Streaming);
        Ok(())
    }

    /// Commits the message (the streamed text, or `content` if given) and ends the stream.
    pub fn finish(&self, content: Option<String>) -> R<String> {
        let mut b = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if b.done {
            return Err(SyncError::invalid("stream already ended"));
        }
        if let Some(c) = content {
            b.text = c;
        }
        let mut m = NewMessage::text(self.role.into(), b.text.clone());
        m.as_agent = self.as_agent;
        m.id = Some(self.id);
        self.node.write(|r| r.send_message(self.scope, m))?;
        b.done = true;
        self.emit(&mut b, CoreStreamState::Finished);
        Ok(self.id.to_hex())
    }

    /// Ends the stream without committing anything.
    pub fn abort(&self) {
        let mut b = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !b.done {
            b.done = true;
            self.emit(&mut b, CoreStreamState::Interrupted);
        }
    }
}

/// Random 32-byte secret for a new agent identity.
#[uniffi::export]
pub fn generate_agent_secret() -> Vec<u8> {
    random_array::<32>(&SystemEnv).to_vec()
}
