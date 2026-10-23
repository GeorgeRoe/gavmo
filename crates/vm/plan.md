# `vm` crate — VM management design

> Crate name on disk: `crates/vm` (Cargo package name `vm`, will likely be published
> under the workspace as `gavmo-vm` if/when namespacing matters).
>
> This is the design for the code that boots, monitors, and stops the per-workspace
> QEMU VMs described in `gavmo-mvp-plan.md`. It is pure mechanism: it knows how to
> drive QEMU given a `VmConfig`; it does not decide ports, paths, or identity — that's
> the daemon's job.

## 1. Decisions (all resolved)

| Area | Decision |
|---|---|
| Host dir sharing | **virtiofs** (`virtiofsd` + `vhost-user-fs-pci`), behind a `ShareBackend` trait seam — see "Why virtiofs, not 9p" below for the M3 switch from the original 9p transport |
| virtiofs coherence | **`cache=auto`** (guest-side mount option and `virtiofsd`'s own `--cache=auto` flag must agree) |
| gcloud creds | virtiofs **read-only** mount |
| OpenCode password | **none** — port is forwarded to `127.0.0.1` only, local-only is sufficient |
| QMP | **`qapi` crate** (async/tokio) for shutdown/status |
| Health | poll the forwarded host port for OpenCode |
| Responsibility split | daemon owns ports/paths/identity/registry; `vm` crate is pure mechanism |
| Acceleration | `-enable-kvm -cpu host`, Linux-only |
| Guest agent privilege | unprivileged `agent` user (uid/gid `1000`, see `share::GUEST_AGENT_UID`/`_GID`), not root — see "Why virtiofs, not 9p" |

### Why virtiofs, not 9p (M3 — supersedes the original MVP decision below)

The original MVP shipped 9p specifically **to support a Windows host** (see the
superseded rationale further down). That constraint was later dropped (GAVMO is
Linux-host-only), which reopened the transport choice — and a second, independent
requirement made virtiofs the clear winner: running the guest agent as an
**unprivileged** user (rather than root) needs file ownership inside the guest to be
correct and predictable.

9p's `security_model=mapped-xattr` stores guest-visible ownership in host-side
xattrs, decoupled from real host uids. That's invisible to a **root** guest agent
(root bypasses DAC entirely, so whatever ownership 9p reports doesn't matter) but
becomes load-bearing for a non-root one: if a pre-existing file's mapped ownership
doesn't resolve to the agent's uid, it simply can't read/write it.

`virtiofsd`'s **internal** UID/GID translation (`--translate-uid`/`--translate-gid`,
see `share::build_virtiofsd_args`) sidesteps this at the transport level: each share
is started with a mapping from its *real* host owner to the guest's `agent` uid/gid,
so ownership "just works" without xattr quirks. This is what makes the non-root guest
agent (see `nix/base/configuration.nix`'s `users.users.agent` and
`gavmo-opencode`'s `serviceConfig`) practical. virtiofs also has stronger
POSIX/locking semantics than 9p, which may eventually make it viable to move
OpenCode's session SQLite back onto `/workspace` (currently on the overlay disk — see
that service's comment) — not pursued as part of this change, since it's orthogonal.

**Validated on real hardware** (virtiofsd 1.13.3, unprivileged): the first attempt
used `--uid-map`/`--gid-map`, which virtiofsd's own `--help` documents as mapping
performed by the **user namespace** it places itself into via
`--sandbox=namespace` — this failed in practice (`Couldn't set the process uid as
root`, and `/workspace` kept its raw, untranslated host ownership) because that
namespace's root-mapping needs privilege an unprivileged `virtiofsd` doesn't have.
The fix was switching to **`--translate-uid`/`--translate-gid`**, which `--help`
documents as mapping performed **internally by virtiofsd**, explicitly *"does not
require using a user namespace"* — paired with **`--sandbox none`** (skip the
namespace/root-mapping step entirely, since nothing here needs it) this works fully
unprivileged. `--inode-file-handles=never` additionally skips a doomed
`CAP_DAC_READ_SEARCH`-requiring file-handle attempt (and its warning) that
unprivileged virtiofsd otherwise makes before falling back anyway.

### Why 9p instead of virtiofs (superseded — kept for history)

> The following was the original MVP-era rationale. It's no longer current (see
> above) but is kept as a record of why 9p was chosen at the time.

Researched directly: `virtiofsd` is a FUSE-based daemon that runs **on the host**, and
FUSE is Linux-only. virtiofs also requires a `memory-backend-memfd` shared-memory
backend, which is Linux-only. The "virtiofs on Windows" documentation that exists is
for Windows as a **guest** (WinFsp + in-guest driver) — the opposite of our case
(Windows host, Linux guest). There is no supported `virtiofsd` for a Windows host.

`virtio-9p-pci` is built directly into QEMU and works identically on Windows and Linux
hosts, with zero extra background processes. Given GAVMO's goal (at the time) of one
mechanism that works cross-platform, 9p was the portable default; virtiofs was flagged
as a possible Linux-only optimization layered in later via the same trait — which is
exactly what happened once the cross-platform requirement was dropped.

