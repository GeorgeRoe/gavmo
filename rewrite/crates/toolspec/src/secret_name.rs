//! [`SecretName`]: what a tool calls the credential the user must supply.

use validated::{CharSet, Chars, Label, Len, Named, Validated};

/// The name of the secret a tool needs (a name, never a value).
pub type SecretName = Validated<String, Named<SecretLabel, (Len<1, 128>, Chars<SecretChars>)>>;

pub struct SecretLabel;
impl Label for SecretLabel {
    const NAME: &'static str = "secret name";
}

pub struct SecretChars;
impl CharSet for SecretChars {
    const ALLOWED: &'static str = "A-Z, a-z, 0-9, _, - and .";
    fn contains(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(name: &str) -> String {
        SecretName::new(name.to_owned()).unwrap_err().to_string()
    }

    #[test]
    fn accepts_names() {
        for name in ["s", "jira-cred", "my.secret_name-2", &"x".repeat(128)] {
            assert!(SecretName::new(name.to_owned()).is_ok(), "{name}");
        }
    }

    #[test]
    fn refuses_other_characters() {
        assert_eq!(
            error("a\u{1f}b"),
            "secret name \"a\\u{1f}b\" contains '\\u{1f}' (allowed: A-Z, a-z, 0-9, _, - and .)"
        );
        assert!(error("a b").contains("contains ' '"));
    }

    #[test]
    fn refuses_empty_and_long_names() {
        assert_eq!(error(""), "secret name \"\" must not be empty");
        assert!(error(&"x".repeat(129)).ends_with("is longer than 128 characters"));
    }
}
