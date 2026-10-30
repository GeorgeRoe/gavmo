//! The workspace grid page.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use super::card::WorkspaceCard;
use super::folder_browser::FolderBrowser;
use super::modal::Modal;
use super::transitions::{go_to_login, set_card_vt_name, start_view_transition, vt_name};
use super::workspace_modal::WorkspaceModal;
use crate::api::list_workspaces;
use crate::types::{AppError, WorkspaceInfo};
use leptos::prelude::*;
/// The workspace grid, the detail modal for the selected workspace, and the
/// add-workspace dialog.
#[component]
pub(crate) fn HomePage() -> impl IntoView {
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
                // An expired session would fail every poll: go log in again.
                Err(AppError::Unauthorized) => go_to_login(),
                // Keep the last-known list so one flaky poll doesn't blank the grid.
                Err(e) => fetch_error.set(Some(e.to_string())),
            }
        }
    });

    // Poll every 3s. In an Effect because `set_interval` needs `window`;
    // cleared if the page unmounts.
    let poll = StoredValue::new(None::<IntervalHandle>);
    Effect::new(move |_| {
        if poll.get_value().is_none() {
            poll.set_value(
                set_interval_with_handle(reload, std::time::Duration::from_secs(3)).ok(),
            );
        }
    });
    on_cleanup(move || {
        if let Some(handle) = poll.get_value() {
            handle.clear();
        }
    });

    let show_browser = RwSignal::new(false);
    let on_changed = Callback::new(move |()| reload());

    // The id of the workspace shown in the detail modal, if any.
    let selected = RwSignal::new(None::<String>);

    // The open card's element, so the closing morph (started from the modal)
    // can hand the `view-transition-name` back to it.
    let open_card_el = StoredValue::new(None::<web_sys::HtmlElement>);

    view! {
        <TopBar on_add=Callback::new(move |()| show_browser.set(true)) />

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
                let on_close = close_with_morph(vt_name(&id), selected, open_card_el);
                Some(view! { <WorkspaceModal id workspaces on_changed on_close /> })
            }}
        </Show>

        <Show when=move || show_browser.get() fallback=|| ()>
            <Modal on_close=Callback::new(move |()| show_browser.set(false)) title="Add workspace">
                <FolderBrowser on_added=Callback::new(move |()| {
                    show_browser.set(false);
                    reload();
                })/>
            </Modal>
        </Show>
    }
}

/// The page header: title, "Add workspace" and "Log out everywhere".
#[component]
fn TopBar(on_add: Callback<()>) -> impl IntoView {
    view! {
        <header class="topbar">
            <h1>"GAVMO"</h1>
            <div class="topbar-actions">
                <button class="primary" on:click=move |_| on_add.run(())>
                    "Add workspace"
                </button>
                // A plain same-origin form POST; the server ends every session.
                <form method="post" action="/logout">
                    <button
                        r#type="submit"
                        title="End every browser session; log in again with the URL gavmo prints"
                    >
                        "Log out everywhere"
                    </button>
                </form>
            </div>
        </header>
    }
}

/// The detail modal's close action: morphs the modal back into its card,
/// then deselects it.
fn close_with_morph(
    name: String,
    selected: RwSignal<Option<String>>,
    open_card_el: StoredValue<Option<web_sys::HtmlElement>>,
) -> Callback<()> {
    Callback::new(move |()| {
        // Captured once and cleared immediately, so a fast reopen of another
        // card can't race this close's async cleanup.
        let el = open_card_el.get_value();
        open_card_el.set_value(None);
        let el_for_cleanup = el.clone();
        let name = name.clone();
        start_view_transition(
            move || {
                // Hand the name back so the modal morphs into the card.
                if let Some(el) = &el {
                    set_card_vt_name(el, Some(&name));
                }
                selected.set(None);
            },
            move || {
                // Animation done: release the name so it doesn't leak into a
                // later transition.
                if let Some(el) = &el_for_cleanup {
                    set_card_vt_name(el, None);
                }
            },
        );
    })
}
