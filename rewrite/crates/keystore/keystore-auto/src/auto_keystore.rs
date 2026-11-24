//! [`AutoKeystore`]: the keystore [`choose`] picks, picked on every call.

use std::path::PathBuf;

use keystore::{Error, Keystore, SecretBytes};
use keystore_secret_service::SecretServiceKeystore;
use keystore_wsl::{DpapiKeystore, KEY_FILE};
use private_fs::{FileName, PrivateDir};
use wsl::PowerShell;

use crate::{AutoError, Backend, Facts, KeystoreChoice, choose};

const NAME: &str = "the vault key store";

/// Gathers the [`Facts`] and passes each call to the keystore [`choose`]
/// picks, afresh every time. With the vault opened lazily and failed opens
/// retried, fixing the environment (enabling interop, creating a keyring)
/// takes effect without a restart, and a momentary problem at startup can't
/// lock a fresh install into the wrong keystore.
#[derive(Debug, Clone)]
pub struct AutoKeystore {
    dir: PrivateDir,
    vault: FileName,
    choice: KeystoreChoice,
    proc: PathBuf,
    powershell: FindPowerShell,
}

/// Where Windows PowerShell comes from.
#[derive(Debug, Clone)]
enum FindPowerShell {
    /// [`PowerShell::find`], when it is needed.
    Search,
    #[cfg(test)]
    Given(Option<PowerShell>),
}

impl AutoKeystore {
    /// For the vault called `vault` in `dir`; the DPAPI key file sits next
    /// to it.
    pub fn new(dir: PrivateDir, vault: FileName, choice: KeystoreChoice) -> Self {
        Self {
            dir,
            vault,
            choice,
            proc: PathBuf::from("/proc"),
            powershell: FindPowerShell::Search,
        }
    }

    /// The facts as they are now.
    pub fn facts(&self) -> Result<Facts, Error> {
        let exists = |name| {
            self.dir
                .exists(name)
                .map_err(|e| Error::unavailable(NAME, AutoError::File(e)))
        };
        Ok(Facts {
            choice: self.choice,
            dpapi_key_exists: exists(KEY_FILE)?,
            vault_exists: exists(self.vault)?,
            wsl: wsl::detect_in(&self.proc),
        })
    }

    /// The keystore that would be used now.
    pub fn backend(&self) -> Result<Backend, Error> {
        self.facts().map(|facts| choose(&facts))
    }

    fn pick(&self) -> Result<Box<dyn Keystore>, Error> {
        let facts = self.facts()?;
        Ok(match choose(&facts) {
            Backend::SecretService => Box::new(SecretServiceKeystore),
            Backend::Windows => {
                let powershell = match &self.powershell {
                    FindPowerShell::Search => PowerShell::find(),
                    #[cfg(test)]
                    FindPowerShell::Given(given) => given.clone(),
                };
                let powershell = powershell.ok_or_else(|| {
                    Error::unavailable(
                        "Windows DPAPI (through WSL interop)",
                        AutoError::NoPowerShell(facts.wsl),
                    )
                })?;
                Box::new(DpapiKeystore::new(self.dir.clone(), powershell))
            }
        })
    }

    #[cfg(test)]
    fn with_environment(mut self, proc: &std::path::Path, powershell: Option<PowerShell>) -> Self {
        self.proc = proc.to_path_buf();
        self.powershell = FindPowerShell::Given(powershell);
        self
    }
}

impl Keystore for AutoKeystore {
    fn name(&self) -> &'static str {
        NAME
    }

    fn load(&self) -> Result<Option<SecretBytes>, Error> {
        self.pick()?.load()
    }

    fn create(&self, secret: &[u8]) -> Result<(), Error> {
        self.pick()?.create(secret)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const VAULT: FileName = FileName::new("secrets.vault");

    fn wsl_proc() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let binfmt = dir.path().join("sys/fs/binfmt_misc");
        std::fs::create_dir_all(&binfmt).unwrap();
        std::fs::write(binfmt.join("WSLInterop"), "enabled\n").unwrap();
        dir
    }

    /// Stands in for `powershell.exe`: seals by base64-encoding a prefix.
    fn fake_powershell(dir: &Path) -> PowerShell {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("powershell.exe");
        std::fs::write(
            &path,
            r#"#!/bin/sh
read -r op
read -r data
case "$op" in
  protect) printf 'sealed:%s' "$data" | base64 -w0 ;;
  unprotect) printf '%s' "$data" | base64 -d | sed 's/^sealed://' ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        PowerShell::at(path)
    }

    /// End to end on a fake WSL: a fresh install's key goes to DPAPI, and
    /// stays there once the key file exists.
    #[test]
    fn a_fresh_install_on_wsl_is_keyed_with_dpapi_and_stays_there() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = wsl_proc();
        let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
        let keys = || {
            AutoKeystore::new(dir.clone(), VAULT, KeystoreChoice::Auto)
                .with_environment(proc.path(), Some(fake_powershell(tmp.path())))
        };
        assert_eq!(keys().backend().unwrap(), Backend::Windows);
        assert_eq!(keys().load().unwrap(), None);
        keys().create(b"key").unwrap();
        assert!(dir.exists(KEY_FILE).unwrap());

        // The vault now exists too; the key file still means Windows.
        dir.replace(VAULT, b"vault").unwrap();
        assert_eq!(keys().backend().unwrap(), Backend::Windows);
        assert_eq!(keys().load().unwrap().unwrap().as_slice(), b"key");
    }

    /// An existing vault without a DPAPI key file stays on the keyring, even
    /// on WSL.
    #[test]
    fn an_existing_keyring_vault_on_wsl_stays_on_the_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = wsl_proc();
        let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
        dir.replace(VAULT, b"existing").unwrap();
        let keys = AutoKeystore::new(dir, VAULT, KeystoreChoice::Auto)
            .with_environment(proc.path(), Some(fake_powershell(tmp.path())));
        assert_eq!(keys.backend().unwrap(), Backend::SecretService);
    }

    #[test]
    fn windows_without_powershell_explains_why() {
        let tmp = tempfile::tempdir().unwrap();
        let not_wsl = tempfile::tempdir().unwrap();
        let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
        let keys = AutoKeystore::new(dir, VAULT, KeystoreChoice::Windows)
            .with_environment(not_wsl.path(), None);
        match keys.load() {
            Err(Error::Unavailable { source, .. }) => {
                assert!(source.to_string().contains("not WSL"), "{source}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn facts_report_what_is_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let proc = wsl_proc();
        let dir = PrivateDir::create(tmp.path().join("data")).unwrap();
        let keys = AutoKeystore::new(dir.clone(), VAULT, KeystoreChoice::SecretService)
            .with_environment(proc.path(), None);
        dir.replace(KEY_FILE, b"").unwrap();
        assert_eq!(
            keys.facts().unwrap(),
            Facts {
                choice: KeystoreChoice::SecretService,
                dpapi_key_exists: true,
                vault_exists: false,
                wsl: wsl::Wsl::Interop,
            }
        );
    }
}
