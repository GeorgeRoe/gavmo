//! The base image's identity: the hex SHA-256 of its contents, recorded with
//! each overlay so a changed base is detected instead of silently booted
//! under an overlay made for another one (see [`vm::VmConfig::base_image_id`]).
//!
//! Hashing a multi-GB image takes seconds, so the result is cached against
//! the file's fingerprint (canonical path, device, inode, size, mtime, ctime)
//! in memory and in `base-image.json` in the data dir. Any replacement or
//! in-place write changes the fingerprint and forces a re-hash; a copy of the
//! same image elsewhere hashes to the same id, so it is still accepted.

use std::io::{self, Read as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Everything about the file that changes if it is replaced or rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Fingerprint {
    pub(super) path: PathBuf,
    pub(super) dev: u64,
    pub(super) ino: u64,
    pub(super) size: u64,
    pub(super) mtime: (i64, i64),
    pub(super) ctime: (i64, i64),
}

/// A hash and the fingerprint of the file it was taken from (the cache
/// file's format).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Identified {
    pub(super) fingerprint: Fingerprint,
    pub(super) id: String,
}

/// The configured base image and its cached identity.
pub(super) struct BaseImage {
    path: PathBuf,
    cache_path: PathBuf,
    /// Also serializes identification, so concurrent boots hash once.
    memo: Mutex<Option<Identified>>,
}

impl BaseImage {
    pub(super) fn new(path: PathBuf, cache_path: PathBuf) -> Self {
        Self {
            path,
            cache_path,
            memo: Mutex::new(None),
        }
    }

    /// The configured path, as given.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The image's id, hashing it only if it changed since last time (in
    /// this process or, via the cache file, a previous one). Fails if it
    /// can't be read or isn't a regular file.
    pub(super) async fn id(&self) -> io::Result<String> {
        let mut memo = self.memo.lock().await;
        let path = self.path.clone();
        let fingerprint = tokio::task::spawn_blocking(move || fingerprint(&path))
            .await
            .map_err(io::Error::other)??;
        if let Some(known) = memo.as_ref()
            && known.fingerprint == fingerprint
        {
            return Ok(known.id.clone());
        }
        if let Some(cached) = load_cache(&self.cache_path).await
            && cached.fingerprint == fingerprint
        {
            let id = cached.id.clone();
            *memo = Some(cached);
            return Ok(id);
        }

        tracing::info!(
            image = %fingerprint.path.display(),
            size = %super::paths::human_bytes(fingerprint.size),
            "identifying the base image (hashing it; once per image)"
        );
        let started = std::time::Instant::now();
        let hashed_path = fingerprint.path.clone();
        let id = tokio::task::spawn_blocking(move || sha256_file(&hashed_path))
            .await
            .map_err(io::Error::other)??;
        tracing::info!(id = %id, elapsed = ?started.elapsed(), "base image identified");

        let identified = Identified { fingerprint, id };
        // Best-effort: without the cache the next daemon start re-hashes.
        if let Err(e) = save_cache(&self.cache_path, &identified).await {
            tracing::warn!(error = %e, cache = %self.cache_path.display(), "could not cache the base image id");
        }
        let id = identified.id.clone();
        *memo = Some(identified);
        Ok(id)
    }
}

/// The fingerprint of the regular file `path` resolves to. Blocking.
pub(super) fn fingerprint(path: &Path) -> io::Result<Fingerprint> {
    let canonical = std::fs::canonicalize(path)?;
    let meta = std::fs::metadata(&canonical)?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(Fingerprint {
        path: canonical,
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
    })
}

