//! Server-side (ssr-only) state for the GAVMO daemon.
//!
//! Holds the persistent workspace registry (a JSON file under a global app
//! dir) and the in-memory map of currently-running VMs. This module is the
//! only place that touches the `vm` crate, keeping all VM-management
//! mechanism behind the daemon's server functions.

use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use vm::{Error as VmError, RunningVm, VmConfig, VmStatus};

use mcp::{TokenAuthority, ToolDescriptor, ToolExecutor, ToolOutcome, WorkspaceScope};
use secrets::SecretStore;

use crate::app::{ToolConfig, VmStats, WorkspaceInfo, WorkspaceStatus};
use crate::integrations::{CandidateTool, ResolvedIntegration};

/// Default guest memory, in MiB.
const DEFAULT_MEMORY_MIB: u32 = 4096;
/// Default guest vCPU count.
const DEFAULT_VCPUS: u32 = 4;

/// Gap between the two `/proc` samples [`AppState::vm_stats`] diffs for CPU
/// percentage. Short enough to keep a poll snappy, long enough that ~10ms
/// jiffy granularity doesn't dominate the measurement.
const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Appended to a running VM's base URL so "Open UI" lands in a new opencode
/// session for the guest's `/workspace`: base64url("/workspace") + "/session",
/// which opencode routes to a new session for that project. The
/// `opencode_workspace_session_path_matches_base64url_workspace` test checks the derivation.
const OPENCODE_WORKSPACE_SESSION_PATH: &str = "/L3dvcmtzcGFjZQ/session";

/// On-disk registry: the set of repos GAVMO knows about. Running state is
/// never persisted - a stopped VM simply isn't in the in-memory map.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    pub workspaces: Vec<WorkspaceRecord>,
}

/// A single known workspace, identified by the repo's absolute host path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub path: String,

    /// The host port this workspace was last booted on, if any. Persisted so
    /// the browser origin stays stable across restarts, letting opencode's
    /// per-origin `localStorage` (last project/session) survive.
    /// `serde(default)` keeps older `workspaces.json` files loadable.
    #[serde(default)]
    pub host_port: Option<u16>,
}

/// Shared, cloneable application state, injected into every server function
/// via Leptos context.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    registry_path: PathBuf,
    registry: Mutex<Registry>,
    running: Mutex<HashMap<String, VmSlot>>,
    base_image: PathBuf,
    gcloud_dir: PathBuf,
    memory_mib: u32,
    vcpus: u32,

    /// Root dir for every workspace's MCP token share
    /// (`~/.local/share/gavmo/mcp`), deliberately outside any repo.
    mcp_base_dir: PathBuf,

    /// Bearer token -> workspace id. A `std::sync::Mutex` because
    /// `TokenAuthority::resolve` is synchronous and the lock is never held
    /// across an `.await`. Re-minted per boot; cleared on `stop`/`reset`.
    mcp_tokens: std::sync::Mutex<HashMap<String, String>>,

    /// Host secret store backing the tool integrations; an encrypted vault
    /// opened lazily on first access.
    secret_store: Arc<dyn SecretStore>,

    /// Workspace id -> tool integrations resolved for its current boot.
    /// Cleared on `stop`/`reset`; `std::sync::Mutex` since `list_tools` is sync.
    workspace_integrations: std::sync::Mutex<HashMap<String, Vec<ResolvedIntegration>>>,

    /// Shared HTTP client for the tool forwarder; clones share one
    /// connection pool across all workspaces.
    http_client: reqwest::Client,
}

/// State of a workspace's VM while a boot is in flight or has settled.
/// Boots happen in a background task (see [`AppState::open_workspace`]) so
/// the server function returns immediately; the live status poll picks up
/// the transition from `Booting` to `Running`/`Failed`.
enum VmSlot {
    /// `RunningVm::boot` is in progress in this task. Stop/reset abort it;
    /// since qemu's `Child` is `kill_on_drop`, awaiting the aborted handle
    /// guarantees qemu is gone before they proceed (e.g. touch the overlay).
    Booting(tokio::task::JoinHandle<()>),
    /// Booted and (as of the last check) healthy. Boxed because `RunningVm`
    /// is much larger than the other variants.
    Running(Box<RunningVm>),
    /// The last boot attempt failed; kept so the reason stays visible in
    /// the UI until the next `open_workspace`.
    Failed(String),
    /// The repo's `gavmo.nix` failed to build (detected early via the
    /// guest's failure marker). `log` is the log filename relative to the
    /// workspace's `.gavmo/vm` dir, readable via [`AppState::read_build_log`].
    BuildFailed { log: String },
}

impl AppState {
    /// Builds the app state, loading the persisted registry if present.
    ///
    /// `base_image` is the read-only NixOS qcow2 (from `GAVMO_BASE_IMAGE`);
    /// `gcloud_dir` is the ADC config dir mounted read-only into every VM.
    pub fn new(base_image: PathBuf, gcloud_dir: PathBuf) -> io::Result<Self> {
        let registry_path = registry_path()?;
        let registry = load_registry(&registry_path)?;
        let gavmo_data_dir = dirs::data_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?
            .join("gavmo");
        let mcp_base_dir = gavmo_data_dir.join("mcp");
        // Opened lazily so a host with a broken/absent OS keystore can still
        // run the daemon and manage VMs; only secret operations fail (closed).
        // There is no plaintext fallback.
        let secret_store: Arc<dyn SecretStore> = Arc::new(secrets::LazyVaultStore::keyring_backed(
            gavmo_data_dir.join("secrets.vault"),
        ));

        Ok(Self {
            inner: Arc::new(Inner {
                registry_path,
                registry: Mutex::new(registry),
                running: Mutex::new(HashMap::new()),
                base_image,
                gcloud_dir,
                memory_mib: DEFAULT_MEMORY_MIB,
                vcpus: DEFAULT_VCPUS,
                mcp_base_dir,
                mcp_tokens: std::sync::Mutex::new(HashMap::new()),
                secret_store,
                workspace_integrations: std::sync::Mutex::new(HashMap::new()),
                http_client: reqwest::Client::new(),
            }),
        })
    }

    /// All known workspaces plus each one's current status.
    pub async fn list(&self) -> Vec<WorkspaceInfo> {
        let registry = self.inner.registry.lock().await;
        let mut running = self.inner.running.lock().await;

        let mut out = Vec::with_capacity(registry.workspaces.len());
        for rec in &registry.workspaces {
            let status = match running.get_mut(&rec.path) {
                Some(slot) => slot_status(slot).await,
                None => WorkspaceStatus::Stopped,
            };
            out.push(WorkspaceInfo {
                id: rec.path.clone(),
                path: rec.path.clone(),
                status,
            });
        }
        out
    }

