# `gavmo` crate — Daemon + Leptos control panel design (M3)

> This is the design for turning the proven `vm` crate (see `crates/vm/plan.md`,
> implemented and boot-tested on real NixOS hardware) into an actual usable app: a
> localhost web control panel to pick a repo, boot/stop its VM, and jump into
> OpenCode.
>
> **Status: ✅ implemented, and confirmed end-to-end on real NixOS/KVM hardware** —
> folder picker, Open/Stop/Status with live polling, and the §9 "Open UI" deep-link
> task are all built and working. This doc was originally written pre-implementation
> to be self-contained for a fresh session with no prior chat context; it's kept as
> the as-built reference now — see §12 for what shipped vs. this original design.

## 1. What this crate is

A single Rust binary (`crates/gavmo`) that:
- Serves a **Leptos UI with full hydration** (not SSR-only) on `127.0.0.1`.
- Exposes its "API" as **Leptos server functions** (`#[server]`), not hand-rolled axum
  JSON routes — idiomatic Leptos, and removes a whole layer of manual plumbing.
- Drives VM lifecycle via the existing `vm` crate (`VmConfig`, `RunningVm::boot/stop/status`).
- Keeps a small persistent **workspace registry** (known repos) and an in-memory map of
  currently-running VMs.

This is **not** where `gavmo.nix`/per-repo NixOS rebuilding happens — that's M2,
deferred until after this milestone (see `gavmo-mvp-plan.md` §4). For now, every
workspace boots the same fixed base image (`nix/base/`).

## 2. Decisions (all resolved during planning)

| Area | Decision |
|---|---|
| Leptos mode | **Full hydration**, not SSR-only — chosen explicitly over the simpler/lower-risk SSR-only option, because live status updates and a snappier UI were wanted now rather than deferred |
| API layer | **Leptos server functions** (`#[server]`), not a hand-written axum JSON API |
| Crate shape | **Single crate**, feature-gated (`ssr` / `hydrate`), cargo-leptos's standard dual-compile pattern — not a multi-crate split |
| `vm` crate dependency | `ssr`-only, `optional = true` — must never be pulled into the `wasm32` hydrate build (see §5) |
| Status updates | **Live polling** from the client (a Leptos resource/interval), not manual refresh |
| Workspace registry storage | JSON file under a global app dir (e.g. `~/.local/share/gavmo/workspaces.json`) |
| Running VM tracking | **In-memory only** (`Mutex<HashMap<WorkspaceId, RunningVm>>`) — nothing to persist, a stopped VM simply isn't in the map |
| Port allocation | Bind to `127.0.0.1:0` at boot time, take whatever the OS hands back — no persisted port assignments |
| Base image location | **CLI arg / env var** (e.g. `GAVMO_BASE_IMAGE=/path/to/result/nixos.qcow2`) — building the image is still a manual `nix build ./nix/base` step, not automated in this milestone |
| Repo entry (folder picker) | A server function (`browse_dir`) backing a GUI folder browser — **not** a typed path input |

## 3. Toolchain (verified working, in the sidecar, before this was written)

```
cargo-leptos 0.3.11
wasm32-unknown-unknown target: installed
wasm-opt version 108
```

This means the **entire UI half of this crate can be built and iterated on inside the
sidecar** (`cargo leptos build` / `cargo leptos watch`) — no host, no KVM, no QEMU
needed for that. The host is only required for the final step: actually booting a VM
through the running daemon.

## 4. Recommended build process

Rather than hand-typing a `Cargo.toml` with `leptos`/`leptos_axum`/`cargo-leptos`
version pins from memory (a common source of breakage — these three must be mutually
compatible), **ground-truth it first**:

```sh
cd /tmp
cargo leptos new --git leptos-rs/start-axum
# project name: gavmo-scratch (throwaway, just to extract a verified Cargo.toml)
```

