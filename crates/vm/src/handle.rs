use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::process::Child;

use crate::config::VmConfig;
use crate::disk::ensure_overlay;
use crate::error::{Error, Result};
use crate::fsutil::{create_private_dir_all, remove_if_exists_async};
use crate::health::{probe_once, wait_for_ready};
use crate::qemu::spawn_qemu;
use crate::qmp;

/// How long to wait for `opencode web` to become reachable during boot.
/// Also covers the in-guest `gavmo-rebuild` of the repo's `gavmo.nix`,
/// which runs before `opencode web` starts.
const BOOT_HEALTH_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for a graceful ACPI shutdown before force-killing.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait after a forced `quit` before giving up and sending
/// SIGKILL directly.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the guest's balloon driver reports memory stats to qemu; the
/// control panel's Memory stat is only as fresh as this.
const BALLOON_STATS_INTERVAL_SECS: u32 = 2;

/// A booted VM's state, from [`RunningVm::status`]. A VM that isn't running
/// has no [`RunningVm`], so there is no "stopped" state here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmStatus {
    /// The qemu process is alive but `opencode web` isn't answering yet.
    Booting,
    /// `opencode web` is reachable at `url`.
    Running { url: String },
    /// The VM is gone; the daemon should treat it as dead.
    Failed(VmFailure),
}

/// Why a booted VM is [`VmStatus::Failed`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VmFailure {
    /// qemu exited (crashed, was killed, or the guest powered off).
    #[error("qemu exited ({0})")]
    QemuExited(std::process::ExitStatus),
    /// qemu's state couldn't be read, so it can't be trusted to be alive.
    #[error("failed to check the qemu process: {0}")]
    Unwaitable(String),
}

/// A booted VM: owns the qemu process, its `virtiofsd` helpers, and the
/// config it was booted with.
///
/// Dropping without [`RunningVm::stop`] still kills qemu (`kill_on_drop`),
/// but skips graceful shutdown and helper cleanup - prefer `stop()`.
pub struct RunningVm {
    cfg: VmConfig,
    qemu: Child,
    helpers: Vec<Child>,
    /// When qemu was spawned, not when `opencode web` first answered (which
    /// can be minutes later), so uptime matches what the boot console shows.
    started: Instant,
}

impl RunningVm {
    /// Boots a VM per `cfg` and waits for `opencode web` to answer on the
    /// forwarded host port.
    pub async fn boot(cfg: VmConfig) -> Result<Self> {
        // Owner-only, so other local users can't read the disk or reach the
        // sockets. (`ensure_overlay` does the same for `state_dir`.)
        create_private_dir_all(&cfg.runtime_dir)?;
        ensure_overlay(
            &cfg.base_image,
            &cfg.base_image_id,
            &cfg.disk_path(),
            &cfg.base_record_path(),
            cfg.disk_size_gib,
        )
        .await?;

        // Clear a stale rebuild-failure marker so a previous boot's result
        // isn't mistaken for this one's. NotFound is the common case; any
        // other failure must abort, or the stale marker would immediately
        // (and wrongly) fail this boot.
        remove_if_exists_async(&cfg.rebuild_failed_marker_path()).await?;

        let (qemu, helpers) = spawn_qemu(&cfg, &cfg.shares()).await?;
        let host_port = cfg.host_port;
        let marker_path = cfg.rebuild_failed_marker_path();
        let mut vm = RunningVm {
            cfg,
            qemu,
            helpers,
            started: Instant::now(),
        };

        // On `Err`, `vm` is dropped and `kill_on_drop` tears down qemu; the
        // rebuild log is already on disk in `guest_output_dir`.
        wait_ready_or_exit(
            &mut vm.qemu,
            &vm.cfg.qemu_log_path(),
            wait_for_ready(host_port, &marker_path, BOOT_HEALTH_TIMEOUT),
        )
        .await?;

        // Best-effort: without balloon stats the VM runs fine, it just shows
        // no live memory usage. Not worth failing a healthy boot over.
        if let Err(e) =
            qmp::set_balloon_stats_interval(&vm.cfg.qmp_socket_path(), BALLOON_STATS_INTERVAL_SECS)
                .await
        {
            tracing::warn!(
                workspace = %vm.cfg.workspace_id,
                error = %e,
                "could not enable balloon stats; guest memory usage will be unavailable"
            );
        }

        Ok(vm)
    }

