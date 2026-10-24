//! Iroh networking for Osaurus sync.
//!
//! One endpoint per device, keyed by the device's Ed25519 key, so the EndpointId is the
//! DeviceKey. Three ALPNs: `osaurus/sync/1` carries the engine's messages on one bidirectional
//! stream per connection, `osaurus/stream/1` carries ephemeral streaming tokens on
//! unidirectional streams, and `osaurus/blobs/1` serves encrypted attachments with resumable
//! range reads. All decisions live in the no-I/O engine; this crate moves bytes and dials.

pub mod agent_record;
mod blobs;
mod frame;
mod streams;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use iroh::address_lookup::memory::MemoryLookup;
use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher, PkarrResolver};
use iroh::endpoint::{Connection, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey, Watcher};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use n0_future::StreamExt;
use osaurus_sync_core::Replica;
use osaurus_sync_core::identity::AgentBinding;
use osaurus_sync_core::ids::{AgentId, DeviceKey, EventId, ScopeId};
use osaurus_sync_core::replica::InviteToken;
use osaurus_sync_core::sync::{Engine, Msg, Notice, Out, PairingInvite};
use osaurus_sync_core::types::{Change, PeerStatus, StreamState, SyncStatus};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub const ALPN_SYNC: &[u8] = b"osaurus/sync/1";
pub const ALPN_STREAM: &[u8] = b"osaurus/stream/1";
pub const ALPN_BLOBS: &[u8] = b"osaurus/blobs/1";

const MDNS_SERVICE: &str = "osaurus-sync";
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const MAINTAIN_EVERY: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relays {
    /// The Osaurus relay fleet. Until it is deployed this is the n0 production relays.
    Default,
    Disabled,
    Custom(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dns {
    /// Publish and resolve through the default discovery server.
    Default,
    /// A self-hosted pkarr relay and DNS origin.
    Custom {
        pkarr_relay: String,
        origin_domain: String,
    },
    Disabled,
}

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub relays: Relays,
    pub dns: Dns,
    pub mdns: bool,
    /// 0 picks any port.
    pub bind_port: u16,
    /// How often to retry dialing known devices that are not connected.
    pub dial_interval: Duration,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig { relays: Relays::Default, dns: Dns::Default, mdns: true, bind_port: 0, dial_interval: Duration::from_secs(30) }
    }
}

impl NetConfig {
    /// Loopback-only: no relays, no DNS, no mDNS. Peers are added with `add_peer_addr`.
    pub fn local() -> Self {
        NetConfig { relays: Relays::Disabled, dns: Dns::Disabled, mdns: false, bind_port: 0, dial_interval: Duration::from_secs(5) }
    }
}

/// Receives view changes and engine notices. Called from network tasks; keep it cheap.
pub trait Listener: Send + Sync + 'static {
    fn changes(&self, changes: Vec<Change>);
    fn notice(&self, notice: Notice);
}

