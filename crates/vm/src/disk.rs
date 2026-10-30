use std::ffi::OsString;
use std::path::Path;

use tokio::process::Command;

use crate::error::{Error, Result};
use crate::fsutil::{create_private_dir_all, remove_if_exists_async};

/// Virtual size of every overlay disk. The base image is auto-sized with
/// little headroom, too small once `gavmo.nix` adds real packages; qcow2 is
/// sparse so this costs nothing until written, and the guest grows its root fs.
const OVERLAY_DISK_SIZE: &str = "32G";

/// Builds the `qemu-img create` argument list for a copy-on-write overlay
/// backed by a read-only base image. Pure, for unit testing without
/// `qemu-img` installed. Paths stay `OsString`s, so nothing is mangled.
pub(crate) fn build_overlay_args(base_image: &Path, overlay_path: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["create", "-f", "qcow2", "-F", "qcow2", "-b"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(base_image.into());
    args.push(overlay_path.into());
    args.push(OVERLAY_DISK_SIZE.into());
    args
}

/// Ensures the per-workspace overlay disk exists, creating it backed by
/// `base_image` if missing. The base image is never modified.
///
/// An existing overlay is left untouched so `/nix` persists across restarts.
/// A new one is created under a temporary name and renamed into place only
/// once `qemu-img` succeeds, so a failed or interrupted creation can never be
/// mistaken for a valid overlay on the next boot. Its directory is created
/// owner-only.
pub(crate) async fn ensure_overlay(base_image: &Path, overlay_path: &Path) -> Result<()> {
    if let Some(parent) = overlay_path.parent() {
        create_private_dir_all(parent)?;
    }
    if tokio::fs::try_exists(overlay_path).await.unwrap_or(false) {
        return Ok(());
    }

    let mut tmp_name = overlay_path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = Path::new(&tmp_name);
    remove_if_exists_async(tmp).await?;

    let output = Command::new("qemu-img")
        .args(build_overlay_args(base_image, tmp))
        .output()
        .await
        .map_err(|source| Error::Spawn {
            program: "qemu-img".to_owned(),
            source,
        })?;

    if !output.status.success() {
        remove_if_exists_async(tmp).await?;
        return Err(Error::DiskCreateFailed {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    tokio::fs::rename(tmp, overlay_path)
        .await
        .map_err(|source| Error::Rename {
            from: tmp.to_path_buf(),
            to: overlay_path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn overlay_args_reference_base_as_backing_file() {
        let base = PathBuf::from("/var/lib/gavmo/base.qcow2");
        let overlay = PathBuf::from("/home/user/.local/share/gavmo/vms/abc/disk.qcow2");

        let args = build_overlay_args(&base, &overlay);

        assert_eq!(
            args,
            [
                "create",
                "-f",
                "qcow2",
                "-F",
                "qcow2",
                "-b",
                "/var/lib/gavmo/base.qcow2",
                "/home/user/.local/share/gavmo/vms/abc/disk.qcow2",
                OVERLAY_DISK_SIZE,
            ]
            .map(OsString::from)
        );
    }
}
