//! Routes the core's `tracing` events to the app, and keeps recent ones for support bundles.
//!
//! Logging is process-wide: one listener serves every open core. Records reach the listener on
//! a dedicated thread, in order. If the app falls behind, records are dropped and counted rather
//! than slowing sync down. Debug and trace records come only from the sync crates; other crates
//! (networking) contribute warnings and errors.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Once, OnceLock};

use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, uniffi::Enum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn of(l: &Level) -> Self {
        match *l {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            Level::DEBUG => LogLevel::Debug,
            Level::TRACE => LogLevel::Trace,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct LogRecord {
    pub level: LogLevel,
    /// The Rust module that logged, such as `osaurus_sync_core::sync::engine`.
    pub target: String,
    pub message: String,
    /// Structured fields, formatted. Device and scope ids appear as their first 12 hex digits.
    /// Message content, titles and keys are never logged.
    pub fields: HashMap<String, String>,
    pub time_ms: u64,
}

#[uniffi::export(callback_interface)]
pub trait LogListener: Send + Sync {
    fn on_log(&self, record: LogRecord);
}

/// Receives log records at `level` and more severe, or stops delivery with `None`. Records
/// arrive on a dedicated thread; keep the callback cheap.
#[uniffi::export]
pub fn set_log_listener(level: LogLevel, listener: Option<Box<dyn LogListener>>) {
    let logs = logs();
    let on = listener.is_some();
    *lock(&logs.listener) = listener.map(Arc::from);
    logs.listener_level.store(if on { level as u8 + 1 } else { 0 }, Ordering::Release);
    install();
    tracing::callsite::rebuild_interest_cache();
}

/// Records kept for support bundles, regardless of the listener.
const RECENT: usize = 500;
const RECENT_LEVEL: LogLevel = LogLevel::Info;
const QUEUE: usize = 4096;

struct Logs {
    /// The listener's level as `LogLevel as u8 + 1`, or 0 without a listener.
    listener_level: AtomicU8,
    listener: Mutex<Option<Arc<dyn LogListener>>>,
    tx: SyncSender<LogRecord>,
    recent: Mutex<VecDeque<LogRecord>>,
    dropped: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn logs() -> &'static Logs {
    static LOGS: OnceLock<Logs> = OnceLock::new();
    LOGS.get_or_init(|| {
        let (tx, rx) = sync_channel::<LogRecord>(QUEUE);
        std::thread::Builder::new()
            .name("osaurus-sync-logs".into())
            .spawn(move || {
                while let Ok(rec) = rx.recv() {
                    let l = lock(&logs().listener).clone();
                    if let Some(l) = l {
                        l.on_log(rec);
                    }
                }
            })
            .expect("spawn log thread");
        Logs { listener_level: AtomicU8::new(0), listener: Mutex::new(None), tx, recent: Mutex::new(VecDeque::new()), dropped: AtomicU64::new(0) }
    })
}

/// Installs the process-wide subscriber, unless the host already installed one.
pub(crate) fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        logs();
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(AppLayer));
    });
}

pub(crate) fn recent() -> Vec<LogRecord> {
    lock(&logs().recent).iter().cloned().collect()
}

pub(crate) fn dropped() -> u64 {
    logs().dropped.load(Ordering::Relaxed)
}

impl Logs {
    fn listener_level(&self) -> Option<LogLevel> {
        match self.listener_level.load(Ordering::Acquire) {
            0 => None,
            n => [LogLevel::Error, LogLevel::Warn, LogLevel::Info, LogLevel::Debug, LogLevel::Trace].get(n as usize - 1).copied(),
        }
    }

    /// The most verbose level anything wants.
    fn max_level(&self) -> LogLevel {
        self.listener_level().map_or(RECENT_LEVEL, |l| l.max(RECENT_LEVEL))
    }

    fn wants(&self, m: &Metadata<'_>, level: LogLevel) -> bool {
        let ours = m.target().starts_with("osaurus_sync");
        (ours || level <= LogLevel::Warn) && level <= self.max_level()
    }

    fn record(&self, e: &Event<'_>) {
        let m = e.metadata();
        let level = LogLevel::of(m.level());
        let mut v = Fields::default();
        e.record(&mut v);
        let time_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let rec = LogRecord { level, target: m.target().to_string(), message: v.message, fields: v.fields, time_ms };
        if level <= RECENT_LEVEL {
            let mut r = lock(&self.recent);
            if r.len() >= RECENT {
                r.pop_front();
            }
            r.push_back(rec.clone());
        }
        if self.listener_level().is_some_and(|l| level <= l) {
            match self.tx.try_send(rec) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    fields: HashMap<String, String>,
}

impl Visit for Fields {
    fn record_str(&mut self, f: &Field, v: &str) {
        if f.name() == "message" {
            self.message = v.to_string();
        } else {
            self.fields.insert(f.name().to_string(), v.to_string());
        }
    }

    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        let mut s = String::new();
        let _ = write!(s, "{v:?}");
        if f.name() == "message" {
            self.message = s;
        } else {
            self.fields.insert(f.name().to_string(), s);
        }
    }
}

struct AppLayer;

impl<S: Subscriber> Layer<S> for AppLayer {
    fn register_callsite(&self, m: &'static Metadata<'static>) -> Interest {
        if m.is_event() && logs().wants(m, LogLevel::of(m.level())) { Interest::always() } else { Interest::never() }
    }

