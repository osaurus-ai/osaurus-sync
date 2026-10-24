//! Sync sessions as a state machine. The transport feeds it messages and carries out its
//! outputs; nothing here touches the network or the clock directly.
//!
//! Each side pulls: after `Hello` both peers send an `Offer` of the scopes the other may read,
//! then each runs its own reconciliation schedule as initiator and requests what it lacks. The
//! schedule reconciles the control layer first, then content in windows from newest to oldest so
//! recent conversations become usable first. New events, local or received, are pushed to every
//! other connected peer that may read the scope.

use std::collections::{BTreeMap, HashMap};

use super::proto::{Layer, Msg, PROTOCOL_VERSION, ScopeOffer};
use super::recon::{self, Key, KeySource, MAX_KEY, MIN_KEY, Range};
use crate::crypto;
use crate::env::random_array;
use crate::envelope::{Decoded, Envelope, kind};
use crate::error::{Error, Result};
use crate::hlc;
use crate::identity::DeviceCert;
use crate::ids::{DeviceKey, EventHash, EventId, ScopeId, UserKey};
use crate::keys::UserSecret;
use crate::replica::{IngestOutcome, InviteToken, Replica};
use crate::types::{HistoryMode, IdentityRotation};

const DAY_MS: u64 = 86_400_000;
/// Content windows, newest first, as days before now.
const WINDOWS_DAYS: [u64; 5] = [1, 7, 30, 90, 365];
/// Windows reaching further back than this are backfill and can be paused.
const BACKFILL_AFTER_DAYS: u64 = 7;
const MAX_IN_FLIGHT: usize = 8;
const WANT_CHUNK: usize = 512;
const EVENTS_CHUNK_BYTES: usize = 1024 * 1024;
const PAIRING_ATTEMPTS: u32 = 5;
/// Per-session receive limits, per window. Catch-up replies are limited loosely; unsolicited
/// pushes (live traffic) much more tightly.
pub const BUDGET_WINDOW_MS: u64 = 60_000;
pub const MAX_EVENTS_PER_WINDOW: usize = 500_000;
pub const MAX_BYTES_PER_WINDOW: usize = 2 << 30;
pub const MAX_PUSHED_PER_WINDOW: usize = 20_000;
pub const MAX_PUSHED_BYTES_PER_WINDOW: usize = 256 << 20;
/// Invalid events (bad signature, malformed, over the held limit) tolerated per session.
pub const MAX_INVALID_PER_SESSION: usize = 1_000;
/// Messages an untrusted session may send while redeeming an invite.
pub const MAX_UNTRUSTED_MSGS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    Send(DeviceKey, Msg),
    Close(DeviceKey, String),
    Notify(Notice),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Pairing finished. On the new device the user identity has been adopted.
    Paired {
        device: DeviceKey,
    },
    PairingFailed(String),
    InviteResult {
        scope: ScopeId,
        error: Option<String>,
    },
    /// A peer was refused because its device is revoked.
    PeerRevoked(DeviceKey),
    /// A peer was refused because it is neither this user's device nor a member's device.
    PeerRejected(DeviceKey),
    /// A peer runs an incompatible protocol version.
    Incompatible {
        device: DeviceKey,
        version: u32,
    },
}

/// Shown as a QR code or typed on the new device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingInvite {
    pub device: DeviceKey,
    pub code: String,
}

impl PairingInvite {
    pub fn encode(&self) -> String {
        format!("osaurus-pair:1:{}:{}", self.device.to_hex(), self.code)
    }

    pub fn decode(s: &str) -> Result<Self> {
        let rest = s.trim().strip_prefix("osaurus-pair:1:").ok_or_else(|| Error::InvalidInput("not a pairing code".into()))?;
        let (dev, code) = rest.split_once(':').ok_or_else(|| Error::InvalidInput("not a pairing code".into()))?;
        Ok(PairingInvite { device: DeviceKey::from_hex(dev).ok_or_else(|| Error::InvalidInput("bad device id".into()))?, code: normalize_code(code) })
    }
}

const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn generate_code(r: &Replica) -> String {
    let bytes: [u8; 8] = random_array(r.env().as_ref());
    let v = u64::from_le_bytes(bytes);
    let chars: Vec<u8> = (0..12).map(|i| CODE_ALPHABET[((v >> (i * 5)) & 31) as usize]).collect();
    format!("{}-{}-{}", std::str::from_utf8(&chars[..4]).unwrap(), std::str::from_utf8(&chars[4..8]).unwrap(), std::str::from_utf8(&chars[8..]).unwrap())
}

/// Uppercases, drops separators and maps look-alike characters.
pub fn normalize_code(code: &str) -> String {
    let c: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            'U' => 'V',
            c => c,
        })
        .collect();
    if c.len() == 12 { format!("{}-{}-{}", &c[..4], &c[4..8], &c[8..]) } else { c }
}

fn pairing_proof(code: &str, joiner: &DeviceKey, host: &DeviceKey) -> [u8; 32] {
    let key = blake3::derive_key("osaurus-sync/pairing/v1", normalize_code(code).as_bytes());
    let mut msg = Vec::with_capacity(64);
    msg.extend_from_slice(&joiner.0);
    msg.extend_from_slice(&host.0);
    *blake3::keyed_hash(&key, &msg).as_bytes()
}

