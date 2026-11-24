//! [`AutoError`]: why no keystore could be picked.

use wsl::{DEFAULT_POWERSHELL, POWERSHELL_ENV, Wsl};

#[derive(Debug, thiserror::Error)]
pub enum AutoError {
    /// Windows was picked, but there is no PowerShell to reach it with.
    #[error("{}", no_powershell(*.0))]
    NoPowerShell(Wsl),
    /// Whether the vault or the key file exists couldn't be told.
    #[error(transparent)]
    File(#[from] private_fs::Error),
}

fn no_powershell(wsl: Wsl) -> String {
    match wsl {
        Wsl::Interop => format!(
            "powershell.exe is not on PATH or at {DEFAULT_POWERSHELL}; set {POWERSHELL_ENV} to its path"
        ),
        Wsl::NoInterop => "WSL interop is disabled, so Windows programs can't run \
                           (enable [interop] in /etc/wsl.conf), or use --key-store secret-service"
            .to_owned(),
        Wsl::NotWsl => "this is not WSL; use --key-store secret-service".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_powershell_says_what_to_do_for_each_host() {
        let why = |wsl| AutoError::NoPowerShell(wsl).to_string();
        assert!(why(Wsl::Interop).contains(POWERSHELL_ENV));
        assert!(why(Wsl::NoInterop).contains("enable [interop]"));
        assert!(why(Wsl::NotWsl).contains("not WSL"));
    }
}
