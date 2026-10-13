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

/// Browser-facing mirror of `vm::VmStatus`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceStatus {
    Stopped,
    Booting,
    Running { url: String },
    Failed { reason: String },
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
                                <WorkspaceRow ws on_changed=Callback::new(move |()| reload())/>
                            }).collect_view()}
                        </div>
                    }.into_any(),
                    Err(e) => view! {
                        <p class="error">"Failed to load workspaces: " {e.to_string()}</p>
                    }.into_any(),
                }
            })}
        </Transition>
    }
}

#[component]
fn WorkspaceRow(ws: WorkspaceInfo, on_changed: Callback<()>) -> impl IntoView {
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

    let (badge_class, badge_text, url) = status_view(&ws.status);

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
                    <button on:click=open disabled=move || busy.get()>"Open"</button>
                    <button on:click=stop disabled=move || busy.get()>"Stop"</button>
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
