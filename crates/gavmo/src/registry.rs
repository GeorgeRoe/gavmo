//! Server-side (ssr-only) state for the GAVMO daemon.
//!
//! Holds the persistent workspace registry (a JSON file under a global app
//! dir) and the in-memory map of currently-running VMs. This module is the
//! only place that touches the `vm` crate, keeping all VM-management
//! mechanism behind the daemon's server functions. See
//! `crates/gavmo/plan.md` §§6–7.

use std::collections::{HashMap, HashSet};
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
use crate::integrations::{CandidateTool, ResolvedIntegration, ScopeKind};

/// Default guest memory, in MiB.
const DEFAULT_MEMORY_MIB: u32 = 4096;
/// Default guest vCPU count.
const DEFAULT_VCPUS: u32 = 4;

/// How long to wait between the two `/proc` samples [`AppState::vm_stats`]
/// takes to compute a CPU percentage (see `vm::stats::cpu_percent_per_core`
/// — it needs a delta between two points in time). Short enough to keep a
/// single poll snappy, long enough that normal scheduler-tick granularity
/// (conventionally ~10ms per jiffy) doesn't dominate the measurement with
/// noise.
const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Appended to a running VM's base URL so "Open UI" lands the user directly
/// in a new opencode session scoped to the guest's `/workspace` mount,
/// skipping opencode's first-run "create a project → type /workspace"
/// screen.
///
/// opencode's web UI routes a project session at `/<dir>/session`, where
/// `<dir>` is URL-safe base64 (no padding) of the absolute directory path
/// (opencode `packages/core/src/util/encode.ts` `base64Encode`, consumed by
/// `packages/app/src/pages/directory-layout.tsx`). The guest always mounts
/// the repo at `/workspace` (`MOUNT_TAG_WORKSPACE` in
/// `crates/vm/src/handle.rs` + `nix/base/configuration.nix`), and
/// base64url("/workspace") = "L3dvcmtzcGFjZQ". Hitting this path with no
/// session id fires opencode's `SessionRoute` effect
/// (`packages/app/src/app.tsx`), which creates a new draft and navigates to
/// `/new-session?draftId=<uuid>` — the same place opencode's own "new
/// session" button leads to. Verified against opencode v1.18.35; see the
/// `opencode_workspace_session_path_matches_base64url_workspace` test below
/// for the derivation, which will catch drift if `/workspace` ever changes.
const OPENCODE_WORKSPACE_SESSION_PATH: &str = "/L3dvcmtzcGFjZQ/session";

/// On-disk registry: the set of repos GAVMO knows about. Running state is
/// never persisted — a stopped VM simply isn't in the in-memory map.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    pub workspaces: Vec<WorkspaceRecord>,
}

/// A single known workspace. Identity is the absolute host path of the repo
/// (see `gavmo-mvp-plan.md` — "Workspace identity: absolute host path").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub path: String,
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

    /// Root dir for every workspace's MCP token share (see `mcp_dir_for`) —
    /// `~/.local/share/gavmo/mcp`, deliberately **outside** any repo (see
    /// `VmConfig::mcp_dir`'s doc comment for why).
    mcp_base_dir: PathBuf,

    /// Bearer token -> workspace id, for [`AppState`]'s [`TokenAuthority`]
    /// impl. A `std::sync::Mutex`, not the `tokio::sync::Mutex` used
    /// elsewhere in this file: `TokenAuthority::resolve` is a synchronous
    /// trait method (see `mcp`'s doc comment on why — it's called from
    /// inside `mcp`'s async `ServerHandler` methods, but as a plain
    /// in-memory `HashMap` lookup there's never a reason to `.await` while
    /// holding this lock, so a blocking mutex is both simpler and correct
    /// here. Minted fresh (and any stale entry for the same workspace
    /// dropped) on every `open_workspace`; cleared on `stop`/`reset`.
    mcp_tokens: std::sync::Mutex<HashMap<String, String>>,

    /// Host secret store backing the tool integrations (V1.3 — see
    /// `crate::integrations`). An `Arc<dyn …>` so the concrete backend
    /// (keyring today, the encrypted vault later) is swappable without
    /// touching `AppState`'s callers.
    secret_store: Arc<dyn SecretStore>,

    /// Workspace id -> the tool integrations resolved for its current boot
    /// (see `crate::integrations::resolve`). Populated by `open_workspace` once
    /// a VM boots and read by this struct's [`ToolExecutor`] impl; cleared on
    /// `stop`/`reset` alongside the MCP token. A `std::sync::Mutex` for the same
    /// reason as `mcp_tokens`: `ToolExecutor::list_tools` is synchronous and
    /// this is a plain in-memory lookup.
    workspace_integrations: std::sync::Mutex<HashMap<String, Vec<ResolvedIntegration>>>,

    /// Shared HTTP client for the tool forwarder (V1.4 — see
    /// `crate::integrations::forward`). Cloning a `reqwest::Client` shares its
    /// connection pool, so one instance serves every workspace's tool calls.
    http_client: reqwest::Client,
}

