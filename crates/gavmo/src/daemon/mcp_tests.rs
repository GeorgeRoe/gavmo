//! End-to-end tests of the MCP gateway against the real `AppState`: a
//! workspace opened through the control-panel API (fake launcher) mints a
//! token, and guest-style JSON-RPC calls go through [`super::mcp_router`] to
//! a local upstream server.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Redirect, Response};
use leptos::server_fn::ServerFn;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::tests::Daemon;
use crate::api;
use crate::types::AppError;

/// Every request an [`Upstream`] received: (path, headers).
type Seen = Arc<Mutex<Vec<(String, HeaderMap)>>>;

/// A local HTTP server standing in for a tool's API. `/go` redirects to
/// `redirect_to`; every other path answers `200 hello`.
struct Upstream {
    url: String,
    seen: Seen,
}

impl Upstream {
    async fn start(redirect_to: Option<String>) -> Self {
        let seen = Seen::default();
        let state = (Arc::clone(&seen), redirect_to);
        let app = Router::new().fallback(record).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, seen }
    }

    fn requests(&self) -> Vec<(String, HeaderMap)> {
        self.seen.lock().unwrap().clone()
    }
}

async fn record(
    State((seen, redirect_to)): State<(Seen, Option<String>)>,
    req: Request<Body>,
) -> Response {
    let path = req.uri().path().to_owned();
    seen.lock()
        .unwrap()
        .push((path.clone(), req.headers().clone()));
    match redirect_to {
        Some(target) if path == "/go" => Redirect::temporary(&target).into_response(),
        _ => "hello".into_response(),
    }
}

/// A bearer-auth tool `api` pointing at `base_url`.
fn bearer_tool(base_url: &str) -> String {
    format!(
        "tools:\n  api:\n    type: http\n    base_url: {base_url}\n    auth: bearer\n    secret: s\n"
    )
}

/// The same tool, but sending its credential in `X-Api-Key`.
fn header_tool(base_url: &str) -> String {
    format!(
        "tools:\n  api:\n    type: http\n    base_url: {base_url}\n    auth: header\n    header_name: X-Api-Key\n    secret: s\n"
    )
}

impl Daemon {
    /// The token the daemon minted into the workspace's MCP share dir, as the
    /// guest reads it.
    fn mcp_token(&self, id: &str) -> String {
        let hash = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(id.as_bytes())
        );
        let env =
            std::fs::read_to_string(self.data.path().join("mcp").join(hash).join("token.env"))
                .unwrap();
        env.lines()
            .find_map(|line| line.strip_prefix("GAVMO_MCP_TOKEN="))
            .unwrap()
            .to_owned()
    }

    async fn save_secret(&self, id: &str, token: &str) {
        self.call::<()>(
            api::SetToolSecret::PATH,
            &[("id", id), ("tool_id", "api"), ("token", token)],
        )
        .await
        .unwrap();
    }

    /// POSTs one JSON-RPC message to the gateway the way the guest does.
    async fn rpc(&self, token: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "10.0.2.2:7071")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = super::mcp_router(&self.state).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 4 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn tool_names(&self, token: &str) -> Vec<String> {
        let (status, body) = self
            .rpc(
                token,
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Calls the `api` tool; returns the JSON-RPC `result` (or `error`).
    async fn call_api(&self, token: &str, path: &str) -> Value {
        let (status, body) = self
            .rpc(
                token,
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "api", "arguments": {"method": "GET", "path": path}}
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }
}

/// The text of a tool result, and whether it was a tool-level error.
fn tool_text(body: &Value) -> (String, bool) {
    let result = &body["result"];
    (
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        result["isError"].as_bool().unwrap_or(false),
    )
}

#[tokio::test]
async fn opening_mints_a_token_that_lists_the_workspace_s_tools() {
    let d = Daemon::start().await;
    let upstream = Upstream::start(None).await;
    let (repo, id) = d.add_repo().await;
    std::fs::write(repo.path().join("gavmo.yaml"), bearer_tool(&upstream.url)).unwrap();
    d.save_secret(&id, "tok").await;
    d.open_running(&id).await;

    let token = d.mcp_token(&id);
    assert_eq!(d.tool_names(&token).await, ["gavmo_ping", "api"]);

    let (status, ping) = d
        .rpc(
            &token,
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "gavmo_ping", "arguments": {}}
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tool_text(&ping).0, format!("pong from workspace {id}"));
}

#[tokio::test]
async fn a_tool_without_a_credential_is_not_listed() {
    let d = Daemon::start().await;
    let (repo, id) = d.add_repo().await;
    std::fs::write(
        repo.path().join("gavmo.yaml"),
        bearer_tool("http://127.0.0.1:9"),
    )
    .unwrap();
    d.open_running(&id).await;
    assert_eq!(d.tool_names(&d.mcp_token(&id)).await, ["gavmo_ping"]);
}

