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
use vm::{RunningVm, VmConfig, VmStatus};

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
    /// `RunningVm::boot` is in progress.
    Booting,
    /// Booted and (as of the last check) healthy. Boxed: `RunningVm` is
    /// much larger than the other variants (it owns a `tokio::process::Child`
    /// plus helper-process handles), and this enum lives in a `HashMap`.
    Running(Box<RunningVm>),
    /// The last boot attempt failed; kept around so the reason is visible
    /// in the UI until the next `open_workspace` call replaces it.
    Failed(String),
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
        {
            let mut running = self.inner.running.lock().await;
            match running.get(id) {
                // Already booting or running: nothing to do.
                Some(VmSlot::Booting | VmSlot::Running(_)) => return Ok(()),
                // No entry, or the last attempt failed: (re)boot below.
                Some(VmSlot::Failed(_)) | None => {}
            }
            running.insert(id.to_string(), VmSlot::Booting);
        }

        let cfg = match self.vm_config(id) {
            Ok(cfg) => cfg,
            Err(e) => {
                let reason = e.to_string();
                self.inner
                    .running
                    .lock()
                    .await
                    .insert(id.to_string(), VmSlot::Failed(reason.clone()));
                return Err(reason);
            }
        };

        let state = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let result = RunningVm::boot(cfg).await;
            let mut running = state.inner.running.lock().await;

            // If the slot is no longer `Booting` (e.g. the user pressed
            // Stop while we were booting, or another open raced us), don't
            // resurrect it in the map — but if we *did* successfully boot a
            // VM, shut it down rather than leaking a running qemu process
            // with no handle anywhere.
            let still_wanted = matches!(running.get(&id), Some(VmSlot::Booting));
            match result {
                Ok(vm) if still_wanted => {
                    running.insert(id, VmSlot::Running(Box::new(vm)));
                }
                Ok(vm) => {
                    drop(running);
                    let _ = vm.stop().await;
                }
                Err(e) if still_wanted => {
                    running.insert(id, VmSlot::Failed(e.to_string()));
                }
                Err(_) => {}
            }
        });

        Ok(())
    }

    /// Stops the VM for a workspace and drops it from the running map. A
    /// no-op if it isn't currently running or booting. If a boot is still
    /// in flight, removing the `Booting` marker here signals the
    /// background task (see `open_workspace`) to shut the VM down once it
    /// finishes, instead of registering it as running.
    pub async fn stop_workspace(&self, id: &str) -> Result<(), String> {
        let slot = self.inner.running.lock().await.remove(id);
        match slot {
            Some(VmSlot::Running(vm)) => vm.stop().await.map_err(|e| e.to_string()),
            Some(VmSlot::Booting | VmSlot::Failed(_)) | None => Ok(()),
        }
    }

    /// Builds the per-VM [`VmConfig`] for a workspace path, allocating a
    /// fresh host port from the OS.
    fn vm_config(&self, id: &str) -> io::Result<VmConfig> {
        let repo_dir = PathBuf::from(id);
        let state_dir = repo_dir.join(".gavmo").join("vm");
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
}

/// Maps a [`VmSlot`] to the serializable view type shared with the browser,
/// probing the live `vm` crate status for a `Running` slot (the qemu
/// process may have exited or stopped answering health checks since the
/// last poll).
async fn slot_status(slot: &mut VmSlot) -> WorkspaceStatus {
    match slot {
        VmSlot::Booting => WorkspaceStatus::Booting,
        VmSlot::Running(vm) => to_status(vm.status().await),
        VmSlot::Failed(reason) => WorkspaceStatus::Failed {
            reason: reason.clone(),
        },
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
}
