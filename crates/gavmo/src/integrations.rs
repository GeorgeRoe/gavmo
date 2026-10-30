//! Policy layer between a workspace's untrusted `gavmo.yaml`, the host secret
//! store, and the MCP gateway's tool interface.
//!
//! A tool is advertised only when the repo requests it and the user has
//! stored its credential. Each credential is bound to the destination URL it
//! was approved for, and [`forward`] host-locks every request to that origin.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use toolspec::{AuthScheme, GavmoConfig, ToolDef};

/// Cap on a tool response returned to the agent, so one call cannot flood
/// the model's context.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Shape of a stored Basic-auth credential (e.g. Jira Cloud: `email` as
/// `username` + API token as `password`).
#[derive(Debug, Deserialize)]
struct BasicCredential {
    username: String,
    password: String,
}

/// Shape of a stored single-token credential (Bearer / custom header):
/// `{"token": "..."}`.
#[derive(Debug, Deserialize)]
struct TokenCredential {
    token: String,
}

/// Envelope written to the secret store: the credential plus the canonical
/// `base_url` it was approved for. If the repo's `base_url` changes, the
/// credential is not released (fails closed).
#[derive(Debug, Serialize, Deserialize)]
struct StoredSecret {
    /// Canonical base URL (see [`toolspec::canonical_base_url`]).
    bound_url: String,
    /// Inner credential JSON.
    credential: String,
}

/// A stored secret with its envelope removed.
pub struct ParsedSecret {
    /// The URL the credential was approved for; `None` if the stored value
    /// has no envelope.
    pub bound_url: Option<String>,
    /// The inner credential JSON to hand to [`apply_auth`].
    pub credential: String,
}

/// Wraps a credential with the URL it is approved for. Saving a secret is the
/// user's approval of `canonical_url` (the control panel shows it first).
pub fn wrap_stored_secret(canonical_url: &str, credential_json: &str) -> String {
    // Cannot realistically fail; the bare fallback is treated as unbound.
    serde_json::to_string(&StoredSecret {
        bound_url: canonical_url.to_string(),
        credential: credential_json.to_string(),
    })
    .unwrap_or_else(|_| credential_json.to_string())
}

/// Removes the stored envelope. Any other shape is treated as unbound
/// (`bound_url: None`), which forces re-approval before release.
pub fn parse_stored_secret(raw: &str) -> ParsedSecret {
    match serde_json::from_str::<StoredSecret>(raw) {
        Ok(env) => ParsedSecret {
            bound_url: Some(env.bound_url),
            credential: env.credential,
        },
        Err(_) => ParsedSecret {
            bound_url: None,
            credential: raw.to_string(),
        },
    }
}

/// A tool's credential state with respect to URL binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingState {
    /// No credential stored.
    NotConfigured,
    /// Credential stored and bound to the tool's current `base_url`.
    Configured,
    /// Credential bound to a different destination than the tool now declares.
    /// `bound_url` lets the UI show "was X, now Y"; `None` means unbound.
    DestinationChanged { bound_url: Option<String> },
}

/// Classifies a tool's binding state from its current `base_url` and the raw
/// stored value. Shared by boot-time resolution and the control panel.
pub fn classify_binding(current_base_url: &str, stored_value: Option<&str>) -> BindingState {
    let Some(raw) = stored_value else {
        return BindingState::NotConfigured;
    };
    let parsed = parse_stored_secret(raw);
    match check_binding(current_base_url, parsed.bound_url.as_deref()) {
        Ok(()) => BindingState::Configured,
        Err(_) => BindingState::DestinationChanged {
            bound_url: parsed.bound_url,
        },
    }
}

/// A tool exposed for a workspace. The credential is looked up by
/// `secret_key` lazily, at call time.
#[derive(Debug, Clone)]
pub struct ResolvedIntegration {
    /// Stable id; also the tool name the agent calls.
    pub id: String,
    pub def: ToolDef,
    /// The composed [`secrets::secret_key`] for this integration's credential.
    pub secret_key: String,
}

