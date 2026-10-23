//! Leptos app: shared view types, the whole server-function "API", and the
//! control-panel UI. Compiled for both `ssr` and `hydrate`; the `#[server]`
//! function bodies only compile on the server (`ssr`), where they touch the
//! registry and `vm` crate directly. See `crates/gavmo/plan.md` §§7–8.

use leptos::prelude::*;
use leptos_meta::{provide_meta_context, MetaTags, Stylesheet, Title};
use leptos_router::{
    components::{Route, Router, Routes},
    StaticSegment,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shared view types (serialized across the server-function boundary).
// ---------------------------------------------------------------------------

/// A known workspace plus its current VM status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub path: String,
    pub status: WorkspaceStatus,
}

/// Browser-facing mirror of `vm::VmStatus`, plus the M2 `BuildFailed` case
/// (`vm::Error::RebuildFailed`) that `vm::VmStatus` itself doesn't have —
/// see `registry::VmSlot::BuildFailed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceStatus {
    Stopped,
    Booting,
    Running {
        url: String,
    },
    Failed {
        reason: String,
    },
    /// The repo's `gavmo.nix` failed to build (detected within seconds via
    /// the guest's failure marker, not a full boot-health timeout — see
    /// `nix/base/configuration.nix`'s `gavmo-rebuild` service). `log` is the
    /// failed run's log filename, fetchable via [`read_build_log`].
    BuildFailed {
        log: String,
    },
}

/// Host-side resource usage for a `Running` workspace's VM, shown in the
/// detail modal's live stats panel (see `registry::AppState::vm_stats`).
/// `None` from the server function means "nothing to show right now" (not
/// `Running`, or the qemu process just exited) rather than an error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmStats {
    /// Top-style CPU usage: `0` idle, `100` per fully-busy core, up to
    /// `vcpus * 100` if every core is saturated.
    pub cpu_percent: f32,
    /// Configured guest vCPU count — the denominator `cpu_percent` is
    /// scaled against.
    pub vcpus: u32,
    /// The guest's own reported memory usage, in bytes: `total - available`
    /// (`available` meaning the guest kernel's own `MemAvailable` — already
    /// counts reclaimable page cache as free, not "used").
    ///
    /// Deliberately **not** qemu's host-side process RSS. An earlier version
    /// of this stat used RSS, and it was actively misleading once the
    /// `/workspace` share moved to virtiofs (see `crates/vm/plan.md`):
    /// virtiofs backs guest RAM with a shared `memfd`, so every page the
    /// guest touches — including purely reclaimable page cache from reading
    /// through a large `node_modules`/`.git` tree — stays resident in
    /// qemu's RSS with nothing to reclaim it. In practice this pinned the
    /// bar at ~100% during ordinary, non-memory-constrained work (a real
    /// case: an `npm test` run that left the guest with ~3 GB still
    /// genuinely available, confirmed via `free -m` inside the guest, while
    /// the old RSS-based bar showed full). These fields instead come from
    /// the guest's own virtio-balloon driver (`vm::guest_memory`), which
    /// reports what the guest kernel itself considers used/available — the
    /// same number `free -m` inside the guest would show.
    ///
    /// `None` (both fields together) means the guest's balloon driver
    /// hasn't reported yet — e.g. moments after boot — not that usage is
    /// zero. The UI should show a placeholder (e.g. "—"), never a 0%/100%
    /// bar, in that case.
    pub mem_used_bytes: Option<u64>,
    /// The guest's total RAM as it reports it, in bytes — shown alongside
    /// `mem_used_bytes` for context. See that field's doc comment for why
    /// this is guest-reported rather than `VmConfig::memory_mib` restated;
    /// in practice the two should be very close, but this is what the
    /// guest itself sees.
    pub mem_total_bytes: Option<u64>,
    /// How long the qemu process has existed, in seconds (from spawn, not
    /// from when it first became healthy — see `vm::RunningVm::uptime`).
    pub uptime_secs: u64,
    /// Size of the persistent overlay disk (`.gavmo/vm/disk.qcow2`), in
    /// bytes.
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

/// A tool a workspace's `gavmo.yaml` requests, for the control panel's Tools
/// panel. `auth` is a plain string — not the `ssr`-only `toolspec` enum — so
/// this type is usable in the wasm/hydrate build too.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolConfig {
    /// Tool id (also the name the agent calls it by).
    pub id: String,
    /// Human-facing description from the config (or a generated fallback).
    pub description: String,
    /// The integration's base URL (shown so the user can verify where their
    /// credential will be sent).
    pub base_url: String,
    /// `"basic"`, `"bearer"`, or `"header"` — drives which secret fields the
    /// configure form shows.
    pub auth: String,
    /// The secret reference name declared in the config.
    pub secret_name: String,
    /// Optional setup help from the config.
    pub instructions: Option<String>,
    /// Whether a credential is currently stored for this tool *and* bound to the
    /// current `base_url` (i.e. usable). False when nothing is stored, or when a
    /// stored credential's destination no longer matches (see `url_changed`).
    pub configured: bool,
    /// True when a credential *is* stored but was approved for a different
    /// destination than the tool now declares (URL-binding mismatch). The tool is
    /// disabled until re-confirmed; the UI shows a warning with `bound_url`.
    pub url_changed: bool,
    /// The destination the stored credential was originally approved for, shown in
    /// the mismatch warning ("was X, now Y"). `Some` only when `url_changed` and
    /// the stored value carried a binding; `None` for an unbound/legacy value.
    pub bound_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Server functions (the entire API surface).
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

