//! Session auth for the control-panel listener.
//!
//! Guest VMs reach host loopback via QEMU's `10.0.2.2`, so binding to
//! `127.0.0.1` does not stop an in-VM agent from calling `#[server]`
//! functions. Every non-public route therefore requires a session cookie
//! matching a host-only token that no guest share can read.
//!
//! The daemon prints a `/login?token=...` URL at startup; visiting it once
//! sets a long-lived cookie. The token is persisted across restarts.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::Deserialize;

/// Session cookie name checked by every guarded route.
pub const SESSION_COOKIE_NAME: &str = "gavmo_session";

/// The one guarded-app route reachable without a session.
pub const LOGIN_PATH: &str = "/login";

/// One year. Revocation is done by deleting `ui_token` and restarting, not
/// by a short cookie lifetime.
const SESSION_COOKIE_MAX_AGE_SECS: u64 = 365 * 24 * 60 * 60;

// --- Token ---

/// `<data_dir>/gavmo/ui_token`. Must stay outside every 9p share a VM
/// mounts, since it authorizes all `#[server]` calls.
fn token_path() -> io::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?
        .join("gavmo");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("ui_token"))
}

/// Loads the persisted session token, minting one on first run. Persisting
/// keeps existing browser sessions valid across daemon restarts.
pub fn load_or_create_token() -> io::Result<String> {
    load_or_create_token_at(&token_path()?)
}

/// [`load_or_create_token`] with an explicit path, for tests.
fn load_or_create_token_at(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(existing) if !existing.trim().is_empty() => Ok(existing.trim().to_string()),
        _ => {
            // Two UUIDv4s (~244 random bits) avoids a dedicated CSPRNG dep.
            let token = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
            std::fs::write(path, &token)?;
            // Owner-only, against other local users on a shared host.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(token)
        }
    }
}

// --- Helpers ---

/// Every value of cookie `name` in the request, across all `Cookie` headers
/// (some clients split them). Non-UTF-8 headers are skipped.
///
/// All values, not just the first: cookies are not port-isolated, so another
/// local page can plant its own `gavmo_session` that sorts first ("cookie
/// tossing"). Accepting any matching value keeps that from locking the user
/// out, and a planted value can never match the token anyway.
fn cookie_values<'a>(
    headers: &'a axum::http::HeaderMap,
    name: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .get_all(header::COOKIE)
        .into_iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(move |pair| {
            let (k, v) = pair.split_once('=')?;
            (k.trim() == name).then(|| v.trim())
        })
}

/// Constant-time comparison so token checks leak no timing information.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Paths reachable without a session: the login page and inert static
/// assets (`/pkg/` bundle, favicon). This is the single public allowlist.
fn is_public_path(path: &str) -> bool {
    path == LOGIN_PATH || path.starts_with("/pkg/") || path == "/favicon.ico"
}

// --- Login route ---

#[derive(Deserialize)]
struct LoginQuery {
    token: Option<String>,
}

/// Shown for a missing or wrong token. Never echoes the token and does not
/// distinguish the two cases.
const LOGIN_FALLBACK_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>GAVMO</title></head>
<body>
<h1>GAVMO control panel</h1>
<p>This link is missing or has an invalid token.</p>
<p>Run <code>gavmo</code> in your terminal and open the URL it prints — it looks like:</p>
<pre>http://localhost:3000/login?token=&lt;token&gt;</pre>
</body>
</html>"#;

/// `GET /login?token=...`: a correct token sets the session cookie and
/// redirects to `/` so the token leaves the address bar.
async fn login_handler(
    State(expected): State<Arc<String>>,
    Query(query): Query<LoginQuery>,
) -> Response {
    match query.token {
        Some(token) if constant_time_eq(&token, &expected) => {
            let cookie = format!(
                "{SESSION_COOKIE_NAME}={expected}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_COOKIE_MAX_AGE_SECS}"
            );
            let mut response = Redirect::to("/").into_response();
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                response.headers_mut().insert(header::SET_COOKIE, value);
            }
            response
        }
        _ => Html(LOGIN_FALLBACK_HTML).into_response(),
    }
}

/// The `/login` route as a stateful sub-router to merge into the app.
pub fn login_router(token: Arc<String>) -> Router {
    Router::new()
        .route(LOGIN_PATH, get(login_handler))
        .with_state(token)
}

// --- Cross-site request check ---

