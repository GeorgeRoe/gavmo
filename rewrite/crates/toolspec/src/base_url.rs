//! [`BaseUrl`]: a tool's destination, and the host-lock that keeps requests
//! made with its credential on that destination.

use std::fmt;

use serde::{Deserialize, Deserializer};
use url::Url;

/// A tool's validated destination: an http(s) URL with a host and no
/// userinfo, query or fragment, none of which can be part of a destination
/// a credential is bound to.
///
/// Displays as written in `gavmo.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    /// As written, for display.
    raw: String,
    /// Parsed, with the path ending in `/`, so `join` keeps the last segment
    /// and the prefix check can't take `/a/bc` as being under `/a/b`.
    url: Url,
    /// See [`Self::canonical`].
    canonical: String,
}

/// Why a `base_url` was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BaseUrlError {
    #[error("base_url is not a valid URL: {0}")]
    Invalid(url::ParseError),
    #[error("base_url must be an http(s) URL with a host")]
    NotHttpWithHost,
    #[error("base_url must not contain user:password@")]
    Userinfo,
    #[error("base_url must not have a query or fragment")]
    QueryOrFragment,
}

/// Why a request couldn't be host-locked. Every variant means "don't send".
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HostLockError {
    #[error("requested path produced an invalid URL: {0}")]
    InvalidResult(url::ParseError),
    #[error(
        "blocked: request would go to {requested} but this integration is locked to {expected}"
    )]
    HostMismatch { expected: String, requested: String },
    #[error("blocked: path {requested:?} escapes this integration's base path {base_path:?}")]
    PathEscape {
        base_path: String,
        requested: String,
    },
    #[error("blocked: requested path contains an encoded slash or backslash: {0:?}")]
    EncodedSeparator(String),
    #[error("blocked: request URL must not carry userinfo (user:password@)")]
    Userinfo,
}

impl BaseUrl {
    pub fn parse(raw: &str) -> Result<Self, BaseUrlError> {
        let mut url = Url::parse(raw).map_err(BaseUrlError::Invalid)?;
        let host = match (url.scheme(), url.host_str()) {
            ("http" | "https", Some(host)) => host.to_ascii_lowercase(),
            _ => return Err(BaseUrlError::NotHttpWithHost),
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(BaseUrlError::Userinfo);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(BaseUrlError::QueryOrFragment);
        }
        if !url.path().ends_with('/') {
            let with_slash = format!("{}/", url.path());
            url.set_path(&with_slash);
        }
        let port = url
            .port_or_known_default()
            .map_or_else(String::new, |p| format!(":{p}"));
        let canonical = format!("{}://{host}{port}{}", url.scheme(), url.path());
        Ok(Self {
            raw: raw.to_owned(),
            url,
            canonical,
        })
    }

    /// The URL as written in `gavmo.yaml`.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The destination a stored credential is bound to: scheme, host,
    /// effective port and path prefix, normalized the same way as
    /// [`Self::lock`] so the two agree on what "the same destination" is.
    /// Nothing cosmetic is included, so rewriting the URL in an equivalent
    /// way doesn't make the user re-approve the credential.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// Resolves an agent-supplied `path` under this base and returns the URL
    /// only if it keeps the base's scheme, host and port *and* stays under
    /// its path prefix, so the credential never leaves the destination it
    /// was approved for.
    ///
    /// `path` is relative to the base's full path, so a prefix such as
    /// `https://api.atlassian.com/ex/jira/<cloudId>` is kept. A query string
    /// is allowed. `//host` and absolute URLs are deliberately left as they
    /// are, so the origin check rejects them.
    pub fn lock(&self, path: &str) -> Result<Url, HostLockError> {
        let base = &self.url;

        // A single leading `/` means "under the base", not "from the host
        // root". `//host` is left intact so the origin check rejects it.
        let relative = match path.strip_prefix('/') {
            Some(rest) if !rest.starts_with('/') => rest,
            _ => path,
        };
        let resolved = base.join(relative).map_err(HostLockError::InvalidResult)?;

        // Effective port, so an implicit 443 equals an explicit `:443`.
        let same_origin = resolved.scheme() == base.scheme()
            && resolved.host_str() == base.host_str()
            && resolved.port_or_known_default() == base.port_or_known_default();
        if !same_origin {
            return Err(HostLockError::HostMismatch {
                expected: origin(base),
                requested: origin(&resolved),
            });
        }

        // Userinfo in the request URL would override (or add) credentials.
        if !resolved.username().is_empty() || resolved.password().is_some() {
            return Err(HostLockError::Userinfo);
        }

        // The path prefix is part of the boundary too (e.g. one tenant's
        // `/ex/jira/<cloudId>/` on a shared gateway host). `join` has already
        // resolved `..`, `%2e%2e` and `\`, so a prefix check is sound.
        if !resolved.path().starts_with(base.path()) {
            return Err(HostLockError::PathEscape {
                base_path: base.path().to_owned(),
                requested: resolved.path().to_owned(),
            });
        }

        // `%2F` and `%5C` survive normalization, but many servers decode them
        // before routing, which would turn `..%2F` back into a traversal on
        // the far side.
        let lower = resolved.path().to_ascii_lowercase();
        if lower.contains("%2f") || lower.contains("%5c") {
            return Err(HostLockError::EncodedSeparator(resolved.path().to_owned()));
        }

        Ok(resolved)
    }
}

