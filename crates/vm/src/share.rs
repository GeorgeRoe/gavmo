use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tokio::process::{Child, Command};

use crate::error::{Error, Result};
use crate::fsutil::{check_socket_path, remove_if_exists};
use crate::qemu::qemu_opt_path;

/// A host directory exposed to the guest over virtiofs: a
/// `vhost-user-fs-pci` device in qemu plus one `virtiofsd` per share. The
/// set of shares is fixed by the base image ([`crate::VmConfig::shares`]).
///
/// virtiofs rather than 9p because virtiofsd can map the share's real host
/// owner to the unprivileged guest `agent` user; 9p's xattr-mapped
/// ownership leaves a non-root agent unable to write. Linux hosts only.
///
/// `--translate-uid`/`--translate-gid` rather than `--uid-map`/`--gid-map`:
/// the latter need a user namespace, which fails unprivileged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShareSpec {
    /// Host directory to expose.
    pub(crate) host_path: PathBuf,
    /// Guest-visible mount tag. Must match the mounts in
    /// `nix/base/configuration.nix`.
    pub(crate) mount_tag: &'static str,
    /// Whether the guest should only get read access.
    pub(crate) read_only: bool,
}

/// Guest uid/gid of the unprivileged `agent` user that runs `opencode`.
/// Must match `users.users.agent` in `nix/base/configuration.nix` by hand.
/// Each share's host owner is mapped to these.
pub(crate) const GUEST_AGENT_UID: u32 = 1000;
pub(crate) const GUEST_AGENT_GID: u32 = 1000;

impl ShareSpec {
    /// This share's virtiofsd unix socket: [`Self::start_virtiofsd`] binds
    /// it, [`Self::qemu_args`] connects to it, and qemu may only start once
    /// it exists.
    pub(crate) fn socket_path(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(format!("virtiofs-{}.sock", self.mount_tag))
    }

    /// The qemu arguments for this share.
    pub(crate) fn qemu_args(&self, runtime_dir: &Path) -> Result<Vec<String>> {
        let chardev_id = format!("char-{}", self.mount_tag);
        let socket = qemu_opt_path(&self.socket_path(runtime_dir))?;

        Ok(vec![
            "-chardev".to_string(),
            format!("socket,id={chardev_id},path={socket}"),
            "-device".to_string(),
            format!(
                "vhost-user-fs-pci,chardev={chardev_id},tag={tag}",
                tag = self.mount_tag,
            ),
        ])
    }

    /// Spawns this share's `virtiofsd`.
    pub(crate) fn start_virtiofsd(&self, runtime_dir: &Path) -> Result<Child> {
        let socket_path = self.socket_path(runtime_dir);
        check_socket_path(&socket_path)?;

        // A stale socket from a crashed boot would make virtiofsd fail to
        // bind. Absent is the normal case; anything else would only resurface
        // later as an opaque virtiofsd/qemu failure, so report it now.
        remove_if_exists(&socket_path)?;

        let meta = std::fs::metadata(&self.host_path).map_err(|source| Error::Stat {
            path: self.host_path.clone(),
            source,
        })?;

        let args = build_virtiofsd_args(&socket_path, self, meta.uid(), meta.gid());
        let mut command = Command::new("virtiofsd");
        // Dies with the daemon; virtiofsd also writes `<socket>.pid`, which
        // `process::reap_stale` uses if it is ever orphaned anyway.
        crate::process::die_with_parent(&mut command);
        command
            .args(&args)
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| Error::Spawn {
                program: "virtiofsd".to_string(),
                source,
            })
    }
}

/// Builds the `virtiofsd` argument list for one share. Pure (host owner
/// passed in rather than stat'd) for unit testing.
#[expect(clippy::similar_names, reason = "uid/gid are the conventional names")]
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
        let runtime_dir = PathBuf::from("/run/user/1000/gavmo/abc");

        let args = spec.qemu_args(&runtime_dir).unwrap();

        assert_eq!(
            args,
            vec![
                "-chardev".to_string(),
                "socket,id=char-workspace,path=/run/user/1000/gavmo/abc/virtiofs-workspace.sock"
                    .to_string(),
                "-device".to_string(),
                "vhost-user-fs-pci,chardev=char-workspace,tag=workspace".to_string(),
            ]
        );
    }

    #[test]
    fn qemu_args_use_a_distinct_socket_and_tag_per_share() {
        let runtime_dir = PathBuf::from("/run/user/1000/gavmo/abc");
        let gcloud_spec = ShareSpec {
            host_path: PathBuf::from("/home/user/.config/gcloud"),
            mount_tag: "gcloud",
            read_only: true,
        };

        let args = gcloud_spec.qemu_args(&runtime_dir).unwrap();

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
    fn socket_path_is_namespaced_by_runtime_dir_and_tag() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/x"),
            mount_tag: "mcp",
            read_only: true,
        };
        assert_eq!(
            spec.socket_path(Path::new("/some/state/dir")),
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
        let socket_path = PathBuf::from("/run/user/1000/gavmo/abc/virtiofs-workspace.sock");

        let args = build_virtiofsd_args(&socket_path, &spec, 1001, 1001);

        assert_eq!(
            args,
            vec![
                "--socket-path".to_string(),
                "/run/user/1000/gavmo/abc/virtiofs-workspace.sock".to_string(),
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