    fn enabled(&self, m: &Metadata<'_>, _: Context<'_, S>) -> bool {
        m.is_event() && logs().wants(m, LogLevel::of(m.level()))
    }

    fn on_event(&self, e: &Event<'_>, _: Context<'_, S>) {
        logs().record(e);
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(match logs().max_level() {
            LogLevel::Error => LevelFilter::ERROR,
            LogLevel::Warn => LevelFilter::WARN,
            LogLevel::Info => LevelFilter::INFO,
            LogLevel::Debug => LevelFilter::DEBUG,
            LogLevel::Trace => LevelFilter::TRACE,
        })
    }
}

/// Replaces identifiers and network addresses in support bundle text. The same id maps to the
/// same token within one bundle, so records can still be correlated.
pub(crate) struct Redactor {
    salt: [u8; 32],
}

impl Redactor {
    pub(crate) fn new(salt: [u8; 32]) -> Self {
        Redactor { salt }
    }

    /// A token for an id, from its first 12 hex digits (the form logs use).
    pub(crate) fn id(&self, hex: &str) -> String {
        let prefix: String = hex.chars().take(12).collect::<String>().to_ascii_lowercase();
        let h = blake3::keyed_hash(&self.salt, prefix.as_bytes());
        format!("id:{}", &h.to_hex()[..8])
    }

    pub(crate) fn text(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_hexdigit() && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
                let end = i + b[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
                let run = &s[i..end];
                let bounded = end == b.len() || !b[end].is_ascii_alphanumeric();
                // An id's 12-digit prefix or full 64 digits can be all decimal, so those lengths
                // are ids even without a letter. Other all-digit runs are numbers (hlc, sizes).
                let id_like = run.len() >= 12 && run.bytes().any(|c| c.is_ascii_alphabetic());
                if bounded && (id_like || run.len() == 12 || run.len() == 64) {
                    out.push_str(&self.id(run));
                    i = end;
                    continue;
                }
                if let Some(len) = ipv4_len(&b[i..]) {
                    out.push_str("<addr>");
                    i += len;
                    continue;
                }
            }
            if c == b'['
                && let Some(len) = ipv6_len(&b[i..])
            {
                out.push_str("<addr>");
                i += len;
                continue;
            }
            let ch = s[i..].chars().next().expect("in bounds");
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }
}

/// Length of a dotted IPv4 address with an optional port at the start of `b`.
fn ipv4_len(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    for part in 0..4 {
        let n = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 || n > 3 {
            return None;
        }
        i += n;
        if part < 3 {
            if b.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
    }
    if b.get(i) == Some(&b':') {
        let n = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n > 0 {
            i += 1 + n;
        }
    }
    Some(i)
}

/// Length of a bracketed IPv6 address with an optional port at the start of `b`.
fn ipv6_len(b: &[u8]) -> Option<usize> {
    let close = b.iter().position(|c| *c == b']')?;
    let inner = &b[1..close];
    let (addr, zone) = match inner.iter().position(|c| *c == b'%') {
        Some(p) => (&inner[..p], &inner[p + 1..]),
        None => (inner, &[][..]),
    };
    if addr.len() < 2
        || !addr.contains(&b':')
        || !addr.iter().all(|c| c.is_ascii_hexdigit() || *c == b':' || *c == b'.')
        || !zone.iter().all(u8::is_ascii_alphanumeric)
    {
        return None;
    }
    let mut i = close + 1;
    if b.get(i) == Some(&b':') {
        let n = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n > 0 {
            i += 1 + n;
        }
    }
    Some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_ids_and_addresses() {
        let r = Redactor::new([7; 32]);
        let id = r.id("01977420dd83");
        let s = r.text("peer=DeviceKey(01977420dd83) at 192.168.1.20:4433 and [fe80::1%en0]:7 hlc 114688026523664384");
        assert_eq!(s, format!("peer=DeviceKey({id}) at <addr> and <addr> hlc 114688026523664384"));
        // A full id redacts to the same token as its prefix.
        assert_eq!(r.id("01977420dd83ffffffffffffffff"), id);
        assert_eq!(r.text("version 1.2.3, café"), "version 1.2.3, café");
        let digits = r.id("019774201234");
        assert_eq!(r.text("scope 019774201234 at 1767225600000"), format!("scope {digits} at 1767225600000"));
    }
}
