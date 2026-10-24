//! Deterministic simulation of replicas and sync engines over a virtual network.
//!
//! Every node gets a seeded environment, and the network delivers messages over per-link
//! ordered queues (like QUIC streams) in an order chosen by the seeded RNG. Links can drop and
//! nodes can restart at any point. The same seed always produces the same run.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use osaurus_sync_core::env::TestEnv;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::{DeviceKey, EventHash, Principal, ScopeId};
use osaurus_sync_core::replica::{InviteToken, MemorySecretStore};
use osaurus_sync_core::store::status;
use osaurus_sync_core::sync::{Engine, Msg, Notice, Out, PairingInvite};
use osaurus_sync_core::types::HistoryMode;
use osaurus_sync_core::{Replica, ReplicaConfig, SecretStore};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};

pub const START_MS: u64 = 1_750_000_000_000;

const APPLY_SLICE: usize = 3;

pub struct Node {
    pub name: String,
    pub platform: Platform,
    pub history: Option<HistoryMode>,
    /// Keeps every scope in full and never compacts, like Osaurus Server.
    pub storage_role: bool,
    pub replica: Option<Replica>,
    pub engine: Engine,
    pub secrets: Arc<MemorySecretStore>,
    pub env: Arc<TestEnv>,
    /// Virtual clock offset, to exercise skew.
    pub skew_ms: i64,
    pub dir: tempfile::TempDir,
    pub notices: Vec<Notice>,
    /// Every hash this node has stored, per scope, kept across restarts.
    pub ever_stored: BTreeMap<ScopeId, BTreeSet<EventHash>>,
}

impl Node {
    pub fn r(&mut self) -> &mut Replica {
        self.replica.as_mut().expect("node is running")
    }

    pub fn device(&self) -> DeviceKey {
        self.replica.as_ref().expect("node is running").device_key()
    }

    pub fn config(&self) -> ReplicaConfig {
        let mut c = ReplicaConfig::new(self.dir.path(), self.name.clone(), self.platform);
        c.history_mode = self.history;
        c.snapshot_every = 8;
        c.storage_role = self.storage_role;
        c.compact_min_updates = 4;
        c
    }
}

pub struct Sim {
    pub nodes: Vec<Node>,
    queues: BTreeMap<(usize, usize), VecDeque<Vec<u8>>>,
    links: BTreeSet<(usize, usize)>,
    /// Directed links to close once their queue drains (the transport flushes before closing).
    closing: BTreeSet<(usize, usize)>,
    pub rng: ChaCha8Rng,
    pub clock: u64,
    pub delivered: u64,
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), to).unwrap();
        }
    }
}

fn pair(a: usize, b: usize) -> (usize, usize) {
    if a < b { (a, b) } else { (b, a) }
}

impl Sim {
    pub fn new(seed: u64) -> Self {
        Sim {
            nodes: vec![],
            queues: BTreeMap::new(),
            links: BTreeSet::new(),
            closing: BTreeSet::new(),
            rng: ChaCha8Rng::seed_from_u64(seed),
            clock: START_MS,
            delivered: 0,
        }
    }

    pub fn add_node(&mut self, name: &str, platform: Platform) -> usize {
        self.add_node_with(name, platform, None, 0)
    }

