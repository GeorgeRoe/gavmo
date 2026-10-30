//! Leptos app: shared view types, the server-function API, and the
//! control-panel UI. Compiled for both `ssr` and `hydrate`; `#[server]`
//! bodies only compile on the server, where they use the registry directly.

use leptos::prelude::*;
use leptos_meta::{MetaTags, Stylesheet, Title, provide_meta_context};
use leptos_router::{
    StaticSegment,
    components::{Route, Router, Routes},
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shared view types (serialized across the server-function boundary)
// ---------------------------------------------------------------------------

/// A known workspace plus its current VM status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub path: String,
    pub status: WorkspaceStatus,
}

/// Browser-facing mirror of `vm::VmStatus`, plus a `BuildFailed` case
/// (from `vm::Error::RebuildFailed`) that `vm::VmStatus` doesn't have.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceStatus {
    Stopped,
    Booting,
    /// A stop, reset or remove is in progress.
    Stopping,
    Running {
        url: String,
    },
    Failed {
        reason: String,
    },
    /// The repo's `gavmo.nix` failed to build (detected via the guest's
    /// failure marker, not a boot timeout). `log` is the failed run's log
    /// filename, fetchable via [`read_build_log`].
    BuildFailed {
        log: String,
    },
}

/// Resource usage for a `Running` workspace's VM, shown in the detail modal's
/// stats panel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmStats {
    /// Top-style CPU usage: `0` idle, `100` per fully-busy core, up to
    /// `vcpus * 100` if every core is saturated.
    pub cpu_percent: f32,
    /// Configured guest vCPU count (the scale for `cpu_percent`).
    pub vcpus: u32,
    /// Guest-reported used memory in bytes (`total - MemAvailable`, via the
    /// virtio-balloon driver). Not qemu's RSS: with virtiofs the shared memfd
    /// keeps reclaimable page cache resident, so RSS reads near 100%.
    /// `None` until the balloon driver first reports; show a placeholder.
    pub mem_used_bytes: Option<u64>,
    /// Guest-reported total RAM in bytes; `None` together with `mem_used_bytes`.
    pub mem_total_bytes: Option<u64>,
    /// Seconds since the qemu process was spawned (not since it became healthy).
    pub uptime_secs: u64,
    /// Size of the persistent overlay disk (`~/.local/share/gavmo/vms/<hash>/disk.qcow2`), in bytes.
    pub disk_bytes: u64,
    /// The host port `opencode web` is forwarded to.
    pub host_port: u16,
}

/// A directory listing for the folder picker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirListing {
    /// Absolute path currently being browsed.
    pub path: String,
    /// Parent directory, if any (for an ".." entry).
    pub parent: Option<String>,
    /// Immediate sub-directories, sorted by name.
    pub entries: Vec<DirEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
}

/// A tool requested by a workspace's `gavmo.yaml`, for the Tools panel.
/// `auth` is a plain string (not the `ssr`-only `toolspec` enum) so this type
/// also builds for hydrate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolConfig {
    /// Tool id (also the name the agent calls it by).
    pub id: String,
    /// Human-facing description from the config (or a generated fallback).
    pub description: String,
    /// The integration's base URL, shown so the user can verify where their
    /// credential will be sent.
    pub base_url: String,
    /// `"basic"`, `"bearer"`, or `"header"`; selects the configure form's fields.
    pub auth: String,
    /// The secret reference name declared in the config.
    pub secret_name: String,
    /// Optional setup help from the config.
    pub instructions: Option<String>,
    /// A credential is stored *and* bound to the current `base_url` (usable).
    pub configured: bool,
    /// A credential is stored but was approved for a different URL; the tool
    /// stays disabled until the user re-confirms it.
    pub url_changed: bool,
    /// The URL the stored credential was approved for, shown in the mismatch
    /// warning. `None` if the stored value carries no binding.
    pub bound_url: Option<String>,
    /// How this tool sends the credential, e.g. "the X-API-Key header".
    pub delivery: String,
    /// `Some` when a credential is stored for this destination but was
    /// approved to be sent differently (description of the approved way); the
    /// tool stays disabled until the user re-confirms it.
    pub approved_delivery: Option<String>,
}

// ---------------------------------------------------------------------------
// Server functions (the entire API surface)
// ---------------------------------------------------------------------------

#[server]
pub async fn list_workspaces() -> Result<Vec<WorkspaceInfo>, ServerFnError> {
    let state = app_state()?;
    Ok(state.list().await)
}

#[server]
pub async fn poll_status(id: String) -> Result<WorkspaceStatus, ServerFnError> {
    let state = app_state()?;
    Ok(state.status(&id).await)
}

