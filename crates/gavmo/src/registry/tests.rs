//! Registry tests: lifecycle races (with a fake VM launcher) and the
//! helpers they share, plus unit tests of each submodule.

use super::*;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vm::{Error as VmError, VmStatus};

use mcp::{TokenAuthority, WorkspaceScope};

use super::lifecycle::{
    OPENCODE_WORKSPACE_SESSION_PATH, VmSlot, mint_mcp_token, slot_for_boot_error, to_status,
};
use super::logs::{MAX_CONSOLE_LOG_BYTES, is_safe_rebuild_log_filename, sanitize_console};
use super::paths::{check_workspace_path, human_bytes, workspace_hash};
use super::ports::{alloc_port, choose_host_port, resolve_host_port_in};
use super::store::{Registry, WorkspaceRecord, load_registry, save_registry};
use super::util::{lock_unpoisoned, remove_dir_all_idempotent, write_owner_only};
use crate::types::{ResourceValues, Resources, ToolConfig, WorkspaceStatus};

/// systemd's SGR-colored `[  OK  ]` lines with `\r\n` come out as plain text.
#[test]
fn sanitize_console_strips_systemd_sgr_color_sequences() {
    let raw = "[\u{1b}[0;32m  OK  \u{1b}[0m] Started \u{1b}[0;1;39mJournal Service\u{1b}[0m.\r\n";
    assert_eq!(
        sanitize_console(raw.as_bytes()),
        "[  OK  ] Started Journal Service.\n"
    );
}

/// OSC sequences are stripped whether terminated by BEL or ST (`ESC \`).
#[test]
fn sanitize_console_strips_osc_sequences() {
    let bel_terminated = "before\u{1b}]0;some title\u{7}after\n";
    assert_eq!(sanitize_console(bel_terminated.as_bytes()), "beforeafter\n");

    let st_terminated = "before\u{1b}]0;some title\u{1b}\\after\n";
    assert_eq!(sanitize_console(st_terminated.as_bytes()), "beforeafter\n");
}

/// DCS sequences (`ESC P ... ST`, as GRUB emits) are stripped entirely.
#[test]
fn sanitize_console_strips_dcs_sequences() {
    assert_eq!(
        sanitize_console("before\u{1b}P+q6E616D65\u{1b}\\after\n".as_bytes()),
        "beforeafter\n"
    );
}

/// Other two-byte escapes (`ESC c`, `ESC 7`, `ESC 8`) are swallowed cleanly.
#[test]
fn sanitize_console_strips_other_two_byte_escapes() {
    assert_eq!(
        sanitize_console("a\u{1b}cb\u{1b}7c\u{1b}8d".as_bytes()),
        "abcd"
    );
}

/// A bare `\r` becomes a line break, as does `\r\n`.
#[test]
fn sanitize_console_treats_bare_cr_as_a_line_break() {
    assert_eq!(sanitize_console(b"one\rtwo\r\nthree"), "one\ntwo\nthree");
}

/// Plain text passes through unchanged.
#[test]
fn sanitize_console_passes_plain_text_through() {
    assert_eq!(
        sanitize_console(b"Booting from Hard Disk...\n"),
        "Booting from Hard Disk...\n"
    );
}

/// URL-safe base64 without padding, matching opencode's encoding;
/// independent of the constant so the test catches drift.
fn base64_url_nopad(input: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = input.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// `OPENCODE_WORKSPACE_SESSION_PATH` equals base64url("/workspace") + "/session".
#[test]
fn opencode_workspace_session_path_matches_base64url_workspace() {
    let expected = format!("/{}/session", base64_url_nopad("/workspace"));
    assert_eq!(OPENCODE_WORKSPACE_SESSION_PATH, expected);
}

#[test]
fn to_status_running_appends_opencode_workspace_session_path() {
    let status = to_status(VmStatus::Running {
        url: "http://127.0.0.1:4096".to_string(),
    });
    assert_eq!(
        status,
        WorkspaceStatus::Running {
            url: "http://127.0.0.1:4096/L3dvcmtzcGFjZQ/session".to_string(),
        }
    );
}

#[test]
fn to_status_maps_rebuild_failed_to_build_failed() {
    let err = VmError::RebuildFailed {
        log_file: "rebuild-2026-01-01T00-00-00Z.log".to_string(),
    };
    let slot = slot_for_boot_error(&err);
    assert!(matches!(
        slot,
        VmSlot::BuildFailed { log } if log == "rebuild-2026-01-01T00-00-00Z.log"
    ));
}

/// Only well-formed rebuild log names are accepted; traversal attempts are rejected.
#[test]
fn rebuild_log_filename_safety() {
    // Shaped exactly like what `gavmo-rebuild` writes.
    assert!(is_safe_rebuild_log_filename(
        "rebuild-2026-10-13T10-10-32Z.log"
    ));

    // Path traversal / escape attempts.
    assert!(!is_safe_rebuild_log_filename("../../../etc/passwd"));
    assert!(!is_safe_rebuild_log_filename("rebuild-../../secret.log"));
    assert!(!is_safe_rebuild_log_filename("rebuild-foo/../bar.log"));
    assert!(!is_safe_rebuild_log_filename("/etc/gavmo/system/flake.nix"));
    assert!(!is_safe_rebuild_log_filename("rebuild-foo\\bar.log"));

    // Wrong shape entirely.
    assert!(!is_safe_rebuild_log_filename("not-a-rebuild-log.txt"));
    assert!(!is_safe_rebuild_log_filename("rebuild-no-extension"));
    assert!(!is_safe_rebuild_log_filename(""));
    assert!(!is_safe_rebuild_log_filename("rebuild-.log"));
    assert!(!is_safe_rebuild_log_filename("rebuild-x.LOG"));

    // Characters a denylist would miss.
    assert!(!is_safe_rebuild_log_filename("rebuild-a\0b.log"));
    assert!(!is_safe_rebuild_log_filename("rebuild-a\nb.log"));
    assert!(!is_safe_rebuild_log_filename("rebuild-a.b.log"));
    assert!(!is_safe_rebuild_log_filename("rebuild-a\u{2215}b.log"));
}

/// With no persisted port, a fresh one is allocated.
#[test]
fn choose_host_port_allocates_fresh_when_none_persisted() {
    let port = choose_host_port(None).expect("allocation should succeed");
    assert!(port > 0);
}

/// A persisted port that is still free is reused as-is.
#[test]
fn choose_host_port_reuses_a_free_persisted_port() {
    // Use a real free port rather than hardcoding one that might collide.
    let persisted = alloc_port().unwrap();

    assert_eq!(choose_host_port(Some(persisted)).unwrap(), persisted);
}

/// An occupied persisted port falls back to a fresh allocation.
#[test]
fn choose_host_port_falls_back_when_persisted_port_is_taken() {
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();

    let chosen = choose_host_port(Some(occupied_port)).expect("should fall back, not error");

    assert_ne!(
        chosen, occupied_port,
        "must not choose a port that's actually in use"
    );
    drop(occupied);
}

/// A new workspace gets a port written into its record, reported as `changed`.
#[test]
fn resolve_host_port_in_persists_a_fresh_port_for_a_new_workspace() {
    let mut registry = Registry {
        workspaces: vec![WorkspaceRecord {
            path: "/workspace/one".to_string(),
            host_port: None,
            resources: Resources::default(),
        }],
    };

    let (port, changed) = resolve_host_port_in(&mut registry, "/workspace/one").unwrap();

    assert!(changed, "a newly-chosen port must be reported as a change");
    assert_eq!(
        registry.workspaces[0].host_port,
        Some(port),
        "the chosen port must be written back into the matching record"
    );
}

/// A still-free persisted port is reused with `changed = false`.
#[test]
fn resolve_host_port_in_is_a_noop_when_the_persisted_port_is_still_free() {
    let persisted = alloc_port().unwrap();
    let mut registry = Registry {
        workspaces: vec![WorkspaceRecord {
            path: "/workspace/two".to_string(),
            host_port: Some(persisted),
            resources: Resources::default(),
        }],
    };

    let (port, changed) = resolve_host_port_in(&mut registry, "/workspace/two").unwrap();

    assert_eq!(port, persisted);
    assert!(
        !changed,
        "reusing the same port must not be reported as a change"
    );
    assert_eq!(registry.workspaces[0].host_port, Some(persisted));
}

/// An occupied persisted port is replaced and the record overwritten.
#[test]
fn resolve_host_port_in_overwrites_a_persisted_port_that_is_now_taken() {
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();
    let mut registry = Registry {
        workspaces: vec![WorkspaceRecord {
            path: "/workspace/three".to_string(),
            host_port: Some(occupied_port),
            resources: Resources::default(),
        }],
    };

    let (port, changed) = resolve_host_port_in(&mut registry, "/workspace/three").unwrap();

    assert_ne!(port, occupied_port);
    assert!(changed);
    assert_eq!(registry.workspaces[0].host_port, Some(port));
    drop(occupied);
}

/// Resolving one workspace's port leaves other records untouched.
#[test]
fn resolve_host_port_in_only_touches_the_matching_workspace() {
    let mut registry = Registry {
        workspaces: vec![
            WorkspaceRecord {
                path: "/workspace/a".to_string(),
                host_port: Some(4096),
                resources: Resources::default(),
            },
            WorkspaceRecord {
                path: "/workspace/b".to_string(),
                host_port: None,
                resources: Resources::default(),
            },
        ],
    };

    resolve_host_port_in(&mut registry, "/workspace/b").unwrap();

    assert_eq!(
        registry.workspaces[0].host_port,
        Some(4096),
        "the untouched workspace's persisted port must be left alone"
    );
    assert!(registry.workspaces[1].host_port.is_some());
}

/// An unknown id still gets a usable port, with `changed = false`.
#[test]
fn resolve_host_port_in_degrades_gracefully_for_an_unknown_workspace() {
    let mut registry = Registry { workspaces: vec![] };

    let (port, changed) = resolve_host_port_in(&mut registry, "/not/registered").unwrap();

    assert!(port > 0);
    assert!(!changed);
    assert!(registry.workspaces.is_empty());
}

/// A not-yet-created path `<tempdir>/<tag>`. Keep the returned `TempDir`
/// alive for the test: dropping it deletes the whole tree, even on failure.
fn unique_temp_dir(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(tag);
    (tmp, path)
}

/// A populated state dir is deleted along with its contents.
#[tokio::test]
async fn remove_dir_all_idempotent_deletes_an_existing_tree() {
    let (_tmp, dir) = unique_temp_dir("reset-exists");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("disk.qcow2"), b"fake overlay")
        .await
        .unwrap();

    remove_dir_all_idempotent(&dir).await.unwrap();

    assert!(!dir.exists());
}

