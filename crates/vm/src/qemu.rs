use tokio::process::Child;

use crate::config::{GUEST_OPENCODE_PORT, VmConfig};
use crate::error::{Error, Result};
use crate::share::{ShareBackend, ShareSpec};

/// The `id=` of the `virtio-balloon-pci` device. `qmp.rs` derives the
/// balloon's QOM path (`/machine/peripheral/<id>`) from this.
pub(crate) const BALLOON_DEVICE_ID: &str = "balloon0";

/// Builds the full `qemu-system-x86_64` argument list for a VM boot. Pure,
/// for unit testing without `qemu` or `/dev/kvm`.
pub fn build_qemu_args(cfg: &VmConfig, shares: &[(&dyn ShareBackend, ShareSpec)]) -> Vec<String> {
    let mut args = Vec::new();

    // Hosts are Linux-only (virtiofs); the `cfg` just keeps non-Linux
    // development builds compiling.
    #[cfg(target_os = "linux")]
    {
        args.push("-enable-kvm".to_string());
        args.push("-cpu".to_string());
        args.push("host".to_string());
    }

    args.push("-smp".to_string());
    args.push(cfg.vcpus.to_string());
    args.push("-m".to_string());
    args.push(cfg.memory_mib.to_string());

    // virtiofs needs guest RAM in a shared memory backend so `virtiofsd`
    // can map it; `-numa node,memdev=mem` is how QEMU wires guest RAM to
    // that backend, even for a single-node guest.
    if !shares.is_empty() {
        args.push("-object".to_string());
        args.push(format!(
            "memory-backend-memfd,id=mem,size={mem}M,share=on",
            mem = cfg.memory_mib,
        ));
        args.push("-numa".to_string());
        args.push("node,memdev=mem".to_string());
    }

    args.push("-drive".to_string());
    args.push(format!(
        "file={file},if=virtio,format=qcow2",
        file = cfg.disk_path().display(),
    ));

    args.push("-netdev".to_string());
    args.push(format!(
        "user,id=net0,hostfwd=tcp:127.0.0.1:{host_port}-:{guest_port}",
        host_port = cfg.host_port,
        guest_port = GUEST_OPENCODE_PORT,
    ));
    args.push("-device".to_string());
    args.push("virtio-net-pci,netdev=net0".to_string());

    // The balloon is never inflated; it exists only so the guest reports
    // its own memory usage over QMP (host RSS doesn't reflect guest usage).
    // `deflate-on-oom=on` is a free safety net.
    args.push("-device".to_string());
    args.push(format!(
        "virtio-balloon-pci,id={BALLOON_DEVICE_ID},deflate-on-oom=on"
    ));

    for (backend, spec) in shares {
        args.extend(backend.qemu_args(&cfg.state_dir, spec));
    }

    args.push("-qmp".to_string());
    args.push(format!(
        "unix:{path},server,nowait",
        path = cfg.qmp_socket_path().display(),
    ));

    args.push("-nographic".to_string());
    args.push("-serial".to_string());
    args.push(format!("file:{}", cfg.console_log_path().display()));

    args
}

