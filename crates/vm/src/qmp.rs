//! A minimal QMP (QEMU Machine Protocol) client used for VM lifecycle
//! control (status checks, graceful shutdown, force quit).
//!
//! This deliberately uses the `qapi` crate's **synchronous** API
//! (`qapi::Qmp` over a plain `std::os::unix::net::UnixStream`) rather than
//! its async/tokio integration. Our usage is low-frequency — a handful of
//! calls per VM lifecycle, not a long-lived streaming connection — so each
//! call opens a fresh short-lived connection on a blocking thread via
//! `spawn_blocking`. This avoids a hard-to-name generic type that the
//! async API would otherwise force into this module's public surface (see
//! `plan.md`'s QMP section for the reasoning).

use std::os::unix::net::UnixStream;
use std::path::Path;

use qapi::qmp;

use crate::error::{Error, Result};

/// Query VM execution status (e.g. whether it's running or still booting).
pub async fn query_status(socket_path: &Path) -> Result<qmp::StatusInfo> {
    execute(socket_path, qmp::query_status {}).await
}

/// Requests a graceful ACPI shutdown. Does not wait for the guest to
/// actually power off; callers should poll the qemu process and/or
/// `query_status` afterwards.
pub async fn system_powerdown(socket_path: &Path) -> Result<()> {
    execute(socket_path, qmp::system_powerdown {}).await?;
    Ok(())
}

/// Forces QEMU to exit immediately, bypassing any guest shutdown sequence.
pub async fn quit(socket_path: &Path) -> Result<()> {
    execute(socket_path, qmp::quit {}).await?;
    Ok(())
}

/// Opens a fresh QMP connection to `socket_path`, performs the capabilities
/// handshake, executes a single command, and closes the connection.
async fn execute<C>(socket_path: &Path, command: C) -> Result<C::Ok>
where
    C: qapi::Command + Send + 'static,
    C::Ok: Send + 'static,
{
    let socket_path = socket_path.to_path_buf();
    tokio::task::spawn_blocking(move || execute_blocking(&socket_path, command))
        .await
        .map_err(|join_err| Error::QmpExecute(format!("qmp task panicked: {join_err}")))?
}

fn execute_blocking<C>(socket_path: &Path, command: C) -> Result<C::Ok>
where
    C: qapi::Command,
{
    let stream = UnixStream::connect(socket_path).map_err(|source| Error::QmpConnect {
        path: socket_path.to_path_buf(),
        source,
    })?;

    // `&UnixStream` implements `Read + Write + Clone` (references are
    // trivially `Copy`/`Clone`), which is what `Qmp::from_stream` requires
    // — matching the crate's own documented usage pattern exactly.
    let mut qmp = qapi::Qmp::from_stream(&stream);

    qmp.handshake()
        .map_err(|e| Error::QmpExecute(format!("handshake failed: {e:?}")))?;

    qmp.execute(&command)
        .map_err(|e| Error::QmpExecute(format!("{e:?}")))
}
