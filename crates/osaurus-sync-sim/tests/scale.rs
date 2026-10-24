//! Scale tests, run nightly in release mode:
//!
//! ```text
//! cargo test -p osaurus-sync-sim --release --test scale -- --ignored --nocapture --test-threads=1
//! ```
//!
//! They print metrics (and append them to `$GITHUB_STEP_SUMMARY` when set) and assert loose
//! bounds that catch regressions in kind, not in degree. `OSAURUS_SCALE=0.05` shrinks every
//! count for a quick local run.

use std::io::Write;
use std::time::{Duration, Instant};

use osaurus_sync_core::identity::Platform;
use osaurus_sync_core::ids::ScopeId;
use osaurus_sync_core::payload::{MessageRole, ScopeKind};
use osaurus_sync_core::replica::Replica;
use osaurus_sync_core::types::{Attachment, NewMessage};
use osaurus_sync_sim::Sim;

fn scaled(n: usize) -> usize {
    let f: f64 = std::env::var("OSAURUS_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    ((n as f64 * f).round() as usize).max(1)
}

/// Resident memory of this process, in MiB.
fn rss_mib() -> Option<u64> {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().ok()?;
    String::from_utf8(out.stdout).ok()?.trim().parse::<u64>().ok().map(|kib| kib / 1024)
}

fn metric(name: &str, value: String) {
    println!("scale metric: {name}: {value}");
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY")
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path)
    {
        let _ = writeln!(f, "| {name} | {value} |");
    }
}

fn header(title: &str) {
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY")
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path)
    {
        let _ = writeln!(f, "\n### {title}\n\n| Metric | Value |\n| --- | --- |");
    }
}

fn text(s: String) -> NewMessage {
    NewMessage::text(MessageRole::User, s)
}

/// Time from opening a replica to having the conversation list and the newest page of one
/// conversation, as an app's first screen needs.
fn time_to_first_view(sim: &mut Sim, i: usize, scope: ScopeId) -> Duration {
    sim.disconnect_all(i);
    let n = &mut sim.nodes[i];
    n.replica.take().unwrap().close().unwrap();
    let t = Instant::now();
    let r = Replica::open(n.config(), n.env.clone(), n.secrets.clone()).unwrap();
    let list = r.conversations().unwrap();
    let page = r.messages(scope, None, 50).unwrap();
    let d = t.elapsed();
    assert!(!list.is_empty() && !page.messages.is_empty());
    n.replica = Some(r);
    d
}

#[test]
#[ignore = "nightly scale test"]
fn many_messages_scopes_and_devices() {
    let scopes = scaled(1_000);
    let messages = scaled(100_000);
    let devices = 10;
    header(&format!("{messages} messages in {scopes} scopes across {devices} devices"));
    let mut sim = Sim::new(7);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let others: Vec<usize> = (1..devices).map(|k| sim.add_node(&format!("Device {k}"), Platform::Windows)).collect();
    for &d in &others {
        sim.pair(mac, d);
        sim.run(1_000_000);
    }
    for &d in &others {
        sim.disconnect_all(d);
    }

    let t = Instant::now();
    let ids: Vec<ScopeId> = (0..scopes).map(|k| sim.r(mac).create_scope(ScopeKind::Conversation, &format!("Conversation {k}")).unwrap()).collect();
    for k in 0..messages {
        let scope = ids[k % scopes];
        sim.r(mac).send_message(scope, text(format!("message {k} with a little text to index"))).unwrap();
    }
    sim.poll(mac);
    metric("write all messages on one device", format!("{:.1?}", t.elapsed()));
    metric("memory after writing", format!("{} MiB", rss_mib().unwrap_or(0)));

    let t = Instant::now();
    sim.heal(&[]);
    sim.run(usize::MAX);
    let sync = t.elapsed();
    metric("sync to 9 devices", format!("{sync:.1?}"));
    metric("memory after sync", format!("{} MiB", rss_mib().unwrap_or(0)));
    for &d in &others {
        assert_eq!(sim.r(d).conversations().unwrap().len(), scopes, "device {d} is missing conversations");
    }
    let total: usize = ids.iter().map(|s| sim.r(others[0]).messages(*s, None, u32::MAX).unwrap().messages.len()).sum();
    assert_eq!(total, messages);

    let first = time_to_first_view(&mut sim, others[0], ids[0]);
    metric("time to first view after restart", format!("{first:.1?}"));
    let t = Instant::now();
    let hits = sim.r(others[0]).search("little", 50).unwrap();
    metric("search across all messages", format!("{:.1?}", t.elapsed()));
    assert_eq!(hits.len(), 50);
    let usage = sim.r(others[0]).storage_usage().unwrap();
    metric("database on a synced device", format!("{} MiB", usage.database_bytes >> 20));
    assert!(first < Duration::from_secs(5), "first view took {first:?}");
}

#[test]
#[ignore = "nightly scale test"]
fn a_gigabyte_of_attachments() {
    const FILE: usize = 4 << 20;
    let files = scaled(256);
    header(&format!("{} MiB of attachments", (files * FILE) >> 20));
    let mut sim = Sim::new(8);
    let mac = sim.add_node("Mac", Platform::MacOs);
    let scopes: Vec<ScopeId> = (0..8).map(|k| sim.r(mac).create_scope(ScopeKind::Conversation, &format!("Files {k}")).unwrap()).collect();
    let src = tempfile::tempdir().unwrap();
    let path = src.path().join("upload.bin");
    let mut buf = vec![0u8; FILE];
    let t = Instant::now();
    for k in 0..files {
        // Distinct content per file, cheap to produce.
        for (j, chunk) in buf.chunks_mut(8).enumerate() {
            chunk.copy_from_slice(&((k as u64) << 32 | j as u64).to_le_bytes()[..chunk.len()]);
        }
        std::fs::write(&path, &buf).unwrap();
        let mut m = text(format!("file {k}"));
        m.attachments.push(Attachment { path: path.clone(), media_type: "application/octet-stream".into(), name: format!("file-{k}.bin") });
        sim.r(mac).send_message(scopes[k % scopes.len()], m).unwrap();
    }
    metric("import attachments", format!("{:.1?}", t.elapsed()));
    metric("memory after import", format!("{} MiB", rss_mib().unwrap_or(0)));
    let usage = sim.r(mac).storage_usage().unwrap();
    metric("encrypted attachments on disk", format!("{} MiB", usage.blob_bytes >> 20));
    assert!(usage.blob_bytes >= (files * FILE) as u64);

    // Open every attachment, as scrolling through them would; the cache stays under its cap.
    let t = Instant::now();
    for &scope in &scopes {
        for m in sim.r(mac).messages(scope, None, u32::MAX).unwrap().messages {
            for b in m.blobs {
                sim.r(mac).blob_path(scope, &b.hash).unwrap().expect("attachment is local");
            }
        }
    }
    metric("open every attachment", format!("{:.1?}", t.elapsed()));
    sim.r(mac).maintain().unwrap();
    let usage = sim.r(mac).storage_usage().unwrap();
    metric("decrypted cache after opening all", format!("{} MiB", usage.attachment_cache_bytes >> 20));
    assert!(usage.attachment_cache_bytes <= 512 << 20);
    metric("memory after opening all", format!("{} MiB", rss_mib().unwrap_or(0)));

    let out = tempfile::tempdir().unwrap();
    let t = Instant::now();
    let summary = sim.r(mac).export_conversation(scopes[0], out.path()).unwrap();
    metric("export one conversation", format!("{:.1?} for {} attachments", t.elapsed(), summary.attachments));
    assert_eq!(summary.missing_attachments, 0);
}
