//! Model provider settings (`settings.json`), the model gateway they
//! configure, and the model list each guest boots with.
//!
//! A provider's settings come from three layers (see
//! [`providers::resolve`]): what was saved in the control panel, overridden
//! field by field by environment variables (`GAVMO_GCP_PROJECT`, ...), with
//! defaults for the rest. Secret fields live in the secret vault, not the
//! settings file.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::AppState;
use super::error::{RegistryError, Result, io_ctx};
use super::store::{load_json_or_set_aside, save_json};
use crate::bifrost::{GatewayPlan, ModelGateway};
use crate::providers::{self, HostContext, Provider, Values};
use crate::types::{GatewayStatus, ModelChoice, ModelSettingsView, ProviderField, ProviderView};

/// How long the settings page's gateway test waits for a reply.
const TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// How long the gateway test waits for a starting Bifrost to become ready.
const READY_WAIT: Duration = Duration::from_secs(30);

/// `settings.json`: global (not per-workspace) settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Settings {
    #[serde(default)]
    pub models: ModelSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ModelSettings {
    /// Saved providers: kind -> non-secret field values. Kinds this version
    /// doesn't know are kept but ignored.
    #[serde(default)]
    pub providers: BTreeMap<String, Values>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
}

/// Loads `settings.json`; a missing file is no settings, an unparsable one
/// is moved aside (see [`load_json_or_set_aside`]).
pub(super) fn load_settings(path: &Path) -> io::Result<Settings> {
    load_json_or_set_aside(
        path,
        "settings.json was unreadable; starting without saved model providers \
         (add them again under Settings)",
    )
}

/// Model configuration from the daemon's command line and environment.
pub struct ModelOptions {
    pub(crate) providers: &'static [&'static dyn Provider],
    /// The provider fields' environment overrides, read once at startup.
    pub(crate) env: Values,
    pub(crate) gateway: Arc<ModelGateway>,
    /// Where the models.dev catalog is fetched from (`file://` for a local
    /// copy).
    pub(crate) catalog_url: String,
}

impl ModelOptions {
    /// Every supported provider, with overrides from this process's
    /// environment, served by `gateway`. `GAVMO_MODELS_DEV_URL` replaces
    /// the models.dev catalog's URL (e.g. with a `file://` copy).
    pub fn from_env(gateway: ModelGateway) -> Self {
        Self {
            providers: providers::ALL,
            env: providers::env_overrides(providers::ALL, |name| std::env::var(name).ok()),
            gateway: Arc::new(gateway),
            catalog_url: std::env::var("GAVMO_MODELS_DEV_URL")
                .ok()
                .map(|url| url.trim().to_owned())
                .filter(|url| !url.is_empty())
                .unwrap_or_else(|| providers::models_dev::DEFAULT_URL.to_owned()),
        }
    }

    /// No environment overrides, no Bifrost binary, and no catalog (never
    /// the network).
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            providers: providers::ALL,
            env: Values::new(),
            catalog_url: "file:///nonexistent/models-dev.json".to_owned(),
            gateway: Arc::new(ModelGateway::new(
                crate::bifrost::GatewayOptions::disabled("GAVMO_BIFROST is not set"),
                std::path::PathBuf::from("/nonexistent/bifrost"),
            )),
        }
    }
}

/// The gateway's reason when no provider is configured; guests see it too.
const NOTHING_CONFIGURED: &str =
    "no model provider is configured; add one under Settings in the GAVMO control panel";

impl AppState {
    /// The model gateway `/llm` forwards to.
    pub fn model_gateway(&self) -> Arc<ModelGateway> {
        Arc::clone(&self.inner.gateway)
    }