pub struct State {
    pub replica: Replica,
    pub engine: Engine,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerDiag {
    pub device: DeviceKey,
    pub name: String,
    pub connected: bool,
    pub established: bool,
    pub relayed: bool,
    pub rtt_ms: Option<u64>,
    pub syncing: bool,
    pub storage_role: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostics {
    pub device: DeviceKey,
    pub running: bool,
    pub home_relays: Vec<(String, bool)>,
    pub local_addrs: Vec<String>,
    pub backfill_paused: bool,
    pub peers: Vec<PeerDiag>,
    pub scopes: Vec<SyncStatus>,
}

enum Cmd {
    Frame(Vec<u8>),
    Close,
}

struct Peer {
    id: u64,
    conn: Connection,
    dialed: bool,
    tx: mpsc::UnboundedSender<Cmd>,
}

struct Net {
    endpoint: Endpoint,
    router: Router,
    tasks: Vec<JoinHandle<()>>,
}

pub(crate) struct Inner {
    state: Mutex<State>,
    peers: Mutex<HashMap<DeviceKey, Peer>>,
    dialing: Mutex<HashSet<DeviceKey>>,
    backoff: Mutex<HashMap<DeviceKey, (Instant, Duration)>>,
    net: tokio::sync::Mutex<Option<Net>>,
    endpoint: Mutex<Option<Endpoint>>,
    listener: Arc<dyn Listener>,
    cfg: NetConfig,
    secret: [u8; 32],
    me: DeviceKey,
    wake: Notify,
    next_conn: AtomicU64,
    memory: MemoryLookup,
    agent_record: Mutex<Option<AgentBinding>>,
    pub(crate) streams: streams::Incoming,
    pub(crate) rt: tokio::runtime::Handle,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncNode").field("device", &self.me).finish()
    }
}

#[derive(Clone)]
pub struct SyncNode {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn device_of(id: EndpointId) -> DeviceKey {
    DeviceKey(*id.as_bytes())
}

pub(crate) fn endpoint_id(d: &DeviceKey) -> Result<EndpointId> {
    EndpointId::from_bytes(&d.0).map_err(|e| anyhow!("bad device key: {e}"))
}

pub fn encode_addr(a: &EndpointAddr) -> String {
    let mut s = device_of(a.id).to_hex();
    for ip in a.ip_addrs() {
        s.push_str(&format!(";{ip}"));
    }
    for r in a.relay_urls() {
        s.push_str(&format!(";relay={r}"));
    }
    s
}

pub fn decode_addr(s: &str) -> Result<EndpointAddr> {
    let mut parts = s.trim().split(';');
    let id = DeviceKey::from_hex(parts.next().unwrap_or_default()).ok_or_else(|| anyhow!("bad device id"))?;
    let mut a = EndpointAddr::new(endpoint_id(&id)?);
    for p in parts.filter(|p| !p.is_empty()) {
        a = match p.strip_prefix("relay=") {
            Some(url) => a.with_relay_url(url.parse().context("relay url")?),
            None => a.with_ip_addr(p.parse().context("socket address")?),
        };
    }
    Ok(a)
}

impl SyncNode {
    /// Binds the endpoint and starts accepting and dialing. Must run inside a multi-threaded
    /// tokio runtime.
    pub async fn start(replica: Replica, cfg: NetConfig, listener: Arc<dyn Listener>) -> Result<Self> {
        let node = Self::offline(replica, cfg, listener, tokio::runtime::Handle::current());
        node.inner.bring_up().await?;
        Ok(node)
    }

    /// Wraps the replica without binding an endpoint. `resume` brings networking up. Network
    /// tasks run on `rt`, so every method may be called from any thread.
    pub fn offline(replica: Replica, cfg: NetConfig, listener: Arc<dyn Listener>, rt: tokio::runtime::Handle) -> Self {
        let secret = replica.device_secret().to_bytes();
        let me = replica.device_key();
        let inner = Arc::new(Inner {
            state: Mutex::new(State { replica, engine: Engine::new() }),
            peers: Mutex::default(),
            dialing: Mutex::default(),
            backoff: Mutex::default(),
            net: tokio::sync::Mutex::new(None),
            endpoint: Mutex::new(None),
            listener,
            cfg,
            secret,
            me,
            wake: Notify::new(),
            next_conn: AtomicU64::new(1),
            memory: MemoryLookup::new(),
            agent_record: Mutex::new(None),
            streams: streams::Incoming::default(),
            rt,
        });
        SyncNode { inner }
    }

    pub fn device_key(&self) -> DeviceKey {
        self.inner.me
    }

    /// Runs a write against the replica, then lets the engine push the result.
    pub fn write<T>(&self, f: impl FnOnce(&mut Replica) -> osaurus_sync_core::Result<T>) -> osaurus_sync_core::Result<T> {
        self.inner.write(f)
    }

    /// Runs a read against the replica. Some reads need `&mut` for caches.
    pub fn read<T>(&self, f: impl FnOnce(&mut Replica) -> T) -> T {
        f(&mut lock(&self.inner.state).replica)
    }

    /// Runs an engine operation and dispatches what it emits.
    pub fn engine<T>(&self, f: impl FnOnce(&mut Engine, &mut Replica) -> osaurus_sync_core::Result<(T, Vec<Out>)>) -> osaurus_sync_core::Result<T> {
        self.inner.engine_op(f)
    }

    /// Our address, for out-of-band exchange (QR codes, tests).
    pub fn addr(&self) -> Option<EndpointAddr> {
        lock(&self.inner.endpoint).as_ref().map(|e| e.addr())
    }

    /// Our address as text: `<device hex>[;ip:port...][;relay=<url>...]`.
    pub fn addr_string(&self) -> Option<String> {
        self.addr().map(|a| encode_addr(&a))
    }

    pub fn add_peer_addr_string(&self, s: &str) -> Result<()> {
        self.add_peer_addr(decode_addr(s)?);
        Ok(())
    }

