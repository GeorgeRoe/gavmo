//! Server-side (ssr-only) state for the GAVMO daemon.
//!
//! Holds the persistent workspace registry (a JSON file under a global app
//! dir) and the in-memory map of currently-running VMs. This module is the
//! only place that touches the `vm` crate, keeping all VM-management
//! mechanism behind the daemon's server functions. See
//! `crates/gavmo/plan.md` §§6–7.

use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use vm::{Error as VmError, RunningVm, VmConfig, VmStatus};

use crate::app::{WorkspaceInfo, WorkspaceStatus};

/// Default guest memory, in MiB.
const DEFAULT_MEMORY_MIB: u32 = 4096;
/// Default guest vCPU count.
const DEFAULT_VCPUS: u32 = 4;

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

        Ok(Self {
            inner: Arc::new(Inner {
                registry_path,
                registry: Mutex::new(registry),
                running: Mutex::new(HashMap::new()),
                base_image,
                gcloud_dir,
                memory_mib: DEFAULT_MEMORY_MIB,
                vcpus: DEFAULT_VCPUS,
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
        let canonical = std::fs::canonicalize(&path)?
            .to_string_lossy()
            .into_owned();

        let mut registry = self.inner.registry.lock().await;
        if !registry.workspaces.iter().any(|w| w.path == canonical) {
            registry.workspaces.push(WorkspaceRecord { path: canonical });
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

        let cfg = match self.vm_config(id) {
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
    /// fresh host port from the OS.
    fn vm_config(&self, id: &str) -> io::Result<VmConfig> {
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

    /// URL-safe base64, no padding — a std-only reimplementation of
    /// opencode's `base64Encode` (`packages/core/src/util/encode.ts`:
    /// `btoa(...).replace(/\+/g, "-").replace(/\//g, "_").replace(/=/g, "")`),
    /// kept intentionally independent of `OPENCODE_WORKSPACE_SESSION_PATH`
    /// so this test actually catches drift rather than just restating the
    /// constant.
    fn base64_url_nopad(input: &str) -> String {
        const ALPHABET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
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
        assert!(!is_safe_rebuild_log_filename(
            "/etc/gavmo/system/flake.nix"
        ));
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
        std::env::temp_dir().join(format!(
            "gavmo-test-{tag}-{}-{n}",
            std::process::id()
        ))
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

        tokio::time::timeout(std::time::Duration::from_secs(5), state.reset_workspace(&id))
            .await
            .expect("reset_workspace should abort the in-flight boot promptly, not wait for it")
            .unwrap();

        assert!(!state_dir.exists());
        assert!(state.inner.running.lock().await.get(&id).is_none());
    }
}
