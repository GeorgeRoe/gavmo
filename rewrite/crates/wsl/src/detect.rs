//! [`Wsl`]: whether this is WSL, and whether Windows programs run here.

use std::fmt;
use std::path::Path;

/// Where this host stands with respect to WSL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wsl {
    /// Not WSL (or WSL that can't be recognized).
    NotWsl,
    /// WSL, but Windows programs can't be run from it (`[interop]` disabled
    /// in `/etc/wsl.conf`, or its binfmt registration lost).
    NoInterop,
    /// WSL with interop: `.exe` files run.
    Interop,
}

impl fmt::Display for Wsl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotWsl => "not WSL",
            Self::NoInterop => "WSL without interop",
            Self::Interop => "WSL with interop",
        })
    }
}

/// [`detect_in`] on the real `/proc`.
pub fn detect() -> Wsl {
    detect_in(Path::new("/proc"))
}

/// Detects WSL from `proc` (normally `/proc`; another directory in tests):
/// - interop is an enabled `binfmt_misc` entry named `WSLInterop*` (WSL
///   registers `WSLInterop`, and `WSLInterop-late` where systemd would
///   otherwise clear it); that is what running `.exe` files needs;
/// - otherwise, a kernel release naming Microsoft/WSL means WSL without
///   interop.
///
/// Unreadable files count as absent: the answer only picks a default.
pub fn detect_in(proc: &Path) -> Wsl {
    let interop = std::fs::read_dir(proc.join("sys/fs/binfmt_misc"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("WSLInterop")
        })
        .any(|entry| {
            std::fs::read_to_string(entry.path())
                .is_ok_and(|status| status.lines().next() == Some("enabled"))
        });
    if interop {
        return Wsl::Interop;
    }
    let release = std::fs::read_to_string(proc.join("sys/kernel/osrelease"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if release.contains("microsoft") || release.contains("wsl") {
        Wsl::NoInterop
    } else {
        Wsl::NotWsl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc(osrelease: &str, binfmt: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sys/kernel")).unwrap();
        std::fs::write(dir.path().join("sys/kernel/osrelease"), osrelease).unwrap();
        let binfmt_dir = dir.path().join("sys/fs/binfmt_misc");
        std::fs::create_dir_all(&binfmt_dir).unwrap();
        for (name, contents) in binfmt {
            std::fs::write(binfmt_dir.join(name), contents).unwrap();
        }
        dir
    }

    const WSL_KERNEL: &str = "6.18.40.1-microsoft-standard-WSL2\n";
    const ENABLED: &str = "enabled\ninterpreter /init\nflags: PF\noffset 0\nmagic 4d5a\n";

    #[test]
    fn detects_wsl_with_and_without_interop() {
        let proc = fake_proc(
            WSL_KERNEL,
            &[("status", "enabled\n"), ("WSLInterop", ENABLED)],
        );
        assert_eq!(detect_in(proc.path()), Wsl::Interop);

        let late = fake_proc(WSL_KERNEL, &[("WSLInterop-late", ENABLED)]);
        assert_eq!(detect_in(late.path()), Wsl::Interop);

        let disabled = fake_proc(WSL_KERNEL, &[("WSLInterop", "disabled\n")]);
        assert_eq!(detect_in(disabled.path()), Wsl::NoInterop);

        let none = fake_proc(WSL_KERNEL, &[]);
        assert_eq!(detect_in(none.path()), Wsl::NoInterop);
    }

    #[test]
    fn plain_linux_is_not_wsl() {
        let proc = fake_proc("6.12.8-nixos\n", &[("status", "enabled\n")]);
        assert_eq!(detect_in(proc.path()), Wsl::NotWsl);
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(detect_in(empty.path()), Wsl::NotWsl);
    }

    #[test]
    fn displays_for_logs_and_errors() {
        assert_eq!(Wsl::Interop.to_string(), "WSL with interop");
        assert_eq!(Wsl::NoInterop.to_string(), "WSL without interop");
        assert_eq!(Wsl::NotWsl.to_string(), "not WSL");
    }
}
