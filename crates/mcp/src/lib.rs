//! GAVMO's host-side MCP tool gateway (brief Phase 4 — see
//! `gavmo-chatgpt-plan.md` §11 and `crates/mcp/plan.md`).
//!
//! This crate is pure mechanism, same spirit as `vm`: it knows how to run an
//! MCP server over Streamable HTTP and gate every call behind a per-workspace
//! bearer token, but it has no idea what a "workspace" or a "secret" actually
//! is — that's the daemon's job (`crates/gavmo`), injected via the
//! [`TokenAuthority`] trait so this crate doesn't depend on `gavmo` (mirrors
//! how `vm` doesn't know about workspace registries either).
//!
//! P4.0 (the current milestone — see `plan.md`) ships exactly one tool,
//! `gavmo_ping`, whose only job is proving the transport end-to-end: guest
//! reachability, bearer-token auth, and OpenCode's remote-MCP wiring. Real
//! tools (Jira, etc.) land in later sub-milestones behind the same
//! `TokenAuthority`-resolved [`WorkspaceScope`].

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};

/// The host port the gateway listens on, constant across every workspace
/// (only the bearer token varies per-workspace — see `plan.md`'s "always-on,
/// empty-by-default" design point). Baked into `nix/base/configuration.nix`'s
/// managed `mcp.gavmo-tools` entry as `http://10.0.2.2:<this>/mcp` — `10.0.2.2`
/// is QEMU user-mode networking's host address (see `vm::qemu`). There is no
/// automatic sharing between this Rust constant and the Nix literal; a change
/// here must be mirrored there by hand (same story as
/// `vm::GUEST_OPENCODE_PORT`).
pub const GAVMO_MCP_PORT: u16 = 7071;

/// What a validated bearer token authorizes. Deliberately minimal for P4.0 —
/// just enough to prove the pipeline (the `gavmo_ping` tool only echoes
/// `workspace_id` back). Later sub-milestones (P4.1+) will extend this with
/// the workspace's enabled/authorized tool set, computed by the daemon from
/// `gavmo.toml` + its secret store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceScope {
    pub workspace_id: String,
}

/// Resolves a bearer token to the workspace it authorizes. Implemented by the
/// daemon's `AppState` (`crates/gavmo/src/registry.rs`) — this crate only
/// consumes the trait, so it has no dependency on `gavmo` and no opinion on
/// how tokens are minted, stored, or expired.
///
/// Synchronous by design: resolution is expected to be a cheap in-memory
/// lookup (a `HashMap` guarded by a `std::sync::Mutex`, not an async one) —
/// see the implementation note in `crates/gavmo/src/registry.rs`.
pub trait TokenAuthority: Send + Sync + 'static {
    /// Returns `Some` only for a currently-valid token. An unknown, stale
    /// (e.g. belonging to a since-stopped VM), or malformed token must
    /// return `None` — callers treat that identically to "no token at all".
    fn resolve(&self, token: &str) -> Option<WorkspaceScope>;
}

/// Extracts the bearer token from an `Authorization` header value, if the
/// header is present, valid UTF-8, and shaped `Bearer <token>`.
///
/// Pure and independent of any rmcp/HTTP-server machinery (just
/// `http::HeaderMap`) so it's exhaustively unit-testable without standing up
/// a real request/response or `RequestContext` — see `handler::authorize`
/// for how this plugs into an actual MCP call, which instead needs
/// hardware-level verification (P4.0's gate — see `plan.md`).
pub fn bearer_token_from_headers(headers: &http::HeaderMap) -> Option<&str> {
    headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// A transport-neutral description of one tool, as returned by a
/// [`ToolExecutor`]. Deliberately *not* an `rmcp::model::Tool`: this is the
/// boundary that keeps the daemon (`crates/gavmo`) — which implements the
/// executor — free of any `rmcp` dependency. This crate translates it into a
/// real `Tool` in [`descriptor_to_tool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    /// The tool name the agent calls (e.g. `"gavmo_ping"`).
    pub name: String,
    /// Human/agent-facing description (shown in `tools/list`).
    pub description: String,
    /// The tool's JSON-Schema input object (the `{"type":"object",…}` map).
    pub input_schema: serde_json::Map<String, serde_json::Value>,
}

/// The result of a tool call, transport-neutral (see [`ToolDescriptor`] for why
/// this isn't an `rmcp` type). Both variants carry text; this crate maps
/// `Success` to a successful `CallToolResult` and `Error` to a *tool-level*
/// error result (not a protocol error — per rmcp's convention, "the tool ran
/// and failed" is a message the caller should see).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    Success(String),
    Error(String),
}

