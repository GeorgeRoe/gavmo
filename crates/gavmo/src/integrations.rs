//! Resolving a workspace's enabled tool integrations from its `gavmo.yaml` and
//! the host secret store (see `gavmo-tools-config-plan.md`).
//!
//! This module is the daemon's *policy* layer between the untrusted repo config
//! (`toolspec::GavmoConfig`), the secret store (`secrets::SecretStore`), and the
//! transport-neutral tool interface the gateway consumes (`mcp::ToolDescriptor`
//! / `mcp::ToolOutcome`).
//!
//! ## What "enabled" means
//!
//! A tool is advertised to the agent only when **both** the repo requests it
//! (declared inline under `gavmo.yaml`'s `tools:` map) **and** the user has
//! configured its secret on the host (stored under the workspace's secret key).
//!
//! Advertising only configured tools means the agent never sees a tool it can't
//! actually use for lack of credentials.
//!
//! [`forward`] performs the real HTTP call: it host-locks the URL (via
//! [`toolspec::lock_url`]), injects the configured credential, forwards any
//! method, and returns a size-capped response.

use serde::Deserialize;
use serde_json::{Map, Value};
use toolspec::{AuthScheme, GavmoConfig, ToolDef};

/// Hard cap on how much of a tool response is returned to the agent. A read-only
/// API can still return large payloads (big JQL result sets, long issue
/// histories); capping keeps a single tool call from flooding the model's
/// context. Chosen generous enough for normal reads, small enough to bound cost.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Shape of a stored Basic-auth credential (e.g. Jira Cloud: `email` as
/// `username` + API token as `password`).
#[derive(Debug, Deserialize)]
struct BasicCredential {
    username: String,
    password: String,
}

/// Shape of a stored single-token credential (Bearer / custom header). Stored as
/// `{"token": "…"}`, but [`token_from_secret`] also accepts a bare string for
/// robustness.
#[derive(Debug, Deserialize)]
struct TokenCredential {
    token: String,
}

/// A tool the daemon has decided to expose for a workspace: its id, the full
/// definition (base_url, auth, etc.), and the *key* under which its credential
/// lives in the secret store (resolved to a value lazily, at call time).
#[derive(Debug, Clone)]
pub struct ResolvedIntegration {
    /// Stable id; also the name the agent calls the tool by.
    pub id: String,
    /// The integration's definition (kind, base_url, auth scheme, secret ref…).
    pub def: ToolDef,
    /// The composed [`secrets::secret_key`] for this integration's credential.
    pub secret_key: String,
}

/// A tool a repo *requests*, paired with its resolved secret-store key, but
/// **without** consulting the secret store — purely structural. Whether its
/// credential is actually configured is a separate, store-touching question the
/// caller answers (via [`crate::registry::AppState`]'s batched, `spawn_blocking`
/// presence check), deliberately kept out of this pure layer so no keyring call
/// ever happens on an async worker thread.
#[derive(Debug, Clone)]
pub struct CandidateTool {
    pub id: String,
    pub def: ToolDef,
    /// The composed [`secrets::secret_key`] for this tool's credential.
    pub secret_key: String,
}

impl CandidateTool {
    /// Narrows to the gateway-facing [`ResolvedIntegration`].
    pub fn into_resolved(self) -> ResolvedIntegration {
        ResolvedIntegration {
            id: self.id,
            def: self.def,
            secret_key: self.secret_key,
        }
    }
}

/// A [`CandidateTool`] plus whether its credential is currently configured. The
/// control panel's view (unlike [`ResolvedIntegration`], which is only the
/// configured/enabled subset the gateway actually exposes). Produced by the pure
/// [`requested_tools`] for unit testing.
#[derive(Debug, Clone)]
pub struct RequestedTool {
    pub id: String,
    pub def: ToolDef,
    /// The composed secret-store key for this tool's credential.
    pub secret_key: String,
    /// Whether a credential is currently stored under `secret_key`.
    pub configured: bool,
}

impl RequestedTool {
    /// Narrows to the gateway-facing [`ResolvedIntegration`] (dropping the
    /// control-panel-only `configured` field).
    pub fn into_resolved(self) -> ResolvedIntegration {
        ResolvedIntegration {
            id: self.id,
            def: self.def,
            secret_key: self.secret_key,
        }
    }
}

