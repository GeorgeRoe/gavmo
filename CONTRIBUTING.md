# Contributing to GAVMO

## Setting up

- **In a GAVMO VM** (simplest): open this repo in GAVMO. Its `gavmo.nix`
  installs the Rust toolchain, `cargo-leptos`, `wasm-bindgen-cli`, headless
  Firefox, `qemu` and `virtiofsd`, with cargo settings that fit the VM's
  memory.
- **On a host**: install the Rust version pinned in `rust-toolchain.toml`
  (rustup picks it up automatically), plus `cargo-leptos`, a
  `wasm-bindgen-cli` matching `wasm-bindgen` in `Cargo.lock`, and
  `cargo-deny`. To run the daemon itself, see "Running the daemon locally"
  in [README.md](README.md).

## Before committing

Run the checks listed under "Lints and builds" in [TESTING.md](TESTING.md).
CI runs the same ones on every push to `main`, with clippy warnings as
errors. TESTING.md also explains the test levels and which tests need a
real VM.

- **Lints are strict**: no `unwrap`/`expect`/`panic` outside tests, and
  lint opt-outs need `#[expect(..., reason = "...")]`.
- **Dependencies**: a new or bumped dependency must pass `cargo deny check
  -D warnings`. If it needs an exception in `deny.toml`, give it a
  `reason`, and remove exceptions that no longer match.
- **Rust version**: bump `rust-toolchain.toml` deliberately, in its own
  commit, together with fixes for any new lints. It should match the
  `rustc` that `gavmo.nix` gets from nixpkgs.

## Commits

One logical change per commit. Subjects are short and say what changed,
prefixed with the area when it is one crate or file, e.g.
`vm: pass paths to qemu and virtiofsd as OsString, not text` or
`README: login model, gavmo.yaml tools, and upgrade notes`. Update
README.md and TESTING.md in the same change when behavior or commands
change.

## License

By contributing you agree that your contributions are licensed under the
[MIT License](LICENSE).
