//! [`SecretMap`]: the vault's decrypted contents.

use std::collections::HashMap;

use zeroize::Zeroize as _;

/// Values by key text ([`SecretKey::as_str`](crate::SecretKey::as_str)), as
/// stored in the vault's JSON. Values are wiped when the map is dropped
/// (including the copies made for each update) and when removed.
#[derive(Clone, Default)]
pub(crate) struct SecretMap(HashMap<String, String>);

impl SecretMap {
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub(crate) fn insert(&mut self, key: &str, value: &str) {
        if let Some(mut old) = self.0.insert(key.to_owned(), value.to_owned()) {
            old.zeroize();
        }
    }

    /// Whether there was a value to remove.
    pub(crate) fn remove(&mut self, key: &str) -> bool {
        self.0.remove(key).map(|mut old| old.zeroize()).is_some()
    }

    pub(crate) fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.0)
    }

    pub(crate) fn from_json(json: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(json).map(Self)
    }
}

impl Drop for SecretMap {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_replaces_and_removes() {
        let mut map = SecretMap::default();
        map.insert("k", "v1");
        map.insert("k", "v2");
        assert_eq!(map.get("k"), Some("v2"));
        assert!(map.remove("k"));
        assert!(!map.remove("k"));
        assert_eq!(map.get("k"), None);
    }

    #[test]
    fn round_trips_through_json() {
        let mut map = SecretMap::default();
        map.insert("a\u{1f}b", "{\"token\":\"x\"}");
        let back = SecretMap::from_json(&map.to_json().unwrap()).unwrap();
        assert_eq!(back.get("a\u{1f}b"), Some("{\"token\":\"x\"}"));
        assert!(SecretMap::from_json(b"[1]").is_err());
    }
}
