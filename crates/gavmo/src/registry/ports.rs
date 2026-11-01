//! Stable host-port allocation per workspace.

use std::io;
use std::net::TcpListener;

use super::AppState;
use super::store::{Registry, commit_registry};

impl AppState {
    /// Returns the workspace's persisted host port if still free, otherwise
    /// allocates and persists a new one. A stable port keeps the browser
    /// origin (and opencode's per-origin `localStorage`) stable across restarts.
    ///
    /// Never call while holding `running` (lock order is registry then
    /// running), nor while this workspace's VM is up (its own VM holds the
    /// port, which would force a needless reallocation).
    pub(super) async fn resolve_host_port(&self, id: &str) -> io::Result<u16> {
        let mut registry = self.inner.registry.lock().await;
        let mut updated = registry.clone();
        let (port, changed) = resolve_host_port_in(&mut updated, id)?;
        if changed {
            commit_registry(&mut registry, &self.inner.registry_path, updated).await?;
        }
        Ok(port)
    }
}

/// Allocates a free TCP port by binding to `127.0.0.1:0`. The small race
/// between releasing the socket and QEMU binding it is accepted.
pub(super) fn alloc_port() -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Reuses `existing` if it is currently bindable, else allocates a fresh
/// port. Same accepted check-then-bind race as [`alloc_port`].
pub(super) fn choose_host_port(existing: Option<u16>) -> io::Result<u16> {
    let reusable = existing.filter(|&port| TcpListener::bind(("127.0.0.1", port)).is_ok());
    match reusable {
        Some(port) => Ok(port),
        None => alloc_port(),
    }
}

/// Chooses `id`'s host port and updates its [`WorkspaceRecord`] if the port
/// changed. Returns `(port, changed)` so the caller only saves when needed.
/// An unknown `id` gets a fresh port and leaves `registry` untouched.
pub(super) fn resolve_host_port_in(registry: &mut Registry, id: &str) -> io::Result<(u16, bool)> {
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