/// Runs the tools a workspace is authorized for. Implemented by the daemon's
/// `AppState` (`crates/gavmo`) alongside [`TokenAuthority`]; this crate only
/// consumes the trait, so credential lookup, HTTP forwarding, etc. never enter
/// `mcp` (mirrors how [`TokenAuthority`] keeps token storage out of here).
///
/// `call_tool` is only ever invoked by this crate for a tool the executor
/// itself advertised via `list_tools` for the same scope — the handler rejects
/// unknown names as a protocol error before dispatching (see
/// [`GavmoMcpHandler::call_tool`]). The returned future is `Send` because it's
/// awaited inside the transport's `Send` request handler.
pub trait ToolExecutor: Send + Sync + 'static {
    /// The tools available to `scope`, in `tools/list` order. An empty vec is
    /// valid (a workspace with nothing enabled).
    fn list_tools(&self, scope: &WorkspaceScope) -> Vec<ToolDescriptor>;

    /// Executes `name` for `scope` with the given JSON arguments.
    fn call_tool(
        &self,
        scope: &WorkspaceScope,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> impl std::future::Future<Output = ToolOutcome> + Send;
}

/// Translates a transport-neutral [`ToolDescriptor`] into the `rmcp` `Tool` the
/// Streamable HTTP transport serializes for `tools/list`.
fn descriptor_to_tool(descriptor: ToolDescriptor) -> Tool {
    Tool::new(
        descriptor.name,
        descriptor.description,
        Arc::new(descriptor.input_schema),
    )
}

/// The MCP `ServerHandler`: a fresh instance is constructed per request (see
/// [`service`] — `StreamableHttpService` runs stateless/per-request, so this
/// struct holds only a cheap `Clone` of the shared backend, not any
/// request-scoped state of its own).
///
/// The single `backend` is both the [`TokenAuthority`] (who is this caller?) and
/// the [`ToolExecutor`] (what can they do, and do it) — the daemon's `AppState`
/// implements both. Keeping it one value means the handler never has to
/// reconcile two separately-cloned views of daemon state.
///
/// Public only because it appears in [`service`]'s return type (the
/// `StreamableHttpService<GavmoMcpHandler<A>, _>` the caller mounts on their
/// router) — there's no reason to construct one directly, so its field stays
/// private.
pub struct GavmoMcpHandler<A> {
    backend: A,
}

impl<A: TokenAuthority + ToolExecutor> GavmoMcpHandler<A> {
    /// Pulls the raw HTTP request parts the Streamable HTTP transport
    /// injects into every [`RequestContext`] (see `StreamableHttpService`'s
    /// own doc comment in the `rmcp` source for this mechanism), extracts a
    /// bearer token, and resolves it. `None` covers every way a call can be
    /// unauthorized: no `Authorization` header, wrong scheme, or a token the
    /// authority doesn't recognize — callers don't need to distinguish
    /// these cases.
    fn authorize(&self, context: &RequestContext<RoleServer>) -> Option<WorkspaceScope> {
        let parts = context.extensions.get::<http::request::Parts>()?;
        let token = bearer_token_from_headers(&parts.headers)?;
        self.backend.resolve(token)
    }
}

impl<A: TokenAuthority + ToolExecutor> ServerHandler for GavmoMcpHandler<A> {
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

