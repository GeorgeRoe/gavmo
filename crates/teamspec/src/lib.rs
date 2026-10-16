//! GAVMO's tool-configuration schema + the host-lock URL math (v1 — see
//! `gavmo-tools-config-plan.md` §13a).
//!
//! This crate is the shared, mostly-pure vocabulary for *what tools a workspace
//! wants*:
//!
//! - [`TeamSpec`] — the team-wide manifest (fetched from a URL) declaring tools
//!   the whole team can use (e.g. "DAFNI Jira lives here; supply a secret named
//!   `jira-cred`"). Holds **references and instructions, never secrets**.
//! - [`GavmoConfig`] — the per-repo `gavmo.yaml`: an **untrusted** pointer at a
//!   team spec + an opt-in list, plus any inline project-only tool requests.
//! - [`lock_url`] — the single security-relevant primitive kept even in v1: it
//!   resolves an agent-supplied path against an integration's `base_url` and
//!   **refuses to produce a URL on any other host/scheme/port**, so the host can
//!   never be tricked into forwarding a credential to `evil.com`.
//!
//! ## v1 posture (security deferred)
//!
//! Signature verification of a fetched spec is **not** implemented — [`verify`]
//! is a documented no-op seam so the call site exists now and the real
//! minisign/Ed25519 check (plan §5) drops in later without reshaping callers.
//! Both files are YAML; GAVMO uses no TOML.

use std::collections::HashMap;

use serde::Deserialize;
use url::Url;

/// The only `TeamSpec::version` this build understands. Bumped when the schema
/// changes incompatibly; [`parse_team_spec`] rejects anything else up front so a
/// newer manifest fails loudly instead of being half-parsed.
pub const SUPPORTED_SPEC_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// The kind of tool an integration exposes. v1 ships exactly one: a generic,
/// host-locked HTTP forwarder (`type: http`) — one type covers Jira, Supabase,
/// etc. with zero per-service code (plan §13a). `deny_unknown`-free on purpose:
/// an unknown `type` is caught by this enum's own deserialization error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// Generic HTTP forwarder: the host prepends `base_url`, host-locks the
    /// result (see [`lock_url`]), injects the credential per [`AuthScheme`], and
    /// forwards the agent's method/path/body.
    Http,
}

/// How the host authenticates to the integration's endpoint. The credential
/// value itself lives in the secret store (keyed by [`ToolDef::secret`]); this
/// only says *how* to attach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthScheme {
    /// HTTP Basic. Credential value is a JSON `{username, password}` — e.g.
    /// Jira Cloud's `email` + API token.
    Basic,
    /// `Authorization: Bearer <token>`. Credential value is the bare token.
    Bearer,
    /// A custom header (name from [`ToolDef::header_name`]) set to the token.
    Header,
}

/// A single integration definition: everything the host needs to forward a
/// request *except* the secret value. Shared by [`TeamSpec`] tools (which carry
/// an `id` alongside — see [`TeamTool`]) and [`GavmoConfig`] inline tools (where
/// the map key is the id).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: ToolKind,
    /// The integration's root URL. Every forwarded request is host-locked to
    /// this (scheme + host + port) — see [`lock_url`].
    pub base_url: String,
    pub auth: AuthScheme,
    /// The *name* of the secret the user must supply (a reference, not a value).
    pub secret: String,
    /// Required iff `auth == Header`: the header name to carry the token.
    #[serde(default)]
    pub header_name: Option<String>,
    /// Human-facing, optional. Surfaced in the control panel / tool description.
    #[serde(default)]
    pub description: Option<String>,
    /// Human-facing setup help (e.g. "create a read-only PAT at …").
    #[serde(default)]
    pub instructions: Option<String>,
    /// Optional path the control panel's "Test" button should GET to verify the
    /// credential (relative to `base_url`, e.g. `rest/api/3/myself` for Jira).
    /// Defaults to `/` (a bare reachability check) when omitted.
    #[serde(default)]
    pub test_path: Option<String>,
}

