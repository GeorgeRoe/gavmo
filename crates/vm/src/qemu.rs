use tokio::process::Child;

use crate::config::{GUEST_OPENCODE_PORT, VmConfig};
use crate::error::{Error, Result};
use crate::share::{ShareBackend, ShareSpec};

/// Builds the full `qemu-system-x86_64` argument list for a VM boot, given
/// its config and the shares it should expose.
///
/// Pure and side-effect free so it can be unit-tested without `qemu`
/// installed (see `plan.md`'s testing strategy: this is exercised in the
/// sidecar, which has no `/dev/kvm`).
pub fn build_qemu_args(cfg: &VmConfig, shares: &[(&dyn ShareBackend, ShareSpec)]) -> Vec<String> {
    let mut args = Vec::new();

    // Hardware acceleration is Linux-only (KVM). A Windows host would use
    // WHPX instead; that's a future cfg-gated addition, not part of the MVP.
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

    for (backend, spec) in shares {
        args.extend(backend.qemu_args(spec));
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

/// Spawns `qemu-system-x86_64` with the given config and shares, starting
/// any helper processes the share backends need first (e.g. a future
/// `virtiofsd`). Returns the qemu child and any helper children, all of
/// which the caller is responsible for cleaning up.
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
    use crate::share::NinePShare;
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

    /// Mirrors the three shares `RunningVm::boot` actually assembles
    /// (`handle.rs`) — workspace (rw), gcloud (ro), and the MCP token share
    /// (ro, see `VmConfig::mcp_dir`/`crates/mcp/plan.md`) — even though
    /// `build_qemu_args` itself is share-count-agnostic.
    #[test]
    fn args_include_each_share() {
        let cfg = test_cfg();
        let nine_p = NinePShare;
        let shares: Vec<(&dyn ShareBackend, ShareSpec)> = vec![
            (
                &nine_p,
                ShareSpec {
                    host_path: cfg.repo_dir.clone(),
                    mount_tag: "workspace",
                    read_only: false,
                },
            ),
            (
                &nine_p,
                ShareSpec {
                    host_path: cfg.gcloud_dir.clone(),
                    mount_tag: "gcloud",
                    read_only: true,
                },
            ),
            (
                &nine_p,
                ShareSpec {
                    host_path: cfg.mcp_dir.clone(),
                    mount_tag: "mcp",
                    read_only: true,
                },
            ),
        ];

        let args = build_qemu_args(&cfg, &shares);

        assert!(args.iter().any(|a| a.contains("mount_tag=workspace")));
        assert!(args.iter().any(|a| a.contains("mount_tag=gcloud")));
        assert!(args.iter().any(|a| a.contains("mount_tag=mcp")));
        assert_eq!(
            args.iter().filter(|a| a.contains("readonly=on")).count(),
            2,
            "gcloud and mcp shares are read-only; workspace is not"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kvm_acceleration_is_enabled_on_linux() {
        let cfg = test_cfg();
        let args = build_qemu_args(&cfg, &[]);

        assert!(args.contains(&"-enable-kvm".to_string()));
    }
}