/// Deleting a missing dir is `Ok(())`, not `NotFound`.
#[tokio::test]
async fn remove_dir_all_idempotent_is_a_noop_when_already_gone() {
    let (_tmp, dir) = unique_temp_dir("reset-missing");
    assert!(!dir.exists());

    remove_dir_all_idempotent(&dir).await.unwrap();
}

/// A throwaway [`AppState`] with placeholder image paths, an in-memory
/// secret store, and its own temp data dir (kept alive by the returned
/// `TempDir`), so tests never read or write the user's real registry.
fn test_app_state() -> (tempfile::TempDir, AppState) {
    let (data, state, _script) = scripted_app_state();
    (data, state)
}

/// [`test_app_state`] plus the controls of its fake VM launcher.
fn scripted_app_state() -> (
    tempfile::TempDir,
    AppState,
    Arc<crate::vm_handle::fake::Script>,
) {
    let script = crate::vm_handle::fake::Script::new();
    let data = tempfile::tempdir().unwrap();
    let state = AppState::with_data_dir(
        PathBuf::from("/nonexistent/gavmo-test-base.qcow2"),
        PathBuf::from("/nonexistent/gavmo-test-gcloud"),
        data.path(),
        Some(Path::new("/nonexistent/gavmo-test-home")),
        &data.path().join("run"),
        Arc::new(secrets::MemoryStore::new()),
        Arc::new(crate::vm_handle::fake::FakeLauncher(Arc::clone(&script))),
    )
    .unwrap();
    (data, state, script)
}

/// A `Booting` slot whose task never finishes in test time, standing in
/// for an in-flight boot.
fn never_finishing_boot_slot() -> VmSlot {
    VmSlot::Booting {
        task: tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(600)).await;
        }),
        generation: u64::MAX,
        token: "test-token".to_owned(),
    }
}

/// `stop_workspace` aborts an in-flight boot and returns promptly
/// (bounded by a timeout so a regression fails instead of hanging).
#[tokio::test]
async fn stop_workspace_aborts_an_in_flight_boot_promptly() {
    let (_data, state) = test_app_state();
    let id = "stop-workspace-aborts-an-in-flight-boot-promptly";
    state
        .inner
        .running
        .lock()
        .await
        .insert(id.to_string(), never_finishing_boot_slot());

    tokio::time::timeout(Duration::from_secs(5), state.stop_workspace(id))
        .await
        .expect("stop_workspace should abort the in-flight boot promptly, not wait for it")
        .unwrap();

    assert!(state.inner.running.lock().await.get(id).is_none());
}

/// `reset_workspace` aborts an in-flight boot promptly, then deletes the state dir.
#[tokio::test]
async fn reset_workspace_aborts_an_in_flight_boot_before_deleting_state() {
    let (_tmp, repo_dir) = unique_temp_dir("reset-abort-repo");
    let state_dir = repo_dir.join(".gavmo").join("vm");
    tokio::fs::create_dir_all(&state_dir).await.unwrap();
    tokio::fs::write(state_dir.join("disk.qcow2"), b"fake overlay")
        .await
        .unwrap();
    let id = repo_dir.to_string_lossy().into_owned();

    let (_data, state) = test_app_state();
    register(&state, &id).await;
    let host_state = state.host_state_dir_for(&id);
    let runtime = state.runtime_dir_for(&id);
    for dir in [&host_state, &runtime] {
        tokio::fs::create_dir_all(dir).await.unwrap();
    }
    tokio::fs::write(host_state.join("disk.qcow2"), b"fake overlay")
        .await
        .unwrap();
    state
        .inner
        .running
        .lock()
        .await
        .insert(id.clone(), never_finishing_boot_slot());

    tokio::time::timeout(Duration::from_secs(5), state.reset_workspace(&id))
        .await
        .expect("reset_workspace should abort the in-flight boot promptly, not wait for it")
        .unwrap();

    assert!(!state_dir.exists());
    assert!(!host_state.exists());
    assert!(!runtime.exists());
    assert!(state.inner.running.lock().await.get(&id).is_none());
}

