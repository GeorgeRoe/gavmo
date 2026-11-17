//! The building blocks rules are made of. None of them is ever constructed:
//! they only exist as type parameters.
//!
//! Strings can't be type parameters, so the data a block needs (a character
//! set, a list, a name) comes from a small marker type implementing
//! [`CharSet`], [`Strings`] or [`Label`].

use std::marker::PhantomData;

use crate::{Problem, Rule};

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
    fn check(value: &str) -> Result<(), Problem> {
        // Stop counting past MAX: the value may be long and untrusted.
        let len = value.chars().take(MAX.saturating_add(1)).count();
        if len > MAX {
            Err(Problem::TooLong { max: MAX })
        } else if len >= MIN {
            Ok(())
        } else if len == 0 {
            Err(Problem::Empty)
        } else {
            Err(Problem::TooShort { min: MIN })
        }
    }
}

impl<C: CharSet> Rule for Chars<C> {
    fn check(value: &str) -> Result<(), Problem> {
        match value.chars().find(|&c| !C::contains(c)) {
            Some(ch) => Err(Problem::BadChar {
                ch,
                allowed: C::ALLOWED,
            }),
            None => Ok(()),
        }
    }
}

impl<S: Strings> Rule for NoneOf<S> {
    fn check(value: &str) -> Result<(), Problem> {
        if S::LIST.iter().any(|s| s.eq_ignore_ascii_case(value)) {
            Err(Problem::NotAllowed { reason: S::REASON })
        } else {
            Ok(())
        }
    }
}

impl<S: Strings> Rule for NotPrefixed<S> {
    fn check(value: &str) -> Result<(), Problem> {
        let has_prefix = |prefix: &str| {
            value
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        };
        if S::LIST.iter().copied().any(has_prefix) {
            Err(Problem::NotAllowed { reason: S::REASON })
        } else {
            Ok(())
        }
    }
}

impl<L: Label, R: Rule> Rule for Named<L, R> {
    const SUBJECT: &'static str = L::NAME;

    fn check(value: &str) -> Result<(), Problem> {
        R::check(value)
    }
}

/// `(A, B, ...)`: every rule must pass, checked left to right.
macro_rules! all_of {
    ($($rule:ident),+) => {
        impl<$($rule: Rule),+> Rule for ($($rule,)+) {
            fn check(value: &str) -> Result<(), Problem> {
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

    fn check<R: Rule>(value: &str) -> Result<(), Problem> {
        R::check(value)
    }

    #[test]
    fn len_counts_characters_inclusively() {
        type L = Len<2, 4>;
        assert_eq!(check::<L>(""), Err(Problem::Empty));
        assert_eq!(check::<L>("a"), Err(Problem::TooShort { min: 2 }));
        assert_eq!(check::<L>("ab"), Ok(()));
        assert_eq!(check::<L>("abcd"), Ok(()));
        assert_eq!(check::<L>("éééé"), Ok(()), "characters, not bytes");
        assert_eq!(check::<L>("abcde"), Err(Problem::TooLong { max: 4 }));
        assert_eq!(check::<Len<0, 1>>(""), Ok(()));
        assert_eq!(check::<Len<0, { usize::MAX }>>("anything"), Ok(()));
    }

    #[test]
    fn chars_reports_the_first_character_not_in_the_set() {
        assert_eq!(check::<Chars<Digits>>("0123"), Ok(()));
        assert_eq!(check::<Chars<Digits>>(""), Ok(()));
        assert_eq!(
            check::<Chars<Digits>>("12a b"),
            Err(Problem::BadChar {
                ch: 'a',
                allowed: "0-9"
            })
        );
    }

    #[test]
    fn none_of_ignores_ascii_case() {
        let refused = Err(Problem::NotAllowed {
            reason: "colours are taken",
        });
        assert_eq!(check::<NoneOf<Colours>>("RED"), refused);
        assert_eq!(check::<NoneOf<Colours>>("green"), refused);
        assert_eq!(check::<NoneOf<Colours>>("reddish"), Ok(()));
        assert_eq!(check::<NoneOf<Colours>>("blue"), Ok(()));
    }

    #[test]
    fn not_prefixed_ignores_ascii_case() {
        let refused = Err(Problem::NotAllowed {
            reason: "colours are taken",
        });
        assert_eq!(check::<NotPrefixed<Colours>>("reddish"), refused);
        assert_eq!(check::<NotPrefixed<Colours>>("Green-ish"), refused);
        assert_eq!(check::<NotPrefixed<Colours>>("red"), refused);
        assert_eq!(check::<NotPrefixed<Colours>>("re"), Ok(()));
        assert_eq!(check::<NotPrefixed<Colours>>("bored"), Ok(()));
        // A multi-byte character where the prefix would end is no panic.
        assert_eq!(check::<NotPrefixed<Colours>>("ré"), Ok(()));
        assert_eq!(check::<NotPrefixed<Colours>>("éé"), Ok(()));
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

        assert_eq!(check::<Pin>("1234"), Ok(()));
        assert_eq!(check::<Pin>("12a"), Err(Problem::TooShort { min: 4 }));
        assert!(matches!(check::<Pin>("12a4"), Err(Problem::BadChar { .. })));
        assert_eq!(check::<One>("1"), Ok(()));
        assert_eq!(check::<Six>("123"), Ok(()));
        assert_eq!(check::<Six>(""), Err(Problem::Empty));
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
