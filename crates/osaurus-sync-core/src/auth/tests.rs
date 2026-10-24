use super::*;
use crate::env::TestEnv;
use crate::identity::Platform;
use crate::keys::{DeviceSecret, UserSecret};
use crate::payload::{GenesisPayload, MemberAddPayload, MemberRemovePayload, RoleChangePayload, ScopeKind};

struct Actor {
    user: UserSecret,
    device: DeviceSecret,
}

impl Actor {
    fn new(env: &TestEnv) -> Self {
        Actor { user: UserSecret::generate(env), device: DeviceSecret::generate(env) }
    }
    fn principal(&self) -> Principal {
        Principal::User(self.user.public())
    }
    fn cert(&self) -> DeviceCert {
        DeviceCert::issue(&self.user, self.device.public(), "dev", Platform::MacOs, 0)
    }
}

struct World {
    auth: ScopeAuth,
    n: u8,
}

impl World {
    fn ev(&mut self, author: &Actor, hlc: u64, parents: &[EventHash], body: Control) -> EventHash {
        self.n += 1;
        let hash = EventHash([self.n; 32]);
        let kind = body.kind();
        let id = if matches!(body, Control::Genesis(_)) { EventId(self.auth.scope.0) } else { EventId([self.n; 16]) };
        self.auth.add(CtlEvent {
            hash,
            id,
            author: author.device.public(),
            hlc,
            kind,
            epoch: 0,
            control_parents: parents.to_vec(),
            body: Some(body),
            malformed: false,
        });
        hash
    }

    fn v(&self, h: EventHash) -> Verdict {
        self.auth.verdict(&h).unwrap()
    }
}

fn setup() -> (TestEnv, World, Actor, EventHash) {
    let env = TestEnv::new(77, 0);
    let owner = Actor::new(&env);
    let mut w = World { auth: ScopeAuth::new(ScopeId([9; 16])), n: 0 };
    let g = w.ev(
        &owner,
        1,
        &[],
        Control::Genesis(GenesisPayload { owner: owner.user.public(), owner_device: owner.cert(), scope_kind: ScopeKind::Workspace, created_ms: 0 }),
    );
    (env, w, owner, g)
}

fn add(p: Principal, role: Role) -> Control {
    Control::MemberAdd(MemberAddPayload { subject: p, role, invite: None })
}

#[test]
fn genesis_makes_owner() {
    let (_env, mut w, owner, g) = setup();
    assert_eq!(w.v(g), Verdict::Valid);
    let st = w.auth.full_state();
    assert_eq!(st.members.get(&owner.principal()), Some(&Role::Owner));
    assert_eq!(st.max_role(&owner.device.public()), Some(Role::Owner));
}

#[test]
fn genesis_with_wrong_id_is_rejected() {
    let env = TestEnv::new(1, 0);
    let owner = Actor::new(&env);
    let mut auth = ScopeAuth::new(ScopeId([9; 16]));
    let out = auth.add(CtlEvent {
        hash: EventHash([1; 32]),
        id: EventId([1; 16]),
        author: owner.device.public(),
        hlc: 1,
        kind: kind::GENESIS,
        epoch: 0,
        control_parents: vec![],
        body: Some(Control::Genesis(GenesisPayload {
            owner: owner.user.public(),
            owner_device: owner.cert(),
            scope_kind: ScopeKind::Conversation,
            created_ms: 0,
        })),
        malformed: false,
    });
    assert_eq!(out.verdict, Some(Verdict::Rejected(Reason::InvalidPayload)));
}

#[test]
fn member_needs_cert_to_act_and_cannot_add() {
    let (env, mut w, owner, g) = setup();
    let alice = Actor::new(&env);
    let a = w.ev(&owner, 2, &[g], add(alice.principal(), Role::Member));
    assert_eq!(w.v(a), Verdict::Valid);
    // Without a certificate the device acts for nobody.
    let bob = Actor::new(&env);
    let early = w.ev(&alice, 3, &[a], add(bob.principal(), Role::Viewer));
    assert_eq!(w.v(early), Verdict::Rejected(Reason::NotMember));
    let c = w.ev(&alice, 4, &[a], Control::DeviceCert(alice.cert()));
    assert_eq!(w.v(c), Verdict::Valid);
    let st = w.auth.state_at(&[c]);
    assert_eq!(st.max_role(&alice.device.public()), Some(Role::Member));
    let add_bob = w.ev(&alice, 5, &[c], add(bob.principal(), Role::Viewer));
    assert_eq!(w.v(add_bob), Verdict::Rejected(Reason::InsufficientRole));
}

