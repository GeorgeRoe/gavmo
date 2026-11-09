//! Server-side (ssr-only) state for the GAVMO daemon.
//!
//! Holds the persistent workspace registry (a JSON file under a global app
//! dir) and the in-memory map of currently-running VMs. This module is the
//! only place that touches the `vm` crate, keeping all VM-management
//! mechanism behind the daemon's server functions.

mod base_image;
mod error;
mod lifecycle;
mod logs;
mod paths;
mod ports;
mod resources;
mod secret_store;
mod stats;
mod store;
#[cfg(test)]
mod tests;
mod tools;
mod util;

pub use self::error::RegistryError;
use self::error::{Result, io_ctx};

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use secrets::SecretStore;

use self::base_image::BaseImage;
use self::lifecycle::{VmSlot, reap_stale_vm_processes, slot_status};
use self::paths::{
    canonical_or_self, check_workspace_path, default_runtime_base, warn_about_legacy_vm_state,
};
use self::store::{Registry, WorkspaceRecord, commit_registry, load_registry};
use crate::integrations::ResolvedIntegration;
use crate::types::{WorkspaceInfo, WorkspaceStatus};
use crate::vm_handle::{Launcher, QemuLauncher};

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
    /// The read-only base image and its cached identity.
    base_image: BaseImage,
    gcloud_dir: PathBuf,

    /// Root dir for every workspace's MCP token share
    /// (`~/.local/share/gavmo/mcp`), deliberately outside any repo.
    mcp_base_dir: PathBuf,

    /// Root of every workspace's host-only VM state (`~/.local/share/gavmo/vms`):
    /// disk and console log, outside the repo the guest can write (see
    /// [`vm::VmConfig`]). A sibling of `mcp_base_dir`, never inside a share.
    vms_base_dir: PathBuf,
    /// Root of every workspace's VM sockets (see [`default_runtime_base`]).
    runtime_base_dir: PathBuf,

    /// Host dirs no workspace may equal, contain, or sit inside (see
    /// [`check_workspace_path`]): the data dir (UI token, vault, other
    /// workspaces' MCP tokens) and the gcloud dir.
    protected_dirs: Vec<PathBuf>,
    /// The user's home dir, which no workspace may equal or contain.
    home_dir: Option<PathBuf>,

    /// Index: bearer token -> workspace id, mirroring the tokens held in
    /// `running`. A `std::sync::Mutex` because `TokenAuthority::resolve` is
    /// synchronous; never held across an `.await`. Only [`AppState::replace_slot`]
    /// writes it.
    mcp_tokens: std::sync::Mutex<HashMap<String, String>>,

    /// Host secret store backing the tool integrations; an encrypted vault
    /// opened lazily on first access.
    secret_store: Arc<dyn SecretStore>,

    /// Index: workspace id -> integrations of its `Running` slot. Only
    /// [`AppState::replace_slot`] writes it; `std::sync::Mutex` since
    /// `list_tools` is sync.
    workspace_integrations: std::sync::Mutex<HashMap<String, Arc<[ResolvedIntegration]>>>,

    /// Per-workspace lifecycle locks: open, stop, reset and remove of one
    /// workspace run one at a time (open refuses rather than waits).
    lifecycle: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Set by [`AppState::shutdown`]; refuses new boots.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Source of [`VmSlot::Booting::generation`].
    boot_generation: std::sync::atomic::AtomicU64,
    /// Boots VMs (qemu in production, a fake in tests).
    launcher: Arc<dyn Launcher>,

    /// Shared HTTP client for the tool forwarder; clones share one
    /// connection pool across all workspaces.
    http_client: reqwest::Client,
}

impl AppState {
    /// Builds the app state, loading the persisted registry if present.
    ///
    /// `base_image` is the read-only NixOS qcow2 (from `GAVMO_BASE_IMAGE`);
    /// `gcloud_dir` is the ADC config dir mounted read-only into every VM.
    pub fn new(base_image: PathBuf, gcloud_dir: PathBuf) -> io::Result<Self> {
        let gavmo_data_dir = dirs::data_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?
            .join("gavmo");
        // Opened lazily so a host with a broken/absent OS keystore can still
        // run the daemon and manage VMs; only secret operations fail (closed).
        // There is no plaintext fallback.
        let secret_store: Arc<dyn SecretStore> = Arc::new(secrets::LazyVaultStore::keyring_backed(
            gavmo_data_dir.join("secrets.vault"),
        ));
        Self::with_data_dir(
            base_image,
            gcloud_dir,
            &gavmo_data_dir,
            dirs::home_dir().as_deref(),
            &default_runtime_base()?,
            secret_store,
            Arc::new(QemuLauncher),
        )
    }

