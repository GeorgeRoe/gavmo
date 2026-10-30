//! Session auth for the control-panel listener.
//!
//! Guest VMs reach host loopback via QEMU's `10.0.2.2`, so binding to
//! `127.0.0.1` does not stop an in-VM agent from calling `#[server]`
//! functions. Every non-public route therefore requires a valid session.
//!
//! The daemon prints a `/login?token=...` URL carrying the master UI token
//! (persisted, host-only). Visiting it creates a *session* (see
//! [`crate::sessions`]): the cookie holds a random session id, never the
//! master token, expires after 30 days, and can be revoked with "log out
//! everywhere".
//!
//! Sessions are only created for the host name `localhost`. Browsers scope
//! cookies by host name, not port, and each VM's opencode UI is served at
//! `127.0.0.1:<port>`: a cookie set for `127.0.0.1` would be sent to every
//! guest. A login at `127.0.0.1` is therefore redirected to `localhost`, and
//! any other host name (e.g. a DNS-rebinding domain) is refused.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::sessions::{SESSION_TTL, SessionStore};

/// Session cookie name checked by every guarded route.
pub const SESSION_COOKIE_NAME: &str = "gavmo_session";

/// The one guarded-app route reachable without a session.
pub const LOGIN_PATH: &str = "/login";

/// "Log out everywhere" (a form POST from the control panel).
pub const LOGOUT_PATH: &str = "/logout";

/// The only host name sessions are created for (see the module docs). Guest
/// UIs must never be served under it.
const SESSION_HOSTNAME: &str = "localhost";

// --- Auth state ---

/// Auth state shared by the login/logout routes and the session middleware.
pub struct Auth {
    token: String,
    sessions: SessionStore,
}

impl Auth {
    /// The real daemon's auth: token and sessions under
    /// `~/.local/share/gavmo`.
    ///
    /// The first start with session support (no `sessions.json` yet)
    /// rotates the master token: earlier versions put it in the cookie, so
    /// any browser that ever logged in held it.
    pub async fn load_default() -> io::Result<Arc<Self>> {
        let dir = data_dir()?;
        let token_path = dir.join("ui_token");
        let sessions_path = dir.join("sessions.json");
        if !tokio::fs::try_exists(&sessions_path).await? {
            match std::fs::remove_file(&token_path) {
                Ok(()) => tracing::info!(
                    "rotated the control-panel token (cookies no longer carry it); \
                     use the new login URL"
                ),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        let token = load_or_create_token_at(&token_path)?;
        Self::new(token, sessions_path).await
    }

    /// Auth with an explicit token and session file (also used by tests).
    pub(crate) async fn new(token: String, sessions_path: PathBuf) -> io::Result<Arc<Self>> {
        let sessions = SessionStore::load(sessions_path)?;
        // Written even when empty, so its presence marks the migration done.
        sessions.persist().await.map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("writing {}: {e}", sessions.path().display()),
            )
        })?;
        Ok(Arc::new(Self { token, sessions }))
    }

    /// The master UI token, for the printed login URL.
    pub fn token(&self) -> &str {
        &self.token
    }
}

// --- Token ---

/// `<data_dir>/gavmo`, holding `ui_token` and `sessions.json`. Must stay
/// outside every share a VM mounts, since they authorize all `#[server]`
/// calls.
fn data_dir() -> io::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?
        .join("gavmo");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Loads the persisted master token at `path`, minting one if absent.
///
/// Only a missing or blank file mints a new token. Any other read error
/// (permissions, I/O, a directory in the way) is returned: silently replacing
/// the token would mask the real problem.
fn load_or_create_token_at(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(existing) if !existing.trim().is_empty() => Ok(existing.trim().to_owned()),
        Ok(_) => {
            // Blank: replace in place (the file already exists).
            let token = new_token();
            write_token(path, &token, false)?;
            Ok(token)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let token = new_token();
            match write_token(path, &token, true) {
                Ok(()) => Ok(token),
                // Another process created it first; use theirs.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => load_or_create_token_at(path),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Two `UUIDv4`s (~244 random bits) avoids a dedicated CSPRNG dep.
fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
}

/// Writes the token owner-only (`0600` at creation, never briefly
/// world-readable). `create_new` refuses to follow or clobber anything
/// already at `path`.
fn write_token(path: &Path, token: &str, create_new: bool) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = std::fs::OpenOptions::new();
    options.write(true).mode(0o600);
    if create_new {
        options.create_new(true);
    } else {
        options.truncate(true);
    }
    let mut file = options.open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(token.as_bytes())?;
    file.sync_all()
}

// --- Helpers ---

/// Every value of cookie `name` in the request, across all `Cookie` headers
/// (some clients split them). Non-UTF-8 headers are skipped.
///
/// All values, not just the first: cookies are not port-isolated, so another
/// local page can plant its own `gavmo_session` that sorts first ("cookie
/// tossing"). Accepting any valid value keeps that from locking the user
/// out, and a planted value never names a valid session.
fn cookie_values<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
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

/// The request's `Host` header, if present and valid UTF-8.
fn request_host(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST)?.to_str().ok()
}

