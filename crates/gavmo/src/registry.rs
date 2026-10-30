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
use vm::{Error as VmError, VmConfig, VmStatus};

use mcp::{TokenAuthority, ToolDescriptor, ToolExecutor, ToolOutcome, WorkspaceScope};
use secrets::SecretStore;

use crate::app::{ToolConfig, VmStats, WorkspaceInfo, WorkspaceStatus};
use crate::integrations::{BindingState, CandidateTool, Credential, Delivery, ResolvedIntegration};
use crate::vm_handle::{Launcher, QemuLauncher, VmHandle, VmInfo};

/// Default guest memory, in MiB.
const DEFAULT_MEMORY_MIB: u32 = 4096;
/// Default guest vCPU count.
const DEFAULT_VCPUS: u32 = 4;

/// How often a running VM's monitor probes it (QMP + HTTP).
const MONITOR_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(2)
};

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

/// The lifecycle state of a workspace's VM. Absence from the map means
/// stopped.
///
/// The MCP token and resolved integrations live *in* the slot, and the
/// lookup indexes `mcp_tokens` / `workspace_integrations` are only ever
/// updated together with the slot (see [`AppState::replace_slot`]), under
/// the `running` lock. A workspace's credentials are therefore valid exactly
/// while it is `Booting` (token only) or `Running`; every other transition
/// revokes them in the same step.
enum VmSlot {
    /// A boot task is in flight. Stop/reset abort it; since qemu's `Child`
    /// is `kill_on_drop`, awaiting the aborted handle guarantees qemu is
    /// gone. `generation` identifies this particular boot, so a finishing
    /// task can tell whether it is still the one wanted.
    Booting {
        task: tokio::task::JoinHandle<()>,
        generation: u64,
        token: String,
    },
    /// Booted and (as of the last check) alive.
    Running(Box<RunningSlot>),
    /// A stop, reset or remove is tearing the VM down. Credentials are
    /// already revoked; a finishing boot task must not resurrect it.
    Stopping,
    /// The last boot attempt failed, or the VM died; kept so the reason
    /// stays visible until the next `open_workspace`.
    Failed(String),
    /// The repo's `gavmo.nix` failed to build (detected early via the
    /// guest's failure marker). `log` is the log filename relative to the
    /// workspace's `.gavmo/vm` dir, readable via [`AppState::read_build_log`].
    BuildFailed { log: String },
}

/// A booted VM, watched by a background monitor task.
///
/// The VM handle sits behind its own lock, which only the monitor (while
/// probing) and a teardown (to take it) ever hold - never the `running`
/// lock. Readers get the latest status from `status` without waiting on
/// qemu, and the stats panel uses the `info` snapshot.
struct RunningSlot {
    vm: Arc<Mutex<Option<Box<dyn VmHandle>>>>,
    status: tokio::sync::watch::Receiver<WorkspaceStatus>,
    monitor: tokio::task::JoinHandle<()>,
    info: VmInfo,
    /// When `info` was taken, so uptime can be derived later.
    info_taken: std::time::Instant,
    token: String,
    integrations: Arc<[ResolvedIntegration]>,
}

impl RunningSlot {
    /// Stops monitoring and takes the VM out for a stop. Aborting cancels a
    /// probe in flight (releasing the VM lock) at its next await point.
    async fn take_vm(&self) -> Option<Box<dyn VmHandle>> {
        self.monitor.abort();
        self.vm.lock().await.take()
    }
}

impl Drop for RunningSlot {
    /// A slot dropped without a teardown (e.g. at shutdown) must not leave
    /// its monitor running.
    fn drop(&mut self) {
        self.monitor.abort();
    }
}

impl VmSlot {
    fn token(&self) -> Option<&str> {
        match self {
            Self::Booting { token, .. } => Some(token),
            Self::Running(running) => Some(&running.token),
            Self::Stopping | Self::Failed(_) | Self::BuildFailed { .. } => None,
        }
    }

    /// Whether a VM (or its boot) is live.
    fn is_active(&self) -> bool {
        matches!(self, Self::Booting { .. } | Self::Running(_))
    }
}

