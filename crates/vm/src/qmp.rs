//! A minimal QMP (QEMU Machine Protocol) client for VM lifecycle control
//! and guest memory stats.
//!
//! Uses `qapi`'s synchronous API with a fresh connection per call on
//! `spawn_blocking`: calls are infrequent, and the async API would leak a
//! hard-to-name generic type into this module's surface.

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

/// QOM path of the balloon device; QEMU places `-device id=X` objects under
/// `/machine/peripheral/X`.
fn balloon_qom_path() -> String {
    format!("/machine/peripheral/{}", crate::qemu::BALLOON_DEVICE_ID)
}

/// Sets how often (in seconds) the guest's balloon driver reports memory
/// stats to qemu. Until this is set, [`guest_memory`] never has readings.
pub async fn set_balloon_stats_interval(socket_path: &Path, interval_secs: u32) -> Result<()> {
    execute(
        socket_path,
        qmp::qom_set {
            path: balloon_qom_path(),
            property: "guest-stats-polling-interval".to_string(),
            value: serde_json::json!(interval_secs),
        },
    )
    .await?;
    Ok(())
}

/// A guest's self-reported memory usage from its balloon driver. Distinct
/// from qemu's host RSS, which reflects pages touched, not guest usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestMemory {
    /// Total guest RAM, in bytes, as reported by the guest kernel.
    pub total_bytes: u64,
    /// The guest's `MemAvailable`, which counts reclaimable page cache as
    /// free, so `total_bytes - available_bytes` isn't inflated by cache.
    pub available_bytes: u64,
}

/// Reads the guest's current memory stats via its balloon driver.
///
/// Returns `Ok(None)` if the guest hasn't reported yet (`last-update` is 0
/// until its first report after boot); treat it as "no stats this tick".
pub async fn guest_memory(socket_path: &Path) -> Result<Option<GuestMemory>> {
    let value = execute(
        socket_path,
        qmp::qom_get {
            path: balloon_qom_path(),
            property: "guest-stats".to_string(),
        },
    )
    .await?;

    Ok(parse_guest_stats(&value))
}

/// Pure parse of the `qom-get .../guest-stats` value, for unit testing.
///
/// Expected shape:
/// ```json
/// {
///   "last-update": 1700000000,
///   "stats": {
///     "stat-total-memory": 4294967296,
///     "stat-available-memory": 3200000000,
///     "stat-free-memory": 3000000000,
///     ...
///   }
/// }
/// ```
/// Missing fields or `last-update` of 0 yield `None`, never a panic.
fn parse_guest_stats(value: &serde_json::Value) -> Option<GuestMemory> {
    let last_update = value.get("last-update")?.as_u64()?;
    if last_update == 0 {
        return None;
    }

    let stats = value.get("stats")?;
    let total_bytes = stats.get("stat-total-memory")?.as_u64()?;
    let available_bytes = stats.get("stat-available-memory")?.as_u64()?;

    Some(GuestMemory {
        total_bytes,
        available_bytes,
    })
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

    // `Qmp::from_stream` needs `Read + Write + Clone`, which `&UnixStream`
    // provides.
    let mut qmp = qapi::Qmp::from_stream(&stream);

    qmp.handshake()
        .map_err(|e| Error::QmpExecute(format!("handshake failed: {e:?}")))?;

    qmp.execute(&command)
        .map_err(|e| Error::QmpExecute(format!("{e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn balloon_qom_path_is_derived_from_the_device_id_constant() {
        assert_eq!(balloon_qom_path(), "/machine/peripheral/balloon0");
    }

    #[test]
    fn parse_guest_stats_reads_total_and_available_once_reported() {
        let value = json!({
            "last-update": 1_700_000_000u64,
            "stats": {
                "stat-total-memory": 4_294_967_296u64,
                "stat-available-memory": 3_200_000_000u64,
                "stat-free-memory": 3_000_000_000u64,
            }
        });

        let mem = parse_guest_stats(&value).expect("stats should parse");

        assert_eq!(mem.total_bytes, 4_294_967_296);
        assert_eq!(mem.available_bytes, 3_200_000_000);
    }

    #[test]
    fn parse_guest_stats_is_none_before_the_first_report() {
        // `last-update` stays 0 until the guest's balloon driver has
        // reported at least once after boot.
        let value = json!({
            "last-update": 0,
            "stats": {
                "stat-total-memory": 4_294_967_296u64,
                "stat-available-memory": 0,
            }
        });

        assert_eq!(parse_guest_stats(&value), None);
    }

    #[test]
    fn parse_guest_stats_is_none_on_unexpected_shape() {
        assert_eq!(parse_guest_stats(&json!({})), None);
        assert_eq!(parse_guest_stats(&json!({"last-update": 123})), None);
        assert_eq!(
            parse_guest_stats(&json!({"last-update": 123, "stats": {}})),
            None
        );
    }
}
