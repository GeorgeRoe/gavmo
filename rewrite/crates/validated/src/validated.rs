//! [`Validated`]: a value that has passed its rule.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Deref;

use serde::{Deserialize, Deserializer};

use crate::{Invalid, Rule};

/// A `T` that has passed rule `R`. Read-only: it derefs to `T`, and the only
/// ways to get one are [`Validated::new`] and deserializing.
///
/// Comparison, hashing and formatting use `T` alone, so the rule types never
/// need to implement anything but [`Rule`].
pub struct Validated<T, R> {
    value: T,
    rule: PhantomData<fn() -> R>,
}

impl<T: fmt::Debug, R: Rule<T>> Validated<T, R> {
    pub fn new(value: T) -> Result<Self, Invalid> {
        match R::check(&value) {
            Ok(()) => Ok(Self {
                value,
                rule: PhantomData,
            }),
            Err(problem) => Err(Invalid {
                subject: R::SUBJECT,
                value: format!("{value:?}"),
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
    T: Deserialize<'de> + fmt::Debug,
    R: Rule<T>,
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
    use serde::de::value::{Error as DeError, StrDeserializer, U32Deserializer};

    use super::*;

    /// Accepts any string except "bad".
    struct NotBad;
    impl<S: AsRef<str> + ?Sized> Rule<S> for NotBad {
        fn check(value: &S) -> Result<(), String> {
            if value.as_ref() == "bad" {
                Err("is not allowed: it's bad".to_owned())
            } else {
                Ok(())
            }
        }
    }

    /// Accepts even numbers.
    struct Even;
    impl Rule<u32> for Even {
        fn check(value: &u32) -> Result<(), String> {
            if value.is_multiple_of(2) {
                Ok(())
            } else {
                Err("is odd".to_owned())
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
                value: "\"bad\"".to_owned(),
                problem: "is not allowed: it's bad".to_owned(),
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
    fn works_over_any_type_with_a_rule() {
        assert_eq!(*Validated::<u32, Even>::new(4).unwrap(), 4);
        assert_eq!(
            Validated::<u32, Even>::new(3).unwrap_err().to_string(),
            "value 3 is odd"
        );
    }

    #[test]
    fn deserializing_checks_the_rule() {
        let de = |s: &'static str| -> StrDeserializer<'static, DeError> { s.into_deserializer() };
        assert_eq!(Checked::deserialize(de("good")).unwrap(), checked("good"));
        assert_eq!(
            Checked::deserialize(de("bad")).unwrap_err().to_string(),
            "value \"bad\" is not allowed: it's bad"
        );

        let de = |n: u32| -> U32Deserializer<DeError> { n.into_deserializer() };
        assert_eq!(*Validated::<u32, Even>::deserialize(de(2)).unwrap(), 2);
        assert_eq!(
            Validated::<u32, Even>::deserialize(de(1))
                .unwrap_err()
                .to_string(),
            "value 1 is odd"
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
}
