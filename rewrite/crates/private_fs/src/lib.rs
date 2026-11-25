//! Files only this user can read, in a directory only this user can enter.
//!
//! A [`PrivateDir`] is checked when created (absolute, a real directory,
//! owned by us, mode `0700`), so holding one means its files are private.
//! Files in it are named by a [`FileName`] and written atomically: a crash
//! leaves the old contents or the new, never a truncated file.
//!
//! Unix only: the daemon runs on Linux (WSL included).

mod error;
mod file_name;
mod lock_file;
mod private_dir;

pub use error::{Error, Op, Unsafe};
pub use file_name::FileName;
pub use lock_file::LockFile;
pub use private_dir::PrivateDir;
