//! The workspace detail modal: actions, boot console, stats, build log and
//! tools for one workspace.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use std::time::Duration;

use leptos::prelude::*;

use super::format::{display_name, format_bytes, format_uptime, percent_of, status_view};
use super::hooks::{run_action, use_polling};
use super::modal::Modal;
use super::tools::WorkspaceTools;
use super::transitions::{scroll_to_bottom, vt_name};
use crate::api::{
    open_workspace, poll_vm_stats, read_build_log, read_console_log, remove_workspace,
    reset_workspace, stop_workspace,
};
use crate::types::{AppError, ConsoleChunk, VmStats, WorkspaceInfo, WorkspaceStatus};

/// Most console text kept in the browser; older output is dropped.
const MAX_CONSOLE_CHARS: usize = 1024 * 1024;

/// The detail/actions view for one workspace, shown in the modal. Mounted on
/// each open, so its local state starts fresh every time.
#[component]
pub(crate) fn WorkspaceModal(
    id: String,
    workspaces: RwSignal<Vec<WorkspaceInfo>>,
    on_changed: Callback<()>,
    on_close: Callback<()>,
) -> impl IntoView {
    // A `Copy` handle so nested reactive `move` closures can all capture the
    // id without becoming `FnOnce`.
    let id = StoredValue::new(id);

    // This workspace's live status; `None` once it's gone from the list.
    let info = Memo::new(move |_| {
        id.with_value(|id| workspaces.with(|ws| ws.iter().find(|w| &w.id == id).cloned()))
    });
    let status = Memo::new(move |_| {
        info.with(|i| {
            i.as_ref()
                .map_or(WorkspaceStatus::Stopped, |i| i.status.clone())
        })
    });

    // Close once the workspace is gone (removed here or in another tab).
    let closed = StoredValue::new(false);
    let close = Callback::new(move |()| {
        if !closed.get_value() {
            closed.set_value(true);
            on_close.run(());
        }
    });
    Effect::new(move |_| {
        if info.with(Option::is_none) {
            close.run(());
        }
    });

    let path = id.get_value();
    let title = display_name(&path).to_owned();
    // The name the clicked card just released; claiming it morphs the card
    // into this modal.
    let vt_name = Some(vt_name(&path));

    view! {
        <Modal on_close=close title vt_name>
            <div class="ws-modal-head">
                {move || {
                    let (badge_class, badge_text, _) = status_view(&status.get());
                    view! { <span class=format!("badge {badge_class}")>{badge_text}</span> }
                }}
                <span class="path">{path}</span>
            </div>
            <ActionsBar id status on_changed on_removed=close />
            {move || matches!(status.get(), WorkspaceStatus::Booting).then(|| view! {
                <p class="hint">
                    "First boot can take a few minutes - longer if "
                    <code>"gavmo.nix"</code>
                    " needs to fetch or build new packages."
                </p>
            })}
            <ConsolePanel id status />
            <StatsPanel id status />
            <BuildLogPanel id status />
            <WorkspaceTools id=id.get_value() />
        </Modal>
    }
}

/// Which destructive action is waiting for confirmation (at most one).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirming {
    Reset,
    Remove,
}

