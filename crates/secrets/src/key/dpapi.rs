//! The data key sealed with Windows DPAPI, for GAVMO running under WSL.
//!
//! A Linux process can't call Windows APIs, but WSL interop lets it run
//! Windows programs: Windows PowerShell (on every Windows 10/11) exposes DPAPI
//! as `[Security.Cryptography.ProtectedData]`. The key is sealed to the
//! current Windows user and the sealed blob is kept next to the vault, so
//! signing in to Windows is what unlocks it - no Linux keyring needed.
//!
//! Like an unlocked keyring, any process running as that Windows user
//! (including any WSL process, through interop) can unseal it.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use zeroize::Zeroizing;

use super::{DataKey, KeyProvider};
use crate::SecretError;
use crate::fs::write_private_atomic;

/// The sealed key's file name, next to the vault.
pub const DPAPI_KEY_FILE: &str = "vault-key.dpapi";

/// First line of the key file; the sealed blob (base64) is the second.
const FILE_HEADER: &str = "gavmo-dpapi-v1";

/// How long one PowerShell run may take. A run is normally well under a
/// second (about 0.6 s measured, including PowerShell's first-use setup),
/// but security software may hold a new process for a while.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The PowerShell side. Fixed text, so nothing secret is ever on a command
/// line (visible in `/proc` and the Windows process list): the operation and
/// its data arrive on stdin (`<op>\n<base64>\n`) and the result (base64)
/// leaves on stdout. Failures are written to stderr as plain text, since
/// PowerShell would otherwise serialize them as CLIXML when stderr isn't a
/// console; progress output (also CLIXML, "Preparing modules for first
/// use") is switched off.
///
/// The optional-entropy tag binds blobs to this purpose, so no other DPAPI
/// blob of the user's can be passed off as GAVMO's key.
const SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {
    $mode = $ExecutionContext.SessionState.LanguageMode
    if ($mode -ne 'FullLanguage') {
        throw ('PowerShell runs in ' + $mode + ' mode here (restricted by policy), which blocks DPAPI')
    }
    Add-Type -AssemblyName System.Security
    $lines = [Console]::In.ReadToEnd().Split([char]10)
    $op = $lines[0].Trim()
    $data = [Convert]::FromBase64String($lines[1].Trim())
    $tag = [Text.Encoding]::UTF8.GetBytes('gavmo-vault-key-v1')
    $scope = [Security.Cryptography.DataProtectionScope]::CurrentUser
    if ($op -eq 'protect') {
        $result = [Security.Cryptography.ProtectedData]::Protect($data, $tag, $scope)
    } elseif ($op -eq 'unprotect') {
        $result = [Security.Cryptography.ProtectedData]::Unprotect($data, $tag, $scope)
    } else {
        throw ('unknown operation ' + $op)
    }
    [Console]::Out.Write([Convert]::ToBase64String($result))
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
";

#[derive(Debug, Clone, Copy)]
enum Op {
    Protect,
    Unprotect,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Self::Protect => "protect",
            Self::Unprotect => "unprotect",
        }
    }
}

/// [`KeyProvider`] for WSL: the data key sealed with DPAPI in
/// [`DPAPI_KEY_FILE`], unsealed through `powershell.exe`.
#[derive(Debug, Clone)]
pub struct DpapiKeyProvider {
    key_path: PathBuf,
    powershell: PathBuf,
    timeout: Duration,
}

