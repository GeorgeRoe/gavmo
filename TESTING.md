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
  the HTTP forwarder (against local axum servers), log reading, model
  providers and their settings (`registry::models`: save, remove,
  environment overrides, vaulted secrets, the per-boot `opencode.json`,
  against a stand-in Bifrost script whose generated config shows what it
  was given), the gateway test (against local axum servers), the model
  catalog (`providers::models_dev`, `registry::model_catalog`: opencode's
  filtering, the cache, the built-in fallback, failed fetches; read from a
  `file://` copy of models.dev's real Vertex section,
  `providers/testdata/models-dev-vertex.json`, never the network), and UI
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

## Live model gateway test

Starts a real Bifrost through the daemon's supervisor and sends a streamed
request through the `/llm` route to Vertex AI. Needs a `bifrost-http` binary,
network access and Google credentials, so it is opt-in:

```sh
GAVMO_TEST_BIFROST=/path/to/bifrost-http \
GAVMO_TEST_GCP_PROJECT=my-project \
GAVMO_TEST_ADC=~/.config/gcloud/application_default_credentials.json \
  cargo test -p gavmo --features ssr live_bifrost -- --ignored --nocapture
```

Set `GAVMO_TEST_BIFROST_DATA_DIR` to a dir with Bifrost's datasheets to skip
their download.

## Coverage

CI fails if the native tests cover less of the code than the floor in
`ci.yml` (90% of regions, 91% of lines, across all crates together). The
floor is a ratchet just under the current coverage: raise it when tests
are added, and don't lower it to make a change pass; add tests instead.

Regions are the closest stable Rust gets to branch coverage: every
`if`/`match` arm and every `?` error path is its own region, so an
untested branch counts even when its line ran.

```sh
cargo llvm-cov clean --workspace
cargo llvm-cov --no-report --workspace --exclude gavmo -- --include-ignored
cargo llvm-cov --no-report -p gavmo --features ssr -- --include-ignored --skip a_real_vm --skip live_bifrost
cargo llvm-cov report --summary-only --fail-under-regions 90 --fail-under-lines 91
cargo llvm-cov report --html   # -> $CARGO_TARGET_DIR/llvm-cov/html/index.html
```

This includes the real-qemu tests (so it needs `qemu`, `virtiofsd` and
`/dev/kvm`, as in a GAVMO VM) but not the full-boot or live model
gateway tests. This repo's `gavmo.nix` provides `cargo-llvm-cov` and
points it at the LLVM tools of the LLVM that rustc was built with; with
rustup, `rustup component add llvm-tools-preview` does the same.

cargo-llvm-cov leaves test files out of the count by default (`tests.rs`,
`*_tests.rs`, `tests/`), but inline `#[cfg(test)] mod tests` blocks are
counted along with the code they test. The browser tests aren't measured
(llvm-cov can't instrument wasm on stable Rust), so components are also
covered natively by rendering them server-side, which runs the code that
builds their views; only event handlers and effects stay browser-only.
On a CI failure, the `coverage-html` artifact shows which lines are missed.

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

Every workflow can also be started by hand ("Run workflow").

- **CI** (`ci.yml`, on push to `main`): the checks above, except
  `cargo deny`, on the Rust pinned in `rust-toolchain.toml`, plus the
  coverage floor (see "Coverage"). The coverage job installs qemu from apt,
  opens up `/dev/kvm`, and takes virtiofsd from the nixpkgs revision that
  `nix/base/flake.lock` pins (Ubuntu's is older than the 1.13 GAVMO needs),
  so the real-qemu tests run there too. The `release-tarball` job builds the
  release tarball (`scripts/build-release-tarball.sh`: a static musl daemon
  plus the site assets) and keeps it as an artifact for a week.
- **Nix** (`nix.yml`, on push to `main`): `nix flake check`, then builds
  `.#gavmo-from-source` (daemon, Bifrost, base image and the wrapper that
  puts qemu and virtiofsd on its `PATH`) in Nix's sandbox and runs
  `scripts/smoke-test-package.sh` on it: version, startup with only the
  wrapper's defaults, login, serving the wasm bundle, clean shutdown. No VM
  is booted. Slow (it builds opencode and the image), so expect most of an
  hour.
- **Release** (`release.yml`, by hand only, from `main`): bump the
  workspace `version` in `Cargo.toml` (it must stay `0.y.z` for now), push,
  then run it with that version. It builds the tarball and the
  zstd-compressed base image, writes `release.json` (their URLs and hashes,
  which switch the flake's default package to the prebuilt release),
  installs and smoke-tests `.#gavmo-bin` from those files, pushes a
  `release: vX.Y.Z` commit to `main` and publishes the GitHub release, tagged
  at that commit. It needs to push to `main`: if branch protection forbids
  `github-actions[bot]` that, allow it or the push step fails (the draft
  release is then deleted, and nothing is published). If `main` moved during
  the run, just run it again.
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

Every `uses:` is pinned to a full commit SHA, with the release tag in a
comment, because tags can be moved to other code. To update one, resolve
the new release tag and replace both, e.g.
`git ls-remote https://github.com/actions/checkout 'refs/tags/v4.4.0^{}'`
(drop `^{}` if it prints nothing). Update every file that uses the action.
The tool versions passed to `taiki-e/install-action` must be in that
release's manifests, or it falls back to a slower install.

## Inside a GAVMO VM

The VM has about 4 GB of RAM and no swap, and two `rustc` processes on the
`gavmo` crate with debug info can exhaust it, which stalls the whole VM.
This repo's `gavmo.nix` sets these for the agent; elsewhere, build with:

```sh
export CARGO_TARGET_DIR=$HOME/.cache/cargo-target  # keep it off /workspace
export CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
```
