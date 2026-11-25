//! [`DpapiKeystore`]: the key sealed with DPAPI, in [`KEY_FILE`].

use base64::Engine as _;
use keystore::{Error, Keystore, SecretBytes};
use private_fs::PrivateDir;
use wsl::PowerShell;
use zeroize::Zeroizing;

use crate::key_file::{self, KEY_FILE};
use crate::{DpapiError, Op};

const NAME: &str = "Windows DPAPI (through WSL interop)";

/// The key sealed to the Windows user in [`KEY_FILE`] in `dir` (the
/// vault's directory), sealed and unsealed by running `powershell`.
#[derive(Debug, Clone)]
pub struct DpapiKeystore {
    dir: PrivateDir,
    powershell: PowerShell,
}

impl DpapiKeystore {
    pub fn new(dir: PrivateDir, powershell: PowerShell) -> Self {
        Self { dir, powershell }
    }

    /// Runs the DPAPI script to `op` the base64 `data`; the result is
    /// base64 too. Key material only passes through PowerShell's pipes.
    fn run(&self, op: Op, data: &str) -> Result<Zeroizing<String>, Error> {
        let input = Zeroizing::new(format!("{op}\n{data}\n"));
        self.powershell
            .run(crate::script::DPAPI, input.as_bytes())
            .map_err(|source| {
                Error::unavailable(
                    NAME,
                    DpapiError::PowerShell {
                        op,
                        powershell: self.powershell.path().to_path_buf(),
                        source,
                    },
                )
            })
    }

    /// Unseals `blob` (base64) into the key.
    fn unseal(&self, blob: &str) -> Result<SecretBytes, Error> {
        let encoded = self.run(Op::Unprotect, blob)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map(Zeroizing::new)
            .map_err(|_decode| {
                Error::invalid(NAME, DpapiError::ResultNotBase64 { op: Op::Unprotect })
            })
    }
}

impl Keystore for DpapiKeystore {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Needs no PowerShell when there is no key file.
    fn load(&self) -> Result<Option<SecretBytes>, Error> {
        let Some(contents) = self.dir.read(KEY_FILE).map_err(file_error)? else {
            return Ok(None);
        };
        let blob = key_file::parse(&contents, &self.dir.file(KEY_FILE))
            .map_err(|e| Error::invalid(NAME, e))?;
        self.unseal(&blob).map(Some)
    }

    /// Seals `secret`, checks the seal opens again, and only then writes
    /// the key file - never over an existing one.
    fn create(&self, secret: &[u8]) -> Result<(), Error> {
        if self.dir.exists(KEY_FILE).map_err(file_error)? {
            return Err(Error::AlreadyExists { keystore: NAME });
        }
        let encoded = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(secret));
        let blob = self.run(Op::Protect, &encoded)?;
        if !key_file::is_base64(&blob) {
            return Err(Error::unavailable(
                NAME,
                DpapiError::ResultNotBase64 { op: Op::Protect },
            ));
        }
        // An unopenable seal would surface only at the next start, after
        // secrets had been encrypted with this key: check now instead.
        if self.unseal(&blob)?.as_slice() != secret {
            return Err(Error::unavailable(NAME, DpapiError::SealDoesNotOpen));
        }
        match self
            .dir
            .create_new(KEY_FILE, key_file::render(&blob).as_bytes())
        {
            Ok(()) => Ok(()),
            Err(private_fs::Error::AlreadyExists { .. }) => {
                Err(Error::AlreadyExists { keystore: NAME })
            }
            Err(e) => Err(file_error(e)),
        }
    }
}