impl fmt::Display for BaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for BaseUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// `scheme://host:port`, or as much of it as the URL has (a `file:` URL has
/// neither host nor port), for refusal messages.
fn origin(url: &Url) -> String {
    match (url.host_str(), url.port_or_known_default()) {
        (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme()),
        (Some(host), None) => format!("{}://{host}", url.scheme()),
        (None, _) => url.scheme().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATEWAY: &str = "https://api.atlassian.com/ex/jira/CLOUDID";

    fn base(url: &str) -> BaseUrl {
        BaseUrl::parse(url).unwrap()
    }

    fn lock(base_url: &str, path: &str) -> Result<String, HostLockError> {
        base(base_url).lock(path).map(String::from)
    }

    // --- parse ---

    #[test]
    fn parse_rejects_what_cannot_be_a_destination() {
        assert!(matches!(
            BaseUrl::parse("not a url"),
            Err(BaseUrlError::Invalid(_))
        ));
        for (url, want) in [
            ("ftp://h.example.com", BaseUrlError::NotHttpWithHost),
            ("https://u:p@h.example.com", BaseUrlError::Userinfo),
            ("https://u@h.example.com", BaseUrlError::Userinfo),
            ("https://h.example.com/b?x=1", BaseUrlError::QueryOrFragment),
            (
                "https://h.example.com/b#frag",
                BaseUrlError::QueryOrFragment,
            ),
        ] {
            assert_eq!(BaseUrl::parse(url), Err(want), "{url}");
        }
    }

    #[test]
    fn displays_as_written() {
        let url = base("https://H.example.com/base");
        assert_eq!(url.to_string(), "https://H.example.com/base");
        assert_eq!(url.as_str(), "https://H.example.com/base");
    }

    // --- canonical ---

    #[test]
    fn canonical_includes_the_port_and_path_prefix() {
        assert_eq!(
            base(GATEWAY).canonical(),
            "https://api.atlassian.com:443/ex/jira/CLOUDID/"
        );
        assert_eq!(
            base("http://h.example.com:8080/x").canonical(),
            "http://h.example.com:8080/x/"
        );
    }

    #[test]
    fn canonical_ignores_cosmetic_differences() {
        let a = base("https://Dafni.Atlassian.net");
        for other in [
            "https://dafni.atlassian.net/",
            "https://dafni.atlassian.net:443",
        ] {
            assert_eq!(a.canonical(), base(other).canonical(), "{other}");
        }
    }

    #[test]
    fn canonical_distinguishes_scheme_host_port_and_path() {
        let a = base("https://h.example.com/ex/jira/GOOD");
        for other in [
            "http://h.example.com/ex/jira/GOOD",
            "https://evil.example.com/ex/jira/GOOD",
            "https://h.example.com:8443/ex/jira/GOOD",
            // Origin-only binding would miss this one.
            "https://h.example.com/ex/jira/EVIL",
        ] {
            assert_ne!(a.canonical(), base(other).canonical(), "{other}");
        }
    }

    // --- lock: allowed ---

    #[test]
    fn lock_resolves_paths_under_the_base() {
        for (base_url, path, want) in [
            (
                "https://dafni.atlassian.net",
                "/rest/api/3/issue/ABC-1",
                "https://dafni.atlassian.net/rest/api/3/issue/ABC-1",
            ),
            // A leading `/` stays under the base path, as does a relative path.
            (
                GATEWAY,
                "/rest/api/3/myself",
                "https://api.atlassian.com/ex/jira/CLOUDID/rest/api/3/myself",
            ),
            (
                GATEWAY,
                "rest/api/3/myself",
                "https://api.atlassian.com/ex/jira/CLOUDID/rest/api/3/myself",
            ),
            (
                "https://api.atlassian.com/ex/jira/CLOUDID/",
                "/rest/x",
                "https://api.atlassian.com/ex/jira/CLOUDID/rest/x",
            ),
            // Dot segments are fine while they stay inside the prefix.
            (
                GATEWAY,
                "rest/x/../myself",
                "https://api.atlassian.com/ex/jira/CLOUDID/rest/myself",
            ),
            // An explicit default port is the same origin.
            (
                "https://h.example.com",
                "https://h.example.com:443/x",
                "https://h.example.com/x",
            ),
            // With a root base, `..` can't leave the prefix.
            (
                "https://h.example.com",
                "/../../x",
                "https://h.example.com/x",
            ),
        ] {
            assert_eq!(
                lock(base_url, path).as_deref(),
                Ok(want),
                "{base_url} + {path}"
            );
        }
    }

    #[test]
    fn lock_keeps_the_query_string() {
        let url = base(GATEWAY)
            .lock("/rest/api/3/search?jql=project%3DABC")
            .unwrap();
        assert_eq!(url.path(), "/ex/jira/CLOUDID/rest/api/3/search");
        assert_eq!(url.query(), Some("jql=project%3DABC"));
    }

    // --- lock: refused ---

    #[test]
    fn lock_refuses_another_origin() {
        for (base_url, path) in [
            (GATEWAY, "https://evil.com/steal"),
            (GATEWAY, "//evil.com/steal"),
            ("https://h.example.com", "http://h.example.com/x"),
            ("https://h.example.com", "https://h.example.com:8443/x"),
            // The real host here is evil.com; the prefix is userinfo.
            (
                "https://trusted.example.com",
                "https://trusted.example.com@evil.com/steal",
            ),
        ] {
            assert!(
                matches!(
                    lock(base_url, path),
                    Err(HostLockError::HostMismatch { .. })
                ),
                "{base_url} + {path}"
            );
        }
    }

    /// The refusal names the origin that was asked for, even for schemes
    /// without a default port or a host.
    #[test]
    fn lock_refusal_names_both_origins() {
        for (path, requested_origin) in [
            ("gopher://evil.com/x", "gopher://evil.com"),
            ("custom://evil.com/x", "custom://evil.com"),
            ("file:///etc/passwd", "file"),
        ] {
            assert_eq!(
                lock(GATEWAY, path),
                Err(HostLockError::HostMismatch {
                    expected: "https://api.atlassian.com:443".to_owned(),
                    requested: requested_origin.to_owned(),
                }),
                "{path}"
            );
        }
    }

    #[test]
    fn lock_refuses_a_path_that_is_not_a_url() {
        assert!(matches!(
            lock(GATEWAY, "http://[::1"),
            Err(HostLockError::InvalidResult(_))
        ));
    }

    #[test]
    fn lock_refuses_same_origin_userinfo() {
        assert_eq!(
            lock("https://h.example.com", "https://u:p@h.example.com/x"),
            Err(HostLockError::Userinfo)
        );
    }

    /// Every spelling of "go up" is caught once normalized.
    #[test]
    fn lock_refuses_escaping_the_path_prefix() {
        for path in [
            "../OTHER/rest",
            "/../OTHER/rest",
            "../../ex/jira/OTHER/rest",
            "rest/../../OTHER",
            "%2e%2e/OTHER",
            ".%2E/OTHER",
            "..\\OTHER",
            "/\\..\\OTHER",
            "https://api.atlassian.com/ex/jira/OTHER/rest",
            "https://api.atlassian.com/",
            "..",
        ] {
            assert!(
                matches!(lock(GATEWAY, path), Err(HostLockError::PathEscape { .. })),
                "{path:?}: {:?}",
                lock(GATEWAY, path)
            );
        }
    }

    /// `/a/bc` is not under `/a/b`.
    #[test]
    fn lock_prefix_check_respects_segment_boundaries() {
        assert!(matches!(
            lock(
                "https://api.atlassian.com/ex/jira/CLOUD",
                "https://api.atlassian.com/ex/jira/CLOUDX/rest"
            ),
            Err(HostLockError::PathEscape { .. })
        ));
    }

    #[test]
    fn lock_refuses_encoded_separators() {
        for path in ["..%2F..%2Fother", "a%2fb", "a%5Cb"] {
            assert!(
                matches!(lock(GATEWAY, path), Err(HostLockError::EncodedSeparator(_))),
                "{path:?}"
            );
        }
    }
}
