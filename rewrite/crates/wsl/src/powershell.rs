//! [`PowerShell`]: Windows PowerShell, run from WSL through interop.

use std::ffi::{OsStr, OsString};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::{RunError, Script};

/// Overrides where `powershell.exe` is (an absolute path).
pub const POWERSHELL_ENV: &str = "GAVMO_POWERSHELL";

/// Where Windows keeps Windows PowerShell 5.1, under WSL's default automount.
pub const DEFAULT_POWERSHELL: &str =
    "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";

/// How long one run may take by default. A run is normally well under a
/// second (about 0.6 s measured, including PowerShell's first-use setup),
/// but security software may hold a new process for a while.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Room for a script's output without the buffer reallocating, which would
/// leave an unwiped copy behind.
const OUTPUT_CAPACITY: usize = 64 * 1024;

/// A Windows PowerShell executable, and how long a run of it may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerShell {
    path: PathBuf,
    timeout: Duration,
}

impl PowerShell {
    /// The PowerShell at `path`, used as given: if it is missing, running it
    /// says so.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// [`POWERSHELL_ENV`] if set, else `powershell.exe` on `$PATH` (WSL
    /// appends the Windows `PATH`), else [`DEFAULT_POWERSHELL`] if it exists.
    pub fn find() -> Option<Self> {
        find_in(
            std::env::var_os(POWERSHELL_ENV),
            std::env::var_os("PATH").as_deref(),
            Path::new(DEFAULT_POWERSHELL),
        )
        .map(Self::at)
    }

    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs `script` with `input` on its stdin and returns its stdout,
    /// trimmed; both may be secret, and only ever pass through the pipes.
    /// The run is killed if it takes longer than the timeout.
    pub fn run(&self, script: Script, input: &[u8]) -> Result<Zeroizing<String>, RunError> {
        let mut child = Command::new(&self.path)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
            ])
            .arg(script.encoded())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(RunError::CannotStart)?;

        // Both pipes are drained on their own threads, so a chatty child
        // can't block on a full pipe while we wait for it.
        let stdout = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let mut buf = Zeroizing::new(Vec::with_capacity(OUTPUT_CAPACITY));
                out.read_to_end(&mut buf).map(|_| buf)
            })
        });
        let stderr = child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                err.read_to_end(&mut buf).map(|_| buf)
            })
        });

        // The input is small, far below a pipe's buffer, so writing it all
        // before reading can't deadlock. A failed write (the child died
        // early) is reported after its exit status and stderr, which say
        // more.
        let written = child
            .stdin
            .take()
            .map_or(Ok(()), |mut stdin| stdin.write_all(input));

        let status = self.wait(&mut child)?;
        let stderr = crate::stderr::describe(&join(stderr).unwrap_or_default());
        if !status.success() {
            return Err(RunError::Failed { status, stderr });
        }
        written.map_err(RunError::WriteInput)?;
        let stdout = join(stdout).map_err(RunError::ReadOutput)?;
        let text = std::str::from_utf8(&stdout)
            .map_err(|_utf8| RunError::NotUtf8)?
            .trim();
        if text.is_empty() {
            return Err(RunError::NoOutput { stderr });
        }
        Ok(Zeroizing::new(text.to_owned()))
    }

    /// Waits for `child` up to the timeout, then kills it. On a timeout the
    /// pipe readers aren't joined: they end when the pipes close.
    fn wait(&self, child: &mut Child) -> Result<ExitStatus, RunError> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(status) = child.try_wait().map_err(RunError::Wait)? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return match child.kill().and_then(|()| child.wait()) {
                    Ok(_) => Err(RunError::TimedOut(self.timeout)),
                    Err(source) => Err(RunError::Unstoppable {
                        after: self.timeout,
                        source,
                    }),
                };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The result of a pipe-reader thread; a missing pipe or a panicked reader
/// is an error like a failed read.
fn join<T: Default>(reader: Option<JoinHandle<std::io::Result<T>>>) -> std::io::Result<T> {
    match reader.map(JoinHandle::join) {
        Some(Ok(result)) => result,
        Some(Err(_panic)) => Err(std::io::Error::other("the pipe reader panicked")),
        None => Ok(T::default()),
    }
}

fn find_in(
    override_path: Option<OsString>,
    path: Option<&OsStr>,
    fallback: &Path,
) -> Option<PathBuf> {
    if let Some(explicit) = override_path.filter(|p| !p.is_empty()) {
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
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const ECHO: Script = Script::new("echo back what came in");

    /// A stand-in `powershell.exe`: a shell script whose body is `body`.
    fn fake(dir: &Path, body: &str) -> PowerShell {
        let path = dir.join("powershell.exe");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        PowerShell::at(path)
    }

    #[test]
    fn input_goes_in_on_stdin_and_output_comes_back_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let ps = fake(
            dir.path(),
            "read -r line; printf '  got:%s\\r\\n' \"$line\"",
        );
        assert_eq!(ps.run(ECHO, b"secret\n").unwrap().as_str(), "got:secret");
    }

    /// Input passes on stdin, never as arguments.
    #[test]
    fn the_command_line_carries_only_the_fixed_script() {
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("args");
        let ps = fake(
            dir.path(),
            &format!("printf '%s\\n' \"$@\" > '{}'; echo ok", args.display()),
        );
        ps.run(ECHO, b"secret-input\n").unwrap();
        let args = std::fs::read_to_string(&args).unwrap();
        assert_eq!(
            args,
            format!(
                "-NoLogo\n-NoProfile\n-NonInteractive\n-EncodedCommand\n{}\n",
                ECHO.encoded()
            )
        );
    }

    #[test]
    fn a_failing_script_reports_its_error_output() {
        let dir = tempfile::tempdir().unwrap();
        let ps = fake(dir.path(), "echo 'blocked by group policy' >&2; exit 3");
        match ps.run(ECHO, b"") {
            Err(RunError::Failed { status, stderr }) => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stderr, "blocked by group policy");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn success_without_output_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ps = fake(dir.path(), "echo 'hmm' >&2");
        assert!(matches!(
            ps.run(ECHO, b""),
            Err(RunError::NoOutput { stderr }) if stderr == "hmm"
        ));
    }

    #[test]
    fn output_that_is_not_utf8_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ps = fake(dir.path(), "printf '\\377'");
        assert!(matches!(ps.run(ECHO, b""), Err(RunError::NotUtf8)));
    }

    #[test]
    fn a_missing_powershell_cannot_start() {
        let dir = tempfile::tempdir().unwrap();
        let ps = PowerShell::at(dir.path().join("missing.exe"));
        assert!(matches!(ps.run(ECHO, b""), Err(RunError::CannotStart(_))));
    }

    #[test]
    fn a_hung_powershell_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let ps = fake(dir.path(), "exec sleep 30").with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        assert!(matches!(ps.run(ECHO, b""), Err(RunError::TimedOut(_))));
        assert!(started.elapsed() < Duration::from_secs(10));
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
            find_in(Some("/x/ps.exe".into()), path.as_deref(), &fallback),
            Some(PathBuf::from("/x/ps.exe"))
        );
        assert_eq!(
            find_in(Some(OsString::new()), path.as_deref(), &fallback),
            Some(on_path.join("powershell.exe")),
            "an empty override is no override"
        );
        assert_eq!(find_in(None, None, &fallback), Some(fallback.clone()));
        assert_eq!(
            find_in(
                None,
                Some(OsStr::new("relative/bin")),
                &dir.path().join("missing")
            ),
            None,
            "relative PATH entries are skipped"
        );
    }
}