struct PairingState {
    code: String,
    expires_ms: u64,
    attempts: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobKind {
    /// Index into the scope's plan.
    Phase(usize),
    /// An explicitly requested older range.
    OnDemand,
}

#[derive(Clone, Debug)]
struct Job {
    scope: ScopeId,
    layer: Layer,
    lo: Key,
    hi: Key,
    kind: JobKind,
}

impl Job {
    fn priority(&self) -> usize {
        match self.kind {
            JobKind::OnDemand => 0,
            JobKind::Phase(i) => i + 1,
        }
    }
}

struct Running {
    job: Job,
    recon_done: bool,
    outstanding: u32,
}

#[derive(Clone, Copy, Debug)]
struct Window {
    layer: Layer,
    lo: Key,
    hi: Key,
    backfill: bool,
}

struct Session {
    cert: Option<DeviceCert>,
    storage_role: bool,
    established: bool,
    /// The peer may sync: one of this user's devices or a member's device. Untrusted sessions
    /// exist only to pair or redeem an invite; nothing they send is stored.
    trusted: bool,
    /// The peer was one of this user's devices before this session's `Hello`, so it may hand
    /// over a rotated identity.
    own_device: bool,
    hello_sent: bool,
    /// Pairing code while this device waits for a grant.
    joining: Option<String>,
    queued_invites: Vec<InviteToken>,
    /// Scopes offered in this session, and whether the peer could read their content then. A
    /// scope is offered again once a control-only peer is admitted, so it fetches the content.
    offered: HashMap<ScopeId, bool>,
    /// Heads and local seq of each scope as offered to the peer, to match its `Seen`.
    offered_at: HashMap<ScopeId, (Vec<EventHash>, Vec<EventHash>, i64)>,
    /// The peer's offered heads, echoed in `Seen` once the plan built on them finishes.
    planned_from: HashMap<ScopeId, (Vec<EventHash>, Vec<EventHash>)>,
    plans: HashMap<ScopeId, Vec<Window>>,
    queue: Vec<Job>,
    parked: Vec<Job>,
    running: BTreeMap<u32, Running>,
    /// Want tag -> recon tag.
    wants: HashMap<u32, u32>,
    budget: Budget,
}

/// What a peer has sent in the current window. A peer over any limit is disconnected.
#[derive(Default)]
struct Budget {
    window_start_ms: u64,
    events: usize,
    bytes: usize,
    pushed: usize,
    pushed_bytes: usize,
    invalid: usize,
    untrusted_msgs: usize,
}

impl Budget {
    /// Charges a batch of received events. Returns why the peer is over budget, if it is.
    fn charge(&mut self, now_ms: u64, events: &[Vec<u8>], pushed: bool) -> Option<&'static str> {
        if now_ms.saturating_sub(self.window_start_ms) >= BUDGET_WINDOW_MS {
            *self = Budget { window_start_ms: now_ms, invalid: self.invalid, untrusted_msgs: self.untrusted_msgs, ..Default::default() };
        }
        let bytes: usize = events.iter().map(|e| e.len()).sum();
        self.events += events.len();
        self.bytes += bytes;
        if pushed {
            self.pushed += events.len();
            self.pushed_bytes += bytes;
        }
        if self.events > MAX_EVENTS_PER_WINDOW || self.bytes > MAX_BYTES_PER_WINDOW {
            Some("event budget exceeded")
        } else if self.pushed > MAX_PUSHED_PER_WINDOW || self.pushed_bytes > MAX_PUSHED_BYTES_PER_WINDOW {
            Some("push budget exceeded")
        } else {
            None
        }
    }
}

impl Session {
    fn new() -> Self {
        Session {
            cert: None,
            storage_role: false,
            established: false,
            trusted: false,
            own_device: false,
            hello_sent: false,
            joining: None,
            queued_invites: vec![],
            offered: HashMap::new(),
            offered_at: HashMap::new(),
            planned_from: HashMap::new(),
            plans: HashMap::new(),
            queue: vec![],
            parked: vec![],
            running: BTreeMap::new(),
            wants: HashMap::new(),
            budget: Budget::default(),
        }
    }

    fn idle(&self) -> bool {
        self.queue.is_empty() && self.running.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub device: DeviceKey,
    pub established: bool,
    pub storage_role: bool,
    pub syncing: bool,
}

#[derive(Default)]
pub struct Engine {
    sessions: BTreeMap<DeviceKey, Session>,
    next_tag: u32,
    pairing: Option<PairingState>,
    backfill_paused: bool,
    on_demand: Vec<Job>,
    /// Which peer an event was just received from, so it is not echoed back.
    came_from: HashMap<EventHash, DeviceKey>,
}

struct ScopeKeys<'a> {
    r: &'a Replica,
    scope: ScopeId,
    control: bool,
}

impl KeySource for ScopeKeys<'_> {
    fn keys(&self, lo: Key, hi: Key) -> Result<Vec<Key>> {
        let mut v = self.r.recon_keys(self.scope, self.control, lo.0, hi.0.saturating_add(1))?;
        v.retain(|k| *k >= lo && *k < hi);
        Ok(v)
    }
}

fn layer_keys(r: &Replica, scope: ScopeId, layer: Layer) -> ScopeKeys<'_> {
    ScopeKeys { r, scope, control: layer == Layer::Control }
}

fn bound(ms: u64) -> Key {
    (hlc::pack(ms, 0), EventId([0; 16]))
}

impl Engine {
    pub fn new() -> Self {
        Engine { next_tag: 1, ..Default::default() }
    }

    fn tag(&mut self) -> u32 {
        self.next_tag = self.next_tag.wrapping_add(1).max(1);
        self.next_tag
    }

