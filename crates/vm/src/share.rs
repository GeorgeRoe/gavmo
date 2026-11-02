use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::error::{Error, Result};
use crate::fsutil::{check_socket_path, remove_if_exists};
use crate::qemu::opt_with_path;

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
    pub(crate) fn qemu_args(&self, runtime_dir: &Path) -> Vec<OsString> {
        let chardev_id = format!("char-{}", self.mount_tag);
        vec![
            "-chardev".into(),
            opt_with_path(
                &format!("socket,id={chardev_id},path="),
                &self.socket_path(runtime_dir),
                "",
            ),
            "-device".into(),
            format!(
                "vhost-user-fs-pci,chardev={chardev_id},tag={tag}",
                tag = self.mount_tag,
            )
            .into(),
        ]
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
/// passed in rather than stat'd) for unit testing. Paths are passed as-is
/// (`OsString`), so a repo path that isn't UTF-8 shares that directory
/// rather than a lossily-converted name.
#[expect(clippy::similar_names, reason = "uid/gid are the conventional names")]
fn build_virtiofsd_args(
    socket_path: &Path,
    spec: &ShareSpec,
    host_uid: u32,
    host_gid: u32,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--socket-path".into(),
        socket_path.into(),
        "--shared-dir".into(),
        spec.host_path.clone().into(),
        // Must match the guest-side mount's cache mode.
        "--cache=auto".into(),
        // The default namespace sandbox needs privilege virtiofsd lacks
        // when run unprivileged; uid/gid translation doesn't need it.
        "--sandbox".into(),
        "none".into(),
        // File handles need CAP_DAC_READ_SEARCH; `never` skips the doomed
        // attempt and its warning. Must be one `=` token (clap require-equals).
        "--inode-file-handles=never".into(),
        // Map the share's host owner to the guest `agent` uid/gid.
        "--translate-uid".into(),
        format!("map:{GUEST_AGENT_UID}:{host_uid}:1").into(),
        "--translate-gid".into(),
        format!("map:{GUEST_AGENT_GID}:{host_gid}:1").into(),
    ];
    if spec.read_only {
        args.push("--readonly".into());
    }
    args
}

/// A virtiofsd release as `(major, minor, patch)`.
type Version = (u32, u32, u32);

/// The first virtiofsd with `--translate-uid`/`--translate-gid` and
/// `--readonly`, which [`build_virtiofsd_args`] passes. Older ones (e.g.
/// Ubuntu 24.04's) reject them and exit with status 2.
const MIN_VIRTIOFSD_VERSION: Version = (1, 13, 0);

/// Refuses a virtiofsd older than [`MIN_VIRTIOFSD_VERSION`], which would
/// otherwise only show up as [`Error::HelperExited`] with no reason.
/// `program` is `"virtiofsd"` outside tests. Output that doesn't parse is
/// let through with a warning, so a distro that formats `--version`
/// differently isn't locked out; a missing binary is a spawn error.
pub(crate) async fn check_virtiofsd_version(program: &OsStr) -> Result<()> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| Error::Spawn {
            program: program.to_string_lossy().into_owned(),
            source,
        })?;
    let text = String::from_utf8_lossy(&output.stdout);
    match parse_virtiofsd_version(&text) {
        Some(found) if found < MIN_VIRTIOFSD_VERSION => Err(Error::VirtiofsdTooOld {
            found: fmt_version(found),
            required: fmt_version(MIN_VIRTIOFSD_VERSION),
        }),
        Some(_) => Ok(()),
        None => {
            tracing::warn!(
                output = %text.trim(),
                "could not read the virtiofsd version; assuming it is new enough"
            );
            Ok(())
        }
    }
}

