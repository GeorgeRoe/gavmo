//! Console and build-log reads for the control panel (bounded, no-follow).

use super::error::{RegistryError, Result, io_ctx};
use std::io;
use std::path::Path;

use super::AppState;

/// Most of the console log returned per poll (the newest output).
pub(super) const MAX_CONSOLE_LOG_BYTES: u64 = 1024 * 1024;
/// Most of a rebuild log shown in the UI (the end, where errors are).
pub(super) const MAX_BUILD_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Reads the tail of `<id>/.gavmo/vm/<name>` (guest build output), which the guest can write
/// (see [`crate::guest_files`]): no symlinks, FIFOs or unbounded reads.
pub(super) async fn read_guest_output_file(
    id: &str,
    name: &str,
    max_bytes: u64,
) -> io::Result<Option<crate::guest_files::GuestFile>> {
    let gavmo_dir = Path::new(id).join(".gavmo");
    let state_dir = AppState::guest_output_dir_for(id);
    crate::guest_files::read_guest_file(
        state_dir.join(name),
        vec![gavmo_dir, state_dir],
        max_bytes,
        crate::guest_files::Keep::Tail,
    )
    .await
}

impl AppState {
    /// Reads a `gavmo.nix` build-failure log for the UI. `log` is
    /// client-supplied and becomes part of a path, so it is validated strictly.
    pub async fn read_build_log(&self, id: &str, log: &str) -> Result<String> {
        self.require_known(id).await?;
        if !is_safe_rebuild_log_filename(log) {
            return Err(RegistryError::InvalidInput(
                "invalid build log filename".to_owned(),
            ));
        }
        let file = read_guest_output_file(id, log, MAX_BUILD_LOG_BYTES)
            .await
            .map_err(io_ctx("reading the build log"))?
            .ok_or_else(|| RegistryError::NotFound("build log".to_owned()))?;
        let mut text = String::from_utf8_lossy(&file.bytes).into_owned();
        if file.truncated {
            text.insert_str(
                0,
                &format!("[... showing the last {MAX_BUILD_LOG_BYTES} bytes ...]\n"),
            );
        }
        Ok(text)
    }

    /// Reads and sanitizes the guest's boot console log for the live panel.
    /// A missing file (qemu not spawned yet) is an empty log, not an error.
    /// Only the newest [`MAX_CONSOLE_LOG_BYTES`] are returned.
    pub async fn read_console_log(&self, id: &str) -> Result<String> {
        self.require_known(id).await?;
        // Host-only now, but read the same bounded, no-follow way regardless:
        // it is still mostly guest-produced text.
        let path = self.host_state_dir_for(id).join("console.log");
        Ok(crate::guest_files::read_guest_file(
            path,
            Vec::new(),
            MAX_CONSOLE_LOG_BYTES,
            crate::guest_files::Keep::Tail,
        )
        .await
        .map_err(io_ctx("reading the console log"))?
        .map(|file| sanitize_console(&file.bytes))
        .unwrap_or_default())
    }
}

/// `true` only for `rebuild-<stem>.log` where `<stem>` is non-empty ASCII
/// alphanumerics, `-` and `_` (what `gavmo-rebuild` writes:
/// `rebuild-2026-01-01T00-00-00Z.log`). The name comes from a guest-written
/// marker, so this is an allowlist rather than a denylist of separators: no
/// `/`, `\`, `.`, NUL, control or non-ASCII characters can get through, so a
/// crafted name can't escape the workspace's state dir. Case-sensitive on
/// purpose.
pub(super) fn is_safe_rebuild_log_filename(name: &str) -> bool {
    name.strip_prefix("rebuild-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .is_some_and(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Strips terminal escape sequences from a raw serial-console capture,
/// leaving plain text for a `<pre>`. `\r\n` and bare `\r` become `\n` (a bare
/// `\r` would otherwise overwrite status lines); invalid UTF-8 is replaced lossily.
pub(super) fn sanitize_console(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // Skip exactly one control sequence, preserving surrounding text.
                match chars.peek() {
                    // CSI: `ESC [ ... final`, final byte in 0x40-0x7E.
                    Some('[') => {
                        chars.next();
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&c) {
                                break;
                            }
                        }
                    }
                    // OSC/DCS/APC/PM string sequences, terminated by BEL or
                    // ST (`ESC \`). GRUB emits DCS in real boot captures.
                    Some(']' | 'P' | '_' | '^') => {
                        chars.next();
                        while let Some(&c) = chars.peek() {
                            if c == '\u{7}' {
                                chars.next();
                                break;
                            }
                            if c == '\u{1b}' {
                                chars.next();
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                            chars.next();
                        }
                    }
                    // Other two-byte escapes (e.g. `ESC c`, `ESC 7`).
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            c => out.push(c),
        }
    }

    out
}
