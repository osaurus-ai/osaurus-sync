//! Membership, roles and validation over a scope's control sub-DAG.
//!
//! Every peer runs the same deterministic computation over the same control events, so all
//! members agree on who may do what without a central authority. Wall clocks play no part in
//! validity: the membership state for an event is computed from its causal ancestors only.
//!
//! Concurrent conflicts resolve in order:
//! 1. Removal wins over any concurrent action by or about the removed member. A removal records,
//!    per device of the removed member, the highest HLC the remover had seen. Events from those
//!    devices above the cutoff that are not causal ancestors of the removal are rejected. Each
//!    device's HLC is strictly increasing, so the cutoff identifies exactly which of its events the
//!    remover had seen.
//! 2. A higher role wins over a lower role.
//! 3. The lower author key wins.
//!
//! Two removals that would each cut the other off resolve by the deterministic topological order
//! (HLC, then hash): the earlier removal stands.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use crate::envelope::kind;
use crate::identity::DeviceCert;
use crate::ids::{AgentId, DeviceKey, EventHash, EventId, Principal, Role, ScopeId, UserKey};
use crate::payload::{Control, InvitePayload};

/// Why an event was rejected. Codes are stable and shown to users as "Not delivered" reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    NotMember = 1,
    InsufficientRole = 2,
    RemovedConcurrently = 3,
    DeviceRevoked = 4,
    NoGenesis = 6,
    InvalidPayload = 7,
    AgentNotHosted = 8,
    NotAuthor = 9,
    BadEpoch = 10,
}

impl Reason {
    pub fn from_code(c: i64) -> Option<Reason> {
        Some(match c {
            1 => Reason::NotMember,
            2 => Reason::InsufficientRole,
            3 => Reason::RemovedConcurrently,
            4 => Reason::DeviceRevoked,
            6 => Reason::NoGenesis,
            7 => Reason::InvalidPayload,
            8 => Reason::AgentNotHosted,
            9 => Reason::NotAuthor,
            10 => Reason::BadEpoch,
            _ => return None,
        })
    }

    pub fn code(self) -> i64 {
        self as i64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Valid,
    Rejected(Reason),
}

impl Verdict {
    pub fn is_valid(&self) -> bool {
        matches!(self, Verdict::Valid)
    }
}

/// A control-layer event as the validator sees it. `body` is `None` for Loro updates and for
/// control kinds this version does not understand.
#[derive(Clone, Debug)]
pub struct CtlEvent {
    pub hash: EventHash,
    pub id: EventId,
    pub author: DeviceKey,
    pub hlc: u64,
    pub kind: u16,
    pub epoch: u64,
    pub control_parents: Vec<EventHash>,
    pub body: Option<Control>,
    /// Set when a plaintext control payload failed to parse.
    pub malformed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MembershipState {
    pub owner: Option<UserKey>,
    pub genesis: Option<EventHash>,
    pub members: BTreeMap<Principal, Role>,
    pub certs: BTreeMap<DeviceKey, BTreeMap<UserKey, DeviceCert>>,
    pub revoked: BTreeSet<DeviceKey>,
    pub agent_hosts: BTreeMap<AgentId, DeviceKey>,
    pub invites: BTreeMap<[u8; 32], InvitePayload>,
    pub used_invites: BTreeSet<[u8; 32]>,
}

impl MembershipState {
    /// Principals a device may act for, with their roles.
    pub fn acting(&self, device: &DeviceKey) -> Vec<(Principal, Role)> {
        if self.revoked.contains(device) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(users) = self.certs.get(device) {
            for user in users.keys() {
                if let Some(role) = self.members.get(&Principal::User(*user)) {
                    out.push((Principal::User(*user), *role));
                }
            }
        }
        for (agent, host) in &self.agent_hosts {
            if host == device
                && let Some(role) = self.members.get(&Principal::Agent(*agent))
            {
                out.push((Principal::Agent(*agent), *role));
            }
        }
        out
    }