/// Pure, **store-free** enumeration of every tool a repo requests (the inline
/// `tools:` map), each with its resolved secret key. The single source of truth
/// for the structure of a workspace's tools; configured-or-not is decided
/// separately by the caller so no keyring access happens here.
pub fn candidate_tools(workspace_id: &str, config: &GavmoConfig) -> Vec<CandidateTool> {
    config
        .tools
        .iter()
        .map(|(id, def)| CandidateTool {
            id: id.clone(),
            def: def.clone(),
            secret_key: secrets::secret_key(workspace_id, &def.secret),
        })
        .collect()
}

/// Pure enumeration of requested tools, each tagged with whether its secret is
/// configured per the `has_secret` predicate. Unit-testable without a live
/// keystore; the daemon's real resolution path uses [`candidate_tools`] + a
/// batched async presence check instead (see [`crate::registry::AppState`]).
pub fn requested_tools(
    workspace_id: &str,
    config: &GavmoConfig,
    has_secret: impl Fn(&str) -> bool,
) -> Vec<RequestedTool> {
    candidate_tools(workspace_id, config)
        .into_iter()
        .map(|c| RequestedTool {
            configured: has_secret(&c.secret_key),
            id: c.id,
            def: c.def,
            secret_key: c.secret_key,
        })
        .collect()
}

/// The *enabled* subset of [`requested_tools`]: only tools whose credential is
/// configured, narrowed to the gateway-facing [`ResolvedIntegration`]. This is
/// what the gateway actually advertises/dispatches.
pub fn select_integrations(
    workspace_id: &str,
    config: &GavmoConfig,
    has_secret: impl Fn(&str) -> bool,
) -> Vec<ResolvedIntegration> {
    requested_tools(workspace_id, config, has_secret)
        .into_iter()
        .filter(|tool| tool.configured)
        .map(RequestedTool::into_resolved)
        .collect()
}

/// Reads and parses a workspace's `gavmo.yaml`. A missing file is normal (the
/// repo requests nothing) and yields an empty config; a *present but invalid*
/// file is logged and also treated as empty, so a malformed repo config can
/// never crash a boot — it just enables no tools.
pub async fn read_config(workspace_path: &str) -> GavmoConfig {
    let path = std::path::Path::new(workspace_path).join("gavmo.yaml");
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => match toolspec::parse_config(&text) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("gavmo: ignoring invalid {}: {e}", path.display());
                GavmoConfig::default()
            }
        },
        // Missing file (or unreadable) -> no requested tools.
        Err(_) => GavmoConfig::default(),
    }
}

/// Resolves a workspace's requested tools structurally by enumerating
/// [`candidate_tools`]. **Touches no secret store** — the caller decides which
/// candidates are configured via a batched, `spawn_blocking` presence check (see
/// [`crate::registry::AppState`]). Keeping the keyring out of this async fn is
/// what prevents a blocking D-Bus call from landing on a tokio worker thread.
pub fn resolve_candidates(workspace_id: &str, config: &GavmoConfig) -> Vec<CandidateTool> {
    candidate_tools(workspace_id, config)
}

/// Builds the gateway tool descriptor for an integration: one generic
/// host-locked HTTP forwarder named after the integration id. The agent supplies
/// `method` + `path` (+ optional `body`); the host prepends `base_url`,
/// host-locks the result, and (from V1.4) attaches the credential.
pub fn integration_tool_descriptor(integration: &ResolvedIntegration) -> mcp::ToolDescriptor {
    let base = &integration.def.base_url;
    let summary = integration
        .def
        .description
        .clone()
        .unwrap_or_else(|| format!("HTTP access to {base}"));
    let description = format!(
        "{summary}\n\nForwards an HTTP request to {base} (host-locked to that \
         origin — other hosts are rejected). Provide `method` (e.g. GET, POST) \
         and `path` (relative to the base URL, e.g. /rest/api/3/issue/ABC-1); \
         the host attaches the configured credentials. Returns the response body."
    );

    let mut properties = Map::new();
    properties.insert(
        "method".to_string(),
        serde_json::json!({
            "type": "string",
            "description": "HTTP method, e.g. GET or POST."
        }),
    );
    properties.insert(
        "path".to_string(),
        serde_json::json!({
            "type": "string",
            "description": "Path (and optional query string) relative to the integration's base URL."
        }),
    );
    properties.insert(
        "body".to_string(),
        serde_json::json!({
            "type": "string",
            "description": "Optional request body, sent as-is."
        }),
    );

    let mut input_schema = Map::new();
    input_schema.insert("type".to_string(), Value::String("object".to_string()));
    input_schema.insert("properties".to_string(), Value::Object(properties));
    input_schema.insert(
        "required".to_string(),
        serde_json::json!(["method", "path"]),
    );
    input_schema.insert("additionalProperties".to_string(), Value::Bool(false));

    mcp::ToolDescriptor {
        name: integration.id.clone(),
        description,
        input_schema,
    }
}

