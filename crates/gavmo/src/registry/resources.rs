//! Per-workspace VM resources: disk size, memory and vCPUs.
//!
//! Chosen by the user in the panel and saved in the registry, never read
//! from the repo: they decide how much of the host a VM may use, and the
//! repo is guest-writable.

use super::AppState;
use super::error::{RegistryError, Result, io_ctx};
use super::store::save_registry;
use crate::types::{ResourceValues, Resources, ResourcesView};

/// What an unset setting means. 32 GiB of disk because the base image is
/// auto-sized with little headroom, too small once `gavmo.nix` adds real
/// packages; qcow2 is sparse, so it costs nothing until written.
pub(super) const DEFAULTS: ResourceValues = ResourceValues {
    disk_gib: 32,
    memory_gib: 4,
    vcpus: 4,
};

/// Lower bounds. 16 GiB of disk leaves room above the base image.
const MIN: ResourceValues = ResourceValues {
    disk_gib: 16,
    memory_gib: 1,
    vcpus: 1,
};

/// Largest disk offered. Sparse, but a guest can fill it.
const MAX_DISK_GIB: u32 = 1024;

const GIB: u64 = 1024 * 1024 * 1024;

/// Upper bounds: the disk cap, and the host's own RAM and CPUs (a guest
/// can't usefully have more, and more RAM than the host risks the host).
fn max_values() -> ResourceValues {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    ResourceValues {
        disk_gib: MAX_DISK_GIB,
        memory_gib: host_memory_gib().max(MIN.memory_gib),
        vcpus: u32::try_from(cpus).unwrap_or(u32::MAX).max(MIN.vcpus),
    }
}

/// Total host RAM in whole GiB, from `/proc/meminfo`; the default memory if
/// it can't be read (so the panel stays usable).
fn host_memory_gib() -> u32 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| parse_mem_total_gib(&text))
        .unwrap_or(DEFAULTS.memory_gib)
}

/// `MemTotal:  16314508 kB` -> 15 (whole GiB, rounded down).
pub(super) fn parse_mem_total_gib(meminfo: &str) -> Option<u32> {
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()?;
    u32::try_from(kib / (1024 * 1024)).ok()
}

/// Checks each set field against `[min, max]`.
pub(super) fn validate(
    settings: Resources,
    min: ResourceValues,
    max: ResourceValues,
) -> Result<()> {
    let check = |name: &str, unit: &str, value: Option<u32>, lo: u32, hi: u32| match value {
        Some(v) if v < lo || v > hi => Err(RegistryError::InvalidInput(format!(
            "{name} must be between {lo} and {hi}{unit}"
        ))),
        _ => Ok(()),
    };
    check(
        "Disk",
        " GiB",
        settings.disk_gib,
        min.disk_gib,
        max.disk_gib,
    )?;
    check(
        "Memory",
        " GiB",
        settings.memory_gib,
        min.memory_gib,
        max.memory_gib,
    )?;
    check("vCPUs", "", settings.vcpus, min.vcpus, max.vcpus)
}

/// Whole GiB, rounded up.
fn bytes_to_gib_ceil(bytes: u64) -> u32 {
    u32::try_from(bytes.div_ceil(GIB)).unwrap_or(u32::MAX)
}

impl AppState {
    /// `id`'s saved settings (unknown ids get the defaults).
    pub(super) async fn saved_resources(&self, id: &str) -> Resources {
        self.inner
            .registry
            .lock()
            .await
            .workspaces
            .iter()
            .find(|w| w.path == id)
            .map(|w| w.resources)
            .unwrap_or_default()
    }

    /// The size of `id`'s existing overlay disk, if there is one.
    async fn current_disk_gib(&self, id: &str) -> Result<Option<u32>> {
        let disk = self.host_state_dir_for(id).join("disk.qcow2");
        if !tokio::fs::try_exists(&disk).await.unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(bytes_to_gib_ceil(
            vm::overlay_virtual_size(&disk).await?,
        )))
    }

    /// The Resources panel's data for `id`.
    pub async fn resources(&self, id: &str) -> Result<ResourcesView> {
        self.require_known(id).await?;
        Ok(ResourcesView {
            saved: self.saved_resources(id).await,
            defaults: DEFAULTS,
            min: MIN,
            max: max_values(),
            disk_current_gib: self.current_disk_gib(id).await?,
        })
    }

    /// Saves `id`'s resource settings. Memory and vCPUs take effect on the
    /// next start. The disk can only change while the VM is stopped (it is
    /// grown on the next start) and never below the existing disk's size:
    /// shrinking would cut off the guest's filesystem, so a smaller disk
    /// needs a reset first.
    pub async fn set_resources(&self, id: &str, settings: Resources) -> Result<()> {
        self.require_known(id).await?;
        validate(settings, MIN, max_values())?;
        // Held so no boot starts (and reads the old size) mid-change.
        let lifecycle = self.lifecycle_lock(id);
        let Ok(_held) = lifecycle.try_lock() else {
            return Err(RegistryError::Busy);
        };

        let old_disk = self.saved_resources(id).await.resolve(DEFAULTS).disk_gib;
        let new_disk = settings.resolve(DEFAULTS).disk_gib;
        if new_disk != old_disk {
            let active = self
                .inner
                .running
                .lock()
                .await
                .get(id)
                .is_some_and(super::lifecycle::VmSlot::is_active);
            if active {
                return Err(RegistryError::InvalidInput(
                    "stop the VM to change its disk size".to_owned(),
                ));
            }
            if let Some(current) = self.current_disk_gib(id).await?
                && new_disk < current
            {
                return Err(RegistryError::InvalidInput(format!(
                    "the disk is already {current} GiB and can't shrink; reset the \
                     workspace first (this deletes the VM disk) to choose a smaller size"
                )));
            }
        }

        let mut registry = self.inner.registry.lock().await;
        let Some(record) = registry.workspaces.iter_mut().find(|w| w.path == id) else {
            return Err(RegistryError::UnknownWorkspace(id.to_owned()));
        };
        record.resources = settings;
        save_registry(&self.inner.registry_path, &registry)
            .await
            .map_err(io_ctx("saving the workspace registry"))
    }
}
