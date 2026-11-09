//! VM lifecycle: the per-workspace slot state machine, open/stop/reset/remove, boot completion, the per-VM monitor and daemon shutdown.

use super::error::{RegistryError, Result, io_ctx};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use vm::{Error as VmError, VmConfig, VmStatus};

use super::AppState;
use super::resources::DEFAULTS;
use super::store::commit_registry;
use super::util::{lock_unpoisoned, remove_dir_all_idempotent, write_owner_only};
use crate::integrations::ResolvedIntegration;
use crate::types::{ResourceValues, WorkspaceStatus};
use crate::vm_handle::{VmHandle, VmInfo};

/// How often a running VM's monitor probes it (QMP + HTTP).
pub(super) const MONITOR_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(2)
};

/// Appended to a running VM's base URL so "Open UI" lands in a new opencode
/// session for the guest's `/workspace`: base64url("/workspace") + "/session",
/// which opencode routes to a new session for that project. The
/// `opencode_workspace_session_path_matches_base64url_workspace` test checks the derivation.
pub(super) const OPENCODE_WORKSPACE_SESSION_PATH: &str = "/L3dvcmtzcGFjZQ/session";

/// The lifecycle state of a workspace's VM. Absence from the map means
/// stopped.
///
/// The MCP token and resolved integrations live *in* the slot, and the
/// lookup indexes `mcp_tokens` / `workspace_integrations` are only ever
/// updated together with the slot (see [`AppState::replace_slot`]), under
/// the `running` lock. A workspace's credentials are therefore valid exactly
/// while it is `Booting` (token only) or `Running`; every other transition
/// revokes them in the same step.
pub(super) enum VmSlot {
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
    /// The VM's disk was made from a different base image than the current
    /// one, so it was not booted; a reset rebuilds it. `reason` names both.
    BaseImageChanged { reason: String },
}

/// A booted VM, watched by a background monitor task.
///
/// The VM handle sits behind its own lock, which only the monitor (while
/// probing) and a teardown (to take it) ever hold - never the `running`
/// lock. Readers get the latest status from `status` without waiting on
/// qemu, and the stats panel uses the `info` snapshot.
pub(super) struct RunningSlot {
    pub(super) vm: Arc<Mutex<Option<Box<dyn VmHandle>>>>,
    pub(super) status: tokio::sync::watch::Receiver<WorkspaceStatus>,
    pub(super) monitor: tokio::task::JoinHandle<()>,
    pub(super) info: VmInfo,
    /// When `info` was taken, so uptime can be derived later.
    pub(super) info_taken: std::time::Instant,
    pub(super) token: String,
    pub(super) integrations: Arc<[ResolvedIntegration]>,
}

impl RunningSlot {
    /// Stops monitoring and takes the VM out for a stop. Aborting cancels a
    /// probe in flight (releasing the VM lock) at its next await point.
    pub(super) async fn take_vm(&self) -> Option<Box<dyn VmHandle>> {
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
    pub(super) fn token(&self) -> Option<&str> {
        match self {
            Self::Booting { token, .. } => Some(token),
            Self::Running(running) => Some(&running.token),
            Self::Stopping
            | Self::Failed(_)
            | Self::BuildFailed { .. }
            | Self::BaseImageChanged { .. } => None,
        }
    }

    /// Whether a VM (or its boot) is live.
    pub(super) fn is_active(&self) -> bool {
        matches!(self, Self::Booting { .. } | Self::Running(_))
    }
}

/// What a teardown found in the slot it replaced with `Stopping`.
pub(super) enum Teardown {
    Vm(Box<RunningSlot>),
    Boot(tokio::task::JoinHandle<()>),
    Nothing,
}

impl AppState {
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
    pub(super) fn lifecycle_lock(&self, id: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            lock_unpoisoned(&self.inner.lifecycle)
                .entry(id.to_owned())
                .or_default(),
        )
    }

