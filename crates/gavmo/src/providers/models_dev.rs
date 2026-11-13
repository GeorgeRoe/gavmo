//! The models.dev catalog (<https://models.dev>): for each provider, the
//! models it serves with their capabilities, limits and cost. opencode
//! ships with it as its model list, so offering a provider's section here
//! gives guests what opencode would show for that provider, routed through
//! the gateway instead.
//!
//! It describes models, not what a given account has enabled: a model can
//! be listed and still be refused upstream (e.g. not enabled in a Vertex
//! project's Model Garden).

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};

use super::{ModelSpec, Provider, sort_newest_first};

/// The whole catalog.
pub(crate) const DEFAULT_URL: &str = "https://models.dev/api.json";

/// The fields of a model's entry opencode's config accepts (it drops
/// others, e.g. `knowledge`), passed through to guests. `name`, `family`
/// and `release_date` are passed too, from [`ModelSpec`]'s own fields.
pub(crate) const PASSED_THROUGH: &[&str] = &[
    "attachment",
    "reasoning",
    "temperature",
    "tool_call",
    "modalities",
    "limit",
    "cost",
    "status",
];

/// Models opencode hides (checked on 1.18.35: it drops deprecated ones and
/// shows alphas and betas).
const HIDDEN_STATUSES: &[&str] = &["deprecated"];

/// The sections of the providers with the models.dev `ids` in the whole
/// catalog (`api.json`), by id; one the catalog lacks is left out. Fails if
/// it isn't the catalog at all.
pub(crate) fn sections(api: &Value, ids: &[&str]) -> Result<BTreeMap<String, Value>, String> {
    let providers = api
        .as_object()
        .filter(|p| {
            p.values()
                .any(|s| s.get("models").is_some_and(Value::is_object))
        })
        .ok_or("it isn't a models.dev catalog (no provider has models)")?;
    Ok(ids
        .iter()
        .filter_map(|&id| {
            let section = providers.get(id)?;
            let models = section.get("models")?.as_object()?;
            // Only what is used: the models.
            Some((id.to_owned(), serde_json::json!({ "models": models })))
        })
        .collect())
}

/// `provider`'s models in its models.dev `section`, as opencode shows them
/// (without deprecated ones), newest first. Entries without a
/// usable id are skipped.
pub(crate) fn models(provider: &dyn Provider, section: &Value) -> Vec<ModelSpec> {
    let Some(entries) = section.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };
    let shown: Vec<(&str, &Map<String, Value>)> = entries
        .iter()
        .filter(|(id, _)| usable_id(id))
        .filter_map(|(id, entry)| Some((id.as_str(), entry.as_object()?)))
        .filter(|(_, entry)| {
            let status = entry.get("status").and_then(Value::as_str);
            !status.is_some_and(|s| HIDDEN_STATUSES.contains(&s))
        })
        .collect();
    // Guests see ids without the `@version`, unless that would merge two.
    let mut unversioned: HashMap<&str, usize> = HashMap::new();
    for (id, _) in &shown {
        *unversioned.entry(without_version(id)).or_default() += 1;
    }
    let mut models: Vec<ModelSpec> = shown
        .into_iter()
        .map(|(id, entry)| {
            let short = without_version(id);
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            ModelSpec {
                id: if unversioned.get(short) == Some(&1) {
                    short
                } else {
                    id
                }
                .to_owned(),
                upstream: provider.upstream(id),
                name: text("name").unwrap_or_else(|| id.to_owned()),
                family: text("family"),
                release_date: text("release_date"),
                details: PASSED_THROUGH
                    .iter()
                    .filter_map(|&key| Some((key.to_owned(), entry.get(key)?.clone())))
                    .collect(),
            }
        })
        .collect();
    sort_newest_first(&mut models);
    models
}

/// `id` without a `@version` suffix (Vertex's `claude-sonnet-5-5@default`).
fn without_version(id: &str) -> &str {
    match id.split_once('@') {
        Some((base, _)) if !base.is_empty() => base,
        _ => id,
    }
}

