use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::Path;
use std::process::Stdio;

use tokio::process::Child;

use crate::config::{GUEST_OPENCODE_PORT, VmConfig};
use crate::error::{Error, Result};
use crate::fsutil::{check_socket_path, remove_if_exists};
use crate::share::ShareSpec;

/// The `id=` of the `virtio-balloon-pci` device. `qmp.rs` derives the
/// balloon's QOM path (`/machine/peripheral/<id>`) from this.
pub(crate) const BALLOON_DEVICE_ID: &str = "balloon0";

/// Formats `path` for a qemu `key=value,...` option string: commas are
/// doubled (qemu's escape). Byte-wise, so any path works unchanged,
/// including non-UTF-8 ones.
pub(crate) fn qemu_opt_path(path: &Path) -> OsString {
    let mut escaped = Vec::new();
    for &byte in path.as_os_str().as_bytes() {
        escaped.push(byte);
        if byte == b',' {
            escaped.push(b',');
        }
    }
    OsString::from_vec(escaped)
}

/// `before`, then `path` escaped as an option value, then `after`.
pub(crate) fn opt_with_path(before: &str, path: &Path, after: &str) -> OsString {
    let mut opt = OsString::from(before);
    opt.push(qemu_opt_path(path));
    opt.push(after);
    opt
}

/// A path embedded in JSON (the `-blockdev` nodes), which can only hold
/// UTF-8: anything else is refused rather than silently mangled. Only the
/// daemon's own disk paths go through here, never the repo's.
fn utf8_path(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| Error::UnsupportedPath {
        path: path.to_path_buf(),
        reason: "is not valid UTF-8",
    })
}

/// The disk as an explicit block graph: the overlay's backing is the
/// configured base image, named here, so qemu never follows the backing file
/// recorded in the overlay's header. The base itself is opened read-only
/// with no backing at all.
fn disk_args(cfg: &VmConfig) -> Result<Vec<String>> {
    let nodes = [
        serde_json::json!({
            "driver": "file",
            "node-name": "base-file",
            "filename": utf8_path(&cfg.base_image)?,
            "read-only": true,
        }),
        serde_json::json!({
            "driver": "qcow2",
            "node-name": "base",
            "file": "base-file",
            "read-only": true,
            "backing": null,
        }),
        serde_json::json!({
            "driver": "file",
            "node-name": "disk-file",
            "filename": utf8_path(&cfg.disk_path())?,
        }),
        serde_json::json!({
            "driver": "qcow2",
            "node-name": "disk",
            "file": "disk-file",
            "backing": "base",
        }),
    ];
    let mut args = Vec::new();
    for node in nodes {
        args.push("-blockdev".to_owned());
        args.push(node.to_string());
    }
    args.push("-device".to_owned());
    args.push("virtio-blk-pci,drive=disk".to_owned());
    Ok(args)
}

