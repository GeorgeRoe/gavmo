//! [`Script`]: PowerShell code fixed when the program is built.

use base64::Engine as _;

/// The text of a PowerShell script. Only `&'static str`, so a script is
/// never assembled at run time: data (and secrets) reach it on stdin, and
/// the command line - visible in `/proc` and the Windows process list -
/// carries nothing but this fixed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Script(&'static str);

impl Script {
    pub const fn new(text: &'static str) -> Self {
        Self(text)
    }

    pub const fn text(&self) -> &'static str {
        self.0
    }

    /// The script as PowerShell's `-EncodedCommand` wants it: base64 of
    /// UTF-16LE. No quoting can go wrong on the way through WSL's
    /// command-line conversion.
    pub(crate) fn encoded(&self) -> String {
        let utf16: Vec<u8> = self.0.encode_utf16().flat_map(u16::to_le_bytes).collect();
        base64::engine::general_purpose::STANDARD.encode(utf16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_is_utf16le_base64_of_the_text() {
        let script = Script::new("Write-Output 'héllo'\n");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(script.encoded())
            .unwrap();
        let (pairs, rest) = bytes.as_chunks::<2>();
        assert!(rest.is_empty());
        let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), script.text());
    }
}
