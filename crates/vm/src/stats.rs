//! Host-side resource usage for a running VM, read from `/proc` on the
//! host (not the guest — this is "how much of the host machine is this
//! qemu process using", the same thing `top`/`htop` would show for its
//! pid). Linux-only, consistent with the rest of this crate's KVM/`/proc`
//! assumptions (see `qemu.rs`'s `#[cfg(target_os = "linux")]` KVM gating).
//!
//! Parsing is kept as small, pure functions (`parse_*`) so they're
//! unit-testable against fixture strings without a real `/proc` or a real
//! qemu process — matching this crate's existing testing convention (see
//! `plan.md` §8: pure functions in the sidecar, real boots only on real
//! hardware).

use std::io;
use std::path::Path;

/// One point-in-time reading of a process's cumulative CPU time plus the
/// host's cumulative CPU time, used in pairs (see [`cpu_percent_per_core`])
/// to compute a CPU percentage over the interval between two samples.
/// Deliberately **not** interpreted as "CPU time in seconds" on its own —
/// `proc_jiffies`/`total_jiffies` are in whatever units the kernel's
/// `/proc` scheduler-tick counters use (conventionally 100/s, but never
/// actually divided by that rate here — see [`cpu_percent_per_core`]'s doc
/// comment for why that constant is never needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcSample {
    /// This process's own (utime + stime), in kernel scheduler-tick units.
    proc_jiffies: u64,
    /// The host's total CPU time across all cores, in the same units.
    total_jiffies: u64,
    /// Number of host CPUs, from counting `cpuN` lines in `/proc/stat`.
    ncpu: u32,
    /// Resident set size, in bytes (`VmRSS` from `/proc/<pid>/status`).
    pub rss_bytes: u64,
}

/// Reads one [`ProcSample`] for `pid` from `/proc`. Returns an error if the
/// process doesn't exist (including "exited between the caller looking it
/// up and this call") or `/proc` is unparseable — callers on a localhost
/// control panel are expected to treat either as "no stats this tick"
/// rather than a hard failure (see `registry::AppState::vm_stats`).
pub fn sample(pid: u32) -> io::Result<ProcSample> {
    let proc_jiffies = parse_pid_stat_jiffies(&std::fs::read_to_string(format!(
        "/proc/{pid}/stat"
    ))?)?;
    let (total_jiffies, ncpu) = parse_stat_total_and_ncpu(&std::fs::read_to_string("/proc/stat")?)?;
    let rss_bytes = parse_vmrss_bytes(&std::fs::read_to_string(format!(
        "/proc/{pid}/status"
    ))?)?;

    Ok(ProcSample {
        proc_jiffies,
        total_jiffies,
        ncpu,
        rss_bytes,
    })
}

/// Top-style CPU percentage over the interval between `prev` and `cur`
/// (both sampled for the same pid, `prev` taken first): `0` means idle,
/// `100` means one core fully busy, up to `ncpu * 100` if every core is
/// saturated by this one process.
///
/// Computed as `(Δproc_jiffies / Δtotal_jiffies) * ncpu * 100` — note this
/// never needs the kernel's actual tick rate (`sysconf(_SC_CLK_TCK)`,
/// conventionally but not guaranteed to be 100): both deltas are in the
/// same unknown-but-shared tick unit, which cancels out in the ratio.
/// `ncpu` is read fresh from `cur` (it can't meaningfully change between
/// two samples a fraction of a second apart on a running host).
///
/// Returns `0.0` for a zero or negative `total_jiffies` delta (clock
/// anomaly, or `prev`/`cur` passed in the wrong order) rather than
/// dividing by zero or going negative.
pub fn cpu_percent_per_core(prev: ProcSample, cur: ProcSample) -> f32 {
    let total_delta = cur.total_jiffies.saturating_sub(prev.total_jiffies);
    if total_delta == 0 {
        return 0.0;
    }
    let proc_delta = cur.proc_jiffies.saturating_sub(prev.proc_jiffies);
    (proc_delta as f64 / total_delta as f64 * cur.ncpu as f64 * 100.0) as f32
}

