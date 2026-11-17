//! [`ToolId`]: the key a tool is listed under in `gavmo.yaml`.

use validated::{CharSet, Chars, Label, Len, Named, NotPrefixed, Strings, Validated};

/// A tool id. It becomes the MCP tool name, so it follows MCP's naming rule,
/// and ids starting with `gavmo_` are left for GAVMO's own tools.
pub type ToolId =
    Validated<String, Named<ToolIdLabel, (Len<1, 64>, Chars<IdChars>, NotPrefixed<ReservedIds>)>>;

pub struct ToolIdLabel;
impl Label for ToolIdLabel {
    const NAME: &'static str = "tool id";
}

pub struct IdChars;
impl CharSet for IdChars {
    const ALLOWED: &'static str = "A-Z, a-z, 0-9, _ and -";
    fn contains(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
    }
}

/// So a repo can't shadow or duplicate GAVMO's tools (e.g. `gavmo_ping`).
pub struct ReservedIds;
impl Strings for ReservedIds {
    const LIST: &'static [&'static str] = &["gavmo_"];
    const REASON: &'static str = "ids starting with gavmo_ are reserved for GAVMO's own tools";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(id: &str) -> String {
        ToolId::new(id.to_owned()).unwrap_err().to_string()
    }

    #[test]
    fn accepts_mcp_names() {
        for id in ["jira", "dafni-jira", "a_b-9", "Gavmo", &"x".repeat(64)] {
            assert!(ToolId::new(id.to_owned()).is_ok(), "{id}");
        }
    }

    #[test]
    fn refuses_other_characters() {
        assert_eq!(
            error("has space"),
            "tool id \"has space\" contains ' ' (allowed: A-Z, a-z, 0-9, _ and -)"
        );
        for id in ["ünicode", "a.b", "a/b"] {
            assert!(error(id).contains("contains"), "{id}");
        }
    }

    #[test]
    fn refuses_empty_and_long_ids() {
        assert_eq!(error(""), "tool id \"\" must not be empty");
        assert!(error(&"x".repeat(65)).ends_with("is longer than 64 characters"));
    }

    #[test]
    fn refuses_the_reserved_prefix_in_any_case() {
        for id in ["gavmo_ping", "GAVMO_x", "Gavmo_"] {
            assert_eq!(
                error(id),
                format!(
                    "tool id {id:?} is not allowed: ids starting with gavmo_ are reserved for GAVMO's own tools"
                )
            );
        }
    }
}
