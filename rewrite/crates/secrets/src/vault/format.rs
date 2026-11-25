//! The vault file's encoding. Pure (no I/O), so every corner is tested.
//!
//! Layout: `MAGIC(2) | VERSION(1) | nonce(24) | ciphertext+tag`, with a fresh
//! random nonce per write (safe for `XChaCha20`). The 3-byte header is
//! authenticated as associated data, so the version can't be altered
//! undetected. The plaintext is the JSON map of secrets.

use chacha20poly1305::aead::{Aead as _, Generate as _, KeyInit as _, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use super::data_key::DataKey;
use super::secret_map::SecretMap;
use crate::FormatError;

const MAGIC: [u8; 2] = *b"GV";
const VERSION: u8 = 2;
const HEADER: [u8; 3] = [MAGIC[0], MAGIC[1], VERSION];
const NONCE_LEN: usize = 24;

fn cipher(key: &DataKey) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&Key::from(*key.as_bytes()))
}

/// Encrypts `map` into a vault file.
pub(crate) fn encrypt(key: &DataKey, map: &SecretMap) -> Result<Vec<u8>, FormatError> {
    let plaintext = Zeroizing::new(map.to_json().map_err(FormatError::Corrupt)?);
    let nonce = XNonce::generate();
    let ciphertext = cipher(key)
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: &HEADER,
            },
        )
        .map_err(|chacha20poly1305::aead::Error| FormatError::Encrypt)?;
    let mut out = Vec::with_capacity(HEADER.len() + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&HEADER);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts a vault file. Corrupt, foreign or tampered files are errors
/// rather than an empty map, so secrets are never silently lost.
pub(crate) fn decrypt(key: &DataKey, bytes: &[u8]) -> Result<SecretMap, FormatError> {
    let (header, rest) = bytes
        .split_first_chunk::<3>()
        .ok_or(FormatError::Truncated)?;
    let [m0, m1, version] = *header;
    if [m0, m1] != MAGIC {
        return Err(FormatError::NotAVault);
    }
    if version != VERSION {
        return Err(FormatError::UnsupportedVersion(version));
    }
    let (nonce, ciphertext) = rest
        .split_first_chunk::<NONCE_LEN>()
        .ok_or(FormatError::Truncated)?;
    let plaintext = Zeroizing::new(
        cipher(key)
            .decrypt(
                &XNonce::from(*nonce),
                Payload {
                    msg: ciphertext,
                    aad: header,
                },
            )
            // `aead::Error` is deliberately opaque: nothing to keep.
            .map_err(|chacha20poly1305::aead::Error| FormatError::Decrypt)?,
    );
    SecretMap::from_json(&plaintext).map_err(FormatError::Corrupt)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::*;
    use crate::vault::data_key::KEY_LEN;

    fn key(byte: u8) -> DataKey {
        DataKey::from_bytes([byte; KEY_LEN])
    }

    fn map(pairs: &[(&str, &str)]) -> SecretMap {
        let mut map = SecretMap::default();
        for (k, v) in pairs {
            map.insert(k, v);
        }
        map
    }

    /// Written by the previous implementation (key: 32 bytes of 0x42), so
    /// existing vaults are known to open.
    const EARLIER_VERSION_VAULT: &str = "R1YCKM5CPoCpMebfPeEYFM4uqlGXyg0tiyqfFxhRqN3/YnPBUwBBHrFDT0uJbUZ4r6/5OIqByc3enEYTj2Vr8D6lSffh5GofUPUsT1Sk3qPJI7ijC5W6fT2Ji0w1UE02sn6B+0B5RYQIbiFCCmYd0L6wxwpDLUKe6MGYL0Ok3TLVPPGqedxohSQudHtKY2SGSg==";

    #[test]
    fn reads_a_vault_written_by_the_previous_implementation() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(EARLIER_VERSION_VAULT)
            .unwrap();
        let map = decrypt(&key(0x42), &bytes).unwrap();
        assert_eq!(
            map.get("/home/me/repo\u{1f}jira-cred"),
            Some(r#"{"token":"abc"}"#)
        );
        assert_eq!(
            map.get("gavmo:providers\u{1f}vertex/api_key"),
            Some("sk-test")
        );
    }

    #[test]
    fn round_trips_and_never_contains_plaintext() {
        let bytes = encrypt(&key(1), &map(&[("k", "super-secret-value")])).unwrap();
        assert_eq!(&bytes[..3], b"GV\x02");
        assert!(!bytes.windows(18).any(|w| w == b"super-secret-value"));
        let decoded = decrypt(&key(1), &bytes).unwrap();
        assert_eq!(decoded.get("k"), Some("super-secret-value"));
    }

    /// The header is bound into the ciphertext as associated data: without
    /// it (as version 1 encrypted), the same bytes don't decrypt.
    #[test]
    fn the_header_is_authenticated() {
        let bytes = encrypt(&key(1), &map(&[("k", "v")])).unwrap();
        let (nonce, ciphertext) = bytes[3..].split_at(NONCE_LEN);
        let nonce = XNonce::from(<[u8; NONCE_LEN]>::try_from(nonce).unwrap());
        let open = |aad: &[u8]| {
            cipher(&key(1)).decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
        };
        assert!(open(&HEADER).is_ok());
        assert!(open(&[]).is_err());
        assert!(open(b"GV\x01").is_err());
    }

    #[test]
    fn a_changed_nonce_fails_to_decrypt() {
        let mut bytes = encrypt(&key(1), &map(&[("k", "v")])).unwrap();
        bytes[3] ^= 0xff;
        assert!(matches!(
            decrypt(&key(1), &bytes),
            Err(FormatError::Decrypt)
        ));
    }

    #[test]
    fn wrong_key_and_tampering_fail_closed() {
        let bytes = encrypt(&key(1), &map(&[("k", "v")])).unwrap();
        assert!(matches!(
            decrypt(&key(2), &bytes),
            Err(FormatError::Decrypt)
        ));
        let mut tampered = bytes;
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(matches!(
            decrypt(&key(1), &tampered),
            Err(FormatError::Decrypt)
        ));
    }

    #[test]
    fn rejects_truncated_foreign_old_and_future_files_without_panicking() {
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
            Err(FormatError::NotAVault)
        ));
        for version in [1, 3] {
            let mut file = vec![0u8; 64];
            file[..2].copy_from_slice(&MAGIC);
            file[2] = version;
            assert!(matches!(
                decrypt(&key(1), &file),
                Err(FormatError::UnsupportedVersion(v)) if v == version
            ));
        }
    }

    /// Authentic ciphertext whose plaintext isn't the JSON map: a bug in a
    /// writer, not tampering, so it is `Corrupt` rather than `Decrypt`.
    #[test]
    fn authentic_but_non_json_contents_are_corrupt() {
        let nonce = XNonce::generate();
        let payload = Payload {
            msg: b"not json",
            aad: &HEADER,
        };
        let ciphertext = cipher(&key(1)).encrypt(&nonce, payload).unwrap();
        let mut bytes = HEADER.to_vec();
        bytes.extend_from_slice(nonce.as_slice());
        bytes.extend_from_slice(&ciphertext);
        assert!(matches!(
            decrypt(&key(1), &bytes),
            Err(FormatError::Corrupt(_))
        ));
    }

    #[test]
    fn each_write_uses_a_fresh_nonce() {
        let m = map(&[("k", "v")]);
        assert_ne!(encrypt(&key(9), &m).unwrap(), encrypt(&key(9), &m).unwrap());
    }
}