impl DpapiKeyProvider {
    /// `key_path` is the sealed key file; `powershell` the Windows
    /// PowerShell to run (see [`crate::wsl::find_powershell`]).
    pub fn new(key_path: impl Into<PathBuf>, powershell: impl Into<PathBuf>) -> Self {
        Self {
            key_path: key_path.into(),
            powershell: powershell.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs [`SCRIPT`] for `op` on `data` (base64), returning its stdout
    /// (base64, trimmed). Key material only ever passes through the pipes.
    fn run(&self, op: Op, data: &str) -> Result<Zeroizing<String>, SecretError> {
        let unavailable = |why: String| {
            SecretError::WindowsKeyUnavailable(format!(
                "{why} (running {} to {} the key)",
                self.powershell.display(),
                op.name()
            ))
        };

        let mut child = Command::new(&self.powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
            ])
            .arg(encoded_script())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| unavailable(format!("cannot start PowerShell: {e}")))?;

        // Read both pipes on their own threads so a chatty child can't block
        // on a full pipe while we wait for it.
        let stdout = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                // Pre-sized so the buffer never reallocates, which would leave
                // an unzeroized copy of the key behind.
                let mut buf = Zeroizing::new(Vec::with_capacity(64 * 1024));
                out.read_to_end(&mut buf).map(|_| buf)
            })
        });
        let stderr = child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                err.read_to_end(&mut buf).map(|_| buf)
            })
        });

        // The input is tiny, far below a pipe's buffer, so writing it all
        // before reading can't deadlock. A failed write (the child died
        // early) is reported below, with its exit status and stderr.
        let input = Zeroizing::new(format!("{}\n{data}\n", op.name()));
        let write_result = child
            .stdin
            .take()
            .map_or(Ok(()), |mut stdin| stdin.write_all(input.as_bytes()));

        let status = match wait_with_timeout(&mut child, self.timeout) {
            Ok(Some(status)) => status,
            Ok(None) => {
                // Best effort: the pipes close with the process, ending the
                // reader threads; they are not joined in case they don't.
                if let Err(e) = child.kill().and_then(|()| child.wait().map(drop)) {
                    return Err(unavailable(format!(
                        "PowerShell took longer than {:?} and could not be stopped: {e}",
                        self.timeout
                    )));
                }
                return Err(unavailable(format!(
                    "PowerShell took longer than {:?}",
                    self.timeout
                )));
            }
            Err(e) => return Err(unavailable(format!("waiting for PowerShell: {e}"))),
        };

        let stderr = stderr
            .and_then(|t| t.join().ok())
            .and_then(Result::ok)
            .unwrap_or_default();
        if !status.success() {
            return Err(unavailable(format!(
                "PowerShell failed ({status}): {}",
                describe_stderr(&stderr)
            )));
        }
        if let Err(e) = write_result {
            return Err(unavailable(format!("writing to PowerShell: {e}")));
        }
        let stdout = stdout
            .and_then(|t| t.join().ok())
            .transpose()
            .map_err(|e| unavailable(format!("reading PowerShell's output: {e}")))?
            .ok_or_else(|| unavailable("no output from PowerShell".to_owned()))?;
        let text = std::str::from_utf8(&stdout)
            .map_err(|e| unavailable(format!("PowerShell's output is not UTF-8: {e}")))?
            .trim();
        if text.is_empty() {
            return Err(unavailable(format!(
                "no output from PowerShell: {}",
                describe_stderr(&stderr)
            )));
        }
        Ok(Zeroizing::new(text.to_owned()))
    }

    fn read_blob(&self) -> Result<Option<String>, SecretError> {
        let contents = match std::fs::read_to_string(&self.key_path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SecretError::io("reading", &self.key_path)(e)),
        };
        parse_key_file(&contents, &self.key_path).map(Some)
    }
}

impl KeyProvider for DpapiKeyProvider {
    fn load_key(&self) -> Result<Option<DataKey>, SecretError> {
        let Some(blob) = self.read_blob()? else {
            return Ok(None);
        };
        let encoded = self.run(Op::Unprotect, &blob)?;
        DataKey::decode(&encoded).map(Some)
    }

    /// Seals a new key, checks the seal opens again, and only then writes
    /// the key file - never over an existing one.
    fn create_key(&self) -> Result<DataKey, SecretError> {
        let key = DataKey::generate();
        let blob = self.run(Op::Protect, &key.encode())?;
        if base64::engine::general_purpose::STANDARD
            .decode(blob.as_bytes())
            .is_err()
        {
            return Err(SecretError::WindowsKeyUnavailable(
                "PowerShell returned a sealed key that is not base64".to_owned(),
            ));
        }
        // An unopenable seal would surface only at the next start, after
        // secrets had been encrypted with this key: check now instead.
        let reopened = DataKey::decode(&self.run(Op::Unprotect, &blob)?)?;
        if reopened.as_bytes() != key.as_bytes() {
            return Err(SecretError::WindowsKeyUnavailable(
                "unsealing the new key gave a different key".to_owned(),
            ));
        }
        let file = format!("{FILE_HEADER}\n{}\n", blob.as_str());
        write_private_atomic(&self.key_path, file.as_bytes(), false)?;
        Ok(key)
    }
}

