//! `/llm/*` on the gateway port: guests' model requests, authenticated with
//! their per-boot workspace token and forwarded to the host-side model
//! gateway (Bifrost, see [`crate::bifrost`]), responses streamed back.
//!
//! The token arrives as `x-api-key` (what Anthropic SDKs send) or a bearer
//! token, and is never forwarded. Only the inference routes are reachable
//! (not Bifrost's admin API), and Bifrost's own `x-bf-*` control headers are
//! stripped so a guest can't change how it is served.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mcp::TokenAuthority;

use crate::bifrost::ModelGateway;

/// Largest request body accepted. Conversations with images can be large;
/// this still keeps one request from buffering unbounded memory.
pub(super) const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Path prefixes (after `/llm`) that are forwarded. Everything else -
/// notably Bifrost's `/api`, `/metrics` and dashboard - is refused.
const ALLOWED_PREFIXES: &[&str] = &["/anthropic/v1/"];

/// Request headers never forwarded: the guest's credential, hop-by-hop
/// headers, and ones the client re-derives.
const DROPPED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    // Responses are streamed through as-is; keep them uncompressed.
    "accept-encoding",
];

/// Response headers not copied back (re-derived by our own server).
const DROPPED_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
];

#[derive(Clone)]
struct Llm {
    authority: Arc<dyn TokenAuthority>,
    gateway: Arc<ModelGateway>,
    client: reqwest::Client,
}

/// The `/llm` router. `authority` resolves workspace tokens; `gateway`
/// says where requests go (looked up per request, so settings changes
/// apply at once).
pub(super) fn router(authority: Arc<dyn TokenAuthority>, gateway: Arc<ModelGateway>) -> Router {
    // No overall timeout: a streamed response legitimately runs for minutes.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default();
    Router::new().fallback(forward).with_state(Llm {
        authority,
        gateway,
        client,
    })
}

/// The workspace token from `x-api-key` or `Authorization: Bearer`.
fn presented_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(key) = headers.get("x-api-key") {
        return key.to_str().ok().map(str::trim).filter(|k| !k.is_empty());
    }
    mcp::bearer_token_from_headers(headers)
}

/// Whether `path` is an allowed inference route: under an allowed prefix,
/// only plain path characters, no `.`/`..` segments.
fn is_allowed_path(path: &str) -> bool {
    ALLOWED_PREFIXES.iter().any(|p| path.starts_with(p))
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.@:".contains(&b))
        && !path.split('/').any(|seg| seg == "." || seg == "..")
}

fn is_forwardable_request_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    !DROPPED_REQUEST_HEADERS.contains(&name) && !name.starts_with("x-bf-")
}

/// An error in the Anthropic API's shape, so the guest's SDK shows the
/// message rather than a parse failure.
fn error(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": kind, "message": format!("GAVMO: {message}") },
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn forward(State(llm): State<Llm>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let Some(scope) = presented_token(&parts.headers).and_then(|t| llm.authority.resolve(t)) else {
        return error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing or invalid workspace token",
        );
    };
    let path = parts.uri.path();
    if !is_allowed_path(path) {
        return error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "no such model route",
        );
    }
    let base = match llm.gateway.upstream().base_url() {
        Ok(base) => base,
        Err(why) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                &format!("the model gateway is unavailable: {why}"),
            );
        }
    };
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body too large or unreadable",
        );
    };

    let query = parts
        .uri
        .query()
        .map_or_else(String::new, |q| format!("?{q}"));
    let mut upstream = llm
        .client
        .request(parts.method, format!("{base}{path}{query}"))
        .body(body);
    for (name, value) in &parts.headers {
        if is_forwardable_request_header(name) {
            upstream = upstream.header(name, value);
        }
    }
    let response = match upstream.send().await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(workspace = %scope.workspace_id, error = %e, "model gateway request failed");
            let message = if e.is_connect() {
                "the model gateway is not running yet (it may still be starting); try again shortly"
            } else {
                "the model gateway request failed"
            };
            return error(StatusCode::BAD_GATEWAY, "api_error", message);
        }
    };
    tracing::debug!(workspace = %scope.workspace_id, %path, status = %response.status(), "model request");

    let mut out = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if !DROPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
            out = out.header(name, value);
        }
    }
    out.body(stream_body(response)).unwrap_or_else(|_| {
        error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "bad upstream response",
        )
    })
}

/// The response body, streamed chunk by chunk as it arrives. (Not
/// reqwest's `stream` feature: that would add a duplicate `wasm-streams` to
/// the lockfile.) A task pulls chunks into a small channel; if the guest
/// goes away the channel closes, the task drops the response, and the
/// upstream request is cancelled.
fn stream_body(mut response: reqwest::Response) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        loop {
            let next = response.chunk().await.transpose();
            let Some(item) = next else { return };
            let failed = item.is_err();
            if tx.send(item).await.is_err() || failed {
                return;
            }
        }
    });
    Body::new(ChannelBody(rx))
}