/// Registers `id` in memory only (as `add_workspace` would, minus the
/// canonicalization), for tests of id-gated entry points.
async fn register(state: &AppState, id: &str) {
    state
        .inner
        .registry
        .lock()
        .await
        .workspaces
        .push(WorkspaceRecord {
            path: id.to_owned(),
            host_port: None,
            resources: Resources::default(),
        });
}

/// Unknown ids must never reach the filesystem or boot a VM: they come
/// from the browser and double as host paths.
#[tokio::test]
async fn id_gated_entry_points_reject_unknown_workspaces() {
    let (_tmp, repo_dir) = unique_temp_dir("unknown-repo");
    let state_dir = repo_dir.join(".gavmo").join("vm");
    tokio::fs::create_dir_all(&state_dir).await.unwrap();
    tokio::fs::write(
        state_dir.join("rebuild-2026-01-01T00-00-00Z.log"),
        b"secret log",
    )
    .await
    .unwrap();
    tokio::fs::write(
            repo_dir.join("gavmo.yaml"),
            "tools:\n  t:\n    type: http\n    base_url: https://h.example.com\n    auth: bearer\n    secret: s\n",
        )
        .await
        .unwrap();
    let id = repo_dir.to_string_lossy().into_owned();
    let (_data, state) = test_app_state();

    assert!(state.open_workspace(&id).await.is_err());
    assert!(state.inner.running.lock().await.is_empty(), "no boot task");
    assert!(state.reset_workspace(&id).await.is_err());
    assert!(state_dir.exists(), "reset must not delete an unknown dir");
    assert!(state.read_console_log(&id, 0).await.is_err());
    assert!(
        state
            .read_build_log(&id, "rebuild-2026-01-01T00-00-00Z.log")
            .await
            .is_err()
    );
    assert!(matches!(
        state.list_tool_configs(&id).await,
        Err(RegistryError::UnknownWorkspace(_))
    ));
    assert!(
        state
            .set_tool_secret(&id, "t", None, "tok".to_owned())
            .await
            .is_err()
    );
    assert_eq!(state.status(&id).await, WorkspaceStatus::Stopped);

    // The same directory, once registered, is reachable.
    register(&state, &id).await;
    assert_eq!(
        state
            .read_build_log(&id, "rebuild-2026-01-01T00-00-00Z.log")
            .await
            .unwrap(),
        "secret log"
    );
    assert_eq!(state.list_tool_configs(&id).await.unwrap().tools.len(), 1);
}

/// An alias of a registered path is not the registered workspace.
#[tokio::test]
async fn aliases_of_a_registered_id_are_unknown() {
    let (_data, state) = test_app_state();
    register(&state, "/srv/repo").await;
    for alias in ["/srv/repo/", "/srv/../srv/repo", "/srv/./repo", "/SRV/repo"] {
        assert!(!state.is_known(alias).await, "{alias}");
    }
    assert!(state.is_known("/srv/repo").await);
}

#[test]
fn check_workspace_path_refuses_root_home_and_its_ancestors() {
    let home = Path::new("/home/u");
    for bad in ["/", "/home", "/home/u"] {
        assert!(
            check_workspace_path(Path::new(bad), &[], Some(home)).is_err(),
            "{bad}"
        );
    }
    assert!(check_workspace_path(Path::new("/home/u/src/proj"), &[], Some(home)).is_ok());
    assert!(check_workspace_path(Path::new("/srv/proj"), &[], Some(home)).is_ok());
}

#[test]
fn check_workspace_path_refuses_any_overlap_with_protected_dirs() {
    let protected = [PathBuf::from("/home/u/.local/share/gavmo")];
    for bad in [
        "/home/u/.local/share/gavmo",
        "/home/u/.local/share/gavmo/mcp/abc",
        "/home/u/.local/share",
        "/home/u/.local",
    ] {
        assert!(
            check_workspace_path(Path::new(bad), &protected, None).is_err(),
            "{bad}"
        );
    }
    // Component-wise: a sibling with a shared name prefix is fine.
    assert!(
        check_workspace_path(
            Path::new("/home/u/.local/share/gavmo-other"),
            &protected,
            None
        )
        .is_ok()
    );
}

#[tokio::test]
async fn add_workspace_refuses_dangerous_paths_and_accepts_a_project() {
    let (data, state) = test_app_state();
    let projects = tempfile::tempdir().unwrap();

    // The data dir itself, something inside it, and a dir containing it.
    assert!(state.add_workspace(path_str(data.path())).await.is_err());
    let inside = data.path().join("mcp");
    std::fs::create_dir_all(&inside).unwrap();
    assert!(state.add_workspace(path_str(&inside)).await.is_err());
    let parent = data.path().parent().unwrap();
    assert!(state.add_workspace(path_str(parent)).await.is_err());

    // A regular file is not a workspace.
    let file = projects.path().join("file");
    std::fs::write(&file, b"").unwrap();
    assert!(state.add_workspace(path_str(&file)).await.is_err());

    // A symlink into the data dir is judged by its target.
    #[cfg(unix)]
    {
        let link = projects.path().join("sneaky");
        std::os::unix::fs::symlink(data.path(), &link).unwrap();
        assert!(state.add_workspace(path_str(&link)).await.is_err());
    }

    let project = projects.path().join("proj");
    std::fs::create_dir(&project).unwrap();
    state.add_workspace(path_str(&project)).await.unwrap();
    let canonical = path_str(&std::fs::canonicalize(&project).unwrap());
    assert!(state.is_known(&canonical).await);
    assert_eq!(state.list().await.len(), 1, "only the project was added");
}

