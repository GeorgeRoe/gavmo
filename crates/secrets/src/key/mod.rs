//! The vault's data key and where it comes from: the OS keyring
//! ([`KeyringKeyProvider`]) or, on WSL, Windows DPAPI ([`DpapiKeyProvider`]),
//! picked per vault open by [`KeyResolver`].

mod dpapi;
mod keyring;
mod select;

use base64::Engine as _;
use chacha20poly1305::Key;
use chacha20poly1305::aead::Generate;
use zeroize::Zeroizing;

use crate::SecretError;

pub use dpapi::{DPAPI_KEY_FILE, DpapiKeyProvider};
pub use keyring::{KEYRING_SERVICE, KeyringKeyProvider, VAULT_KEY_ENTRY};
pub use select::{KeyBackend, KeyFacts, KeyResolver, KeyStoreChoice, choose};

/// XChaCha20-Poly1305 uses a 256-bit key.
pub const KEY_LEN: usize = 32;

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

    /// Base64, the form both key stores hold (their values are strings).
    pub(crate) fn encode(&self) -> Zeroizing<String> {
        Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(self.as_bytes()))
    }

    pub(crate) fn decode(encoded: &str) -> Result<Self, SecretError> {
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