    pub fn max_role(&self, device: &DeviceKey) -> Option<Role> {
        self.acting(device).into_iter().map(|(_, r)| r).max()
    }

    pub fn user_role(&self, device: &DeviceKey) -> Option<(UserKey, Role)> {
        self.acting(device)
            .into_iter()
            .filter_map(|(p, r)| match p {
                Principal::User(u) => Some((u, r)),
                _ => None,
            })
            .max_by_key(|(_, r)| *r)
    }

    pub fn devices_of(&self, user: &UserKey) -> Vec<DeviceKey> {
        self.certs.iter().filter(|(d, users)| users.contains_key(user) && !self.revoked.contains(d)).map(|(d, _)| *d).collect()
    }

    pub fn is_certified(&self, device: &DeviceKey) -> bool {
        self.certs.contains_key(device) && !self.revoked.contains(device)
    }
}

#[derive(Clone, Debug, Default)]
struct BitSet(Vec<u64>);

impl BitSet {
    fn insert(&mut self, i: usize) {
        let w = i / 64;
        if self.0.len() <= w {
            self.0.resize(w + 1, 0);
        }
        self.0[w] |= 1 << (i % 64);
    }
    fn contains(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
    fn union(&mut self, o: &BitSet) {
        if self.0.len() < o.0.len() {
            self.0.resize(o.0.len(), 0);
        }
        for (a, b) in self.0.iter_mut().zip(o.0.iter()) {
            *a |= *b;
        }
    }
    fn count(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }
    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(wi, w)| (0..64).filter(move |b| w & (1 << b) != 0).map(move |b| wi * 64 + b))
    }
}

/// Result of adding a control event.
#[derive(Debug, Default)]
pub struct AddOutcome {
    pub verdict: Option<Verdict>,
    /// Earlier control events whose verdict flipped.
    pub changed: Vec<(EventHash, Verdict)>,
    /// Content events in the scope must be revalidated (a removal or revocation arrived, or an
    /// earlier control verdict changed).
    pub revalidate_content: bool,
}

/// Validator state for one scope.
pub struct ScopeAuth {
    pub scope: ScopeId,
    events: Vec<CtlEvent>,
    index: HashMap<EventHash, usize>,
    /// Ancestors of each event including itself.
    anc: Vec<BitSet>,
    verdicts: Vec<Verdict>,
    state_cache: HashMap<Vec<EventHash>, Arc<MembershipState>>,
    full: Option<Arc<MembershipState>>,
}

impl ScopeAuth {
    pub fn new(scope: ScopeId) -> Self {
        Self { scope, events: Vec::new(), index: HashMap::new(), anc: Vec::new(), verdicts: Vec::new(), state_cache: HashMap::new(), full: None }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn contains(&self, h: &EventHash) -> bool {
        self.index.contains_key(h)
    }

    pub fn verdict(&self, h: &EventHash) -> Option<Verdict> {
        self.index.get(h).map(|&i| self.verdicts[i])
    }

    pub fn event(&self, h: &EventHash) -> Option<&CtlEvent> {
        self.index.get(h).map(|&i| &self.events[i])
    }

    pub fn events(&self) -> impl Iterator<Item = (&CtlEvent, Verdict)> {
        self.events.iter().zip(self.verdicts.iter().copied())
    }

    pub fn missing_parents(&self, parents: &[EventHash]) -> Vec<EventHash> {
        parents.iter().filter(|p| !self.index.contains_key(p)).copied().collect()
    }

    fn insert_raw(&mut self, ev: CtlEvent) -> usize {
        let i = self.events.len();
        let mut a = BitSet::default();
        a.insert(i);
        for p in &ev.control_parents {
            if let Some(&pi) = self.index.get(p) {
                a.union(&self.anc[pi].clone());
            }
        }
        self.index.insert(ev.hash, i);
        self.events.push(ev);
        self.anc.push(a);
        self.verdicts.push(Verdict::Rejected(Reason::NoGenesis));
        i
    }

    /// Loads stored events without re-evaluating after each one. Call `recompute` afterwards.
    pub fn load(&mut self, ev: CtlEvent) {
        if !self.index.contains_key(&ev.hash) {
            self.insert_raw(ev);
        }
    }

    /// Adds a control event whose control parents are all present.
    pub fn add(&mut self, ev: CtlEvent) -> AddOutcome {
        if let Some(&i) = self.index.get(&ev.hash) {
            return AddOutcome { verdict: Some(self.verdicts[i]), ..Default::default() };
        }
        let is_cut = is_cutting(&ev);
        let n_before = self.events.len();
        let i = self.insert_raw(ev);
        self.state_cache.clear();
        self.full = None;
        // Fast path: an event that has every existing control event in its past and removes nobody
        // cannot change any earlier verdict.
        if !is_cut && self.anc[i].count() == n_before + 1 {
            let v = self.eval(i, &self.verdicts.clone(), &HashSet::new(), None);
            self.verdicts[i] = v;
            return AddOutcome { verdict: Some(v), changed: Vec::new(), revalidate_content: false };
        }
        let before: Vec<Verdict> = self.verdicts[..n_before].to_vec();
        self.recompute();
        let changed: Vec<(EventHash, Verdict)> =
            (0..n_before).filter(|&j| before[j] != self.verdicts[j]).map(|j| (self.events[j].hash, self.verdicts[j])).collect();
        let revalidate = !changed.is_empty() || (is_cut && self.verdicts[i].is_valid());
        AddOutcome { verdict: Some(self.verdicts[i]), changed, revalidate_content: revalidate }
    }

    /// Deterministic topological order: Kahn's algorithm, ties broken by (HLC, hash).
    fn topo_order(&self) -> Vec<usize> {
        let n = self.events.len();
        let mut indeg = vec![0usize; n];
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, e) in self.events.iter().enumerate() {
            let mut seen = HashSet::new();
            for p in &e.control_parents {
                if let Some(&pi) = self.index.get(p)
                    && seen.insert(pi)
                {
                    indeg[i] += 1;
                    children[pi].push(i);
                }
            }
        }
        let mut heap: BinaryHeap<Reverse<(u64, EventHash, usize)>> = BinaryHeap::new();
        for (i, d) in indeg.iter().enumerate() {
            if *d == 0 {
                heap.push(Reverse((self.events[i].hlc, self.events[i].hash, i)));
            }
        }
        let mut out = Vec::with_capacity(n);
        while let Some(Reverse((_, _, i))) = heap.pop() {
            out.push(i);
            for &c in &children[i] {
                indeg[c] -= 1;
                if indeg[c] == 0 {
                    heap.push(Reverse((self.events[c].hlc, self.events[c].hash, c)));
                }
            }
        }
        out
    }