    /// [`Self::new`] with an explicit data dir and secret store, so tests
    /// never touch the user's real `~/.local/share/gavmo` or keyring.
    pub(crate) fn with_data_dir(
        base_image: PathBuf,
        gcloud_dir: PathBuf,
        gavmo_data_dir: &Path,
        home_dir: Option<&Path>,
        runtime_base_dir: &Path,
        secret_store: Arc<dyn SecretStore>,
        launcher: Arc<dyn Launcher>,
    ) -> io::Result<Self> {
        std::fs::create_dir_all(gavmo_data_dir)?;
        vm::create_private_dir_all(runtime_base_dir).map_err(io::Error::other)?;
        reap_stale_vm_processes(runtime_base_dir);
        let protected_dirs = [gavmo_data_dir, gcloud_dir.as_path(), runtime_base_dir]
            .into_iter()
            .map(canonical_or_self)
            .collect();
        let home_dir = home_dir.map(canonical_or_self);
        let registry_path = gavmo_data_dir.join("workspaces.json");
        let registry = load_registry(&registry_path)?;
        let mcp_base_dir = gavmo_data_dir.join("mcp");
        let vms_base_dir = gavmo_data_dir.join("vms");
        for workspace in &registry.workspaces {
            warn_about_legacy_vm_state(&workspace.path);
        }

        Ok(Self {
            inner: Arc::new(Inner {
                registry_path,
                registry: Mutex::new(registry),
                running: Mutex::new(HashMap::new()),
                base_image: BaseImage::new(base_image, gavmo_data_dir.join("base-image.json")),
                gcloud_dir,
                mcp_base_dir,
                vms_base_dir,
                runtime_base_dir: runtime_base_dir.to_path_buf(),
                protected_dirs,
                home_dir,
                mcp_tokens: std::sync::Mutex::new(HashMap::new()),
                secret_store,
                workspace_integrations: std::sync::Mutex::new(HashMap::new()),
                lifecycle: std::sync::Mutex::new(HashMap::new()),
                boot_generation: std::sync::atomic::AtomicU64::new(0),
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                launcher,
                http_client: crate::integrations::forwarding_client().map_err(io::Error::other)?,
            }),
        })
    }

    /// All known workspaces plus each one's current status.
    pub async fn list(&self) -> Vec<WorkspaceInfo> {
        // Both locks are held only for in-memory reads: statuses are published
        // by each VM's monitor, so nothing here waits on qemu.
        let registry = self.inner.registry.lock().await;
        let running = self.inner.running.lock().await;
        registry
            .workspaces
            .iter()
            .map(|rec| WorkspaceInfo {
                id: rec.path.clone(),
                path: rec.path.clone(),
                status: running
                    .get(&rec.path)
                    .map_or(WorkspaceStatus::Stopped, slot_status),
            })
            .collect()
    }

    /// Registers a new workspace by absolute host path. Idempotent: adding
    /// a path that is already known is a no-op.
    ///
    /// The directory is later mounted **read-write** into a VM, so paths that
    /// would expose the daemon's own secrets or the user's whole home are
    /// refused (see [`check_workspace_path`]).
    pub async fn add_workspace(&self, path: String) -> Result<()> {
        // Canonical: symlinks resolved, so the checks see the real target.
        let canonical = tokio::fs::canonicalize(&path)
            .await
            .map_err(|e| RegistryError::InvalidInput(format!("{path}: {e}")))?;
        let meta = tokio::fs::metadata(&canonical)
            .await
            .map_err(io_ctx(format!("reading {}", canonical.display())))?;
        if !meta.is_dir() {
            return Err(RegistryError::InvalidInput(format!(
                "{} is not a directory",
                canonical.display()
            )));
        }
        check_workspace_path(
            &canonical,
            &self.inner.protected_dirs,
            self.inner.home_dir.as_deref(),
        )
        .map_err(RegistryError::InvalidInput)?;
        // Ids are UTF-8 strings; a lossy conversion would register a path
        // that doesn't exist.
        let canonical = canonical.into_os_string().into_string().map_err(|p| {
            RegistryError::InvalidInput(format!("{} is not valid UTF-8", Path::new(&p).display()))
        })?;

        let mut registry = self.inner.registry.lock().await;
        if !registry.workspaces.iter().any(|w| w.path == canonical) {
            warn_about_legacy_vm_state(&canonical);
            let mut updated = registry.clone();
            updated.workspaces.push(WorkspaceRecord {
                path: canonical,
                host_port: None,
                resources: crate::types::Resources::default(),
            });
            commit_registry(&mut registry, &self.inner.registry_path, updated)
                .await
                .map_err(io_ctx("saving the workspace registry"))?;
        }
        Ok(())
    }

    /// Whether `id` is exactly the id of a registered workspace.
    ///
    /// Ids arrive from the browser and double as host paths (`<id>/.gavmo/vm`
    /// is deleted on reset, `<id>` is mounted read-write into a VM on open),
    /// so every entry point that touches the filesystem or boots a VM must
    /// check this first. Exact match against the stored canonical path also
    /// rules out aliases such as `/a/../a` or a trailing slash.
    async fn is_known(&self, id: &str) -> bool {
        self.inner
            .registry
            .lock()
            .await
            .workspaces
            .iter()
            .any(|w| w.path == id)
    }

    /// [`Self::is_known`] as a `Result` for `?`.
    async fn require_known(&self, id: &str) -> Result<()> {
        if self.is_known(id).await {
            Ok(())
        } else {
            Err(RegistryError::UnknownWorkspace(id.to_owned()))
        }
    }

    /// Current status of a single workspace.
    pub async fn status(&self, id: &str) -> WorkspaceStatus {
        if !self.is_known(id).await {
            return WorkspaceStatus::Stopped;
        }
        self.inner
            .running
            .lock()
            .await
            .get(id)
            .map_or(WorkspaceStatus::Stopped, slot_status)
    }
}
