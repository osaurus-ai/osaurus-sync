//! Ephemeral streaming tokens. Never stored; the committed message replaces them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use osaurus_sync_core::ids::{AgentId, DeviceKey, EventId, ScopeId};
use osaurus_sync_core::sync::proto::{MAX_FRAME, StreamFrame};
use osaurus_sync_core::types::{Change, StreamState, StreamUpdate};
use tokio::sync::mpsc;
use tracing::debug;

use crate::{ALPN_STREAM, Inner, Listener, device_of, lock};

const DONE_TTL: Duration = Duration::from_secs(120);

#[derive(Default)]
pub(crate) struct Incoming {
    active: Mutex<HashMap<EventId, (u32, StreamUpdate)>>,
    done: Mutex<HashMap<EventId, Instant>>,
    out: Mutex<HashMap<DeviceKey, mpsc::UnboundedSender<StreamFrame>>>,
}

fn state_of(s: u8) -> Option<StreamState> {
    match s {
        StreamFrame::STREAMING => Some(StreamState::Streaming),
        StreamFrame::FINISHED => Some(StreamState::Finished),
        StreamFrame::INTERRUPTED => Some(StreamState::Interrupted),
        _ => None,
    }
}

fn code_of(s: StreamState) -> u8 {
    match s {
        StreamState::Streaming => StreamFrame::STREAMING,
        StreamState::Finished => StreamFrame::FINISHED,
        StreamState::Interrupted => StreamFrame::INTERRUPTED,
    }
}

impl Incoming {
    /// Orders frames by sequence number and drops stale or repeated ones.
    fn accept(&self, host: DeviceKey, f: StreamFrame) -> Option<StreamUpdate> {
        let state = state_of(f.state)?;
        {
            let mut done = lock(&self.done);
            done.retain(|_, at| at.elapsed() < DONE_TTL);
            if done.contains_key(&f.message_id) {
                return None;
            }
        }
        let mut active = lock(&self.active);
        if let Some((seq, u)) = active.get(&f.message_id)
            && (u.host != host || f.seq <= *seq)
        {
            return None;
        }
        let u = StreamUpdate { scope: f.scope, message_id: f.message_id, host, as_agent: f.as_agent, text: f.text, state };
        if state == StreamState::Streaming {
            active.insert(f.message_id, (f.seq, u.clone()));
        } else {
            active.remove(&f.message_id);
            lock(&self.done).insert(f.message_id, Instant::now());
        }
        Some(u)
    }

    fn interrupt(&self, filter: impl Fn(&StreamUpdate) -> bool, listener: &dyn Listener) {
        let ended: Vec<StreamUpdate> = {
            let mut active = lock(&self.active);
            let ids: Vec<EventId> = active.iter().filter(|(_, (_, u))| filter(u)).map(|(id, _)| *id).collect();
            ids.into_iter().filter_map(|id| active.remove(&id)).map(|(_, u)| u).collect()
        };
        if ended.is_empty() {
            return;
        }
        let mut done = lock(&self.done);
        let changes = ended
            .into_iter()
            .map(|mut u| {
                done.insert(u.message_id, Instant::now());
                u.state = StreamState::Interrupted;
                Change::Stream(u)
            })
            .collect();
        drop(done);
        listener.changes(changes);
    }

    pub(crate) fn interrupt_peer(&self, peer: &DeviceKey, listener: &dyn Listener) {
        self.interrupt(|u| u.host == *peer, listener);
    }

    pub(crate) fn interrupt_all(&self, listener: &dyn Listener) {
        lock(&self.out).clear();
        self.interrupt(|_| true, listener);
    }
}

impl Inner {
    pub(crate) fn send_stream(self: Arc<Self>, scope: ScopeId, message_id: EventId, as_agent: Option<AgentId>, seq: u32, text: &str, state: StreamState) {
        let connected: Vec<DeviceKey> = lock(&self.peers).keys().copied().collect();
        let targets: Vec<DeviceKey> = {
            let mut st = lock(&self.state);
            connected.into_iter().filter(|d| st.replica.is_authorized(scope, d).unwrap_or(false)).collect()
        };
        let frame = StreamFrame { scope, message_id, as_agent, seq, text: text.to_string(), state: code_of(state) };
        for peer in targets {
            let mut out = lock(&self.streams.out);
            if let Some(tx) = out.get(&peer)
                && tx.send(frame.clone()).is_ok()
            {
                continue;
            }
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(frame.clone());
            out.insert(peer, tx);
            self.rt.spawn(self.clone().stream_writer(peer, rx));
        }
    }

    async fn stream_writer(self: Arc<Self>, peer: DeviceKey, mut rx: mpsc::UnboundedReceiver<StreamFrame>) {
        let res: anyhow::Result<()> = async {
            let conn = self.connect(&peer, ALPN_STREAM).await?;
            while let Some(first) = rx.recv().await {
                // Coalesce a backlog: only the latest frame per message matters, plus any
                // terminal frame.
                let mut batch = vec![first];
                while let Ok(f) = rx.try_recv() {
                    batch.push(f);
                }
                let mut latest: HashMap<EventId, StreamFrame> = HashMap::new();
                let mut order = Vec::new();
                for f in batch {
                    if !latest.contains_key(&f.message_id) {
                        order.push(f.message_id);
                    }
                    let keep = latest.get(&f.message_id).is_some_and(|old| old.state != StreamFrame::STREAMING);
                    if !keep {
                        latest.insert(f.message_id, f);
                    }
                }
                for id in order {
                    let f = latest.remove(&id).expect("present");
                    let mut send = conn.open_uni().await?;
                    send.write_all(&f.encode()).await?;
                    send.finish()?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            debug!(?peer, "stream writer ended: {e:#}");
        }
        lock(&self.streams.out).remove(&peer);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StreamProtocol(pub Arc<Inner>);

impl ProtocolHandler for StreamProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let inner = &self.0;
        let peer = device_of(conn.remote_id());
        let mut allowed: HashMap<(ScopeId, Option<AgentId>), bool> = HashMap::new();
        while let Ok(mut recv) = conn.accept_uni().await {
            let Ok(bytes) = recv.read_to_end(MAX_FRAME).await else { break };
            let Ok(f) = StreamFrame::decode(&bytes) else { continue };
            let key = (f.scope, f.as_agent);
            let ok = *allowed.entry(key).or_insert_with(|| {
                let mut st = lock(&inner.state);
                let r = &mut st.replica;
                r.is_authorized(f.scope, &peer).unwrap_or(false)
                    && match &f.as_agent {
                        Some(a) => r.agent_host(f.scope, a).ok().flatten() == Some(peer),
                        None => true,
                    }
            });
            if !ok {
                continue;
            }
            if let Some(u) = inner.streams.accept(peer, f) {
                inner.listener.changes(vec![Change::Stream(u)]);
            }
        }
        inner.streams.interrupt_peer(&peer, &*inner.listener);
        Ok(())
    }
}