impl ToolDef {
    /// Checks the self-consistency the type system can't: a non-empty
    /// `base_url` that is a valid http(s) URL with a host, and a `header_name`
    /// present exactly when `auth == Header`.
    pub fn validate(&self, id: &str) -> Result<(), ValidationError> {
        let url = Url::parse(&self.base_url).map_err(|e| ValidationError::BadBaseUrl {
            id: id.to_string(),
            reason: e.to_string(),
        })?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(ValidationError::BadBaseUrl {
                id: id.to_string(),
                reason: "base_url must be an http(s) URL with a host".to_string(),
            });
        }
        match self.auth {
            AuthScheme::Header if self.header_name.as_deref().unwrap_or("").is_empty() => {
                Err(ValidationError::MissingHeaderName { id: id.to_string() })
            }
            _ => Ok(()),
        }
    }
}

/// A [`ToolDef`] plus its `id`, as it appears in a [`TeamSpec`]'s `tools` list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TeamTool {
    pub id: String,
    #[serde(flatten)]
    pub def: ToolDef,
}

/// The team-wide manifest fetched from a URL.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TeamSpec {
    pub version: u32,
    /// Human-readable team label (e.g. "DAFNI"); display-only.
    pub team: String,
    #[serde(default)]
    pub tools: Vec<TeamTool>,
}

impl TeamSpec {
    /// Looks up a declared tool by id.
    pub fn tool(&self, id: &str) -> Option<&TeamTool> {
        self.tools.iter().find(|t| t.id == id)
    }
}

/// The per-repo `gavmo.yaml`. **Untrusted** (repo-controlled): it may only
/// *request* and *point*, never supply a credential.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GavmoConfig {
    /// Optional reference to a team spec + which of its tools this repo opts into.
    pub team: Option<TeamRef>,
    /// Optional inline project-scoped tool requests, keyed by id.
    pub tools: HashMap<String, ToolDef>,
}

/// A repo's reference to a team spec: where to fetch it, and which of its tools
/// this repo wants enabled.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TeamRef {
    /// Raw HTTPS URL of the team-spec YAML (v1: no signature — plan §13a).
    pub spec: String,
    /// Ids of team tools this repo opts into.
    #[serde(default)]
    pub tools: Vec<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Schema-level problems found after a YAML document parses structurally.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("unsupported team spec version {found} (this build supports {SUPPORTED_SPEC_VERSION})")]
    UnsupportedVersion { found: u32 },
    #[error("tool {id:?} has an invalid base_url: {reason}")]
    BadBaseUrl { id: String, reason: String },
    #[error("tool {id:?} uses auth: header but has no header_name")]
    MissingHeaderName { id: String },
    #[error("duplicate tool id {id:?} in team spec")]
    DuplicateToolId { id: String },
}

/// Anything that can go wrong turning YAML text into a validated schema value.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error(transparent)]
    Validation(#[from] ValidationError),
}

/// Anything that can go wrong fetching + parsing a team spec from a URL.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("fetching team spec failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Parse(#[from] ParseError),
}

/// Why a request couldn't be host-locked to an integration's `base_url`. Every
/// variant means "refuse to send" — these are the states that would otherwise
/// leak a credential to the wrong place.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HostLockError {
    #[error("integration base_url is not a valid URL: {0}")]
    InvalidBaseUrl(String),
    #[error("integration base_url has no host: {0}")]
    BaseUrlHasNoHost(String),
    #[error("requested path produced an invalid URL: {0}")]
    InvalidResult(String),
    #[error(
        "blocked: request would go to {requested} but this integration is locked to {expected}"
    )]
    HostMismatch { expected: String, requested: String },
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parses and validates a team-spec YAML document.
pub fn parse_team_spec(yaml: &str) -> Result<TeamSpec, ParseError> {
    let spec: TeamSpec = serde_yaml::from_str(yaml)?;
    if spec.version != SUPPORTED_SPEC_VERSION {
        return Err(ValidationError::UnsupportedVersion {
            found: spec.version,
        }
        .into());
    }
    let mut seen = std::collections::HashSet::new();
    for tool in &spec.tools {
        if !seen.insert(tool.id.as_str()) {
            return Err(ValidationError::DuplicateToolId {
                id: tool.id.clone(),
            }
            .into());
        }
        tool.def.validate(&tool.id)?;
    }
    Ok(spec)
}

/// Parses and validates a `gavmo.yaml` document.
pub fn parse_config(yaml: &str) -> Result<GavmoConfig, ParseError> {
    let config: GavmoConfig = serde_yaml::from_str(yaml)?;
    for (id, def) in &config.tools {
        def.validate(id)?;
    }
    Ok(config)
}

