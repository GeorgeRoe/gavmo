//! The registry's error type, and how it is shown to the browser.

use std::io;

use crate::app::AppError;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("unknown workspace: {0}")]
    UnknownWorkspace(String),
    #[error("workspace is busy")]
    Busy,
    #[error("GAVMO is shutting down")]
    ShuttingDown,
    /// The request was invalid (bad path, unknown tool, wrong credential
    /// fields); the message is for the user.
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("secret store unavailable: {0}")]
    SecretStore(#[from] secrets::SecretError),
    /// A tool test (or similar user-visible operation) ran and failed.
    #[error("{0}")]
    Failed(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Vm(#[from] vm::Error),
    #[error("background task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub(crate) type Result<T, E = RegistryError> = std::result::Result<T, E>;

impl RegistryError {
    /// An I/O error with what was being done.
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Wraps an `io::Error` with context, for `map_err`.
pub(crate) fn io_ctx(context: impl Into<String>) -> impl FnOnce(io::Error) -> RegistryError {
    let context = context.into();
    move |source| RegistryError::Io { context, source }
}

impl From<RegistryError> for AppError {
    fn from(e: RegistryError) -> Self {
        match e {
            RegistryError::UnknownWorkspace(id) => Self::UnknownWorkspace(id),
            RegistryError::Busy => Self::Busy,
            RegistryError::ShuttingDown => Self::ShuttingDown,
            RegistryError::InvalidInput(msg) => Self::InvalidInput(msg),
            RegistryError::NotFound(what) => Self::NotFound(what),
            RegistryError::SecretStore(e) => Self::SecretStore(e.to_string()),
            RegistryError::Failed(msg) => Self::Failed(msg),
            // Full context is useful in the UI: these are the user's own
            // host paths and VMs, not a third party's internals.
            e @ (RegistryError::Io { .. } | RegistryError::Vm(_) | RegistryError::Task(_)) => {
                Self::Internal(error_chain(&e))
            }
        }
    }
}

/// `e` and its sources, joined with ": " (skipping repeats).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = s.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_distinct_app_errors() {
        assert_eq!(
            AppError::from(RegistryError::UnknownWorkspace("/x".to_owned())),
            AppError::UnknownWorkspace("/x".to_owned())
        );
        assert_eq!(AppError::from(RegistryError::Busy), AppError::Busy);
        let io = RegistryError::io("reading x", io::Error::other("boom"));
        assert_eq!(
            AppError::from(io),
            AppError::Internal("reading x: boom".to_owned())
        );
    }
}
