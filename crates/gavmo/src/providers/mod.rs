//! The model providers the host-side model gateway can serve, described as
//! data: each provider declares its settings as form fields (rendered by the
//! control panel without provider-specific UI), its models, and how to turn
//! its settings into Bifrost configuration.
//!
//! Adding a provider means implementing [`Provider`] and listing it in
//! [`ALL`]. Its settings are saved in `settings.json` as plain strings keyed
//! by field; [`FieldSpec::secret`] fields (API keys) go to the host's
//! secret vault instead, under [`secret_key`].
//!
//! Guests reach every model through the gateway's Anthropic-format route
//! (`/llm/anthropic/v1`), so a provider's models must be servable there by
//! Bifrost; one with another wire format would also need a route in
//! `daemon::llm` and a guest provider in the base image's opencode config.

pub(crate) mod vertex;

use std::collections::BTreeMap;
use std::path::Path;

use crate::bifrost::ProviderBlock;
use crate::types::ProviderCheck;

/// A provider's settings: field key -> value.
pub(crate) type Values = BTreeMap<String, String>;

/// Every provider GAVMO supports, in display order (which is also the
/// order models are offered in).
pub(crate) static ALL: &[&dyn Provider] = &[&vertex::Vertex];

/// The secret-store "workspace" provider credentials are filed under. Real
/// workspace ids are absolute paths, so it can't collide with one.
const SECRET_SCOPE: &str = "gavmo:providers";

/// One setting of a provider, shown as one form field.
#[derive(Debug)]
pub(crate) struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub placeholder: &'static str,
    /// A credential: kept in the secret vault and never sent to the browser.
    pub secret: bool,
    pub required: bool,
    /// Used when the field is left empty.
    pub default: Option<&'static str>,
    /// An environment variable that, when set (and not empty), overrides the
    /// saved value.
    pub env: Option<&'static str>,
}

/// A model a provider serves.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModelSpec {
    /// The name guests see (`gavmo/<id>`); provider-neutral, so it can stay
    /// the same if another provider serves the model.
    pub id: &'static str,
    /// The model as Bifrost routes it (`<bifrost provider>/<model>`).
    pub upstream: &'static str,
    pub name: &'static str,
    pub context: u32,
    pub output: u32,
}

/// Host facts providers' checks and configuration depend on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HostContext<'a> {
    /// The user's gcloud config dir (`GAVMO_GCLOUD_DIR`).
    pub gcloud_dir: &'a Path,
}

