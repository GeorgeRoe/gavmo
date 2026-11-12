//! GAVMO's host-side MCP tool gateway: an MCP server over Streamable HTTP
//! with every request gated by a per-workspace bearer token.
//!
//! Pure mechanism: workspaces, secrets and tools belong to the daemon, which
//! plugs in via [`TokenAuthority`] and [`ToolExecutor`] so this crate never
//! depends on `gavmo`.
//!
//! Authentication happens in front of the MCP machinery ([`Gateway`]): a
//! request without a valid token gets HTTP 401 with `WWW-Authenticate:
//! Bearer` and never reaches rmcp. An authorized request carries its
//! [`WorkspaceScope`] in the request extensions, which the handler reads.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::BodyExt as _;
use http_body_util::combinators::BoxBody;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};

/// Default host port of the gateway (the daemon's `--mcp-port`); the same for
/// every workspace (only the token differs). Guests learn the actual port from
/// their token share (`GAVMO_HOST_PORT`), so the image doesn't hardcode it.
pub const DEFAULT_MCP_PORT: u16 = 7071;

/// Largest JSON-RPC request body accepted. Tool arguments are small; this
/// keeps one request from making the daemon buffer megabytes.
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// What a validated bearer token authorizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceScope {
    pub workspace_id: String,
}

/// Resolves a bearer token to the workspace it authorizes. Token minting and
/// expiry are the implementor's concern. Synchronous because resolution is
/// expected to be a cheap in-memory lookup.
pub trait TokenAuthority: Send + Sync + 'static {
    /// `Some` only for a currently-valid token; unknown, stale or malformed
    /// tokens return `None`, treated the same as no token at all.
    fn resolve(&self, token: &str) -> Option<WorkspaceScope>;
}

/// Extracts the bearer token from the `Authorization` header: present, valid
/// UTF-8, the `Bearer` scheme (case-insensitive, per RFC 9110) and a
/// non-empty token. Kept free of rmcp types for unit testing.
pub fn bearer_token_from_headers(headers: &http::HeaderMap) -> Option<&str> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// A transport-neutral tool description. Not an `rmcp::model::Tool`, so the
/// daemon implementing [`ToolExecutor`] needs no `rmcp` dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    /// The tool name the agent calls (e.g. `"gavmo_ping"`).
    pub name: String,
    /// Description shown in `tools/list`.
    pub description: String,
    /// The tool's JSON-Schema input object.
    pub input_schema: serde_json::Map<String, serde_json::Value>,
}

/// Transport-neutral result of a tool call. `Error` becomes a tool-level error
/// result the caller sees, not a protocol error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    Success(String),
    Error(String),
}

/// Runs the tools a workspace is authorized for, keeping credential lookup and
/// forwarding out of this crate. `call_tool` is only invoked for names
/// `has_tool` accepts for the same scope.
pub trait ToolExecutor: Send + Sync + 'static {
    /// The tools available to `scope`, in `tools/list` order (may be empty).
    fn list_tools(&self, scope: &WorkspaceScope) -> Vec<ToolDescriptor>;

    /// Whether `scope` may call `name`. The default builds the whole list;
    /// implementors should override it with a direct lookup.
    fn has_tool(&self, scope: &WorkspaceScope, name: &str) -> bool {
        self.list_tools(scope).iter().any(|tool| tool.name == name)
    }

    /// Executes `name` for `scope` with the given JSON arguments.
    fn call_tool(
        &self,
        scope: &WorkspaceScope,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> impl Future<Output = ToolOutcome> + Send;
}

fn descriptor_to_tool(descriptor: ToolDescriptor) -> Tool {
    Tool::new(
        descriptor.name,
        descriptor.description,
        Arc::new(descriptor.input_schema),
    )
}

/// The MCP `ServerHandler`, constructed fresh per request. Requests only get
/// here through [`Gateway`], which has already authorized them.
struct GavmoMcpHandler<A> {
    backend: A,
}

/// The scope [`Gateway`] attached to this request. `None` would mean a
/// request bypassed the gateway; handlers then refuse it.
fn scope_of(context: &RequestContext<RoleServer>) -> Option<WorkspaceScope> {
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<WorkspaceScope>())
        .cloned()
}

fn not_authorized() -> McpError {
    McpError::invalid_request("missing workspace authorization", None)
}