/// What a teardown found in the slot it replaced with `Stopping`.
enum Teardown {
    Vm(Box<RunningSlot>),
    Boot(tokio::task::JoinHandle<()>),
    Nothing,
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
    fn with_data_dir(
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
                base_image,
                gcloud_dir,
                memory_mib: DEFAULT_MEMORY_MIB,
                vcpus: DEFAULT_VCPUS,
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
    pub async fn add_workspace(&self, path: String) -> io::Result<()> {
        // Canonical: symlinks resolved, so the checks see the real target.
        let canonical = tokio::fs::canonicalize(&path).await?;
        if !tokio::fs::metadata(&canonical).await?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", canonical.display()),
            ));
        }
        check_workspace_path(
            &canonical,
            &self.inner.protected_dirs,
            self.inner.home_dir.as_deref(),
        )
        .map_err(|reason| io::Error::new(io::ErrorKind::InvalidInput, reason))?;
        // Ids are UTF-8 strings; a lossy conversion would register a path
        // that doesn't exist.
        let canonical = canonical.into_os_string().into_string().map_err(|p| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not valid UTF-8", Path::new(&p).display()),
            )
        })?;

        let mut registry = self.inner.registry.lock().await;
        if !registry.workspaces.iter().any(|w| w.path == canonical) {
            warn_about_legacy_vm_state(&canonical);
            registry.workspaces.push(WorkspaceRecord {
                path: canonical,
                host_port: None,
            });
            save_registry(&self.inner.registry_path, &registry).await?;
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
    async fn require_known(&self, id: &str) -> Result<(), String> {
        if self.is_known(id).await {
            Ok(())
        } else {
            Err(format!("unknown workspace: {id}"))
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

    // --- Lifecycle ---
    //
    // Invariants:
    // - open/stop/reset/remove of one workspace hold its lifecycle lock for
    //   their whole duration, so they never interleave (open refuses instead
    //   of waiting, so a click during a slow stop errors instead of hanging);
    // - every slot change goes through `replace_slot` under the `running`
    //   lock, which keeps credentials in step with the slot;
    // - the `running` lock is never held across a VM stop or a boot.

    /// The lifecycle lock for `id`, created on first use.
    fn lifecycle_lock(&self, id: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            lock_unpoisoned(&self.inner.lifecycle)
                .entry(id.to_owned())
                .or_default(),
        )
    }

    /// Replaces (`Some`) or removes (`None`) `id`'s slot, updating the token
    /// and integration indexes in the same step. Callers hold the `running`
    /// lock (they pass its map), so no reader sees the two disagree.
    fn replace_slot(
        &self,
        running: &mut HashMap<String, VmSlot>,
        id: &str,
        slot: Option<VmSlot>,
    ) -> Option<VmSlot> {
        let new_token = slot.as_ref().and_then(VmSlot::token).map(str::to_owned);
        let new_integrations = match &slot {
            Some(VmSlot::Running(running)) => Some(Arc::clone(&running.integrations)),
            _ => None,
        };
        let old = match slot {
            Some(slot) => running.insert(id.to_owned(), slot),
            None => running.remove(id),
        };

        let mut tokens = lock_unpoisoned(&self.inner.mcp_tokens);
        tokens.retain(|_, owner| owner != id);
        if let Some(token) = new_token {
            tokens.insert(token, id.to_owned());
        }
        drop(tokens);
        let mut integrations = lock_unpoisoned(&self.inner.workspace_integrations);
        match new_integrations {
            Some(list) => integrations.insert(id.to_owned(), list),
            None => integrations.remove(id),
        };
        old
    }

    /// Kicks off booting the VM for a workspace (or is a no-op if it's
    /// already booting/running) and returns immediately without waiting for
    /// the boot. Callers poll `status`/`list` to observe the transition.
    ///
    /// Refused while another lifecycle operation (stop, reset, remove) on the
    /// same workspace is in progress.
    pub async fn open_workspace(&self, id: &str) -> Result<(), String> {
        // Otherwise any host directory could be booted with `id` mounted
        // read-write into the guest.
        self.require_known(id).await?;
        if self
            .inner
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err("GAVMO is shutting down".to_owned());
        }
        let Ok(_lifecycle) = self.lifecycle_lock(id).try_lock_owned() else {
            return Err(
                "this workspace is busy (stopping, resetting or being removed); \
                        try again in a moment"
                    .to_owned(),
            );
        };
        // Re-checked under the lifecycle lock: a remove that just finished
        // has dropped it from the registry.
        self.require_known(id).await?;

        // Bail early if already up: our own live VM holds the persisted port,
        // so `resolve_host_port` would see it as taken and needlessly
        // reallocate it. Nothing else can start or stop it meanwhile.
        if self
            .inner
            .running
            .lock()
            .await
            .get(id)
            .is_some_and(VmSlot::is_active)
        {
            return Ok(());
        }

        // Takes only the `registry` lock, not nested under `running`: lock
        // order is registry then running (as in `list`).
        let host_port = self
            .resolve_host_port(id)
            .await
            .map_err(|e| format!("failed to resolve a host port: {e}"))?;

        // Mint the per-boot MCP token only on a real new boot: rotating a
        // running VM's token would break its already-loaded `gavmo-opencode`.
        let mcp_dir = self.mcp_dir_for(id);
        let token = match mint_mcp_token(&mcp_dir).await {
            Ok(token) => token,
            Err(e) => {
                let reason = format!("failed to write MCP token: {e}");
                let mut running = self.inner.running.lock().await;
                self.replace_slot(&mut running, id, Some(VmSlot::Failed(reason.clone())));
                return Err(reason);
            }
        };

        let cfg = self.vm_config(id, mcp_dir, host_port);
        let generation = self
            .inner
            .boot_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Held across spawn + insert, so the task (which needs this lock to
        // publish its result) can't finish before its `Booting` slot exists.
        let mut running = self.inner.running.lock().await;
        let state = self.clone();
        let task_id = id.to_owned();
        let task_token = token.clone();
        let boot = self.inner.launcher.boot(cfg);
        let task = tokio::spawn(async move {
            let result = boot.await;
            state
                .finish_boot(&task_id, generation, task_token, result)
                .await;
        });
        self.replace_slot(
            &mut running,
            id,
            Some(VmSlot::Booting {
                task,
                generation,
                token,
            }),
        );
        Ok(())
    }

    /// Publishes a boot's outcome, but only if its `Booting` slot (same
    /// generation) is still there; otherwise a stop/reset has taken over and
    /// any VM that did boot is stopped here rather than leaked.
    async fn finish_boot(
        &self,
        id: &str,
        generation: u64,
        token: String,
        result: vm::Result<Box<dyn VmHandle>>,
    ) {
        // Resolved before taking the `running` lock so the keyring check
        // can't block `status`/`list`. A failed boot exposes no tools.
        let integrations: Arc<[ResolvedIntegration]> = if result.is_ok() {
            self.resolve_integrations_for(id).await.into()
        } else {
            Arc::new([])
        };

        let mut running = self.inner.running.lock().await;
        let still_wanted = matches!(
            running.get(id),
            Some(VmSlot::Booting { generation: g, .. }) if *g == generation
        );
        match result {
            Ok(vm) if still_wanted => {
                let info = vm.info();
                // It just passed its health check, so it starts out Running.
                let (status_tx, status) =
                    tokio::sync::watch::channel(to_status(VmStatus::Running {
                        url: format!("http://127.0.0.1:{}", info.host_port),
                    }));
                let vm = Arc::new(Mutex::new(Some(vm)));
                let monitor = self.spawn_monitor(id.to_owned(), Arc::clone(&vm), status_tx);
                self.replace_slot(
                    &mut running,
                    id,
                    Some(VmSlot::Running(Box::new(RunningSlot {
                        vm,
                        status,
                        monitor,
                        info,
                        info_taken: std::time::Instant::now(),
                        token,
                        integrations,
                    }))),
                );
            }
            Ok(vm) => {
                drop(running);
                stop_vm_logged(id, vm, "superseded").await;
            }
            Err(e) if still_wanted => {
                self.replace_slot(&mut running, id, Some(slot_for_boot_error(&e)));
            }
            Err(_) => {}
        }
    }

    /// Marks `id`'s slot `Stopping` (revoking its credentials at once) and
    /// returns what has to be torn down. The caller must hold `id`'s
    /// lifecycle lock and must later replace the `Stopping` slot.
    async fn begin_teardown(&self, id: &str) -> Teardown {
        let mut running = self.inner.running.lock().await;
        if !running.get(id).is_some_and(VmSlot::is_active) {
            return Teardown::Nothing;
        }
        match self.replace_slot(&mut running, id, Some(VmSlot::Stopping)) {
            Some(VmSlot::Running(slot)) => Teardown::Vm(slot),
            Some(VmSlot::Booting { task, .. }) => Teardown::Boot(task),
            _ => Teardown::Nothing,
        }
    }

    /// Carries out a [`Teardown`]: stops the VM, or aborts and awaits the boot
    /// task (so qemu is gone on return either way).
    async fn run_teardown(id: &str, teardown: Teardown) -> Result<(), String> {
        match teardown {
            Teardown::Vm(slot) => match slot.take_vm().await {
                Some(vm) => vm.stop().await.map_err(|e| e.to_string()),
                None => Ok(()),
            },
            Teardown::Boot(task) => {
                abort_boot_task(id, task).await;
                Ok(())
            }
            Teardown::Nothing => Ok(()),
        }
    }

    /// Replaces `id`'s slot with `slot` (or removes it), unless it changed
    /// from what a teardown left. Only `Stopping` (ours) or an inactive slot
    /// is replaced; under the lifecycle lock nothing else can appear.
    async fn end_teardown(&self, id: &str, slot: Option<VmSlot>) {
        let mut running = self.inner.running.lock().await;
        if !running.get(id).is_some_and(VmSlot::is_active) {
            self.replace_slot(&mut running, id, slot);
        }
    }

    /// Stops the VM for a workspace and drops it from the running map. A
    /// no-op if it isn't currently running or booting. Credentials are
    /// revoked before the (possibly slow) shutdown starts.
    ///
    /// An in-flight boot is aborted and awaited, so qemu is gone on return
    /// (killed on drop, or gracefully stopped by the boot task if it had
    /// just succeeded - rarely blocking up to ~35s).
    pub async fn stop_workspace(&self, id: &str) -> Result<(), String> {
        // Unknown ids with nothing running have nothing to stop (and get no
        // lifecycle lock entry).
        if !self.is_known(id).await && !self.inner.running.lock().await.contains_key(id) {
            return Ok(());
        }
        let lifecycle = self.lifecycle_lock(id);
        let _held = lifecycle.lock().await;
        let teardown = self.begin_teardown(id).await;
        let result = Self::run_teardown(id, teardown).await;
        self.end_teardown(id, None).await;
        result
    }

    /// Stops the VM and drops the workspace from the registry. Non-destructive:
    /// its VM state (`~/.local/share/gavmo/vms/<hash>`) and `.gavmo/` in the
    /// repo are kept, so re-adding the path restores its overlay and sessions.
    /// A no-op for unknown ids.
    ///
    /// The registry entry goes first, so no `open` can start meanwhile.
    pub async fn remove_workspace(&self, id: &str) -> Result<(), String> {
        let lifecycle = self.lifecycle_lock(id);
        let _held = lifecycle.lock().await;
        {
            let mut registry = self.inner.registry.lock().await;
            let before = registry.workspaces.len();
            registry.workspaces.retain(|w| w.path != id);
            if registry.workspaces.len() != before {
                save_registry(&self.inner.registry_path, &registry)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        let teardown = self.begin_teardown(id).await;
        let result = Self::run_teardown(id, teardown).await;
        self.end_teardown(id, None).await;
        result
    }

    /// Stops the VM and deletes its host state (overlay disk, console log),
    /// its sockets, and `<repo>/.gavmo/vm` (rebuild logs), so the next open
    /// builds a fresh overlay from the current base image - needed after a
    /// base-image rebuild, since an overlay only makes sense on the exact base
    /// it was created from.
    ///
    /// Destructive (the guest's `/nix` cache is lost; opencode sessions in
    /// `<repo>/.gavmo/opencode` are kept); the caller must have obtained user
    /// confirmation. An in-flight boot is aborted and awaited first, and the
    /// workspace stays `Stopping` (refusing opens) until the files are gone,
    /// so qemu can't be writing the overlay while it is deleted.
    pub async fn reset_workspace(&self, id: &str) -> io::Result<()> {
        // The state dir is derived from `id` and deleted below.
        self.require_known(id).await.map_err(unknown_workspace_io)?;
        let lifecycle = self.lifecycle_lock(id);
        let _held = lifecycle.lock().await;

        let teardown = self.begin_teardown(id).await;
        // The state dir is deleted regardless: qemu has been killed by `stop`
        // at the latest via SIGKILL, so a failure is only worth logging, not
        // blocking the reset the user confirmed.
        if let Err(e) = Self::run_teardown(id, teardown).await {
            tracing::warn!(workspace = %id, error = %e, "VM did not stop cleanly during reset");
        }

        // The next open builds a fresh overlay from the current base image.
        // The guest output dir goes too (stale rebuild logs, and any VM files
        // left there by GAVMO versions that kept state in the repo);
        // `remove_dir_all` never follows a symlink planted in its place.
        let result = async {
            remove_dir_all_idempotent(&self.host_state_dir_for(id)).await?;
            remove_dir_all_idempotent(&self.runtime_dir_for(id)).await?;
            remove_dir_all_idempotent(&Self::guest_output_dir_for(id)).await
        }
        .await;
        let after = match &result {
            Ok(()) => None,
            Err(e) => Some(VmSlot::Failed(format!(
                "reset could not delete VM state: {e}"
            ))),
        };
        self.end_teardown(id, after).await;
        result
    }

    /// Stops every VM, for daemon shutdown: refuses new boots, then stops all
    /// workspaces concurrently, waiting at most `limit` overall. Anything
    /// still running after that is left to the kernel: VM processes get
    /// `SIGTERM` when the daemon exits (`vm` crate, `PR_SET_PDEATHSIG`).
    pub async fn shutdown(&self, limit: Duration) {
        self.inner
            .shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let ids: Vec<String> = self.inner.running.lock().await.keys().cloned().collect();
        if ids.is_empty() {
            return;
        }
        tracing::info!("stopping {} VM(s) before exit", ids.len());
        let mut stops = tokio::task::JoinSet::new();
        for id in ids {
            let state = self.clone();
            stops.spawn(async move {
                if let Err(e) = state.stop_workspace(&id).await {
                    tracing::warn!(workspace = %id, error = %e, "VM did not stop cleanly at shutdown");
                }
            });
        }
        let all = async { while stops.join_next().await.is_some() {} };
        if tokio::time::timeout(limit, all).await.is_err() {
            tracing::warn!("timed out stopping VMs at shutdown; the kernel will signal the rest");
        }
    }

    /// Moves a `Running` slot whose VM has died to `Failed` (revoking its
    /// credentials, and letting `open` boot it again), and reaps the dead
    /// VM's helpers in the background. A no-op if the slot changed meanwhile
    /// or `id` is mid-lifecycle-operation (that operation handles it).
    ///
    /// Returns `false` only if it had to back off because a lifecycle
    /// operation holds the lock, so the caller should check again later.
    async fn mark_dead(&self, id: &str, reason: String) -> bool {
        let lifecycle = self.lifecycle_lock(id);
        let Ok(_held) = lifecycle.try_lock() else {
            return false;
        };
        let mut running = self.inner.running.lock().await;
        if !matches!(running.get(id), Some(VmSlot::Running(_))) {
            return true;
        }
        if let Some(VmSlot::Running(slot)) =
            self.replace_slot(&mut running, id, Some(VmSlot::Failed(reason)))
        {
            drop(running);
            let id = id.to_owned();
            tokio::spawn(async move {
                if let Some(vm) = slot.take_vm().await {
                    stop_vm_logged(&id, vm, "dead").await;
                }
            });
        }
        true
    }

    /// Watches a running VM: every [`MONITOR_INTERVAL`] probes it (holding
    /// only its own VM lock, never `running`) and publishes the result for
    /// `list`/`status`. A dead VM is moved to `Failed` and monitoring ends.
    /// Aborted by teardown via [`RunningSlot::take_vm`].
    fn spawn_monitor(
        &self,
        id: String,
        vm: Arc<Mutex<Option<Box<dyn VmHandle>>>>,
        status_tx: tokio::sync::watch::Sender<WorkspaceStatus>,
    ) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(MONITOR_INTERVAL).await;
                let status = {
                    let mut guard = vm.lock().await;
                    let Some(vm) = guard.as_mut() else { return };
                    vm.status().await
                };
                // The `vm` crate reports `Failed` only for an exited (or
                // unwaitable) qemu.
                if let VmStatus::Failed { reason } = status {
                    if state.mark_dead(&id, reason).await {
                        return;
                    }
                    continue;
                }
                status_tx.send_replace(to_status(status));
            }
        })
    }

    /// Builds the per-VM [`VmConfig`] for a workspace path. `host_port` is
    /// passed in because resolving it takes the `registry` lock, which must
    /// not nest under the `running` lock held here.
    fn vm_config(&self, id: &str, mcp_dir: PathBuf, host_port: u16) -> VmConfig {
        let repo_dir = PathBuf::from(id);
        let workspace_id = repo_dir.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );

        VmConfig {
            workspace_id,
            guest_output_dir: Self::guest_output_dir_for(id),
            state_dir: self.host_state_dir_for(id),
            runtime_dir: self.runtime_dir_for(id),
            repo_dir,
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
            save_registry(&self.inner.registry_path, &registry).await?;
        }
        Ok(port)
    }

    /// Where the guest writes its build output (`<repo>/.gavmo/vm`): rebuild
    /// logs and the failure marker. Guest-controlled; only ever read with
    /// [`read_guest_output_file`]. Derived from `id` because a `BuildFailed` slot
    /// outlives its `VmConfig`.
    fn guest_output_dir_for(id: &str) -> PathBuf {
        PathBuf::from(id).join(".gavmo").join("vm")
    }

    /// Host-only persistent VM state (disk, console log):
    /// `~/.local/share/gavmo/vms/<sha256 of id>`.
    fn host_state_dir_for(&self, id: &str) -> PathBuf {
        self.inner.vms_base_dir.join(workspace_hash(id))
    }

    /// Host-only VM sockets: `<runtime base>/<first 16 hex of sha256 of id>`,
    /// short enough for the unix socket path limit.
    fn runtime_dir_for(&self, id: &str) -> PathBuf {
        let hash = workspace_hash(id);
        self.inner
            .runtime_base_dir
            .join(hash.get(..16).unwrap_or(&hash))
    }

    /// This workspace's MCP token share dir, under GAVMO's app-data dir and
    /// outside the repo so the token can't leak into the user's project.
    fn mcp_dir_for(&self, id: &str) -> PathBuf {
        self.inner.mcp_base_dir.join(workspace_hash(id))
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
                        &candidate.def,
                        values.get(&candidate.secret_key).map(String::as_str),
                    ),
                    BindingState::Configured
                )
            })
            .map(CandidateTool::into_resolved)
            .collect()
    }

    /// The integrations of `id`'s `Running` slot (empty if none/stopped).
    fn integrations_for(&self, id: &str) -> Arc<[ResolvedIntegration]> {
        lock_unpoisoned(&self.inner.workspace_integrations)
            .get(id)
            .cloned()
            .unwrap_or_else(|| Arc::new([]))
    }

    /// Every tool this workspace's `gavmo.yaml` requests, read live (no secret
    /// access) so the control panel reflects config changes immediately.
    async fn candidates_for(&self, id: &str) -> Vec<CandidateTool> {
        // Reads `<id>/gavmo.yaml`.
        if !self.is_known(id).await {
            return Vec::new();
        }
        let config = crate::integrations::read_config(id).await;
        crate::integrations::resolve_candidates(id, &config)
    }

    /// Finds a single requested tool by id (structural; no secret-store access).
    async fn candidate(&self, id: &str, tool_id: &str) -> Result<CandidateTool, String> {
        self.require_known(id).await?;
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
                    &tool.def,
                    values.get(&tool.secret_key).map(String::as_str),
                );
                let delivery = Delivery::of(&tool.def)
                    .map(|d| d.describe())
                    .unwrap_or_default();
                let (configured, url_changed, bound_url, approved_delivery) = match state {
                    BindingState::NotConfigured => (false, false, None, None),
                    BindingState::Configured => (true, false, None, None),
                    BindingState::DestinationChanged { bound_url } => {
                        (false, true, bound_url, None)
                    }
                    BindingState::DeliveryChanged { approved } => (
                        false,
                        false,
                        None,
                        Some(approved.map_or_else(
                            || "an unrecorded method (saved by an older GAVMO)".to_owned(),
                            |d| d.describe(),
                        )),
                    ),
                };
                ToolConfig {
                    configured,
                    url_changed,
                    bound_url,
                    delivery,
                    approved_delivery,
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

        let delivery = Delivery::of(&tool.def)?;
        let credential = match delivery {
            Delivery::Basic => {
                let username = username
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| "basic auth requires a username/email".to_owned())?;
                Credential::Basic {
                    username,
                    password: token,
                }
            }
            Delivery::Bearer | Delivery::Header { .. } => Credential::Token { token },
        };

        // Bind the credential to the destination and delivery the user
        // approved, so a later `base_url` or `auth`/`header_name` change in the
        // untrusted `gavmo.yaml` fails closed instead of silently redirecting
        // or re-routing the token.
        let canonical = toolspec::canonical_base_url(&tool.def.base_url)
            .map_err(|e| format!("tool base_url could not be canonicalized: {e}"))?;
        let bound = crate::integrations::wrap_stored_secret(&canonical, delivery, credential)?;

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
        self.require_known(id).await.map_err(unknown_workspace_io)?;
        if !is_safe_rebuild_log_filename(log) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid build log filename",
            ));
        }
        let file = read_guest_output_file(id, log, MAX_BUILD_LOG_BYTES)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "build log not found"))?;
        let mut text = String::from_utf8_lossy(&file.bytes).into_owned();
        if file.truncated {
            text.insert_str(
                0,
                &format!("[... showing the last {MAX_BUILD_LOG_BYTES} bytes ...]\n"),
            );
        }
        Ok(text)
    }

    /// Reads and sanitizes the guest's boot console log for the live panel.
    /// A missing file (qemu not spawned yet) is an empty log, not an error.
    /// Only the newest [`MAX_CONSOLE_LOG_BYTES`] are returned.
    pub async fn read_console_log(&self, id: &str) -> io::Result<String> {
        self.require_known(id).await.map_err(unknown_workspace_io)?;
        // Host-only now, but read the same bounded, no-follow way regardless:
        // it is still mostly guest-produced text.
        let path = self.host_state_dir_for(id).join("console.log");
        Ok(crate::guest_files::read_guest_file(
            path,
            Vec::new(),
            MAX_CONSOLE_LOG_BYTES,
            crate::guest_files::Keep::Tail,
        )
        .await?
        .map(|file| sanitize_console(&file.bytes))
        .unwrap_or_default())
    }

    /// Resource usage for a `Running` VM: per-core CPU %, guest-reported
    /// memory, uptime, overlay size and host port. `None` (no stats this
    /// tick) if not running or a `/proc` read fails.
    ///
    /// The `running` lock is released before the CPU sampling sleep and the
    /// QMP call, so stats polls never block `stop`/`reset`.
    pub async fn vm_stats(&self, id: &str) -> Option<VmStats> {
        let info = match self.inner.running.lock().await.get(id) {
            Some(VmSlot::Running(running)) => VmInfo {
                uptime: running.info.uptime + running.info_taken.elapsed(),
                ..running.info.clone()
            },
            _ => return None,
        };
        let pid = info.qemu_pid?;
        let (uptime_secs, vcpus, host_port, qmp_socket_path) = (
            info.uptime.as_secs(),
            info.vcpus,
            info.host_port,
            info.qmp_socket_path,
        );

        let disk_bytes =
            vm::stats::file_size_or_zero(&self.host_state_dir_for(id).join("disk.qcow2")).ok()?;

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
        for integration in self.integrations_for(&scope.workspace_id).iter() {
            tools.push(crate::integrations::integration_tool_descriptor(
                integration,
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
            .iter()
            .find(|integration| integration.id == name)
            .cloned()
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

/// Deterministic, filesystem-safe, fixed-length name for a workspace's
/// per-workspace host dirs (MCP token share, VM state, VM runtime): the hex
/// SHA-256 of the workspace path.
///
/// SHA-256 rather than the hex-encoded path itself: hex doubles the length, so
/// any path over 127 bytes exceeded the 255-byte filename limit and that
/// workspace could never boot. The digest is always 64 chars.
fn workspace_hash(id: &str) -> String {
    use sha2::Digest as _;

    format!("{:x}", sha2::Sha256::digest(id.as_bytes()))
}

/// Terminates VM processes an earlier daemon left behind (each VM's runtime
/// dir holds pidfiles; see [`vm::reap_stale`]), so they can't hold a disk
/// lock or a port the next boot needs.
fn reap_stale_vm_processes(runtime_base: &Path) {
    let Ok(entries) = std::fs::read_dir(runtime_base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            vm::reap_stale(&path);
        }
    }
}

/// Where VM sockets live: `$XDG_RUNTIME_DIR/gavmo` (private to the user,
/// short, and cleared on reboot, so stale sockets disappear), else
/// `/tmp/gavmo-<uid>`. Either way it is created `0700`, and an existing dir
/// is refused unless it is a real directory owned by us
/// ([`vm::create_private_dir_all`]) - in `/tmp` another user could have
/// created it first.
fn default_runtime_base() -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir.join("gavmo"));
    }
    let uid = std::fs::metadata("/proc/self")?.uid();
    Ok(std::env::temp_dir().join(format!("gavmo-{uid}")))
}