    /// Recomputes every verdict from scratch.
    pub fn recompute(&mut self) {
        let order = self.topo_order();
        let mut pos = vec![0usize; self.events.len()];
        for (k, &i) in order.iter().enumerate() {
            pos[i] = k;
        }
        let mut forced: HashSet<usize> = HashSet::new();
        loop {
            let mut verdicts = vec![Verdict::Rejected(Reason::NoGenesis); self.events.len()];
            for &i in &order {
                verdicts[i] = self.eval(i, &verdicts, &forced, Some(&pos));
            }
            let cutters: Vec<usize> = (0..self.events.len()).filter(|&r| verdicts[r].is_valid() && is_cutting(&self.events[r])).collect();
            let mut grew = false;
            for i in 0..self.events.len() {
                if verdicts[i].is_valid() && !is_cutting(&self.events[i]) && !forced.contains(&i) {
                    let past = self.past_of(i);
                    let st = self.state_for(&past, &verdicts);
                    if let Some(reason) = self.cut_off(&self.events[i], &past, &st, cutters.iter().copied()) {
                        let _ = reason;
                        forced.insert(i);
                        grew = true;
                    }
                }
            }
            if !grew {
                self.verdicts = verdicts;
                break;
            }
        }
        self.state_cache.clear();
        self.full = None;
    }