    /// Registers a new workspace by absolute host path. Idempotent: adding
    /// a path that is already known is a no-op.
    pub async fn add_workspace(&self, path: String) -> io::Result<()> {
        let canonical = std::fs::canonicalize(&path)?.to_string_lossy().into_owned();

        let mut registry = self.inner.registry.lock().await;
        if !registry.workspaces.iter().any(|w| w.path == canonical) {
            registry.workspaces.push(WorkspaceRecord {
                path: canonical,
                host_port: None,
            });
            save_registry(&self.inner.registry_path, &registry)?;
        }
        Ok(())
    }

    /// Current status of a single workspace.
    pub async fn status(&self, id: &str) -> WorkspaceStatus {
        let mut running = self.inner.running.lock().await;
        match running.get_mut(id) {
            Some(slot) => slot_status(slot).await,
            None => WorkspaceStatus::Stopped,
        }
    }

    /// Kicks off booting the VM for a workspace (or is a no-op if it's
    /// already booting/running) and returns immediately without waiting for
    /// the boot. Callers poll `status`/`list` to observe the transition.
    pub async fn open_workspace(&self, id: &str) -> Result<(), String> {
        // Bail early if already up: our own live VM holds the persisted port,
        // so `resolve_host_port` would see it as taken and needlessly
        // reallocate it.
        if matches!(
            self.inner.running.lock().await.get(id),
            Some(VmSlot::Booting(_) | VmSlot::Running(_))
        ) {
            return Ok(());
        }

        // Takes only the `registry` lock, not nested under `running`: lock
        // order is registry then running (as in `list`), so the reverse
        // would risk deadlock.
        let host_port = self
            .resolve_host_port(id)
            .await
            .map_err(|e| format!("failed to resolve a host port: {e}"))?;

        // Check, mint, spawn and insert under one held lock with no `.await`
        // between check and insert (TOCTOU): two concurrent opens for the
        // same `id` can never both boot. This check, not the early peek, is
        // the load-bearing one.
        let mut running = self.inner.running.lock().await;
        match running.get(id) {
            Some(VmSlot::Booting(_) | VmSlot::Running(_)) => return Ok(()),
            // Retrying after `BuildFailed` is expected once `gavmo.nix` is fixed.
            Some(VmSlot::Failed(_) | VmSlot::BuildFailed { .. }) | None => {}
        }

        // Mint the per-boot MCP token only on a real new boot: rotating a
        // running VM's token would break its already-loaded `gavmo-opencode`.
        // The small file write under the `running` lock is accepted.
        let mcp_dir = self.mcp_dir_for(id);
        if let Err(e) = self.mint_mcp_token(id, &mcp_dir).await {
            let reason = format!("failed to write MCP token: {e}");
            running.insert(id.to_string(), VmSlot::Failed(reason.clone()));
            return Err(reason);
        }

        let cfg = self.vm_config(id, mcp_dir, host_port);

        let state = self.clone();
        let task_id = id.to_string();
        let handle = tokio::spawn(async move {
            let result = RunningVm::boot(cfg).await;

            // Resolved before taking the `running` lock so the keyring check
            // can't block `status`/`list`. A failed boot exposes no tools.
            let integrations = if result.is_ok() {
                state.resolve_integrations_for(&task_id).await
            } else {
                Vec::new()
            };

            let mut running = state.inner.running.lock().await;

            // If the slot is no longer `Booting` (superseded or stopped),
            // don't resurrect it; stop any VM we booted rather than leak qemu.
            // A variant check, not task identity: being replaced by another
            // `Booting` before we get here is an accepted small race.
            let still_wanted = matches!(running.get(&task_id), Some(VmSlot::Booting(_)));
            match result {
                Ok(vm) if still_wanted => {
                    state.set_integrations(&task_id, integrations);
                    running.insert(task_id, VmSlot::Running(Box::new(vm)));
                }
                Ok(vm) => {
                    drop(running);
                    if let Err(e) = vm.stop().await {
                        tracing::warn!(
                            workspace = %task_id,
                            error = %e,
                            "failed to stop a superseded VM cleanly"
                        );
                    }
                }
                Err(e) if still_wanted => {
                    let slot = match &e {
                        VmError::RebuildFailed { log_file } => VmSlot::BuildFailed {
                            log: log_file.clone(),
                        },
                        _ => VmSlot::Failed(e.to_string()),
                    };
                    running.insert(task_id, slot);
                }
                Err(_) => {}
            }
        });

        running.insert(id.to_string(), VmSlot::Booting(handle));

        Ok(())
    }

    /// Stops the VM for a workspace and drops it from the running map. A
    /// no-op if it isn't currently running or booting.
    ///
    /// An in-flight boot is aborted and awaited, so qemu is gone on return
    /// (killed on drop, or gracefully stopped by the boot task if it had
    /// just succeeded - rarely blocking up to ~35s).
    pub async fn stop_workspace(&self, id: &str) -> Result<(), String> {
        // Revoke the MCP token first so a stopping VM can never authorize
        // gateway calls again, however the stop resolves.
        self.clear_mcp_token(id);
        self.clear_integrations(id);

        let slot = self.inner.running.lock().await.remove(id);
        match slot {
            Some(VmSlot::Running(vm)) => vm.stop().await.map_err(|e| e.to_string()),
            Some(VmSlot::Booting(handle)) => {
                abort_boot_task(id, handle).await;
                Ok(())
            }
            Some(VmSlot::Failed(_) | VmSlot::BuildFailed { .. }) | None => Ok(()),
        }
    }

    /// Stops the VM and drops the workspace from the registry. Non-destructive:
    /// `.gavmo/vm` is kept, so re-adding the path restores its overlay and
    /// session history. A no-op for unknown ids.
    pub async fn remove_workspace(&self, id: &str) -> Result<(), String> {
        self.stop_workspace(id).await?;

        let mut registry = self.inner.registry.lock().await;
        registry.workspaces.retain(|w| w.path != id);
        save_registry(&self.inner.registry_path, &registry).map_err(|e| e.to_string())
    }

