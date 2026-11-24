//! [`DpapiError`]: the DPAPI side of a [`keystore::Error`].

use std::fmt;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum DpapiError {
    /// PowerShell couldn't do it: no interop, a policy, a foreign blob, ...
    #[error("running {powershell} to {op} the key: {source}")]
    PowerShell {
        op: Op,
        powershell: PathBuf,
        #[source]
        source: wsl::RunError,
    },
    #[error(transparent)]
    File(#[from] private_fs::Error),
    #[error("{path}: not a GAVMO DPAPI key file (unknown header)")]
    NotAKeyFile { path: PathBuf },
    #[error("{path}: the sealed key is missing or not base64")]
    NoSealedKey { path: PathBuf },
    #[error("PowerShell's result is not base64 (to {op} the key)")]
    ResultNotBase64 { op: Op },
    /// A fresh seal didn't give back the key it sealed; nothing was stored.
    #[error("unsealing the new key gave a different key")]
    SealDoesNotOpen,
}

/// What PowerShell was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Protect,
    Unprotect,
}

impl Op {
    /// The word the script reads on its first line.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Protect => "protect",
            Self::Unprotect => "unprotect",
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