### Known 9p tradeoffs (historical — recorded for the same reason as above)

- **Performance** on large/busy trees (`node_modules`, big `.git`, builds) — 9p does a
  round-trip per metadata op and can be noticeably slower than virtiofs/native.
- **File watching** — `inotify` over 9p is unreliable; dev servers/watchers inside the
  VM may need polling mode to see host edits.
- **Caching semantics** — `cache=none` (the old default) favored coherence over speed.
- **Permissions/ownership** — `security_model=mapped-xattr`; git can be sensitive to
  ownership/exec bits, and this is ultimately what motivated the virtiofs switch above.

virtiofs resolves the ownership tradeoff by construction (uid mapping) and is
generally expected to perform better, though that hasn't been freshly re-benchmarked
as part of this change.

## 2. The Rust ↔ base-image contract

This is the fixed interface between this crate (host side) and the NixOS base image
(guest side). Everything else on either side is an implementation detail.

| Item | Value |
|---|---|
| workspace share | virtiofs, `mount_tag=workspace` → guest mounts `/workspace`, `rw`; host side started with `--cache=auto` and a uid/gid map to the guest's `agent` user |
| creds share | virtiofs, `mount_tag=gcloud`, read-only → guest `/etc/gavmo/gcloud`, read-only; `GOOGLE_APPLICATION_CREDENTIALS` points inside |
| guest OpenCode port | fixed `7080`; guest runs `opencode web --hostname 0.0.0.0 --port 7080` |
| bootstrap | guest reads `/workspace/gavmo.nix` → runs `nixos-rebuild test` → launches `opencode web` (M2, entirely guest-side — see `nix/spike-m2.md`; no `vm`-crate code change expected) |
| control channel | QMP over a unix socket at `.gavmo/vm/qmp.sock` |
| guest agent identity | unprivileged `agent`, uid/gid `1000` (`share::GUEST_AGENT_UID`/`_GID`) — must match `users.users.agent`'s pinned uid/gid in `nix/base/configuration.nix` by hand, same "no automatic sharing" contract as `GAVMO_MCP_PORT` |

Cache mode and mount tags are **guest-side mount options** baked into the base NixOS
image's fstab — part of this contract, not QEMU arguments on their own.

## 3. Crate layout

```
crates/vm/src/
  lib.rs      # public API: boot/stop/status/health, re-exports
  config.rs   # VmConfig (repo_dir, state_dir, base_image, gcloud_dir, mem, vcpus, host_port)
  share.rs    # ShareBackend trait + VirtiofsShare (uid/gid-mapped, cache=auto); ShareSpec
  disk.rs     # qcow2 overlay creation via `qemu-img`
  qemu.rs     # pure: build_qemu_args(&VmConfig, &[&dyn ShareBackend]) -> Vec<String>; spawn wrapper
  qmp.rs      # thin wrapper over `qapi` (powerdown, query-status, quit)
  health.rs   # poll http://127.0.0.1:<host_port> until OpenCode answers
  handle.rs   # RunningVm: owns qemu Child + per-share virtiofsd Children + qmp sock + cfg; stop()/status()
  error.rs    # thiserror Error
```

## 4. Core types

```rust
pub struct VmConfig {
    pub workspace_id: String,
    pub repo_dir: PathBuf,
    pub state_dir: PathBuf,
    pub base_image: PathBuf,
    pub gcloud_dir: PathBuf,
    pub memory_mib: u32,
    pub vcpus: u32,
    pub host_port: u16,
}

pub trait ShareBackend {
    fn qemu_args(&self, state_dir: &Path, spec: &ShareSpec) -> Vec<String>;
    fn start(&self, state_dir: &Path, spec: &ShareSpec) -> Result<Option<Child>>; // Some(virtiofsd) always, for VirtiofsShare
}

pub struct VirtiofsShare; // only implementation; stats the share's host owner and
                           // uid/gid-maps it to GUEST_AGENT_UID/_GID for virtiofsd

pub struct RunningVm {
    // qemu Child, per-share virtiofsd Children, qmp socket path, cfg
}

pub enum VmStatus {
    Stopped,
    Booting,
    Running { url: String },
    Failed { reason: String },
}
```

All APIs are async (`tokio::process`), matching the axum daemon. Public verbs: `boot`,
`stop`, `status`, `health`. `reset` is deferred (would be `stop` + delete overlay).

## 5. Lifecycle flows

**`boot(cfg) -> Result<RunningVm>`**
1. `mkdir -p .gavmo/vm`.
2. Create the overlay disk if missing:
   `qemu-img create -f qcow2 -F qcow2 -b <base_image> .gavmo/vm/disk.qcow2`
   (base image stays read-only; the overlay persists `/nix` + OpenCode sessions
   across restarts, which is the cache-reuse story for the MVP).
3. `backend.start()` for each share — spawns one `virtiofsd` per share, each stat-ing
   its host directory to derive the uid/gid map passed on its command line.
4. Assemble QEMU args (below) and spawn the qemu child, headless
   (`-nographic`, serial redirected to `.gavmo/vm/console.log`).
