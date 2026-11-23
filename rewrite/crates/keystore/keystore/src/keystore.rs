//! [`Keystore`]: holds one secret, created once.

use zeroize::Zeroizing;

use crate::Error;

/// Secret bytes, wiped from memory when dropped.
pub type SecretBytes = Zeroizing<Vec<u8>>;

/// Holds one secret (the vault's data key) somewhere the user's login
/// protects, outside the files it encrypts.
///
/// A secret is created once and never replaced: replacing it would make
/// everything encrypted with the old one unrecoverable without saying so.
/// Takes `&self` and is `Send + Sync` because keystores are shared and
/// synchronize internally. Calls block, possibly for seconds (a keyring
/// prompting to unlock, a process starting).
pub trait Keystore: Send + Sync {
    /// What this keystore is, for logs and errors, e.g. "the OS keyring
    /// (Secret Service)".
    fn name(&self) -> &'static str;

    /// The secret, or `Ok(None)` if none was ever created. An error means
    /// it can't be told, and callers must fail closed.
    fn load(&self) -> Result<Option<SecretBytes>, Error>;

    /// Stores `secret`. If one is already stored, [`Error::AlreadyExists`]
    /// and the stored one is kept.
    fn create(&self, secret: &[u8]) -> Result<(), Error>;
}