    /// Teaches the endpoint a peer's addresses without discovery.
    pub fn add_peer_addr(&self, addr: EndpointAddr) {
        self.inner.memory.add_endpoint_info(addr);
        self.inner.wake.notify_one();
    }

    /// Dial everything now instead of waiting for the next interval.
    pub fn sync_now(&self) {
        lock(&self.inner.backoff).clear();
        self.inner.wake.notify_one();
    }

    pub fn is_connected(&self, device: &DeviceKey) -> bool {
        lock(&self.inner.peers).contains_key(device)
    }

    pub fn start_pairing(&self, ttl_ms: u64) -> PairingInvite {
        let mut st = lock(&self.inner.state);
        let State { replica, engine } = &mut *st;
        engine.start_pairing(replica, ttl_ms)
    }

    pub fn cancel_pairing(&self) {
        lock(&self.inner.state).engine.cancel_pairing();
    }

    /// Joins the user that issued `invite`. Completion arrives as `Notice::Paired` or
    /// `Notice::PairingFailed`.
    pub async fn pair(&self, invite: &str) -> Result<()> {
        let invite = PairingInvite::decode(invite)?;
        let peer = invite.device;
        if peer == self.inner.me {
            bail!("cannot pair with this device");
        }
        let conn = self.inner.connect(&peer, ALPN_SYNC).await?;
        self.inner.clone().spawn_session(conn, true, Some(invite));
        Ok(())
    }

    /// Queues an invite redemption; it is sent once a session with the inviter exists.
    pub fn redeem_invite(&self, token: &str) -> Result<()> {
        let token = InviteToken::decode(token)?;
        let inviter = token.inviter;
        self.inner.engine_op(|e, r| Ok(((), e.redeem_invite(r, token)?)))?;
        if !self.is_connected(&inviter) {
            self.inner.clone().dial_soon(inviter);
        }
        Ok(())
    }

    pub fn set_backfill_paused(&self, paused: bool) -> osaurus_sync_core::Result<()> {
        self.inner.engine_op(|e, r| Ok(((), e.set_backfill_paused(r, paused)?)))
    }

    pub fn request_history(&self, scope: ScopeId, before_hlc: u64, days: u32) -> osaurus_sync_core::Result<()> {
        self.inner.engine_op(|e, r| Ok(((), e.request_history(r, scope, before_hlc, days)?)))
    }

    /// Sends a streaming update to every connected device that can read `scope`. Text is the
    /// full text so far; the final message is committed separately as a normal event.
    pub fn stream_update(&self, scope: ScopeId, message_id: EventId, as_agent: Option<AgentId>, seq: u32, text: &str, state: StreamState) {
        self.inner.clone().send_stream(scope, message_id, as_agent, seq, text, state);
    }

    /// Downloads an attachment from any connected device that can read the scope. Returns the
    /// decrypted file path.
    pub async fn fetch_blob(&self, scope: ScopeId, hash: [u8; 32]) -> Result<std::path::PathBuf> {
        self.inner.clone().fetch_blob(scope, hash).await
    }

    /// Publishes a signed agent binding in this device's discovery record.
    pub fn publish_agent(&self, binding: Option<AgentBinding>) -> Result<()> {
        if let Some(b) = &binding
            && (b.host_device != self.inner.me || !b.verify())
        {
            bail!("binding does not name this device");
        }
        *lock(&self.inner.agent_record) = binding;
        self.inner.apply_agent_record();
        Ok(())
    }

    /// Looks up the agent binding a host publishes, through any configured discovery.
    pub async fn resolve_agent(&self, host: DeviceKey) -> Result<Option<AgentBinding>> {
        let ep = self.inner.endpoint()?;
        let mut stream = ep.address_lookup()?.resolve(endpoint_id(&host)?);
        let deadline = tokio::time::sleep(DIAL_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Ok(None),
                item = stream.next() => match item {
                    None => return Ok(None),
                    Some(Ok(Ok(item))) => {
                        if let Some(b) = item.endpoint_info().user_data().and_then(agent_record::decode)
                            && b.host_device == host {
                                return Ok(Some(b));
                            }
                    }
                    Some(_) => {}
                },
            }
        }
    }

    pub fn sync_status(&self, scope: ScopeId) -> osaurus_sync_core::Result<SyncStatus> {
        self.inner.sync_status(scope)
    }

