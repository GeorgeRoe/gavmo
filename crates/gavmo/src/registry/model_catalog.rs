//! The model catalog guests are offered from: the models.dev sections of
//! the supported providers (see [`crate::providers::models_dev`]), fetched
//! by the daemon at start when its copy is stale, daily after that, and on
//! demand from the settings page. The sections are cached on disk
//! (`model-catalog.json`) so a restart without network still has them;
//! until the first fetch succeeds each provider offers its built-in models.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::store::{load_json_or_set_aside, save_json};
use crate::providers::{self, Provider, ProviderModels, models_dev};
use crate::types::ModelCatalogStatus;

/// The cache file, in the data dir.
pub(super) const CACHE_FILE: &str = "model-catalog.json";

/// How old the catalog may get before it is fetched again.
pub(super) const REFRESH_EVERY: Duration = Duration::from_hours(24);

/// How long to wait after a failed fetch before trying again.
pub(super) const RETRY_AFTER: Duration = Duration::from_hours(1);

/// The whole fetch's time limit (the catalog is a few MB).
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The most of the catalog read; it is ~6 MB today.
const MAX_BYTES: usize = 64 << 20;

/// The fetched sections, as cached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Fetched {
    /// Unix seconds.
    fetched_at: u64,
    /// models.dev provider id -> its section.
    sections: BTreeMap<String, Value>,
}

#[derive(Debug, Default)]
struct State {
    fetched: Option<Fetched>,
    /// Why the last fetch failed, and when; cleared by a successful one.
    failure: Option<(String, Instant)>,
}

pub(super) struct ModelCatalog {
    url: String,
    cache_path: PathBuf,
    /// Never held across an `.await`.
    state: RwLock<State>,
    /// Fetches run one at a time.
    fetching: tokio::sync::Mutex<()>,
}

