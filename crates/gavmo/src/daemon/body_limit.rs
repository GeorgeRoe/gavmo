//! Bounds on control-panel request bodies. Guests can reach the UI port, so
//! a body is read in full, under a size cap and one total deadline, before
//! a handler sees it.
//!
//! Only the read is timed. Handlers are never cancelled: stop, reset and
//! remove tear a VM down inline, and dropping one partway would leave the
//! workspace stuck `Stopping`, its VM killed instead of shut down, or a
//! confirmed reset not done.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::types::AppError;

#[derive(Clone, Copy, Debug)]
pub(super) struct BodyLimits {
    pub(super) max_bytes: usize,
    /// For the whole body, not per chunk, so a client trickling bytes can't
    /// hold a connection open.
    pub(super) deadline: Duration,
}

impl BodyLimits {
    /// Server-function arguments are a few form fields, the largest a tool
    /// credential; pages are GETs with no body.
    pub(super) const UI: Self = Self {
        max_bytes: 64 * 1024,
        deadline: Duration::from_secs(10),
    };
}

/// Middleware: buffers the request body within `limits`, then passes the
/// request on with the buffered body. Too large is a 413 (checked against
/// `Content-Length` up front, then while reading), too slow a 408.
pub(super) async fn buffer_body(
    State(limits): State<BodyLimits>,
    req: Request,
    next: Next,
) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let declared_too_large = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|len| u64::try_from(limits.max_bytes).is_ok_and(|max| len > max));
    if declared_too_large {
        return too_large(is_api, limits);
    }

    let (parts, body) = req.into_parts();
    let read = axum::body::to_bytes(body, limits.max_bytes);
    let bytes = match tokio::time::timeout(limits.deadline, read).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            let e = e.into_inner();
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                return too_large(is_api, limits);
            }
            return error(
                is_api,
                StatusCode::BAD_REQUEST,
                format!("could not read the request body: {e}"),
            );
        }
        Err(_elapsed) => {
            return error(
                is_api,
                StatusCode::REQUEST_TIMEOUT,
                format!(
                    "request body not received within {}s",
                    limits.deadline.as_secs()
                ),
            );
        }
    };
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

fn too_large(is_api: bool, limits: BodyLimits) -> Response {
    error(
        is_api,
        StatusCode::PAYLOAD_TOO_LARGE,
        format!(
            "request body is larger than {} KiB",
            limits.max_bytes / 1024
        ),
    )
}

/// `/api/*` errors carry a JSON-encoded [`AppError`], as server functions'
/// own errors do, so the panel shows the message; other paths get the bare
/// status.
fn error(is_api: bool, status: StatusCode, message: String) -> Response {
    if !is_api {
        return status.into_response();
    }
    let body = serde_json::to_string(&AppError::InvalidInput(message))
        .unwrap_or_else(|_| "\"InvalidInput\"".to_owned());
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Bytes;
    use axum::routing::post;
    use tower::ServiceExt as _;

    use super::*;

    const LIMITS: BodyLimits = BodyLimits {
        max_bytes: 16,
        deadline: Duration::from_millis(50),
    };

    /// Echoes the body's length, so a test sees what the handler received.
    fn router() -> Router {
        Router::new()
            .route(
                "/api/echo",
                post(|body: Bytes| async move { body.len().to_string() }),
            )
            .route("/page", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(LIMITS, buffer_body))
    }

    async fn send(path: &str, body: Body, len: Option<usize>) -> (StatusCode, String) {
        let mut req = axum::http::Request::builder().method("POST").uri(path);
        if let Some(len) = len {
            req = req.header(header::CONTENT_LENGTH, len);
        }
        let res = router().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    fn api_error(body: &str) -> AppError {
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn a_body_within_the_limit_reaches_the_handler_whole() {
        let (status, body) = send("/api/echo", Body::from("0123456789abcdef"), Some(16)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "16"));
    }

    #[tokio::test]
    async fn a_declared_length_over_the_limit_is_refused_before_reading() {
        // The body itself is tiny; the header alone decides.
        let (status, body) = send("/api/echo", Body::from("x"), Some(17)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(matches!(api_error(&body), AppError::InvalidInput(m) if m.contains("larger than")));
    }

    #[tokio::test]
    async fn an_undeclared_body_over_the_limit_is_refused_while_reading() {
        let (status, body) = send("/api/echo", Body::from("x".repeat(17)), None).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(matches!(api_error(&body), AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn a_body_that_stalls_past_the_deadline_is_a_timeout() {
        let (mut tx, stalled) = http_body_util::Channel::<Bytes>::new(1);
        tx.send_data(Bytes::from_static(b"part")).await.unwrap();
        // `tx` stays alive, so the body never ends.
        let (status, body) = send("/api/echo", Body::new(stalled), None).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        assert!(
            matches!(api_error(&body), AppError::InvalidInput(m) if m.contains("not received"))
        );
        drop(tx);
    }

    #[tokio::test]
    async fn the_deadline_covers_the_whole_body_not_each_chunk() {
        // Every chunk arrives well within the deadline; the total does not.
        let (mut tx, trickle) = http_body_util::Channel::<Bytes>::new(1);
        let sender = tokio::spawn(async move {
            for _ in 0..10 {
                if tx.send_data(Bytes::from_static(b"x")).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let (status, _) = send("/api/echo", Body::new(trickle), None).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        sender.abort();
    }

    #[tokio::test]
    async fn non_api_paths_get_a_bare_status() {
        let (status, body) = send("/page", Body::from("x".repeat(17)), None).await;
        assert_eq!((status, body.as_str()), (StatusCode::PAYLOAD_TOO_LARGE, ""));
    }
}
