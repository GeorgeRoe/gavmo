//! Somewhere outside the vault to keep the vault's key: the [`Keystore`]
//! trait, implemented by one crate per platform next to this one.

mod error;
mod keystore;
#[cfg(any(test, feature = "test-util"))]
mod memory;

pub use error::{BoxError, Error};
pub use keystore::{Keystore, SecretBytes};
#[cfg(any(test, feature = "test-util"))]
pub use memory::MemoryKeystore;