/// Spawns `qemu-system-x86_64`, first starting any share helper processes
/// (`virtiofsd`). The caller is responsible for cleaning up all returned
/// children.
pub async fn spawn_qemu(
    cfg: &VmConfig,
    shares: &[(&dyn ShareBackend, ShareSpec)],
) -> Result<(Child, Vec<Child>)> {
    let mut helpers = Vec::new();
    for (backend, spec) in shares {
        if let Some(child) = backend.start(&cfg.state_dir, spec)? {
            helpers.push(child);
        }
    }

    let args = build_qemu_args(cfg, shares);
    let child = tokio::process::Command::new("qemu-system-x86_64")
        .args(&args)
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| Error::Spawn {
            program: "qemu-system-x86_64".to_string(),
            source,
        })?;

    Ok((child, helpers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::VirtiofsShare;
    use std::path::PathBuf;

    fn test_cfg() -> VmConfig {
        VmConfig {
            workspace_id: "test-workspace".to_string(),
            repo_dir: PathBuf::from("/home/user/project"),
            state_dir: PathBuf::from("/home/user/project/.gavmo/vm"),
            base_image: PathBuf::from("/var/lib/gavmo/base.qcow2"),
            gcloud_dir: PathBuf::from("/home/user/.config/gcloud"),
            mcp_dir: PathBuf::from("/var/lib/gavmo/mcp/test-workspace"),
            memory_mib: 4096,
            vcpus: 4,
            host_port: 45123,
        }
    }

    #[test]
    fn args_include_forwarded_port_and_guest_port() {
        let cfg = test_cfg();
        let args = build_qemu_args(&cfg, &[]);

        assert!(args.contains(&"-netdev".to_string()));
        let netdev = args
            .iter()
            .find(|a| a.starts_with("user,id=net0"))
            .expect("netdev arg present");
        assert_eq!(netdev, "user,id=net0,hostfwd=tcp:127.0.0.1:45123-:7080");
    }

    #[test]
    fn args_reference_the_overlay_disk_and_qmp_socket() {
        let cfg = test_cfg();
        let args = build_qemu_args(&cfg, &[]);

        assert!(
            args.iter()
                .any(|a| a.contains("/home/user/project/.gavmo/vm/disk.qcow2"))
        );
        assert!(
            args.iter()
                .any(|a| a == "unix:/home/user/project/.gavmo/vm/qmp.sock,server,nowait")
        );
    }

    /// Mirrors the three shares `RunningVm::boot` assembles.
    #[test]
    fn args_include_each_share() {
        let cfg = test_cfg();
        let virtiofs = VirtiofsShare;
        let shares: Vec<(&dyn ShareBackend, ShareSpec)> = vec![
            (
                &virtiofs,
                ShareSpec {
                    host_path: cfg.repo_dir.clone(),
                    mount_tag: "workspace",
                    read_only: false,
                },
            ),
            (
                &virtiofs,
                ShareSpec {
                    host_path: cfg.gcloud_dir.clone(),
                    mount_tag: "gcloud",
                    read_only: true,
                },
            ),
            (
                &virtiofs,
                ShareSpec {
                    host_path: cfg.mcp_dir.clone(),
                    mount_tag: "mcp",
                    read_only: true,
                },
            ),
        ];

        let args = build_qemu_args(&cfg, &shares);

        assert!(
            args.iter()
                .any(|a| a == "vhost-user-fs-pci,chardev=char-workspace,tag=workspace")
        );
        assert!(
            args.iter()
                .any(|a| a == "vhost-user-fs-pci,chardev=char-gcloud,tag=gcloud")
        );
        assert!(
            args.iter()
                .any(|a| a == "vhost-user-fs-pci,chardev=char-mcp,tag=mcp")
        );
    }

    /// virtiofs requires shared guest RAM, but only when a share is present.
    #[test]
    fn args_add_shared_memory_backend_when_shares_are_present() {
        let cfg = test_cfg();
        let virtiofs = VirtiofsShare;
        let shares: Vec<(&dyn ShareBackend, ShareSpec)> = vec![(
            &virtiofs,
            ShareSpec {
                host_path: cfg.repo_dir.clone(),
                mount_tag: "workspace",
                read_only: false,
            },
        )];

        let args = build_qemu_args(&cfg, &shares);

        assert!(
            args.iter()
                .any(|a| a == "memory-backend-memfd,id=mem,size=4096M,share=on")
        );
        assert!(args.iter().any(|a| a == "node,memdev=mem"));
    }

    #[test]
    fn args_omit_shared_memory_backend_when_there_are_no_shares() {
        let cfg = test_cfg();

        let args = build_qemu_args(&cfg, &[]);

        assert!(!args.iter().any(|a| a.contains("memory-backend-memfd")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kvm_acceleration_is_enabled_on_linux() {
        let cfg = test_cfg();
        let args = build_qemu_args(&cfg, &[]);

        assert!(args.contains(&"-enable-kvm".to_string()));
    }

    /// The balloon is the memory stats channel, independent of virtiofs.
    #[test]
    fn args_include_the_balloon_device_for_guest_memory_stats() {
        let cfg = test_cfg();
        let args = build_qemu_args(&cfg, &[]);

        assert!(
            args.iter()
                .any(|a| a
                    == &format!("virtio-balloon-pci,id={BALLOON_DEVICE_ID},deflate-on-oom=on"))
        );
    }
}
