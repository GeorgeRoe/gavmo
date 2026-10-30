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
/// the model's context. Also the most the daemon ever buffers per response:
/// the body is streamed and reading stops here.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Upper bound on a whole forwarded request, so a hung upstream can't pin a
/// tool call (and its gateway task) forever.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The HTTP client every forwarded tool call must go through.
///
/// Redirects are **never** followed: `reqwest` strips `Authorization` and
/// `Cookie` on a cross-host redirect, but not custom headers (`auth: header`,
/// e.g. `X-API-Key`), so an open redirect on the approved origin would hand
/// the credential to any host. Even same-origin redirects could leave the
/// approved path prefix. A 3xx is returned to the agent instead, with its
/// `Location`, so it can re-issue the request through the host-lock.
pub fn forwarding_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
}

/// How a credential is attached to requests. Part of what the user approves
/// when saving it, alongside the destination: if the untrusted `gavmo.yaml`
/// later switches e.g. `bearer` to `header: X-Echo-Me` (a header the origin
/// reflects back, or one that survives redirects), the credential must not be
/// released until the user re-confirms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "auth", rename_all = "lowercase", deny_unknown_fields)]
pub enum Delivery {
    Basic,
    Bearer,
    Header { header_name: String },
}

impl Delivery {
    /// The delivery a tool definition currently declares.
    pub fn of(def: &ToolDef) -> Result<Self, String> {
        Ok(match def.auth {
            AuthScheme::Basic => Self::Basic,
            AuthScheme::Bearer => Self::Bearer,
            AuthScheme::Header => Self::Header {
                header_name: def
                    .header_name
                    .clone()
                    .ok_or_else(|| "header auth is missing its header_name".to_owned())?,
            },
        })
    }

    /// Equality with HTTP's case-insensitive header names.
    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Header { header_name: a }, Self::Header { header_name: b }) => {
                a.eq_ignore_ascii_case(b)
            }
            _ => self == other,
        }
    }

    /// Human-readable, for the control panel and error messages.
    pub fn describe(&self) -> String {
        match self {
            Self::Basic => "HTTP Basic auth".to_owned(),
            Self::Bearer => "a Bearer token in the Authorization header".to_owned(),
            Self::Header { header_name } => format!("the {header_name} header"),
        }
    }
}

/// A stored credential. `Debug` is redacting so it can't leak via logs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Credential {
    /// e.g. Jira Cloud: email as `username`, API token as `password`.
    Basic { username: String, password: String },
    /// Bearer or custom-header token.
    Token { token: String },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::Token { .. } => f
                .debug_struct("Token")
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

impl Credential {
    /// Whether this credential's shape is the one `delivery` needs.
    fn fits(&self, delivery: &Delivery) -> bool {
        matches!(
            (self, delivery),
            (Self::Basic { .. }, Delivery::Basic)
                | (
                    Self::Token { .. },
                    Delivery::Bearer | Delivery::Header { .. }
                )
        )
    }
}

/// Current envelope version written to the secret store.
const STORED_SECRET_VERSION: u8 = 2;

/// Envelope written to the secret store: the credential plus everything the
/// user approved about where and how it is sent. Any difference from the
/// tool's current definition withholds the credential (fails closed).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSecret {
    v: u8,
    /// Canonical base URL (see [`toolspec::canonical_base_url`]).
    bound_url: String,
    delivery: Delivery,
    credential: Credential,
}

/// The v1 envelope: bound to a URL only, not to a delivery. Recognized so the
/// UI can explain why re-approval is needed; its credential is never used.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyStoredSecret {
    bound_url: String,
    #[expect(dead_code, reason = "parsed only to recognize the shape")]
    credential: String,
}

/// A stored value, classified.
#[derive(Debug)]
pub enum ParsedSecret {
    /// Current envelope; usable if it matches the tool's definition.
    Bound {
        bound_url: String,
        delivery: Delivery,
        credential: Credential,
    },
    /// v1 envelope: has a destination but no recorded delivery.
    LegacyUrlOnly { bound_url: String },
    /// No recognizable envelope (pre-binding value, or malformed).
    Unbound,
}

/// Wraps a credential with the destination and delivery it is approved for.
/// Saving a secret is the user's approval of both (the control panel shows
/// them first).
pub fn wrap_stored_secret(
    canonical_url: &str,
    delivery: Delivery,
    credential: Credential,
) -> Result<String, String> {
    if !credential.fits(&delivery) {
        return Err(format!(
            "credential shape does not match {}",
            delivery.describe()
        ));
    }
    serde_json::to_string(&StoredSecret {
        v: STORED_SECRET_VERSION,
        bound_url: canonical_url.to_owned(),
        delivery,
        credential,
    })
    .map_err(|e| format!("failed to encode credential: {e}"))
}

