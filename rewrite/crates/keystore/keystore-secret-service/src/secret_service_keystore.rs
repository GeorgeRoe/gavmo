//! [`SecretServiceKeystore`]: the vault key in one OS keyring entry.

use base64::Engine as _;
use keystore::{Error, Keystore, SecretBytes};
use zeroize::Zeroizing;

use crate::SecretServiceError;

/// The `service` all GAVMO entries live under in the OS keyring.
pub const SERVICE: &str = "gavmo";

/// The entry holding the vault's key, base64-encoded.
pub const ENTRY: &str = "vault-data-key";

const NAME: &str = "the OS keyring (Secret Service)";

/// The key in the [`SERVICE`]/[`ENTRY`] item of the OS keyring.
///
/// Connects afresh on each call, through its own store rather than
/// keyring-core's process-wide default: no global state, and the keyring is
/// only touched when the vault opens.
#[derive(Debug, Default, Clone, Copy)]
pub struct SecretServiceKeystore;

impl SecretServiceKeystore {
    fn entry() -> Result<keyring_core::Entry, Error> {
        use keyring_core::api::CredentialStoreApi as _;
        zbus_secret_service_keyring_store::Store::new()
            .and_then(|store| store.build(SERVICE, ENTRY, None))
            .map_err(unavailable)
    }
}

impl Keystore for SecretServiceKeystore {
    fn name(&self) -> &'static str {
        NAME
    }

    fn load(&self) -> Result<Option<SecretBytes>, Error> {
        let encoded = match Self::entry()?.get_password() {
            Ok(encoded) => Zeroizing::new(encoded),
            Err(keyring_core::Error::NoEntry) => return Ok(None),
            Err(e) => return Err(unavailable(e)),
        };
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map(|bytes| Some(Zeroizing::new(bytes)))
            .map_err(|e| Error::invalid(NAME, SecretServiceError::NotBase64(e)))
    }

    fn create(&self, secret: &[u8]) -> Result<(), Error> {
        if self.load()?.is_some() {
            return Err(Error::AlreadyExists { keystore: NAME });
        }
        let encoded = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(secret));
        Self::entry()?
            .set_password(&encoded)
            .map_err(|e| Error::unavailable(NAME, SecretServiceError::storing(e)))
    }
}

fn unavailable(e: keyring_core::Error) -> Error {
    Error::unavailable(NAME, SecretServiceError::Keyring(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set in the child process of the test below.
    const NO_BUS_CHILD: &str = "GAVMO_KEYSTORE_TEST_NO_BUS_CHILD";

    /// Without a reachable Secret Service, every call fails closed. The real
    /// work runs in a child process whose session bus points at nothing, so
    /// the keyring of whoever runs the tests is never touched (the
    /// environment can't be changed safely in-process).
    #[test]
    fn fails_closed_without_a_secret_service() {
        let unavailable = |r: Result<(), Error>| matches!(r, Err(Error::Unavailable { .. }));
        if std::env::var_os(NO_BUS_CHILD).is_some() {
            assert!(unavailable(SecretServiceKeystore.load().map(drop)));
            assert!(unavailable(SecretServiceKeystore.create(b"key")));
            return;
        }
        let dir = std::env::temp_dir().join(format!("no-bus-{}", std::process::id()));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "secret_service_keystore::tests::fails_closed_without_a_secret_service",
                "--nocapture",
            ])
            .env(NO_BUS_CHILD, "1")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", dir.join("no-bus").display()),
            )
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn errors_name_the_keyring() {
        let err = Error::unavailable(NAME, SecretServiceError::NoDefaultKeyring);
        assert!(
            err.to_string().starts_with(
                "the OS keyring (Secret Service) is unavailable: there is no default keyring"
            ),
            "{err}"
        );
    }
}
