//! The `gavmo.yaml` schema, as types that are valid by construction: every
//! check runs while deserializing, so a value that exists has passed them,
//! and errors carry their position in the file.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

use crate::BaseUrl;

/// Most tools one `gavmo.yaml` may request.
const MAX_TOOLS: usize = 64;
/// Tool ids become MCP tool names, which are at most 64 characters.
const MAX_TOOL_ID_LEN: usize = 64;
/// Reserved for GAVMO's own tools (e.g. `gavmo_ping`), so a repo can't
/// shadow or duplicate them.
const RESERVED_TOOL_PREFIX: &str = "gavmo_";
const MAX_SECRET_NAME_LEN: usize = 128;
const MAX_HEADER_NAME_LEN: usize = 128;

/// Headers that can't carry a credential: they control HTTP framing,
/// routing or connection handling, or carry other credentials, so setting
/// them could smuggle or retarget requests or mix credentials. Anything
/// starting with `proxy-` is refused as well.
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
];

/// The per-repo `gavmo.yaml`. Untrusted: a repo can request a tool and name
/// the secret it needs, but never supply a credential.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GavmoConfig {
    /// Sorted by id, so listings are stable.
    pub tools: BTreeMap<ToolId, ToolDef>,
}

/// One requested tool: everything needed to forward a request except the
/// credential itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub kind: ToolKind,
    /// Every forwarded request is host-locked to this URL's origin and path.
    pub base_url: BaseUrl,
    pub auth: Auth,
    /// The secret the user is asked for (a name, not a value).
    pub secret: SecretName,
    /// Shown in the control panel and the MCP tool description.
    pub description: Option<String>,
    /// Setup help shown to the user.
    pub instructions: Option<String>,
    /// Path, relative to `base_url`, that the control panel's "Test" button
    /// requests to check the credential. The caller decides the default.
    pub test_path: Option<String>,
}

/// The kind of tool. Unknown `type`s are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// Generic HTTP forwarder: the host host-locks the agent's path to
    /// `base_url`, attaches the credential and forwards the request.
    Http,
}

/// How the host attaches the credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// HTTP Basic. The credential is a JSON `{username, password}`.
    Basic,
    /// `Authorization: Bearer <token>`. The credential is the bare token.
    Bearer,
    /// The token in a custom header.
    Header(HeaderName),
}

impl Auth {
    /// The `auth:` value as written in `gavmo.yaml`.
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Bearer => "bearer",
            Self::Header(_) => "header",
        }
    }
}

/// A tool id: 1-64 of `[A-Za-z0-9_-]`, not starting with `gavmo_`. Becomes
/// the MCP tool name, so it follows MCP's naming rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolId(String);

/// A secret's name: 1-128 of `[A-Za-z0-9_.-]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretName(String);

/// An HTTP header name (an RFC 9110 token) that may carry a credential.
/// Compares case-insensitively, as HTTP does; displays as written.
#[derive(Debug, Clone)]
pub struct HeaderName(String);

/// Why a value in `gavmo.yaml` was rejected. Only ever seen as a message.
#[derive(Debug, thiserror::Error)]
enum Invalid {
    #[error("tool id {0:?} must be 1-64 characters of A-Z, a-z, 0-9, _ and -")]
    ToolId(String),
    #[error("tool id {0:?} is reserved (ids starting with gavmo_ are GAVMO's own)")]
    ReservedToolId(String),
    #[error("secret name {0:?} must be 1-128 characters of A-Z, a-z, 0-9, _, - and .")]
    SecretName(String),
    #[error("header_name {0:?} is not a valid HTTP header name")]
    HeaderName(String),
    #[error("header_name {0:?} controls the request itself and can't carry a credential")]
    ForbiddenHeader(String),
    #[error("auth: header needs a header_name")]
    MissingHeaderName,
    #[error("header_name is only allowed with auth: header")]
    UnexpectedHeaderName,
    #[error("too many tools ({0}); at most {MAX_TOOLS} are allowed")]
    TooManyTools(usize),
}

