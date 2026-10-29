//! GAVMO's host-side MCP tool gateway: an MCP server over Streamable HTTP
//! with every call gated by a per-workspace bearer token.
//!
//! Pure mechanism: workspaces, secrets and tools belong to the daemon, which
//! plugs in via [`TokenAuthority`] and [`ToolExecutor`] so this crate never
//! depends on `gavmo`.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};

/// Host port the gateway listens on; the same for every workspace (only the
/// token differs). Hardcoded as `http://10.0.2.2:7071/mcp` in
/// `nix/base/configuration.nix` - keep the two in sync by hand.
pub const GAVMO_MCP_PORT: u16 = 7071;

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

/// Extracts the bearer token from an `Authorization` header value, if the
/// header is present, valid UTF-8, and shaped `Bearer <token>`. Kept free of
/// rmcp types so it is unit-testable without a live `RequestContext`.
pub fn bearer_token_from_headers(headers: &http::HeaderMap) -> Option<&str> {
    headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
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
/// forwarding out of this crate. `call_tool` is only invoked for names that
/// `list_tools` advertised for the same scope.
pub trait ToolExecutor: Send + Sync + 'static {
    /// The tools available to `scope`, in `tools/list` order (may be empty).
    fn list_tools(&self, scope: &WorkspaceScope) -> Vec<ToolDescriptor>;

    /// Executes `name` for `scope` with the given JSON arguments.
    fn call_tool(
        &self,
        scope: &WorkspaceScope,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> impl std::future::Future<Output = ToolOutcome> + Send;
}

fn descriptor_to_tool(descriptor: ToolDescriptor) -> Tool {
    Tool::new(
        descriptor.name,
        descriptor.description,
        Arc::new(descriptor.input_schema),
    )
}

/// The MCP `ServerHandler`, constructed fresh per request. One `backend` serves
/// as both authority and executor so the two never see divergent state.
/// Public only because it appears in [`service`]'s return type.
pub struct GavmoMcpHandler<A> {
    backend: A,
}

impl<A: TokenAuthority + ToolExecutor> GavmoMcpHandler<A> {
    /// Resolves the bearer token from the HTTP parts the transport injects into
    /// every [`RequestContext`]. `None` for any kind of unauthorized call.
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

    /// Unauthorized callers get an empty list rather than an error, so a probe
    /// can't distinguish "bad token" from "no tools enabled".
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

    /// Unknown tool names are a protocol error, rejected before the executor
    /// runs; a tool that ran and failed is a tool-level error result.
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

/// The host's address as seen from a QEMU user-net guest, and so the `Host`
/// header on every guest request to the gateway.
const QEMU_USER_NET_HOST_ADDR: &str = "10.0.2.2";

/// Builds the gateway as a `tower` service to mount at `/mcp`.
///
/// Stateless with plain JSON responses: each call carries its own token, so
/// sessions buy nothing. rmcp's default `Host` allowlist is loopback-only and
/// 403s guest requests, so the QEMU host address is added alongside loopback.
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

    /// Rejecting an empty token is [`TokenAuthority::resolve`]'s job, not the
    /// parser's.
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

    /// rmcp's default `allowed_hosts` is loopback-only and 403s every guest
    /// request (`Host: 10.0.2.2:<port>`) before auth runs.
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
