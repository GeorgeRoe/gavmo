//! A workspace's tool integrations and their credentials.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use crate::api::{clear_tool_secret, list_tool_configs, set_tool_secret, test_tool};
use crate::types::ToolConfig;
use leptos::prelude::*;
/// The modal's Tools panel: lists the workspace's requested tools and lets
/// the user set each one's credential. Reloads after any change.
#[component]
pub(crate) fn WorkspaceTools(id: String) -> impl IntoView {
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

    // Reads no reactive state, so this runs once after mount.
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

/// One tool's row: details, status, and a form to set, clear, or test its
/// credential. Fires `on_changed` after a change so the parent reloads.
#[component]
#[expect(
    clippy::too_many_lines,
    reason = "split into smaller components in the UI-fixes commit"
)]
pub(crate) fn ToolRow(
    tool: ToolConfig,
    workspace_id: StoredValue<String>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let auth = tool.auth.clone();
    let is_basic = auth == "basic";
    let configured = tool.configured;
    let url_changed = tool.url_changed;
    let bound_url = tool.bound_url.clone();
    let approved_delivery = tool.approved_delivery.clone();
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
                } else if approved_delivery.is_some() {
                    view! { <span class="tool-status tool-status--warn">"auth method changed"</span> }.into_any()
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
                " as " {header.delivery.clone()}
            </p>
            {approved_delivery.clone().map(|was| view! {
                <div class="tool-warning">
                    <strong>"This tool's auth method changed."</strong>
                    <p>
                        "Your saved credential was approved to be sent as " {was}
                        " but this tool now sends it as " {header.delivery.clone()} "."
                    </p>
                    <p>"The credential is disabled until you re-enter it below to confirm."</p>
                </div>
            })}
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
