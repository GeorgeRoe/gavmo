//! [`Rule`]: the check a [`Validated`](crate::Validated) value passed.

/// A check on a `T`. Tuples of rules are rules too: every one must pass,
/// checked left to right, and the first failure is reported.
pub trait Rule<T: ?Sized> {
    /// What the value is called in error messages. Set with
    /// [`Named`](crate::Named).
    const SUBJECT: &'static str = "value";

    /// On failure, what's wrong, phrased to follow the value in a sentence:
    /// `"is longer than 64 characters"`.
    fn check(value: &T) -> Result<(), String>;
}