/// Parses `utime` (field 14) + `stime` (field 15) from the contents of
/// `/proc/<pid>/stat`, in scheduler-tick units. Field indices per
/// `proc_pid_stat(5)`.
///
/// Can't naively `split_whitespace()` the whole line: field 2 (`comm`, the
/// executable name in parens) can itself contain spaces or even
/// parentheses, e.g. `(qemu-system-x86_64)`. Instead, split off everything
/// after the **last** `)` — `comm` is the only field that can contain `)`,
/// so this is the robust way real tools (e.g. `procps`) parse this file —
/// and index from there, 0-based, where index 0 is field 3 (`state`).
fn parse_pid_stat_jiffies(contents: &str) -> io::Result<u64> {
    let after_comm = contents
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or_else(|| invalid_data("/proc/<pid>/stat: no ')' found"))?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Field 3 is index 0 here (state); utime is field 14 -> index 11,
    // stime is field 15 -> index 12.
    let utime = fields
        .get(11)
        .ok_or_else(|| invalid_data("/proc/<pid>/stat: missing utime field"))?;
    let stime = fields
        .get(12)
        .ok_or_else(|| invalid_data("/proc/<pid>/stat: missing stime field"))?;
    let utime: u64 = utime
        .parse()
        .map_err(|_| invalid_data("/proc/<pid>/stat: utime not a number"))?;
    let stime: u64 = stime
        .parse()
        .map_err(|_| invalid_data("/proc/<pid>/stat: stime not a number"))?;
    Ok(utime + stime)
}

/// Parses the aggregate `cpu` line (sum of all cores, same tick units as
/// [`parse_pid_stat_jiffies`]) plus a count of `cpuN` lines (number of
/// host CPUs) from the contents of `/proc/stat`.
fn parse_stat_total_and_ncpu(contents: &str) -> io::Result<(u64, u32)> {
    let mut total = None;
    let mut ncpu = 0u32;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("cpu ") {
            let sum: u64 = rest
                .split_whitespace()
                .map(|f| f.parse::<u64>().unwrap_or(0))
                .sum();
            total = Some(sum);
        } else if line.starts_with("cpu") && line[3..].starts_with(|c: char| c.is_ascii_digit()) {
            ncpu += 1;
        }
    }
    let total = total.ok_or_else(|| invalid_data("/proc/stat: missing aggregate 'cpu' line"))?;
    if ncpu == 0 {
        return Err(invalid_data("/proc/stat: no per-core 'cpuN' lines found"));
    }
    Ok((total, ncpu))
}

/// Parses `VmRSS` (in kB, per `proc_pid_status(5)`) from the contents of
/// `/proc/<pid>/status`, converting to bytes.
fn parse_vmrss_bytes(contents: &str) -> io::Result<u64> {
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest
                .trim()
                .strip_suffix("kB")
                .unwrap_or(rest.trim())
                .trim()
                .parse()
                .map_err(|_| invalid_data("/proc/<pid>/status: VmRSS not a number"))?;
            return Ok(kb * 1024);
        }
    }
    Err(invalid_data("/proc/<pid>/status: missing VmRSS line"))
}

