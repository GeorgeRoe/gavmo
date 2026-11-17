//! The `gavmo.yaml` schema, as types that are valid by construction: every
//! check runs while deserializing, so a value that exists has passed them,
//! and errors carry their position in the file.

use std::collections::BTreeMap;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

use crate::{BaseUrl, HeaderName, SecretName, ToolId};

/// Most tools one `gavmo.yaml` may request.
const MAX_TOOLS: usize = 64;

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

/// Checks on more than one value at once. Only ever seen as a message.
#[derive(Debug, thiserror::Error)]
enum Invalid {
    #[error("auth: header needs a header_name")]
    MissingHeaderName,
    #[error("header_name is only allowed with auth: header")]
    UnexpectedHeaderName,
    #[error("too many tools ({0}); at most {MAX_TOOLS} are allowed")]
    TooManyTools(usize),
}

// --- Deserializing ---

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
    use crate::header_name::header_name;
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
        assert_eq!(docs.auth, Auth::Header(header_name("X-API-Key")));
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
            .map(ToolId::into_inner)
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

    /// Each value's own checks are tested next to its type; here, that they
    /// run while parsing and report where the value is.
    #[test]
    fn invalid_values_are_reported_where_they_are() {
        assert_eq!(
            error(&tool("gavmo_ping", BEARER)),
            "tool id \"gavmo_ping\" is not allowed: ids starting with gavmo_ are reserved for GAVMO's own tools at line 2, column 3"
        );
        assert_eq!(
            error(&tool("t", "    auth: bearer\n    secret: \"a b\"\n")),
            "secret name \"a b\" contains ' ' (allowed: A-Z, a-z, 0-9, _, - and .) at line 6, column 13"
        );
        assert_eq!(
            error(&tool(
                "t",
                "    auth: header\n    header_name: Host\n    secret: s\n"
            )),
            "header_name \"Host\" is not allowed: it controls the request itself and can't carry a credential at line 6, column 18"
        );
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
    fn auth_scheme_is_the_yaml_value() {
        assert_eq!(Auth::Basic.scheme(), "basic");
        assert_eq!(Auth::Bearer.scheme(), "bearer");
        assert_eq!(Auth::Header(header_name("X-Key")).scheme(), "header");
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