#[test]
fn admin_limits() {
    let (env, mut w, owner, g) = setup();
    let admin = Actor::new(&env);
    let a = w.ev(&owner, 2, &[g], add(admin.principal(), Role::Admin));
    let c = w.ev(&admin, 3, &[a], Control::DeviceCert(admin.cert()));
    let x = Actor::new(&env);
    let ok = w.ev(&admin, 4, &[c], add(x.principal(), Role::Member));
    assert_eq!(w.v(ok), Verdict::Valid);
    let bad = w.ev(&admin, 5, &[ok], add(Actor::new(&env).principal(), Role::Admin));
    assert_eq!(w.v(bad), Verdict::Rejected(Reason::InsufficientRole));
    let rm_owner = w.ev(&admin, 6, &[bad], Control::MemberRemove(MemberRemovePayload { subject: owner.principal(), cutoffs: vec![] }));
    assert_eq!(w.v(rm_owner), Verdict::Rejected(Reason::InsufficientRole));
}

#[test]
fn offline_posts_concurrent_with_removal_are_rejected() {
    let (env, mut w, owner, g) = setup();
    let alice = Actor::new(&env);
    let a = w.ev(&owner, 2, &[g], add(alice.principal(), Role::Admin));
    let c = w.ev(&alice, 3, &[a], Control::DeviceCert(alice.cert()));
    // Alice (offline) adds Bob at hlc 10; the owner removes Alice having seen her events up to hlc 3.
    let bob = Actor::new(&env);
    let offline = w.ev(&alice, 10, &[c], add(bob.principal(), Role::Member));
    assert_eq!(w.v(offline), Verdict::Valid);
    let rm = w.ev(&owner, 5, &[c], Control::MemberRemove(MemberRemovePayload { subject: alice.principal(), cutoffs: vec![(alice.device.public(), 3)] }));
    assert_eq!(w.v(rm), Verdict::Valid);
    assert_eq!(w.v(offline), Verdict::Rejected(Reason::RemovedConcurrently));
    // Alice's event before the cutoff stays valid.
    assert_eq!(w.v(c), Verdict::Valid);
    // Content from Alice concurrent with the removal is rejected too.
    let (v, _) = w.auth.validate_content(alice.device.public(), 11, &[c], 1);
    assert_eq!(v, Verdict::Rejected(Reason::RemovedConcurrently));
    let st = w.auth.full_state();
    assert!(!st.members.contains_key(&alice.principal()));
    assert!(!st.members.contains_key(&bob.principal()));
}

#[test]
fn removal_wins_over_concurrent_role_change() {
    let (env, mut w, owner, g) = setup();
    let owner2 = Actor::new(&env);
    let a = w.ev(&owner, 2, &[g], add(owner2.principal(), Role::Owner));
    let c = w.ev(&owner2, 3, &[a], Control::DeviceCert(owner2.cert()));
    let alice = Actor::new(&env);
    let b = w.ev(&owner, 4, &[c], add(alice.principal(), Role::Member));
    let promote = w.ev(&owner, 5, &[b], Control::RoleChange(RoleChangePayload { subject: alice.principal(), role: Role::Admin }));
    let remove = w.ev(&owner2, 6, &[b], Control::MemberRemove(MemberRemovePayload { subject: alice.principal(), cutoffs: vec![] }));
    assert!(w.v(promote).is_valid() && w.v(remove).is_valid());
    assert!(!w.auth.full_state().members.contains_key(&alice.principal()));
}

#[test]
fn higher_role_wins_concurrent_role_changes() {
    let (env, mut w, owner, g) = setup();
    let owner2 = Actor::new(&env);
    let a = w.ev(&owner, 2, &[g], add(owner2.principal(), Role::Owner));
    let c = w.ev(&owner2, 3, &[a], Control::DeviceCert(owner2.cert()));
    let alice = Actor::new(&env);
    let b = w.ev(&owner, 4, &[c], add(alice.principal(), Role::Member));
    w.ev(&owner, 5, &[b], Control::RoleChange(RoleChangePayload { subject: alice.principal(), role: Role::Viewer }));
    w.ev(&owner2, 6, &[b], Control::RoleChange(RoleChangePayload { subject: alice.principal(), role: Role::Admin }));
    assert_eq!(w.auth.full_state().members.get(&alice.principal()), Some(&Role::Admin));
}

