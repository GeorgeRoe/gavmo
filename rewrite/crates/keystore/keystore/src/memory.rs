//! [`MemoryKeystore`]: an in-process keystore for tests (behind the
//! `test-util` feature).

use std::sync::{Mutex, MutexGuard, PoisonError};

use zeroize::Zeroizing;

use crate::{Error, Keystore, SecretBytes};

/// A keystore in memory that can also play an unreachable one. Not
/// persisted; for tests only.
#[derive(Debug, Default)]
pub struct MemoryKeystore {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Empty,
    Holding(SecretBytes),
    /// Every call fails, like a keystore that can't be reached.
    Unavailable,
    /// Reads say "nothing stored", but storing fails: reachable, but with
    /// nowhere to put a new secret.
    RefusesCreate,
}

impl MemoryKeystore {
    /// Holding nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn holding(secret: &[u8]) -> Self {
        Self::with(State::Holding(Zeroizing::new(secret.to_vec())))
    }

    pub fn unavailable() -> Self {
        Self::with(State::Unavailable)
    }

    pub fn refusing_create() -> Self {
        Self::with(State::RefusesCreate)
    }

    /// A copy of the secret held, if any.
    pub fn held(&self) -> Option<Vec<u8>> {
        match &*self.lock() {
            State::Holding(secret) => Some(secret.to_vec()),
            State::Empty | State::Unavailable | State::RefusesCreate => None,
        }
    }

    fn with(state: State) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }

    /// Recovers from poisoning: there is one value, with nothing to keep
    /// consistent.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

const NAME: &str = "the in-memory keystore";

impl Keystore for MemoryKeystore {
    fn name(&self) -> &'static str {
        NAME
    }

    fn load(&self) -> Result<Option<SecretBytes>, Error> {
        match &*self.lock() {
            State::Empty | State::RefusesCreate => Ok(None),
            State::Holding(secret) => Ok(Some(secret.clone())),
            State::Unavailable => Err(Error::unavailable(NAME, "unreachable")),
        }
    }

    fn create(&self, secret: &[u8]) -> Result<(), Error> {
        let mut state = self.lock();
        match &*state {
            State::Empty => {
                *state = State::Holding(Zeroizing::new(secret.to_vec()));
                Ok(())
            }
            State::Holding(_) => Err(Error::AlreadyExists { keystore: NAME }),
            State::Unavailable => Err(Error::unavailable(NAME, "unreachable")),
            State::RefusesCreate => Err(Error::unavailable(NAME, "nowhere to store a key")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_then_loads_and_keeps_the_first() {
        let keys = MemoryKeystore::new();
        assert_eq!(keys.load().unwrap(), None);
        keys.create(b"one").unwrap();
        assert!(matches!(
            keys.create(b"two"),
            Err(Error::AlreadyExists { .. })
        ));
        assert_eq!(
            keys.load().unwrap().as_deref().map(Vec::as_slice),
            Some(&b"one"[..])
        );
        assert_eq!(keys.held(), Some(b"one".to_vec()));
    }

    #[test]
    fn plays_unreachable_keystores() {
        let down = MemoryKeystore::unavailable();
        assert!(matches!(down.load(), Err(Error::Unavailable { .. })));
        assert!(matches!(down.create(b"k"), Err(Error::Unavailable { .. })));

        let full = MemoryKeystore::refusing_create();
        assert_eq!(full.load().unwrap(), None);
        assert!(matches!(full.create(b"k"), Err(Error::Unavailable { .. })));
        assert_eq!(full.held(), None);
    }

    #[test]
    fn is_object_safe() {
        let keys: Box<dyn Keystore> = Box::new(MemoryKeystore::holding(b"k"));
        assert_eq!(keys.name(), "the in-memory keystore");
        assert!(keys.load().unwrap().is_some());
    }
}