/// Logs a one-time hint if a workspace still has VM files from GAVMO versions
/// that kept them in the repo. They are no longer used (and the disk may have
/// been modified by the guest, so it is deliberately not migrated). Only
/// `symlink_metadata` is used: nothing guest-controlled is followed.
fn warn_about_legacy_vm_state(id: &str) {
    let legacy = AppState::guest_output_dir_for(id).join("disk.qcow2");
    if let Ok(meta) = std::fs::symlink_metadata(&legacy)
        && meta.is_file()
    {
        tracing::info!(
            "old VM disk at {} is no longer used (VM state now lives in \
             ~/.local/share/gavmo/vms); it is safe to delete and frees {}",
            legacy.display(),
            human_bytes(meta.len())
        );
    }
}

/// `1536` -> `"1.5 KiB"`, for log messages.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS.get(unit).unwrap_or(&"B"))
    }
}

/// Refuses workspace paths whose read-write mount would hand the guest more
/// than a project: `/`, the user's home or any ancestor of it, and anything
/// overlapping a protected dir in either direction (equal to it, inside it,
/// or containing it). All paths must be canonical. Component-wise, so
/// `/home/u/.local/share/gavmo-other` doesn't count as inside `.../gavmo`.
fn check_workspace_path(
    canonical: &Path,
    protected: &[PathBuf],
    home: Option<&Path>,
) -> Result<(), String> {
    if canonical.parent().is_none() {
        return Err("the filesystem root cannot be a workspace".to_owned());
    }
    if let Some(home) = home
        && home.starts_with(canonical)
    {
        return Err(format!(
            "{} would expose your entire home directory to the VM; choose a project directory",
            canonical.display()
        ));
    }
    for dir in protected {
        if canonical.starts_with(dir) || dir.starts_with(canonical) {
            return Err(format!(
                "{} overlaps {}, which holds GAVMO's own secrets or credentials",
                canonical.display(),
                dir.display()
            ));
        }
    }
    Ok(())
}