/// Parses `virtiofsd --version` output: `virtiofsd 1.14.0`, possibly with a
/// distro suffix (`1.10.1-1ubuntu1`) or no patch number. `None` for
/// anything else, including the old C virtiofsd's `virtiofsd version 7.2.0`
/// (a qemu version, which must not be compared as a virtiofsd one).
fn parse_virtiofsd_version(output: &str) -> Option<Version> {
    let mut words = output.split_whitespace();
    if words.next()? != "virtiofsd" {
        return None;
    }
    let mut parts = words.next()?.split('.');
    // The leading digits of the next component: `1-1ubuntu1` is 1.
    let mut number = || -> Option<u32> {
        parts
            .next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    let major = number()?;
    let minor = number()?;
    let patch = number().unwrap_or(0);
    Some((major, minor, patch))
}

fn fmt_version((major, minor, patch): Version) -> String {
    format!("{major}.{minor}.{patch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_virtiofsd_versions_and_rejects_other_output() {
        assert_eq!(
            parse_virtiofsd_version("virtiofsd 1.14.0\n"),
            Some((1, 14, 0))
        );
        assert_eq!(
            parse_virtiofsd_version("virtiofsd 1.10.1-1ubuntu1"),
            Some((1, 10, 1))
        );
        assert_eq!(parse_virtiofsd_version("virtiofsd 1.13"), Some((1, 13, 0)));
        assert_eq!(parse_virtiofsd_version("virtiofsd version 7.2.0"), None);
        assert_eq!(parse_virtiofsd_version("virtiofsd"), None);
        assert_eq!(parse_virtiofsd_version("virtiofsd 1"), None);
        assert_eq!(parse_virtiofsd_version("qemu 1.14.0"), None);
        assert_eq!(parse_virtiofsd_version(""), None);
    }

    /// A stand-in `virtiofsd` that prints `version_output` for `--version`.
    fn fake_virtiofsd(dir: &Path, version_output: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("virtiofsd");
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s\\n' '{version_output}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn an_old_virtiofsd_is_refused_by_name_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_virtiofsd(dir.path(), "virtiofsd 1.10.1");

        let err = check_virtiofsd_version(fake.as_os_str()).await.unwrap_err();

        assert!(
            matches!(&err, Error::VirtiofsdTooOld { found, required }
                if found == "1.10.1" && required == "1.13.0"),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "virtiofsd 1.10.1 is too old: GAVMO needs 1.13.0 or newer \
             (for --translate-uid and --readonly)"
        );
    }

    #[tokio::test]
    async fn a_new_or_unrecognized_virtiofsd_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        for output in ["virtiofsd 1.13.0", "virtiofsd 1.14.0", "something else"] {
            let fake = fake_virtiofsd(dir.path(), output);
            check_virtiofsd_version(fake.as_os_str()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_missing_virtiofsd_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("virtiofsd");

        let err = check_virtiofsd_version(missing.as_os_str())
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Spawn { .. }), "{err:?}");
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn qemu_args_reference_the_sockets_virtiofsd_will_bind() {
        let spec = ShareSpec {
            host_path: PathBuf::from("/home/user/project"),
            mount_tag: "workspace",
            read_only: false,
        };
        let runtime_dir = PathBuf::from("/run/user/1000/gavmo/abc");

        let args = strings(spec.qemu_args(&runtime_dir));

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

        let args = strings(gcloud_spec.qemu_args(&runtime_dir));

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

        let args = strings(build_virtiofsd_args(&socket_path, &spec, 1001, 1001));

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

    /// A repo path that isn't UTF-8 reaches virtiofsd byte-for-byte; it used
    /// to go through `Path::display`, which swaps such bytes for U+FFFD and
    /// so names a different directory.
    #[test]
    fn virtiofsd_args_pass_non_utf8_paths_through_unchanged() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let repo = OsStr::from_bytes(b"/home/user/caf\xe9");
        let spec = ShareSpec {
            host_path: PathBuf::from(repo),
            mount_tag: "workspace",
            read_only: false,
        };
        let args = build_virtiofsd_args(Path::new("/run/s.sock"), &spec, 1000, 1000);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--shared-dir" && w[1].as_os_str() == repo),
            "{args:?}"
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

        let args = strings(build_virtiofsd_args(&socket_path, &spec, 4321, 100));

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

        let args = strings(build_virtiofsd_args(&socket_path, &spec, 1000, 1000));

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

        let args = strings(build_virtiofsd_args(&socket_path, &spec, 1000, 1000));

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

        let args = strings(build_virtiofsd_args(&socket_path, &spec, 1000, 1000));

        assert!(!args.iter().any(|a| a == "--readonly"));
    }
}
