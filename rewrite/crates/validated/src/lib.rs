//! Values that are checked once, when created, against a [`Rule`] chosen by
//! type parameter, so holding a value means it passed.
//!
//! Rules are built from small parts, for example:
//!
//! ```
//! use validated::{CharSet, Chars, InRange, Label, Len, Named, Validated};
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
//! type Percent = Validated<u8, InRange<0, 100>>;
//!
//! assert!(Username::new("george".to_owned()).is_ok());
//! assert_eq!(
//!     Username::new("George".to_owned()).unwrap_err().to_string(),
//!     "username \"George\" contains 'G' (allowed: a-z)"
//! );
//! assert_eq!(
//!     Percent::new(101).unwrap_err().to_string(),
//!     "value 101 is greater than 100"
//! );
//! ```
//!
//! A rule of your own implements [`Rule`] for the types it can check; for
//! strings, usually every `S: AsRef<str>`, so it works on `String` and `&str`.

mod combine;
mod invalid;
mod numbers;
mod rule;
mod strings;
mod validated;

pub use combine::{Label, Named};
pub use invalid::Invalid;
pub use numbers::InRange;
pub use rule::Rule;
pub use strings::{CharSet, Chars, Len, NoneOf, NotPrefixed, Strings};
pub use validated::Validated;