/// Open UI / build log toggle / Start-Stop / Reset / Remove.
///
/// Start/Stop is one toggle because each status maps to exactly one valid
/// action (Stop also cancels a boot). Reset is disabled while `Booting`: a
/// destructive action is reserved for a settled state.
#[component]
fn ActionsBar(
    id: StoredValue<String>,
    status: Memo<WorkspaceStatus>,
    on_changed: Callback<()>,
    on_removed: Callback<()>,
) -> impl IntoView {
    let busy = RwSignal::new(false);
    // Whichever action last failed.
    let error = RwSignal::new(None::<String>);
    let confirming = RwSignal::new(None::<Confirming>);

    let finish = move |outcome: Result<(), AppError>| {
        error.set(outcome.err().map(|e| e.to_string()));
        confirming.set(None);
        on_changed.run(());
    };
    let is_up = move || {
        matches!(
            status.get(),
            WorkspaceStatus::Booting | WorkspaceStatus::Running { .. }
        )
    };
    let start_or_stop = move |_| {
        error.set(None);
        let id = id.get_value();
        if is_up() {
            run_action(busy, stop_workspace(id), finish);
        } else {
            // Returns once the boot starts; the live poll shows progress.
            run_action(busy, open_workspace(id), finish);
        }
    };
    let reset = Callback::new(move |()| {
        error.set(None);
        run_action(busy, reset_workspace(id.get_value()), finish);
    });
    let remove = Callback::new(move |()| {
        error.set(None);
        run_action(busy, remove_workspace(id.get_value()), move |outcome| {
            let removed = outcome.is_ok();
            finish(outcome);
            if removed {
                on_removed.run(());
            }
        });
    });

    view! {
        <div class="actions">
            {move || status_view(&status.get()).2.map(|url| view! {
                <a href=url target="_blank" rel="noreferrer">
                    <button>"Open UI"</button>
                </a>
            })}
            <button
                on:click=start_or_stop
                disabled=move || busy.get() || matches!(status.get(), WorkspaceStatus::Stopping)
            >
                {move || if is_up() { "Stop" } else { "Start" }}
            </button>
            <ConfirmButton
                which=Confirming::Reset
                confirming
                label="Reset"
                prompt="Delete all VM state?"
                busy_label="Resetting…"
                busy
                disabled=Signal::derive(move || matches!(status.get(), WorkspaceStatus::Booting))
                on_confirm=reset
            />
            <ConfirmButton
                which=Confirming::Remove
                confirming
                label="Remove"
                prompt="Remove from list?"
                busy_label="Removing…"
                busy
                disabled=Signal::derive(|| false)
                on_confirm=remove
            />
        </div>
        {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
    }
}

/// A button that asks for confirmation inline before running `on_confirm`.
#[component]
fn ConfirmButton(
    which: Confirming,
    confirming: RwSignal<Option<Confirming>>,
    label: &'static str,
    prompt: &'static str,
    busy_label: &'static str,
    busy: RwSignal<bool>,
    disabled: Signal<bool>,
    on_confirm: Callback<()>,
) -> impl IntoView {
    move || {
        if confirming.get() == Some(which) {
            view! {
                <span class="reset-confirm">
                    <span class="reset-confirm-label">{prompt}</span>
                    <button
                        class="danger"
                        on:click=move |_| on_confirm.run(())
                        disabled=move || busy.get()
                    >
                        {move || if busy.get() { busy_label } else { "Confirm" }}
                    </button>
                    <button on:click=move |_| confirming.set(None) disabled=move || busy.get()>
                        "Cancel"
                    </button>
                </span>
            }
            .into_any()
        } else {
            view! {
                <button
                    on:click=move |_| confirming.set(Some(which))
                    disabled=move || busy.get() || disabled.get()
                >
                    {label}
                </button>
            }
            .into_any()
        }
    }
}

/// The live boot console: followed incrementally every second while
/// `Booting`, fetched once more when booting ends, and kept visible after so
/// the log doesn't vanish mid-read.
#[component]
pub(super) fn ConsolePanel(
    id: StoredValue<String>,
    status: Memo<WorkspaceStatus>,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let offset = StoredValue::new(0u64);
    let console_ref: NodeRef<leptos::html::Pre> = NodeRef::new();
    let booting = move || matches!(status.get(), WorkspaceStatus::Booting);

    use_polling(
        booting,
        Duration::from_secs(1),
        move || {
            let id = id.get_value();
            let from = offset.get_value();
            async move { read_console_log(id, from).await.ok() }
        },
        move |chunk: ConsoleChunk| {
            offset.set_value(chunk.next_offset);
            text.update(|t| merge_console_chunk(t, &chunk, MAX_CONSOLE_CHARS));
        },
        || {},
        true,
    );

    // Keep the console scrolled to the newest output.
    Effect::new(move |_| {
        text.track();
        if let Some(el) = console_ref.get() {
            scroll_to_bottom(&el);
        }
    });

    move || {
        (booting() || !text.with(String::is_empty)).then(|| {
            view! {
                <div class="console-panel">
                    <pre class="console-log" node_ref=console_ref>
                        {move || {
                            if text.with(String::is_empty) {
                                "Waiting for boot console output…".to_owned()
                            } else {
                                text.get()
                            }
                        }}
                    </pre>
                </div>
            }
        })
    }
}

/// Applies a console chunk to the text shown: replaces or appends it, then
/// keeps at most about `max` bytes of the newest output.
fn merge_console_chunk(text: &mut String, chunk: &ConsoleChunk, max: usize) {
    if chunk.replace {
        text.clear();
    }
    text.push_str(&chunk.text);
    trim_front(text, max);
}

/// Drops text from the front so at most about `max` bytes remain, cutting at
/// a line start where possible.
fn trim_front(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    // Never past the last newline: a line longer than `max` is cut
    // mid-line rather than dropped entirely.
    if let Some(newline) = text.get(cut..).and_then(|rest| rest.find('\n'))
        && cut + newline + 1 < text.len()
    {
        cut += newline + 1;
    }
    text.replace_range(..cut, "");
}

/// VM resource stats, polled every 2s while `Running` (each request waits
/// between two CPU samples, so faster polls would overlap) and cleared when
/// it stops.
#[component]
pub(super) fn StatsPanel(id: StoredValue<String>, status: Memo<WorkspaceStatus>) -> impl IntoView {
    let latest = RwSignal::new(None::<VmStats>);
    use_polling(
        move || matches!(status.get(), WorkspaceStatus::Running { .. }),
        Duration::from_secs(2),
        move || {
            let id = id.get_value();
            async move { poll_vm_stats(id).await.ok() }
        },
        move |value| latest.set(value),
        move || latest.set(None),
        false,
    );
    move || latest.get().map(|s| view! { <StatsView stats=s /> })
}

#[component]
fn StatsView(stats: VmStats) -> impl IntoView {
    let s = stats;
    let cpu_bar = percent_of(f64::from(s.cpu_percent), f64::from(s.vcpus.max(1)) * 100.0);
    view! {
        <div class="stats-panel">
            <div class="stat-row">
                <span class="stat-label">"CPU"</span>
                <div class="stat-bar">
                    <div class="stat-bar-fill" style:width=format!("{cpu_bar}%")></div>
                </div>
                <span class="stat-value">
                    {format!("{:.0}% / {} vCPU", s.cpu_percent, s.vcpus)}
                </span>
            </div>
            <div class="stat-row">
                <span class="stat-label">"Memory"</span>
                // Placeholder until the balloon driver reports.
                {match (s.mem_used_bytes, s.mem_total_bytes) {
                    (Some(used), Some(total)) => {
                        let bar = percent_of(used as f64, total.max(1) as f64);
                        view! {
                            <div class="stat-bar">
                                <div class="stat-bar-fill" style:width=format!("{bar}%")></div>
                            </div>
                            <span class="stat-value">
                                {format!("{} / {}", format_bytes(used), format_bytes(total))}
                            </span>
                        }
                        .into_any()
                    }
                    _ => view! {
                        <div class="stat-bar"></div>
                        <span class="stat-value">"—"</span>
                    }
                    .into_any(),
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
    }
}

/// "View build log" for a `BuildFailed` workspace, and the log itself.
#[component]
fn BuildLogPanel(id: StoredValue<String>, status: Memo<WorkspaceStatus>) -> impl IntoView {
    let open = RwSignal::new(false);
    // `None` while loading.
    let content = RwSignal::new(None::<Result<String, String>>);
    let loading = RwSignal::new(false);

    let toggle = move |_| {
        if open.get_untracked() {
            open.set(false);
            return;
        }
        let WorkspaceStatus::BuildFailed { log } = status.get_untracked() else {
            return;
        };
        open.set(true);
        content.set(None);
        run_action(
            loading,
            read_build_log(id.get_value(), log),
            move |result| {
                content.set(Some(result.map_err(|e| e.to_string())));
            },
        );
    };

    view! {
        {move || matches!(status.get(), WorkspaceStatus::BuildFailed { .. }).then(|| view! {
            <div class="actions">
                <button on:click=toggle>
                    {move || if open.get() { "Hide build log" } else { "View build log" }}
                </button>
            </div>
        })}
        {move || open.get().then(|| view! {
            <div class="build-log-panel">
                {move || match content.get() {
                    None => view! { <p class="empty">"Loading…"</p> }.into_any(),
                    Some(Ok(text)) => view! { <pre class="build-log">{text}</pre> }.into_any(),
                    Some(Err(e)) => view! {
                        <p class="error">"Failed to load build log: " {e}</p>
                    }
                    .into_any(),
                }}
            </div>
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_front_keeps_the_newest_text_from_a_line_start() {
        let mut text = "line one\nline two\nline three\n".to_owned();
        trim_front(&mut text, 15);
        assert_eq!(text, "line three\n");
        let mut short = "abc".to_owned();
        trim_front(&mut short, 10);
        assert_eq!(short, "abc");
        // Never splits a multi-byte character.
        let mut wide = "ééééé".to_owned();
        trim_front(&mut wide, 3);
        assert!(wide.len() <= 4 && wide.chars().all(|c| c == 'é'));
    }

    /// One line longer than the limit is cut mid-line, not dropped whole.
    #[test]
    fn trim_front_never_empties_the_text() {
        let mut long_line = "aaaaaaaaaa\n".to_owned();
        trim_front(&mut long_line, 5);
        assert_eq!(long_line, "aaaa\n");
        let mut no_newline = "abcdefghij".to_owned();
        trim_front(&mut no_newline, 4);
        assert_eq!(no_newline, "ghij");
    }

    fn chunk(text: &str, replace: bool) -> ConsoleChunk {
        ConsoleChunk {
            text: text.to_owned(),
            replace,
            next_offset: 0,
        }
    }

    #[test]
    fn console_chunks_append_unless_they_replace() {
        let mut text = String::new();
        merge_console_chunk(&mut text, &chunk("boot\n", true), 100);
        merge_console_chunk(&mut text, &chunk("ok\n", false), 100);
        assert_eq!(text, "boot\nok\n");
        // The log restarted (or we fell behind): start over.
        merge_console_chunk(&mut text, &chunk("again\n", true), 100);
        assert_eq!(text, "again\n");
        merge_console_chunk(&mut text, &chunk("", false), 100);
        assert_eq!(text, "again\n");
    }

    #[test]
    fn console_merging_keeps_the_newest_output_within_the_limit() {
        let mut text = "old one\nold two\n".to_owned();
        merge_console_chunk(&mut text, &chunk("new line\n", false), 12);
        assert_eq!(text, "new line\n");
        let mut text = String::new();
        merge_console_chunk(&mut text, &chunk("l1\nl2\nl3\nl4\n", true), 6);
        assert_eq!(text, "l4\n");
    }
}
