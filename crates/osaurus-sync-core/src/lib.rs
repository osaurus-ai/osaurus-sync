//! Osaurus sync core: an append-only, signed, encrypted event log per scope, stored in SQLite,
//! merged with Loro, and reconciled between peers by a sans-I/O engine.

pub mod auth;
pub mod blobs;
pub mod crypto;
pub mod env;
pub mod envelope;
pub mod error;
pub mod hlc;
pub mod identity;
pub mod ids;
pub mod keys;
pub mod payload;
pub mod replica;
pub mod store;
pub mod sync;
pub mod types;

pub use error::{Error, Result};
pub use replica::{Replica, ReplicaConfig, SecretStore};
