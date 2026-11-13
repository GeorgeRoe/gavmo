//! The model providers the host-side model gateway can serve, described as
//! data: each provider declares its settings as form fields (rendered by the
//! control panel without provider-specific UI), where its models are listed,
//! and how to turn its settings into Bifrost configuration.
//!
//! Adding a provider means implementing [`Provider`] and listing it in
//! [`ALL`]. Its settings are saved in `settings.json` as plain strings keyed
//! by field; [`FieldSpec::secret`] fields (API keys) go to the host's
//! secret vault instead, under [`secret_key`].
//!
//! A provider's models come from its section of the models.dev catalog
//! ([`models_dev`]), the list opencode itself ships with, so guests are
//! offered what opencode would show for that provider; its built-in
//! [`Provider::fallback_models`] stand in until the catalog has been
//! fetched once.
//!
//! Guests reach every model through the gateway's Anthropic-format route
//! (`/llm/anthropic/v1`), so a provider's models must be servable there by
//! Bifrost; one with another wire format would also need a route in
//! `daemon::llm` and a guest provider in the base image's opencode config.

pub(crate) mod models_dev;
pub(crate) mod vertex;

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelSpec {
    /// The name guests see (`gavmo/<id>`): the catalog's id without a
    /// provider's `@version` suffix, so it can stay the same if another
    /// provider serves the model.
    pub id: String,
    /// The model as Bifrost routes it (`<bifrost provider>/<model>`).
    pub upstream: String,
    pub name: String,
    /// models.dev's family (`claude-sonnet`), which the suggested defaults
    /// are picked by.
    pub family: Option<String>,
    /// `YYYY-MM-DD`; models are offered newest first.
    pub release_date: Option<String>,
    /// The rest of opencode's entry for the model, passed through to guests:
    /// capabilities, limits (which drive auto-compaction), cost... See
    /// [`models_dev::PASSED_THROUGH`].
    pub details: Map<String, Value>,
}

impl ModelSpec {
    /// opencode's config entry for the model; its `id` is what the gateway
    /// routes.
    fn opencode_entry(&self) -> Value {
        let mut entry = self.details.clone();
        entry.insert("id".to_owned(), self.upstream.clone().into());
        entry.insert("name".to_owned(), self.name.clone().into());
        if let Some(family) = &self.family {
            entry.insert("family".to_owned(), family.clone().into());
        }
        if let Some(date) = &self.release_date {
            entry.insert("release_date".to_owned(), date.clone().into());
        }
        Value::Object(entry)
    }
}

