//! Runner that boots the GAVMO base NixOS image against a repository
//! directory and waits for it to become healthy. A debug driver for the
//! `vm` crate, not the GAVMO daemon.
//!
//! Usage:
//!   cargo run -p vm --example boot -- \
//!       <base-image.qcow2> <repo-dir> <gcloud-dir> <host-port>
//!
//! Then open `http://127.0.0.1:<host-port>`. Requires `virtiofsd` on `PATH`.
//! Ctrl-C requests a graceful shutdown.

use std::path::PathBuf;

use vm::{RunningVm, VmConfig};

const USAGE: &str = "usage: boot <base-image.qcow2> <repo-dir> <gcloud-dir> <host-port>";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);

    let base_image = PathBuf::from(args.next().ok_or(USAGE)?);
    let repo_dir = PathBuf::from(args.next().ok_or(USAGE)?);
    let gcloud_dir = PathBuf::from(args.next().ok_or(USAGE)?);
    let host_port: u16 = args
        .next()
        .ok_or(USAGE)?
        .parse()
        .map_err(|e| format!("host-port must be a valid u16: {e}"))?;

    for (label, path) in [
        ("base image", &base_image),
        ("repo dir", &repo_dir),
        ("gcloud dir", &gcloud_dir),
    ] {
        if !path.exists() {
            return Err(format!("{label} does not exist: {}", path.display()).into());
        }
    }

    // Host-only state must stay outside the (guest-writable) repo; see
    // `VmConfig`. This debug runner keeps it in a per-port temp dir.
    let scratch = std::env::temp_dir().join(format!("gavmo-boot-example-{host_port}"));
    let state_dir = scratch.join("state");
    let runtime_dir = scratch.join("run");
    let guest_output_dir = repo_dir.join(".gavmo").join("vm");
    let workspace_id = repo_dir.file_name().map_or_else(
        || "workspace".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );

    // Outside the repo, so the MCP token never lands in it.
    let mcp_dir = scratch.join("mcp");
    std::fs::create_dir_all(&mcp_dir)?;

    let cfg = VmConfig {
        workspace_id,
        repo_dir: repo_dir.clone(),
        state_dir,
        runtime_dir,
        guest_output_dir,
        base_image,
        gcloud_dir,
        mcp_dir,
        disk_size_gib: 32,
        memory_mib: 4096,
        vcpus: 4,
        host_port,
    };

    println!("Booting VM for {}...", repo_dir.display());
    let mut running = RunningVm::boot(cfg).await?;
    println!("Ready: http://127.0.0.1:{}", running.host_port());
    println!("Press Ctrl-C to stop.");

    tokio::signal::ctrl_c().await?;

    println!("Status before stopping: {:?}", running.status().await);
    println!("Stopping...");
    running.stop().await?;
    println!("Stopped.");

    Ok(())
}
