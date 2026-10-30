//! Parsing and validation of the per-repo `gavmo.yaml` tool config, plus the
//! URL math that keeps credentials on their integration's origin: [`lock_url`]
//! (host-locking) and [`canonical_base_url`] (URL-binding).
//!
//! `gavmo.yaml` is untrusted: a repo can request a tool and name a secret, but
//! never supply a credential.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use url::Url;

// --- Schema ---

/// The kind of tool an integration exposes. Unknown `type`s fail
/// deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// Generic HTTP forwarder: the host host-locks the agent's path to
    /// `base_url`, attaches the credential, and forwards the request.
    Http,
}

/// How the host attaches the credential stored under [`ToolDef::secret`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthScheme {
    /// HTTP Basic. Credential value is a JSON `{username, password}`.
    Basic,
    /// `Authorization: Bearer <token>`. Credential value is the bare token.
    Bearer,
    /// A custom header (name from [`ToolDef::header_name`]) set to the token.
    Header,
}

impl AuthScheme {
    /// The `auth:` value in `gavmo.yaml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Bearer => "bearer",
            Self::Header => "header",
        }
    }
}

impl fmt::Display for AuthScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Tool ids become MCP tool names, so they follow MCP's naming rule.
pub const MAX_TOOL_ID_LEN: usize = 64;
/// Tool ids starting with this are reserved for GAVMO's own tools
/// (e.g. `gavmo_ping`), so a repo can't shadow or duplicate them.
pub const RESERVED_TOOL_PREFIX: &str = "gavmo_";
/// Most tools one `gavmo.yaml` may request.
pub const MAX_TOOLS: usize = 64;
const MAX_SECRET_NAME_LEN: usize = 128;

/// Headers a tool may not use for its credential: ones that control HTTP
/// framing, routing or connection handling, or carry other credentials.
/// Setting them could smuggle requests, retarget them, or mix credentials.
const FORBIDDEN_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "expect",
    "cookie",
    "proxy-authorization",
    "proxy-connection",
];

/// One integration: everything needed to forward a request except the secret.
/// Unknown keys are rejected, so a typo (`test_paht`) is an error rather than
/// silently ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: ToolKind,
    /// Every forwarded request is host-locked to this URL's origin.
    pub base_url: String,
    pub auth: AuthScheme,
    /// Name of the secret the user must supply (a reference, not a value).
    pub secret: String,
    /// Required iff `auth == Header`.
    #[serde(default)]
    pub header_name: Option<String>,
    /// Shown in the control panel and tool description.
    #[serde(default)]
    pub description: Option<String>,
    /// Setup help shown to the user.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Path (relative to `base_url`) the control panel's "Test" button GETs to
    /// verify the credential; defaults to `/`.
    #[serde(default)]
    pub test_path: Option<String>,
}

