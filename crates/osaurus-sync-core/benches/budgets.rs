//! Local performance budgets:
//!
//! | Operation | Budget |
//! | --- | --- |
//! | Local send to view update | under 5 ms |
//! | Open a conversation, first 50 messages | under 20 ms |
//! | Cold start with 100 scopes, to the first view | under 300 ms |
//!
//! Each benchmark prints its median against the budget. With `OSAURUS_BUDGETS=enforce` a median
//! over budget fails the run.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use osaurus_sync_core::env::SystemEnv;
use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::ScopeId;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::MemorySecretStore;
use osaurus_sync_core::types::{Change, NewMessage};
use osaurus_sync_core::{Replica, ReplicaConfig};

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

fn open(dir: &Path, secrets: &Arc<MemorySecretStore>) -> Replica {
    Replica::open(ReplicaConfig::new(dir, "bench", Platform::MacOs), Arc::new(SystemEnv), secrets.clone()).unwrap()
}

fn fill(r: &mut Replica, scope: ScopeId, n: usize) {
    for i in 0..n {
        let role = if i % 2 == 0 { MessageRole::User } else { MessageRole::Assistant };
        r.send_message(scope, NewMessage::text(role, format!("message {i}: the quick brown fox jumps over the lazy dog"))).unwrap();
    }
    r.take_changes();
}

fn local_send(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let mut r = open(dir.path(), &secrets);
    let scope = r.create_scope(ScopeKind::Conversation, "Bench").unwrap();
    fill(&mut r, scope, 1_000);
    let mut samples = Vec::new();
    c.bench_function("local send to view update", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let t = Instant::now();
                r.send_message(scope, NewMessage::text(MessageRole::User, "hello")).unwrap();
                let changes = r.take_changes();
                let d = t.elapsed();
                assert!(changes.iter().any(|c| matches!(c, Change::MessageAdded(_))));
                samples.push(d);
                total += d;
            }
            total
        })
    });
    check_budget("local send to view update", Duration::from_millis(5), &mut samples);
}

fn open_conversation(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let mut r = open(dir.path(), &secrets);
    let scope = r.create_scope(ScopeKind::Conversation, "Long").unwrap();
    fill(&mut r, scope, 10_000);
    let mut samples = Vec::new();
    c.bench_function("open conversation, first 50 messages", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let t = Instant::now();
                let page = r.messages(scope, None, 50).unwrap();
                let d = t.elapsed();
                assert_eq!(page.messages.len(), 50);
                samples.push(d);
                total += d;
            }
            total
        })
    });
    check_budget("open conversation, first 50 messages", Duration::from_millis(20), &mut samples);
}

fn cold_start(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    {
        let mut r = open(dir.path(), &secrets);
        for i in 0..100 {
            let scope = r.create_scope(ScopeKind::Conversation, &format!("Conversation {i}")).unwrap();
            fill(&mut r, scope, 20);
        }
        r.close().unwrap();
    }
    let mut samples = Vec::new();
    let mut group = c.benchmark_group("cold start");
    group.sample_size(20);
    group.bench_function("100 scopes to first view", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let t = Instant::now();
                let r = open(dir.path(), &secrets);
                let list = r.conversations().unwrap();
                let first = r.messages(list[0].scope, None, 50).unwrap();
                let d = t.elapsed();
                assert_eq!(list.len(), 100);
                assert_eq!(first.messages.len(), 20);
                drop(r);
                samples.push(d);
                total += d;
            }
            total
        })
    });
    group.finish();
    check_budget("cold start with 100 scopes", Duration::from_millis(300), &mut samples);
}

criterion_group!(benches, local_send, open_conversation, cold_start);
criterion_main!(benches);
