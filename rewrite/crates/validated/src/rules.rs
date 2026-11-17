//! The building blocks rules are made of. None of them is ever constructed:
//! they only exist as type parameters.
//!
//! Strings can't be type parameters, so the data a block needs (a character
//! set, a list, a name) comes from a small marker type implementing
//! [`CharSet`], [`Strings`] or [`Label`].

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

/// What a value is called in error messages; see [`Named`].
pub trait Label {
    const NAME: &'static str;
}

/// Between `MIN` and `MAX` characters long, inclusive.
pub struct Len<const MIN: usize, const MAX: usize>;

/// Only characters from `C`.
pub struct Chars<C>(PhantomData<fn() -> C>);

/// Not any of `S`'s strings, ignoring ASCII case.
pub struct NoneOf<S>(PhantomData<fn() -> S>);

/// Not starting with any of `S`'s strings, ignoring ASCII case.
pub struct NotPrefixed<S>(PhantomData<fn() -> S>);

/// Rule `R`, with errors calling the value `L::NAME`.
pub struct Named<L, R>(PhantomData<fn() -> (L, R)>);

impl<const MIN: usize, const MAX: usize> Rule for Len<MIN, MAX> {
    fn check(value: &str) -> Result<(), String> {
        // Stop counting past MAX: the value may be long and untrusted.
        let len = value.chars().take(MAX.saturating_add(1)).count();
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

impl<C: CharSet> Rule for Chars<C> {
    fn check(value: &str) -> Result<(), String> {
        match value.chars().find(|&c| !C::contains(c)) {
            Some(c) => Err(format!("contains {c:?} (allowed: {})", C::ALLOWED)),
            None => Ok(()),
        }
    }
}

impl<S: Strings> Rule for NoneOf<S> {
    fn check(value: &str) -> Result<(), String> {
        if S::LIST.iter().any(|s| s.eq_ignore_ascii_case(value)) {
            Err(format!("is not allowed: {}", S::REASON))
        } else {
            Ok(())
        }
    }
}

impl<S: Strings> Rule for NotPrefixed<S> {
    fn check(value: &str) -> Result<(), String> {
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

impl<L: Label, R: Rule> Rule for Named<L, R> {
    const SUBJECT: &'static str = L::NAME;

    fn check(value: &str) -> Result<(), String> {
        R::check(value)
    }
}

/// `(A, B, ...)`: every rule must pass, checked left to right.
macro_rules! all_of {
    ($($rule:ident),+) => {
        impl<$($rule: Rule),+> Rule for ($($rule,)+) {
            fn check(value: &str) -> Result<(), String> {
                $($rule::check(value)?;)+
                Ok(())
            }
        }
    };
}
all_of!(A);
all_of!(A, B);
all_of!(A, B, C);
all_of!(A, B, C, D);
all_of!(A, B, C, D, E);
all_of!(A, B, C, D, E, F);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Validated;

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

    struct PinLabel;
    impl Label for PinLabel {
        const NAME: &'static str = "PIN";
    }

    fn passes<R: Rule>(value: &str) -> bool {
        R::check(value).is_ok()
    }

    fn problem<R: Rule>(value: &str) -> String {
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
    fn tuples_check_left_to_right_and_stop_at_the_first_failure() {
        type Pin = (Len<4, 4>, Chars<Digits>);
        type One = (Chars<Digits>,);
        type Six = (
            Len<1, 9>,
            Chars<Digits>,
            NoneOf<Colours>,
            NotPrefixed<Colours>,
            Len<0, 9>,
            Chars<Digits>,
        );

        assert!(passes::<Pin>("1234"));
        assert_eq!(problem::<Pin>("12a"), "is shorter than 4 characters");
        assert_eq!(problem::<Pin>("12a4"), "contains 'a' (allowed: 0-9)");
        assert!(passes::<One>("1"));
        assert!(passes::<Six>("123"));
        assert_eq!(problem::<Six>(""), "must not be empty");
    }

    #[test]
    fn named_sets_the_subject_of_errors() {
        type Pin = Validated<String, Named<PinLabel, (Len<4, 4>, Chars<Digits>)>>;
        assert!(Pin::new("1234".to_owned()).is_ok());
        assert_eq!(
            Pin::new("12x4".to_owned()).unwrap_err().to_string(),
            "PIN \"12x4\" contains 'x' (allowed: 0-9)"
        );
        assert_eq!(
            Validated::<String, Len<1, 2>>::new(String::new())
                .unwrap_err()
                .subject,
            "value",
            "unnamed rules call it a value"
        );
    }
}
