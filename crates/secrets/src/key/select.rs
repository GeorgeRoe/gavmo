//! Which key store holds the vault's data key, decided each time the vault
//! opens.

use std::path::PathBuf;

use super::{DPAPI_KEY_FILE, DataKey, DpapiKeyProvider, KeyProvider, KeyringKeyProvider};
use crate::SecretError;
use crate::wsl::{self, Wsl};

/// The user's choice of key store (`--key-store`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyStoreChoice {
    /// Wherever the key already is; for a new vault, Windows on WSL with
    /// interop and the OS keyring everywhere else (see [`choose`]).
    #[default]
    Auto,
    /// Always the OS keyring (Secret Service).
    SecretService,
    /// Always Windows DPAPI, through WSL interop.
    Windows,
}

/// A key store [`choose`] can pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyBackend {
    SecretService,
    Windows,
}

impl std::fmt::Display for KeyBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SecretService => "the OS keyring (Secret Service)",
            Self::Windows => "Windows DPAPI (through WSL interop)",
        })
    }
}

/// Everything [`choose`] decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyFacts {
    pub choice: KeyStoreChoice,
    /// The DPAPI-sealed key file exists.
    pub dpapi_key_exists: bool,
    /// The vault file exists.
    pub vault_exists: bool,
    pub wsl: Wsl,
}

/// Picks the key store. An explicit choice wins; otherwise an existing key
/// stays where it is, so nothing moves under an existing vault:
///
/// 1. a DPAPI key file means Windows;
/// 2. a vault without one means the OS keyring (where every vault's key was
///    before the Windows store existed);
/// 3. a fresh install uses Windows on WSL with interop (stock WSL has no
///    usable keyring), and the OS keyring elsewhere.
///
/// Whatever it picks, [`crate::VaultStore::open`] still refuses to mint a
/// new key next to an existing vault.
#[expect(
    clippy::match_same_arms,
    reason = "one arm per rule, in order, reads as the rule list above"
)]
pub fn choose(facts: &KeyFacts) -> KeyBackend {
    match facts.choice {
        KeyStoreChoice::SecretService => KeyBackend::SecretService,
        KeyStoreChoice::Windows => KeyBackend::Windows,
        KeyStoreChoice::Auto if facts.dpapi_key_exists => KeyBackend::Windows,
        KeyStoreChoice::Auto if facts.vault_exists => KeyBackend::SecretService,
        KeyStoreChoice::Auto if facts.wsl == Wsl::Interop => KeyBackend::Windows,
        KeyStoreChoice::Auto => KeyBackend::SecretService,
    }
}

/// The production [`KeyProvider`]: gathers the [`KeyFacts`] and delegates to
/// the store [`choose`] picks, afresh on every call. The vault is opened
/// lazily and failed opens are retried, so fixing the environment (enabling
/// interop, creating a keyring) takes effect without a restart, and a
/// momentary problem at startup can't lock a fresh install into the wrong
/// store. Within one open, `load_key` and `create_key` see the same facts:
/// nothing on disk changes between them.
#[derive(Debug, Clone)]
pub struct KeyResolver {
    vault_path: PathBuf,
    dpapi_key_path: PathBuf,
    choice: KeyStoreChoice,
    proc_root: PathBuf,
    powershell: Option<PathBuf>,
}

impl KeyResolver {
    /// For the vault at `vault_path`; the DPAPI key file sits next to it.
    pub fn new(vault_path: impl Into<PathBuf>, choice: KeyStoreChoice) -> Self {
        let vault_path = vault_path.into();
        Self {
            dpapi_key_path: vault_path.with_file_name(DPAPI_KEY_FILE),
            vault_path,
            choice,
            proc_root: PathBuf::from("/proc"),
            powershell: None,
        }
    }

    /// The facts as they are now.
    pub fn facts(&self) -> Result<KeyFacts, SecretError> {
        Ok(KeyFacts {
            choice: self.choice,
            dpapi_key_exists: crate::fs::exists(&self.dpapi_key_path)?,
            vault_exists: crate::fs::exists(&self.vault_path)?,
            wsl: wsl::detect_in(&self.proc_root),
        })
    }

    /// The store the vault would use if it opened now.
    pub fn backend(&self) -> Result<KeyBackend, SecretError> {
        self.facts().map(|facts| choose(&facts))
    }

    fn provider(&self) -> Result<Box<dyn KeyProvider>, SecretError> {
        let facts = self.facts()?;
        Ok(match choose(&facts) {
            KeyBackend::SecretService => Box::new(KeyringKeyProvider),
            KeyBackend::Windows => {
                let powershell = self
                    .powershell
                    .clone()
                    .or_else(wsl::find_powershell)
                    .ok_or_else(|| no_powershell(facts.wsl))?;
                Box::new(DpapiKeyProvider::new(&self.dpapi_key_path, powershell))
            }
        })
    }
}

fn no_powershell(wsl: Wsl) -> SecretError {
    let why = match wsl {
        Wsl::Interop => format!(
            "powershell.exe is not on PATH or at {}; set {} to its path",
            wsl::DEFAULT_POWERSHELL,
            wsl::POWERSHELL_ENV
        ),
        Wsl::NoInterop => "WSL interop is disabled, so Windows programs can't run \
                           (enable [interop] in /etc/wsl.conf), or use --key-store secret-service"
            .to_owned(),
        Wsl::NotWsl => "this is not WSL; use --key-store secret-service".to_owned(),
    };
    SecretError::WindowsKeyUnavailable(why)
}

impl KeyProvider for KeyResolver {
    fn load_key(&self) -> Result<Option<DataKey>, SecretError> {
        self.provider()?.load_key()
    }

