//! The add-workspace folder picker.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use crate::api::{add_workspace, browse_dir};
use leptos::prelude::*;
#[component]
pub(crate) fn FolderBrowser(on_added: Callback<()>) -> impl IntoView {
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
