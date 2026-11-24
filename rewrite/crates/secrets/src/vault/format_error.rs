//! [`FormatError`]: a vault file that can't be read, or contents that can't
//! be written.

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("not a GAVMO vault file (bad magic)")]
    NotAVault,
    #[error("vault file is truncated")]
    Truncated,
    /// Version 1 (written before the header was authenticated) is no longer
    /// read; neither is anything newer than this build.
    #[error("unsupported vault version {0} (this build reads version 2)")]
    UnsupportedVersion(u8),
    /// Wrong key and tampering are indistinguishable by design.
    #[error("vault could not be decrypted (wrong key, or the file was tampered with or corrupted)")]
    Decrypt,
    /// Authentic, but not the JSON map: a writer's bug, not tampering.
    #[error("vault contents are corrupt: {0}")]
    Corrupt(#[source] serde_json::Error),
    #[error("encrypting the vault failed")]
    Encrypt,
}
