//! The PowerShell side of DPAPI.

use wsl::Script;

/// The operation and its data arrive on stdin (`<op>\n<base64>\n`) and the
/// result (base64) leaves on stdout. Failures are written to stderr as plain
/// text, since PowerShell would otherwise serialize them as CLIXML when
/// stderr isn't a console; progress output (also CLIXML, "Preparing modules
/// for first use") is switched off.
///
/// The optional-entropy tag binds blobs to this purpose, so no other DPAPI
/// blob of the user's can be passed off as GAVMO's key. The tag and the
/// `CurrentUser` scope must never change: existing key files need them.
pub(crate) const DPAPI: Script = Script::new(
    r"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {
    $mode = $ExecutionContext.SessionState.LanguageMode
    if ($mode -ne 'FullLanguage') {
        throw ('PowerShell runs in ' + $mode + ' mode here (restricted by policy), which blocks DPAPI')
    }
    Add-Type -AssemblyName System.Security
    $lines = [Console]::In.ReadToEnd().Split([char]10)
    $op = $lines[0].Trim()
    $data = [Convert]::FromBase64String($lines[1].Trim())
    $tag = [Text.Encoding]::UTF8.GetBytes('gavmo-vault-key-v1')
    $scope = [Security.Cryptography.DataProtectionScope]::CurrentUser
    if ($op -eq 'protect') {
        $result = [Security.Cryptography.ProtectedData]::Protect($data, $tag, $scope)
    } elseif ($op -eq 'unprotect') {
        $result = [Security.Cryptography.ProtectedData]::Unprotect($data, $tag, $scope)
    } else {
        throw ('unknown operation ' + $op)
    }
    [Console]::Out.Write([Convert]::ToBase64String($result))
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
",
);