/// The script as PowerShell's `-EncodedCommand` wants it: base64 of UTF-16LE.
/// No quoting can go wrong on the way through WSL's command-line conversion.
fn encoded_script() -> String {
    let utf16: Vec<u8> = SCRIPT.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

/// Waits up to `timeout` for `child`: `Ok(None)` if it is still running.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The sealed blob from a key file, checked to be base64.
fn parse_key_file(contents: &str, path: &Path) -> Result<String, SecretError> {
    let bad = |why: &str| SecretError::BadKey(format!("{}: {why}", path.display()));
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some(FILE_HEADER) {
        return Err(bad("not a GAVMO DPAPI key file (unknown header)"));
    }
    let blob = lines.next().map(str::trim).unwrap_or_default();
    if blob.is_empty()
        || base64::engine::general_purpose::STANDARD
            .decode(blob)
            .is_err()
    {
        return Err(bad("the sealed key is missing or not base64"));
    }
    Ok(blob.to_owned())
}

/// Error messages longer than this (in characters) are cut short.
const MAX_MESSAGE_CHARS: usize = 500;

/// PowerShell's stderr as a short, readable message. Normally plain text
/// (see [`SCRIPT`]); if PowerShell itself reported an error, it is CLIXML,
/// and the error strings are pulled out of it.
fn describe_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let message = if text.contains("#< CLIXML") {
        clixml_errors(&text)
    } else {
        text.trim().to_owned()
    };
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if message.is_empty() {
        return "no error output".to_owned();
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        format!(
            "{}...",
            message.chars().take(MAX_MESSAGE_CHARS).collect::<String>()
        )
    } else {
        message
    }
}

/// The `<S S="Error">` strings of a CLIXML stream, unescaped.
fn clixml_errors(text: &str) -> String {
    const OPEN: &str = "<S S=\"Error\">";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        rest = rest.get(start + OPEN.len()..).unwrap_or_default();
        let end = rest.find("</S>").unwrap_or(rest.len());
        out.push_str(rest.get(..end).unwrap_or_default());
        rest = rest.get(end..).unwrap_or_default();
    }
    out.replace("_x000D_", "")
        .replace("_x000A_", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in `powershell.exe`: "seals" by prefixing `sealed:` and
    /// base64-encoding, and refuses to unseal anything else, like DPAPI
    /// refusing a foreign blob. `extra` runs first (e.g. to fail or log).
    fn fake_powershell(dir: &Path, extra: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("powershell.exe");
        let script = format!(
            r#"#!/bin/sh
{extra}
read -r op
read -r data
case "$op" in
  protect) printf 'sealed:%s' "$data" | base64 -w0 ;;
  unprotect)
    plain=$(printf '%s' "$data" | base64 -d 2>/dev/null) || {{ echo 'not a blob' >&2; exit 1; }}
    case "$plain" in
      sealed:*) printf '%s\r\n' "${{plain#sealed:}}" ;;
      *) echo 'Key not valid for use in specified state.' >&2; exit 1 ;;
    esac ;;
  *) echo "unknown operation $op" >&2; exit 1 ;;
