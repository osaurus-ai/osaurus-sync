//! Network performance budgets, over real QUIC on loopback:
//!
//! | Operation | Budget |
//! | --- | --- |
//! | Catch-up of 1,000 events on a LAN | under 1 s |
//! | Live delivery to a connected peer | one round trip plus 10 ms |
//! | Local send while catching up 10,000 events | under 5 ms at the 95th percentile |
//!
//! Catch-up is timed from resuming a suspended device to the last message reaching its view, so
//! it includes reconnecting. Each benchmark prints its median against the budget. With
//! `OSAURUS_BUDGETS=enforce` a median over budget fails the run; `OSAURUS_BUDGET_SLACK`
//! multiplies every budget (CI uses 3).
//!
//! When a message does not arrive, the run fails with both nodes' diagnostics and their recent
//! network log.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
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
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

const TIMEOUT: Duration = Duration::from_secs(30);
const LOG_LINES: usize = 400;

/// The last `LOG_LINES` debug records from the network crate and info records from the rest.
fn recent_log() -> &'static Mutex<VecDeque<String>> {
    static LOG: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    LOG.get_or_init(Default::default)
}

struct Recent;

impl<S: Subscriber> Layer<S> for Recent {
    fn enabled(&self, m: &tracing::Metadata<'_>, _: Context<'_, S>) -> bool {
        m.is_event() && (*m.level() <= Level::INFO || m.target().starts_with("osaurus_sync_net"))
    }

    fn on_event(&self, e: &Event<'_>, _: Context<'_, S>) {
        struct Fields(String);
        impl Visit for Fields {
            fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
                let _ = write!(self.0, " {}={v:?}", f.name());
            }
        }
        let m = e.metadata();
        let mut line = Fields(format!("{:?} {} {}:", std::time::SystemTime::now(), m.level(), m.target()));
        e.record(&mut line);
        let mut log = recent_log().lock().unwrap();
        if log.len() == LOG_LINES {
            log.pop_front();
        }
        log.push_back(line.0);
    }
}

fn check_budget(name: &str, limit: Duration, samples: &mut [Duration]) {
    check_percentile(name, 50, limit, samples);
}

fn check_percentile(name: &str, pct: usize, limit: Duration, samples: &mut [Duration]) {
    if samples.is_empty() {
        return;
    }
    samples.sort();
    let median = samples[(samples.len() * pct / 100).min(samples.len() - 1)];
    // Shared CI runners are noisy; they enforce a multiple of the budget, and nightly the budget.
    let slack: f64 = std::env::var("OSAURUS_BUDGET_SLACK").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let limit = limit.mul_f64(slack.max(1.0));
    let ok = median <= limit;
    let what = if pct == 50 { "median".to_string() } else { format!("p{pct}") };
    eprintln!("budget {name}: {what} {median:?} of {} samples, limit {limit:?}: {}", samples.len(), if ok { "ok" } else { "OVER BUDGET" });
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
    let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Recent));
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

fn wait_for(p: &Pair, id: EventId) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match p.pc_added.recv_timeout(left) {
            Ok(got) if got == id => return,
            Ok(_) => {}
            Err(_) => {
                let log: Vec<String> = recent_log().lock().unwrap().iter().cloned().collect();
                panic!("message did not arrive\nmac: {:#?}\npc: {:#?}\nrecent log:\n{}", p.mac.diagnostics(), p.pc.diagnostics(), log.join("\n"));
            }
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
                wait_for(&p, last.unwrap());
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
    wait_for(&p, warm);
    let mut samples = Vec::new();
    let mut group = c.benchmark_group("live delivery");
    group.sample_size(50);
    group.bench_function("connected peer", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let t = Instant::now();
                let id = p.mac.write(|r| r.send_message(scope, NewMessage::text(MessageRole::User, "ping"))).unwrap();
                wait_for(&p, id);
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

/// Local sends on a device while it applies a large catch-up. Runs once rather than under
/// criterion's sampling: the catch-up itself is the setup.
fn send_during_catch_up(_: &mut Criterion) {
    let p = paired();
    let local = p.pc.write(|r| r.create_scope(ScopeKind::Conversation, "Local")).unwrap();
    p.rt.block_on(p.pc.suspend()).unwrap();
    let scope = p.mac.write(|r| r.create_scope(ScopeKind::Conversation, "Big")).unwrap();
    let mut last = None;
    for i in 0..10_000 {
        last = Some(p.mac.write(|r| r.send_message(scope, NewMessage::text(MessageRole::User, format!("message {i}")))).unwrap());
    }
    while p.pc_added.try_recv().is_ok() {}
    let last = last.unwrap();

    p.rt.block_on(p.pc.resume()).unwrap();
    p.pc.add_peer_addr(p.mac.addr().unwrap());
    p.mac.add_peer_addr(p.pc.addr().unwrap());
    p.pc.sync_now();
    let mut samples = Vec::new();
    let deadline = Instant::now() + TIMEOUT * 4;
    let mut done = false;
    while !done {
        assert!(Instant::now() < deadline, "catch-up did not finish");
        let t = Instant::now();
        p.pc.write(|r| r.send_message(local, NewMessage::text(MessageRole::User, "typing"))).unwrap();
        samples.push(t.elapsed());
        while let Ok(id) = p.pc_added.try_recv() {
            done |= id == last;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    check_percentile("local send while catching up 10,000 events", 95, Duration::from_millis(5), &mut samples);
}

criterion_group!(benches, catch_up, live_delivery, send_during_catch_up);
criterion_main!(benches);