/// An id that is safe in a guest's model name and in Bifrost's routing.
fn usable_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._:@/".contains(c))
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;
    use crate::providers::vertex::Vertex;

    /// models.dev's real Vertex (Anthropic) section as of November 2026, as
    /// a catalog of its own: `{"google-vertex-anthropic": <section>}`, copied
    /// (pretty-printed, otherwise unedited) from `https://models.dev/api.json`. To refresh it, copy that
    /// section in again; the tests asserting its exact model list (here and
    /// in `providers`, `registry::model_catalog` and `registry::models`)
    /// change with it.
    pub(crate) const VERTEX_FIXTURE: &str = include_str!("testdata/models-dev-vertex.json");

    fn ids(models: &[ModelSpec]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn vertex_offers_what_opencode_shows_newest_first() {
        let api: Value = serde_json::from_str(VERTEX_FIXTURE).unwrap();
        let sections = sections(&api, &["google-vertex-anthropic", "missing"]).unwrap();
        assert_eq!(
            sections.keys().collect::<Vec<_>>(),
            ["google-vertex-anthropic"]
        );
        let models = models(&Vertex, &sections["google-vertex-anthropic"]);
        // Deprecated ones (Sonnet 4 and 4.5, Opus 4 and 4.1) are left out.
        assert_eq!(
            ids(&models),
            [
                "claude-fable-5-2",
                "claude-haiku-5-5",
                "claude-sonnet-5-5",
                "claude-opus-5-5",
                "claude-fable-5-1",
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-fable-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-sonnet-4-6",
                "claude-opus-4-6",
                "claude-opus-4-5",
                "claude-haiku-4-5",
            ]
        );
        let haiku = models.last().unwrap();
        assert_eq!(haiku.upstream, "vertex/claude-haiku-4-5@20251001");
        assert_eq!(haiku.name, "Claude Haiku 4.5");
        assert_eq!(haiku.family.as_deref(), Some("claude-haiku"));
        assert_eq!(haiku.release_date.as_deref(), Some("2025-10-15"));
        assert_eq!(
            haiku.details,
            json!({
                "attachment": true,
                "reasoning": true,
                "temperature": true,
                "tool_call": true,
                "modalities": { "input": ["text", "image", "pdf"], "output": ["text"] },
                "limit": { "context": 200_000, "output": 64_000 },
                "cost": { "input": 1, "output": 5, "cache_read": 0.1, "cache_write": 1.25 },
            })
            .as_object()
            .unwrap()
            .clone()
        );
    }

    #[test]
    fn deprecated_and_unusable_entries_are_left_out() {
        let section = json!({ "models": {
            "old@1": { "name": "Old", "status": "deprecated" },
            "early@1": { "name": "Early", "status": "alpha" },
            "trial@1": { "name": "Trial", "status": "beta", "release_date": "2026-01-02" },
            "bad id": { "name": "Bad" },
            "not-an-object": 3,
            "plain": {},
        }});
        let models = models(&Vertex, &section);
        // Like opencode, alphas and betas are offered; undated ones go last.
        assert_eq!(ids(&models), ["trial", "early", "plain"]);
        assert_eq!(models[0].details["status"], "beta");
        assert_eq!(models[1].details["status"], "alpha");
        // No name: the catalog id; nothing to pass through: no details.
        assert_eq!(models[2].name, "plain");
        assert!(models[2].details.is_empty());
        assert!(super::models(&Vertex, &json!({})).is_empty());
    }

    #[test]
    fn versions_are_kept_when_dropping_them_would_merge_models() {
        let section = json!({ "models": {
            "m@2024": { "release_date": "2024-01-01" },
            "m@2025": { "release_date": "2025-01-01" },
            "n@x": {},
            "@odd": {},
        }});
        assert_eq!(
            ids(&models(&Vertex, &section)),
            ["m@2025", "m@2024", "@odd", "n"]
        );
    }

    #[test]
    fn something_else_is_not_a_catalog() {
        for bad in [
            json!([]),
            json!({}),
            json!({ "x": { "name": "no models" } }),
        ] {
            assert!(sections(&bad, &["x"]).is_err(), "{bad}");
        }
        let other = json!({ "other": { "models": {} } });
        assert!(sections(&other, &["x"]).unwrap().is_empty());
    }
}
