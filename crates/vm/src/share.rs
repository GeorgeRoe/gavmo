use std::path::{Path, PathBuf};

use tokio::process::Child;

use crate::error::Result;

/// A single host directory exposed to the guest.
#[derive(Debug, Clone)]
pub struct ShareSpec {
    /// Host directory to expose.
    pub host_path: PathBuf,
    /// Guest-visible mount tag. Must match the tag the base NixOS image's
    /// fstab expects (see `plan.md`'s Rust-to-base-image contract).
    pub mount_tag: &'static str,
    /// Whether the guest should only get read access.
    pub read_only: bool,
}

/// How a host directory is made available inside the guest.
///
/// 9p (see [`NinePShare`]) is the only implementation for the MVP: it's
/// built into QEMU and works identically on Windows and Linux hosts with no
/// extra background process. `virtiofsd` is Linux-host-only (it's a FUSE
/// daemon, and FUSE doesn't exist on Windows), so a `VirtiofsShare` could
/// only ever be a Linux-specific optimization behind this same trait if 9p
/// performance proves inadequate later.
pub trait ShareBackend: Send + Sync {
    /// QEMU command-line arguments contributed by this share.
    fn qemu_args(&self, spec: &ShareSpec) -> Vec<String>;

    /// Spawns any helper process this backend needs (e.g. a future
    /// `virtiofsd`). 9p needs nothing extra, so the default implementation
    /// returns `Ok(None)`.
    fn start(&self, _state_dir: &Path, _spec: &ShareSpec) -> Result<Option<Child>> {
        Ok(None)
    }
}

/// 9p share backend (`virtio-9p-pci`), built directly into QEMU.
pub struct NinePShare;

impl ShareBackend for NinePShare {
    fn qemu_args(&self, spec: &ShareSpec) -> Vec<String> {
        let fsdev_id = format!("fsdev-{}", spec.mount_tag);

        let mut fsdev = format!(
            "local,id={id},path={path},security_model=mapped-xattr",
            id = fsdev_id,
            path = spec.host_path.display(),
        );
        if spec.read_only {
            fsdev.push_str(",readonly=on");
        }

        vec![
            "-fsdev".to_string(),
            fsdev,
            "-device".to_string(),
            format!(
                "virtio-9p-pci,fsdev={id},mount_tag={tag}",
                id = fsdev_id,
                tag = spec.mount_tag,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nine_p_read_write_share_has_no_readonly_flag() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/project"),
            mount_tag: "workspace",
            read_only: false,
        };

        let args = NinePShare.qemu_args(&spec);

        assert_eq!(
            args,
            vec![
                "-fsdev".to_string(),
                "local,id=fsdev-workspace,path=/home/user/project,security_model=mapped-xattr"
                    .to_string(),
                "-device".to_string(),
                "virtio-9p-pci,fsdev=fsdev-workspace,mount_tag=workspace".to_string(),
            ]
        );
    }

    #[test]
    fn nine_p_read_only_share_adds_readonly_flag() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/.config/gcloud"),
            mount_tag: "gcloud",
            read_only: true,
        };

        let args = NinePShare.qemu_args(&spec);

        assert_eq!(
            args,
            vec![
                "-fsdev".to_string(),
                "local,id=fsdev-gcloud,path=/home/user/.config/gcloud,security_model=mapped-xattr,readonly=on"
                    .to_string(),
                "-device".to_string(),
                "virtio-9p-pci,fsdev=fsdev-gcloud,mount_tag=gcloud".to_string(),
            ]
        );
    }
}
