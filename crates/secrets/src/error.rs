use std::path::PathBuf;

/// Errors from a secret backend. There is no `NotFound`: a missing secret is
/// normal, so `get` returns `Ok(None)`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    /// The OS keystore couldn't be reached or refused access (locked, no
    /// Secret Service, ...).
    #[error("the OS keystore is unavailable: {0}")]
    KeyUnavailable(#[source] keyring_core::Error),
    /// The Secret Service is reachable but has no default keyring to store a
    /// new key in.
    #[error(
        "the OS keyring has no default keyring to store the vault key in \
         (common on WSL and headless Linux, where nothing creates the \"login\" \
         keyring): create one, e.g. with seahorse or `gnome-keyring-daemon --unlock`; \
         on WSL, --key-store windows avoids the Linux keyring altogether"
    )]
    NoDefaultKeyring,
    /// The Windows key store (DPAPI, reached through WSL interop) couldn't be
    /// used: no interop, no PowerShell, a policy blocking it, ...
    #[error("the Windows key store is unavailable: {0}")]
    WindowsKeyUnavailable(String),
    /// A vault exists but its data key is gone from the key store. A new key
    /// is deliberately *not* minted: that would make the existing secrets
    /// unrecoverable without saying so.
    #[error(
        "the vault {path} exists but its key is missing from the key store \
         (the OS keyring, or vault-key.dpapi next to the vault on WSL); \
         restore the key, or move the vault aside to start over"
    )]
    KeyMissing { path: PathBuf },
    #[error("the stored vault data key is invalid: {0}")]
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