/// Fetches a team spec over HTTPS and parses it.
///
/// **v1 (security deferred):** the response is trusted after a plain
/// `error_for_status` — there is no signature check. [`verify`] marks where that
/// check will go.
pub async fn fetch_spec(url: &str) -> Result<TeamSpec, FetchError> {
    let body = reqwest::get(url).await?.error_for_status()?.text().await?;
    Ok(parse_team_spec(&body)?)
}

/// Signature-verification seam. **No-op in v1** (always `Ok`) — placeholder for
/// the minisign/Ed25519 check described in `gavmo-tools-config-plan.md` §5. Kept
/// as real API so the import path can call it today and only its body changes
/// later.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("signature verification failed")]
    Invalid,
}

/// See [`VerifyError`]. Returns `Ok(())` unconditionally in v1.
pub fn verify(_spec_bytes: &[u8], _signature: &[u8], _public_key: &str) -> Result<(), VerifyError> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Host-lock (the one security-relevant primitive kept in v1)
// ---------------------------------------------------------------------------

/// Resolves an agent-supplied `path` against an integration's `base_url` and
/// returns the full URL **only if it stays on the exact same scheme + host +
/// port**. Any attempt to reach a different origin — an absolute URL, a
/// scheme-relative `//other-host`, a different port — is rejected with
/// [`HostLockError::HostMismatch`], so the host never forwards the credential
/// off-origin.
///
/// `path` is always treated as **relative to `base_url`'s full path**, so a
/// `base_url` with a path prefix (e.g. an Atlassian gateway base like
/// `https://api.atlassian.com/ex/jira/<cloudId>`) is preserved rather than
/// discarded. Two normalizations make that work against `Url::join`'s standard
/// RFC-3986 resolution:
///
/// - the base's path is made to end in `/` (so `join` appends under it rather
///   than replacing its last segment), and
/// - a *single* leading `/` on `path` is stripped (so a caller writing
///   `/rest/api/3/myself` means "under the base", not "from the host root").
///
/// The single-slash strip is deliberately **not** applied to a `//host` network-
/// path reference: that form legitimately overrides the host and is exactly the
/// cross-origin attack the host-lock check below must still catch (see the
/// `lock_url_rejects_scheme_relative_other_host` test). Absolute URLs
/// (`https://other/…`) are likewise left for the origin check to reject. So this
/// normalization only ever changes which same-origin *path* is hit — never
/// whether the origin check can be bypassed.
pub fn lock_url(base_url: &str, path: &str) -> Result<Url, HostLockError> {
    let mut base =
        Url::parse(base_url).map_err(|e| HostLockError::InvalidBaseUrl(e.to_string()))?;
    if base.host_str().is_none() {
        return Err(HostLockError::BaseUrlHasNoHost(base_url.to_string()));
    }

    // Ensure the base path ends in `/` so `join` resolves `path` *under* it
    // rather than replacing its final segment. Harmless for a root-only base
    // (its path is already `/`).
    if !base.path().ends_with('/') {
        let with_slash = format!("{}/", base.path());
        base.set_path(&with_slash);
    }

    // Treat a single leading slash as "relative to the base path", not "from the
    // host root". A `//host` network-path reference is left intact so the origin
    // check still rejects it.
    let relative_path = if path.starts_with('/') && !path.starts_with("//") {
        &path[1..]
    } else {
        path
    };

    let resolved = base
        .join(relative_path)
        .map_err(|e| HostLockError::InvalidResult(e.to_string()))?;

    // Compare the full origin: scheme, host, and effective port. Using
    // `port_or_known_default` means https default 443 == explicit :443.
    let same_origin = resolved.scheme() == base.scheme()
        && resolved.host_str() == base.host_str()
        && resolved.port_or_known_default() == base.port_or_known_default();

    if !same_origin {
        return Err(HostLockError::HostMismatch {
            expected: origin_string(&base),
            requested: origin_string(&resolved),
        });
    }
    Ok(resolved)
}