    pub fn sessions(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|(d, s)| SessionInfo { device: *d, established: s.established, storage_role: s.storage_role, syncing: !s.idle() }).collect()
    }

    pub fn is_connected(&self, device: &DeviceKey) -> bool {
        self.sessions.get(device).is_some_and(|s| s.established && s.trusted)
    }

    pub fn backfill_paused(&self) -> bool {
        self.backfill_paused
    }

    /// Pauses or resumes reconciling windows older than a week (for battery or metered networks).
    pub fn set_backfill_paused(&mut self, r: &mut Replica, paused: bool) -> Result<Vec<Out>> {
        self.backfill_paused = paused;
        if !paused {
            for s in self.sessions.values_mut() {
                let parked = std::mem::take(&mut s.parked);
                s.queue.extend(parked);
            }
        }
        self.poll(r)
    }

    /// Fetches history older than `before_hlc` from the first peer that has the scope.
    pub fn request_history(&mut self, r: &mut Replica, scope: ScopeId, before_hlc: u64, days: u32) -> Result<Vec<Out>> {
        let lo_ms = hlc::physical_ms(before_hlc).saturating_sub(days as u64 * DAY_MS);
        let job = Job { scope, layer: Layer::Content, lo: bound(lo_ms), hi: (before_hlc, EventId([0; 16])), kind: JobKind::OnDemand };
        self.on_demand.push(job);
        self.poll(r)
    }

    // ---- connection lifecycle ----

    pub fn connect(&mut self, r: &mut Replica, peer: DeviceKey) -> Result<Vec<Out>> {
        if r.is_self_revoked() {
            return Ok(vec![Out::Close(peer, "this device is revoked".into())]);
        }
        let queued = self.sessions.remove(&peer).map(|s| s.queued_invites).unwrap_or_default();
        let mut s = Session::new();
        s.queued_invites = queued;
        s.hello_sent = true;
        self.sessions.insert(peer, s);
        Ok(vec![Out::Send(peer, self.hello(r))])
    }

    /// Connects as a new device using a code shown on an existing device.
    pub fn connect_pairing(&mut self, r: &mut Replica, invite: &PairingInvite) -> Result<Vec<Out>> {
        let mut s = Session::new();
        s.joining = Some(invite.code.clone());
        self.sessions.insert(invite.device, s);
        let proof = pairing_proof(&invite.code, &r.device_key(), &invite.device);
        Ok(vec![Out::Send(invite.device, Msg::PairRequest { cert: r.cert().clone(), proof })])
    }

    /// Asks the inviter to admit this user. Sent once the session to the inviter is up.
    pub fn redeem_invite(&mut self, r: &mut Replica, token: InviteToken) -> Result<Vec<Out>> {
        let peer = token.inviter;
        let s = self.sessions.entry(peer).or_insert_with(Session::new);
        if s.established {
            s.trusted = true;
            let msg = Msg::InviteRedeem { scope: token.scope, token: token.token, cert: r.cert().clone() };
            r.join_scope_tentatively(token.scope)?;
            return Ok(vec![Out::Send(peer, msg)]);
        }
        s.queued_invites.push(token);
        Ok(vec![])
    }

    pub fn disconnect(&mut self, peer: &DeviceKey) {
        if let Some(s) = self.sessions.remove(peer) {
            // Unfinished on-demand requests move to the next peer.
            for j in s.queue.into_iter().chain(s.running.into_values().map(|r| r.job)) {
                if j.kind == JobKind::OnDemand {
                    self.on_demand.push(j);
                }
            }
        }
    }

    /// Ends every session (app suspend). The caller closes the connections.
    pub fn disconnect_all(&mut self) -> Vec<Out> {
        let peers: Vec<DeviceKey> = self.sessions.keys().copied().collect();
        let mut out = Vec::new();
        for p in peers {
            out.push(Out::Send(p, Msg::Bye { reason: "suspended".into() }));
            out.push(Out::Close(p, "suspended".into()));
            self.disconnect(&p);
        }
        out
    }

    /// Opens a pairing window on this (existing) device.
    pub fn start_pairing(&mut self, r: &Replica, ttl_ms: u64) -> PairingInvite {
        let code = generate_code(r);
        self.pairing = Some(PairingState { code: code.clone(), expires_ms: r.env().now_ms() + ttl_ms, attempts: 0 });
        PairingInvite { device: r.device_key(), code }
    }

    pub fn cancel_pairing(&mut self) {
        self.pairing = None;
    }

    fn hello(&self, r: &Replica) -> Msg {
        Msg::Hello { version: PROTOCOL_VERSION, cert: r.cert().clone(), storage_role: r.config().storage_role }
    }

    // ---- messages ----

    pub fn handle(&mut self, r: &mut Replica, peer: DeviceKey, msg: Msg) -> Result<Vec<Out>> {
        let mut out = Vec::new();
        let sync_msg = matches!(msg, Msg::Offer { .. } | Msg::Recon { .. } | Msg::Want { .. } | Msg::Events { .. } | Msg::Ack { .. } | Msg::Seen { .. });
        let handshake = matches!(msg, Msg::Hello { .. } | Msg::PairRequest { .. } | Msg::PairingGrant { .. } | Msg::PairDenied { .. } | Msg::Bye { .. });
        let trusted = self.sessions.get(&peer).is_some_and(|s| s.established && s.trusted);
        let over = if trusted {
            match &msg {
                Msg::Events { tag, events, .. } => self.sessions.get_mut(&peer).unwrap().budget.charge(r.env().now_ms(), events, *tag == 0),
                _ => None,
            }
        } else if handshake {
            None
        } else {
            // Without a session there is nothing to answer.
            let Some(s) = self.sessions.get_mut(&peer) else { return Ok(out) };
            s.budget.untrusted_msgs += 1;
            (s.budget.untrusted_msgs > MAX_UNTRUSTED_MSGS).then_some("too many messages before admission")
        };
        if let Some(why) = over {
            tracing::warn!(?peer, why, "disconnecting peer");
            out.push(Out::Send(peer, Msg::Bye { reason: why.into() }));
            out.push(Out::Close(peer, why.into()));
            self.disconnect(&peer);
            return Ok(out);
        }
        if sync_msg && !trusted {
            return Ok(out);
        }
        self.sessions.entry(peer).or_insert_with(Session::new);
        match msg {
            Msg::Hello { version, cert, storage_role } => self.on_hello(r, peer, version, cert, storage_role, &mut out)?,
            Msg::Offer { scopes } => self.on_offer(r, peer, scopes)?,
            Msg::Recon { scope, layer, tag, lo, ranges, from_initiator } => {
                if from_initiator {
                    self.on_recon_request(r, peer, scope, layer, tag, lo, ranges, &mut out)?
                } else {
                    self.on_recon_reply(r, peer, tag, lo, ranges, &mut out)?
                }
            }
            Msg::Want { scope, tag, ids, hashes } => self.on_want(r, peer, scope, tag, ids, hashes, &mut out)?,
            Msg::Events { scope: _, tag, events, last } => self.on_events(r, peer, tag, events, last, &mut out)?,
            Msg::Ack { hashes } => r.ack(&hashes)?,
            Msg::Denied { scope } => {
                if let Some(s) = self.sessions.get_mut(&peer) {
                    s.queue.retain(|j| j.scope != scope);
                    s.running.retain(|_, j| j.job.scope != scope);
                }
                if r.is_tentative(scope)? && !self.sessions.values().any(|s| s.running.values().any(|j| j.job.scope == scope)) {
                    r.settle_scope(scope)?;
                }
            }
            Msg::PairRequest { cert, proof } => self.on_pair_request(r, peer, cert, proof, &mut out)?,
            Msg::PairingGrant { sealed_user, cert } => self.on_pairing_grant(r, peer, sealed_user, cert, &mut out)?,
            Msg::PairDenied { reason } => {
                out.push(Out::Notify(Notice::PairingFailed(reason)));
                out.push(Out::Close(peer, "pairing denied".into()));
                self.sessions.remove(&peer);
            }
            Msg::InviteRedeem { scope, token, cert } => {
                let res = if !self.sessions[&peer].established {
                    Err(Error::InvalidInput("hello required".into()))
                } else if cert.device != peer || !cert.verify() {
                    Err(Error::InvalidInput("bad certificate".into()))
                } else {
                    r.admit_invitee(scope, token, &cert)
                };
                let error = res.err().map(|e| e.to_string());
                out.push(Out::Send(peer, Msg::InviteResult { scope, error: error.clone() }));
                if error.is_none() {
                    self.sessions.get_mut(&peer).unwrap().trusted = true;
                    r.upsert_known_device(&cert)?;
                    r.record_contact(&peer, None)?;
                    self.offer_new(r, peer, &mut out)?;
                }
            }
            Msg::InviteResult { scope, error } => {
                if error.is_some() {
                    r.settle_scope(scope)?;
                }
                out.push(Out::Notify(Notice::InviteResult { scope, error }))
            }
            Msg::Bye { reason } => {
                self.disconnect(&peer);
                out.push(Out::Close(peer, reason));
                return Ok(out);
            }
            Msg::Seen { scope, control_heads, heads } => self.on_seen(r, peer, scope, control_heads, heads)?,
            Msg::IdentityHandoff { sealed_user, cert, retired } => self.on_identity_handoff(r, peer, sealed_user, cert, retired, &mut out)?,
        }
        out.extend(self.poll(r)?);
        Ok(out)
    }

    fn on_hello(&mut self, r: &mut Replica, peer: DeviceKey, version: u32, cert: DeviceCert, storage_role: bool, out: &mut Vec<Out>) -> Result<()> {
        if version != PROTOCOL_VERSION {
            out.push(Out::Notify(Notice::Incompatible { device: peer, version }));
            out.push(Out::Send(peer, Msg::Bye { reason: format!("protocol {PROTOCOL_VERSION} required") }));
            out.push(Out::Close(peer, "incompatible protocol".into()));
            self.sessions.remove(&peer);
            return Ok(());
        }
        if cert.device != peer || !cert.verify() {
            out.push(Out::Close(peer, "bad certificate".into()));
            self.sessions.remove(&peer);
            return Ok(());
        }
        if r.is_device_revoked(&peer)? {
            out.push(Out::Notify(Notice::PeerRevoked(peer)));
            out.push(Out::Send(peer, Msg::Bye { reason: "revoked".into() }));
            out.push(Out::Close(peer, "revoked".into()));
            self.sessions.remove(&peer);
            return Ok(());
        }
        // Admission: this user's devices and members' devices sync. The inviter of a queued
        // invite is trusted by the redeemer. Strangers stay only while an invite is open, so
        // they can redeem it; otherwise they are turned away.
        let (vouched, was_trusted) = {
            let s = &self.sessions[&peer];
            (!s.queued_invites.is_empty(), s.trusted)
        };
        // A device of this user still on a retired identity is owed the current one.
        let successor = r.successor_cert(&cert)?;
        let trusted = was_trusted || vouched || successor.is_some() || r.is_trusted_peer(&cert)?;
        let own_device = r.is_own_device(&peer)?;
        if !trusted && !r.has_open_invites()? {
            out.push(Out::Notify(Notice::PeerRejected(peer)));
            out.push(Out::Send(peer, Msg::Bye { reason: "not a member".into() }));
            out.push(Out::Close(peer, "not a member".into()));
            self.sessions.remove(&peer);
            return Ok(());
        }
        let send_hello = {
            let s = self.sessions.get_mut(&peer).expect("session exists");
            s.cert = Some(cert.clone());
            s.storage_role = storage_role;
            s.established = true;
            s.trusted = trusted;
            s.own_device = own_device;
            s.offered.clear();
            !std::mem::replace(&mut s.hello_sent, true)
        };
        if send_hello {
            out.push(Out::Send(peer, self.hello(r)));
        }
        if !trusted {
            return Ok(());
        }
        r.upsert_known_device(&cert)?;
        r.admit_device(&cert)?;
        r.record_contact(&peer, None)?;
        if let Some(c) = successor {
            out.push(Out::Send(peer, handoff(r, &cert, c)?));
        }
        let invites = std::mem::take(&mut self.sessions.get_mut(&peer).unwrap().queued_invites);
        for t in invites {
            r.join_scope_tentatively(t.scope)?;
            out.push(Out::Send(peer, Msg::InviteRedeem { scope: t.scope, token: t.token, cert: r.cert().clone() }));
        }
        self.offer_new(r, peer, out)
    }

    /// Offers every scope the peer may read that has not been offered in this session, or that
    /// it can now read in full.
    fn offer_new(&mut self, r: &mut Replica, peer: DeviceKey, out: &mut Vec<Out>) -> Result<()> {
        let mut scopes = Vec::new();
        let mut full = Vec::new();
        for scope in r.scope_ids()? {
            let was = self.sessions[&peer].offered.get(&scope).copied();
            if !r.may_read_control(scope, &peer)? {
                continue;
            }
            let now_full = r.is_authorized(scope, &peer)?;
            match was {
                Some(w) if w == now_full => continue,
                Some(true) => {
                    self.sessions.get_mut(&peer).unwrap().offered.insert(scope, false);
                    continue;
                }
                _ => {}
            }
            let (control_heads, heads) = r.scope_heads(scope)?;
            scopes.push(ScopeOffer { scope, control_heads, heads });
            full.push(now_full);
        }
        if scopes.is_empty() {
            return Ok(());
        }
        let storing = self.sessions[&peer].storage_role;
        let mut offered_at = Vec::new();
        for o in &scopes {
            if storing {
                r.record_storage_peer(&peer, o.scope)?;
            }
            offered_at.push((o.scope, (o.control_heads.clone(), o.heads.clone(), r.max_seq(o.scope)?)));
        }
        let s = self.sessions.get_mut(&peer).unwrap();
        s.offered.extend(scopes.iter().map(|o| o.scope).zip(full));
        s.offered_at.extend(offered_at);
        out.push(Out::Send(peer, Msg::Offer { scopes }));
        Ok(())
    }

    fn on_offer(&mut self, r: &mut Replica, peer: DeviceKey, scopes: Vec<ScopeOffer>) -> Result<()> {
        for o in scopes {
            if !r.scope_exists(o.scope)? {
                r.join_scope_tentatively(o.scope)?;
            }
            r.record_contact(&peer, Some(o.scope))?;
            // Equal heads do not imply equal sets: pushes can leave gaps below the heads. Every
            // window is reconciled; in-sync windows cost one fingerprint round.
            let plan = self.plan(r, o.scope)?;
            let s = self.sessions.get_mut(&peer).unwrap();
            s.queue.retain(|j| j.scope != o.scope || j.kind == JobKind::OnDemand);
            s.planned_from.insert(o.scope, (o.control_heads, o.heads));
            let w = plan[0];
            s.queue.push(Job { scope: o.scope, layer: w.layer, lo: w.lo, hi: w.hi, kind: JobKind::Phase(0) });
            s.plans.insert(o.scope, plan);
        }
        Ok(())
    }

    fn plan(&self, r: &Replica, scope: ScopeId) -> Result<Vec<Window>> {
        let now = r.env().now_ms();
        let mut plan = vec![Window { layer: Layer::Control, lo: MIN_KEY, hi: MAX_KEY, backfill: false }];
        let limit_days = match r.history_mode(scope)? {
            HistoryMode::Full => None,
            HistoryMode::Recent { days } => Some(days as u64),
        };
        let mut hi = MAX_KEY;
        for d in WINDOWS_DAYS {
            if let Some(limit) = limit_days
                && d >= limit
            {
                plan.push(Window {
                    layer: Layer::Content,
                    lo: bound(now.saturating_sub(limit * DAY_MS)),
                    hi,
                    backfill: limit > BACKFILL_AFTER_DAYS && d > BACKFILL_AFTER_DAYS,
                });
                return Ok(plan);
            }
            let lo = bound(now.saturating_sub(d * DAY_MS));
            plan.push(Window { layer: Layer::Content, lo, hi, backfill: d > BACKFILL_AFTER_DAYS });
            hi = lo;
        }
        if let Some(limit) = limit_days {
            plan.push(Window { layer: Layer::Content, lo: bound(now.saturating_sub(limit * DAY_MS)), hi, backfill: true });
        } else {
            plan.push(Window { layer: Layer::Content, lo: MIN_KEY, hi, backfill: true });
        }
        Ok(plan)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_recon_request(
        &mut self,
        r: &mut Replica,
        peer: DeviceKey,
        scope: ScopeId,
        layer: Layer,
        tag: u32,
        lo: Key,
        ranges: Vec<Range>,
        out: &mut Vec<Out>,
    ) -> Result<()> {
        let allowed = match layer {
            Layer::Control => r.may_read_control(scope, &peer)?,
            Layer::Content => r.is_authorized(scope, &peer)?,
        };
        if !self.sessions[&peer].established || !allowed {
            out.push(Out::Send(peer, Msg::Denied { scope }));
            return Ok(());
        }
        let reply = recon::respond(&layer_keys(r, scope, layer), lo, &ranges)?;
        out.push(Out::Send(peer, Msg::Recon { scope, layer, tag, lo, ranges: reply.ranges, from_initiator: false }));
        Ok(())
    }

    fn on_recon_reply(&mut self, r: &mut Replica, peer: DeviceKey, tag: u32, lo: Key, ranges: Vec<Range>, out: &mut Vec<Out>) -> Result<()> {
        let Some(job) = self.sessions.get(&peer).and_then(|s| s.running.get(&tag)).map(|j| j.job.clone()) else { return Ok(()) };
        if ranges.is_empty() {
            self.sessions.get_mut(&peer).unwrap().running.get_mut(&tag).unwrap().recon_done = true;
        } else {
            let reply = recon::respond(&layer_keys(r, job.scope, job.layer), lo, &ranges)?;
            for chunk in reply.need.chunks(WANT_CHUNK) {
                let wt = self.tag();
                let s = self.sessions.get_mut(&peer).unwrap();
                s.wants.insert(wt, tag);
                s.running.get_mut(&tag).unwrap().outstanding += 1;
                out.push(Out::Send(peer, Msg::Want { scope: job.scope, tag: wt, ids: chunk.to_vec(), hashes: vec![] }));
            }
            if reply.ranges.is_empty() {
                self.sessions.get_mut(&peer).unwrap().running.get_mut(&tag).unwrap().recon_done = true;
            } else {
                out.push(Out::Send(peer, Msg::Recon { scope: job.scope, layer: job.layer, tag, lo, ranges: reply.ranges, from_initiator: true }));
            }
        }
        self.maybe_finish(r, peer, tag, out)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_want(
        &mut self,
        r: &mut Replica,
        peer: DeviceKey,
        scope: ScopeId,
        tag: u32,
        ids: Vec<EventId>,
        hashes: Vec<EventHash>,
        out: &mut Vec<Out>,
    ) -> Result<()> {
        let full = r.is_authorized(scope, &peer)?;
        if !self.sessions[&peer].established || !(full || r.may_read_control(scope, &peer)?) {
            out.push(Out::Send(peer, Msg::Denied { scope }));
            return Ok(());
        }
        let mut events = r.events_by_ids(scope, &ids)?;
        events.extend(r.events_by_hashes(&hashes)?);
        if !full {
            events.retain(|e| is_control_event(e));
        }
        let mut chunk = Vec::new();
        let mut size = 0;
        for e in events {
            if size + e.len() > EVENTS_CHUNK_BYTES && !chunk.is_empty() {
                out.push(Out::Send(peer, Msg::Events { scope, tag, events: std::mem::take(&mut chunk), last: false }));
                size = 0;
            }
            size += e.len();
            chunk.push(e);
        }
        out.push(Out::Send(peer, Msg::Events { scope, tag, events: chunk, last: true }));
        Ok(())
    }

    fn on_events(&mut self, r: &mut Replica, peer: DeviceKey, tag: u32, events: Vec<Vec<u8>>, last: bool, out: &mut Vec<Out>) -> Result<()> {
        let mut acks = Vec::new();
        let mut missing: HashMap<ScopeId, Vec<EventHash>> = HashMap::new();
        for b in events {
            match r.ingest_from(&b, Some(peer))? {
                IngestOutcome::Stored { hash, .. } => {
                    self.came_from.insert(hash, peer);
                    acks.push(hash);
                }
                IngestOutcome::Duplicate { hash } => acks.push(hash),
                IngestOutcome::Held { scope, missing: m, .. } => {
                    let v = missing.entry(scope).or_default();
                    for h in m {
                        if !v.contains(&h) {
                            v.push(h);
                        }
                    }
                }
                IngestOutcome::NotJoined { scope } => tracing::debug!(?scope, "dropped event for a scope not joined"),
                IngestOutcome::Invalid(reason) => {
                    tracing::debug!(%reason, "dropped invalid event");
                    let s = self.sessions.get_mut(&peer).unwrap();
                    s.budget.invalid += 1;
                    if s.budget.invalid > MAX_INVALID_PER_SESSION {
                        tracing::warn!(?peer, "disconnecting peer: too many invalid events");
                        out.push(Out::Send(peer, Msg::Bye { reason: "too many invalid events".into() }));
                        out.push(Out::Close(peer, "too many invalid events".into()));
                        self.disconnect(&peer);
                        return Ok(());
                    }
                }
            }
        }
        if !acks.is_empty() {
            out.push(Out::Send(peer, Msg::Ack { hashes: acks }));
        }
        for (scope, hashes) in missing {
            let wt = self.tag();
            out.push(Out::Send(peer, Msg::Want { scope, tag: wt, ids: vec![], hashes }));
        }
        if tag != 0
            && last
            && let Some(s) = self.sessions.get_mut(&peer)
            && let Some(rt) = s.wants.remove(&tag)
        {
            if let Some(j) = s.running.get_mut(&rt) {
                j.outstanding = j.outstanding.saturating_sub(1);
            }
            self.maybe_finish(r, peer, rt, out)?;
        }
        Ok(())
    }

    fn maybe_finish(&mut self, r: &mut Replica, peer: DeviceKey, tag: u32, out: &mut Vec<Out>) -> Result<()> {
        let s = self.sessions.get_mut(&peer).unwrap();
        let done = s.running.get(&tag).is_some_and(|j| j.recon_done && j.outstanding == 0);
        if !done {
            return Ok(());
        }
        let job = s.running.remove(&tag).unwrap().job;
        match job.kind {
            JobKind::Phase(i) => {
                if job.layer == Layer::Content {
                    r.mark_synced_from(job.scope, job.lo.0)?;
                } else if r.settle_scope(job.scope)? {
                    r.mark_synced(job.scope)?;
                } else {
                    // Offered a scope this user isn't in: it has been purged.
                    for s in self.sessions.values_mut() {
                        s.plans.remove(&job.scope);
                        s.planned_from.remove(&job.scope);
                        s.queue.retain(|j| j.scope != job.scope);
                        s.parked.retain(|j| j.scope != job.scope);
                        s.running.retain(|_, j| j.job.scope != job.scope);
                    }
                    self.on_demand.retain(|j| j.scope != job.scope);
                    return Ok(());
                }
                let s = self.sessions.get_mut(&peer).unwrap();
                if let Some(w) = s.plans.get(&job.scope).and_then(|p| p.get(i + 1)).copied() {
                    let next = Job { scope: job.scope, layer: w.layer, lo: w.lo, hi: w.hi, kind: JobKind::Phase(i + 1) };
                    if w.backfill && self.backfill_paused {
                        s.parked.push(next);
                    } else {
                        s.queue.push(next);
                    }
                } else if r.config().storage_role
                    && r.history_mode(job.scope)? == HistoryMode::Full
                    && let Some((control_heads, heads)) = s.planned_from.remove(&job.scope)
                {
                    out.push(Out::Send(peer, Msg::Seen { scope: job.scope, control_heads, heads }));
                }
            }
            JobKind::OnDemand => {
                if let Some(from) = r.synced_from_hlc(job.scope)?
                    && job.hi.0 >= from
                    && job.lo.0 < from
                {
                    r.mark_synced_from(job.scope, job.lo.0)?;
                }
            }
        }
        Ok(())
    }

    /// A storing peer finished a full pass over an offer: compact what it now holds.
    fn on_seen(&mut self, r: &mut Replica, peer: DeviceKey, scope: ScopeId, control_heads: Vec<EventHash>, heads: Vec<EventHash>) -> Result<()> {
        let Some(s) = self.sessions.get_mut(&peer) else { return Ok(()) };
        if !s.storage_role {
            return Ok(());
        }
        let Some((c, h, seq)) = s.offered_at.get(&scope) else { return Ok(()) };
        if *c != control_heads || *h != heads {
            return Ok(());
        }
        let seq = *seq;
        r.record_storage_seen(&peer, scope, seq)?;
        let done = r.compact(scope)?;
        if done.edits_dropped + done.loro_folded > 0 {
            tracing::debug!(?scope, upto = done.upto_seq, edits = done.edits_dropped, loro = done.loro_folded, "compacted");
        }
        Ok(())
    }

    fn on_pair_request(&mut self, r: &mut Replica, peer: DeviceKey, cert: DeviceCert, proof: [u8; 32], out: &mut Vec<Out>) -> Result<()> {
        let now = r.env().now_ms();
        let ok = match self.pairing.as_mut() {
            Some(p) if p.expires_ms >= now && p.attempts < PAIRING_ATTEMPTS => {
                p.attempts += 1;
                cert.device == peer && cert.verify() && crypto_eq(&pairing_proof(&p.code, &peer, &r.device_key()), &proof)
            }
            _ => false,
        };
        if !ok {
            if self.pairing.as_ref().is_some_and(|p| p.attempts >= PAIRING_ATTEMPTS) {
                self.pairing = None;
            }
            out.push(Out::Send(peer, Msg::PairDenied { reason: "invalid or expired code".into() }));
            out.push(Out::Close(peer, "pairing denied".into()));
            self.sessions.remove(&peer);
            return Ok(());
        }
        self.pairing = None;
        let user = r.user_secret().entropy();
        let new_cert = DeviceCert::issue(r.user_secret(), peer, &cert.name, cert.platform, now);
        let sealed_user = crypto::seal_to_device(r.env().as_ref(), &peer.0, &user)?;
        r.admit_device(&new_cert)?;
        out.push(Out::Send(peer, Msg::PairingGrant { sealed_user, cert: new_cert }));
        out.push(Out::Notify(Notice::Paired { device: peer }));
        Ok(())
    }

    fn on_pairing_grant(&mut self, r: &mut Replica, peer: DeviceKey, sealed: Vec<u8>, cert: DeviceCert, out: &mut Vec<Out>) -> Result<()> {
        if self.sessions.get(&peer).and_then(|s| s.joining.as_ref()).is_none() {
            return Ok(());
        }
        let entropy: [u8; 32] = crypto::unseal(r.device_secret().signing_key(), &sealed)?.try_into().map_err(|_| Error::Pairing("bad grant".into()))?;
        let user = UserSecret::from_entropy(entropy);
        if cert.user != user.public() || cert.device != r.device_key() || !cert.verify() {
            out.push(Out::Notify(Notice::PairingFailed("grant does not match".into())));
            return Ok(());
        }
        r.adopt_user(user)?;
        let s = self.sessions.get_mut(&peer).unwrap();
        s.joining = None;
        s.hello_sent = true;
        out.push(Out::Notify(Notice::Paired { device: peer }));
        out.push(Out::Send(peer, self.hello(r)));
        Ok(())
    }

    /// Another device of this user rotated the identity. Only a device that was already one of
    /// this user's devices may hand it over.
    fn on_identity_handoff(&mut self, r: &mut Replica, peer: DeviceKey, sealed: Vec<u8>, cert: DeviceCert, retired: UserKey, out: &mut Vec<Out>) -> Result<()> {
        if !self.sessions.get(&peer).is_some_and(|s| s.established && s.trusted && s.own_device) {
            return Ok(());
        }
        let Ok(entropy) = crypto::unseal(r.device_secret().signing_key(), &sealed).map(<[u8; 32]>::try_from) else { return Ok(()) };
        let Ok(entropy) = entropy else { return Ok(()) };
        if !r.accept_identity_handoff(UserSecret::from_entropy(entropy), cert, retired)? {
            return Ok(());
        }
        tracing::info!(?peer, "took on the rotated identity");
        out.extend(self.rehello(r)?);
        Ok(())
    }

    /// Replaces this user's identity after a device was stolen; see `Replica::rotate_identity`.
    /// Connected devices of this user get the new secret right away.
    pub fn rotate_identity(&mut self, r: &mut Replica, revoke: &[DeviceKey]) -> Result<(IdentityRotation, Vec<Out>)> {
        let report = r.rotate_identity(revoke)?;
        let mut out = self.rehello(r)?;
        out.extend(self.poll(r)?);
        Ok((report, out))
    }

    /// Presents this device's current certificate to every established peer, and hands the
    /// current identity to peers still on a retired one.
    fn rehello(&mut self, r: &mut Replica) -> Result<Vec<Out>> {
        let mut out = Vec::new();
        let hello = self.hello(r);
        for (peer, s) in &self.sessions {
            if !s.established || r.is_device_revoked(peer)? {
                continue;
            }
            out.push(Out::Send(*peer, hello.clone()));
            if let Some(cert) = &s.cert
                && let Some(c) = r.successor_cert(cert)?
            {
                out.push(Out::Send(*peer, handoff(r, cert, c)?));
            }
        }
        Ok(out)
    }

    // ---- driving ----

    /// Pushes new events and starts queued reconciliation. Call after local writes.
    pub fn poll(&mut self, r: &mut Replica) -> Result<Vec<Out>> {
        let mut out = Vec::new();
        if r.is_self_revoked() {
            let peers: Vec<_> = self.sessions.keys().copied().collect();
            for p in peers {
                out.push(Out::Close(p, "this device is revoked".into()));
            }
            self.sessions.clear();
            return Ok(out);
        }
        // Peers revoked while connected.
        let revoked: Vec<DeviceKey> = self.sessions.keys().copied().filter(|p| r.is_device_revoked(p).unwrap_or(false)).collect();
        for p in revoked {
            out.push(Out::Send(p, Msg::Bye { reason: "revoked".into() }));
            out.push(Out::Close(p, "revoked".into()));
            self.sessions.remove(&p);
        }

        let fresh = r.take_new_events();
        let mut touched: Vec<ScopeId> = Vec::new();
        let peers: Vec<DeviceKey> = self.sessions.iter().filter(|(_, s)| s.established && s.trusted).map(|(d, _)| *d).collect();
        let mut batches: BTreeMap<(DeviceKey, ScopeId), Vec<Vec<u8>>> = BTreeMap::new();
        let mut auth_cache: HashMap<(DeviceKey, ScopeId), bool> = HashMap::new();
        for (scope, hash, bytes) in fresh {
            if !touched.contains(&scope) {
                touched.push(scope);
            }
            let from = self.came_from.get(&hash).copied();
            for p in &peers {
                if Some(*p) == from {
                    continue;
                }
                let full = match auth_cache.get(&(*p, scope)) {
                    Some(v) => *v,
                    None => {
                        let v = r.is_authorized(scope, p)?;
                        auth_cache.insert((*p, scope), v);
                        v
                    }
                };
                if full || (is_control_event(&bytes) && r.may_read_control(scope, p)?) {
                    batches.entry((*p, scope)).or_default().push(bytes.clone());
                }
            }
        }
        self.came_from.clear();
        // Connected devices of members join scopes they are not yet certified in, and keys
        // this device received are sealed to its user's recovery key if nobody has yet.
        let mut admitted = false;
        for scope in &touched {
            admitted |= r.repair_recovery_grants(*scope)?;
            for p in &peers {
                if auth_cache.get(&(*p, *scope)) == Some(&true) {
                    continue;
                }
                if let Some(cert) = self.sessions[p].cert.clone()
                    && !r.is_authorized(*scope, p)?
                    && r.would_admit(*scope, &cert)?
                {
                    r.admit_device(&cert)?;
                    admitted = true;
                }
            }
        }
        for ((p, scope), events) in batches {
            out.push(Out::Send(p, Msg::Events { scope, tag: 0, events, last: true }));
        }
        // Membership may have changed: offer newly readable scopes.
        if !touched.is_empty() {
            for p in &peers {
                self.offer_new(r, *p, &mut out)?;
            }
        }

        // Hand on-demand requests to a peer that can serve them.
        let pending = std::mem::take(&mut self.on_demand);
        for job in pending {
            let target = peers.iter().copied().find(|p| self.sessions[p].offered.contains_key(&job.scope) || self.sessions[p].plans.contains_key(&job.scope));
            match target {
                Some(p) => self.sessions.get_mut(&p).unwrap().queue.push(job),
                None => self.on_demand.push(job),
            }
        }

        for p in peers {
            loop {
                let s = self.sessions.get_mut(&p).unwrap();
                if s.running.len() >= MAX_IN_FLIGHT || s.queue.is_empty() {
                    break;
                }
                let idx = (0..s.queue.len()).min_by_key(|i| s.queue[*i].priority()).unwrap();
                let job = s.queue.remove(idx);
                let tag = self.tag();
                let ranges = recon::initiate(&layer_keys(r, job.scope, job.layer), job.lo, job.hi)?;
                out.push(Out::Send(p, Msg::Recon { scope: job.scope, layer: job.layer, tag, lo: job.lo, ranges, from_initiator: true }));
                self.sessions.get_mut(&p).unwrap().running.insert(tag, Running { job, recon_done: false, outstanding: 0 });
            }
        }
        if admitted {
            out.extend(self.poll(r)?);
        }
        Ok(out)
    }
}

/// The current user secret sealed to a device presenting `presented` (a retired user's
/// certificate), with `successor`, its certificate under the current user.
fn handoff(r: &Replica, presented: &DeviceCert, successor: DeviceCert) -> Result<Msg> {
    let sealed_user = crypto::seal_to_device(r.env().as_ref(), &presented.device.0, &r.user_secret().entropy())?;
    Ok(Msg::IdentityHandoff { sealed_user, cert: successor, retired: presented.user })
}

fn is_control_event(bytes: &[u8]) -> bool {
    matches!(Envelope::decode(bytes), Ok(Decoded::Known(e)) if kind::is_control(e.kind))
}

fn crypto_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