    pub fn add_node_with(&mut self, name: &str, platform: Platform, history: Option<HistoryMode>, skew_ms: i64) -> usize {
        let env = Arc::new(TestEnv::new(self.rng.next_u64(), (self.clock as i64 + skew_ms) as u64));
        let secrets = Arc::new(MemorySecretStore::default());
        let dir = tempfile::tempdir().unwrap();
        let mut node = Node {
            name: name.into(),
            platform,
            history,
            storage_role: false,
            replica: None,
            engine: Engine::new(),
            secrets,
            env,
            skew_ms,
            dir,
            notices: vec![],
            ever_stored: BTreeMap::new(),
        };
        let r = Replica::open(node.config(), node.env.clone(), node.secrets.clone()).unwrap();
        node.replica = Some(r);
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// A new device holding node `i`'s user secret under a device key of its own, as a thief
    /// who copied the secret off a stolen device would run.
    pub fn add_node_with_user_of(&mut self, name: &str, platform: Platform, i: usize) -> usize {
        let user = self.nodes[i].secrets.get(osaurus_sync_core::replica::USER_SECRET).expect("node has a user secret");
        let j = self.add_node(name, platform);
        let n = &mut self.nodes[j];
        n.replica.take().unwrap().close().unwrap();
        n.secrets.store(osaurus_sync_core::replica::USER_SECRET, &user).unwrap();
        n.replica = Some(Replica::open(n.config(), n.env.clone(), n.secrets.clone()).unwrap());
        j
    }

    /// Rotates node `i`'s identity, revoking `revoke`, and sends what the engine produces.
    pub fn rotate_identity(&mut self, i: usize, revoke: &[DeviceKey]) -> osaurus_sync_core::types::IdentityRotation {
        let n = &mut self.nodes[i];
        let (report, outs) = n.engine.rotate_identity(n.replica.as_mut().unwrap(), revoke).unwrap();
        self.process(i, outs);
        report
    }

    /// Adds a node with the storage role.
    pub fn add_storage_node(&mut self, name: &str) -> usize {
        let i = self.add_node(name, Platform::Server);
        let n = &mut self.nodes[i];
        n.storage_role = true;
        n.replica = None;
        n.replica = Some(Replica::open(n.config(), n.env.clone(), n.secrets.clone()).unwrap());
        i
    }

    pub fn r(&mut self, i: usize) -> &mut Replica {
        self.nodes[i].r()
    }

    pub fn index_of(&self, d: &DeviceKey) -> Option<usize> {
        self.nodes.iter().position(|n| n.replica.as_ref().is_some_and(|r| r.device_key() == *d))
    }

    pub fn advance(&mut self, ms: u64) {
        self.clock += ms;
        for n in &self.nodes {
            n.env.set_now((self.clock as i64 + n.skew_ms) as u64);
        }
    }

    pub fn is_linked(&self, a: usize, b: usize) -> bool {
        self.links.contains(&pair(a, b))
    }

    pub fn connect(&mut self, a: usize, b: usize) {
        if a == b || self.is_linked(a, b) || self.nodes[a].replica.is_none() || self.nodes[b].replica.is_none() {
            return;
        }
        self.links.insert(pair(a, b));
        let peer = self.nodes[b].device();
        let n = &mut self.nodes[a];
        let outs = n.engine.connect(n.replica.as_mut().unwrap(), peer).unwrap();
        self.process(a, outs);
    }

    /// Connects a new device to an existing one with a pairing code.
    pub fn pair(&mut self, host: usize, joiner: usize) {
        let invite = {
            let n = &mut self.nodes[host];
            n.engine.start_pairing(n.replica.as_ref().unwrap(), 300_000)
        };
        let invite = PairingInvite::decode(&invite.encode()).unwrap();
        self.links.insert(pair(host, joiner));
        let n = &mut self.nodes[joiner];
        let outs = n.engine.connect_pairing(n.replica.as_mut().unwrap(), &invite).unwrap();
        self.process(joiner, outs);
    }

    /// Invites `joiner`'s user to `scope` and lets the joiner redeem the invite.
    pub fn invite(&mut self, host: usize, joiner: usize, scope: ScopeId, role: osaurus_sync_core::ids::Role) {
        let token = self.r(host).create_invite(scope, role, 3_600_000).unwrap();
        self.poll(host);
        let token = InviteToken::decode(&token.encode()).unwrap();
        let n = &mut self.nodes[joiner];
        let outs = n.engine.redeem_invite(n.replica.as_mut().unwrap(), token).unwrap();
        self.process(joiner, outs);
        if !self.is_linked(host, joiner) {
            self.connect(joiner, host);
        } else {
            // Already connected: the redeem message was sent directly.
        }
    }

    /// Opens a link without starting a session.
    pub fn link(&mut self, a: usize, b: usize) {
        self.links.insert(pair(a, b));
    }

    pub fn disconnect(&mut self, a: usize, b: usize) {
        if !self.links.remove(&pair(a, b)) {
            return;
        }
        self.queues.remove(&(a, b));
        self.queues.remove(&(b, a));
        self.closing.remove(&(a, b));
        self.closing.remove(&(b, a));
        for (x, y) in [(a, b), (b, a)] {
            if self.nodes[y].replica.is_some() {
                let peer = self.nodes[y].device();
                self.nodes[x].engine.disconnect(&peer);
            }
        }
    }

    pub fn disconnect_all(&mut self, i: usize) {
        let peers: Vec<usize> = self
            .links
            .iter()
            .filter_map(|&(a, b)| {
                if a == i {
                    Some(b)
                } else if b == i {
                    Some(a)
                } else {
                    None
                }
            })
            .collect();
        for p in peers {
            self.disconnect(i, p);
        }
    }

    /// Stops and reopens a node from disk, losing all in-memory state.
    pub fn restart(&mut self, i: usize) {
        self.disconnect_all(i);
        let n = &mut self.nodes[i];
        n.replica = None;
        n.engine = Engine::new();
        n.replica = Some(Replica::open(n.config(), n.env.clone(), n.secrets.clone()).unwrap());
    }

    /// Retires node `i` and starts a new device from a copy of its data directory, as when a
    /// backup is restored onto new hardware. The new node has its own, fresh secrets.
    pub fn restore_onto_new_device(&mut self, i: usize, name: &str) -> usize {
        self.disconnect_all(i);
        if let Some(mut r) = self.nodes[i].replica.take() {
            r.close().unwrap();
        }
        let platform = self.nodes[i].platform;
        let j = self.add_node(name, platform);
        let src = self.nodes[i].dir.path().to_path_buf();
        let n = &mut self.nodes[j];
        n.replica.take().unwrap().close().unwrap();
        std::fs::remove_dir_all(n.dir.path()).unwrap();
        copy_dir(&src, n.dir.path());
        n.replica = Some(Replica::open(n.config(), n.env.clone(), n.secrets.clone()).unwrap());
        j
    }

    /// Lets the engine push local writes. Call after every operation on a replica.
    pub fn poll(&mut self, i: usize) {
        let n = &mut self.nodes[i];
        let outs = n.engine.poll(n.replica.as_mut().unwrap()).unwrap();
        self.process(i, outs);
    }

    pub fn process(&mut self, from: usize, outs: Vec<Out>) {
        for o in outs {
            match o {
                Out::Send(peer, msg) => {
                    let Some(to) = self.index_of(&peer) else { continue };
                    if self.is_linked(from, to) {
                        self.queues.entry((from, to)).or_default().push_back(msg.encode());
                    }
                }
                Out::Close(peer, _) => {
                    if let Some(to) = self.index_of(&peer) {
                        if self.queues.get(&(from, to)).is_some_and(|q| !q.is_empty()) {
                            self.closing.insert((from, to));
                        } else {
                            self.disconnect(from, to);
                        }
                    }
                }
                Out::Notify(n) => self.nodes[from].notices.push(n),
            }
        }
    }

    /// Delivers one message on a random link. Returns false when nothing is in flight.
    pub fn step(&mut self) -> bool {
        let ready: Vec<(usize, usize)> = self.queues.iter().filter(|(_, q)| !q.is_empty()).map(|(k, _)| *k).collect();
        if ready.is_empty() {
            return false;
        }
        let (from, to) = ready[(self.rng.next_u32() as usize) % ready.len()];
        let bytes = self.queues.get_mut(&(from, to)).unwrap().pop_front().unwrap();
        let msg = Msg::decode(&bytes).expect("frames decode");
        let close_after = self.closing.contains(&(from, to)) && self.queues[&(from, to)].is_empty();
        let peer = self.nodes[from].device();
        // Like the network node, apply large chunks in slices (small here, to exercise it).
        for msg in msg.into_slices(APPLY_SLICE) {
            if !self.is_linked(from, to) {
                break;
            }
            let n = &mut self.nodes[to];
            let outs = n.engine.handle(n.replica.as_mut().unwrap(), peer, msg).unwrap();
            self.process(to, outs);
        }
        self.delivered += 1;
        if close_after {
            self.disconnect(from, to);
        }
        true
    }

    /// Runs until no message is in flight. Panics if that takes more than `max` deliveries.
    pub fn run(&mut self, max: usize) -> usize {
        let mut n = 0;
        while self.step() {
            n += 1;
            assert!(n < max, "network did not quiesce after {max} deliveries");
        }
        self.record_stored();
        n
    }

    /// Delivers up to `n` messages.
    pub fn run_some(&mut self, n: usize) {
        for _ in 0..n {
            if !self.step() {
                break;
            }
        }
        self.record_stored();
    }

    pub fn record_stored(&mut self) {
        for node in &mut self.nodes {
            if let Some(r) = node.replica.as_ref() {
                for s in r.scope_ids().unwrap() {
                    node.ever_stored.entry(s).or_default().extend(r.all_hashes(s).unwrap());
                }
            }
        }
    }

    pub fn running(&self) -> Vec<usize> {
        (0..self.nodes.len()).filter(|i| self.nodes[*i].replica.is_some()).collect()
    }

    /// Connects every pair of running nodes from scratch.
    pub fn heal(&mut self, exclude: &[usize]) {
        let nodes: Vec<usize> = self.running().into_iter().filter(|i| !exclude.contains(i)).collect();
        for &a in &nodes {
            self.disconnect_all(a);
        }
        for (k, &a) in nodes.iter().enumerate() {
            for &b in &nodes[k + 1..] {
                self.connect(a, b);
            }
        }
    }

    pub fn rand(&mut self, n: usize) -> usize {
        (self.rng.next_u32() as usize) % n.max(1)
    }

    pub fn chance(&mut self, percent: u32) -> bool {
        self.rng.next_u32() % 100 < percent
    }

    /// A fresh replica with the same device and user keys as node `i`, in an empty directory.
    pub fn clone_identity(&self, i: usize) -> (Replica, tempfile::TempDir) {
        let n = &self.nodes[i];
        let secrets = Arc::new(MemorySecretStore::default());
        for k in ["device-key", "user-secret"] {
            if let Some(v) = n.secrets.get(k) {
                secrets.store(k, &v).unwrap();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = ReplicaConfig::new(dir.path(), n.name.clone(), n.platform);
        cfg.history_mode = Some(HistoryMode::Full);
        let env = Arc::new(TestEnv::new(7, self.clock));
        (Replica::open(cfg, env, secrets).unwrap(), dir)
    }
}

// ---- views for comparison ----

#[derive(Debug, Clone, PartialEq)]
pub struct MessageSnap {
    pub id: String,
    pub author: String,
    pub content: String,
    pub edited: bool,
    pub reactions: Vec<(String, u32)>,
    pub blobs: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScopeSnap {
    pub title: String,
    pub pinned: bool,
    pub archived: bool,
    pub sort_order: Option<String>,
    pub members: Vec<(String, u8)>,
    pub messages: Vec<MessageSnap>,
}

/// Device-independent view of a scope: no delivery status, "mine" flags or unread counts.
pub fn snapshot(r: &Replica, scope: ScopeId) -> Option<ScopeSnap> {
    let c = r.conversation(scope).unwrap()?;
    let page = r.messages(scope, None, 1_000_000).unwrap();
    let mut members: Vec<(String, u8)> = c.members.iter().map(|m| (m.principal.to_hex(), m.role as u8)).collect();
    members.sort();
    Some(ScopeSnap {
        title: c.title,
        pinned: c.pinned,
        archived: c.archived,
        sort_order: c.sort_order.map(|v| format!("{v}")),
        members,
        messages: page
            .messages
            .into_iter()
            .filter(|m| !matches!(m.status, osaurus_sync_core::types::DeliveryStatus::Rejected(_)))
            .map(|m| MessageSnap {
                id: m.id.to_hex(),
                author: m.author.to_hex(),
                content: m.content,
                edited: m.edited,
                reactions: m.reactions.iter().map(|x| (x.reaction.clone(), x.count)).collect(),
                blobs: m.blobs.len(),
            })
            .collect(),
    })
}

/// `(hash, valid)` for every stored event in a scope.
pub fn verdicts(r: &mut Replica, scope: ScopeId) -> Vec<(EventHash, bool)> {
    let hashes = r.all_hashes(scope).unwrap();
    hashes.into_iter().map(|h| (h, r.verdict_of(&h).unwrap().is_some_and(|v| v.is_valid()))).collect()
}

/// Whether a device currently acts for someone in the scope (per this replica's view).
pub fn is_member_device(r: &mut Replica, scope: ScopeId, device: &DeviceKey) -> bool {
    r.is_authorized(scope, device).unwrap()
}

pub fn principal_of(r: &Replica) -> Principal {
    r.me()
}

/// Status codes re-exported for tests.
pub const VALID: i64 = status::VALID;
