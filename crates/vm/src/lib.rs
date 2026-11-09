//! VM lifecycle management for GAVMO workspaces.
//!
//! This crate drives QEMU to boot, monitor, and stop the per-workspace
//! NixOS VMs that run `opencode web`. It is pure mechanism: the GAVMO
//! daemon owns port allocation, paths, and workspace identity and passes
//! them in via [`VmConfig`].

mod config;
mod disk;
mod error;
#[cfg(test)]
mod fake_qmp;
mod fsutil;
mod handle;
mod health;
mod process;
mod qemu;
mod qmp;
mod share;
pub mod stats;

pub use config::{GUEST_OPENCODE_PORT, VmConfig};
pub use disk::overlay_virtual_size;
pub use error::{Error, Result};
pub use fsutil::create_private_dir_all;
pub use handle::{RunningVm, VmFailure, VmStatus};
pub use process::{die_with_parent, reap_stale};
pub use qmp::{GuestMemory, guest_memory};
