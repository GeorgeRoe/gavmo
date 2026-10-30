//! Parsing and validation of the per-repo `gavmo.yaml` tool config, plus the
//! URL math that keeps credentials on their integration's origin: [`lock_url`]
//! (host-locking) and [`canonical_base_url`] (URL-binding).
//!
//! `gavmo.yaml` is untrusted: a repo can request a tool and name a secret, but
//! never supply a credential.

use std::collections::HashMap;

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

/// One integration: everything needed to forward a request except the secret.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    /// Requires an http(s) `base_url` with a host, and a `header_name` when
    /// `auth == Header`.
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

/// The per-repo `gavmo.yaml` (untrusted).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GavmoConfig {
    /// Tool requests, keyed by id.
    pub tools: HashMap<String, ToolDef>,
}

// --- Errors ---

/// Problems found after a YAML document parses structurally.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("tool {id:?} has an invalid base_url: {reason}")]
    BadBaseUrl { id: String, reason: String },
    #[error("tool {id:?} uses auth: header but has no header_name")]
    MissingHeaderName { id: String },
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
}

// --- Parsing ---

/// Parses and validates a `gavmo.yaml` document.
pub fn parse_config(yaml: &str) -> Result<GavmoConfig, ParseError> {
    let config: GavmoConfig = serde_yaml_ng::from_str(yaml)?;
    for (id, def) in &config.tools {
        def.validate(id)?;
    }
    Ok(config)
}

// --- Host-lock ---

/// Resolves an agent-supplied `path` under `base_url` and returns it only if it
/// stays on the same scheme, host and port; anything else is
/// [`HostLockError::HostMismatch`], so credentials never leave the origin.
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
}
