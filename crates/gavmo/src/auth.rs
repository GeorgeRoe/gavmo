//! Session auth for the daemon's control-panel listener (`127.0.0.1:3000`).
//!
//! **Why this exists:** QEMU's user-mode networking means a workspace's
//! guest VM can reach this daemon's loopback listener via `10.0.2.2` exactly
//! as easily as the host's own browser can (see `crates/mcp/plan.md` §3/§6,
//! which flagged this as a known, then-unfixed exposure). Binding to
//! `127.0.0.1` is therefore **not** a boundary against the guest — every
//! `#[server]` function in `app.rs` (host filesystem browsing, workspace
//! reset/remove, stored-credential probing, etc.) was reachable from inside
//! any one workspace, with no auth at all. A malicious or prompt-injected
//! agent — which runs with wide-open root inside its VM by design (see the
//! top-level README and `nix/base/configuration.nix`'s permission policy) —
//! could use this to control every other workspace and read arbitrary host
//! files.
//!
//! **The fix:** gate every route except the login page and static assets
//! behind a session cookie that must match a host-only secret
//! ([`load_or_create_token`]) — the same bearer-token posture `crates/mcp`
//! already uses for its own gateway, just cookie-based here since the
//! consumer is a browser rather than an MCP client. The secret is never
//! written anywhere a guest's 9p shares can reach and never echoed into any
//! served page — the login flow only ever *compares* against it.
//!
//! **Delivery (the "completely automatic" requirement):** the daemon prints
//! `http://localhost:3000/login?token=<token>` to its own stdout at startup
//! (see `main.rs`) — the same channel a user already watches to run `gavmo`
//! in the first place. Visiting that URL once sets a long-lived cookie, so a
//! given browser profile never needs the token again, including across
//! daemon restarts (the token itself is persisted, not regenerated per run).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

/// The cookie every guarded route checks for. `HttpOnly` (set in
/// [`login_handler`]) so it's inaccessible to any JS running on the page —
/// not that this page runs attacker JS, but there's no reason to make it
/// readable either.
pub const SESSION_COOKIE_NAME: &str = "gavmo_session";

/// The one route reachable with no session at all — otherwise nobody could
/// ever obtain one. See [`is_public_path`].
pub const LOGIN_PATH: &str = "/login";

/// One year, in seconds — the session cookie's `Max-Age`. Long enough that a
/// user effectively never has to re-authenticate in the same browser
/// profile. The real revocation lever is the daemon-side token file itself
/// (delete `ui_token` and restart to invalidate every outstanding cookie at
/// once), not a short cookie lifetime.
const SESSION_COOKIE_MAX_AGE_SECS: u64 = 365 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// Token: minted once, persisted host-only, never guest-visible.
// ---------------------------------------------------------------------------

/// `~/.local/share/gavmo/ui_token` (or platform equivalent). Deliberately
/// **outside** every 9p share a VM ever mounts (compare `VmConfig`'s share
/// list in `crates/vm/src/config.rs`) — this file is the one thing that
/// authorizes calling any `#[server]` function, so a guest must never be
/// able to read it the way it *can* read, say, the MCP token share.
fn token_path() -> io::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?
        .join("gavmo");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("ui_token"))
}

/// Loads the persisted session token, minting and persisting a fresh one on
/// first run. Persisting (rather than regenerating every boot) is what lets
/// a browser that already holds the session cookie from a previous run stay
/// authenticated across a daemon restart, rather than needing the printed
/// URL again every time.
pub fn load_or_create_token() -> io::Result<String> {
    load_or_create_token_at(&token_path()?)
}

