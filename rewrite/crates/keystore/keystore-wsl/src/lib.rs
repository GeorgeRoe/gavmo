//! A [`Keystore`](keystore::Keystore) for GAVMO under WSL: the key sealed
//! with Windows DPAPI.
//!
//! A Linux process can't call Windows APIs, but WSL interop lets it run
//! Windows programs: Windows PowerShell (on every Windows 10/11) exposes
//! DPAPI as `[Security.Cryptography.ProtectedData]`. The key is sealed to the
//! current Windows user and the sealed blob is kept next to the vault, so
//! signing in to Windows is what unlocks it - no Linux keyring needed.
//!
//! Like an unlocked keyring, any process running as that Windows user
//! (including any WSL process, through interop) can unseal it.

mod dpapi_error;
mod dpapi_keystore;
mod key_file;
mod script;

pub use dpapi_error::{DpapiError, Op};
pub use dpapi_keystore::DpapiKeystore;
pub use key_file::KEY_FILE;