    /// Stops the VM and deletes `<repo>/.gavmo/vm` (overlay, logs, sockets),
    /// so the next open builds a fresh overlay from the current base image -
    /// needed after a base-image rebuild, since an overlay's backing file is
    /// fixed at creation.
    ///
    /// Destructive (session history and `/nix` cache are lost); the caller
    /// must have obtained user confirmation. An in-flight boot is aborted and
    /// awaited first, so qemu can't still be writing the overlay.
    pub async fn reset_workspace(&self, id: &str) -> io::Result<()> {
        self.clear_mcp_token(id);
        self.clear_integrations(id);

        let slot = self.inner.running.lock().await.remove(id);
        match slot {
            // The state dir is deleted regardless: qemu has been killed by
            // `stop` at the latest via SIGKILL, so a failure here is only
            // worth logging, not blocking the reset the user confirmed.
            Some(VmSlot::Running(vm)) => {
                if let Err(e) = vm.stop().await {
                    tracing::warn!(workspace = %id, error = %e, "VM did not stop cleanly during reset");
                }
            }
            Some(VmSlot::Booting(handle)) => abort_boot_task(id, handle).await,
            Some(VmSlot::Failed(_) | VmSlot::BuildFailed { .. }) | None => {}
        }

        remove_dir_all_idempotent(&Self::state_dir_for(id)).await
    }

    /// Builds the per-VM [`VmConfig`] for a workspace path. `host_port` is
    /// passed in because resolving it takes the `registry` lock, which must
    /// not nest under the `running` lock held here.
    fn vm_config(&self, id: &str, mcp_dir: PathBuf, host_port: u16) -> VmConfig {
        let repo_dir = PathBuf::from(id);
        let state_dir = Self::state_dir_for(id);
        let workspace_id = repo_dir.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );

