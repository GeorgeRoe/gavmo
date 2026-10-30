//! A workspace's card in the grid.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use super::format::status_view;
use super::transitions::{click_target, set_card_vt_name, start_view_transition, vt_name};
use crate::types::{WorkspaceInfo, WorkspaceStatus};
use leptos::prelude::*;
/// One workspace's card in the grid (badge, name, path). Clicking it opens
/// [`WorkspaceModal`].
#[component]
pub(crate) fn WorkspaceCard(
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