/// A tool a repo requests, with its secret key but without touching the
/// store, so no blocking keyring call happens on an async worker thread.
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

/// A [`CandidateTool`] plus whether its credential is configured.
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
    /// Narrows to the gateway-facing [`ResolvedIntegration`].
    pub fn into_resolved(self) -> ResolvedIntegration {
        ResolvedIntegration {
            id: self.id,
            def: self.def,
            secret_key: self.secret_key,
        }
    }
}

/// Every tool the repo requests, with its secret key. Store-free.
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

/// Requested tools tagged as configured per `has_secret`.
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

/// The enabled (configured) subset of [`requested_tools`].
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

/// Reads a workspace's `gavmo.yaml`. Missing or invalid files yield an empty
/// config, so a malformed repo config never breaks a boot.
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
        Err(_) => GavmoConfig::default(),
    }
}

/// Resolves requested tools without touching the secret store; the caller
/// checks presence off the async runtime (keyring calls block on D-Bus).
pub fn resolve_candidates(workspace_id: &str, config: &GavmoConfig) -> Vec<CandidateTool> {
    candidate_tools(workspace_id, config)
}

/// Builds the gateway descriptor: a host-locked HTTP forwarder named after
/// the integration id, taking `method`, `path` and optional `body`.
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

/// Forwards an agent-requested HTTP call with the credential attached.
///
/// All rejections (args, host-lock, method, credential, URL binding) happen
/// before network I/O. Non-2xx responses are returned as tool errors.
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

    // Never send the credential anywhere but the integration's own origin.
    let url = match toolspec::lock_url(&integration.def.base_url, path) {
        Ok(url) => url,
        Err(e) => return mcp::ToolOutcome::Error(format!("request blocked by host-lock: {e}")),
    };

    let method = match reqwest::Method::from_bytes(method_str.as_bytes()) {
        Ok(method) => method,
        Err(e) => {
            return mcp::ToolOutcome::Error(format!("invalid HTTP method {method_str:?}: {e}"));
        }
    };

    let Some(secret_value) = secret_value else {
        return mcp::ToolOutcome::Error(format!(
            "no credential configured for integration '{}'",
            integration.id
        ));
    };

    // The hard security boundary (the UI warning is only UX): release the
    // credential only to the destination it was approved for.
    let parsed = parse_stored_secret(&secret_value);
    if let Err(message) = check_binding(&integration.def.base_url, parsed.bound_url.as_deref()) {
        return mcp::ToolOutcome::Error(message);
    }

    let request = client.request(method, url);
    let request = match apply_auth(
        request,
        integration.def.auth,
        integration.def.header_name.as_deref(),
        &parsed.credential,
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

/// Checks that a credential's bound URL matches the integration's current
/// `base_url`. Fails closed on unbound values, uncanonicalizable URLs and
/// mismatches; the error names both destinations.
pub fn check_binding(current_base_url: &str, bound_url: Option<&str>) -> Result<(), String> {
    let current = toolspec::canonical_base_url(current_base_url).map_err(|e| {
        format!("integration base_url could not be canonicalized for binding check: {e}")
    })?;
    match bound_url {
        Some(bound) if bound == current => Ok(()),
        Some(bound) => Err(format!(
            "credential was approved for {bound} but this tool now points at {current}; \
             re-confirm it in the GAVMO control panel"
        )),
        None => Err(
            "credential is not bound to a destination (saved before URL-binding, or the stored \
             value is malformed); re-save it in the GAVMO control panel to confirm its destination"
                .to_string(),
        ),
    }
}

/// Attaches the credential per its auth scheme. Errs if the secret is not
/// the shape the scheme expects.
fn apply_auth(
    request: reqwest::RequestBuilder,
    scheme: AuthScheme,
    header_name: Option<&str>,
    secret_value: &str,
) -> Result<reqwest::RequestBuilder, String> {
    match scheme {
        AuthScheme::Basic => {
            #[expect(
                clippy::map_err_ignore,
                reason = "serde_json errors can quote the input, i.e. the secret; never surface it"
            )]
            let cred: BasicCredential = serde_json::from_str(secret_value).map_err(|_| {
                "stored credential for basic auth must be JSON {\"username\",\"password\"}"
                    .to_owned()
            })?;
            Ok(request.basic_auth(cred.username, Some(cred.password)))
        }
        AuthScheme::Bearer => Ok(request.bearer_auth(token_from_secret(secret_value))),
        AuthScheme::Header => {
            // Validated at parse time; guard rather than unwrap.
            let name =
                header_name.ok_or_else(|| "header auth is missing its header_name".to_string())?;
            Ok(request.header(name, token_from_secret(secret_value)))
        }
    }
}

