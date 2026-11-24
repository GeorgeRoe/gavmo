//! The key file: the sealed key, next to the vault.
//!
//! Two lines: [`HEADER`], then the DPAPI blob in base64. The format is what
//! existing installs have on disk; it must not change.

use std::path::Path;

use base64::Engine as _;
use private_fs::FileName;

use crate::DpapiError;

/// The sealed key's file name, next to the vault.
pub const KEY_FILE: FileName = FileName::new("vault-key.dpapi");

const HEADER: &str = "gavmo-dpapi-v1";

/// The file's contents for `blob` (base64).
pub(crate) fn render(blob: &str) -> String {
    format!("{HEADER}\n{blob}\n")
}

/// The sealed blob (base64) from a key file at `path`.
pub(crate) fn parse(contents: &[u8], path: &Path) -> Result<String, DpapiError> {
    let text = std::str::from_utf8(contents).map_err(|_utf8| DpapiError::NotAKeyFile {
        path: path.to_path_buf(),
    })?;
    let mut lines = text.lines().map(str::trim);
    if lines.next() != Some(HEADER) {
        return Err(DpapiError::NotAKeyFile {
            path: path.to_path_buf(),
        });
    }
    match lines.next() {
        Some(blob) if is_base64(blob) => Ok(blob.to_owned()),
        _ => Err(DpapiError::NoSealedKey {
            path: path.to_path_buf(),
        }),
    }
}

pub(crate) fn is_base64(text: &str) -> bool {
    !text.is_empty()
        && base64::engine::general_purpose::STANDARD
            .decode(text)
            .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(contents: &str) -> Result<String, DpapiError> {
        parse(contents.as_bytes(), Path::new("/d/vault-key.dpapi"))
    }

    #[test]
    fn reads_what_it_writes_and_what_earlier_versions_wrote() {
        assert_eq!(render("QUJD"), "gavmo-dpapi-v1\nQUJD\n");
        assert_eq!(parsed(&render("QUJD")).unwrap(), "QUJD");
        assert_eq!(parsed("gavmo-dpapi-v1\r\nQUJD\r\n").unwrap(), "QUJD");
    }

    #[test]
    fn refuses_foreign_or_damaged_files() {
        assert!(matches!(
            parsed("something else\nQUJD\n"),
            Err(DpapiError::NotAKeyFile { .. })
        ));
        assert!(matches!(
            parse(b"\xff\xfe", Path::new("/k")),
            Err(DpapiError::NotAKeyFile { .. })
        ));
        for damaged in [
            "gavmo-dpapi-v1\n",
            "gavmo-dpapi-v1\n\n",
            "gavmo-dpapi-v1\n!!\n",
        ] {
            assert!(
                matches!(parsed(damaged), Err(DpapiError::NoSealedKey { .. })),
                "{damaged:?}"
            );
        }
        assert_eq!(
            parsed("x\n").unwrap_err().to_string(),
            "/d/vault-key.dpapi: not a GAVMO DPAPI key file (unknown header)"
        );
    }
}
