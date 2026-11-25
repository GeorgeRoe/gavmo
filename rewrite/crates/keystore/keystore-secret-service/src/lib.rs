//! A [`Keystore`](keystore::Keystore) in the OS keyring, over the
//! freedesktop Secret Service (GNOME Keyring, `KWallet`, ...).

mod error;
mod secret_service_keystore;

pub use error::SecretServiceError;
pub use secret_service_keystore::{ENTRY, SERVICE, SecretServiceKeystore};