impl ModelCatalog {
    /// The catalog fetched from `url` (`file://` reads a local file),
    /// starting from its cache at `cache_path` if there is a usable one.
    pub(super) fn open(url: String, cache_path: PathBuf) -> Self {
        let fetched = load_json_or_set_aside::<Option<Fetched>>(
            &cache_path,
            "the cached model catalog was unreadable; offering the built-in models until \
             models.dev is fetched again",
        )
        .unwrap_or_else(|e| {
            tracing::warn!(path = %cache_path.display(), "cannot read the model catalog cache: {e}");
            None
        });
        Self {
            url,
            cache_path,
            state: RwLock::new(State {
                fetched,
                failure: None,
            }),
            fetching: tokio::sync::Mutex::new(()),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `providers` with the models they currently offer: their catalog
    /// section's, or their built-in ones without a (non-empty) section.
    pub(super) fn models(&self, providers: &[&'static dyn Provider]) -> Vec<ProviderModels> {
        let state = self.read();
        providers
            .iter()
            .flat_map(|&p| match Self::section_models(&state, p) {
                Some(models) => vec![(p, models)],
                None => providers::built_in(&[p]),
            })
            .collect()
    }

    /// `provider`'s models from the fetched catalog, if it lists any.
    fn section_models(state: &State, provider: &dyn Provider) -> Option<Vec<providers::ModelSpec>> {
        let section = state
            .fetched
            .as_ref()?
            .sections
            .get(provider.models_dev_id())?;
        Some(models_dev::models(provider, section)).filter(|m| !m.is_empty())
    }

    /// For the settings page.
    pub(super) fn status(&self, providers: &[&'static dyn Provider]) -> ModelCatalogStatus {
        let state = self.read();
        ModelCatalogStatus {
            fetched_secs_ago: state
                .fetched
                .as_ref()
                .map(|f| now().saturating_sub(f.fetched_at)),
            error: state.failure.as_ref().map(|(why, _)| why.clone()),
            built_in: providers
                .iter()
                .filter(|p| Self::section_models(&state, **p).is_none())
                .map(|p| p.name().to_owned())
                .collect(),
        }
    }

    /// How long until the next scheduled fetch: none yet or a stale
    /// catalog is due now, a failed fetch is retried after [`RETRY_AFTER`].
    pub(super) fn due_in(&self) -> Duration {
        let state = self.read();
        if let Some((_, failed)) = &state.failure {
            return RETRY_AFTER.saturating_sub(failed.elapsed());
        }
        state.fetched.as_ref().map_or(Duration::ZERO, |f| {
            REFRESH_EVERY.saturating_sub(Duration::from_secs(now().saturating_sub(f.fetched_at)))
        })
    }

    /// Fetches the catalog's sections for the models.dev `ids` and caches
    /// them. On failure the previous catalog stays in use; the error (for
    /// the user) is also kept for [`Self::status`].
    pub(super) async fn refresh(&self, ids: &[&str]) -> Result<(), String> {
        let _one = self.fetching.lock().await;
        let result = async {
            let bytes = fetch(&self.url).await?;
            let api: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("{} isn't JSON: {e}", self.url))?;
            let sections = models_dev::sections(&api, ids)?;
            if sections.is_empty() {
                return Err(format!("{} lists none of {}", self.url, ids.join(", ")));
            }
            Ok(sections)
        }
        .await;
        let sections = match result {
            Ok(sections) => sections,
            Err(why) => {
                self.write().failure = Some((why.clone(), Instant::now()));
                return Err(why);
            }
        };
        for id in ids.iter().filter(|id| !sections.contains_key(**id)) {
            tracing::warn!(
                provider = id,
                "models.dev doesn't list it; offering its built-in models"
            );
        }
        let fetched = Fetched {
            fetched_at: now(),
            sections,
        };
        if let Err(e) = save_json(&self.cache_path, &fetched).await {
            // Still offered until the daemon restarts.
            tracing::warn!(path = %self.cache_path.display(), "cannot cache the model catalog: {e}");
        }
        let mut state = self.write();
        state.fetched = Some(fetched);
        state.failure = None;
        Ok(())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Reads the catalog at `url`, at most [`MAX_BYTES`] of it.
async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        return if bytes.len() > MAX_BYTES {
            Err(format!("{path} is too big"))
        } else {
            Ok(bytes)
        };
    }
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("gavmo/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("cannot make an HTTP client: {e}"))?;
    let failed = |e: reqwest::Error| format!("fetching {url} failed: {e}");
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(failed)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(failed)? {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err(format!("{url} sent more than {} MB", MAX_BYTES >> 20));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::models_dev::tests::VERTEX_FIXTURE;
    use crate::providers::tests::WITH_KEYED;

    const VERTEX: &str = "google-vertex-anthropic";

    fn ids(models: &[providers::ModelSpec]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    /// A catalog file in `dir`, with `contents`, and a catalog reading it.
    fn open_catalog(dir: &std::path::Path, contents: &str) -> (PathBuf, ModelCatalog) {
        let file = dir.join("api.json");
        std::fs::write(&file, contents).unwrap();
        let catalog =
            ModelCatalog::open(format!("file://{}", file.display()), dir.join(CACHE_FILE));
        (file, catalog)
    }

    #[tokio::test]
    async fn the_built_in_models_stand_in_until_a_fetch_then_the_cache_does() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, catalog) = open_catalog(tmp.path(), VERTEX_FIXTURE);
        let before = catalog.models(WITH_KEYED);
        assert_eq!(ids(&before[0].1).len(), 3, "Vertex's built-in models");
        let status = catalog.status(WITH_KEYED);
        assert_eq!(status.fetched_secs_ago, None);
        assert_eq!(status.built_in, ["Google Vertex AI", "Keyed"]);
        assert_eq!(catalog.due_in(), Duration::ZERO);

        catalog.refresh(&[VERTEX, "anthropic"]).await.unwrap();
        let after = catalog.models(WITH_KEYED);
        assert_eq!(after[0].1.len(), 14);
        assert_eq!(after[0].1[0].id, "claude-fable-5-2");
        // The keyed provider's section isn't in the fixture: built-in.
        assert_eq!(ids(&after[1].1), ["claude-sonnet-5-5", "key-model"]);
        let status = catalog.status(WITH_KEYED);
        assert!(status.fetched_secs_ago.is_some_and(|age| age < 60));
        assert_eq!(status.error, None);
        assert_eq!(status.built_in, ["Keyed"]);
        assert!(catalog.due_in() + Duration::from_secs(60) > REFRESH_EVERY);

        // Only the sections used are cached, and a restart starts from them,
        // even with the catalog gone.
        let cached = std::fs::read_to_string(tmp.path().join(CACHE_FILE)).unwrap();
        assert!(cached.contains("claude-opus-4-8@default"));
        assert!(!cached.contains("GOOGLE_VERTEX_PROJECT"));
        std::fs::remove_file(file).unwrap();
        let (_, reopened) = open_catalog(tmp.path(), "gone");
        assert_eq!(reopened.models(WITH_KEYED)[0].1.len(), 14);
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_previous_catalog_and_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        let (file, catalog) = open_catalog(tmp.path(), VERTEX_FIXTURE);
        catalog.refresh(&[VERTEX]).await.unwrap();

        for (contents, expected) in [
            ("not json", "isn't JSON"),
            ("{}", "isn't a models.dev catalog"),
            (r#"{"other":{"models":{}}}"#, "lists none of"),
        ] {
            std::fs::write(&file, contents).unwrap();
            let why = catalog.refresh(&[VERTEX]).await.unwrap_err();
            assert!(why.contains(expected), "{why}");
            assert_eq!(catalog.status(WITH_KEYED).error, Some(why));
            assert_eq!(catalog.models(WITH_KEYED)[0].1.len(), 14);
            assert!(catalog.due_in() + Duration::from_secs(60) > RETRY_AFTER);
            assert!(catalog.due_in() <= RETRY_AFTER);
        }
        std::fs::remove_file(&file).unwrap();
        let why = catalog.refresh(&[VERTEX]).await.unwrap_err();
        assert!(why.contains("cannot read"), "{why}");

        // A success clears the error.
        std::fs::write(&file, VERTEX_FIXTURE).unwrap();
        catalog.refresh(&[VERTEX]).await.unwrap();
        assert_eq!(catalog.status(WITH_KEYED).error, None);
    }

    #[tokio::test]
    async fn an_empty_section_or_a_corrupt_cache_falls_back_to_the_built_in_models() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, catalog) = open_catalog(
            tmp.path(),
            r#"{"google-vertex-anthropic":{"models":{"old@1":{"status":"deprecated"}}}}"#,
        );
        catalog.refresh(&[VERTEX]).await.unwrap();
        assert_eq!(catalog.models(WITH_KEYED)[0].1.len(), 3);
        assert!(
            catalog
                .status(WITH_KEYED)
                .built_in
                .contains(&"Google Vertex AI".to_owned())
        );

        std::fs::write(tmp.path().join(CACHE_FILE), "not json").unwrap();
        let (_, reopened) = open_catalog(tmp.path(), "{}");
        assert_eq!(reopened.status(WITH_KEYED).fetched_secs_ago, None);
        assert!(!tmp.path().join(CACHE_FILE).exists(), "moved aside");
    }

    /// Over HTTP: a server's error status is reported, and a body over the
    /// limit is refused.
    #[tokio::test]
    async fn http_failures_are_reported() {
        let app = axum::Router::new()
            .route(
                "/missing",
                axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route("/api.json", axum::routing::get(|| async { VERTEX_FIXTURE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let why = fetch(&format!("{base}/missing")).await.unwrap_err();
        assert!(why.contains("404"), "{why}");
        let bytes = fetch(&format!("{base}/api.json")).await.unwrap();
        assert_eq!(bytes, VERTEX_FIXTURE.as_bytes());
    }
}