/// Fetches the full `nixos-rebuild` output for a
/// [`WorkspaceStatus::BuildFailed`] workspace. `log` must be the filename that
/// status reported; the server validates it strictly before reading.
#[server]
pub async fn read_build_log(id: String, log: String) -> Result<String, ServerFnError> {
    let state = app_state()?;
    state
        .read_build_log(&id, &log)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Fetches the guest's boot console (kernel + systemd output), sanitized for
/// display. The log lives at a fixed path, so there is no client-controlled
/// filename to validate.
#[server]
pub async fn read_console_log(id: String) -> Result<String, ServerFnError> {
    let state = app_state()?;
    state
        .read_console_log(&id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Fetches resource usage for a workspace's VM. `Ok(None)` means nothing to
/// show right now (not `Running`, or qemu just exited); `Err` means the
/// request failed.
#[server]
pub async fn poll_vm_stats(id: String) -> Result<Option<VmStats>, ServerFnError> {
    let state = app_state()?;
    Ok(state.vm_stats(&id).await)
}

#[server]
pub async fn add_workspace(path: String) -> Result<(), ServerFnError> {
    let state = app_state()?;
    state
        .add_workspace(path)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Starts booting the workspace's VM (no-op if already booting/running) and
/// returns immediately; the client follows progress via the polled list so
/// the UI never blocks for the whole boot.
#[server]
pub async fn open_workspace(id: String) -> Result<(), ServerFnError> {
    let state = app_state()?;
    state.open_workspace(&id).await.map_err(ServerFnError::new)
}

#[server]
pub async fn stop_workspace(id: String) -> Result<(), ServerFnError> {
    let state = app_state()?;
    state.stop_workspace(&id).await.map_err(ServerFnError::new)
}

/// Stops (if needed) and wipes a workspace's VM state, e.g. to recover from
/// an overlay built on a stale base image. **Destructive**: the client must
/// confirm with the user first; this function does not.
#[server]
pub async fn reset_workspace(id: String) -> Result<(), ServerFnError> {
    let state = app_state()?;
    state
        .reset_workspace(&id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Stops (if needed) and drops a workspace from the registry. Its VM disk
/// and `.gavmo/` are kept, so re-adding the path restores its state. The
/// client must confirm with the user first.
#[server]
pub async fn remove_workspace(id: String) -> Result<(), ServerFnError> {
    let state = app_state()?;
    state
        .remove_workspace(&id)
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn browse_dir(path: Option<String>) -> Result<DirListing, ServerFnError> {
    use std::path::PathBuf;

    let dir = match path {
        Some(p) => PathBuf::from(p),
        None => dirs::home_dir().ok_or_else(|| ServerFnError::new("no home directory"))?,
    };
    let dir = std::fs::canonicalize(&dir)
        .map_err(|e| ServerFnError::new(format!("{}: {e}", dir.display())))?;

    let mut entries = Vec::new();
    let read = std::fs::read_dir(&dir)
        .map_err(|e| ServerFnError::new(format!("{}: {e}", dir.display())))?;
    for entry in read.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // hide dotfiles/dirs from the picker
        }
        entries.push(DirEntry {
            name,
            path: entry.path().to_string_lossy().into_owned(),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(DirListing {
        path: dir.to_string_lossy().into_owned(),
        parent: dir.parent().map(|p| p.to_string_lossy().into_owned()),
        entries,
    })
}

/// Lists the tools a workspace's `gavmo.yaml` requests, with their configured
/// status. Read live, so config/secret changes show without a restart.
#[server]
pub async fn list_tool_configs(id: String) -> Result<Vec<ToolConfig>, ServerFnError> {
    let state = app_state()?;
    Ok(state.list_tool_configs(&id).await)
}

/// Stores (or replaces) a tool's credential on the host. `username` is only used
/// for Basic auth; Bearer/Header pass just `token`. The credential never leaves
/// the host.
#[server]
pub async fn set_tool_secret(
    id: String,
    tool_id: String,
    username: Option<String>,
    token: String,
) -> Result<(), ServerFnError> {
    let state = app_state()?;
    state
        .set_tool_secret(&id, &tool_id, username, token)
        .await
        .map_err(ServerFnError::new)
}

/// Removes a tool's stored credential (disabling it on the workspace's next open).
#[server]
pub async fn clear_tool_secret(id: String, tool_id: String) -> Result<(), ServerFnError> {
    let state = app_state()?;
    state
        .clear_tool_secret(&id, &tool_id)
        .await
        .map_err(ServerFnError::new)
}

/// Live connectivity/credential test for a configured tool: forwards a `GET /`
/// with the stored credential and returns the status + body. `Err` is returned
/// for a non-2xx (or transport failure) so the UI can show a red result.
#[server]
pub async fn test_tool(id: String, tool_id: String) -> Result<String, ServerFnError> {
    let state = app_state()?;
    state
        .test_tool(&id, &tool_id)
        .await
        .map_err(ServerFnError::new)
}

/// Fetches the injected [`registry::AppState`] from Leptos context.
#[cfg(feature = "ssr")]
fn app_state() -> Result<crate::registry::AppState, ServerFnError> {
    use_context::<crate::registry::AppState>()
        .ok_or_else(|| ServerFnError::new("app state not available"))
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <AutoReload options=options.clone() />
                <HydrationScripts options/>
                <MetaTags/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    view! {
        <Stylesheet id="leptos" href="/pkg/gavmo.css"/>
        <Title text="GAVMO"/>
        <Router>
            <main class="page">
                <Routes fallback=|| "Page not found.".into_view()>
                    <Route path=StaticSegment("") view=HomePage/>
                </Routes>
            </main>
        </Router>
    }
}

// ---------------------------------------------------------------------------
// Card <-> modal morph (View Transitions API)
// ---------------------------------------------------------------------------

/// A stable `view-transition-name` for a workspace. Ids are filesystem paths,
/// which aren't valid CSS idents, so the id is hashed.
fn vt_name(id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    // Prefixed because a CSS ident must not start with a digit.
    format!("ws-vt-{:x}", hasher.finish())
}

/// The element a click listener is attached to (`current_target`, so clicks
/// on children still resolve to the card). Always `None` under `ssr`; the
/// shared signature spares callers a `#[cfg]` split.
fn click_target(ev: &web_sys::MouseEvent) -> Option<web_sys::HtmlElement> {
    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::JsCast;
        ev.current_target()?.dyn_into::<web_sys::HtmlElement>().ok()
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = ev;
        None
    }
}

/// Sets or clears (`None`) a card's inline `view-transition-name` directly,
/// bypassing reactive `style:` so it applies synchronously inside the
/// transition callback. No-op under `ssr`.
fn set_card_vt_name(el: &web_sys::HtmlElement, name: Option<&str>) {
    #[cfg(feature = "hydrate")]
    {
        let style = el.style();
        match name {
            Some(name) => {
                if let Err(e) = style.set_property("view-transition-name", name) {
                    leptos::logging::warn!("failed to set view-transition-name: {e:?}");
                }
            }
            None => {
                if let Err(e) = style.remove_property("view-transition-name") {
                    leptos::logging::warn!("failed to clear view-transition-name: {e:?}");
                }
            }
        }
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (el, name);
    }
}

/// Scrolls an element to the bottom, keeping the live console pinned to the
/// newest output like a terminal. No-op under `ssr`.
fn scroll_to_bottom(el: &web_sys::HtmlPreElement) {
    #[cfg(feature = "hydrate")]
    {
        el.set_scroll_top(el.scroll_height());
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = el;
    }
}

/// Runs `update` (a signal write) inside `document.startViewTransition` so
/// the card <-> modal change morphs; without the API, both closures just run
/// in order (instant open/close). Looked up via `Reflect`, as
/// `leptos_router` does, to avoid `web_sys_unstable_apis`.
///
/// Signal writes patch the DOM on a later tick, so the callback awaits
/// `tick()` before resolving; otherwise the browser would snapshot the old DOM.
///
/// `on_finished` runs once the whole animation has played (`.finished`), so
/// the closing morph can release the card's name without leaking it into a
/// later transition.
#[cfg(feature = "hydrate")]
fn start_view_transition(update: impl FnOnce() + 'static, on_finished: impl FnOnce() + 'static) {
    use js_sys::Reflect;
    use wasm_bindgen::{JsCast, JsValue, closure::Closure};
    use wasm_bindgen_futures::{JsFuture, future_to_promise};

    let document = document();
    let svt = Reflect::get(&document, &JsValue::from_str("startViewTransition"))
        .and_then(JsCast::dyn_into::<js_sys::Function>);

    let Ok(svt) = svt else {
        update();
        on_finished();
        return;
    };

    let cb = Closure::once_into_js(move || {
        future_to_promise(async move {
            update();
            leptos::task::tick().await;
            Ok(JsValue::UNDEFINED)
        })
    });
    match svt.call1(document.unchecked_ref(), &cb) {
        Ok(view_transition) => {
            let finished = Reflect::get(&view_transition, &JsValue::from_str("finished"))
                .ok()
                .and_then(|p| p.dyn_into::<js_sys::Promise>().ok());
            match finished {
                Some(finished) => {
                    leptos::task::spawn_local(async move {
                        // A rejected `finished` (transition skipped or aborted)
                        // still needs the cleanup below.
                        if let Err(e) = JsFuture::from(finished).await {
                            leptos::logging::debug_warn!("view transition did not finish: {e:?}");
                        }
                        on_finished();
                    });
                }
                // No `finished` promise: clean up now rather than leak the name.
                None => on_finished(),
            }
        }
        // Practically unreachable; `update` is lost inside `cb`, but still run
        // the cleanup.
        Err(_) => on_finished(),
    }
}

#[cfg(not(feature = "hydrate"))]
fn start_view_transition(update: impl FnOnce(), on_finished: impl FnOnce()) {
    update();
    on_finished();
}

#[component]
fn HomePage() -> impl IntoView {
    // Bumped to re-fetch the workspace list (after actions and on a timer).
    let refresh = RwSignal::new(0u32);
    let reload = move || refresh.update(|n| *n += 1);

    let workspaces_resource = Resource::new(
        move || refresh.get(),
        |_| async move { list_workspaces().await },
    );

    // A persistent signal for `<For>` to diff, so polls update cards in place
    // and keep their local state, instead of remounting them as a resolved
    // `Resource` would. Filled client-side only, so SSR always shows "Loading...".
    let workspaces = RwSignal::new(Vec::<WorkspaceInfo>::new());
    let fetch_error = RwSignal::new(None::<String>);
    // Set by the client-side effect, so server and first hydration pass both
    // start at `false` (reading the resource directly risks a mismatch).
    let loaded = RwSignal::new(false);
    Effect::new(move |_| {
        if let Some(result) = workspaces_resource.get() {
            loaded.set(true);
            match result {
                Ok(list) => {
                    workspaces.set(list);
                    fetch_error.set(None);
                }
                // Keep the last-known list so one flaky poll doesn't blank the grid.
                Err(e) => fetch_error.set(Some(e.to_string())),
            }
        }
    });

    // Poll every 3s. In an Effect because `set_interval` needs `window`.
    Effect::new(move |_| {
        set_interval(
            move || refresh.update(|n| *n += 1),
            std::time::Duration::from_secs(3),
        );
    });

    let show_browser = RwSignal::new(false);
    let on_changed = Callback::new(move |()| reload());

    // The id of the workspace shown in the detail modal, if any.
    let selected = RwSignal::new(None::<String>);

    // The open card's element, so the closing morph (started from the modal)
    // can hand the `view-transition-name` back to it.
    let open_card_el = StoredValue::new(None::<web_sys::HtmlElement>);

    view! {
        <header class="topbar">
            <h1>"GAVMO"</h1>
            <button class="primary" on:click=move |_| show_browser.set(true)>
                "Add workspace"
            </button>
        </header>

        <Show
            // Memoized: the grid only remounts on empty<->non-empty, not per poll.
            when=move || !workspaces.with(Vec::is_empty)
            fallback=move || view! {
                <p class="empty">
                    {move || if loaded.get() {
                        "No workspaces yet. Click \"Add workspace\" to pick a repo."
                    } else {
                        "Loading…"
                    }}
                </p>
            }
        >
            <div class="ws-grid">
                <For
                    // Braced so view! doesn't parse the turbofish as tags.
                    each={move || workspaces.get().into_iter().map(|w| w.id).collect::<Vec<_>>()}
                    key=|id| id.clone()
                    let:id
                >
                    <WorkspaceCard id workspaces selected open_card_el/>
                </For>
            </div>
        </Show>
        {move || fetch_error.get().map(|e| view! {
            <p class="error">"Failed to refresh workspaces: " {e}</p>
        })}

        // Untracked: `selected` only goes None <-> Some (the backdrop blocks
        // other cards), so the id is fixed while the modal is mounted.
        <Show when=move || selected.get().is_some() fallback=|| ()>
            {move || {
                // `None` only if `selected` was cleared between `when` and this
                // closure running; render nothing rather than panic.
                let id = selected.get_untracked()?;
                let name = vt_name(&id);
                Some(view! {
                    <WorkspaceModal
                        id
                        workspaces
                        on_changed
                        on_close=Callback::new(move |()| {
                            // Captured once and cleared immediately, so a fast
                            // reopen of another card can't race this close's
                            // async cleanup.
                            let el = open_card_el.get_value();
                            open_card_el.set_value(None);
                            let el_for_cleanup = el.clone();
                            let name = name.clone();
                            start_view_transition(
                                move || {
                                    // Hand the name back so the modal morphs
                                    // into the card.
                                    if let Some(el) = &el {
                                        set_card_vt_name(el, Some(&name));
                                    }
                                    selected.set(None);
                                },
                                move || {
                                    let el = el_for_cleanup;
                                    // Animation done: release the name so it
                                    // doesn't leak into a later transition.
                                    if let Some(el) = &el {
                                        set_card_vt_name(el, None);
                                    }
                                },
                            );
                        })
                    />
                })
            }}
        </Show>

        <Show when=move || show_browser.get() fallback=|| ()>
            <Modal on_close=Callback::new(move |()| show_browser.set(false)) title="Add workspace".to_string()>
                <FolderBrowser on_added=Callback::new(move |()| {
                    show_browser.set(false);
                    reload();
                })/>
            </Modal>
        </Show>
    }
}

/// A floating modal over a dimmed backdrop; backdrop click, the close button,
/// and Escape all call `on_close`. `vt_name` lets the panel morph from the
/// card that opened it; the folder picker passes `None` and has no card.
#[component]
fn Modal(
    on_close: Callback<()>,
    title: String,
    #[prop(optional_no_strip)] vt_name: Option<String>,
    children: Children,
) -> impl IntoView {
    // Escape-to-close; the listener is removed when the modal unmounts.
    let handle = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" {
            on_close.run(());
        }
    });
    on_cleanup(move || handle.remove());

    // Without a card to morph from, use the plain CSS mount animation.
    let morphing = vt_name.is_some();

    view! {
        <div class="modal-backdrop" on:click=move |_| on_close.run(())>
            <div
                class="modal"
                class:modal--plain=!morphing
                style:view-transition-name=vt_name
                on:click=|ev| ev.stop_propagation()
            >
                <div class="modal-bar">
                    <h2>{title}</h2>
                    <button
                        class="modal-close"
                        aria-label="Close"
                        on:click=move |_| on_close.run(())
                    >
                        "×"
                    </button>
                </div>
                <div class="modal-body">
                    {children()}
                </div>
            </div>
        </div>
    }
}

/// One workspace's card in the grid (badge, name, path). Clicking it opens
/// [`WorkspaceModal`].
#[component]
fn WorkspaceCard(
    id: String,
    workspaces: RwSignal<Vec<WorkspaceInfo>>,
    selected: RwSignal<Option<String>>,
    open_card_el: StoredValue<Option<web_sys::HtmlElement>>,
) -> impl IntoView {
    let id = StoredValue::new(id);

    // This card's slice of the list. The fallback only covers the instant
    // between removal and `<For>` unmounting the card.
    let card_info = Memo::new(move |_| {
        let current_id = id.get_value();
        workspaces
            .get()
            .into_iter()
            .find(|w| w.id == current_id)
            .unwrap_or_else(|| WorkspaceInfo {
                id: current_id.clone(),
                path: current_id,
                status: WorkspaceStatus::Stopped,
            })
    });

    // Only the card being opened/closed carries a `view-transition-name`:
    // named elements get lifted above the backdrop during any transition.
    view! {
        <div
            class="ws-card"
            on:click=move |ev| {
                let clicked = id.get_value();
                let name = vt_name(&clicked);
                let el = click_target(&ev);
                if let Some(el) = &el {
                    set_card_vt_name(el, Some(&name));
                }
                open_card_el.set_value(el.clone());
                start_view_transition(
                    move || {
                        // The modal takes the name; two elements can't share it.
                        if let Some(el) = &el {
                            set_card_vt_name(el, None);
                        }
                        selected.set(Some(clicked));
                    },
                    || {},
                );
            }
        >
            {move || {
                let (badge_class, badge_text, _) = status_view(&card_info.get().status);
                view! { <span class=format!("badge {badge_class}")>{badge_text}</span> }
            }}
            <div class="ws-id">
                <span class="name">
                    {move || {
                        let path = card_info.get().path;
                        path.rsplit('/').next().unwrap_or(&path).to_string()
                    }}
                </span>
                <span class="path">{move || card_info.get().path}</span>
            </div>
        </div>
    }
}

/// The detail/actions view for one workspace, shown in the modal. Mounted on
/// each open, so its local state starts fresh every time.
#[component]
fn WorkspaceModal(
    id: String,
    workspaces: RwSignal<Vec<WorkspaceInfo>>,
    on_changed: Callback<()>,
    on_close: Callback<()>,
) -> impl IntoView {
    // A `Copy` handle so nested reactive `move` closures can all capture the
    // id without becoming `FnOnce`.
    let id = StoredValue::new(id);

    // This workspace's live slice of the list. The fallback covers the gap
    // between removal and the modal closing.
    let card_info = Memo::new(move |_| {
        let current_id = id.get_value();
        workspaces
            .get()
            .into_iter()
            .find(|w| w.id == current_id)
            .unwrap_or_else(|| WorkspaceInfo {
                id: current_id.clone(),
                path: current_id,
                status: WorkspaceStatus::Stopped,
            })
    });

    let busy = RwSignal::new(false);
    // Shared across open/stop/reset/remove: whichever of them last failed.
    let action_error = RwSignal::new(None::<String>);
    // Opening one closes the other, so only one confirm prompt shows at a time.
    let confirming_reset = RwSignal::new(false);
    let confirming_remove = RwSignal::new(false);
    // Build-log viewer; `content` is `None` while loading.
    let build_log_open = RwSignal::new(false);
    let build_log_content = RwSignal::new(None::<Result<String, String>>);

    // Live boot console, polled every second while `Booting`.
    let console_text = RwSignal::new(String::new());
    let console_ref: NodeRef<leptos::html::Pre> = NodeRef::new();
    // Kept so polling stops once the workspace leaves `Booting` or the modal
    // unmounts.
    let console_interval = StoredValue::new(None::<IntervalHandle>);

    let fetch_console = move || {
        let id = id.get_value();
        leptos::task::spawn_local(async move {
            if let Ok(text) = read_console_log(id).await {
                console_text.set(text);
            }
        });
    };

    // On leaving `Booting`, fetch once more so the panel shows the real tail
    // of the log.
    Effect::new(move |_| {
        let booting = matches!(card_info.get().status, WorkspaceStatus::Booting);
        let already_polling = console_interval.get_value().is_some();

        if booting && !already_polling {
            fetch_console();
            let handle =
                set_interval_with_handle(fetch_console, std::time::Duration::from_secs(1)).ok();
            console_interval.set_value(handle);
        } else if !booting && already_polling {
            console_interval.update_value(|h| {
                if let Some(h) = h.take() {
                    h.clear();
                }
            });
            fetch_console();
        }
    });
    on_cleanup(move || {
        if let Some(h) = console_interval.get_value() {
            h.clear();
        }
    });

    // Keep the console scrolled to the newest output.
    Effect::new(move |_| {
        console_text.track();
        if let Some(el) = console_ref.get() {
            scroll_to_bottom(&el);
        }
    });

    // VM stats, polled while `Running`. Every 2s rather than 1s because each
    // request waits between two CPU samples, so faster polls would overlap.
    let vm_stats = RwSignal::new(None::<VmStats>);
    let stats_interval = StoredValue::new(None::<IntervalHandle>);

    let fetch_stats = move || {
        let id = id.get_value();
        leptos::task::spawn_local(async move {
            if let Ok(stats) = poll_vm_stats(id).await {
                vm_stats.set(stats);
            }
        });
    };

    Effect::new(move |_| {
        let running = matches!(card_info.get().status, WorkspaceStatus::Running { .. });
        let already_polling = stats_interval.get_value().is_some();

        if running && !already_polling {
            fetch_stats();
            let handle =
                set_interval_with_handle(fetch_stats, std::time::Duration::from_secs(2)).ok();
            stats_interval.set_value(handle);
        } else if !running && already_polling {
            stats_interval.update_value(|h| {
                if let Some(h) = h.take() {
                    h.clear();
                }
            });
            vm_stats.set(None);
        }
    });
    on_cleanup(move || {
        if let Some(h) = stats_interval.get_value() {
            h.clear();
        }
    });

    // All captures are `Copy`, so these handlers are `Copy` and can be used
    // from nested reactive branches without cloning.
    let open = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            // Returns once the boot starts; the live poll shows its progress.
            if let Err(e) = open_workspace(id).await {
                leptos::logging::error!("open failed: {e}");
                action_error.set(Some(e.to_string()));
            }
            busy.set(false);
            on_changed.run(());
        });
    };

    let stop = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            // May take a moment if it races a boot that's about to finish.
            if let Err(e) = stop_workspace(id).await {
                leptos::logging::error!("stop failed: {e}");
                action_error.set(Some(e.to_string()));
            }
            busy.set(false);
            on_changed.run(());
        });
    };

    let reset = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            if let Err(e) = reset_workspace(id).await {
                leptos::logging::error!("reset failed: {e}");
                action_error.set(Some(e.to_string()));
            }
            busy.set(false);
            confirming_reset.set(false);
            on_changed.run(());
        });
    };

    let remove = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            if let Err(e) = remove_workspace(id).await {
                leptos::logging::error!("remove failed: {e}");
                action_error.set(Some(e.to_string()));
            }
            busy.set(false);
            confirming_remove.set(false);
            on_changed.run(());
        });
    };

    // Untracked: the modal is mounted per open and the path can't change
    // while it's open.
    let title = {
        let path = card_info.get_untracked().path;
        path.rsplit('/').next().unwrap_or(&path).to_string()
    };

    // The name the clicked card just released; claiming it morphs the card
    // into this modal.
    let vt_name = Some(vt_name(&id.get_value()));

    // Start/Stop is one toggle because each status maps to exactly one valid
    // action (Stop also cancels a boot). Reset is disabled while `Booting`:
    // a destructive action is reserved for a settled state.
    view! {
        <Modal on_close title vt_name>
            <div class="ws-modal-head">
                {move || {
                    let (badge_class, badge_text, _) = status_view(&card_info.get().status);
                    view! { <span class=format!("badge {badge_class}")>{badge_text}</span> }
                }}
                <span class="path">{move || card_info.get().path}</span>
            </div>
                <div class="actions">
                    {move || status_view(&card_info.get().status).2.map(|u| view! {
                        <a href=u target="_blank" rel="noreferrer">
                            <button>"Open UI"</button>
                        </a>
                    })}
                    {move || matches!(card_info.get().status, WorkspaceStatus::BuildFailed { .. }).then(|| view! {
                        <button on:click=move |_| {
                            if build_log_open.get_untracked() {
                                build_log_open.set(false);
                                return;
                            }
                            let WorkspaceStatus::BuildFailed { log } = card_info.get_untracked().status else {
                                return;
                            };
                            build_log_open.set(true);
                            build_log_content.set(None);
                            let id = id.get_value();
                            leptos::task::spawn_local(async move {
                                let result = read_build_log(id, log).await.map_err(|e| e.to_string());
                                build_log_content.set(Some(result));
                            });
                        }>
                            {move || if build_log_open.get() { "Hide build log" } else { "View build log" }}
                        </button>
                    })}
                    <button
                        on:click=move |ev| {
                            if matches!(
                                card_info.get_untracked().status,
                                WorkspaceStatus::Booting | WorkspaceStatus::Running { .. }
                            ) {
                                stop(ev);
                            } else {
                                open(ev);
                            }
                        }
                        disabled=move || busy.get() || matches!(card_info.get().status, WorkspaceStatus::Stopping)
                    >
                        {move || if matches!(
                            card_info.get().status,
                            WorkspaceStatus::Booting | WorkspaceStatus::Running { .. }
                        ) {
                            "Stop"
                        } else {
                            "Start"
                        }}
                    </button>
                    {move || if confirming_reset.get() {
                        view! {
                            <span class="reset-confirm">
                                <span class="reset-confirm-label">"Delete all VM state?"</span>
                                <button class="danger" on:click=reset disabled=move || busy.get()>
                                    {move || if busy.get() { "Resetting…" } else { "Confirm" }}
                                </button>
                                <button
                                    on:click=move |_| confirming_reset.set(false)
                                    disabled=move || busy.get()
                                >
                                    "Cancel"
                                </button>
                            </span>
                        }.into_any()
                    } else {
                        view! {
                            <button
                                on:click=move |_| {
                                    confirming_remove.set(false);
                                    confirming_reset.set(true);
                                }
                                disabled=move || {
                                    busy.get() || matches!(card_info.get().status, WorkspaceStatus::Booting)
                                }
                            >
                                "Reset"
                            </button>
                        }.into_any()
                    }}
                    {move || if confirming_remove.get() {
                        view! {
                            <span class="reset-confirm">
                                <span class="reset-confirm-label">"Remove from list?"</span>
                                <button class="danger" on:click=remove disabled=move || busy.get()>
                                    {move || if busy.get() { "Removing…" } else { "Confirm" }}
                                </button>
                                <button
                                    on:click=move |_| confirming_remove.set(false)
                                    disabled=move || busy.get()
                                >
                                    "Cancel"
                                </button>
                            </span>
                        }.into_any()
                    } else {
                        view! {
                            <button
                                on:click=move |_| {
                                    confirming_reset.set(false);
                                    confirming_remove.set(true);
                                }
                                disabled=move || busy.get()
                            >
                                "Remove"
                            </button>
                        }.into_any()
                    }}
                </div>
                {move || matches!(card_info.get().status, WorkspaceStatus::Booting).then(|| view! {
                    <p class="hint">
                        "First boot can take a few minutes - longer if "
                        <code>"gavmo.nix"</code>
                        " needs to fetch or build new packages."
                    </p>
                })}
                // Stays visible after boot so the log doesn't vanish mid-read.
                {move || {
                    let booting = matches!(card_info.get().status, WorkspaceStatus::Booting);
                    let has_content = !console_text.get().is_empty();
                    (booting || has_content).then(|| view! {
                        <div class="console-panel">
                            <pre class="console-log" node_ref=console_ref>
                                {move || {
                                    let text = console_text.get();
                                    if text.is_empty() {
                                        "Waiting for boot console output…".to_string()
                                    } else {
                                        text
                                    }
                                }}
                            </pre>
                        </div>
                    })
                }}
                {move || vm_stats.get().map(|s| view! {
                    <div class="stats-panel">
                        <div class="stat-row">
                            <span class="stat-label">"CPU"</span>
                            <div class="stat-bar">
                                <div
                                    class="stat-bar-fill"
                                    style:width=format!(
                                        "{}%",
                                        (s.cpu_percent / (s.vcpus.max(1) as f32 * 100.0) * 100.0)
                                            .clamp(0.0, 100.0)
                                    )
                                ></div>
                            </div>
                            <span class="stat-value">
                                {format!("{:.0}% / {} vCPU", s.cpu_percent, s.vcpus)}
                            </span>
                        </div>
                        <div class="stat-row">
                            <span class="stat-label">"Memory"</span>
                            // Placeholder until the balloon driver reports.
                            {match (s.mem_used_bytes, s.mem_total_bytes) {
                                (Some(used), Some(total)) => view! {
                                    <div class="stat-bar">
                                        <div
                                            class="stat-bar-fill"
                                            style:width=format!(
                                                "{}%",
                                                (used as f32 / total.max(1) as f32 * 100.0)
                                                    .clamp(0.0, 100.0)
                                            )
                                        ></div>
                                    </div>
                                    <span class="stat-value">
                                        {format!("{} / {}", format_bytes(used), format_bytes(total))}
                                    </span>
                                }.into_any(),
                                _ => view! {
                                    <div class="stat-bar"></div>
                                    <span class="stat-value">"—"</span>
                                }.into_any(),
                            }}
                        </div>
                        <div class="stat-row stat-row--plain">
                            <span class="stat-label">"Uptime"</span>
                            <span class="stat-value">{format_uptime(s.uptime_secs)}</span>
                        </div>
                        <div class="stat-row stat-row--plain">
                            <span class="stat-label">"Disk"</span>
                            <span class="stat-value">{format_bytes(s.disk_bytes)}</span>
                        </div>
                        <div class="stat-row stat-row--plain">
                            <span class="stat-label">"Port"</span>
                            <span class="stat-value">{s.host_port.to_string()}</span>
                        </div>
                    </div>
                })}
                {move || action_error.get().map(|e| view! {
                    <p class="error">{e}</p>
                })}
                {move || build_log_open.get().then(|| view! {
                    <div class="build-log-panel">
                        {move || match build_log_content.get() {
                            None => view! { <p class="empty">"Loading…"</p> }.into_any(),
                            Some(Ok(text)) => view! { <pre class="build-log">{text}</pre> }.into_any(),
                            Some(Err(e)) => view! {
                                <p class="error">"Failed to load build log: " {e}</p>
                            }.into_any(),
                        }}
                    </div>
                })}
                <WorkspaceTools id=id.get_value() />
        </Modal>
    }
}

