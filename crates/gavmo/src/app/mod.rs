//! Leptos app: the control-panel UI (components only; data types are in
//! [`crate::types`], server functions in [`crate::api`]). Compiled for both
//! `ssr` and `hydrate`.

#[cfg(test)]
#[cfg(feature = "hydrate")]
mod browser_tests;
mod card;
mod folder_browser;
mod format;
mod home;
mod hooks;
mod modal;
mod polling;
mod resources;
mod settings;
#[cfg(test)]
#[cfg(feature = "ssr")]
mod ssr_tests;
mod tools;
mod transitions;
mod workspace_modal;

use self::home::HomePage;
use self::settings::SettingsPage;
use leptos::prelude::*;
use leptos_meta::{MetaTags, Stylesheet, Title, provide_meta_context};
use leptos_router::{
    StaticSegment,
    components::{Route, Router, Routes},
};

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
                    <Route path=StaticSegment("settings") view=SettingsPage/>
                </Routes>
            </main>
        </Router>
    }
}
