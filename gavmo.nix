# This repo's own `gavmo.nix`: everything the agent needs to build, test, and
# run GAVMO inside its own VM. See README.md ("Running the daemon locally",
# "Customizing your repo's VM with `gavmo.nix`"); smaller recipes live in
# `examples/`.
#
# Once booted:
#   cargo test -p toolspec
#   cargo build -p gavmo --features ssr --no-default-features
#   cargo leptos build

{ pkgs, ... }:

let
  # Must equal the `wasm-bindgen` crate version in `Cargo.lock` exactly; bump
  # together. nixpkgs' `wasm-bindgen-cli` lags behind, hence `cargo install`.
  wasmBindgenVersion = "0.2.129";

  # Wrapper instead of a `PATH` session var, which `/etc/profile` discards
  # (README: "Installing tools from outside Nix").
  cargoBinDir = "/home/agent/.cargo/bin";
  wasmBindgenWrapper = pkgs.writeShellScriptBin "wasm-bindgen" ''
    exec "${cargoBinDir}/wasm-bindgen" "$@"
  '';
in
{
  environment.systemPackages = with pkgs; [
    # Version tracks this VM's nixpkgs, not `rust-toolchain.toml`. nixpkgs'
    # rustc already ships wasm32 std for the Leptos hydrate build.
    rustc
    cargo
    clippy
    rustfmt

    # rustc needs `cc` to link, and some deps build C shims; without it:
    # `error: linker `cc` not found`.
    clang

    # The wasm32 target links with lld, and nixpkgs' rustc doesn't ship
    # `rust-lld`; without it `cargo leptos build` fails the wasm half.
    lld

    cargo-leptos
    wasmBindgenWrapper

    # To run the GAVMO daemon itself.
    qemu
    virtiofsd

    pkg-config
    openssl
    git
  ];

  # One-time ~90s compile on first boot, a version check after. Only
  # `cargo leptos build` needs it; `|| true` keeps opencode starting offline.
  systemd.services.gavmo-opencode.preStart = ''
    installed="$(${cargoBinDir}/wasm-bindgen --version 2>/dev/null || true)"
    if [ "$installed" != "wasm-bindgen ${wasmBindgenVersion}" ]; then
      HOME=/home/agent \
        ${pkgs.cargo}/bin/cargo install wasm-bindgen-cli \
          --version "${wasmBindgenVersion}" || true
    fi
  '';
}
