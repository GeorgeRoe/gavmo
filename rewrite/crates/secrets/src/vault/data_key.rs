//! [`DataKey`]: the key the vault is encrypted with.

use std::fmt;

use chacha20poly1305::Key;
use chacha20poly1305::aead::Generate as _;
use zeroize::Zeroizing;

/// XChaCha20-Poly1305 uses a 256-bit key.
pub(crate) const KEY_LEN: usize = 32;

/// The vault's 256-bit key; wiped when dropped, redacted in `Debug`. Kept
/// by a [`Keystore`](keystore::Keystore) as its raw bytes.
#[derive(Clone)]
pub(crate) struct DataKey(Zeroizing<[u8; KEY_LEN]>);

impl DataKey {
    /// A fresh random key from the OS CSPRNG.
    pub(crate) fn generate() -> Self {
        Self::from_bytes(Key::generate().into())
    }

    pub(crate) fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The key a keystore holds, or `None` if it isn't [`KEY_LEN`] bytes.
    pub(crate) fn from_stored(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self::from_bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for DataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DataKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_keys_must_be_the_right_length() {
        assert!(DataKey::from_stored(&[0; 16]).is_none());
        assert!(DataKey::from_stored(&[0; 33]).is_none());
        let key = DataKey::generate();
        assert_eq!(
            DataKey::from_stored(key.as_bytes()).unwrap().as_bytes(),
            key.as_bytes()
        );
    }

    #[test]
    fn generated_keys_differ() {
        assert_ne!(
            DataKey::generate().as_bytes(),
            DataKey::generate().as_bytes()
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
