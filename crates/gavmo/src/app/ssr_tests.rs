//! Server-side rendering of the components, natively: what each one renders
//! for a given state. Event handlers and effects only run in a browser (see
//! `browser_tests`), so these cover the views and their initial state.

use futures::StreamExt as _;
use leptos::prelude::*;

use super::card::WorkspaceCard;
use super::folder_browser::FolderBrowser;
use super::home::HomePage;
use super::modal::Modal;
use super::resources::ResourcesPanel;
use super::tools::{CredentialForm, ToolRow, WorkspaceTools};
use super::workspace_modal::{ConfirmButton, Confirming, StatsView, WorkspaceModal};
use crate::types::{ToolConfig, VmStats, WorkspaceInfo, WorkspaceStatus};

/// Renders `view` to HTML under a fresh reactive owner.
pub(super) fn render<V: IntoView + 'static>(view: impl FnOnce() -> V) -> String {
    let owner = Owner::new();
    owner.with(|| view().into_view().to_html())
}

/// Renders `view` waiting for its async data, as the daemon streams pages.
async fn render_resolved<V: IntoView + 'static>(view: impl FnOnce() -> V) -> String {
    // Resources spawn their fetches on the task executor.
    _ = any_spawner::Executor::init_tokio();
    let owner = Owner::new();
    let stream = owner.with(|| view().into_view().to_html_stream_in_order());
    stream.collect::<String>().await
}

#[test]
fn a_modal_renders_its_title_and_children() {
    let html = render(|| {
        view! {
            <Modal on_close=Callback::new(|()| ()) title="Add workspace">
                <p>"inside"</p>
            </Modal>
        }
    });
    assert!(html.contains("<h2>Add workspace</h2>"), "{html}");
    assert!(html.contains("<p>inside</p>"), "{html}");
    assert!(html.contains("modal--plain"), "{html}");
}

fn stats(mem: Option<(u64, u64)>) -> VmStats {
    VmStats {
        cpu_percent: 50.0,
        vcpus: 2,
        mem_used_bytes: mem.map(|m| m.0),
        mem_total_bytes: mem.map(|m| m.1),
        uptime_secs: 75,
        disk_bytes: 3 << 20,
        host_port: 4567,
    }
}

#[test]
fn stats_show_memory_once_the_guest_reports_it() {
    let html = render(|| view! { <StatsView stats=stats(Some((1 << 30, 4 << 30)))/> });
    assert!(html.contains("50% / 2 vCPU"), "{html}");
    assert!(html.contains("1.00 GiB / 4.00 GiB"), "{html}");
    assert!(
        html.contains("1m 15s") && html.contains("3.0 MiB") && html.contains("4567"),
        "{html}"
    );

    let html = render(|| view! { <StatsView stats=stats(None)/> });
    assert!(html.contains("—"), "{html}");
}

fn workspace(status: WorkspaceStatus) -> WorkspaceInfo {
    WorkspaceInfo {
        id: "/home/me/project".to_owned(),
        path: "/home/me/project".to_owned(),
        status,
    }
}

/// The detail modal for a workspace in `status`.
fn render_modal(status: WorkspaceStatus) -> String {
    render(move || {
        let workspaces = RwSignal::new(vec![workspace(status)]);
        view! {
            <WorkspaceModal
                id="/home/me/project".to_owned()
                workspaces
                on_changed=Callback::new(|()| ())
                on_close=Callback::new(|()| ())
            />
        }
    })
}

#[test]
fn the_modal_offers_start_for_a_stopped_workspace() {
    let html = render_modal(WorkspaceStatus::Stopped);
    assert!(html.contains("<h2>project</h2>"), "{html}");
    assert!(html.contains("badge idle"), "{html}");
    assert!(html.contains(">Start<"), "{html}");
    assert!(!html.contains("Open UI"), "{html}");
    assert!(
        html.contains(">Reset<") && html.contains(">Remove<"),
        "{html}"
    );
    // Panels that load after mounting start out empty.
    assert!(html.contains("Loading tools…"), "{html}");
}

