use std::path::PathBuf;

/// Errors from a secret backend. There is no `NotFound`: a missing secret is
/// normal, so `get` returns `Ok(None)`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    /// The OS keystore couldn't be reached or refused access (locked, no
    /// Secret Service, ...).
    #[error("the OS keystore is unavailable: {0}")]
    KeyUnavailable(#[source] keyring::Error),
    /// A vault exists but its data key is gone from the keystore. A new key
    /// is deliberately *not* minted: that would make the existing secrets
    /// unrecoverable without saying so.
    #[error(
        "the vault {path} exists but its key is missing from the OS keystore; \
         restore the key, or move the vault aside to start over"
    )]
    KeyMissing { path: PathBuf },
    #[error("the vault data key in the OS keystore is invalid: {0}")]
    BadKey(String),
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Another process (e.g. a second daemon) has the vault open.
    #[error("the vault {path} is in use by another GAVMO process")]
    InUse { path: PathBuf },
    #[error("not a GAVMO vault file (bad magic)")]
    NotAVault,
    #[error("vault file is truncated")]
    Truncated,
    #[error("unsupported vault version {0} (this build understands 1 and 2)")]
    UnsupportedVersion(u8),
    /// Wrong key and tampering are indistinguishable by design.
    #[error("vault could not be decrypted (wrong key or the file was tampered with/corrupted)")]
    Decrypt,
    #[error("vault contents are corrupt: {0}")]
    Corrupt(#[source] serde_json::Error),
    /// Any other backend failure (also handy for test doubles).
    #[error("secret store backend error: {0}")]
    Backend(String),
}

impl SecretError {
    pub(crate) fn io(
        action: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }
}
