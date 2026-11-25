//! [`SecretKey`]: where a secret is stored, made of a scope and a name.

use std::fmt;

use validated::{CharSet, Chars, Label, Len, Named, Validated};

/// The name part of a [`SecretKey`]: `A-Z a-z 0-9 _ - . /`, 1 to 256 long.
/// It can't contain the separator, so a key splits uniquely at its last
/// separator whatever the scope holds.
pub type KeyName = Validated<String, Named<KeyNameLabel, (Len<1, 256>, Chars<KeyNameChars>)>>;

pub struct KeyNameLabel;
impl Label for KeyNameLabel {
    const NAME: &'static str = "secret key name";
}

pub struct KeyNameChars;
impl CharSet for KeyNameChars {
    const ALLOWED: &'static str = "A-Z, a-z, 0-9, _, -, . and /";
    fn contains(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')
    }
}

/// Separates scope and name: not typeable, and refused in [`KeyName`].
const SEPARATOR: char = '\u{1f}';

/// The key a secret is stored under: `<scope>\u{1f}<name>`.
///
/// The scope says whose secret it is (a workspace's absolute host path, or
/// a fixed scope such as the model providers'); the name says which. Two
/// different (scope, name) pairs never make the same key. The text is what
/// earlier versions stored, so existing vaults keep working.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretKey(String);

impl SecretKey {
    pub fn new(scope: &str, name: &KeyName) -> Self {
        Self(format!("{scope}{SEPARATOR}{name}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Shows the separator escaped, so logs stay readable.
impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> KeyName {
        KeyName::new(text.to_owned()).unwrap()
    }

    /// The exact text earlier versions stored, for both kinds of key.
    #[test]
    fn keys_are_unchanged_from_earlier_versions() {
        assert_eq!(
            SecretKey::new("/home/me/repo", &name("jira-cred")).as_str(),
            "/home/me/repo\u{1f}jira-cred"
        );
        assert_eq!(
            SecretKey::new("gavmo:providers", &name("vertex/api_key")).as_str(),
            "gavmo:providers\u{1f}vertex/api_key"
        );
    }

    #[test]
    fn different_pairs_never_collide() {
        assert_ne!(
            SecretKey::new("a", &name("bc")),
            SecretKey::new("ab", &name("c"))
        );
        // A separator in the scope can't forge another pair's key: the name
        // can't hold one, so the key splits at its last separator.
        assert_ne!(
            SecretKey::new("/a\u{1f}b", &name("c")),
            SecretKey::new("/a", &name("b"))
        );
    }

    #[test]
    fn names_refuse_the_separator_and_percent() {
        for bad in ["a\u{1f}b", "a%1Fb", "", "a b"] {
            assert!(KeyName::new(bad.to_owned()).is_err(), "{bad:?}");
        }
        assert_eq!(
            KeyName::new("a%b".to_owned()).unwrap_err().to_string(),
            "secret key name \"a%b\" contains '%' (allowed: A-Z, a-z, 0-9, _, -, . and /)"
        );
    }

    #[test]
    fn debug_escapes_the_separator() {
        assert_eq!(
            format!("{:?}", SecretKey::new("/ws", &name("n"))),
            "\"/ws\\u{1f}n\""
        );
    }
}