/// Fetches the full `nixos-rebuild` output for a failed M2 build, for the
/// "View build log" action on a [`WorkspaceStatus::BuildFailed`] row. `log`
/// must be exactly the filename that status reported — the server validates
/// it strictly (see `registry::is_safe_rebuild_log_filename`) before
/// touching the filesystem.
#[server]
pub async fn read_build_log(id: String, log: String) -> Result<String, ServerFnError> {
    let state = app_state()?;
    state
        .read_build_log(&id, &log)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Fetches the guest's boot console (kernel + systemd output), sanitized
/// for display — see `registry::AppState::read_console_log`. Polled by the
/// modal's console panel while a workspace is `Booting`; unlike
/// `read_build_log`, there's no client-controlled filename here (the
/// console log always lives at a fixed path under the workspace's state
/// dir), so no separate validation is needed beyond `id` itself already
/// being a registry-known workspace path.
#[server]
pub async fn read_console_log(id: String) -> Result<String, ServerFnError> {
    let state = app_state()?;
    state
        .read_console_log(&id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Fetches host-side resource usage (CPU/memory/uptime/disk) for a
/// workspace's VM — see `registry::AppState::vm_stats`. Polled by the
/// modal's stats panel while a workspace is `Running`, mirroring
/// `read_console_log`'s polling pattern for the `Booting` boot console.
/// `Ok(None)` is the normal "nothing to show right now" case (not
/// `Running`, or the process just exited) — distinct from `Err`, which
/// means the request itself failed.
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

/// Kicks off booting the workspace's VM (or is a no-op if it's already
/// booting/running). Returns immediately — the client is expected to
/// observe the boot's progress via the live-polled workspace list, which
/// transitions `idle -> booting… -> running`/`failed: ...`. See
/// `crates/gavmo/plan.md` §9 — a synchronous `open` that blocked for the
/// whole boot made the UI look unresponsive for minutes with no feedback.
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

/// Stops (if needed) and wipes a workspace's entire VM state — the general
/// recovery action for a wedged workspace, including "the base image was
/// rebuilt but this workspace's overlay is still on the old one" (see
/// `registry::AppState::reset_workspace`'s doc comment). **Destructive** —
/// the client is expected to have already gotten explicit user confirmation
/// before calling this (see `WorkspaceRow`'s inline confirm step); this
/// function itself does not re-confirm.
#[server]
pub async fn reset_workspace(id: String) -> Result<(), ServerFnError> {
    let state = app_state()?;
    state
        .reset_workspace(&id)
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Stops (if needed) and forgets a workspace — drops it from the registry
/// so it no longer appears in the control panel. **Non-destructive**,
/// unlike [`reset_workspace`]: nothing under `.gavmo/vm` is deleted (see
/// `registry::AppState::remove_workspace`'s doc comment), so re-adding the
/// same path later finds its old overlay/session history again. The client
/// is expected to have already gotten explicit user confirmation before
/// calling this (see `WorkspaceRow`'s inline confirm step), same as
/// [`reset_workspace`].
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

/// Lists the tools a workspace's `gavmo.yaml` requests, with each one's
/// configured status — for the modal's Tools panel. Computed live (reads the
/// config), so it reflects config/secret changes without a restart.
#[server]
pub async fn list_tool_configs(id: String) -> Result<Vec<ToolConfig>, ServerFnError> {
    let state = app_state()?;
    Ok(state.list_tool_configs(&id).await)
}

/// Stores (or replaces) a tool's credential on the host. `username` is only used
/// for Basic auth; Bearer/Header pass just `token`. The credential never leaves
/// the host — see `registry::AppState::set_tool_secret`.
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

/// Fetches the injected [`registry::AppState`] from Leptos context. Only
/// compiled on the server.
#[cfg(feature = "ssr")]
fn app_state() -> Result<crate::registry::AppState, ServerFnError> {
    use_context::<crate::registry::AppState>()
        .ok_or_else(|| ServerFnError::new("app state not available"))
}

// ---------------------------------------------------------------------------
// UI.
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
// Card <-> modal morph: the View Transitions API.
// ---------------------------------------------------------------------------

/// A stable, CSS-ident-safe `view-transition-name` derived from a
/// workspace's id. Ids are absolute filesystem paths (see
/// `WorkspaceInfo::id`), which aren't valid CSS idents on their own - this
/// hashes the id into a short, always-valid name instead.
fn vt_name(id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    // The `ws-vt-` prefix guarantees a valid CSS identifier (must not
    // start with a digit) regardless of the hash's own leading hex digit.
    format!("ws-vt-{:x}", hasher.finish())
}

/// Extracts the DOM element a click event's listener is attached to
/// (`current_target`, not `target` - correct even if the click actually
/// landed on, and bubbled up from, a child element). Always `None` under
/// `ssr`: there's no click event loop in server-side rendering to begin
/// with, so this is dead code there, never actually invoked - it's written
/// this way (one shared signature, `hydrate`-gated body) purely so
/// `WorkspaceCard`'s `on:click` handler - itself shared, compiled-for-both-
/// targets view code - doesn't need its own `#[cfg]` split.
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

/// Directly sets or clears (bypassing Leptos's reactive `style:` system -
/// see [`start_view_transition`]'s doc comment for why) a card's inline
/// `view-transition-name`; `name: None` clears it back to the default
/// (`auto`). A no-op under `ssr`, for the same reason as [`click_target`].
fn set_card_vt_name(el: &web_sys::HtmlElement, name: Option<&str>) {
    #[cfg(feature = "hydrate")]
    {
        let style = el.style();
        match name {
            Some(name) => {
                let _ = style.set_property("view-transition-name", name);
            }
            None => {
                let _ = style.remove_property("view-transition-name");
            }
        }
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (el, name);
    }
}

/// Scrolls an element to the bottom of its content (`scroll_top =
/// scroll_height`) — used to keep the live boot-console panel pinned to
/// the newest output as it streams in, same way a real terminal would. A
/// no-op under `ssr`, for the same reason as [`click_target`].
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

/// Runs `update` - a Leptos signal write - wrapped in a View Transition
/// when the browser supports `document.startViewTransition`, so whatever
/// DOM change it causes (here: a card turning into the detail modal, or
/// back) morphs instead of popping instantly; `on_finished` runs once the
/// *entire* animated transition has completed (not just the DOM update
/// itself - see below). Falls back to just calling both directly, in
/// order, when the API isn't available - an **instant** open/close on
/// older browsers, not an error.
///
/// Looked up dynamically via `js_sys::Reflect` rather than a typed web-sys
/// binding - the exact approach `leptos_router` itself uses for its own
/// route transitions (see its private `view_transition` module) - which
/// avoids needing `--cfg=web_sys_unstable_apis` at all.
///
/// The one thing genuinely specific to *this* call site, rather than just
/// copied from the router: Leptos signal writes don't patch the DOM
/// synchronously - a `RenderEffect` re-run is scheduled on the reactive
/// executor and only actually runs on a later "tick" (see
/// `reactive_graph::effect::render_effect`). `startViewTransition`'s own
/// callback is expected to return a thenable that it awaits *before*
/// snapshotting the "new" state, so this runs `update()` (the signal
/// write) and then awaits `leptos::task::tick()` to give the executor a
/// turn to actually run the resulting render effect - mounting/unmounting
/// the modal - before the promise resolves and the browser captures the
/// after-snapshot.
///
/// `on_finished` exists for exactly one reason: the *closing* morph needs
/// to hand a card's `view-transition-name` back to it for the post-close
/// snapshot (see `WorkspaceCard`/`set_card_vt_name`), but that name must
/// then be released again - otherwise it would linger on that card and get
/// incorrectly swept into some later, unrelated transition for a
/// *different* card. `.finished` (unlike the callback's own promise) only
/// resolves once the whole animation has actually played out, which is the
/// right moment to release it.
#[cfg(feature = "hydrate")]
fn start_view_transition(update: impl FnOnce() + 'static, on_finished: impl FnOnce() + 'static) {
    use js_sys::Reflect;
    use wasm_bindgen::{closure::Closure, JsCast, JsValue};
    use wasm_bindgen_futures::{future_to_promise, JsFuture};

    let document = document();
    let svt = Reflect::get(&document, &JsValue::from_str("startViewTransition"))
        .and_then(|f| f.dyn_into::<js_sys::Function>());

    let Ok(svt) = svt else {
        // Not supported in this browser - just apply both directly.
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
                        let _ = JsFuture::from(finished).await;
                        on_finished();
                    });
                }
                // Shouldn't happen (every real `ViewTransition` has a
                // `finished` property) - fail safe by running the
                // cleanup immediately rather than leaking it forever.
                None => on_finished(),
            }
        }
        // `svt` was just confirmed callable above; a call failing here
        // would mean the document became inactive between the two calls -
        // vanishingly unlikely for a click handler in a localhost control
        // panel. `update` is already consumed inside `cb`, uncalled, in
        // this case; not worth a recovery path for it, but `on_finished`
        // is still ours to run so its cleanup isn't silently lost.
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
    // Bumped to force a re-fetch of the workspace list (after an action, and
    // on a live polling interval — see the Effect below).
    let refresh = RwSignal::new(0u32);
    let reload = move || refresh.update(|n| *n += 1);

    let workspaces_resource = Resource::new(
        move || refresh.get(),
        |_| async move { list_workspaces().await },
    );

    // The stable list every `WorkspaceCard` is keyed and diffed against
    // (see `<For>` below) - deliberately **not** read from
    // `workspaces_resource` directly. A `Resource` read inside
    // `<Transition>`/`Suspend` (the old approach) re-resolves to a brand
    // new value on every poll, and `<For>` has no way to tell "this is
    // conceptually the same list, just one field updated" from "this is a
    // wholesale replacement" unless the *signal* itself is stable across
    // updates - which a fresh `Suspend` future is not. Copying the
    // resolved value into this plain `RwSignal` on each resolution (via
    // the `Effect` below) gives `<For>` one persistent signal to diff,
    // which is what lets it update an item in place instead of
    // unmounting/remounting the whole card - the fix for the collapse-
    // after-one-poll class of bugs the old `OpenBuildLog` (now deleted)
    // had to work around for just the build-log viewer specifically; this
    // generalizes that fix to every piece of a card's local state (expand,
    // confirm toggles, the build-log viewer, busy/error) all at once.
    //
    // This only pays off, though, if `<For>` itself isn't sitting inside a
    // closure that *also* reads `workspaces` - such a closure would re-run
    // (and rebuild its whole subtree, tearing `<For>` down) on every single
    // poll, since signal updates don't dedupe by value. See the `<Show>`
    // below, whose memoized `when` is what actually keeps `<For>` mounted
    // across polls - not just this stable signal on its own.
    //
    // Tradeoff accepted: `Effect`s only run client-side, so (unlike the
    // old `<Transition>`-based version) this list no longer participates
    // in SSR streaming - the server-rendered HTML always shows "Loading…"
    // and the real list appears once hydration runs this effect for the
    // first time. Fine for a localhost-only control panel with no SEO/
    // first-paint concerns; not a tradeoff worth making in a public app.
    let workspaces = RwSignal::new(Vec::<WorkspaceInfo>::new());
    let fetch_error = RwSignal::new(None::<String>);
    // Tracks "has the resource resolved at least once" as a plain signal,
    // rather than reading `workspaces_resource` directly in the view below
    // to distinguish "still loading" from "loaded but genuinely empty".
    // Leptos warns (correctly) about reading a resource outside
    // `Suspense`/`Transition`/an effect - doing so risks a hydration
    // mismatch if the server happens to resolve it before sending HTML
    // while the client's first hydration pass hasn't yet. Since `Effect`s
    // only run client-side, both the server's initial render *and* the
    // client's first hydration pass start from `loaded = false` and agree
    // on "Loading…" - this is actually more hydration-correct than reading
    // the resource directly would have been, not just quieter.
    let loaded = RwSignal::new(false);
    Effect::new(move |_| {
        if let Some(result) = workspaces_resource.get() {
            loaded.set(true);
            match result {
                Ok(list) => {
                    workspaces.set(list);
                    fetch_error.set(None);
                }
                // Deliberately leave `workspaces` as whatever it last was
                // on a transient fetch error, rather than clearing it to
                // empty - a single flaky poll shouldn't blank the whole
                // grid when we already have a perfectly good last-known
                // list to keep showing.
                Err(e) => fetch_error.set(Some(e.to_string())),
            }
        }
    });

    // Live polling: re-fetch every 3s on the client. `set_interval` touches
    // `window`, so it must run only in the browser — Effects run client-side.
    Effect::new(move |_| {
        set_interval(
            move || refresh.update(|n| *n += 1),
            std::time::Duration::from_secs(3),
        );
    });

    let show_browser = RwSignal::new(false);
    let on_changed = Callback::new(move |()| reload());

    // The currently open workspace's id, if any - drives the single detail
    // modal below. At most one workspace is ever "open" at a time (opening
    // a card replaces whatever was previously selected), unlike the old
    // per-card inline `expanded` state, which let any number of cards be
    // expanded simultaneously.
    let selected = RwSignal::new(None::<String>);

    // The DOM element of whichever `WorkspaceCard` is currently open (set
    // directly by its `on:click`, alongside the matching
    // `view-transition-name` it stamps onto that same element - see
    // `click_target`/`set_card_vt_name`). Stashed here so the *closing*
    // morph, triggered from inside the modal rather than the card, can
    // still get back to the exact element it needs to hand the name back
    // to. `None` whenever no workspace is selected.
    let open_card_el = StoredValue::new(None::<web_sys::HtmlElement>);

    view! {
        <header class="topbar">
            <h1>"GAVMO"</h1>
            <button class="primary" on:click=move |_| show_browser.set(true)>
                "Add workspace"
            </button>
        </header>

        <Show
            // `<Show>` memoizes this boolean (leptos's `show.rs`
            // wraps `when` in an `ArcMemo`), so the `<div class="ws-
            // grid"><For/></div>` subtree below is only torn down
            // and rebuilt when the list flips empty<->non-empty -
            // *not* on every 3s poll that leaves it non-empty. That
            // stability is what actually keeps `<For>` (and every
            // card's local state) alive across polls - the stable
            // `workspaces` signal above is necessary for `<For>` to
            // diff correctly, but isn't sufficient on its own if
            // something reading `workspaces` wraps `<For>` itself.
            // The previous `{move || if workspaces.get().is_empty()
            // {...} else {<For/>}}` did exactly that: its own
            // condition read `workspaces`, so it re-ran (and
            // remounted the whole grid) on every poll, and right
            // after any in-card action too (via `on_changed` ->
            // `reload()`).
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
                    // Braced (not bare `move || ...`): the
                    // turbofish in `::<Vec<_>>()` contains
                    // `<`/`>`, which the view! macro's
                    // RSX-style attribute parser otherwise
                    // misreads as tag syntax.
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

        // The detail modal - mounted only while a workspace is selected.
        // Captured with `get_untracked()` inside the (otherwise static,
        // non-reactive) closure below: `selected` only ever transitions
        // `None <-> Some(id)` (never `Some(a) -> Some(b)` directly, since
        // the backdrop blocks clicks on other cards while open), so the id
        // is stable for the modal's whole mounted lifetime and doesn't need
        // to react to further changes in `selected` itself.
        <Show when=move || selected.get().is_some() fallback=|| ()>
            {move || {
                let id = selected.get_untracked().expect("selected is Some");
                let name = vt_name(&id);
                view! {
                    <WorkspaceModal
                        id
                        workspaces
                        on_changed
                        on_close=Callback::new(move |()| {
                            // Captured *locally*, once, right as closing
                            // starts - rather than re-reading
                            // `open_card_el` later from inside the
                            // closures below - so a fast reopen (of a
                            // *different* card) racing this same close's
                            // own async cleanup can never stomp on each
                            // other: `open_card_el` is immediately cleared
                            // here, freeing a new `WorkspaceCard` click to
                            // overwrite it safely, while this close's own
                            // two closures keep working with the one
                            // element they captured up front.
                            let el = open_card_el.get_value();
                            open_card_el.set_value(None);
                            let el_for_cleanup = el.clone();
                            let name = name.clone();
                            start_view_transition(
                                move || {
                                    // New (post-close) snapshot: hand the
                                    // name back to the card so the modal
                                    // can morph back into it.
                                    if let Some(el) = &el {
                                        set_card_vt_name(el, Some(&name));
                                    }
                                    selected.set(None);
                                },
                                move || {
                                    let el = el_for_cleanup;
                                    // The whole close animation is done -
                                    // release the name again so it doesn't
                                    // linger on this card and get swept
                                    // into some later, unrelated
                                    // transition for a *different* card.
                                    if let Some(el) = &el {
                                        set_card_vt_name(el, None);
                                    }
                                },
                            );
                        })
                    />
                }
            }}
        </Show>

        // The "add workspace" folder picker, in the same modal shell.
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

/// A floating modal: a dimmed full-screen backdrop plus a centered square
/// panel. Clicking the backdrop or the close button, or pressing Escape,
/// all invoke `on_close`. Used for both the workspace detail view
/// ([`WorkspaceModal`]) and the "add workspace" folder picker.
///
/// `vt_name`, when `Some`, is applied to the panel as its
/// `view-transition-name` - this is what lets [`WorkspaceModal`] morph out
/// of (and back into) the card that opened it (see [`start_view_transition`]
/// and `vt_name`'s own doc comment). The folder picker passes `None`: it
/// has no corresponding card to morph from, so it just pops in/out with the
/// browser's default root crossfade like any other DOM change.
#[component]
fn Modal(
    on_close: Callback<()>,
    title: String,
    #[prop(optional_no_strip)] vt_name: Option<String>,
    children: Children,
) -> impl IntoView {
    // Escape-to-close. `window_event_listener` is a no-op on the server
    // (see its `is_server()` check) and is cleaned up via `on_cleanup` when
    // this component's owner is disposed - i.e. whenever the `<Show>` above
    // unmounts it, which is exactly when the listener should stop existing.
    let handle = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" {
            on_close.run(());
        }
    });
    on_cleanup(move || handle.remove());

    // Whether this modal has a card to morph from. When `true`, the panel
    // relies entirely on the native View Transition morph (driven by
    // `vt_name` below) for its open/close motion; when `false` (the folder
    // picker), there's no corresponding card, so it keeps the plain CSS
    // mount animation (`.modal--plain`) instead. Read before `vt_name` is
    // moved into the `style:` binding just below.
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