/// Builds the full `qemu-system-x86_64` argument list for a VM boot. Pure,
/// for unit testing without `qemu` or `/dev/kvm`.
pub(crate) fn build_qemu_args(cfg: &VmConfig, shares: &[ShareSpec]) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |arg: &dyn AsRef<std::ffi::OsStr>| args.push(arg.as_ref().to_owned());

    // Hosts are Linux-only (virtiofs); the `cfg` just keeps non-Linux
    // development builds compiling.
    #[cfg(target_os = "linux")]
    for arg in ["-enable-kvm", "-cpu", "host"] {
        push(&arg);
    }

    push(&"-smp");
    push(&cfg.vcpus.to_string());
    push(&"-m");
    push(&cfg.memory_mib.to_string());

    // virtiofs needs guest RAM in a shared memory backend so `virtiofsd`
    // can map it; `-numa node,memdev=mem` is how QEMU wires guest RAM to
    // that backend, even for a single-node guest.
    if !shares.is_empty() {
        push(&"-object");
        push(&format!(
            "memory-backend-memfd,id=mem,size={mem}M,share=on",
            mem = cfg.memory_mib,
        ));
        push(&"-numa");
        push(&"node,memdev=mem");
    }

    for arg in disk_args(cfg)? {
        push(&arg);
    }

    push(&"-netdev");
    push(&format!(
        "user,id=net0,hostfwd=tcp:127.0.0.1:{host_port}-:{guest_port}",
        host_port = cfg.host_port,
        guest_port = GUEST_OPENCODE_PORT,
    ));
    push(&"-device");
    push(&"virtio-net-pci,netdev=net0");

    // The balloon is never inflated; it exists only so the guest reports
    // its own memory usage over QMP (host RSS doesn't reflect guest usage).
    // `deflate-on-oom=on` is a free safety net.
    push(&"-device");
    push(&format!(
        "virtio-balloon-pci,id={BALLOON_DEVICE_ID},deflate-on-oom=on"
    ));

    for share in shares {
        for arg in share.qemu_args(&cfg.runtime_dir) {
            push(&arg);
        }
    }

    // QMP on a unix socket; no human monitor anywhere (in particular not on
    // the daemon's stdio, where `-nographic` would put it).
    // `-qmp` rather than `-chardev` + `-mon`: `-mon` is deprecated in recent
    // qemu and its `monitor-qmp` replacement is missing from older ones.
    push(&"-qmp");
    push(&opt_with_path(
        "unix:",
        &cfg.qmp_socket_path(),
        ",server=on,wait=off",
    ));
    for arg in ["-monitor", "none", "-display", "none"] {
        push(&arg);
    }

    // Lets a later daemon find and stop this qemu if it is ever orphaned
    // (see `process::reap_stale`). A plain path argument, not an option
    // string, so no comma escaping.
    push(&"-pidfile");
    push(&crate::process::qemu_pidfile(&cfg.runtime_dir));

    push(&"-chardev");
    push(&opt_with_path(
        "file,id=serial0,path=",
        &cfg.console_log_path(),
        "",
    ));
    push(&"-serial");
    push(&"chardev:serial0");

    Ok(args)
}

/// Spawns `qemu-system-x86_64`, first starting any share helper processes
/// (`virtiofsd`). The caller is responsible for cleaning up all returned
/// children.
pub(crate) async fn spawn_qemu(
    cfg: &VmConfig,
    shares: &[ShareSpec],
) -> Result<(Child, Vec<Child>)> {
    // Validate everything before starting any helper, so a bad path doesn't
    // leave virtiofsd processes behind.
    let args = build_qemu_args(cfg, shares)?;
    let qmp_socket = cfg.qmp_socket_path();
    check_socket_path(&qmp_socket)?;
    // A stale socket from a crashed run would make qemu's bind fail.
    remove_if_exists(&qmp_socket)?;

    let mut helpers = Vec::new();
    for share in shares {
        helpers.push(share.start_virtiofsd(&cfg.runtime_dir)?);
    }

    // qemu connects to each helper's socket at startup and fails outright if
    // it isn't there yet. On error the helpers are dropped (`kill_on_drop`).
    for share in shares {
        wait_for_path(&share.socket_path(&cfg.runtime_dir), &mut helpers).await?;
    }

    let log = open_qemu_log(&cfg.qemu_log_path())?;
    let log_err = |source| Error::QemuLog {
        path: cfg.qemu_log_path(),
        source,
    };
    let mut command = tokio::process::Command::new("qemu-system-x86_64");
    // Dies with the daemon instead of running on unmanaged.
    crate::process::die_with_parent(&mut command);
    let child = command
        .args(&args)
        // Nothing may read the daemon's terminal.
        .stdin(Stdio::null())
        // Per-VM log instead of the daemon's stdio, so an early exit can be
        // explained (see `Error::QemuExited`).
        .stdout(log.try_clone().map_err(log_err)?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| Error::Spawn {
            program: "qemu-system-x86_64".to_owned(),
            source,
        })?;

    Ok((child, helpers))
}

