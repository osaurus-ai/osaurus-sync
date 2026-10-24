//! Network performance budgets, over real QUIC on loopback:
//!
//! | Operation | Budget |
//! | --- | --- |
//! | Catch-up of 1,000 events on a LAN | under 1 s |
//! | Live delivery to a connected peer | one round trip plus 10 ms |
//!
//! Catch-up is timed from resuming a suspended device to the last message reaching its view, so
//! it includes reconnecting. Each benchmark prints its median against the budget. With
//! `OSAURUS_BUDGETS=enforce` a median over budget fails the run.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use osaurus_sync_core::env::SystemEnv;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::EventId;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::MemorySecretStore;
use osaurus_sync_core::sync::Notice;
use osaurus_sync_core::types::{Change, NewMessage};
use osaurus_sync_core::{Replica, ReplicaConfig};
use osaurus_sync_net::{Listener, NetConfig, SyncNode};
use tokio::runtime::Runtime;

const TIMEOUT: Duration = Duration::from_secs(30);

fn check_budget(name: &str, limit: Duration, samples: &mut [Duration]) {
    if samples.is_empty() {
        return;
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    let ok = median <= limit;
    eprintln!("budget {name}: median {median:?}, limit {limit:?}: {}", if ok { "ok" } else { "OVER BUDGET" });
    assert!(ok || std::env::var("OSAURUS_BUDGETS").as_deref() != Ok("enforce"), "{name} is over budget");
}

/// Forwards added message ids and pairing notices to the benchmark thread.
struct Feed {
    added: Mutex<Sender<EventId>>,
    paired: Mutex<Sender<()>>,
}

impl Listener for Feed {
    fn changes(&self, changes: Vec<Change>) {
        let tx = self.added.lock().unwrap();
        for c in changes {
            if let Change::MessageAdded(m) = c {
                let _ = tx.send(m.id);
            }
        }
    }
    fn notice(&self, n: Notice) {
        if matches!(n, Notice::Paired { .. }) {
            let _ = self.paired.lock().unwrap().send(());
        }
    }
}

struct Pair {
    rt: Runtime,
    mac: SyncNode,
    pc: SyncNode,
    pc_added: Receiver<EventId>,
    _dir: tempfile::TempDir,
}

fn node(rt: &Runtime, dir: &std::path::Path, name: &str, platform: Platform) -> (SyncNode, Receiver<EventId>, Receiver<()>) {
    let r = Replica::open(ReplicaConfig::new(dir.join(name), name, platform), Arc::new(SystemEnv), Arc::new(MemorySecretStore::default())).unwrap();
    let (added_tx, added) = channel();
    let (paired_tx, paired) = channel();
    let feed = Arc::new(Feed { added: Mutex::new(added_tx), paired: Mutex::new(paired_tx) });
    let n = rt.block_on(SyncNode::start(r, NetConfig::local(), feed)).unwrap();
    (n, added, paired)
}

fn paired() -> Pair {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (mac, _, _) = node(&rt, dir.path(), "mac", Platform::MacOs);
    let (pc, pc_added, pc_paired) = node(&rt, dir.path(), "pc", Platform::Windows);
    mac.add_peer_addr(pc.addr().unwrap());
    pc.add_peer_addr(mac.addr().unwrap());
    let invite = mac.start_pairing(300_000);
    rt.block_on(pc.pair(&invite.encode())).unwrap();
    pc_paired.recv_timeout(TIMEOUT).expect("pairing");
    Pair { rt, mac, pc, pc_added, _dir: dir }
}

fn wait_for(rx: &Receiver<EventId>, id: EventId) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if rx.recv_timeout(left).expect("message did not arrive") == id {
            return;
        }
    }
}

fn catch_up(c: &mut Criterion) {
    let p = paired();
    let mut samples = Vec::new();
    let mut group = c.benchmark_group("catch-up");
    group.sample_size(10).measurement_time(Duration::from_secs(30));
    group.bench_function("1000 events on loopback", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                p.rt.block_on(p.pc.suspend()).unwrap();
                let scope = p.mac.write(|r| r.create_scope(ScopeKind::Conversation, "Catch-up")).unwrap();
                let mut last = None;
                for i in 0..999 {
                    last = Some(p.mac.write(|r| r.send_message(scope, NewMessage::text(MessageRole::User, format!("message {i}")))).unwrap());
                }
                while p.pc_added.try_recv().is_ok() {}

                let t = Instant::now();
                p.rt.block_on(p.pc.resume()).unwrap();
                p.pc.add_peer_addr(p.mac.addr().unwrap());
                p.mac.add_peer_addr(p.pc.addr().unwrap());
                p.pc.sync_now();
                wait_for(&p.pc_added, last.unwrap());
                let d = t.elapsed();
                samples.push(d);
                total += d;
            }
            total
        })
    });
    group.finish();
    check_budget("catch-up of 1,000 events", Duration::from_secs(1), &mut samples);
}

fn live_delivery(c: &mut Criterion) {
    let p = paired();
    let scope = p.mac.write(|r| r.create_scope(ScopeKind::Conversation, "Live")).unwrap();
    let warm = p.mac.write(|r| r.send_message(scope, NewMessage::text(MessageRole::User, "warm up"))).unwrap();
    wait_for(&p.pc_added, warm);
    let mut samples = Vec::new();
    let mut group = c.benchmark_group("live delivery");
    group.sample_size(50);
    group.bench_function("connected peer", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let t = Instant::now();
                let id = p.mac.write(|r| r.send_message(scope, NewMessage::text(MessageRole::User, "ping"))).unwrap();
                wait_for(&p.pc_added, id);
                let d = t.elapsed();
                samples.push(d);
                total += d;
            }
            total
        })
    });
    group.finish();
    // Loopback round trips are well under a millisecond.
    check_budget("live delivery to a connected peer", Duration::from_millis(11), &mut samples);
}

criterion_group!(benches, catch_up, live_delivery);
criterion_main!(benches);