/// Classifies a raw stored value. Nothing but a well-formed current envelope
/// whose credential fits its delivery yields a usable credential.
pub fn parse_stored_secret(raw: &str) -> ParsedSecret {
    if let Ok(env) = serde_json::from_str::<StoredSecret>(raw)
        && env.v == STORED_SECRET_VERSION
        && env.credential.fits(&env.delivery)
    {
        return ParsedSecret::Bound {
            bound_url: env.bound_url,
            delivery: env.delivery,
            credential: env.credential,
        };
    }
    match serde_json::from_str::<LegacyStoredSecret>(raw) {
        Ok(legacy) => ParsedSecret::LegacyUrlOnly {
            bound_url: legacy.bound_url,
        },
        Err(_) => ParsedSecret::Unbound,
    }
}

/// A tool's credential state with respect to its approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingState {
    /// No credential stored.
    NotConfigured,
    /// Stored, and approved for exactly the tool's current destination and
    /// delivery.
    Configured,
    /// Approved for a different destination than the tool now declares.
    /// `bound_url` lets the UI show "was X, now Y"; `None` means unbound.
    DestinationChanged { bound_url: Option<String> },
    /// Same destination, but approved for a different delivery (or, for
    /// `None`, saved before deliveries were recorded).
    DeliveryChanged { approved: Option<Delivery> },
}

/// Decides whether `stored_value` may be released for `def`. `Ok` carries the
/// credential and its (current == approved) delivery; `Err` carries the state
/// to report and a message for the agent.
fn release<'a>(
    def: &ToolDef,
    parsed: &'a ParsedSecret,
) -> Result<(&'a Delivery, &'a Credential), (BindingState, String)> {
    const FIX: &str = "re-confirm it in the GAVMO control panel";

    let current_url = toolspec::canonical_base_url(&def.base_url).map_err(|e| {
        (
            BindingState::DestinationChanged { bound_url: None },
            format!("integration base_url could not be canonicalized for binding check: {e}"),
        )
    })?;
    let current_delivery =
        Delivery::of(def).map_err(|e| (BindingState::DeliveryChanged { approved: None }, e))?;

    let url_changed = |bound: &str| {
        (
            BindingState::DestinationChanged {
                bound_url: Some(bound.to_owned()),
            },
            format!(
                "credential was approved for {bound} but this tool now points at {current_url}; {FIX}"
            ),
        )
    };

    match parsed {
        ParsedSecret::Unbound => Err((
            BindingState::DestinationChanged { bound_url: None },
            format!(
                "credential is not bound to a destination (saved before URL-binding, or the \
                 stored value is malformed); {FIX}"
            ),
        )),
        ParsedSecret::LegacyUrlOnly { bound_url } | ParsedSecret::Bound { bound_url, .. }
            if *bound_url != current_url =>
        {
            Err(url_changed(bound_url))
        }
        ParsedSecret::LegacyUrlOnly { .. } => Err((
            BindingState::DeliveryChanged { approved: None },
            format!(
                "credential was saved before GAVMO recorded how it may be sent; {FIX} \
                 (this tool sends it as {})",
                current_delivery.describe()
            ),
        )),
        ParsedSecret::Bound { delivery, .. } if !delivery.same_as(&current_delivery) => Err((
            BindingState::DeliveryChanged {
                approved: Some(delivery.clone()),
            },
            format!(
                "credential was approved to be sent as {} but this tool now sends it as {}; {FIX}",
                delivery.describe(),
                current_delivery.describe()
            ),
        )),
        ParsedSecret::Bound {
            delivery,
            credential,
            ..
        } => Ok((delivery, credential)),
    }
}

