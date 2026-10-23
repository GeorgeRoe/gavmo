use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("qemu-img failed (exit status {status}): {stderr}")]
    DiskCreateFailed { status: ExitStatus, stderr: String },

    #[error("failed to connect to QMP socket {path}: {source}")]
    QmpConnect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("QMP command failed: {0}")]
    QmpExecute(String),

    #[error("vm did not become healthy within {0:?}")]
    HealthTimeout(Duration),

    /// The guest's `gavmo-rebuild` service (M2 — see
    /// `nix/base/configuration.nix`) failed to apply the repo's `gavmo.nix`.
    /// Detected via the `rebuild-failed` marker file in `VmConfig::state_dir`
    /// (host-visible over the virtiofs `/workspace` share), which the guest writes
    /// instead of ever starting opencode — this resolves much faster than
    /// waiting out [`Error::HealthTimeout`]. `log_file` is the filename
    /// (relative to `state_dir`) of the full `nixos-rebuild` output, for the
    /// caller to surface/read.
    #[error("gavmo.nix failed to build - see {log_file}")]
    RebuildFailed { log_file: String },
}

pub type Result<T> = std::result::Result<T, Error>;