/// State of a workspace's VM while a boot is in flight or has settled.
/// Boots happen in a background task (see [`AppState::open_workspace`]) so
/// the server function returns immediately; the live status poll picks up
/// the transition from `Booting` to `Running`/`Failed`.
enum VmSlot {
    /// `RunningVm::boot` is in progress, in the task tracked by this handle.
    /// Carrying the handle (rather than a bare marker, as this used to be)
    /// is what makes [`AppState::stop_workspace`]/[`AppState::reset_workspace`]
    /// race-free against an in-flight boot: `handle.abort()` cancels the
    /// boot task at its next await point, which drops any `RunningVm`
    /// constructed so far — and `RunningVm`'s `Child` has `kill_on_drop` set
    /// (`vm::handle`), so qemu is torn down either way. Awaiting the handle
    /// afterwards then guarantees qemu is actually gone (not just "asked to
    /// stop") before the caller proceeds — e.g. before `reset_workspace`
    /// touches the overlay this VM might otherwise still be writing to.
    Booting(tokio::task::JoinHandle<()>),
    /// Booted and (as of the last check) healthy. Boxed: `RunningVm` is
    /// much larger than the other variants (it owns a `tokio::process::Child`
    /// plus helper-process handles), and this enum lives in a `HashMap`.
    Running(Box<RunningVm>),
    /// The last boot attempt failed; kept around so the reason is visible
    /// in the UI until the next `open_workspace` call replaces it.
    Failed(String),
    /// M2: the repo's `gavmo.nix` failed to build (`vm::Error::RebuildFailed`,
    /// detected within seconds via the guest's failure marker rather than a
    /// full boot-health timeout — see `nix/base/configuration.nix`'s
    /// `gavmo-rebuild` service). `log` is the failed run's log filename
    /// (relative to the workspace's `.gavmo/vm` state dir), fetchable via
    /// [`AppState::read_build_log`].
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
        let mcp_base_dir = dirs::data_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?
            .join("gavmo")
            .join("mcp");

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
                secret_store: Arc::new(secrets::KeyringStore::new()),
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
            registry
                .workspaces
                .push(WorkspaceRecord { path: canonical });
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
    /// already booting/running) and returns immediately — it does **not**
    /// wait for the boot to finish. The boot runs in a background task;
    /// callers should poll `status`/`list` to observe the transition from
    /// `Booting` to `Running`/`Failed`.
    pub async fn open_workspace(&self, id: &str) -> Result<(), String> {
        // The whole check -> build-config -> spawn -> insert sequence runs
        // under one continuously-held lock (no `.await` in between: the
        // status check and `vm_config` are both synchronous, and
        // `tokio::spawn` itself doesn't await the spawned future's body —
        // it just queues it and returns the handle). This preserves the
        // same TOCTOU-free guarantee the old "check-then-insert-a-bare-
        // marker" version had: two concurrent `open_workspace` calls for
        // the same `id` can never both decide to boot.
        let mut running = self.inner.running.lock().await;
        match running.get(id) {
            // Already booting or running: nothing to do.
            Some(VmSlot::Booting(_) | VmSlot::Running(_)) => return Ok(()),
            // No entry, or the last attempt failed (generically or a
            // broken `gavmo.nix`): (re)boot below. Retrying after
            // `BuildFailed` is the expected path once the user fixes
            // `gavmo.nix`.
            Some(VmSlot::Failed(_) | VmSlot::BuildFailed { .. }) | None => {}
        }

        // Mint a fresh per-boot MCP capability token and publish it into the
        // read-only share the guest mounts at `/etc/gavmo/mcp` (see
        // `crates/mcp/plan.md`), *before* building `VmConfig` (which embeds
        // the same directory as `mcp_dir`). Must only happen on an actual
        // new boot — not on the early-return "already booting/running"
        // paths above — since silently rotating a *running* VM's token
        // would invalidate the one its already-started `gavmo-opencode`
        // loaded into its environment, breaking an otherwise-healthy VM's
        // tool calls for no reason.
        //
        // This does mean a (cheap, one-shot) file write happens while
        // holding `running`'s lock, slightly widening the window other
        // `status`/`list` callers wait behind it — accepted the same way
        // this function already accepts holding the lock across its other
        // synchronous setup work.
        let mcp_dir = self.mcp_dir_for(id);
        if let Err(e) = self.mint_mcp_token(id, &mcp_dir).await {
            let reason = format!("failed to write MCP token: {e}");
            running.insert(id.to_string(), VmSlot::Failed(reason.clone()));
            return Err(reason);
        }

        let cfg = match self.vm_config(id, mcp_dir) {
            Ok(cfg) => cfg,
            Err(e) => {
                let reason = e.to_string();
                running.insert(id.to_string(), VmSlot::Failed(reason.clone()));
                return Err(reason);
            }
        };

        let state = self.clone();
        let task_id = id.to_string();
        let handle = tokio::spawn(async move {
            let result = RunningVm::boot(cfg).await;

            // Resolve this workspace's enabled tool integrations (reads its
            // `gavmo.yaml`, fetches any referenced team spec) *before* taking
            // the running lock, so that network fetch can't block `status`/
            // `list` callers waiting on it. Only meaningful on a successful
            // boot; a failed boot exposes no tools.
            let integrations = if result.is_ok() {
                state.resolve_integrations_for(&task_id).await
            } else {
                Vec::new()
            };

            let mut running = state.inner.running.lock().await;

            // If the slot is no longer `Booting` (e.g. `stop`/`reset` raced
            // us — though those now `abort()` this very task first, so
            // reaching here with `still_wanted == false` mainly covers a
            // *different* open superseding us, not a stop/reset — or this
            // task itself was the loser of that race before `abort` even
            // registers), don't resurrect it in the map — but if we *did*
            // successfully boot a VM, shut it down rather than leaking a
            // running qemu process with no handle anywhere. Deliberately a
            // simple slot-variant check, not a task-identity comparison
            // (e.g. via `tokio::task::id()`): closing that last sliver —
            // another boot already having replaced us with its own
            // `Booting(handle2)` by the time we get here — is accepted as
            // out of scope for this pass, consistent with other small
            // accepted races already documented in this file (see
            // `alloc_port`).
            let still_wanted = matches!(running.get(&task_id), Some(VmSlot::Booting(_)));
            match result {
                Ok(vm) if still_wanted => {
                    state.set_integrations(&task_id, integrations);
                    running.insert(task_id, VmSlot::Running(Box::new(vm)));
                }
                Ok(vm) => {
                    drop(running);
                    let _ = vm.stop().await;
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
    /// If a boot is still in flight, this `abort()`s its task and awaits
    /// it before returning — so by the time this call returns, qemu is
    /// guaranteed to actually be gone, not just "asked to stop eventually".
    /// Two things can happen inside that awaited task, both of which tear
    /// qemu down: the abort takes effect at the task's next await point,
    /// dropping any `RunningVm` constructed so far (whose `Child` has
    /// `kill_on_drop` set); or the task was already past `RunningVm::boot`
    /// and racing us to its own tail, in which case it notices the slot is
    /// no longer `Booting` and performs its own graceful `vm.stop()` before
    /// returning. The latter case means this call can — rarely, only when
    /// a boot happens to succeed in the same instant it's being stopped —
    /// block for up to `SHUTDOWN_TIMEOUT` + `QUIT_TIMEOUT`
    /// (`vm::handle`'s constants, ~35s worst case) rather than returning
    /// instantly; accepted, since "stop" blocking until actually stopped is
    /// arguably more correct than the previous fire-and-forget behavior.
    pub async fn stop_workspace(&self, id: &str) -> Result<(), String> {
        // Invalidate this workspace's MCP capability token up front — a
        // stopped (or about-to-be-replaced-by-abort) VM should never be
        // able to authorize gateway calls again, regardless of how the
        // match below resolves. Idempotent and harmless if there was never
        // a token (a workspace that was never opened).
        self.clear_mcp_token(id);
        self.clear_integrations(id);

        let slot = self.inner.running.lock().await.remove(id);
        match slot {
            Some(VmSlot::Running(vm)) => vm.stop().await.map_err(|e| e.to_string()),
            Some(VmSlot::Booting(handle)) => {
                handle.abort();
                // Expected to be `Err` (cancelled) in the common case; if
                // the task won the race and ran to completion instead
                // (see doc comment above), it returns `Ok(())` - either
                // way there's nothing more for us to do here.
                let _ = handle.await;
                Ok(())
            }
            Some(VmSlot::Failed(_) | VmSlot::BuildFailed { .. }) | None => Ok(()),
        }
    }

    /// Stops the VM for a workspace (if running/booting) and forgets it —
    /// drops it from the registry entirely, so it no longer appears in the
    /// control panel. **Non-destructive**, unlike [`Self::reset_workspace`]:
    /// nothing under `.gavmo/vm` is deleted, so re-adding the same path
    /// later will find its old overlay (and OpenCode session history)
    /// again. A no-op if `id` isn't a known workspace.
    pub async fn remove_workspace(&self, id: &str) -> Result<(), String> {
        self.stop_workspace(id).await?;

        let mut registry = self.inner.registry.lock().await;
        registry.workspaces.retain(|w| w.path != id);
        save_registry(&self.inner.registry_path, &registry).map_err(|e| e.to_string())
    }

    /// Stops the VM for a workspace (if running/booting) and deletes its
    /// entire persistent state dir (`<repo>/.gavmo/vm` — the overlay disk,
    /// console log, QMP socket, and any M2 rebuild log/marker). The next
    /// `open_workspace` recreates a fresh overlay from the **current** base
    /// image.
    ///
    /// This is the general recovery/escape-hatch action for a wedged
    /// workspace, and specifically the fix for "the base image was rebuilt
    /// but this workspace's overlay is still pinned to the old one" (see
    /// `vm::disk::ensure_overlay` — an overlay's backing file is fixed at
    /// creation time; rebuilding the base image alone does nothing for
    /// workspaces that already have an overlay).
    ///
    /// **Destructive**: discards OpenCode's session history and the warm
    /// `/nix` store cache for this workspace. No undo. The caller (a
    /// `#[server]` function) is expected to have already gotten explicit
    /// user confirmation — this method itself does not prompt.
    ///
    /// Race-free against an in-flight boot: if the workspace is `Booting`,
    /// its task is `abort()`ed and awaited (same mechanism as
    /// [`Self::stop_workspace`] — see its doc comment for exactly what that
    /// guarantees and the rare bounded-wait tradeoff it accepts) before this
    /// function ever touches the overlay, so qemu is guaranteed to no
    /// longer be running — and therefore not still writing to
    /// `.gavmo/vm` — by the time it's deleted below.
    pub async fn reset_workspace(&self, id: &str) -> io::Result<()> {
        self.clear_mcp_token(id);
        self.clear_integrations(id);

        let slot = self.inner.running.lock().await.remove(id);
        match slot {
            Some(VmSlot::Running(vm)) => {
                let _ = vm.stop().await;
            }
            Some(VmSlot::Booting(handle)) => {
                handle.abort();
                let _ = handle.await;
            }
            Some(VmSlot::Failed(_) | VmSlot::BuildFailed { .. }) | None => {}
        }

        remove_dir_all_idempotent(&Self::state_dir_for(id)).await
    }

    /// Builds the per-VM [`VmConfig`] for a workspace path, allocating a
    /// fresh host port from the OS. `mcp_dir` is threaded in rather than
    /// recomputed here since the caller ([`Self::open_workspace`]) already
    /// needed it a step earlier, to mint/write that boot's MCP token before
    /// `VmConfig` existed.
    fn vm_config(&self, id: &str, mcp_dir: PathBuf) -> io::Result<VmConfig> {
        let repo_dir = PathBuf::from(id);
        let state_dir = Self::state_dir_for(id);
        let workspace_id = repo_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".to_string());
        let host_port = alloc_port()?;

        Ok(VmConfig {
            workspace_id,
            repo_dir,
            state_dir,
            base_image: self.inner.base_image.clone(),
            gcloud_dir: self.inner.gcloud_dir.clone(),
            mcp_dir,
            memory_mib: self.inner.memory_mib,
            vcpus: self.inner.vcpus,
            host_port,
        })
    }

    /// The conventional per-workspace state dir (`<repo>/.gavmo/vm` — see
    /// `vm::VmConfig::state_dir` / `plan.md`). A free function of `id` rather
    /// than something stored, since [`Self::read_build_log`] needs it even
    /// when there's no live `VmConfig` in scope (a `BuildFailed` slot can
    /// outlive the boot attempt that produced it).
    fn state_dir_for(id: &str) -> PathBuf {
        PathBuf::from(id).join(".gavmo").join("vm")
    }

    /// This workspace's MCP token share directory — **deliberately outside**
    /// any repo (unlike `state_dir_for`), under GAVMO's own app-data dir
    /// (`mcp_base_dir`, set in [`Self::new`]). See `VmConfig::mcp_dir`'s doc
    /// comment for why the token must not live inside the user's repo.
    /// `mcp_share_dir_name` turns the workspace's absolute-path identity
    /// into a stable, filesystem-safe (and repo-independent) directory name.
    fn mcp_dir_for(&self, id: &str) -> PathBuf {
        self.inner.mcp_base_dir.join(mcp_share_dir_name(id))
    }

    /// Mints a fresh MCP capability token for `id`, writes it to that
    /// workspace's token share as `token.env` (loaded by the guest's
    /// `gavmo-opencode` service via systemd's `EnvironmentFile` — see
    /// `nix/base/configuration.nix` — as `GAVMO_MCP_TOKEN`), and records it
    /// in the in-memory `mcp_tokens` map this struct's [`TokenAuthority`]
    /// impl resolves against. Drops any token this workspace held from a
    /// previous boot first: exactly one token is ever valid for a workspace
    /// at a time.
    async fn mint_mcp_token(&self, id: &str, dir: &Path) -> io::Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let token = uuid::Uuid::new_v4().to_string();
        let path = dir.join("token.env");
        tokio::fs::write(&path, format!("GAVMO_MCP_TOKEN={token}\n")).await?;

        // Best-effort: the share is already read-only in the guest and this
        // dir isn't inside any repo, but owner-only permissions on the host
        // copy are cheap insurance against another local user reading it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
        }

        let mut tokens = self.inner.mcp_tokens.lock().unwrap();
        tokens.retain(|_, v| v != id);
        tokens.insert(token, id.to_string());
        Ok(())
    }

    /// Invalidates whatever MCP token `id` currently holds, if any. Called
    /// whenever a workspace's VM is stopped or reset (see
    /// [`Self::stop_workspace`]/[`Self::reset_workspace`]) so a no-longer-
    /// running VM can never successfully call the gateway again, even if its
    /// `token.env` file is still sitting on disk.
    fn clear_mcp_token(&self, id: &str) {
        self.inner.mcp_tokens.lock().unwrap().retain(|_, v| v != id);
    }

    /// Resolves the *enabled* tool integrations for `id` (the configured subset
    /// the gateway exposes): enumerates requested tools (reads `gavmo.yaml` +
    /// fetches any team spec), then keeps only those with a configured secret via
    /// a batched `spawn_blocking` keyring check. Called from the boot task
    /// *before* the running lock is taken, so neither the team-spec HTTP fetch nor
    /// the keyring access blocks other `running`-lock callers.
    async fn resolve_integrations_for(&self, id: &str) -> Vec<ResolvedIntegration> {
        let candidates = self.candidates_for(id).await;
        let present = self
            .secrets_present(candidates.iter().map(|c| c.secret_key.clone()).collect())
            .await;
        candidates
            .into_iter()
            .filter(|candidate| present.contains(&candidate.secret_key))
            .map(CandidateTool::into_resolved)
            .collect()
    }

    /// Records the resolved integrations for a freshly-booted workspace, so
    /// this struct's [`ToolExecutor`] impl can advertise/dispatch them.
    fn set_integrations(&self, id: &str, integrations: Vec<ResolvedIntegration>) {
        self.inner
            .workspace_integrations
            .lock()
            .unwrap()
            .insert(id.to_string(), integrations);
    }

    /// Forgets a workspace's integrations. Called on stop/reset alongside
    /// [`Self::clear_mcp_token`] so a stopped VM exposes no tools (the cleared
    /// token already makes it unauthorized; this just keeps the map tidy).
    fn clear_integrations(&self, id: &str) {
        self.inner.workspace_integrations.lock().unwrap().remove(id);
    }

    /// The integrations currently resolved for `id` (empty if none/stopped).
    fn integrations_for(&self, id: &str) -> Vec<ResolvedIntegration> {
        self.inner
            .workspace_integrations
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Resolves *every* tool this workspace's `gavmo.yaml` requests
    /// (structurally, without touching the secret store), by reading the config
    /// and fetching any referenced team spec. Unlike the boot-time
    /// `workspace_integrations` cache (enabled subset, frozen at boot), this is
    /// computed live — so the control panel reflects config changes immediately.
    async fn candidates_for(&self, id: &str) -> Vec<CandidateTool> {
        let config = crate::integrations::read_config(id).await;
        crate::integrations::resolve_candidates(id, &config).await
    }

    /// Finds a single requested tool by id (structural; no secret-store access).
    async fn candidate(&self, id: &str, tool_id: &str) -> Result<CandidateTool, String> {
        self.candidates_for(id)
            .await
            .into_iter()
            .find(|candidate| candidate.id == tool_id)
            .ok_or_else(|| format!("unknown tool: {tool_id}"))
    }

    // --- keyring access helpers (always off the async worker) ---------------
    //
    // Every one of these runs the (synchronous, potentially slow — a locked
    // keyring can prompt) `SecretStore` call on a blocking thread via
    // `spawn_blocking`, so it can never stall a tokio worker. Combined with the
    // `async-io` keyring backend (see `crates/secrets/Cargo.toml`), this is what
    // makes keyring access safe from the daemon's async code.

    /// Fetches one secret value (`None` if absent or on backend error).
    async fn secret_get(&self, key: String) -> Option<String> {
        let store = self.inner.secret_store.clone();
        tokio::task::spawn_blocking(move || store.get(&key).ok().flatten())
            .await
            .unwrap_or(None)
    }

    /// Stores one secret value.
    async fn secret_set(&self, key: String, value: String) -> Result<(), String> {
        let store = self.inner.secret_store.clone();
        tokio::task::spawn_blocking(move || store.set(&key, &value))
            .await
            .map_err(|e| format!("secret store task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Deletes one secret value.
    async fn secret_delete(&self, key: String) -> Result<(), String> {
        let store = self.inner.secret_store.clone();
        tokio::task::spawn_blocking(move || store.delete(&key))
            .await
            .map_err(|e| format!("secret store task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Checks which of `keys` currently have a stored value, in a single
    /// blocking batch (one `spawn_blocking` for the whole set rather than one per
    /// key).
    async fn secrets_present(&self, keys: Vec<String>) -> HashSet<String> {
        let store = self.inner.secret_store.clone();
        tokio::task::spawn_blocking(move || {
            keys.into_iter()
                .filter(|key| store.get(key).ok().flatten().is_some())
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// The control panel's view of a workspace's requested tools and whether
    /// each has its credential configured (see [`crate::app::ToolConfig`]).
    pub async fn list_tool_configs(&self, id: &str) -> Vec<ToolConfig> {
        let candidates = self.candidates_for(id).await;
        let present = self
            .secrets_present(candidates.iter().map(|c| c.secret_key.clone()).collect())
            .await;

        candidates
            .into_iter()
            .map(|tool| ToolConfig {
                configured: present.contains(&tool.secret_key),
                id: tool.id,
                description: tool
                    .def
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("HTTP access to {}", tool.def.base_url)),
                base_url: tool.def.base_url,
                scope: match tool.scope_kind {
                    ScopeKind::Team => "team",
                    ScopeKind::Project => "project",
                }
                .to_string(),
                auth: match tool.def.auth {
                    teamspec::AuthScheme::Basic => "basic",
                    teamspec::AuthScheme::Bearer => "bearer",
                    teamspec::AuthScheme::Header => "header",
                }
                .to_string(),
                secret_name: tool.def.secret,
                instructions: tool.def.instructions,
            })
            .collect()
    }

    /// Stores (or replaces) a tool's credential in the secret store. For Basic
    /// auth both `username` and `token` are required; for Bearer/Header only
    /// `token`. The value is JSON-encoded in the shape the forwarder expects
    /// (see `crate::integrations`'s credential structs). Takes effect for a VM
    /// on its next open (a running VM's advertised tools are frozen at boot).
    pub async fn set_tool_secret(
        &self,
        id: &str,
        tool_id: &str,
        username: Option<String>,
        token: String,
    ) -> Result<(), String> {
        let tool = self.candidate(id, tool_id).await?;

        let value = match tool.def.auth {
            teamspec::AuthScheme::Basic => {
                let username = username
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| "basic auth requires a username/email".to_string())?;
                serde_json::json!({ "username": username, "password": token })
            }
            teamspec::AuthScheme::Bearer | teamspec::AuthScheme::Header => {
                serde_json::json!({ "token": token })
            }
        };

        self.secret_set(tool.secret_key, value.to_string()).await
    }

    /// Removes a tool's stored credential (disabling it on the next open).
    pub async fn clear_tool_secret(&self, id: &str, tool_id: &str) -> Result<(), String> {
        let tool = self.candidate(id, tool_id).await?;
        self.secret_delete(tool.secret_key).await
    }

    /// Live connectivity/credential test: forwards a `GET` (to the tool's
    /// configured `test_path`, or `/` if none) with the stored credential and
    /// returns the resulting status + body (`Ok` on 2xx, `Err` otherwise) so the
    /// control panel can show a red/green result. With a `test_path` pointing at
    /// an auth-reflecting endpoint (e.g. Jira's `rest/api/3/myself`) this is a
    /// real credential check; without one it's a bare reachability probe.
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

    /// Reads a build-failure log written by the guest's M2 `gavmo-rebuild`
    /// service (`nix/base/configuration.nix`) for the UI's "view build log"
    /// action. `log` must be exactly the filename reported in a workspace's
    /// `BuildFailed` status — validated strictly (see
    /// [`is_safe_rebuild_log_filename`]) since it becomes part of a
    /// filesystem path and nothing else constrains what a client can send a
    /// server function.
    pub async fn read_build_log(&self, id: &str, log: &str) -> io::Result<String> {
        if !is_safe_rebuild_log_filename(log) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid build log filename",
            ));
        }
        tokio::fs::read_to_string(Self::state_dir_for(id).join(log)).await
    }

    /// Reads and sanitizes the guest's boot console log (`console.log`
    /// under the workspace's state dir — see `vm::VmConfig::console_log_path`)
    /// for the live boot-console panel. The file doesn't exist until qemu
    /// has actually been spawned for this workspace at least once, so
    /// "not found" is treated as an empty log rather than an error — a
    /// perfectly normal state to poll (e.g. in the instant between
    /// `open_workspace` returning and the boot task actually spawning
    /// qemu), not a failure the caller needs to handle specially.
    ///
    /// Unlike `read_build_log`, `log` isn't a client-supplied filename —
    /// the path is always this one fixed name — so there's no filename
    /// validation to do here; `id` is the same registry-known workspace
    /// path `state_dir_for` already uses elsewhere.
    pub async fn read_console_log(&self, id: &str) -> io::Result<String> {
        let path = Self::state_dir_for(id).join("console.log");
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(sanitize_console(&bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// Host-side resource usage for a `Running` workspace's VM — CPU
    /// (top-style, per-core percentage), memory (RSS vs. the configured
    /// cap), uptime, overlay disk size, and the forwarded host port. For
    /// the live stats panel in the detail modal, polled while `Running`
    /// the same way [`Self::read_console_log`] is polled while `Booting`.
    ///
    /// Returns `None` whenever there's nothing meaningful to show: the
    /// workspace isn't `Running`, its qemu process has already exited
    /// (`RunningVm::qemu_pid` returning `None`), or a `/proc` read failed
    /// (e.g. the process exited in the narrow window between this
    /// function grabbing its pid and actually sampling it) — all treated
    /// as "no stats this tick" rather than an error a caller needs to
    /// handle specially, consistent with this being a best-effort,
    /// frequently-repolled display rather than a one-shot query.
    ///
    /// Deliberately does **not** hold the `running` lock across the
    /// `CPU_SAMPLE_INTERVAL` sleep between the two `/proc` samples this
    /// needs (see `vm::stats::cpu_percent_per_core`) — it copies out the
    /// handful of `Copy` values it needs from the `RunningVm` up front and
    /// releases the lock before sampling, so a concurrent `stop`/`reset`
    /// for this (or any other) workspace is never blocked behind a stats
    /// poll in flight.
    pub async fn vm_stats(&self, id: &str) -> Option<VmStats> {
        let (pid, uptime_secs, mem_cap_mib, vcpus, host_port) = {
            let mut running = self.inner.running.lock().await;
            match running.get_mut(id) {
                Some(VmSlot::Running(vm)) => (
                    vm.qemu_pid()?,
                    vm.uptime().as_secs(),
                    vm.memory_mib(),
                    vm.vcpus(),
                    vm.host_port(),
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

        Some(VmStats {
            cpu_percent,
            vcpus,
            mem_bytes: cur.rss_bytes,
            mem_cap_bytes: u64::from(mem_cap_mib) * 1024 * 1024,
            uptime_secs,
            disk_bytes,
            host_port,
        })
    }
}

/// Lets `crates/mcp`'s gateway resolve a bearer token to a workspace without
/// that crate depending on `gavmo` at all (see `mcp`'s own doc comment on
/// [`TokenAuthority`]) — `AppState` just answers from the in-memory
/// `mcp_tokens` map maintained by [`AppState::mint_mcp_token`]/
/// [`AppState::clear_mcp_token`].
impl TokenAuthority for AppState {
    fn resolve(&self, token: &str) -> Option<WorkspaceScope> {
        let tokens = self.inner.mcp_tokens.lock().unwrap();
        let workspace_id = tokens.get(token)?.clone();
        Some(WorkspaceScope { workspace_id })
    }
}

/// The daemon's tool implementation, consumed by the `mcp` gateway (see
/// [`mcp::ToolExecutor`]). This is the seam where real tools (the generic
/// host-locked HTTP forwarder, resolving credentials from the secret store)
/// will land in V1.3+. For now it ports P4.0's connectivity probe `gavmo_ping`
/// onto the trait so the gateway's behavior is unchanged: an authorized
/// workspace sees exactly `gavmo_ping`, and calling it echoes back the
/// workspace id.
impl ToolExecutor for AppState {
    fn list_tools(&self, scope: &WorkspaceScope) -> Vec<ToolDescriptor> {
        // Always expose the connectivity probe, plus one generic forwarder tool
        // per enabled integration for this workspace.
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

        // Otherwise it must be one of this workspace's integration tools. The
        // gateway already rejected names we never advertised, so a miss here is
        // only possible in a narrow race (tools cleared between list and call);
        // treat it as a tool-level error.
        match self
            .integrations_for(&scope.workspace_id)
            .into_iter()
            .find(|integration| integration.id == name)
        {
            Some(integration) => {
                // Fetch the credential value fresh at call time (never cached in
                // the resolved-integrations map, which holds only the key), via
                // `spawn_blocking` so the keyring round trip never stalls an async
                // worker (see `Self::secret_get`).
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
/// (discovery + call + auth) through the whole pipeline. Mirrors the
/// empty-object input schema the tool previously advertised from `crates/mcp`.
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

/// Deterministic, filesystem-safe directory name for a workspace's MCP
/// token share (`AppState::mcp_dir_for`), derived from its absolute-path
/// identity. Plain hex rather than e.g. base64: trivially correct (no
/// alphabet/padding edge cases to get right) and the names are internal
/// plumbing never meant to be read by a human, so hex's verbosity costs
/// nothing. Deliberately a *different* encoding from
/// `OPENCODE_WORKSPACE_SESSION_PATH`'s base64url — that one's shape is
/// dictated by opencode's own route scheme; this one is GAVMO's own
/// choice and has no reason to match it.
fn mcp_share_dir_name(id: &str) -> String {
    id.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Deletes a directory tree, treating "already gone" as success rather than
/// an error. Used by [`AppState::reset_workspace`] for `<repo>/.gavmo/vm`:
/// a workspace that was never opened (or was already reset) has no state
/// dir at all, and that's a perfectly fine outcome for a "make sure this is
/// gone" action — not a failure the caller needs to handle specially.
async fn remove_dir_all_idempotent(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()), // already gone
        Err(e) => Err(e),
    }
}

/// `true` only for filenames shaped exactly like what `gavmo-rebuild` writes
/// (`rebuild-<UTC timestamp>.log` — see `nix/base/configuration.nix`): no
/// path separators, no `..`, nothing that could escape the workspace's state
/// dir via a crafted server-function argument.
fn is_safe_rebuild_log_filename(name: &str) -> bool {
    name.starts_with("rebuild-")
        && name.ends_with(".log")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
}

/// Strips terminal control sequences out of a raw serial-console capture
/// (ANSI/VT colors, cursor moves, OSC title-setting, etc. — see
/// `qemu::build_qemu_args`'s `-serial file:...` redirect, which captures
/// the guest's console byte-for-byte, escape sequences included) down to
/// plain, readable text suitable for a `<pre>` in the UI. Also normalizes
/// line endings (`\r\n`/bare `\r` -> `\n`, and bare `\r` used as its own
/// line separator — common in boot logs for status lines that get
/// overwritten in place on a real terminal).
///
/// Deliberately *stripping* rather than translating to styled HTML: this
/// is plain-text-only, so a bare `\r` with no following `\n` is still
/// treated as a line break (there is no "overwrite the previous line"
/// concept left once colors/cursor-moves are gone) rather than losing that
/// output entirely.
///
/// Invalid UTF-8 (shouldn't normally happen - this is text the guest's
/// console actually printed - but a stray binary byte mid-boot is possible)
/// is replaced via `String::from_utf8_lossy` rather than erroring, since a
/// slightly-mangled live boot log is far more useful than none at all.
fn sanitize_console(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // ESC - the start of some control sequence. Figure out
                // which kind and skip exactly it, leaving the cursor right
                // after the sequence so surrounding text is preserved.
                match chars.peek() {
                    // CSI: `ESC [ ... final` where `final` is `@`-`~`
                    // (0x40-0x7E) - e.g. `ESC[0;32m` (SGR color).
                    Some('[') => {
                        chars.next();
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&c) {
                                break;
                            }
                        }
                    }
                    // String-type sequences, all terminated the same way
                    // (BEL, or the two-byte ST `ESC \`) regardless of which
                    // one introduces them: OSC (`]` - e.g. terminal
                    // title-setting), DCS (`P` - e.g. GRUB's
                    // `ESC P + q <hex> ESC \` seen in real boot captures),
                    // APC (`_`), PM (`^`).
                    Some(']') | Some('P') | Some('_') | Some('^') => {
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
                    // Any other two-byte escape (e.g. `ESC c` - full
                    // reset, `ESC 7`/`ESC 8` - save/restore cursor): just
                    // swallow the one byte that follows ESC.
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            // A bare `\r` (not already consumed as part of `\r\n` by the
            // match arm below) stands in for a line break once cursor
            // control is gone - see doc comment above.
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

/// Allocates a free TCP port by binding to `127.0.0.1:0` and taking what
/// the OS hands back. There's an inherent (small) race between releasing
/// the socket and QEMU binding it; accepted for the MVP (see plan §2).
fn alloc_port() -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
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

    /// A real-shaped snippet from an actual `console.log` capture (systemd's
    /// colored `[  OK  ]` status lines use SGR color + bold, `\r\n` line
    /// endings) should come out as the plain text a human would read on a
    /// real terminal.
    #[test]
    fn sanitize_console_strips_systemd_sgr_color_sequences() {
        let raw =
            "[\u{1b}[0;32m  OK  \u{1b}[0m] Started \u{1b}[0;1;39mJournal Service\u{1b}[0m.\r\n";
        assert_eq!(
            sanitize_console(raw.as_bytes()),
            "[  OK  ] Started Journal Service.\n"
        );
    }

    /// OSC sequences (e.g. terminal title-setting, seen in real boot logs
    /// right as systemd takes over the console) are skipped in their
    /// entirety, whether terminated by BEL or the two-byte ST (`ESC \`).
    #[test]
    fn sanitize_console_strips_osc_sequences() {
        let bel_terminated = "before\u{1b}]0;some title\u{7}after\n";
        assert_eq!(sanitize_console(bel_terminated.as_bytes()), "beforeafter\n");

        let st_terminated = "before\u{1b}]0;some title\u{1b}\\after\n";
        assert_eq!(sanitize_console(st_terminated.as_bytes()), "beforeafter\n");
    }

    /// DCS sequences (`ESC P ... ST`) — seen in real captures from GRUB's
    /// terminal-capability probing (`ESC P + q <hex> ESC \`) — are stripped
    /// in their entirety, same as OSC.
    #[test]
    fn sanitize_console_strips_dcs_sequences() {
        assert_eq!(
            sanitize_console("before\u{1b}P+q6E616D65\u{1b}\\after\n".as_bytes()),
            "beforeafter\n"
        );
    }

    /// Other two-byte escapes (full reset `ESC c`, cursor save/restore
    /// `ESC 7`/`ESC 8` — all seen in the GRUB/early-boot portion of a real
    /// capture) are swallowed without disturbing surrounding text.
    #[test]
    fn sanitize_console_strips_other_two_byte_escapes() {
        assert_eq!(
            sanitize_console("a\u{1b}cb\u{1b}7c\u{1b}8d".as_bytes()),
            "abcd"
        );
    }

    /// A bare `\r` (no following `\n`) still becomes a line break — the
    /// right behavior once cursor-control sequences are gone, since
    /// there's no "overwrite the previous line" concept left to honor; the
    /// output would otherwise run two unrelated status lines together.
    #[test]
    fn sanitize_console_treats_bare_cr_as_a_line_break() {
        assert_eq!(sanitize_console(b"one\rtwo\r\nthree"), "one\ntwo\nthree");
    }

    /// Plain text with no escapes or special line endings passes through
    /// unchanged.
    #[test]
    fn sanitize_console_passes_plain_text_through() {
        assert_eq!(
            sanitize_console(b"Booting from Hard Disk...\n"),
            "Booting from Hard Disk...\n"
        );
    }

    /// URL-safe base64, no padding — a std-only reimplementation of
    /// opencode's `base64Encode` (`packages/core/src/util/encode.ts`:
    /// `btoa(...).replace(/\+/g, "-").replace(/\//g, "_").replace(/=/g, "")`),
    /// kept intentionally independent of `OPENCODE_WORKSPACE_SESSION_PATH`
    /// so this test actually catches drift rather than just restating the
    /// constant.
    fn base64_url_nopad(input: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
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

    /// Guards `OPENCODE_WORKSPACE_SESSION_PATH` against drift: if the guest
    /// mount path ever stops being `/workspace` (see `MOUNT_TAG_WORKSPACE`
    /// in `crates/vm/src/handle.rs`), this fails loudly instead of letting
    /// "Open UI" silently deep-link to the wrong directory.
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
        // Mirrors the `match &e` in `open_workspace`'s background task -
        // exercised directly here since that task itself isn't unit-testable
        // without a real `RunningVm::boot`.
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

    /// Guards the one real attack surface in `read_build_log`: a client
    /// fully controls the `log` argument of a `#[server]` function, and it
    /// becomes part of a filesystem path.
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
    }

    /// A unique scratch dir under the OS temp dir, named with the test's own
    /// identity (PID + a monotonic counter) so concurrent `cargo test`
    /// threads can't collide. No `tempfile` crate dependency needed for
    /// this one narrow use.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("gavmo-test-{tag}-{}-{n}", std::process::id()))
    }

    /// [`AppState::reset_workspace`]'s core guarantee, exercised directly on
    /// [`remove_dir_all_idempotent`] (the `AppState` method itself also
    /// touches the `running` map and the real registry dir, which isn't
    /// worth standing up just to test "delete this directory tree"):
    /// deletes a populated state dir, including nested files, in one shot.
    #[tokio::test]
    async fn remove_dir_all_idempotent_deletes_an_existing_tree() {
        let dir = unique_temp_dir("reset-exists");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("disk.qcow2"), b"fake overlay")
            .await
            .unwrap();

        remove_dir_all_idempotent(&dir).await.unwrap();

        assert!(!dir.exists());
    }

    /// The other half of "delete this directory tree" being a safe,
    /// idempotent escape hatch: a workspace that was never opened (or was
    /// already reset) has no state dir at all, and that must be `Ok(())`,
    /// not a `NotFound` error the UI would otherwise have to special-case.
    #[tokio::test]
    async fn remove_dir_all_idempotent_is_a_noop_when_already_gone() {
        let dir = unique_temp_dir("reset-missing");
        assert!(!dir.exists());

        remove_dir_all_idempotent(&dir).await.unwrap();
    }

    /// A throwaway [`AppState`] for exercising `stop_workspace`/
    /// `reset_workspace`'s interaction with the `running` map directly.
    /// Unlike a `remove_workspace`/`add_workspace` test (which would
    /// persist to the *real*, shared `~/.local/share/gavmo/workspaces.json`
    /// via `save_registry` — not something a unit test should touch),
    /// `AppState::new` only *reads* that file, and neither method under
    /// test here touches the registry at all (only `self.inner.running`
    /// and, for `reset`, a throwaway temp dir) — so standing one up is
    /// safe. The base-image/gcloud-dir paths are never used by the code
    /// paths under test (no real `RunningVm::boot` happens in either test
    /// below), so placeholder paths are fine.
    fn test_app_state() -> AppState {
        AppState::new(
            PathBuf::from("/nonexistent/gavmo-test-base.qcow2"),
            PathBuf::from("/nonexistent/gavmo-test-gcloud"),
        )
        .expect("AppState::new only touches the real registry file, which is harmless to read")
    }

    /// A `VmSlot::Booting` wrapping a task that sleeps far longer than any
    /// sane test timeout, standing in for an in-flight `RunningVm::boot`
    /// call. Used to prove `stop_workspace`/`reset_workspace` actually
    /// `abort()` it (returning promptly) rather than waiting for it to
    /// finish naturally.
    fn never_finishing_boot_slot() -> VmSlot {
        VmSlot::Booting(tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        }))
    }

    /// The core of R1 (see `VmSlot::Booting`'s doc comment): `stop_workspace`
    /// on a `Booting` slot must `abort()` the boot task and return once it's
    /// actually gone, not leave it running in the background. Bounded by an
    /// outer `timeout` so a regression back to "wait for the boot to finish
    /// naturally" fails this test instead of hanging the test suite.
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

        tokio::time::timeout(std::time::Duration::from_secs(5), state.stop_workspace(id))
            .await
            .expect("stop_workspace should abort the in-flight boot promptly, not wait for it")
            .unwrap();

        assert!(state.inner.running.lock().await.get(id).is_none());
    }

    /// The other half of R1: `reset_workspace` must abort an in-flight boot
    /// *before* touching the overlay, so by the time it deletes
    /// `.gavmo/vm`, nothing (qemu included) is still writing to it. Proven
    /// here by actually populating a throwaway state dir and asserting it's
    /// gone afterward, combined with the same promptness bound as above.
    #[tokio::test]
    async fn reset_workspace_aborts_an_in_flight_boot_before_deleting_state() {
        let repo_dir = unique_temp_dir("reset-abort-repo");
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

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            state.reset_workspace(&id),
        )
        .await
        .expect("reset_workspace should abort the in-flight boot promptly, not wait for it")
        .unwrap();

        assert!(!state_dir.exists());
        assert!(state.inner.running.lock().await.get(&id).is_none());
    }

    /// Pure, deterministic, and distinct for distinct inputs — the basic
    /// contract `AppState::mcp_dir_for` relies on to avoid two workspaces
    /// colliding on the same token-share directory.
    #[test]
    fn mcp_share_dir_name_is_deterministic_and_filesystem_safe() {
        let a = mcp_share_dir_name("/home/user/project-a");
        let b = mcp_share_dir_name("/home/user/project-b");

        assert_eq!(a, mcp_share_dir_name("/home/user/project-a"));
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    /// The core round trip [`AppState::mint_mcp_token`] /
    /// [`TokenAuthority::resolve`] must support: minting a token for a
    /// workspace writes a guest-loadable `token.env` *and* makes that exact
    /// token resolve back to the same workspace id.
    #[tokio::test]
    async fn mint_mcp_token_writes_env_file_and_resolves_via_token_authority() {
        let state = test_app_state();
        let id = "mint-mcp-token-writes-env-file-and-resolves";
        let dir = unique_temp_dir("mcp-token-mint");

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

    /// A workspace holds exactly one *currently valid* token at a time:
    /// re-minting (a fresh `open_workspace` boot) must invalidate whatever
    /// token a previous boot handed out, not leave both valid.
    #[tokio::test]
    async fn minting_a_new_token_invalidates_the_previous_one_for_the_same_workspace() {
        let state = test_app_state();
        let id = "minting-a-new-token-invalidates-the-previous-one";
        let dir = unique_temp_dir("mcp-token-remint");

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

    /// [`AppState::clear_mcp_token`] (called from `stop_workspace`/
    /// `reset_workspace`) must make a workspace's token stop resolving
    /// immediately — a stopped VM's capability token must not keep working.
    #[tokio::test]
    async fn clear_mcp_token_invalidates_resolution() {
        let state = test_app_state();
        let id = "clear-mcp-token-invalidates-resolution";
        let dir = unique_temp_dir("mcp-token-clear");

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

    /// An unknown token (never minted) must resolve to nothing — the
    /// baseline "unauthenticated" case every real MCP call that's missing
    /// or has a garbage bearer token hits.
    #[test]
    fn resolve_returns_none_for_an_unknown_token() {
        let state = test_app_state();
        assert_eq!(TokenAuthority::resolve(&state, "never-minted"), None);
    }
}