impl ToolDef {
    /// Checks everything that isn't structural:
    /// - `id`: 1-64 of `[A-Za-z0-9_-]`, not starting with `gavmo_`;
    /// - `base_url`: http(s) with a host, and no userinfo, query or fragment
    ///   (none of which can be part of a destination a credential is bound
    ///   to);
    /// - `secret`: 1-128 of `[A-Za-z0-9_.-]`;
    /// - `header_name`: required for, and only allowed with, `auth: header`;
    ///   a valid header name that isn't framing/routing/credential-related.
    pub fn validate(&self, id: &str) -> Result<(), ValidationError> {
        validate_tool_id(id)?;
        let bad_url = |reason: &str| ValidationError::BadBaseUrl {
            id: id.to_owned(),
            reason: reason.to_owned(),
        };
        let url = Url::parse(&self.base_url).map_err(|e| bad_url(&e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(bad_url("base_url must be an http(s) URL with a host"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(bad_url("base_url must not contain user:password@"));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(bad_url("base_url must not have a query or fragment"));
        }
        if !is_name(&self.secret, MAX_SECRET_NAME_LEN, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
        }) {
            return Err(ValidationError::BadSecretName { id: id.to_owned() });
        }
        match (self.auth, self.header_name.as_deref()) {
            (AuthScheme::Header, None | Some("")) => {
                Err(ValidationError::MissingHeaderName { id: id.to_owned() })
            }
            (AuthScheme::Header, Some(name)) => validate_header_name(id, name),
            (_, Some(_)) => Err(ValidationError::UnexpectedHeaderName { id: id.to_owned() }),
            (_, None) => Ok(()),
        }
    }
}

/// `true` if `s` is 1..=`max` chars, all accepted by `ok`.
fn is_name(s: &str, max: usize, ok: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(ok)
}

/// Tool ids become MCP tool names: 1-64 of `[A-Za-z0-9_-]`, and the
/// `gavmo_` prefix is reserved.
pub fn validate_tool_id(id: &str) -> Result<(), ValidationError> {
    if !is_name(id, MAX_TOOL_ID_LEN, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
    }) {
        return Err(ValidationError::BadToolId { id: id.to_owned() });
    }
    if id.starts_with(RESERVED_TOOL_PREFIX) {
        return Err(ValidationError::ReservedToolId { id: id.to_owned() });
    }
    Ok(())
}

/// A header name: an RFC 9110 token (letters, digits and a few symbols), not forbidden.
fn validate_header_name(id: &str, name: &str) -> Result<(), ValidationError> {
    let is_token_char = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    let bad = |reason: &str| ValidationError::BadHeaderName {
        id: id.to_owned(),
        reason: reason.to_owned(),
    };
    if !is_name(name, 128, is_token_char) {
        return Err(bad("not a valid HTTP header name"));
    }
    let lower = name.to_ascii_lowercase();
    if FORBIDDEN_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
        return Err(bad(
            "this header controls the request itself and can't carry a credential",
        ));
    }
    Ok(())
}

/// The per-repo `gavmo.yaml` (untrusted). Unknown keys are rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GavmoConfig {
    /// Tool requests, keyed by id. Sorted, so listings are stable.
    pub tools: BTreeMap<String, ToolDef>,
}

// --- Errors ---

/// Problems found after a YAML document parses structurally.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("tool {id:?} has an invalid base_url: {reason}")]
    BadBaseUrl { id: String, reason: String },
    #[error("tool {id:?} uses auth: header but has no header_name")]
    MissingHeaderName { id: String },
    #[error("tool {id:?} sets header_name but doesn't use auth: header")]
    UnexpectedHeaderName { id: String },
    #[error("tool {id:?} has an invalid header_name: {reason}")]
    BadHeaderName { id: String, reason: String },
    #[error("tool id {id:?} must be 1-64 characters of A-Z, a-z, 0-9, _ and -")]
    BadToolId { id: String },
    #[error("tool id {id:?} is reserved (ids starting with gavmo_ are GAVMO's own)")]
    ReservedToolId { id: String },
    #[error(
        "tool {id:?} has an invalid secret name (1-128 characters of A-Z, a-z, 0-9, _, - and .)"
    )]
    BadSecretName { id: String },
    #[error("too many tools ({count}); at most {max} are allowed")]
    TooManyTools { count: usize, max: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error(transparent)]
    Validation(#[from] ValidationError),
}

/// Why a request couldn't be host-locked. Every variant means "refuse to send".
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
    #[error("blocked: path {requested:?} escapes this integration's base path {base_path:?}")]
    PathEscape {
        base_path: String,
        requested: String,
    },
    #[error("blocked: requested path contains an encoded slash or backslash: {0:?}")]
    EncodedSeparator(String),
    #[error("blocked: request URL must not carry userinfo (user:password@)")]
    Userinfo,
}

// --- Parsing ---