/// Classifies a tool's credential state from its current definition and the
/// raw stored value. Shared by boot-time resolution and the control panel.
pub fn classify_binding(def: &ToolDef, stored_value: Option<&str>) -> BindingState {
    let Some(raw) = stored_value else {
        return BindingState::NotConfigured;
    };
    match release(def, &parse_stored_secret(raw)) {
        Ok(_) => BindingState::Configured,
        Err((state, _)) => state,
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
    // credential only to the destination, and in the way, it was approved for.
    let parsed = parse_stored_secret(&secret_value);
    let (delivery, credential) = match release(&integration.def, &parsed) {
        Ok(approved) => approved,
        Err((_, message)) => return mcp::ToolOutcome::Error(message),
    };

    let request = apply_auth(client.request(method, url), delivery, credential);
    let request = match body {
        Some(body) => request.body(body),
        None => request,
    };

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => return mcp::ToolOutcome::Error(format!("request failed: {e}")),
    };
    let status = response.status();
    // Surfaced so the agent can follow a redirect itself, through the
    // host-lock; the client never follows one (see `forwarding_client`).
    let location = status
        .is_redirection()
        .then(|| response.headers().get(reqwest::header::LOCATION))
        .flatten()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    let body = match read_capped(response, MAX_RESPONSE_BYTES).await {
        Ok(body) => body,
        Err(e) => return mcp::ToolOutcome::Error(format!("failed to read response body: {e}")),
    };

    let redirect_note = location.map_or_else(String::new, |location| {
        format!(
            "Location: {location}\n(redirects are not followed; request that path explicitly if it is on this integration)\n"
        )
    });
    let payload = format!("HTTP {}\n{redirect_note}\n{body}", status.as_u16());
    if status.is_success() {
        mcp::ToolOutcome::Success(payload)
    } else {
        mcp::ToolOutcome::Error(payload)
    }
}

/// Attaches an approved credential per its delivery. Shapes always fit:
/// [`parse_stored_secret`] rejects envelopes where they don't.
fn apply_auth(
    request: reqwest::RequestBuilder,
    delivery: &Delivery,
    credential: &Credential,
) -> reqwest::RequestBuilder {
    match (delivery, credential) {
        (Delivery::Basic, Credential::Basic { username, password }) => {
            request.basic_auth(username, Some(password))
        }
        (Delivery::Bearer, Credential::Token { token }) => request.bearer_auth(token),
        (Delivery::Header { header_name }, Credential::Token { token }) => {
            request.header(header_name.as_str(), token.as_str())
        }
        // Unreachable by construction; never send a mismatched credential.
        _ => request,
    }
}

