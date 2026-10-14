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

/// Which M2 build-failure log (if any) is currently expanded, lifted up to
/// `HomePage` — deliberately **not** local state inside `WorkspaceRow`.
///
/// `HomePage`'s 3-second live poll (see the `Effect` below) re-fetches the
/// whole workspace list and re-renders every `WorkspaceRow` from scratch via
/// `<Transition>`'s `Suspend` block. A signal local to `WorkspaceRow` gets
/// thrown away and recreated on every one of those polls, which is why an
/// earlier version of this panel would silently collapse a few seconds after
/// opening — not a fetch bug, a "the component holding the state no longer
/// exists" bug. `HomePage` itself is never recreated, so state here survives
/// the poll; the log's own rendering also lives in `HomePage` (outside
/// `<Transition>`, see below) for the same reason — rendering it *inside*
/// the polled subtree would still tear down and rebuild the `<pre>` every
/// 3s, resetting scroll position even if the text itself didn't disappear.
///
/// `key` is `(workspace_id, log_filename)` — `None` means no log is open.
/// `content` is the fetched text, `None` while loading.
#[derive(Clone, Copy)]
struct OpenBuildLog {
    key: RwSignal<Option<(String, String)>>,
    content: RwSignal<Option<Result<String, String>>>,
}

impl OpenBuildLog {
    fn new() -> Self {
        Self {
            key: RwSignal::new(None),
            content: RwSignal::new(None),
        }
    }

    fn close(&self) {
        self.key.set(None);
        self.content.set(None);
    }
}

#[component]
fn HomePage() -> impl IntoView {
    // Bumped to force a re-fetch of the workspace list (after an action, and
    // on a live polling interval — see the Effect below).
    let refresh = RwSignal::new(0u32);
    let reload = move || refresh.update(|n| *n += 1);

    let workspaces = Resource::new(
        move || refresh.get(),
        |_| async move { list_workspaces().await },
    );

    // Live polling: re-fetch every 3s on the client. `set_interval` touches
    // `window`, so it must run only in the browser — Effects run client-side.
    Effect::new(move |_| {
        set_interval(
            move || refresh.update(|n| *n += 1),
            std::time::Duration::from_secs(3),
        );
    });

    let show_browser = RwSignal::new(false);
    let open_build_log = OpenBuildLog::new();

    view! {
        <h1>"GAVMO — workspaces"</h1>

        <div class="toolbar">
            <button
                class="primary"
                on:click=move |_| show_browser.update(|v| *v = !*v)
            >
                {move || if show_browser.get() { "Close picker" } else { "Add workspace" }}
            </button>
        </div>

        <Show when=move || show_browser.get() fallback=|| ()>
            <FolderBrowser on_added=Callback::new(move |()| {
                show_browser.set(false);
                reload();
            })/>
        </Show>

        // `Transition` (not `Suspense`): the list polls every 3s via
        // `refresh`, and `Suspense` drops back to its fallback on every
        // refetch, flashing "Loading…" each tick. `Transition` keeps the
        // previously-rendered rows on screen while the next poll resolves.
        <Transition fallback=move || view! { <p class="empty">"Loading…"</p> }>
            {move || Suspend::new(async move {
                match workspaces.await {
                    Ok(list) if list.is_empty() => view! {
                        <p class="empty">"No workspaces yet. Click \"Add workspace\" to pick a repo."</p>
                    }.into_any(),
                    Ok(list) => view! {
                        <div>
                            {list.into_iter().map(|ws| view! {
                                <WorkspaceRow ws on_changed=Callback::new(move |()| reload()) open_build_log/>
                            }).collect_view()}
                        </div>
                    }.into_any(),
                    Err(e) => view! {
                        <p class="error">"Failed to load workspaces: " {e.to_string()}</p>
                    }.into_any(),
                }
            })}
        </Transition>

        // Deliberately rendered here, outside `<Transition>` above - see
        // `OpenBuildLog`'s doc comment for why.
        {move || open_build_log.key.get().map(|(_, log)| view! {
            <div class="build-log-panel">
                <div class="build-log-panel-header">
                    <span>{format!("Build log: {log}")}</span>
                    <button on:click=move |_| open_build_log.close()>"Close"</button>
                </div>
                {move || match open_build_log.content.get() {
                    None => view! { <p class="empty">"Loading…"</p> }.into_any(),
                    Some(Ok(text)) => view! { <pre class="build-log">{text}</pre> }.into_any(),
                    Some(Err(e)) => view! {
                        <p class="error">"Failed to load build log: " {e}</p>
                    }.into_any(),
                }}
            </div>
        })}
    }
}

