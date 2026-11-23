//! [`Invalid`]: a value that failed its rule.

/// A value that failed its rule, e.g. `tool id "a b" contains ' ' (allowed: ...)`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{subject} {value} {problem}")]
pub struct Invalid {
    pub subject: &'static str,
    /// The value as `{:?}` shows it: strings quoted, numbers plain.
    pub value: String,
    pub problem: String,
}