struct ChannelBody(tokio::sync::mpsc::Receiver<reqwest::Result<axum::body::Bytes>>);

impl http_body::Body for ChannelBody {
    type Data = axum::body::Bytes;
    type Error = reqwest::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|item| item.map(|chunk| chunk.map(http_body::Frame::data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use mcp::WorkspaceScope;
    use tower::ServiceExt as _;

    struct OneToken;
    impl TokenAuthority for OneToken {
        fn resolve(&self, token: &str) -> Option<WorkspaceScope> {
            (token == "good").then(|| WorkspaceScope {
                workspace_id: "/w".to_owned(),
            })
        }
    }

    /// A fake Bifrost that echoes what it received: method, path, query and
    /// headers in response headers, the body as the body.
    async fn fake_upstream() -> String {
        let app = Router::new().fallback(|request: Request| async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let mut seen = Vec::new();
            for (k, v) in &parts.headers {
                seen.push(format!("{k}={}", v.to_str().unwrap()));
            }
            Response::builder()
                .status(201)
                .header("x-seen-uri", parts.uri.to_string())
                .header("x-seen-method", parts.method.as_str())
                .header("x-seen-headers", seen.join(";"))
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    use crate::bifrost::Upstream;

    fn app(upstream: Upstream) -> Router {
        router(Arc::new(OneToken), Arc::new(ModelGateway::fixed(upstream)))
    }

    async fn send(app: Router, request: HttpRequest<Body>) -> (StatusCode, HeaderMap, String) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    fn post(path: &str) -> axum::http::request::Builder {
        HttpRequest::post(path).header("content-type", "application/json")
    }

    async fn upstream_at(url: &str) -> Upstream {
        Upstream::Fixed(url.to_owned())
    }

    #[tokio::test]
    async fn requests_are_forwarded_with_the_token_and_control_headers_stripped() {
        let base = fake_upstream().await;
        let request = post("/anthropic/v1/messages?beta=true")
            .header("x-api-key", "good")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "thinking-binding-controls-2026-08-01")
            .header("x-bf-direct-key", "true")
            .header("accept-encoding", "gzip")
            .body(Body::from(r#"{"model":"m"}"#))
            .unwrap();

        let (status, headers, body) = send(app(upstream_at(&base).await), request).await;

        assert_eq!(status, 201);
        assert_eq!(body, r#"{"model":"m"}"#);
        assert_eq!(headers["content-type"], "text/event-stream");
        assert_eq!(headers["x-seen-uri"], "/anthropic/v1/messages?beta=true");
        assert_eq!(headers["x-seen-method"], "POST");
        let seen = headers["x-seen-headers"].to_str().unwrap();
        assert!(seen.contains("anthropic-version=2023-06-01"), "{seen}");
        assert!(
            seen.contains("anthropic-beta=thinking-binding-controls-2026-08-01"),
            "{seen}"
        );
        for gone in [
            "x-api-key",
            "x-bf-direct-key",
            "accept-encoding",
            "authorization",
        ] {
            assert!(!seen.contains(&format!("{gone}=")), "{gone} leaked: {seen}");
        }
    }

    /// Chunks reach the guest as the upstream sends them, not at the end.
    #[tokio::test]
    async fn responses_are_streamed_as_they_arrive() {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let released = Arc::new(tokio::sync::Mutex::new(Some(released)));
        let upstream = Router::new().fallback(move || {
            let released = Arc::clone(&released);
            async move {
                let (tx, rx) = tokio::sync::mpsc::channel::<reqwest::Result<axum::body::Bytes>>(4);
                tokio::spawn(async move {
                    tx.send(Ok("event: one\n\n".into())).await.unwrap();
                    if let Some(released) = released.lock().await.take() {
                        released.await.unwrap();
                    }
                    tx.send(Ok("event: two\n\n".into())).await.unwrap();
                });
                Body::new(ChannelBody(rx))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let request = post("/anthropic/v1/messages")
            .header("x-api-key", "good")
            .body(Body::empty())
            .unwrap();
        let response = app(upstream_at(&base).await)
            .oneshot(request)
            .await
            .unwrap();
        let mut body = response.into_body();
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            http_body_util::BodyExt::frame(&mut body),
        )
        .await
        .expect("the first chunk must arrive before the upstream finishes")
        .unwrap()
        .unwrap();
        assert_eq!(first.into_data().unwrap(), "event: one\n\n");
        release.send(()).unwrap();
        let rest = http_body_util::BodyExt::collect(body)
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(rest, "event: two\n\n");
    }

    #[tokio::test]
    async fn a_bearer_token_works_too() {
        let base = fake_upstream().await;
        let request = post("/anthropic/v1/messages")
            .header("authorization", "Bearer good")
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send(app(upstream_at(&base).await), request).await;
        assert_eq!(status, 201);
        assert!(
            !headers["x-seen-headers"]
                .to_str()
                .unwrap()
                .contains("authorization=")
        );
    }

    #[tokio::test]
    async fn requests_without_a_valid_token_are_refused() {
        let base = fake_upstream().await;
        for token in [None, Some("bad"), Some("")] {
            let mut request = post("/anthropic/v1/messages");
            if let Some(t) = token {
                request = request.header("x-api-key", t);
            }
            let (status, _, body) = send(
                app(upstream_at(&base).await),
                request.body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(status, 401, "{token:?}");
            assert!(body.contains("authentication_error"), "{body}");
        }
    }

    #[tokio::test]
    async fn only_inference_routes_are_reachable() {
        let base = fake_upstream().await;
        for path in [
            "/api/config",
            "/metrics",
            "/",
            "/v1/models",
            "/anthropic/v1/../../api/config",
            "/anthropic/v1/%2e%2e/api",
        ] {
            let request = post(path)
                .header("x-api-key", "good")
                .body(Body::empty())
                .unwrap();
            let (status, _, _) = send(app(upstream_at(&base).await), request).await;
            assert_eq!(status, 404, "{path}");
        }
        assert!(is_allowed_path("/anthropic/v1/messages"));
        assert!(is_allowed_path("/anthropic/v1/messages/count_tokens"));
    }

    #[tokio::test]
    async fn an_unconfigured_or_unreachable_gateway_explains_itself() {
        let request = || {
            post("/anthropic/v1/messages")
                .header("x-api-key", "good")
                .body(Body::empty())
                .unwrap()
        };
        let (status, _, body) = send(
            app(Upstream::NotConfigured(
                "GAVMO_BIFROST is not set".to_owned(),
            )),
            request(),
        )
        .await;
        assert_eq!(status, 503);
        assert!(body.contains("GAVMO_BIFROST is not set"), "{body}");

        // A port nothing listens on.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let (status, _, body) = send(app(Upstream::Fixed(url)), request()).await;
        assert_eq!(status, 502);
        assert!(body.contains("not running yet"), "{body}");
    }

    /// Live: the real supervisor starts a real Bifrost, and a streamed
    /// request through this route reaches Vertex. Needs network and
    /// credentials, so opt-in:
    ///
    /// ```sh
    /// GAVMO_TEST_BIFROST=/path/to/bifrost-http GAVMO_TEST_GCP_PROJECT=my-project \
    /// GAVMO_TEST_ADC=~/.config/gcloud/application_default_credentials.json \
    ///   cargo test -p gavmo --features ssr live_bifrost -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs a bifrost-http binary, network and Google credentials"]
    async fn live_bifrost_serves_a_streamed_request_through_the_route() {
        use crate::bifrost::{BifrostConfig, Supervisor};
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let tmp = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::start(BifrostConfig {
            binary: var("GAVMO_TEST_BIFROST").into(),
            app_dir: tmp.path().join("bifrost"),
            providers: vec![crate::providers::vertex::block(
                &var("GAVMO_TEST_GCP_PROJECT"),
                "global",
                std::path::Path::new(&var("GAVMO_TEST_ADC")),
            )],
            data_dir: std::env::var("GAVMO_TEST_BIFROST_DATA_DIR")
                .ok()
                .map(Into::into),
            port: None,
        })
        .unwrap();
        let app = app(Upstream::Bifrost(Arc::clone(&supervisor)));
        let body = r#"{"model":"vertex/claude-haiku-5-5@default","max_tokens":30,"stream":true,
            "messages":[{"role":"user","content":"Reply with exactly: pong"}]}"#;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let (status, headers, text) = loop {
            let request = post("/anthropic/v1/messages")
                .header("x-api-key", "good")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(body))
                .unwrap();
            let result = send(app.clone(), request).await;
            // 502 while Bifrost is still starting.
            if result.0 != StatusCode::BAD_GATEWAY || tokio::time::Instant::now() > deadline {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        eprintln!("{status}\n{text}");
        if status != 200 {
            let log = std::fs::read_to_string(tmp.path().join("bifrost").join("bifrost.log"));
            eprintln!("bifrost.log:\n{}", log.unwrap_or_default());
        }
        assert_eq!(status, 200, "{text}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        assert!(
            text.contains("message_stop") && text.to_lowercase().contains("pong"),
            "{text}"
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused() {
        let base = fake_upstream().await;
        let request = post("/anthropic/v1/messages")
            .header("x-api-key", "good")
            .body(Body::from(vec![b'x'; MAX_REQUEST_BODY_BYTES + 1]))
            .unwrap();
        let (status, _, _) = send(app(upstream_at(&base).await), request).await;
        assert_eq!(status, 413);
    }
}