    fn past_of(&self, i: usize) -> BitSet {
        let mut past = BitSet::default();
        for p in &self.events[i].control_parents {
            if let Some(&pi) = self.index.get(p) {
                past.union(&self.anc[pi]);
            }
        }
        past
    }

    fn closure(&self, parents: &[EventHash]) -> BitSet {
        let mut past = BitSet::default();
        for p in parents {
            if let Some(&pi) = self.index.get(p) {
                past.union(&self.anc[pi]);
            }
        }
        past
    }

    fn is_ancestor(&self, a: usize, b: usize) -> bool {
        a != b && self.anc[b].contains(a)
    }

    fn state_for(&self, closure: &BitSet, verdicts: &[Verdict]) -> MembershipState {
        let mut st = MembershipState::default();
        let mut about: BTreeMap<Principal, Vec<usize>> = BTreeMap::new();
        let mut assigns: BTreeMap<AgentId, Vec<usize>> = BTreeMap::new();
        let mut geneses: Vec<usize> = Vec::new();
        for i in closure.iter() {
            if i >= self.events.len() || !verdicts[i].is_valid() {
                continue;
            }
            let e = &self.events[i];
            match &e.body {
                Some(Control::Genesis(_)) => geneses.push(i),
                Some(Control::MemberAdd(p)) => {
                    about.entry(p.subject).or_default().push(i);
                    if let Some(t) = p.invite {
                        st.used_invites.insert(t);
                    }
                }
                Some(Control::MemberRemove(p)) => about.entry(p.subject).or_default().push(i),
                Some(Control::RoleChange(p)) => about.entry(p.subject).or_default().push(i),
                Some(Control::DeviceCert(c)) => {
                    st.certs.entry(c.device).or_default().insert(c.user, c.clone());
                }
                Some(Control::DeviceRevoke(p)) => {
                    st.revoked.insert(p.device);
                }
                Some(Control::AgentAssign(b)) => assigns.entry(b.agent).or_default().push(i),
                Some(Control::Invite(p)) => {
                    st.invites.insert(p.token_hash, p.clone());
                }
                Some(Control::KeyUpdate(_)) | None => {}
            }
        }
        // Only one genesis is ever valid; see `eval`.
        if let Some(&g) = geneses.first()
            && let Some(Control::Genesis(p)) = &self.events[g].body
        {
            st.owner = Some(p.owner);
            st.genesis = Some(self.events[g].hash);
            st.certs.entry(p.owner_device.device).or_default().insert(p.owner, p.owner_device.clone());
            about.entry(Principal::User(p.owner)).or_default().insert(0, g);
        }
        for (subject, evs) in about {
            let maximal: Vec<usize> = evs.iter().copied().filter(|&a| !evs.iter().any(|&b| self.is_ancestor(a, b))).collect();
            let mut removed = false;
            let mut role: Option<Role> = None;
            for &m in &maximal {
                match &self.events[m].body {
                    Some(Control::MemberRemove(_)) => removed = true,
                    Some(Control::MemberAdd(p)) => role = role.max(Some(p.role)),
                    Some(Control::RoleChange(p)) => role = role.max(Some(p.role)),
                    Some(Control::Genesis(_)) => role = Some(Role::Owner),
                    _ => {}
                }
            }
            if !removed && let Some(r) = role {
                st.members.insert(subject, r);
            }
        }
        for (agent, evs) in assigns {
            let mut maximal: Vec<usize> = evs.iter().copied().filter(|&a| !evs.iter().any(|&b| self.is_ancestor(a, b))).collect();
            maximal.sort_by_key(|&m| (self.events[m].author, self.events[m].hash));
            if let Some(&m) = maximal.first()
                && let Some(Control::AgentAssign(b)) = &self.events[m].body
            {
                st.agent_hosts.insert(agent, b.host_device);
            }
        }
        st
    }

