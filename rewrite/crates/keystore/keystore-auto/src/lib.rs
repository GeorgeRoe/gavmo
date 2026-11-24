//! Which [`Keystore`](keystore::Keystore) holds the vault's key: the OS
//! keyring or, on WSL, Windows DPAPI. [`choose`] is the rule;
//! [`AutoKeystore`] applies it afresh on every call.

mod auto_error;
mod auto_keystore;
mod choose;

pub use auto_error::AutoError;
pub use auto_keystore::AutoKeystore;
pub use choose::{Backend, Facts, KeystoreChoice, choose};