/// One workspace's card in the grid: just the compact, always-visible
/// summary (badge + name + path). Mounted once per `id` by `<For>` in
/// `HomePage` and never torn down while the id remains in the list (see
/// `HomePage`'s doc comment on `workspaces`). Clicking it opens
/// [`WorkspaceModal`] - all of the former inline detail/actions panel now
/// lives there instead, as a single floating modal shared across every
/// card rather than per-card state.
#[component]
fn WorkspaceCard(
    id: String,
    workspaces: RwSignal<Vec<WorkspaceInfo>>,
    selected: RwSignal<Option<String>>,
    open_card_el: StoredValue<Option<web_sys::HtmlElement>>,
) -> impl IntoView {
    let id = StoredValue::new(id);

    // This card's own reactive slice of the shared `workspaces` list - the
    // mechanism that lets status/path update live without the card itself
    // being recreated. Falls back to `Stopped` with this card's own `id` as
    // a placeholder path in the narrow window between this workspace being
    // removed from `workspaces` (e.g. via Remove) and `<For>` unmounting
    // this card in response to that same change - both happen off the same
    // signal in the same reactive pass, so this is never visible for more
    // than a frame, if at all.
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

    view! {
        <div
            class="ws-card"
            // No `view-transition-name` in steady state - only the one
            // card actually being opened/closed at any given moment ever
            // carries one (set directly below, bypassing Leptos entirely;
            // see `start_view_transition`'s doc comment for why). Giving
            // *every* card a permanent name here instead was the first
            // version of this - and it was wrong: a `view-transition-name`
            // pulls an element into its own composited layer for the
            // *entire* duration of *any* transition happening anywhere on
            // the page, regardless of whether that element is actually
            // involved. With every card permanently named, opening one
            // card lifted *all the others* (and nothing else) above the
            // backdrop's dimming layer for the whole animation, so they
            // visibly only caught up to the dimmed look once the real DOM
            // took back over at the end - the "other cards gray out late"
            // bug. Scoping the name to just the one involved card - set
            // right before the transition starts, cleared right after -
            // keeps every uninvolved card (and the "Add workspace" button)
            // correctly part of the plain, un-lifted root snapshot, so the
            // backdrop dims everything in one synchronized pass like any
            // ordinary DOM change would.
            on:click=move |ev| {
                let clicked = id.get_value();
                let name = vt_name(&clicked);
                let el = click_target(&ev);
                if let Some(el) = &el {
                    // Old snapshot: this card must already show the name
                    // - set synchronously, *before* `start_view_transition`
                    // (and the browser, inside it) captures it.
                    set_card_vt_name(el, Some(&name));
                }
                open_card_el.set_value(el.clone());
                start_view_transition(
                    move || {
                        // New snapshot: the modal claims the name now -
                        // this card must give it up, or the snapshot
                        // would have two elements sharing one name.
                        if let Some(el) = &el {
                            set_card_vt_name(el, None);
                        }
                        selected.set(Some(clicked));
                    },
                    // Nothing to release once *opening* finishes - the
                    // card legitimately stays unnamed for as long as the
                    // modal remains open; see the `on_close` handler in
                    // `HomePage` for the matching close-side cleanup.
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

/// The detail/actions view for one workspace, shown as the content of the
/// floating modal (see `HomePage`'s `selected` signal and `Modal`). Mounted
/// fresh each time a card is opened, and unmounted when it's closed - all
/// local state here (`busy`, confirm toggles, the build-log viewer) starts
/// over on each open, same as the rest of the modal's content would.
#[component]
fn WorkspaceModal(
    id: String,
    workspaces: RwSignal<Vec<WorkspaceInfo>>,
    on_changed: Callback<()>,
    on_close: Callback<()>,
) -> impl IntoView {
    // A `Copy` handle to this modal's id, even though `String` itself isn't.
    // Below, several closures end up nested multiple levels deep inside
    // `<Show>`'s children (which - like any Leptos reactive content - must
    // stay callable repeatedly, not just once). A *plain* `String` moved
    // into one `move` closure, then referenced by a `move` closure nested
    // inside *that*, hits a real borrow-checker wall: the inner `move`
    // takes ownership away from the outer closure's own captured copy,
    // leaving nothing for the outer closure's next invocation (`error[E0525]:
    // ...only implements FnOnce`) - no amount of `.clone()`-before-use
    // avoids this once there are two or more nesting levels, since each
    // `move` closure captures by value regardless of what its body actually
    // needs. `StoredValue` is Leptos's idiomatic answer: it hands out a
    // `Copy` handle backed by the reactive runtime's own storage, so *any*
    // number of nested `move` closures can freely capture the handle itself
    // (trivial, non-destructive, same as copying an integer) and each
    // independently call `.get_value()` for a fresh owned clone of the real
    // `String` whenever it actually runs.
    let id = StoredValue::new(id);

    // This modal's own reactive slice of the shared `workspaces` list - the
    // mechanism that lets status update live while the modal is open,
    // without the modal itself being recreated. Falls back to `Stopped`
    // with this modal's own `id` as a placeholder path in the narrow window
    // between this workspace being removed from `workspaces` (e.g. via
    // Remove) and `on_changed`/the caller closing the modal in response to
    // that same change.
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
    // Opening one of these closes the other, so the card never shows two
    // confirm prompts at once.
    let confirming_reset = RwSignal::new(false);
    let confirming_remove = RwSignal::new(false);
    // M2 build-log viewer: `open` gates visibility, `content` is the
    // fetched text (`None` while loading, same three-state shape the old
    // `OpenBuildLog` used).
    let build_log_open = RwSignal::new(false);
    let build_log_content = RwSignal::new(None::<Result<String, String>>);

    // Live boot console: the guest's serial output (kernel + systemd,
    // already sanitized server-side — see `registry::sanitize_console`),
    // polled once a second via `read_console_log` while this workspace is
    // `Booting`. `console_ref` is the mounted `<pre>` the auto-scroll
    // `Effect` further down keeps pinned to the bottom, same pairing
    // `set_card_vt_name` uses for the view-transition morph elsewhere in
    // this file.
    let console_text = RwSignal::new(String::new());
    let console_ref: NodeRef<leptos::html::Pre> = NodeRef::new();
    // Holds the running interval's handle (see `fetch_console` below) so
    // it can be torn down the moment this workspace stops `Booting` —
    // including when the whole modal unmounts, via the `on_cleanup` this
    // same handle is registered with. Not holding onto a finished boot's
    // interval forever is the whole point: with no stop condition, this
    // would otherwise keep silently polling a workspace long after its
    // console had anything new to say.
    let console_interval = StoredValue::new(None::<IntervalHandle>);

    let fetch_console = move || {
        let id = id.get_value();
        leptos::task::spawn_local(async move {
            if let Ok(text) = read_console_log(id).await {
                console_text.set(text);
            }
        });
    };

    // Starts polling the instant this workspace becomes `Booting`, and
    // stops the moment it settles into anything else — but not before one
    // last fetch, so the panel ends up showing the actual tail of the log
    // (e.g. the OpenCode startup line) rather than whatever it last showed
    // mid-poll a second earlier.
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

    // Keeps the console panel scrolled to the bottom as new output
    // arrives, same way a real terminal would — otherwise a user watching
    // a multi-minute boot would have to keep manually re-scrolling every
    // second. A no-op under `ssr` (there's no DOM to scroll), same as
    // `click_target`/`set_card_vt_name` elsewhere in this file.
    Effect::new(move |_| {
        console_text.track();
        if let Some(el) = console_ref.get() {
            scroll_to_bottom(&el);
        }
    });

    // Live host-side resource stats (CPU/memory/uptime/disk) for this
    // workspace's VM — see `registry::AppState::vm_stats`. Same
    // start/stop-polling shape as the boot console above, but gated on
    // `Running` instead of `Booting` (there's nothing meaningful to show
    // in any other state — `vm_stats` itself returns `None` then too).
    // Each poll's own request takes ~`CPU_SAMPLE_INTERVAL` to resolve (the
    // server needs two `/proc` samples a beat apart to compute a CPU
    // delta - see `registry.rs`), so this polls less frequently than the
    // boot console's 1s tick to avoid overlapping requests piling up.
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

    // Every capture below (`id`, `busy`, `action_error`, `on_changed`) is
    // now `Copy`, which makes these closures themselves `Copy` too - no
    // `.clone()` dance needed anywhere they're used, including from inside
    // the nested reactive branches further down (e.g. `reset`/`remove`
    // referenced from inside `{move || if confirming_reset.get() {...}}`).
    let open = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            // Fire-and-forget: this returns as soon as the boot starts in
            // the background. The badge picks up the idle -> booting… ->
            // running/failed transition via the live poll; the "Open UI"
            // link appears once running.
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
            // Can legitimately take a little while if it races a boot
            // that's just about to succeed - see
            // `registry::AppState::stop_workspace`'s doc comment for why
            // that's an accepted, bounded tradeoff of the R1 fix.
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

    // The modal's title - the path's last segment, same as the card's
    // name. Computed once (untracked) since the modal is mounted fresh per
    // open; a workspace's path/name doesn't change while it's open.
    let title = {
        let path = card_info.get_untracked().path;
        path.rsplit('/').next().unwrap_or(&path).to_string()
    };

    // Same name the clicked `WorkspaceCard` just handed off (see its
    // `style:view-transition-name`) - claiming it here is what makes the
    // browser morph that card into this modal, and back again on close.
    let vt_name = Some(vt_name(&id.get_value()));

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
                    // M2: if the last boot attempt failed because the
                    // repo's `gavmo.nix` didn't build, offer an
                    // on-demand viewer for the full `nixos-rebuild` log
                    // (`vm::Error::RebuildFailed` /
                    // `AppState::read_build_log`).
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
                            build_log_content.set(None); // "Loading…" until the fetch below resolves
                            let id = id.get_value();
                            leptos::task::spawn_local(async move {
                                let result = read_build_log(id, log).await.map_err(|e| e.to_string());
                                build_log_content.set(Some(result));
                            });
                        }>
                            {move || if build_log_open.get() { "Hide build log" } else { "View build log" }}
                        </button>
                    })}
                    // U3: a single Start/Stop toggle rather than two
                    // separate buttons - the live status always maps to
                    // exactly one valid action, so there's no need for
                    // per-status disabling the way the old two-button
                    // version needed (disabled states followed the live
                    // poll, not just `busy` - previously both stayed
                    // clickable for the ~3s between a fire-and-forget
                    // action returning and the next poll picking up the
                    // real status, which was exactly how a user could
                    // reach Reset while still `Booting` and hit the R1
                    // race that was paired with fixing). "Stop" while
                    // Booting/Running (lets an in-progress boot be
                    // cancelled, same as the old standalone Stop did);
                    // "Start" otherwise - idle, or retrying after
                    // Failed/BuildFailed. Disabled only while this
                    // card's own `busy` action is in flight.
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
                        disabled=move || busy.get()
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
                                // U3: disabled while `Booting` too - R1
                                // makes resetting mid-boot safe/race-
                                // free, but it's still a destructive
                                // action better reserved for a settled
                                // state; Stop (or just waiting) is the
                                // better tool for an in-progress boot.
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
                    // U5: a static hint rather than an elapsed-time
                    // counter - a live timer would need its own
                    // tick-driven signal here; not worth the added
                    // complexity for a first pass.
                    <p class="hint">
                        "First boot can take a few minutes - longer if "
                        <code>"gavmo.nix"</code>
                        " needs to fetch or build new packages."
                    </p>
                })}
                // Live boot console: visible for as long as there's
                // something to show — from the moment this workspace
                // starts `Booting` (even before the first poll resolves,
                // via the placeholder text below) through to the tail end
                // of the log fetched right as it settles into
                // `Running`/`Failed`/`BuildFailed` (see the `Effect` above
                // that drives `console_text`). Kept visible after boot
                // finishes rather than disappearing the instant status
                // flips, so the log doesn't vanish out from under a user
                // still reading it.
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
                // Live host-side resource stats: visible only while
                // `Running` *and* a poll has actually resolved at least
                // once (`vm_stats` starts `None` and the first poll takes
                // a beat - see `fetch_stats`/`CPU_SAMPLE_INTERVAL` - so
                // there'd otherwise be an empty flash right as the modal
                // for a running workspace opens).
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
                            // Guest-reported (virtio-balloon), not qemu's host-side
                            // RSS - see `VmStats::mem_used_bytes`'s doc comment for
                            // why. `None` means the guest's balloon driver hasn't
                            // reported yet (e.g. moments after boot) - show a plain
                            // placeholder rather than a misleading 0%/full bar.
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

