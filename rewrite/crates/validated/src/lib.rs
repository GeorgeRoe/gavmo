//! Strings that are checked once, when created, against a [`Rule`] chosen by
//! type parameter, so holding a value means it passed.
//!
//! Rules are built from small parts, for example:
//!
//! ```
//! use validated::{CharSet, Chars, Label, Len, Named, Validated};
//!
//! struct Lowercase;
//! impl CharSet for Lowercase {
//!     const ALLOWED: &'static str = "a-z";
//!     fn contains(c: char) -> bool {
//!         c.is_ascii_lowercase()
//!     }
//! }
//!
//! struct UsernameLabel;
//! impl Label for UsernameLabel {
//!     const NAME: &'static str = "username";
//! }
//!
//! type Username = Validated<String, Named<UsernameLabel, (Len<1, 16>, Chars<Lowercase>)>>;
//!
//! assert!(Username::new("george".to_owned()).is_ok());
//! assert_eq!(
//!     Username::new("George".to_owned()).unwrap_err().to_string(),
//!     "username \"George\" contains 'G' (allowed: a-z)"
//! );
//! ```

mod rules;

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Deref;

use serde::{Deserialize, Deserializer};

pub use rules::{CharSet, Chars, Label, Len, Named, NoneOf, NotPrefixed, Strings};

/// A check on a string. Tuples of rules are rules too: every one must pass,
/// checked left to right, and the first failure is reported.
pub trait Rule {
    /// What the value is called in error messages. Set with [`Named`].
    const SUBJECT: &'static str = "value";

    fn check(value: &str) -> Result<(), Problem>;
}

/// What a [`Rule`] found wrong. Phrased to follow the value in a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("must not be empty")]
    Empty,
    #[error("is shorter than {min} characters")]
    TooShort { min: usize },
    #[error("is longer than {max} characters")]
    TooLong { max: usize },
    #[error("contains {ch:?} (allowed: {allowed})")]
    BadChar { ch: char, allowed: &'static str },
    #[error("is not allowed: {reason}")]
    NotAllowed { reason: &'static str },
}

/// A value that failed its rule, e.g. `tool id "a b" contains ' ' (allowed: ...)`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{subject} {value:?} {problem}")]
pub struct Invalid {
    pub subject: &'static str,
    pub value: String,
    pub problem: Problem,
}

/// A `T` that has passed rule `R`. Read-only: it derefs to `T`, and the only
/// ways to get one are [`Validated::new`] and deserializing.
///
/// Comparison, hashing and formatting use `T` alone, so the rule types never
/// need to implement anything but [`Rule`].
pub struct Validated<T, R> {
    value: T,
    rule: PhantomData<fn() -> R>,
}

impl<T: AsRef<str>, R: Rule> Validated<T, R> {
    pub fn new(value: T) -> Result<Self, Invalid> {
        match R::check(value.as_ref()) {
            Ok(()) => Ok(Self {
                value,
                rule: PhantomData,
            }),
            Err(problem) => Err(Invalid {
                subject: R::SUBJECT,
                value: value.as_ref().to_owned(),
                problem,
            }),
        }
    }
}

impl<T, R> Validated<T, R> {
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T, R> Deref for Validated<T, R> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

/// Lets a map keyed by validated strings be searched with a plain `&str`.
impl<T: Borrow<str>, R> Borrow<str> for Validated<T, R> {
    fn borrow(&self) -> &str {
        self.value.borrow()
    }
}

impl<'de, T, R> Deserialize<'de> for Validated<T, R>
where
    T: Deserialize<'de> + AsRef<str>,
    R: Rule,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(T::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl<T: fmt::Display, R> fmt::Display for Validated<T, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(f)
    }
}

impl<T: fmt::Debug, R> fmt::Debug for Validated<T, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(f)
    }
}

impl<T: Clone, R> Clone for Validated<T, R> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            rule: PhantomData,
        }
    }
}

impl<T: PartialEq, R> PartialEq for Validated<T, R> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq, R> Eq for Validated<T, R> {}

impl<T: PartialOrd, R> PartialOrd for Validated<T, R> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.value.partial_cmp(&other.value)
    }
}

impl<T: Ord, R> Ord for Validated<T, R> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value.cmp(&other.value)
    }
}

impl<T: Hash, R> Hash for Validated<T, R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use serde::de::IntoDeserializer;
    use serde::de::value::{Error as DeError, StrDeserializer};

    use super::*;

    /// Accepts anything except "bad".
    struct NotBad;
    impl Rule for NotBad {
        fn check(value: &str) -> Result<(), Problem> {
            if value == "bad" {
                Err(Problem::NotAllowed { reason: "it's bad" })
            } else {
                Ok(())
            }
        }
    }

    type Checked = Validated<String, NotBad>;

    fn checked(s: &str) -> Checked {
        Checked::new(s.to_owned()).unwrap()
    }

    #[test]
    fn new_keeps_a_value_that_passes() {
        let value = checked("good");
        assert_eq!(*value, "good");
        assert_eq!(value.as_str(), "good");
        assert_eq!(value.into_inner(), "good");
    }

    #[test]
    fn new_refuses_a_value_that_fails() {
        let err = Checked::new("bad".to_owned()).unwrap_err();
        assert_eq!(
            err,
            Invalid {
                subject: "value",
                value: "bad".to_owned(),
                problem: Problem::NotAllowed { reason: "it's bad" },
            }
        );
        assert_eq!(err.to_string(), "value \"bad\" is not allowed: it's bad");
    }

    #[test]
    fn works_over_borrowed_strings_too() {
        assert!(Validated::<&str, NotBad>::new("good").is_ok());
        assert!(Validated::<&str, NotBad>::new("bad").is_err());
    }

    #[test]
    fn deserializing_checks_the_rule() {
        let de = |s: &'static str| -> StrDeserializer<'static, DeError> { s.into_deserializer() };
        assert_eq!(Checked::deserialize(de("good")).unwrap(), checked("good"));
        assert_eq!(
            Checked::deserialize(de("bad")).unwrap_err().to_string(),
            "value \"bad\" is not allowed: it's bad"
        );
    }

    #[test]
    fn formats_compares_and_hashes_as_the_inner_value() {
        assert_eq!(checked("a").to_string(), "a");
        assert_eq!(format!("{:?}", checked("a")), "\"a\"");
        assert_eq!(checked("a").clone(), checked("a"));
        assert_ne!(checked("a"), checked("b"));
        assert!(checked("a") < checked("b"));
        assert_eq!(
            checked("a").partial_cmp(&checked("b")),
            Some(Ordering::Less)
        );
        let set: HashSet<_> = [checked("a"), checked("a"), checked("b")].into();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn maps_can_be_searched_by_str() {
        let map = BTreeMap::from([(checked("key"), 1)]);
        assert_eq!(map.get("key"), Some(&1));
        assert_eq!(map["key"], 1);
    }

    #[test]
    fn problems_read_as_the_end_of_a_sentence() {
        for (problem, want) in [
            (Problem::Empty, "must not be empty"),
            (Problem::TooShort { min: 3 }, "is shorter than 3 characters"),
            (Problem::TooLong { max: 9 }, "is longer than 9 characters"),
            (
                Problem::BadChar {
                    ch: ' ',
                    allowed: "a-z",
                },
                "contains ' ' (allowed: a-z)",
            ),
            (Problem::NotAllowed { reason: "why" }, "is not allowed: why"),
        ] {
            assert_eq!(problem.to_string(), want);
        }
    }
}