#[test]
fn mutual_removal_earlier_wins() {
    let (env, mut w, owner, g) = setup();
    let owner2 = Actor::new(&env);
    let a = w.ev(&owner, 2, &[g], add(owner2.principal(), Role::Owner));
    let c = w.ev(&owner2, 3, &[a], Control::DeviceCert(owner2.cert()));
    let r1 = w.ev(&owner, 10, &[c], Control::MemberRemove(MemberRemovePayload { subject: owner2.principal(), cutoffs: vec![(owner2.device.public(), 3)] }));
    let r2 = w.ev(&owner2, 11, &[c], Control::MemberRemove(MemberRemovePayload { subject: owner.principal(), cutoffs: vec![(owner.device.public(), 2)] }));
    assert_eq!(w.v(r1), Verdict::Valid);
    assert_eq!(w.v(r2), Verdict::Rejected(Reason::RemovedConcurrently));
    let st = w.auth.full_state();
    assert!(st.members.contains_key(&owner.principal()));
    assert!(!st.members.contains_key(&owner2.principal()));
}

#[test]
fn revoked_device_cannot_post_after_cutoff() {
    let (env, mut w, owner, g) = setup();
    let second = Actor { user: owner.user.clone(), device: DeviceSecret::generate(&env) };
    let c = w.ev(&owner, 2, &[g], Control::DeviceCert(second.cert()));
    let rv = w.ev(&owner, 5, &[c], Control::DeviceRevoke(crate::payload::DeviceRevokePayload { device: second.device.public(), cutoff: Some(3) }));
    assert!(w.v(rv).is_valid());
    let (before, _) = w.auth.validate_content(second.device.public(), 3, &[c], 1);
    assert_eq!(before, Verdict::Valid);
    let (concurrent, _) = w.auth.validate_content(second.device.public(), 4, &[c], 1);
    assert_eq!(concurrent, Verdict::Rejected(Reason::DeviceRevoked));
    let (after, _) = w.auth.validate_content(second.device.public(), 6, &[rv], 1);
    assert_eq!(after, Verdict::Rejected(Reason::DeviceRevoked));
}

#[test]
fn verdicts_independent_of_insertion_order() {
    let env = TestEnv::new(5, 0);
    let owner = Actor::new(&env);
    let alice = Actor::new(&env);
    let bob = Actor::new(&env);
    let mk = |n: u8, author: &Actor, hlc: u64, parents: Vec<EventHash>, body: Control| CtlEvent {
        hash: EventHash([n; 32]),
        id: if matches!(body, Control::Genesis(_)) { EventId([9; 16]) } else { EventId([n; 16]) },
        author: author.device.public(),
        hlc,
        kind: body.kind(),
        epoch: 0,
        control_parents: parents,
        body: Some(body),
        malformed: false,
    };
    let g = EventHash([1; 32]);
    let evs = [
        mk(
            1,
            &owner,
            1,
            vec![],
            Control::Genesis(GenesisPayload { owner: owner.user.public(), owner_device: owner.cert(), scope_kind: ScopeKind::Workspace, created_ms: 0 }),
        ),
        mk(2, &owner, 2, vec![g], add(alice.principal(), Role::Admin)),
        mk(3, &alice, 3, vec![EventHash([2; 32])], Control::DeviceCert(alice.cert())),
        mk(4, &alice, 9, vec![EventHash([3; 32])], add(bob.principal(), Role::Member)),
        mk(
            5,
            &owner,
            5,
            vec![EventHash([3; 32])],
            Control::MemberRemove(MemberRemovePayload { subject: alice.principal(), cutoffs: vec![(alice.device.public(), 3)] }),
        ),
    ];
    let run = |order: &[usize]| {
        let mut a = ScopeAuth::new(ScopeId([9; 16]));
        let mut pending: Vec<CtlEvent> = order.iter().map(|&i| evs[i].clone()).collect();
        while !pending.is_empty() {
            let pos = pending.iter().position(|e| a.missing_parents(&e.control_parents).is_empty()).unwrap();
            a.add(pending.remove(pos));
        }
        (1..=5u8).map(|n| a.verdict(&EventHash([n; 32])).unwrap()).collect::<Vec<_>>()
    };
    let base = run(&[0, 1, 2, 3, 4]);
    assert_eq!(base, run(&[0, 1, 2, 4, 3]));
    assert_eq!(base, run(&[4, 3, 2, 1, 0]));
    assert_eq!(base[3], Verdict::Rejected(Reason::RemovedConcurrently));
}
