//! The settings page: model providers, the gateway, and the default models.
//!
//! Provider forms are schema-driven: [`ProviderCard`] renders whatever
//! fields a [`ProviderView`] lists, so adding a provider needs no UI code.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use std::collections::BTreeMap;

use leptos::prelude::*;

use super::hooks::run_action;
use super::workspace_modal::{ConfirmButton, Confirming};
use crate::api::{
    get_model_settings, refresh_model_catalog, remove_provider, save_provider, set_default_models,
    test_model_gateway,
};
use crate::types::{
    AppError, GatewayStatus, ModelCatalogStatus, ModelSettingsView, ProviderCheck, ProviderField,
    ProviderView,
};

/// The `/settings` route.
#[component]
pub(crate) fn SettingsPage() -> impl IntoView {
    // `None` = still loading.
    let settings = RwSignal::new(None::<ModelSettingsView>);
    let load_error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let reload = move || {
        run_action(loading, get_model_settings(), move |result| match result {
            Ok(view) => {
                load_error.set(None);
                settings.set(Some(view));
            }
            Err(e) => load_error.set(Some(e.to_string())),
        });
    };
    // Reads no reactive state, so this runs once after mount.
    Effect::new(move |_| reload());
    let on_changed = Callback::new(move |()| reload());

    let kinds = move || {
        settings.with(|s| {
            s.as_ref()
                .map(|s| {
                    s.providers
                        .iter()
                        .map(|p| p.kind.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };

    view! {
        <header class="topbar">
            <h1><a class="home-link" href="/">"GAVMO"</a> " / Settings"</h1>
            <div class="topbar-actions">
                <a class="button-link" href="/">"Back to workspaces"</a>
            </div>
        </header>

        <section class="settings-section">
            <h2 class="settings-title">"Models"</h2>
            <p class="hint">
                "Agents in every workspace reach models through GAVMO's model gateway \
                 (Bifrost), which runs on this machine: provider credentials never \
                 enter a VM. Model changes apply to workspaces started afterwards."
            </p>
            <Show
                when=move || settings.with(Option::is_some)
                fallback=move || view! {
                    <p class="empty">
                        {move || if load_error.with(Option::is_some) { "" } else { "Loading…" }}
                    </p>
                }
            >
                <GatewayPanel settings on_changed />
                <h3 class="settings-subtitle">"Providers"</h3>
                <div class="provider-list">
                    <For each=kinds key=|kind| kind.clone() let:kind>
                        {
                            let provider = Memo::new(move |_| {
                                settings.with(|s| {
                                    s.as_ref().and_then(|s| {
                                        s.providers.iter().find(|p| p.kind == kind).cloned()
                                    })
                                })
                            });
                            view! { <ProviderCard provider on_changed /> }
                        }
                    </For>
                </div>
                <h3 class="settings-subtitle">"Default models"</h3>
                <DefaultModels settings on_changed />
            </Show>
            {move || load_error.get().map(|e| view! {
                <p class="error">"Failed to load the settings: " {e}</p>
            })}
        </section>
    }
    .into_any()
}

/// The gateway's state, with a button that sends it a test request.
#[component]
pub(super) fn GatewayPanel(
    settings: RwSignal<Option<ModelSettingsView>>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<String, String>>);
    let status = move || settings.with(|s| s.as_ref().map(|s| s.gateway.clone()));
    let test = move |_| {
        result.set(None);
        run_action(busy, test_model_gateway(), move |outcome| {
            result.set(Some(outcome.map_err(|e| e.to_string())));
            // The test may have (re)started the gateway; show its new state.
            on_changed.run(());
        });
    };
    view! {
        <div class="gateway-panel">
            <span class="settings-label">"Gateway"</span>
            {move || status().map(|s| view! { <GatewayBadge status=s /> })}
            <button on:click=test disabled=move || busy.get()>
                {move || if busy.get() { "Testing…" } else { "Test" }}
            </button>
        </div>
        {move || status().and_then(|s| gateway_problem(&s)).map(|why| view! {
            <p class="hint">{why}</p>
        })}
        <ActionResult result />
    }
    .into_any()
}

#[component]
fn GatewayBadge(status: GatewayStatus) -> impl IntoView {
    let (class, label) = match status {
        GatewayStatus::Ready => ("tool-status tool-status--ok", "ready"),
        GatewayStatus::Starting => ("tool-status tool-status--warn", "starting"),
        GatewayStatus::Failing { .. } => ("tool-status tool-status--err", "failing"),
        GatewayStatus::NotConfigured { .. } => ("tool-status", "not running"),
    };
    view! { <span class=class>{label}</span> }
}

/// Why the gateway isn't serving, if it isn't (`None` while it starts).
pub(super) fn gateway_problem(status: &GatewayStatus) -> Option<String> {
    match status {
        GatewayStatus::Ready | GatewayStatus::Starting => None,
        GatewayStatus::NotConfigured { reason } => Some(capitalize(reason)),
        GatewayStatus::Failing { reason } => Some(format!("It keeps failing: {reason}")),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// The last action's outcome, shown inline (Ok = green, Err = red).
#[component]
pub(super) fn ActionResult(result: RwSignal<Option<Result<String, String>>>) -> impl IntoView {
    move || {
        result.get().map(|r| match r {
            Ok(msg) => view! { <pre class="tool-result tool-result--ok">{msg}</pre> }.into_any(),
            Err(msg) => view! { <pre class="tool-result tool-result--err">{msg}</pre> }.into_any(),
        })
    }
}

/// One provider: its status, host-side checks, form, and Save / Remove.
/// Mounted once per provider, so typed values and the last result survive
/// the page's reloads. `provider` is `None` only in the instant before a
/// card unmounts.
#[component]
pub(super) fn ProviderCard(
    provider: Memo<Option<ProviderView>>,
    on_changed: Callback<()>,
) -> impl IntoView {
    // What the user typed, by field key; other fields show their value.
    let edits = RwSignal::new(BTreeMap::<String, String>::new());
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<String, String>>);
    let confirming = RwSignal::new(None::<Confirming>);

    let configured = move || provider.with(|p| p.as_ref().is_some_and(|p| p.configured));
    let removable =
        move || provider.with(|p| p.as_ref().is_some_and(|p| p.configured && !p.env_only));
    let done = move |outcome: Result<(), AppError>, message: &'static str| {
        if outcome.is_ok() {
            edits.set(BTreeMap::new());
            on_changed.run(());
        }
        result.set(Some(
            outcome
                .map(|()| message.to_owned())
                .map_err(|e| e.to_string()),
        ));
    };

    let save = move |_| {
        let Some(p) = provider.get_untracked() else {
            return;
        };
        result.set(None);
        let values = edits.with_untracked(|typed| form_values(&p, typed));
        run_action(busy, save_provider(p.kind, values), move |outcome| {
            done(outcome, "Saved. The gateway restarted with it.");
        });
    };
    let remove = Callback::new(move |()| {
        let Some(p) = provider.get_untracked() else {
            return;
        };
        result.set(None);
        run_action(busy, remove_provider(p.kind), move |outcome| {
            confirming.set(None);
            done(outcome, "Removed.");
        });
    });

    let header = move || {
        provider
            .get()
            .map(|provider| view! { <ProviderSummary provider /> })
    };
    // Re-rendered when the provider reloads (only after an action); typed
    // values live in `edits`, so they survive it.
    let fields = move || {
        provider
            .with(|p| p.as_ref().map(|p| p.fields.clone()))
            .unwrap_or_default()
            .into_iter()
            .map(|field| view! { <FieldInput field edits busy /> })
            .collect_view()
    };

    view! {
        <div class="tool-row provider-card">
            {header}
            <div class="tool-form">
                {fields}
                <div class="tool-actions">
                    <button class="primary" on:click=save disabled=move || busy.get()>
                        {move || match (busy.get(), configured()) {
                            (true, _) => "Saving…",
                            (false, true) => "Save",
                            (false, false) => "Add",
                        }}
                    </button>
                    {move || removable().then(|| view! {
                        <ConfirmButton
                            which=Confirming::Remove
                            confirming
                            label="Remove"
                            prompt="Forget these settings and stored credentials?"
                            busy_label="Removing…"
                            busy
                            disabled=Signal::derive(|| false)
                            on_confirm=remove
                        />
                    })}
                </div>
                {move || configured().then(|| view! {
                    <p class="hint">
                        "Saving restarts the gateway, cutting off model requests in flight."
                    </p>
                })}
            </div>
            <ActionResult result />
        </div>
    }
    .into_any()
}

/// A provider's name, status, host-side checks and problem.
#[component]
fn ProviderSummary(provider: ProviderView) -> impl IntoView {
    view! {
        <div class="tool-head">
            <span class="tool-name">{provider.name.clone()}</span>
            <ProviderBadge provider=provider.clone() />
        </div>
        <p class="tool-desc">{provider.description.clone()}</p>
        {provider.env_only.then(|| view! {
            <p class="hint">
                "Configured through GAVMO's environment variables; unset them to \
                 manage it here."
            </p>
        })}
        {provider.checks.into_iter().map(|check| view! { <CheckLine check /> }).collect_view()}
        {provider.error.map(|e| view! { <p class="error">{e}</p> })}
    }
}

#[component]
fn ProviderBadge(provider: ProviderView) -> impl IntoView {
    let (class, label) = match (provider.configured, provider.error.is_some()) {
        (false, _) => ("tool-status", "not configured"),
        (true, true) => ("tool-status tool-status--warn", "needs attention"),
        (true, false) => ("tool-status tool-status--ok", "configured"),
    };
    view! { <span class=class>{label}</span> }
}

/// A host-side prerequisite: passed, or failed with the command that fixes it.
#[component]
pub(super) fn CheckLine(check: ProviderCheck) -> impl IntoView {
    let class = if check.ok {
        "check check--ok"
    } else {
        "check check--err"
    };
    view! {
        <div class=class>
            <span class="check-mark">{if check.ok { "✓" } else { "✗" }}</span>
            <span class="check-message">{check.message}</span>
            {check.fix.map(|fix| view! {
                <code class="check-fix" title="Run this on the host">{fix}</code>
            })}
        </div>
    }
}

/// One form field. Secrets are never shown (the server doesn't send them);
/// a field set through the environment is read-only.
#[component]
pub(super) fn FieldInput(
    field: ProviderField,
    edits: RwSignal<BTreeMap<String, String>>,
    busy: RwSignal<bool>,
) -> impl IntoView {
    let key = StoredValue::new(field.key.clone());
    let initial = field.value.clone();
    let value = move || {
        edits
            .with(|e| key.with_value(|k| e.get(k).cloned()))
            .unwrap_or_else(|| initial.clone())
    };
    let read_only = field.overridden_by.is_some();
    let (input_type, autocomplete) = if field.secret {
        // `new-password`: a provider credential, not a login to this page,
        // so the browser shouldn't offer to save it.
        ("password", "new-password")
    } else {
        ("text", "off")
    };
    let placeholder = if field.secret && field.is_set {
        "Stored; enter a new one to replace it".to_owned()
    } else {
        field.placeholder.clone()
    };
    view! {
        <label class="field">
            <span class="field-label">
                {field.label.clone()}
                {field.required.then_some(" (required)")}
            </span>
            <input
                class="tool-input"
                r#type=input_type
                autocomplete=autocomplete
                placeholder=placeholder
                prop:value=value
                readonly=read_only
                disabled=move || busy.get()
                on:input=move |ev| {
                    let typed = event_target_value(&ev);
                    edits.update(|e| {
                        e.insert(key.get_value(), typed);
                    });
                }
            />
            {field.overridden_by.map(|var| view! {
                <span class="hint">"Set by " <code>{var}</code> " in GAVMO's environment."</span>
            })}
            {(!field.help.is_empty()).then(|| view! { <span class="hint">{field.help}</span> })}
        </label>
    }
    .into_any()
}

/// What Save sends for `provider`'s form: each field's typed value, or the
/// value shown. Secrets are only sent when typed (empty keeps the stored
/// one), and fields set through the environment not at all.
pub(super) fn form_values(
    provider: &ProviderView,
    typed: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    provider
        .fields
        .iter()
        .filter(|f| f.overridden_by.is_none())
        .filter_map(|f| {
            let value = typed
                .get(&f.key)
                .cloned()
                .or_else(|| (!f.secret).then(|| f.value.clone()))?;
            Some((f.key.clone(), value))
        })
        .collect()
}

/// Where the offered models come from, for the default models section.
pub(super) fn catalog_summary(status: &ModelCatalogStatus) -> String {
    let Some(age) = status.fetched_secs_ago else {
        return "The built-in model list: the list from models.dev (the one opencode uses) \
                hasn't been fetched yet."
            .to_owned();
    };
    let ago = match age {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", age / 60),
        3_600..86_400 => format!("{} h ago", age / 3_600),
        _ => match age / 86_400 {
            1 => "1 day ago".to_owned(),
            days => format!("{days} days ago"),
        },
    };
    let built_in = if status.built_in.is_empty() {
        String::new()
    } else {
        format!(
            " {} offers its built-in models (models.dev doesn't list it).",
            status.built_in.join(", ")
        )
    };
    format!(
        "Models from models.dev (the list opencode uses), updated {ago}; it is updated \
         daily.{built_in}"
    )
}

/// Where the offered models come from, with a button that updates them.
/// `status` is `None` until the settings load.
#[component]
pub(super) fn ModelCatalogPanel(
    status: Signal<Option<ModelCatalogStatus>>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<String, String>>);
    let refresh = move |_| {
        result.set(None);
        run_action(busy, refresh_model_catalog(), move |outcome| {
            on_changed.run(());
            result.set(Some(outcome.map_err(|e| e.to_string())));
        });
    };
    // An update that just failed says why itself (below the button).
    let just_failed = move || result.with(|r| matches!(r, Some(Err(_))));
    view! {
        <div class="tool-row">
            <div class="tool-form">
                {move || status.get().map(|status| view! {
                    <p class="hint">{catalog_summary(&status)}</p>
                    {status.error.filter(|_| !just_failed()).map(|e| view! {
                        <p class="error">"The last update failed: " {e}</p>
                    })}
                })}
                <div class="tool-actions">
                    <button on:click=refresh disabled=move || busy.get()>
                        {move || if busy.get() { "Updating…" } else { "Update model list" }}
                    </button>
                </div>
            </div>
            <ActionResult result />
        </div>
    }
    .into_any()
}

/// The default and small model pickers. "Automatic" follows the providers'
/// suggestion.
#[component]
pub(super) fn DefaultModels(
    settings: RwSignal<Option<ModelSettingsView>>,
    on_changed: Callback<()>,
) -> impl IntoView {
    // `None` = not touched since the last load: show the saved choice.
    let default_choice = RwSignal::new(None::<String>);
    let small_choice = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<String, String>>);

    let saved = move |small: bool| {
        settings.with(|s| {
            s.as_ref()
                .and_then(|s| {
                    if small {
                        s.saved_small_model.clone()
                    } else {
                        s.saved_default_model.clone()
                    }
                })
                .unwrap_or_default()
        })
    };
    let current = move |small: bool| {
        let choice = if small { small_choice } else { default_choice };
        choice.get().unwrap_or_else(|| saved(small))
    };
    let in_use = move || {
        settings.with(|s| {
            s.as_ref().map(|s| {
                let name = |id: &str| {
                    s.models
                        .iter()
                        .find(|m| m.id == id)
                        .map_or_else(|| id.to_owned(), |m| m.name.clone())
                };
                format!(
                    "Workspaces start with {} (and {} for titles and summaries).",
                    name(&s.default_model),
                    name(&s.small_model)
                )
            })
        })
    };
    let save = move |_| {
        let chosen = |small: bool| Some(current(small)).filter(|id| !id.is_empty());
        result.set(None);
        let call = set_default_models(chosen(false), chosen(true));
        run_action(busy, call, move |outcome| {
            if outcome.is_ok() {
                default_choice.set(None);
                small_choice.set(None);
                on_changed.run(());
            }
            result.set(Some(
                outcome
                    .map(|()| "Saved. Workspaces use it from their next start.".to_owned())
                    .map_err(|e| e.to_string()),
            ));
        });
    };

    let picker = move |small: bool, label: &'static str| {
        let choice = if small { small_choice } else { default_choice };
        let options = move || {
            settings
                .with(|s| s.as_ref().map(|s| s.models.clone()))
                .unwrap_or_default()
                .into_iter()
                .map(|m| {
                    let id = m.id.clone();
                    view! {
                        <option value=m.id selected=move || current(small) == id>{m.name}</option>
                    }
                })
                .collect_view()
        };
        view! {
            <label class="field">
                <span class="field-label">{label}</span>
                <select
                    class="tool-input"
                    disabled=move || busy.get()
                    on:change=move |ev| choice.set(Some(event_target_value(&ev)))
                >
                    <option value="" selected=move || current(small).is_empty()>"Automatic"</option>
                    {options}
                </select>
            </label>
        }
    };
    let catalog = Signal::derive(move || settings.with(|s| s.as_ref().map(|s| s.catalog.clone())));

    view! {
        <div class="tool-row">
            <div class="tool-form">
                {picker(false, "Default model")}
                {picker(true, "Small model (titles, summaries)")}
                <p class="hint">{in_use}</p>
                <div class="tool-actions">
                    <button on:click=save disabled=move || busy.get()>"Save"</button>
                </div>
            </div>
            <ActionResult result />
        </div>
        <ModelCatalogPanel status=catalog on_changed />
    }
    .into_any()
}
