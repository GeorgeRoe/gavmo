//! Resource stats for a running VM.

use std::time::Duration;

use super::AppState;
use super::lifecycle::VmSlot;
use crate::types::VmStats;
use crate::vm_handle::VmInfo;

/// Gap between the two `/proc` samples [`AppState::vm_stats`] diffs for CPU
/// percentage. Short enough to keep a poll snappy, long enough that ~10ms
/// jiffy granularity doesn't dominate the measurement.
pub(super) const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

impl AppState {
    /// Resource usage for a `Running` VM: per-core CPU %, guest-reported
    /// memory, uptime, overlay size and host port. `None` (no stats this
    /// tick) if not running or a `/proc` read fails.
    ///
    /// The `running` lock is released before the CPU sampling sleep and the
    /// QMP call, so stats polls never block `stop`/`reset`.
    pub async fn vm_stats(&self, id: &str) -> Option<VmStats> {
        let info = match self.inner.running.lock().await.get(id) {
            Some(VmSlot::Running(running)) => VmInfo {
                uptime: running.info.uptime + running.info_taken.elapsed(),
                ..running.info.clone()
            },
            _ => return None,
        };
        let pid = info.qemu_pid?;
        let (uptime_secs, vcpus, host_port, qmp_socket_path) = (
            info.uptime.as_secs(),
            info.vcpus,
            info.host_port,
            info.qmp_socket_path,
        );

        let disk_bytes =
            vm::stats::file_size_or_zero(&self.host_state_dir_for(id).join("disk.qcow2")).ok()?;

        let prev = vm::stats::sample(pid).ok()?;
        tokio::time::sleep(CPU_SAMPLE_INTERVAL).await;
        let cur = vm::stats::sample(pid).ok()?;
        let cpu_percent = vm::stats::cpu_percent_per_core(prev, cur);

        // If the balloon driver hasn't reported yet, show no memory figures
        // rather than discarding the other stats.
        let guest_mem = vm::guest_memory(&qmp_socket_path).await.ok().flatten();

        Some(VmStats {
            cpu_percent,
            vcpus,
            mem_used_bytes: guest_mem.map(|m| m.total_bytes.saturating_sub(m.available_bytes)),
            mem_total_bytes: guest_mem.map(|m| m.total_bytes),
            uptime_secs,
            disk_bytes,
            host_port,
        })
    }
}