        VmConfig {
            workspace_id,
            repo_dir,
            state_dir,
            base_image: self.inner.base_image.clone(),
            gcloud_dir: self.inner.gcloud_dir.clone(),
            mcp_dir,
            memory_mib: self.inner.memory_mib,
            vcpus: self.inner.vcpus,
            host_port,
        }
    }

    /// Returns the workspace's persisted host port if still free, otherwise
    /// allocates and persists a new one. A stable port keeps the browser
    /// origin (and opencode's per-origin `localStorage`) stable across restarts.
    ///
    /// Never call while holding `running` (lock order is registry then
    /// running), nor while this workspace's VM is up (its own VM holds the
    /// port, which would force a needless reallocation).
    async fn resolve_host_port(&self, id: &str) -> io::Result<u16> {
        let mut registry = self.inner.registry.lock().await;
        let (port, changed) = resolve_host_port_in(&mut registry, id)?;
        if changed {
            save_registry(&self.inner.registry_path, &registry)?;
        }
        Ok(port)
    }

    /// The per-workspace state dir (`<repo>/.gavmo/vm`). Derived from `id`
    /// because a `BuildFailed` slot outlives its `VmConfig`.
    fn state_dir_for(id: &str) -> PathBuf {
        PathBuf::from(id).join(".gavmo").join("vm")
    }

    /// This workspace's MCP token share dir, under GAVMO's app-data dir and
    /// outside the repo so the token can't leak into the user's project.
    fn mcp_dir_for(&self, id: &str) -> PathBuf {
        self.inner.mcp_base_dir.join(mcp_share_dir_name(id))
    }

    /// Mints a fresh MCP token for `id`, writes it to the share as
    /// `token.env` (the guest's `GAVMO_MCP_TOKEN`), and records it in
    /// `mcp_tokens`, replacing any previous one: one valid token per workspace.
    async fn mint_mcp_token(&self, id: &str, dir: &Path) -> io::Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let token = uuid::Uuid::new_v4().to_string();
        let path = dir.join("token.env");
        write_owner_only(&path, format!("GAVMO_MCP_TOKEN={token}\n").as_bytes()).await?;

        let mut tokens = lock_unpoisoned(&self.inner.mcp_tokens);
        tokens.retain(|_, v| v != id);
        tokens.insert(token, id.to_string());
        Ok(())
    }

    /// Invalidates `id`'s MCP token, so a stopped VM can't call the gateway
    /// even if its `token.env` is still on disk.
    fn clear_mcp_token(&self, id: &str) {
        lock_unpoisoned(&self.inner.mcp_tokens).retain(|_, v| v != id);
    }

    /// Resolves the enabled tool integrations for `id`: tools requested in
    /// `gavmo.yaml` that have a correctly bound secret configured.
    async fn resolve_integrations_for(&self, id: &str) -> Vec<ResolvedIntegration> {
        let candidates = self.candidates_for(id).await;
        // Values, not just presence: a credential bound to a different
        // destination than the current `base_url` isn't advertised at all
        // (defense in depth over the forwarder's own check).
        let values = self
            .secrets_fetch_many(candidates.iter().map(|c| c.secret_key.clone()).collect())
            .await;
        candidates
            .into_iter()
            .filter(|candidate| {
                matches!(
                    crate::integrations::classify_binding(
                        &candidate.def.base_url,
                        values.get(&candidate.secret_key).map(String::as_str),
                    ),
                    crate::integrations::BindingState::Configured
                )
            })
            .map(CandidateTool::into_resolved)
            .collect()
    }

    /// Records the resolved integrations for a freshly-booted workspace.
    fn set_integrations(&self, id: &str, integrations: Vec<ResolvedIntegration>) {
        lock_unpoisoned(&self.inner.workspace_integrations).insert(id.to_string(), integrations);
    }

    /// Forgets a workspace's integrations on stop/reset.
    fn clear_integrations(&self, id: &str) {
        lock_unpoisoned(&self.inner.workspace_integrations).remove(id);
    }

    /// The integrations currently resolved for `id` (empty if none/stopped).
    fn integrations_for(&self, id: &str) -> Vec<ResolvedIntegration> {
        lock_unpoisoned(&self.inner.workspace_integrations)
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Every tool this workspace's `gavmo.yaml` requests, read live (no secret
    /// access) so the control panel reflects config changes immediately.
    async fn candidates_for(&self, id: &str) -> Vec<CandidateTool> {
        let config = crate::integrations::read_config(id).await;
        crate::integrations::resolve_candidates(id, &config)
    }

    /// Finds a single requested tool by id (structural; no secret-store access).
    async fn candidate(&self, id: &str, tool_id: &str) -> Result<CandidateTool, String> {
        self.candidates_for(id)
            .await
            .into_iter()
            .find(|candidate| candidate.id == tool_id)
            .ok_or_else(|| format!("unknown tool: {tool_id}"))
    }

    // --- Secret store helpers ---
    //
    // `SecretStore` calls are synchronous and can be slow (a locked keyring
    // may prompt), so they always run via `spawn_blocking`.

    /// Fetches one secret value (`None` if absent or on backend error).
    async fn secret_get(&self, key: String) -> Option<String> {
        let store = Arc::clone(&self.inner.secret_store);
        match tokio::task::spawn_blocking(move || store.get(&key)).await {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "secret store read failed; treating as unset");
                None
            }
            Err(e) => {
                tracing::error!(error = %e, "secret store task failed");
                None
            }
        }
    }

    /// Stores one secret value.
    async fn secret_set(&self, key: String, value: String) -> Result<(), String> {
        let store = Arc::clone(&self.inner.secret_store);
        tokio::task::spawn_blocking(move || store.set(&key, &value))
            .await
            .map_err(|e| format!("secret store task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Deletes one secret value.
    async fn secret_delete(&self, key: String) -> Result<(), String> {
        let store = Arc::clone(&self.inner.secret_store);
        tokio::task::spawn_blocking(move || store.delete(&key))
            .await
            .map_err(|e| format!("secret store task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Fetches the values for `keys` in one blocking batch; absent or errored
    /// keys are omitted.
    async fn secrets_fetch_many(&self, keys: Vec<String>) -> HashMap<String, String> {
        let store = Arc::clone(&self.inner.secret_store);
        let fetched = tokio::task::spawn_blocking(move || {
            keys.into_iter()
                .filter_map(|key| match store.get(&key) {
                    Ok(value) => value.map(|v| (key, v)),
                    Err(e) => {
                        tracing::warn!(error = %e, "secret store read failed; treating as unset");
                        None
                    }
                })
                .collect()
        })
        .await;
        fetched.unwrap_or_else(|e| {
            tracing::error!(error = %e, "secret store task failed");
            HashMap::new()
        })
    }

    /// The control panel's view of a workspace's requested tools and whether
    /// each has its credential configured.
    pub async fn list_tool_configs(&self, id: &str) -> Vec<ToolConfig> {
        let candidates = self.candidates_for(id).await;
        // Values are needed to detect a changed destination, not just presence.
        let values = self
            .secrets_fetch_many(candidates.iter().map(|c| c.secret_key.clone()).collect())
            .await;

        candidates
            .into_iter()
            .map(|tool| {
                let state = crate::integrations::classify_binding(
                    &tool.def.base_url,
                    values.get(&tool.secret_key).map(String::as_str),
                );
                let (configured, url_changed, bound_url) = match state {
                    crate::integrations::BindingState::NotConfigured => (false, false, None),
                    crate::integrations::BindingState::Configured => (true, false, None),
                    crate::integrations::BindingState::DestinationChanged { bound_url } => {
                        (false, true, bound_url)
                    }
                };
                ToolConfig {
                    configured,
                    url_changed,
                    bound_url,
                    id: tool.id,
                    description: tool
                        .def
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("HTTP access to {}", tool.def.base_url)),
                    base_url: tool.def.base_url,
                    auth: match tool.def.auth {
                        toolspec::AuthScheme::Basic => "basic",
                        toolspec::AuthScheme::Bearer => "bearer",
                        toolspec::AuthScheme::Header => "header",
                    }
                    .to_string(),
                    secret_name: tool.def.secret,
                    instructions: tool.def.instructions,
                }
            })
            .collect()
    }

    /// Stores (or replaces) a tool's credential in the secret store. For Basic
    /// auth both `username` and `token` are required; for Bearer/Header only
    /// `token`. Takes effect on the VM's next open, since a running VM's
    /// tools are fixed at boot.
    pub async fn set_tool_secret(
        &self,
        id: &str,
        tool_id: &str,
        username: Option<String>,
        token: String,
    ) -> Result<(), String> {
        let tool = self.candidate(id, tool_id).await?;

        let value = match tool.def.auth {
            toolspec::AuthScheme::Basic => {
                let username = username
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| "basic auth requires a username/email".to_string())?;
                serde_json::json!({ "username": username, "password": token })
            }
            toolspec::AuthScheme::Bearer | toolspec::AuthScheme::Header => {
                serde_json::json!({ "token": token })
            }
        };

        // Bind the credential to the destination the user approved, so a later
        // `base_url` change in the untrusted `gavmo.yaml` fails closed instead
        // of silently redirecting the token.
        let canonical = toolspec::canonical_base_url(&tool.def.base_url)
            .map_err(|e| format!("tool base_url could not be canonicalized: {e}"))?;
        let bound = crate::integrations::wrap_stored_secret(&canonical, &value.to_string());

        self.secret_set(tool.secret_key, bound).await
    }

    /// Removes a tool's stored credential (disabling it on the next open).
    pub async fn clear_tool_secret(&self, id: &str, tool_id: &str) -> Result<(), String> {
        let tool = self.candidate(id, tool_id).await?;
        self.secret_delete(tool.secret_key).await
    }

    /// Live credential test: forwards a `GET` to the tool's `test_path` (or
    /// `/`) with the stored credential; `Ok` on 2xx. Without an auth-reflecting
    /// `test_path` this is only a reachability probe.
    pub async fn test_tool(&self, id: &str, tool_id: &str) -> Result<String, String> {
        let tool = self.candidate(id, tool_id).await?;
        let secret_value = self.secret_get(tool.secret_key.clone()).await;

        let test_path = tool
            .def
            .test_path
            .clone()
            .unwrap_or_else(|| "/".to_string());
        let mut args = serde_json::Map::new();
        args.insert("method".to_string(), serde_json::json!("GET"));
        args.insert("path".to_string(), serde_json::json!(test_path));

        match crate::integrations::forward(
            &tool.into_resolved(),
            secret_value,
            &args,
            &self.inner.http_client,
        )
        .await
        {
            ToolOutcome::Success(text) => Ok(text),
            ToolOutcome::Error(text) => Err(text),
        }
    }

    /// Reads a `gavmo.nix` build-failure log for the UI. `log` is
    /// client-supplied and becomes part of a path, so it is validated strictly.
    pub async fn read_build_log(&self, id: &str, log: &str) -> io::Result<String> {
        if !is_safe_rebuild_log_filename(log) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid build log filename",
            ));
        }
        tokio::fs::read_to_string(Self::state_dir_for(id).join(log)).await
    }

    /// Reads and sanitizes the guest's boot console log for the live panel.
    /// A missing file (qemu not spawned yet) is an empty log, not an error.
    pub async fn read_console_log(&self, id: &str) -> io::Result<String> {
        let path = Self::state_dir_for(id).join("console.log");
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(sanitize_console(&bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// Resource usage for a `Running` VM: per-core CPU %, guest-reported
    /// memory, uptime, overlay size and host port. `None` (no stats this
    /// tick) if not running or a `/proc` read fails.
    ///
    /// The `running` lock is released before the CPU sampling sleep and the
    /// QMP call, so stats polls never block `stop`/`reset`.
    pub async fn vm_stats(&self, id: &str) -> Option<VmStats> {
        let (pid, uptime_secs, vcpus, host_port, qmp_socket_path) = {
            let mut running = self.inner.running.lock().await;
            match running.get_mut(id) {
                Some(VmSlot::Running(vm)) => (
                    vm.qemu_pid()?,
                    vm.uptime().as_secs(),
                    vm.vcpus(),
                    vm.host_port(),
                    vm.qmp_socket_path(),
                ),
                _ => return None,
            }
        };

        let disk_bytes =
            vm::stats::file_size_or_zero(&Self::state_dir_for(id).join("disk.qcow2")).ok()?;

        let prev = vm::stats::sample(pid).ok()?;
        tokio::time::sleep(CPU_SAMPLE_INTERVAL).await;
        let cur = vm::stats::sample(pid).ok()?;
        let cpu_percent = vm::stats::cpu_percent_per_core(prev, cur);

        // If the balloon driver hasn't reported yet, show no memory figures
        // rather than discarding the other stats.
        let guest_mem = vm::guest_memory(&qmp_socket_path).await.ok().flatten();

        Some(VmStats {
            cpu_percent,
            vcpus,
            mem_used_bytes: guest_mem.map(|m| m.total_bytes.saturating_sub(m.available_bytes)),
            mem_total_bytes: guest_mem.map(|m| m.total_bytes),
            uptime_secs,
            disk_bytes,
            host_port,
        })
    }
}

/// Lets the `mcp` gateway resolve a bearer token to a workspace without
/// depending on `gavmo`.
impl TokenAuthority for AppState {
    fn resolve(&self, token: &str) -> Option<WorkspaceScope> {
        let tokens = lock_unpoisoned(&self.inner.mcp_tokens);
        let workspace_id = tokens.get(token)?.clone();
        Some(WorkspaceScope { workspace_id })
    }
}

/// The daemon's MCP tools: the `gavmo_ping` probe plus one host-locked HTTP
/// forwarder per enabled integration of the authorized workspace.
impl ToolExecutor for AppState {
    fn list_tools(&self, scope: &WorkspaceScope) -> Vec<ToolDescriptor> {
        let mut tools = vec![ping_descriptor()];
        for integration in self.integrations_for(&scope.workspace_id) {
            tools.push(crate::integrations::integration_tool_descriptor(
                &integration,
            ));
        }
        tools
    }

    async fn call_tool(
        &self,
        scope: &WorkspaceScope,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> ToolOutcome {
        if name == "gavmo_ping" {
            return ToolOutcome::Success(format!("pong from workspace {}", scope.workspace_id));
        }

        // A miss is only possible if tools were cleared between list and call.
        match self
            .integrations_for(&scope.workspace_id)
            .into_iter()
            .find(|integration| integration.id == name)
        {
            Some(integration) => {
                // Fetched fresh per call; credentials are never cached.
                let secret_value = self.secret_get(integration.secret_key.clone()).await;
                crate::integrations::forward(
                    &integration,
                    secret_value,
                    &arguments,
                    &self.inner.http_client,
                )
                .await
            }
            None => ToolOutcome::Error(format!("unknown tool: {name}")),
        }
    }
}

/// The `gavmo_ping` tool descriptor: no arguments, just proves a round trip
/// (discovery + call + auth) through the whole pipeline.
fn ping_descriptor() -> ToolDescriptor {
    let mut input_schema = serde_json::Map::new();
    input_schema.insert(
        "type".to_string(),
        serde_json::Value::String("object".into()),
    );
    input_schema.insert("properties".to_string(), serde_json::json!({}));
    input_schema.insert(
        "additionalProperties".to_string(),
        serde_json::Value::Bool(false),
    );
    ToolDescriptor {
        name: "gavmo_ping".to_string(),
        description: "Checks connectivity to the GAVMO host-side tool gateway and reports \
             which workspace this VM is authorized for. Takes no arguments."
            .to_string(),
        input_schema,
    }
}

/// Deterministic, filesystem-safe directory name for a workspace's MCP token
/// share: the hex SHA-256 of the workspace path.
///
/// SHA-256 rather than the hex-encoded path itself: hex doubles the length, so
/// any path over 127 bytes exceeded the 255-byte filename limit and that
/// workspace could never boot. The digest is always 64 chars.
fn mcp_share_dir_name(id: &str) -> String {
    use sha2::Digest as _;

    format!("{:x}", sha2::Sha256::digest(id.as_bytes()))
}

/// Aborts an in-flight boot task and waits for it to finish, so qemu (owned
/// by the task, `kill_on_drop`) is gone on return. Cancellation, or the task
/// having already finished, is the expected outcome; a panic is logged.
async fn abort_boot_task(id: &str, handle: tokio::task::JoinHandle<()>) {
    handle.abort();
    match handle.await {
        Ok(()) => {}
        Err(e) if e.is_cancelled() => {}
        Err(e) => tracing::error!(workspace = %id, error = %e, "boot task panicked"),
    }
}

/// Recovers a poisoned `std` mutex. Every map guarded this way (MCP tokens,
/// resolved integrations) is updated with single, self-contained operations,
/// so a panic elsewhere can't leave it half-modified; serving it is safer
/// than turning one panic into a panic on every later request.
fn lock_unpoisoned<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Writes `contents` to `path`, truncating any existing file, with mode
/// `0600` applied at creation on Unix - never briefly readable by other users
/// as with write-then-chmod. An existing file's mode is forced back to `0600`
/// too, since `mode` only applies when the file is created.
async fn write_owner_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    file.write_all(contents).await?;
    file.flush().await
}

/// Deletes a directory tree, treating "already gone" as success.
async fn remove_dir_all_idempotent(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `true` only for `rebuild-<stem>.log` where `<stem>` is non-empty ASCII
/// alphanumerics, `-` and `_` (what `gavmo-rebuild` writes:
/// `rebuild-2026-01-01T00-00-00Z.log`). The name comes from a guest-written
/// marker, so this is an allowlist rather than a denylist of separators: no
/// `/`, `\`, `.`, NUL, control or non-ASCII characters can get through, so a
/// crafted name can't escape the workspace's state dir. Case-sensitive on
/// purpose.
fn is_safe_rebuild_log_filename(name: &str) -> bool {
    name.strip_prefix("rebuild-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .is_some_and(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Strips terminal escape sequences from a raw serial-console capture,
/// leaving plain text for a `<pre>`. `\r\n` and bare `\r` become `\n` (a bare
/// `\r` would otherwise overwrite status lines); invalid UTF-8 is replaced lossily.
fn sanitize_console(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // Skip exactly one control sequence, preserving surrounding text.
                match chars.peek() {
                    // CSI: `ESC [ ... final`, final byte in 0x40-0x7E.
                    Some('[') => {
                        chars.next();
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&c) {
                                break;
                            }
                        }
                    }
                    // OSC/DCS/APC/PM string sequences, terminated by BEL or
                    // ST (`ESC \`). GRUB emits DCS in real boot captures.
                    Some(']' | 'P' | '_' | '^') => {
                        chars.next();
                        while let Some(&c) = chars.peek() {
                            if c == '\u{7}' {
                                chars.next();
                                break;
                            }
                            if c == '\u{1b}' {
                                chars.next();
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                            chars.next();
                        }
                    }
                    // Other two-byte escapes (e.g. `ESC c`, `ESC 7`).
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            c => out.push(c),
        }
    }

    out
}

/// Maps a [`VmSlot`] to the serializable view type shared with the browser,
/// probing the live `vm` crate status for a `Running` slot (the qemu
/// process may have exited or stopped answering health checks since the
/// last poll).
async fn slot_status(slot: &mut VmSlot) -> WorkspaceStatus {
    match slot {
        VmSlot::Booting(_) => WorkspaceStatus::Booting,
        VmSlot::Running(vm) => to_status(vm.status().await),
        VmSlot::Failed(reason) => WorkspaceStatus::Failed {
            reason: reason.clone(),
        },
        VmSlot::BuildFailed { log } => WorkspaceStatus::BuildFailed { log: log.clone() },
    }
}

/// Maps the `vm` crate's status into the serializable view type shared with
/// the browser.
fn to_status(status: VmStatus) -> WorkspaceStatus {
    match status {
        VmStatus::Stopped => WorkspaceStatus::Stopped,
        VmStatus::Booting => WorkspaceStatus::Booting,
        VmStatus::Running { url } => WorkspaceStatus::Running {
            url: format!("{url}{OPENCODE_WORKSPACE_SESSION_PATH}"),
        },
        VmStatus::Failed { reason } => WorkspaceStatus::Failed { reason },
    }
}

/// Allocates a free TCP port by binding to `127.0.0.1:0`. The small race
/// between releasing the socket and QEMU binding it is accepted.
fn alloc_port() -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Reuses `existing` if it is currently bindable, else allocates a fresh
/// port. Same accepted check-then-bind race as [`alloc_port`].
fn choose_host_port(existing: Option<u16>) -> io::Result<u16> {
    let reusable = existing.filter(|&port| TcpListener::bind(("127.0.0.1", port)).is_ok());
    match reusable {
        Some(port) => Ok(port),
        None => alloc_port(),
    }
}

/// Chooses `id`'s host port and updates its [`WorkspaceRecord`] if the port
/// changed. Returns `(port, changed)` so the caller only saves when needed.
/// An unknown `id` gets a fresh port and leaves `registry` untouched.
fn resolve_host_port_in(registry: &mut Registry, id: &str) -> io::Result<(u16, bool)> {
    let existing = registry
        .workspaces
        .iter()
        .find(|w| w.path == id)
        .and_then(|w| w.host_port);

    let port = choose_host_port(existing)?;

    // `changed` means the registry was mutated, which differs from
    // `existing != Some(port)` when `id` has no record.
    let mut changed = false;
    if existing != Some(port)
        && let Some(record) = registry.workspaces.iter_mut().find(|w| w.path == id)
    {
        record.host_port = Some(port);
        changed = true;
    }

    Ok((port, changed))
}

/// `~/.local/share/gavmo/workspaces.json` (or platform equivalent).
fn registry_path() -> io::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?
        .join("gavmo");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("workspaces.json"))
}

fn load_registry(path: &Path) -> io::Result<Registry> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(e),
    }
}

