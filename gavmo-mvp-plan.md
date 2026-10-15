# GAVMO — MVP Plan

> **GAVMO**: George's Agent Virtual Machine Orchestrator.
> A local-first tool that runs OpenCode in an isolated NixOS VM per repository.
> This plan is the ruthlessly-scoped MVP, derived from (and much smaller than)
> `gavmo-chatgpt-plan.md`.

## 0. Status

| Milestone | What it proves | Status |
|---|---|---|
| **M0** — plumbing proof | QEMU/KVM + 9p shares + port-forward + health-check + QMP stop, all working end to end on real NixOS hardware | ✅ Done |
| **M1** — real OpenCode + Vertex | Real `opencode web` in the VM, authenticated to Vertex, editing live host files both directions | ✅ Done |
| **M1.5** — permission/git polish | Frictionless agent (no prompts), git works over 9p | ✅ Done |
| **M1.6** — opencode-web UI defaults | Patches opencode's own source at image-build time so useful web-UI panels (file tree, command palette, etc.) are on by default | ✅ Done |
| **M3** — daemon + Leptos control panel | Turns the proven `vm` crate into an actual usable app | ✅ Done — see `crates/gavmo/plan.md` |
| **M2** — `gavmo.nix` rebuild-on-open | Per-repo NixOS config applied via `nixos-rebuild switch` at boot | 🔜 Next |

See `crates/vm/plan.md` for the VM-management crate's design (implemented, boot-tested).
See `crates/gavmo/plan.md` for the daemon + control-panel design (M3, ✅ implemented —
see its §12 for as-built notes/deviations).

## 1. What the MVP is (scope lock)

A **Linux-only, solo proof-of-concept** that proves one loop end-to-end:

> From a localhost web UI, pick a repo → GAVMO boots a NixOS QEMU VM → `opencode web`
> opens, authenticated to Vertex → the agent edits files that are live on the host →
> **multiple repos can run concurrently**.

**In scope:**

- Rust daemon (single binary) + Leptos control-panel UI (full hydration).
- QEMU/KVM VM per workspace (system-installed QEMU).
- NixOS base image, built manually via `nix build ./nix/base`.
- 9p `/workspace` (live host repo dir, read-write) and `/etc/gavmo/gcloud` (read-only ADC).
- Vertex AI via mounted ADC credentials.
- `opencode web` port-forwarded to the host.
- Lifecycle: Open / Stop / Status / Reset / Remove (with live status polling in the UI).
- Multiple concurrent workspaces.
- Per-workspace state: VM overlay disk under the repo's `.gavmo/` (gitignored).

**Explicitly deferred (NOT in this MVP slice):**

- Per-repo `gavmo.nix` rebuild-on-open (**M2** — valuable, next up now that M3 is
  done; see §4).
- Jira / secrets / MCP tool gateway.
- Windows and cross-platform support.
- QEMU bundling / installer / code signing / auto-update.
- Host-side token-proxy auth (keeping creds fully out of the VM) — creds are mounted
  read-only into the VM instead; accepted risk (see §6).
- DAFNAI's git-attribution and home-dir refusal.
- Committing a shared nix cache to the repo (the persistent overlay disk gives cache
  reuse for free without this).