/// The modal's Tools panel: lists the workspace's requested tools and lets
/// the user set each one's credential. Reloads after any change.
#[component]
fn WorkspaceTools(id: String) -> impl IntoView {
    let id = StoredValue::new(id);
    // `None` = still loading; `Some(vec)` = loaded (possibly empty).
    let tools = RwSignal::new(None::<Vec<ToolConfig>>);
    let load_error = RwSignal::new(None::<String>);

    let reload = move || {
        let id = id.get_value();
        leptos::task::spawn_local(async move {
            match list_tool_configs(id).await {
                Ok(list) => {
                    load_error.set(None);
                    tools.set(Some(list));
                }
                Err(e) => {
                    load_error.set(Some(e.to_string()));
                    tools.set(Some(Vec::new()));
                }
            }
        });
    };

    // Reads no reactive state, so this runs once after mount.
    Effect::new(move |_| reload());

    let on_changed = Callback::new(move |()| reload());

    view! {
        <div class="tools-panel">
            <h3 class="tools-title">"Tools"</h3>
            {move || match tools.get() {
                None => view! { <p class="empty">"Loading tools…"</p> }.into_any(),
                Some(list) if list.is_empty() => view! {
                    <p class="hint">
                        "This workspace requests no tools. Add a "
                        <code>"gavmo.yaml"</code>
                        " to its repo to enable Jira or other integrations."
                    </p>
                }.into_any(),
                Some(list) => view! {
                    <div class="tool-list">
                        {list.into_iter()
                            .map(|tool| view! {
                                <ToolRow tool=tool workspace_id=id on_changed=on_changed />
                            })
                            .collect_view()}
                    </div>
                    <p class="hint">
                        "Credential changes take effect the next time this workspace starts."
                    </p>
                }.into_any(),
            }}
            {move || load_error.get().map(|e| view! {
                <p class="error">"Failed to load tools: " {e}</p>
            })}
        </div>
    }
}

