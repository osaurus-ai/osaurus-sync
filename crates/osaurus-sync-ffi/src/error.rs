use osaurus_sync_core::Error as CoreError;

/// Closed error set. `code` is stable across releases; apps map it to fixed copy and use
/// `detail` only for logs.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SyncError {
    #[error("storage error: {detail}")]
    Storage { code: u32, detail: String },
    #[error("invalid input: {detail}")]
    InvalidInput { code: u32, detail: String },
    #[error("not found: {detail}")]
    NotFound { code: u32, detail: String },
    #[error("permission denied: {detail}")]
    PermissionDenied { code: u32, detail: String },
    #[error("crypto error: {detail}")]
    Crypto { code: u32, detail: String },
    #[error("malformed event: {detail}")]
    Malformed { code: u32, detail: String },
    #[error("data directory is already open: {detail}")]
    AlreadyOpen { code: u32, detail: String },
    #[error("this device has been revoked: {detail}")]
    DeviceRevoked { code: u32, detail: String },
    #[error("no key available: {detail}")]
    NoKey { code: u32, detail: String },
    #[error("network error: {detail}")]
    Network { code: u32, detail: String },
    #[error("incompatible API version: {detail}")]
    IncompatibleVersion { code: u32, detail: String },
    #[error("pairing failed: {detail}")]
    Pairing { code: u32, detail: String },
    #[error("the core is closed: {detail}")]
    Closed { code: u32, detail: String },
    #[error("key store error: {detail}")]
    KeyStore { code: u32, detail: String },
    #[error("internal error: {detail}")]
    Internal { code: u32, detail: String },
    #[error("the database is corrupt: {detail}")]
    Corrupt { code: u32, detail: String },
    #[error("the database is from a newer release: {detail}")]
    NewerDatabase { code: u32, detail: String },
}

pub const KEY_STORE_CODE: u32 = 113;

impl SyncError {
    pub fn code(&self) -> u32 {
        match self {
            SyncError::Storage { code, .. }
            | SyncError::InvalidInput { code, .. }
            | SyncError::NotFound { code, .. }
            | SyncError::PermissionDenied { code, .. }
            | SyncError::Crypto { code, .. }
            | SyncError::Malformed { code, .. }
            | SyncError::AlreadyOpen { code, .. }
            | SyncError::DeviceRevoked { code, .. }
            | SyncError::NoKey { code, .. }
            | SyncError::Network { code, .. }
            | SyncError::IncompatibleVersion { code, .. }
            | SyncError::Pairing { code, .. }
            | SyncError::Closed { code, .. }
            | SyncError::KeyStore { code, .. }
            | SyncError::Internal { code, .. }
            | SyncError::Corrupt { code, .. }
            | SyncError::NewerDatabase { code, .. } => *code,
        }
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        CoreError::InvalidInput(detail.into()).into()
    }

    pub fn closed() -> Self {
        CoreError::Closed.into()
    }

    pub fn key_store(detail: impl Into<String>) -> Self {
        SyncError::KeyStore { code: KEY_STORE_CODE, detail: detail.into() }
    }
}

impl From<CoreError> for SyncError {
    fn from(e: CoreError) -> Self {
        let code = e.code();
        let detail = match &e {
            CoreError::Storage(s)
            | CoreError::InvalidInput(s)
            | CoreError::NotFound(s)
            | CoreError::PermissionDenied(s)
            | CoreError::Crypto(s)
            | CoreError::Malformed(s)
            | CoreError::Network(s)
            | CoreError::Pairing(s)
            | CoreError::Internal(s)
            | CoreError::Corrupt(s) => s.clone(),
            other => other.to_string(),
        };
        match e {
            CoreError::Storage(_) => SyncError::Storage { code, detail },
            CoreError::InvalidInput(_) => SyncError::InvalidInput { code, detail },
            CoreError::NotFound(_) => SyncError::NotFound { code, detail },
            CoreError::PermissionDenied(_) => SyncError::PermissionDenied { code, detail },
            CoreError::Crypto(_) => SyncError::Crypto { code, detail },
            CoreError::Malformed(_) => SyncError::Malformed { code, detail },
            CoreError::AlreadyOpen => SyncError::AlreadyOpen { code, detail },
            CoreError::DeviceRevoked => SyncError::DeviceRevoked { code, detail },
            CoreError::NoKey => SyncError::NoKey { code, detail },
            CoreError::Network(_) => SyncError::Network { code, detail },
            CoreError::IncompatibleVersion { .. } => SyncError::IncompatibleVersion { code, detail },
            CoreError::Pairing(_) => SyncError::Pairing { code, detail },
            CoreError::Closed => SyncError::Closed { code, detail },
            CoreError::Internal(_) => SyncError::Internal { code, detail },
            CoreError::Corrupt(_) => SyncError::Corrupt { code, detail },
            CoreError::NewerDatabase { .. } => SyncError::NewerDatabase { code, detail },
        }
    }
}

impl From<anyhow::Error> for SyncError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<CoreError>() {
            Ok(c) => c.into(),
            Err(e) => CoreError::Network(format!("{e:#}")).into(),
        }
    }
}

impl From<uniffi::UnexpectedUniFFICallbackError> for SyncError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        SyncError::key_store(e.reason)
    }
}