/// The modal's Tools panel: lists the tools this workspace's `gavmo.yaml`
/// requests and lets the user configure each one's credential on the host. Loads
/// live (so it reflects config/secret changes), reloading itself after any
/// change so the configured/enabled status stays accurate.
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

    // One-shot initial load (the closure reads no reactive state, so the
    // effect runs exactly once after mount).
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

/// One tool's row in the Tools panel: its details, configured/enabled status,
/// and a form to set (or clear/test) its credential. Holds its own form state;
/// after a successful change it fires `on_changed` so the parent reloads (which
/// recreates this row with fresh status).
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
            </p>
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
        // `WorkspaceStatus::Stopped` covers both "never booted" and
        // "cleanly stopped" (see `vm::VmStatus::Stopped`) — "idle" avoids
        // implying the workspace was previously running.
        WorkspaceStatus::Stopped => ("idle", "idle".into(), None),
        WorkspaceStatus::Booting => ("booting", "booting…".into(), None),
        WorkspaceStatus::Running { url } => ("running", "running".into(), Some(url.clone())),
        WorkspaceStatus::Failed { reason } => ("failed", format!("failed: {reason}"), None),
        // Reuses the "failed" badge color - it's a real failure to the
        // user, just a more specific/actionable one (see `WorkspaceRow`'s
        // "View build log" button for the specific part).
        WorkspaceStatus::BuildFailed { .. } => ("failed", "gavmo.nix build failed".into(), None),
    }
}

