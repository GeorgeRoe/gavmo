//! Keeping VM processes from outliving the daemon.
//!
//! Two layers:
//! - [`die_with_parent`]: qemu and virtiofsd get `SIGTERM` from the kernel
//!   when the daemon goes away, however it goes away (crash, SIGKILL, OOM).
//! - [`reap_stale`]: on startup, processes recorded in pidfiles under a VM
//!   runtime dir that are somehow still alive are terminated - but only
//!   after checking they really are ours (same uid, and their command line
//!   names that runtime dir), so a recycled PID is never killed.
//!
//! This is the only module that needs `unsafe` (raw `prctl`/`kill`).

#![expect(
    unsafe_code,
    reason = "prctl(PR_SET_PDEATHSIG) and kill(2) have no safe std API"
)]

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Signal the kernel sends a VM process when the daemon dies. `SIGTERM`
/// rather than `SIGKILL`: qemu then exits cleanly, flushing the disk image.
const DEATH_SIGNAL: libc::c_int = libc::SIGTERM;

/// Arranges for the spawned process to receive [`DEATH_SIGNAL`] when its
/// parent goes away.
///
/// Linux delivers this when the *thread* that spawned the child exits, not
/// the process. Children are spawned from tokio worker threads, which live
/// as long as the runtime, i.e. the daemon.
pub(crate) fn die_with_parent(command: &mut tokio::process::Command) {
    let parent = std::process::id();
    let in_child = move || {
        // SAFETY: plain syscall with integer arguments.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, DEATH_SIGNAL) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The parent may have died before the prctl took effect; then
        // no signal will ever come, so don't start at all.
        // SAFETY: plain syscall, no arguments.
        if u32::try_from(unsafe { libc::getppid() }).ok() != Some(parent) {
            return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
        }
        Ok(())
    };
    // SAFETY: `in_child` runs in the child between fork and exec, where only
    // async-signal-safe functions may be called. It calls only `prctl` and
    // `getppid` (async-signal-safe syscalls), allocates nothing and takes no
    // locks: `io::Error::last_os_error` and `from_raw_os_error` only wrap an
    // integer.
    unsafe {
        command.pre_exec(in_child);
    }
}

/// How long a stale process gets to exit after `SIGTERM` before `SIGKILL`.
const STALE_TERM_GRACE: Duration = Duration::from_secs(5);

/// Terminates processes left over from an earlier daemon in `runtime_dir`:
/// every `*.pid` file there whose PID is alive, owned by us, and has
/// `runtime_dir` in its command line. Returns the PIDs it signalled.
/// Pidfiles are removed. Best effort; problems are logged.
pub fn reap_stale(runtime_dir: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut reaped = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "pid") {
            continue;
        }
        if let Some(pid) = read_pid(&path)
            && is_our_vm_process(pid, runtime_dir)
        {
            tracing::warn!(
                pid,
                pidfile = %path.display(),
                "terminating a VM process left over from a previous daemon"
            );
            terminate(pid);
            reaped.push(pid);
        }
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(pidfile = %path.display(), error = %e, "could not remove stale pidfile");
        }
    }
    reaped
}

/// Pidfile path for qemu in a runtime dir (qemu's `-pidfile`).
pub(crate) fn qemu_pidfile(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("qemu.pid")
}

fn read_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    // 0 and 1 are never ours; refusing them also keeps `kill` from ever
    // targeting a process group.
    (pid > 1).then_some(pid)
}

/// Same uid as us, and `runtime_dir` appears in its command line (every VM
/// process is started with its socket paths inside it).
fn is_our_vm_process(pid: u32, runtime_dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let (Ok(theirs), Ok(ours)) = (
        std::fs::metadata(&proc_dir),
        std::fs::metadata("/proc/self"),
    ) else {
        return false;
    };
    if theirs.uid() != ours.uid() {
        return false;
    }
    let Ok(cmdline) = std::fs::read(proc_dir.join("cmdline")) else {
        return false;
    };
    let needle = runtime_dir.as_os_str().as_bytes();
    !needle.is_empty() && cmdline.windows(needle.len()).any(|w| w == needle)
}

/// `SIGTERM`, then `SIGKILL` if it hasn't exited within the grace period.
fn terminate(pid: u32) {
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return;
    };
    send(raw, libc::SIGTERM);
    let deadline = Instant::now() + STALE_TERM_GRACE;
    while Instant::now() < deadline {
        if !alive(raw) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    send(raw, libc::SIGKILL);
}

fn send(pid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: plain syscall; `pid` > 1 (see `read_pid`), so it names a
    // single process, never a group or "all processes".
    unsafe {
        libc::kill(pid, signal);
    }
}

/// Exists and is not a zombie.
fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks for existence/permission.
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    exists
        && std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
            })
            .is_some_and(|state| state != "Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid_state(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, rest) = stat.rsplit_once(')')?;
        rest.split_whitespace().next().map(str::to_owned)
    }

    fn wait_until_dead(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if pid_state(pid).is_none_or(|s| s == "Z") {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// The child is signalled when the thread that spawned it exits, the
    /// same mechanism that fires when the daemon dies.
    #[test]
    fn children_die_with_their_spawning_thread() {
        let (pid_tx, pid_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut command = tokio::process::Command::new("sleep");
                command.arg("600");
                die_with_parent(&mut command);
                let child = command.spawn().unwrap();
                pid_tx.send(child.id().unwrap()).unwrap();
                // Not killed on drop: only the death signal can stop it.
                drop(child);
            });
        })
        .join()
        .unwrap();
        let pid = pid_rx.recv().unwrap();
        assert!(wait_until_dead(pid), "child outlived its parent thread");
    }

    #[test]
    fn reap_stale_kills_only_processes_naming_the_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("run");
        std::fs::create_dir(&runtime).unwrap();

        // "Ours": its command line mentions the runtime dir.
        let mut ours = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 600; : {}", runtime.display()))
            .spawn()
            .unwrap();
        // Not ours: a recycled PID would look like this.
        let mut other = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap();
        std::fs::write(runtime.join("qemu.pid"), format!("{}\n", ours.id())).unwrap();
        std::fs::write(runtime.join("virtiofs-x.sock.pid"), other.id().to_string()).unwrap();
        std::fs::write(runtime.join("garbage.pid"), "1").unwrap();

        let reaped = reap_stale(&runtime);
        assert_eq!(reaped, [ours.id()]);
        assert!(ours.wait().unwrap().code().is_none(), "killed by a signal");
        assert!(
            other.try_wait().unwrap().is_none(),
            "the other process survives"
        );
        assert!(
            std::fs::read_dir(&runtime).unwrap().next().is_none(),
            "pidfiles removed"
        );

        other.kill().unwrap();
        other.wait().unwrap();
    }
}
