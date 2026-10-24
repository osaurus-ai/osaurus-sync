//! Fans core changes out to foreign listeners on one dedicated thread.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};

use osaurus_sync_core::ids::ScopeId;
use osaurus_sync_core::sync::Notice;
use osaurus_sync_core::types::Change;

use crate::types::{ConversationChange, CoreEvent, ScopeChange, stream_update};
use crate::{ConversationListener, CoreListener, ScopeListener};

enum Ev {
    Changes(Vec<Change>),
    Notice(Notice),
}

#[derive(Default)]
struct Subs {
    next: u64,
    core: Option<Arc<dyn CoreListener>>,
    scopes: HashMap<u64, (ScopeId, Arc<dyn ScopeListener>)>,
    convs: HashMap<u64, Arc<dyn ConversationListener>>,
}

pub(crate) struct Hub {
    tx: Mutex<Option<mpsc::Sender<Ev>>>,
    subs: Mutex<Subs>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Hub {
    pub(crate) fn start() -> Arc<Hub> {
        let (tx, rx) = mpsc::channel::<Ev>();
        let hub = Arc::new(Hub { tx: Mutex::new(Some(tx)), subs: Mutex::default() });
        let weak = Arc::downgrade(&hub);
        std::thread::Builder::new()
            .name("osaurus-sync-callbacks".into())
            .spawn(move || {
                while let Ok(ev) = rx.recv() {
                    let Some(hub) = weak.upgrade() else { break };
                    hub.deliver(ev);
                }
            })
            .expect("spawn callback thread");
        hub
    }

    pub(crate) fn stop(&self) {
        lock(&self.tx).take();
    }

    fn send(&self, ev: Ev) {
        if let Some(tx) = lock(&self.tx).as_ref() {
            let _ = tx.send(ev);
        }
    }

    pub(crate) fn set_core(&self, l: Option<Box<dyn CoreListener>>) {
        lock(&self.subs).core = l.map(Arc::from);
    }

    pub(crate) fn subscribe_scope(self: &Arc<Self>, scope: ScopeId, l: Box<dyn ScopeListener>) -> Arc<Subscription> {
        let mut s = lock(&self.subs);
        s.next += 1;
        let id = s.next;
        s.scopes.insert(id, (scope, Arc::from(l)));
        Arc::new(Subscription { hub: Arc::downgrade(self), id })
    }

    pub(crate) fn subscribe_conversations(self: &Arc<Self>, l: Box<dyn ConversationListener>) -> Arc<Subscription> {
        let mut s = lock(&self.subs);
        s.next += 1;
        let id = s.next;
        s.convs.insert(id, Arc::from(l));
        Arc::new(Subscription { hub: Arc::downgrade(self), id })
    }

    fn cancel(&self, id: u64) {
        let mut s = lock(&self.subs);
        s.scopes.remove(&id);
        s.convs.remove(&id);
    }

    fn deliver(&self, ev: Ev) {
        // Listeners are cloned out so callbacks may subscribe or cancel re-entrantly.
        let (core, scopes, convs) = {
            let s = lock(&self.subs);
            (s.core.clone(), s.scopes.values().cloned().collect::<Vec<_>>(), s.convs.values().cloned().collect::<Vec<_>>())
        };
        let changes = match ev {
            Ev::Notice(n) => {
                if let Some(c) = core {
                    c.on_event(n.into());
                }
                return;
            }
            Ev::Changes(c) => c,
        };
        let mut per_scope: HashMap<ScopeId, Vec<ScopeChange>> = HashMap::new();
        let mut conv_changes = Vec::new();
        let mut core_events = Vec::new();
        for c in changes {
            match c {
                Change::MessageAdded(m) => per_scope.entry(m.scope).or_default().push(ScopeChange::MessageAdded { message: m.into() }),
                Change::MessageChanged(m) => per_scope.entry(m.scope).or_default().push(ScopeChange::MessageChanged { message: m.into() }),
                Change::MessageRemoved { scope, id } => per_scope.entry(scope).or_default().push(ScopeChange::MessageRemoved { id: id.to_hex() }),
                Change::ScopeReset(scope) => per_scope.entry(scope).or_default().push(ScopeChange::Reset),
                Change::Stream(u) => per_scope.entry(u.scope).or_default().push(ScopeChange::Stream { update: stream_update(u) }),
                Change::SyncStatusChanged(scope) => per_scope.entry(scope).or_default().push(ScopeChange::SyncStatusChanged),
                Change::ConversationChanged(c) => conv_changes.push(ConversationChange::Changed { conversation: c.into() }),
                Change::ConversationRemoved(s) => conv_changes.push(ConversationChange::Removed { scope: s.to_hex() }),
                Change::DevicesChanged => core_events.push(CoreEvent::DevicesChanged),
                Change::SelfRevoked => core_events.push(CoreEvent::SelfRevoked),
                Change::IdentityChanged => core_events.push(CoreEvent::IdentityChanged),
                Change::RecoveryAvailable => core_events.push(CoreEvent::RecoveryAvailable),
            }
        }
        for (scope, l) in scopes {
            if let Some(v) = per_scope.get(&scope) {
                l.on_changes(v.clone());
            }
        }
        if !conv_changes.is_empty() {
            for l in convs {
                l.on_changes(conv_changes.clone());
            }
        }
        if let Some(c) = core {
            core_events.dedup();
            for e in core_events {
                c.on_event(e);
            }
        }
    }
}

impl osaurus_sync_net::Listener for Hub {
    fn changes(&self, changes: Vec<Change>) {
        self.send(Ev::Changes(changes));
    }
    fn notice(&self, notice: Notice) {
        self.send(Ev::Notice(notice));
    }
}

/// Live subscription handle. Dropping it cancels.
#[derive(uniffi::Object)]
pub struct Subscription {
    hub: Weak<Hub>,
    id: u64,
}

#[uniffi::export]
impl Subscription {
    pub fn cancel(&self) {
        if let Some(h) = self.hub.upgrade() {
            h.cancel(self.id);
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}
