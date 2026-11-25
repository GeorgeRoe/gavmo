//! Rules made of other rules: [`Named`], and tuples (every part must pass).
//! Both work for any value type their parts can check.

use std::marker::PhantomData;

use crate::Rule;

/// What a value is called in error messages; see [`Named`].
pub trait Label {
    const NAME: &'static str;
}

/// Rule `R`, with errors calling the value `L::NAME`.
pub struct Named<L, R>(PhantomData<fn() -> (L, R)>);

impl<T: ?Sized, L: Label, R: Rule<T>> Rule<T> for Named<L, R> {
    const SUBJECT: &'static str = L::NAME;

    fn check(value: &T) -> Result<(), String> {
        R::check(value)
    }
}

/// `(A, B, ...)`: every rule must pass, checked left to right.
macro_rules! all_of {
    ($($rule:ident),+) => {
        impl<Value: ?Sized, $($rule: Rule<Value>),+> Rule<Value> for ($($rule,)+) {
            fn check(value: &Value) -> Result<(), String> {
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
    use crate::{InRange, Validated};

    /// Refuses one number, to see which part of a tuple failed.
    struct Not<const N: i64>;
    impl<const N: i64> Rule<i64> for Not<N> {
        fn check(value: &i64) -> Result<(), String> {
            if *value == N {
                Err(format!("is not allowed: {N} is taken"))
            } else {
                Ok(())
            }
        }
    }

    struct CountLabel;
    impl Label for CountLabel {
        const NAME: &'static str = "count";
    }

    fn check<R: Rule<i64>>(value: i64) -> Result<(), String> {
        R::check(&value)
    }

    #[test]
    fn tuples_check_left_to_right_and_stop_at_the_first_failure() {
        type Two = (InRange<0, 9>, Not<5>);
        type One = (Not<5>,);
        type Six = (Not<1>, Not<2>, Not<3>, Not<4>, Not<5>, Not<6>);

        assert_eq!(check::<Two>(4), Ok(()));
        assert_eq!(check::<Two>(10), Err("is greater than 9".to_owned()));
        assert_eq!(
            check::<Two>(5),
            Err("is not allowed: 5 is taken".to_owned())
        );
        assert_eq!(
            check::<One>(5),
            Err("is not allowed: 5 is taken".to_owned())
        );
        assert_eq!(check::<Six>(7), Ok(()));
        assert_eq!(
            check::<Six>(6),
            Err("is not allowed: 6 is taken".to_owned())
        );
    }

    #[test]
    fn named_sets_the_subject_of_errors() {
        type Count = Validated<i64, Named<CountLabel, (InRange<0, 9>, Not<5>)>>;
        assert!(Count::new(3).is_ok());
        assert_eq!(
            Count::new(5).unwrap_err().to_string(),
            "count 5 is not allowed: 5 is taken"
        );
        assert_eq!(
            Validated::<i64, Not<5>>::new(5).unwrap_err().subject,
            "value",
            "unnamed rules call it a value"
        );
    }
}
