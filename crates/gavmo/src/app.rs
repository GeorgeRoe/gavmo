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
    Running { url: String },
    Failed { reason: String },
    /// The repo's `gavmo.nix` failed to build (detected within seconds via
    /// the guest's failure marker, not a full boot-health timeout — see
    /// `nix/base/configuration.nix`'s `gavmo-rebuild` service). `log` is the
    /// failed run's log filename, fetchable via [`read_build_log`].
    BuildFailed { log: String },
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
    state
        .open_workspace(&id)
        .await
        .map_err(ServerFnError::new)
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
        None => dirs::home_dir()
            .ok_or_else(|| ServerFnError::new("no home directory"))?,
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
            <main>
                <Routes fallback=|| "Page not found.".into_view()>
                    <Route path=StaticSegment("") view=HomePage/>
                </Routes>
            </main>
        </Router>
    }
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

    view! {
        <div class="panel">
            <section class="section section-header">
                <h1>"GAVMO"</h1>
                <div class="toolbar">
                    <button
                        class="primary"
                        on:click=move |_| show_browser.update(|v| *v = !*v)
                    >
                        {move || if show_browser.get() { "Close picker" } else { "Add workspace" }}
                    </button>
                </div>
            </section>

            <Show when=move || show_browser.get() fallback=|| ()>
                <section class="section">
                    <FolderBrowser on_added=Callback::new(move |()| {
                        show_browser.set(false);
                        reload();
                    })/>
                </section>
            </Show>

            <section class="section section-grid">
                <Show
                    // `<Show>` memoizes this boolean (leptos's `show.rs`
                    // wraps `when` in an `ArcMemo`), so the `<div class="ws-
                    // grid"><For/></div>` subtree below is only torn down
                    // and rebuilt when the list flips empty<->non-empty -
                    // *not* on every 3s poll that leaves it non-empty. That
                    // stability is what actually keeps `<For>` (and every
                    // card's local state: expanded, confirm toggles, the
                    // build-log viewer) alive across polls - the stable
                    // `workspaces` signal above is necessary for `<For>` to
                    // diff correctly, but isn't sufficient on its own if
                    // something reading `workspaces` wraps `<For>` itself.
                    // The previous `{move || if workspaces.get().is_empty()
                    // {...} else {<For/>}}` did exactly that: its own
                    // condition read `workspaces`, so it re-ran (and
                    // remounted the whole grid, collapsing any expanded
                    // card) on every poll, and right after any in-card
                    // action too (via `on_changed` -> `reload()`).
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
                            <WorkspaceCard id workspaces on_changed/>
                        </For>
                    </div>
                </Show>
                {move || fetch_error.get().map(|e| view! {
                    <p class="error">"Failed to refresh workspaces: " {e}</p>
                })}
            </section>
        </div>
    }
}

/// One workspace's card in the grid: a compact header (always visible) that
/// expands on click into the full detail/actions panel. Mounted once per
/// `id` by `<For>` in `HomePage` and never torn down while the id remains
/// in the list (see `HomePage`'s doc comment on `workspaces`) - so every
/// signal declared here (`expanded`, `busy`, confirm toggles, the build-log
/// viewer) persists across every live-status poll, not just one.
#[component]
fn WorkspaceCard(
    id: String,
    workspaces: RwSignal<Vec<WorkspaceInfo>>,
    on_changed: Callback<()>,
) -> impl IntoView {
    // A `Copy` handle to this card's id, even though `String` itself isn't.
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

    // This card's own reactive slice of the shared `workspaces` list - the
    // mechanism that lets status/path update live without the card itself
    // (and all the state below) being recreated. Falls back to `Stopped`
    // with this card's own `id` as a placeholder path in the narrow window
    // between this workspace being removed from `workspaces` (e.g. via
    // Remove) and `<For>` unmounting this card in response to that same
    // change - both happen off the same signal in the same reactive pass,
    // so this is never visible for more than a frame, if at all.
    let card_info = Memo::new(move |_| {
        let current_id = id.get_value();
        workspaces.get().into_iter().find(|w| w.id == current_id).unwrap_or_else(|| WorkspaceInfo {
            id: current_id.clone(),
            path: current_id,
            status: WorkspaceStatus::Stopped,
        })
    });

    let expanded = RwSignal::new(false);
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

    view! {
        <div class="ws-card" class:expanded=move || expanded.get()>
            <div class="ws-card-header" on:click=move |_| expanded.update(|e| *e = !*e)>
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
                <span class="chevron" aria-hidden="true">
                    {move || if expanded.get() { "▾" } else { "▸" }}
                </span>
            </div>
            <Show when=move || expanded.get()>
                <div class="ws-card-detail" on:click=|ev| ev.stop_propagation()>
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
                </div>
            </Show>
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