esac
"#
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn provider(dir: &tempfile::TempDir, extra: &str) -> DpapiKeyProvider {
        DpapiKeyProvider::new(
            dir.path().join(DPAPI_KEY_FILE),
            fake_powershell(dir.path(), extra),
        )
    }

    fn windows_error(result: Result<impl std::fmt::Debug, SecretError>) -> String {
        match result {
            Err(SecretError::WindowsKeyUnavailable(why)) => why,
            other => panic!("expected WindowsKeyUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn creates_then_loads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let keys = provider(&dir, "");
        assert!(keys.load_key().unwrap().is_none(), "no key file yet");

        let created = keys.create_key().unwrap();
        let file = std::fs::read_to_string(dir.path().join(DPAPI_KEY_FILE)).unwrap();
        assert!(file.starts_with("gavmo-dpapi-v1\n"), "{file}");
        assert!(
            !file.contains(created.encode().as_str()),
            "the key itself is never written"
        );

        let loaded = keys.load_key().unwrap().unwrap();
        assert_eq!(loaded.as_bytes(), created.as_bytes());
    }

    #[test]
    fn never_overwrites_an_existing_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let keys = provider(&dir, "");
        let first = keys.create_key().unwrap();
        let err = keys.create_key().unwrap_err();
        assert!(
            matches!(
                err,
                SecretError::Io {
                    action: "creating",
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            keys.load_key().unwrap().unwrap().as_bytes(),
            first.as_bytes()
        );
    }

    /// Keys pass on stdin, never as arguments.
    #[test]
    fn the_command_line_carries_only_the_fixed_script() {
        let dir = tempfile::tempdir().unwrap();
        let args_log = dir.path().join("args");
        let keys = provider(
            &dir,
            &format!("printf '%s\\n' \"$@\" >> '{}'", args_log.display()),
        );
        let key = keys.create_key().unwrap();
        let args = std::fs::read_to_string(&args_log).unwrap();
        let expected = format!(
            "-NoLogo\n-NoProfile\n-NonInteractive\n-EncodedCommand\n{}\n",
            encoded_script()
        );
        assert_eq!(args, expected.repeat(2), "protect, then the check");
        assert!(!args.contains(key.encode().as_str()));
    }

    #[test]
    fn encoded_script_is_utf16le_base64_of_the_script() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded_script())
            .unwrap();
        let (pairs, rest) = bytes.as_chunks::<2>();
        assert!(rest.is_empty());
        let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), SCRIPT);
    }

    #[test]
    fn a_failing_powershell_reports_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let keys = provider(&dir, "echo 'blocked by group policy' >&2; exit 3");
        let why = windows_error(keys.create_key());
        assert!(why.contains("blocked by group policy"), "{why}");
        assert!(why.contains("to protect the key"), "{why}");
        assert!(
            !dir.path().join(DPAPI_KEY_FILE).exists(),
            "no key file written"
        );
    }

    #[test]
    fn a_seal_that_does_not_open_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        // Seals fine, but unsealing returns some other key.
        let other = DataKey::generate().encode();
        let keys = provider(
            &dir,
            &format!(
                "read -r op; read -r data; \
                 if [ \"$op\" = protect ]; then printf 'c2VhbGVk'; else printf '{}'; fi; exit 0",
                other.as_str()
            ),
        );
        let why = windows_error(keys.create_key());
        assert!(why.contains("different key"), "{why}");
        assert!(!dir.path().join(DPAPI_KEY_FILE).exists());
    }

    #[test]
    fn a_missing_powershell_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let keys = DpapiKeyProvider::new(
            dir.path().join(DPAPI_KEY_FILE),
            dir.path().join("no-such-powershell.exe"),
        );
        let why = windows_error(keys.create_key());
        assert!(why.contains("cannot start PowerShell"), "{why}");
        // Loading without a key file needs no PowerShell at all.
        assert!(keys.load_key().unwrap().is_none());
    }

    #[test]
    fn a_hung_powershell_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let keys = provider(&dir, "exec sleep 30").with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let why = windows_error(keys.create_key());
        assert!(why.contains("took longer than"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_foreign_or_corrupt_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let keys = provider(&dir, "");
        let path = dir.path().join(DPAPI_KEY_FILE);

        std::fs::write(&path, "something else\nQUJD\n").unwrap();
        assert!(matches!(keys.load_key(), Err(SecretError::BadKey(_))));
        std::fs::write(&path, "gavmo-dpapi-v1\n!!not base64!!\n").unwrap();
        assert!(matches!(keys.load_key(), Err(SecretError::BadKey(_))));
        std::fs::write(&path, "gavmo-dpapi-v1\n").unwrap();
        assert!(matches!(keys.load_key(), Err(SecretError::BadKey(_))));

        // Valid base64, but not something this user sealed.
        std::fs::write(&path, "gavmo-dpapi-v1\nQUJD\n").unwrap();
        let why = windows_error(keys.load_key());
        assert!(why.contains("Key not valid"), "{why}");
    }

    #[test]
    fn stderr_is_summarized_including_clixml() {
        assert_eq!(describe_stderr(b"  boom \r\n"), "boom");
        assert_eq!(describe_stderr(b""), "no error output");
        let clixml = concat!(
            "#< CLIXML\r\n<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">",
            "<Obj S=\"progress\" RefId=\"0\"><TN RefId=\"0\"><T>System.Management.Automation.PSCustomObject</T></TN>",
            "<MS><PR N=\"Record\"><AV>Preparing modules for first use.</AV></PR></MS></Obj>",
            "<S S=\"Error\">Cannot invoke method. Method invocation is supported only on core types in this language mode._x000D__x000A_</S>",
            "<S S=\"Error\">At line:1 char:1 &amp; more_x000D__x000A_</S></Objs>"
        );
        assert_eq!(
            describe_stderr(clixml.as_bytes()),
            "Cannot invoke method. Method invocation is supported only on core types in \
             this language mode. At line:1 char:1 & more"
        );
        let long = "x".repeat(600);
        assert_eq!(describe_stderr(long.as_bytes()).len(), 503);
    }
}