/// One tool's row: details, status, and a form to set, clear, or test its
/// credential. Fires `on_changed` after a change so the parent reloads.
#[component]
fn ToolRow(
    tool: ToolConfig,
    workspace_id: StoredValue<String>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let auth = tool.auth.clone();
    let is_basic = auth == "basic";
    let configured = tool.configured;
    let url_changed = tool.url_changed;
    let bound_url = tool.bound_url.clone();
    let approved_delivery = tool.approved_delivery.clone();
    let tool = StoredValue::new(tool);

    let username = RwSignal::new(String::new());
    let token = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    // Result of the last save/clear/test, shown inline (Ok = green, Err = red).
    let result = RwSignal::new(None::<Result<String, String>>);

    let save = move |_| {
        let current = tool.get_value();
        let token_value = token.get_untracked();
        if token_value.is_empty() {
            result.set(Some(Err("Enter a token first".to_string())));
            return;
        }
        let username_value = is_basic.then(|| username.get_untracked());
        let wid = workspace_id.get_value();
        busy.set(true);
        result.set(None);
        leptos::task::spawn_local(async move {
            let outcome = set_tool_secret(wid, current.id, username_value, token_value).await;
            busy.set(false);
            match outcome {
                Ok(()) => {
                    token.set(String::new());
                    result.set(Some(Ok("Saved".to_string())));
                    on_changed.run(());
                }
                Err(e) => result.set(Some(Err(e.to_string()))),
            }
        });
    };

    let clear = move |_| {
        let current = tool.get_value();
        let wid = workspace_id.get_value();
        busy.set(true);
        result.set(None);
        leptos::task::spawn_local(async move {
            let outcome = clear_tool_secret(wid, current.id).await;
            busy.set(false);
            match outcome {
                Ok(()) => {
                    result.set(Some(Ok("Credential cleared".to_string())));
                    on_changed.run(());
                }
                Err(e) => result.set(Some(Err(e.to_string()))),
            }
        });
    };

    let test = move |_| {
        let current = tool.get_value();
        let wid = workspace_id.get_value();
        busy.set(true);
        result.set(None);
        leptos::task::spawn_local(async move {
            let outcome = test_tool(wid, current.id).await;
            busy.set(false);
            result.set(Some(outcome.map_err(|e| e.to_string())));
        });
    };

    let header = tool.get_value();
    view! {
        <div class="tool-row">
            <div class="tool-head">
                <span class="tool-name">{header.id.clone()}</span>
                {if url_changed {
                    view! { <span class="tool-status tool-status--warn">"destination changed"</span> }.into_any()
                } else if approved_delivery.is_some() {
                    view! { <span class="tool-status tool-status--warn">"auth method changed"</span> }.into_any()
                } else if configured {
                    view! { <span class="tool-status tool-status--ok">"configured"</span> }.into_any()
                } else {
                    view! { <span class="tool-status">"not configured"</span> }.into_any()
                }}
            </div>
            <p class="tool-desc">{header.description.clone()}</p>
            <p class="tool-url">
                <span class="tool-url-label">"Sends your credential to: "</span>
                <code>{header.base_url.clone()}</code>
                " as " {header.delivery.clone()}
            </p>
            {approved_delivery.clone().map(|was| view! {
                <div class="tool-warning">
                    <strong>"This tool's auth method changed."</strong>
                    <p>
                        "Your saved credential was approved to be sent as " {was}
                        " but this tool now sends it as " {header.delivery.clone()} "."
                    </p>
                    <p>"The credential is disabled until you re-enter it below to confirm."</p>
                </div>
            })}
            {url_changed.then(|| view! {
                <div class="tool-warning">
                    <strong>"This tool's destination changed."</strong>
                    {match bound_url.clone() {
                        Some(was) => view! {
                            <p>
                                "Your saved credential was approved for "
                                <code>{was}</code>
                                " but this tool now points at "
                                <code>{header.base_url.clone()}</code>
                                "."
                            </p>
                        }.into_any(),
                        None => view! {
                            <p>
                                "Your saved credential wasn't bound to a destination (it predates \
                                 URL-binding). Re-enter it to confirm it should go to "
                                <code>{header.base_url.clone()}</code>
                                "."
                            </p>
                        }.into_any(),
                    }}
                    <p>"The credential is disabled until you re-enter it below to confirm."</p>
                </div>
            })}
            {header.instructions.clone().map(|text| view! {
                <p class="hint">{text}</p>
            })}
            <div class="tool-form">
                {is_basic.then(|| view! {
                    <input
                        class="tool-input"
                        r#type="text"
                        placeholder="Username / email"
                        prop:value=move || username.get()
                        on:input=move |ev| username.set(event_target_value(&ev))
                        disabled=move || busy.get()
                    />
                })}
                <input
                    class="tool-input"
                    r#type="password"
                    placeholder=move || if configured { "Enter a new token to replace" } else { "Token / API key" }
                    prop:value=move || token.get()
                    on:input=move |ev| token.set(event_target_value(&ev))
                    disabled=move || busy.get()
                />
                <div class="tool-actions">
                    <button on:click=save disabled=move || busy.get()>
                        {move || if configured { "Update" } else { "Save" }}
                    </button>
                    {configured.then(|| view! {
                        <button on:click=test disabled=move || busy.get()>"Test"</button>
                        <button class="danger" on:click=clear disabled=move || busy.get()>"Clear"</button>
                    })}
                </div>
            </div>
            {move || result.get().map(|r| match r {
                Ok(msg) => view! { <pre class="tool-result tool-result--ok">{msg}</pre> }.into_any(),
                Err(msg) => view! { <pre class="tool-result tool-result--err">{msg}</pre> }.into_any(),
            })}
        </div>
    }
}