/// Hex SHA-256 of a file's contents. Blocking.
pub(super) fn sha256_file(path: &Path) -> io::Result<String> {
    use sha2::Digest as _;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The cached identity, if the cache file exists and parses. Anything else
/// just means re-hashing.
async fn load_cache(path: &Path) -> Option<Identified> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn save_cache(path: &Path, identified: &Identified) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(identified).map_err(io::Error::other)?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::atomic_file::write_atomically(&path, &bytes))
        .await
        .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sha256("hello\n").
    const HELLO_SHA256: &str = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";

    fn setup(contents: &[u8]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("base.qcow2");
        std::fs::write(&image, contents).unwrap();
        let cache = tmp.path().join("base-image.json");
        (tmp, image, cache)
    }

    #[test]
    fn files_hash_to_their_sha256() {
        let (_tmp, image, _) = setup(b"hello\n");
        assert_eq!(sha256_file(&image).unwrap(), HELLO_SHA256);
    }

    #[tokio::test]
    async fn the_id_is_hashed_once_and_cached_on_disk() {
        let (_tmp, image, cache) = setup(b"hello\n");
        let base = BaseImage::new(image.clone(), cache.clone());

        assert_eq!(base.id().await.unwrap(), HELLO_SHA256);
        assert_eq!(base.path(), image);
        let cached = load_cache(&cache).await.expect("cache written");
        assert_eq!(cached.id, HELLO_SHA256);
        assert_eq!(cached.fingerprint, fingerprint(&image).unwrap());

        // A cache entry for the same fingerprint is trusted without
        // re-hashing, by this process (memo) and the next one (file).
        let planted = Identified {
            id: "planted".to_owned(),
            ..cached
        };
        save_cache(&cache, &planted).await.unwrap();
        assert_eq!(base.id().await.unwrap(), HELLO_SHA256, "memo wins");
        let restarted = BaseImage::new(image, cache);
        assert_eq!(restarted.id().await.unwrap(), "planted");
    }

    /// Replacing the image (a rebuilt or updated base) changes its id, even
    /// at the same path; an unparsable cache just means hashing again.
    #[tokio::test]
    async fn a_replaced_image_is_hashed_again() {
        let (tmp, image, cache) = setup(b"hello\n");
        let base = BaseImage::new(image.clone(), cache.clone());
        assert_eq!(base.id().await.unwrap(), HELLO_SHA256);

        let replacement = tmp.path().join("new.qcow2");
        std::fs::write(&replacement, b"another image").unwrap();
        std::fs::rename(&replacement, &image).unwrap();
        let new_id = base.id().await.unwrap();
        assert_ne!(new_id, HELLO_SHA256);
        assert_eq!(new_id, sha256_file(&image).unwrap());

        std::fs::write(&cache, b"{not json").unwrap();
        let restarted = BaseImage::new(image, cache.clone());
        assert_eq!(restarted.id().await.unwrap(), new_id);
        assert_eq!(load_cache(&cache).await.unwrap().id, new_id, "rewritten");
    }

    /// A symlinked image (like nix's `result/nixos.qcow2`) is identified by
    /// its target: repointing the link is a different image.
    #[tokio::test]
    async fn a_symlink_is_identified_by_its_target() {
        let (tmp, image, cache) = setup(b"hello\n");
        let link = tmp.path().join("result.qcow2");
        std::os::unix::fs::symlink(&image, &link).unwrap();
        let base = BaseImage::new(link.clone(), cache);
        assert_eq!(base.id().await.unwrap(), HELLO_SHA256);

        let other = tmp.path().join("other.qcow2");
        std::fs::write(&other, b"other").unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_eq!(base.id().await.unwrap(), sha256_file(&other).unwrap());
    }

    #[tokio::test]
    async fn a_missing_or_non_file_image_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("base-image.json");

        let missing = BaseImage::new(tmp.path().join("missing.qcow2"), cache.clone());
        let err = missing.id().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let dir = BaseImage::new(tmp.path().to_path_buf(), cache);
        let err = dir.id().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// A cache that can't be written is only a warning.
    #[tokio::test]
    async fn an_unwritable_cache_does_not_fail_identification() {
        let (tmp, image, _) = setup(b"hello\n");
        let base = BaseImage::new(image, tmp.path().join("missing").join("cache.json"));
        assert_eq!(base.id().await.unwrap(), HELLO_SHA256);
    }
}
