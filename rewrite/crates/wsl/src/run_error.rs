//! [`RunError`]: why a PowerShell run gave no result.

use std::process::ExitStatus;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Missing, not executable, or interop is off.
    #[error("cannot start PowerShell: {0}")]
    CannotStart(#[source] std::io::Error),
    #[error("PowerShell took longer than {0:?}")]
    TimedOut(Duration),
    #[error("PowerShell took longer than {after:?} and could not be stopped: {source}")]
    Unstoppable {
        after: Duration,
        #[source]
        source: std::io::Error,
    },
    #[error("waiting for PowerShell: {0}")]
    Wait(#[source] std::io::Error),
    /// The script failed; `stderr` is its error output, summarized.
    #[error("PowerShell failed ({status}): {stderr}")]
    Failed { status: ExitStatus, stderr: String },
    #[error("writing to PowerShell: {0}")]
    WriteInput(#[source] std::io::Error),
    #[error("reading PowerShell's output: {0}")]
    ReadOutput(#[source] std::io::Error),
    #[error("PowerShell's output is not UTF-8")]
    NotUtf8,
    /// Succeeded but wrote nothing; `stderr` may say why.
    #[error("no output from PowerShell: {stderr}")]
    NoOutput { stderr: String },
}
