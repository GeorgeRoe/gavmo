//! The data key in the OS keyring, over the freedesktop Secret Service.

use zeroize::Zeroizing;

use super::{DataKey, KeyProvider};
use crate::SecretError;

/// The `service` name all GAVMO entries live under in the OS keystore.
pub const KEYRING_SERVICE: &str = "gavmo";

/// Keyring entry holding the vault's base64-encoded data key.
pub const VAULT_KEY_ENTRY: &str = "vault-data-key";

/// Production [`KeyProvider`] outside WSL: the key lives in the
/// [`VAULT_KEY_ENTRY`] keyring entry.
#[derive(Debug, Default, Clone)]
pub struct KeyringKeyProvider;

impl KeyringKeyProvider {
    /// Built from its own store (connecting to the Secret Service), not
    /// keyring-core's process-wide default store: no global state, and the
    /// key is read only when a vault opens.
    fn entry() -> Result<keyring_core::Entry, SecretError> {
        use keyring_core::api::CredentialStoreApi as _;
        zbus_secret_service_keyring_store::Store::new()
            .and_then(|store| store.build(KEYRING_SERVICE, VAULT_KEY_ENTRY, None))
            .map_err(SecretError::KeyUnavailable)
    }
}

impl KeyProvider for KeyringKeyProvider {
    fn load_key(&self) -> Result<Option<DataKey>, SecretError> {
        match Self::entry()?.get_password() {
            Ok(encoded) => DataKey::decode(&Zeroizing::new(encoded)).map(Some),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::KeyUnavailable(e)),
        }
    }

    fn create_key(&self) -> Result<DataKey, SecretError> {
        let key = DataKey::generate();
        Self::entry()?
            .set_password(&key.encode())
            .map_err(store_error)?;
        Ok(key)
    }
}

/// A new item goes into the keyring the `default` alias names. A Secret
/// Service with no such keyring (stock WSL, headless boxes: nothing ever
/// created the "login" keyring) answers "no result", which the store reports
/// as the cryptic "result not returned from SS API"; say what it means.
fn store_error(e: keyring_core::Error) -> SecretError {
    let no_default = matches!(
        &e,
        keyring_core::Error::NoStorageAccess(inner)
            if matches!(
                inner.downcast_ref::<secret_service::Error>(),
                Some(secret_service::Error::NoResult)
            )
    );
    if no_default {
        SecretError::NoDefaultKeyring
    } else {
        SecretError::KeyUnavailable(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_default_keyring_gets_its_own_error() {
        let no_result =
            keyring_core::Error::NoStorageAccess(Box::new(secret_service::Error::NoResult));
        assert!(matches!(
            store_error(no_result),
            SecretError::NoDefaultKeyring
        ));

        let locked = keyring_core::Error::NoStorageAccess(Box::new(secret_service::Error::Locked));
        assert!(matches!(
            store_error(locked),
            SecretError::KeyUnavailable(_)
        ));
        let other = keyring_core::Error::NoStorageAccess("other".into());
        assert!(matches!(store_error(other), SecretError::KeyUnavailable(_)));
    }
}