/// Parses and validates a `gavmo.yaml` document.
pub fn parse_config(yaml: &str) -> Result<GavmoConfig, ParseError> {
    let config: GavmoConfig = serde_yaml_ng::from_str(yaml)?;
    if config.tools.len() > MAX_TOOLS {
        return Err(ValidationError::TooManyTools {
            count: config.tools.len(),
            max: MAX_TOOLS,
        }
        .into());
    }
    for (id, def) in &config.tools {
        def.validate(id)?;
    }
    Ok(config)
}

// --- Host-lock ---

/// Resolves an agent-supplied `path` under `base_url` and returns it only if it
/// stays on the same scheme, host and port *and* under the base's path
/// prefix, so credentials never leave the destination they were approved for.
///
/// `path` is relative to the base's full path, preserving prefixes such as
/// `https://api.atlassian.com/ex/jira/<cloudId>`. `//host` and absolute URLs
/// are deliberately not normalized, so the origin check still rejects them.
pub fn lock_url(base_url: &str, path: &str) -> Result<Url, HostLockError> {
    let mut base =
        Url::parse(base_url).map_err(|e| HostLockError::InvalidBaseUrl(e.to_string()))?;
    if base.host_str().is_none() {
        return Err(HostLockError::BaseUrlHasNoHost(base_url.to_string()));
    }

    // Without a trailing `/`, `join` replaces the base's last segment.
    if !base.path().ends_with('/') {
        let with_slash = format!("{}/", base.path());
        base.set_path(&with_slash);
    }

    // A single leading `/` means "under the base", not "from the host root".
    // `//host` is left intact so the origin check rejects it.
    let relative_path = match path.strip_prefix('/') {
        Some(rest) if !rest.starts_with('/') => rest,
        _ => path,
    };

    let resolved = base
        .join(relative_path)
        .map_err(|e| HostLockError::InvalidResult(e.to_string()))?;

    // Effective port, so an implicit 443 equals an explicit `:443`.
    let same_origin = resolved.scheme() == base.scheme()
        && resolved.host_str() == base.host_str()
        && resolved.port_or_known_default() == base.port_or_known_default();

    if !same_origin {
        return Err(HostLockError::HostMismatch {
            expected: origin_string(&base),
            requested: origin_string(&resolved),
        });
    }

    // Userinfo in the request URL would override (or add) credentials.
    if !resolved.username().is_empty() || resolved.password().is_some() {
        return Err(HostLockError::Userinfo);
    }

    // The path prefix is part of the security boundary too (e.g. one tenant's
    // `/ex/jira/<cloudId>/` on a shared gateway host). `Url::join` has already
    // resolved `..`, `%2e%2e` and `\`, so a plain prefix check on the
    // normalized path is sound. `base.path()` ends in `/`, so `/a/b` can't
    // match a base of `/a/bc/`.
    if !resolved.path().starts_with(base.path()) {
        return Err(HostLockError::PathEscape {
            base_path: base.path().to_owned(),
            requested: resolved.path().to_owned(),
        });
    }

    // `%2F`/`%5C` survive normalization as literal bytes, but many servers
    // decode them before routing, which would turn `..%2F` back into a
    // traversal on the far side.
    let lower = resolved.path().to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") {
        return Err(HostLockError::EncodedSeparator(resolved.path().to_owned()));
    }

    Ok(resolved)
}

