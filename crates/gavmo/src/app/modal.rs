//! A generic modal dialog.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use leptos::prelude::*;
/// A floating modal over a dimmed backdrop; backdrop click, the close button,
/// and Escape all call `on_close`. `vt_name` lets the panel morph from the
/// card that opened it; the folder picker passes `None` and has no card.
#[component]
pub(crate) fn Modal(
    on_close: Callback<()>,
    #[prop(into)] title: String,
    #[prop(optional_no_strip)] vt_name: Option<String>,
    children: Children,
) -> impl IntoView {
    // Escape-to-close; the listener is removed when the modal unmounts.
    // Browser only: there is no `window` to listen on when rendering on the
    // server (natively, the call aborts the process).
    #[cfg(feature = "hydrate")]
    {
        let handle = window_event_listener(leptos::ev::keydown, move |ev| {
            if ev.key() == "Escape" {
                on_close.run(());
            }
        });
        on_cleanup(move || handle.remove());
    }

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
