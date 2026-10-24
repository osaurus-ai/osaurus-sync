//! Randomized runs that assert the spec's four invariants:
//!
//! 1. All peers with the same events produce identical views.
//! 2. No accepted event is ever lost, through any sequence of partitions and restarts.
//! 3. No event from a non-member or revoked device is applied.
//! 4. Applying events in any order produces the same result as applying them in causal order.
//!
//! `SIM_SEEDS=500 cargo test -p osaurus-sync-sim --release` runs more seeds.

use std::collections::BTreeSet;

use osaurus_sync_core::Replica;
use osaurus_sync_core::error::Error;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::{EventId, Principal, Role, ScopeId};
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::IngestOutcome;
use osaurus_sync_core::types::{DeliveryStatus, NewMessage};
use osaurus_sync_sim::{Sim, snapshot, verdicts};
use rand_core::RngCore;

const ALICE_MAC: usize = 0;
const ALICE_PC: usize = 1;
const BOB: usize = 2;
const CAROL: usize = 3;

fn tolerate<T>(r: osaurus_sync_core::Result<T>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(Error::PermissionDenied(_) | Error::NotFound(_) | Error::NoKey | Error::InvalidInput(_)) => None,
        Err(e) => panic!("unexpected error: {e}"),
    }
}

fn my_scopes(r: &mut Replica) -> Vec<ScopeId> {
    let me = r.device_key();
    r.scope_ids().unwrap().into_iter().filter(|s| r.is_authorized(*s, &me).unwrap()).collect()
}

fn my_messages(r: &Replica, scope: ScopeId) -> Vec<EventId> {
    let me = r.me();
    r.messages(scope, None, 100_000)
        .unwrap()
        .messages
        .into_iter()
        .filter(|m| m.author == me && m.status != DeliveryStatus::Rejected(osaurus_sync_core::auth::Reason::NotMember))
        .map(|m| m.id)
        .collect()
}

struct Run {
    sim: Sim,
    workspace: ScopeId,
    /// Messages that must never be applied anywhere but on their author.
    forbidden: Vec<EventId>,
    revoked: Vec<usize>,
    removed: Vec<usize>,
}

fn setup(seed: u64) -> Run {
    let mut sim = Sim::new(seed);
    let skew = |sim: &mut Sim| (sim.rng.next_u32() % 4000) as i64 - 2000;
    for (name, p) in [("Alice Mac", Platform::MacOs), ("Alice PC", Platform::Windows), ("Bob", Platform::Windows), ("Carol", Platform::MacOs)] {
        let s = skew(&mut sim);
        sim.add_node_with(name, p, None, s);
    }
    // A storing peer for Alice, so other devices compact during the run.
    let server = sim.add_storage_node("Alice Server");
    sim.pair(ALICE_MAC, ALICE_PC);
    sim.run(100_000);
    sim.pair(ALICE_MAC, server);
    sim.run(100_000);
    let workspace = sim.r(ALICE_MAC).create_scope(ScopeKind::Workspace, "Team").unwrap();
    sim.r(ALICE_MAC).create_scope(ScopeKind::Conversation, "Private").unwrap();
    sim.poll(ALICE_MAC);
    sim.invite(ALICE_MAC, BOB, workspace, Role::Member);
    sim.invite(ALICE_MAC, CAROL, workspace, Role::Member);
    sim.run(100_000);
    for n in [BOB, CAROL] {
        let d = sim.nodes[n].device();
        assert!(sim.r(n).is_authorized(workspace, &d).unwrap(), "seed {seed}: invite failed");
    }
    Run { sim, workspace, forbidden: vec![], revoked: vec![], removed: vec![] }
}

