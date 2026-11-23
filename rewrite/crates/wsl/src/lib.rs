//! Running under WSL: telling whether this is WSL ([`detect`]), and running
//! Windows PowerShell from it ([`PowerShell`]), which is how a Linux process
//! reaches Windows-only services such as DPAPI.

mod detect;
mod powershell;
mod run_error;
mod script;
mod stderr;

pub use detect::{Wsl, detect, detect_in};
pub use powershell::{DEFAULT_POWERSHELL, POWERSHELL_ENV, PowerShell};
pub use run_error::RunError;
pub use script::Script;
