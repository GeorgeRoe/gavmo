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

    #[error("failed to remove stale file {path}: {source}")]
    RemoveStale {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "virtiofsd {found} is too old: GAVMO needs {required} or newer \
         (for --translate-uid and --readonly)"
    )]
    VirtiofsdTooOld { found: String, required: String },

    #[error("a VM helper process (virtiofsd) exited during startup: {status}")]
    HelperExited { status: ExitStatus },

    #[error("{path} did not appear within {timeout:?}; virtiofsd failed to start")]
    HelperNotReady { path: PathBuf, timeout: Duration },

    #[error("failed to rename {from} to {to}: {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("refusing to use {path}: it {reason}")]
    UnsafeDir { path: PathBuf, reason: &'static str },

    #[error("socket path {path} is longer than the {max}-byte unix socket limit")]
    SocketPathTooLong { path: PathBuf, max: usize },

    /// A disk path that can't go into qemu's `-blockdev` JSON, which is UTF-8
    /// only. (Everything else is passed to qemu byte-for-byte.)
    #[error("path {path} cannot be passed to qemu: {reason}")]
    UnsupportedPath { path: PathBuf, reason: &'static str },

    #[error("failed to read {path}: {source}")]
    ReadMarker {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to stat {path}: {source}")]
    Stat {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to kill `{program}`: {source}")]
    Kill {
        program: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("qemu-img failed (exit status {status}): {stderr}")]
    QemuImg { status: ExitStatus, stderr: String },

    #[error("qemu-img info returned no virtual-size")]
    QemuImgOutput,

    #[error("cannot read the base image {path}: {source}")]
    BaseImage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to access the base image record {path}: {source}")]
    BaseRecord {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The overlay was created from a different base image than the current
    /// one (or its record is unreadable), so booting it would pair its
    /// blocks with the wrong base. Resetting the workspace rebuilds it.
    #[error(
        "the base image has changed since this VM's disk was created \
         ({}, now {}); reset the workspace to rebuild its disk from the new image",
        recorded.as_deref().map_or_else(|| "unknown image".to_owned(), |r| format!("was {}", short_id(r))),
        short_id(current)
    )]
    BaseImageChanged {
        /// The id recorded with the overlay; `None` if the record is
        /// unreadable.
        recorded: Option<String>,
        current: String,
    },

    #[error("failed to connect to QMP socket {path}: {source}")]
    QmpConnect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("QMP handshake on {path} failed: {source}")]
    QmpHandshake {
        path: PathBuf,
        #[source]
        source: qapi::ExecuteError,
    },

    #[error("QMP command `{command}` failed: {source}")]
    QmpCommand {
        command: &'static str,
        #[source]
        source: qapi::ExecuteError,
    },

    #[error("QMP command `{command}` did not complete within {timeout:?}")]
    QmpTimeout {
        command: &'static str,
        timeout: Duration,
    },

    #[error("QMP worker task failed: {0}")]
    QmpTask(#[source] tokio::task::JoinError),

    #[error("qemu exited during boot ({status}){}", fmt_tail(stderr_tail))]
    QemuExited {
        status: ExitStatus,
        /// The end of qemu's stderr, usually naming the problem.
        stderr_tail: String,
    },

    #[error("failed to wait for `{program}`: {source}")]
    Wait {
        program: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("failed to open qemu log {path}: {source}")]
    QemuLog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("vm did not become healthy within {0:?}")]
    HealthTimeout(Duration),

    /// The guest's `gavmo-rebuild` failed to apply the repo's `gavmo.nix`,
    /// detected via its marker file well before [`Error::HealthTimeout`].
    /// `log_file` is the rebuild log's filename, relative to `state_dir`.
    #[error("gavmo.nix failed to build - see {log_file}")]
    RebuildFailed { log_file: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// The first 12 characters of an image id, for messages.
fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

fn fmt_tail(tail: &str) -> String {
    if tail.is_empty() {
        String::new()
    } else {
        format!(": {tail}")
    }
}