    pub fn diagnostics(&self) -> Diagnostics {
        self.inner.diagnostics()
    }

    /// Stops networking for app suspension: closes sessions, flushes and checkpoints storage
    /// and closes the endpoint. Local reads and writes keep working.
    pub async fn suspend(&self) -> Result<()> {
        self.inner.suspend().await
    }

    /// Rebinds the endpoint and reconnects; sessions catch up through reconciliation.
    pub async fn resume(&self) -> Result<()> {
        if self.inner.net.lock().await.is_some() {
            return Ok(());
        }
        self.inner.bring_up().await?;
        self.sync_now();
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        lock(&self.inner.endpoint).is_some()
    }

    /// Stops networking and releases the data directory.
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.suspend().await?;
        lock(&self.inner.state).replica.close()?;
        Ok(())
    }
}

impl Inner {
    fn endpoint(&self) -> Result<Endpoint> {
        lock(&self.endpoint).clone().ok_or_else(|| anyhow!("networking is suspended"))
    }

    async fn bring_up(self: &Arc<Self>) -> Result<()> {
        let mut net = self.net.lock().await;
        if net.is_some() {
            return Ok(());
        }
        let relay_mode = match &self.cfg.relays {
            Relays::Default => RelayMode::Default,
            Relays::Disabled => RelayMode::Disabled,
            Relays::Custom(urls) => {
                let urls: Vec<RelayUrl> = urls.iter().map(|u| u.parse()).collect::<Result<_, _>>().context("relay url")?;
                RelayMode::Custom(urls.into_iter().collect::<RelayMap>())
            }
        };
        let mut b = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&self.secret))
            .alpns(vec![ALPN_SYNC.to_vec(), ALPN_STREAM.to_vec(), ALPN_BLOBS.to_vec()])
            .relay_mode(relay_mode)
            .address_lookup(self.memory.clone());
        match &self.cfg.dns {
            Dns::Default => {
                b = b.address_lookup(PkarrPublisher::n0_dns()).address_lookup(PkarrResolver::n0_dns()).address_lookup(DnsAddressLookup::n0_dns());
            }
            Dns::Custom { pkarr_relay, origin_domain } => {
                let url = pkarr_relay.parse().context("pkarr relay url")?;
                b = b.address_lookup(PkarrPublisher::builder(url)).address_lookup(DnsAddressLookup::builder(origin_domain.clone()));
            }
            Dns::Disabled => {}
        }
        if self.cfg.bind_port != 0 {
            b = b.bind_addr(std::net::SocketAddr::from(([0, 0, 0, 0], self.cfg.bind_port)))?;
        }
        let endpoint = b.bind().await.context("bind endpoint")?;
        let mut tasks = Vec::new();
        if self.cfg.mdns {
            match MdnsAddressLookup::builder().service_name(MDNS_SERVICE).build(endpoint.id()) {
                Ok(mdns) => {
                    endpoint.address_lookup()?.add(mdns.clone());
                    let me = self.clone();
                    tasks.push(tokio::spawn(async move {
                        let mut events = mdns.subscribe().await;
                        while let Some(ev) = events.next().await {
                            if let DiscoveryEvent::Discovered { endpoint_info, .. } = ev {
                                let d = device_of(endpoint_info.endpoint_id);
                                lock(&me.backoff).remove(&d);
                                me.wake.notify_one();
                            }
                        }
                    }));
                }
                Err(e) => warn!("mDNS unavailable: {e}"),
            }
        }
        let router = Router::builder(endpoint.clone())
            .accept(ALPN_SYNC, SyncProtocol(self.clone()))
            .accept(ALPN_STREAM, streams::StreamProtocol(self.clone()))
            .accept(ALPN_BLOBS, blobs::BlobProtocol(self.clone()))
            .spawn();
        let me = self.clone();
        tasks.push(tokio::spawn(async move { me.dial_loop().await }));
        let me = self.clone();
        tasks.push(tokio::spawn(async move { me.maintain_loop().await }));
        *lock(&self.endpoint) = Some(endpoint.clone());
        *net = Some(Net { endpoint, router, tasks });
        drop(net);
        self.apply_agent_record();
        Ok(())
    }

    async fn suspend(self: &Arc<Self>) -> Result<()> {
        let mut net = self.net.lock().await;
        let Some(n) = net.take() else {
            lock(&self.state).replica.flush()?;
            return Ok(());
        };
        *lock(&self.endpoint) = None;
        for t in &n.tasks {
            t.abort();
        }
        let notices = {
            let mut st = lock(&self.state);
            let outs = st.engine.disconnect_all();
            self.route(outs)
        };
        self.deliver(notices, Vec::new());
        let peers: Vec<Peer> = lock(&self.peers).drain().map(|(_, p)| p).collect();
        for p in &peers {
            let _ = p.tx.send(Cmd::Close);
        }
        // Give writers a moment to flush their Bye.
        tokio::time::sleep(Duration::from_millis(50)).await;
        for p in peers {
            p.conn.close(0u32.into(), b"suspend");
        }
        self.streams.interrupt_all(&*self.listener);
        lock(&self.state).replica.flush()?;
        let _ = n.router.shutdown().await;
        n.endpoint.close().await;
        Ok(())
    }

    fn apply_agent_record(&self) {
        let rec = lock(&self.agent_record).as_ref().and_then(agent_record::encode);
        if let Some(ep) = lock(&self.endpoint).as_ref() {
            ep.set_user_data_for_address_lookup(rec);
        }
    }

    fn write<T>(&self, f: impl FnOnce(&mut Replica) -> osaurus_sync_core::Result<T>) -> osaurus_sync_core::Result<T> {
        let (out, notices, changes) = {
            let mut st = lock(&self.state);
            let State { replica, engine } = &mut *st;
            let out = f(replica);
            let outs = engine.poll(replica).unwrap_or_else(|e| {
                warn!("engine poll failed: {e}");
                Vec::new()
            });
            (out, self.route(outs), replica.take_changes())
        };
        self.deliver(notices, changes);
        out
    }

    fn engine_op<T>(&self, f: impl FnOnce(&mut Engine, &mut Replica) -> osaurus_sync_core::Result<(T, Vec<Out>)>) -> osaurus_sync_core::Result<T> {
        let (res, notices, changes) = {
            let mut st = lock(&self.state);
            let State { replica, engine } = &mut *st;
            match f(engine, replica) {
                Ok((t, outs)) => (Ok(t), self.route(outs), replica.take_changes()),
                Err(e) => (Err(e), Vec::new(), replica.take_changes()),
            }
        };
        self.deliver(notices, changes);
        res
    }

    /// Queues frames to peer writers. Called with the state lock held so frames to one peer
    /// leave in the order the engine produced them. Lock order is always state, then peers.
    fn route(&self, outs: Vec<Out>) -> Vec<Notice> {
        let peers = lock(&self.peers);
        let mut notices = Vec::new();
        for out in outs {
            match out {
                Out::Send(peer, msg) => {
                    if let Some(p) = peers.get(&peer) {
                        let _ = p.tx.send(Cmd::Frame(msg.encode()));
                    }
                }
                Out::Close(peer, why) => {
                    debug!(?peer, why, "closing session");
                    if let Some(p) = peers.get(&peer) {
                        let _ = p.tx.send(Cmd::Close);
                    }
                }
                Out::Notify(n) => notices.push(n),
            }
        }
        notices
    }

    /// Hands results to the listener. Called without locks so it may call back in.
    fn deliver(&self, notices: Vec<Notice>, changes: Vec<Change>) {
        if !changes.is_empty() {
            self.listener.changes(changes);
        }
        for n in notices {
            self.listener.notice(n);
        }
    }

    pub(crate) async fn connect(&self, peer: &DeviceKey, alpn: &[u8]) -> Result<Connection> {
        let ep = self.endpoint()?;
        let id = endpoint_id(peer)?;
        tokio::time::timeout(DIAL_TIMEOUT, ep.connect(id, alpn)).await.map_err(|_| anyhow!("dial timed out"))?.map_err(|e| anyhow!("dial failed: {e}"))
    }

    /// Registers a sync connection, resolving simultaneous dials: when both sides dial, the
    /// connection dialed by the lower device key wins.
    fn register(&self, peer: DeviceKey, conn: &Connection, dialed: bool, tx: mpsc::UnboundedSender<Cmd>) -> Option<u64> {
        // Hold state across the swap so no engine output is routed to the wrong connection.
        let mut st = lock(&self.state);
        let mut peers = lock(&self.peers);
        if let Some(existing) = peers.get(&peer) {
            let preferred = self.me < peer;
            let existing_wins = existing.conn.close_reason().is_none() && (existing.dialed == preferred || dialed != preferred);
            if existing_wins {
                return None;
            }
            let old = peers.remove(&peer).expect("present");
            old.conn.close(0u32.into(), b"duplicate");
            st.engine.disconnect(&peer);
        }
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        peers.insert(peer, Peer { id, conn: conn.clone(), dialed, tx });
        Some(id)
    }

    fn unregister(&self, peer: &DeviceKey, id: u64) {
        let mut st = lock(&self.state);
        let mut peers = lock(&self.peers);
        if peers.get(peer).is_some_and(|p| p.id == id) {
            peers.remove(peer);
            drop(peers);
            st.engine.disconnect(peer);
            drop(st);
            self.streams.interrupt_peer(peer, &*self.listener);
            self.listener.changes(vec![Change::DevicesChanged]);
        }
    }

    fn spawn_session(self: Arc<Self>, conn: Connection, dialed: bool, pairing: Option<PairingInvite>) {
        let rt = self.rt.clone();
        rt.spawn(async move {
            let peer = device_of(conn.remote_id());
            if let Err(e) = self.clone().run_session(conn, dialed, pairing).await {
                debug!(?peer, "session ended: {e:#}");
            }
        });
    }

    async fn run_session(self: Arc<Self>, conn: Connection, dialed: bool, pairing: Option<PairingInvite>) -> Result<()> {
        let peer = device_of(conn.remote_id());
        let (mut send, mut recv) = if dialed { conn.open_bi().await? } else { conn.accept_bi().await? };
        let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();
        let Some(id) = self.register(peer, &conn, dialed, tx) else {
            conn.close(0u32.into(), b"duplicate");
            return Ok(());
        };
        lock(&self.backoff).remove(&peer);
        let wconn = conn.clone();
        let writer = tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    Cmd::Frame(b) => {
                        if frame::write(&mut send, &b).await.is_err() {
                            break;
                        }
                    }
                    Cmd::Close => {
                        let _ = send.finish();
                        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
                        wconn.close(0u32.into(), b"bye");
                        break;
                    }
                }
            }
        });
        if dialed {
            let me = self.clone();
            let (notices, changes) = tokio::task::spawn_blocking(move || -> osaurus_sync_core::Result<_> {
                let mut st = lock(&me.state);
                let State { replica, engine } = &mut *st;
                let outs = match &pairing {
                    Some(inv) => engine.connect_pairing(replica, inv)?,
                    None => engine.connect(replica, peer)?,
                };
                Ok((me.route(outs), replica.take_changes()))
            })
            .await??;
            self.deliver(notices, changes);
        }
        self.listener.changes(vec![Change::DevicesChanged]);
        let res = self.clone().read_loop(peer, &mut recv).await;
        self.unregister(&peer, id);
        writer.abort();
        res
    }

    async fn read_loop(self: Arc<Self>, peer: DeviceKey, recv: &mut iroh::endpoint::RecvStream) -> Result<()> {
        while let Some(bytes) = frame::read(recv).await? {
            let msg = Msg::decode(&bytes)?;
            let me = self.clone();
            let res = tokio::task::spawn_blocking(move || {
                let mut st = lock(&me.state);
                let State { replica, engine } = &mut *st;
                let res = engine.handle(replica, peer, msg).map(|outs| me.route(outs));
                (res, replica.take_changes())
            })
            .await?;
            match res {
                (Ok(notices), changes) => self.deliver(notices, changes),
                (Err(e), changes) => {
                    self.deliver(Vec::new(), changes);
                    warn!(?peer, "protocol error: {e}");
                    bail!("protocol error: {e}");
                }
            }
        }
        Ok(())
    }

    fn dial_soon(self: Arc<Self>, peer: DeviceKey) {
        if peer == self.me || !lock(&self.dialing).insert(peer) {
            return;
        }
        let rt = self.rt.clone();
        rt.spawn(async move {
            let res = self.connect(&peer, ALPN_SYNC).await;
            lock(&self.dialing).remove(&peer);
            match res {
                Ok(conn) => {
                    lock(&self.backoff).remove(&peer);
                    self.clone().spawn_session(conn, true, None);
                }
                Err(e) => {
                    debug!(?peer, "dial failed: {e:#}");
                    let mut bo = lock(&self.backoff);
                    let next = bo.get(&peer).map_or(BACKOFF_MIN, |(_, d)| (*d * 2).min(BACKOFF_MAX));
                    bo.insert(peer, (Instant::now() + next, next));
                }
            }
        });
    }

    /// Prunes local tables at start and then hourly.
    async fn maintain_loop(self: Arc<Self>) {
        loop {
            let res = { lock(&self.state).replica.maintain() };
            match res {
                Ok(m) if m != Default::default() => debug!(?m, "maintenance"),
                Ok(_) => {}
                Err(e) => warn!("maintenance failed: {e}"),
            }
            tokio::time::sleep(MAINTAIN_EVERY).await;
        }
    }

    async fn dial_loop(self: Arc<Self>) {
        loop {
            let targets = {
                let mut st = lock(&self.state);
                let mut t = st.replica.known_peer_devices().unwrap_or_default();
                t.extend(st.engine.sessions().into_iter().map(|s| s.device));
                t
            };
            let now = Instant::now();
            for d in targets {
                if d == self.me || lock(&self.peers).contains_key(&d) {
                    continue;
                }
                if lock(&self.backoff).get(&d).is_some_and(|(at, _)| *at > now) {
                    continue;
                }
                self.clone().dial_soon(d);
            }
            tokio::select! {
                _ = tokio::time::sleep(self.cfg.dial_interval) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    fn sync_status(&self, scope: ScopeId) -> osaurus_sync_core::Result<SyncStatus> {
        let relayed = self.relayed_peers();
        let peers: HashSet<DeviceKey> = lock(&self.peers).keys().copied().collect();
        let mut st = lock(&self.state);
        let r = &mut st.replica;
        let contacts = r.peer_contacts(scope)?;
        let mut out = Vec::new();
        for (device, last) in contacts {
            let name = r.device_name(&device)?.unwrap_or_default();
            out.push(PeerStatus { device, name, connected: peers.contains(&device), relayed: relayed.contains(&device), last_contact_ms: Some(last) });
        }
        Ok(SyncStatus {
            scope,
            pending_outbox: r.pending_outbox(scope)?,
            last_sync_ms: r.last_sync_ms(scope)?,
            synced_from_hlc: r.synced_from_hlc(scope)?,
            backfill_complete: r.backfill_complete(scope)?,
            peers: out,
        })
    }

    fn relayed_peers(&self) -> HashSet<DeviceKey> {
        lock(&self.peers).iter().filter(|(_, p)| p.conn.paths().iter().any(|path| path.is_selected() && path.is_relay())).map(|(d, _)| *d).collect()
    }

    fn diagnostics(&self) -> Diagnostics {
        let ep = lock(&self.endpoint).clone();
        let (home_relays, local_addrs) = match &ep {
            Some(ep) => (
                ep.home_relay_status().get().iter().map(|s| (s.url().to_string(), s.is_connected())).collect(),
                ep.bound_sockets().iter().map(|s| s.to_string()).collect(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let conns: HashMap<DeviceKey, (bool, Option<u64>)> = lock(&self.peers)
            .iter()
            .map(|(d, p)| {
                let paths = p.conn.paths();
                let sel = paths.iter().find(|x| x.is_selected());
                (*d, (sel.as_ref().is_some_and(|x| x.is_relay()), sel.map(|x| x.rtt().as_millis() as u64)))
            })
            .collect();
        let st = lock(&self.state);
        let sessions = st.engine.sessions();
        let backfill_paused = st.engine.backfill_paused();
        let mut peers = Vec::new();
        for s in sessions {
            let (relayed, rtt_ms) = conns.get(&s.device).copied().unwrap_or((false, None));
            peers.push(PeerDiag {
                device: s.device,
                name: st.replica.device_name(&s.device).ok().flatten().unwrap_or_default(),
                connected: conns.contains_key(&s.device),
                established: s.established,
                relayed,
                rtt_ms,
                syncing: s.syncing,
                storage_role: s.storage_role,
            });
        }
        let scopes_ids = st.replica.scope_ids().unwrap_or_default();
        drop(st);
        let scopes = scopes_ids.into_iter().filter_map(|s| self.sync_status(s).ok()).collect();
        Diagnostics { device: self.me, running: ep.is_some(), home_relays, local_addrs, backfill_paused, peers, scopes }
    }
}

#[derive(Debug, Clone)]
struct SyncProtocol(Arc<Inner>);

impl ProtocolHandler for SyncProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = device_of(conn.remote_id());
        if let Err(e) = self.0.clone().run_session(conn, false, None).await {
            debug!(?peer, "inbound session ended: {e:#}");
        }
        Ok(())
    }
}