/// `path` canonicalized if it exists; otherwise as given (a protected dir
/// that doesn't exist yet can't be reached through a symlink either).
fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Most of the console log returned per poll (the newest output).
const MAX_CONSOLE_LOG_BYTES: u64 = 1024 * 1024;
/// Most of a rebuild log shown in the UI (the end, where errors are).
const MAX_BUILD_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Reads the tail of `<id>/.gavmo/vm/<name>` (guest build output), which the guest can write
/// (see [`crate::guest_files`]): no symlinks, FIFOs or unbounded reads.
async fn read_guest_output_file(
    id: &str,
    name: &str,
    max_bytes: u64,
) -> io::Result<Option<crate::guest_files::GuestFile>> {
    let gavmo_dir = Path::new(id).join(".gavmo");
    let state_dir = AppState::guest_output_dir_for(id);
    crate::guest_files::read_guest_file(
        state_dir.join(name),
        vec![gavmo_dir, state_dir],
        max_bytes,
        crate::guest_files::Keep::Tail,
    )
    .await
}

/// Maps a [`AppState::require_known`] failure into the `io::Error` some
/// entry points return.
fn unknown_workspace_io(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, message)
}

/// Mints a fresh MCP token and writes it to the share dir as `token.env` (the
/// guest's `GAVMO_MCP_TOKEN`). It becomes valid only once the caller puts it
/// in a slot (see [`AppState::replace_slot`]).
async fn mint_mcp_token(dir: &Path) -> io::Result<String> {
    tokio::fs::create_dir_all(dir).await?;
    let token = uuid::Uuid::new_v4().to_string();
    let path = dir.join("token.env");
    write_owner_only(&path, format!("GAVMO_MCP_TOKEN={token}\n").as_bytes()).await?;
    Ok(token)
}