/// A compact "scheme://host:port" for error messages (best-effort; only used in
/// the rejected path).
fn origin_string(u: &Url) -> String {
    match (u.host_str(), u.port_or_known_default()) {
        (Some(h), Some(p)) => format!("{}://{h}:{p}", u.scheme()),
        (Some(h), None) => format!("{}://{h}", u.scheme()),
        _ => u.scheme().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM_SPEC: &str = r#"
version: 1
team: DAFNI
tools:
  - id: dafni-jira
    type: http
    base_url: https://dafni.atlassian.net
    auth: basic
    secret: jira-cred
    description: "DAFNI Jira (read via your PAT)"
    instructions: "Create an API token at id.atlassian.net; provide your email + token."
"#;

    #[test]
    fn parses_a_valid_team_spec() {
        let spec = parse_team_spec(TEAM_SPEC).unwrap();
        assert_eq!(spec.version, 1);
        assert_eq!(spec.team, "DAFNI");
        let tool = spec.tool("dafni-jira").expect("tool present");
        assert_eq!(tool.def.kind, ToolKind::Http);
        assert_eq!(tool.def.auth, AuthScheme::Basic);
        assert_eq!(tool.def.base_url, "https://dafni.atlassian.net");
        assert_eq!(tool.def.secret, "jira-cred");
    }

    #[test]
    fn rejects_unsupported_version() {
        let yaml = "version: 2\nteam: X\ntools: []\n";
        let err = parse_team_spec(yaml).unwrap_err();
        assert!(matches!(
            err,
            ParseError::Validation(ValidationError::UnsupportedVersion { found: 2 })
        ));
    }

    #[test]
    fn rejects_duplicate_tool_ids() {
        let yaml = r#"
version: 1
team: X
tools:
  - id: dup
    type: http
    base_url: https://a.example.com
    auth: bearer
    secret: s
  - id: dup
    type: http
    base_url: https://b.example.com
    auth: bearer
    secret: s
"#;
        let err = parse_team_spec(yaml).unwrap_err();
        assert!(matches!(
            err,
            ParseError::Validation(ValidationError::DuplicateToolId { .. })
        ));
    }

    #[test]
    fn rejects_header_auth_without_header_name() {
        let yaml = r#"
version: 1
team: X
tools:
  - id: t
    type: http
    base_url: https://a.example.com
    auth: header
    secret: s
"#;
        let err = parse_team_spec(yaml).unwrap_err();
        assert!(matches!(
            err,
            ParseError::Validation(ValidationError::MissingHeaderName { .. })
        ));
    }

    #[test]
    fn rejects_non_http_base_url() {
        let yaml = r#"
version: 1
team: X
tools:
  - id: t
    type: http
    base_url: "ftp://a.example.com"
    auth: bearer
    secret: s
"#;
        let err = parse_team_spec(yaml).unwrap_err();
        assert!(matches!(
            err,
            ParseError::Validation(ValidationError::BadBaseUrl { .. })
        ));
    }

    #[test]
    fn rejects_unknown_tool_type() {
        let yaml = r#"
version: 1
team: X
tools:
  - id: t
    type: smtp
    base_url: https://a.example.com
    auth: bearer
    secret: s
"#;
        // Unknown enum variant -> structural YAML/serde error.
        assert!(matches!(parse_team_spec(yaml), Err(ParseError::Yaml(_))));
    }

    #[test]
    fn parses_gavmo_config_with_team_ref_and_inline_tool() {
        let yaml = r#"
team:
  spec: https://gitlab.com/dafni/gavmo-tools/-/raw/main/team-tools.yaml
  tools:
    - dafni-jira
tools:
  project-docs:
    type: http
    base_url: https://project.example.com
    auth: bearer
    secret: project-docs-token
"#;
        let config = parse_config(yaml).unwrap();
        let team = config.team.expect("team ref present");
        assert_eq!(team.tools, vec!["dafni-jira".to_string()]);
        assert!(team.spec.ends_with("team-tools.yaml"));
        let proj = config.tools.get("project-docs").expect("inline tool");
        assert_eq!(proj.auth, AuthScheme::Bearer);
    }

    #[test]
    fn empty_gavmo_config_is_valid_and_default() {
        // A repo with an empty gavmo.yaml requests nothing.
        let config = parse_config("{}\n").unwrap();
        assert!(config.team.is_none());
        assert!(config.tools.is_empty());
    }

    // --- host-lock ---------------------------------------------------------

    #[test]
    fn lock_url_allows_absolute_path_on_same_host() {
        let u = lock_url("https://dafni.atlassian.net", "/rest/api/3/issue/ABC-1").unwrap();
        assert_eq!(
            u.as_str(),
            "https://dafni.atlassian.net/rest/api/3/issue/ABC-1"
        );
    }

    #[test]
    fn lock_url_allows_path_with_query() {
        let u = lock_url("https://h.example.com", "/search?jql=project%3DABC").unwrap();
        assert_eq!(u.host_str(), Some("h.example.com"));
        assert_eq!(u.query(), Some("jql=project%3DABC"));
    }

    // --- base_url with a path prefix (e.g. Atlassian gateway) ---------------

    const GATEWAY: &str = "https://api.atlassian.com/ex/jira/CLOUDID";

    #[test]
    fn lock_url_preserves_base_path_prefix_for_a_leading_slash_path() {
        // The regression: a leading-slash path must NOT discard /ex/jira/CLOUDID.
        let u = lock_url(GATEWAY, "/rest/api/3/myself").unwrap();
        assert_eq!(
            u.as_str(),
            "https://api.atlassian.com/ex/jira/CLOUDID/rest/api/3/myself"
        );
    }

    #[test]
    fn lock_url_preserves_base_path_prefix_for_a_relative_path() {
        let u = lock_url(GATEWAY, "rest/api/3/myself").unwrap();
        assert_eq!(
            u.as_str(),
            "https://api.atlassian.com/ex/jira/CLOUDID/rest/api/3/myself"
        );
    }

    #[test]
    fn lock_url_handles_a_base_that_already_ends_in_slash() {
        // No double slash, prefix still preserved.
        let u = lock_url("https://api.atlassian.com/ex/jira/CLOUDID/", "/rest/x").unwrap();
        assert_eq!(
            u.as_str(),
            "https://api.atlassian.com/ex/jira/CLOUDID/rest/x"
        );
    }

    #[test]
    fn lock_url_preserves_prefix_with_query_string() {
        let u = lock_url(GATEWAY, "/rest/api/3/search?jql=project%3DABC").unwrap();
        assert_eq!(u.path(), "/ex/jira/CLOUDID/rest/api/3/search");
        assert_eq!(u.query(), Some("jql=project%3DABC"));
    }

    #[test]
    fn lock_url_still_rejects_cross_host_even_with_a_base_path_prefix() {
        // The normalization must not weaken the origin check for prefixed bases.
        assert!(matches!(
            lock_url(GATEWAY, "https://evil.com/steal").unwrap_err(),
            HostLockError::HostMismatch { .. }
        ));
        assert!(matches!(
            lock_url(GATEWAY, "//evil.com/steal").unwrap_err(),
            HostLockError::HostMismatch { .. }
        ));
    }

    #[test]
    fn lock_url_rejects_absolute_url_to_another_host() {
        let err = lock_url("https://dafni.atlassian.net", "https://evil.com/steal").unwrap_err();
        assert!(matches!(err, HostLockError::HostMismatch { .. }));
    }

    #[test]
    fn lock_url_rejects_scheme_relative_other_host() {
        let err = lock_url("https://dafni.atlassian.net", "//evil.com/steal").unwrap_err();
        assert!(matches!(err, HostLockError::HostMismatch { .. }));
    }

    #[test]
    fn lock_url_rejects_scheme_downgrade() {
        let err = lock_url("https://h.example.com", "http://h.example.com/x").unwrap_err();
        assert!(matches!(err, HostLockError::HostMismatch { .. }));
    }

    #[test]
    fn lock_url_rejects_different_port() {
        let err = lock_url("https://h.example.com", "https://h.example.com:8443/x").unwrap_err();
        assert!(matches!(err, HostLockError::HostMismatch { .. }));
    }

    #[test]
    fn lock_url_rejects_userinfo_confusion() {
        // A classic trick: "https://trusted.example.com@evil.com/..." — the real
        // host is evil.com. The url crate parses this correctly; we must reject.
        let err = lock_url(
            "https://trusted.example.com",
            "https://trusted.example.com@evil.com/steal",
        )
        .unwrap_err();
        assert!(matches!(err, HostLockError::HostMismatch { .. }));
    }

    #[test]
    fn lock_url_allows_explicit_default_port_match() {
        // https://h:443 is the same origin as https://h.
        let u = lock_url("https://h.example.com", "https://h.example.com:443/x").unwrap();
        assert_eq!(u.host_str(), Some("h.example.com"));
    }

    #[test]
    fn verify_is_a_noop_in_v1() {
        assert!(verify(b"spec", b"sig", "pubkey").is_ok());
    }
}