impl<A: ToolExecutor> ServerHandler for GavmoMcpHandler<A> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "gavmo-tools",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Host-side tool gateway for this GAVMO workspace. Available tools \
                 depend on what the repository's gavmo.yaml enables and what \
                 secrets the user has configured in the GAVMO control panel.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let scope = scope_of(&context).ok_or_else(not_authorized)?;
        let tools = self
            .backend
            .list_tools(&scope)
            .into_iter()
            .map(descriptor_to_tool)
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    /// Unknown tool names are a protocol error (invalid params), rejected
    /// before the executor runs; a tool that ran and failed is a tool-level
    /// error result.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let scope = scope_of(&context).ok_or_else(not_authorized)?;
        let name = request.name.as_ref();
        if !self.backend.has_tool(&scope, name) {
            return Err(McpError::invalid_params(
                format!("unknown tool: {name}"),
                None,
            ));
        }
        let arguments = request.arguments.unwrap_or_default();
        let result = match self.backend.call_tool(&scope, name, arguments).await {
            ToolOutcome::Success(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            ToolOutcome::Error(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
        };
        Ok(result.into())
    }
}

/// The host's address as seen from a QEMU user-net guest, and so the `Host`
/// header on every guest request to the gateway.
const QEMU_USER_NET_HOST_ADDR: &str = "10.0.2.2";

/// The gateway as a `tower` service, to mount at `/mcp`: bearer-token auth in
/// front of rmcp's Streamable HTTP server. Opaque, so rmcp types don't leak
/// into the daemon.
#[derive(Clone)]
pub struct Gateway<A: TokenAuthority + ToolExecutor + Clone> {
    authority: A,
    inner: StreamableHttpService<GavmoMcpHandler<A>, LocalSessionManager>,
}

/// The gateway's response type.
pub type GatewayResponse = http::Response<BoxBody<Bytes, Infallible>>;

