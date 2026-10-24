use thiserror::Error;

/// Closed set of core errors. Each variant has a stable numeric code that both apps map to fixed copy.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("malformed event: {0}")]
    Malformed(String),
    #[error("data directory is already open in another instance")]
    AlreadyOpen,
    #[error("this device has been revoked")]
    DeviceRevoked,
    #[error("no key available for this scope")]
    NoKey,
    #[error("network error: {0}")]
    Network(String),
    #[error("incompatible API version: core {core}, bindings {bindings}")]
    IncompatibleVersion { core: u32, bindings: u32 },
    #[error("pairing failed: {0}")]
    Pairing(String),
    #[error("the core is closed")]
    Closed,
    /// The OS key store failed, for example a Keychain read before the device's first unlock.
    #[error("key store error: {0}")]
    KeyStore(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// The database failed its integrity check and couldn't be repaired from the event log.
    #[error("the database is corrupt: {0}")]
    Corrupt(String),
    /// The data directory was written by a newer release; opening it would lose data.
    #[error("the database is from a newer release (store version {found}, this release supports {supported})")]
    NewerDatabase { found: i64, supported: i64 },
}

impl Error {
    /// Stable code. Never renumber; only append.
    pub fn code(&self) -> u32 {
        match self {
            Error::Storage(_) => 100,
            Error::InvalidInput(_) => 101,
            Error::NotFound(_) => 102,
            Error::PermissionDenied(_) => 103,
            Error::Crypto(_) => 104,
            Error::Malformed(_) => 105,
            Error::AlreadyOpen => 106,
            Error::DeviceRevoked => 107,
            Error::NoKey => 108,
            Error::Network(_) => 109,
            Error::IncompatibleVersion { .. } => 110,
            Error::Pairing(_) => 111,
            Error::Closed => 112,
            Error::KeyStore(_) => 113,
            Error::Internal(_) => 199,
            Error::Corrupt(_) => 114,
            Error::NewerDatabase { .. } => 115,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode;
        match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => Error::Corrupt(e.to_string()),
            _ => Error::Storage(e.to_string()),
        }
    }
}

impl From<postcard::Error> for Error {
    fn from(e: postcard::Error) -> Self {
        Error::Malformed(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Storage(e.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