fn status_view(status: &WorkspaceStatus) -> (&'static str, String, Option<String>) {
    match status {
        // Covers "never booted" too, so "idle" rather than "stopped".
        WorkspaceStatus::Stopped => ("idle", "idle".into(), None),
        WorkspaceStatus::Booting => ("booting", "booting…".into(), None),
        WorkspaceStatus::Stopping => ("booting", "stopping…".into(), None),
        WorkspaceStatus::Running { url } => ("running", "running".into(), Some(url.clone())),
        WorkspaceStatus::Failed { reason } => ("failed", format!("failed: {reason}"), None),
        // Same badge color as "failed"; the modal offers the build log.
        WorkspaceStatus::BuildFailed { .. } => ("failed", "gavmo.nix build failed".into(), None),
    }
}

/// Formats a byte count with binary units: whole KiB, one decimal for MiB,
/// two for GiB.
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.0} KiB", b / KIB)
    } else if b < GIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{:.2} GiB", b / GIB)
    }
}

/// Formats seconds as a compact uptime using at most the two most
/// significant units (`"45s"`, `"3m 12s"`, `"2h 05m"`, `"1d 03h"`).
fn format_uptime(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else if mins > 0 {
        format!("{mins}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

#[component]
fn FolderBrowser(on_added: Callback<()>) -> impl IntoView {
    let cwd = RwSignal::new(None::<String>);
    let listing = Resource::new(
        move || cwd.get(),
        |path| async move { browse_dir(path).await },
    );

    // Why the last "Use this directory" was refused (e.g. a protected path).
    let add_error = RwSignal::new(None::<String>);
    let adding = RwSignal::new(false);

    let add_current = move |_| {
        let path = listing.get_untracked().and_then(Result::ok).map(|l| l.path);
        if let Some(path) = path {
            add_error.set(None);
            adding.set(true);
            leptos::task::spawn_local(async move {
                let outcome = add_workspace(path).await;
                adding.set(false);
                match outcome {
                    // Close only on success, so a refusal stays readable.
                    Ok(()) => on_added.run(()),
                    Err(e) => add_error.set(Some(e.to_string())),
                }
            });
        }
    };

    // Manual path entry, since the list hides dotdirs. `browse_dir` validates
    // the path, so a typed path reuses the same listing/error handling.
    let path_input = RwSignal::new(String::new());
    let go_to_typed_path = move || {
        let typed = path_input.get_untracked();
        let trimmed = typed.trim();
        if !trimmed.is_empty() {
            cwd.set(Some(trimmed.to_string()));
        }
    };

    view! {
        <div class="browser">
            <div class="browser-goto">
                <input
                    type="text"
                    placeholder="Or type an absolute path…"
                    prop:value=move || path_input.get()
                    on:input=move |ev| path_input.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            go_to_typed_path();
                        }
                    }
                />
                <button on:click=move |_| go_to_typed_path()>"Go"</button>
            </div>
            <Suspense fallback=move || view! { <p class="cwd">"Loading…"</p> }>
                {move || Suspend::new(async move {
                    match listing.await {
                        Ok(l) => {
                            let parent = l.parent.clone();
                            let here = l.path.clone();
                            view! {
                                <p class="cwd">{here.clone()}</p>
                                <div class="toolbar">
                                    <button
                                        on:click=add_current
                                        class="primary"
                                        prop:disabled=move || adding.get()
                                    >
                                        "Use this directory"
                                    </button>
                                </div>
                                {move || add_error.get().map(|e| view! {
                                    <p class="error">"Cannot add this directory: " {e}</p>
                                })}
                                <ul>
                                    {parent.map(|p| view! {
                                        <li on:click=move |_| cwd.set(Some(p.clone()))>".."</li>
                                    })}
                                    {l.entries.into_iter().map(|e| {
                                        let p = e.path.clone();
                                        view! {
                                            <li on:click=move |_| cwd.set(Some(p.clone()))>
                                                {e.name}
                                            </li>
                                        }
                                    }).collect_view()}
                                </ul>
                            }.into_any()
                        }
                        Err(e) => view! {
                            <p class="error">"Cannot read directory: " {e.to_string()}</p>
                        }.into_any(),
                    }
                })}
            </Suspense>
        </div>
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn format_bytes_picks_the_right_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KiB");
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn format_uptime_shows_the_two_most_significant_units() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(45), "45s");
        assert_eq!(format_uptime(192), "3m 12s");
        assert_eq!(format_uptime(7_500), "2h 05m");
        assert_eq!(format_uptime(97_200), "1d 03h");
    }
}
