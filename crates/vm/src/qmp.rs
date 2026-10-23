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

/// QOM path of the `virtio-balloon-pci` device `qemu.rs` always adds. QEMU
/// places a `-device`'s QOM object under `/machine/peripheral/<id>`, so this
/// is derived from the same [`crate::qemu::BALLOON_DEVICE_ID`] constant that
/// names the device on the qemu command line — one source of truth instead
/// of the id and this path drifting apart.
fn balloon_qom_path() -> String {
    format!("/machine/peripheral/{}", crate::qemu::BALLOON_DEVICE_ID)
}

/// Sets how often (in seconds) the guest's virtio-balloon driver reports
/// memory stats back to qemu, by writing the balloon device's
/// `guest-stats-polling-interval` QOM property. Called once at the end of
/// [`crate::handle::RunningVm::boot`].
///
/// Best-effort by design: a VM this fails for (or that's never called this
/// at all) still boots and runs completely normally — it just never gets
/// [`guest_memory`] readings, and the control panel shows "—" for memory
/// instead of a number. Never treat a failure here as fatal to boot.
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

/// A guest's self-reported memory usage, from its virtio-balloon driver —
/// deliberately distinct from qemu's host-side process RSS (see
/// `app::VmStats`'s doc comment for why RSS was the wrong number to show).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestMemory {
    /// Total guest RAM, in bytes, as reported by the guest kernel.
    pub total_bytes: u64,
    /// Memory the guest kernel considers available for new allocations
    /// without swapping — i.e. `MemAvailable` from the guest's own
    /// `/proc/meminfo`, which already counts reclaimable page cache as
    /// free. `total_bytes - available_bytes` is therefore a "real pressure"
    /// used figure, not inflated by cache the guest could drop instantly.
    pub available_bytes: u64,
}

/// Reads the guest's current memory stats via its virtio-balloon driver
/// (see [`set_balloon_stats_interval`] for how the guest is told to start
/// reporting).
///
/// Returns `Ok(None)` — not an error — if the driver hasn't reported yet:
/// the QOM property's `last-update` is `0` until the guest's first
/// stats round-trip after boot. Callers should treat `None` the same as
/// `vm::stats::sample`'s errors elsewhere in this crate: "no stats this
/// tick", not a hard failure.
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

/// Pure parse of the JSON value `qom-get .../guest-stats` returns — split
/// out from the QMP round-trip above so it's unit-testable in the sidecar
/// against a literal JSON fixture, with no real qemu/socket needed (same
/// "pure fn split from IO" convention as
/// `share::build_virtiofsd_args`/`disk::build_overlay_args` elsewhere in
/// this crate).
///
/// Expected shape (QEMU's virtio-balloon `guest-stats` QOM property, stable
/// API since QEMU 2.9's balloon memory-stats support):
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
/// Any deviation from this (missing fields, `last-update` still `0`) is
/// treated as "not ready yet" via `None`, never a panic — defensive against
/// a QEMU version that renamed/omitted a field, same posture as the rest of
/// this crate's `/proc`-parsing in `stats.rs`.
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

    // `&UnixStream` implements `Read + Write + Clone` (references are
    // trivially `Copy`/`Clone`), which is what `Qmp::from_stream` requires
    // — matching the crate's own documented usage pattern exactly.
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