fn path_str(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

/// A guest can replace `console.log` (or the whole state dir) with a
/// symlink; the daemon must not follow it into a host file.
#[cfg(unix)]
#[tokio::test]
async fn console_and_build_logs_refuse_guest_planted_symlinks() {
    let (_tmp, repo_dir) = unique_temp_dir("symlink-repo");
    let host = tempfile::tempdir().unwrap();
    let secret = host.path().join("rebuild-2026-01-01T00-00-00Z.log");
    std::fs::write(&secret, b"HOST SECRET").unwrap();
    let id = repo_dir.to_string_lossy().into_owned();
    let (_data, state) = test_app_state();
    register(&state, &id).await;

    // A console.log planted in the repo is not the console log at all:
    // qemu writes it to the host-only state dir.
    let state_dir = repo_dir.join(".gavmo").join("vm");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::os::unix::fs::symlink(&secret, state_dir.join("console.log")).unwrap();
    assert_eq!(state.read_console_log(&id, 0).await.unwrap().text, "");
    let host_state = state.host_state_dir_for(&id);
    std::fs::create_dir_all(&host_state).unwrap();
    std::fs::write(host_state.join("console.log"), b"real console\n").unwrap();
    assert_eq!(
        state.read_console_log(&id, 0).await.unwrap().text,
        "real console\n"
    );

    // Whole state dir swapped for a symlink to a host dir.
    std::fs::remove_dir_all(&state_dir).unwrap();
    std::os::unix::fs::symlink(host.path(), &state_dir).unwrap();
    assert!(
        state
            .read_build_log(&id, "rebuild-2026-01-01T00-00-00Z.log")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn console_log_returns_only_the_newest_bytes() {
    let (_tmp, repo_dir) = unique_temp_dir("big-console");
    let id = repo_dir.to_string_lossy().into_owned();
    let (_data, state) = test_app_state();
    register(&state, &id).await;
    let state_dir = state.host_state_dir_for(&id);
    std::fs::create_dir_all(&state_dir).unwrap();
    let mut big = vec![b'a'; usize::try_from(MAX_CONSOLE_LOG_BYTES).unwrap() * 2];
    big.extend_from_slice(b"\nNEWEST\n");
    std::fs::write(state_dir.join("console.log"), &big).unwrap();

    let chunk = state.read_console_log(&id, 0).await.unwrap();
    assert!(chunk.replace);
    assert!(chunk.text.ends_with("NEWEST\n"));
    assert!(chunk.text.len() as u64 <= MAX_CONSOLE_LOG_BYTES);
    assert_eq!(chunk.next_offset, big.len() as u64);
}

/// The console is followed incrementally, a line at a time, and restarts
/// cleanly if the log shrinks (a new boot).
#[tokio::test]
async fn console_log_is_followed_from_an_offset() {
    let (_tmp, repo_dir) = unique_temp_dir("follow-console");
    let id = repo_dir.to_string_lossy().into_owned();
    let (_data, state) = test_app_state();
    register(&state, &id).await;
    let dir = state.host_state_dir_for(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("console.log");

    // Nothing yet.
    let first = state.read_console_log(&id, 0).await.unwrap();
    assert_eq!((first.text.as_str(), first.next_offset), ("", 0));

    // A partial line is held back until it is complete.
    std::fs::write(&log, b"one\ntw").unwrap();
    let a = state.read_console_log(&id, 0).await.unwrap();
    assert_eq!(
        (a.text.as_str(), a.replace, a.next_offset),
        ("one\n", true, 4)
    );
    std::fs::write(&log, b"one\ntwo\nthree\n").unwrap();
    let b = state.read_console_log(&id, a.next_offset).await.unwrap();
    assert_eq!((b.text.as_str(), b.replace), ("two\nthree\n", false));
    let c = state.read_console_log(&id, b.next_offset).await.unwrap();
    assert_eq!((c.text.as_str(), c.replace), ("", false));

    // The log was restarted: shorter than our offset.
    std::fs::write(&log, b"new boot\n").unwrap();
    let d = state.read_console_log(&id, b.next_offset).await.unwrap();
    assert_eq!(
        (d.text.as_str(), d.replace, d.next_offset),
        ("new boot\n", true, 9)
    );
}

/// An invalid `gavmo.yaml` is reported, not shown as "no tools".
#[tokio::test]
async fn an_invalid_gavmo_yaml_is_reported_in_the_tool_list() {
    let (_data, state) = test_app_state();
    let (_repo, id) = registered_repo(&state).await;
    tokio::fs::write(Path::new(&id).join("gavmo.yaml"), "tools: [not, a, map]\n")
        .await
        .unwrap();
    let list = state.list_tool_configs(&id).await.unwrap();
    assert_eq!(list.tools, Vec::<ToolConfig>::new());
    assert!(list.config_error.is_some_and(|e| !e.is_empty()));
}

/// Everything a host process opens lives outside the guest-writable repo;
/// only the guest's own build output stays in it.
#[test]
fn vm_config_keeps_host_state_out_of_the_repo() {
    let (data, state) = test_app_state();
    let id = "/home/u/src/project";
    let cfg = state.vm_config(id, state.mcp_dir_for(id), 40000, resources::DEFAULTS);
    let hash = workspace_hash(id);

    assert_eq!(cfg.state_dir, data.path().join("vms").join(&hash));
    assert_eq!(
        cfg.runtime_dir,
        data.path().join("run").join(hash.get(..16).unwrap())
    );
    assert_eq!(cfg.guest_output_dir, Path::new(id).join(".gavmo/vm"));
    for host_path in [
        cfg.disk_path(),
        cfg.console_log_path(),
        cfg.qmp_socket_path(),
    ] {
        assert!(!host_path.starts_with(id), "{}", host_path.display());
        assert!(
            !host_path.starts_with(&cfg.mcp_dir),
            "{}",
            host_path.display()
        );
    }
    assert!(cfg.rebuild_failed_marker_path().starts_with(id));
}

/// The longest socket name must fit the unix socket limit under a typical
/// runtime dir, however long the workspace path is.
#[test]
fn runtime_socket_paths_fit_the_unix_limit_for_any_workspace() {
    let id = format!("/home/u/{}", "deep/".repeat(100));
    let runtime = Path::new("/run/user/100000/gavmo")
        .join(workspace_hash(&id).get(..16).unwrap())
        .join("virtiofs-workspace.sock");
    assert!(runtime.as_os_str().len() <= 107, "{}", runtime.display());
}

#[test]
fn human_bytes_formats_log_sizes() {
    assert_eq!(human_bytes(512), "512 B");
    assert_eq!(human_bytes(1536), "1.5 KiB");
    assert_eq!(human_bytes(29_757_145_088), "27.7 GiB");
}

#[test]
fn the_runtime_base_is_created_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (data, _state) = test_app_state();
    let mode = std::fs::metadata(data.path().join("run"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

// --- Lifecycle (with the fake launcher) ---

/// A registered workspace backed by a real (empty) temp dir.
async fn registered_repo(state: &AppState) -> (tempfile::TempDir, String) {
    let repo = tempfile::tempdir().unwrap();
    let id = repo.path().to_str().unwrap().to_owned();
    register(state, &id).await;
    (repo, id)
}

/// Polls `cond` until true, failing after 5s instead of hanging.
async fn eventually(what: &str, mut cond: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !cond().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn slot_is(state: &AppState, id: &str, pred: impl Fn(Option<&VmSlot>) -> bool) -> bool {
    pred(state.inner.running.lock().await.get(id))
}

fn current_token(state: &AppState, id: &str) -> Option<String> {
    lock_unpoisoned(&state.inner.mcp_tokens)
        .iter()
        .find(|(_, owner)| *owner == id)
        .map(|(token, _)| token.clone())
}

/// Opens `id` and lets its fake boot finish.
async fn open_and_run(state: &AppState, script: &crate::vm_handle::fake::Script, id: &str) {
    state.open_workspace(id).await.unwrap();
    script.release_boots(1);
    eventually("Running", async || {
        slot_is(state, id, |s| matches!(s, Some(VmSlot::Running(_)))).await
    })
    .await;
}

#[tokio::test]
async fn concurrent_opens_boot_once() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    let (a, b) = tokio::join!(state.open_workspace(&id), state.open_workspace(&id));
    // One boots; the other is a no-op or refused as busy, never a 2nd boot.
    assert!(a.is_ok() || b.is_ok());
    script.release_boots(2);
    eventually("Running", async || {
        slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Running(_)))).await
    })
    .await;
    state.open_workspace(&id).await.unwrap();
    assert_eq!(script.boots(), 1);
}

#[tokio::test]
async fn stop_revokes_credentials_and_stops_the_vm() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    let token = current_token(&state, &id).expect("a running VM has a token");
    assert!(TokenAuthority::resolve(&state, &token).is_some());

    state.stop_workspace(&id).await.unwrap();
    assert_eq!(TokenAuthority::resolve(&state, &token), None);
    assert!(state.integrations_for(&id).is_empty());
    assert!(slot_is(&state, &id, |s| s.is_none()).await);
    assert_eq!(script.stops(), 1);
}

#[tokio::test]
async fn stop_during_boot_aborts_it_and_revokes_the_token() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    state.open_workspace(&id).await.unwrap();
    let token = current_token(&state, &id).expect("booting VMs hold a token");

    state.stop_workspace(&id).await.unwrap();
    // A late permit must not resurrect anything.
    script.release_boots(1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(slot_is(&state, &id, |s| s.is_none()).await);
    assert_eq!(TokenAuthority::resolve(&state, &token), None);
}

/// While a stop is in progress, open is refused (not queued) and the UI
/// sees `Stopping`.
#[tokio::test]
async fn open_is_refused_while_a_stop_is_in_progress() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    script
        .stops_block
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let stopping = tokio::spawn({
        let state = state.clone();
        let id = id.clone();
        async move { state.stop_workspace(&id).await }
    });
    eventually("Stopping", async || {
        slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Stopping))).await
    })
    .await;
    assert_eq!(state.status(&id).await, WorkspaceStatus::Stopping);
    let refused = state.open_workspace(&id).await.unwrap_err();
    assert!(matches!(refused, RegistryError::Busy), "{refused}");

    script.stop_gate.add_permits(1);
    stopping.await.unwrap().unwrap();
    open_and_run(&state, &script, &id).await;
    assert_eq!(script.boots(), 2);
}

