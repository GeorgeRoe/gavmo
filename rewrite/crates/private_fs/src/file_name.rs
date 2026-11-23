//! [`FileName`]: one path component, checked when the program compiles.

use std::fmt;
use std::path::Path;

/// The name of a file directly inside a [`PrivateDir`](crate::PrivateDir):
/// not empty, at most 255 bytes, no `/` or NUL, and not `.` or `..`, so it
/// can't point outside the directory.
///
/// Every such name in GAVMO is fixed, so it is a `const`, and a bad one
/// fails the build:
///
/// ```
/// use private_fs::FileName;
///
/// const VAULT: FileName = FileName::new("secrets.vault");
/// assert_eq!(VAULT.as_str(), "secrets.vault");
/// ```
///
/// ```compile_fail
/// use private_fs::FileName;
///
/// const ESCAPES: FileName = FileName::new("../secrets.vault");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileName(&'static str);

impl FileName {
    /// Panics if `name` is not a single path component; in a `const`, that
    /// is a compile error.
    pub const fn new(name: &'static str) -> Self {
        assert!(is_file_name(name), "not a plain file name");
        Self(name)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

const fn is_file_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 255 {
        return false;
    }
    if matches!(bytes, b"." | b"..") {
        return false;
    }
    let mut rest = bytes;
    while let [first, tail @ ..] = rest {
        if matches!(first, b'/' | b'\0') {
            return false;
        }
        rest = tail;
    }
    true
}

impl AsRef<Path> for FileName {
    fn as_ref(&self) -> &Path {
        Path::new(self.0)
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_names() {
        for name in ["a", "secrets.vault", ".hidden", "...", &"x".repeat(255)] {
            assert!(is_file_name(name), "{name}");
        }
    }

    #[test]
    fn refuses_names_that_are_not_one_component() {
        for name in ["", ".", "..", "a/b", "/a", "a/", "a\0b", &"x".repeat(256)] {
            assert!(!is_file_name(name), "{name:?}");
        }
    }

    #[test]
    #[should_panic(expected = "not a plain file name")]
    fn new_panics_outside_a_const() {
        let name: &'static str = String::from("a/b").leak();
        FileName::new(name);
    }
}
