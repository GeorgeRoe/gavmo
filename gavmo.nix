# This repo's own `gavmo.nix`: everything the agent needs to build, test, and
# run GAVMO inside its own VM. See README.md ("Running the daemon locally",
# "Customizing your repo's VM with `gavmo.nix`") and TESTING.md; smaller
# recipes live in `examples/`.
#
# Once booted:
#   cargo test -p gavmo --features ssr
#   cargo leptos build

{ pkgs, lib, ... }:

let
  # `wasm-bindgen` CLI and crate must match exactly. The pinned nixpkgs ships
  # the CLI, so the check is just a warning in the rebuild log if a
  # `Cargo.lock` bump ever outpaces it (then pin a CLI build here).
  cargoLock = builtins.fromTOML (builtins.readFile ./Cargo.lock);
  wasmBindgenCrate =
    (lib.findFirst (p: p.name == "wasm-bindgen") { version = "none"; } cargoLock.package).version;
  wasmBindgenCli = pkgs.wasm-bindgen-cli;

  # CI uses the Rust pinned in rust-toolchain.toml; this VM uses nixpkgs'
  # rustc. When they differ, clippy can pass here and fail in CI (or the
  # reverse), so bump the pin to match.
  rustPin = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
  rustVm = pkgs.rustc.version;

  # cargo-llvm-cov reads rustc's coverage profiles with llvm-profdata and
  # llvm-cov, which must come from the same LLVM major version rustc uses.
  # nixpkgs' rustc doesn't ship them (no `llvm-tools-preview`), so take them
  # from the LLVM it was built against. If that attribute ever disappears,
  # fall back to the default LLVM (with a warning) rather than fail the
  # rebuild, since that would keep opencode from starting.
  rustLlvm = pkgs.rustc.llvmPackages.llvm or null;
  llvmTools = if rustLlvm != null then rustLlvm else pkgs.llvm;
in
{
  warnings =
    lib.optional (wasmBindgenCli.version != wasmBindgenCrate) ''
      gavmo.nix: nixpkgs' wasm-bindgen-cli is ${wasmBindgenCli.version} but Cargo.lock
      uses wasm-bindgen ${wasmBindgenCrate}; `cargo leptos build` will refuse to run.
    ''
    ++ lib.optional (rustPin != rustVm) ''
      gavmo.nix: this VM's rustc is ${rustVm} but rust-toolchain.toml pins ${rustPin},
      which CI uses; lints can differ between them. Set the pin to ${rustVm}, in its
      own commit with any new lint fixes (see CONTRIBUTING.md).
    ''
    ++ lib.optional (rustLlvm == null) ''
      gavmo.nix: pkgs.rustc.llvmPackages is gone, so coverage uses the default LLVM
      (${llvmTools.version}); if its major version differs from `rustc -vV`'s,
      `cargo llvm-cov` fails to read the profiles. Find rustc's LLVM another way.
    '';

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
    # Also provides `wasm-bindgen-test-runner` for the browser tests.
    wasmBindgenCli
    # `wasm-opt`, for `cargo leptos build --release`.
    binaryen

    # Headless browser tests (wasm-bindgen-test, see TESTING.md).
    firefox
    geckodriver

    # To run the GAVMO daemon itself.
    qemu
    virtiofsd

    # Test coverage (TESTING.md, "Coverage"). Its LLVM tools are passed
    # through LLVM_COV/LLVM_PROFDATA below rather than put on PATH.
    cargo-llvm-cov

    pkg-config
    openssl
    git
  ];

  # Builds that fit the VM: about 4 GB of RAM and no swap, where two rustc
  # processes on the `gavmo` crate with debug info exhaust memory and stall
  # the whole VM (agent included). The target dir stays on the VM's own disk,
  # off the virtiofs share (which is slow and would put gigabytes in the
  # user's repo).
  environment.sessionVariables = {
    CARGO_TARGET_DIR = "/home/agent/.cache/cargo-target";
    CARGO_BUILD_JOBS = "1";
    CARGO_INCREMENTAL = "0";
    CARGO_PROFILE_DEV_DEBUG = "0";
    CARGO_PROFILE_TEST_DEBUG = "0";

    # For cargo-llvm-cov, which otherwise looks for rustup's llvm-tools.
    LLVM_COV = "${llvmTools}/bin/llvm-cov";
    LLVM_PROFDATA = "${llvmTools}/bin/llvm-profdata";
  };
}
