//! Background WAL checkpoints.
//!
//! SQLite's automatic checkpoint runs inside whichever commit crosses the threshold, and
//! copying the log into the database ends in an fsync. Behind the replica's lock that stalls
//! every local write during a large catch-up. A passive checkpoint from a second connection
//! doesn't block writers, so callers that hold the replica behind a lock run this instead, and
//! the automatic checkpoint stays only as a backstop.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

use super::Replica;
use crate::error::Result;

/// Pages the write-ahead log may reach before a commit checkpoints it inline, once a
/// `Checkpointer` is in use (about 40 MB).
const BACKSTOP_PAGES: i64 = 10_000;

#[derive(Clone)]
pub struct Checkpointer(Arc<Gate>);

struct Gate {
    path: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    conn: Option<Connection>,
    closed: bool,
}

impl Checkpointer {
    /// Copies committed log frames into the database without waiting for readers or writers.
    /// Call it every second or so from a thread that doesn't hold the replica. Does nothing
    /// once the replica is closed.
    pub fn run(&self) -> Result<()> {
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed {
            return Ok(());
        }
        if st.conn.is_none() {
            let conn = Connection::open_with_flags(&self.0.path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
            st.conn = Some(conn);
        }
        let conn = st.conn.as_ref().expect("just opened");
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        Ok(())
    }

    pub(super) fn new(path: PathBuf) -> Self {
        Checkpointer(Arc::new(Gate { path, state: Mutex::new(State::default()) }))
    }

    /// Closes the second connection for good, so the database files can be deleted.
    pub(super) fn close(&self) {
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        st.closed = true;
        st.conn = None;
    }
}

impl Replica {
    /// A handle for running checkpoints off the replica's thread. From the first call, commits
    /// leave checkpointing to it unless the log grows past a backstop.
    pub fn checkpointer(&mut self) -> Result<Checkpointer> {
        self.db.pragma_update(None, "wal_autocheckpoint", BACKSTOP_PAGES)?;
        Ok(self.checkpointer.clone())
    }
}
