# Testing GAVMO

Tests are layered from pure logic up to a real VM boot. Everything except
the full-boot test runs anywhere, including inside a GAVMO VM.

| Level | Where | Needs | Run by default |
|---|---|---|---|
| Unit | every crate, `#[cfg(test)]` modules | nothing | yes |
| Daemon end-to-end | `crates/gavmo/src/daemon/{tests,mcp_tests}.rs` | nothing | yes |
| Browser | `crates/gavmo/src/app/browser_tests.rs` | headless Firefox, `geckodriver`, `wasm-bindgen-test-runner` | separate command |
| Real qemu | `crates/vm` (`#[ignore]`) | `qemu`, `virtiofsd`, `/dev/kvm` | no |
| Full boot | `crates/gavmo/src/daemon/boot_tests.rs` (`#[ignore]`) | the above plus a built base image | no |

## Unit tests

```sh
cargo test --workspace --exclude gavmo
cargo test -p gavmo --features ssr
```

`gavmo` has no featureless build: always pass `--features ssr` (native) or
`--features hydrate` (wasm).

What they cover, roughly by crate:

- `toolspec`: `gavmo.yaml` parsing and validation, the host-lock
  (`lock_url`) and `canonical_base_url`.
- `secrets`: the vault format (v1 to v2 migration, the `.v1.bak` backup),
  locking, key handling, the in-memory store.
- `mcp`: bearer parsing, and the gateway driven through its tower service
  with a fake authority (401s, scoping, unknown tools, `Origin` refusal).
- `vm`: qemu argument building, disk/overlay handling, QMP, health
  probing, process supervision, using stand-in processes.
- `gavmo`: the registry and lifecycle state machine with a scriptable fake
  launcher (`vm_handle::fake`), sessions and auth, credential binding and
  the HTTP forwarder (against local axum servers), log reading, and UI
  helpers that are plain Rust (e.g. `trim_front`).

## Daemon end-to-end tests

Part of `cargo test -p gavmo --features ssr`. They build the real routers
(`daemon::ui_router`, `daemon::mcp_router`, the same ones `main` serves) over
temp dirs, with the fake VM launcher and `secrets::MemoryStore`, so they
never touch `~/.local/share/gavmo` or the OS keyring.

- `daemon::tests`: logs in, then calls server functions the way the browser
  does (form-encoded, same-origin POSTs). Sessions, CSRF refusal, the
  add/open/stop/reset/remove round trip, typed errors, console reads, tool
  credentials.
- `daemon::mcp_tests`: opens a workspace, reads the minted token from the
  MCP share dir like the guest does, and sends JSON-RPC to the gateway.
  Tools listing, forwarding with the bound credential to a local upstream
  server, credentials withheld after a URL or delivery change, redirects not
  followed, old tokens rejected after stop.

## Browser tests

```sh
cargo test -p gavmo --lib --features hydrate --target wasm32-unknown-unknown
```

Components are mounted into a real DOM in headless Firefox, with real
timers and the real server function client. `.cargo/config.toml` makes
`wasm-bindgen-test-runner` the wasm32 test runner; it finds `firefox` and
`geckodriver` on `PATH` (this repo's `gavmo.nix` provides all three, and
`wasm-bindgen-cli` must match the `wasm-bindgen` version in `Cargo.lock`).

The seam is the browser's `fetch`: each test replaces `globalThis.fetch`
with a Rust handler that answers server function calls by path, so
requests and replies go through the same encoding as in production and no
production code has a test hook. Covered: `use_polling` never overlapping
requests and stopping on stop and unmount, the boot console appending
chunks (and its final fetch), stats clearing when the VM stops, the modal
closing once when its workspace disappears, and a tool row keeping
"Saved" across the list reload.

The plain `#[test]` unit tests are not run in the browser; they already run
natively.

## Real-qemu tests

```sh
cargo test --workspace --exclude gavmo -- --include-ignored
```

A few `vm` tests start real `qemu` and `virtiofsd` (no base image needed;
they check that boot failures are reported promptly and that qemu uses the
explicit backing file rather than the overlay's header). Others run real
`qemu-img` to check that overlay disks are created at the requested size,
grown in place and never shrunk; for the daemon-side version of that, run
`cargo test -p gavmo --features ssr -- --ignored an_existing_disk`. They work inside a
GAVMO VM too, since it has `qemu`, `virtiofsd` and `/dev/kvm`.

## Full-boot test (host only)

Boots a real VM from a real base image through `AppState` and the qemu
launcher: waits for opencode, checks it answers, that the boot console has
output and stats are reported, then stops and resets and checks the VM's
state dir is deleted.

```sh
nix build ./nix/base   # -> result/nixos.qcow2
GAVMO_BASE_IMAGE="$(readlink -f result/nixos.qcow2)" \
  cargo test -p gavmo --features ssr a_real_vm -- --ignored --nocapture
```

The first boot takes a few minutes; the timeout is 15. Everything lives in
temp dirs, and the VM is killed if the test process dies.

## Lints and builds

Every commit should also pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude gavmo --all-targets
cargo clippy -p gavmo --features ssr --all-targets
cargo clippy -p gavmo --lib --features hydrate --target wasm32-unknown-unknown
cargo clippy -p gavmo --lib --tests --features hydrate --target wasm32-unknown-unknown
cargo test -p gavmo --lib --features hydrate --target wasm32-unknown-unknown
cargo leptos build
cargo deny check -D warnings
```

`cargo deny` checks dependencies against `deny.toml`: RustSec advisories,
licenses, duplicate and banned crates, build scripts, and sources. Every
exception there carries its reason; delete one once it no longer matches.

Lints are strict (pedantic, `unwrap_used`, `unreachable_pub`, ...). Tests
are exempt from the panic-family lints through `clippy.toml`.

## GitHub Actions

All three workflows can also be started by hand ("Run workflow").

- **CI** (`ci.yml`, on push to `main`): the checks above, except
  `cargo deny`, on the Rust pinned in `rust-toolchain.toml`.
- **Deny** (`deny.yml`, on push to `main` and daily): `cargo deny`. It is
  the only check whose result changes without a code change, since the
  RustSec database grows daily. A scheduled failure means a new advisory
  affects `main` as it is: usually `cargo update -p <crate>` fixes it.
- **Upcoming** (`upcoming.yml`, weekly): clippy, the native tests and
  `cargo deny` on the latest stable Rust after an uncommitted
  `cargo update`. A failure doesn't mean `main` is broken; it previews what
  the next toolchain or dependency bump will need (new lints, upstream
  breakage, `deny.toml` skips and checksums to refresh).

GitHub emails scheduled-run failures to whoever last changed the `cron`
line, if Actions notifications are enabled in their settings.

## Inside a GAVMO VM

The VM has about 4 GB of RAM and no swap, and two `rustc` processes on the
`gavmo` crate with debug info can exhaust it, which stalls the whole VM.
This repo's `gavmo.nix` sets these for the agent; elsewhere, build with:

```sh
export CARGO_TARGET_DIR=$HOME/.cache/cargo-target  # keep it off /workspace
export CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
```