    fn create_key(&self) -> Result<DataKey, SecretError> {
        self.provider()?.create_key()
    }
}

#[cfg(test)]
impl KeyResolver {
    fn with_environment(
        mut self,
        proc_root: &std::path::Path,
        powershell: Option<PathBuf>,
    ) -> Self {
        self.proc_root = proc_root.to_path_buf();
        self.powershell = powershell;
        self
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn facts(choice: KeyStoreChoice, dpapi: bool, vault: bool, wsl: Wsl) -> KeyFacts {
        KeyFacts {
            choice,
            dpapi_key_exists: dpapi,
            vault_exists: vault,
            wsl,
        }
    }

    #[test]
    fn explicit_choices_always_win() {
        for dpapi in [false, true] {
            for vault in [false, true] {
                for wsl in [Wsl::NotWsl, Wsl::NoInterop, Wsl::Interop] {
                    let f = |choice| facts(choice, dpapi, vault, wsl);
                    assert_eq!(
                        choose(&f(KeyStoreChoice::SecretService)),
                        KeyBackend::SecretService
                    );
                    assert_eq!(choose(&f(KeyStoreChoice::Windows)), KeyBackend::Windows);
                }
            }
        }
    }

    #[test]
    fn auto_keeps_an_existing_key_where_it_is() {
        use KeyStoreChoice::Auto;
        for wsl in [Wsl::NotWsl, Wsl::NoInterop, Wsl::Interop] {
            // A DPAPI key file wins, with or without a vault.
            assert_eq!(choose(&facts(Auto, true, true, wsl)), KeyBackend::Windows);
            assert_eq!(choose(&facts(Auto, true, false, wsl)), KeyBackend::Windows);
            // A vault without one: its key is in the keyring, even on WSL.
            assert_eq!(
                choose(&facts(Auto, false, true, wsl)),
                KeyBackend::SecretService
            );
        }
    }

    #[test]
    fn auto_on_a_fresh_install_uses_windows_only_on_wsl_with_interop() {
        use KeyStoreChoice::Auto;
        assert_eq!(
            choose(&facts(Auto, false, false, Wsl::Interop)),
            KeyBackend::Windows
        );
        assert_eq!(
            choose(&facts(Auto, false, false, Wsl::NoInterop)),
            KeyBackend::SecretService
        );
        assert_eq!(
            choose(&facts(Auto, false, false, Wsl::NotWsl)),
            KeyBackend::SecretService
        );
    }

    fn wsl_proc() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let binfmt = dir.path().join("sys/fs/binfmt_misc");
        std::fs::create_dir_all(&binfmt).unwrap();
        std::fs::write(binfmt.join("WSLInterop"), "enabled\n").unwrap();
        dir
    }

    /// Stands in for `powershell.exe`: seals by base64-encoding a prefix.
    fn fake_powershell(dir: &Path) -> PathBuf {
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
        path
    }

    /// End to end on a fake WSL: a fresh vault gets a DPAPI key, and keeps
    /// using it when reopened.
    #[test]
    fn a_fresh_vault_on_wsl_is_keyed_with_dpapi_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let proc = wsl_proc();
        let vault_path = dir.path().join("data").join("secrets.vault");
        let resolver = || {
            KeyResolver::new(&vault_path, KeyStoreChoice::Auto)
                .with_environment(proc.path(), Some(fake_powershell(dir.path())))
        };
        assert_eq!(resolver().backend().unwrap(), KeyBackend::Windows);
        {
            let vault = crate::VaultStore::open(&vault_path, &resolver()).unwrap();
            crate::SecretStore::set(&vault, "k", "v").unwrap();
        }
        assert!(dir.path().join("data").join(DPAPI_KEY_FILE).exists());
        let vault = crate::VaultStore::open(&vault_path, &resolver()).unwrap();
        assert_eq!(
            crate::SecretStore::get(&vault, "k").unwrap().as_deref(),
            Some("v")
        );
    }

    /// An existing vault without a DPAPI key file stays on the keyring, even
    /// on WSL: no new key is ever minted next to it.
    #[test]
    fn an_existing_keyring_vault_on_wsl_stays_on_the_keyring() {
        let dir = tempfile::tempdir().unwrap();
        let proc = wsl_proc();
        let vault_path = dir.path().join("secrets.vault");
        std::fs::write(&vault_path, b"existing").unwrap();
        let resolver = KeyResolver::new(&vault_path, KeyStoreChoice::Auto)
            .with_environment(proc.path(), Some(fake_powershell(dir.path())));
        assert_eq!(resolver.backend().unwrap(), KeyBackend::SecretService);
    }

    #[test]
    fn windows_without_powershell_explains_why() {
        let dir = tempfile::tempdir().unwrap();
        let not_wsl = tempfile::tempdir().unwrap();
        let resolver = KeyResolver::new(dir.path().join("secrets.vault"), KeyStoreChoice::Windows)
            .with_environment(not_wsl.path(), None);
        // `find_powershell` runs for real here; on a non-WSL test host there
        // is none to find.
        if wsl::find_powershell().is_none() {
            match resolver.load_key() {
                Err(SecretError::WindowsKeyUnavailable(why)) => {
                    assert!(why.contains("not WSL"), "{why}");
                }
                other => panic!("expected WindowsKeyUnavailable, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_dpapi_key_file_sits_next_to_the_vault() {
        let resolver = KeyResolver::new("/data/gavmo/secrets.vault", KeyStoreChoice::Auto);
        assert_eq!(
            resolver.dpapi_key_path,
            Path::new("/data/gavmo/vault-key.dpapi")
        );
    }
}