fn random_step(run: &mut Run, step: usize) {
    let sim = &mut run.sim;
    let advance = sim.rand(5_000) as u64;
    sim.advance(advance);
    let running = sim.running();
    let actor = running[sim.rand(running.len())];
    let roll = sim.rand(100);
    let scopes = my_scopes(sim.r(actor));
    let pick_scope = |sim: &mut Sim, scopes: &[ScopeId]| if scopes.is_empty() { None } else { Some(scopes[sim.rand(scopes.len())]) };
    let removed_or_revoked = run.removed.contains(&actor) || run.revoked.contains(&actor);
    match roll {
        0..=29 => {
            if let Some(s) = pick_scope(sim, &scopes) {
                let body = format!("m{step} from {}", sim.nodes[actor].name);
                if let Some(id) = tolerate(sim.r(actor).send_message(s, NewMessage::text(MessageRole::User, body)))
                    && (removed_or_revoked && s == run.workspace || run.revoked.contains(&actor))
                {
                    run.forbidden.push(id);
                }
            }
        }
        30..=37 => {
            if let Some(s) = pick_scope(sim, &scopes) {
                let mine = my_messages(sim.r(actor), s);
                if !mine.is_empty() {
                    let id = mine[sim.rand(mine.len())];
                    tolerate(sim.r(actor).edit_message(id, &format!("edited at {step}")));
                }
            }
        }
        38..=41 => {
            if let Some(s) = pick_scope(sim, &scopes) {
                let mine = my_messages(sim.r(actor), s);
                if !mine.is_empty() {
                    let id = mine[sim.rand(mine.len())];
                    tolerate(sim.r(actor).delete_message(id));
                }
            }
        }
        42..=49 => {
            if let Some(s) = pick_scope(sim, &scopes) {
                let all = sim.r(actor).messages(s, None, 100).unwrap().messages;
                if !all.is_empty() {
                    let m = all[sim.rand(all.len())].id;
                    let emoji = ["👍", "🎉", "❤️"][sim.rand(3)];
                    if sim.chance(70) {
                        tolerate(sim.r(actor).add_reaction(m, emoji));
                    } else {
                        tolerate(sim.r(actor).remove_reaction(m, emoji));
                    }
                }
            }
        }
        50..=54 => {
            if let Some(s) = pick_scope(sim, &scopes) {
                tolerate(sim.r(actor).rename_conversation(s, &format!("title {step}")));
            }
        }
        55..=56 => {
            if let Some(s) = pick_scope(sim, &scopes) {
                let v = sim.chance(50);
                tolerate(sim.r(actor).set_pinned(s, v));
            }
        }
        57..=69 => {
            let a = running[sim.rand(running.len())];
            let b = running[sim.rand(running.len())];
            if a != b && !run.revoked.contains(&a) && !run.revoked.contains(&b) {
                if sim.is_linked(a, b) { sim.disconnect(a, b) } else { sim.connect(a, b) }
            }
        }
        70..=73 => sim.restart(actor),
        _ => {
            let n = 1 + sim.rand(40);
            sim.run_some(n);
        }
    }
    if sim.nodes[actor].replica.is_some() {
        sim.poll(actor);
    }
}

fn membership_events(run: &mut Run, step: usize, total: usize) {
    let sim = &mut run.sim;
    if step == total / 3 && sim.chance(60) {
        // Alice removes Carol, possibly while Carol is partitioned and still posting.
        let carol = sim.r(CAROL).user_key();
        if tolerate(sim.r(ALICE_MAC).remove_member(run.workspace, Principal::User(carol))).is_some() {
            run.removed.push(CAROL);
            sim.poll(ALICE_MAC);
        }
    }
    if step == total / 2 && sim.chance(40) {
        let pc = sim.nodes[ALICE_PC].device();
        sim.disconnect_all(ALICE_PC);
        if tolerate(sim.r(ALICE_MAC).revoke_device(pc)).is_some() {
            run.revoked.push(ALICE_PC);
            sim.poll(ALICE_MAC);
        }
    }
    if step == 2 * total / 3 && sim.chance(50) {
        let bob = sim.r(BOB).user_key();
        tolerate(sim.r(ALICE_MAC).change_role(run.workspace, Principal::User(bob), Role::Admin));
        sim.poll(ALICE_MAC);
    }
}

/// Nodes that may read `scope` at the end, by their own account.
fn final_members(sim: &mut Sim, scope: ScopeId, exclude: &[usize]) -> Vec<usize> {
    sim.running()
        .into_iter()
        .filter(|i| !exclude.contains(i))
        .filter(|i| {
            let d = sim.nodes[*i].device();
            let r = sim.r(*i);
            r.scope_ids().unwrap().contains(&scope) && r.is_authorized(scope, &d).unwrap()
        })
        .collect()
}