/// Whether a state-changing `/api/*` request demonstrably comes from the
/// control panel's own pages.
///
/// The session cookie alone is not enough: `SameSite` is about *sites*, and
/// every port on `localhost` / `127.0.0.1` is the same site. Any other local
/// page - a dev server, or a page a guest VM serves on its forwarded port -
/// could otherwise POST a form to `/api/*` with the cookie attached.
///
/// Browsers send `Sec-Fetch-Site` on every request (Chrome 76+, Firefox 90+,
/// Safari 16.4+); it must be `same-origin`. Where it is missing, `Origin`
/// must name exactly this host. A request with neither is refused, so the
/// check fails closed.
fn is_same_origin_request(headers: &axum::http::HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site.as_bytes() == b"same-origin";
    }
    let (Some(origin), Some(host)) = (headers.get(header::ORIGIN), headers.get(header::HOST))
    else {
        return false;
    };
    let (Ok(origin), Ok(host)) = (origin.to_str(), host.to_str()) else {
        return false;
    };
    // The control panel is served over plain HTTP on loopback.
    origin
        .strip_prefix("http://")
        .is_some_and(|origin_host| origin_host.eq_ignore_ascii_case(host))
}

// --- Auth gate ---

/// Middleware for the whole app router: non-public requests need a valid
/// session cookie. `/api/*` gets a bare 401 (it is called via `fetch`);
/// page routes redirect to [`LOGIN_PATH`]. `/api/*` additionally accepts
/// only same-origin `POST`s (see [`is_same_origin_request`]).
pub async fn require_session(
    State(expected): State<Arc<String>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if is_public_path(path) {
        return next.run(req).await;
    }
    let is_api = path.starts_with("/api/");

    let authenticated = cookie_values(req.headers(), SESSION_COOKIE_NAME)
        .any(|value| constant_time_eq(value, &expected));
    if !authenticated {
        return if is_api {
            StatusCode::UNAUTHORIZED.into_response()
        } else {
            Redirect::to(LOGIN_PATH).into_response()
        };
    }

    if is_api {
        // Server functions are all POST; refusing GET keeps navigations and
        // `<img src>` from ever reaching them.
        if req.method() != axum::http::Method::POST {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        if !is_same_origin_request(req.headers()) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    // --- cookie_values ---

    fn session_values(cookie_headers: &[&'static str]) -> Vec<String> {
        let mut headers = axum::http::HeaderMap::new();
        for value in cookie_headers {
            headers.append(header::COOKIE, HeaderValue::from_static(value));
        }
        cookie_values(&headers, SESSION_COOKIE_NAME)
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn cookie_values_extracts_named_cookie_among_several() {
        assert_eq!(
            session_values(&["foo=bar; gavmo_session=abc123; baz=qux"]),
            ["abc123"]
        );
    }

    #[test]
    fn cookie_values_handles_missing_header_and_absent_cookie() {
        assert!(session_values(&[]).is_empty());
        assert!(session_values(&["foo=bar"]).is_empty());
    }

    #[test]
    fn cookie_values_handles_whitespace_around_pairs() {
        assert_eq!(
            session_values(&[" foo=bar ;  gavmo_session=abc123 "]),
            ["abc123"]
        );
    }

    #[test]
    fn cookie_values_returns_every_duplicate_across_headers() {
        assert_eq!(
            session_values(&["gavmo_session=planted", "x=1; gavmo_session=real"]),
            ["planted", "real"]
        );
    }

    // --- constant_time_eq ---

    #[test]
    fn constant_time_eq_matches_equal_strings() {
        assert!(constant_time_eq("abc123", "abc123"));
    }

    #[test]
    fn constant_time_eq_rejects_different_strings_of_equal_length() {
        assert!(!constant_time_eq("abc123", "abc124"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn constant_time_eq_treats_two_empty_strings_as_equal() {
        assert!(constant_time_eq("", ""));
    }

    // --- is_public_path ---

    #[test]
    fn is_public_path_allows_login_and_static_assets() {
        assert!(is_public_path("/login"));
        assert!(is_public_path("/pkg/gavmo.css"));
        assert!(is_public_path("/pkg/gavmo.js"));
        assert!(is_public_path("/favicon.ico"));
    }

    #[test]
    fn is_public_path_guards_everything_else() {
        assert!(!is_public_path("/"));
        assert!(!is_public_path("/api/list_workspaces"));
        assert!(!is_public_path("/api/browse_dir"));
        assert!(!is_public_path("/pkg")); // no trailing slash: not a real route anyway
    }

    // --- load_or_create_token_at ---

    #[test]
    fn load_or_create_token_at_mints_and_persists_a_token_on_first_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui_token");

        let token = load_or_create_token_at(&path).unwrap();

        assert!(!token.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), token);
    }

    #[test]
    fn load_or_create_token_at_reuses_an_existing_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui_token");
        std::fs::write(&path, "existing-token").unwrap();

        let token = load_or_create_token_at(&path).unwrap();

        assert_eq!(token, "existing-token");
    }

    #[test]
    fn load_or_create_token_at_treats_a_blank_file_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui_token");
        std::fs::write(&path, "   \n").unwrap();

        let token = load_or_create_token_at(&path).unwrap();

        assert!(!token.trim().is_empty());
    }

    // --- require_session / login_router ---

    /// Minimal stand-in for the real app router: one API route, one page
    /// route, and the real `/login`.
    fn test_router(token: Arc<String>) -> Router {
        Router::new()
            .route("/api/test", axum::routing::post(|| async { "ok" }))
            .route("/", get(|| async { "home" }))
            .merge(login_router(Arc::clone(&token)))
            .layer(axum::middleware::from_fn_with_state(token, require_session))
    }

    #[tokio::test]
    async fn api_route_without_a_cookie_is_rejected() {
        let app = test_router(Arc::new("expected-token".to_string()));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_route_with_the_correct_cookie_passes_through() {
        let app = test_router(Arc::new("expected-token".to_string()));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header("sec-fetch-site", "same-origin")
                    .header(header::COOKIE, "gavmo_session=expected-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_route_with_the_wrong_cookie_is_still_rejected() {
        let app = test_router(Arc::new("expected-token".to_string()));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header("sec-fetch-site", "same-origin")
                    .header(header::COOKIE, "gavmo_session=wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn page_route_without_a_cookie_redirects_to_login() {
        let app = test_router(Arc::new("expected-token".to_string()));

        let res = app
            .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            res.headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            LOGIN_PATH
        );
    }

    #[tokio::test]
    async fn login_is_reachable_with_no_cookie_and_no_token() {
        let app = test_router(Arc::new("expected-token".to_string()));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Fallback page, not a redirect or 401.
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_with_the_correct_token_sets_a_cookie_and_redirects() {
        let app = test_router(Arc::new("expected-token".to_string()));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/login?token=expected-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let set_cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.starts_with("gavmo_session=expected-token"));
        assert!(set_cookie.contains("HttpOnly"));
    }

    #[tokio::test]
    async fn login_with_the_wrong_token_does_not_set_a_cookie() {
        let app = test_router(Arc::new("expected-token".to_string()));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/login?token=nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get(header::SET_COOKIE).is_none());
    }

    // --- Cross-site protection on /api ---

    /// Sends an authenticated request to `/api/test` with extra headers.
    async fn api_status(method: &str, headers: &[(&str, &str)]) -> StatusCode {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri("/api/test")
            .header(header::HOST, "localhost:3000")
            .header(header::COOKIE, "gavmo_session=expected-token");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        test_router(Arc::new("expected-token".to_owned()))
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn api_accepts_same_origin_fetches() {
        assert_eq!(
            api_status("POST", &[("sec-fetch-site", "same-origin")]).await,
            StatusCode::OK
        );
        // Older browsers: no Sec-Fetch-Site, but a matching Origin.
        assert_eq!(
            api_status("POST", &[("origin", "http://localhost:3000")]).await,
            StatusCode::OK
        );
    }

    /// A page on another port of the same host is same-*site*, so the cookie
    /// is attached; the origin check is what stops it.
    #[tokio::test]
    async fn api_rejects_cross_origin_posts_even_with_a_valid_cookie() {
        for headers in [
            &[("sec-fetch-site", "same-site")][..],
            &[("sec-fetch-site", "cross-site")],
            &[("sec-fetch-site", "none")],
            // Sec-Fetch-Site takes precedence over a matching Origin.
            &[
                ("sec-fetch-site", "same-site"),
                ("origin", "http://localhost:3000"),
            ],
            &[("origin", "http://localhost:8080")],
            &[("origin", "http://127.0.0.1:3000")],
            &[("origin", "https://localhost:3000")],
            &[("origin", "null")],
            // Neither header: fail closed.
            &[],
        ] {
            assert_eq!(
                api_status("POST", headers).await,
                StatusCode::FORBIDDEN,
                "{headers:?}"
            );
        }
    }

    #[tokio::test]
    async fn api_rejects_non_post_methods() {
        assert_eq!(
            api_status("GET", &[("sec-fetch-site", "same-origin")]).await,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    /// A planted cookie sorting first must not lock the real session out.
    #[tokio::test]
    async fn a_planted_session_cookie_does_not_shadow_the_real_one() {
        let res = test_router(Arc::new("expected-token".to_owned()))
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header("sec-fetch-site", "same-origin")
                    .header(
                        header::COOKIE,
                        "gavmo_session=planted; gavmo_session=expected-token",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