fn invalid_data(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Size in bytes of a file (e.g. the VM's overlay disk), or `0` if it
/// doesn't exist yet (a workspace that's never been booted). Any other
/// error (permissions, etc.) is propagated.
pub fn file_size_or_zero(path: &Path) -> io::Result<u64> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.len()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real-shaped `/proc/<pid>/stat` line (comm has no spaces here, the
    /// simple case).
    #[test]
    fn parse_pid_stat_jiffies_simple_comm() {
        // state=S ppid=1 ... utime=1234 stime=56 (fields after ')': state
        // is index 0, utime is index 11, stime is index 12). The fields
        // vector below *includes* state at index 0, matching exactly what
        // `parse_pid_stat_jiffies` indexes into after splitting on `)`.
        let fields: Vec<String> = (0..52usize)
            .map(|i| match i {
                0 => "S".to_string(),
                11 => "1234".to_string(),
                12 => "56".to_string(),
                _ => "0".to_string(),
            })
            .collect();
        let line = format!("1 (qemu-system-x86_64) {}", fields.join(" "));
        assert_eq!(parse_pid_stat_jiffies(&line).unwrap(), 1234 + 56);
    }

    /// `comm` can itself contain spaces and parentheses (an adversarial but
    /// real possibility per `proc_pid_stat(5)`) — parsing must split on the
    /// *last* `)`, not the first.
    #[test]
    fn parse_pid_stat_jiffies_comm_with_parens_and_spaces() {
        let fields: Vec<String> = (0..52usize)
            .map(|i| match i {
                0 => "S".to_string(),
                11 => "10".to_string(),
                12 => "20".to_string(),
                _ => "0".to_string(),
            })
            .collect();
        let line = format!("1 (weird (name) proc) {}", fields.join(" "));
        assert_eq!(parse_pid_stat_jiffies(&line).unwrap(), 30);
    }

    #[test]
    fn parse_pid_stat_jiffies_errors_on_missing_fields() {
        assert!(parse_pid_stat_jiffies("1 (short) S 0 0").is_err());
        assert!(parse_pid_stat_jiffies("no parens here").is_err());
    }

    /// A realistically-shaped 4-core `/proc/stat` snippet.
    #[test]
    fn parse_stat_total_and_ncpu_counts_cores_and_sums_aggregate() {
        let contents = "\
cpu  100 0 50 850 0 0 0 0 0 0
cpu0 30 0 10 200 0 0 0 0 0 0
cpu1 20 0 15 220 0 0 0 0 0 0
cpu2 25 0 15 215 0 0 0 0 0 0
cpu3 25 0 10 215 0 0 0 0 0 0
intr 12345 0 0 0
ctxt 98765
";
        let (total, ncpu) = parse_stat_total_and_ncpu(contents).unwrap();
        assert_eq!(total, 100 + 50 + 850);
        assert_eq!(ncpu, 4);
    }

    #[test]
    fn parse_stat_total_and_ncpu_errors_when_missing() {
        assert!(parse_stat_total_and_ncpu("intr 0\nctxt 0\n").is_err());
    }

    #[test]
    fn parse_vmrss_bytes_converts_kb_to_bytes() {
        let contents = "\
VmPeak:   123456 kB
VmRSS:     45678 kB
VmSize:   123456 kB
";
        assert_eq!(parse_vmrss_bytes(contents).unwrap(), 45678 * 1024);
    }

    #[test]
    fn parse_vmrss_bytes_errors_when_missing() {
        assert!(parse_vmrss_bytes("VmPeak: 123 kB\n").is_err());
    }

    /// The core of the CPU-percent math: a process that used 50 of the
    /// 200-jiffy total delta across 4 cores should read as 100% (one core
    /// fully busy) — `(50/200) * 4 * 100`.
    #[test]
    fn cpu_percent_per_core_computes_top_style_percentage() {
        let prev = ProcSample {
            proc_jiffies: 1000,
            total_jiffies: 10_000,
            ncpu: 4,
            rss_bytes: 0,
        };
        let cur = ProcSample {
            proc_jiffies: 1050,
            total_jiffies: 10_200,
            ncpu: 4,
            rss_bytes: 0,
        };
        let pct = cpu_percent_per_core(prev, cur);
        assert!((pct - 100.0).abs() < 0.01, "expected ~100%, got {pct}");
    }

    #[test]
    fn cpu_percent_per_core_is_zero_when_total_delta_is_zero_or_negative() {
        let prev = ProcSample {
            proc_jiffies: 100,
            total_jiffies: 5000,
            ncpu: 2,
            rss_bytes: 0,
        };
        let same = prev;
        assert_eq!(cpu_percent_per_core(prev, same), 0.0);

        let earlier = ProcSample {
            total_jiffies: 4000,
            ..prev
        };
        assert_eq!(cpu_percent_per_core(prev, earlier), 0.0);
    }

    #[test]
    fn file_size_or_zero_is_zero_for_missing_file() {
        let path = std::env::temp_dir().join("gavmo-stats-test-definitely-missing-file");
        assert_eq!(file_size_or_zero(&path).unwrap(), 0);
    }

    #[test]
    fn file_size_or_zero_returns_real_size_for_existing_file() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gavmo-stats-test-{}-{n}",
            std::process::id()
        ));
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(file_size_or_zero(&path).unwrap(), 5);
        let _ = std::fs::remove_file(&path);
    }
}
