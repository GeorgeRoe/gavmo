use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tokio::process::{Child, Command};

use crate::error::{Error, Result};

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

/// The guest-side uid/gid of the unprivileged `agent` user that runs
/// `opencode` (see `users.users.agent` in `nix/base/configuration.nix`).
/// Pinned here *and* there as plain numeric literals — like
/// `GAVMO_MCP_PORT`/`mcp.gavmo-tools` elsewhere in this codebase, there is no
/// automatic sharing between the two declarations, so a change to one must be
/// mirrored in the other by hand. [`VirtiofsShare`] maps each share's host
/// owner to this uid/gid so `agent` (not root) can read/write it without the
/// host and guest uid *numbers* needing to match.
pub const GUEST_AGENT_UID: u32 = 1000;
pub const GUEST_AGENT_GID: u32 = 1000;

/// How a host directory is made available inside the guest.
///
/// [`VirtiofsShare`] is the only implementation (GAVMO M3 onward — see
/// `plan.md`'s "Why virtiofs now" for the Windows-support tradeoff that
/// motivated the switch from the original 9p backend). The trait seam is
/// kept mainly so a share backend remains swappable without touching
/// `qemu.rs`/`handle.rs`.
pub trait ShareBackend: Send + Sync {
    /// QEMU command-line arguments contributed by this share.
    fn qemu_args(&self, state_dir: &Path, spec: &ShareSpec) -> Vec<String>;

    /// Spawns any helper process this backend needs. virtiofs needs one
    /// `virtiofsd` per share; the default implementation (for a hypothetical
    /// future backend that needs none) returns `Ok(None)`.
    fn start(&self, _state_dir: &Path, _spec: &ShareSpec) -> Result<Option<Child>> {
        Ok(None)
    }
}

/// virtiofs share backend (`vhost-user-fs-pci` + a per-share `virtiofsd`
/// helper process), replacing the original 9p transport.
///
/// ## Why virtiofs instead of 9p
///
/// 9p's `security_model=mapped-xattr` stores guest-visible file ownership in
/// host-side xattrs, decoupled from real host uids. That's fine for a
/// **root** guest agent (root bypasses DAC entirely, so whatever ownership
/// the guest sees doesn't matter) but becomes load-bearing the moment the
/// guest agent is unprivileged: if pre-existing files present with
/// unexpected/root ownership, a non-root agent can neither write `/workspace`
/// nor read the mounted credentials.
///
/// virtiofsd's **internal** UID/GID translation (`--translate-uid`/
/// `--translate-gid` below) sidesteps this entirely: it translates the
/// *real* host owner of the shared directory to the guest's unprivileged
/// `agent` user, so ownership "just works" without xattr quirks or
/// host/guest uid numbers needing to numerically coincide. This is the
/// change that makes a non-root guest agent (see
/// `nix/base/configuration.nix`) practical. The cost is dropping Windows
/// host support: `virtiofsd` is a Linux-only (FUSE-based) daemon. See
/// `plan.md` for the full history of that tradeoff.
///
/// ## Why `--translate-uid`/`--translate-gid`, not `--uid-map`/`--gid-map`
///
/// virtiofsd has two different UID/GID remapping mechanisms, and only one
/// works unprivileged (confirmed against `virtiofsd --help`, version 1.13.3):
///
/// - `--uid-map`/`--gid-map`: mapping performed by the **user namespace**
///   virtiofsd places itself into via `--sandbox=namespace` — requires that
///   namespace's root-mapping to succeed, which needs privilege virtiofsd
///   doesn't have when run as a plain unprivileged user (confirmed by real
///   hardware testing: this produced `Couldn't set the process uid as root`
///   and left `/workspace` with its raw, untranslated host ownership).
/// - **`--translate-uid`/`--translate-gid`**: mapping performed **internally
///   by virtiofsd itself**, explicitly documented as *"does not require
///   using a user namespace"*. This is what we use. Combined with
///   `--sandbox none` (below), virtiofsd needs no special privilege at all —
///   it runs as whatever unprivileged user the GAVMO daemon itself runs as,
///   which already owns the shared directories it's translating.
///
/// We use each type's `map:<guest base>:<host base>:<count>` form — a
/// bidirectional 1:1 map between [`GUEST_AGENT_UID`]/[`GUEST_AGENT_GID`] and
/// the share's real host owner (stat'd at `start()` time, so this is
/// correct for *any* host uid/gid, not just a coincidentally-matching one).
pub struct VirtiofsShare;

impl VirtiofsShare {
    /// Path to this share's virtiofsd unix socket, under the VM's state dir.
    /// Must agree between [`Self::start`] (which binds it) and
    /// [`Self::qemu_args`] (which tells qemu where to connect) — both derive
    /// it from the same `state_dir` + `mount_tag` rather than threading a
    /// socket path through `ShareSpec`.
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

        // Best-effort: a stale socket left behind by a previous (e.g.
        // crashed) boot would otherwise make virtiofsd fail to bind.
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

/// Builds the `virtiofsd` argument list for one share. Pure (takes the host
/// owner's uid/gid as plain arguments rather than stat-ing `spec.host_path`
/// itself) so it's unit-testable without touching the filesystem — mirrors
/// `disk.rs::build_overlay_args`'s split of "pure arg-building" from
/// "actual spawn" for the same reason.
///
/// Validated against real hardware (virtiofsd 1.13.3) as part of the design
/// discussion that landed this — see [`VirtiofsShare`]'s doc comment for why
/// `--translate-uid`/`--translate-gid` + `--sandbox none` is the combination
/// that works fully unprivileged.
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
        "--cache=auto".to_string(),
        // No user namespace: `--translate-uid`/`--translate-gid` below do
        // their own internal mapping and need no privilege to do it (see
        // VirtiofsShare's doc comment) - `--sandbox=namespace` (the
        // default) would otherwise require a privilege virtiofsd doesn't
        // have when run as a plain unprivileged user.
        "--sandbox".to_string(),
        "none".to_string(),
        // Unprivileged virtiofsd can't use `name_to_handle_at` (needs
        // `CAP_DAC_READ_SEARCH`) to reference inodes via file handles - left
        // at the default ("prefer"), it tries anyway, fails, and logs a
        // "file handles do not appear safe" warning before falling back.
        // `never` skips straight to the fallback (plain `O_PATH` file
        // descriptors) with no wasted attempt or warning.
        //
        // One combined `--flag=value` token, NOT two separate args: clap
        // marks this specific option require-equals (per `virtiofsd --help`:
        // `--inode-file-handles=<INODE_FILE_HANDLES>`, unlike e.g. `--cache
        // <CACHE>`) - confirmed on real hardware, `--inode-file-handles
        // never` as two args is rejected with "equal sign is needed when
        // assigning values to '--inode-file-handles=<INODE_FILE_HANDLES>'",
        // which aborts virtiofsd before it ever binds its socket.
        "--inode-file-handles=never".to_string(),
        // Maps the share's real host owner to the guest's unprivileged
        // `agent` uid/gid (`GUEST_AGENT_UID`/`_GID`) so `agent` - not root -
        // can access it correctly, regardless of what the host uid actually
        // is. Internal to virtiofsd (see this function's doc comment), so
        // needs no privilege.
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

    /// Distinct uid and gid values (unlike the test above, which reuses the
    /// same number for both) - guards against a copy-paste bug swapping the
    /// uid/gid translation targets.
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