/// Extracts a token from `{"token":"..."}` JSON or a bare token string.
fn token_from_secret(secret_value: &str) -> String {
    match serde_json::from_str::<TokenCredential>(secret_value) {
        Ok(cred) => cred.token,
        Err(_) => secret_value.to_string(),
    }
}

/// Truncates to [`MAX_RESPONSE_BYTES`] on a char boundary, with a notice.
fn cap_response(text: &str) -> String {
    if text.len() <= MAX_RESPONSE_BYTES {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(MAX_RESPONSE_BYTES);
    // `end` is a char boundary within `text`, so `get` always succeeds.
    let head = text.get(..end).unwrap_or_default();
    format!("{head}\n\n[... response truncated at {MAX_RESPONSE_BYTES} bytes ...]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrets::{MemoryStore, SecretStore};

    const JIRA_CONFIG: &str = r"
tools:
  dafni-jira:
    type: http
    base_url: https://dafni.atlassian.net
    auth: basic
    secret: jira-cred
";

    fn jira_config() -> GavmoConfig {
        toolspec::parse_config(JIRA_CONFIG).unwrap()
    }

    /// A `MemoryStore` with the jira secret configured for `workspace_id`.
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

    fn args(json: Value) -> Map<String, Value> {
        serde_json::from_value(json).unwrap()
    }

    // --- forward() pre-network rejections ---

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
            other @ mcp::ToolOutcome::Success(_) => panic!("expected error, got {other:?}"),
        }
    }

    // --- URL binding ---

    /// A properly-bound Basic credential for the jira integration's own `base_url`.
    fn bound_jira_secret() -> String {
        let url = toolspec::canonical_base_url("https://dafni.atlassian.net").unwrap();
        wrap_stored_secret(&url, r#"{"username":"a@b.c","password":"t"}"#)
    }

    #[test]
    fn stored_secret_envelope_round_trips() {
        let wrapped = wrap_stored_secret("https://h.example.com/", r#"{"token":"abc"}"#);
        let parsed = parse_stored_secret(&wrapped);
        assert_eq!(parsed.bound_url.as_deref(), Some("https://h.example.com/"));
        assert_eq!(parsed.credential, r#"{"token":"abc"}"#);
    }

    #[test]
    fn parse_stored_secret_treats_a_bare_credential_as_unbound() {
        // No envelope -> unbound.
        let parsed = parse_stored_secret(r#"{"username":"a","password":"b"}"#);
        assert!(parsed.bound_url.is_none());
        assert_eq!(parsed.credential, r#"{"username":"a","password":"b"}"#);
    }

    #[test]
    fn check_binding_accepts_a_matching_destination() {
        let bound = toolspec::canonical_base_url("https://dafni.atlassian.net").unwrap();
        assert!(check_binding("https://dafni.atlassian.net", Some(&bound)).is_ok());
        // Cosmetic differences (trailing slash) must still match.
        assert!(check_binding("https://dafni.atlassian.net/", Some(&bound)).is_ok());
    }

    #[test]
    fn check_binding_rejects_a_changed_destination() {
        let bound = toolspec::canonical_base_url("https://dafni.atlassian.net").unwrap();
        let err = check_binding("https://evil.com", Some(&bound)).unwrap_err();
        assert!(err.contains("evil.com"), "{err}");
        assert!(err.contains("re-confirm"), "{err}");
    }

    #[test]
    fn check_binding_rejects_an_unbound_credential() {
        let err = check_binding("https://dafni.atlassian.net", None).unwrap_err();
        assert!(err.contains("not bound"), "{err}");
    }

    #[tokio::test]
    async fn forward_rejects_a_credential_bound_to_a_different_url() {
        // Credential approved for example.com, tool now points at atlassian.
        let wrong = wrap_stored_secret(
            &toolspec::canonical_base_url("https://example.com").unwrap(),
            r#"{"username":"a@b.c","password":"t"}"#,
        );
        let outcome = forward(
            &jira_integration(),
            Some(wrong),
            &args(serde_json::json!({"method": "GET", "path": "/rest/api/3/myself"})),
            &reqwest::Client::new(),
        )
        .await;
        match outcome {
            mcp::ToolOutcome::Error(text) => assert!(text.contains("re-confirm"), "{text}"),
            other @ mcp::ToolOutcome::Success(_) => panic!("expected binding error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_rejects_an_unbound_legacy_credential() {
        let outcome = forward(
            &jira_integration(),
            // Bare credential, no envelope.
            Some(r#"{"username":"a@b.c","password":"t"}"#.to_string()),
            &args(serde_json::json!({"method": "GET", "path": "/rest/api/3/myself"})),
            &reqwest::Client::new(),
        )
        .await;
        match outcome {
            mcp::ToolOutcome::Error(text) => assert!(text.contains("not bound"), "{text}"),
            other @ mcp::ToolOutcome::Success(_) => panic!("expected binding error, got {other:?}"),
        }
    }

    #[test]
    fn classify_binding_covers_the_tri_state() {
        let base = "https://dafni.atlassian.net";
        let good = wrap_stored_secret(
            &toolspec::canonical_base_url(base).unwrap(),
            r#"{"token":"t"}"#,
        );
        let bad = wrap_stored_secret(
            &toolspec::canonical_base_url("https://evil.com").unwrap(),
            r#"{"token":"t"}"#,
        );

        assert_eq!(classify_binding(base, None), BindingState::NotConfigured);
        assert_eq!(
            classify_binding(base, Some(&good)),
            BindingState::Configured
        );
        match classify_binding(base, Some(&bad)) {
            BindingState::DestinationChanged { bound_url } => {
                assert_eq!(
                    bound_url.as_deref(),
                    Some(
                        toolspec::canonical_base_url("https://evil.com")
                            .unwrap()
                            .as_str()
                    )
                );
            }
            other => panic!("expected DestinationChanged, got {other:?}"),
        }
    }

    #[test]
    fn classify_binding_reports_a_legacy_unbound_value_as_changed_with_no_url() {
        match classify_binding("https://h.example.com", Some(r#"{"token":"t"}"#)) {
            BindingState::DestinationChanged { bound_url } => assert!(bound_url.is_none()),
            other => panic!("expected DestinationChanged, got {other:?}"),
        }
    }

    #[test]
    fn a_bound_secret_for_the_integration_passes_its_own_binding_check() {
        // A credential bound to the tool's own base_url must never be rejected.
        let integration = jira_integration();
        let parsed = parse_stored_secret(&bound_jira_secret());
        assert!(
            check_binding(&integration.def.base_url, parsed.bound_url.as_deref()).is_ok(),
            "a freshly-bound credential must pass its own tool's binding check"
        );
    }

    // --- Auth injection ---

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
        // Slicing off a char boundary would panic.
        let big = "é".repeat(MAX_RESPONSE_BYTES); // 2 bytes each
        let capped = cap_response(&big);
        assert!(capped.contains("truncated"));
    }
}