/// Formats a byte count the way a human skimming a stats panel wants:
/// whole-number KiB, one decimal for MiB/GiB. Binary (1024-based) units,
/// matching how RSS/overlay-disk sizes are actually measured.
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

/// Formats a duration in seconds as a compact, human-readable uptime
/// (`"45s"`, `"3m 12s"`, `"2h 05m"`, `"1d 03h"`) — always the two most
/// significant units, never more, so it stays short in a compact stats
/// panel.
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

    let add_current = move |_| {
        let path = listing.get().and_then(|r| r.ok()).map(|l| l.path);
        if let Some(path) = path {
            leptos::task::spawn_local(async move {
                if let Err(e) = add_workspace(path).await {
                    leptos::logging::error!("add failed: {e}");
                }
                on_added.run(());
            });
        }
    };

    // U4: a manual path entry, since the click-through list below hides
    // dotfiles/dirs (`browse_dir`'s own filtering — see `app.rs`) and has
    // no way to type an exact destination. `browse_dir` already
    // canonicalizes and reports a clear error for anything invalid, so
    // jumping straight to a typed path reuses the exact same `listing`
    // resource/error path as clicking through would. Captures only `Copy`
    // signals, so the closure itself is `Copy` - reused as-is from both
    // `on:click` and `on:keydown` below without needing `.clone()`.
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
                                    <button on:click=add_current class="primary">
                                        "Use this directory"
                                    </button>
                                </div>
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
        assert_eq!(format_bytes(1_572_864), "1.5 MiB"); // 1.5 MiB
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
