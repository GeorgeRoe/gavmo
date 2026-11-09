//! Status badges and number formatting.

use crate::types::WorkspaceStatus;

/// A workspace's short name: its directory name, or the whole path for `/`.
pub(crate) fn display_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// `part` as a percentage of `whole`, clamped to `0..=100` (for bar widths).
pub(crate) fn percent_of(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 {
        return 0.0;
    }
    (part / whole * 100.0).clamp(0.0, 100.0)
}

/// Badge class, label and (for running VMs) URL for a status.
pub(crate) fn status_view(status: &WorkspaceStatus) -> (&'static str, String, Option<String>) {
    match status {
        // Covers "never booted" too, so "idle" rather than "stopped".
        WorkspaceStatus::Stopped => ("idle", "idle".into(), None),
        WorkspaceStatus::Booting => ("booting", "booting…".into(), None),
        WorkspaceStatus::Stopping => ("booting", "stopping…".into(), None),
        WorkspaceStatus::Running { url } => ("running", "running".into(), Some(url.clone())),
        WorkspaceStatus::Failed { reason } => ("failed", format!("failed: {reason}"), None),
        // Same badge color as "failed"; the modal offers the build log.
        WorkspaceStatus::BuildFailed { .. } => ("failed", "gavmo.nix build failed".into(), None),
        // The modal explains and points at Reset.
        WorkspaceStatus::BaseImageChanged { .. } => {
            ("failed", "base image updated - reset needed".into(), None)
        }
    }
}

/// Formats a byte count with binary units: whole KiB, one decimal for MiB,
/// two for GiB.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.0} KiB", b / KIB)
    } else if b < GIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{:.2} GiB", b / GIB)
    }
}

/// Formats seconds as a compact uptime using at most the two most
/// significant units (`"45s"`, `"3m 12s"`, `"2h 05m"`, `"1d 03h"`).
pub(crate) fn format_uptime(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else if mins > 0 {
        format!("{mins}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn format_bytes_picks_the_right_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KiB");
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn format_uptime_shows_the_two_most_significant_units() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(45), "45s");
        assert_eq!(format_uptime(192), "3m 12s");
        assert_eq!(format_uptime(7_500), "2h 05m");
        assert_eq!(format_uptime(97_200), "1d 03h");
    }

    #[test]
    fn display_name_is_the_last_component() {
        assert_eq!(display_name("/home/u/project"), "project");
        assert_eq!(display_name("/home/u/project/"), "project");
        assert_eq!(display_name("/"), "/");
    }

    #[test]
    fn percent_of_clamps_and_handles_zero() {
        assert!((percent_of(50.0, 200.0) - 25.0).abs() < 1e-9);
        assert!((percent_of(500.0, 200.0) - 100.0).abs() < 1e-9);
        assert!(percent_of(5.0, 0.0).abs() < 1e-9);
        assert!(percent_of(-5.0, 10.0).abs() < 1e-9);
    }
}
