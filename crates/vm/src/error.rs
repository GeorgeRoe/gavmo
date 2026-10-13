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
}

pub type Result<T> = std::result::Result<T, Error>;