/// Reset keeps the workspace `Stopping` until its files are deleted.
#[tokio::test]
async fn reset_holds_off_opens_until_state_is_deleted() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    let disk_dir = state.host_state_dir_for(&id);
    tokio::fs::create_dir_all(&disk_dir).await.unwrap();
    script
        .stops_block
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let resetting = tokio::spawn({
        let state = state.clone();
        let id = id.clone();
        async move { state.reset_workspace(&id).await }
    });
    eventually("Stopping", async || {
        slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Stopping))).await
    })
    .await;
    assert!(state.open_workspace(&id).await.is_err());
    script.stop_gate.add_permits(1);
    resetting.await.unwrap().unwrap();
    assert!(!disk_dir.exists());
    assert!(slot_is(&state, &id, |s| s.is_none()).await);
}

#[tokio::test]
async fn remove_unregisters_first_then_stops() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;

    state.remove_workspace(&id).await.unwrap();
    assert!(!state.is_known(&id).await);
    assert!(slot_is(&state, &id, |s| s.is_none()).await);
    assert_eq!(script.stops(), 1);
    assert!(state.open_workspace(&id).await.is_err());
    assert_eq!(script.boots(), 1);
}

/// A Running VM whose qemu died becomes Failed (credentials revoked, VM
/// reaped) and can be opened again.
#[tokio::test]
async fn a_dead_vm_becomes_failed_and_can_be_reopened() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    let token = current_token(&state, &id).unwrap();

    script.set_status(VmStatus::Failed(vm::VmFailure::QemuExited(
        std::os::unix::process::ExitStatusExt::from_raw(9),
    )));
    // Noticed by the monitor, not by the status read.
    eventually("Failed", async || {
        matches!(state.status(&id).await, WorkspaceStatus::Failed { .. })
    })
    .await;
    // The reason is readable (Display, not `ExitStatus(unix_wait_status(9))`).
    assert_eq!(
        state.status(&id).await,
        WorkspaceStatus::Failed {
            reason: "qemu exited (signal: 9 (SIGKILL))".to_owned()
        }
    );
    assert!(slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Failed(_)))).await);
    assert_eq!(TokenAuthority::resolve(&state, &token), None);
    eventually("dead VM reaped", async || script.stops() == 1).await;

    script.set_status(VmStatus::Running {
        url: "http://127.0.0.1:1".to_owned(),
    });
    open_and_run(&state, &script, &id).await;
    assert_eq!(script.boots(), 2);
}

/// A VM whose status probe never returns (wedged qemu) must not block
/// `list`, `status`, or stopping it.
#[tokio::test]
async fn a_wedged_vm_blocks_neither_reads_nor_stop() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    script
        .status_hangs
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // Let the monitor get stuck inside a probe.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let quick = Duration::from_secs(1);
    let listed = tokio::time::timeout(quick, state.list()).await.unwrap();
    assert!(matches!(listed[0].status, WorkspaceStatus::Running { .. }));
    tokio::time::timeout(quick, state.status(&id))
        .await
        .unwrap();
    tokio::time::timeout(quick, state.stop_workspace(&id))
        .await
        .expect("stop must cancel the stuck probe")
        .unwrap();
    assert_eq!(script.stops(), 1);
}

/// The monitor publishes what the VM reports.
#[tokio::test]
async fn the_monitor_publishes_status_changes() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    script.set_status(VmStatus::Booting);
    eventually("Booting published", async || {
        state.status(&id).await == WorkspaceStatus::Booting
    })
    .await;
}

#[tokio::test]
async fn shutdown_stops_every_vm_and_refuses_new_boots() {
    let (_data, state, script) = scripted_app_state();
    let (_repo_a, a) = registered_repo(&state).await;
    let (_repo_b, b) = registered_repo(&state).await;
    open_and_run(&state, &script, &a).await;
    open_and_run(&state, &script, &b).await;

    tokio::time::timeout(
        Duration::from_secs(5),
        state.shutdown(Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert_eq!(script.stops(), 2);
    assert!(state.inner.running.lock().await.is_empty());
    assert!(state.open_workspace(&a).await.is_err());
}

/// A VM that won't stop can't hold up exit past the limit.
#[tokio::test]
async fn shutdown_gives_up_after_its_limit() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    script
        .stops_block
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::timeout(
        Duration::from_secs(5),
        state.shutdown(Duration::from_millis(100)),
    )
    .await
    .expect("shutdown must respect its limit");
}

/// A boot finishing after its slot was taken over stops its VM instead of
/// publishing it.
#[tokio::test]
async fn a_superseded_boot_result_is_stopped_not_published() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    {
        let mut running = state.inner.running.lock().await;
        state.replace_slot(&mut running, &id, Some(VmSlot::Stopping));
    }
    let cfg = state.vm_config(&id, state.mcp_dir_for(&id), 1, resources::DEFAULTS);
    script.release_boots(1);
    let vm = state.inner.launcher.boot(cfg).await.unwrap();

    state
        .finish_boot(&id, 12345, "late".to_owned(), Ok(vm))
        .await;
    assert!(slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Stopping))).await);
    assert_eq!(TokenAuthority::resolve(&state, "late"), None);
    assert_eq!(script.stops(), 1);
}

/// Installs `slot` through `replace_slot`, so its token is indexed as a
/// real boot's would be.
async fn install_slot(state: &AppState, id: &str, slot: VmSlot) {
    let mut running = state.inner.running.lock().await;
    state.replace_slot(&mut running, id, Some(slot));
}

/// A boot that fails shows why, and revokes the boot's token at once.
#[tokio::test]
async fn a_failed_boot_shows_the_reason_and_revokes_the_token() {
    let (_data, state) = test_app_state();
    let (_repo, id) = registered_repo(&state).await;
    install_slot(&state, &id, never_finishing_boot_slot()).await;
    assert!(TokenAuthority::resolve(&state, "test-token").is_some());

    let err = VmError::HealthTimeout(Duration::from_secs(1));
    state
        .finish_boot(&id, u64::MAX, "test-token".to_owned(), Err(err))
        .await;

    assert_eq!(
        state.status(&id).await,
        WorkspaceStatus::Failed {
            reason: "vm did not become healthy within 1s".to_owned()
        }
    );
    assert_eq!(TokenAuthority::resolve(&state, "test-token"), None);
    assert!(state.integrations_for(&id).is_empty());
}

