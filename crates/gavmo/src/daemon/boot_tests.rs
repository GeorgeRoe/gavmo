//! Opt-in full-boot test: the real qemu launcher and a real base image.
//! Host only (it needs KVM and a built `nix/base` image), so `#[ignore]`d:
//!
//! ```sh
//! nix build ./nix/base   # -> result/nixos.qcow2
//! GAVMO_BASE_IMAGE="$(readlink -f result/nixos.qcow2)" \
//!   cargo test -p gavmo --features ssr a_real_vm -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::registry::AppState;
use crate::types::WorkspaceStatus;
use crate::vm_handle::QemuLauncher;

/// A cold first boot creates the overlay and starts opencode; allow plenty.
const BOOT_TIMEOUT: Duration = Duration::from_mins(15);

#[tokio::test]
#[ignore = "boots a real VM: needs GAVMO_BASE_IMAGE, qemu, virtiofsd and /dev/kvm (host only)"]
async fn a_real_vm_boots_reports_stops_and_resets() {
    let base_image = PathBuf::from(
        std::env::var_os("GAVMO_BASE_IMAGE")
            .expect("set GAVMO_BASE_IMAGE to the nixos.qcow2 built from nix/base"),
    );
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let gcloud_dir = tmp.path().join("gcloud");
    std::fs::create_dir_all(&gcloud_dir).unwrap();
    let state = AppState::with_data_dir(
        base_image,
        &gcloud_dir,
        mcp::DEFAULT_MCP_PORT,
        &data_dir,
        None,
        &tmp.path().join("run"),
        Arc::new(secrets::MemoryStore::new()),
        Arc::new(QemuLauncher),
        crate::registry::ModelOptions::for_tests(),
    )
    .unwrap();

    let repo = tempfile::tempdir().unwrap();
    let id = std::fs::canonicalize(repo.path())
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    state.add_workspace(id.clone()).await.unwrap();
    state.open_workspace(&id).await.unwrap();

    // `Running` means opencode answered the health probe on the forwarded
    // port (vm::health); check it from here too.
    let deadline = tokio::time::Instant::now() + BOOT_TIMEOUT;
    let url = loop {
        match state.status(&id).await {
            WorkspaceStatus::Running { url } => break url,
            WorkspaceStatus::Booting => {}
            other => panic!("boot failed: {other:?}"),
        }
        assert!(tokio::time::Instant::now() < deadline, "boot timed out");
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let response = reqwest::get(&url).await.expect("opencode must answer");
    eprintln!("opencode at {url}: HTTP {}", response.status());

    let console = state.read_console_log(&id, 0).await.unwrap();
    assert!(
        !console.text.trim().is_empty(),
        "the boot console must have output"
    );

    let usage = state.vm_stats(&id).await.expect("stats for a running VM");
    assert!(usage.vcpus > 0 && usage.disk_bytes > 0, "{usage:?}");

    state.stop_workspace(&id).await.unwrap();
    assert_eq!(state.status(&id).await, WorkspaceStatus::Stopped);

    let hash = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(id.as_bytes())
    );
    let vm_dir = data_dir.join("vms").join(hash);
    assert!(vm_dir.exists(), "the VM's state dir must exist: {vm_dir:?}");
    state.reset_workspace(&id).await.unwrap();
    assert!(!vm_dir.exists(), "reset must delete {vm_dir:?}");
    assert_eq!(state.status(&id).await, WorkspaceStatus::Stopped);
}
