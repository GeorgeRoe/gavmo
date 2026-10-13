//! Health checking for a booting/running VM.
//!
//! We treat "`opencode web` answers on the forwarded host port" as the
//! definition of healthy. This is a minimal raw HTTP probe rather than a
//! full HTTP client, since we don't otherwise need an HTTP client
//! dependency in this crate.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

use crate::error::{Error, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Polls `127.0.0.1:port` with a minimal HTTP GET until it responds or
/// `overall_timeout` elapses.
pub async fn wait_for_http(port: u16, overall_timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + overall_timeout;

    loop {
        if probe_once(port).await {
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(Error::HealthTimeout(overall_timeout));
        }

        sleep(POLL_INTERVAL).await;
    }
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
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        assert!(probe_once(port).await);
    }

    #[tokio::test]
    async fn probe_fails_when_nothing_is_listening() {
        // Port 1 is privileged/unassigned; nothing should be listening.
        assert!(!probe_once(1).await);
    }

    #[tokio::test]
    async fn wait_for_http_times_out_when_nothing_ever_responds() {
        let result = wait_for_http(1, Duration::from_millis(600)).await;
        assert!(matches!(result, Err(Error::HealthTimeout(_))));
    }
}