#[test]
fn the_modal_explains_a_first_boot_and_waits_for_the_console() {
    let html = render_modal(WorkspaceStatus::Booting);
    assert!(html.contains("badge booting"), "{html}");
    assert!(html.contains(">Stop<"), "{html}");
    assert!(html.contains("First boot can take a few minutes"), "{html}");
    assert!(html.contains("Waiting for boot console output…"), "{html}");
    assert!(html.contains("Changes apply the next time"), "{html}");
}

#[test]
fn the_modal_links_to_a_running_workspace() {
    let html = render_modal(WorkspaceStatus::Running {
        url: "http://127.0.0.1:4096/s".to_owned(),
    });
    assert!(html.contains(r#"href="http://127.0.0.1:4096/s""#), "{html}");
    assert!(
        html.contains("Open UI") && html.contains(">Stop<"),
        "{html}"
    );
}

#[test]
fn the_modal_shows_failures_and_offers_the_build_log() {
    let html = render_modal(WorkspaceStatus::Failed {
        reason: "qemu exited".to_owned(),
    });
    assert!(html.contains("failed: qemu exited"), "{html}");

    let html = render_modal(WorkspaceStatus::BuildFailed {
        log: "rebuild-x.log".to_owned(),
    });
    assert!(html.contains("gavmo.nix build failed"), "{html}");
    assert!(html.contains("View build log"), "{html}");

    let html = render_modal(WorkspaceStatus::Stopping);
    assert!(html.contains("stopping…"), "{html}");
    assert!(!html.contains("base-image-changed"), "{html}");
}

/// A disk from an older base image explains itself and points at Reset.
#[test]
fn the_modal_explains_a_changed_base_image() {
    let html = render_modal(WorkspaceStatus::BaseImageChanged {
        reason: "the base image has changed (was aaaa, now bbbb)".to_owned(),
    });
    assert!(html.contains("base image updated - reset needed"), "{html}");
    assert!(html.contains("base-image-changed"), "{html}");
    assert!(html.contains("Reset the workspace"), "{html}");
    assert!(html.contains("(was aaaa, now bbbb)"), "{html}");
    assert!(
        html.contains(">Reset<") && html.contains(">Start<"),
        "{html}"
    );
}

/// A workspace that disappeared from the list renders as stopped (the
/// modal then closes itself, in the browser).
#[test]
fn the_modal_tolerates_a_workspace_missing_from_the_list() {
    let html = render(|| {
        view! {
            <WorkspaceModal
                id="/gone".to_owned()
                workspaces=RwSignal::new(Vec::new())
                on_changed=Callback::new(|()| ())
                on_close=Callback::new(|()| ())
            />
        }
    });
    assert!(html.contains("badge idle"), "{html}");
}

#[test]
fn a_confirm_button_asks_before_acting() {
    let button = |confirming: Option<Confirming>, busy: bool| {
        render(move || {
            view! {
                <ConfirmButton
                    which=Confirming::Reset
                    confirming=RwSignal::new(confirming)
                    label="Reset"
                    prompt="Delete all VM state?"
                    busy_label="Resetting…"
                    busy=RwSignal::new(busy)
                    disabled=Signal::derive(|| false)
                    on_confirm=Callback::new(|()| ())
                />
            }
        })
    };
    let idle = button(None, false);
    assert!(
        idle.contains(">Reset<") && !idle.contains("Delete all"),
        "{idle}"
    );
    // Another button's confirmation doesn't affect this one.
    assert!(button(Some(Confirming::Remove), false).contains(">Reset<"));
    let asking = button(Some(Confirming::Reset), false);
    assert!(
        asking.contains("Delete all VM state?") && asking.contains(">Confirm<"),
        "{asking}"
    );
    assert!(button(Some(Confirming::Reset), true).contains("Resetting…"));
}

#[test]
fn a_card_shows_the_status_name_and_path() {
    let card = |id: &'static str| {
        render(move || {
            let workspaces = RwSignal::new(vec![workspace(WorkspaceStatus::Running {
                url: "http://x".to_owned(),
            })]);
            view! {
                <WorkspaceCard
                    id=id.to_owned()
                    workspaces
                    selected=RwSignal::new(None)
                    open_card_el=StoredValue::new(None)
                />
            }
        })
    };
    let html = card("/home/me/project");
    assert!(html.contains("badge running"), "{html}");
    assert!(
        html.contains(">project<") && html.contains(">/home/me/project<"),
        "{html}"
    );
    // The instant between removal and unmounting: shown as idle.
    let html = card("/elsewhere");
    assert!(
        html.contains("badge idle") && html.contains(">elsewhere<"),
        "{html}"
    );
}

fn tool(edit: impl FnOnce(&mut ToolConfig)) -> ToolConfig {
    let mut tool = ToolConfig {
        id: "jira".to_owned(),
        description: "Jira Cloud".to_owned(),
        base_url: "https://jira.example.com".to_owned(),
        auth: "bearer".to_owned(),
        secret_name: "jira_token".to_owned(),
        instructions: None,
        configured: false,
        url_changed: false,
        bound_url: None,
        delivery: "a Bearer token in the Authorization header".to_owned(),
        approved_delivery: None,
    };
    edit(&mut tool);
    tool
}

/// One tool's row in the Tools panel: details, status, warnings, form.
fn row(t: Option<ToolConfig>) -> String {
    render(move || {
        view! {
            <ToolRow
                tool=Memo::new(move |_| t.clone())
                workspace_id=StoredValue::new("/w".to_owned())
                on_changed=Callback::new(|()| ())
            />
        }
    })
}

#[test]
fn a_tool_badge_says_why_a_credential_is_or_isn_t_used() {
    let badge = |t: ToolConfig| row(Some(t));
    assert!(badge(tool(|_| ())).contains("not configured"));
    assert!(badge(tool(|t| t.configured = true)).contains(">configured<"));
    assert!(badge(tool(|t| t.url_changed = true)).contains("destination changed"));
    assert!(
        badge(tool(|t| t.approved_delivery = Some("x".to_owned()))).contains("auth method changed")
    );
}

#[test]
fn tool_warnings_name_what_the_credential_was_approved_for() {
    let warnings = |t: ToolConfig| row(Some(t));
    assert!(!warnings(tool(|_| ())).contains("tool-warning"));

    let moved = warnings(tool(|t| {
        t.url_changed = true;
        t.bound_url = Some("https://old.example.com".to_owned());
    }));
    assert!(
        moved.contains("https://old.example.com") && moved.contains("https://jira.example.com"),
        "{moved}"
    );

    let unbound = warnings(tool(|t| t.url_changed = true));
    assert!(unbound.contains("predates"), "{unbound}");

    let redelivered = warnings(tool(|t| {
        t.approved_delivery = Some("HTTP Basic auth".to_owned());
    }));
    assert!(
        redelivered.contains("approved to be sent as") && redelivered.contains("HTTP Basic auth"),
        "{redelivered}"
    );
}

/// A row's details and form, for a configured Basic-auth tool with setup
/// instructions, and for an unconfigured Bearer one.
#[test]
fn a_tool_row_shows_details_and_the_matching_form() {
    let basic = row(Some(tool(|t| {
        t.auth = "basic".to_owned();
        t.configured = true;
        t.instructions = Some("Create a token at example.com".to_owned());
    })));
    assert!(basic.contains("Username / email"), "{basic}");
    assert!(basic.contains("Enter a new token to replace"), "{basic}");
    assert!(
        basic.contains(">Update<") && basic.contains(">Test<") && basic.contains(">Clear<"),
        "{basic}"
    );
    assert!(basic.contains("Create a token at example.com"), "{basic}");
    assert!(basic.contains("Sends your credential to"), "{basic}");

    let bearer = row(Some(tool(|_| ())));
    assert!(!bearer.contains("Username / email"), "{bearer}");
    assert!(
        bearer.contains("Token / API key") && bearer.contains(">Save<"),
        "{bearer}"
    );
    assert!(!bearer.contains(">Test<"), "{bearer}");

    // A row about to unmount renders just an empty form.
    let gone = row(None);
    assert!(!gone.contains("tool-head"), "{gone}");
}

#[test]
fn a_credential_form_alone_matches_its_tool() {
    let html = render(|| {
        let t = tool(|t| t.configured = true);
        view! {
            <CredentialForm
                tool=Memo::new(move |_| Some(t.clone()))
                workspace_id=StoredValue::new("/w".to_owned())
                on_changed=Callback::new(|()| ())
            />
        }
    });
    assert!(html.contains(">Update<"), "{html}");
}

#[test]
fn the_resources_and_tools_panels_start_out_loading() {
    let html = render(|| {
        view! {
            <ResourcesPanel id="/w".to_owned() status=Memo::new(|_| WorkspaceStatus::Stopped)/>
            <WorkspaceTools id="/w".to_owned()/>
        }
    });
    assert!(
        html.contains("Resources") && !html.contains("Stop it to change"),
        "{html}"
    );
    assert!(html.contains("Loading tools…"), "{html}");
}

/// The page as first served: the header, and "Loading…" until the
/// browser's first poll fills the grid.
#[tokio::test]
async fn the_home_page_starts_out_loading() {
    let html = render_resolved(|| view! { <HomePage/> }).await;
    assert!(
        html.contains("Add workspace") && html.contains("Log out everywhere"),
        "{html}"
    );
    assert!(html.contains(r#"href="/settings""#), "{html}");
    assert!(
        !html.contains("reach any model"),
        "the banner waits for the browser"
    );
    assert!(html.contains("Loading…"), "{html}");
}

/// The folder picker lists the home directory first. (`browse_dir` needs
/// no daemon state, so it runs for real here.)
#[tokio::test]
async fn the_folder_picker_lists_the_home_directory() {
    let html = render_resolved(|| view! { <FolderBrowser on_added=Callback::new(|()| ())/> }).await;
    let home = std::fs::canonicalize(dirs::home_dir().unwrap()).unwrap();
    assert!(html.contains(home.to_str().unwrap()), "{html}");
    assert!(html.contains("Use this directory"), "{html}");
}

mod settings {
    use std::collections::BTreeMap;

    use leptos::prelude::*;

    use super::{render, render_resolved};
    use crate::app::settings::{
        ActionResult, CheckLine, DefaultModels, FieldInput, GatewayPanel, ProviderCard,
        SettingsPage, catalog_summary, form_values, gateway_problem,
    };
    use crate::types::{
        GatewayStatus, ModelCatalogStatus, ModelChoice, ModelSettingsView, ProviderCheck,
        ProviderField, ProviderView,
    };

    fn field(key: &str, secret: bool) -> ProviderField {
        ProviderField {
            key: key.to_owned(),
            label: format!("{key} label"),
            help: format!("{key} help"),
            placeholder: format!("{key} placeholder"),
            secret,
            required: true,
            value: if secret {
                String::new()
            } else {
                format!("{key} value")
            },
            is_set: false,
            overridden_by: None,
        }
    }

    fn provider(configured: bool) -> ProviderView {
        let mut project = field("project", false);
        project.overridden_by = Some("GAVMO_GCP_PROJECT".to_owned());
        let mut key = field("api_key", true);
        key.is_set = true;
        ProviderView {
            kind: "vertex".to_owned(),
            name: "Google Vertex AI".to_owned(),
            description: "Claude through Google Cloud.".to_owned(),
            configured,
            env_only: false,
            fields: vec![project, field("region", false), key],
            checks: vec![ProviderCheck {
                ok: false,
                message: "No credentials".to_owned(),
                fix: Some("gcloud auth application-default login".to_owned()),
            }],
            error: configured.then(|| "Region is invalid".to_owned()),
        }
    }

    fn settings_view() -> ModelSettingsView {
        let model = |id: &str, name: &str| ModelChoice {
            id: id.to_owned(),
            name: name.to_owned(),
        };
        ModelSettingsView {
            gateway: GatewayStatus::Failing {
                reason: "exited".to_owned(),
            },
            providers: vec![provider(true)],
            models: vec![model("big", "Big model"), model("small", "Small model")],
            saved_default_model: Some("big".to_owned()),
            saved_small_model: None,
            default_model: "big".to_owned(),
            small_model: "small".to_owned(),
            catalog: ModelCatalogStatus {
                fetched_secs_ago: Some(7_200),
                error: Some("models.dev is down".to_owned()),
                built_in: Vec::new(),
            },
        }
    }

    /// The page as first served: the header and "Loading…" until the
    /// browser fetches the settings.
    #[tokio::test]
    async fn the_settings_page_starts_out_loading() {
        let html = render_resolved(|| view! { <SettingsPage/> }).await;
        assert!(
            html.contains("Settings") && html.contains("Back to workspaces"),
            "{html}"
        );
        assert!(html.contains("Loading…"), "{html}");
    }

    /// A configured provider: its badge, checks with the fixing command,
    /// error, fields (the overridden one read-only, the secret masked) and
    /// Save / Remove.
    #[test]
    fn a_provider_card_renders_its_schema() {
        let html = render(|| {
            let provider = Memo::new(|_| Some(provider(true)));
            view! { <ProviderCard provider on_changed=Callback::new(|()| ())/> }
        });
        for expected in [
            "Google Vertex AI",
            "Claude through Google Cloud.",
            "needs attention",
            "No credentials",
            "gcloud auth application-default login",
            "Region is invalid",
            "region label",
            "region help",
            "Set by <code>GAVMO_GCP_PROJECT</code>",
            "readonly",
            r#"type="password""#,
            "Stored; enter a new one to replace it",
            ">Save<",
            ">Remove<",
            "Saving restarts the gateway",
        ] {
            assert!(html.contains(expected), "{expected:?} in {html}");
        }
    }

    /// An unconfigured provider is added, not saved, and can't be removed.
    #[test]
    fn an_unconfigured_provider_card_offers_to_add_it() {
        let html = render(|| {
            let provider = Memo::new(|_| Some(provider(false)));
            view! { <ProviderCard provider on_changed=Callback::new(|()| ())/> }
        });
        assert!(
            html.contains("not configured") && html.contains(">Add<"),
            "{html}"
        );
        assert!(
            !html.contains(">Remove<") && !html.contains("Saving restarts"),
            "{html}"
        );

        let mut env_only = provider(true);
        env_only.env_only = true;
        let html = render(move || {
            let provider = Memo::new(move |_| Some(env_only.clone()));
            view! { <ProviderCard provider on_changed=Callback::new(|()| ())/> }
        });
        assert!(
            html.contains("environment variables") && !html.contains(">Remove<"),
            "{html}"
        );
    }

    /// A passed check has no fix; an unset secret shows its placeholder.
    #[test]
    fn checks_and_fields_render_their_state() {
        let html = render(|| {
            let check = ProviderCheck {
                ok: true,
                message: "Found".to_owned(),
                fix: None,
            };
            view! { <CheckLine check/> }
        });
        assert!(
            html.contains("check--ok") && !html.contains("check-fix"),
            "{html}"
        );

        let html = render(|| {
            let edits = RwSignal::new(BTreeMap::new());
            let busy = RwSignal::new(false);
            view! { <FieldInput field=field("api_key", true) edits busy/> }
        });
        assert!(
            html.contains("api_key placeholder") && html.contains("(required)"),
            "{html}"
        );
    }

    /// Save sends typed values over shown ones, secrets only when typed,
    /// and nothing for fields set through the environment.
    #[test]
    fn the_form_sends_typed_or_shown_values() {
        let typed = BTreeMap::from([("region".to_owned(), "us-east5".to_owned())]);
        let sent = form_values(&provider(true), &typed);
        assert_eq!(sent, typed, "project is overridden, the key not typed");

        let typed = BTreeMap::from([("api_key".to_owned(), "sk".to_owned())]);
        let sent = form_values(&provider(true), &typed);
        assert_eq!(sent["region"], "region value");
        assert_eq!(sent["api_key"], "sk");
    }

    #[test]
    fn gateway_problems_are_explained_except_while_it_works() {
        assert_eq!(gateway_problem(&GatewayStatus::Ready), None);
        assert_eq!(gateway_problem(&GatewayStatus::Starting), None);
        let not_configured = GatewayStatus::NotConfigured {
            reason: "no model provider".to_owned(),
        };
        assert_eq!(
            gateway_problem(&not_configured).as_deref(),
            Some("No model provider")
        );
        let empty = GatewayStatus::NotConfigured {
            reason: String::new(),
        };
        assert_eq!(gateway_problem(&empty).as_deref(), Some(""));
        let failing = GatewayStatus::Failing {
            reason: "exited".to_owned(),
        };
        assert!(gateway_problem(&failing).unwrap().contains("exited"));
    }

    /// Each gateway state has its own badge; a working one explains nothing.
    #[test]
    fn every_gateway_state_has_a_badge() {
        for (status, class, label) in [
            (GatewayStatus::Ready, "tool-status--ok", "ready"),
            (GatewayStatus::Starting, "tool-status--warn", "starting"),
            (
                GatewayStatus::NotConfigured {
                    reason: "no model provider".to_owned(),
                },
                "tool-status\"",
                "not running",
            ),
        ] {
            let working = gateway_problem(&status).is_none();
            let html = render(move || {
                let mut view = settings_view();
                view.gateway = status;
                let settings = RwSignal::new(Some(view));
                view! { <GatewayPanel settings on_changed=Callback::new(|()| ())/> }
            });
            assert!(
                html.contains(class) && html.contains(&format!(">{label}<")),
                "{label}: {html}"
            );
            assert_eq!(!working, html.contains("No model provider"), "{html}");
        }
    }

    /// A configured provider without a problem says so.
    #[test]
    fn a_working_provider_is_shown_as_configured() {
        let mut working = provider(true);
        working.error = None;
        let html = render(move || {
            let provider = Memo::new(move |_| Some(working.clone()));
            view! { <ProviderCard provider on_changed=Callback::new(|()| ())/> }
        });
        assert!(
            html.contains("tool-status--ok") && html.contains(">configured<"),
            "{html}"
        );
    }

    /// An action's outcome: nothing before it, then success or the error.
    #[test]
    fn action_results_show_success_and_errors_apart() {
        for (outcome, class) in [
            (None, None),
            (Some(Ok("Saved.".to_owned())), Some("tool-result--ok")),
            (Some(Err("refused".to_owned())), Some("tool-result--err")),
        ] {
            let text = outcome
                .clone()
                .map(|r: Result<String, String>| r.unwrap_or_else(|e| e));
            let html = render(move || {
                let result = RwSignal::new(outcome);
                view! { <ActionResult result/> }
            });
            match (class, text) {
                (Some(class), Some(text)) => {
                    assert!(html.contains(class) && html.contains(&text), "{html}");
                }
                _ => assert!(!html.contains("tool-result"), "{html}"),
            }
        }
    }

    /// The gateway's state, and the model pickers with the saved choice
    /// selected and what workspaces start with.
    #[test]
    fn the_gateway_and_default_models_show_the_settings() {
        let html = render(|| {
            let settings = RwSignal::new(Some(settings_view()));
            view! { <GatewayPanel settings on_changed=Callback::new(|()| ())/> }
        });
        assert!(
            html.contains("failing") && html.contains("It keeps failing: exited"),
            "{html}"
        );

        let html = render(|| {
            let settings = RwSignal::new(Some(settings_view()));
            view! { <DefaultModels settings on_changed=Callback::new(|()| ())/> }
        });
        assert!(
            html.contains("Workspaces start with Big model (and Small model"),
            "{html}"
        );
        assert!(html.contains(r#"<option value="big" selected"#), "{html}");
        assert!(
            html.contains(r#"<option value="" selected"#),
            "small is automatic: {html}"
        );
        for expected in [
            "Models from models.dev",
            "updated 2 h ago",
            "The last update failed: <!>models.dev is down",
            ">Update model list<",
        ] {
            assert!(html.contains(expected), "{expected:?} in {html}");
        }
    }

    /// Where the models come from, by how fresh the catalog is.
    #[test]
    fn the_catalog_summary_says_where_the_models_come_from() {
        let summary = |fetched_secs_ago: Option<u64>, built_in: &[&str]| {
            catalog_summary(&ModelCatalogStatus {
                fetched_secs_ago,
                error: None,
                built_in: built_in.iter().map(|s| (*s).to_owned()).collect(),
            })
        };
        assert!(summary(None, &["Google Vertex AI"]).starts_with("The built-in model list"));
        for (age, ago) in [
            (5, "just now"),
            (600, "10 min ago"),
            (3_600 * 5, "5 h ago"),
            (86_400, "1 day ago"),
            (86_400 * 3, "3 days ago"),
        ] {
            let text = summary(Some(age), &[]);
            assert!(text.contains(&format!("updated {ago};")), "{text}");
            assert!(!text.contains("built-in"), "{text}");
        }
        let partial = summary(Some(5), &["Keyed"]);
        assert!(
            partial.contains("Keyed offers its built-in models"),
            "{partial}"
        );
    }
}