/// `true` if `s` is 1..=`max` bytes, every char accepted by `ok`.
fn is_name(s: &str, max: usize, ok: impl Fn(char) -> bool) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(ok)
}

impl ToolId {
    fn new(id: String) -> Result<Self, Invalid> {
        if !is_name(&id, MAX_TOOL_ID_LEN, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
        }) {
            return Err(Invalid::ToolId(id));
        }
        if id.starts_with(RESERVED_TOOL_PREFIX) {
            return Err(Invalid::ReservedToolId(id));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SecretName {
    fn new(name: String) -> Result<Self, Invalid> {
        if is_name(&name, MAX_SECRET_NAME_LEN, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
        }) {
            Ok(Self(name))
        } else {
            Err(Invalid::SecretName(name))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl HeaderName {
    fn new(name: String) -> Result<Self, Invalid> {
        let is_token_char = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
        if !is_name(&name, MAX_HEADER_NAME_LEN, is_token_char) {
            return Err(Invalid::HeaderName(name));
        }
        let lower = name.to_ascii_lowercase();
        if FORBIDDEN_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
            return Err(Invalid::ForbiddenHeader(name));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PartialEq for HeaderName {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for HeaderName {}

/// Lets `tools.get("jira")` look up by plain string.
impl Borrow<str> for ToolId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

macro_rules! display_as_str {
    ($($ty:ty),*) => {$(
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    )*};
}
display_as_str!(ToolId, SecretName, HeaderName);

// --- Deserializing ---

/// Deserializes a string, then checks it with `new`.
macro_rules! deserialize_checked {
    ($($ty:ty),*) => {$(
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    )*};
}
deserialize_checked!(ToolId, SecretName, HeaderName);

/// `gavmo.yaml` as written. Unknown keys are rejected, so a typo is an error
/// rather than silently ignored.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    tools: BTreeMap<ToolId, ToolDef>,
}

impl<'de> Deserialize<'de> for GavmoConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let RawConfig { tools } = RawConfig::deserialize(deserializer)?;
        if tools.len() > MAX_TOOLS {
            return Err(D::Error::custom(Invalid::TooManyTools(tools.len())));
        }
        Ok(Self { tools })
    }
}

/// A tool as written: `auth` and `header_name` are separate keys.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolDef {
    #[serde(rename = "type")]
    kind: ToolKind,
    base_url: BaseUrl,
    auth: RawAuth,
    secret: SecretName,
    header_name: Option<HeaderName>,
    description: Option<String>,
    instructions: Option<String>,
    test_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawAuth {
    Basic,
    Bearer,
    Header,
}

impl<'de> Deserialize<'de> for ToolDef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawToolDef::deserialize(deserializer)?;
        let auth = match (raw.auth, raw.header_name) {
            (RawAuth::Basic, None) => Auth::Basic,
            (RawAuth::Bearer, None) => Auth::Bearer,
            (RawAuth::Header, Some(name)) => Auth::Header(name),
            (RawAuth::Header, None) => return Err(D::Error::custom(Invalid::MissingHeaderName)),
            (RawAuth::Basic | RawAuth::Bearer, Some(_)) => {
                return Err(D::Error::custom(Invalid::UnexpectedHeaderName));
            }
        };
        Ok(Self {
            kind: raw.kind,
            base_url: raw.base_url,
            auth,
            secret: raw.secret,
            description: raw.description,
            instructions: raw.instructions,
            test_path: raw.test_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::parse_config;

    /// A valid one-tool config; `fields` replaces the default `auth` and
    /// `secret` lines.
    fn tool(id: &str, fields: &str) -> String {
        format!("tools:\n  {id}:\n    type: http\n    base_url: https://h.example.com\n{fields}")
    }

    const BEARER: &str = "    auth: bearer\n    secret: s\n";

    fn error(yaml: &str) -> String {
        parse_config(yaml).unwrap_err().to_string()
    }

    fn header_auth(name: &str) -> String {
        tool(
            "t",
            &format!("    auth: header\n    header_name: \"{name}\"\n    secret: s\n"),
        )
    }

    #[test]
    fn parses_a_full_config() {
        let yaml = r#"
tools:
  dafni-jira:
    type: http
    base_url: https://dafni.atlassian.net
    auth: basic
    secret: jira-cred
    description: "DAFNI Jira (read via your PAT)"
    instructions: "Create an API token at id.atlassian.net."
    test_path: /rest/api/3/myself
  docs:
    type: http
    base_url: https://docs.example.com
    auth: header
    header_name: X-API-Key
    secret: docs-token
"#;
        let config = parse_config(yaml).unwrap();
        let jira = &config.tools["dafni-jira"];
        assert_eq!(jira.kind, ToolKind::Http);
        assert_eq!(jira.base_url.as_str(), "https://dafni.atlassian.net");
        assert_eq!(jira.auth, Auth::Basic);
        assert_eq!(jira.secret.as_str(), "jira-cred");
        assert_eq!(
            jira.description.as_deref(),
            Some("DAFNI Jira (read via your PAT)")
        );
        assert!(jira.instructions.is_some());
        assert_eq!(jira.test_path.as_deref(), Some("/rest/api/3/myself"));

        let docs = &config.tools["docs"];
        let Auth::Header(name) = &docs.auth else {
            panic!("expected header auth, got {:?}", docs.auth);
        };
        assert_eq!(name.as_str(), "X-API-Key");
        assert_eq!(docs.description, None);
    }

    #[test]
    fn an_empty_config_has_no_tools() {
        for yaml in ["{}\n", "", "tools: {}\n"] {
            assert_eq!(
                parse_config(yaml).unwrap(),
                GavmoConfig::default(),
                "{yaml:?}"
            );
        }
    }

    #[test]
    fn tools_are_listed_in_a_stable_order() {
        let yaml = format!(
            "{}{}",
            tool("zeta", BEARER),
            tool("alpha", BEARER).replace("tools:\n", "")
        );
        let ids: Vec<_> = parse_config(&yaml)
            .unwrap()
            .tools
            .into_keys()
            .map(|id| id.to_string())
            .collect();
        assert_eq!(ids, ["alpha", "zeta"]);
    }

    #[test]
    fn at_most_64_tools() {
        let mut yaml = String::from("tools:\n");
        for i in 0..MAX_TOOLS {
            writeln!(yaml, "  t{i}: {{type: http, base_url: \"https://h.example.com\", auth: bearer, secret: s}}").unwrap();
        }
        assert_eq!(parse_config(&yaml).unwrap().tools.len(), MAX_TOOLS);
        yaml.push_str("  one-more: {type: http, base_url: \"https://h.example.com\", auth: bearer, secret: s}\n");
        assert_eq!(error(&yaml), "too many tools (65); at most 64 are allowed");
    }

    #[test]
    fn tool_ids_must_be_mcp_names() {
        for good in ["jira", "dafni-jira", "a_b-9", &"x".repeat(64)] {
            assert!(parse_config(&tool(good, BEARER)).is_ok(), "{good}");
        }
        for bad in [
            "\"has space\"",
            "\"ünicode\"",
            "\"a.b\"",
            &"x".repeat(65),
            "\"\"",
        ] {
            let err = error(&tool(bad, BEARER));
            assert!(err.contains("must be 1-64 characters"), "{bad}: {err}");
        }
    }

    #[test]
    fn gavmo_tool_ids_are_reserved() {
        assert_eq!(
            error(&tool("gavmo_ping", BEARER)),
            "tool id \"gavmo_ping\" is reserved (ids starting with gavmo_ are GAVMO's own) at line 2, column 3"
        );
    }

    #[test]
    fn secret_names_are_restricted() {
        for bad in ["\"a\\x1fb\"", "\"a b\"", "\"\"", &"x".repeat(129)] {
            let err = error(&tool(
                "t",
                &format!("    auth: bearer\n    secret: {bad}\n"),
            ));
            assert!(
                err.contains("secret name") && err.contains("line 6"),
                "{bad}: {err}"
            );
        }
        let ok = tool("t", "    auth: bearer\n    secret: my.secret_name-2\n");
        assert!(parse_config(&ok).is_ok());
    }

    #[test]
    fn header_names_are_validated() {
        for good in ["X-API-Key", "Authorization", "x!#$%&'*+.^_`|~"] {
            assert!(parse_config(&header_auth(good)).is_ok(), "{good}");
        }
        for bad in ["X Bad", "X:Y", "Ünicode", "", &"x".repeat(129)] {
            let err = error(&header_auth(bad));
            assert!(
                err.contains("is not a valid HTTP header name"),
                "{bad:?}: {err}"
            );
        }
    }

    #[test]
    fn headers_that_control_the_request_are_refused() {
        for bad in [
            "Host",
            "content-length",
            "Transfer-Encoding",
            "Cookie",
            "Proxy-Authorization",
            "Proxy-Foo",
        ] {
            let err = error(&header_auth(bad));
            assert!(err.contains("can't carry a credential"), "{bad}: {err}");
        }
    }

    #[test]
    fn header_name_goes_with_auth_header_only() {
        assert_eq!(
            error(&tool("t", "    auth: header\n    secret: s\n")),
            "auth: header needs a header_name at line 3, column 5"
        );
        for auth in ["basic", "bearer"] {
            let yaml = tool(
                "t",
                &format!("    auth: {auth}\n    header_name: X-Key\n    secret: s\n"),
            );
            assert_eq!(
                error(&yaml),
                "header_name is only allowed with auth: header at line 3, column 5"
            );
        }
    }

    #[test]
    fn header_names_compare_case_insensitively_but_display_as_written() {
        let name = |s: &str| HeaderName::new(s.to_owned()).unwrap();
        assert_eq!(name("X-API-Key"), name("x-api-key"));
        assert_ne!(name("X-API-Key"), name("X-Other"));
        assert_eq!(name("X-API-Key").to_string(), "X-API-Key");
    }

    #[test]
    fn auth_scheme_is_the_yaml_value() {
        let header = Auth::Header(HeaderName::new("X-Key".to_owned()).unwrap());
        assert_eq!(Auth::Basic.scheme(), "basic");
        assert_eq!(Auth::Bearer.scheme(), "bearer");
        assert_eq!(header.scheme(), "header");
    }

    #[test]
    fn bad_base_urls_are_reported_where_they_are() {
        for (url, want) in [
            ("ftp://a.example.com", "must be an http(s) URL with a host"),
            (
                "https://u:p@h.example.com",
                "must not contain user:password@",
            ),
            (
                "https://h.example.com/?x=1",
                "must not have a query or fragment",
            ),
        ] {
            let yaml = format!("tools:\n  t:\n    type: http\n    base_url: \"{url}\"\n{BEARER}");
            let err = error(&yaml);
            assert!(
                err.contains(want) && err.contains("line 4, column 15"),
                "{url}: {err}"
            );
        }
        let yaml =
            "tools:\n  t:\n    type: http\n    base_url: [a]\n    auth: bearer\n    secret: s\n";
        assert!(error(yaml).contains("line 4"));
    }

    #[test]
    fn unknown_keys_types_and_schemes_are_refused() {
        for (yaml, want) in [
            (
                tool("t", &format!("{BEARER}    test_paht: /x\n")),
                "unknown field `test_paht`",
            ),
            ("tool:\n  t: {}\n".to_owned(), "unknown field `tool`"),
            (
                tool("t", BEARER).replace("type: http", "type: smtp"),
                "unknown variant `smtp`",
            ),
            (
                tool("t", "    auth: digest\n    secret: s\n"),
                "unknown variant `digest`",
            ),
        ] {
            let err = error(&yaml);
            assert!(err.contains(want), "{err}");
        }
    }

    #[test]
    fn required_fields_are_required() {
        for field in ["type", "base_url", "auth", "secret"] {
            let without_field = tool("t", BEARER)
                .lines()
                .filter(|line| !line.trim_start().starts_with(&format!("{field}:")))
                .collect::<Vec<_>>()
                .join("\n");
            let err = error(&without_field);
            assert!(
                err.contains(&format!("missing field `{field}`")),
                "{field}: {err}"
            );
        }
    }
}