Then hand-port the **structure** (not the demo counter-app content) into
`crates/gavmo`:
- The `[features]` block (`ssr` / `hydrate` with their exact dependency gating)
- The `[package.metadata.leptos]` block (`site-root`, `site-addr`, `bin-features`,
  `lib-features`, etc.)
- The `[lib]` `crate-type = ["cdylib", "rlib"]` setup
- The `main.rs` / `lib.rs` split pattern (`#[cfg(feature = "ssr")] fn main()`,
  `#[wasm_bindgen] pub fn hydrate()`)

Add `vm = { path = "../vm", optional = true }` and wire it into the `ssr` feature list
— do **not** add it unconditionally.

## 5. The critical constraint: keep `vm` out of the WASM build

`vm` depends on `tokio::process`, Unix domain sockets, `qapi` — none of it compiles to
`wasm32-unknown-unknown`, nor should it try to. Cargo-leptos compiles this single crate
**twice**:
- Once **natively**, with `bin-features = ["ssr"]`, producing the actual server binary.
- Once for **`wasm32`**, with `lib-features = ["hydrate"]`, producing the browser bundle.

As long as `vm`'s dependency is `optional = true` and only appears in the `ssr`
feature's dependency list (never in `hydrate`'s, never unconditional), cargo-leptos's
own build orchestration guarantees it's never compiled for `wasm32`. This is a
well-trodden pattern (same shape as `axum`/`tokio`/`leptos_axum` themselves, which are
also `ssr`-only in every cargo-leptos template) — just needs the `Cargo.toml` feature
wiring to be correct. **Verify this by actually running `cargo leptos build` in the
sidecar** before considering the scaffold done; don't just trust the Cargo.toml by eye.

## 6. Crate layout

```
crates/gavmo/
  Cargo.toml          # ssr + hydrate features; vm is an ssr-only optional dependency
  src/
    lib.rs            # hydrate() entry point (#[wasm_bindgen]), re-exports
    app.rs            # App shell/router, page components, #[server] functions
                       # co-located with the components that call them (idiomatic
                       # Leptos file organization)
    main.rs           # ssr-only: axum bootstrap - serves the Leptos app, registers
                       # server-fn routes via leptos_axum, reads GAVMO_BASE_IMAGE
    registry.rs        # ssr-only: workspace registry (JSON load/save) + the
                       # Mutex<HashMap<WorkspaceId, RunningVm>> shared app state
  style/
    main.css           # minimal/empty for this slice
```

## 7. Server functions (the whole API surface)

All defined with `#[server]` in `app.rs`, co-located with their calling components.
Under `ssr`, each compiles to the real implementation (touching `registry`/`vm`
directly); under `hydrate`, the **same function signature** compiles to a small
fetch-based stub that calls the server over HTTP — that's the entire point of the
macro, no manual HTTP wiring needed on either side.

| Function | Responsibility |
|---|---|
| `list_workspaces()` | Return all known workspaces + their current status |
| `browse_dir(path: Option<String>)` | Directory listing for the folder picker (default start: `$HOME`) |
| `add_workspace(path: String)` | Register a new workspace at the given host path |
| `open_workspace(id: String)` | Boot (or reuse, if already running) the VM for this workspace; return the OpenCode URL once the health check passes |
| `stop_workspace(id: String)` | Stop the VM, remove it from the in-memory running map |
| `workspace_status(id: String)` | Return current `VmStatus` — called on a client-side polling interval |

Shared server state (the registry + the running-VM map) needs to be reachable from
inside `#[server]` function bodies. The idiomatic Leptos pattern is injecting it as
context during request setup in `main.rs` and retrieving it via
`leptos::context::use_context::<T>()` inside each server function. **Don't trust this
API shape from memory** — `leptos_axum`'s exact context-injection calls have shifted
across versions; verify the correct calls against whatever `leptos`/`leptos_axum`
version `cargo leptos new` resolves to (§4), using real compiler feedback.

## 8. Pages / UI structure