/// A `gavmo.nix` build failure is reported as such (with its log), and the
/// workspace can be opened again once it is fixed.
#[tokio::test]
async fn a_build_failure_shows_its_log_and_can_be_reopened() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    install_slot(&state, &id, never_finishing_boot_slot()).await;
    let log = "rebuild-2026-01-01T00-00-00Z.log".to_owned();

    let err = VmError::RebuildFailed {
        log_file: log.clone(),
    };
    state
        .finish_boot(&id, u64::MAX, "test-token".to_owned(), Err(err))
        .await;

    assert_eq!(
        state.status(&id).await,
        WorkspaceStatus::BuildFailed { log }
    );
    assert_eq!(TokenAuthority::resolve(&state, "test-token"), None);
    open_and_run(&state, &script, &id).await;
    assert_eq!(script.boots(), 1);
}

/// A boot result for an older boot than the one in flight is stopped; the
/// current boot and its token are untouched.
#[tokio::test]
async fn a_boot_result_from_an_older_generation_is_stopped() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    install_slot(&state, &id, never_finishing_boot_slot()).await;
    let cfg = state.vm_config(&id, state.mcp_dir_for(&id), 1, resources::DEFAULTS);
    script.release_boots(1);
    let vm = state.inner.launcher.boot(cfg).await.unwrap();

    state.finish_boot(&id, 7, "old".to_owned(), Ok(vm)).await;

    assert!(slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Booting { .. }))).await);
    assert!(TokenAuthority::resolve(&state, "test-token").is_some());
    assert_eq!(TokenAuthority::resolve(&state, "old"), None);
    assert_eq!(script.stops(), 1);
}

/// A failed boot whose slot is already gone (stopped meanwhile) leaves it
/// gone rather than showing a stale failure.
#[tokio::test]
async fn a_failed_boot_nobody_waits_for_changes_nothing() {
    let (_data, state) = test_app_state();
    let (_repo, id) = registered_repo(&state).await;

    let err = VmError::HealthTimeout(Duration::from_secs(1));
    state.finish_boot(&id, 0, "t".to_owned(), Err(err)).await;

    assert!(slot_is(&state, &id, |s| s.is_none()).await);
    assert_eq!(state.status(&id).await, WorkspaceStatus::Stopped);
}

/// Opening a workspace that is already booting neither boots it again nor
/// rotates the token the guest has been given.
#[tokio::test]
async fn opening_while_booting_neither_reboots_nor_rotates_the_token() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    state.open_workspace(&id).await.unwrap();
    let token = current_token(&state, &id).unwrap();

    state.open_workspace(&id).await.unwrap();

    assert_eq!(current_token(&state, &id), Some(token));
    script.release_boots(2);
    eventually("Running", async || {
        slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Running(_)))).await
    })
    .await;
    assert_eq!(script.boots(), 1);
}

/// A dead VM is noticed even if a lifecycle operation held its lock at the
/// time: the monitor backs off and checks again.
#[tokio::test]
async fn a_dead_vm_is_marked_failed_once_a_held_lock_is_released() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    let token = current_token(&state, &id).unwrap();
    let lifecycle = state.lifecycle_lock(&id);
    let held = lifecycle.lock().await;

    script.set_status(VmStatus::Failed(vm::VmFailure::Unwaitable(
        "gone".to_owned(),
    )));
    // Several monitor intervals: each one backs off.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Running(_)))).await);

    drop(held);
    eventually("Failed", async || {
        slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Failed(_)))).await
    })
    .await;
    assert_eq!(TokenAuthority::resolve(&state, &token), None);
    eventually("dead VM reaped", async || script.stops() == 1).await;
}

/// A reset that stops the VM but can't delete its state shows the failure
/// (not a misleading Stopped), and the next open still works.
#[tokio::test]
async fn a_reset_that_cannot_delete_state_is_failed_not_stopped() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    let token = current_token(&state, &id).unwrap();
    // A file where the state dir should be: deleting it as a dir fails.
    let state_dir = state.host_state_dir_for(&id);
    tokio::fs::create_dir_all(state_dir.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&state_dir, b"not a dir").await.unwrap();

    state.reset_workspace(&id).await.unwrap_err();

    assert_eq!(script.stops(), 1);
    assert_eq!(TokenAuthority::resolve(&state, &token), None);
    match state.status(&id).await {
        WorkspaceStatus::Failed { reason } => {
            assert!(
                reason.starts_with("reset could not delete VM state"),
                "{reason}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    tokio::fs::remove_file(&state_dir).await.unwrap();
    open_and_run(&state, &script, &id).await;
}

/// If the per-boot MCP token can't be written, nothing boots and the
/// failure is shown.
#[tokio::test]
async fn a_token_that_cannot_be_written_fails_the_open_without_booting() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    let mcp_dir = state.mcp_dir_for(&id);
    tokio::fs::create_dir_all(mcp_dir.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&mcp_dir, b"not a dir").await.unwrap();

    state.open_workspace(&id).await.unwrap_err();

    assert_eq!(script.boots(), 0);
    assert_eq!(current_token(&state, &id), None);
    match state.status(&id).await {
        WorkspaceStatus::Failed { reason } => {
            assert!(reason.contains("writing the MCP token"), "{reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// Removing a booting workspace aborts the boot; a late boot result can't
/// bring it back.
#[tokio::test]
async fn remove_during_boot_aborts_it_and_revokes_the_token() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    state.open_workspace(&id).await.unwrap();
    let token = current_token(&state, &id).unwrap();

    state.remove_workspace(&id).await.unwrap();
    script.release_boots(1);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(!state.is_known(&id).await);
    assert!(slot_is(&state, &id, |s| s.is_none()).await);
    assert_eq!(TokenAuthority::resolve(&state, &token), None);
}

/// Stop and reset clear a failed or build-failed workspace back to Stopped,
/// the normal way out after fixing `gavmo.nix`.
#[tokio::test]
async fn stop_and_reset_clear_failed_and_build_failed_workspaces() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    let failed = || VmSlot::Failed("boom".to_owned());
    let build_failed = || VmSlot::BuildFailed {
        log: "rebuild-2026-01-01T00-00-00Z.log".to_owned(),
    };

    for slot in [failed(), build_failed()] {
        install_slot(&state, &id, slot).await;
        state.stop_workspace(&id).await.unwrap();
        assert_eq!(state.status(&id).await, WorkspaceStatus::Stopped);
    }
    for slot in [failed(), build_failed()] {
        install_slot(&state, &id, slot).await;
        state.reset_workspace(&id).await.unwrap();
        assert_eq!(state.status(&id).await, WorkspaceStatus::Stopped);
    }
    assert_eq!(script.stops(), 0);
}

/// Shutdown aborts a boot in flight; its late result can't start a VM.
#[tokio::test]
async fn shutdown_aborts_an_in_flight_boot() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    state.open_workspace(&id).await.unwrap();
    let token = current_token(&state, &id).unwrap();

    tokio::time::timeout(
        Duration::from_secs(5),
        state.shutdown(Duration::from_secs(5)),
    )
    .await
    .unwrap();
    script.release_boots(1);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(state.inner.running.lock().await.is_empty());
    assert_eq!(TokenAuthority::resolve(&state, &token), None);
    assert_eq!(script.stops(), 0);
}

/// Even with nothing running, shutdown refuses new boots, with a distinct
/// error.
#[tokio::test]
async fn shutdown_with_nothing_running_still_refuses_opens() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;

    state.shutdown(Duration::from_secs(1)).await;

    let err = state.open_workspace(&id).await.unwrap_err();
    assert!(matches!(err, RegistryError::ShuttingDown), "{err}");
    assert_eq!(script.boots(), 0);
}

/// A slot dropped without a teardown stops its monitor, which would
/// otherwise keep the VM handle alive and keep probing it.
#[tokio::test]
async fn dropping_a_running_slot_stops_its_monitor() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    let vm = match state.inner.running.lock().await.get(&id) {
        Some(VmSlot::Running(slot)) => Arc::clone(&slot.vm),
        _ => panic!("expected Running"),
    };
    assert!(Arc::strong_count(&vm) >= 3, "slot, monitor and this test");

    state.inner.running.lock().await.remove(&id);

    eventually("monitor released the VM", async || {
        Arc::strong_count(&vm) == 1
    })
    .await;
}