/// Forwards an agent-requested HTTP call to an integration's host, with the
/// configured credential attached (V1.4).
///
/// Order of checks is deliberate — the cheap, local rejections happen before any
/// network I/O: argument validation, **host-lock** (the credential never leaves
/// the integration's own origin), method parsing, and credential presence. Only
/// then is the request built and sent via the shared `client`. The response is
/// returned with its status line and a size-capped body; a non-2xx status comes
/// back as a tool-level error (the agent still sees the body) so a failed read
/// is visibly distinct from a successful one.
pub async fn forward(
    integration: &ResolvedIntegration,
    secret_value: Option<String>,
    args: &Map<String, Value>,
    client: &reqwest::Client,
) -> mcp::ToolOutcome {
    let method_str = args.get("method").and_then(Value::as_str).unwrap_or("GET");
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return mcp::ToolOutcome::Error("missing required argument: path".to_string());
    };
    let body = args.get("body").and_then(Value::as_str).map(str::to_string);

    // Host-lock: refuse to send anywhere but the integration's own origin.
    let url = match toolspec::lock_url(&integration.def.base_url, path) {
        Ok(url) => url,
        Err(e) => return mcp::ToolOutcome::Error(format!("request blocked by host-lock: {e}")),
    };

    let method = match reqwest::Method::from_bytes(method_str.as_bytes()) {
        Ok(method) => method,
        Err(_) => return mcp::ToolOutcome::Error(format!("invalid HTTP method: {method_str}")),
    };

    let Some(secret_value) = secret_value else {
        return mcp::ToolOutcome::Error(format!(
            "no credential configured for integration '{}'",
            integration.id
        ));
    };

    let request = client.request(method, url);
    let request = match apply_auth(
        request,
        integration.def.auth,
        integration.def.header_name.as_deref(),
        &secret_value,
    ) {
        Ok(request) => request,
        Err(e) => return mcp::ToolOutcome::Error(e),
    };
    let request = match body {
        Some(body) => request.body(body),
        None => request,
    };

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => return mcp::ToolOutcome::Error(format!("request failed: {e}")),
    };
    let status = response.status();
    let text = match response.text().await {
        Ok(text) => text,
        Err(e) => return mcp::ToolOutcome::Error(format!("failed to read response body: {e}")),
    };

    let payload = format!("HTTP {}\n\n{}", status.as_u16(), cap_response(&text));
    if status.is_success() {
        mcp::ToolOutcome::Success(payload)
    } else {
        mcp::ToolOutcome::Error(payload)
    }
}

/// Attaches the configured credential to a request per its auth scheme. Returns
/// `Err` with an agent-visible message if the stored secret isn't the shape the
/// scheme expects (e.g. Basic auth without a `username`/`password` object).
fn apply_auth(
    request: reqwest::RequestBuilder,
    scheme: AuthScheme,
    header_name: Option<&str>,
    secret_value: &str,
) -> Result<reqwest::RequestBuilder, String> {
    match scheme {
        AuthScheme::Basic => {
            let cred: BasicCredential = serde_json::from_str(secret_value).map_err(|_| {
                "stored credential for basic auth must be JSON {\"username\",\"password\"}"
                    .to_string()
            })?;
            Ok(request.basic_auth(cred.username, Some(cred.password)))
        }
        AuthScheme::Bearer => Ok(request.bearer_auth(token_from_secret(secret_value))),
        AuthScheme::Header => {
            // `header_name` presence is validated at parse time (see
            // `ToolDef::validate`), but guard defensively rather than unwrap.
            let name =
                header_name.ok_or_else(|| "header auth is missing its header_name".to_string())?;
            Ok(request.header(name, token_from_secret(secret_value)))
        }
    }
}

/// Extracts a single token from a stored secret, accepting either the canonical
/// `{"token":"…"}` JSON or a bare token string (robustness against either shape
/// the UI might write).
fn token_from_secret(secret_value: &str) -> String {
    match serde_json::from_str::<TokenCredential>(secret_value) {
        Ok(cred) => cred.token,
        Err(_) => secret_value.to_string(),
    }
}

