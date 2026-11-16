//! Detecting WSL, and finding the Windows PowerShell it can run.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

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

impl std::fmt::Display for Wsl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

/// Detects WSL from `proc` (normally `/proc`):
/// - interop is an enabled `binfmt_misc` entry named `WSLInterop*` (WSL
///   registers `WSLInterop`, and `WSLInterop-late` where systemd would
///   otherwise clear it); that is what running `.exe` files needs;
/// - otherwise, a kernel release naming Microsoft/WSL means WSL without
///   interop.
///
/// Unreadable files count as absent: the answer only picks a default.
pub fn detect_in(proc: &Path) -> Wsl {
    let binfmt = proc.join("sys/fs/binfmt_misc");
    let interop = std::fs::read_dir(&binfmt)
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

/// Overrides where `powershell.exe` is (an absolute path).
pub const POWERSHELL_ENV: &str = "GAVMO_POWERSHELL";

/// Where Windows keeps Windows PowerShell 5.1, under WSL's default automount.
pub const DEFAULT_POWERSHELL: &str =
    "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";

/// Finds Windows PowerShell: [`POWERSHELL_ENV`], else `powershell.exe` on
/// `$PATH` (WSL appends the Windows `PATH`), else [`DEFAULT_POWERSHELL`].
pub fn find_powershell() -> Option<PathBuf> {
    find_powershell_in(
        std::env::var_os(POWERSHELL_ENV),
        std::env::var_os("PATH").as_deref(),
        Path::new(DEFAULT_POWERSHELL),
    )
}

pub(crate) fn find_powershell_in(
    override_path: Option<OsString>,
    path: Option<&OsStr>,
    fallback: &Path,
) -> Option<PathBuf> {
    if let Some(explicit) = override_path.filter(|p| !p.is_empty()) {
        // Used as given, even if missing: running it then says why.
        return Some(PathBuf::from(explicit));
    }
    path.iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("powershell.exe"))
        .chain(std::iter::once(fallback.to_path_buf()))
        .find(|candidate| candidate.is_file())
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
    fn powershell_is_found_by_override_then_path_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let on_path = dir.path().join("bin");
        std::fs::create_dir(&on_path).unwrap();
        std::fs::write(on_path.join("powershell.exe"), b"").unwrap();
        let fallback = dir.path().join("default.exe");
        std::fs::write(&fallback, b"").unwrap();
        let path = Some(std::env::join_paths([dir.path().join("empty"), on_path.clone()]).unwrap());

        assert_eq!(
            find_powershell_in(Some("/x/ps.exe".into()), path.as_deref(), &fallback),
            Some(PathBuf::from("/x/ps.exe"))
        );
        assert_eq!(
            find_powershell_in(Some(OsString::new()), path.as_deref(), &fallback),
            Some(on_path.join("powershell.exe")),
            "an empty override is no override"
        );
        assert_eq!(
            find_powershell_in(None, None, &fallback),
            Some(fallback.clone())
        );
        assert_eq!(
            find_powershell_in(
                None,
                Some(OsStr::new("relative/bin")),
                &dir.path().join("missing")
            ),
            None,
            "relative PATH entries are skipped"
        );
    }
}
