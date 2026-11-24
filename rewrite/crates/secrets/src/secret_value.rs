//! [`SecretValue`]: a stored secret, hidden from logs.

use std::fmt;

use zeroize::Zeroizing;

/// A secret (a token, a JSON-encoded credential, ...): wiped from memory
/// when dropped, and shown as `<redacted>` by `Debug`, so it can't be logged
/// by accident. Read it with [`Self::expose`].
#[derive(Clone)]
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// The secret itself. Named so every read stands out in review.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretValue {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let value = SecretValue::new("hunter2");
        assert_eq!(format!("{value:?}"), "SecretValue(<redacted>)");
        assert_eq!(
            format!("{:?}", Some(value.clone())),
            "Some(SecretValue(<redacted>))"
        );
        assert_eq!(value.expose(), "hunter2");
    }
}
