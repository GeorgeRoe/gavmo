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

/// Format version of the base image record.
const BASE_RECORD_VERSION: u64 = 1;

/// The contents of a base image record: which base an overlay was created
/// from. `base_image` (the path) is informational only.
fn base_record_json(base_image: &Path, base_image_id: &str) -> Vec<u8> {
    let record = serde_json::json!({
        "version": BASE_RECORD_VERSION,
        "base_image_id": base_image_id,
        "base_image": base_image.to_string_lossy(),
    });
    let mut bytes = record.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

/// The base image id in a record, or `None` if it isn't a record this
/// version understands.
pub(crate) fn parse_base_record(bytes: &[u8]) -> Option<String> {
    let record = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    if record.get("version")?.as_u64()? != BASE_RECORD_VERSION {
        return None;
    }
    let id = record.get("base_image_id")?.as_str()?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// Writes the base image record for a new overlay, via a temporary file
/// renamed into place, so a crash never leaves a truncated record.
async fn write_base_record(
    record_path: &Path,
    base_image: &Path,
    base_image_id: &str,
) -> Result<()> {
    let mut tmp_name = record_path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = Path::new(&tmp_name);
    let to_err = |source| Error::BaseRecord {
        path: record_path.to_path_buf(),
        source,
    };
    tokio::fs::write(tmp, base_record_json(base_image, base_image_id))
        .await
        .map_err(to_err)?;
    tokio::fs::rename(tmp, record_path).await.map_err(to_err)
}

/// Checks that an existing overlay was created from the base image
/// identified by `base_image_id`, refusing it with
/// [`Error::BaseImageChanged`] if not.
///
/// An overlay with no record at all predates base image tracking. It is
/// adopted: the current base is recorded for it, the same assumption every
/// boot made before records existed.
async fn check_base_record(
    record_path: &Path,
    base_image: &Path,
    base_image_id: &str,
) -> Result<()> {
    match tokio::fs::read(record_path).await {
        Ok(bytes) => match parse_base_record(&bytes) {
            Some(recorded) if recorded == base_image_id => Ok(()),
            recorded => Err(Error::BaseImageChanged {
                recorded,
                current: base_image_id.to_owned(),
            }),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                record = %record_path.display(),
                "this VM's disk has no base image record (created by an older GAVMO); \
                 assuming it was made from the current base image"
            );
            write_base_record(record_path, base_image, base_image_id).await
        }
        Err(source) => Err(Error::BaseRecord {
            path: record_path.to_path_buf(),
            source,
        }),
    }
}

/// Ensures the per-workspace overlay disk exists with a virtual size of at
/// least `size_gib` GiB, creating it backed by `base_image` if missing. The
/// base image is never modified.
///
/// An existing overlay must have been created from the same base image
/// (`base_image_id`, checked against the record at `record_path`), or this
/// fails with [`Error::BaseImageChanged`] before touching it. It keeps its
/// contents so `/nix` persists across restarts. If it is smaller than
/// `size_gib` it is grown (the guest grows its partition and root
/// filesystem to fit on boot); a larger one is left alone, since shrinking
/// would cut off the guest's filesystem.
///
/// A new overlay is created under a temporary name and renamed into place
/// only once `qemu-img` succeeds and its base record is written, so a failed
/// or interrupted creation can never be mistaken for a valid overlay on the
/// next boot. Its directory is created owner-only.
pub(crate) async fn ensure_overlay(
    base_image: &Path,
    base_image_id: &str,
    overlay_path: &Path,
    record_path: &Path,
    size_gib: u32,
) -> Result<()> {
    if let Some(parent) = overlay_path.parent() {
        create_private_dir_all(parent)?;
    }
    if tokio::fs::try_exists(overlay_path).await.unwrap_or(false) {
        check_base_record(record_path, base_image, base_image_id).await?;
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
    // Before the rename: a disk must never exist without its record (a
    // record without a disk is harmless and overwritten next time).
    if let Err(e) = write_base_record(record_path, base_image, base_image_id).await {
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

    const ID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn recorded_id(record: &Path) -> Option<String> {
        parse_base_record(&std::fs::read(record).unwrap())
    }

    #[test]
    fn base_records_round_trip_and_reject_what_they_do_not_understand() {
        let json = base_record_json(Path::new("/b/nixos.qcow2"), ID_A);
        assert_eq!(parse_base_record(&json).as_deref(), Some(ID_A));
        let text = String::from_utf8(json).unwrap();
        assert!(
            text.contains("/b/nixos.qcow2") && text.ends_with('\n'),
            "{text}"
        );

        assert_eq!(parse_base_record(b"not json"), None);
        assert_eq!(parse_base_record(b"{}"), None);
        assert_eq!(
            parse_base_record(br#"{"version": 2, "base_image_id": "x"}"#),
            None,
            "a future format is not guessed at"
        );
        assert_eq!(
            parse_base_record(br#"{"version": 1, "base_image_id": ""}"#),
            None
        );
        assert_eq!(
            parse_base_record(br#"{"version": 1, "base_image_id": 7}"#),
            None
        );
    }

    /// A tempdir holding a stand-in overlay (never read: the record check
    /// comes first), plus the base and record paths `ensure_overlay` takes.
    fn existing_overlay() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base.qcow2");
        let disk = tmp.path().join("vm").join("disk.qcow2");
        let record = tmp.path().join("vm").join("base.json");
        std::fs::create_dir_all(tmp.path().join("vm")).unwrap();
        std::fs::write(&disk, b"overlay").unwrap();
        (tmp, base, disk, record)
    }

    /// An overlay made from another base is refused before it is touched.
    #[tokio::test]
    async fn an_overlay_from_a_different_base_is_refused() {
        let (_tmp, base, disk, record) = existing_overlay();
        std::fs::write(&record, base_record_json(&base, ID_A)).unwrap();

        let err = ensure_overlay(&base, ID_B, &disk, &record, 1)
            .await
            .unwrap_err();

        assert!(
            matches!(&err, Error::BaseImageChanged { recorded: Some(r), current }
                if r == ID_A && current == ID_B),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains("(was aaaaaaaaaaaa, now bbbbbbbbbbbb)") && message.contains("reset"),
            "{message}"
        );
        assert_eq!(std::fs::read(&disk).unwrap(), b"overlay");
        assert_eq!(recorded_id(&record).as_deref(), Some(ID_A));
    }

    /// A record this version can't understand can't vouch for the overlay.
    #[tokio::test]
    async fn an_overlay_with_a_corrupt_record_is_refused() {
        let (_tmp, base, disk, record) = existing_overlay();
        std::fs::write(&record, b"{trunc").unwrap();

        let err = ensure_overlay(&base, ID_A, &disk, &record, 1)
            .await
            .unwrap_err();

        assert!(
            matches!(&err, Error::BaseImageChanged { recorded: None, .. }),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("(unknown image, now aaaaaaaaaaaa)"),
            "{err}"
        );
    }

    /// A record that can't be read at all (here: a directory) is an error,
    /// not an adoption.
    #[tokio::test]
    async fn an_unreadable_record_is_an_error() {
        let (_tmp, base, disk, record) = existing_overlay();
        std::fs::create_dir(&record).unwrap();

        let err = ensure_overlay(&base, ID_A, &disk, &record, 1)
            .await
            .unwrap_err();

        assert!(matches!(err, Error::BaseRecord { .. }), "{err:?}");
    }

    /// An overlay from before records existed is adopted: the current base
    /// is recorded for it. (Sizing the stand-in overlay then fails, with or
    /// without qemu-img installed.)
    #[tokio::test]
    async fn an_overlay_without_a_record_adopts_the_current_base() {
        let (tmp, base, disk, record) = existing_overlay();

        let result = ensure_overlay(&base, ID_A, &disk, &record, 1).await;

        assert!(
            matches!(result, Err(Error::Spawn { .. } | Error::QemuImg { .. })),
            "{result:?}"
        );
        assert_eq!(recorded_id(&record).as_deref(), Some(ID_A));
        assert!(!tmp.path().join("vm").join("base.json.tmp").exists());
    }

    /// Writing a record into a missing directory fails as a record error.
    #[tokio::test]
    async fn a_record_that_cannot_be_written_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let record = tmp.path().join("missing").join("base.json");

        let err = write_base_record(&record, Path::new("/b"), ID_A)
            .await
            .unwrap_err();

        assert!(matches!(err, Error::BaseRecord { .. }), "{err:?}");
    }

    /// A real (empty) base image for the qemu-img tests.
    async fn real_base(dir: &Path) -> PathBuf {
        let base = dir.join("base.qcow2");
        qemu_img(
            ["create", "-f", "qcow2"]
                .into_iter()
                .map(OsString::from)
                .chain([base.clone().into(), "1G".into()])
                .collect(),
        )
        .await
        .unwrap();
        base
    }

    /// Real qemu-img: a new overlay gets the requested size and records its
    /// base, a smaller request leaves it alone, a larger one grows it in
    /// place, and another base is refused.
    #[tokio::test]
    #[ignore = "needs qemu-img"]
    async fn real_overlays_are_created_grown_and_never_shrunk() {
        let tmp = tempfile::tempdir().unwrap();
        let base = real_base(tmp.path()).await;
        let disk = tmp.path().join("vm").join("disk.qcow2");
        let record = tmp.path().join("vm").join("base.json");

        ensure_overlay(&base, ID_A, &disk, &record, 2)
            .await
            .unwrap();
        assert_eq!(overlay_virtual_size(&disk).await.unwrap(), 2 * GIB);
        assert_eq!(recorded_id(&record).as_deref(), Some(ID_A));

        ensure_overlay(&base, ID_A, &disk, &record, 1)
            .await
            .unwrap();
        assert_eq!(
            overlay_virtual_size(&disk).await.unwrap(),
            2 * GIB,
            "never shrunk"
        );

        ensure_overlay(&base, ID_A, &disk, &record, 3)
            .await
            .unwrap();
        assert_eq!(overlay_virtual_size(&disk).await.unwrap(), 3 * GIB, "grown");

        let err = ensure_overlay(&base, ID_B, &disk, &record, 3)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BaseImageChanged { .. }), "{err:?}");
    }

    /// Real qemu-img: a missing base fails creation without leaving a
    /// half-made overlay, its temp file or a record behind, and a file that
    /// isn't an image can't be sized.
    #[tokio::test]
    #[ignore = "needs qemu-img"]
    async fn real_qemu_img_failures_are_reported_and_leave_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let disk = tmp.path().join("vm").join("disk.qcow2");
        let record = tmp.path().join("vm").join("base.json");
        let missing = tmp.path().join("missing.qcow2");

        let err = ensure_overlay(&missing, ID_A, &disk, &record, 1)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::QemuImg { .. }), "{err:?}");
        let left: Vec<_> = std::fs::read_dir(tmp.path().join("vm")).unwrap().collect();
        assert_eq!(left.len(), 0, "{left:?}");

        std::fs::write(&disk, b"not an image").unwrap();
        std::fs::write(&record, base_record_json(&missing, ID_A)).unwrap();
        let err = ensure_overlay(&missing, ID_A, &disk, &record, 1)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::QemuImg { stderr, .. } if !stderr.is_empty()),
            "{err:?}"
        );
    }

    /// Real qemu-img: if the record can't be written, the new overlay is
    /// not put in place.
    #[tokio::test]
    #[ignore = "needs qemu-img"]
    async fn real_overlays_are_not_kept_without_a_record() {
        let tmp = tempfile::tempdir().unwrap();
        let base = real_base(tmp.path()).await;
        let disk = tmp.path().join("vm").join("disk.qcow2");
        // A non-empty directory in the record's place makes the rename fail.
        let record = tmp.path().join("vm").join("base.json");
        std::fs::create_dir_all(record.join("occupied")).unwrap();

        let err = ensure_overlay(&base, ID_A, &disk, &record, 1)
            .await
            .unwrap_err();

        assert!(matches!(err, Error::BaseRecord { .. }), "{err:?}");
        assert!(!disk.exists());
        assert!(!tmp.path().join("vm").join("disk.qcow2.tmp").exists());
    }
}
