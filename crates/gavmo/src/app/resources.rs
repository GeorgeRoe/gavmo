//! A workspace's VM resources: disk size, memory and vCPUs.

#![expect(
    unreachable_pub,
    reason = "#[component] gives its props builder `pub` items, unreachable from a private module"
)]

use leptos::prelude::*;

use super::hooks::run_action;
use crate::api::{get_resources, set_resources};
use crate::types::{ResourcesView, WorkspaceStatus};

/// The text in a field: empty means "use the default".
fn field_text(value: Option<u32>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// Parses a field: empty is `None` (the default), anything else must be a
/// whole number.
pub(super) fn parse_field(name: &str, text: &str) -> Result<Option<u32>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|e| format!("{name} must be a whole number ({e})"))
}

/// The modal's Resources panel. Memory and vCPUs apply on the next start;
/// the disk can only grow, and only while the VM is stopped.
#[component]
pub(crate) fn ResourcesPanel(id: String, status: Memo<WorkspaceStatus>) -> impl IntoView {
    let id = StoredValue::new(id);
    // `None` = still loading.
    let view_data = RwSignal::new(None::<ResourcesView>);
    let disk = RwSignal::new(String::new());
    let memory = RwSignal::new(String::new());
    let vcpus = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<Result<String, String>>);

    let vm_active = move || {
        matches!(
            status.get(),
            WorkspaceStatus::Booting | WorkspaceStatus::Running { .. } | WorkspaceStatus::Stopping
        )
    };

    let load = move || {
        run_action(
            busy,
            get_resources(id.get_value()),
            move |outcome| match outcome {
                Ok(data) => {
                    disk.set(field_text(data.saved.disk_gib));
                    memory.set(field_text(data.saved.memory_gib));
                    vcpus.set(field_text(data.saved.vcpus));
                    view_data.set(Some(data));
                }
                Err(e) => result.set(Some(Err(format!("Failed to load resources: {e}")))),
            },
        );
    };
    // Reads no reactive state, so this runs once after mount.
    Effect::new(move |_| load());

    let save = move |_| {
        let fields = (|| {
            Ok::<_, String>((
                parse_field("Disk", &disk.get_untracked())?,
                parse_field("Memory", &memory.get_untracked())?,
                parse_field("vCPUs", &vcpus.get_untracked())?,
            ))
        })();
        let (disk_gib, memory_gib, cpus) = match fields {
            Ok(fields) => fields,
            Err(e) => {
                result.set(Some(Err(e)));
                return;
            }
        };
        result.set(None);
        let call = set_resources(id.get_value(), disk_gib, memory_gib, cpus);
        run_action(busy, call, move |outcome| {
            result.set(Some(match outcome {
                Ok(()) => {
                    load();
                    Ok("Saved; applies the next time this workspace starts".to_owned())
                }
                Err(e) => Err(e.to_string()),
            }));
        });
    };

    view! {
        <div class="tools-panel resources-panel">
            <h3 class="tools-title">"Resources"</h3>
            {move || view_data.get().map(|data| view! {
                <ResourceFields data disk memory vcpus busy disk_locked=Signal::derive(vm_active) />
            })}
            {move || vm_active().then(|| view! {
                <p class="hint">
                    "Changes apply the next time this workspace starts. Stop it to change the disk size."
                </p>
            })}
            <div class="tool-actions">
                <button class="resources-save" on:click=save disabled=move || busy.get() || view_data.with(Option::is_none)>
                    "Save"
                </button>
            </div>
            {move || result.get().map(|r| match r {
                Ok(msg) => view! { <pre class="tool-result tool-result--ok">{msg}</pre> }.into_any(),
                Err(msg) => view! { <pre class="tool-result tool-result--err">{msg}</pre> }.into_any(),
            })}
        </div>
    }
    .into_any()
}

/// The three fields, with defaults as placeholders and the allowed ranges.
#[component]
fn ResourceFields(
    data: ResourcesView,
    disk: RwSignal<String>,
    memory: RwSignal<String>,
    vcpus: RwSignal<String>,
    busy: RwSignal<bool>,
    disk_locked: Signal<bool>,
) -> impl IntoView {
    let (d, lo, hi) = (data.defaults, data.min, data.max);
    let disk_min = data
        .disk_current_gib
        .map_or(lo.disk_gib, |c| c.max(lo.disk_gib));
    let input = move |signal: RwSignal<String>, default: u32, locked: Signal<bool>| {
        view! {
            <input
                class="tool-input resource-input"
                r#type="number"
                min="1"
                placeholder=format!("{default} (default)")
                prop:value=move || signal.get()
                on:input=move |ev| signal.set(event_target_value(&ev))
                disabled=move || busy.get() || locked.get()
            />
        }
    };
    let unlocked = Signal::derive(|| false);
    view! {
        <div class="resource-grid">
            <label>"Disk (GiB)"</label>
            {input(disk, d.disk_gib, disk_locked)}
            <span class="hint">
                {match data.disk_current_gib {
                    Some(c) => format!("now {c} GiB; {disk_min}-{} (grow only)", hi.disk_gib),
                    None => format!("{disk_min}-{}", hi.disk_gib),
                }}
            </span>
            <label>"Memory (GiB)"</label>
            {input(memory, d.memory_gib, unlocked)}
            <span class="hint">{format!("{}-{}", lo.memory_gib, hi.memory_gib)}</span>
            <label>"vCPUs"</label>
            {input(vcpus, d.vcpus, unlocked)}
            <span class="hint">{format!("{}-{}", lo.vcpus, hi.vcpus)}</span>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_fields_mean_the_default_and_others_must_be_numbers() {
        assert_eq!(parse_field("Disk", ""), Ok(None));
        assert_eq!(parse_field("Disk", "  "), Ok(None));
        assert_eq!(parse_field("Disk", " 64 "), Ok(Some(64)));
        assert!(
            parse_field("Disk", "1.5")
                .unwrap_err()
                .contains("whole number")
        );
        assert!(parse_field("Disk", "-1").is_err());
        assert_eq!(field_text(None), "");
        assert_eq!(field_text(Some(8)), "8");
    }
}
