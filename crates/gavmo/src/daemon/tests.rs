//! End-to-end tests of the control panel: real router, real session layer,
//! real server functions and registry, with a fake VM launcher and an
//! in-memory secret store.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use leptos::server_fn::ServerFn;
use tower::ServiceExt as _;

use crate::api;
use crate::registry::AppState;
use crate::types::{AppError, ConsoleChunk, ToolConfig, ToolList, WorkspaceInfo, WorkspaceStatus};
use crate::vm_handle::fake::{FakeLauncher, Script};

const TOKEN: &str = "test-ui-token";
const HOST: &str = "localhost:3000";

/// A daemon over temp dirs, plus a logged-in session.
pub(super) struct Daemon {
    pub(super) data: tempfile::TempDir,
    pub(super) state: AppState,
    pub(super) script: Arc<Script>,
    router: Router,
    cookie: String,
}

impl Daemon {
    pub(super) async fn start() -> Self {
        let data = tempfile::tempdir().unwrap();
        let script = Script::new();
        let state = AppState::with_data_dir(
            PathBuf::from("/nonexistent/base.qcow2"),
            PathBuf::from("/nonexistent/gcloud"),
            data.path(),
            Some(Path::new("/nonexistent/home")),
            &data.path().join("run"),
            Arc::new(secrets::MemoryStore::new()),
            Arc::new(FakeLauncher(Arc::clone(&script))),
        )
        .unwrap();
        let auth = crate::auth::Auth::new(TOKEN.to_owned(), data.path().join("sessions.json"))
            .await
            .unwrap();
        let options = leptos::prelude::LeptosOptions::builder()
            .output_name("gavmo")
            .build();
        let router = super::ui_router(options, &state, &auth);
        let mut daemon = Self {
            data,
            state,
            script,
            router,
            cookie: String::new(),
        };
        daemon.cookie = daemon.login().await;
        daemon
    }

    async fn send(&self, req: Request<Body>) -> axum::response::Response {
        self.router.clone().oneshot(req).await.unwrap()
    }

    /// Logs in at `localhost` and returns the `gavmo_session=<id>` pair.
    async fn login(&self) -> String {
        let res = self
            .send(
                Request::builder()
                    .uri(format!("/login?token={TOKEN}"))
                    .header(header::HOST, HOST)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let set_cookie = res.headers()[header::SET_COOKIE].to_str().unwrap();
        set_cookie.split(';').next().unwrap().to_owned()
    }

    /// Calls a server function the way the browser does (form-encoded POST,
    /// same-origin) and decodes its result.
    pub(super) async fn call<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        args: &[(&str, &str)],
    ) -> Result<T, AppError> {
        let (status, body) = self.raw_call(path, args, &self.cookie, SAME_ORIGIN).await;
        if status.is_success() {
            Ok(serde_json::from_slice(&body)
                .unwrap_or_else(|e| panic!("{path}: {e}: {}", String::from_utf8_lossy(&body))))
        } else {
            Err(serde_json::from_slice(&body).unwrap_or_else(|e| {
                panic!("{path} {status}: {e}: {}", String::from_utf8_lossy(&body))
            }))
        }
    }

    async fn raw_call(
        &self,
        path: &str,
        args: &[(&str, &str)],
        cookie: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Vec<u8>) {
        let mut req = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = self
            .send(req.body(Body::from(form_encode(args))).unwrap())
            .await;
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 4 << 20)
            .await
            .unwrap()
            .to_vec();
        (status, body)
    }

    /// Registers a new temp repo via the API and returns (dir, id).
    pub(super) async fn add_repo(&self) -> (tempfile::TempDir, String) {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().to_str().unwrap().to_owned();
        self.call::<()>(api::AddWorkspace::PATH, &[("path", &path)])
            .await
            .unwrap();
        let id = std::fs::canonicalize(&path)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        (repo, id)
    }

    pub(super) async fn status(&self, id: &str) -> WorkspaceStatus {
        self.call(api::PollStatus::PATH, &[("id", id)])
            .await
            .unwrap()
    }

    /// Polls `status` until `pred` holds (fails after 5s).
    pub(super) async fn wait_for(&self, id: &str, pred: impl Fn(&WorkspaceStatus) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = self.status(id).await;
            if pred(&status) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "last status: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Opens `id` and lets the fake boot complete.
    pub(super) async fn open_running(&self, id: &str) {
        self.call::<()>(api::OpenWorkspace::PATH, &[("id", id)])
            .await
            .unwrap();
        self.script.release_boots(1);
        self.wait_for(id, |s| matches!(s, WorkspaceStatus::Running { .. }))
            .await;
    }
}

const SAME_ORIGIN: &[(&str, &str)] = &[("sec-fetch-site", "same-origin")];