#[tokio::test]
async fn tools_call_forwards_with_the_bound_credential() {
    let d = Daemon::start().await;
    let upstream = Upstream::start(None).await;
    let (repo, id) = d.add_repo().await;
    std::fs::write(repo.path().join("gavmo.yaml"), bearer_tool(&upstream.url)).unwrap();
    d.save_secret(&id, "tok-123").await;
    d.open_running(&id).await;

    let body = d.call_api(&d.mcp_token(&id), "/me").await;
    assert_eq!(tool_text(&body), ("HTTP 200\n\nhello".to_owned(), false));
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/me");
    assert_eq!(requests[0].1[header::AUTHORIZATION], "Bearer tok-123");
}

/// Approved for one URL, then `gavmo.yaml` moved the tool elsewhere before
/// the boot: the tool isn't advertised, can't be called, and neither server
/// sees the credential.
#[tokio::test]
async fn a_credential_approved_for_another_url_is_not_advertised() {
    let d = Daemon::start().await;
    let approved = Upstream::start(None).await;
    let other = Upstream::start(None).await;
    let (repo, id) = d.add_repo().await;
    let yaml = repo.path().join("gavmo.yaml");
    std::fs::write(&yaml, bearer_tool(&approved.url)).unwrap();
    d.save_secret(&id, "tok").await;
    std::fs::write(&yaml, bearer_tool(&other.url)).unwrap();
    d.open_running(&id).await;

    let token = d.mcp_token(&id);
    assert_eq!(d.tool_names(&token).await, ["gavmo_ping"]);
    let body = d.call_api(&token, "/me").await;
    assert_eq!(body["error"]["code"], -32602, "{body}");
    assert!(approved.requests().is_empty() && other.requests().is_empty());
}

/// The forwarder's own check, behind the boot-time filter: while the VM
/// runs, the credential is re-approved for another URL (the running tool
/// still points at the first), so it must be withheld at call time.
#[tokio::test]
async fn a_credential_rebound_to_another_url_while_running_is_withheld() {
    let d = Daemon::start().await;
    let running = Upstream::start(None).await;
    let other = Upstream::start(None).await;
    let (repo, id) = d.add_repo().await;
    let yaml = repo.path().join("gavmo.yaml");
    std::fs::write(&yaml, bearer_tool(&running.url)).unwrap();
    d.save_secret(&id, "tok").await;
    d.open_running(&id).await;

    std::fs::write(&yaml, bearer_tool(&other.url)).unwrap();
    d.save_secret(&id, "tok").await;

    let (text, is_error) = tool_text(&d.call_api(&d.mcp_token(&id), "/me").await);
    assert!(is_error && text.contains("was approved for"), "{text}");
    assert!(running.requests().is_empty() && other.requests().is_empty());
}

/// Same, for the delivery: approved as a bearer token, re-approved as a
/// custom header while the VM (still sending bearer) runs.
#[tokio::test]
async fn a_credential_approved_for_another_delivery_is_withheld() {
    let d = Daemon::start().await;
    let upstream = Upstream::start(None).await;
    let (repo, id) = d.add_repo().await;
    let yaml = repo.path().join("gavmo.yaml");
    std::fs::write(&yaml, bearer_tool(&upstream.url)).unwrap();
    d.save_secret(&id, "tok").await;
    d.open_running(&id).await;

    std::fs::write(&yaml, header_tool(&upstream.url)).unwrap();
    d.save_secret(&id, "tok").await;

    let (text, is_error) = tool_text(&d.call_api(&d.mcp_token(&id), "/me").await);
    assert!(
        is_error && text.contains("approved to be sent as"),
        "{text}"
    );
    assert_eq!(upstream.requests(), Vec::new());
}

#[tokio::test]
async fn redirects_are_reported_not_followed() {
    let d = Daemon::start().await;
    let elsewhere = Upstream::start(None).await;
    let origin = Upstream::start(Some(format!("{}/steal", elsewhere.url))).await;
    let (repo, id) = d.add_repo().await;
    std::fs::write(repo.path().join("gavmo.yaml"), bearer_tool(&origin.url)).unwrap();
    d.save_secret(&id, "tok").await;
    d.open_running(&id).await;

    let (text, is_error) = tool_text(&d.call_api(&d.mcp_token(&id), "/go").await);
    assert!(is_error && text.starts_with("HTTP 307"), "{text}");
    assert!(
        text.contains(&format!("Location: {}/steal", elsewhere.url)),
        "{text}"
    );
    assert_eq!(origin.requests().len(), 1);
    assert_eq!(elsewhere.requests(), Vec::new());
}