/// Sorts `models` newest first (undated last), then by id.
pub(crate) fn sort_newest_first(models: &mut [ModelSpec]) {
    models.sort_by(|a, b| {
        let undated = |m: &ModelSpec| m.release_date.is_none();
        undated(a)
            .cmp(&undated(b))
            .then_with(|| b.release_date.cmp(&a.release_date))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// A provider with the models it currently offers.
pub(crate) type ProviderModels = (&'static dyn Provider, Vec<ModelSpec>);

/// `providers` with their built-in models.
pub(crate) fn built_in(providers: &[&'static dyn Provider]) -> Vec<ProviderModels> {
    providers
        .iter()
        .map(|&p| {
            let mut models = p.fallback_models();
            sort_newest_first(&mut models);
            (p, models)
        })
        .collect()
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
    /// Its provider id in the models.dev catalog, whose models it offers.
    fn models_dev_id(&self) -> &'static str;
    /// How Bifrost routes the catalog's model `id` (`<bifrost
    /// provider>/<model>`).
    fn upstream(&self, id: &str) -> String;
    /// The models it offers until the catalog has been fetched (or if its
    /// section there is empty).
    fn fallback_models(&self) -> Vec<ModelSpec>;
    /// The model families it suggests the default and the small model from
    /// (the newest of each).
    fn suggested_families(&self) -> (&'static str, &'static str);
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

/// The models guests are offered, from `providers`' current models: those of
/// the `configured` providers (in [`ALL`] order, the first provider of a
/// model id winning), or every provider's if none is configured.
pub(crate) fn catalog(providers: &[ProviderModels], configured: &[&str]) -> Vec<ModelSpec> {
    let mut models: Vec<ModelSpec> = Vec::new();
    let any_configured = providers
        .iter()
        .any(|(p, _)| configured.contains(&p.kind()));
    for (provider, offered) in providers {
        if any_configured && !configured.contains(&provider.kind()) {
            continue;
        }
        for model in offered {
            if !models.iter().any(|m| m.id == model.id) {
                models.push(model.clone());
            }
        }
    }
    models
}

/// The default and small model ids guests start with: the saved choices if
/// still offered, else the newest offered models of the first configured
/// provider's suggested families, else the first model offered (empty if
/// none).
pub(crate) fn pick_models(
    providers: &[ProviderModels],
    configured: &[&str],
    saved: (Option<&str>, Option<&str>),
) -> (String, String) {
    let models = catalog(providers, configured);
    let offered = |id: &str| models.iter().any(|m| m.id == id);
    let suggesting = providers
        .iter()
        .find(|(p, _)| configured.contains(&p.kind()))
        .or_else(|| providers.first());
    let newest_of = |family: &str| -> Option<&str> {
        let (_, own) = suggesting?;
        own.iter()
            .filter(|m| m.family.as_deref() == Some(family) && offered(&m.id))
            // The first of the newest, as they're listed newest first.
            .rev()
            .max_by(|a, b| a.release_date.cmp(&b.release_date))
            .map(|m| m.id.as_str())
    };
    let families = suggesting.map(|(p, _)| p.suggested_families());
    let first = models.first().map_or("", |m| m.id.as_str());
    let pick = |saved: Option<&str>, family: Option<&str>| {
        saved
            .filter(|id| offered(id))
            .or_else(|| family.and_then(newest_of))
            .unwrap_or(first)
            .to_owned()
    };
    (
        pick(saved.0, families.map(|f| f.0)),
        pick(saved.1, families.map(|f| f.1)),
    )
}

/// The opencode config each guest loads at boot (through `OPENCODE_CONFIG`)
/// on top of the base image's managed config, which pins the `gavmo`
/// provider's endpoint and credential: the models it offers, and the
/// default ones. opencode merges the two; a repo's own `opencode.json` can
/// still choose another default.
pub(crate) fn opencode_config(
    models: &[ModelSpec],
    default_model: &str,
    small_model: &str,
) -> Value {
    let models: Map<String, Value> = models
        .iter()
        .map(|m| (m.id.clone(), m.opencode_entry()))
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

    /// [`KeyProvider`]'s models: one shared with Vertex, one of its own.
    pub(crate) fn key_models(provider: &dyn Provider) -> Vec<ModelSpec> {
        models_dev::models(
            provider,
            &serde_json::json!({ "models": {
                "key-model": {
                    "name": "Key model",
                    "family": "key",
                    "release_date": "2026-01-01",
                    "limit": { "context": 100_000, "output": 8_000 },
                },
                "claude-sonnet-5-5": {
                    "name": "Claude Sonnet 5.5 (direct)",
                    "family": "claude-sonnet",
                    "release_date": "2026-09-28",
                },
            }}),
        )
    }

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
        fn models_dev_id(&self) -> &'static str {
            "anthropic"
        }
        fn upstream(&self, id: &str) -> String {
            format!("anthropic/{id}")
        }
        fn fallback_models(&self) -> Vec<ModelSpec> {
            key_models(self)
        }
        fn suggested_families(&self) -> (&'static str, &'static str) {
            ("key", "key")
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

    fn ids(models: &[ModelSpec]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn the_catalog_offers_configured_providers_models_once() {
        let providers = built_in(WITH_KEYED);
        let vertex_ids = ["claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"];
        assert_eq!(ids(&catalog(&providers, &["vertex"])), vertex_ids);
        assert_eq!(
            ids(&catalog(&providers, &["keyed"])),
            ["claude-sonnet-5-5", "key-model"]
        );
        // Both: Vertex comes first and keeps the shared id.
        let both = catalog(&providers, &["keyed", "vertex"]);
        assert_eq!(
            ids(&both),
            [
                "claude-haiku-5-5",
                "claude-sonnet-5-5",
                "claude-opus-5-5",
                "key-model"
            ]
        );
        assert_eq!(both[1].upstream, "vertex/claude-sonnet-5-5@default");
        // None configured (or only unknown kinds): everything.
        assert_eq!(catalog(&providers, &[]).len(), 4);
        assert_eq!(catalog(&providers, &["gone"]).len(), 4);
        assert!(catalog(&[], &[]).is_empty());
    }

    #[test]
    fn models_sort_newest_first_then_by_id() {
        let model = |id: &str, date: Option<&str>| ModelSpec {
            id: id.to_owned(),
            upstream: String::new(),
            name: String::new(),
            family: None,
            release_date: date.map(str::to_owned),
            details: Map::new(),
        };
        let mut models = vec![
            model("undated", None),
            model("old", Some("2024-01-01")),
            model("b", Some("2026-01-01")),
            model("a", Some("2026-01-01")),
        ];
        sort_newest_first(&mut models);
        assert_eq!(ids(&models), ["a", "b", "old", "undated"]);
    }

    #[test]
    fn suggestions_are_the_newest_of_their_family() {
        let api: Value = serde_json::from_str(models_dev::tests::VERTEX_FIXTURE).unwrap();
        let section = &api["google-vertex-anthropic"];
        let providers = vec![(ALL[0], models_dev::models(ALL[0], section))];
        assert_eq!(
            pick_models(&providers, &["vertex"], (None, None)),
            (
                "claude-sonnet-5-5".to_owned(),
                "claude-haiku-5-5".to_owned()
            )
        );
        // Older ones stay pickable.
        assert_eq!(
            pick_models(&providers, &["vertex"], (Some("claude-opus-4-6"), None)).0,
            "claude-opus-4-6"
        );
    }

    #[test]
    fn default_models_prefer_saved_then_suggested_then_first() {
        let providers = built_in(WITH_KEYED);
        let pick = |configured: &[&str], saved| pick_models(&providers, configured, saved);
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
            fn models_dev_id(&self) -> &'static str {
                "odd"
            }
            fn upstream(&self, id: &str) -> String {
                format!("odd/{id}")
            }
            fn fallback_models(&self) -> Vec<ModelSpec> {
                key_models(self)
            }
            fn suggested_families(&self) -> (&'static str, &'static str) {
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
        assert_eq!(odd.models_dev_id(), "odd");
        assert_eq!(
            pick_models(&built_in(ODD), &["odd"], (None, None)),
            (
                "claude-sonnet-5-5".to_owned(),
                "claude-sonnet-5-5".to_owned()
            )
        );
    }

    #[test]
    fn the_guest_config_lists_models_and_defaults() {
        let models = catalog(&built_in(ALL), &["vertex"]);
        let config = opencode_config(&models, "claude-sonnet-5-5", "claude-haiku-5-5");
        assert_eq!(config["$schema"], "https://opencode.ai/config.json");
        assert_eq!(config["model"], "gavmo/claude-sonnet-5-5");
        assert_eq!(config["small_model"], "gavmo/claude-haiku-5-5");
        let sonnet = &config["provider"]["gavmo"]["models"]["claude-sonnet-5-5"];
        assert_eq!(sonnet["id"], "vertex/claude-sonnet-5-5@default");
        assert_eq!(sonnet["name"], "Claude Sonnet 5.5");
        assert_eq!(sonnet["family"], "claude-sonnet");
        assert_eq!(sonnet["release_date"], "2026-09-28");
        assert_eq!(sonnet["tool_call"], true);
        assert_eq!(sonnet["temperature"], false);
        assert_eq!(sonnet["limit"]["context"], 1_000_000);
        assert_eq!(sonnet["limit"]["output"], 128_000);
        assert_eq!(sonnet["cost"]["input"], 2.0);
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
        assert_eq!(p.models_dev_id(), "anthropic");
        assert!(p.checks(host).is_empty() && p.suggestions(host).is_empty());
        assert!(p.bifrost(&Values::new(), &Values::new(), host).is_err());
        let block = p
            .bifrost(&Values::new(), &values(&[("api_key", "k")]), host)
            .unwrap();
        assert_eq!(block.key, "anthropic");
        assert_eq!(block.config["keys"][0]["value"], "k");
    }
}
