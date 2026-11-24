//! [`SecretServiceError`]: the Secret Service's side of a
//! [`keystore::Error`].

#[derive(Debug, thiserror::Error)]
pub enum SecretServiceError {
    /// The Secret Service is reachable but has no default keyring to store
    /// a new key in.
    #[error(
        "there is no default keyring to store the key in (common on WSL and \
         headless Linux, where nothing creates the \"login\" keyring): create one, \
         e.g. with seahorse or `gnome-keyring-daemon --unlock`; on WSL, \
         --key-store windows avoids the Linux keyring altogether"
    )]
    NoDefaultKeyring,
    /// Not running, locked, refused, ...
    #[error(transparent)]
    Keyring(#[from] keyring_core::Error),
    #[error("the stored key is not base64: {0}")]
    NotBase64(#[from] base64::DecodeError),
}

impl SecretServiceError {
    /// A new item goes into the keyring the `default` alias names. A Secret
    /// Service with no such keyring (stock WSL, headless boxes: nothing ever
    /// created the "login" keyring) answers "no result", which the store
    /// reports as the cryptic "result not returned from SS API"; say what
    /// it means.
    pub(crate) fn storing(e: keyring_core::Error) -> Self {
        let no_default = matches!(
            &e,
            keyring_core::Error::NoStorageAccess(inner)
                if matches!(
                    inner.downcast_ref::<secret_service::Error>(),
                    Some(secret_service::Error::NoResult)
                )
        );
        if no_default {
            Self::NoDefaultKeyring
        } else {
            Self::Keyring(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_default_keyring_gets_its_own_error() {
        let no_result =
            keyring_core::Error::NoStorageAccess(Box::new(secret_service::Error::NoResult));
        assert!(matches!(
            SecretServiceError::storing(no_result),
            SecretServiceError::NoDefaultKeyring
        ));

        let locked = keyring_core::Error::NoStorageAccess(Box::new(secret_service::Error::Locked));
        assert!(matches!(
            SecretServiceError::storing(locked),
            SecretServiceError::Keyring(_)
        ));
        let other = keyring_core::Error::NoStorageAccess("other".into());
        assert!(matches!(
            SecretServiceError::storing(other),
            SecretServiceError::Keyring(_)
        ));
    }
}