/// The testable half of [`load_or_create_token`] — takes an explicit path
/// rather than resolving `token_path()` itself, so this can be exercised
/// against a scratch directory without touching the real app-data dir (same
/// split as `registry.rs`'s `resolve_host_port_in` taking an explicit
/// `&mut Registry` rather than loading one itself).
fn load_or_create_token_at(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(existing) if !existing.trim().is_empty() => Ok(existing.trim().to_string()),
        _ => {
            // Two concatenated UUIDv4s (~244 bits) rather than one: `uuid`
            // is already a dependency (see the MCP token minting in
            // `registry.rs`), so this avoids pulling in a dedicated CSPRNG
            // crate purely for a few extra bits that aren't the limiting
            // factor here — the real security boundary is "the guest never
            // sees this file at all", not token length.
            let token = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
            std::fs::write(path, &token)?;
            // Best-effort, same posture as the MCP token's `set_permissions`
            // in `registry.rs`: owner-only permissions are cheap insurance
            // against another local user on a shared host reading it.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(token)
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (cookie parsing, constant-time compare, the public-path
// allowlist) — exhaustively unit-tested without any real HTTP transport.
// ---------------------------------------------------------------------------

/// Extracts the value of cookie `name` from a request's `Cookie` header(s).
/// Handles the normal single-header `; `-separated multi-pair format, and
/// also checks every `Cookie` header if more than one is present (some
/// proxies/clients split them) — `None` if the named cookie isn't present in
/// any of them, or a header value isn't valid UTF-8.
fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    for header in headers.get_all(header::COOKIE) {
        let Ok(value) = header.to_str() else { continue };
        for pair in value.split(';') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Constant-time string comparison — belt-and-suspenders against a timing
/// side-channel on the session-cookie check below. The threat model here
/// (a single-user localhost daemon) makes exploiting a timing difference
/// impractical in the first place, but the check costs nothing and mirrors
/// the posture security-sensitive comparisons conventionally take.
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

/// Paths reachable with **no** session cookie. Deliberately narrow:
/// [`LOGIN_PATH`] itself (otherwise nobody could ever authenticate), the
/// static WASM/CSS bundle under `/pkg/` (`site-pkg-dir` in `Cargo.toml`),
/// and `/favicon.ico`. None of these carry any capability or secret — just
/// inert bytes — so gating them would only add friction (the login
/// fallback page can't even render its own styling without `/pkg/`) without
/// protecting anything.
fn is_public_path(path: &str) -> bool {
    path == LOGIN_PATH || path.starts_with("/pkg/") || path == "/favicon.ico"
}

// ---------------------------------------------------------------------------
// The /login route.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginQuery {
    token: Option<String>,
}

/// Shown when `/login` is visited with no (or the wrong) token — i.e. *not*
/// via the URL the daemon itself printed at startup. Deliberately never
/// echoes the real token back into the page, and doesn't distinguish
/// "missing" from "wrong" (nothing useful to a legitimate user either way).
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

/// `GET /login?token=...` — the only way to obtain the session cookie every
/// other guarded route requires (see [`require_session`]). A correct token
/// sets a long-lived `HttpOnly` cookie and redirects to `/`, so the token
/// itself never lingers in the visible address bar past this one request.
/// Anything else (no token, wrong token) gets [`LOGIN_FALLBACK_HTML`].
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

/// The `/login` route as a standalone, already-stated sub-router — merge
/// this into the main app (see `main.rs`). Kept separate from
/// [`require_session`]'s path-allowlist approach so this handler is the
/// *only* place the real token is ever compared against user input.
pub fn login_router(token: Arc<String>) -> Router {
    Router::new()
        .route(LOGIN_PATH, get(login_handler))
        .with_state(token)
}

// ---------------------------------------------------------------------------
// The auth gate itself.
// ---------------------------------------------------------------------------

/// The auth gate. Apply as a layer over the *entire* app router (see
/// `main.rs`) — every request not covered by [`is_public_path`] must carry a
/// `gavmo_session` cookie matching the host-only `ui_token`
/// ([`load_or_create_token`]).
///
/// `/api/*` (the actual `#[server]` function surface — filesystem browsing,
/// workspace reset/remove, stored-credential probing, etc.) gets a bare
/// `401` on failure: there's no useful "redirect to login" UX for a
/// `#[server]` function's own `fetch` call, and a bare 401 reveals nothing
/// beyond "not authenticated". Page routes instead redirect to
/// [`LOGIN_PATH`], so a human landing on the bare control-panel URL with no
/// cookie gets pointed at the fallback page telling them to run `gavmo`
/// again, rather than a cryptic 401.
pub async fn require_session(
    State(expected): State<Arc<String>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    if is_public_path(&path) {
        return next.run(req).await;
    }

    let authenticated = cookie_value(req.headers(), SESSION_COOKIE_NAME)
        .map(|value| constant_time_eq(&value, &expected))
        .unwrap_or(false);
    if authenticated {
        return next.run(req).await;
    }

    if path.starts_with("/api/") {
        StatusCode::UNAUTHORIZED.into_response()
    } else {
        Redirect::to(LOGIN_PATH).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    /// A unique scratch dir under the OS temp dir, named with the test's own
    /// identity (PID + a monotonic counter) so concurrent `cargo test`
    /// threads can't collide. Mirrors `registry.rs`'s own `unique_temp_dir`
    /// test helper (not shared across modules — small enough not to be
    /// worth a shared test-utils module).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("gavmo-auth-test-{tag}-{}-{n}", std::process::id()))
    }

    // --- cookie_value ------------------------------------------------------

    #[test]
    fn cookie_value_extracts_named_cookie_among_several() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("foo=bar; gavmo_session=abc123; baz=qux"),
        );
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE_NAME),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn cookie_value_handles_missing_header() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(cookie_value(&headers, SESSION_COOKIE_NAME), None);
    }

    #[test]
    fn cookie_value_handles_whitespace_around_pairs() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static(" foo=bar ;  gavmo_session=abc123 "),
        );
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE_NAME),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn cookie_value_returns_none_when_named_cookie_absent() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("foo=bar"));
        assert_eq!(cookie_value(&headers, SESSION_COOKIE_NAME), None);
    }

    // --- constant_time_eq ---------------------------------------------------

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

    // --- is_public_path ------------------------------------------------------

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

    // --- load_or_create_token_at ---------------------------------------------

    #[test]
    fn load_or_create_token_at_mints_and_persists_a_token_on_first_call() {
        let dir = unique_temp_dir("create");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui_token");

        let token = load_or_create_token_at(&path).unwrap();

        assert!(!token.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), token);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_or_create_token_at_reuses_an_existing_token() {
        let dir = unique_temp_dir("reuse");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui_token");
        std::fs::write(&path, "existing-token").unwrap();

        let token = load_or_create_token_at(&path).unwrap();

        assert_eq!(token, "existing-token");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_or_create_token_at_treats_a_blank_file_as_absent() {
        let dir = unique_temp_dir("blank");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui_token");
        std::fs::write(&path, "   \n").unwrap();

        let token = load_or_create_token_at(&path).unwrap();

        assert!(!token.trim().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- require_session / login_router, end to end over a real router -----

    /// Builds a minimal stand-in for `main.rs`'s real app router — just
    /// enough surface (one `/api/*`-shaped route, one page route, the real
    /// `/login` route) to exercise [`require_session`]'s actual routing
    /// behavior via `tower::ServiceExt::oneshot`, without needing the real
    /// Leptos routes or `AppState`.
    fn test_router(token: Arc<String>) -> Router {
        Router::new()
            .route("/api/test", get(|| async { "ok" }))
            .route("/", get(|| async { "home" }))
            .merge(login_router(token.clone()))
            .layer(axum::middleware::from_fn_with_state(token, require_session))
    }

    #[tokio::test]
    async fn api_route_without_a_cookie_is_rejected() {
        let app = test_router(Arc::new("expected-token".to_string()));

        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/test")
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
                    .uri("/api/test")
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
                    .uri("/api/test")
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

        // The fallback page, not a redirect/401 — `/login` itself must
        // never be gated, or nobody could ever authenticate.
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
}
