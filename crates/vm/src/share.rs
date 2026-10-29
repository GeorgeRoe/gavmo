use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tokio::process::{Child, Command};

use crate::error::{Error, Result};

/// A single host directory exposed to the guest.
#[derive(Debug, Clone)]
pub struct ShareSpec {
    /// Host directory to expose.
    pub host_path: PathBuf,
    /// Guest-visible mount tag. Must match the mounts in
    /// `nix/base/configuration.nix`.
    pub mount_tag: &'static str,
    /// Whether the guest should only get read access.
    pub read_only: bool,
}

/// Guest uid/gid of the unprivileged `agent` user that runs `opencode`.
/// Must match `users.users.agent` in `nix/base/configuration.nix` by hand.
/// Each share's host owner is mapped to these.
pub const GUEST_AGENT_UID: u32 = 1000;
pub const GUEST_AGENT_GID: u32 = 1000;

/// How a host directory is made available inside the guest.
/// [`VirtiofsShare`] is the only implementation.
pub trait ShareBackend: Send + Sync {
    /// QEMU command-line arguments contributed by this share.
    fn qemu_args(&self, state_dir: &Path, spec: &ShareSpec) -> Vec<String>;

    /// Spawns any helper process this backend needs (`virtiofsd` per share).
    fn start(&self, _state_dir: &Path, _spec: &ShareSpec) -> Result<Option<Child>> {
        Ok(None)
    }
}

/// virtiofs share backend: `vhost-user-fs-pci` plus one `virtiofsd` per
/// share.
///
/// virtiofs rather than 9p because virtiofsd can map the share's real host
/// owner to the unprivileged guest `agent` user; 9p's xattr-mapped
/// ownership leaves a non-root agent unable to write. Linux hosts only.
///
/// `--translate-uid`/`--translate-gid` rather than `--uid-map`/`--gid-map`:
/// the latter need a user namespace, which fails unprivileged.
pub struct VirtiofsShare;

impl VirtiofsShare {
    /// Path to this share's virtiofsd unix socket, shared by
    /// [`Self::start`] (binds) and [`Self::qemu_args`] (connects).
    fn socket_path(state_dir: &Path, mount_tag: &str) -> PathBuf {
        state_dir.join(format!("virtiofs-{mount_tag}.sock"))
    }
}

impl ShareBackend for VirtiofsShare {
    fn qemu_args(&self, state_dir: &Path, spec: &ShareSpec) -> Vec<String> {
        let chardev_id = format!("char-{}", spec.mount_tag);

        vec![
            "-chardev".to_string(),
            format!(
                "socket,id={id},path={path}",
                id = chardev_id,
                path = Self::socket_path(state_dir, spec.mount_tag).display(),
            ),
            "-device".to_string(),
            format!(
                "vhost-user-fs-pci,chardev={id},tag={tag}",
                id = chardev_id,
                tag = spec.mount_tag,
            ),
        ]
    }

    fn start(&self, state_dir: &Path, spec: &ShareSpec) -> Result<Option<Child>> {
        let socket_path = Self::socket_path(state_dir, spec.mount_tag);

        // A stale socket from a crashed boot would make virtiofsd fail to
        // bind.
        let _ = std::fs::remove_file(&socket_path);

        let meta = std::fs::metadata(&spec.host_path).map_err(|source| Error::Spawn {
            program: format!("virtiofsd (stat {})", spec.host_path.display()),
            source,
        })?;

        let args = build_virtiofsd_args(&socket_path, spec, meta.uid(), meta.gid());
        let child = Command::new("virtiofsd")
            .args(&args)
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| Error::Spawn {
                program: "virtiofsd".to_string(),
                source,
            })?;

        Ok(Some(child))
    }
}

