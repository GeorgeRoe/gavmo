//! [`parse_config`]: reading an untrusted `gavmo.yaml` with a strict parser.

use crate::GavmoConfig;

/// A `gavmo.yaml` that isn't valid YAML, doesn't match the schema, fails a
/// check, or goes over a parser budget. The message is one line, with the
/// position in the file, ready to show to the user.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(String);

/// Parses and validates a `gavmo.yaml` document.
pub fn parse_config(yaml: &str) -> Result<GavmoConfig, ParseError> {
    serde_saphyr::from_str_with_options(yaml, yaml_options())
        .map_err(|e| ParseError(user_facing(&e.to_string())))
}

/// Parser settings for untrusted input. Anything a tool list doesn't need is
/// an error, and every limit is sized to the schema (at most 64 tools, three
/// levels deep) rather than to YAML in general, so a hostile file is refused
/// early instead of being parsed in full.
///
/// The scalar-byte cap is what stops alias amplification: one anchored value
/// reused thousands of times can't expand beyond 512 KiB in total, about
/// twice the largest file the daemon reads.
fn yaml_options() -> serde_saphyr::Options {
    let mut budget = serde_saphyr::Budget::default();
    budget.max_documents = 1;
    budget.max_depth = 8;
    budget.max_nodes = 10_000;
    budget.max_events = 50_000;
    budget.max_anchors = 1_000;
    budget.max_aliases = 1_000;
    budget.max_merge_keys = 0;
    budget.max_total_scalar_bytes = 512 * 1024;
    budget.max_recorded_anchor_bytes = 512 * 1024;

    let mut options = serde_saphyr::Options::default();
    options.budget = Some(budget);
    // A second `jira:` would otherwise silently replace the first.
    options.duplicate_keys = serde_saphyr::DuplicateKeyPolicy::Error;
    // Merging would make a tool's fields come from elsewhere in the file.
    options.merge_keys = serde_saphyr::MergeKeyPolicy::Error;
    options.reject_unsupported_tags = true;
    // One-line messages: they are shown in the control panel and logged.
    options.with_snippet = false;
    options
}

/// Drops the parser's advice about its own settings, which means nothing to
/// someone editing `gavmo.yaml`.
fn user_facing(message: &str) -> String {
    message.replace(", set DuplicateKeyPolicy in Options if acceptable", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_yaml(id: &str) -> String {
        format!(
            "tools:\n  {id}:\n    type: http\n    base_url: https://h.example.com\n    auth: bearer\n    secret: s\n"
        )
    }

    fn error(yaml: &str) -> String {
        parse_config(yaml).unwrap_err().to_string()
    }

    #[test]
    fn a_duplicate_tool_id_is_an_error_not_last_wins() {
        let one = tool_yaml("t");
        let yaml = format!("{one}{}", one.strip_prefix("tools:\n").unwrap());
        let err = error(&yaml);
        assert!(err.contains("duplicate mapping key: t"), "{err}");
        // No advice about parser settings in a user-facing message.
        assert!(!err.contains("DuplicateKeyPolicy"), "{err}");
    }

    #[test]
    fn merge_keys_are_refused() {
        let yaml = "tools:\n  a: &a {type: http, base_url: \"https://h.example.com\", auth: bearer, secret: s}\n  b:\n    <<: *a\n";
        assert!(error(yaml).contains("merge keys"));
    }

    #[test]
    fn unknown_tags_are_refused_but_standard_ones_are_fine() {
        let tagged = |tag: &str| tool_yaml("t").replace("secret: s", &format!("secret: {tag} s"));
        assert!(error(&tagged("!custom")).contains("unsupported tag"));
        assert_eq!(
            parse_config(&tagged("!!str")).unwrap().tools["t"]
                .secret
                .as_str(),
            "s"
        );
    }

    #[test]
    fn only_one_document_is_read() {
        let one = tool_yaml("t");
        assert!(error(&format!("{one}---\n{one}")).contains("Documents"));
    }

    #[test]
    fn anchors_may_share_a_value_between_tools() {
        let yaml = "tools:\n  a: {type: http, base_url: &u \"https://h.example.com\", auth: bearer, secret: s}\n  b: {type: http, base_url: *u, auth: bearer, secret: s}\n";
        let config = parse_config(yaml).unwrap();
        assert_eq!(config.tools["a"].base_url, config.tools["b"].base_url);
    }

    /// One anchored tool with a long description, reused thousands of times:
    /// small on disk, gigabytes once expanded. Refused on the scalar budget
    /// long before the tool-count check could see it.
    #[test]
    fn alias_amplification_is_refused_by_the_budget() {
        let mut yaml = format!(
            "tools:\n  t0: &tool {{type: http, base_url: \"https://h.example.com\", auth: bearer, secret: s, description: \"{}\"}}\n",
            "x".repeat(50 * 1024)
        );
        yaml.extend((1..=13_000).map(|i| format!("  t{i}: *tool\n")));
        assert!(yaml.len() < 256 * 1024, "within the daemon's file size cap");
        let err = error(&yaml);
        assert!(err.contains("budget breached: ScalarBytes"), "{err}");
    }

    /// Parsing streams: hostile structure where a string belongs is refused
    /// at its first bracket, not parsed in full first.
    #[test]
    fn nesting_where_a_string_belongs_is_refused_at_once() {
        let yaml = format!(
            "tools:\n  t:\n    description: {}{}\n",
            "[".repeat(10_000),
            "]".repeat(10_000)
        );
        let err = error(&yaml);
        assert!(err.contains("line 3, column 18"), "{err}");
    }
}
