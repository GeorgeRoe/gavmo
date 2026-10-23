# This repo's own `gavmo.nix` - the GAVMO project is itself a Rust workspace
# (a Leptos control panel + the `vm`/`mcp`/`secrets`/`toolspec` crates), so
# this gives the agent everything it needs to build, test, and run GAVMO
# inside its own VM. It's merged additively onto GAVMO's base config - see the
# main README's "Customizing your repo's VM with `gavmo.nix`" section.
#
# For users browsing this repo as a reference: this is a real, working
# `gavmo.nix`, not a minimal placeholder. Smaller, self-contained examples of
# the mechanism live under `examples/` (e.g. `examples/docker`,
# `examples/playwright`).
#
# What's here and why
# -------------------
# - Rust toolchain from nixpkgs (`rustc`/`cargo`/`clippy`/`rustfmt`). The
#   workspace's `rust-toolchain.toml` asks for the stable channel with the
#   `wasm32-unknown-unknown` target and the `clippy`/`rustfmt` components;
#   nixpkgs' `rustc`/`cargo` already ship std for `wasm32-unknown-unknown`, so
#   the hydrate (browser/WASM) half of the Leptos build works without a
#   separate target install. (nixpkgs ignores `rust-toolchain.toml`'s channel
#   pin - the version is whatever this VM's nixpkgs provides; that's fine for
#   a "stable channel" project like this one.)
# - `cargo-leptos` - the build driver this project uses (`cargo leptos watch`
#   / `cargo leptos build`), which orchestrates both the native SSR build and
#   the wasm32 hydrate build and bundles the site assets.
# - `wasm-bindgen` CLI - `cargo leptos build` shells out to a `wasm-bindgen`
#   binary to generate the JS/WASM glue, and that binary's version must match
#   the `wasm-bindgen` *crate* in `Cargo.lock` EXACTLY. The lock pins
#   `0.2.129` (forced by `js-sys 0.3.106`'s `=0.2.129` dependency - it can't
#   be downgraded), but this VM's nixpkgs only ships `pkgs.wasm-bindgen-cli`
#   at `0.2.108`, so that package can't be used. Instead we `cargo install
#   wasm-bindgen-cli --version <pinned>` into the agent's `~/.cargo/bin` as a
#   `gavmo-opencode` `preStart` step (same reliable-every-boot pattern as the
#   playwright example's npm install - see `examples/playwright/gavmo.nix` for
#   why a separate systemd unit would NOT run under GAVMO's in-guest rebuild).
#   A small wrapper puts `~/.cargo/bin` on PATH (a `PATH` sessionVariable
#   wouldn't survive `/etc/profile` rebuilding PATH - same reasoning as the
#   playwright wrapper). Bump `wasmBindgenVersion` below in lockstep with
#   `Cargo.lock` whenever `wasm-bindgen` changes.
# - `qemu` + `virtiofsd` - running the GAVMO daemon itself boots QEMU VMs and
#   shares host dirs over virtiofs, so both binaries must be on PATH (see the
#   main README's "Running the daemon locally"). Note this is nested
#   virtualization: running GAVMO *inside* a GAVMO VM needs the outer VM to
#   expose KVM (`-cpu host` / nested KVM on the host) for acceptable speed -
#   builds and `cargo test` work regardless.
# - `clang` - a C toolchain is required even for a pure-Rust workspace: rustc
#   invokes `cc` to link the final binaries, and many build-script/proc-macro
#   deps compile C shims. Without it, cargo fails immediately with
#   `error: linker `cc` not found`. `clang`'s wrapper provides `cc` and drives
#   the system `ld`. (GCC would work too; clang chosen to match the common
#   host setup.)
# - A few CLIs the workflow leans on: `pkg-config` + `openssl` for any crate
#   that still probes system OpenSSL at build time (the daemon itself uses
#   rustls, but having these present avoids surprises), and `git`.
#
# Using it
# --------
# Once booted, the agent can build and test the workspace directly, e.g.:
#   cargo test -p toolspec
#   cargo test -p vm
#   cargo build -p gavmo --features ssr --no-default-features
#   cargo leptos build
#
# Caveats
# -------
# - First build is slow and downloads the full crate graph; the persistent
#   overlay disk (see `crates/vm/src/disk.rs`) caches it across reboots, but
#   make sure the guest has disk headroom for `target/` plus the toolchain.
# - The toolchain version tracks this VM's nixpkgs, not `rust-toolchain.toml`.
#   If a specific pinned Rust is ever required, add a Rust overlay (fenix /
#   rust-overlay) via a flake instead - out of scope for this plain module.
# - The `wasm-bindgen` CLI is compiled on first boot (a one-time ~90s
#   `cargo install`, then a fast version check on later boots). It needs
#   working network and is non-fatal (`|| true`) so it can never block
#   `gavmo-opencode` from starting - the native `cargo` build/`cargo test`
#   work without it; only `cargo leptos build`'s wasm glue step needs it.

