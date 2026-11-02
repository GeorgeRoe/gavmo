use std::ffi::OsString;
use std::path::Path;

use tokio::process::Command;

use crate::error::{Error, Result};
use crate::fsutil::{create_private_dir_all, remove_if_exists_async};

const GIB: u64 = 1024 * 1024 * 1024;

/// Builds the `qemu-img create` argument list for a copy-on-write overlay
/// backed by a read-only base image, `size_gib` GiB in virtual size. Pure,
/// for unit testing without `qemu-img` installed. Paths stay `OsString`s,
/// so nothing is mangled.
pub(crate) fn build_overlay_args(
    base_image: &Path,
    overlay_path: &Path,
    size_gib: u32,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["create", "-f", "qcow2", "-F", "qcow2", "-b"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(base_image.into());
    args.push(overlay_path.into());
    args.push(format!("{size_gib}G").into());
    args
}

/// `qemu-img resize` arguments growing `overlay_path` to `size_gib` GiB.
/// `-f qcow2` so qemu-img never probes the format of a file the guest
/// could have influenced.
pub(crate) fn build_resize_args(overlay_path: &Path, size_gib: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["resize", "-f", "qcow2"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(overlay_path.into());
    args.push(format!("{size_gib}G").into());
    args
}

/// The `virtual-size` field of `qemu-img info --output=json`.
pub(crate) fn parse_virtual_size(json: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(json)
        .ok()?
        .get("virtual-size")?
        .as_u64()
}

/// Runs `qemu-img` with `args`, mapping failure to [`Error::QemuImg`].
async fn qemu_img(args: Vec<OsString>) -> Result<Vec<u8>> {
    let output = Command::new("qemu-img")
        .args(args)
        .output()
        .await
        .map_err(|source| Error::Spawn {
            program: "qemu-img".to_owned(),
            source,
        })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(Error::QemuImg {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// The virtual size of the overlay at `overlay_path`, in bytes.
pub async fn overlay_virtual_size(overlay_path: &Path) -> Result<u64> {
    let mut args: Vec<OsString> = ["info", "-f", "qcow2", "--output=json", "-U"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(overlay_path.into());
    let json = qemu_img(args).await?;
    parse_virtual_size(&json).ok_or(Error::QemuImgOutput)
}

/// Ensures the per-workspace overlay disk exists with a virtual size of at
/// least `size_gib` GiB, creating it backed by `base_image` if missing. The
/// base image is never modified.
///
/// An existing overlay keeps its contents so `/nix` persists across
/// restarts. If it is smaller than `size_gib` it is grown (the guest grows
/// its partition and root filesystem to fit on boot); a larger one is left
/// alone, since shrinking would cut off the guest's filesystem.
///
/// A new overlay is created under a temporary name and renamed into place
/// only once `qemu-img` succeeds, so a failed or interrupted creation can
/// never be mistaken for a valid overlay on the next boot. Its directory is
/// created owner-only.
pub(crate) async fn ensure_overlay(
    base_image: &Path,
    overlay_path: &Path,
    size_gib: u32,
) -> Result<()> {
    if let Some(parent) = overlay_path.parent() {
        create_private_dir_all(parent)?;
    }
    if tokio::fs::try_exists(overlay_path).await.unwrap_or(false) {
        let current = overlay_virtual_size(overlay_path).await?;
        if current < u64::from(size_gib) * GIB {
            tracing::info!(
                disk = %overlay_path.display(),
                from_bytes = current,
                to_gib = size_gib,
                "growing the overlay disk"
            );
            qemu_img(build_resize_args(overlay_path, size_gib)).await?;
        }
        return Ok(());
    }

    let mut tmp_name = overlay_path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = Path::new(&tmp_name);
    remove_if_exists_async(tmp).await?;

    if let Err(e) = qemu_img(build_overlay_args(base_image, tmp, size_gib)).await {
        remove_if_exists_async(tmp).await?;
        return Err(e);
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

        let args = build_overlay_args(&base, &overlay, 48);

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
                "48G",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn resize_args_name_the_format_and_the_new_size() {
        assert_eq!(
            build_resize_args(Path::new("/d/disk.qcow2"), 64),
            ["resize", "-f", "qcow2", "/d/disk.qcow2", "64G"].map(OsString::from)
        );
    }

    #[test]
    fn virtual_size_is_read_from_qemu_img_info() {
        let json = br#"{"virtual-size": 34359738368, "filename": "disk.qcow2"}"#;
        assert_eq!(parse_virtual_size(json), Some(32 * GIB));
        assert_eq!(parse_virtual_size(b"{}"), None);
        assert_eq!(parse_virtual_size(b"not json"), None);
    }

    /// Real qemu-img: a new overlay gets the requested size, a smaller
    /// request leaves it alone, a larger one grows it in place.
    #[tokio::test]
    #[ignore = "needs qemu-img"]
    async fn real_overlays_are_created_grown_and_never_shrunk() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base.qcow2");
        qemu_img(
            ["create", "-f", "qcow2"]
                .into_iter()
                .map(OsString::from)
                .chain([base.clone().into(), "1G".into()])
                .collect(),
        )
        .await
        .unwrap();
        let disk = tmp.path().join("vm").join("disk.qcow2");

        ensure_overlay(&base, &disk, 2).await.unwrap();
        assert_eq!(overlay_virtual_size(&disk).await.unwrap(), 2 * GIB);

        ensure_overlay(&base, &disk, 1).await.unwrap();
        assert_eq!(
            overlay_virtual_size(&disk).await.unwrap(),
            2 * GIB,
            "never shrunk"
        );

        ensure_overlay(&base, &disk, 3).await.unwrap();
        assert_eq!(overlay_virtual_size(&disk).await.unwrap(), 3 * GIB, "grown");
    }
    /// Real qemu-img: a missing base fails creation without leaving a
    /// half-made overlay (or its temp file) behind, and a file that isn't
    /// an image can't be sized.
    #[tokio::test]
    #[ignore = "needs qemu-img"]
    async fn real_qemu_img_failures_are_reported_and_leave_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let disk = tmp.path().join("vm").join("disk.qcow2");

        let err = ensure_overlay(&tmp.path().join("missing.qcow2"), &disk, 1)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::QemuImg { .. }), "{err:?}");
        let left: Vec<_> = std::fs::read_dir(tmp.path().join("vm")).unwrap().collect();
        assert_eq!(left.len(), 0, "{left:?}");

        std::fs::write(&disk, b"not an image").unwrap();
        let err = ensure_overlay(&tmp.path().join("missing.qcow2"), &disk, 1)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::QemuImg { stderr, .. } if !stderr.is_empty()),
            "{err:?}"
        );
    }
}