fn save_registry(path: &Path, registry: &Registry) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(registry)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// systemd's SGR-colored `[  OK  ]` lines with `\r\n` come out as plain text.
    #[test]
    fn sanitize_console_strips_systemd_sgr_color_sequences() {
        let raw =
            "[\u{1b}[0;32m  OK  \u{1b}[0m] Started \u{1b}[0;1;39mJournal Service\u{1b}[0m.\r\n";
        assert_eq!(
            sanitize_console(raw.as_bytes()),
            "[  OK  ] Started Journal Service.\n"
        );
    }

    /// OSC sequences are stripped whether terminated by BEL or ST (`ESC \`).
    #[test]
    fn sanitize_console_strips_osc_sequences() {
        let bel_terminated = "before\u{1b}]0;some title\u{7}after\n";
        assert_eq!(sanitize_console(bel_terminated.as_bytes()), "beforeafter\n");

        let st_terminated = "before\u{1b}]0;some title\u{1b}\\after\n";
        assert_eq!(sanitize_console(st_terminated.as_bytes()), "beforeafter\n");
    }

    /// DCS sequences (`ESC P ... ST`, as GRUB emits) are stripped entirely.
    #[test]
    fn sanitize_console_strips_dcs_sequences() {
        assert_eq!(
            sanitize_console("before\u{1b}P+q6E616D65\u{1b}\\after\n".as_bytes()),
            "beforeafter\n"
        );
    }

    /// Other two-byte escapes (`ESC c`, `ESC 7`, `ESC 8`) are swallowed cleanly.
    #[test]
    fn sanitize_console_strips_other_two_byte_escapes() {
        assert_eq!(
            sanitize_console("a\u{1b}cb\u{1b}7c\u{1b}8d".as_bytes()),
            "abcd"
        );
    }

    /// A bare `\r` becomes a line break, as does `\r\n`.
    #[test]
    fn sanitize_console_treats_bare_cr_as_a_line_break() {
        assert_eq!(sanitize_console(b"one\rtwo\r\nthree"), "one\ntwo\nthree");
    }

    /// Plain text passes through unchanged.
    #[test]
    fn sanitize_console_passes_plain_text_through() {
        assert_eq!(
            sanitize_console(b"Booting from Hard Disk...\n"),
            "Booting from Hard Disk...\n"
        );
    }

    /// URL-safe base64 without padding, matching opencode's encoding;
    /// independent of the constant so the test catches drift.
    fn base64_url_nopad(input: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
            let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 0x3F) as usize] as char);
            }
        }
        out
    }

    /// `OPENCODE_WORKSPACE_SESSION_PATH` equals base64url("/workspace") + "/session".
    #[test]
    fn opencode_workspace_session_path_matches_base64url_workspace() {
        let expected = format!("/{}/session", base64_url_nopad("/workspace"));
        assert_eq!(OPENCODE_WORKSPACE_SESSION_PATH, expected);
    }

    #[test]
    fn to_status_running_appends_opencode_workspace_session_path() {
        let status = to_status(VmStatus::Running {
            url: "http://127.0.0.1:4096".to_string(),
        });
        assert_eq!(
            status,
            WorkspaceStatus::Running {
                url: "http://127.0.0.1:4096/L3dvcmtzcGFjZQ/session".to_string(),
            }
        );
    }

    #[test]
    fn to_status_maps_rebuild_failed_to_build_failed() {
        let err = VmError::RebuildFailed {
            log_file: "rebuild-2026-01-01T00-00-00Z.log".to_string(),
        };
        // Mirrors the boot task's error mapping, which needs a real VM to test.
        let slot = match &err {
            VmError::RebuildFailed { log_file } => VmSlot::BuildFailed {
                log: log_file.clone(),
            },
            _ => VmSlot::Failed(err.to_string()),
        };
        assert!(matches!(
            slot,
            VmSlot::BuildFailed { log } if log == "rebuild-2026-01-01T00-00-00Z.log"
        ));
    }

    /// Only well-formed rebuild log names are accepted; traversal attempts are rejected.
    #[test]
    fn rebuild_log_filename_safety() {
        // Shaped exactly like what `gavmo-rebuild` writes.
        assert!(is_safe_rebuild_log_filename(
            "rebuild-2026-10-13T10-10-32Z.log"
        ));

        // Path traversal / escape attempts.
        assert!(!is_safe_rebuild_log_filename("../../../etc/passwd"));
        assert!(!is_safe_rebuild_log_filename("rebuild-../../secret.log"));
        assert!(!is_safe_rebuild_log_filename("rebuild-foo/../bar.log"));
        assert!(!is_safe_rebuild_log_filename("/etc/gavmo/system/flake.nix"));
        assert!(!is_safe_rebuild_log_filename("rebuild-foo\\bar.log"));

        // Wrong shape entirely.
        assert!(!is_safe_rebuild_log_filename("not-a-rebuild-log.txt"));
        assert!(!is_safe_rebuild_log_filename("rebuild-no-extension"));
        assert!(!is_safe_rebuild_log_filename(""));
        assert!(!is_safe_rebuild_log_filename("rebuild-.log"));
        assert!(!is_safe_rebuild_log_filename("rebuild-x.LOG"));

        // Characters a denylist would miss.
        assert!(!is_safe_rebuild_log_filename("rebuild-a\0b.log"));
        assert!(!is_safe_rebuild_log_filename("rebuild-a\nb.log"));
        assert!(!is_safe_rebuild_log_filename("rebuild-a.b.log"));
        assert!(!is_safe_rebuild_log_filename("rebuild-a\u{2215}b.log"));
    }

    /// With no persisted port, a fresh one is allocated.
    #[test]
    fn choose_host_port_allocates_fresh_when_none_persisted() {
        let port = choose_host_port(None).expect("allocation should succeed");
        assert!(port > 0);
    }

    /// A persisted port that is still free is reused as-is.
    #[test]
    fn choose_host_port_reuses_a_free_persisted_port() {
        // Use a real free port rather than hardcoding one that might collide.
        let persisted = alloc_port().unwrap();

        assert_eq!(choose_host_port(Some(persisted)).unwrap(), persisted);
    }

    /// An occupied persisted port falls back to a fresh allocation.
    #[test]
    fn choose_host_port_falls_back_when_persisted_port_is_taken() {
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let chosen = choose_host_port(Some(occupied_port)).expect("should fall back, not error");

        assert_ne!(
            chosen, occupied_port,
            "must not choose a port that's actually in use"
        );
        drop(occupied);
    }

    /// A new workspace gets a port written into its record, reported as `changed`.
    #[test]
    fn resolve_host_port_in_persists_a_fresh_port_for_a_new_workspace() {
        let mut registry = Registry {
            workspaces: vec![WorkspaceRecord {
                path: "/workspace/one".to_string(),
                host_port: None,
            }],
        };

        let (port, changed) = resolve_host_port_in(&mut registry, "/workspace/one").unwrap();

        assert!(changed, "a newly-chosen port must be reported as a change");
        assert_eq!(
            registry.workspaces[0].host_port,
            Some(port),
            "the chosen port must be written back into the matching record"
        );
    }

    /// A still-free persisted port is reused with `changed = false`.
    #[test]
    fn resolve_host_port_in_is_a_noop_when_the_persisted_port_is_still_free() {
        let persisted = alloc_port().unwrap();
        let mut registry = Registry {
            workspaces: vec![WorkspaceRecord {
                path: "/workspace/two".to_string(),
                host_port: Some(persisted),
            }],
        };

        let (port, changed) = resolve_host_port_in(&mut registry, "/workspace/two").unwrap();

        assert_eq!(port, persisted);
        assert!(
            !changed,
            "reusing the same port must not be reported as a change"
        );
        assert_eq!(registry.workspaces[0].host_port, Some(persisted));
    }

    /// An occupied persisted port is replaced and the record overwritten.
    #[test]
    fn resolve_host_port_in_overwrites_a_persisted_port_that_is_now_taken() {
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let mut registry = Registry {
            workspaces: vec![WorkspaceRecord {
                path: "/workspace/three".to_string(),
                host_port: Some(occupied_port),
            }],
        };

        let (port, changed) = resolve_host_port_in(&mut registry, "/workspace/three").unwrap();

        assert_ne!(port, occupied_port);
        assert!(changed);
        assert_eq!(registry.workspaces[0].host_port, Some(port));
        drop(occupied);
    }

    /// Resolving one workspace's port leaves other records untouched.
    #[test]
    fn resolve_host_port_in_only_touches_the_matching_workspace() {
        let mut registry = Registry {
            workspaces: vec![
                WorkspaceRecord {
                    path: "/workspace/a".to_string(),
                    host_port: Some(4096),
                },
                WorkspaceRecord {
                    path: "/workspace/b".to_string(),
                    host_port: None,
                },
            ],
        };

        resolve_host_port_in(&mut registry, "/workspace/b").unwrap();

        assert_eq!(
            registry.workspaces[0].host_port,
            Some(4096),
            "the untouched workspace's persisted port must be left alone"
        );
        assert!(registry.workspaces[1].host_port.is_some());
    }

    /// An unknown id still gets a usable port, with `changed = false`.
    #[test]
    fn resolve_host_port_in_degrades_gracefully_for_an_unknown_workspace() {
        let mut registry = Registry { workspaces: vec![] };

        let (port, changed) = resolve_host_port_in(&mut registry, "/not/registered").unwrap();

        assert!(port > 0);
        assert!(!changed);
        assert!(registry.workspaces.is_empty());
    }

    /// A not-yet-created path `<tempdir>/<tag>`. Keep the returned `TempDir`
    /// alive for the test: dropping it deletes the whole tree, even on failure.
    fn unique_temp_dir(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(tag);
        (tmp, path)
    }

    /// A populated state dir is deleted along with its contents.
    #[tokio::test]
    async fn remove_dir_all_idempotent_deletes_an_existing_tree() {
        let (_tmp, dir) = unique_temp_dir("reset-exists");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("disk.qcow2"), b"fake overlay")
            .await
            .unwrap();

        remove_dir_all_idempotent(&dir).await.unwrap();

        assert!(!dir.exists());
    }

    /// Deleting a missing dir is `Ok(())`, not `NotFound`.
    #[tokio::test]
    async fn remove_dir_all_idempotent_is_a_noop_when_already_gone() {
        let (_tmp, dir) = unique_temp_dir("reset-missing");
        assert!(!dir.exists());

        remove_dir_all_idempotent(&dir).await.unwrap();
    }

    /// A throwaway [`AppState`] with placeholder image paths. Safe because
    /// `AppState::new` only reads the real registry; tests using it must not
    /// call methods that save it (e.g. `add_workspace`).
    fn test_app_state() -> AppState {
        AppState::new(
            PathBuf::from("/nonexistent/gavmo-test-base.qcow2"),
            PathBuf::from("/nonexistent/gavmo-test-gcloud"),
        )
        .expect("AppState::new only touches the real registry file, which is harmless to read")
    }

    /// A `Booting` slot whose task never finishes in test time, standing in
    /// for an in-flight boot.
    fn never_finishing_boot_slot() -> VmSlot {
        VmSlot::Booting(tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(600)).await;
        }))
    }

    /// `stop_workspace` aborts an in-flight boot and returns promptly
    /// (bounded by a timeout so a regression fails instead of hanging).
    #[tokio::test]
    async fn stop_workspace_aborts_an_in_flight_boot_promptly() {
        let state = test_app_state();
        let id = "stop-workspace-aborts-an-in-flight-boot-promptly";
        state
            .inner
            .running
            .lock()
            .await
            .insert(id.to_string(), never_finishing_boot_slot());

        tokio::time::timeout(Duration::from_secs(5), state.stop_workspace(id))
            .await
            .expect("stop_workspace should abort the in-flight boot promptly, not wait for it")
            .unwrap();

        assert!(state.inner.running.lock().await.get(id).is_none());
    }

    /// `reset_workspace` aborts an in-flight boot promptly, then deletes the state dir.
    #[tokio::test]
    async fn reset_workspace_aborts_an_in_flight_boot_before_deleting_state() {
        let (_tmp, repo_dir) = unique_temp_dir("reset-abort-repo");
        let state_dir = repo_dir.join(".gavmo").join("vm");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("disk.qcow2"), b"fake overlay")
            .await
            .unwrap();
        let id = repo_dir.to_string_lossy().into_owned();

        let state = test_app_state();
        state
            .inner
            .running
            .lock()
            .await
            .insert(id.clone(), never_finishing_boot_slot());

        tokio::time::timeout(Duration::from_secs(5), state.reset_workspace(&id))
            .await
            .expect("reset_workspace should abort the in-flight boot promptly, not wait for it")
            .unwrap();

        assert!(!state_dir.exists());
        assert!(state.inner.running.lock().await.get(&id).is_none());
    }

    /// Share dir names are deterministic, hex-only, and distinct per workspace.
    #[test]
    fn mcp_share_dir_name_is_deterministic_and_filesystem_safe() {
        let a = mcp_share_dir_name("/home/user/project-a");
        let b = mcp_share_dir_name("/home/user/project-b");

        assert_eq!(a, mcp_share_dir_name("/home/user/project-a"));
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    /// Long workspace paths must still give a usable (<= 255 byte) name.
    #[test]
    fn mcp_share_dir_name_has_fixed_length_for_long_paths() {
        let long = format!("/home/user/{}", "d/".repeat(200));
        assert_eq!(mcp_share_dir_name(&long).len(), 64);
        assert_eq!(mcp_share_dir_name("/").len(), 64);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_owner_only_creates_and_resets_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.env");

        write_owner_only(&path, b"a").await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // A pre-existing, wider file is narrowed and overwritten.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_owner_only(&path, b"b").await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"b");
    }

    /// Minting writes `token.env` and the token resolves to the workspace.
    #[tokio::test]
    async fn mint_mcp_token_writes_env_file_and_resolves_via_token_authority() {
        let state = test_app_state();
        let id = "mint-mcp-token-writes-env-file-and-resolves";
        let (_tmp, dir) = unique_temp_dir("mcp-token-mint");

        state.mint_mcp_token(id, &dir).await.unwrap();

        let contents = tokio::fs::read_to_string(dir.join("token.env"))
            .await
            .unwrap();
        let token = contents
            .trim()
            .strip_prefix("GAVMO_MCP_TOKEN=")
            .expect("token.env should contain GAVMO_MCP_TOKEN=<token>")
            .to_string();
        assert!(!token.is_empty());

        assert_eq!(
            TokenAuthority::resolve(&state, &token),
            Some(WorkspaceScope {
                workspace_id: id.to_string()
            })
        );
    }

    /// Re-minting invalidates the workspace's previous token.
    #[tokio::test]
    async fn minting_a_new_token_invalidates_the_previous_one_for_the_same_workspace() {
        let state = test_app_state();
        let id = "minting-a-new-token-invalidates-the-previous-one";
        let (_tmp, dir) = unique_temp_dir("mcp-token-remint");

        state.mint_mcp_token(id, &dir).await.unwrap();
        let old_token = {
            let tokens = state.inner.mcp_tokens.lock().unwrap();
            tokens
                .iter()
                .find(|(_, v)| *v == id)
                .map(|(k, _)| k.clone())
                .unwrap()
        };

        state.mint_mcp_token(id, &dir).await.unwrap();

        assert_eq!(TokenAuthority::resolve(&state, &old_token), None);
    }

    /// `clear_mcp_token` makes the workspace's token stop resolving.
    #[tokio::test]
    async fn clear_mcp_token_invalidates_resolution() {
        let state = test_app_state();
        let id = "clear-mcp-token-invalidates-resolution";
        let (_tmp, dir) = unique_temp_dir("mcp-token-clear");

        state.mint_mcp_token(id, &dir).await.unwrap();
        let token = {
            let tokens = state.inner.mcp_tokens.lock().unwrap();
            tokens
                .iter()
                .find(|(_, v)| *v == id)
                .map(|(k, _)| k.clone())
                .unwrap()
        };
        assert!(TokenAuthority::resolve(&state, &token).is_some());

        state.clear_mcp_token(id);

        assert_eq!(TokenAuthority::resolve(&state, &token), None);
    }

    /// An unknown token resolves to nothing.
    #[test]
    fn resolve_returns_none_for_an_unknown_token() {
        let state = test_app_state();
        assert_eq!(TokenAuthority::resolve(&state, "never-minted"), None);
    }
}
