//! The vault's data key and where it comes from.

use base64::Engine as _;
use chacha20poly1305::Key;
use chacha20poly1305::aead::Generate;
use zeroize::Zeroizing;

use crate::SecretError;

/// XChaCha20-Poly1305 uses a 256-bit key.
pub const KEY_LEN: usize = 32;

/// The `service` name all GAVMO entries live under in the OS keystore.
pub const KEYRING_SERVICE: &str = "gavmo";

/// Keyring entry holding the vault's base64-encoded data key.
pub const VAULT_KEY_ENTRY: &str = "vault-data-key";

/// The vault's 256-bit data key; zeroized on drop, redacted in `Debug`.
#[derive(Clone)]
pub struct DataKey(Zeroizing<[u8; KEY_LEN]>);

impl DataKey {
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// A fresh random key from the OS CSPRNG.
    pub fn generate() -> Self {
        Self::from_bytes(Key::generate().into())
    }

    pub(crate) fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    fn encode(&self) -> Zeroizing<String> {
        Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(self.as_bytes()))
    }

    fn decode(encoded: &str) -> Result<Self, SecretError> {
        let bytes = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|e| SecretError::BadKey(format!("not valid base64: {e}")))?,
        );
        let array: [u8; KEY_LEN] =
            bytes
                .as_slice()
                .try_into()
                .map_err(|_e: std::array::TryFromSliceError| {
                    SecretError::BadKey(format!("{} bytes, expected {KEY_LEN}", bytes.len()))
                })?;
        Ok(Self::from_bytes(array))
    }
}

impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataKey(<redacted>)")
    }
}

/// Supplies the vault's data key; abstracted so the crypto path is testable
/// without an OS keystore. Loading and creating are separate so the vault can
/// refuse to mint a new key next to an existing vault ([`SecretError::KeyMissing`]).
pub trait KeyProvider: Send + Sync {
    /// The existing key, or `Ok(None)` if none was ever created. `Err` means
    /// the backing store is unavailable, which makes the vault fail closed.
    fn load_key(&self) -> Result<Option<DataKey>, SecretError>;

    /// Creates and persists a new key. Only called when no vault exists yet.
    fn create_key(&self) -> Result<DataKey, SecretError>;
}

/// Production [`KeyProvider`]: the key lives in the [`VAULT_KEY_ENTRY`] keyring
/// entry.
#[derive(Debug, Default, Clone)]
pub struct KeyringKeyProvider;

impl KeyringKeyProvider {
    fn entry() -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(KEYRING_SERVICE, VAULT_KEY_ENTRY).map_err(SecretError::KeyUnavailable)
    }
}

impl KeyProvider for KeyringKeyProvider {
    fn load_key(&self) -> Result<Option<DataKey>, SecretError> {
        match Self::entry()?.get_password() {
            Ok(encoded) => DataKey::decode(&Zeroizing::new(encoded)).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::KeyUnavailable(e)),
        }
    }

    fn create_key(&self) -> Result<DataKey, SecretError> {
        let key = DataKey::generate();
        Self::entry()?
            .set_password(&key.encode())
            .map_err(SecretError::KeyUnavailable)?;
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_wrong_length_and_bad_base64() {
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        assert!(matches!(
            DataKey::decode(&short),
            Err(SecretError::BadKey(_))
        ));
        assert!(matches!(DataKey::decode("!!"), Err(SecretError::BadKey(_))));
        let key = DataKey::generate();
        assert_eq!(
            DataKey::decode(&key.encode()).unwrap().as_bytes(),
            key.as_bytes()
        );
    }

    #[test]
    fn debug_is_redacted() {
        assert_eq!(
            format!("{:?}", DataKey::from_bytes([7; KEY_LEN])),
            "DataKey(<redacted>)"
        );
    }
}