- **`/`** — list of known workspaces (path, status badge that **polls live**), each
  with Open/Stop buttons, plus an "Add workspace" entry point into the folder browser.
- **Folder browser** (could be a modal/panel driven by `browse_dir`, or its own route —
  implementer's call) — navigate directories starting from `$HOME`, select a directory,
  calls `add_workspace`.
- **Workspace row/detail** — Open button calls `open_workspace`; once it resolves,
  surface the returned OpenCode URL (as a link to open in a new tab — GAVMO's own UI
  stays a control panel, it does not embed OpenCode's UI, per the original MVP
  decision).

## 9. "Open UI" deep-link ✅ resolved

Originally a deferred/not-yet-investigated UX task: skip OpenCode's manual "create a
Project → type `/workspace`" step and land the user directly in a usable session
instead of its default first-run screen. **Resolved** — not by probing a live
instance's `/project`/`/project/current`/`/path` endpoints as originally planned, but
by reading opencode's own web UI TypeScript source directly (`packages/app/src/...`
at the pinned opencode version). That confirmed the original hypothesis (project/
session selection is **client-side** browser state, not server state) and revealed
the exact mechanism:

- opencode's web router creates a new session draft for a directory at `/<dir>/session`
  (no session id), where `<dir>` is URL-safe, no-padding base64 of the absolute
  directory path — the same encoding opencode's own `base64Encode` uses
  (`packages/core/src/util/encode.ts`). This is exactly what clicking opencode's own
  "new session" button does under the hood.
- That route's effect (`SessionRoute` in `packages/app/src/app.tsx`) creates a draft
  tab (`tabs.newDraft`) and navigates to `/new-session?draftId=<uuid>` — **the `uuid`
  is minted in the browser and stored in a `localStorage`-backed tab store**, which
  disproves the plan's original `/new-session?draftId=<uuid-we-generate>` idea: a
  server-minted id isn't in the client's tab store, so `DraftRoute` would just bounce
  it to the home screen on a fresh profile. The `/<dir>/session` route is the correct,
  server-constructible entry point; the `draftId` URL is only ever reached by letting
  the client mint it itself.
- The guest always mounts the repo at the fixed path `/workspace`
  (`MOUNT_TAG_WORKSPACE` in `crates/vm/src/handle.rs` + `nix/base/configuration.nix`),
  so the deep-link is fully deterministic: `base64url("/workspace") =
  "L3dvcmtzcGFjZQ"` → path `/L3dvcmtzcGFjZQ/session`.

**Implementation:** `OPENCODE_WORKSPACE_SESSION_PATH` in `crates/gavmo/src/registry.rs`,
appended to the VM's base URL in `to_status()` (where `vm::VmStatus::Running { url }`
maps to the browser-facing `WorkspaceStatus`) — no change needed in `app.rs`, since
"Open UI" just renders whatever URL it's given. A unit test independently re-derives
the base64 constant from `"/workspace"` via a small std-only encoder (deliberately
kept separate from the real constant, so it actually catches drift rather than
restating it), guarding against a future opencode release moving `/workspace`'s
encoding. Verified against opencode v1.18.35 and confirmed working end-to-end on real
hardware: clicking "Open UI" now lands directly in a new-session composer instead of
the first-run project picker.