fn file_error(e: private_fs::Error) -> Error {
    Error::unavailable(NAME, DpapiError::File(e))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A stand-in `powershell.exe`: "seals" by prefixing `sealed:` and
    /// base64-encoding, and refuses to unseal anything else, like DPAPI
    /// refusing a foreign blob. `extra` runs first (e.g. to fail).
    fn fake_powershell(dir: &Path, extra: &str) -> PowerShell {
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
        PowerShell::at(path)
    }

    struct Fixture {
        tmp: tempfile::TempDir,
        dir: PrivateDir,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
            Self { tmp, dir }
        }

        fn keystore(&self, extra: &str) -> DpapiKeystore {
            DpapiKeystore::new(self.dir.clone(), fake_powershell(self.tmp.path(), extra))
        }

        fn key_file(&self) -> std::path::PathBuf {
            self.dir.file(KEY_FILE)
        }
    }

    fn unavailable_because(result: Result<impl std::fmt::Debug, Error>) -> String {
        match result {
            Err(Error::Unavailable { source, .. }) => source.to_string(),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn creates_then_loads_the_same_key() {
        let fx = Fixture::new();
        let keys = fx.keystore("");
        assert!(keys.load().unwrap().is_none(), "no key file yet");

        keys.create(b"0123456789abcdef").unwrap();
        let file = std::fs::read_to_string(fx.key_file()).unwrap();
        assert!(file.starts_with("gavmo-dpapi-v1\n"), "{file}");
        assert!(
            !file.contains(&base64::engine::general_purpose::STANDARD.encode(b"0123456789abcdef")),
            "the key itself is never written"
        );

        assert_eq!(
            keys.load().unwrap().unwrap().as_slice(),
            b"0123456789abcdef"
        );
    }

    /// A key file as earlier versions wrote it still opens.
    #[test]
    fn loads_a_key_file_written_by_earlier_versions() {
        let fx = Fixture::new();
        // base64("sealed:" + base64("key")) as the fake seals it.
        std::fs::write(fx.key_file(), "gavmo-dpapi-v1\nc2VhbGVkOmEyVjU=\n").unwrap();
        assert_eq!(fx.keystore("").load().unwrap().unwrap().as_slice(), b"key");
    }

    #[test]
    fn never_overwrites_an_existing_key_file() {
        let fx = Fixture::new();
        let keys = fx.keystore("");
        keys.create(b"first").unwrap();
        assert!(matches!(
            keys.create(b"second"),
            Err(Error::AlreadyExists { .. })
        ));
        assert_eq!(keys.load().unwrap().unwrap().as_slice(), b"first");
    }

    #[test]
    fn a_failing_powershell_reports_its_error_and_writes_nothing() {
        let fx = Fixture::new();
        let keys = fx.keystore("echo 'blocked by group policy' >&2; exit 3");
        let why = unavailable_because(keys.create(b"key"));
        assert!(why.contains("blocked by group policy"), "{why}");
        assert!(why.contains("to protect the key"), "{why}");
        assert!(!fx.key_file().exists());
    }

    #[test]
    fn a_seal_that_does_not_open_is_not_written() {
        let fx = Fixture::new();
        // Seals fine, but unsealing returns some other key.
        let keys = fx.keystore(
            "read -r op; read -r data; \
             if [ \"$op\" = protect ]; then printf 'c2VhbGVk'; else printf 'b3RoZXI='; fi; exit 0",
        );
        let why = unavailable_because(keys.create(b"key"));
        assert!(why.contains("different key"), "{why}");
        assert!(!fx.key_file().exists());
    }

    #[test]
    fn a_seal_that_is_not_base64_is_not_written() {
        let fx = Fixture::new();
        let keys = fx.keystore("printf 'not base64!'; exit 0");
        let why = unavailable_because(keys.create(b"key"));
        assert!(why.contains("not base64"), "{why}");
        assert!(!fx.key_file().exists());
    }

    #[test]
    fn a_missing_powershell_is_reported_but_not_needed_without_a_key_file() {
        let fx = Fixture::new();
        let keys = DpapiKeystore::new(
            fx.dir.clone(),
            PowerShell::at(fx.tmp.path().join("no-such-powershell.exe")),
        );
        assert!(keys.load().unwrap().is_none());
        let why = unavailable_because(keys.create(b"key"));
        assert!(why.contains("cannot start PowerShell"), "{why}");
    }

    #[test]
    fn a_foreign_or_corrupt_key_file_is_refused() {
        let fx = Fixture::new();
        let keys = fx.keystore("");
        std::fs::write(fx.key_file(), "something else\nQUJD\n").unwrap();
        assert!(matches!(keys.load(), Err(Error::Invalid { .. })));
        std::fs::write(fx.key_file(), "gavmo-dpapi-v1\n!!not base64!!\n").unwrap();
        assert!(matches!(keys.load(), Err(Error::Invalid { .. })));

        // Valid base64, but not something this user sealed.
        std::fs::write(fx.key_file(), "gavmo-dpapi-v1\nQUJD\n").unwrap();
        let why = unavailable_because(keys.load());
        assert!(why.contains("Key not valid"), "{why}");
    }

    #[test]
    fn an_unsealed_result_that_is_not_base64_is_invalid() {
        let fx = Fixture::new();
        std::fs::write(fx.key_file(), "gavmo-dpapi-v1\nQUJD\n").unwrap();
        let keys = fx.keystore("printf 'not base64!'; exit 0");
        assert!(matches!(keys.load(), Err(Error::Invalid { .. })));
    }
}