/// Truncates a response body to [`MAX_RESPONSE_BYTES`] on a UTF-8 char boundary,
/// appending a visible truncation notice. Returns the text unchanged if small.
fn cap_response(text: &str) -> String {
    if text.len() <= MAX_RESPONSE_BYTES {
        return text.to_string();
    }
    let mut end = MAX_RESPONSE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[... response truncated at {MAX_RESPONSE_BYTES} bytes ...]",
        &text[..end]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrets::{MemoryStore, SecretStore};

    const JIRA_CONFIG: &str = r#"
tools:
  dafni-jira:
    type: http
    base_url: https://dafni.atlassian.net
    auth: basic
    secret: jira-cred
"#;

    fn jira_config() -> GavmoConfig {
        toolspec::parse_config(JIRA_CONFIG).unwrap()
    }

    /// Helper: a MemoryStore with the jira secret already configured for `/ws`.
    fn store_with_secret(workspace_id: &str) -> MemoryStore {
        let store = MemoryStore::new();
        let key = secrets::secret_key(workspace_id, "jira-cred");
        store
            .set(&key, r#"{"username":"a@b.c","password":"tok"}"#)
            .unwrap();
        store
    }

    fn select_with_store(
        workspace_id: &str,
        config: &GavmoConfig,
        store: &MemoryStore,
    ) -> Vec<ResolvedIntegration> {
        select_integrations(workspace_id, config, |k| store.get(k).unwrap().is_some())
    }

    #[test]
    fn tool_enabled_when_requested_and_secret_configured() {
        let config = jira_config();
        let store = store_with_secret("/ws");

        let resolved = select_with_store("/ws", &config, &store);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "dafni-jira");
        assert_eq!(resolved[0].def.base_url, "https://dafni.atlassian.net");
    }

    #[test]
    fn tool_disabled_when_secret_missing() {
        let config = jira_config();
        let store = MemoryStore::new(); // no secret configured

        let resolved = select_with_store("/ws", &config, &store);
        assert!(resolved.is_empty());
    }

    #[test]
    fn inline_tool_enabled_when_secret_configured() {
        let config = toolspec::parse_config(
            "tools:\n  pdocs:\n    type: http\n    base_url: https://p.example.com\n    auth: bearer\n    secret: pdocs-token\n",
        )
        .unwrap();
        let store = MemoryStore::new();
        let key = secrets::secret_key("/ws", "pdocs-token");
        store.set(&key, "tok").unwrap();

        let resolved = select_with_store("/ws", &config, &store);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "pdocs");
        assert_eq!(resolved[0].secret_key, key);
    }

    #[test]
    fn secret_is_scoped_to_its_workspace() {
        let config = toolspec::parse_config(
            "tools:\n  pdocs:\n    type: http\n    base_url: https://p.example.com\n    auth: bearer\n    secret: pdocs-token\n",
        )
        .unwrap();
        let store = MemoryStore::new();
        // Secret configured for a *different* workspace id.
        let other = secrets::secret_key("/other", "pdocs-token");
        store.set(&other, "tok").unwrap();

        let resolved = select_with_store("/ws", &config, &store);
        assert!(
            resolved.is_empty(),
            "a secret bound to /other must not enable /ws"
        );
    }

    #[test]
    fn empty_config_yields_no_integrations() {
        let config = GavmoConfig::default();
        let store = MemoryStore::new();
        assert!(select_with_store("/ws", &config, &store).is_empty());
    }

    #[test]
    fn descriptor_is_named_after_the_integration_id_and_requires_method_and_path() {
        let descriptor = integration_tool_descriptor(&jira_integration());
        assert_eq!(descriptor.name, "dafni-jira");
        assert_eq!(
            descriptor.input_schema.get("required"),
            Some(&serde_json::json!(["method", "path"]))
        );
    }

    fn jira_integration() -> ResolvedIntegration {
        let mut config = jira_config();
        ResolvedIntegration {
            id: "dafni-jira".to_string(),
            def: config.tools.remove("dafni-jira").unwrap(),
            secret_key: "k".to_string(),
        }
    }

    fn args(json: serde_json::Value) -> Map<String, Value> {
        serde_json::from_value(json).unwrap()
    }

    // --- forward() pre-network rejections (no server needed) ----------------
    //
    // These exercise the branches that must return *before* any HTTP I/O. The
    // actual on-the-wire send (success path, auth header on the real request,
    // response capping) is covered by `apply_auth`/`cap_response` unit tests
    // below plus the hardware gate — same split as P4.0 (see
    // `crates/mcp/plan.md` testing strategy).

    #[tokio::test]
    async fn forward_blocks_a_cross_host_path_before_sending() {
        let outcome = forward(
            &jira_integration(),
            Some(r#"{"username":"a@b.c","password":"t"}"#.to_string()),
            &args(serde_json::json!({"method": "GET", "path": "https://evil.com/steal"})),
            &reqwest::Client::new(),
        )
        .await;
        assert!(matches!(outcome, mcp::ToolOutcome::Error(_)));
    }

    #[tokio::test]
    async fn forward_requires_path() {
        let outcome = forward(
            &jira_integration(),
            Some(r#"{"username":"a@b.c","password":"t"}"#.to_string()),
            &args(serde_json::json!({"method": "GET"})),
            &reqwest::Client::new(),
        )
        .await;
        assert!(matches!(outcome, mcp::ToolOutcome::Error(_)));
    }

    #[tokio::test]
    async fn forward_rejects_an_invalid_method() {
        let outcome = forward(
            &jira_integration(),
            Some(r#"{"username":"a@b.c","password":"t"}"#.to_string()),
            &args(serde_json::json!({"method": "IN VALID", "path": "/x"})),
            &reqwest::Client::new(),
        )
        .await;
        assert!(matches!(outcome, mcp::ToolOutcome::Error(_)));
    }

    #[tokio::test]
    async fn forward_errors_when_no_credential_configured() {
        let outcome = forward(
            &jira_integration(),
            None,
            &args(serde_json::json!({"method": "GET", "path": "/rest/api/3/myself"})),
            &reqwest::Client::new(),
        )
        .await;
        match outcome {
            mcp::ToolOutcome::Error(text) => assert!(text.contains("no credential"), "{text}"),
            other => panic!("expected error, got {other:?}"),
        }
    }

    // --- auth injection (build the request and inspect it, no network) ------

    fn built_request(
        scheme: AuthScheme,
        header_name: Option<&str>,
        secret: &str,
    ) -> reqwest::Request {
        let client = reqwest::Client::new();
        let builder = client.get("https://h.example.com/x");
        apply_auth(builder, scheme, header_name, secret)
            .unwrap()
            .build()
            .unwrap()
    }

    #[test]
    fn apply_auth_basic_sets_base64_authorization_header() {
        let req = built_request(
            AuthScheme::Basic,
            None,
            r#"{"username":"user","password":"pass"}"#,
        );
        // base64("user:pass") == "dXNlcjpwYXNz"
        assert_eq!(
            req.headers().get("authorization").unwrap(),
            "Basic dXNlcjpwYXNz"
        );
    }

    #[test]
    fn apply_auth_basic_rejects_a_non_object_secret() {
        let client = reqwest::Client::new();
        let builder = client.get("https://h.example.com/x");
        assert!(apply_auth(builder, AuthScheme::Basic, None, "not-json").is_err());
    }

    #[test]
    fn apply_auth_bearer_sets_bearer_authorization_header() {
        let req = built_request(AuthScheme::Bearer, None, r#"{"token":"abc123"}"#);
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer abc123");
    }

    #[test]
    fn apply_auth_header_sets_the_named_header() {
        let req = built_request(
            AuthScheme::Header,
            Some("X-API-Key"),
            r#"{"token":"abc123"}"#,
        );
        assert_eq!(req.headers().get("x-api-key").unwrap(), "abc123");
    }

    #[test]
    fn token_from_secret_accepts_json_or_bare_string() {
        assert_eq!(token_from_secret(r#"{"token":"abc"}"#), "abc");
        assert_eq!(token_from_secret("raw-token"), "raw-token");
    }

    #[test]
    fn cap_response_passes_small_bodies_through_unchanged() {
        assert_eq!(cap_response("hello"), "hello");
    }

    #[test]
    fn cap_response_truncates_oversized_bodies_on_a_char_boundary() {
        let big = "a".repeat(MAX_RESPONSE_BYTES + 100);
        let capped = cap_response(&big);
        assert!(capped.len() < big.len());
        assert!(capped.contains("truncated"));
    }

    #[test]
    fn cap_response_truncation_never_splits_a_multibyte_char() {
        // A body of multibyte chars whose byte length exceeds the cap; the cut
        // must land on a char boundary (no panic, valid UTF-8 out).
        let big = "é".repeat(MAX_RESPONSE_BYTES); // 2 bytes each
        let capped = cap_response(&big);
        assert!(capped.contains("truncated"));
        // If this didn't panic and produced a String, UTF-8 is intact.
    }
}
