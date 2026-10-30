//! Card <-> modal morph (View Transitions API) and small DOM helpers.

#[cfg(feature = "hydrate")]
use leptos::prelude::{document, window};

// ---------------------------------------------------------------------------
// Card <-> modal morph (View Transitions API)
// ---------------------------------------------------------------------------

/// A stable `view-transition-name` for a workspace. Ids are filesystem paths,
/// which aren't valid CSS idents, so the id is hashed.
pub(crate) fn vt_name(id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    // Prefixed because a CSS ident must not start with a digit.
    format!("ws-vt-{:x}", hasher.finish())
}

/// Sends the browser to `/login` (whose page explains how to get a fresh
/// login link). No-op under `ssr`.
pub(crate) fn go_to_login() {
    #[cfg(feature = "hydrate")]
    {
        if let Err(e) = window().location().set_href("/login") {
            leptos::logging::warn!("could not redirect to /login: {e:?}");
        }
    }
}

/// The element a click listener is attached to (`current_target`, so clicks
/// on children still resolve to the card). Always `None` under `ssr`; the
/// shared signature spares callers a `#[cfg]` split.
pub(crate) fn click_target(ev: &web_sys::MouseEvent) -> Option<web_sys::HtmlElement> {
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
pub(crate) fn set_card_vt_name(el: &web_sys::HtmlElement, name: Option<&str>) {
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
pub(crate) fn scroll_to_bottom(el: &web_sys::HtmlPreElement) {
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
pub(crate) fn start_view_transition(
    update: impl FnOnce() + 'static,
    on_finished: impl FnOnce() + 'static,
) {
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
pub(crate) fn start_view_transition(update: impl FnOnce(), on_finished: impl FnOnce()) {
    update();
    on_finished();
}