/// `application/x-www-form-urlencoded`, enough for test arguments.
fn form_encode(args: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    char::from(b).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    args.iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

// --- Pages and sessions ---

#[tokio::test]
async fn the_page_renders_for_a_session_and_redirects_without_one() {
    let d = Daemon::start().await;
    let page = |cookie: &str| {
        Request::builder()
            .uri("/")
            .header(header::HOST, HOST)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    let res = d.send(page(&d.cookie)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&html);
    assert!(html.contains("Add workspace") && html.contains("Log out everywhere"));

    let res = d.send(page("")).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], "/login");
}

#[tokio::test]
async fn api_without_a_session_is_a_typed_unauthorized_error() {
    let d = Daemon::start().await;
    let (status, body) = d
        .raw_call(api::ListWorkspaces::PATH, &[], "", SAME_ORIGIN)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let err: AppError = serde_json::from_slice(&body).unwrap();
    assert_eq!(err, AppError::Unauthorized);
}

#[tokio::test]
async fn oversized_api_bodies_are_refused_after_the_session_check() {
    let d = Daemon::start().await;
    let big = "x".repeat(65 * 1024);
    let args = [("path", big.as_str())];

    let (status, body) = d
        .raw_call(api::AddWorkspace::PATH, &args, &d.cookie, SAME_ORIGIN)
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let err: AppError = serde_json::from_slice(&body).unwrap();
    assert!(matches!(err, AppError::InvalidInput(m) if m.contains("larger than")));

    // Without a session the auth layer answers first.
    let (status, _) = d
        .raw_call(api::AddWorkspace::PATH, &args, "", SAME_ORIGIN)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_refuses_cross_site_posts_and_gets() {
    let d = Daemon::start().await;
    let (status, _) = d
        .raw_call(
            api::ListWorkspaces::PATH,
            &[],
            &d.cookie,
            &[("sec-fetch-site", "same-site")],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let res = d
        .send(
            Request::builder()
                .uri(api::ListWorkspaces::PATH)
                .header(header::HOST, HOST)
                .header(header::COOKIE, &d.cookie)
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn logout_ends_the_session() {
    let d = Daemon::start().await;
    let res = d
        .send(
            Request::builder()
                .method("POST")
                .uri("/logout")
                .header(header::HOST, HOST)
                .header(header::COOKIE, &d.cookie)
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        d.call::<Vec<WorkspaceInfo>>(api::ListWorkspaces::PATH, &[])
            .await,
        Err(AppError::Unauthorized)
    );
}

// --- Workspace lifecycle through the API ---

#[tokio::test]
async fn add_open_stop_reset_remove_round_trip() {
    let d = Daemon::start().await;
    let (_repo, id) = d.add_repo().await;
    let listed: Vec<WorkspaceInfo> = d.call(api::ListWorkspaces::PATH, &[]).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].status, WorkspaceStatus::Stopped);

    d.open_running(&id).await;
    d.call::<()>(api::StopWorkspace::PATH, &[("id", &id)])
        .await
        .unwrap();
    assert_eq!(d.status(&id).await, WorkspaceStatus::Stopped);
    assert_eq!(d.script.stops(), 1);

    d.call::<()>(api::ResetWorkspace::PATH, &[("id", &id)])
        .await
        .unwrap();
    d.call::<()>(api::RemoveWorkspace::PATH, &[("id", &id)])
        .await
        .unwrap();
    let listed: Vec<WorkspaceInfo> = d.call(api::ListWorkspaces::PATH, &[]).await.unwrap();
    assert_eq!(listed, Vec::<WorkspaceInfo>::new());
}

#[tokio::test]
async fn unknown_ids_are_typed_errors_on_every_id_taking_function() {
    let d = Daemon::start().await;
    let unknown = d.data.path().to_str().unwrap().to_owned();
    let expect_unknown = |r: Result<serde_json::Value, AppError>| {
        assert!(matches!(r, Err(AppError::UnknownWorkspace(_))), "{r:?}");
    };
    expect_unknown(d.call(api::OpenWorkspace::PATH, &[("id", &unknown)]).await);
    expect_unknown(d.call(api::ResetWorkspace::PATH, &[("id", &unknown)]).await);
    expect_unknown(
        d.call(api::ListToolConfigs::PATH, &[("id", &unknown)])
            .await,
    );
    expect_unknown(
        d.call(
            api::ReadConsoleLog::PATH,
            &[("id", &unknown), ("offset", "0")],
        )
        .await,
    );
    expect_unknown(
        d.call(
            api::ReadBuildLog::PATH,
            &[
                ("id", &unknown),
                ("log", "rebuild-2026-01-01T00-00-00Z.log"),
            ],
        )
        .await,
    );
}

#[tokio::test]
async fn a_dangerous_path_is_refused_as_invalid_input() {
    let d = Daemon::start().await;
    let data_dir = d.data.path().to_str().unwrap().to_owned();
    let r = d
        .call::<()>(api::AddWorkspace::PATH, &[("path", &data_dir)])
        .await;
    assert!(matches!(r, Err(AppError::InvalidInput(_))), "{r:?}");
}

#[tokio::test]
async fn opening_during_a_stop_is_busy() {
    let d = Daemon::start().await;
    let (_repo, id) = d.add_repo().await;
    d.open_running(&id).await;
    d.script
        .stops_block
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let stop = {
        let router = d.router.clone();
        let cookie = d.cookie.clone();
        let id = id.clone();
        tokio::spawn(async move {
            router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(api::StopWorkspace::PATH)
                        .header(header::HOST, HOST)
                        .header(header::COOKIE, cookie)
                        .header("sec-fetch-site", "same-origin")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(form_encode(&[("id", &id)])))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        })
    };
    d.wait_for(&id, |s| *s == WorkspaceStatus::Stopping).await;
    assert_eq!(
        d.call::<()>(api::OpenWorkspace::PATH, &[("id", &id)]).await,
        Err(AppError::Busy)
    );
    d.script.stop_gate.add_permits(1);
    assert_eq!(stop.await.unwrap(), StatusCode::OK);
}

#[tokio::test]
async fn console_log_is_read_incrementally_through_the_api() {
    let d = Daemon::start().await;
    let (_repo, id) = d.add_repo().await;
    let empty: ConsoleChunk = d
        .call(api::ReadConsoleLog::PATH, &[("id", &id), ("offset", "0")])
        .await
        .unwrap();
    assert_eq!(empty.text, "");

    let hash = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(id.as_bytes())
    );
    let dir = d.data.path().join("vms").join(hash);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("console.log"), "booting\nok\n").unwrap();
    let chunk: ConsoleChunk = d
        .call(api::ReadConsoleLog::PATH, &[("id", &id), ("offset", "0")])
        .await
        .unwrap();
    assert_eq!(
        (chunk.text.as_str(), chunk.next_offset),
        ("booting\nok\n", 11)
    );
}

