//! Control-panel browser sessions.
//!
//! The session cookie holds a random session id, never the master UI token,
//! so a leaked cookie is a revocable, expiring session rather than a
//! permanent credential. Only SHA-256 hashes of session ids are kept, in
//! memory and in `sessions.json` (`0600`), so neither the file nor a memory
//! dump yields a usable cookie. Each session remembers the `Host` it was
//! created for and is only accepted under that host.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

/// How long a session lasts. Absolute, not sliding: a stolen cookie can't be
/// kept alive by using it.
pub(crate) const SESSION_TTL: Duration = Duration::from_hours(30 * 24);

/// Most sessions kept at once (one per browser/profile you log in from).
/// Beyond this the oldest are dropped, which bounds the file.
const MAX_SESSIONS: usize = 64;

const FILE_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    /// The `Host` header (lowercased) the session was created under.
    host: String,
    /// Unix seconds after which the session is invalid.
    expires_at: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionFile {
    v: u8,
    /// Hex SHA-256 of session id -> session.
    sessions: HashMap<String, Session>,
}

pub(crate) struct SessionStore {
    path: PathBuf,
    sessions: std::sync::Mutex<HashMap<String, Session>>,
    /// Serializes updates, so writes reach disk in the order they were made.
    write_lock: tokio::sync::Mutex<()>,
}