    /// Requests a graceful shutdown via QMP (ACPI powerdown), falling back
    /// to a forced `quit` and then SIGKILL if the guest doesn't exit in
    /// time. The overlay disk is always preserved.
    ///
    /// Every helper is torn down even if an earlier step fails; the first
    /// failure to actually terminate a process is returned.
    pub async fn stop(mut self) -> Result<()> {
        let workspace = self.cfg.workspace_id.clone();
        let qmp_sock = self.cfg.qmp_socket_path();

        let mut shut_down_gracefully = false;
        if qmp_sock.exists() {
            match qmp::system_powerdown(&qmp_sock).await {
                Ok(()) => {
                    shut_down_gracefully = matches!(
                        tokio::time::timeout(SHUTDOWN_TIMEOUT, self.qemu.wait()).await,
                        Ok(Ok(_))
                    );
                    if !shut_down_gracefully {
                        tracing::warn!(%workspace, "guest did not power off in time; forcing quit");
                    }
                }
                Err(e) => {
                    tracing::warn!(%workspace, error = %e, "ACPI powerdown failed; forcing quit");
                }
            }
        }

        let mut result = Ok(());
        if !shut_down_gracefully {
            if let Err(e) = qmp::quit(&qmp_sock).await {
                tracing::warn!(%workspace, error = %e, "QMP quit failed; will SIGKILL qemu");
            }
            if tokio::time::timeout(QUIT_TIMEOUT, self.qemu.wait())
                .await
                .is_err()
            {
                tracing::warn!(%workspace, "qemu ignored quit; sending SIGKILL");
            }
            result = kill_and_reap(&mut self.qemu, "qemu-system-x86_64").await;
        }

        for mut helper in self.helpers.drain(..) {
            let helper_result = kill_and_reap(&mut helper, "virtiofsd").await;
            if let Err(e) = &helper_result {
                tracing::warn!(%workspace, error = %e, "failed to stop virtiofsd helper");
            }
            result = result.and(helper_result);
        }

        result
    }

    /// Checks whether the VM process is alive, and if so whether
    /// `opencode web` is currently answering.
    pub async fn status(&mut self) -> VmStatus {
        match self.qemu.try_wait() {
            Ok(Some(exit)) => {
                return VmStatus::Failed(VmFailure::QemuExited(exit));
            }
            Ok(None) => {}
            Err(e) => {
                return VmStatus::Failed(VmFailure::Unwaitable(e.to_string()));
            }
        }

        // A missing or unresponsive QMP socket just means early boot, not
        // a failure.
        let qmp_sock = self.cfg.qmp_socket_path();
        if qmp_sock.exists()
            && let Ok(info) = qmp::query_status(&qmp_sock).await
            && !info.running
        {
            return VmStatus::Booting;
        }

        if probe_once(self.cfg.host_port).await {
            VmStatus::Running {
                url: format!("http://127.0.0.1:{}", self.cfg.host_port),
            }
        } else {
            VmStatus::Booting
        }
    }

    /// The host port `opencode web` is forwarded to.
    pub fn host_port(&self) -> u16 {
        self.cfg.host_port
    }

    /// The workspace this VM belongs to.
    pub fn workspace_id(&self) -> &str {
        &self.cfg.workspace_id
    }

    /// How long ago this VM's qemu process was spawned (not when it first
    /// became healthy).
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// The host pid of the qemu process, for host-side resource stats.
    /// `None` if the process has already exited.
    pub fn qemu_pid(&self) -> Option<u32> {
        self.qemu.id()
    }

    /// Configured guest vCPU count.
    pub fn vcpus(&self) -> u32 {
        self.cfg.vcpus
    }

    /// Path to this VM's QMP control socket, for
    /// [`crate::qmp::guest_memory`].
    pub fn qmp_socket_path(&self) -> PathBuf {
        self.cfg.qmp_socket_path()
    }

