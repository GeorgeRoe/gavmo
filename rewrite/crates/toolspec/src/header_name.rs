//! [`HeaderName`]: the header an `auth: header` tool sends its token in.

use std::fmt;

use serde::Deserialize;
use validated::{CharSet, Chars, Label, Len, Named, NoneOf, NotPrefixed, Strings, Validated};

/// An HTTP header name that may carry a credential. Compares
/// case-insensitively, as HTTP does; displays as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct HeaderName(Validated<String, HeaderNameRule>);

type HeaderNameRule = Named<
    HeaderLabel,
    (
        Len<1, 128>,
        Chars<TokenChars>,
        NoneOf<RequestHeaders>,
        NotPrefixed<ProxyHeaders>,
    ),
>;

impl HeaderName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PartialEq for HeaderName {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for HeaderName {}

impl fmt::Display for HeaderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

struct HeaderLabel;
impl Label for HeaderLabel {
    const NAME: &'static str = "header_name";
}

/// An RFC 9110 token.
struct TokenChars;
impl CharSet for TokenChars {
    const ALLOWED: &'static str = "A-Z, a-z, 0-9 and !#$%&'*+-.^_`|~";
    fn contains(c: char) -> bool {
        c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
    }
}

/// Headers that control HTTP framing, routing or connection handling, or
/// carry other credentials: setting them could smuggle or retarget requests
/// or mix credentials.
struct RequestHeaders;
impl Strings for RequestHeaders {
    const LIST: &'static [&'static str] = &[
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "upgrade",
        "te",
        "trailer",
        "expect",
        "cookie",
    ];
    const REASON: &'static str = "it controls the request itself and can't carry a credential";
}

/// `Proxy-Authorization`, `Proxy-Connection` and the like.
struct ProxyHeaders;
impl Strings for ProxyHeaders {
    const LIST: &'static [&'static str] = &["proxy-"];
    const REASON: &'static str = RequestHeaders::REASON;
}

#[cfg(test)]
pub(crate) fn header_name(name: &str) -> HeaderName {
    HeaderName(Validated::new(name.to_owned()).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(name: &str) -> String {
        Validated::<String, HeaderNameRule>::new(name.to_owned())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn accepts_tokens() {
        for name in ["X-API-Key", "Authorization", "x!#$%&'*+.^_`|~"] {
            assert_eq!(header_name(name).as_str(), name);
        }
    }

    #[test]
    fn refuses_what_is_not_a_token() {
        assert_eq!(
            error("X Bad"),
            "header_name \"X Bad\" contains ' ' (allowed: A-Z, a-z, 0-9 and !#$%&'*+-.^_`|~)"
        );
        for name in ["X:Y", "Ünicode"] {
            assert!(error(name).contains("contains"), "{name}");
        }
        assert_eq!(error(""), "header_name \"\" must not be empty");
        assert!(error(&"x".repeat(129)).ends_with("is longer than 128 characters"));
    }

    #[test]
    fn refuses_headers_that_control_the_request() {
        for name in [
            "Host",
            "content-length",
            "Transfer-Encoding",
            "Cookie",
            "Proxy-Authorization",
            "proxy-foo",
        ] {
            assert_eq!(
                error(name),
                format!(
                    "header_name {name:?} is not allowed: it controls the request itself and can't carry a credential"
                )
            );
        }
    }

    #[test]
    fn compares_case_insensitively_but_displays_as_written() {
        assert_eq!(header_name("X-API-Key"), header_name("x-api-key"));
        assert_ne!(header_name("X-API-Key"), header_name("X-Other"));
        assert_eq!(header_name("X-API-Key").to_string(), "X-API-Key");
    }
}