{ pkgs, ... }:

let
  # MUST match the `wasm-bindgen` crate version in `Cargo.lock` exactly (see
  # the header comment). Bump in lockstep whenever the lock's changes.
  wasmBindgenVersion = "0.2.129";

  # The agent's own home is the one writable place for a `cargo install`
  # prefix (the Nix store + system are read-only to it). Exposing
  # `~/.cargo/bin` on PATH has to go through a wrapper package in
  # `systemPackages` (which lands in `/run/current-system/sw/bin`, picked up by
  # `/etc/profile`), NOT a `PATH` sessionVariable - `gavmo-opencode`'s startup
  # sources `/etc/profile`, which rebuilds PATH from scratch and would discard
  # a `PATH` session var. Same mechanism as the playwright example's wrapper.
  cargoBinDir = "/home/agent/.cargo/bin";
  wasmBindgenWrapper = pkgs.writeShellScriptBin "wasm-bindgen" ''
    exec "${cargoBinDir}/wasm-bindgen" "$@"
  '';
in
{
  environment.systemPackages = with pkgs; [
    # Rust toolchain (stable, from nixpkgs; wasm32 std included for the
    # Leptos hydrate build - see header).
    rustc
    cargo
    clippy
    rustfmt

    # C toolchain - rustc needs `cc` to link, and some deps build C shims.
    # Without this, cargo fails with `error: linker `cc` not found`.
    clang

    # `lld` - the `wasm32-unknown-unknown` target (the Leptos hydrate build)
    # links with LLVM's lld. nixpkgs' `rustc` does not ship `rust-lld` in its
    # sysroot, so the standalone `lld` must be on PATH or `cargo leptos build`
    # fails the wasm half with `error: linker `lld` not found`. (The native
    # ssr build uses `cc`/clang above; only the wasm target needs this.)
    lld

    # Leptos build driver this project uses.
    cargo-leptos

    # Puts the `cargo install`-ed `wasm-bindgen` (see preStart below) on PATH -
    # a `PATH` session var wouldn't survive `/etc/profile`; see header.
    wasmBindgenWrapper

    # Needed to actually run the GAVMO daemon (boots QEMU, virtiofs shares).
    qemu
    virtiofsd

    # Build-time odds and ends + version control.
    pkg-config
    openssl
    git
  ];

  # Install the lock-matching `wasm-bindgen` CLI into the agent's `~/.cargo`
  # as a `preStart` of `gavmo-opencode` itself. This runs reliably every boot
  # under GAVMO's in-guest `nixos-rebuild test` flow, whereas a brand-new
  # standalone systemd unit would not (see `examples/playwright/gavmo.nix`'s
  # header for the full transaction-gap explanation). `preStart` inherits
  # `gavmo-opencode`'s `User=agent`, its `ReadWritePaths` (includes
  # `/home/agent`), and runs `set -e`, so the trailing `|| true` is what keeps
  # a failure (e.g. no network yet) from failing opencode's own startup.
  #
  # Idempotent: once installed, the version check short-circuits and no
  # recompile happens. The `cc`/linker (`clang`) needed to build the CLI is
  # already on PATH via `systemPackages` above.
  systemd.services.gavmo-opencode.preStart = ''
    installed="$(${cargoBinDir}/wasm-bindgen --version 2>/dev/null || true)"
    if [ "$installed" != "wasm-bindgen ${wasmBindgenVersion}" ]; then
      HOME=/home/agent \
        ${pkgs.cargo}/bin/cargo install wasm-bindgen-cli \
          --version "${wasmBindgenVersion}" || true
    fi
  '';
}