    /// Unauthenticated/unrecognized tokens see an **empty** tool list, not an
    /// error — indistinguishable from "this workspace has no tools enabled"
    /// (see `plan.md`'s "always-on, empty-by-default" design point). This
    /// also means a client that never sends a bearer token at all (e.g. a
    /// probe) learns nothing about what tools exist. An authorized caller gets
    /// exactly what the [`ToolExecutor`] advertises for their scope.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let Some(scope) = self.authorize(&context) else {
            return Ok(ListToolsResult::with_all_items(vec![]));
        };
        let tools = self
            .backend
            .list_tools(&scope)
            .into_iter()
            .map(descriptor_to_tool)
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    /// Dispatches an authorized call to the [`ToolExecutor`]. Error handling
    /// follows rmcp's convention (see its README's "Error handling" section):
    /// a protocol error (`Err(McpError)`) means "the server can't route this at
    /// all" (unauthorized, or an unknown tool name), whereas a tool that *ran*
    /// and failed comes back as a tool-level [`ToolOutcome::Error`] the caller
    /// should see rendered.
    ///
    /// Unknown tool names are rejected here, *before* the executor runs, by
    /// checking the name against what the executor advertised for this scope —
    /// so [`ToolExecutor::call_tool`] only ever sees names it offered.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(scope) = self.authorize(&context) else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "unauthorized: missing or invalid bearer token",
            )])
            .into());
        };

        let name = request.name.as_ref();
        let known = self
            .backend
            .list_tools(&scope)
            .iter()
            .any(|tool| tool.name == name);
        if !known {
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

/// The guest-side address of the host, under QEMU user-mode networking
/// (`-netdev user,...`) — see `vm::qemu`. This is the `Host` header every
/// guest-originated request to this gateway carries (`Host: 10.0.2.2:7071`),
/// which is why it must be added to [`StreamableHttpServerConfig`]'s
/// `allowed_hosts` below.
const QEMU_USER_NET_HOST_ADDR: &str = "10.0.2.2";

/// Builds the MCP gateway as a `tower` service, ready to mount on an axum
/// router (`Router::nest_service("/mcp", mcp::service(app_state))`) — see
/// `crates/gavmo/src/main.rs`.
///
/// Runs **stateless** (`legacy_session_mode(false)`): there is no
/// conversation-scoped state to keep per MCP session (every call is
/// independently authorized by its own bearer token), so there is nothing
/// for a session to buy here — matching rmcp's own stateless-mode guidance
/// (see the crate's README "Stateless Streamable HTTP" section) that
/// server-side state belongs in a `Clone` handle captured by the service
/// factory, not in a session. `with_json_response(true)` additionally means
/// a simple request/response tool call (which `gavmo_ping` is) gets a plain
/// JSON reply instead of an SSE stream.
///
/// `with_allowed_hosts` widens rmcp's built-in DNS-rebinding `Host`-header
/// allowlist (which otherwise defaults to loopback names only —
/// `localhost`/`127.0.0.1`/`::1`) to also accept
/// [`QEMU_USER_NET_HOST_ADDR`] — **discovered the hard way** during P4.0's
/// real-hardware gate: the guest's requests carry `Host: 10.0.2.2:<port>`,
/// which rmcp rejected with a bare `403 Forbidden` (no auth/tool code ever
/// ran) until this was added. Loopback names are kept (not replaced) so
/// host-side testing — e.g. `curl http://127.0.0.1:7071/mcp` — still works.
pub fn service<A>(backend: A) -> StreamableHttpService<GavmoMcpHandler<A>, LocalSessionManager>
where
    A: TokenAuthority + ToolExecutor + Clone,
{
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(["localhost", "127.0.0.1", "::1", QEMU_USER_NET_HOST_ADDR]);

    StreamableHttpService::new(
        move || {
            Ok(GavmoMcpHandler {
                backend: backend.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    )
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

    /// Shaped correctly but empty — rejecting an empty token is
    /// [`TokenAuthority::resolve`]'s job (an empty string just won't be a
    /// key in any real authority's token map), not this parser's.
    #[test]
    fn empty_token_after_bearer_prefix_is_some_empty_string() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer "),
        );
        assert_eq!(bearer_token_from_headers(&headers), Some(""));
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

    /// A fake backend standing in for `gavmo`'s real registry-backed
    /// `AppState`, implementing both [`TokenAuthority`] and [`ToolExecutor`] so
    /// the "known token -> workspace" / "unknown token -> nothing" contract (and
    /// the fact that [`service`] now requires an executor) is exercised without
    /// pulling in `gavmo` (which would be a circular dev-dependency) or standing
    /// up a real `RequestContext` (impractical outside a live transport — see
    /// `plan.md`'s testing strategy for why the auth-gated *handler* methods
    /// themselves are verified on real hardware instead).
    #[derive(Clone)]
    struct FakeAuthority(std::collections::HashMap<&'static str, &'static str>);

    impl TokenAuthority for FakeAuthority {
        fn resolve(&self, token: &str) -> Option<WorkspaceScope> {
            self.0.get(token).map(|id| WorkspaceScope {
                workspace_id: (*id).to_string(),
            })
        }
    }

    impl ToolExecutor for FakeAuthority {
        fn list_tools(&self, _scope: &WorkspaceScope) -> Vec<ToolDescriptor> {
            vec![]
        }

        async fn call_tool(
            &self,
            _scope: &WorkspaceScope,
            name: &str,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> ToolOutcome {
            ToolOutcome::Error(format!("unknown tool: {name}"))
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

    /// Regression test for the real-hardware P4.0 finding: rmcp's default
    /// `allowed_hosts` (loopback names only) rejects every guest-originated
    /// request with a bare `403 Forbidden` before any auth/tool code runs,
    /// because the guest's `Host` header is `10.0.2.2:<port>`, not a
    /// loopback name. `service()` must widen the allowlist to include it
    /// (while keeping loopback names for host-side testing).
    #[test]
    fn service_allows_both_loopback_and_the_qemu_guest_host_address() {
        let authority = FakeAuthority(std::collections::HashMap::new());
        let svc = service(authority);

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
}