    fn host(&self) -> HostContext<'_> {
        HostContext {
            gcloud_dir: &self.inner.gcloud_dir,
        }
    }

    /// The providers in effect (saved, or set through the environment),
    /// with their resolved settings.
    fn configured(&self, settings: &Settings) -> Vec<(&'static dyn Provider, providers::Resolved)> {
        self.inner
            .providers
            .iter()
            .map(|&p| {
                let saved = settings.models.providers.get(p.kind());
                (p, providers::resolve(p, saved, &self.inner.provider_env))
            })
            .filter(|(_, resolved)| resolved.configured)
            .collect()
    }

    /// Reads the stored secret fields of `provider` (by field key). Doesn't
    /// touch the store for a provider without secrets.
    async fn provider_secrets(&self, provider: &dyn Provider) -> Result<Values> {
        let fields: Vec<&str> = provider
            .fields()
            .iter()
            .filter(|f| f.secret)
            .map(|f| f.key)
            .collect();
        if fields.is_empty() {
            return Ok(Values::new());
        }
        let keys = fields
            .iter()
            .map(|f| providers::secret_key(provider.kind(), f))
            .collect();
        let stored = self.secrets_fetch_many(keys).await?;
        Ok(fields
            .into_iter()
            .filter_map(|f| {
                stored
                    .get(&providers::secret_key(provider.kind(), f))
                    .map(|v| (f.to_owned(), v.clone()))
            })
            .collect())
    }

    /// What the gateway should serve for `settings`: every usable configured
    /// provider. Unusable ones are left out (logged); with none usable, the
    /// reasons.
    async fn gateway_plan(&self, settings: &Settings) -> GatewayPlan {
        let configured = self.configured(settings);
        if configured.is_empty() {
            return Err(NOTHING_CONFIGURED.to_owned());
        }
        let mut blocks = Vec::new();
        let mut problems = Vec::new();
        for (provider, resolved) in configured {
            let block = match providers::check_values(provider, &resolved.values) {
                Err(e) => Err(e),
                Ok(()) => match self.provider_secrets(provider).await {
                    Err(e) => Err(e.to_string()),
                    Ok(secrets) => provider.bifrost(&resolved.values, &secrets, self.host()),
                },
            };
            match block {
                Ok(block) => blocks.push(block),
                Err(e) => {
                    tracing::warn!(provider = provider.kind(), "model provider unusable: {e}");
                    problems.push(format!("{}: {e}", provider.name()));
                }
            }
        }
        if blocks.is_empty() {
            Err(problems.join("; "))
        } else {
            Ok(blocks)
        }
    }

    /// Brings the model gateway in line with the current settings (and the
    /// host: e.g. credentials that appeared since). Cheap if nothing
    /// changed.
    pub async fn apply_model_settings(&self) {
        let _applying = self.inner.apply_lock.lock().await;
        let settings = self.inner.settings.lock().await.clone();
        let plan = self.gateway_plan(&settings).await;
        self.inner.gateway.apply(plan).await;
    }

    /// The models guests are offered and the default/small ids, for
    /// `settings`.
    fn model_choices(&self, settings: &Settings) -> (Vec<providers::ModelSpec>, String, String) {
        let configured = self.configured(settings);
        let kinds: Vec<&str> = configured.iter().map(|(p, _)| p.kind()).collect();
        let offered = self.inner.model_catalog.models(self.inner.providers);
        let models = providers::catalog(&offered, &kinds);
        let (default_model, small_model) = providers::pick_models(
            &offered,
            &kinds,
            (
                settings.models.default_model.as_deref(),
                settings.models.small_model.as_deref(),
            ),
        );
        (models, default_model, small_model)
    }

    /// The opencode config a booting guest loads (see
    /// [`providers::opencode_config`]).
    pub(super) async fn guest_opencode_config(&self) -> Vec<u8> {
        let settings = self.inner.settings.lock().await.clone();
        let (models, default_model, small_model) = self.model_choices(&settings);
        let config = providers::opencode_config(&models, &default_model, &small_model);
        // Serializing a `Value` can't fail; fall back to "no additions".
        serde_json::to_vec_pretty(&config).unwrap_or_else(|_| b"{}".to_vec())
    }

    pub fn model_gateway_status(&self) -> GatewayStatus {
        self.inner.gateway.status()
    }

    /// Fetches the model catalog from models.dev now, returning how many
    /// models guests are offered after it. Workspaces started afterwards
    /// get the new list.
    pub async fn refresh_model_catalog(&self) -> Result<String> {
        let ids: Vec<&str> = self
            .inner
            .providers
            .iter()
            .map(|p| p.models_dev_id())
            .collect();
        self.inner
            .model_catalog
            .refresh(&ids)
            .await
            .map_err(|why| {
                RegistryError::Failed(format!("Couldn't update the model list: {why}"))
            })?;
        let settings = self.inner.settings.lock().await.clone();
        let (models, _, _) = self.model_choices(&settings);
        Ok(format!("{} models offered", models.len()))
    }

    /// Keeps the model catalog fresh for the daemon's lifetime: fetches it
    /// now if the cached copy is stale (or missing), then daily, retrying
    /// hourly after a failure.
    pub fn spawn_model_catalog_refresh(&self) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(state.inner.model_catalog.due_in()).await;
                match state.refresh_model_catalog().await {
                    Ok(offered) => {
                        tracing::info!("model catalog updated from models.dev: {offered}");
                    }
                    Err(e) => tracing::warn!("{e}"),
                }
            }
        })
    }

    /// The settings page's view. Re-applies the settings first, so fixing a
    /// host-side prerequisite (signing in with gcloud) takes effect on the
    /// next look.
    pub async fn model_settings(&self) -> Result<ModelSettingsView> {
        self.apply_model_settings().await;
        let settings = self.inner.settings.lock().await.clone();
        let mut views = Vec::new();
        for &provider in self.inner.providers {
            views.push(self.provider_view(provider, &settings).await?);
        }
        let (models, default_model, small_model) = self.model_choices(&settings);
        Ok(ModelSettingsView {
            gateway: self.inner.gateway.status(),
            providers: views,
            models: models
                .into_iter()
                .map(|m| ModelChoice {
                    id: m.id,
                    name: m.name,
                })
                .collect(),
            catalog: self.inner.model_catalog.status(self.inner.providers),
            saved_default_model: settings.models.default_model.clone(),
            saved_small_model: settings.models.small_model.clone(),
            default_model,
            small_model,
        })
    }

    async fn provider_view(
        &self,
        provider: &'static dyn Provider,
        settings: &Settings,
    ) -> Result<ProviderView> {
        let saved = settings.models.providers.get(provider.kind());
        let resolved = providers::resolve(provider, saved, &self.inner.provider_env);
        let suggestions = if resolved.configured {
            Values::new()
        } else {
            provider.suggestions(self.host())
        };
        let secrets = self.provider_secrets(provider).await?;
        let fields: Vec<ProviderField> = provider
            .fields()
            .iter()
            .map(|f| ProviderField {
                key: f.key.to_owned(),
                label: f.label.to_owned(),
                help: f.help.to_owned(),
                placeholder: f.placeholder.to_owned(),
                secret: f.secret,
                required: f.required,
                value: if f.secret {
                    String::new()
                } else {
                    resolved
                        .values
                        .get(f.key)
                        .or_else(|| suggestions.get(f.key))
                        .cloned()
                        .unwrap_or_default()
                },
                is_set: f.secret && secrets.contains_key(f.key),
                overridden_by: resolved.overridden.get(f.key).map(|v| (*v).to_owned()),
            })
            .collect();
        let error = if resolved.configured {
            providers::check_values(provider, &resolved.values)
                .err()
                .or_else(|| {
                    fields
                        .iter()
                        .find(|f| f.secret && f.required && !f.is_set)
                        .map(|f| format!("{} is not set", f.label))
                })
        } else {
            None
        };
        Ok(ProviderView {
            kind: provider.kind().to_owned(),
            name: provider.name().to_owned(),
            description: provider.description().to_owned(),
            configured: resolved.configured,
            env_only: resolved.env_only,
            fields,
            checks: provider.checks(self.host()),
            error,
        })
    }

    fn find_provider(&self, kind: &str) -> Result<&'static dyn Provider> {
        providers::find(self.inner.providers, kind)
            .ok_or_else(|| RegistryError::InvalidInput(format!("unknown model provider {kind:?}")))
    }

    /// Saves a provider's settings from its form (field key -> value) and
    /// restarts the gateway with them. Empty fields mean the default; an
    /// empty secret keeps the stored one. Fields set through the environment
    /// can't be changed here.
    pub async fn save_provider(&self, kind: &str, values: Values) -> Result<()> {
        let provider = self.find_provider(kind)?;
        if let Some(unknown) = values
            .keys()
            .find(|k| !provider.fields().iter().any(|f| f.key == k.as_str()))
        {
            return Err(RegistryError::InvalidInput(format!(
                "{} has no setting {unknown:?}",
                provider.name()
            )));
        }
        let mut settings = self.inner.settings.lock().await;
        let previous = settings.models.providers.get(kind);
        let mut saved = Values::new();
        let mut new_secrets = Vec::new();
        for field in provider.fields() {
            let submitted = values.get(field.key).map(|v| v.trim());
            if field.secret {
                if let Some(value) = submitted.filter(|v| !v.is_empty()) {
                    new_secrets.push((field, value.to_owned()));
                }
                continue;
            }
            let env = field
                .env
                .and_then(|name| self.inner.provider_env.get(name).map(|v| (name, v)));
            if let Some((name, env_value)) = env {
                if submitted.is_some_and(|v| v != env_value) {
                    return Err(RegistryError::InvalidInput(format!(
                        "{} is set by the {name} environment variable; change or unset it there",
                        field.label
                    )));
                }
                // Kept for when the variable is unset.
                if let Some(value) = previous.and_then(|p| p.get(field.key)) {
                    saved.insert(field.key.to_owned(), value.clone());
                }
            } else if let Some(value) = submitted.filter(|v| !v.is_empty()) {
                saved.insert(field.key.to_owned(), value.to_owned());
            }
        }
        let resolved = providers::resolve(provider, Some(&saved), &self.inner.provider_env);
        providers::check_values(provider, &resolved.values).map_err(RegistryError::InvalidInput)?;
        let stored = self.provider_secrets(provider).await?;
        for field in provider.fields().iter().filter(|f| f.secret && f.required) {
            if !stored.contains_key(field.key)
                && !new_secrets.iter().any(|(f, _)| f.key == field.key)
            {
                return Err(RegistryError::InvalidInput(format!(
                    "{} is required",
                    field.label
                )));
            }
        }
        for (field, value) in new_secrets {
            self.secret_set(providers::secret_key(kind, field.key), value)
                .await?;
        }
        let mut updated = settings.clone();
        updated.models.providers.insert(kind.to_owned(), saved);
        self.commit_settings(&mut settings, updated).await?;
        drop(settings);
        self.apply_model_settings().await;
        Ok(())
    }

    /// Forgets a provider's saved settings and stored secrets, and restarts
    /// the gateway without it. Refused while environment variables configure
    /// it (it would stay configured).
    pub async fn remove_provider(&self, kind: &str) -> Result<()> {
        let provider = self.find_provider(kind)?;
        let resolved = providers::resolve(provider, None, &self.inner.provider_env);
        if !resolved.overridden.is_empty() {
            let vars: Vec<&str> = resolved.overridden.values().copied().collect();
            return Err(RegistryError::InvalidInput(format!(
                "{} is configured through the environment ({}); unset that to remove it",
                provider.name(),
                vars.join(", ")
            )));
        }
        let mut settings = self.inner.settings.lock().await;
        for field in provider.fields().iter().filter(|f| f.secret) {
            self.secret_delete(providers::secret_key(kind, field.key))
                .await?;
        }
        if settings.models.providers.contains_key(kind) {
            let mut updated = settings.clone();
            updated.models.providers.remove(kind);
            self.commit_settings(&mut settings, updated).await?;
        }
        drop(settings);
        self.apply_model_settings().await;
        Ok(())
    }

    /// Saves the default and small model choices (ids from the offered
    /// models; `None` or empty for the providers' suggestion). Applies to
    /// workspaces started afterwards.
    pub async fn set_default_models(
        &self,
        default_model: Option<String>,
        small_model: Option<String>,
    ) -> Result<()> {
        let mut settings = self.inner.settings.lock().await;
        let (models, _, _) = self.model_choices(&settings);
        let check = |choice: Option<String>| -> Result<Option<String>> {
            match choice
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty())
            {
                Some(id) if !models.iter().any(|m| m.id == id) => Err(RegistryError::InvalidInput(
                    format!("{id:?} is not an offered model"),
                )),
                choice => Ok(choice),
            }
        };
        let mut updated = settings.clone();
        updated.models.default_model = check(default_model)?;
        updated.models.small_model = check(small_model)?;
        self.commit_settings(&mut settings, updated).await
    }

    async fn commit_settings(&self, current: &mut Settings, updated: Settings) -> Result<()> {
        save_json(&self.inner.settings_path, &updated)
            .await
            .map_err(io_ctx("saving settings.json"))?;
        *current = updated;
        Ok(())
    }

    /// Sends one tiny request for the small model through the gateway (not
    /// through a guest), returning how it went. `Err` explains a failure.
    pub async fn test_model_gateway(&self) -> Result<String> {
        self.apply_model_settings().await;
        // Just after a save Bifrost is still starting; give it a moment.
        let deadline = tokio::time::Instant::now() + READY_WAIT;
        while self.inner.gateway.status() == GatewayStatus::Starting
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let base = self.inner.gateway.upstream().base_url().map_err(|why| {
            RegistryError::Failed(format!("The model gateway isn't running: {why}"))
        })?;
        let settings = self.inner.settings.lock().await.clone();
        let (models, _, small_model) = self.model_choices(&settings);
        let model = models
            .iter()
            .find(|m| m.id == small_model)
            .ok_or_else(|| RegistryError::Failed("no model to test with".to_owned()))?;

        let client = reqwest::Client::builder()
            .timeout(TEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| RegistryError::Failed(format!("cannot make an HTTP client: {e}")))?;
        let started = std::time::Instant::now();
        let response = client
            .post(format!("{base}/anthropic/v1/messages"))
            .header("anthropic-version", "2023-06-01")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(
                serde_json::json!({
                    "model": model.upstream,
                    "max_tokens": 16,
                    "messages": [{ "role": "user", "content": "Reply with the single word: ok" }],
                })
                .to_string(),
            )
            .send()
            .await
            .map_err(|e| {
                RegistryError::Failed(if e.is_connect() {
                    "The model gateway isn't answering yet (Bifrost may still be starting); \
                     try again in a moment"
                        .to_owned()
                } else {
                    format!("The request to the model gateway failed: {e}")
                })
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(format!(
                "{} answered in {:.1} s",
                model.name,
                started.elapsed().as_secs_f64()
            ))
        } else {
            Err(RegistryError::Failed(format!(
                "{} failed with HTTP {status}: {}",
                model.name,
                error_message(&body)
            )))
        }
    }
}