/// If the registry can't be saved, remove changes nothing: the workspace
/// stays listed and its VM keeps running with a working token, rather than
/// running on unlisted.
#[tokio::test]
async fn a_remove_that_cannot_save_the_registry_changes_nothing() {
    let (data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    let token = current_token(&state, &id).unwrap();
    block_registry_saves(data.path()).await;

    let err = state.remove_workspace(&id).await.unwrap_err();

    assert_saving_failed(&err);
    assert!(state.is_known(&id).await);
    assert!(slot_is(&state, &id, |s| matches!(s, Some(VmSlot::Running(_)))).await);
    assert!(TokenAuthority::resolve(&state, &token).is_some());
    assert_eq!(script.stops(), 0);
}

#[tokio::test]
async fn save_registry_round_trips_atomically_and_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let registry = Registry {
        workspaces: vec![WorkspaceRecord {
            path: "/srv/a".to_owned(),
            host_port: Some(40000),
            resources: Resources::default(),
        }],
    };
    save_registry(&path, &registry).await.unwrap();
    save_registry(&path, &registry).await.unwrap();

    let loaded = load_registry(&path).unwrap();
    assert_eq!(loaded.workspaces.len(), 1);
    assert_eq!(loaded.workspaces[0].host_port, Some(40000));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["workspaces.json"], "no temp files left behind");
}

/// A truncated registry (as a crash mid-write used to leave) no longer
/// stops the daemon starting; it is kept aside for inspection.
#[test]
fn a_corrupt_registry_is_moved_aside_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    std::fs::write(&path, br#"{"workspaces":[{"path":"/sr"#).unwrap();

    let loaded = load_registry(&path).unwrap();
    assert!(loaded.workspaces.is_empty());
    assert!(!path.exists());
    let aside: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(aside.len(), 1);
    assert!(
        aside[0].starts_with("workspaces.json.corrupt-"),
        "{aside:?}"
    );
}

/// Unreadable (as opposed to unparseable) is an error: never discard data
/// on a guess.
#[test]
fn an_unreadable_registry_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    std::fs::create_dir(&path).unwrap();
    assert!(load_registry(&path).is_err());
    assert!(path.is_dir());
}

/// Share dir names are deterministic, hex-only, and distinct per workspace.
#[test]
fn workspace_hash_is_deterministic_and_filesystem_safe() {
    let a = workspace_hash("/home/user/project-a");
    let b = workspace_hash("/home/user/project-b");

    assert_eq!(a, workspace_hash("/home/user/project-a"));
    assert_ne!(a, b);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
}

/// Long workspace paths must still give a usable (<= 255 byte) name.
#[test]
fn workspace_hash_has_fixed_length_for_long_paths() {
    let long = format!("/home/user/{}", "d/".repeat(200));
    assert_eq!(workspace_hash(&long).len(), 64);
    assert_eq!(workspace_hash("/").len(), 64);
}

#[cfg(unix)]
#[tokio::test]
async fn write_owner_only_creates_and_resets_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token.env");

    write_owner_only(&path, b"a").await.unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // A pre-existing, wider file is narrowed and overwritten.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    write_owner_only(&path, b"b").await.unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(std::fs::read(&path).unwrap(), b"b");
}

/// Minting writes `token.env`; the token is not valid until a slot
/// holds it.
#[tokio::test]
async fn a_minted_token_is_written_but_only_valid_once_installed() {
    let (_data, state) = test_app_state();
    let (_tmp, dir) = unique_temp_dir("mcp-token-mint");

    let token = mint_mcp_token(&dir).await.unwrap();
    let contents = tokio::fs::read_to_string(dir.join("token.env"))
        .await
        .unwrap();
    assert_eq!(contents, format!("GAVMO_MCP_TOKEN={token}\n"));
    assert_eq!(TokenAuthority::resolve(&state, &token), None);
}

/// The token index follows the slot: valid while Booting/Running,
/// revoked by any other slot, and replaced by a newer boot's token.
#[tokio::test]
async fn replace_slot_keeps_tokens_in_step_with_the_slot() {
    let (_data, state) = test_app_state();
    let id = "/ws";
    let booting = |token: &str| VmSlot::Booting {
        task: tokio::spawn(async {}),
        generation: 0,
        token: token.to_owned(),
    };
    let mut running = state.inner.running.lock().await;

    state.replace_slot(&mut running, id, Some(booting("old")));
    assert_eq!(
        TokenAuthority::resolve(&state, "old"),
        Some(WorkspaceScope {
            workspace_id: id.to_owned()
        })
    );
    state.replace_slot(&mut running, id, Some(booting("new")));
    assert_eq!(TokenAuthority::resolve(&state, "old"), None);
    assert!(TokenAuthority::resolve(&state, "new").is_some());

    for revoking in [
        Some(VmSlot::Stopping),
        Some(VmSlot::Failed("x".to_owned())),
        None,
    ] {
        state.replace_slot(&mut running, id, Some(booting("t")));
        state.replace_slot(&mut running, id, revoking);
        assert_eq!(TokenAuthority::resolve(&state, "t"), None);
    }
}

/// An unknown token resolves to nothing.
#[test]
fn resolve_returns_none_for_an_unknown_token() {
    let (_data, state) = test_app_state();
    assert_eq!(TokenAuthority::resolve(&state, "never-minted"), None);
}