/// Splits `host[:port]` into its host name and the `:port` suffix (possibly
/// empty). Bracketed IPv6 literals stay whole.
fn split_host(host: &str) -> (&str, &str) {
    match host.rfind(':') {
        Some(i) if !host.get(i..).unwrap_or_default().contains(']') => host.split_at(i),
        _ => (host, ""),
    }
}

// --- Login / logout ---

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

const LOGGED_OUT_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>GAVMO</title></head>
<body>
<h1>GAVMO control panel</h1>
<p>Logged out of all sessions. Open the URL <code>gavmo</code> printed in your terminal to log in again.</p>
</body>
</html>"#;

/// `GET /login?token=...`: a correct token creates a session for this host
/// (see the module docs for which hosts qualify), sets its cookie and
/// redirects to `/` so the token leaves the address bar.
async fn login_handler(
    State(auth): State<Arc<Auth>>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let mut response = match query.token {
        Some(token) if constant_time_eq(&token, &auth.token) => {
            login_with_valid_token(&auth, &headers, &token).await
        }
        _ => Html(LOGIN_FALLBACK_HTML).into_response(),
    };
    // The URL carries the master token: keep it out of caches and out of the
    // Referer of anything the next page loads.
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// The token was right; decide what this host gets.
async fn login_with_valid_token(auth: &Auth, headers: &HeaderMap, token: &str) -> Response {
    let Some(host) = request_host(headers) else {
        return (StatusCode::BAD_REQUEST, "missing Host header").into_response();
    };
    let (hostname, port) = split_host(host);

    if hostname.eq_ignore_ascii_case(SESSION_HOSTNAME) {
        let id = match auth.sessions.create(host).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, "could not save the new session");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        let cookie = format!(
            "{SESSION_COOKIE_NAME}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
            SESSION_TTL.as_secs()
        );
        return match HeaderValue::from_str(&cookie) {
            Ok(value) => {
                let mut response = Redirect::to("/").into_response();
                response.headers_mut().insert(header::SET_COOKIE, value);
                response
            }
            // Redirecting without the cookie would loop back to /login.
            Err(e) => {
                tracing::error!(error = %e, "session id is not a valid cookie value");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }

    // Loopback IPs share a host name with the guest UIs: move to `localhost`
    // (same port) so the cookie is never sent to a guest.
    if hostname == "127.0.0.1" || hostname == "[::1]" {
        let target = format!("http://{SESSION_HOSTNAME}{port}{LOGIN_PATH}?token={token}");
        return Redirect::to(&target).into_response();
    }

    (
        StatusCode::BAD_REQUEST,
        Html(format!(
            "<p>Log in via <code>http://{SESSION_HOSTNAME}{port}{LOGIN_PATH}</code> \
             (the URL gavmo prints).</p>"
        )),
    )
        .into_response()
}

/// `POST /logout`: ends every session ("log out everywhere") and clears this
/// browser's cookie. Only reachable with a valid session, and as a
/// state-changing request it must be same-origin (see [`require_session`]).
async fn logout_handler(State(auth): State<Arc<Auth>>) -> Response {
    if let Err(e) = auth.sessions.revoke_all().await {
        tracing::error!(error = %e, "could not revoke sessions");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut response = Html(LOGGED_OUT_HTML).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("gavmo_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
    );
    response
}

/// The `/login` and `/logout` routes as a stateful sub-router to merge into
/// the app.
pub fn login_router(auth: Arc<Auth>) -> Router {
    Router::new()
        .route(LOGIN_PATH, get(login_handler))
        .route(LOGOUT_PATH, post(logout_handler))
        .with_state(auth)
}

// --- Cross-site request check ---

/// Whether a state-changing request demonstrably comes from the control
/// panel's own pages.
///
/// The session cookie alone is not enough: `SameSite` is about *sites*, and
/// every port on `localhost` / `127.0.0.1` is the same site. Any other local
/// page - a dev server, or a page a guest VM serves on its forwarded port -
/// could otherwise POST a form to the panel with the cookie attached.
///
/// Browsers send `Sec-Fetch-Site` on every request (Chrome 76+, Firefox 90+,
/// Safari 16.4+); it must be `same-origin`. Where it is missing, `Origin`
/// must name exactly this host. A request with neither is refused, so the
/// check fails closed.
fn is_same_origin_request(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site.as_bytes() == b"same-origin";
    }
    let (Some(origin), Some(host)) = (headers.get(header::ORIGIN), request_host(headers)) else {
        return false;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    // The control panel is served over plain HTTP on loopback.
    origin
        .strip_prefix("http://")
        .is_some_and(|origin_host| origin_host.eq_ignore_ascii_case(host))
}

/// A 401 whose body is a JSON-encoded [`crate::types::AppError::Unauthorized`],
/// the same encoding server functions use for errors, so the browser can
/// tell "logged out" from other failures and send the user to log in.
fn unauthorized_api_response() -> Response {
    let body = serde_json::to_string(&crate::types::AppError::Unauthorized)
        .unwrap_or_else(|_| "\"Unauthorized\"".to_owned());
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

// --- Auth gate ---

/// Middleware for the whole app router: non-public requests need a valid
/// session for this `Host`. `/api/*` gets a bare 401 (it is called via
/// `fetch`); page routes redirect to [`LOGIN_PATH`]. Every state-changing
/// request (anything but `GET`/`HEAD`) must be same-origin (see
/// [`is_same_origin_request`]), and `/api/*` accepts only `POST`.
pub async fn require_session(State(auth): State<Arc<Auth>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if is_public_path(path) {
        return next.run(req).await;
    }
    let is_api = path.starts_with("/api/");

    let host = request_host(req.headers()).unwrap_or_default();
    let authenticated = !host.is_empty()
        && cookie_values(req.headers(), SESSION_COOKIE_NAME)
            .any(|id| auth.sessions.is_valid(id, host));
    if !authenticated {
        return if is_api {
            unauthorized_api_response()
        } else {
            Redirect::to(LOGIN_PATH).into_response()
        };
    }

    // Server functions are all POST; refusing GET keeps navigations and
    // `<img src>` from ever reaching them.
    if is_api && req.method() != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let read_only = req.method() == Method::GET || req.method() == Method::HEAD;
    if !read_only && !is_same_origin_request(req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
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
        let mut headers = HeaderMap::new();
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

    /// A read error other than "missing" must not mint (and persist) a new
    /// token, which would silently log out every session.
    #[test]
    fn load_or_create_token_at_propagates_other_read_errors() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: read fails with EISDIR.
        let path = dir.path().join("ui_token");
        std::fs::create_dir(&path).unwrap();
        assert!(load_or_create_token_at(&path).is_err());
        assert!(path.is_dir(), "nothing may be written over it");
    }

    #[cfg(unix)]
    #[test]
    fn load_or_create_token_at_writes_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui_token");
        load_or_create_token_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // A blank, wider file is narrowed when it is refilled.
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        load_or_create_token_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // --- Router: login, sessions, logout, cross-site protection ---

    const TOKEN: &str = "expected-token";
    const PANEL: &str = "localhost:3000";

    /// The real `/login`/`/logout` and middleware around one API route and
    /// one page, over a temp session file.
    struct Panel {
        _dir: tempfile::TempDir,
        router: Router,
    }

    async fn panel() -> Panel {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(TOKEN.to_owned(), dir.path().join("sessions.json"))
            .await
            .unwrap();
        let router = Router::new()
            .route("/api/test", post(|| async { "ok" }))
            .route("/", get(|| async { "home" }))
            .merge(login_router(Arc::clone(&auth)))
            .layer(axum::middleware::from_fn_with_state(auth, require_session));
        Panel { _dir: dir, router }
    }

    impl Panel {
        async fn send(&self, req: HttpRequest<Body>) -> Response {
            self.router.clone().oneshot(req).await.unwrap()
        }

        async fn login(&self, host: &str, token: &str) -> Response {
            self.send(
                HttpRequest::builder()
                    .uri(format!("/login?token={token}"))
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
        }

        /// Logs in at [`PANEL`] and returns the `gavmo_session=<id>` pair.
        async fn session_cookie(&self) -> String {
            let res = self.login(PANEL, TOKEN).await;
            assert_eq!(res.status(), StatusCode::SEE_OTHER);
            set_cookie(&res).split(';').next().unwrap().to_owned()
        }

        /// An authenticated POST to `/api/test` with extra headers.
        async fn api(&self, method: &str, cookie: &str, extra: &[(&str, &str)]) -> StatusCode {
            let mut builder = HttpRequest::builder()
                .method(method)
                .uri("/api/test")
                .header(header::HOST, PANEL)
                .header(header::COOKIE, cookie);
            for (name, value) in extra {
                builder = builder.header(*name, *value);
            }
            self.send(builder.body(Body::empty()).unwrap())
                .await
                .status()
        }
    }

    fn set_cookie(res: &Response) -> &str {
        res.headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
    }

    const SAME_ORIGIN: &[(&str, &str)] = &[("sec-fetch-site", "same-origin")];

    #[tokio::test]
    async fn login_sets_a_session_cookie_that_is_not_the_token() {
        let panel = panel().await;
        let res = panel.login(PANEL, TOKEN).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/");
        let cookie = set_cookie(&res);
        assert!(cookie.starts_with("gavmo_session="), "{cookie}");
        assert!(
            !cookie.contains(TOKEN),
            "the master token must not be in the cookie"
        );
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=2592000"), "30 days: {cookie}");
        assert_eq!(
            res.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(
            res.headers().get(header::REFERRER_POLICY).unwrap(),
            "no-referrer"
        );
    }

    #[tokio::test]
    async fn the_session_cookie_authorizes_and_others_do_not() {
        let panel = panel().await;
        let cookie = panel.session_cookie().await;
        assert_eq!(
            panel.api("POST", &cookie, SAME_ORIGIN).await,
            StatusCode::OK
        );
        // The master token itself (what old cookies held) is not a session.
        let old = format!("gavmo_session={TOKEN}");
        assert_eq!(
            panel.api("POST", &old, SAME_ORIGIN).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            panel.api("POST", "gavmo_session=nope", SAME_ORIGIN).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            panel.api("POST", "", SAME_ORIGIN).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_session_is_rejected_under_another_host() {
        let panel = panel().await;
        let cookie = panel.session_cookie().await;
        for host in ["127.0.0.1:3000", "evil.example:3000", "localhost:4000"] {
            let res = panel
                .send(
                    HttpRequest::builder()
                        .method("POST")
                        .uri("/api/test")
                        .header(header::HOST, host)
                        .header(header::COOKIE, &cookie)
                        .header("sec-fetch-site", "same-origin")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{host}");
        }
    }

    /// Guests are served at 127.0.0.1: a session must never be created
    /// there, or its cookie would be sent to every guest port.
    #[tokio::test]
    async fn login_on_a_loopback_ip_moves_to_localhost_without_a_cookie() {
        let panel = panel().await;
        for (host, target) in [
            (
                "127.0.0.1:3000",
                "http://localhost:3000/login?token=expected-token",
            ),
            (
                "[::1]:3000",
                "http://localhost:3000/login?token=expected-token",
            ),
            ("127.0.0.1", "http://localhost/login?token=expected-token"),
        ] {
            let res = panel.login(host, TOKEN).await;
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{host}");
            assert_eq!(res.headers().get(header::LOCATION).unwrap(), target);
            assert!(res.headers().get(header::SET_COOKIE).is_none(), "{host}");
        }
    }

    #[tokio::test]
    async fn login_under_any_other_host_is_refused() {
        let panel = panel().await;
        let res = panel.login("evil.example:3000", TOKEN).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(res.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn login_with_a_wrong_or_missing_token_sets_nothing() {
        let panel = panel().await;
        for uri in ["/login?token=wrong", "/login"] {
            let res = panel
                .send(
                    HttpRequest::builder()
                        .uri(uri)
                        .header(header::HOST, PANEL)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
            assert_eq!(res.status(), StatusCode::OK, "{uri}");
            assert!(res.headers().get(header::SET_COOKIE).is_none(), "{uri}");
        }
    }

    #[tokio::test]
    async fn page_route_without_a_session_redirects_to_login() {
        let panel = panel().await;
        let res = panel
            .send(
                HttpRequest::builder()
                    .uri("/")
                    .header(header::HOST, PANEL)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers().get(header::LOCATION).unwrap(), LOGIN_PATH);
    }

    #[tokio::test]
    async fn logout_ends_every_session() {
        let panel = panel().await;
        let a = panel.session_cookie().await;
        let b = panel.session_cookie().await;
        let res = panel
            .send(
                HttpRequest::builder()
                    .method("POST")
                    .uri(LOGOUT_PATH)
                    .header(header::HOST, PANEL)
                    .header(header::COOKIE, &a)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(set_cookie(&res).contains("Max-Age=0"));
        for cookie in [a, b] {
            assert_eq!(
                panel.api("POST", &cookie, SAME_ORIGIN).await,
                StatusCode::UNAUTHORIZED
            );
        }
    }

    /// Another local page must not be able to log the user out.
    #[tokio::test]
    async fn logout_requires_a_same_origin_request() {
        let panel = panel().await;
        let cookie = panel.session_cookie().await;
        let res = panel
            .send(
                HttpRequest::builder()
                    .method("POST")
                    .uri(LOGOUT_PATH)
                    .header(header::HOST, PANEL)
                    .header(header::COOKIE, &cookie)
                    .header("sec-fetch-site", "same-site")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            panel.api("POST", &cookie, SAME_ORIGIN).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn sessions_survive_a_daemon_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let first = Auth::new(TOKEN.to_owned(), path.clone()).await.unwrap();
        let id = first.sessions.create(PANEL).await.unwrap();
        drop(first);
        let restarted = Auth::new(TOKEN.to_owned(), path).await.unwrap();
        assert!(restarted.sessions.is_valid(&id, PANEL));
    }

    #[test]
    fn split_host_separates_the_port() {
        assert_eq!(split_host("localhost:3000"), ("localhost", ":3000"));
        assert_eq!(split_host("localhost"), ("localhost", ""));
        assert_eq!(split_host("[::1]:3000"), ("[::1]", ":3000"));
        assert_eq!(split_host("[::1]"), ("[::1]", ""));
    }

    // --- Cross-site protection on /api ---

    #[tokio::test]
    async fn api_accepts_same_origin_fetches() {
        let panel = panel().await;
        let cookie = panel.session_cookie().await;
        assert_eq!(
            panel.api("POST", &cookie, SAME_ORIGIN).await,
            StatusCode::OK
        );
        // Older browsers: no Sec-Fetch-Site, but a matching Origin.
        assert_eq!(
            panel
                .api("POST", &cookie, &[("origin", "http://localhost:3000")])
                .await,
            StatusCode::OK
        );
    }

    /// A page on another port of the same host is same-*site*, so the cookie
    /// is attached; the origin check is what stops it.
    #[tokio::test]
    async fn api_rejects_cross_origin_posts_even_with_a_valid_session() {
        let panel = panel().await;
        let cookie = panel.session_cookie().await;
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
                panel.api("POST", &cookie, headers).await,
                StatusCode::FORBIDDEN,
                "{headers:?}"
            );
        }
    }

    #[tokio::test]
    async fn api_rejects_non_post_methods() {
        let panel = panel().await;
        let cookie = panel.session_cookie().await;
        assert_eq!(
            panel.api("GET", &cookie, SAME_ORIGIN).await,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    /// A planted cookie sorting first must not lock the real session out.
    #[tokio::test]
    async fn a_planted_session_cookie_does_not_shadow_the_real_one() {
        let panel = panel().await;
        let cookie = panel.session_cookie().await;
        let both = format!("gavmo_session=planted; {cookie}");
        assert_eq!(panel.api("POST", &both, SAME_ORIGIN).await, StatusCode::OK);
    }

    /// The 401 body decodes as `AppError::Unauthorized`, which the browser
    /// uses to send the user back to log in.
    #[tokio::test]
    async fn api_401_body_is_an_encoded_unauthorized_error() {
        let panel = panel().await;
        let res = panel
            .send(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header(header::HOST, PANEL)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        let decoded: crate::types::AppError = serde_json::from_slice(&body).unwrap();
        assert_eq!(decoded, crate::types::AppError::Unauthorized);
    }
}