/// Opens (truncating) qemu's log in the host-only state dir: `0600`, never
/// following a symlink.
fn open_qemu_log(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| Error::QemuLog {
            path: path.to_path_buf(),
            source,
        })
}

/// The last few KiB of qemu's log, trimmed, for error messages. Empty if
/// unreadable.
pub(crate) async fn qemu_log_tail(path: &Path) -> String {
    const TAIL: u64 = 2048;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .ok()?;
        let len = file.metadata().ok()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
        let mut bytes = Vec::new();
        file.take(TAIL).read_to_end(&mut bytes).ok()?;
        Some(String::from_utf8_lossy(&bytes).trim().to_owned())
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

/// How long a share helper may take to create its socket.
const HELPER_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Waits for `path` to appear, failing early if any helper exits.
async fn wait_for_path(path: &Path, helpers: &mut [Child]) -> Result<()> {
    let deadline = tokio::time::Instant::now() + HELPER_READY_TIMEOUT;
    loop {
        if tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(());
        }
        for helper in helpers.iter_mut() {
            if let Ok(Some(status)) = helper.try_wait() {
                return Err(Error::HelperExited { status });
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::HelperNotReady {
                path: path.to_path_buf(),
                timeout: HELPER_READY_TIMEOUT,
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arguments as strings (the test paths are UTF-8).
    fn str_args(cfg: &VmConfig, shares: &[ShareSpec]) -> Vec<String> {
        build_qemu_args(cfg, shares)
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }
    use std::path::PathBuf;

    fn test_cfg() -> VmConfig {
        VmConfig {
            workspace_id: "test-workspace".to_string(),
            repo_dir: PathBuf::from("/home/user/project"),
            state_dir: PathBuf::from("/home/user/.local/share/gavmo/vms/abc"),
            runtime_dir: PathBuf::from("/run/user/1000/gavmo/abc"),
            guest_output_dir: PathBuf::from("/home/user/project/.gavmo/vm"),
            base_image: PathBuf::from("/var/lib/gavmo/base.qcow2"),
            gcloud_dir: PathBuf::from("/home/user/.config/gcloud"),
            mcp_dir: PathBuf::from("/var/lib/gavmo/mcp/test-workspace"),
            disk_size_gib: 32,
            memory_mib: 4096,
            vcpus: 4,
            host_port: 45123,
        }
    }

    #[test]
    fn args_include_forwarded_port_and_guest_port() {
        let cfg = test_cfg();
        let args = str_args(&cfg, &[]);

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
        let args = str_args(&cfg, &[]);

        assert!(args.windows(2).any(|w| w[0] == "-blockdev"
            && w[1].contains(r#""filename":"/home/user/.local/share/gavmo/vms/abc/disk.qcow2""#)));
        assert!(args.windows(2).any(|w| w[0] == "-qmp"
            && w[1] == "unix:/run/user/1000/gavmo/abc/qmp.sock,server=on,wait=off"));
        assert!(args.windows(2).any(|w| w[0] == "-chardev"
            && w[1] == "file,id=serial0,path=/home/user/.local/share/gavmo/vms/abc/console.log"));
    }

    /// Nothing host processes open may live in the guest-writable repo.
    #[test]
    fn no_host_opened_file_is_under_the_repo() {
        let cfg = test_cfg();
        let shares = cfg.shares();
        let args = str_args(&cfg, &shares);
        assert!(
            !args.iter().any(|a| a.contains("/home/user/project")),
            "{args:?}"
        );
    }

    /// The overlay's backing is pinned to the configured base image, so a
    /// tampered qcow2 header can't make qemu open another host file.
    #[test]
    fn disk_backing_is_explicit_not_read_from_the_header() {
        let args = str_args(&test_cfg(), &[]);
        let nodes: Vec<serde_json::Value> = args
            .windows(2)
            .filter(|w| w[0] == "-blockdev")
            .map(|w| serde_json::from_str(&w[1]).unwrap())
            .collect();
        let node = |name: &str| {
            nodes
                .iter()
                .find(|n| n["node-name"] == name)
                .unwrap_or_else(|| panic!("no node {name}"))
        };
        assert_eq!(node("disk")["backing"], "base");
        assert_eq!(node("base")["backing"], serde_json::Value::Null);
        assert_eq!(node("base")["read-only"], true);
        assert_eq!(node("base-file")["filename"], "/var/lib/gavmo/base.qcow2");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-device" && w[1] == "virtio-blk-pci,drive=disk")
        );
        assert!(!args.iter().any(|a| a == "-drive"));
    }

    #[test]
    fn no_human_monitor_and_no_stdio_display() {
        let args = str_args(&test_cfg(), &[]);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-monitor" && w[1] == "none")
        );
        assert!(!args.iter().any(|a| a == "-nographic"));
    }

    #[test]
    fn option_paths_escape_commas_bytewise() {
        assert_eq!(qemu_opt_path(Path::new("/a,b/c,,d")), "/a,,b/c,,,,d");
        let mut cfg = test_cfg();
        cfg.state_dir = PathBuf::from("/x,file.locking=off");
        let args = str_args(&cfg, &[]);
        assert!(
            args.iter()
                .any(|a| a == "file,id=serial0,path=/x,,file.locking=off/console.log")
        );
    }

    /// Option strings and plain path arguments carry any path unchanged;
    /// only the `-blockdev` JSON (the daemon's own disk paths) needs UTF-8,
    /// and refuses rather than mangles anything else.
    #[test]
    fn non_utf8_paths_pass_through_except_into_json() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let mut cfg = test_cfg();
        cfg.runtime_dir = PathBuf::from(OsStr::from_bytes(b"/run/b,ad\xff"));
        let args = build_qemu_args(&cfg, &cfg.shares()).unwrap();
        let has = |bytes: &[u8]| args.iter().any(|a| a.as_bytes() == bytes);
        assert!(has(b"unix:/run/b,,ad\xff/qmp.sock,server=on,wait=off"));
        assert!(
            has(b"/run/b,ad\xff/qemu.pid"),
            "the pidfile is a plain argument"
        );
        assert!(has(
            b"socket,id=char-mcp,path=/run/b,,ad\xff/virtiofs-mcp.sock"
        ));

        let mut cfg = test_cfg();
        cfg.state_dir = PathBuf::from(OsStr::from_bytes(b"/bad\xff"));
        assert!(matches!(
            build_qemu_args(&cfg, &[]),
            Err(Error::UnsupportedPath { .. })
        ));
    }

    /// Only the repo is writable from the guest; the gcloud credentials and
    /// the MCP token are read-only.
    #[test]
    fn shares_are_the_repo_read_write_and_the_rest_read_only() {
        let cfg = test_cfg();
        let shares: Vec<_> = cfg
            .shares()
            .into_iter()
            .map(|s| (s.mount_tag, s.host_path, s.read_only))
            .collect();
        assert_eq!(
            shares,
            [
                ("workspace", cfg.repo_dir.clone(), false),
                ("gcloud", cfg.gcloud_dir.clone(), true),
                ("mcp", cfg.mcp_dir.clone(), true),
            ]
        );
    }

    /// The shares `RunningVm::boot` uses ([`VmConfig::shares`]), with the
    /// tags the base image mounts.
    #[test]
    fn args_include_each_share() {
        let cfg = test_cfg();
        let shares = cfg.shares();

        let args = str_args(&cfg, &shares);

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
        let shares = cfg.shares();

        let args = str_args(&cfg, &shares);

        assert!(
            args.iter()
                .any(|a| a == "memory-backend-memfd,id=mem,size=4096M,share=on")
        );
        assert!(args.iter().any(|a| a == "node,memdev=mem"));
    }

    #[test]
    fn args_omit_shared_memory_backend_when_there_are_no_shares() {
        let cfg = test_cfg();

        let args = str_args(&cfg, &[]);

        assert!(!args.iter().any(|a| a.contains("memory-backend-memfd")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kvm_acceleration_is_enabled_on_linux() {
        let cfg = test_cfg();
        let args = str_args(&cfg, &[]);

        assert!(args.contains(&"-enable-kvm".to_string()));
    }

    /// The balloon is the memory stats channel, independent of virtiofs.
    #[test]
    fn args_include_the_balloon_device_for_guest_memory_stats() {
        let cfg = test_cfg();
        let args = str_args(&cfg, &[]);

        assert!(
            args.iter()
                .any(|a| a
                    == &format!("virtio-balloon-pci,id={BALLOON_DEVICE_ID},deflate-on-oom=on"))
        );
    }

    /// Real qemu + virtiofsd: the argument list is accepted, the helpers'
    /// sockets are awaited, QMP answers, and the overlay header's backing
    /// file is ignored. Needs qemu, virtiofsd and /dev/kvm, so it is opt-in:
    /// `cargo test -p vm -- --ignored real_qemu`.
    #[tokio::test]
    #[ignore = "needs qemu, virtiofsd and /dev/kvm"]
    async fn real_qemu_boots_with_explicit_backing_and_ignores_the_header() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base,with,commas.qcow2");
        let run = |args: &[&std::ffi::OsStr]| {
            let status = std::process::Command::new("qemu-img")
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&[
            "create".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            base.as_os_str(),
            "64M".as_ref(),
        ]);

        let mut cfg = test_cfg();
        cfg.repo_dir = tmp.path().join("repo");
        cfg.state_dir = tmp.path().join("state");
        cfg.runtime_dir = tmp.path().join("run");
        cfg.guest_output_dir = cfg.repo_dir.join(".gavmo").join("vm");
        cfg.gcloud_dir = tmp.path().join("gcloud");
        cfg.mcp_dir = tmp.path().join("mcp");
        cfg.base_image = base;
        cfg.memory_mib = 256;
        cfg.vcpus = 1;
        for dir in [&cfg.repo_dir, &cfg.gcloud_dir, &cfg.mcp_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        crate::fsutil::create_private_dir_all(&cfg.runtime_dir).unwrap();
        crate::fsutil::create_private_dir_all(&cfg.state_dir).unwrap();

        // An overlay whose *header* names a backing file that doesn't exist.
        // If qemu honoured the header it would fail to open the disk.
        let missing = tmp.path().join("decoy-does-not-exist.qcow2");
        run(&[
            "create".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            "-F".as_ref(),
            "qcow2".as_ref(),
            "-u".as_ref(),
            "-b".as_ref(),
            missing.as_os_str(),
            cfg.disk_path().as_os_str(),
            "64M".as_ref(),
        ]);

        let shares = cfg.shares();
        let (mut qemu, mut helpers) = spawn_qemu(&cfg, &shares).await.unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let status = loop {
            if let Some(exit) = qemu.try_wait().unwrap() {
                panic!("qemu exited early: {exit}");
            }
            if let Ok(status) = crate::qmp::query_status(&cfg.qmp_socket_path()).await {
                break status;
            }
            assert!(tokio::time::Instant::now() < deadline, "QMP never answered");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        };
        assert!(status.running, "{status:?}");
        assert!(
            cfg.console_log_path().exists(),
            "serial chardev file created"
        );

        crate::qmp::quit(&cfg.qmp_socket_path()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), qemu.wait())
            .await
            .unwrap()
            .unwrap();
        for helper in &mut helpers {
            helper.kill().await.unwrap();
        }
    }
}