/// A secret store that can't be read is reported as such, not as "no
/// tools configured".
#[tokio::test]
async fn an_unreadable_secret_store_is_an_error_not_unconfigured() {
    struct Broken;
    impl SecretStore for Broken {
        fn get(&self, _: &str) -> Result<Option<String>, secrets::SecretError> {
            Err(secrets::SecretError::Backend("keyring locked".to_owned()))
        }
        fn set(&self, _: &str, _: &str) -> Result<(), secrets::SecretError> {
            Err(secrets::SecretError::Backend("keyring locked".to_owned()))
        }
        fn delete(&self, _: &str) -> Result<(), secrets::SecretError> {
            Err(secrets::SecretError::Backend("keyring locked".to_owned()))
        }
    }
    let data = tempfile::tempdir().unwrap();
    let state = AppState::with_data_dir(
        PathBuf::from("/nonexistent/base.qcow2"),
        PathBuf::from("/nonexistent/gcloud"),
        data.path(),
        Some(Path::new("/nonexistent/home")),
        &data.path().join("run"),
        Arc::new(Broken),
        Arc::new(crate::vm_handle::fake::FakeLauncher(
            crate::vm_handle::fake::Script::new(),
        )),
    )
    .unwrap();
    let (_repo, id) = registered_repo(&state).await;
    tokio::fs::write(
        Path::new(&id).join("gavmo.yaml"),
        "tools:\n  t:\n    type: http\n    base_url: https://h.example.com\n    auth: bearer\n    secret: s\n",
    )
    .await
    .unwrap();

    let err = state.list_tool_configs(&id).await.unwrap_err();
    assert!(matches!(err, RegistryError::SecretStore(_)), "{err}");
    assert!(matches!(
        crate::types::AppError::from(err),
        crate::types::AppError::SecretStore(msg) if msg.contains("keyring locked")
    ));
    // Booting still works, just without tools.
    assert!(state.resolve_integrations_for(&id).await.is_empty());
}

// --- Resources ---

#[test]
fn mem_total_is_read_in_whole_gib() {
    let meminfo = "MemTotal:       16314508 kB\nMemFree:  1 kB\n";
    assert_eq!(resources::parse_mem_total_gib(meminfo), Some(15));
    assert_eq!(resources::parse_mem_total_gib("MemFree: 1 kB\n"), None);
}

#[test]
fn resources_outside_the_limits_are_refused() {
    let min = ResourceValues {
        disk_gib: 16,
        memory_gib: 1,
        vcpus: 1,
    };
    let max = ResourceValues {
        disk_gib: 1024,
        memory_gib: 8,
        vcpus: 4,
    };
    let ok = Resources {
        disk_gib: Some(16),
        memory_gib: Some(8),
        vcpus: None,
    };
    assert!(resources::validate(ok, min, max).is_ok());
    for bad in [
        Resources {
            disk_gib: Some(15),
            ..ok
        },
        Resources {
            memory_gib: Some(9),
            ..ok
        },
        Resources {
            vcpus: Some(0),
            ..ok
        },
    ] {
        assert!(
            matches!(
                resources::validate(bad, min, max),
                Err(RegistryError::InvalidInput(_))
            ),
            "{bad:?}"
        );
    }
}

/// Saved settings persist, and the next boot uses them.
#[tokio::test]
async fn saved_resources_persist_and_reach_the_next_boot() {
    let (data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    let settings = Resources {
        disk_gib: Some(48),
        memory_gib: Some(2),
        vcpus: Some(1),
    };
    state.set_resources(&id, settings).await.unwrap();
    assert_eq!(state.resources(&id).await.unwrap().saved, settings);

    let reloaded = super::store::load_registry(&data.path().join("workspaces.json")).unwrap();
    assert_eq!(reloaded.workspaces[0].resources, settings);

    open_and_run(&state, &script, &id).await;
    let cfg = script.booted.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (cfg.disk_size_gib, cfg.memory_mib, cfg.vcpus),
        (48, 2048, 1)
    );
}

/// Unset fields use the defaults.
#[tokio::test]
async fn unset_resources_boot_with_the_defaults() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;
    let cfg = script.booted.lock().unwrap().last().cloned().unwrap();
    let d = resources::DEFAULTS;
    assert_eq!(
        (cfg.disk_size_gib, cfg.memory_mib, cfg.vcpus),
        (d.disk_gib, d.memory_gib * 1024, d.vcpus)
    );
}

/// Memory and vCPUs can change while running (next start); the disk can't.
#[tokio::test]
async fn the_disk_size_cannot_change_while_running() {
    let (_data, state, script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    open_and_run(&state, &script, &id).await;

    let memory = Resources {
        memory_gib: Some(2),
        ..Resources::default()
    };
    state.set_resources(&id, memory).await.unwrap();
    let disk = Resources {
        disk_gib: Some(64),
        ..memory
    };
    let err = state.set_resources(&id, disk).await.unwrap_err();
    assert!(err.to_string().contains("stop the VM"), "{err}");
    assert_eq!(state.resources(&id).await.unwrap().saved, memory);
}

/// An older `workspaces.json` without `resources` still loads.
#[test]
fn a_registry_without_resources_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    std::fs::write(
        &path,
        r#"{"workspaces":[{"path":"/srv/a","host_port":4000}]}"#,
    )
    .unwrap();
    let registry = super::store::load_registry(&path).unwrap();
    assert_eq!(registry.workspaces[0].resources, Resources::default());
}

/// With a real overlay: it can be grown but not shrunk, and its size is
/// reported.
#[tokio::test]
#[ignore = "needs qemu-img"]
async fn an_existing_disk_can_grow_but_not_shrink() {
    let (_data, state, _script) = scripted_app_state();
    let (_repo, id) = registered_repo(&state).await;
    let dir = state.host_state_dir_for(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let status = std::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(dir.join("disk.qcow2"))
        .arg("40G")
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        state.resources(&id).await.unwrap().disk_current_gib,
        Some(40)
    );

    let size = |gib| Resources {
        disk_gib: Some(gib),
        ..Resources::default()
    };
    let err = state.set_resources(&id, size(33)).await.unwrap_err();
    assert!(err.to_string().contains("can't shrink"), "{err}");
    state.set_resources(&id, size(40)).await.unwrap();
    state.set_resources(&id, size(64)).await.unwrap();
}

/// Makes every later registry save fail: a non-empty dir where
/// `workspaces.json` goes can't be renamed over.
async fn block_registry_saves(data: &Path) {
    let file = data.join("workspaces.json");
    if file.exists() {
        tokio::fs::remove_file(&file).await.unwrap();
    }
    tokio::fs::create_dir(&file).await.unwrap();
    tokio::fs::write(file.join("x"), b"").await.unwrap();
}

fn assert_saving_failed(err: &RegistryError) {
    assert!(
        matches!(err, RegistryError::Io { context, .. } if context == "saving the workspace registry"),
        "{err}"
    );
}

/// An add whose save fails doesn't list a workspace that would vanish on
/// restart.
#[tokio::test]
async fn an_add_that_cannot_save_the_registry_changes_nothing() {
    let (data, state) = test_app_state();
    let repo = tempfile::tempdir().unwrap();
    block_registry_saves(data.path()).await;

    let err = state
        .add_workspace(repo.path().to_str().unwrap().to_owned())
        .await
        .unwrap_err();

    assert_saving_failed(&err);

    assert!(state.list().await.is_empty());
}

/// Resource settings whose save fails aren't applied to the next boot
/// either, so what the panel shows after the error is what is used.
#[tokio::test]
async fn resources_that_cannot_be_saved_are_not_applied() {
    let (data, state) = test_app_state();
    let (_repo, id) = registered_repo(&state).await;
    block_registry_saves(data.path()).await;
    let wanted = Resources {
        disk_gib: None,
        memory_gib: Some(2),
        vcpus: Some(1),
    };

    let err = state.set_resources(&id, wanted).await.unwrap_err();

    assert_saving_failed(&err);
    assert_eq!(state.saved_resources(&id).await, Resources::default());
}
