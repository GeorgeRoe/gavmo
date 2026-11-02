//! Health checking for a booting/running VM.
//!
//! Healthy means `opencode web` answers on the forwarded host port, checked
//! with a raw HTTP probe to avoid an HTTP client dependency.
//!
//! During boot the probe is raced against the `gavmo-rebuild` failure
//! marker: a broken `gavmo.nix` means opencode never starts, so this fails
//! fast instead of waiting out the full boot timeout.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

use crate::error::{Error, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Polls `127.0.0.1:port` until `opencode web` answers, the `gavmo-rebuild`
/// failure marker at `marker_path` appears, or `overall_timeout` elapses.
/// The marker's contents become [`Error::RebuildFailed::log_file`].
pub(crate) async fn wait_for_ready(
    port: u16,
    marker_path: &Path,
    overall_timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + overall_timeout;

    loop {
        if probe_once(port).await {
            return Ok(());
        }

        if let Some(contents) = read_marker(marker_path).await? {
            return Err(Error::RebuildFailed {
                log_file: contents.trim().to_owned(),
            });
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(Error::HealthTimeout(overall_timeout));
        }

        sleep(POLL_INTERVAL).await;
    }
}

/// The marker holds one short filename; anything longer is not a marker.
const MAX_MARKER_BYTES: u64 = 4096;

/// Reads the guest-written failure marker. The guest controls this path, so
/// it is opened without following symlinks and without blocking (a FIFO
/// would otherwise hang the boot forever), must be a regular file, and is
/// read with a size cap. `Ok(None)` while it doesn't exist, or while it is
/// still empty (the guest may be mid-write).
async fn read_marker(path: &Path) -> Result<Option<String>> {
    let owned = path.to_path_buf();
    let read = tokio::task::spawn_blocking(move || -> std::io::Result<Option<String>> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;

        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&owned)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "rebuild marker is not a regular file",
            ));
        }
        let mut text = String::new();
        file.take(MAX_MARKER_BYTES).read_to_string(&mut text)?;
        Ok((!text.trim().is_empty()).then_some(text))
    })
    .await
    .map_err(|e| Error::ReadMarker {
        path: path.to_path_buf(),
        source: std::io::Error::other(e),
    })?;
    read.map_err(|source| Error::ReadMarker {
        path: path.to_path_buf(),
        source,
    })
}

/// A single, non-retrying check of whether something is answering HTTP
/// requests on `127.0.0.1:port`.
pub(crate) async fn probe_once(port: u16) -> bool {
    let Ok(Ok(mut stream)) = timeout(PROBE_TIMEOUT, TcpStream::connect(("127.0.0.1", port))).await
    else {
        return false;
    };

    let request = b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return false;
    }

    let mut buf = [0u8; 16];
    matches!(
        timeout(PROBE_TIMEOUT, stream.read(&mut buf)).await,
        Ok(Ok(n)) if n > 0
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn probe_succeeds_against_a_responding_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "probe sent no request");
            socket
                .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });

        assert!(probe_once(port).await);
    }

    /// Binds then drops a listener so the port is known-closed, rather than
    /// assuming some fixed port is free.
    async fn closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn probe_fails_when_nothing_is_listening() {
        assert!(!probe_once(closed_port().await).await);
    }

    /// A missing marker (`NotFound`) is the normal "no failure yet" state and
    /// must not short-circuit the wait.
    #[tokio::test]
    async fn wait_for_ready_times_out_when_nothing_ever_responds_and_no_marker_appears() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("rebuild-failed"); // deliberately never created

        let result = wait_for_ready(closed_port().await, &marker, Duration::from_millis(600)).await;
        assert!(matches!(result, Err(Error::HealthTimeout(_))));
    }

    #[tokio::test]
    async fn wait_for_ready_detects_the_rebuild_failure_marker_before_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("rebuild-failed");
        tokio::fs::write(&marker, "rebuild-2026-01-01T00-00-00Z.log\n")
            .await
            .unwrap();

        // Nothing answers; a long overall_timeout proves the marker check,
        // not a timeout, resolves this.
        let result = wait_for_ready(closed_port().await, &marker, Duration::from_secs(30)).await;
        assert!(matches!(
            result,
            Err(Error::RebuildFailed { log_file })
                if log_file == "rebuild-2026-01-01T00-00-00Z.log"
        ));
    }

    #[tokio::test]
    async fn read_marker_ignores_missing_and_empty_markers() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("rebuild-failed");
        assert_eq!(read_marker(&marker).await.unwrap(), None);
        tokio::fs::write(&marker, "  \n").await.unwrap();
        assert_eq!(read_marker(&marker).await.unwrap(), None);
    }

    #[tokio::test]
    async fn read_marker_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("host-secret");
        tokio::fs::write(&target, "secret").await.unwrap();
        let marker = dir.path().join("rebuild-failed");
        std::os::unix::fs::symlink(&target, &marker).unwrap();
        assert!(matches!(
            read_marker(&marker).await,
            Err(Error::ReadMarker { .. })
        ));
    }

    /// A FIFO would block a plain open forever, hanging the boot.
    #[tokio::test]
    async fn read_marker_refuses_a_fifo_without_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("rebuild-failed");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&marker)
                .status()
                .unwrap()
                .success()
        );
        let result = timeout(Duration::from_secs(5), read_marker(&marker))
            .await
            .expect("must not hang");
        assert!(matches!(result, Err(Error::ReadMarker { .. })));
    }
    #[tokio::test]
    async fn wait_for_ready_returns_once_the_port_answers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"HTTP/1.0 200 OK\r\n\r\n").await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();

        wait_for_ready(port, &dir.path().join("no-marker"), Duration::from_secs(10))
            .await
            .unwrap();
    }
}
