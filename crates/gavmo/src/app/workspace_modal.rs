//! The workspace detail modal.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use super::format::{format_bytes, format_uptime, status_view};
use super::modal::Modal;
use super::tools::WorkspaceTools;
use super::transitions::{scroll_to_bottom, vt_name};
use crate::api::{
    open_workspace, poll_vm_stats, read_build_log, read_console_log, remove_workspace,
    reset_workspace, stop_workspace,
};
use crate::types::{VmStats, WorkspaceInfo, WorkspaceStatus};
use leptos::prelude::*;
/// The detail/actions view for one workspace, shown in the modal. Mounted on
/// each open, so its local state starts fresh every time.
#[component]
#[expect(
    clippy::too_many_lines,
    reason = "split into smaller components in the UI-fixes commit"
)]
pub(crate) fn WorkspaceModal(
    id: String,
    workspaces: RwSignal<Vec<WorkspaceInfo>>,
    on_changed: Callback<()>,
    on_close: Callback<()>,
) -> impl IntoView {
    // A `Copy` handle so nested reactive `move` closures can all capture the
    // id without becoming `FnOnce`.
    let id = StoredValue::new(id);

    // This workspace's live slice of the list. The fallback covers the gap
    // between removal and the modal closing.
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

    let busy = RwSignal::new(false);
    // Shared across open/stop/reset/remove: whichever of them last failed.
    let action_error = RwSignal::new(None::<String>);
    // Opening one closes the other, so only one confirm prompt shows at a time.
    let confirming_reset = RwSignal::new(false);
    let confirming_remove = RwSignal::new(false);
    // Build-log viewer; `content` is `None` while loading.
    let build_log_open = RwSignal::new(false);
    let build_log_content = RwSignal::new(None::<Result<String, String>>);

    // Live boot console, polled every second while `Booting`.
    let console_text = RwSignal::new(String::new());
    let console_ref: NodeRef<leptos::html::Pre> = NodeRef::new();
    // Kept so polling stops once the workspace leaves `Booting` or the modal
    // unmounts.
    let console_interval = StoredValue::new(None::<IntervalHandle>);

    let fetch_console = move || {
        let id = id.get_value();
        leptos::task::spawn_local(async move {
            if let Ok(text) = read_console_log(id).await {
                console_text.set(text);
            }
        });
    };

    // On leaving `Booting`, fetch once more so the panel shows the real tail
    // of the log.
    Effect::new(move |_| {
        let booting = matches!(card_info.get().status, WorkspaceStatus::Booting);
        let already_polling = console_interval.get_value().is_some();

        if booting && !already_polling {
            fetch_console();
            let handle =
                set_interval_with_handle(fetch_console, std::time::Duration::from_secs(1)).ok();
            console_interval.set_value(handle);
        } else if !booting && already_polling {
            console_interval.update_value(|h| {
                if let Some(h) = h.take() {
                    h.clear();
                }
            });
            fetch_console();
        }
    });
    on_cleanup(move || {
        if let Some(h) = console_interval.get_value() {
            h.clear();
        }
    });

    // Keep the console scrolled to the newest output.
    Effect::new(move |_| {
        console_text.track();
        if let Some(el) = console_ref.get() {
            scroll_to_bottom(&el);
        }
    });

    // VM stats, polled while `Running`. Every 2s rather than 1s because each
    // request waits between two CPU samples, so faster polls would overlap.
    let vm_stats = RwSignal::new(None::<VmStats>);
    let stats_interval = StoredValue::new(None::<IntervalHandle>);

    let fetch_stats = move || {
        let id = id.get_value();
        leptos::task::spawn_local(async move {
            if let Ok(stats) = poll_vm_stats(id).await {
                vm_stats.set(stats);
            }
        });
    };

    Effect::new(move |_| {
        let running = matches!(card_info.get().status, WorkspaceStatus::Running { .. });
        let already_polling = stats_interval.get_value().is_some();

        if running && !already_polling {
            fetch_stats();
            let handle =
                set_interval_with_handle(fetch_stats, std::time::Duration::from_secs(2)).ok();
            stats_interval.set_value(handle);
        } else if !running && already_polling {
            stats_interval.update_value(|h| {
                if let Some(h) = h.take() {
                    h.clear();
                }
            });
            vm_stats.set(None);
        }
    });
    on_cleanup(move || {
        if let Some(h) = stats_interval.get_value() {
            h.clear();
        }
    });

    // All captures are `Copy`, so these handlers are `Copy` and can be used
    // from nested reactive branches without cloning.
    let open = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            // Returns once the boot starts; the live poll shows its progress.
            if let Err(e) = open_workspace(id).await {
                leptos::logging::error!("open failed: {e}");
                action_error.set(Some(e.to_string()));
            }
            busy.set(false);
            on_changed.run(());
        });
    };

    let stop = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            // May take a moment if it races a boot that's about to finish.
            if let Err(e) = stop_workspace(id).await {
                leptos::logging::error!("stop failed: {e}");
                action_error.set(Some(e.to_string()));
            }
            busy.set(false);
            on_changed.run(());
        });
    };

    let reset = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            if let Err(e) = reset_workspace(id).await {
                leptos::logging::error!("reset failed: {e}");
                action_error.set(Some(e.to_string()));
            }
            busy.set(false);
            confirming_reset.set(false);
            on_changed.run(());
        });
    };

    let remove = move |_| {
        let id = id.get_value();
        busy.set(true);
        action_error.set(None);
        leptos::task::spawn_local(async move {
            if let Err(e) = remove_workspace(id).await {
                leptos::logging::error!("remove failed: {e}");
                action_error.set(Some(e.to_string()));
            }
            busy.set(false);
            confirming_remove.set(false);
            on_changed.run(());
        });
    };

    // Untracked: the modal is mounted per open and the path can't change
    // while it's open.
    let title = {
        let path = card_info.get_untracked().path;
        path.rsplit('/').next().unwrap_or(&path).to_string()
    };

    // The name the clicked card just released; claiming it morphs the card
    // into this modal.
    let vt_name = Some(vt_name(&id.get_value()));

    // Start/Stop is one toggle because each status maps to exactly one valid
    // action (Stop also cancels a boot). Reset is disabled while `Booting`:
    // a destructive action is reserved for a settled state.
    view! {
        <Modal on_close title vt_name>
            <div class="ws-modal-head">
                {move || {
                    let (badge_class, badge_text, _) = status_view(&card_info.get().status);
                    view! { <span class=format!("badge {badge_class}")>{badge_text}</span> }
                }}
                <span class="path">{move || card_info.get().path}</span>
            </div>
                <div class="actions">
                    {move || status_view(&card_info.get().status).2.map(|u| view! {
                        <a href=u target="_blank" rel="noreferrer">
                            <button>"Open UI"</button>
                        </a>
                    })}
                    {move || matches!(card_info.get().status, WorkspaceStatus::BuildFailed { .. }).then(|| view! {
                        <button on:click=move |_| {
                            if build_log_open.get_untracked() {
                                build_log_open.set(false);
                                return;
                            }
                            let WorkspaceStatus::BuildFailed { log } = card_info.get_untracked().status else {
                                return;
                            };
                            build_log_open.set(true);
                            build_log_content.set(None);
                            let id = id.get_value();
                            leptos::task::spawn_local(async move {
                                let result = read_build_log(id, log).await.map_err(|e| e.to_string());
                                build_log_content.set(Some(result));
                            });
                        }>
                            {move || if build_log_open.get() { "Hide build log" } else { "View build log" }}
                        </button>
                    })}
                    <button
                        on:click=move |ev| {
                            if matches!(
                                card_info.get_untracked().status,
                                WorkspaceStatus::Booting | WorkspaceStatus::Running { .. }
                            ) {
                                stop(ev);
                            } else {
                                open(ev);
                            }
                        }
                        disabled=move || busy.get() || matches!(card_info.get().status, WorkspaceStatus::Stopping)
                    >
                        {move || if matches!(
                            card_info.get().status,
                            WorkspaceStatus::Booting | WorkspaceStatus::Running { .. }
                        ) {
                            "Stop"
                        } else {
                            "Start"
                        }}
                    </button>
                    {move || if confirming_reset.get() {
                        view! {
                            <span class="reset-confirm">
                                <span class="reset-confirm-label">"Delete all VM state?"</span>
                                <button class="danger" on:click=reset disabled=move || busy.get()>
                                    {move || if busy.get() { "Resetting…" } else { "Confirm" }}
                                </button>
                                <button
                                    on:click=move |_| confirming_reset.set(false)
                                    disabled=move || busy.get()
                                >
                                    "Cancel"
                                </button>
                            </span>
                        }.into_any()
                    } else {
                        view! {
                            <button
                                on:click=move |_| {
                                    confirming_remove.set(false);
                                    confirming_reset.set(true);
                                }
                                disabled=move || {
                                    busy.get() || matches!(card_info.get().status, WorkspaceStatus::Booting)
                                }
                            >
                                "Reset"
                            </button>
                        }.into_any()
                    }}
                    {move || if confirming_remove.get() {
                        view! {
                            <span class="reset-confirm">
                                <span class="reset-confirm-label">"Remove from list?"</span>
                                <button class="danger" on:click=remove disabled=move || busy.get()>
                                    {move || if busy.get() { "Removing…" } else { "Confirm" }}
                                </button>
                                <button
                                    on:click=move |_| confirming_remove.set(false)
                                    disabled=move || busy.get()
                                >
                                    "Cancel"
                                </button>
                            </span>
                        }.into_any()
                    } else {
                        view! {
                            <button
                                on:click=move |_| {
                                    confirming_reset.set(false);
                                    confirming_remove.set(true);
                                }
                                disabled=move || busy.get()
                            >
                                "Remove"
                            </button>
                        }.into_any()
                    }}
                </div>
                {move || matches!(card_info.get().status, WorkspaceStatus::Booting).then(|| view! {
                    <p class="hint">
                        "First boot can take a few minutes - longer if "
                        <code>"gavmo.nix"</code>
                        " needs to fetch or build new packages."
                    </p>
                })}
                // Stays visible after boot so the log doesn't vanish mid-read.
                {move || {
                    let booting = matches!(card_info.get().status, WorkspaceStatus::Booting);
                    let has_content = !console_text.get().is_empty();
                    (booting || has_content).then(|| view! {
                        <div class="console-panel">
                            <pre class="console-log" node_ref=console_ref>
                                {move || {
                                    let text = console_text.get();
                                    if text.is_empty() {
                                        "Waiting for boot console output…".to_string()
                                    } else {
                                        text
                                    }
                                }}
                            </pre>
                        </div>
                    })
                }}
                {move || vm_stats.get().map(|s| view! {
                    <div class="stats-panel">
                        <div class="stat-row">
                            <span class="stat-label">"CPU"</span>
                            <div class="stat-bar">
                                <div
                                    class="stat-bar-fill"
                                    style:width=format!(
                                        "{}%",
                                        (s.cpu_percent / (s.vcpus.max(1) as f32 * 100.0) * 100.0)
                                            .clamp(0.0, 100.0)
                                    )
                                ></div>
                            </div>
                            <span class="stat-value">
                                {format!("{:.0}% / {} vCPU", s.cpu_percent, s.vcpus)}
                            </span>
                        </div>
                        <div class="stat-row">
                            <span class="stat-label">"Memory"</span>
                            // Placeholder until the balloon driver reports.
                            {match (s.mem_used_bytes, s.mem_total_bytes) {
                                (Some(used), Some(total)) => view! {
                                    <div class="stat-bar">
                                        <div
                                            class="stat-bar-fill"
                                            style:width=format!(
                                                "{}%",
                                                (used as f32 / total.max(1) as f32 * 100.0)
                                                    .clamp(0.0, 100.0)
                                            )
                                        ></div>
                                    </div>
                                    <span class="stat-value">
                                        {format!("{} / {}", format_bytes(used), format_bytes(total))}
                                    </span>
                                }.into_any(),
                                _ => view! {
                                    <div class="stat-bar"></div>
                                    <span class="stat-value">"—"</span>
                                }.into_any(),
                            }}
                        </div>
                        <div class="stat-row stat-row--plain">
                            <span class="stat-label">"Uptime"</span>
                            <span class="stat-value">{format_uptime(s.uptime_secs)}</span>
                        </div>
                        <div class="stat-row stat-row--plain">
                            <span class="stat-label">"Disk"</span>
                            <span class="stat-value">{format_bytes(s.disk_bytes)}</span>
                        </div>
                        <div class="stat-row stat-row--plain">
                            <span class="stat-label">"Port"</span>
                            <span class="stat-value">{s.host_port.to_string()}</span>
                        </div>
                    </div>
                })}
                {move || action_error.get().map(|e| view! {
                    <p class="error">{e}</p>
                })}
                {move || build_log_open.get().then(|| view! {
                    <div class="build-log-panel">
                        {move || match build_log_content.get() {
                            None => view! { <p class="empty">"Loading…"</p> }.into_any(),
                            Some(Ok(text)) => view! { <pre class="build-log">{text}</pre> }.into_any(),
                            Some(Err(e)) => view! {
                                <p class="error">"Failed to load build log: " {e}</p>
                            }.into_any(),
                        }}
                    </div>
                })}
                <WorkspaceTools id=id.get_value() />
        </Modal>
    }
}
