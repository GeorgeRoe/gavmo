//! The registry's view of a VM: a small trait seam over [`vm::RunningVm`] so
//! the lifecycle logic (races, revocation, dead-VM handling) can be tested
//! without qemu.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use vm::{RunningVm, VmConfig, VmStatus};

/// A boxed, `Send` future, as trait objects can't have `async fn`s.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the stats panel needs to know about a running VM.
#[derive(Debug, Clone)]
pub(crate) struct VmInfo {
    /// `None` once qemu has exited.
    pub(crate) qemu_pid: Option<u32>,
    pub(crate) uptime: Duration,
    pub(crate) vcpus: u32,
    pub(crate) host_port: u16,
    pub(crate) qmp_socket_path: PathBuf,
}

/// A booted VM.
pub(crate) trait VmHandle: Send + Sync {
    /// Shuts the VM down (gracefully if possible) and reaps its processes.
    fn stop(self: Box<Self>) -> BoxFuture<'static, vm::Result<()>>;
    /// Liveness and health right now. Slow (QMP + HTTP probe); bounded by
    /// the `vm` crate's timeouts.
    fn status(&mut self) -> BoxFuture<'_, VmStatus>;
    fn info(&self) -> VmInfo;
}

/// Boots VMs.
pub(crate) trait Launcher: Send + Sync {
    fn boot(&self, cfg: VmConfig) -> BoxFuture<'static, vm::Result<Box<dyn VmHandle>>>;
}

impl VmHandle for RunningVm {
    fn stop(self: Box<Self>) -> BoxFuture<'static, vm::Result<()>> {
        Box::pin(RunningVm::stop(*self))
    }

    fn status(&mut self) -> BoxFuture<'_, VmStatus> {
        Box::pin(RunningVm::status(self))
    }

    fn info(&self) -> VmInfo {
        VmInfo {
            qemu_pid: self.qemu_pid(),
            uptime: self.uptime(),
            vcpus: self.vcpus(),
            host_port: self.host_port(),
            qmp_socket_path: self.qmp_socket_path(),
        }
    }
}

/// The real launcher: qemu via [`RunningVm::boot`].
pub(crate) struct QemuLauncher;

impl Launcher for QemuLauncher {
    fn boot(&self, cfg: VmConfig) -> BoxFuture<'static, vm::Result<Box<dyn VmHandle>>> {
        Box::pin(async move {
            let vm: Box<dyn VmHandle> = Box::new(RunningVm::boot(cfg).await?);
            Ok(vm)
        })
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scriptable launcher for lifecycle tests.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::Semaphore;

    use super::{BoxFuture, Launcher, VmHandle, VmInfo};
    use vm::{VmConfig, VmStatus};

    /// Shared controls and counters for [`FakeLauncher`] and its VMs.
    pub(crate) struct Script {
        /// Boots block until a permit is added (`release_boots`).
        pub(crate) boot_gate: Semaphore,
        /// Stops block until a permit is added (`release_stops`), unless
        /// `stops_block` is false.
        pub(crate) stop_gate: Semaphore,
        pub(crate) stops_block: std::sync::atomic::AtomicBool,
        /// When set, `status()` never returns (a wedged qemu).
        pub(crate) status_hangs: std::sync::atomic::AtomicBool,
        pub(crate) boots_started: AtomicUsize,
        pub(crate) stops: AtomicUsize,
        /// What every fake VM's `status()` reports.
        pub(crate) status: Mutex<Option<VmStatus>>,
        /// The config of every boot, in order.
        pub(crate) booted: Mutex<Vec<VmConfig>>,
    }

    impl Script {
        pub(crate) fn new() -> Arc<Self> {
            let script = Arc::new(Self {
                boot_gate: Semaphore::new(0),
                stop_gate: Semaphore::new(0),
                stops_block: std::sync::atomic::AtomicBool::new(false),
                status_hangs: std::sync::atomic::AtomicBool::new(false),
                boots_started: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
                status: Mutex::new(None),
                booted: Mutex::new(Vec::new()),
            });
            script.set_status(VmStatus::Running {
                url: "http://127.0.0.1:1".to_owned(),
            });
            script
        }

        pub(crate) fn release_boots(&self, n: usize) {
            self.boot_gate.add_permits(n);
        }

        pub(crate) fn set_status(&self, status: VmStatus) {
            *self.status.lock().unwrap() = Some(status);
        }

        pub(crate) fn boots(&self) -> usize {
            self.boots_started.load(Ordering::SeqCst)
        }

        pub(crate) fn stops(&self) -> usize {
            self.stops.load(Ordering::SeqCst)
        }
    }

    pub(crate) struct FakeLauncher(pub(crate) Arc<Script>);

    impl Launcher for FakeLauncher {
        fn boot(&self, cfg: VmConfig) -> BoxFuture<'static, vm::Result<Box<dyn VmHandle>>> {
            let script = Arc::clone(&self.0);
            Box::pin(async move {
                script.boots_started.fetch_add(1, Ordering::SeqCst);
                script.booted.lock().unwrap().push(cfg.clone());
                script.boot_gate.acquire().await.unwrap().forget();
                let vm: Box<dyn VmHandle> = Box::new(FakeVm { script, cfg });
                Ok(vm)
            })
        }
    }

    struct FakeVm {
        script: Arc<Script>,
        cfg: VmConfig,
    }

    impl VmHandle for FakeVm {
        fn stop(self: Box<Self>) -> BoxFuture<'static, vm::Result<()>> {
            Box::pin(async move {
                if self.script.stops_block.load(Ordering::SeqCst) {
                    self.script.stop_gate.acquire().await.unwrap().forget();
                }
                self.script.stops.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }

        fn status(&mut self) -> BoxFuture<'_, VmStatus> {
            let status = self.script.status.lock().unwrap().clone();
            let hangs = self.script.status_hangs.load(Ordering::SeqCst);
            Box::pin(async move {
                if hangs {
                    std::future::pending::<()>().await;
                }
                status.unwrap_or(VmStatus::Booting)
            })
        }

        fn info(&self) -> VmInfo {
            VmInfo {
                qemu_pid: None,
                uptime: std::time::Duration::ZERO,
                vcpus: self.cfg.vcpus,
                host_port: self.cfg.host_port,
                qmp_socket_path: self.cfg.qmp_socket_path(),
            }
        }
    }
}