    /// Whether a valid removal or revocation outside the event's past cuts it off.
    fn cut_off(&self, e: &CtlEvent, past: &BitSet, st: &MembershipState, cutters: impl Iterator<Item = usize>) -> Option<Reason> {
        cut_off_by(e.author, e.hlc, st, cutters.filter(|&r| !past.contains(r)).map(|r| &self.events[r]))
    }

    fn eval(&self, i: usize, verdicts: &[Verdict], forced: &HashSet<usize>, pos: Option<&[usize]>) -> Verdict {
        let e = &self.events[i];
        if forced.contains(&i) {
            return Verdict::Rejected(Reason::RemovedConcurrently);
        }
        if e.malformed {
            return Verdict::Rejected(Reason::InvalidPayload);
        }
        let past = self.past_of(i);
        let st = self.state_for(&past, verdicts);

        if let Some(Control::Genesis(g)) = &e.body {
            let ok = e.control_parents.is_empty()
                && e.id.0 == self.scope.0
                && g.owner_device.user == g.owner
                && g.owner_device.device == e.author
                && g.owner_device.verify();
            if !ok {
                return Verdict::Rejected(Reason::InvalidPayload);
            }
            // A second genesis with the scope's id is a fork; the lowest hash wins.
            let lowest = self.events.iter().filter(|o| matches!(o.body, Some(Control::Genesis(_))) && o.id == e.id && !o.malformed).map(|o| o.hash).min();
            return if lowest == Some(e.hash) { Verdict::Valid } else { Verdict::Rejected(Reason::InvalidPayload) };
        }
        if st.owner.is_none() {
            return Verdict::Rejected(Reason::NoGenesis);
        }
        // Removals and revocations ordered earlier cut this event off.
        if let Some(pos) = pos {
            let earlier = (0..self.events.len()).filter(|&r| pos[r] < pos[i] && verdicts[r].is_valid() && is_cutting(&self.events[r]));
            if let Some(reason) = self.cut_off(e, &past, &st, earlier) {
                return Verdict::Rejected(reason);
            }
        }
        if st.revoked.contains(&e.author) {
            return Verdict::Rejected(Reason::DeviceRevoked);
        }
        let acting = st.acting(&e.author);
        let max_role = acting.iter().map(|(_, r)| *r).max();

        let Some(body) = &e.body else {
            // Loro updates and unknown control kinds need a posting member.
            if e.kind == kind::LORO_UPDATE && e.epoch == 0 {
                return Verdict::Rejected(Reason::BadEpoch);
            }
            return match max_role {
                None => Verdict::Rejected(Reason::NotMember),
                Some(r) if r >= Role::Member => Verdict::Valid,
                Some(_) => Verdict::Rejected(Reason::InsufficientRole),
            };
        };
        if e.epoch != 0 {
            return Verdict::Rejected(Reason::BadEpoch);
        }
        match body {
            Control::Genesis(_) => unreachable!(),
            Control::DeviceCert(c) => {
                if !c.verify() || st.revoked.contains(&c.device) {
                    return Verdict::Rejected(Reason::InvalidPayload);
                }
                if !st.members.contains_key(&Principal::User(c.user)) {
                    return Verdict::Rejected(Reason::NotMember);
                }
                // Any member device may publish a member's certificate, and a device may present its own.
                if max_role.is_some() || c.device == e.author { Verdict::Valid } else { Verdict::Rejected(Reason::NotMember) }
            }
            _ => {
                let Some(role) = max_role else { return Verdict::Rejected(Reason::NotMember) };
                match body {
                    Control::MemberAdd(p) => {
                        let ok = match role {
                            Role::Owner => true,
                            Role::Admin => p.role < Role::Admin,
                            _ => false,
                        };
                        if ok { Verdict::Valid } else { Verdict::Rejected(Reason::InsufficientRole) }
                    }
                    Control::MemberRemove(p) => {
                        let Some(&subject_role) = st.members.get(&p.subject) else {
                            return Verdict::Rejected(Reason::NotMember);
                        };
                        let is_self = acting.iter().any(|(pr, _)| *pr == p.subject);
                        let ok = is_self
                            || match role {
                                Role::Owner => true,
                                Role::Admin => subject_role < Role::Admin,
                                _ => false,
                            };
                        if ok { Verdict::Valid } else { Verdict::Rejected(Reason::InsufficientRole) }
                    }
                    Control::RoleChange(p) => {
                        let Some(&subject_role) = st.members.get(&p.subject) else {
                            return Verdict::Rejected(Reason::NotMember);
                        };
                        let ok = match role {
                            Role::Owner => true,
                            Role::Admin => subject_role < Role::Admin && p.role < Role::Admin,
                            _ => false,
                        };
                        if ok { Verdict::Valid } else { Verdict::Rejected(Reason::InsufficientRole) }
                    }
                    Control::DeviceRevoke(p) => {
                        let same_user =
                            st.certs.get(&p.device).is_some_and(|users| acting.iter().any(|(pr, _)| matches!(pr, Principal::User(u) if users.contains_key(u))));
                        if same_user || role == Role::Owner { Verdict::Valid } else { Verdict::Rejected(Reason::InsufficientRole) }
                    }
                    Control::AgentAssign(b) => {
                        if !b.verify() {
                            return Verdict::Rejected(Reason::InvalidPayload);
                        }
                        if !st.members.contains_key(&Principal::Agent(b.agent)) {
                            return Verdict::Rejected(Reason::NotMember);
                        }
                        if role >= Role::Member { Verdict::Valid } else { Verdict::Rejected(Reason::InsufficientRole) }
                    }
                    Control::KeyUpdate(_) => Verdict::Valid,
                    Control::Invite(p) => {
                        let ok = match role {
                            Role::Owner => true,
                            Role::Admin => p.role < Role::Admin,
                            _ => false,
                        };
                        if ok { Verdict::Valid } else { Verdict::Rejected(Reason::InsufficientRole) }
                    }
                    Control::Genesis(_) | Control::DeviceCert(_) => unreachable!(),
                }
            }
        }
    }

