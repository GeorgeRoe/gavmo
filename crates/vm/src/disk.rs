use std::path::Path;

use tokio::process::Command;

use crate::error::{Error, Result};

/// Virtual size of every overlay disk. The base image is auto-sized with
/// little headroom, too small once `gavmo.nix` adds real packages; qcow2 is
/// sparse so this costs nothing until written, and the guest grows its root fs.
const OVERLAY_DISK_SIZE: &str = "32G";

/// Builds the `qemu-img create` argument list for a copy-on-write overlay
/// backed by a read-only base image. Pure, for unit testing without
/// `qemu-img` installed.
pub fn build_overlay_args(base_image: &Path, overlay_path: &Path) -> Vec<String> {
    vec![
        "create".to_string(),
        "-f".to_string(),
        "qcow2".to_string(),
        "-F".to_string(),
        "qcow2".to_string(),
        "-b".to_string(),
        base_image.display().to_string(),
        overlay_path.display().to_string(),
        OVERLAY_DISK_SIZE.to_string(),
    ]
}

/// Ensures the per-workspace overlay disk exists, creating it backed by
/// `base_image` if missing. The base image is never modified.
///
/// An existing overlay is left untouched so `/nix` and OpenCode session
/// state persist across restarts.
pub async fn ensure_overlay(base_image: &Path, overlay_path: &Path) -> Result<()> {
    if overlay_path.exists() {
        return Ok(());
    }

    if let Some(parent) = overlay_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| Error::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
    }

    let args = build_overlay_args(base_image, overlay_path);
    let output = Command::new("qemu-img")
        .args(&args)
        .output()
        .await
        .map_err(|source| Error::Spawn {
            program: "qemu-img".to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(Error::DiskCreateFailed {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn overlay_args_reference_base_as_backing_file() {
        let base = PathBuf::from("/var/lib/gavmo/base.qcow2");
        let overlay = PathBuf::from("/home/user/project/.gavmo/vm/disk.qcow2");

        let args = build_overlay_args(&base, &overlay);

        assert_eq!(
            args,
            vec![
                "create",
                "-f",
                "qcow2",
                "-F",
                "qcow2",
                "-b",
                "/var/lib/gavmo/base.qcow2",
                "/home/user/project/.gavmo/vm/disk.qcow2",
                OVERLAY_DISK_SIZE,
            ]
        );
    }
}