- Any custom chat UI (we use OpenCode's own `opencode web`).
- `OPENCODE_SERVER_PASSWORD` (dropped — see §2, port is `127.0.0.1`-only).

## 2. Context & decisions

| Decision | Choice |
|---|---|
| Audience | Solo PoC first; org-internal later |
| Platform | Linux x86-64 only |
| Isolation | QEMU/KVM VM from the start (the whole point of GAVMO) |
| North-star goal | Non-technical ease of use (design goal, not MVP acceptance gate) |
| Guest OS | NixOS |
| Base image source | Pre-built `qcow2` via `nixos-generators` (manual `nix build`), daemon points at it |
| QEMU | System-installed `qemu-system-x86_64` + KVM |
| Dir sharing | **9p** (`virtio-9p-pci`), not virtiofs — see rationale below |
| Model auth | Vertex AI via mounted ADC file (read-only 9p), like DAFNAI |
| Agent UI | OpenCode's own `opencode web` (confirmed to exist and work) |
| OpenCode state | VM's own overlay disk (`HOME=/root`), **not** the 9p share — SQLite needs it |
| OpenCode permissions | Baked `"*": "allow"` — the VM is the real trust boundary, not per-tool prompts |
| GAVMO UI | Leptos, **full hydration** (not SSR-only) |
| UI delivery | Localhost web app (axum + Leptos), not Tauri |
| Repo entry | Folder picker in the GUI (server function, not a typed path) |
| Concurrency | Multiple concurrent VMs |
| Lifecycle buttons | Open, Stop, Status (status auto-polls live) |
| State location | VM overlay disk under the repo's `.gavmo/vm/` (gitignored) |
| Workspace identity | Absolute host path of the repo |
| OpenCode password | None — forwarded port is bound to `127.0.0.1` only, local-only is sufficient |

### Why 9p, not virtiofs (changed from the original plan)

The original plan assumed virtiofs. Researched directly: `virtiofsd` is a FUSE-based
daemon that only runs on a **Linux host** (FUSE doesn't exist on Windows), so virtiofs
can never be the one cross-platform mechanism GAVMO wants. `virtio-9p` is built directly
into QEMU and works identically on Windows and Linux hosts with zero extra processes.
9p is the portable default; virtiofs would only ever be a Linux-only optimization added
later behind the same `ShareBackend` trait in the `vm` crate — not needed yet.

### Why OpenCode state lives on the overlay disk, not the 9p share

Tried first: OpenCode's XDG dirs pointed at `/workspace/.gavmo/local` (host-visible,
survives a VM reset, matching DAFNAI's layout). It failed hard: OpenCode's SQLite
session database threw `disk I/O error` (`SQLITE_IOERR`) on every startup — 9p's
locking/IO semantics don't satisfy SQLite, even with `cache=none`. Switching
`HOME=/root` (native ext4 on the overlay disk) fixed it immediately. Tradeoff: session
history persists across VM stop/restart (the overlay persists) but is wiped on a full
VM reset, and isn't visible from the host. The project files in `/workspace` are
unaffected — only OpenCode's internal DB moved.

### Why OpenCode permissions are wide open

Initially baked a DAFNAI-style mixed policy (allow `edit`/`bash`, leave other
categories like `external_directory` to prompt). This produced inconsistent UX (e.g.
"tell me about the host" triggered an `external_directory` prompt for `/etc`, `/proc`)
and, on reflection, doesn't add real protection: with `bash` allowed, any denied
category is trivially bypassable via a shell command (a denied `external_directory`
read is just `cat <path>` through bash). The VM is the actual security boundary, not
OpenCode's per-tool permission prompts — so the config is a top-level `"*": "allow"`,
with git remote operations denied as a harmless tripwire (bypassable too, but there are
no git credentials in the VM for them to work anyway).

## 3. Architecture

```
Browser (localhost)
  ├── GAVMO Leptos UI (hydrated) ──HTTP/server-fns──┐
  └── OpenCode web UI (per-workspace forwarded port, opened separately)
                                                     ▼
                                     ┌───────────────────────┐
                                     │ GAVMO daemon (Rust)   │  axum + Leptos (ssr+hydrate), one binary
                                     │  workspace registry   │  JSON file, global app dir
                                     │  VM manager (`vm` crate) │ spawns qemu, 9p shares
                                     │  port allocator       │  bind :0, take what the OS gives
                                     │  health poller        │
                                     └──────────┬────────────┘
                  per workspace: qemu-system-x86_64 (KVM)
                                                 ▼
                                     ┌───────────────────────┐
                                     │ NixOS VM              │
                                     │  base image (ro) +    │  qcow2 overlay in .gavmo/vm/ (persists /nix, OpenCode state)
                                     │  opencode web :7080   │  bound to guest 0.0.0.0, HOME=/root
                                     │  /workspace (9p, rw)  │  <- host repo dir, live, cache=none
                                     │  /etc/gavmo/gcloud (9p, ro) │ <- ~/.config/gcloud, read-only
                                     └───────────────────────┘
```

**Key mechanics (as implemented in `crates/vm` + `nix/base` + `crates/gavmo`):**

- **One binary** (implemented, M3 — see `crates/gavmo/plan.md`). axum serves both
  Leptos server functions and the hydrated UI on `127.0.0.1`.
- **Per-workspace QEMU VM** using system `qemu-system-x86_64` + `-enable-kvm -cpu host`
  (Linux-only, cfg-gated in `vm::qemu`).
- **Networking.** QEMU user-mode NAT (`-netdev user`) gives outbound access (Vertex,
  nix substituters, git) with zero host setup; `hostfwd` maps a unique host port →
  the guest's fixed `opencode web` port (`7080`, `GUEST_OPENCODE_PORT` in
  `crates/vm/src/config.rs`).
- **`/workspace` and gcloud creds.** Both 9p (`virtio-9p-pci`), `cache=none`,
  `security_model=mapped-xattr`. Workspace is read-write; gcloud is read-only.
- **Vertex auth.** `GOOGLE_APPLICATION_CREDENTIALS` points at the 9p-mounted ADC file;
  `GOOGLE_CLOUD_PROJECT` + `VERTEX_LOCATION=global` set on the `gavmo-opencode`
  systemd service. Managed `/etc/opencode/opencode.json` (non-overridable by any repo)
  pins the provider to `google-vertex-anthropic` and whitelists 4 Claude models —
  copied from DAFNAI's config.
- **Persistence / caching.** The per-workspace root disk is a **persistent qcow2
  overlay** on the read-only base image, stored in `.gavmo/vm/`. Because it persists,
  `/nix` and OpenCode's own state survive restarts → cache reuse "for free" with
  nothing committed to the repo.
- **Guest control.** A systemd service (`gavmo-opencode`) mounts the 9p shares (via
  NixOS `fileSystems`), then runs `opencode web --hostname 0.0.0.0 --port 7080`. No
  SSH, no custom guest agent. Logs route to `journal+console` so the serial console
  (`crates/vm`'s `console.log`) shows real startup errors — essential for debugging
  (this is what caught the SQLite-over-9p issue).
- **Agent awareness.** `/etc/gavmo/AGENTS.md` (baked into the image, referenced via
  OpenCode's `instructions`) tells the agent only that it runs in a GAVMO-managed VM
  with the project at `/workspace` — nothing about the host/VM plumbing underneath.
- **OpenCode web UI defaults (M1.6).** Several opencode-web toggles (File tree,
  Command palette, Server status, Show agent, Show reasoning summaries, Expand
  shell/edit tool parts) ship off by default upstream with no config-file
  equivalent — they're pure browser `localStorage`. `nix/base/configuration.nix`
  patches opencode's own TypeScript source (`pkgs.opencode.overrideAttrs` +
  `postPatch`, before nixpkgs' vite build runs) to flip these defaults on, rather
  than patching the compiled/minified output — minified names churn between
  opencode releases (confirmed: they differ between 1.18.31 and 1.18.35), while
  readable source identifiers are comparatively stable. Every substitution asserts
  its target string occurs exactly once before touching anything, so a future
  opencode bump that moves/renames these fails the image build loudly (naming the
  exact string to fix) instead of silently shipping the toggles off.
- **"Open UI" deep-link (M3).** Clicking "Open UI" lands directly in a new opencode
  session scoped to `/workspace`, instead of opencode's first-run "create a
  project" screen. `crates/gavmo/src/registry.rs` appends a deep-link path
  (`base64url("/workspace") + "/session"`) to the VM's URL when reporting status —
  see `crates/gavmo/plan.md` §9 for the full mechanism and why the originally
  planned `draftId`-based URL doesn't work.
- **State layout.** VM overlay disk + QMP socket + console log under the repo's
  `.gavmo/vm/` (gitignored). OpenCode's own session state is on the VM's native overlay
  disk, not under `.gavmo/` (see §2).

## 4. Build order (de-risk first) — actual milestones

The original generic "Phase 0–3" plan has been superseded by concrete milestones as
the project actually progressed:

### M0 — plumbing proof ✅ (`nix/base/` + `crates/vm/examples/boot.rs`)

A deliberately dumb image (Python `http.server` listing `/workspace`) proved: QEMU
boots under KVM, both 9p shares work, the host port-forward reaches the guest, the
health-check passes, and QMP graceful shutdown works — the entire `vm` crate, boot-
tested on real NixOS hardware.

### M1 — real OpenCode + Vertex ✅

Swapped the dummy server for `opencode web`, baked the managed OpenCode config and
Vertex env vars. Hit and fixed two real issues along the way (both now documented in
`nix/base/configuration.nix`'s comments):
- `network-online.target` could stall the service; reverted to `network.target`.
- `opencode web`'s browser-auto-open needed a no-op `xdg-open` shim.
- OpenCode's SQLite session DB doesn't work over 9p (`disk I/O error`) — state moved to
  the VM's native overlay disk (`HOME=/root`).

Confirmed end to end: real model responses via Vertex, and bidirectional file sync
(host↔guest, both directions) over 9p.

### M1.5 — permission/git polish ✅

Baked a top-level `"*": "allow"` OpenCode permission policy (removes all prompts; see
§2 for why per-category denies don't add real protection once `bash` is open), plus
`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/`GIT_CONFIG_VALUE_0` env vars forcing
`safe.directory=*` so git doesn't choke on 9p ownership.

### M1.6 — opencode-web UI defaults ✅

Several opencode-web UI toggles (File tree, Command palette, Server status, Show
agent, Show reasoning summaries, Expand shell tool parts, Expand edit tool parts) ship
off by default upstream, with **no config-file equivalent** — they're pure browser
`localStorage` (key `settings.v3`), seeded from a `defaultSettings.general` object in
opencode's own TypeScript source.

`nix/base/configuration.nix` patches that source directly at image-build time
(`pkgs.opencode.overrideAttrs` + `postPatch`, running *before* nixpkgs' vite build)
rather than the compiled/minified output: the minified property names churn between
opencode releases (confirmed differing between 1.18.31 and 1.18.35 — an earlier,
binary-patching version of this fix failed a rebuild for exactly that reason), while
the readable source identifiers are comparatively stable. Every substitution asserts
its target string occurs **exactly once** before touching anything, so a future
opencode bump that moves/renames these fails the image build loudly (naming the exact
string to fix) instead of silently shipping the toggles off.

"Show agent" needed one extra patch beyond the plain default flip: opencode has a
one-time, client-side auto-init effect that recomputes and overwrites it on a fresh
browser profile's first load, regardless of the default — that initializer's
"undecided" branch is forced to always resolve "on" too.

### M3 — daemon + Leptos control panel ✅ done

Turned the proven `vm` crate into an actual usable app: folder picker, Open/Stop/Status
with live polling, multiple concurrent workspaces — confirmed end to end on real
NixOS/KVM hardware. Full design, plus as-built notes on where the implementation
deviated from this plan, in **`crates/gavmo/plan.md`** (see its §12).

The UX task below (originally filed as "deferred, folds into M3") was resolved as
part of this milestone — see `crates/gavmo/plan.md` §9 for the full mechanism; this
entry just records that it's done.

**"Open UI" deep-link ✅ resolved** (was: "Deferred UX task — needs investigation").
Clicking "Open UI" now lands the user directly in a usable opencode session instead of
its manual "create a Project → type `/workspace`" first-run screen. Resolved by
reading opencode's own web UI source rather than probing a live instance's
`/project`/`/project/current`/`/path` endpoints as originally planned: project/session
selection turned out to be entirely client-side browser state, and the originally
guessed `/new-session?draftId=<uuid-we-generate>` deep-link doesn't actually work (that
`draftId` is a `uuid()` minted and stored client-side; a server-minted one just bounces
to the home screen on a fresh profile). The real mechanism — hitting
`/<base64url(dir)>/session` to trigger the same draft-creation opencode's own "new
session" button uses — is deterministic for the guest's fixed `/workspace` mount
(`base64url("/workspace") = "L3dvcmtzcGFjZQ"`) and is implemented in
`crates/gavmo/src/registry.rs`.

### M2 — per-repo `gavmo.nix` rebuild-on-open ✅ mechanism implemented

The idea: on boot, `nixos-rebuild test` the VM using a `gavmo.nix` module from the
repo, merged on top of the base config — "the repo's config *is* the VM." Was
deliberately **deferred until after M3** (now done — see above) because:
- It's the most technically uncertain remaining piece (our base image is flake-built
  via `nixos-generators`, which has no traditional `/etc/nixos`/channels setup — making
  `nixos-rebuild` work *inside* the running guest against a flake-built system
  is unproven and will likely need its own debugging cycle, similar to M1's).
- The "surface a building/rebuilding state" UX it needs is naturally built *into* the
  control panel from M3 — having M3 done first gives M2 a better home to land in,
  rather than redoing UI work.
- M3 shipped fully useful without it: the daemon just boots the same fixed base image
  for every workspace until M2 lands.

**Repo-facing contract (decided):** `gavmo.nix` only — no `flake.nix`/devShell
support. In a dedicated per-repo VM there's no multi-project contention to isolate
a devShell against, so `environment.systemPackages` in `gavmo.nix` covers project
tooling just as well, while also covering things a devShell structurally cannot
(Docker/system daemons, kernel modules, sysctls, the system CA trust store). devShell
support can be added later cheaply if it ever proves worth it.

**Status: ✅ spike validated end-to-end on real NixOS/KVM hardware, mechanism
implemented in `nix/base/`.** See `nix/spike-m2.md` (§9) for the full recorded
results. Summary:

- An in-guest `nixos-rebuild test --flake /etc/gavmo/system#gavmo --impure` merges
  the repo's `/workspace/gavmo.nix` (if present) onto the base config via the
  NixOS module system — `nixosConfigurations.gavmo` in `nix/base/flake.nix`, sharing
  `commonModules` with the image build so the two can never silently diverge.
  `nix/base/disk.nix` replicates the root-fs/bootloader subset that
  `nixos-generators`' qcow format module injects at image-build time, which the
  standalone rebuild target needs supplied manually — this was the single biggest
  predicted risk and **it built clean on the first attempt** in the spike.
- `test`, not `switch`: activates immediately without ever touching the bootloader
  or default boot entry — confirmed in the spike to not even restart
  `gavmo-opencode`, let alone wedge the guest.
- The whole flake (pinned to the image's own nixpkgs rev) is baked into the image
  at `/etc/gavmo/system`, so the in-guest rebuild reuses the guest's warm store.
  Measured: a no-op rebuild took **2.3s**; a `gavmo.nix` adding a brand-new systemd
  service (Docker) took **under 30s total**, well inside the existing
  `BOOT_HEALTH_TIMEOUT` (5 min) — no timeout change was needed.
- A new `gavmo-rebuild.service` (`nix/base/configuration.nix`) runs this before
  `gavmo-opencode`, gated by `ConditionPathExists=/workspace/gavmo.nix` (so repos
  without one see it cleanly skipped, zero behavior change). Waits on
  `network-online.target` (not just `network.target`) — found during
  end-to-end testing of the *baked* image that a `gavmo.nix` fetching new
  packages (e.g. Docker) can run before DHCP completes otherwise.
- **Failure policy is strict, by design.** `gavmo-opencode` both `after`s AND
  `requires`s `gavmo-rebuild.service` — a broken `gavmo.nix` means opencode
  **never starts**, rather than silently falling back to the base config and
  letting the agent run with tools it believes exist but doesn't. (The debug
  console's getty units are also ordered, `after`-only, behind
  `gavmo-rebuild` — so a manual QEMU session doesn't drop you into a shell
  mid-rebuild either; this is purely a manual-debugging nicety, not part of
  the production mechanism, which has no interactive console at all.)
- **Repo-facing contract is `gavmo.nix` only** (no devShell support) — decided
  before the spike; see above. Proven in the spike with a sample module adding
  `pkgs.jq` and `virtualisation.docker.enable`, including a working
  `docker run hello-world` — the concrete case a devShell could never cover.
- **Fast, UI-visible build-failure reporting — done** (see below): a broken
  `gavmo.nix` surfaces in the control panel within seconds, with the real Nix
  error one click away, rather than as a generic 5-minute boot timeout.

### Fast build-failure detection + in-UI log viewer ✅ done

Originally filed as a follow-up: with strict failure, a broken `gavmo.nix`
meant opencode never started, but the host had no way to know *why* — it just
saw `:7080` never come up and waited out the full `BOOT_HEALTH_TIMEOUT` (5
min), with the real Nix error sitting unreachable in the guest's journal
(console logging had just been turned off for being pure clutter — see M1.6-
era reasoning above). Resolved:

- **Guest side** (`gavmo-rebuild.service`): every run `tee`s its full
  `nixos-rebuild` output to a timestamped, host-visible log —
  `/workspace/.gavmo/vm/rebuild-<UTC timestamp>.log` (colon-safe ISO,
  sortable) — since `/workspace/.gavmo/vm` is exactly `VmConfig::state_dir`,
  the same place the host daemon already keeps the disk overlay/QMP
  socket/console log. On success, all `rebuild-*.log` files and the marker
  below are deleted (a success makes any prior failure moot — the dir only
  ever holds logs for an *unresolved* failure). On failure, the log is kept
  and a `rebuild-failed` marker file is written containing its filename.
- **`vm` crate:** `RunningVm::boot` clears any stale marker before spawning
  qemu, then races opencode's port answering against the marker appearing
  (`health::wait_for_ready`) — a broken `gavmo.nix` now fails in **seconds**,
  not 5 minutes, surfaced as a new `Error::RebuildFailed { log_file }`.
- **Daemon** (`crates/gavmo`): a new `VmSlot`/`WorkspaceStatus::BuildFailed { log }`
  and a `read_build_log(id, log)` server function (strict filename validation
  — the argument is fully client-controlled and becomes part of a filesystem
  path) feed an inline, expandable build-log viewer in the control panel
  ("View build log" button on the workspace row).
- Verified: `cargo test -p vm`, `cargo test -p gavmo --features ssr
  --no-default-features`, and `cargo leptos build` (both the wasm32 hydrate
  and native ssr targets) all pass in the sidecar.

### Reset workspace button ✅ done

A fourth lifecycle action, alongside Open/Stop/Status: stops the VM (if running)
and deletes its entire `.gavmo/vm` state dir (overlay disk, console log, QMP
socket, any M2 rebuild log/marker). The next Open recreates a fresh overlay from
the **current** base image.

Added as the general escape hatch for a wedged workspace, and specifically to
close a gap M2 opened: an overlay's backing file is fixed at creation time
(`vm::disk::ensure_overlay`), so rebuilding the base image (`nix build
./nix/base`) does nothing for a workspace that already has one. Without Reset,
the only fix was deleting `.gavmo/vm` by hand on the host.

- **Destructive, confirmed inline.** `AppState::reset_workspace`
  (`crates/gavmo/src/registry.rs`) does not prompt itself; the `WorkspaceRow` UI
  gates it behind a two-step "Reset → Delete all VM state? Confirm/Cancel"
  toggle before calling the `#[server] reset_workspace` function.
  `is_safe_rebuild_log_filename`-style free functions keep the destructive bits
  (`remove_dir_all_idempotent`) unit-testable independent of `AppState`.
- Paired with an M2 hardening fix found at the same time: Nix's own
  fetcher/eval-cache SQLite DBs under `/root/.cache/nix` run with
  `synchronous = off` (not crash-safe) and previously lived on the persistent
  overlay — an abrupt VM kill mid-rebuild (including Reset's own `vm.stop()`,
  or the M2 fast-fail path) could leave one corrupted, poisoning every later
  rebuild attempt even after fixing the `gavmo.nix` that caused the original
  failure. Fixed by mounting `/root/.cache/nix` as `tmpfs` (wiped every boot) —
  see `nix/spike-m2.md` §10 finding 3 and `nix/base/configuration.nix`.

### Robustness/UX polish pass ✅ done

A follow-up pass on the control panel, after dogfooding surfaced both a real race
condition and some rough UX edges. All in `crates/gavmo/src/registry.rs` /
`crates/gavmo/src/app.rs` / `crates/gavmo/style/main.css`:

- **Race-free Reset/Stop during a `Booting` VM.** `VmSlot::Booting` previously
  carried no handle, so reset/stop could only remove the in-memory marker and
  rely on the in-flight boot task to *eventually* notice and clean up itself —
  reset in particular could delete `.gavmo/vm` while that cleanup (or the boot
  itself) was still touching the overlay. `VmSlot::Booting` now carries the
  boot task's own `tokio::task::JoinHandle`; `stop_workspace`/`reset_workspace`
  `abort()` it and `await` the result, so qemu is *guaranteed* gone (via
  `kill_on_drop` on an aborted boot, or the task's own graceful self-stop if it
  won the race) before either function returns. Covered by two tests
  (`stop_workspace_aborts_an_in_flight_boot_promptly`,
  `reset_workspace_aborts_an_in_flight_boot_before_deleting_state`) that spawn
  a deliberately never-finishing dummy boot task and assert the call returns
  well within a bounded timeout rather than hanging. Accepted tradeoff: `stop`
  can now rarely block up to `SHUTDOWN_TIMEOUT` + `QUIT_TIMEOUT` (~35s) if it
  races a boot that wins and completes a graceful shutdown instead of being
  cleanly aborted — judged better than the previous instant-but-not-actually-
  stopped-yet return.
- **Remove a workspace.** No way previously existed to un-add a mistaken
  workspace. `AppState::remove_workspace` stops the VM (reusing the now
  race-free `stop_workspace`) and drops it from the registry —
  **non-destructive**, unlike Reset: `.gavmo/vm` is left untouched, so
  re-adding the same path later finds its old overlay/session history again.
  Same inline two-step confirm UI pattern as Reset (its own
  `confirming_remove` signal; opening one confirm closes the other).
  Deliberately not unit-tested directly: unlike `reset_workspace`, it calls
  `save_registry`, which would write the real shared
  `~/.local/share/gavmo/workspaces.json` from a test run.
- **Buttons follow live status, not just a local busy flag.** Open/Stop/Reset
  used to only disable for the ~couple-hundred-ms of their own fire-and-forget
  call, leaving all three clickable throughout an entire multi-minute boot —
  which was also how a user could reach Reset while `Booting` and hit the race
  above. Disabled state now also keys off the polled `ws.status` (Open: off
  while `Booting`/`Running`; Stop: off unless one of those; Reset: off while
  `Booting`, belt-and-suspenders with the fix above).
- **Stop/Reset/Remove failures are visible.** Previously only `Open`'s failure
  reached the UI; a failed `stop`/`reset` just logged to the browser console.
  Generalized into one shared `action_error` per row.
- **Workspace rows show a name, not just a path.** The repo basename is now
  the bold primary label, with the full absolute path as muted secondary
  text underneath (full path kept — it's still the workspace's real identity,
  see `WorkspaceRecord`).
- **Folder picker accepts a typed path.** The click-through browser hides
  dotfiles/dirs and had no way to jump straight to a known location; a text
  input + "Go" (plus Enter) now sets the browser's current directory directly,
  reusing `browse_dir`'s existing canonicalization/error handling.
- **A boot-in-progress hint.** A static note under a `Booting` row ("first
  boot can take a few minutes…") — deliberately static rather than a live
  elapsed-time counter, since rows are recreated wholesale on every ~3s poll
  (see `OpenBuildLog`'s doc comment in `app.rs`) and a real timer would need
  the same kind of state-lifted-to-`HomePage` treatment that required.

  **Superseded by the grid redesign immediately below** — the "rows are
  recreated wholesale" constraint this bullet (and `OpenBuildLog`) worked
  around no longer exists; `WorkspaceCard`s are now mounted once and never
  torn down by polling, so this tradeoff doesn't apply to the hint, or
  anything else, anymore.

### Grid redesign + stable-card rendering ✅ done

Replaced the single-column workspace list with a grid of compact, expand-on-click
`WorkspaceCard`s, visually restyled around aihero.dev's collapsed-border
section/grid technique (a dark, info-dense control panel borrowing that site's
*border/typography language*, not its marketing-page layout). Full technical
writeup — the rendering-architecture fix this required (stable, poll-surviving
cards via a keyed `<For>` + per-card `Memo`), the `StoredValue<String>` fix for a
genuine Rust borrow-checker wall hit along the way, and the CSS technique itself —
lives in `crates/gavmo/plan.md` §13.

In short: `OpenBuildLog`'s lift-state-to-`HomePage` workaround is gone (no longer
needed — cards survive polling on their own now), the list is now a responsive
grid (`repeat(auto-fill, minmax(260px, 1fr))`) with single-pixel hairlines between
cards, and an expanded card spans the full grid width to host its (taller,
variable-height) detail panel without making the grid ragged.

**Deliberately deferred to a follow-up phase:** a live "what's this VM doing" view
inside an expanded card — streaming the guest's boot console while `Booting`,
host-side CPU/mem/uptime stats while `Running`. The stable per-card architecture
above was built with this in mind (each `WorkspaceCard` is now a natural, stable
place to poll its own live data) but the feature itself isn't built yet.

## 5. Actual layout (as of M3)

```
gavmo/                        # repo root
  Cargo.toml                   # [workspace], members = ["crates/*"]
  crates/
    vm/                        # IMPLEMENTED — see crates/vm/plan.md
      Cargo.toml
      plan.md
      src/{lib,config,share,disk,qemu,qmp,health,handle,error}.rs
      examples/boot.rs         # throwaway runner: cargo run -p vm --example boot
    gavmo/                     # IMPLEMENTED — see crates/gavmo/plan.md (M3, §12 for as-built notes)
      Cargo.toml
      plan.md
      style/main.css
      src/
        lib.rs                  # hydrate() entry point, module wiring
        main.rs                 # ssr-only: axum bootstrap, reads GAVMO_BASE_IMAGE
        app.rs                  # App shell/router, UI components, #[server] functions
        registry.rs             # ssr-only: workspace registry + running-VM map +
                                # OPENCODE_WORKSPACE_SESSION_PATH (the §9 deep-link)
  nix/
    base/
      flake.nix                # nixos-generators -> qcow2
      flake.lock
      configuration.nix        # the whole M0/M1/M1.5/M1.6 image definition
```

## 6. Open risks to watch

- **9p performance** on large/busy trees (`node_modules`, big `.git`) — not yet stress-
  tested; `cache=none` favors correctness over speed. Revisit if it proves too slow.
- **File watching** (`inotify`) over 9p is unreliable — dev servers/watchers inside the
  VM may need polling mode. Not yet hit, not yet tested.
- **Vertex ADC is readable by the agent.** With permissions wide open and the agent
  running as root, it *can* `cat` the mounted ADC token. Accepted risk for the MVP
  (short-lived ~24h credentials, solo use); the brief's deferred host-side token-proxy
  would eventually remove this.
- **KVM availability** confirmed working on the real NixOS host (`vmx` present,
  user in `kvm` group).
- **First boot is slower** than subsequent ones (GRUB, ext4 resize, OpenCode startup);
  the persistent overlay disk mitigates repeat boots. `vm::handle::BOOT_HEALTH_TIMEOUT`
  is 5 minutes.
- **M2's `nixos-rebuild`-against-a-flake-built-guest** mechanism is unproven — the
  single biggest remaining technical unknown in the whole project.

## 7. MVP "done" definition

From the GUI: pick a repo → VM boots → OpenCode web opens, authenticated to Vertex →
the agent can edit files visible on the host, in both directions → **and several
workspaces/VMs can run at once without clashing**. (Per-repo `gavmo.nix` environments
are a valuable follow-up, not a blocker for calling this MVP done — see M2.)

**Status: ✅ met.** Confirmed end-to-end through the M3 control panel on real
NixOS/KVM hardware: folder picker → Open → live status polling → "Open UI" opens
directly into a new opencode session scoped to `/workspace`, authenticated to Vertex.
M2 (`gavmo.nix` rebuild-on-open) remains the one open follow-up milestone.

## 8. Relationship to the original brief

This MVP deliberately cuts the entire cross-platform / QEMU-packaging / secrets / MCP
half of the original brief, and leans on `opencode web` + NixOS to avoid building a
chat UI or a custom environment manager. Those deferred items remain valid future
phases once the core loop is proven — which, as of M3's control panel, it now is.