/// A model provider. See the module docs.
pub(crate) trait Provider: Send + Sync {
    /// Stable id: the key in `settings.json` and in the secret store.
    fn kind(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn fields(&self) -> &'static [FieldSpec];
    fn models(&self) -> &'static [ModelSpec];
    /// The ids (from [`Self::models`]) it suggests as the default and the
    /// small model.
    fn suggested_models(&self) -> (&'static str, &'static str);
    /// Checks resolved values (see [`resolve`]: trimmed, defaults and
    /// environment overrides applied, required fields present). The error is
    /// for the user.
    fn validate(&self, values: &Values) -> Result<(), String>;
    /// Its part of Bifrost's configuration, from valid `values` and its
    /// stored `secrets` (by field key). Fails, for the user, if a host-side
    /// prerequisite is missing.
    fn bifrost(
        &self,
        values: &Values,
        secrets: &Values,
        host: HostContext<'_>,
    ) -> Result<ProviderBlock, String>;
    /// Host-side prerequisites, for the settings page.
    fn checks(&self, host: HostContext<'_>) -> Vec<ProviderCheck>;
    /// Values to pre-fill for an unconfigured provider, read from local
    /// files only (never the network).
    fn suggestions(&self, host: HostContext<'_>) -> Values;
}

/// The provider called `kind`.
pub(crate) fn find(
    providers: &[&'static dyn Provider],
    kind: &str,
) -> Option<&'static dyn Provider> {
    providers.iter().copied().find(|p| p.kind() == kind)
}

/// The secret-store key of a provider's secret field.
pub(crate) fn secret_key(kind: &str, field: &str) -> String {
    secrets::secret_key(SECRET_SCOPE, &format!("{kind}/{field}"))
}

/// The values of the environment variables that override `providers`'
/// fields (variable -> value), read once through `var`. Empty values count
/// as unset.
pub(crate) fn env_overrides(
    providers: &[&'static dyn Provider],
    var: impl Fn(&str) -> Option<String>,
) -> Values {
    providers
        .iter()
        .flat_map(|p| p.fields())
        .filter_map(|f| f.env)
        .filter_map(|name| {
            var(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .map(|v| (name.to_owned(), v))
        })
        .collect()
}

/// A provider's settings in effect: saved values, overridden by the
/// environment, with defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// Non-secret fields only; a field with no value at all is absent.
    pub values: Values,
    /// Field key -> the environment variable overriding it.
    pub overridden: BTreeMap<&'static str, &'static str>,
    /// Saved, or set through the environment.
    pub configured: bool,
    /// Set through the environment only.
    pub env_only: bool,
}

/// `provider`'s settings in effect, from its `saved` values (if saved) and
/// the [`env_overrides`].
pub(crate) fn resolve(provider: &dyn Provider, saved: Option<&Values>, env: &Values) -> Resolved {
    let mut values = Values::new();
    let mut overridden = BTreeMap::new();
    for field in provider.fields().iter().filter(|f| !f.secret) {
        let from_env = field.env.and_then(|name| env.get(name).map(|v| (name, v)));
        let value = if let Some((name, value)) = from_env {
            overridden.insert(field.key, name);
            Some(value.clone())
        } else {
            saved
                .and_then(|s| s.get(field.key))
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .or_else(|| field.default.map(str::to_owned))
        };
        if let Some(value) = value {
            values.insert(field.key.to_owned(), value);
        }
    }
    Resolved {
        configured: saved.is_some() || !overridden.is_empty(),
        env_only: saved.is_none() && !overridden.is_empty(),
        values,
        overridden,
    }
}

/// Checks resolved values: required fields present, then the provider's own
/// validation.
pub(crate) fn check_values(provider: &dyn Provider, values: &Values) -> Result<(), String> {
    for field in provider.fields() {
        if field.required && !field.secret && !values.contains_key(field.key) {
            return Err(format!("{} is required", field.label));
        }
    }
    provider.validate(values)
}

/// The models guests are offered: those of the `configured` providers (in
/// [`ALL`] order, the first provider of a model id winning), or every
/// provider's if none is configured.
pub(crate) fn catalog(
    providers: &[&'static dyn Provider],
    configured: &[&str],
) -> Vec<&'static ModelSpec> {
    let mut models: Vec<&'static ModelSpec> = Vec::new();
    let any_configured = providers.iter().any(|p| configured.contains(&p.kind()));
    for provider in providers {
        if any_configured && !configured.contains(&provider.kind()) {
            continue;
        }
        for model in provider.models() {
            if !models.iter().any(|m| m.id == model.id) {
                models.push(model);
            }
        }
    }
    models
}

/// The default and small model ids guests start with: the saved choices if
/// still offered, else the first configured provider's suggestions, else the
/// first model offered (empty if none).
pub(crate) fn pick_models(
    providers: &[&'static dyn Provider],
    configured: &[&str],
    saved: (Option<&str>, Option<&str>),
) -> (String, String) {
    let models = catalog(providers, configured);
    let offered = |id: &str| models.iter().any(|m| m.id == id);
    let suggested = providers
        .iter()
        .find(|p| configured.contains(&p.kind()))
        .or_else(|| providers.first())
        .map(|p| p.suggested_models());
    let first = models.first().map_or("", |m| m.id);
    let pick = |saved: Option<&str>, suggestion: Option<&'static str>| {
        saved
            .filter(|id| offered(id))
            .or(suggestion.filter(|id| offered(id)))
            .unwrap_or(first)
            .to_owned()
    };
    (
        pick(saved.0, suggested.map(|s| s.0)),
        pick(saved.1, suggested.map(|s| s.1)),
    )
}

/// The opencode config each guest loads at boot (through `OPENCODE_CONFIG`)
/// on top of the base image's managed config, which pins the `gavmo`
/// provider's endpoint and credential: the models it offers, and the
/// default ones. opencode merges the two; a repo's own `opencode.json` can
/// still choose another default.
pub(crate) fn opencode_config(
    models: &[&ModelSpec],
    default_model: &str,
    small_model: &str,
) -> serde_json::Value {
    let models: serde_json::Map<String, serde_json::Value> = models
        .iter()
        .map(|m| {
            (
                m.id.to_owned(),
                serde_json::json!({
                    "id": m.upstream,
                    "name": m.name,
                    "reasoning": true,
                    "tool_call": true,
                    "attachment": true,
                    "temperature": true,
                    // Drives opencode's auto-compaction threshold.
                    "limit": { "context": m.context, "output": m.output },
                }),
            )
        })
        .collect();
    let mut config = serde_json::json!({
        // Present, so opencode doesn't try to add it to this read-only file.
        "$schema": "https://opencode.ai/config.json",
        "provider": { "gavmo": { "models": models } },
    });
    if let Some(fields) = config.as_object_mut() {
        for (key, id) in [("model", default_model), ("small_model", small_model)] {
            if !id.is_empty() {
                fields.insert(key.to_owned(), format!("gavmo/{id}").into());
            }
        }
    }
    config
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A provider with an API key, standing in for the direct providers
    /// GAVMO doesn't have yet, to exercise secret fields.
    pub(crate) struct KeyProvider;

    pub(crate) static KEY_MODELS: &[ModelSpec] = &[
        ModelSpec {
            id: "claude-sonnet-5-5",
            upstream: "anthropic/claude-sonnet-5-5",
            name: "Claude Sonnet 5.5 (direct)",
            context: 200_000,
            output: 64_000,
        },
        ModelSpec {
            id: "key-model",
            upstream: "anthropic/key-model",
            name: "Key model",
            context: 100_000,
            output: 8_000,
        },
    ];

    impl Provider for KeyProvider {
        fn kind(&self) -> &'static str {
            "keyed"
        }
        fn name(&self) -> &'static str {
            "Keyed"
        }
        fn description(&self) -> &'static str {
            "A test provider with an API key."
        }
        fn fields(&self) -> &'static [FieldSpec] {
            &[
                FieldSpec {
                    key: "api_key",
                    label: "API key",
                    help: "",
                    placeholder: "sk-...",
                    secret: true,
                    required: true,
                    default: None,
                    env: None,
                },
                FieldSpec {
                    key: "base_url",
                    label: "Base URL",
                    help: "",
                    placeholder: "",
                    secret: false,
                    required: false,
                    default: None,
                    env: Some("GAVMO_TEST_KEYED_URL"),
                },
            ]
        }
        fn models(&self) -> &'static [ModelSpec] {
            KEY_MODELS
        }
        fn suggested_models(&self) -> (&'static str, &'static str) {
            ("key-model", "key-model")
        }
        fn validate(&self, values: &Values) -> Result<(), String> {
            match values.get("base_url") {
                Some(url) if !url.starts_with("https://") => {
                    Err("Base URL must start with https://".to_owned())
                }
                _ => Ok(()),
            }
        }
        fn bifrost(
            &self,
            _values: &Values,
            secrets: &Values,
            _host: HostContext<'_>,
        ) -> Result<ProviderBlock, String> {
            let key = secrets.get("api_key").ok_or("no API key stored")?;
            Ok(ProviderBlock {
                key: "anthropic".to_owned(),
                config: serde_json::json!({ "keys": [{ "value": key }] }),
                env: Vec::new(),
            })
        }
        fn checks(&self, _host: HostContext<'_>) -> Vec<ProviderCheck> {
            Vec::new()
        }
        fn suggestions(&self, _host: HostContext<'_>) -> Values {
            Values::new()
        }
    }

    pub(crate) static WITH_KEYED: &[&dyn Provider] = &[&vertex::Vertex, &KeyProvider];

    fn values(pairs: &[(&str, &str)]) -> Values {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn providers_are_found_by_kind() {
        assert_eq!(find(WITH_KEYED, "keyed").map(Provider::name), Some("Keyed"));
        assert!(find(ALL, "keyed").is_none());
        assert!(find(ALL, "vertex").is_some());
    }

    #[test]
    fn secret_keys_are_scoped_away_from_workspaces() {
        assert_eq!(
            secret_key("keyed", "api_key"),
            secrets::secret_key("gavmo:providers", "keyed/api_key")
        );
    }

    #[test]
    fn only_declared_non_empty_variables_override() {
        let env = env_overrides(WITH_KEYED, |name| match name {
            "GAVMO_GCP_PROJECT" => Some(" my-project-1 ".to_owned()),
            "GAVMO_GCP_REGION" => Some(String::new()),
            "UNRELATED" => Some("x".to_owned()),
            _ => None,
        });
        assert_eq!(env, values(&[("GAVMO_GCP_PROJECT", "my-project-1")]));
    }

    #[test]
    fn resolution_layers_saved_values_env_and_defaults() {
        let vertex = &vertex::Vertex;
        let none = Values::new();

        let unconfigured = resolve(vertex, None, &none);
        assert!(!unconfigured.configured && !unconfigured.env_only);
        assert_eq!(unconfigured.values, values(&[("region", "global")]));

        let saved = values(&[("project", " saved-project "), ("region", "")]);
        let r = resolve(vertex, Some(&saved), &none);
        assert!(r.configured && !r.env_only);
        assert!(r.overridden.is_empty());
        assert_eq!(
            r.values,
            values(&[("project", "saved-project"), ("region", "global")])
        );

        let env = values(&[("GAVMO_GCP_PROJECT", "env-project")]);
        let r = resolve(vertex, Some(&saved), &env);
        assert!(r.configured && !r.env_only);
        assert_eq!(r.values["project"], "env-project");
        assert_eq!(r.overridden.get("project"), Some(&"GAVMO_GCP_PROJECT"));

        let r = resolve(vertex, None, &env);
        assert!(r.configured && r.env_only);

        // Secret fields are never part of the resolved values.
        let keyed = resolve(&KeyProvider, Some(&values(&[("api_key", "leak")])), &none);
        assert!(keyed.values.is_empty());
    }

    #[test]
    fn required_fields_are_checked_before_the_provider_validates() {
        let vertex = &vertex::Vertex;
        assert_eq!(
            check_values(vertex, &values(&[("region", "global")])),
            Err("Google Cloud project is required".to_owned())
        );
        assert!(
            check_values(vertex, &values(&[("project", "BAD"), ("region", "global")])).is_err()
        );
        assert!(
            check_values(
                vertex,
                &values(&[("project", "my-project-1"), ("region", "global")])
            )
            .is_ok()
        );
        // A required secret isn't a resolved value; it's checked on save.
        assert!(check_values(&KeyProvider, &Values::new()).is_ok());
        assert!(check_values(&KeyProvider, &values(&[("base_url", "http://x")])).is_err());
    }

    #[test]
    fn the_catalog_offers_configured_providers_models_once() {
        let ids = |models: Vec<&ModelSpec>| models.iter().map(|m| m.id).collect::<Vec<_>>();
        let vertex_ids = ["claude-sonnet-5-5", "claude-haiku-5-5", "claude-opus-5-5"];
        assert_eq!(ids(catalog(WITH_KEYED, &["vertex"])), vertex_ids);
        assert_eq!(
            ids(catalog(WITH_KEYED, &["keyed"])),
            ["claude-sonnet-5-5", "key-model"]
        );
        // Both: Vertex comes first and keeps the shared id.
        let both = catalog(WITH_KEYED, &["keyed", "vertex"]);
        assert_eq!(
            ids(both.clone()),
            [
                "claude-sonnet-5-5",
                "claude-haiku-5-5",
                "claude-opus-5-5",
                "key-model"
            ]
        );
        assert_eq!(both[0].upstream, "vertex/claude-sonnet-5-5@default");
        // None configured (or only unknown kinds): everything.
        assert_eq!(ids(catalog(WITH_KEYED, &[])).len(), 4);
        assert_eq!(ids(catalog(WITH_KEYED, &["gone"])).len(), 4);
        assert!(catalog(&[], &[]).is_empty());
    }

    #[test]
    fn default_models_prefer_saved_then_suggested_then_first() {
        let pick = |configured: &[&str], saved| pick_models(WITH_KEYED, configured, saved);
        assert_eq!(
            pick(&["vertex"], (None, None)),
            (
                "claude-sonnet-5-5".to_owned(),
                "claude-haiku-5-5".to_owned()
            )
        );
        assert_eq!(
            pick(
                &["vertex"],
                (Some("claude-opus-5-5"), Some("claude-opus-5-5"))
            ),
            ("claude-opus-5-5".to_owned(), "claude-opus-5-5".to_owned())
        );
        // A saved model no longer offered falls back.
        assert_eq!(
            pick(&["vertex"], (Some("key-model"), None)).0,
            "claude-sonnet-5-5"
        );
        assert_eq!(
            pick(&["keyed"], (None, None)),
            ("key-model".to_owned(), "key-model".to_owned())
        );
        // Unconfigured: the first provider's suggestions.
        assert_eq!(pick(&[], (None, None)).0, "claude-sonnet-5-5");
        assert_eq!(
            pick_models(&[], &[], (Some("x"), None)),
            (String::new(), String::new())
        );
    }

    /// A suggestion that isn't offered falls back to the first model.
    #[test]
    fn an_unoffered_suggestion_falls_back_to_the_first_model() {
        struct Odd;
        impl Provider for Odd {
            fn kind(&self) -> &'static str {
                "odd"
            }
            fn name(&self) -> &'static str {
                "Odd"
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn fields(&self) -> &'static [FieldSpec] {
                &[]
            }
            fn models(&self) -> &'static [ModelSpec] {
                KEY_MODELS
            }
            fn suggested_models(&self) -> (&'static str, &'static str) {
                ("missing", "missing")
            }
            fn validate(&self, _: &Values) -> Result<(), String> {
                Ok(())
            }
            fn bifrost(
                &self,
                _: &Values,
                _: &Values,
                _: HostContext<'_>,
            ) -> Result<ProviderBlock, String> {
                Err("no".to_owned())
            }
            fn checks(&self, _: HostContext<'_>) -> Vec<ProviderCheck> {
                Vec::new()
            }
            fn suggestions(&self, _: HostContext<'_>) -> Values {
                Values::new()
            }
        }
        static ODD: &[&dyn Provider] = &[&Odd];
        let odd = &Odd;
        assert_eq!(odd.name(), "Odd");
        assert!(odd.description().is_empty());
        assert!(odd.validate(&Values::new()).is_ok());
        assert!(
            odd.checks(HostContext {
                gcloud_dir: Path::new("/")
            })
            .is_empty()
        );
        assert!(
            odd.suggestions(HostContext {
                gcloud_dir: Path::new("/")
            })
            .is_empty()
        );
        assert!(
            odd.bifrost(
                &Values::new(),
                &Values::new(),
                HostContext {
                    gcloud_dir: Path::new("/")
                }
            )
            .is_err()
        );
        assert_eq!(
            pick_models(ODD, &["odd"], (None, None)),
            (
                "claude-sonnet-5-5".to_owned(),
                "claude-sonnet-5-5".to_owned()
            )
        );
    }

    #[test]
    fn the_guest_config_lists_models_and_defaults() {
        let models = catalog(ALL, &["vertex"]);
        let config = opencode_config(&models, "claude-sonnet-5-5", "claude-haiku-5-5");
        assert_eq!(config["$schema"], "https://opencode.ai/config.json");
        assert_eq!(config["model"], "gavmo/claude-sonnet-5-5");
        assert_eq!(config["small_model"], "gavmo/claude-haiku-5-5");
        let sonnet = &config["provider"]["gavmo"]["models"]["claude-sonnet-5-5"];
        assert_eq!(sonnet["id"], "vertex/claude-sonnet-5-5@default");
        assert_eq!(sonnet["name"], "Claude Sonnet 5.5");
        assert_eq!(sonnet["tool_call"], true);
        assert_eq!(sonnet["limit"]["context"], 200_000);
        assert_eq!(sonnet["limit"]["output"], 64_000);
        // Only what the guest may change: no endpoint or credential.
        assert!(config["provider"]["gavmo"].get("options").is_none());
        assert!(config.get("enabled_providers").is_none());

        let empty = opencode_config(&[], "", "");
        assert!(empty.get("model").is_none() && empty.get("small_model").is_none());
        assert_eq!(empty["provider"]["gavmo"]["models"], serde_json::json!({}));
    }

    #[test]
    fn the_test_provider_reports_its_secret_and_url() {
        let host = HostContext {
            gcloud_dir: Path::new("/nonexistent"),
        };
        let p = &KeyProvider;
        assert!(!p.description().is_empty());
        assert!(p.checks(host).is_empty() && p.suggestions(host).is_empty());
        assert!(p.bifrost(&Values::new(), &Values::new(), host).is_err());
        let block = p
            .bifrost(&Values::new(), &values(&[("api_key", "k")]), host)
            .unwrap();
        assert_eq!(block.key, "anthropic");
        assert_eq!(block.config["keys"][0]["value"], "k");
    }
}