/// The message of an Anthropic-format error body, or the start of the body.
fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(500).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_extracted_or_truncated() {
        assert_eq!(
            error_message(r#"{"type":"error","error":{"type":"x","message":"quota"}}"#),
            "quota"
        );
        assert_eq!(error_message("plain"), "plain");
        assert_eq!(error_message(&"y".repeat(600)).len(), 500);
    }

    #[test]
    fn settings_files_round_trip_and_tolerate_missing_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        assert_eq!(load_settings(&path).unwrap(), Settings::default());

        std::fs::write(&path, "{}").unwrap();
        assert_eq!(load_settings(&path).unwrap(), Settings::default());
        std::fs::write(
            &path,
            r#"{"models":{"providers":{"vertex":{"project":"p"}},"future":1}}"#,
        )
        .unwrap();
        let loaded = load_settings(&path).unwrap();
        assert_eq!(loaded.models.providers["vertex"]["project"], "p");
        assert_eq!(loaded.models.default_model, None);

        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_settings(&path).unwrap(), Settings::default());
        assert!(!path.exists(), "the corrupt file is moved aside");
    }

    use crate::bifrost::GatewayOptions;
    use crate::bifrost::tests::fake_binary;
    use crate::providers::tests::WITH_KEYED;
    use crate::vm_handle::fake::{FakeLauncher, Script};
    use std::path::PathBuf;

    /// A test daemon's model side: its data dir, a gcloud dir with
    /// credentials, and a gateway whose "Bifrost" is a script that never
    /// gets ready (so its generated `config.json` shows what it was given).
    struct Harness {
        tmp: tempfile::TempDir,
        state: AppState,
    }

    impl Harness {
        fn new(providers: &'static [&'static dyn Provider], env: &[(&str, &str)]) -> Self {
            Self::with_catalog(providers, env, None)
        }

        /// [`Self::new`], with a models.dev catalog file holding `catalog`.
        fn with_catalog(
            providers: &'static [&'static dyn Provider],
            env: &[(&str, &str)],
            catalog: Option<&str>,
        ) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let catalog_url = match catalog {
                Some(contents) => {
                    let file = tmp.path().join("models-dev.json");
                    std::fs::write(&file, contents).unwrap();
                    format!("file://{}", file.display())
                }
                None => ModelOptions::for_tests().catalog_url,
            };
            let gcloud = tmp.path().join("gcloud");
            std::fs::create_dir(&gcloud).unwrap();
            std::fs::write(gcloud.join("application_default_credentials.json"), "{}").unwrap();
            let gateway = ModelGateway::new(
                GatewayOptions {
                    binary: Ok(fake_binary(tmp.path(), "exec sleep 600")),
                    data_dir: None,
                    port: None,
                },
                tmp.path().join("bifrost"),
            );
            let options = ModelOptions {
                providers,
                env: env
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                gateway: Arc::new(gateway),
                catalog_url,
            };
            let state = Self::open(tmp.path(), options);
            Self { tmp, state }
        }

        /// An `AppState` on `dir` (the data and gcloud dirs live there).
        fn open(dir: &Path, models: ModelOptions) -> AppState {
            AppState::with_data_dir(
                dir.join("base.qcow2"),
                &dir.join("gcloud"),
                mcp::DEFAULT_MCP_PORT,
                &dir.join("data"),
                Some(Path::new("/nonexistent/home")),
                &dir.join("run"),
                Arc::new(secrets::MemoryStore::new()),
                Arc::new(FakeLauncher(Script::new())),
                models,
            )
            .unwrap()
        }

        fn adc(&self) -> PathBuf {
            self.tmp
                .path()
                .join("gcloud/application_default_credentials.json")
        }

        /// The providers the running "Bifrost" was configured with.
        fn bifrost_providers(&self) -> Vec<String> {
            let path = self.tmp.path().join("bifrost/config.json");
            let config: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            config["providers"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        }

        fn saved(&self) -> serde_json::Value {
            let path = self.tmp.path().join("data/settings.json");
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
        }

        async fn view(&self) -> ModelSettingsView {
            self.state.model_settings().await.unwrap()
        }

        async fn provider(&self, kind: &str) -> ProviderView {
            self.view()
                .await
                .providers
                .into_iter()
                .find(|p| p.kind == kind)
                .unwrap()
        }

        async fn shutdown(self) {
            self.state.model_gateway().shutdown().await;
        }
    }

    fn values(pairs: &[(&str, &str)]) -> Values {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn invalid(result: Result<()>) -> String {
        match result {
            Err(RegistryError::InvalidInput(why)) => why,
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// Out of the box nothing is configured: the gateway says how to fix
    /// that, and guests are offered every model (so they get that reason).
    #[tokio::test]
    async fn with_nothing_configured_the_gateway_explains_and_every_model_is_offered() {
        let h = Harness::new(WITH_KEYED, &[]);
        let view = h.view().await;
        assert_eq!(
            view.gateway,
            GatewayStatus::NotConfigured {
                reason: NOTHING_CONFIGURED.to_owned()
            }
        );
        assert!(
            view.providers
                .iter()
                .all(|p| !p.configured && p.error.is_none())
        );
        let ids: Vec<&str> = view.models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"claude-opus-5-5") && ids.contains(&"key-model"));
        assert_eq!(
            (view.default_model.as_str(), view.small_model.as_str()),
            ("claude-sonnet-5-5", "claude-haiku-5-5")
        );
        let vertex = &view.providers[0];
        assert_eq!(vertex.fields[1].value, "global", "the region's default");
        assert!(vertex.checks[0].ok, "the harness has credentials");
        h.shutdown().await;
    }

    /// Saving a provider persists it and restarts the gateway with it;
    /// removing it takes it out again.
    #[tokio::test]
    async fn saving_and_removing_a_provider_reconfigures_the_gateway() {
        let h = Harness::new(WITH_KEYED, &[]);
        h.state
            .save_provider(
                "vertex",
                values(&[("project", " my-project-1 "), ("region", "")]),
            )
            .await
            .unwrap();
        assert_eq!(
            h.saved()["models"]["providers"]["vertex"],
            serde_json::json!({ "project": "my-project-1" }),
            "trimmed; an empty field means the default"
        );
        assert_eq!(h.bifrost_providers(), ["vertex"]);
        assert_eq!(h.state.model_gateway_status(), GatewayStatus::Starting);

        let vertex = h.provider("vertex").await;
        assert!(vertex.configured && !vertex.env_only && vertex.error.is_none());
        assert_eq!(vertex.fields[0].value, "my-project-1");
        let view = h.view().await;
        assert!(
            view.models.iter().all(|m| m.id != "key-model"),
            "only Vertex's"
        );

        // The settings survive a restart.
        let reopened = Harness::open(h.tmp.path(), ModelOptions::for_tests());
        let settings = reopened.inner.settings.lock().await.clone();
        assert_eq!(
            settings.models.providers["vertex"]["project"],
            "my-project-1"
        );

        h.state.remove_provider("vertex").await.unwrap();
        assert!(!h.provider("vertex").await.configured);
        assert_eq!(
            h.state.model_gateway_status(),
            GatewayStatus::NotConfigured {
                reason: NOTHING_CONFIGURED.to_owned()
            }
        );
        // Removing what isn't there is fine.
        h.state.remove_provider("vertex").await.unwrap();
        h.shutdown().await;
    }

    /// Bad input is refused before anything is saved.
    #[tokio::test]
    async fn invalid_provider_settings_are_refused() {
        let h = Harness::new(WITH_KEYED, &[]);
        let save =
            |kind: &'static str, pairs: &[(&str, &str)]| h.state.save_provider(kind, values(pairs));
        assert!(invalid(save("nope", &[]).await).contains("unknown model provider"));
        assert!(invalid(save("vertex", &[("colour", "red")]).await).contains("no setting"));
        assert!(invalid(save("vertex", &[]).await).contains("is required"));
        assert!(invalid(save("vertex", &[("project", "x")]).await).contains("project"));
        assert!(invalid(save("keyed", &[]).await).contains("API key is required"));
        assert!(
            invalid(save("keyed", &[("api_key", "k"), ("base_url", "http://x")]).await)
                .contains("https://")
        );
        assert!(invalid(h.state.remove_provider("nope").await).contains("unknown"));
        assert!(!h.tmp.path().join("data/settings.json").exists());
        h.shutdown().await;
    }

    /// Secrets go to the vault, never to the settings file or the browser;
    /// an empty secret keeps the stored one, and removing forgets it.
    #[tokio::test]
    async fn provider_secrets_are_vaulted_kept_and_forgotten() {
        let h = Harness::new(WITH_KEYED, &[]);
        let key = providers::secret_key("keyed", "api_key");
        h.state
            .save_provider("keyed", values(&[("api_key", "sk-1")]))
            .await
            .unwrap();
        let stored = h.state.secrets_fetch_many(vec![key.clone()]).await.unwrap();
        assert_eq!(stored[&key], "sk-1");
        assert!(!h.saved().to_string().contains("sk-1"));
        assert_eq!(h.bifrost_providers(), ["anthropic"]);

        let field = &h.provider("keyed").await.fields[0];
        assert!(field.secret && field.is_set && field.value.is_empty());

        // Saving again without typing the key keeps it.
        h.state
            .save_provider(
                "keyed",
                values(&[("api_key", ""), ("base_url", "https://x")]),
            )
            .await
            .unwrap();
        let stored = h.state.secrets_fetch_many(vec![key.clone()]).await.unwrap();
        assert_eq!(stored[&key], "sk-1");

        h.state.remove_provider("keyed").await.unwrap();
        let stored = h.state.secrets_fetch_many(vec![key]).await.unwrap();
        assert!(stored.is_empty());
        assert!(!h.provider("keyed").await.fields[0].is_set);
        h.shutdown().await;
    }

    /// Environment variables beat the saved settings: such a field is
    /// read-only, the provider counts as configured, and it can't be removed.
    #[tokio::test]
    async fn environment_overrides_are_read_only() {
        let h = Harness::new(WITH_KEYED, &[("GAVMO_GCP_PROJECT", "env-project-1")]);
        let vertex = h.provider("vertex").await;
        assert!(vertex.configured && vertex.env_only);
        assert_eq!(vertex.fields[0].value, "env-project-1");
        assert_eq!(
            vertex.fields[0].overridden_by.as_deref(),
            Some("GAVMO_GCP_PROJECT")
        );
        assert_eq!(
            h.bifrost_providers(),
            ["vertex"],
            "applied on the first look"
        );

        let save = |pairs: &[(&str, &str)]| h.state.save_provider("vertex", values(pairs));
        assert!(invalid(save(&[("project", "other-project")]).await).contains("GAVMO_GCP_PROJECT"));
        save(&[("project", "env-project-1"), ("region", "us-east5")])
            .await
            .unwrap();
        assert_eq!(
            h.saved()["models"]["providers"]["vertex"],
            serde_json::json!({ "region": "us-east5" }),
            "the overridden field isn't saved"
        );
        assert!(!h.provider("vertex").await.env_only);
        assert!(invalid(h.state.remove_provider("vertex").await).contains("GAVMO_GCP_PROJECT"));
        h.shutdown().await;
    }

    /// A configured provider whose host-side prerequisites went missing is
    /// left out of the gateway, with the reason.
    #[tokio::test]
    async fn an_unusable_provider_is_reported() {
        let h = Harness::new(WITH_KEYED, &[]);
        h.state
            .save_provider("vertex", values(&[("project", "my-project-1")]))
            .await
            .unwrap();
        std::fs::remove_file(h.adc()).unwrap();
        let view = h.view().await;
        let GatewayStatus::NotConfigured { reason } = view.gateway else {
            panic!("{:?}", view.gateway);
        };
        assert!(
            reason.contains("gcloud auth application-default login"),
            "{reason}"
        );
        assert!(!view.providers[0].checks[0].ok);
        h.shutdown().await;
    }

    /// The default and small model choices must be offered models; they
    /// reach the guests' opencode config.
    #[tokio::test]
    async fn default_models_are_checked_saved_and_given_to_guests() {
        let h = Harness::new(WITH_KEYED, &[]);
        let set = |default: Option<&str>, small: Option<&str>| {
            h.state
                .set_default_models(default.map(str::to_owned), small.map(str::to_owned))
        };
        set(Some("claude-opus-5-5"), Some(" ")).await.unwrap();
        let view = h.view().await;
        assert_eq!(view.saved_default_model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(view.saved_small_model, None, "blank means automatic");
        assert_eq!(view.default_model, "claude-opus-5-5");
        let config: serde_json::Value =
            serde_json::from_slice(&h.state.guest_opencode_config().await).unwrap();
        assert_eq!(config["model"], "gavmo/claude-opus-5-5");

        let refused = match set(Some("gpt-0"), None).await {
            Err(RegistryError::InvalidInput(why)) => why,
            other => panic!("{other:?}"),
        };
        assert!(refused.contains("not an offered model"), "{refused}");
        assert_eq!(
            h.saved()["models"]["default_model"],
            "claude-opus-5-5",
            "unchanged"
        );
        h.shutdown().await;
    }

    /// Once fetched, models.dev's list is what guests are offered, newest
    /// first, with its metadata; the suggested defaults follow the newest
    /// models while a saved choice stays.
    #[tokio::test]
    async fn the_fetched_catalog_is_offered_to_guests() {
        use crate::providers::models_dev::tests::VERTEX_FIXTURE;
        let h = Harness::with_catalog(WITH_KEYED, &[], Some(VERTEX_FIXTURE));
        h.state
            .save_provider("vertex", values(&[("project", "my-project-1")]))
            .await
            .unwrap();
        let before = h.view().await;
        assert_eq!(before.models.len(), 3, "the built-in list");
        assert_eq!(before.catalog.fetched_secs_ago, None);
        h.state
            .set_default_models(Some("claude-opus-5-5".to_owned()), None)
            .await
            .unwrap();

        let message = h.state.refresh_model_catalog().await.unwrap();
        assert_eq!(message, "14 models offered");
        let view = h.view().await;
        assert_eq!(view.models[0].id, "claude-fable-5-2");
        assert!(view.models.iter().any(|m| m.id == "claude-opus-4-8"));
        assert_eq!(view.catalog.built_in, ["Keyed"]);
        assert_eq!(
            (view.default_model.as_str(), view.small_model.as_str()),
            ("claude-opus-5-5", "claude-haiku-5-5")
        );
        let config: serde_json::Value =
            serde_json::from_slice(&h.state.guest_opencode_config().await).unwrap();
        let opus = &config["provider"]["gavmo"]["models"]["claude-opus-4-8"];
        assert_eq!(opus["id"], "vertex/claude-opus-4-8@default");
        assert_eq!(opus["limit"]["context"], 1_000_000);
        assert_eq!(opus["temperature"], false);
        assert!(opus.get("knowledge").is_none(), "opencode would drop it");
        // Older models can be picked too.
        h.state
            .set_default_models(Some("claude-sonnet-4-6".to_owned()), None)
            .await
            .unwrap();

        // A failed update keeps the list and says why.
        std::fs::write(h.tmp.path().join("models-dev.json"), "oops").unwrap();
        let failed = h
            .state
            .refresh_model_catalog()
            .await
            .unwrap_err()
            .to_string();
        assert!(
            failed.contains("Couldn't update the model list"),
            "{failed}"
        );
        let view = h.view().await;
        assert_eq!(view.models.len(), 14);
        assert!(view.catalog.error.is_some_and(|e| e.contains("isn't JSON")));
        h.shutdown().await;
    }

    /// A fake model API answering `status` with `body`.
    async fn fake_model_api(status: u16, body: &'static str) -> String {
        let app = axum::Router::new().route(
            "/anthropic/v1/messages",
            axum::routing::post(move || async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [("content-type", "application/json")],
                    body,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    fn fixed(upstream: crate::bifrost::Upstream) -> AppState {
        let options = ModelOptions {
            gateway: Arc::new(ModelGateway::fixed(upstream)),
            ..ModelOptions::for_tests()
        };
        let tmp = tempfile::tempdir().unwrap();
        // The state only needs the dir while it is created.
        Harness::open(tmp.path(), options)
    }

    /// The gateway test reports a reply, an API error's message, a gateway
    /// that isn't running, and one that doesn't answer.
    #[tokio::test]
    async fn the_gateway_test_reports_how_it_went() {
        use crate::bifrost::Upstream;
        let ok = fixed(Upstream::Fixed(fake_model_api(200, "{}").await));
        let message = ok.test_model_gateway().await.unwrap();
        assert!(message.contains("answered in"), "{message}");

        let error = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        let limited = fixed(Upstream::Fixed(fake_model_api(429, error).await));
        let failed = limited.test_model_gateway().await.unwrap_err().to_string();
        assert!(
            failed.contains("429") && failed.contains("slow down"),
            "{failed}"
        );

        let off = fixed(Upstream::NotConfigured("no binary".to_owned()));
        let failed = off.test_model_gateway().await.unwrap_err().to_string();
        assert!(failed.contains("isn't running: no binary"), "{failed}");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let silent = fixed(Upstream::Fixed(closed));
        let failed = silent.test_model_gateway().await.unwrap_err().to_string();
        assert!(failed.contains("isn't answering"), "{failed}");
    }
}