/// Canonical form of a `base_url` (scheme, host, effective port, path prefix)
/// that a stored credential is bound to. Normalized like [`lock_url`] so the two
/// agree on "same destination"; other fields are excluded to avoid re-prompts.
pub fn canonical_base_url(base_url: &str) -> Result<String, HostLockError> {
    let mut url = Url::parse(base_url).map_err(|e| HostLockError::InvalidBaseUrl(e.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| HostLockError::BaseUrlHasNoHost(base_url.to_string()))?
        .to_ascii_lowercase();

    // Same trailing-slash treatment as `lock_url`.
    if !url.path().ends_with('/') {
        let with_slash = format!("{}/", url.path());
        url.set_path(&with_slash);
    }

    let scheme = url.scheme().to_ascii_lowercase();
    let port = url.port_or_known_default();
    let path = url.path();

    Ok(match port {
        Some(p) => format!("{scheme}://{host}:{p}{path}"),
        None => format!("{scheme}://{host}{path}"),
    })
}

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

    #[test]
    fn parses_gavmo_config_with_inline_tools() {
        let yaml = r#"
tools:
  dafni-jira:
    type: http
    base_url: https://dafni.atlassian.net
    auth: basic
    secret: jira-cred
    description: "DAFNI Jira (read via your PAT)"
    instructions: "Create an API token at id.atlassian.net; provide your email + token."
  project-docs:
    type: http
    base_url: https://project.example.com
    auth: bearer
    secret: project-docs-token
"#;
        let config = parse_config(yaml).unwrap();
        let jira = config.tools.get("dafni-jira").expect("jira tool");
        assert_eq!(jira.kind, ToolKind::Http);
        assert_eq!(jira.auth, AuthScheme::Basic);
        assert_eq!(jira.base_url, "https://dafni.atlassian.net");
        assert_eq!(jira.secret, "jira-cred");
        let proj = config.tools.get("project-docs").expect("inline tool");
        assert_eq!(proj.auth, AuthScheme::Bearer);
    }

    #[test]
    fn rejects_header_auth_without_header_name() {
        let yaml = r"
tools:
  t:
    type: http
    base_url: https://a.example.com
    auth: header
    secret: s
";
        let err = parse_config(yaml).unwrap_err();
        assert!(matches!(
            err,
            ParseError::Validation(ValidationError::MissingHeaderName { .. })
        ));
    }

    #[test]
    fn rejects_non_http_base_url() {
        let yaml = r#"
tools:
  t:
    type: http
    base_url: "ftp://a.example.com"
    auth: bearer
    secret: s
"#;
        let err = parse_config(yaml).unwrap_err();
        assert!(matches!(
            err,
            ParseError::Validation(ValidationError::BadBaseUrl { .. })
        ));
    }

    #[test]
    fn rejects_unknown_tool_type() {
        let yaml = r"
tools:
  t:
    type: smtp
    base_url: https://a.example.com
    auth: bearer
    secret: s
";
        assert!(matches!(parse_config(yaml), Err(ParseError::Yaml(_))));
    }

    #[test]
    fn empty_gavmo_config_is_valid_and_default() {
        let config = parse_config("{}\n").unwrap();
        assert!(config.tools.is_empty());
    }

    // --- Host-lock ---

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

    // --- Base URL with a path prefix ---

    const GATEWAY: &str = "https://api.atlassian.com/ex/jira/CLOUDID";

    #[test]
    fn lock_url_preserves_base_path_prefix_for_a_leading_slash_path() {
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
        // The real host here is evil.com; the prefix is userinfo.
        let err = lock_url(
            "https://trusted.example.com",
            "https://trusted.example.com@evil.com/steal",
        )
        .unwrap_err();
        assert!(matches!(err, HostLockError::HostMismatch { .. }));
    }

    /// Every spelling of "go up" must be caught once normalized.
    #[test]
    fn lock_url_rejects_escaping_the_base_path_prefix() {
        for path in [
            "../OTHER/rest",
            "/../OTHER/rest",
            "../../ex/jira/OTHER/rest",
            "rest/../../OTHER",
            "%2e%2e/OTHER",
            ".%2E/OTHER",
            "..\\OTHER",
            "/\\..\\OTHER",
            "https://api.atlassian.com/ex/jira/OTHER/rest",
            "https://api.atlassian.com/",
            "..",
        ] {
            assert!(
                matches!(
                    lock_url(GATEWAY, path),
                    Err(HostLockError::PathEscape { .. })
                ),
                "{path:?} must be rejected, got {:?}",
                lock_url(GATEWAY, path)
            );
        }
    }

    /// `/a/b` must not count as "under" `/a/bc/` (sibling-prefix confusion).
    #[test]
    fn lock_url_prefix_check_respects_segment_boundaries() {
        let err = lock_url(
            "https://api.atlassian.com/ex/jira/CLOUD",
            "https://api.atlassian.com/ex/jira/CLOUDX/rest",
        )
        .unwrap_err();
        assert!(matches!(err, HostLockError::PathEscape { .. }), "{err:?}");
    }

    #[test]
    fn lock_url_allows_dot_segments_that_stay_inside_the_prefix() {
        let u = lock_url(GATEWAY, "rest/x/../myself").unwrap();
        assert_eq!(
            u.as_str(),
            "https://api.atlassian.com/ex/jira/CLOUDID/rest/myself"
        );
    }

    #[test]
    fn lock_url_rejects_encoded_separators() {
        for path in ["..%2F..%2Fother", "a%2fb", "a%5Cb"] {
            assert!(
                matches!(
                    lock_url(GATEWAY, path),
                    Err(HostLockError::EncodedSeparator(_))
                ),
                "{path:?}"
            );
        }
    }

    #[test]
    fn lock_url_rejects_same_origin_userinfo() {
        let err = lock_url("https://h.example.com", "https://u:p@h.example.com/x").unwrap_err();
        assert_eq!(err, HostLockError::Userinfo);
    }

    #[test]
    fn lock_url_with_a_root_base_allows_any_path_on_the_origin() {
        assert!(lock_url("https://h.example.com", "/../../x").is_ok());
    }

    #[test]
    fn lock_url_allows_explicit_default_port_match() {
        let u = lock_url("https://h.example.com", "https://h.example.com:443/x").unwrap();
        assert_eq!(u.host_str(), Some("h.example.com"));
    }

    // --- URL-binding ---

    #[test]
    fn canonical_base_url_is_stable_and_includes_the_path_prefix() {
        assert_eq!(
            canonical_base_url("https://api.atlassian.com/ex/jira/CLOUDID").unwrap(),
            "https://api.atlassian.com:443/ex/jira/CLOUDID/"
        );
    }

    #[test]
    fn canonical_base_url_normalizes_cosmetic_differences_to_the_same_string() {
        let a = canonical_base_url("https://Dafni.Atlassian.net").unwrap();
        let b = canonical_base_url("https://dafni.atlassian.net/").unwrap();
        let c = canonical_base_url("https://dafni.atlassian.net:443").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn canonical_base_url_distinguishes_same_host_different_tenant_path() {
        // Origin-only binding would miss this.
        let good = canonical_base_url("https://api.atlassian.com/ex/jira/GOOD").unwrap();
        let evil = canonical_base_url("https://api.atlassian.com/ex/jira/EVIL").unwrap();
        assert_ne!(good, evil);
    }

    #[test]
    fn canonical_base_url_distinguishes_host_port_and_scheme() {
        let base = canonical_base_url("https://h.example.com").unwrap();
        assert_ne!(
            base,
            canonical_base_url("https://evil.example.com").unwrap()
        );
        assert_ne!(
            base,
            canonical_base_url("https://h.example.com:8443").unwrap()
        );
        assert_ne!(base, canonical_base_url("http://h.example.com").unwrap());
    }

    #[test]
    fn canonical_base_url_drops_query_and_fragment() {
        assert_eq!(
            canonical_base_url("https://h.example.com/base?x=1#frag").unwrap(),
            canonical_base_url("https://h.example.com/base").unwrap()
        );
    }

    #[test]
    fn canonical_base_url_rejects_a_non_url() {
        assert!(matches!(
            canonical_base_url("not a url"),
            Err(HostLockError::InvalidBaseUrl(_))
        ));
    }

    fn tool_yaml(id: &str, extra: &str) -> String {
        format!(
            "tools:\n  {id}:\n    type: http\n    base_url: https://h.example.com\n    auth: bearer\n    secret: s\n{extra}"
        )
    }

    #[test]
    fn tool_ids_must_be_mcp_names_and_not_reserved() {
        for good in ["jira", "dafni-jira", "a_b-9", &"x".repeat(64)] {
            assert!(parse_config(&tool_yaml(good, "")).is_ok(), "{good}");
        }
        for bad in [
            "\"has space\"",
            "\"ünicode\"",
            "\"a.b\"",
            &"x".repeat(65),
            "\"\"",
        ] {
            assert!(
                matches!(
                    parse_config(&tool_yaml(bad, "")),
                    Err(ParseError::Validation(ValidationError::BadToolId { .. }))
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            parse_config(&tool_yaml("gavmo_ping", "")),
            Err(ParseError::Validation(
                ValidationError::ReservedToolId { .. }
            ))
        ));
    }

    #[test]
    fn secret_names_are_restricted() {
        let yaml = "tools:\n  t:\n    type: http\n    base_url: https://h.example.com\n    auth: bearer\n    secret: \"a\\x1fb\"\n";
        assert!(matches!(
            parse_config(yaml),
            Err(ParseError::Validation(
                ValidationError::BadSecretName { .. }
            ))
        ));
    }

    #[test]
    fn header_names_are_validated_and_dangerous_ones_refused() {
        let with_header = |name: &str| {
            format!(
                "tools:\n  t:\n    type: http\n    base_url: https://h.example.com\n    auth: header\n    header_name: \"{name}\"\n    secret: s\n"
            )
        };
        assert!(parse_config(&with_header("X-API-Key")).is_ok());
        assert!(parse_config(&with_header("Authorization")).is_ok());
        for bad in [
            "Host",
            "content-length",
            "Transfer-Encoding",
            "Cookie",
            "Proxy-Foo",
            "X Bad",
            "X:Y",
        ] {
            assert!(
                matches!(
                    parse_config(&with_header(bad)),
                    Err(ParseError::Validation(
                        ValidationError::BadHeaderName { .. }
                    ))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn header_name_is_only_allowed_with_header_auth() {
        assert!(matches!(
            parse_config(&tool_yaml("t", "    header_name: X-Key\n")),
            Err(ParseError::Validation(
                ValidationError::UnexpectedHeaderName { .. }
            ))
        ));
    }

    #[test]
    fn base_url_must_not_carry_userinfo_query_or_fragment() {
        for url in [
            "https://u:p@h.example.com",
            "https://h.example.com/?x=1",
            "https://h.example.com/#f",
        ] {
            let yaml = format!(
                "tools:\n  t:\n    type: http\n    base_url: \"{url}\"\n    auth: bearer\n    secret: s\n"
            );
            assert!(
                matches!(
                    parse_config(&yaml),
                    Err(ParseError::Validation(ValidationError::BadBaseUrl { .. }))
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(matches!(
            parse_config(&tool_yaml("t", "    test_paht: /x\n")),
            Err(ParseError::Yaml(_))
        ));
        assert!(matches!(
            parse_config("tool:\n  t: {}\n"),
            Err(ParseError::Yaml(_))
        ));
    }

    #[test]
    fn tools_are_listed_in_a_stable_order() {
        let yaml = "tools:\n  zeta:\n    type: http\n    base_url: https://a.example.com\n    auth: bearer\n    secret: s\n  alpha:\n    type: http\n    base_url: https://b.example.com\n    auth: bearer\n    secret: s\n";
        let ids: Vec<_> = parse_config(yaml).unwrap().tools.into_keys().collect();
        assert_eq!(ids, ["alpha", "zeta"]);
    }

    #[test]
    fn auth_scheme_displays_as_its_yaml_value() {
        assert_eq!(AuthScheme::Header.to_string(), "header");
        assert_eq!(AuthScheme::Basic.as_str(), "basic");
    }
}
