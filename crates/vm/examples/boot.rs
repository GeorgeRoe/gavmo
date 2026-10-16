//! Milestone 0 runner: boots the GAVMO base NixOS image (see
//! `nix/base/`) against a real repository directory and waits for its
//! dummy health service to answer, proving the `vm` crate end to end
//! (see `crates/vm/plan.md`).
//!
//! This is deliberately a `cargo` example rather than a real binary: it's
//! a throwaway driver to see the `vm` crate work, not the GAVMO daemon.
//!
//! Usage:
//!   cargo run -p vm --example boot -- \
//!       <base-image.qcow2> <repo-dir> <gcloud-dir> <host-port>
//!
//! Then open http://127.0.0.1:<host-port> in a browser — it should show a
//! file listing of <repo-dir>, served from inside the VM over 9p.
//! Ctrl-C to request a graceful shutdown.

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
        .map_err(|_| "host-port must be a valid u16")?;

    for (label, path) in [
        ("base image", &base_image),
        ("repo dir", &repo_dir),
        ("gcloud dir", &gcloud_dir),
    ] {
        if !path.exists() {
            return Err(format!("{label} does not exist: {}", path.display()).into());
        }
    }

    // Conventional per-workspace state location from the MVP plan: all
    // GAVMO state lives under <repo>/.gavmo, gitignored.
    let state_dir = repo_dir.join(".gavmo").join("vm");
    let workspace_id = repo_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string());

    // The real daemon keeps this *outside* the repo (see
    // `crates/gavmo/src/registry.rs::mcp_dir_for`) so the MCP capability
    // token never lands in the user's repo. This throwaway example has no
    // daemon app-data dir to put it in, so it just reuses `.gavmo` for
    // simplicity — fine for a manual debug run, not a pattern to copy.
    let mcp_dir = repo_dir.join(".gavmo").join("mcp");
    std::fs::create_dir_all(&mcp_dir)?;

    let cfg = VmConfig {
        workspace_id,
        repo_dir: repo_dir.clone(),
        state_dir,
        base_image,
        gcloud_dir,
        mcp_dir,
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