/// Returns how many events the members compacted.
fn check(run: &mut Run, seed: u64) -> u32 {
    let sim = &mut run.sim;
    let excluded = run.revoked.clone();
    sim.heal(&excluded);
    sim.run(2_000_000);
    // A second round from fresh sessions must find nothing new.
    sim.heal(&excluded);
    sim.run(2_000_000);

    let scopes: BTreeSet<ScopeId> = sim.running().into_iter().flat_map(|i| sim.r(i).scope_ids().unwrap()).collect();
    let mut compacted = 0;
    for scope in scopes {
        let members = final_members(sim, scope, &excluded);
        if members.is_empty() {
            continue;
        }
        for &m in &members {
            compacted += sim.r(m).compacted_events(scope).unwrap();
        }
        // 1. Same events, same views.
        let base = members[0];
        let base_hashes = sim.r(base).all_hashes(scope).unwrap();
        let base_snap = snapshot(sim.r(base), scope);
        let base_verdicts = verdicts(sim.r(base), scope);
        for &m in &members[1..] {
            let other = sim.r(m).all_hashes(scope).unwrap();
            if other != base_hashes {
                let missing: Vec<_> = other.iter().filter(|h| !base_hashes.contains(h)).copied().collect();
                let extra: Vec<_> = base_hashes.iter().filter(|h| !other.contains(h)).copied().collect();
                let evs = sim.r(m).events_by_hashes(&missing).unwrap();
                let held_b = sim.r(base).held_missing(scope).unwrap();
                let outcomes: Vec<_> = evs.iter().take(5).map(|e| sim.r(base).ingest(e).unwrap()).collect();
                panic!(
                    "seed {seed}: event sets differ between {base} and {m}: base lacks {}, other lacks {}; removed {:?} revoked {:?}; base held-missing {}; linked {}; outcomes {:?}",
                    missing.len(),
                    extra.len(),
                    run.removed,
                    run.revoked,
                    held_b.len(),
                    sim.is_linked(base, m),
                    outcomes
                );
            }
            assert_eq!(verdicts(sim.r(m), scope), base_verdicts, "seed {seed}: verdicts differ between {base} and {m}");
            assert_eq!(snapshot(sim.r(m), scope), base_snap, "seed {seed}: views differ between {base} and {m}");
        }
        // 2. Nothing a member ever stored is missing now.
        let have: BTreeSet<_> = base_hashes.iter().copied().collect();
        for &m in &members {
            if let Some(ever) = sim.nodes[m].ever_stored.get(&scope) {
                let lost: Vec<_> = ever.difference(&have).collect();
                assert!(lost.is_empty(), "seed {seed}: node {m} lost {} events in a scope", lost.len());
            }
        }
        // 3. Messages from removed or revoked devices are applied nowhere but on their author.
        for &m in &members {
            for id in &run.forbidden {
                if let Some(v) = sim.r(m).message_view(id).unwrap() {
                    assert!(matches!(v.status, DeliveryStatus::Rejected(_)), "seed {seed}: forbidden message applied on node {m}");
                }
            }
        }
        // 4. Causal order and random order give the same result.
        let (mut causal, _d1) = sim.clone_identity(base);
        let (mut shuffled, _d2) = sim.clone_identity(base);
        let src = sim.r(base);
        let ids = |control| src.recon_keys(scope, control, 0, u64::MAX).unwrap().into_iter().map(|k| k.1).collect::<Vec<_>>();
        let mut ordered = src.events_by_ids(scope, &ids(true)).unwrap();
        ordered.extend(src.events_by_ids(scope, &ids(false)).unwrap());
        let mut random = ordered.clone();
        for i in (1..random.len()).rev() {
            let j = sim.rand(i + 1);
            random.swap(i, j);
        }
        causal.join_scope(scope).unwrap();
        shuffled.join_scope(scope).unwrap();
        for e in &ordered {
            assert!(!matches!(causal.ingest(e).unwrap(), IngestOutcome::Invalid(_)));
        }
        for e in &random {
            shuffled.ingest(e).unwrap();
        }
        let a = snapshot(&causal, scope);
        let b = snapshot(&shuffled, scope);
        assert_eq!(a, b, "seed {seed}: delivery order changed the view");
        assert_eq!(a, base_snap, "seed {seed}: rebuilt view differs from the live one");
        assert_eq!(verdicts(&mut shuffled, scope), base_verdicts, "seed {seed}: delivery order changed verdicts");
    }
    compacted
}

fn run_seed(seed: u64, steps: usize) -> u32 {
    let mut run = setup(seed);
    for step in 0..steps {
        membership_events(&mut run, step, steps);
        random_step(&mut run, step);
    }
    check(&mut run, seed)
}

#[test]
fn invariants_hold_across_seeds() {
    let seeds: u64 = std::env::var("SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(12);
    let start: u64 = std::env::var("SIM_SEED_START").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut compacted = 0;
    for seed in start..start + seeds {
        compacted += run_seed(seed, 160);
    }
    assert!(compacted > 0, "no run compacted anything");
}

#[test]
fn same_seed_same_outcome() {
    let digest = |seed| {
        let mut run = setup(seed);
        for step in 0..60 {
            random_step(&mut run, step);
        }
        run.sim.heal(&[]);
        run.sim.run(1_000_000);
        let ws = run.workspace;
        snapshot(run.sim.r(ALICE_MAC), ws).map(|s| s.messages.len())
    };
    assert_eq!(digest(99), digest(99));
}
