//! A minimal QMP (QEMU Machine Protocol) client for VM lifecycle control
//! and guest memory stats.
//!
//! Uses `qapi`'s synchronous API with a fresh connection per call on
//! `spawn_blocking`: calls are infrequent, and the async API would leak a
//! hard-to-name generic type into this module's surface.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use qapi::qmp;

use crate::error::{Error, Result};

/// Query VM execution status (e.g. whether it's running or still booting).
pub(crate) async fn query_status(socket_path: &Path) -> Result<qmp::StatusInfo> {
    execute(socket_path, qmp::query_status {}).await
}

/// Requests a graceful ACPI shutdown. Does not wait for the guest to
/// actually power off; callers should poll the qemu process and/or
/// `query_status` afterwards.
pub(crate) async fn system_powerdown(socket_path: &Path) -> Result<()> {
    execute(socket_path, qmp::system_powerdown {}).await?;
    Ok(())
}

/// Forces QEMU to exit immediately, bypassing any guest shutdown sequence.
pub(crate) async fn quit(socket_path: &Path) -> Result<()> {
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
pub(crate) async fn set_balloon_stats_interval(
    socket_path: &Path,
    interval_secs: u32,
) -> Result<()> {
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

/// Upper bound on a whole QMP exchange (connect, handshake, command). A
/// wedged qemu must never hang a caller: `stop()` would otherwise never reach
/// its SIGKILL fallback, and status polls would pile up.
const QMP_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens a fresh QMP connection to `socket_path`, performs the capabilities
/// handshake, executes a single command, and closes the connection. Fails
/// with [`Error::QmpTimeout`] after [`QMP_TIMEOUT`].
async fn execute<C>(socket_path: &Path, command: C) -> Result<C::Ok>
where
    C: qapi::Command + Send + 'static,
    C::Ok: Send + 'static,
{
    execute_with_timeout(socket_path, command, QMP_TIMEOUT).await
}

async fn execute_with_timeout<C>(socket_path: &Path, command: C, limit: Duration) -> Result<C::Ok>
where
    C: qapi::Command + Send + 'static,
    C::Ok: Send + 'static,
{
    let socket_path = socket_path.to_path_buf();
    // The socket timeouts make the blocking task itself finish soon after
    // `limit` too, rather than lingering on the blocking pool.
    let task = tokio::task::spawn_blocking(move || execute_blocking(&socket_path, command, limit));
    match tokio::time::timeout(limit, task).await {
        Ok(joined) => joined.map_err(Error::QmpTask)?,
        Err(_elapsed) => Err(Error::QmpTimeout {
            command: C::NAME,
            timeout: limit,
        }),
    }
}

fn execute_blocking<C>(socket_path: &Path, command: C, limit: Duration) -> Result<C::Ok>
where
    C: qapi::Command,
{
    let connect_err = |source| Error::QmpConnect {
        path: socket_path.to_path_buf(),
        source,
    };
    let stream = UnixStream::connect(socket_path).map_err(connect_err)?;
    stream.set_read_timeout(Some(limit)).map_err(connect_err)?;
    stream.set_write_timeout(Some(limit)).map_err(connect_err)?;

    // `Qmp::from_stream` needs `Read + Write + Clone`, which `&UnixStream`
    // provides.
    let mut qmp = qapi::Qmp::from_stream(&stream);

    qmp.handshake().map_err(|source| Error::QmpHandshake {
        path: socket_path.to_path_buf(),
        source,
    })?;

    qmp.execute(&command).map_err(|source| Error::QmpCommand {
        command: C::NAME,
        source,
    })
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

    /// A peer that accepts but never speaks (a wedged qemu) must time out
    /// promptly rather than hang the caller.
    #[tokio::test]
    async fn execute_times_out_against_a_silent_peer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qmp.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let _held = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(10));
            drop(conn);
        });

        let started = std::time::Instant::now();
        let result =
            execute_with_timeout(&path, qmp::query_status {}, Duration::from_millis(300)).await;
        assert!(
            matches!(
                result,
                Err(Error::QmpTimeout { .. } | Error::QmpHandshake { .. })
            ),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn execute_reports_a_missing_socket_as_connect_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute(&dir.path().join("absent.sock"), qmp::query_status {}).await;
        assert!(
            matches!(result, Err(Error::QmpConnect { .. })),
            "{result:?}"
        );
    }
    use crate::fake_qmp::{FakeQmp, error, ok};

    fn socket() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qmp.sock");
        (dir, path)
    }

    #[tokio::test]
    async fn lifecycle_commands_are_sent_by_name() {
        let (_dir, path) = socket();
        let fake = FakeQmp::serve(&path, |name, _| match name {
            "query-status" => ok(json!({"running": false, "status": "prelaunch"})),
            _ => ok(json!({})),
        });

        let status = query_status(&path).await.unwrap();
        assert!(!status.running);
        system_powerdown(&path).await.unwrap();
        quit(&path).await.unwrap();

        assert_eq!(
            fake.commands(),
            ["query-status", "system_powerdown", "quit"]
        );
    }

    #[tokio::test]
    async fn balloon_polling_is_enabled_on_the_balloon_device() {
        let (_dir, path) = socket();
        let args = std::sync::Arc::new(std::sync::Mutex::new(json!(null)));
        let seen = std::sync::Arc::clone(&args);
        let fake = FakeQmp::serve(&path, move |_, arguments| {
            *seen.lock().unwrap() = arguments.clone();
            ok(json!({}))
        });

        set_balloon_stats_interval(&path, 7).await.unwrap();

        assert_eq!(fake.commands(), ["qom-set"]);
        assert_eq!(
            *args.lock().unwrap(),
            json!({
                "path": "/machine/peripheral/balloon0",
                "property": "guest-stats-polling-interval",
                "value": 7
            })
        );
    }

    #[tokio::test]
    async fn guest_memory_reads_the_balloon_stats() {
        let (_dir, path) = socket();
        let fake = FakeQmp::serve(&path, |_, arguments| {
            assert_eq!(arguments["property"], "guest-stats");
            ok(json!({
                "last-update": 1,
                "stats": {"stat-total-memory": 100, "stat-available-memory": 40}
            }))
        });

        let mem = guest_memory(&path).await.unwrap();

        assert_eq!(
            mem,
            Some(GuestMemory {
                total_bytes: 100,
                available_bytes: 40
            })
        );
        assert_eq!(fake.commands(), ["qom-get"]);
    }

    #[tokio::test]
    async fn a_refused_command_is_a_command_error_naming_it() {
        let (_dir, path) = socket();
        let _fake = FakeQmp::serve(&path, |_, _| error("no balloon device"));

        for result in [
            guest_memory(&path).await.map(drop),
            set_balloon_stats_interval(&path, 2).await,
            system_powerdown(&path).await,
            quit(&path).await,
        ] {
            let err = result.unwrap_err();
            assert!(matches!(err, Error::QmpCommand { .. }), "{err:?}");
        }
    }

    /// A qemu that dies mid-handshake makes `qapi` panic on the early EOF;
    /// that must come back as an error, not take the daemon down.
    #[tokio::test]
    async fn a_peer_that_hangs_up_before_greeting_is_an_error() {
        let (_dir, path) = socket();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || drop(listener.accept()));

        let err = query_status(&path).await.unwrap_err();
        assert!(matches!(err, Error::QmpTask(_)), "{err:?}");
    }
}
