//! Rules for numbers.

use crate::Rule;

/// Between `MIN` and `MAX`, inclusive. Works for every integer type: the
/// bounds are `i128` so one rule covers them all, e.g.
/// `Validated<u32, InRange<16, 1024>>` or `Validated<i8, InRange<-5, 5>>`.
pub struct InRange<const MIN: i128, const MAX: i128>;

/// `None` is a value too big for `i128`, which only a `u128` can be.
fn in_range<const MIN: i128, const MAX: i128>(value: Option<i128>) -> Result<(), String> {
    match value {
        Some(v) if v < MIN => Err(format!("is less than {MIN}")),
        Some(v) if v <= MAX => Ok(()),
        _ => Err(format!("is greater than {MAX}")),
    }
}

macro_rules! in_range_for {
    ($to_i128:expr => $($int:ty),+) => {$(
        impl<const MIN: i128, const MAX: i128> Rule<$int> for InRange<MIN, MAX> {
            fn check(value: &$int) -> Result<(), String> {
                in_range::<MIN, MAX>($to_i128(*value))
            }
        }
    )+};
}
in_range_for!(|v| Some(i128::from(v)) => i8, i16, i32, i64, u8, u16, u32, u64);
in_range_for!(Some => i128);
in_range_for!(|v| i128::try_from(v).ok() => isize, usize, u128);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Label, Named, Validated};

    fn check<R: Rule<T>, T: Copy>(value: T) -> Result<(), String> {
        R::check(&value)
    }

    #[test]
    fn bounds_are_inclusive() {
        type R = InRange<16, 1024>;
        assert_eq!(check::<R, u32>(15), Err("is less than 16".to_owned()));
        assert_eq!(check::<R, u32>(16), Ok(()));
        assert_eq!(check::<R, u32>(1024), Ok(()));
        assert_eq!(
            check::<R, u32>(1025),
            Err("is greater than 1024".to_owned())
        );
    }

    #[test]
    fn negative_bounds_work() {
        type R = InRange<-5, 5>;
        assert_eq!(check::<R, i8>(-6), Err("is less than -5".to_owned()));
        assert_eq!(check::<R, i8>(-5), Ok(()));
        assert_eq!(check::<R, i8>(5), Ok(()));
        assert_eq!(check::<R, i8>(i8::MIN), Err("is less than -5".to_owned()));
    }

    #[test]
    fn works_for_every_integer_type() {
        type R = InRange<1, 9>;
        assert!(check::<R, i8>(1).is_ok() && check::<R, i8>(0).is_err());
        assert!(check::<R, i16>(1).is_ok() && check::<R, i16>(0).is_err());
        assert!(check::<R, i32>(1).is_ok() && check::<R, i32>(0).is_err());
        assert!(check::<R, i64>(1).is_ok() && check::<R, i64>(0).is_err());
        assert!(check::<R, i128>(1).is_ok() && check::<R, i128>(0).is_err());
        assert!(check::<R, isize>(1).is_ok() && check::<R, isize>(0).is_err());
        assert!(check::<R, u8>(1).is_ok() && check::<R, u8>(0).is_err());
        assert!(check::<R, u16>(1).is_ok() && check::<R, u16>(0).is_err());
        assert!(check::<R, u32>(1).is_ok() && check::<R, u32>(0).is_err());
        assert!(check::<R, u64>(1).is_ok() && check::<R, u64>(0).is_err());
        assert!(check::<R, u128>(1).is_ok() && check::<R, u128>(0).is_err());
        assert!(check::<R, usize>(1).is_ok() && check::<R, usize>(0).is_err());
    }

    #[test]
    fn extremes_compare_correctly() {
        assert_eq!(
            check::<InRange<0, { i128::MAX }>, u128>(u128::MAX),
            Err(format!("is greater than {}", i128::MAX)),
            "beyond i128"
        );
        assert_eq!(
            check::<InRange<0, { i128::MAX }>, u128>(i128::MAX.cast_unsigned()),
            Ok(())
        );
        assert_eq!(
            check::<InRange<{ i128::MIN }, { i128::MAX }>, i128>(i128::MIN),
            Ok(())
        );
        assert_eq!(
            check::<InRange<0, 10>, u64>(u64::MAX),
            Err("is greater than 10".to_owned())
        );
    }

    #[test]
    fn errors_show_the_number() {
        struct DiskLabel;
        impl Label for DiskLabel {
            const NAME: &'static str = "disk size";
        }
        type DiskGib = Validated<u32, Named<DiskLabel, InRange<16, 1024>>>;

        assert_eq!(*DiskGib::new(32).unwrap(), 32);
        assert_eq!(
            DiskGib::new(2048).unwrap_err().to_string(),
            "disk size 2048 is greater than 1024"
        );
    }
}