#[tokio::test]
async fn after_stop_the_old_token_is_rejected() {
    let d = Daemon::start().await;
    let (_repo, id) = d.add_repo().await;
    d.open_running(&id).await;
    let token = d.mcp_token(&id);
    assert_eq!(d.tool_names(&token).await, ["gavmo_ping"]);

    d.call::<()>(api::StopWorkspace::PATH, &[("id", &id)])
        .await
        .unwrap();
    let (status, _) = d
        .rpc(
            &token,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A new boot mints a new token; the old one stays dead.
    d.open_running(&id).await;
    let fresh = d.mcp_token(&id);
    assert_ne!(fresh, token);
    assert_eq!(d.tool_names(&fresh).await, ["gavmo_ping"]);
    let (status, _) = d
        .rpc(
            &token,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The panel's "Test" button: a `GET` of the tool's `test_path` with the
/// stored credential, reported as success or failure.
#[tokio::test]
async fn the_test_button_calls_the_tool_with_its_credential() {
    let d = Daemon::start().await;
    let upstream = Upstream::start(None).await;
    let (repo, id) = d.add_repo().await;
    let yaml = format!("{}    test_path: /me\n", bearer_tool(&upstream.url));
    std::fs::write(repo.path().join("gavmo.yaml"), yaml).unwrap();
    d.set_token(&id, "tok").await;

    let text: String = d
        .call(api::TestTool::PATH, &[("id", &id), ("tool_id", "api")])
        .await
        .unwrap();

    assert!(text.contains("hello"), "{text}");
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/me");
    assert_eq!(requests[0].1[header::AUTHORIZATION], "Bearer tok");
}

#[tokio::test]
async fn the_test_button_reports_an_unreachable_tool_and_a_missing_credential() {
    let d = Daemon::start().await;
    let (repo, id) = d.add_repo().await;
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    std::fs::write(repo.path().join("gavmo.yaml"), bearer_tool(&closed)).unwrap();
    let test = async || {
        d.call::<String>(api::TestTool::PATH, &[("id", &id), ("tool_id", "api")])
            .await
    };

    let missing = test().await;
    assert!(missing.is_err(), "{missing:?}");

    d.set_token(&id, "tok").await;
    let unreachable = test().await;
    assert!(
        matches!(unreachable, Err(AppError::Failed(_))),
        "{unreachable:?}"
    );
}

/// A stored credential whose binding no longer matches `gavmo.yaml` shows
/// as such in the panel, saying what it was approved for.
#[tokio::test]
async fn the_tool_list_explains_a_changed_destination_or_delivery() {
    let d = Daemon::start().await;
    let (repo, id) = d.add_repo().await;
    let yaml = repo.path().join("gavmo.yaml");
    std::fs::write(&yaml, bearer_tool("https://a.example.com")).unwrap();
    d.set_token(&id, "tok").await;

    std::fs::write(&yaml, bearer_tool("https://b.example.com")).unwrap();
    let tool = d.only_tool(&id).await;
    assert!(!tool.configured && tool.url_changed, "{tool:?}");
    assert_eq!(
        tool.bound_url.as_deref(),
        Some("https://a.example.com:443/")
    );

    std::fs::write(&yaml, header_tool("https://a.example.com")).unwrap();
    let tool = d.only_tool(&id).await;
    assert!(!tool.configured && !tool.url_changed, "{tool:?}");
    assert_eq!(
        tool.approved_delivery.as_deref(),
        Some("a Bearer token in the Authorization header")
    );
}

/// Basic auth needs a username as well as the password/token.
#[tokio::test]
async fn basic_auth_credentials_need_a_username() {
    let d = Daemon::start().await;
    let (repo, id) = d.add_repo().await;
    std::fs::write(
        repo.path().join("gavmo.yaml"),
        "tools:\n  api:\n    type: http\n    base_url: https://a.example.com\n    auth: basic\n    secret: s\n",
    )
    .unwrap();
    let set = async |username: &str| {
        d.call::<()>(
            api::SetToolSecret::PATH,
            &[
                ("id", &id),
                ("tool_id", "api"),
                ("username", username),
                ("token", "pw"),
            ],
        )
        .await
    };

    assert!(matches!(set("").await, Err(AppError::InvalidInput(_))));
    assert!(!d.only_tool(&id).await.configured);
    set("me@example.com").await.unwrap();
    assert!(d.only_tool(&id).await.configured);
}

impl Daemon {
    async fn set_token(&self, id: &str, token: &str) {
        self.call::<()>(
            api::SetToolSecret::PATH,
            &[("id", id), ("tool_id", "api"), ("token", token)],
        )
        .await
        .unwrap();
    }

    async fn only_tool(&self, id: &str) -> crate::types::ToolConfig {
        let mut list: crate::types::ToolList = self
            .call(api::ListToolConfigs::PATH, &[("id", id)])
            .await
            .unwrap();
        assert_eq!(list.tools.len(), 1, "{list:?}");
        list.tools.remove(0)
    }
}
