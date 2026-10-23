//! VM lifecycle management for GAVMO workspaces.
//!
//! This crate drives QEMU to boot, monitor, and stop the per-workspace
//! NixOS VMs that run `opencode web`. It is pure mechanism: the GAVMO
//! daemon owns port allocation, paths, and workspace identity and passes
//! them in via [`VmConfig`]. See `plan.md` in this crate for the full
//! design and the rationale behind each decision.

mod config;
mod disk;
mod error;
mod handle;
mod health;
mod qemu;
mod qmp;
mod share;
pub mod stats;

pub use config::{GUEST_OPENCODE_PORT, VmConfig};
pub use error::{Error, Result};
pub use handle::{RunningVm, VmStatus};
pub use share::{GUEST_AGENT_GID, GUEST_AGENT_UID, ShareBackend, ShareSpec, VirtiofsShare};
