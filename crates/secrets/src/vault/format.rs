//! Vault file encoding. Pure (no I/O), so every corner is unit-tested.
//!
//! Layout: `MAGIC(2) | VERSION(1) | nonce(24) | ciphertext+tag`, with a fresh
//! random nonce per write (safe for `XChaCha20`). The plaintext is the JSON
//! map of secrets.
//!
//! - v2 (written now) authenticates the 3-byte header as associated data,
//!   so the version byte can't be altered undetected (e.g. to downgrade).
//! - v1 (read only, for migration) didn't.

use std::collections::HashMap;

use chacha20poly1305::aead::{Aead, Generate, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, Zeroizing};

use crate::{DataKey, SecretError};

pub(super) const MAGIC: [u8; 2] = *b"GV";
pub(super) const VERSION_V1: u8 = 1;
pub(super) const VERSION: u8 = 2;
const NONCE_LEN: usize = 24;

/// The decrypted secrets; values are zeroized when the map is dropped
/// (including the copies made for each update).
#[derive(Clone, Default)]
pub(super) struct SecretMap(pub(super) HashMap<String, String>);

impl Drop for SecretMap {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

fn cipher(key: &DataKey) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&Key::from(*key.as_bytes()))
}

/// Encrypts `map` in the current (v2) format.
pub(super) fn encrypt(key: &DataKey, map: &SecretMap) -> Result<Vec<u8>, SecretError> {
    let plaintext = Zeroizing::new(serde_json::to_vec(&map.0).map_err(SecretError::Corrupt)?);
    let header = [MAGIC[0], MAGIC[1], VERSION];
    let nonce = XNonce::generate();
    let ciphertext = cipher(key)
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: &header,
            },
        )
        .map_err(|chacha20poly1305::aead::Error| {
            SecretError::Backend("encrypting the vault failed".to_owned())
        })?;
    let mut out = Vec::with_capacity(header.len() + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts a vault file of either version; returns the map and the version
/// it was in. Corrupt, foreign or tampered files are errors rather than an
/// empty map, so secrets are never silently lost.
pub(super) fn decrypt(key: &DataKey, bytes: &[u8]) -> Result<(SecretMap, u8), SecretError> {
    let (header, rest) = bytes
        .split_first_chunk::<3>()
        .ok_or(SecretError::Truncated)?;
    if header[..2] != MAGIC {
        return Err(SecretError::NotAVault);
    }
    let version = header[2];
    let aad: &[u8] = match version {
        VERSION => header,
        VERSION_V1 => &[],
        other => return Err(SecretError::UnsupportedVersion(other)),
    };
    let (nonce, ciphertext) = rest
        .split_first_chunk::<NONCE_LEN>()
        .ok_or(SecretError::Truncated)?;
    let plaintext = Zeroizing::new(
        cipher(key)
            .decrypt(
                &XNonce::from(*nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            // `aead::Error` is deliberately opaque: nothing to keep.
            .map_err(|chacha20poly1305::aead::Error| SecretError::Decrypt)?,
    );
    let map = serde_json::from_slice(&plaintext).map_err(SecretError::Corrupt)?;
    Ok((SecretMap(map), version))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::KEY_LEN;

    fn key(byte: u8) -> DataKey {
        DataKey::from_bytes([byte; KEY_LEN])
    }

    fn map(pairs: &[(&str, &str)]) -> SecretMap {
        SecretMap(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    /// The v1 writer, kept to produce migration fixtures.
    pub(in crate::vault) fn encrypt_v1(key: &DataKey, map: &SecretMap) -> Vec<u8> {
        let plaintext = serde_json::to_vec(&map.0).unwrap();
        let nonce = XNonce::generate();
        let ciphertext = cipher(key).encrypt(&nonce, plaintext.as_ref()).unwrap();
        let mut out = vec![MAGIC[0], MAGIC[1], VERSION_V1];
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ciphertext);
        out
    }

    #[test]
    fn round_trips_in_v2_and_never_contains_plaintext() {
        let bytes = encrypt(&key(1), &map(&[("k", "super-secret-value")])).unwrap();
        assert_eq!(&bytes[..3], &[b'G', b'V', VERSION]);
        assert!(!bytes.windows(18).any(|w| w == b"super-secret-value"));
        let (decoded, version) = decrypt(&key(1), &bytes).unwrap();
        assert_eq!(version, VERSION);
        assert_eq!(
            decoded.0.get("k").map(String::as_str),
            Some("super-secret-value")
        );
    }

    #[test]
    fn reads_v1_for_migration() {
        let bytes = encrypt_v1(&key(1), &map(&[("k", "v")]));
        let (decoded, version) = decrypt(&key(1), &bytes).unwrap();
        assert_eq!(
            (version, decoded.0.get("k").map(String::as_str)),
            (VERSION_V1, Some("v"))
        );
    }

    /// v2 authenticates the header: relabelling a v2 file as v1 (or anything
    /// else) must fail, not decrypt.
    #[test]
    fn the_v2_header_is_authenticated() {
        let mut bytes = encrypt(&key(1), &map(&[("k", "v")])).unwrap();
        bytes[2] = VERSION_V1;
        assert!(matches!(
            decrypt(&key(1), &bytes),
            Err(SecretError::Decrypt)
        ));
    }

    #[test]
    fn wrong_key_and_tampering_fail_closed() {
        let bytes = encrypt(&key(1), &map(&[("k", "v")])).unwrap();
        assert!(matches!(
            decrypt(&key(2), &bytes),
            Err(SecretError::Decrypt)
        ));
        let mut tampered = bytes;
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(matches!(
            decrypt(&key(1), &tampered),
            Err(SecretError::Decrypt)
        ));
    }

    #[test]
    fn rejects_truncated_foreign_and_future_files_without_panicking() {
        for len in 0..=(3 + NONCE_LEN) {
            let mut bytes = vec![0u8; len];
            if len >= 2 {
                bytes[..2].copy_from_slice(&MAGIC);
            }
            if len >= 3 {
                bytes[2] = VERSION;
            }
            assert!(decrypt(&key(1), &bytes).is_err(), "{len} bytes");
        }
        assert!(matches!(
            decrypt(&key(1), b"not-a-vault-file-at-all...................."),
            Err(SecretError::NotAVault)
        ));
        let mut future = vec![0u8; 64];
        future[..2].copy_from_slice(&MAGIC);
        future[2] = VERSION + 1;
        assert!(matches!(
            decrypt(&key(1), &future),
            Err(SecretError::UnsupportedVersion(3))
        ));
    }

    /// Authentic ciphertext whose plaintext isn't the JSON map: a bug in a
    /// writer, not tampering, so it is `Corrupt` rather than `Decrypt`.
    #[test]
    fn authentic_but_non_json_contents_are_corrupt() {
        let header = [MAGIC[0], MAGIC[1], VERSION];
        let nonce = XNonce::generate();
        let payload = Payload {
            msg: b"not json",
            aad: &header,
        };
        let ciphertext = cipher(&key(1)).encrypt(&nonce, payload).unwrap();
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(nonce.as_slice());
        bytes.extend_from_slice(&ciphertext);
        assert!(matches!(
            decrypt(&key(1), &bytes),
            Err(SecretError::Corrupt(_))
        ));
    }

    #[test]
    fn each_write_uses_a_fresh_nonce() {
        let m = map(&[("k", "v")]);
        assert_ne!(encrypt(&key(9), &m).unwrap(), encrypt(&key(9), &m).unwrap());
    }
}