    /// Membership state as of a set of control parents.
    pub fn state_at(&mut self, control_parents: &[EventHash]) -> Arc<MembershipState> {
        let mut key = control_parents.to_vec();
        key.sort();
        key.dedup();
        if let Some(s) = self.state_cache.get(&key) {
            return s.clone();
        }
        let closure = self.closure(&key);
        let st = Arc::new(self.state_for(&closure, &self.verdicts));
        self.state_cache.insert(key, st.clone());
        st
    }

    /// Membership state over every known control event.
    pub fn full_state(&mut self) -> Arc<MembershipState> {
        if let Some(s) = &self.full {
            return s.clone();
        }
        let mut all = BitSet::default();
        for i in 0..self.events.len() {
            all.insert(i);
        }
        let st = Arc::new(self.state_for(&all, &self.verdicts));
        self.full = Some(st.clone());
        st
    }

    pub fn heads(&self) -> Vec<EventHash> {
        let mut has_child = vec![false; self.events.len()];
        for e in &self.events {
            for p in &e.control_parents {
                if let Some(&pi) = self.index.get(p) {
                    has_child[pi] = true;
                }
            }
        }
        let mut h: Vec<EventHash> = (0..self.events.len()).filter(|&i| !has_child[i]).map(|i| self.events[i].hash).collect();
        h.sort();
        h
    }