    /// Path to this VM's persistent overlay disk.
    pub fn disk_path(&self) -> PathBuf {
        self.cfg.disk_path()
    }
}

/// Runs the readiness wait, but fails as soon as qemu exits - bad arguments,
/// a missing `/dev/kvm`, a locked or unopenable disk - with the tail of its
/// log, instead of waiting out the whole boot timeout.
async fn wait_ready_or_exit(
    qemu: &mut Child,
    log_path: &std::path::Path,
    ready: impl Future<Output = Result<()>>,
) -> Result<()> {
    tokio::select! {
        result = ready => result,
        exit = qemu.wait() => {
            let status = exit.map_err(|source| Error::Wait {
                program: "qemu-system-x86_64",
                source,
            })?;
            Err(Error::QemuExited {
                status,
                stderr_tail: crate::qemu::qemu_log_tail(log_path).await,
            })
        }
    }
}

/// SIGKILLs and reaps `child`, unless it has already exited (tokio's
/// `kill` errors on an already-reaped child, which is not a failure here).
async fn kill_and_reap(child: &mut Child, program: &'static str) -> Result<()> {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Ok(());
    }
    child
        .kill()
        .await
        .map_err(|source| Error::Kill { program, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    use crate::fake_qmp::{FakeQmp, error, ok};
    use serde_json::json;

    fn sleeper() -> Child {
        Command::new("sleep")
            .arg("600")
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn kill_and_reap_kills_a_live_child() {
        let mut child = sleeper();
        kill_and_reap(&mut child, "sleep").await.unwrap();
        assert!(child.try_wait().unwrap().is_some());
    }

    /// Killing a child that `wait()` already reaped must not be an error.
    #[tokio::test]
    async fn kill_and_reap_tolerates_an_already_reaped_child() {
        let mut child = Command::new("true").spawn().unwrap();
        child.wait().await.unwrap();
        kill_and_reap(&mut child, "true").await.unwrap();
    }

    /// A qemu that dies during boot fails the boot at once, with its stderr.
    #[tokio::test]
    async fn boot_wait_fails_fast_with_the_log_tail_when_qemu_exits() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("qemu.log");
        let file = std::fs::File::create(&log).unwrap();
        let mut child = Command::new("sh")
            .args(["-c", "echo 'qemu: could not open disk' >&2; exit 3"])
            .stderr(file)
            .spawn()
            .unwrap();

        let never_ready = std::future::pending::<Result<()>>();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            wait_ready_or_exit(&mut child, &log, never_ready),
        )
        .await
        .expect("must not wait for readiness once qemu has exited");

        match result {
            Err(Error::QemuExited {
                status,
                stderr_tail,
            }) => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stderr_tail, "qemu: could not open disk");
            }
            other => panic!("expected QemuExited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn boot_wait_returns_readiness_while_qemu_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = sleeper();
        let result =
            wait_ready_or_exit(&mut child, &dir.path().join("qemu.log"), async { Ok(()) }).await;
        assert!(result.is_ok());
        kill_and_reap(&mut child, "sleep").await.unwrap();
    }

    /// Real qemu + virtiofsd: an unopenable base image makes qemu exit, and
    /// `boot` reports qemu's own message within seconds.
    #[tokio::test]
    #[ignore = "needs qemu, virtiofsd and /dev/kvm"]
    async fn real_qemu_boot_failure_is_reported_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = VmConfig {
            base_image: tmp.path().join("missing-base.qcow2"),
            base_image_id: "test-base".to_owned(),
            ..real_cfg(tmp.path())
        };
        // A pre-existing overlay, so only qemu (not qemu-img) sees the
        // missing base.
        let status = std::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(cfg.disk_path())
            .arg("64M")
            .status()
            .unwrap();
        assert!(status.success());

        let started = Instant::now();
        let Err(err) = RunningVm::boot(cfg).await else {
            panic!("boot must fail");
        };
        assert!(started.elapsed() < Duration::from_secs(30), "{err}");
        assert!(matches!(err, Error::QemuExited { .. }), "{err}");
        assert!(err.to_string().contains("missing-base.qcow2"), "{err}");
    }
    /// A config under `tmp` whose host dirs exist, for real and fake VMs.
    fn real_cfg(tmp: &std::path::Path) -> VmConfig {
        let cfg = VmConfig {
            workspace_id: "t".to_owned(),
            repo_dir: tmp.join("repo"),
            state_dir: tmp.join("state"),
            runtime_dir: tmp.join("run"),
            guest_output_dir: tmp.join("repo/.gavmo/vm"),
            base_image: tmp.join("base.qcow2"),
            base_image_id: "test-base".to_owned(),
            gcloud_dir: tmp.join("gcloud"),
            mcp_dir: tmp.join("mcp"),
            disk_size_gib: 1,
            memory_mib: 256,
            vcpus: 1,
            host_port: free_port(),
        };
        for dir in [&cfg.repo_dir, &cfg.gcloud_dir, &cfg.mcp_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        create_private_dir_all(&cfg.state_dir).unwrap();
        create_private_dir_all(&cfg.runtime_dir).unwrap();
        cfg
    }

    /// A localhost port nothing listens on (just released by the kernel).
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A [`RunningVm`] around stand-in processes instead of qemu/virtiofsd.
    fn fake_vm(tmp: &std::path::Path, qemu: Child, helpers: Vec<Child>) -> RunningVm {
        RunningVm {
            cfg: real_cfg(tmp),
            qemu,
            helpers,
            started: Instant::now(),
        }
    }

    /// Answers every connection on `port` like a (very) minimal web server.
    async fn serve_http(port: u16) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                socket
                    .write_all(b"HTTP/1.0 200 OK\r\n\r\n")
                    .await
                    .unwrap_or_default();
            }
        });
    }

    /// Kills `pid` from outside, as the guest powering off would end qemu.
    fn kill(pid: u32) {
        let status = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn is_gone(pid: u32) -> bool {
        !std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    #[tokio::test]
    async fn status_is_failed_once_qemu_has_exited() {
        let tmp = tempfile::tempdir().unwrap();
        let mut qemu = Command::new("true").spawn().unwrap();
        qemu.wait().await.unwrap();
        let mut vm = fake_vm(tmp.path(), qemu, Vec::new());

        assert!(matches!(
            vm.status().await,
            VmStatus::Failed(VmFailure::QemuExited(exit)) if exit.success()
        ));
    }

    #[tokio::test]
    async fn status_is_booting_until_opencode_answers_then_running() {
        let tmp = tempfile::tempdir().unwrap();
        let mut vm = fake_vm(tmp.path(), sleeper(), Vec::new());
        assert_eq!(vm.status().await, VmStatus::Booting);

        serve_http(vm.host_port()).await;
        assert_eq!(
            vm.status().await,
            VmStatus::Running {
                url: format!("http://127.0.0.1:{}", vm.host_port())
            }
        );
    }

    /// qemu's own run state wins over a port that happens to answer: a VM
    /// still in `prelaunch` (or paused) isn't running.
    #[tokio::test]
    async fn status_follows_qemu_s_run_state_when_qmp_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let mut vm = fake_vm(tmp.path(), sleeper(), Vec::new());
        serve_http(vm.host_port()).await;
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&running);
        let _qmp = FakeQmp::serve(&vm.qmp_socket_path(), move |_, _| {
            let running = flag.load(std::sync::atomic::Ordering::SeqCst);
            ok(json!({"running": running, "status": if running { "running" } else { "prelaunch" }}))
        });

        assert_eq!(vm.status().await, VmStatus::Booting);
        running.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(vm.status().await, VmStatus::Running { .. }));
    }

    #[tokio::test]
    async fn accessors_report_the_config_and_the_qemu_process() {
        let tmp = tempfile::tempdir().unwrap();
        let qemu = sleeper();
        let pid = qemu.id();
        let vm = fake_vm(tmp.path(), qemu, Vec::new());

        assert_eq!(vm.workspace_id(), "t");
        assert_eq!(vm.vcpus(), 1);
        assert_eq!(vm.qemu_pid(), pid);
        assert!(vm.uptime() < Duration::from_secs(60));
        assert_eq!(vm.qmp_socket_path(), tmp.path().join("run/qmp.sock"));
        assert_eq!(vm.disk_path(), tmp.path().join("state/disk.qcow2"));
    }

    #[tokio::test]
    async fn stop_powers_off_gracefully_and_kills_the_helpers() {
        let tmp = tempfile::tempdir().unwrap();
        let qemu = sleeper();
        let qemu_pid = qemu.id().unwrap();
        let helper = sleeper();
        let helper_pid = helper.id().unwrap();
        let vm = fake_vm(tmp.path(), qemu, vec![helper]);
        let qmp = FakeQmp::serve(&vm.qmp_socket_path(), move |_, _| {
            kill(qemu_pid);
            ok(json!({}))
        });

        vm.stop().await.unwrap();

        assert_eq!(qmp.commands(), ["system_powerdown"]);
        assert!(is_gone(qemu_pid) && is_gone(helper_pid));
    }

    #[tokio::test]
    async fn stop_falls_back_to_quit_when_powerdown_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let qemu = sleeper();
        let qemu_pid = qemu.id().unwrap();
        let vm = fake_vm(tmp.path(), qemu, Vec::new());
        let qmp = FakeQmp::serve(&vm.qmp_socket_path(), move |name, _| {
            if name == "quit" {
                kill(qemu_pid);
                ok(json!({}))
            } else {
                error("ACPI unavailable")
            }
        });

        vm.stop().await.unwrap();

        assert_eq!(qmp.commands(), ["system_powerdown", "quit"]);
        assert!(is_gone(qemu_pid));
    }

    /// A guest that ignores the powerdown gets `quit` after the shutdown
    /// timeout. (Paused clock: the timeout passes at once.)
    #[tokio::test(start_paused = true)]
    async fn stop_forces_quit_when_the_guest_ignores_powerdown() {
        let tmp = tempfile::tempdir().unwrap();
        let qemu = sleeper();
        let qemu_pid = qemu.id().unwrap();
        let vm = fake_vm(tmp.path(), qemu, Vec::new());
        let qmp = FakeQmp::serve(&vm.qmp_socket_path(), move |name, _| {
            if name == "quit" {
                kill(qemu_pid);
            }
            ok(json!({}))
        });

        vm.stop().await.unwrap();

        assert_eq!(qmp.commands(), ["system_powerdown", "quit"]);
        assert!(is_gone(qemu_pid));
    }

    /// No QMP at all (qemu wedged before creating its socket): SIGKILL
    /// after the quit timeout, and still no error.
    #[tokio::test(start_paused = true)]
    async fn stop_sigkills_a_qemu_without_qmp() {
        let tmp = tempfile::tempdir().unwrap();
        let qemu = sleeper();
        let qemu_pid = qemu.id().unwrap();
        let vm = fake_vm(tmp.path(), qemu, Vec::new());

        vm.stop().await.unwrap();

        assert!(is_gone(qemu_pid));
    }

    /// Real qemu + virtiofsd booting a blank disk: the whole spawn path
    /// works, and a guest's failed `gavmo.nix` rebuild (its marker file)
    /// fails the boot with the log's name instead of waiting for opencode.
    #[tokio::test]
    #[ignore = "needs qemu, virtiofsd and /dev/kvm"]
    async fn real_qemu_boot_reports_a_failed_gavmo_nix_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = real_cfg(tmp.path());
        let status = std::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(&cfg.base_image)
            .arg("64M")
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::create_dir_all(&cfg.guest_output_dir).unwrap();
        // A stale marker from an earlier boot must not count...
        let marker = cfg.rebuild_failed_marker_path();
        std::fs::write(&marker, "rebuild-old.log").unwrap();
        // ...but one written once qemu is up does.
        let qemu_log = cfg.qemu_log_path();
        let writer = tokio::spawn(async move {
            while !qemu_log.exists() || marker.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            std::fs::write(&marker, "rebuild-new.log\n").unwrap();
        });

        let Err(err) = RunningVm::boot(cfg).await else {
            panic!("boot must fail");
        };
        writer.await.unwrap();
        assert!(
            matches!(&err, Error::RebuildFailed { log_file } if log_file == "rebuild-new.log"),
            "{err}"
        );
    }
}
