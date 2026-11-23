//! [`Error`]: why a [`Keystore`](crate::Keystore) gave no answer.

/// A backend's own error, kept as the source of an [`Error`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// What went wrong, at the level a caller acts on: retry once the
/// environment is fixed, repair or move the stored key, or keep the key
/// that is already there. The backend's own error is the source.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The keystore can't be reached or refused (locked, not running, no
    /// interop, ...). Fixing the environment and retrying may work.
    #[error("{keystore} is unavailable: {source}")]
    Unavailable {
        keystore: &'static str,
        #[source]
        source: BoxError,
    },
    /// A secret is stored but is malformed or can't be unsealed.
    #[error("the key in {keystore} is invalid: {source}")]
    Invalid {
        keystore: &'static str,
        #[source]
        source: BoxError,
    },
    /// [`Keystore::create`](crate::Keystore::create) found a secret already
    /// stored, and kept it.
    #[error("{keystore} already holds a key")]
    AlreadyExists { keystore: &'static str },
}

impl Error {
    pub fn unavailable(keystore: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Unavailable {
            keystore,
            source: source.into(),
        }
    }

    pub fn invalid(keystore: &'static str, source: impl Into<BoxError>) -> Self {
        Self::Invalid {
            keystore,
            source: source.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_keystore_and_the_cause() {
        assert_eq!(
            Error::unavailable("the OS keyring", "locked").to_string(),
            "the OS keyring is unavailable: locked"
        );
        assert_eq!(
            Error::invalid("the OS keyring", "not base64").to_string(),
            "the key in the OS keyring is invalid: not base64"
        );
        assert_eq!(
            Error::AlreadyExists {
                keystore: "the OS keyring"
            }
            .to_string(),
            "the OS keyring already holds a key"
        );
    }
}