/// Builds the gateway.
///
/// Stateless with plain JSON responses: each call carries its own token, so
/// sessions buy nothing. rmcp's default `Host` allowlist is loopback-only and
/// 403s guest requests, so the QEMU host address is added alongside loopback.
/// Requests carrying an `Origin` header (i.e. from a browser page, such as a
/// DNS-rebinding attack on the host) are refused: guests never send one.
pub fn service<A>(backend: A) -> Gateway<A>
where
    A: TokenAuthority + ToolExecutor + Clone,
{
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(["localhost", "127.0.0.1", "::1", QEMU_USER_NET_HOST_ADDR])
        .enforce_origin_validation()
        .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES);

    let handler_backend = backend.clone();
    let inner = StreamableHttpService::new(
        move || {
            Ok(GavmoMcpHandler {
                backend: handler_backend.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Gateway {
        authority: backend,
        inner,
    }
}

/// `401` with `WWW-Authenticate: Bearer` (RFC 6750), as MCP's HTTP auth
/// expects for a missing or invalid token.
fn unauthorized() -> GatewayResponse {
    let mut response = http::Response::new(
        http_body_util::Full::new(Bytes::from_static(b"missing or invalid bearer token")).boxed(),
    );
    *response.status_mut() = http::StatusCode::UNAUTHORIZED;
    response.headers_mut().insert(
        http::header::WWW_AUTHENTICATE,
        http::HeaderValue::from_static("Bearer realm=\"gavmo\""),
    );
    response
}

impl<A, B> tower_service::Service<http::Request<B>> for Gateway<A>
where
    A: TokenAuthority + ToolExecutor + Clone,
    StreamableHttpService<GavmoMcpHandler<A>, LocalSessionManager>: tower_service::Service<
            http::Request<B>,
            Response = GatewayResponse,
            Error = Infallible,
            Future = Pin<Box<dyn Future<Output = Result<GatewayResponse, Infallible>> + Send>>,
        >,
{
    type Response = GatewayResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<GatewayResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let scope =
            bearer_token_from_headers(req.headers()).and_then(|t| self.authority.resolve(t));
        match scope {
            Some(scope) => {
                req.extensions_mut().insert(scope);
                self.inner.call(req)
            }
            None => Box::pin(std::future::ready(Ok(unauthorized()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};

    #[test]
    fn extracts_token_from_well_formed_bearer_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc123"),
        );
        assert_eq!(bearer_token_from_headers(&headers), Some("abc123"));
    }

    #[test]
    fn missing_header_returns_none() {
        let headers = HeaderMap::new();
        assert_eq!(bearer_token_from_headers(&headers), None);
    }

    #[test]
    fn wrong_scheme_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic abc123"),
        );
        assert_eq!(bearer_token_from_headers(&headers), None);
    }

    #[test]
    fn empty_token_is_none() {
        for value in ["Bearer ", "Bearer", "Bearer    "] {
            let mut headers = HeaderMap::new();
            headers.insert(http::header::AUTHORIZATION, HeaderValue::from_static(value));
            assert_eq!(bearer_token_from_headers(&headers), None, "{value:?}");
        }
    }

    /// RFC 9110: auth scheme names are case-insensitive.
    #[test]
    fn scheme_is_case_insensitive_and_whitespace_tolerant() {
        for value in ["bearer abc", "BEARER abc", "Bearer  abc "] {
            let mut headers = HeaderMap::new();
            headers.insert(http::header::AUTHORIZATION, HeaderValue::from_static(value));
            assert_eq!(
                bearer_token_from_headers(&headers),
                Some("abc"),
                "{value:?}"
            );
        }
    }

    #[test]
    fn non_utf8_header_value_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert_eq!(bearer_token_from_headers(&headers), None);
    }

    #[test]
    fn descriptor_to_tool_preserves_name_description_and_schema() {
        let mut schema = serde_json::Map::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));
        let tool = descriptor_to_tool(ToolDescriptor {
            name: "gavmo_ping".to_string(),
            description: "pings".to_string(),
            input_schema: schema,
        });
        assert_eq!(tool.name.as_ref(), "gavmo_ping");
        assert_eq!(tool.description.as_deref(), Some("pings"));
        assert_eq!(
            tool.input_schema.get("type"),
            Some(&serde_json::json!("object"))
        );
    }

    /// Stand-in for the daemon's `AppState`; depending on `gavmo` here would be
    /// a circular dev-dependency.
    #[derive(Clone)]
    struct FakeAuthority(std::collections::HashMap<&'static str, &'static str>);

    impl TokenAuthority for FakeAuthority {
        fn resolve(&self, token: &str) -> Option<WorkspaceScope> {
            self.0.get(token).map(|id| WorkspaceScope {
                workspace_id: (*id).to_string(),
            })
        }
    }

    /// One tool, `echo`, available to every authorized workspace; it reports
    /// which workspace called it.
    impl ToolExecutor for FakeAuthority {
        fn list_tools(&self, _scope: &WorkspaceScope) -> Vec<ToolDescriptor> {
            let mut schema = serde_json::Map::new();
            schema.insert("type".to_owned(), serde_json::json!("object"));
            vec![ToolDescriptor {
                name: "echo".to_owned(),
                description: "echoes the workspace".to_owned(),
                input_schema: schema,
            }]
        }

        async fn call_tool(
            &self,
            scope: &WorkspaceScope,
            name: &str,
            arguments: serde_json::Map<String, serde_json::Value>,
        ) -> ToolOutcome {
            let text = format!("{name} from {}", scope.workspace_id);
            if arguments.contains_key("fail") {
                ToolOutcome::Error(text)
            } else {
                ToolOutcome::Success(text)
            }
        }
    }

    #[test]
    fn fake_authority_resolves_known_token_and_rejects_unknown() {
        let mut map = std::collections::HashMap::new();
        map.insert("good-token", "/home/user/project");
        let authority = FakeAuthority(map);

        assert_eq!(
            authority.resolve("good-token"),
            Some(WorkspaceScope {
                workspace_id: "/home/user/project".to_string()
            })
        );
        assert_eq!(authority.resolve("bad-token"), None);
    }

    /// rmcp's default `allowed_hosts` is loopback-only and 403s every guest
    /// request (`Host: 10.0.2.2:<port>`) before auth runs.
    #[test]
    fn service_allows_both_loopback_and_the_qemu_guest_host_address() {
        let authority = FakeAuthority(std::collections::HashMap::new());
        let svc = service(authority).inner;

        assert!(
            svc.config
                .allowed_hosts
                .iter()
                .any(|h| h == QEMU_USER_NET_HOST_ADDR),
            "allowed_hosts must include the guest-side host address: {:?}",
            svc.config.allowed_hosts
        );
        assert!(
            svc.config.allowed_hosts.iter().any(|h| h == "127.0.0.1"),
            "allowed_hosts must still include loopback for host-side testing: {:?}",
            svc.config.allowed_hosts
        );
    }

    // --- End to end, through the real gateway service ---

    use tower::ServiceExt as _;

    fn gateway() -> Gateway<FakeAuthority> {
        let mut map = std::collections::HashMap::new();
        map.insert("good-token", "/home/user/project");
        service(FakeAuthority(map))
    }

    /// POSTs one JSON-RPC message (stateless, JSON response mode).
    async fn rpc(
        token: Option<&str>,
        extra: &[(&str, &str)],
        body: serde_json::Value,
    ) -> (http::StatusCode, HeaderMap, serde_json::Value) {
        read(
            gateway()
                .oneshot(request(token, extra, &body))
                .await
                .unwrap(),
        )
        .await
    }

    fn request(
        token: Option<&str>,
        extra: &[(&str, &str)],
        body: &serde_json::Value,
    ) -> http::Request<http_body_util::Full<Bytes>> {
        let mut req = http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(http::header::HOST, "10.0.2.2:7071")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "application/json, text/event-stream");
        if let Some(token) = token {
            req = req.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        for (name, value) in extra {
            req = req.header(*name, *value);
        }
        req.body(http_body_util::Full::new(Bytes::from(body.to_string())))
            .unwrap()
    }

    async fn read(res: GatewayResponse) -> (http::StatusCode, HeaderMap, serde_json::Value) {
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, headers, json)
    }

    fn initialize() -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        })
    }

    #[tokio::test]
    async fn requests_without_a_valid_token_get_401_and_never_reach_mcp() {
        for token in [None, Some("bad-token")] {
            let (status, headers, _) = rpc(token, &[], initialize()).await;
            assert_eq!(status, http::StatusCode::UNAUTHORIZED, "{token:?}");
            assert!(
                headers
                    .get(http::header::WWW_AUTHENTICATE)
                    .is_some_and(|v| v.to_str().unwrap().starts_with("Bearer")),
                "{headers:?}"
            );
        }
    }

    #[tokio::test]
    async fn initialize_succeeds_with_a_valid_token() {
        let (status, _, body) = rpc(Some("good-token"), &[], initialize()).await;
        assert_eq!(status, http::StatusCode::OK, "{body}");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["serverInfo"]["name"], "gavmo-tools");
    }

    #[tokio::test]
    async fn tools_list_and_call_are_scoped_to_the_token_s_workspace() {
        let (status, _, list) = rpc(
            Some("good-token"),
            &[],
            serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, http::StatusCode::OK, "{list}");
        assert_eq!(list["result"]["tools"][0]["name"], "echo");

        let (_, _, call) = rpc(
            Some("good-token"),
            &[],
            serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "echo", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(call["id"], 3);
        assert_eq!(
            call["result"]["content"][0]["text"],
            "echo from /home/user/project"
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_is_an_invalid_params_error() {
        let (_, _, call) = rpc(
            Some("good-token"),
            &[],
            serde_json::json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "nope", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(call["id"], 4);
        assert_eq!(call["error"]["code"], -32602, "{call}");
    }

    #[tokio::test]
    async fn a_failed_tool_is_an_error_result_not_a_protocol_error() {
        let (_, _, call) = rpc(
            Some("good-token"),
            &[],
            serde_json::json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": "echo", "arguments": {"fail": true}}
            }),
        )
        .await;
        assert_eq!(call["result"]["isError"], true, "{call}");
        assert_eq!(
            call["result"]["content"][0]["text"],
            "echo from /home/user/project"
        );
    }

    /// Defense in depth: a request that reached the MCP handler without
    /// going through the gateway's auth (no scope attached) is refused.
    #[tokio::test]
    async fn the_handler_refuses_requests_that_bypassed_the_gateway() {
        for (id, method) in [(6, "tools/list"), (7, "tools/call")] {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": method,
                "params": {"name": "echo", "arguments": {}}
            });
            let res = gateway()
                .inner
                .oneshot(request(None, &[], &body))
                .await
                .unwrap();
            let (_, _, reply) = read(res).await;
            assert_eq!(
                reply["error"]["message"], "missing workspace authorization",
                "{reply}"
            );
        }
    }

    /// Browser pages send `Origin`; guests never do. A page on the host must
    /// not be able to drive the gateway even with a stolen token.
    #[tokio::test]
    async fn browser_origin_requests_are_refused() {
        let (status, _, _) = rpc(
            Some("good-token"),
            &[("origin", "http://evil.example")],
            initialize(),
        )
        .await;
        assert!(status.is_client_error(), "{status}");
    }
}