/// The slot for a failed boot: `BuildFailed` for a `gavmo.nix` failure.
fn slot_for_boot_error(e: &VmError) -> VmSlot {
    match e {
        VmError::RebuildFailed { log_file } => VmSlot::BuildFailed {
            log: log_file.clone(),
        },
        _ => VmSlot::Failed(e.to_string()),
    }
}

/// Stops a VM that no slot owns any more, logging (not returning) failure.
async fn stop_vm_logged(id: &str, vm: Box<dyn VmHandle>, what: &str) {
    if let Err(e) = vm.stop().await {
        tracing::warn!(workspace = %id, error = %e, "failed to stop a {what} VM cleanly");
    }
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

/// Maps a [`VmSlot`] to the serializable view type shared with the browser.
/// A `Running` slot reports what its monitor last published.
fn slot_status(slot: &VmSlot) -> WorkspaceStatus {
    match slot {
        VmSlot::Booting { .. } => WorkspaceStatus::Booting,
        VmSlot::Running(running) => running.status.borrow().clone(),
        VmSlot::Stopping => WorkspaceStatus::Stopping,
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

/// Loads `~/.local/share/gavmo/workspaces.json` (or platform equivalent).
///
/// A missing file is an empty registry. A file that exists but isn't a valid
/// registry (e.g. truncated by a crash before writes were atomic, or
/// hand-edited) is moved aside to `workspaces.json.corrupt-<unix time>` and
/// replaced by an empty registry, so the daemon still starts; the workspaces
/// can be re-added (their VM disks are found again by path). Other read
/// errors (permissions, I/O) are returned: nothing is discarded on a guess.
fn load_registry(path: &Path) -> io::Result<Registry> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(registry) => Ok(registry),
        Err(parse_error) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".corrupt-{stamp}"));
            std::fs::rename(path, &aside)?;
            tracing::error!(
                error = %parse_error,
                moved_to = %Path::new(&aside).display(),
                "workspace registry was unreadable; starting with an empty one \
                 (re-add your workspaces; their VM disks are kept)"
            );
            Ok(Registry::default())
        }
    }
}