#[component]
fn WorkspaceRow(ws: WorkspaceInfo, on_changed: Callback<()>, open_build_log: OpenBuildLog) -> impl IntoView {
    let id = ws.id.clone();
    let busy = RwSignal::new(false);

    let open_error = RwSignal::new(None::<String>);

    let open = {
        let id = id.clone();
        move |_| {
            let id = id.clone();
            busy.set(true);
            open_error.set(None);
            leptos::task::spawn_local(async move {
                // Fire-and-forget: this returns as soon as the boot starts
                // in the background. The badge below picks up the
                // idle -> booting… -> running/failed transition via the
                // live poll; the "Open UI" link appears once running.
                if let Err(e) = open_workspace(id).await {
                    leptos::logging::error!("open failed: {e}");
                    open_error.set(Some(e.to_string()));
                }
                busy.set(false);
                on_changed.run(());
            });
        }
    };

    let stop = {
        let id = id.clone();
        move |_| {
            let id = id.clone();
            busy.set(true);
            leptos::task::spawn_local(async move {
                if let Err(e) = stop_workspace(id).await {
                    leptos::logging::error!("stop failed: {e}");
                }
                busy.set(false);
                on_changed.run(());
            });
        }
    };

    // Reset is destructive (wipes the overlay disk + OpenCode history for
    // this workspace), so it's gated behind an inline two-step confirm
    // rather than firing on the first click — `confirming_reset` toggles
    // that prompt. The actual reset reuses `busy` (shared with Open/Stop)
    // so no two destructive/lifecycle actions on the same row can overlap.
    let confirming_reset = RwSignal::new(false);
    let reset = {
        let id = id.clone();
        move |_| {
            let id = id.clone();
            busy.set(true);
            leptos::task::spawn_local(async move {
                if let Err(e) = reset_workspace(id).await {
                    leptos::logging::error!("reset failed: {e}");
                }
                busy.set(false);
                confirming_reset.set(false);
                on_changed.run(());
            });
        }
    };

    let (badge_class, badge_text, url) = status_view(&ws.status);

    // M2: if the last boot attempt failed because the repo's `gavmo.nix`
    // didn't build, offer an on-demand viewer for the full `nixos-rebuild`
    // log (`vm::Error::RebuildFailed` / `AppState::read_build_log`). `None`
    // for every other status, so the button below simply doesn't render.
    // The viewer's open/loaded state lives in `open_build_log`, shared
    // across all rows and lifted up to `HomePage` — see `OpenBuildLog`'s
    // doc comment for why it can't be a signal local to this component.
    let build_log_name = match &ws.status {
        WorkspaceStatus::BuildFailed { log } => Some(log.clone()),
        _ => None,
    };
    let is_log_open = {
        let id = id.clone();
        let build_log_name = build_log_name.clone();
        move || match &build_log_name {
            Some(log) => open_build_log.key.get() == Some((id.clone(), log.clone())),
            None => false,
        }
    };
    let toggle_build_log = {
        let id = id.clone();
        let build_log_name = build_log_name.clone();
        move |_| {
            let Some(log) = build_log_name.clone() else {
                return;
            };
            if open_build_log.key.get_untracked() == Some((id.clone(), log.clone())) {
                open_build_log.close();
                return;
            }
            open_build_log.key.set(Some((id.clone(), log.clone())));
            open_build_log.content.set(None); // shows "Loading…" until the fetch below resolves
            let id = id.clone();
            leptos::task::spawn_local(async move {
                let result = read_build_log(id, log).await.map_err(|e| e.to_string());
                open_build_log.content.set(Some(result));
            });
        }
    };

    view! {
        <div class="ws">
            <div class="ws-row">
                <span class=format!("badge {badge_class}")>{badge_text}</span>
                <span class="path">{ws.path.clone()}</span>
                <div class="actions">
                    {url.map(|u| view! {
                        <a href=u target="_blank" rel="noreferrer">
                            <button>"Open UI"</button>
                        </a>
                    })}
                    {build_log_name.clone().map(|_| view! {
                        <button on:click=toggle_build_log>
                            {move || if is_log_open() { "Hide build log" } else { "View build log" }}
                        </button>
                    })}
                    <button on:click=open disabled=move || busy.get()>"Open"</button>
                    <button on:click=stop disabled=move || busy.get()>"Stop"</button>
                    {move || if confirming_reset.get() {
                        // `reset` is cloned here (not used directly) because
                        // this whole branch is re-evaluated every time
                        // `confirming_reset` changes - moving `reset` itself
                        // into `on:click` would only let this reactive
                        // closure run once (FnOnce), but Leptos needs to
                        // call it repeatedly (FnMut) as the toggle flips.
                        let reset = reset.clone();
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
                                on:click=move |_| confirming_reset.set(true)
                                disabled=move || busy.get()
                            >
                                "Reset"
                            </button>
                        }.into_any()
                    }}
                </div>
            </div>
            {move || open_error.get().map(|e| view! {
                <p class="error">{e}</p>
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

    view! {
        <div class="browser">
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
