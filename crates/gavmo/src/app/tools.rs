//! A workspace's tool integrations and their credentials.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use leptos::prelude::*;

use super::hooks::run_action;
use crate::api::{clear_tool_secret, list_tool_configs, set_tool_secret, test_tool};
use crate::types::{AppError, ToolConfig, ToolList};

/// The modal's Tools panel: lists the workspace's requested tools and lets
/// the user set each one's credential. Reloads after any change.
///
/// Rows are keyed by tool id and read their data reactively, so a reload
/// updates them in place: a row's last result ("Saved", a test response) and
/// anything typed into another row survive it.
#[component]
pub(crate) fn WorkspaceTools(id: String) -> impl IntoView {
    let id = StoredValue::new(id);
    // `None` = still loading.
    let list = RwSignal::new(None::<ToolList>);
    let load_error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);

    let reload = move || {
        run_action(
            loading,
            list_tool_configs(id.get_value()),
            move |result| match result {
                Ok(tools) => {
                    load_error.set(None);
                    list.set(Some(tools));
                }
                Err(e) => {
                    load_error.set(Some(e.to_string()));
                    list.update(|l| {
                        l.get_or_insert(ToolList {
                            tools: Vec::new(),
                            config_error: None,
                        });
                    });
                }
            },
        );
    };

    // Reads no reactive state, so this runs once after mount.
    Effect::new(move |_| reload());
    let on_changed = Callback::new(move |()| reload());

    let tool_ids = move || {
        list.with(|l| {
            l.as_ref()
                .map(|l| l.tools.iter().map(|t| t.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    };

    view! {
        <div class="tools-panel">
            <h3 class="tools-title">"Tools"</h3>
            {move || list.with(|l| match l {
                None => view! { <p class="empty">"Loading tools…"</p> }.into_any(),
                Some(ToolList { config_error: Some(e), .. }) => view! {
                    <p class="error">
                        <code>"gavmo.yaml"</code> " was ignored: " {e.clone()}
                    </p>
                }
                .into_any(),
                Some(l) if l.tools.is_empty() => view! {
                    <p class="hint">
                        "This workspace requests no tools. Add a "
                        <code>"gavmo.yaml"</code>
                        " to its repo to enable Jira or other integrations."
                    </p>
                }
                .into_any(),
                Some(_) => view! {
                    <p class="hint">
                        "Credential changes take effect the next time this workspace starts."
                    </p>
                }
                .into_any(),
            })}
            <div class="tool-list">
                <For each=tool_ids key=|tool_id| tool_id.clone() let:tool_id>
                    {
                        let wanted = tool_id.clone();
                        let tool = Memo::new(move |_| {
                            list.with(|l| {
                                l.as_ref()
                                    .and_then(|l| l.tools.iter().find(|t| t.id == wanted).cloned())
                            })
                        });
                        view! { <ToolRow tool workspace_id=id on_changed /> }
                    }
                </For>
            </div>
            {move || load_error.get().map(|e| view! {
                <p class="error">"Failed to load tools: " {e}</p>
            })}
        </div>
    }
}

/// One tool's row: details, status, warnings, and the credential form.
/// `tool` is `None` only in the instant before a removed tool's row unmounts.
#[component]
fn ToolRow(
    tool: Memo<Option<ToolConfig>>,
    workspace_id: StoredValue<String>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let details = move || {
        tool.get().map(|t| {
            view! {
                <div class="tool-head">
                    <span class="tool-name">{t.id.clone()}</span>
                    <ToolStatusBadge tool=t.clone() />
                </div>
                <p class="tool-desc">{t.description.clone()}</p>
                <p class="tool-url">
                    <span class="tool-url-label">"Sends your credential to: "</span>
                    <code>{t.base_url.clone()}</code>
                    " as " {t.delivery.clone()}
                </p>
                <ToolWarnings tool=t.clone() />
                {t.instructions.clone().map(|text| view! { <p class="hint">{text}</p> })}
            }
        })
    };
    view! {
        <div class="tool-row">
            {details}
            <CredentialForm tool workspace_id on_changed />
        </div>
    }
    .into_any()
}

#[component]
fn ToolStatusBadge(tool: ToolConfig) -> impl IntoView {
    if tool.url_changed {
        view! { <span class="tool-status tool-status--warn">"destination changed"</span> }
            .into_any()
    } else if tool.approved_delivery.is_some() {
        view! { <span class="tool-status tool-status--warn">"auth method changed"</span> }
            .into_any()
    } else if tool.configured {
        view! { <span class="tool-status tool-status--ok">"configured"</span> }.into_any()
    } else {
        view! { <span class="tool-status">"not configured"</span> }.into_any()
    }
}

/// Why a stored credential is currently withheld, if it is.
#[component]
fn ToolWarnings(tool: ToolConfig) -> impl IntoView {
    const DISABLED: &str = "The credential is disabled until you re-enter it below to confirm.";
    let delivery_warning = tool.approved_delivery.clone().map(|was| {
        view! {
            <div class="tool-warning">
                <strong>"This tool's auth method changed."</strong>
                <p>
                    "Your saved credential was approved to be sent as " {was}
                    " but this tool now sends it as " {tool.delivery.clone()} "."
                </p>
                <p>{DISABLED}</p>
            </div>
        }
    });
    let url_warning = tool.url_changed.then(|| {
        let now = tool.base_url.clone();
        view! {
            <div class="tool-warning">
                <strong>"This tool's destination changed."</strong>
                {match tool.bound_url.clone() {
                    Some(was) => view! {
                        <p>
                            "Your saved credential was approved for " <code>{was}</code>
                            " but this tool now points at " <code>{now}</code> "."
                        </p>
                    }
                    .into_any(),
                    None => view! {
                        <p>
                            "Your saved credential wasn't bound to a destination (it predates \
                             URL-binding). Re-enter it to confirm it should go to "
                            <code>{now}</code> "."
                        </p>
                    }
                    .into_any(),
                }}
                <p>{DISABLED}</p>
            </div>
        }
    });
    view! { {delivery_warning} {url_warning} }
}

/// Save / update / test / clear a tool's credential. Mounted once per row,
/// so its inputs and last result survive the panel's reloads.
#[component]
fn CredentialForm(
    tool: Memo<Option<ToolConfig>>,
    workspace_id: StoredValue<String>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let username = RwSignal::new(String::new());
    let token = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    // Result of the last save/clear/test, shown inline (Ok = green, Err = red).
    let result = RwSignal::new(None::<Result<String, String>>);

    let is_basic = move || tool.with(|t| t.as_ref().is_some_and(|t| t.auth == "basic"));
    let configured = move || tool.with(|t| t.as_ref().is_some_and(|t| t.configured));
    let tool_id = move || tool.with(|t| t.as_ref().map(|t| t.id.clone()).unwrap_or_default());
    let show = move |outcome: Result<String, AppError>| {
        result.set(Some(outcome.map_err(|e| e.to_string())));
    };

    let save = move |_| {
        let token_value = token.get_untracked();
        if token_value.is_empty() {
            result.set(Some(Err("Enter a token first".to_owned())));
            return;
        }
        let username_value = is_basic().then(|| username.get_untracked());
        result.set(None);
        let call = set_tool_secret(
            workspace_id.get_value(),
            tool_id(),
            username_value,
            token_value,
        );
        run_action(busy, call, move |outcome| {
            if outcome.is_ok() {
                token.set(String::new());
                on_changed.run(());
            }
            show(outcome.map(|()| "Saved".to_owned()));
        });
    };
    let clear = move |_| {
        result.set(None);
        let call = clear_tool_secret(workspace_id.get_value(), tool_id());
        run_action(busy, call, move |outcome| {
            if outcome.is_ok() {
                on_changed.run(());
            }
            show(outcome.map(|()| "Credential cleared".to_owned()));
        });
    };
    let test = move |_| {
        result.set(None);
        run_action(busy, test_tool(workspace_id.get_value(), tool_id()), show);
    };

    view! {
        <div class="tool-form">
            {move || is_basic().then(|| view! {
                <input
                    class="tool-input"
                    r#type="text"
                    placeholder="Username / email"
                    autocomplete="off"
                    prop:value=move || username.get()
                    on:input=move |ev| username.set(event_target_value(&ev))
                    disabled=move || busy.get()
                />
            })}
            // `new-password`: a credential for a third-party service, not a
            // login to this page, so the browser shouldn't offer to save it.
            <input
                class="tool-input"
                r#type="password"
                autocomplete="new-password"
                placeholder=move || {
                    if configured() { "Enter a new token to replace" } else { "Token / API key" }
                }
                prop:value=move || token.get()
                on:input=move |ev| token.set(event_target_value(&ev))
                disabled=move || busy.get()
            />
            <div class="tool-actions">
                <button on:click=save disabled=move || busy.get()>
                    {move || if configured() { "Update" } else { "Save" }}
                </button>
                {move || configured().then(|| view! {
                    <button on:click=test disabled=move || busy.get()>"Test"</button>
                    <button class="danger" on:click=clear disabled=move || busy.get()>"Clear"</button>
                })}
            </div>
        </div>
        {move || result.get().map(|r| match r {
            Ok(msg) => view! { <pre class="tool-result tool-result--ok">{msg}</pre> }.into_any(),
            Err(msg) => view! { <pre class="tool-result tool-result--err">{msg}</pre> }.into_any(),
        })}
    }
    .into_any()
}
