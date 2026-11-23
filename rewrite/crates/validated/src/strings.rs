//! Rules for strings. Each works on anything that is `AsRef<str>`, so the
//! same rule checks a `String`, a `&str` or a `Box<str>`.
//!
//! Strings can't be type parameters, so the data a rule needs (a character
//! set, a list) comes from a small marker type implementing [`CharSet`] or
//! [`Strings`].

use std::marker::PhantomData;

use crate::Rule;

/// The characters [`Chars`] allows.
pub trait CharSet {
    /// For error messages, e.g. `"A-Z, a-z and 0-9"`.
    const ALLOWED: &'static str;

    fn contains(c: char) -> bool;
}

/// A list of strings for [`NoneOf`] or [`NotPrefixed`], and why they're
/// refused.
pub trait Strings {
    const LIST: &'static [&'static str];
    /// Completes "is not allowed: ...".
    const REASON: &'static str;
}

/// Between `MIN` and `MAX` characters long, inclusive.
pub struct Len<const MIN: usize, const MAX: usize>;

/// Only characters from `C`.
pub struct Chars<C>(PhantomData<fn() -> C>);

/// Not any of `S`'s strings, ignoring ASCII case.
pub struct NoneOf<S>(PhantomData<fn() -> S>);

/// Not starting with any of `S`'s strings, ignoring ASCII case.
pub struct NotPrefixed<S>(PhantomData<fn() -> S>);

impl<V, const MIN: usize, const MAX: usize> Rule<V> for Len<MIN, MAX>
where
    V: AsRef<str> + ?Sized,
{
    fn check(value: &V) -> Result<(), String> {
        // Stop counting past MAX: the value may be long and untrusted.
        let len = value.as_ref().chars().take(MAX.saturating_add(1)).count();
        if len > MAX {
            Err(format!("is longer than {MAX} characters"))
        } else if len >= MIN {
            Ok(())
        } else if len == 0 {
            Err("must not be empty".to_owned())
        } else {
            Err(format!("is shorter than {MIN} characters"))
        }
    }
}

impl<V: AsRef<str> + ?Sized, C: CharSet> Rule<V> for Chars<C> {
    fn check(value: &V) -> Result<(), String> {
        match value.as_ref().chars().find(|&c| !C::contains(c)) {
            Some(c) => Err(format!("contains {c:?} (allowed: {})", C::ALLOWED)),
            None => Ok(()),
        }
    }
}

impl<V: AsRef<str> + ?Sized, S: Strings> Rule<V> for NoneOf<S> {
    fn check(value: &V) -> Result<(), String> {
        let value = value.as_ref();
        if S::LIST.iter().any(|s| s.eq_ignore_ascii_case(value)) {
            Err(format!("is not allowed: {}", S::REASON))
        } else {
            Ok(())
        }
    }
}

impl<V: AsRef<str> + ?Sized, S: Strings> Rule<V> for NotPrefixed<S> {
    fn check(value: &V) -> Result<(), String> {
        let value = value.as_ref();
        let has_prefix = |prefix: &str| {
            value
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        };
        if S::LIST.iter().copied().any(has_prefix) {
            Err(format!("is not allowed: {}", S::REASON))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Label, Named, Validated};

    struct Digits;
    impl CharSet for Digits {
        const ALLOWED: &'static str = "0-9";
        fn contains(c: char) -> bool {
            c.is_ascii_digit()
        }
    }

    struct Colours;
    impl Strings for Colours {
        const LIST: &'static [&'static str] = &["red", "green"];
        const REASON: &'static str = "colours are taken";
    }

    fn passes<R: Rule<str>>(value: &str) -> bool {
        R::check(value).is_ok()
    }

    fn problem<R: Rule<str>>(value: &str) -> String {
        R::check(value).unwrap_err()
    }

    #[test]
    fn len_counts_characters_inclusively() {
        type L = Len<2, 4>;
        assert_eq!(problem::<L>(""), "must not be empty");
        assert_eq!(problem::<L>("a"), "is shorter than 2 characters");
        assert!(passes::<L>("ab"));
        assert!(passes::<L>("abcd"));
        assert!(passes::<L>("éééé"), "characters, not bytes");
        assert_eq!(problem::<L>("abcde"), "is longer than 4 characters");
        assert!(passes::<Len<0, 1>>(""));
        assert!(passes::<Len<0, { usize::MAX }>>("anything"));
    }

    #[test]
    fn chars_reports_the_first_character_not_in_the_set() {
        assert!(passes::<Chars<Digits>>("0123"));
        assert!(passes::<Chars<Digits>>(""));
        assert_eq!(
            problem::<Chars<Digits>>("12a b"),
            "contains 'a' (allowed: 0-9)"
        );
    }

    #[test]
    fn none_of_ignores_ascii_case() {
        for refused in ["RED", "green"] {
            assert_eq!(
                problem::<NoneOf<Colours>>(refused),
                "is not allowed: colours are taken"
            );
        }
        assert!(passes::<NoneOf<Colours>>("reddish"));
        assert!(passes::<NoneOf<Colours>>("blue"));
    }

    #[test]
    fn not_prefixed_ignores_ascii_case() {
        for refused in ["reddish", "Green-ish", "red"] {
            assert_eq!(
                problem::<NotPrefixed<Colours>>(refused),
                "is not allowed: colours are taken"
            );
        }
        assert!(passes::<NotPrefixed<Colours>>("re"));
        assert!(passes::<NotPrefixed<Colours>>("bored"));
        // A multi-byte character where the prefix would end is no panic.
        assert!(passes::<NotPrefixed<Colours>>("ré"));
        assert!(passes::<NotPrefixed<Colours>>("éé"));
    }

    #[test]
    fn work_on_any_string_type() {
        struct PinLabel;
        impl Label for PinLabel {
            const NAME: &'static str = "PIN";
        }
        type Rules = Named<PinLabel, (Len<4, 4>, Chars<Digits>)>;

        assert!(Validated::<String, Rules>::new("1234".to_owned()).is_ok());
        assert!(Validated::<&str, Rules>::new("1234").is_ok());
        assert!(Validated::<Box<str>, Rules>::new("1234".into()).is_ok());
        assert_eq!(
            Validated::<&str, Rules>::new("12x4")
                .unwrap_err()
                .to_string(),
            "PIN \"12x4\" contains 'x' (allowed: 0-9)"
        );
    }
}