/// Builds the `virtiofsd` argument list for one share. Pure (host owner
/// passed in rather than stat'd) for unit testing.
fn build_virtiofsd_args(
    socket_path: &Path,
    spec: &ShareSpec,
    host_uid: u32,
    host_gid: u32,
) -> Vec<String> {
    let mut args = vec![
        "--socket-path".to_string(),
        socket_path.display().to_string(),
        "--shared-dir".to_string(),
        spec.host_path.display().to_string(),
        // Must match the guest-side mount's cache mode.
        "--cache=auto".to_string(),
        // The default namespace sandbox needs privilege virtiofsd lacks
        // when run unprivileged; uid/gid translation doesn't need it.
        "--sandbox".to_string(),
        "none".to_string(),
        // File handles need CAP_DAC_READ_SEARCH; `never` skips the doomed
        // attempt and its warning. Must be one `=` token (clap require-equals).
        "--inode-file-handles=never".to_string(),
        // Map the share's host owner to the guest `agent` uid/gid.
        "--translate-uid".to_string(),
        format!("map:{GUEST_AGENT_UID}:{host_uid}:1"),
        "--translate-gid".to_string(),
        format!("map:{GUEST_AGENT_GID}:{host_gid}:1"),
    ];
    if spec.read_only {
        args.push("--readonly".to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_args_reference_the_sockets_virtiofsd_will_bind() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/project"),
            mount_tag: "workspace",
            read_only: false,
        };
        let state_dir = PathBuf::from("/home/user/project/.gavmo/vm");

        let args = VirtiofsShare.qemu_args(&state_dir, &spec);

        assert_eq!(
            args,
            vec![
                "-chardev".to_string(),
                "socket,id=char-workspace,path=/home/user/project/.gavmo/vm/virtiofs-workspace.sock"
                    .to_string(),
                "-device".to_string(),
                "vhost-user-fs-pci,chardev=char-workspace,tag=workspace".to_string(),
            ]
        );
    }

    #[test]
    fn qemu_args_use_a_distinct_socket_and_tag_per_share() {
        let state_dir = PathBuf::from("/home/user/project/.gavmo/vm");
        let gcloud_spec = ShareSpec {
            host_path: PathBuf::from("/home/user/.config/gcloud"),
            mount_tag: "gcloud",
            read_only: true,
        };

        let args = VirtiofsShare.qemu_args(&state_dir, &gcloud_spec);

        assert!(
            args.iter()
                .any(|a| a.contains("virtiofs-gcloud.sock")
                    && a.starts_with("socket,id=char-gcloud"))
        );
        assert!(
            args.iter()
                .any(|a| a == "vhost-user-fs-pci,chardev=char-gcloud,tag=gcloud")
        );
    }

    #[test]
    fn socket_path_is_namespaced_by_state_dir_and_tag() {
        let state_dir = PathBuf::from("/some/state/dir");
        assert_eq!(
            VirtiofsShare::socket_path(&state_dir, "mcp"),
            PathBuf::from("/some/state/dir/virtiofs-mcp.sock")
        );
    }

    #[test]
    fn virtiofsd_args_map_the_host_owner_to_the_guest_agent_uid() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/project"),
            mount_tag: "workspace",
            read_only: false,
        };
        let socket_path = PathBuf::from("/home/user/project/.gavmo/vm/virtiofs-workspace.sock");

        let args = build_virtiofsd_args(&socket_path, &spec, 1001, 1001);

        assert_eq!(
            args,
            vec![
                "--socket-path".to_string(),
                "/home/user/project/.gavmo/vm/virtiofs-workspace.sock".to_string(),
                "--shared-dir".to_string(),
                "/home/user/project".to_string(),
                "--cache=auto".to_string(),
                "--sandbox".to_string(),
                "none".to_string(),
                "--inode-file-handles=never".to_string(),
                "--translate-uid".to_string(),
                format!("map:{GUEST_AGENT_UID}:1001:1"),
                "--translate-gid".to_string(),
                format!("map:{GUEST_AGENT_GID}:1001:1"),
            ]
        );
    }

    /// Distinct uid and gid values guard against swapping the uid/gid
    /// translation targets.
    #[test]
    fn virtiofsd_args_translate_uid_and_gid_independently() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/.config/gcloud"),
            mount_tag: "gcloud",
            read_only: true,
        };
        let socket_path = PathBuf::from("/state/virtiofs-gcloud.sock");

        let args = build_virtiofsd_args(&socket_path, &spec, 4321, 100);

        assert!(
            args.windows(2)
                .any(|w| w[0] == "--translate-uid"
                    && w[1] == format!("map:{GUEST_AGENT_UID}:4321:1"))
        );
        assert!(
            args.windows(2).any(
                |w| w[0] == "--translate-gid" && w[1] == format!("map:{GUEST_AGENT_GID}:100:1")
            )
        );
    }

    #[test]
    fn virtiofsd_args_run_unprivileged_sandbox_none() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/project"),
            mount_tag: "workspace",
            read_only: false,
        };
        let socket_path = PathBuf::from("/state/virtiofs-workspace.sock");

        let args = build_virtiofsd_args(&socket_path, &spec, 1000, 1000);

        assert!(
            args.windows(2)
                .any(|w| w[0] == "--sandbox" && w[1] == "none")
        );
    }

    #[test]
    fn virtiofsd_args_add_readonly_flag_for_ro_shares() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/.config/gcloud"),
            mount_tag: "gcloud",
            read_only: true,
        };
        let socket_path = PathBuf::from("/state/virtiofs-gcloud.sock");

        let args = build_virtiofsd_args(&socket_path, &spec, 1000, 1000);

        assert_eq!(args.last().unwrap(), "--readonly");
    }

    #[test]
    fn virtiofsd_args_omit_readonly_flag_for_rw_shares() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/project"),
            mount_tag: "workspace",
            read_only: false,
        };
        let socket_path = PathBuf::from("/state/virtiofs-workspace.sock");

        let args = build_virtiofsd_args(&socket_path, &spec, 1000, 1000);

        assert!(!args.iter().any(|a| a == "--readonly"));
    }
}