One accepted tradeoff: unlike the opencode-web-defaults patch (see
`gavmo-mvp-plan.md`'s M1.6), a future opencode release changing this route scheme
won't fail a build — the deep-link would just silently fall back to today's
first-run-screen behavior. Safe-degrading, but worth re-checking after an opencode
version bump if "Open UI" stops deep-linking.

## 10. Explicitly out of scope for this slice

- `gavmo.nix` / per-repo rebuild (M2 — deferred, separate milestone).
- A "Reset" button (stop + delete the VM overlay disk) — Open/Stop/Status only for
  this M3 slice. **Added later**, outside this milestone, as the escape hatch for
  "an overlay's backing file is pinned at creation time (`vm::disk::ensure_overlay`),
  so rebuilding the base image does nothing for a workspace that already has one" —
  see `gavmo-mvp-plan.md`'s "Reset workspace button" entry. Implemented in
  `AppState::reset_workspace` (`crates/gavmo/src/registry.rs`) and the
  `WorkspaceRow` confirm-toggle in `crates/gavmo/src/app.rs`.
- A "Remove" action (forget a workspace without touching its disk state), live-
  status-aware button disabling, and surfaced stop/reset errors — all **added
  later**, outside this milestone, in the same robustness/UX pass as a race-free
  fix for Reset/Stop during an in-flight boot. See `gavmo-mvp-plan.md`'s
  "Robustness/UX polish pass" entry.
- Automating the base-image build (`nix build`) — still a manual step; the daemon only
  consumes an already-built qcow2 path.
- Any non-Linux support.
- Embedding OpenCode's UI inside GAVMO's own UI — GAVMO is a control panel that links
  out to OpenCode's own web UI in a new tab, not a custom chat interface.

## 11. Testing strategy

- **In the sidecar:** the entire UI/server-function layer builds and runs via
  `cargo leptos build` / `cargo leptos watch` — no KVM needed for this.
  `registry.rs`'s `OPENCODE_WORKSPACE_SESSION_PATH` derivation, the build-log
  filename validation, and `reset_workspace`'s idempotent-delete step
  (`remove_dir_all_idempotent`) all have `cargo test` coverage
  (`cargo test -p gavmo --features ssr --no-default-features`). The
  race-free-abort guarantee for `stop_workspace`/`reset_workspace` during a
  `Booting` VM is also covered this way — by spawning a dummy boot task that
  sleeps far longer than any sane test timeout and asserting the call returns
  promptly (`stop_workspace_aborts_an_in_flight_boot_promptly`,
  `reset_workspace_aborts_an_in_flight_boot_before_deleting_state`) — real
  `qemu`/KVM isn't needed to exercise the abort/await mechanism itself.
- **On the real NixOS host — ✅ done:** the full `open_workspace` →
  `vm::RunningVm::boot()` → browser → OpenCode flow, driven through the daemon's UI
  (not just the `crates/vm/examples/boot.rs` CLI example), confirmed working
  end-to-end: folder picker → Open → live status polling → "Open UI" lands directly
  in a new opencode session scoped to `/workspace`.

## 12. As-built notes (deviations from this plan)

Everything in §§1–8 shipped as designed. Two points worth flagging for anyone
comparing this doc against the actual code:

- **`workspace_status` → `poll_status`.** The server function in §7's table was
  implemented as `poll_status(id: String)` — same responsibility (current
  `WorkspaceStatus` for one workspace, called on the client's polling interval), just
  a different name than originally sketched.
- **`open_workspace` is fire-and-forget, not synchronous.** §7 originally implied
  `open_workspace` would "return the OpenCode URL once the health check passes." The
  as-built version kicks off the boot in a background task and returns immediately;
  the client observes the `Booting → Running/Failed` transition (and picks up the
  URL) via the existing live-polled `list_workspaces`. This was a deliberate change
  made during implementation: a synchronous `open` blocked the UI for the whole
  multi-minute first boot with no feedback, which looked broken rather than slow.
  See the `open_workspace` doc comment in `crates/gavmo/src/app.rs`.
- **§9's deep-link mechanism** differs from the plan's original guess (a
  server-minted `/new-session?draftId=<uuid>`) — see §9 above for what actually
  works and why.

Separately, `nix/base/configuration.nix` (outside this crate) grew a complementary
piece of opencode-web UX work in the same arc — patching opencode's own source so
several useful web-UI panels are on by default. See `gavmo-mvp-plan.md`'s **M1.6** for
that; it's not part of this crate's scope but pairs naturally with the §9 deep-link
(both exist to make the opencode web UI a better experience by default).

## 13. Grid redesign + stable-card rendering ✅ done

A visual/structural rework of §8's single-column workspace list, done in two parts:
a rendering-architecture fix that was a prerequisite for the second part, and the
grid/visual restyle itself (inspired by aihero.dev's collapsed-border section/card
technique — see `gavmo-mvp-plan.md` for the fuller writeup of that research).

**Rendering architecture: cards are now mounted once and never torn down by
polling.** The original list rendered via `<Transition>` + `Suspend` over a
`Resource` — on every ~3s poll, the resource resolved to a *brand new* value and
the whole row list was rebuilt from scratch. Any state local to a row (the build-log
viewer, in the old `OpenBuildLog`) had to be lifted up to `HomePage` to survive that
rebuild — a workaround, not a fix, and one that would have had to be repeated for
expand-on-click state and (later) any live view. Replaced with:
- `HomePage` copies each resolved `Resource` value into a persistent
  `RwSignal<Vec<WorkspaceInfo>>` via an `Effect`, and renders via a **keyed
  `<For>`** over just the ids. `<For>` then diffs by key instead of rebuilding
  wholesale — an existing id's `WorkspaceCard` is never unmounted just because
  its status changed.
- Each `WorkspaceCard` derives its own live status via a `Memo` that looks itself
  up by id in that same shared signal — so status/path stay reactive without the
  card itself being recreated.
- Net effect: `expanded`, `busy`, the confirm toggles, and the build-log viewer's
  fetched text are now ordinary signals declared once inside `WorkspaceCard` — no
  more lifting state up to `HomePage`. `OpenBuildLog` is deleted.
- Accepted tradeoff: this list no longer participates in SSR streaming (`Effect`s
  are client-only), so the server-rendered HTML always shows "Loading…" until
  hydration runs once. Not worth avoiding for a localhost-only control panel with
  no SEO/first-paint stakes; see the doc comment on `workspaces` in `app.rs`.

**The `StoredValue<String>` fix for nested `move` closures.** Converting the
detail section into `<Show when=move || expanded.get()>...</Show>` (needed for
expand-on-click) surfaced a real, somewhat surprising Rust borrow-checker wall:
`Show`'s children must implement `Fn` (callable repeatedly, since `when` can flip
back and forth), but several of its descendant reactive closures (the Reset/Remove
confirm toggles, the build-log button) were themselves nested `move` closures each
needing their own owned copy of `id: String`. A `move` closure captures referenced
variables *by value* regardless of whether its body only needs a reference — so an
inner `move` closure referencing `id` moves it out of whatever outer `move` closure
already captured it, leaving the outer closure with nothing for its *next*
invocation (`error[E0525]: ...only implements FnOnce`). No amount of
`let id = id.clone()` immediately before the inner closure fixes this once there
are two or more levels of nesting, because the clone itself is what the inner
`move` closure's capture consumes.

Fix: `WorkspaceCard` now holds `id` as `StoredValue<String>` (`leptos::prelude`),
not a plain `String`. `StoredValue<T>` is `Copy` regardless of `T`, backed by the
reactive runtime's own arena — so *any* number of nested `move` closures can
freely capture the handle itself (trivial, non-destructive, like copying an
integer) and independently call `.get_value()` for a fresh owned clone whenever
they actually run. This incidentally made `open`/`stop`/`reset`/`remove`
themselves `Copy` too (every one of their captures — `id`, the `RwSignal`s,
`Callback`) — removing the `let x = x.clone();` boilerplate those closures
previously needed wherever they were referenced from inside a reactively
re-evaluated branch.

**Visual restyle, aihero-style.** `crates/gavmo/style/main.css` rewritten around:
- A single `.panel` outer frame (one border, rounded, `overflow: hidden`) with
  `.section`s stacked inside sharing hairlines via `.section + .section { border-top
  }` — never a doubled border between adjacent sections.
- `.ws-grid`: the same collapsed-border idea generalized to two dimensions — the
  grid container's own background *is* the border colour, each `.ws-card` is
  opaque, and a `1px` `gap` lets exactly one pixel of the container show between
  cells in every direction.
- Mono/uppercase/tracked treatment for the status `.badge` (aihero's small-stat-
  label look), muted secondary `.path` text under a bold `.name`.
- An expanded `.ws-card.expanded` spans every grid column (`grid-column: 1 / -1`)
  rather than stretching its row — the reason a tall, variable-height detail panel
  (today's actions/build-log, tomorrow's live view) doesn't make the grid ragged.

System font stacks only (`system-ui` / `ui-monospace`) — no fetched webfonts, to
keep this fully offline-capable.
**The live "what's this VM doing" view — ✅ done, in two parts:**

- **Boot console** (`Show systemd boot sequence in UI`): a streaming tail of
  the guest's serial console (`vm::VmConfig::console_log_path`) while
  `Booting`, sanitized server-side (`registry::sanitize_console` strips the
  raw capture's ANSI/VT escapes down to plain text) and polled once a second
  via `read_console_log`. Stays visible through the tail end of the log once
  boot settles, rather than vanishing the instant status flips.
- **Host-side resource stats** (this entry): CPU/memory/uptime/disk/port
  while `Running`, polled every 2s via `poll_vm_stats`
  (`registry::AppState::vm_stats`). The `vm` crate grew a small
  `stats` module (`crates/vm/src/stats.rs`) of pure, unit-tested `/proc`
  parsers — `RunningVm` already owned the qemu `Child` the pid comes from,
  plus a new `started: Instant` (set at spawn, not at "first healthy", so
  first-boot/`gavmo.nix`-rebuild time counts as uptime like a user watching
  the console would expect). CPU is computed top-style
  (`0..vcpus*100`) from two `/proc` samples ~500ms apart
  (`CPU_SAMPLE_INTERVAL`) — taken **after** releasing the `running` map's
  lock, so a stats poll in flight never blocks a concurrent stop/reset. The
  ratio of jiffy deltas cancels out the kernel's tick rate, so no
  `sysconf(_SC_CLK_TCK)`/libc dependency was needed. Memory is shown as the
  qemu process's actual host RSS (not a guest-internal figure) against the
  configured `memory_mib` cap.

Both reuse the same start/stop-polling-on-status-change shape (a
`StoredValue<Option<IntervalHandle>>` + `on_cleanup`), just gated on
different `WorkspaceStatus` variants — confirming the `<Show>`/`StoredValue`
groundwork from the grid redesign did make this a straightforward addition,
as predicted.

## 14. Control-panel session auth ✅ done

**The problem.** The daemon binds its control panel + every `#[server]`
function (`app.rs`) to `127.0.0.1:3000` with no authentication. QEMU's
user-mode networking means a workspace's own guest VM can reach that
listener via `10.0.2.2` exactly as easily as the host's browser can (flagged
as a known, then-unfixed exposure in `crates/mcp/plan.md` §3/§6). Since a
workspace's agent runs with wide-open root inside its VM by design (the VM
*is* the trust boundary, not opencode's own permission system — see
`nix/base/configuration.nix`'s permission comments), a malicious or
prompt-injected agent in any one workspace could call `browse_dir` (reads
anywhere on the host filesystem), `reset_workspace`/`remove_workspace`
(destroys another workspace's VM state), `read_console_log`/`read_build_log`
(reads host files), or `set_tool_secret`/`test_tool` (exfiltrate/probe
stored credentials) — on every workspace GAVMO knows about, not just its
own.

**The constraint that ruled out the easy fixes:** the guest must keep
internet access (Vertex AI, nix substituters, git) and MCP access, both via
the same `10.0.2.2` → host-loopback slirp path — so there's no "block the
guest, keep the internet" QEMU netdev toggle (`restrict=on` kills both), and
slirp's forwarded connections arrive on the host as plain `127.0.0.1`,
indistinguishable from the browser by source address alone. The only real
separator left is a secret the guest can never obtain.

**The fix (`crates/gavmo/src/auth.rs`):** the same bearer-token posture
`crates/mcp` already uses for its own gateway, adapted to a browser
consumer via a session cookie:

- A high-entropy token is minted once and persisted host-only at
  `~/.local/share/gavmo/ui_token` (mode `0600`) — deliberately **outside**
  every 9p share any VM ever mounts (unlike the MCP token, which the guest
  *is* meant to read), so no workspace can ever read it. Persisted (not
  regenerated per boot) so a browser's session cookie survives a daemon
  restart.
- An axum middleware (`require_session`, applied as a `.layer(...)` over the
  whole app router) allowlists only `GET /login` and the static `/pkg/*`
  bundle (`is_public_path`) — everything else, including every `/api/*`
  `#[server]`-function call, requires a `gavmo_session` cookie matching that
  token (constant-time compared). `/api/*` without it gets a bare `401`;
  page routes get redirected to `/login`.
- `GET /login?token=...` is the only place the real token is ever compared
  against user input: a match sets a long-lived (`Max-Age` ~1 year)
  `HttpOnly; SameSite=Strict` cookie and redirects to `/`; anything else
  shows a static fallback page (never echoing the real token).

**Delivery — "completely automatic" without any manual token-copying.**
GAVMO's primary deployment target is a non-technical user running `gavmo` in
a WSL2 Ubuntu shell (Windows + WSL2 Ubuntu is assumed to be
DI-team-approved and simple enough to install). The daemon prints the full
login URL — `http://localhost:3000/login?token=<token>` — to its own
stdout at startup: the exact same terminal the user is already watching
after typing `gavmo`, no separate step to remember. `localhost` (not
`127.0.0.1`) is deliberate: it's what Windows' WSL2 localhost-forwarding
resolves from the host browser's side, and works identically for a native
Linux desktop too. `main.rs` additionally makes a best-effort attempt to
open that URL automatically (`wslview`, falling back to `xdg-open`) — purely
a convenience layered on top of the printed URL, never a requirement; if it
silently fails (headless host, missing opener), the printed URL is still
there. This was chosen over two alternatives considered and rejected:
- *Relying solely on auto-opening a browser with no printed fallback* — too
  fragile for a non-technical audience if the default-browser launch fails
  silently or targets an unexpected browser profile.
- *Binding the UI to a guest-unroutable loopback address (e.g.
  `127.0.0.2`)* — would need zero auth at all, but depends on
  libslirp-version-specific behavior for whether a guest can still reach an
  arbitrary host loopback address directly (not just via the `10.0.2.2`
  gateway alias), which wasn't verified and would need a companion firewall
  rule to be trustworthy anyway — no simpler than the token approach in the
  end, and less clearly robust against a root-capable in-guest adversary.

**Scope:** daemon-only change. No VM image rebuild — the guest-facing MCP
gateway (`127.0.0.1:7071`, its own bearer-token gate) is untouched.

**Testing:** `crates/gavmo/src/auth.rs`'s `#[cfg(test)]` module, run via
`cargo test -p gavmo --features ssr --no-default-features` — pure unit
tests for cookie parsing, constant-time comparison, and the public-path
allowlist, plus `tower::ServiceExt::oneshot` integration-style tests against
a real (minimal) axum `Router` covering: `/api/*` 401s with no/wrong cookie
and passes with the right one, page routes redirect to `/login`, `/login`
itself stays reachable with no cookie, and a correct/incorrect token at
`/login` does/doesn't set the cookie. `tower` was already present
transitively via `axum` (see `Cargo.lock`), so this added it only as a
dev-dependency, not a new crate to the dependency tree.

Not yet verified on real WSL2/NixOS hardware — the next hardware gate
should confirm `localhost:3000` is actually reachable from the Windows
browser via WSL2's localhost-forwarding, and that a fresh guest VM's
`#[server]`-function requests to `10.0.2.2:3000` now get a `401` rather than
succeeding.
