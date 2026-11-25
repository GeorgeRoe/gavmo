//! [`choose`]: the rule for which keystore holds the vault's key.

use std::fmt;

use wsl::Wsl;

/// The user's choice of keystore (`--key-store`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeystoreChoice {
    /// Wherever the key already is; for a new vault, Windows on WSL with
    /// interop and the OS keyring everywhere else (see [`choose`]).
    #[default]
    Auto,
    /// Always the OS keyring (Secret Service).
    SecretService,
    /// Always Windows DPAPI, through WSL interop.
    Windows,
}

/// A keystore [`choose`] can pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    SecretService,
    Windows,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SecretService => "the OS keyring (Secret Service)",
            Self::Windows => "Windows DPAPI (through WSL interop)",
        })
    }
}

/// Everything [`choose`] decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub choice: KeystoreChoice,
    /// The DPAPI key file exists.
    pub dpapi_key_exists: bool,
    /// The vault file exists.
    pub vault_exists: bool,
    pub wsl: Wsl,
}

/// Picks the keystore. An explicit choice wins; otherwise an existing key
/// stays where it is, so nothing moves under an existing vault:
///
/// 1. a DPAPI key file means Windows;
/// 2. a vault without one means the OS keyring (where every vault's key was
///    before the Windows keystore existed);
/// 3. a fresh install uses Windows on WSL with interop (stock WSL has no
///    usable keyring), and the OS keyring elsewhere.
///
/// Whatever it picks, the vault still refuses to create a new key next to
/// an existing vault.
#[expect(
    clippy::match_same_arms,
    reason = "one arm per rule, in order, reads as the rule list above"
)]
pub fn choose(facts: &Facts) -> Backend {
    match facts.choice {
        KeystoreChoice::SecretService => Backend::SecretService,
        KeystoreChoice::Windows => Backend::Windows,
        KeystoreChoice::Auto if facts.dpapi_key_exists => Backend::Windows,
        KeystoreChoice::Auto if facts.vault_exists => Backend::SecretService,
        KeystoreChoice::Auto if facts.wsl == Wsl::Interop => Backend::Windows,
        KeystoreChoice::Auto => Backend::SecretService,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_WSL: [Wsl; 3] = [Wsl::NotWsl, Wsl::NoInterop, Wsl::Interop];

    fn facts(choice: KeystoreChoice, dpapi: bool, vault: bool, wsl: Wsl) -> Facts {
        Facts {
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
                for wsl in EVERY_WSL {
                    let f = |choice| facts(choice, dpapi, vault, wsl);
                    assert_eq!(
                        choose(&f(KeystoreChoice::SecretService)),
                        Backend::SecretService
                    );
                    assert_eq!(choose(&f(KeystoreChoice::Windows)), Backend::Windows);
                }
            }
        }
    }

    #[test]
    fn auto_keeps_an_existing_key_where_it_is() {
        use KeystoreChoice::Auto;
        for wsl in EVERY_WSL {
            // A DPAPI key file wins, with or without a vault.
            assert_eq!(choose(&facts(Auto, true, true, wsl)), Backend::Windows);
            assert_eq!(choose(&facts(Auto, true, false, wsl)), Backend::Windows);
            // A vault without one: its key is in the keyring, even on WSL.
            assert_eq!(
                choose(&facts(Auto, false, true, wsl)),
                Backend::SecretService
            );
        }
    }

    #[test]
    fn auto_on_a_fresh_install_uses_windows_only_on_wsl_with_interop() {
        use KeystoreChoice::Auto;
        assert_eq!(
            choose(&facts(Auto, false, false, Wsl::Interop)),
            Backend::Windows
        );
        assert_eq!(
            choose(&facts(Auto, false, false, Wsl::NoInterop)),
            Backend::SecretService
        );
        assert_eq!(
            choose(&facts(Auto, false, false, Wsl::NotWsl)),
            Backend::SecretService
        );
    }
}