impl SessionStore {
    /// Loads the store, dropping expired sessions. A missing file is an
    /// empty store; an unparseable one is also treated as empty (sessions are
    /// disposable: the worst case is logging in again) and replaced on the
    /// next update. Other read errors are returned.
    pub(crate) fn load(path: PathBuf) -> io::Result<Self> {
        let sessions = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<SessionFile>(&bytes) {
                Ok(file) if file.v == FILE_VERSION => file.sessions,
                Ok(_) | Err(_) => {
                    tracing::warn!(path = %path.display(), "ignoring unreadable session file; log in again");
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e),
        };
        let now = now_secs();
        Ok(Self {
            path,
            sessions: std::sync::Mutex::new(
                sessions
                    .into_iter()
                    .filter(|(_, s)| s.expires_at > now)
                    .collect(),
            ),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Creates a session for `host` and returns its id (the cookie value).
    /// Persisted before it is published, so an id is never handed out that
    /// would vanish on restart.
    pub(crate) async fn create(&self, host: &str) -> io::Result<String> {
        let id = new_session_id();
        let session = Session {
            host: host.to_ascii_lowercase(),
            expires_at: now_secs().saturating_add(SESSION_TTL.as_secs()),
        };
        let hash = hash_id(&id);
        self.update(move |sessions| {
            let now = now_secs();
            sessions.retain(|_, s| s.expires_at > now);
            while sessions.len() >= MAX_SESSIONS {
                let oldest = sessions
                    .iter()
                    .min_by_key(|(_, s)| s.expires_at)
                    .map(|(k, _)| k.clone());
                match oldest {
                    Some(key) => sessions.remove(&key),
                    None => break,
                };
            }
            sessions.insert(hash, session);
        })
        .await?;
        Ok(id)
    }

    /// Whether `id` names a live session created under `host`. Looks up by
    /// hash, so lookup timing reveals nothing about valid ids.
    pub(crate) fn is_valid(&self, id: &str, host: &str) -> bool {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions
            .get(&hash_id(id))
            .is_some_and(|s| s.expires_at > now_secs() && s.host.eq_ignore_ascii_case(host))
    }

    /// Ends every session ("log out everywhere").
    pub(crate) async fn revoke_all(&self) -> io::Result<()> {
        self.update(HashMap::clear).await
    }

    /// Ensures the file exists (written even if empty).
    pub(crate) async fn persist(&self) -> io::Result<()> {
        self.update(|_| {}).await
    }

    /// Applies `mutate` to a copy, writes the copy durably, then publishes it.
    async fn update(&self, mutate: impl FnOnce(&mut HashMap<String, Session>)) -> io::Result<()> {
        let _ordered = self.write_lock.lock().await;
        let mut next = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        mutate(&mut next);
        let bytes = serde_json::to_vec(&SessionFile {
            v: FILE_VERSION,
            sessions: next.clone(),
        })
        .map_err(io::Error::other)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || crate::atomic_file::write_atomically(&path, &bytes))
            .await
            .map_err(io::Error::other)??;
        *self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        Ok(())
    }
}

/// 244 random bits (two v4 UUIDs, from the OS CSPRNG), as 64 hex chars.
fn new_session_id() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn hash_id(id: &str) -> String {
    format!("{:x}", sha2::Sha256::digest(id.as_bytes()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::load(dir.path().join("sessions.json")).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn a_session_is_valid_only_under_its_host() {
        let (_dir, store) = store();
        let id = store.create("localhost:3000").await.unwrap();
        assert!(store.is_valid(&id, "localhost:3000"));
        assert!(store.is_valid(&id, "LOCALHOST:3000"));
        assert!(!store.is_valid(&id, "127.0.0.1:3000"));
        assert!(!store.is_valid(&id, "evil.example:3000"));
        assert!(!store.is_valid("not-a-session", "localhost:3000"));
    }

    #[tokio::test]
    async fn sessions_survive_a_reload_and_only_hashes_are_stored() {
        let (dir, store) = store();
        let id = store.create("localhost:3000").await.unwrap();
        let raw = std::fs::read_to_string(dir.path().join("sessions.json")).unwrap();
        assert!(!raw.contains(&id), "the file must not contain usable ids");

        let reloaded = SessionStore::load(dir.path().join("sessions.json")).unwrap();
        assert!(reloaded.is_valid(&id, "localhost:3000"));
    }

    #[tokio::test]
    async fn the_session_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, store) = store();
        store.create("localhost:3000").await.unwrap();
        let mode = std::fs::metadata(dir.path().join("sessions.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn revoke_all_ends_every_session_durably() {
        let (dir, store) = store();
        let a = store.create("localhost:3000").await.unwrap();
        let b = store.create("localhost:3000").await.unwrap();
        store.revoke_all().await.unwrap();
        assert!(!store.is_valid(&a, "localhost:3000"));
        let reloaded = SessionStore::load(dir.path().join("sessions.json")).unwrap();
        assert!(!reloaded.is_valid(&b, "localhost:3000"));
    }

    #[tokio::test]
    async fn expired_sessions_are_rejected_and_dropped_on_load() {
        let (dir, store) = store();
        let id = store.create("localhost:3000").await.unwrap();
        store
            .update(|s| s.values_mut().for_each(|s| s.expires_at = 1))
            .await
            .unwrap();
        assert!(!store.is_valid(&id, "localhost:3000"));
        let reloaded = SessionStore::load(dir.path().join("sessions.json")).unwrap();
        assert!(reloaded.sessions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_number_of_sessions_is_bounded() {
        let (_dir, store) = store();
        let first = store.create("localhost:3000").await.unwrap();
        // Make the first the oldest so it is the one evicted.
        store
            .update(|s| s.values_mut().for_each(|s| s.expires_at -= 10))
            .await
            .unwrap();
        for _ in 0..MAX_SESSIONS {
            store.create("localhost:3000").await.unwrap();
        }
        assert_eq!(store.sessions.lock().unwrap().len(), MAX_SESSIONS);
        assert!(!store.is_valid(&first, "localhost:3000"));
    }

    #[test]
    fn a_corrupt_file_is_an_empty_store_but_unreadable_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        std::fs::write(&path, b"{nope").unwrap();
        assert!(
            SessionStore::load(path.clone())
                .unwrap()
                .sessions
                .lock()
                .unwrap()
                .is_empty()
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(SessionStore::load(path).is_err());
    }

    #[test]
    fn session_ids_are_long_and_distinct() {
        let a = new_session_id();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, new_session_id());
    }
}