    /// Replaces (`Some`) or removes (`None`) `id`'s slot, updating the token
    /// and integration indexes in the same step. Callers hold the `running`
    /// lock (they pass its map), so no reader sees the two disagree.
    pub(super) fn replace_slot(
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
    pub async fn open_workspace(&self, id: &str) -> Result<()> {
        // Otherwise any host directory could be booted with `id` mounted
        // read-write into the guest.
        self.require_known(id).await?;
        if self
            .inner
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RegistryError::ShuttingDown);
        }
        let Ok(_lifecycle) = self.lifecycle_lock(id).try_lock_owned() else {
            return Err(RegistryError::Busy);
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
            .map_err(io_ctx("allocating a host port"))?;

        // Mint the per-boot MCP token only on a real new boot: rotating a
        // running VM's token would break its already-loaded `gavmo-opencode`.
        let mcp_dir = self.mcp_dir_for(id);
        let token = match mint_mcp_token(&mcp_dir).await {
            Ok(token) => token,
            Err(e) => {
                let e = RegistryError::io("writing the MCP token", e);
                let mut running = self.inner.running.lock().await;
                self.replace_slot(&mut running, id, Some(VmSlot::Failed(e.to_string())));
                return Err(e);
            }
        };

        // Read before the `running` lock (lock order: registry, then running).
        let resources = self.saved_resources(id).await.resolve(DEFAULTS);
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
        let task = tokio::spawn(async move {
            // In the task, not before it: the first identification of an
            // image hashes it, which takes seconds; the slot shows `Booting`.
            let result = match state.inner.base_image.id().await {
                Ok(base_image_id) => {
                    let cfg =
                        state.vm_config(&task_id, mcp_dir, host_port, resources, base_image_id);
                    state.inner.launcher.boot(cfg).await
                }
                Err(source) => Err(VmError::BaseImage {
                    path: state.inner.base_image.path().to_path_buf(),
                    source,
                }),
            };
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
    pub(super) async fn finish_boot(
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
    pub(super) async fn begin_teardown(&self, id: &str) -> Teardown {
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
    pub(super) async fn run_teardown(id: &str, teardown: Teardown) -> Result<()> {
        match teardown {
            Teardown::Vm(slot) => match slot.take_vm().await {
                Some(vm) => Ok(vm.stop().await?),
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
    pub(super) async fn end_teardown(&self, id: &str, slot: Option<VmSlot>) {
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
    pub async fn stop_workspace(&self, id: &str) -> Result<()> {
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
    pub async fn remove_workspace(&self, id: &str) -> Result<()> {
        let lifecycle = self.lifecycle_lock(id);
        let _held = lifecycle.lock().await;
        {
            // If this fails, the workspace stays listed and its VM keeps
            // running with valid credentials; nothing is half-removed.
            let mut registry = self.inner.registry.lock().await;
            if registry.workspaces.iter().any(|w| w.path == id) {
                let mut updated = registry.clone();
                updated.workspaces.retain(|w| w.path != id);
                commit_registry(&mut registry, &self.inner.registry_path, updated)
                    .await
                    .map_err(io_ctx("saving the workspace registry"))?;
            }
        }
        let teardown = self.begin_teardown(id).await;
        let result = Self::run_teardown(id, teardown).await;
        self.end_teardown(id, None).await;
        result
    }

    /// Stops the VM and deletes its host state (overlay disk and its base
    /// image record, console log), its sockets, and `<repo>/.gavmo/vm`
    /// (rebuild logs), so the next open builds a fresh overlay from the
    /// current base image - the way out of `BaseImageChanged`, since an
    /// overlay only makes sense on the exact base it was created from.
    ///
    /// Destructive (the guest's `/nix` cache is lost; opencode sessions in
    /// `<repo>/.gavmo/opencode` are kept); the caller must have obtained user
    /// confirmation. An in-flight boot is aborted and awaited first, and the
    /// workspace stays `Stopping` (refusing opens) until the files are gone,
    /// so qemu can't be writing the overlay while it is deleted.
    pub async fn reset_workspace(&self, id: &str) -> Result<()> {
        // The state dir is derived from `id` and deleted below.
        self.require_known(id).await?;
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
        .await
        .map_err(io_ctx("deleting VM state"));
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
    pub(super) async fn mark_dead(&self, id: &str, reason: String) -> bool {
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
    pub(super) fn spawn_monitor(
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
                if let VmStatus::Failed(failure) = status {
                    if state.mark_dead(&id, failure.to_string()).await {
                        return;
                    }
                    continue;
                }
                status_tx.send_replace(to_status(status));
            }
        })
    }

    /// Builds the per-VM [`VmConfig`] for a workspace path. `host_port` and
    /// `resources` are passed in because reading them takes the `registry` lock, which must
    /// not nest under the `running` lock held here.
    pub(super) fn vm_config(
        &self,
        id: &str,
        mcp_dir: PathBuf,
        host_port: u16,
        resources: ResourceValues,
        base_image_id: String,
    ) -> VmConfig {
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
            base_image: self.inner.base_image.path().to_path_buf(),
            base_image_id,
            mcp_dir,
            disk_size_gib: resources.disk_gib,
            memory_mib: resources.memory_gib.saturating_mul(1024),
            vcpus: resources.vcpus,
            host_port,
        }
    }
}

/// Terminates VM processes an earlier daemon left behind (each VM's runtime
/// dir holds pidfiles; see [`vm::reap_stale`]), so they can't hold a disk
/// lock or a port the next boot needs.
pub(super) fn reap_stale_vm_processes(runtime_base: &Path) {
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

/// Mints a fresh MCP token and writes it to the share dir as `token.env` (the
/// guest's `GAVMO_MCP_TOKEN`). It becomes valid only once the caller puts it
/// in a slot (see [`AppState::replace_slot`]).
pub(super) async fn mint_mcp_token(dir: &Path) -> io::Result<String> {
    tokio::fs::create_dir_all(dir).await?;
    let token = uuid::Uuid::new_v4().to_string();
    let path = dir.join("token.env");
    write_owner_only(&path, format!("GAVMO_MCP_TOKEN={token}\n").as_bytes()).await?;
    Ok(token)
}

/// The slot for a failed boot: `BuildFailed` for a `gavmo.nix` failure,
/// `BaseImageChanged` for a disk made from another base image.
pub(super) fn slot_for_boot_error(e: &VmError) -> VmSlot {
    match e {
        VmError::RebuildFailed { log_file } => VmSlot::BuildFailed {
            log: log_file.clone(),
        },
        VmError::BaseImageChanged { .. } => VmSlot::BaseImageChanged {
            reason: e.to_string(),
        },
        _ => VmSlot::Failed(e.to_string()),
    }
}

/// Stops a VM that no slot owns any more, logging (not returning) failure.
pub(super) async fn stop_vm_logged(id: &str, vm: Box<dyn VmHandle>, what: &str) {
    if let Err(e) = vm.stop().await {
        tracing::warn!(workspace = %id, error = %e, "failed to stop a {what} VM cleanly");
    }
}

/// Aborts an in-flight boot task and waits for it to finish, so qemu (owned
/// by the task, `kill_on_drop`) is gone on return. Cancellation, or the task
/// having already finished, is the expected outcome; a panic is logged.
pub(super) async fn abort_boot_task(id: &str, handle: tokio::task::JoinHandle<()>) {
    handle.abort();
    match handle.await {
        Ok(()) => {}
        Err(e) if e.is_cancelled() => {}
        Err(e) => tracing::error!(workspace = %id, error = %e, "boot task panicked"),
    }
}

/// Maps a [`VmSlot`] to the serializable view type shared with the browser.
/// A `Running` slot reports what its monitor last published.
pub(super) fn slot_status(slot: &VmSlot) -> WorkspaceStatus {
    match slot {
        VmSlot::Booting { .. } => WorkspaceStatus::Booting,
        VmSlot::Running(running) => running.status.borrow().clone(),
        VmSlot::Stopping => WorkspaceStatus::Stopping,
        VmSlot::Failed(reason) => WorkspaceStatus::Failed {
            reason: reason.clone(),
        },
        VmSlot::BuildFailed { log } => WorkspaceStatus::BuildFailed { log: log.clone() },
        VmSlot::BaseImageChanged { reason } => WorkspaceStatus::BaseImageChanged {
            reason: reason.clone(),
        },
    }
}

/// Maps the `vm` crate's status into the serializable view type shared with
/// the browser.
pub(super) fn to_status(status: VmStatus) -> WorkspaceStatus {
    match status {
        VmStatus::Booting => WorkspaceStatus::Booting,
        VmStatus::Running { url } => WorkspaceStatus::Running {
            url: format!("{url}{OPENCODE_WORKSPACE_SESSION_PATH}"),
        },
        VmStatus::Failed(failure) => WorkspaceStatus::Failed {
            reason: failure.to_string(),
        },
    }
}