5. Connect to the QMP unix socket, send `qmp_capabilities`.
6. Poll the forwarded host port for OpenCode health until ready or timeout
   (status is `Booting` in the meantime).
7. Return `RunningVm` (status becomes `Running { url }`).

**`stop()`**
QMP `system_powerdown` (ACPI) → wait for SHUTDOWN event/exit with a timeout → else
`quit`/kill → stop each `virtiofsd` child process → remove sockets. The overlay disk
is always preserved.

**`status()`**
Child process alive? + QMP `query-status` + a health poll → resolves to `VmStatus`.

**Concurrency**
Every VM is fully isolated under its own `.gavmo/vm/` (own disk, sockets, console
log). The daemon allocates a unique `host_port` and QMP socket path per workspace and
holds a `HashMap<workspace_id, RunningVm>`.

## 6. QEMU invocation (current, virtiofs for all shares)

Each share's `virtiofsd` is spawned first, listening on its own unix socket under
`.gavmo/vm/`; QEMU then connects to those sockets via `vhost-user-fs-pci`. virtiofs
additionally requires guest RAM backed by shared memory (`memory-backend-memfd`),
only emitted when at least one share is present:

```
# per share, started before qemu (crates/vm/src/share.rs::build_virtiofsd_args):
virtiofsd --socket-path .gavmo/vm/virtiofs-workspace.sock \
  --shared-dir <repo_dir> --cache=auto \
  --sandbox none --inode-file-handles=never \
  --translate-uid map:1000:<host_uid>:1 --translate-gid map:1000:<host_gid>:1
virtiofsd --socket-path .gavmo/vm/virtiofs-gcloud.sock \
  --shared-dir <gcloud_dir> --cache=auto --readonly \
  --sandbox none --inode-file-handles=never \
  --translate-uid map:1000:<host_uid>:1 --translate-gid map:1000:<host_gid>:1

qemu-system-x86_64 \
  -enable-kvm -cpu host -smp <vcpus> -m <mem> \
  -object memory-backend-memfd,id=mem,size=<mem>M,share=on \
  -numa node,memdev=mem \
  -drive file=.gavmo/vm/disk.qcow2,if=virtio,format=qcow2 \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:<host_port>-:7080 \
  -device virtio-net-pci,netdev=net0 \
  -chardev socket,id=char-workspace,path=.gavmo/vm/virtiofs-workspace.sock \
  -device vhost-user-fs-pci,chardev=char-workspace,tag=workspace \
  -chardev socket,id=char-gcloud,path=.gavmo/vm/virtiofs-gcloud.sock \
  -device vhost-user-fs-pci,chardev=char-gcloud,tag=gcloud \
  -qmp unix:.gavmo/vm/qmp.sock,server,nowait \
  -nographic -serial file:.gavmo/vm/console.log
```

(The third share, the MCP token mount, follows the same `gcloud` pattern and is
omitted above for brevity — see `handle.rs::boot`.)

Validated on real hardware (virtiofsd 1.13.3) — see §1's "Why virtiofs, not 9p" for
why `--translate-uid`/`--translate-gid` + `--sandbox none` is the combination that
works fully unprivileged, and what was tried first and didn't.

## 7. Dependencies

- `tokio` — async process/io/time.
- `qapi` — QMP client (async/tokio integration).
- `thiserror` — error enum.
- `serde` / `serde_json` — config and QMP payloads.
- `tracing` — logging.

## 8. Testing strategy

- **In the workbench sidecar (available now):** unit-test `build_qemu_args`,
  `VirtiofsShare::qemu_args`, `build_virtiofsd_args`, overlay path logic, and QMP
  message encode/decode — all pure functions, no `qemu`/`virtiofsd`/`/dev/kvm`
  required. The sidecar has cargo but **not** qemu or `/dev/kvm`, so this is the
  right boundary. (`build_virtiofsd_args` is deliberately split out from
  `VirtiofsShare::start`'s stat+spawn for exactly this reason, mirroring
  `disk.rs`'s `build_overlay_args`/`ensure_overlay` split.)
- **On the real host (NixOS, KVM available):** one integration test that actually
  boots a VM end-to-end, gated behind an env var or feature flag so `cargo test` in
  the sidecar stays green. This is also how the `virtiofsd` flag syntax (see §1/§6)
  was confirmed for real against virtiofsd 1.13.3.

## 9. Open items for later (not MVP)

- Windows host support: not currently planned (dropped as part of the M3 virtiofs
  switch — see §1's "Why virtiofs, not 9p"). Would require reintroducing a 9p (or
  other cross-platform) `ShareBackend` alongside `VirtiofsShare`, gated to the host's
  actual OS, plus WHPX acceleration instead of KVM.
- `reset` lifecycle action (stop + delete overlay, rebuild from base).
- Revisit moving OpenCode's session SQLite from the overlay disk onto `/workspace`,
  now that virtiofs's stronger POSIX/locking semantics than 9p might support it (see
  `nix/base/configuration.nix`'s `gavmo-opencode` comment).