#[tokio::test]
async fn tool_credentials_round_trip_through_the_api() {
    let d = Daemon::start().await;
    let (repo, id) = d.add_repo().await;
    std::fs::write(
        repo.path().join("gavmo.yaml"),
        "tools:\n  t:\n    type: http\n    base_url: https://h.example.com\n    auth: bearer\n    secret: s\n",
    )
    .unwrap();
    let list: ToolList = d
        .call(api::ListToolConfigs::PATH, &[("id", &id)])
        .await
        .unwrap();
    assert_eq!(list.tools.len(), 1);
    assert!(!list.tools[0].configured);

    d.call::<()>(
        api::SetToolSecret::PATH,
        &[("id", &id), ("tool_id", "t"), ("token", "tok")],
    )
    .await
    .unwrap();
    let list: ToolList = d
        .call(api::ListToolConfigs::PATH, &[("id", &id)])
        .await
        .unwrap();
    assert!(list.tools[0].configured);

    let unknown_tool = d
        .call::<()>(
            api::SetToolSecret::PATH,
            &[("id", &id), ("tool_id", "nope"), ("token", "tok")],
        )
        .await;
    assert!(
        matches!(unknown_tool, Err(AppError::InvalidInput(_))),
        "{unknown_tool:?}"
    );

    d.call::<()>(api::ClearToolSecret::PATH, &[("id", &id), ("tool_id", "t")])
        .await
        .unwrap();
    let list: ToolList = d
        .call(api::ListToolConfigs::PATH, &[("id", &id)])
        .await
        .unwrap();
    assert!(!list.tools[0].configured);
}

#[tokio::test]
async fn an_invalid_gavmo_yaml_is_reported_through_the_api() {
    let d = Daemon::start().await;
    let (repo, id) = d.add_repo().await;
    std::fs::write(repo.path().join("gavmo.yaml"), "tools: nope\n").unwrap();
    let list: ToolList = d
        .call(api::ListToolConfigs::PATH, &[("id", &id)])
        .await
        .unwrap();
    assert_eq!(list.tools, Vec::<ToolConfig>::new());
    assert!(list.config_error.is_some());
}

#[tokio::test]
async fn resources_round_trip_through_the_api() {
    let d = Daemon::start().await;
    let (_repo, id) = d.add_repo().await;
    let view: crate::types::ResourcesView = d
        .call(api::GetResources::PATH, &[("id", &id)])
        .await
        .unwrap();
    assert_eq!(view.saved, crate::types::Resources::default());
    assert_eq!(view.disk_current_gib, None);

    // Absent fields are "use the default".
    d.call::<()>(
        api::SetResources::PATH,
        &[("id", &id), ("disk_gib", "48"), ("vcpus", "1")],
    )
    .await
    .unwrap();
    let view: crate::types::ResourcesView = d
        .call(api::GetResources::PATH, &[("id", &id)])
        .await
        .unwrap();
    assert_eq!(
        view.saved,
        crate::types::Resources {
            disk_gib: Some(48),
            memory_gib: None,
            vcpus: Some(1),
        }
    );

    let too_small = d
        .call::<()>(api::SetResources::PATH, &[("id", &id), ("disk_gib", "1")])
        .await;
    assert!(
        matches!(too_small, Err(AppError::InvalidInput(_))),
        "{too_small:?}"
    );
}