    /// Validates a content event (message, edit, tombstone, reaction) by its causal control past
    /// and every valid removal outside that past. Returns the verdict and the acting principals.
    pub fn validate_content(&mut self, author: DeviceKey, hlc: u64, control_parents: &[EventHash], epoch: u64) -> (Verdict, Vec<(Principal, Role)>) {
        let st = self.state_at(control_parents);
        if st.owner.is_none() {
            return (Verdict::Rejected(Reason::NoGenesis), Vec::new());
        }
        if epoch == 0 {
            return (Verdict::Rejected(Reason::BadEpoch), Vec::new());
        }
        if st.revoked.contains(&author) {
            return (Verdict::Rejected(Reason::DeviceRevoked), Vec::new());
        }
        let acting = st.acting(&author);
        match acting.iter().map(|(_, r)| *r).max() {
            None => return (Verdict::Rejected(Reason::NotMember), acting),
            Some(r) if r < Role::Member => return (Verdict::Rejected(Reason::InsufficientRole), acting),
            _ => {}
        }
        let past = self.closure(control_parents);
        let cutters: Vec<&CtlEvent> = (0..self.events.len())
            .filter(|&r| !past.contains(r) && self.verdicts[r].is_valid() && is_cutting(&self.events[r]))
            .map(|r| &self.events[r])
            .collect();
        if let Some(reason) = cut_off_by(author, hlc, &st, cutters.into_iter()) {
            return (Verdict::Rejected(reason), acting);
        }
        (Verdict::Valid, acting)
    }

    /// Who the author acts for if only its causal past is considered. Deletions and edits use
    /// this: purging a payload cannot be undone, so it must not depend on removals that arrive
    /// later.
    pub fn causal_acting(&mut self, author: DeviceKey, control_parents: &[EventHash], epoch: u64) -> Vec<(Principal, Role)> {
        let st = self.state_at(control_parents);
        if st.owner.is_none() || epoch == 0 || st.revoked.contains(&author) {
            return Vec::new();
        }
        let acting = st.acting(&author);
        if acting.iter().map(|(_, r)| *r).max() < Some(Role::Member) {
            return Vec::new();
        }
        acting
    }

    /// Valid removals of an agent that are outside a set of control parents, as (host cutoffs).
    pub fn agent_removed_outside(&mut self, agent: &AgentId, control_parents: &[EventHash], author: DeviceKey, hlc: u64) -> bool {
        let past = self.closure(control_parents);
        (0..self.events.len()).any(|r| {
            if past.contains(r) || !self.verdicts[r].is_valid() {
                return false;
            }
            match &self.events[r].body {
                Some(Control::MemberRemove(p)) if p.subject == Principal::Agent(*agent) => {
                    p.cutoffs.iter().find(|(d, _)| *d == author).is_none_or(|(_, c)| hlc > *c)
                }
                _ => false,
            }
        })
    }
}

fn is_cutting(e: &CtlEvent) -> bool {
    matches!(e.body, Some(Control::MemberRemove(_)) | Some(Control::DeviceRevoke(_)))
}

fn cut_off_by<'a>(author: DeviceKey, hlc: u64, st: &MembershipState, cutters: impl Iterator<Item = &'a CtlEvent>) -> Option<Reason> {
    for r in cutters {
        match &r.body {
            Some(Control::MemberRemove(p)) => {
                let Principal::User(u) = p.subject else { continue };
                let acts_for = st.certs.get(&author).is_some_and(|users| users.contains_key(&u));
                if !acts_for {
                    continue;
                }
                match p.cutoffs.iter().find(|(d, _)| *d == author) {
                    Some((_, c)) if hlc <= *c => {}
                    _ => return Some(Reason::RemovedConcurrently),
                }
            }
            Some(Control::DeviceRevoke(p)) if p.device == author => match p.cutoff {
                Some(c) if hlc <= c => {}
                _ => return Some(Reason::DeviceRevoked),
            },
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests;