/// Durably replaces the registry file: a uniquely named `0600` temp file in
/// the same dir is written and fsynced, renamed over the old file, and the
/// dir is fsynced - so a crash leaves the old or the new registry, never a
/// truncated one. Runs on the blocking pool; callers hold the registry lock
/// across it, which keeps concurrent saves ordered.
async fn save_registry(path: &Path, registry: &Registry) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(registry)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || write_atomically(&path, &bytes))
        .await
        .map_err(io::Error::other)?
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".workspaces.")
        .suffix(".tmp")
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(dir)?.sync_all()
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
        let slot = slot_for_boot_error(&err);
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

    /// A throwaway [`AppState`] with placeholder image paths, an in-memory
    /// secret store, and its own temp data dir (kept alive by the returned
    /// `TempDir`), so tests never read or write the user's real registry.
    fn test_app_state() -> (tempfile::TempDir, AppState) {
        let (data, state, _script) = scripted_app_state();
        (data, state)
    }

    /// [`test_app_state`] plus the controls of its fake VM launcher.
    fn scripted_app_state() -> (
        tempfile::TempDir,
        AppState,
        Arc<crate::vm_handle::fake::Script>,
    ) {
        let script = crate::vm_handle::fake::Script::new();
        let data = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(
            PathBuf::from("/nonexistent/gavmo-test-base.qcow2"),
            PathBuf::from("/nonexistent/gavmo-test-gcloud"),
            data.path(),
            Some(Path::new("/nonexistent/gavmo-test-home")),
            &data.path().join("run"),
            Arc::new(secrets::MemoryStore::new()),
            Arc::new(crate::vm_handle::fake::FakeLauncher(Arc::clone(&script))),
        )
        .unwrap();
        (data, state, script)
    }

    /// A `Booting` slot whose task never finishes in test time, standing in
    /// for an in-flight boot.
    fn never_finishing_boot_slot() -> VmSlot {
        VmSlot::Booting {
            task: tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(600)).await;
            }),
            generation: u64::MAX,
            token: "test-token".to_owned(),
        }
    }

    /// `stop_workspace` aborts an in-flight boot and returns promptly
    /// (bounded by a timeout so a regression fails instead of hanging).
    #[tokio::test]
    async fn stop_workspace_aborts_an_in_flight_boot_promptly() {
        let (_data, state) = test_app_state();
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

        let (_data, state) = test_app_state();
        register(&state, &id).await;
        let host_state = state.host_state_dir_for(&id);
        let runtime = state.runtime_dir_for(&id);
        for dir in [&host_state, &runtime] {
            tokio::fs::create_dir_all(dir).await.unwrap();
        }
        tokio::fs::write(host_state.join("disk.qcow2"), b"fake overlay")
            .await
            .unwrap();
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
        assert!(!host_state.exists());
        assert!(!runtime.exists());
        assert!(state.inner.running.lock().await.get(&id).is_none());
    }

    /// Registers `id` in memory only (as `add_workspace` would, minus the
    /// canonicalization), for tests of id-gated entry points.
    async fn register(state: &AppState, id: &str) {
        state
            .inner
            .registry
            .lock()
            .await
            .workspaces
            .push(WorkspaceRecord {
                path: id.to_owned(),
                host_port: None,
            });
    }

    /// Unknown ids must never reach the filesystem or boot a VM: they come
    /// from the browser and double as host paths.
    #[tokio::test]
    async fn id_gated_entry_points_reject_unknown_workspaces() {
        let (_tmp, repo_dir) = unique_temp_dir("unknown-repo");
        let state_dir = repo_dir.join(".gavmo").join("vm");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("rebuild-2026-01-01T00-00-00Z.log"),
            b"secret log",
        )
        .await
        .unwrap();
        tokio::fs::write(
            repo_dir.join("gavmo.yaml"),
            "tools:\n  t:\n    type: http\n    base_url: https://h.example.com\n    auth: bearer\n    secret: s\n",
        )
        .await
        .unwrap();
        let id = repo_dir.to_string_lossy().into_owned();
        let (_data, state) = test_app_state();

        assert!(state.open_workspace(&id).await.is_err());
        assert!(state.inner.running.lock().await.is_empty(), "no boot task");
        assert!(state.reset_workspace(&id).await.is_err());
        assert!(state_dir.exists(), "reset must not delete an unknown dir");
        assert!(state.read_console_log(&id).await.is_err());
        assert!(
            state
                .read_build_log(&id, "rebuild-2026-01-01T00-00-00Z.log")
                .await
                .is_err()
        );
        assert!(state.list_tool_configs(&id).await.is_empty());
        assert!(
            state
                .set_tool_secret(&id, "t", None, "tok".to_owned())
                .await
                .is_err()
        );
        assert_eq!(state.status(&id).await, WorkspaceStatus::Stopped);

        // The same directory, once registered, is reachable.
        register(&state, &id).await;
        assert_eq!(
            state
                .read_build_log(&id, "rebuild-2026-01-01T00-00-00Z.log")
                .await
                .unwrap(),
            "secret log"
        );
        assert_eq!(state.list_tool_configs(&id).await.len(), 1);
    }

    /// An alias of a registered path is not the registered workspace.
    #[tokio::test]
    async fn aliases_of_a_registered_id_are_unknown() {
        let (_data, state) = test_app_state();
        register(&state, "/srv/repo").await;
        for alias in ["/srv/repo/", "/srv/../srv/repo", "/srv/./repo", "/SRV/repo"] {
            assert!(!state.is_known(alias).await, "{alias}");
        }
        assert!(state.is_known("/srv/repo").await);
    }

    #[test]
    fn check_workspace_path_refuses_root_home_and_its_ancestors() {
        let home = Path::new("/home/u");
        for bad in ["/", "/home", "/home/u"] {
            assert!(
                check_workspace_path(Path::new(bad), &[], Some(home)).is_err(),
                "{bad}"
            );
        }
        assert!(check_workspace_path(Path::new("/home/u/src/proj"), &[], Some(home)).is_ok());
        assert!(check_workspace_path(Path::new("/srv/proj"), &[], Some(home)).is_ok());
    }

    #[test]
    fn check_workspace_path_refuses_any_overlap_with_protected_dirs() {
        let protected = [PathBuf::from("/home/u/.local/share/gavmo")];
        for bad in [
            "/home/u/.local/share/gavmo",
            "/home/u/.local/share/gavmo/mcp/abc",
            "/home/u/.local/share",
            "/home/u/.local",
        ] {
            assert!(
                check_workspace_path(Path::new(bad), &protected, None).is_err(),
                "{bad}"
            );
        }
        // Component-wise: a sibling with a shared name prefix is fine.
        assert!(
            check_workspace_path(
                Path::new("/home/u/.local/share/gavmo-other"),
                &protected,
                None
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn add_workspace_refuses_dangerous_paths_and_accepts_a_project() {
        let (data, state) = test_app_state();
        let projects = tempfile::tempdir().unwrap();

        // The data dir itself, something inside it, and a dir containing it.
        assert!(state.add_workspace(path_str(data.path())).await.is_err());
        let inside = data.path().join("mcp");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(state.add_workspace(path_str(&inside)).await.is_err());
        let parent = data.path().parent().unwrap();
        assert!(state.add_workspace(path_str(parent)).await.is_err());

        // A regular file is not a workspace.
        let file = projects.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(state.add_workspace(path_str(&file)).await.is_err());

        // A symlink into the data dir is judged by its target.
        #[cfg(unix)]
        {
            let link = projects.path().join("sneaky");
            std::os::unix::fs::symlink(data.path(), &link).unwrap();
            assert!(state.add_workspace(path_str(&link)).await.is_err());
        }

        let project = projects.path().join("proj");
        std::fs::create_dir(&project).unwrap();
        state.add_workspace(path_str(&project)).await.unwrap();
        let canonical = path_str(&std::fs::canonicalize(&project).unwrap());
        assert!(state.is_known(&canonical).await);
        assert_eq!(state.list().await.len(), 1, "only the project was added");
    }

    fn path_str(path: &Path) -> String {
        path.to_str().unwrap().to_owned()
    }

    /// A guest can replace `console.log` (or the whole state dir) with a
    /// symlink; the daemon must not follow it into a host file.
    #[cfg(unix)]
    #[tokio::test]
    async fn console_and_build_logs_refuse_guest_planted_symlinks() {
        let (_tmp, repo_dir) = unique_temp_dir("symlink-repo");
        let host = tempfile::tempdir().unwrap();
        let secret = host.path().join("rebuild-2026-01-01T00-00-00Z.log");
        std::fs::write(&secret, b"HOST SECRET").unwrap();
        let id = repo_dir.to_string_lossy().into_owned();
        let (_data, state) = test_app_state();
        register(&state, &id).await;

        // A console.log planted in the repo is not the console log at all:
        // qemu writes it to the host-only state dir.
        let state_dir = repo_dir.join(".gavmo").join("vm");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::os::unix::fs::symlink(&secret, state_dir.join("console.log")).unwrap();
        assert_eq!(state.read_console_log(&id).await.unwrap(), "");
        let host_state = state.host_state_dir_for(&id);
        std::fs::create_dir_all(&host_state).unwrap();
        std::fs::write(host_state.join("console.log"), b"real console").unwrap();
        assert_eq!(state.read_console_log(&id).await.unwrap(), "real console");

        // Whole state dir swapped for a symlink to a host dir.
        std::fs::remove_dir_all(&state_dir).unwrap();
        std::os::unix::fs::symlink(host.path(), &state_dir).unwrap();
        assert!(
            state
                .read_build_log(&id, "rebuild-2026-01-01T00-00-00Z.log")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn console_log_returns_only_the_newest_bytes() {
        let (_tmp, repo_dir) = unique_temp_dir("big-console");
        let id = repo_dir.to_string_lossy().into_owned();
        let (_data, state) = test_app_state();
        register(&state, &id).await;
        let state_dir = state.host_state_dir_for(&id);
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut big = vec![b'a'; usize::try_from(MAX_CONSOLE_LOG_BYTES).unwrap() * 2];
        big.extend_from_slice(b"NEWEST");
        std::fs::write(state_dir.join("console.log"), &big).unwrap();

        let text = state.read_console_log(&id).await.unwrap();
        assert!(text.ends_with("NEWEST"));
        assert!(text.len() as u64 <= MAX_CONSOLE_LOG_BYTES);
    }

    /// Everything a host process opens lives outside the guest-writable repo;
    /// only the guest's own build output stays in it.
    #[test]
    fn vm_config_keeps_host_state_out_of_the_repo() {
        let (data, state) = test_app_state();
        let id = "/home/u/src/project";
        let cfg = state.vm_config(id, state.mcp_dir_for(id), 40000);
        let hash = workspace_hash(id);

        assert_eq!(cfg.state_dir, data.path().join("vms").join(&hash));
        assert_eq!(
            cfg.runtime_dir,
            data.path().join("run").join(hash.get(..16).unwrap())
        );
        assert_eq!(cfg.guest_output_dir, Path::new(id).join(".gavmo/vm"));
        for host_path in [
            cfg.disk_path(),
            cfg.console_log_path(),
            cfg.qmp_socket_path(),
        ] {
            assert!(!host_path.starts_with(id), "{}", host_path.display());
            assert!(
                !host_path.starts_with(&cfg.mcp_dir),
                "{}",
                host_path.display()
            );
        }
        assert!(cfg.rebuild_failed_marker_path().starts_with(id));
    }

    /// The longest socket name must fit the unix socket limit under a typical
    /// runtime dir, however long the workspace path is.
    #[test]
    fn runtime_socket_paths_fit_the_unix_limit_for_any_workspace() {
        let id = format!("/home/u/{}", "deep/".repeat(100));
        let runtime = Path::new("/run/user/100000/gavmo")
            .join(workspace_hash(&id).get(..16).unwrap())
            .join("virtiofs-workspace.sock");
        assert!(runtime.as_os_str().len() <= 107, "{}", runtime.display());
    }

    #[test]
    fn human_bytes_formats_log_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(29_757_145_088), "27.7 GiB");
    }

    #[test]
    fn the_runtime_base_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (data, _state) = test_app_state();
        let mode = std::fs::metadata(data.path().join("run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    // --- Lifecycle (with the fake launcher) ---

    /// A registered workspace backed by a real (empty) temp dir.
    async fn registered_repo(state: &AppState) -> (tempfile::TempDir, String) {
        let repo = tempfile::tempdir().unwrap();
        let id = repo.path().to_str().unwrap().to_owned();
        register(state, &id).await;
        (repo, id)
    }

    /// Polls `cond` until true, failing after 5s instead of hanging.
    async fn eventually(what: &str, mut cond: impl AsyncFnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !cond().await {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn slot_is(state: &AppState, id: &str, pred: impl Fn(Option<&VmSlot>) -> bool) -> bool {
        pred(state.inner.running.lock().await.get(id))
    }

    fn current_token(state: &AppState, id: &str) -> Option<String> {
        lock_unpoisoned(&state.inner.mcp_tokens)
            .iter()
            .find(|(_, owner)| *owner == id)
            .map(|(token, _)| token.clone())
    }

    /// Opens `id` and lets its fake boot finish.
    async fn open_and_run(state: &AppState, script: &crate::vm_handle::fake::Script, id: &str) {
        state.open_workspace(id).await.unwrap();
        script.release_boots(1);
        eventually("Running", async || {
            slot_is(state, id, |s| matches!(s, Some(VmSlot::Running(_)))).await
        })
        .await;
    }

    #[tokio::test]
    async fn concurrent_opens_boot_once() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        let (a, b) = tokio::join!(state.open_workspace(&id), state.open_workspace(&id));
        // One boots; the other is a no-op or refused as busy, never a 2nd boot.
        assert!(a.is_ok() || b.is_ok());
        script.release_boots(2);
        eventually("Running", async || {
            slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Running(_)))).await
        })
        .await;
        state.open_workspace(&id).await.unwrap();
        assert_eq!(script.boots(), 1);
    }

    #[tokio::test]
    async fn stop_revokes_credentials_and_stops_the_vm() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        open_and_run(&state, &script, &id).await;
        let token = current_token(&state, &id).expect("a running VM has a token");
        assert!(TokenAuthority::resolve(&state, &token).is_some());

        state.stop_workspace(&id).await.unwrap();
        assert_eq!(TokenAuthority::resolve(&state, &token), None);
        assert!(state.integrations_for(&id).is_empty());
        assert!(slot_is(&state, &id, |s| s.is_none()).await);
        assert_eq!(script.stops(), 1);
    }

    #[tokio::test]
    async fn stop_during_boot_aborts_it_and_revokes_the_token() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        state.open_workspace(&id).await.unwrap();
        let token = current_token(&state, &id).expect("booting VMs hold a token");

        state.stop_workspace(&id).await.unwrap();
        // A late permit must not resurrect anything.
        script.release_boots(1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(slot_is(&state, &id, |s| s.is_none()).await);
        assert_eq!(TokenAuthority::resolve(&state, &token), None);
    }

    /// While a stop is in progress, open is refused (not queued) and the UI
    /// sees `Stopping`.
    #[tokio::test]
    async fn open_is_refused_while_a_stop_is_in_progress() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        open_and_run(&state, &script, &id).await;
        script
            .stops_block
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let stopping = tokio::spawn({
            let state = state.clone();
            let id = id.clone();
            async move { state.stop_workspace(&id).await }
        });
        eventually("Stopping", async || {
            slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Stopping))).await
        })
        .await;
        assert_eq!(state.status(&id).await, WorkspaceStatus::Stopping);
        let refused = state.open_workspace(&id).await.unwrap_err();
        assert!(refused.contains("busy"), "{refused}");

        script.stop_gate.add_permits(1);
        stopping.await.unwrap().unwrap();
        open_and_run(&state, &script, &id).await;
        assert_eq!(script.boots(), 2);
    }

    /// Reset keeps the workspace `Stopping` until its files are deleted.
    #[tokio::test]
    async fn reset_holds_off_opens_until_state_is_deleted() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        open_and_run(&state, &script, &id).await;
        let disk_dir = state.host_state_dir_for(&id);
        tokio::fs::create_dir_all(&disk_dir).await.unwrap();
        script
            .stops_block
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let resetting = tokio::spawn({
            let state = state.clone();
            let id = id.clone();
            async move { state.reset_workspace(&id).await }
        });
        eventually("Stopping", async || {
            slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Stopping))).await
        })
        .await;
        assert!(state.open_workspace(&id).await.is_err());
        script.stop_gate.add_permits(1);
        resetting.await.unwrap().unwrap();
        assert!(!disk_dir.exists());
        assert!(slot_is(&state, &id, |s| s.is_none()).await);
    }

    #[tokio::test]
    async fn remove_unregisters_first_then_stops() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        open_and_run(&state, &script, &id).await;

        state.remove_workspace(&id).await.unwrap();
        assert!(!state.is_known(&id).await);
        assert!(slot_is(&state, &id, |s| s.is_none()).await);
        assert_eq!(script.stops(), 1);
        assert!(state.open_workspace(&id).await.is_err());
        assert_eq!(script.boots(), 1);
    }

    /// A Running VM whose qemu died becomes Failed (credentials revoked, VM
    /// reaped) and can be opened again.
    #[tokio::test]
    async fn a_dead_vm_becomes_failed_and_can_be_reopened() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        open_and_run(&state, &script, &id).await;
        let token = current_token(&state, &id).unwrap();

        script.set_status(VmStatus::Failed {
            reason: "qemu exited: signal 9".to_owned(),
        });
        // Noticed by the monitor, not by the status read.
        eventually("Failed", async || {
            matches!(state.status(&id).await, WorkspaceStatus::Failed { .. })
        })
        .await;
        assert!(slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Failed(_)))).await);
        assert_eq!(TokenAuthority::resolve(&state, &token), None);
        eventually("dead VM reaped", async || script.stops() == 1).await;

        script.set_status(VmStatus::Running {
            url: "http://127.0.0.1:1".to_owned(),
        });
        open_and_run(&state, &script, &id).await;
        assert_eq!(script.boots(), 2);
    }

    /// A VM whose status probe never returns (wedged qemu) must not block
    /// `list`, `status`, or stopping it.
    #[tokio::test]
    async fn a_wedged_vm_blocks_neither_reads_nor_stop() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        open_and_run(&state, &script, &id).await;
        script
            .status_hangs
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Let the monitor get stuck inside a probe.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let quick = Duration::from_secs(1);
        let listed = tokio::time::timeout(quick, state.list()).await.unwrap();
        assert!(matches!(listed[0].status, WorkspaceStatus::Running { .. }));
        tokio::time::timeout(quick, state.status(&id))
            .await
            .unwrap();
        tokio::time::timeout(quick, state.stop_workspace(&id))
            .await
            .expect("stop must cancel the stuck probe")
            .unwrap();
        assert_eq!(script.stops(), 1);
    }

    /// The monitor publishes what the VM reports.
    #[tokio::test]
    async fn the_monitor_publishes_status_changes() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        open_and_run(&state, &script, &id).await;
        script.set_status(VmStatus::Booting);
        eventually("Booting published", async || {
            state.status(&id).await == WorkspaceStatus::Booting
        })
        .await;
    }

    #[tokio::test]
    async fn shutdown_stops_every_vm_and_refuses_new_boots() {
        let (_data, state, script) = scripted_app_state();
        let (_repo_a, a) = registered_repo(&state).await;
        let (_repo_b, b) = registered_repo(&state).await;
        open_and_run(&state, &script, &a).await;
        open_and_run(&state, &script, &b).await;

        tokio::time::timeout(
            Duration::from_secs(5),
            state.shutdown(Duration::from_secs(5)),
        )
        .await
        .unwrap();
        assert_eq!(script.stops(), 2);
        assert!(state.inner.running.lock().await.is_empty());
        assert!(state.open_workspace(&a).await.is_err());
    }

    /// A VM that won't stop can't hold up exit past the limit.
    #[tokio::test]
    async fn shutdown_gives_up_after_its_limit() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        open_and_run(&state, &script, &id).await;
        script
            .stops_block
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::timeout(
            Duration::from_secs(5),
            state.shutdown(Duration::from_millis(100)),
        )
        .await
        .expect("shutdown must respect its limit");
    }

    /// A boot finishing after its slot was taken over stops its VM instead of
    /// publishing it.
    #[tokio::test]
    async fn a_superseded_boot_result_is_stopped_not_published() {
        let (_data, state, script) = scripted_app_state();
        let (_repo, id) = registered_repo(&state).await;
        {
            let mut running = state.inner.running.lock().await;
            state.replace_slot(&mut running, &id, Some(VmSlot::Stopping));
        }
        let cfg = state.vm_config(&id, state.mcp_dir_for(&id), 1);
        script.release_boots(1);
        let vm = state.inner.launcher.boot(cfg).await.unwrap();

        state
            .finish_boot(&id, 12345, "late".to_owned(), Ok(vm))
            .await;
        assert!(slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Stopping))).await);
        assert_eq!(TokenAuthority::resolve(&state, "late"), None);
        assert_eq!(script.stops(), 1);
    }

    #[tokio::test]
    async fn save_registry_round_trips_atomically_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        let registry = Registry {
            workspaces: vec![WorkspaceRecord {
                path: "/srv/a".to_owned(),
                host_port: Some(40000),
            }],
        };
        save_registry(&path, &registry).await.unwrap();
        save_registry(&path, &registry).await.unwrap();

        let loaded = load_registry(&path).unwrap();
        assert_eq!(loaded.workspaces.len(), 1);
        assert_eq!(loaded.workspaces[0].host_port, Some(40000));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["workspaces.json"], "no temp files left behind");
    }

    /// A truncated registry (as a crash mid-write used to leave) no longer
    /// stops the daemon starting; it is kept aside for inspection.
    #[test]
    fn a_corrupt_registry_is_moved_aside_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, br#"{"workspaces":[{"path":"/sr"#).unwrap();

        let loaded = load_registry(&path).unwrap();
        assert!(loaded.workspaces.is_empty());
        assert!(!path.exists());
        let aside: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(aside.len(), 1);
        assert!(
            aside[0].starts_with("workspaces.json.corrupt-"),
            "{aside:?}"
        );
    }

    /// Unreadable (as opposed to unparseable) is an error: never discard data
    /// on a guess.
    #[test]
    fn an_unreadable_registry_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::create_dir(&path).unwrap();
        assert!(load_registry(&path).is_err());
        assert!(path.is_dir());
    }

    /// Share dir names are deterministic, hex-only, and distinct per workspace.
    #[test]
    fn workspace_hash_is_deterministic_and_filesystem_safe() {
        let a = workspace_hash("/home/user/project-a");
        let b = workspace_hash("/home/user/project-b");

        assert_eq!(a, workspace_hash("/home/user/project-a"));
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    /// Long workspace paths must still give a usable (<= 255 byte) name.
    #[test]
    fn workspace_hash_has_fixed_length_for_long_paths() {
        let long = format!("/home/user/{}", "d/".repeat(200));
        assert_eq!(workspace_hash(&long).len(), 64);
        assert_eq!(workspace_hash("/").len(), 64);
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

    /// Minting writes `token.env`; the token is not valid until a slot
    /// holds it.
    #[tokio::test]
    async fn a_minted_token_is_written_but_only_valid_once_installed() {
        let (_data, state) = test_app_state();
        let (_tmp, dir) = unique_temp_dir("mcp-token-mint");

        let token = mint_mcp_token(&dir).await.unwrap();
        let contents = tokio::fs::read_to_string(dir.join("token.env"))
            .await
            .unwrap();
        assert_eq!(contents, format!("GAVMO_MCP_TOKEN={token}\n"));
        assert_eq!(TokenAuthority::resolve(&state, &token), None);
    }

    /// The token index follows the slot: valid while Booting/Running,
    /// revoked by any other slot, and replaced by a newer boot's token.
    #[tokio::test]
    async fn replace_slot_keeps_tokens_in_step_with_the_slot() {
        let (_data, state) = test_app_state();
        let id = "/ws";
        let booting = |token: &str| VmSlot::Booting {
            task: tokio::spawn(async {}),
            generation: 0,
            token: token.to_owned(),
        };
        let mut running = state.inner.running.lock().await;

        state.replace_slot(&mut running, id, Some(booting("old")));
        assert_eq!(
            TokenAuthority::resolve(&state, "old"),
            Some(WorkspaceScope {
                workspace_id: id.to_owned()
            })
        );
        state.replace_slot(&mut running, id, Some(booting("new")));
        assert_eq!(TokenAuthority::resolve(&state, "old"), None);
        assert!(TokenAuthority::resolve(&state, "new").is_some());

        for revoking in [
            Some(VmSlot::Stopping),
            Some(VmSlot::Failed("x".to_owned())),
            None,
        ] {
            state.replace_slot(&mut running, id, Some(booting("t")));
            state.replace_slot(&mut running, id, revoking);
            assert_eq!(TokenAuthority::resolve(&state, "t"), None);
        }
    }

    /// An unknown token resolves to nothing.
    #[test]
    fn resolve_returns_none_for_an_unknown_token() {
        let (_data, state) = test_app_state();
        assert_eq!(TokenAuthority::resolve(&state, "never-minted"), None);
    }
}