/// Streams at most `limit` bytes of the body - never buffering more, however
/// large the upstream response - and decodes it lossily, with a notice if
/// anything was cut off.
async fn read_capped(mut response: reqwest::Response, limit: usize) -> reqwest::Result<String> {
    let mut buf = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        let room = limit.saturating_sub(buf.len());
        if chunk.len() > room {
            buf.extend_from_slice(chunk.get(..room).unwrap_or_default());
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(decode_capped(&buf, truncated, limit))
}

/// Decodes a (possibly cut-off) body. A cut can land inside a multi-byte
/// character; that incomplete tail is dropped rather than shown as `�`.
fn decode_capped(buf: &[u8], truncated: bool, limit: usize) -> String {
    let text = match std::str::from_utf8(buf) {
        Ok(text) => text.to_owned(),
        Err(e) if truncated && e.error_len().is_none() => {
            String::from_utf8_lossy(buf.get(..e.valid_up_to()).unwrap_or_default()).into_owned()
        }
        Err(_) => String::from_utf8_lossy(buf).into_owned(),
    };
    if truncated {
        format!("{text}\n\n[... response truncated at {limit} bytes ...]")
    } else {
        text
    }
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

    // --- Binding (destination + delivery) ---

    fn jira_basic() -> Credential {
        Credential::Basic {
            username: "a@b.c".to_owned(),
            password: "t".to_owned(),
        }
    }

    fn token(t: &str) -> Credential {
        Credential::Token {
            token: t.to_owned(),
        }
    }

    fn header(name: &str) -> Delivery {
        Delivery::Header {
            header_name: name.to_owned(),
        }
    }

    /// A tool definition at `base_url` with the given delivery.
    fn def(base_url: &str, delivery: &Delivery) -> ToolDef {
        let (auth, header_line) = match delivery {
            Delivery::Basic => ("basic", String::new()),
            Delivery::Bearer => ("bearer", String::new()),
            Delivery::Header { header_name } => {
                ("header", format!("    header_name: {header_name}\n"))
            }
        };
        let yaml = format!(
            "tools:\n  t:\n    type: http\n    base_url: {base_url}\n    auth: {auth}\n{header_line}    secret: s\n"
        );
        toolspec::parse_config(&yaml)
            .unwrap()
            .tools
            .remove("t")
            .unwrap()
    }

    fn bound(base_url: &str, delivery: Delivery, credential: Credential) -> String {
        wrap_stored_secret(
            &toolspec::canonical_base_url(base_url).unwrap(),
            delivery,
            credential,
        )
        .unwrap()
    }

    /// A properly-bound Basic credential for the jira integration.
    fn bound_jira_secret() -> String {
        bound("https://dafni.atlassian.net", Delivery::Basic, jira_basic())
    }

    #[test]
    fn stored_secret_envelope_round_trips() {
        let wrapped = bound("https://h.example.com", header("X-Key"), token("abc"));
        match parse_stored_secret(&wrapped) {
            ParsedSecret::Bound {
                bound_url,
                delivery,
                credential: Credential::Token { token },
            } => {
                assert_eq!(bound_url, "https://h.example.com:443/");
                assert_eq!(delivery, header("X-Key"));
                assert_eq!(token, "abc");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn wrap_refuses_a_credential_that_does_not_fit_its_delivery() {
        assert!(wrap_stored_secret("https://h/", Delivery::Basic, token("t")).is_err());
        assert!(wrap_stored_secret("https://h/", Delivery::Bearer, jira_basic()).is_err());
    }

    #[test]
    fn parse_classifies_bare_legacy_and_malformed_values() {
        assert!(matches!(
            parse_stored_secret(r#"{"username":"a","password":"b"}"#),
            ParsedSecret::Unbound
        ));
        assert!(matches!(
            parse_stored_secret("not json"),
            ParsedSecret::Unbound
        ));
        let v1 = r#"{"bound_url":"https://h.example.com:443/","credential":"{\"token\":\"t\"}"}"#;
        assert!(matches!(
            parse_stored_secret(v1),
            ParsedSecret::LegacyUrlOnly { bound_url } if bound_url == "https://h.example.com:443/"
        ));
        // A hand-edited envelope whose credential doesn't fit is not usable.
        let mismatched = r#"{"v":2,"bound_url":"https://h/","delivery":{"auth":"basic"},"credential":{"token":"t"}}"#;
        assert!(matches!(
            parse_stored_secret(mismatched),
            ParsedSecret::Unbound
        ));
    }

    #[test]
    fn credential_debug_is_redacted() {
        let debug = format!("{:?} {:?}", jira_basic(), token("hunter2"));
        assert!(debug.contains("a@b.c"), "{debug}");
        assert!(
            !debug.contains("hunter2") && !debug.contains("\"t\""),
            "{debug}"
        );
    }

    #[test]
    fn classify_binding_configured_only_for_exact_destination_and_delivery() {
        let base = "https://dafni.atlassian.net";
        let tool = def(base, &header("X-API-Key"));

        assert_eq!(classify_binding(&tool, None), BindingState::NotConfigured);
        assert_eq!(
            classify_binding(&tool, Some(&bound(base, header("X-API-Key"), token("t")))),
            BindingState::Configured
        );
        // Header names are case-insensitive in HTTP.
        assert_eq!(
            classify_binding(&tool, Some(&bound(base, header("x-api-key"), token("t")))),
            BindingState::Configured
        );
        // Trailing slash is cosmetic.
        assert_eq!(
            classify_binding(
                &tool,
                Some(&bound(
                    "https://dafni.atlassian.net/",
                    header("X-API-Key"),
                    token("t")
                ))
            ),
            BindingState::Configured
        );
    }

    #[test]
    fn classify_binding_reports_a_changed_destination() {
        let tool = def("https://dafni.atlassian.net", &Delivery::Basic);
        let stored = bound("https://evil.com", Delivery::Basic, jira_basic());
        assert_eq!(
            classify_binding(&tool, Some(&stored)),
            BindingState::DestinationChanged {
                bound_url: Some("https://evil.com:443/".to_owned())
            }
        );
    }

    /// The attack this binding exists for: the repo keeps the URL but changes
    /// how the credential is sent.
    #[test]
    fn classify_binding_reports_a_changed_delivery() {
        let base = "https://h.example.com";
        let stored = bound(base, Delivery::Bearer, token("t"));
        for now in [header("X-Echo"), Delivery::Basic] {
            assert_eq!(
                classify_binding(&def(base, &now), Some(&stored)),
                BindingState::DeliveryChanged {
                    approved: Some(Delivery::Bearer)
                },
                "{now:?}"
            );
        }
        let stored = bound(base, header("X-API-Key"), token("t"));
        assert_eq!(
            classify_binding(&def(base, &header("Cookie")), Some(&stored)),
            BindingState::DeliveryChanged {
                approved: Some(header("X-API-Key"))
            }
        );
    }

    #[test]
    fn classify_binding_requires_reapproval_for_v1_and_bare_values() {
        let base = "https://h.example.com";
        let tool = def(base, &Delivery::Bearer);
        let v1_same =
            r#"{"bound_url":"https://h.example.com:443/","credential":"{\"token\":\"t\"}"}"#;
        assert_eq!(
            classify_binding(&tool, Some(v1_same)),
            BindingState::DeliveryChanged { approved: None }
        );
        let v1_other = r#"{"bound_url":"https://evil.com:443/","credential":"{\"token\":\"t\"}"}"#;
        assert_eq!(
            classify_binding(&tool, Some(v1_other)),
            BindingState::DestinationChanged {
                bound_url: Some("https://evil.com:443/".to_owned())
            }
        );
        assert_eq!(
            classify_binding(&tool, Some(r#"{"token":"t"}"#)),
            BindingState::DestinationChanged { bound_url: None }
        );
    }

    async fn forward_with(stored: String) -> mcp::ToolOutcome {
        forward(
            &jira_integration(),
            Some(stored),
            &args(serde_json::json!({"method": "GET", "path": "/rest/api/3/myself"})),
            &reqwest::Client::new(),
        )
        .await
    }

    fn assert_error_contains(outcome: mcp::ToolOutcome, needle: &str) {
        match outcome {
            mcp::ToolOutcome::Error(text) => assert!(text.contains(needle), "{text}"),
            other @ mcp::ToolOutcome::Success(_) => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_rejects_a_credential_bound_to_a_different_url() {
        let stored = bound("https://evil.com", Delivery::Basic, jira_basic());
        assert_error_contains(forward_with(stored).await, "re-confirm");
    }

    #[tokio::test]
    async fn forward_rejects_a_credential_approved_for_a_different_delivery() {
        let stored = bound("https://dafni.atlassian.net", Delivery::Bearer, token("t"));
        assert_error_contains(forward_with(stored).await, "approved to be sent as");
    }

    #[tokio::test]
    async fn forward_rejects_bare_and_v1_credentials() {
        assert_error_contains(
            forward_with(r#"{"username":"a@b.c","password":"t"}"#.to_owned()).await,
            "not bound",
        );
        let v1 = r#"{"bound_url":"https://dafni.atlassian.net:443/","credential":"{\"username\":\"a\",\"password\":\"b\"}"}"#;
        assert_error_contains(forward_with(v1.to_owned()).await, "saved before");
    }

    #[test]
    fn a_bound_secret_for_the_integration_passes_its_own_binding_check() {
        let integration = jira_integration();
        assert_eq!(
            classify_binding(&integration.def, Some(&bound_jira_secret())),
            BindingState::Configured,
            "a freshly-bound credential must pass its own tool's binding check"
        );
    }

    // --- Auth injection ---

    fn built_request(delivery: &Delivery, credential: &Credential) -> reqwest::Request {
        let client = reqwest::Client::new();
        apply_auth(client.get("https://h.example.com/x"), delivery, credential)
            .build()
            .unwrap()
    }

    #[test]
    fn apply_auth_basic_sets_base64_authorization_header() {
        let creds = Credential::Basic {
            username: "user".to_owned(),
            password: "pass".to_owned(),
        };
        let req = built_request(&Delivery::Basic, &creds);
        // base64("user:pass") == "dXNlcjpwYXNz"
        assert_eq!(
            req.headers().get("authorization").unwrap(),
            "Basic dXNlcjpwYXNz"
        );
    }

    #[test]
    fn apply_auth_bearer_sets_bearer_authorization_header() {
        let req = built_request(&Delivery::Bearer, &token("abc123"));
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer abc123");
    }

    #[test]
    fn apply_auth_header_sets_the_named_header() {
        let req = built_request(&header("X-API-Key"), &token("abc123"));
        assert_eq!(req.headers().get("x-api-key").unwrap(), "abc123");
    }

    /// A mismatched pair (impossible via parse) must send no credential.
    #[test]
    fn apply_auth_sends_nothing_for_a_mismatched_pair() {
        let req = built_request(&Delivery::Bearer, &jira_basic());
        assert!(req.headers().get("authorization").is_none());
    }

    #[test]
    fn decode_capped_passes_small_bodies_through_unchanged() {
        assert_eq!(decode_capped(b"hello", false, 10), "hello");
    }

    #[test]
    fn decode_capped_drops_a_split_multibyte_tail() {
        // "é" is 0xC3 0xA9; cut after the first byte.
        let capped = decode_capped(b"ab\xC3", true, 3);
        assert_eq!(capped, "ab\n\n[... response truncated at 3 bytes ...]");
    }

    #[test]
    fn decode_capped_keeps_invalid_utf8_visible_when_not_truncated() {
        assert_eq!(decode_capped(b"a\xFFb", false, 10), "a\u{FFFD}b");
    }

    // --- forward() against a real local server ---

    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    use axum::routing::get;

    /// Serves `router` on an ephemeral loopback port; returns its base URL.
    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    /// A header-auth integration at `base_url`, plus a correctly bound secret.
    fn header_integration(base_url: &str) -> (ResolvedIntegration, String) {
        let yaml = format!(
            "tools:\n  t:\n    type: http\n    base_url: {base_url}\n    auth: header\n    header_name: X-API-Key\n    secret: s\n"
        );
        let mut config = toolspec::parse_config(&yaml).unwrap();
        let integration = ResolvedIntegration {
            id: "t".to_owned(),
            def: config.tools.remove("t").unwrap(),
            secret_key: "k".to_owned(),
        };
        let secret = bound(base_url, header("X-API-Key"), token("SUPER-SECRET"));
        (integration, secret)
    }

    /// The credential must never reach another origin via a redirect: custom
    /// headers are not stripped by reqwest on cross-host redirects.
    #[tokio::test]
    async fn forward_never_follows_a_redirect_to_another_origin() {
        let seen: Arc<Mutex<Vec<HeaderMap>>> = Arc::default();
        let seen_by_evil = Arc::clone(&seen);
        let evil = serve(Router::new().route(
            "/steal",
            get(move |headers: HeaderMap| async move {
                seen_by_evil.lock().unwrap().push(headers);
                "stolen"
            }),
        ))
        .await;
        let target = format!("{evil}/steal");
        let origin = serve(Router::new().route(
            "/go",
            get(move || async move { axum::response::Redirect::temporary(&target) }),
        ))
        .await;

        let (integration, secret) = header_integration(&origin);
        let outcome = forward(
            &integration,
            Some(secret),
            &args(serde_json::json!({"method": "GET", "path": "/go"})),
            &forwarding_client().unwrap(),
        )
        .await;

        match outcome {
            mcp::ToolOutcome::Error(text) => {
                assert!(text.starts_with("HTTP 307"), "{text}");
                assert!(text.contains("Location: "), "{text}");
            }
            other @ mcp::ToolOutcome::Success(_) => panic!("expected a 3xx error, got {other:?}"),
        }
        assert!(
            seen.lock().unwrap().is_empty(),
            "the redirect target must never be contacted"
        );
    }

    #[tokio::test]
    async fn forward_sends_the_credential_to_the_approved_origin() {
        let origin = serve(Router::new().route(
            "/me",
            get(|headers: HeaderMap| async move {
                headers
                    .get("x-api-key")
                    .map(|v| v.to_str().unwrap().to_owned())
                    .unwrap_or_default()
            }),
        ))
        .await;
        let (integration, secret) = header_integration(&origin);
        let outcome = forward(
            &integration,
            Some(secret),
            &args(serde_json::json!({"method": "GET", "path": "/me"})),
            &forwarding_client().unwrap(),
        )
        .await;
        match outcome {
            mcp::ToolOutcome::Success(text) => assert_eq!(text, "HTTP 200\n\nSUPER-SECRET"),
            other @ mcp::ToolOutcome::Error(_) => panic!("expected success, got {other:?}"),
        }
    }

    /// A huge response is cut at the cap while streaming.
    #[tokio::test]
    async fn forward_caps_oversized_responses() {
        let origin = serve(Router::new().route(
            "/big",
            get(|| async { "a".repeat(4 * MAX_RESPONSE_BYTES).into_response() }),
        ))
        .await;
        let (integration, secret) = header_integration(&origin);
        let outcome = forward(
            &integration,
            Some(secret),
            &args(serde_json::json!({"method": "GET", "path": "/big"})),
            &forwarding_client().unwrap(),
        )
        .await;
        let mcp::ToolOutcome::Success(text) = outcome else {
            panic!("expected success, got {outcome:?}");
        };
        assert!(text.ends_with("[... response truncated at 262144 bytes ...]"));
        assert!(text.len() < MAX_RESPONSE_BYTES + 200, "{}", text.len());
    }
}
