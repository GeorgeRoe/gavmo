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
| Host dir sharing | **9p** (cross-platform, no extra daemon), behind a `ShareBackend` trait seam |
| 9p coherence | **`cache=none`** (favor correctness over speed; guest-side mount option) |
| gcloud creds | 9p **read-only** mount |
| OpenCode password | **none** — port is forwarded to `127.0.0.1` only, local-only is sufficient |
| QMP | **`qapi` crate** (async/tokio) for shutdown/status |
| Health | poll the forwarded host port for OpenCode |
| Responsibility split | daemon owns ports/paths/identity/registry; `vm` crate is pure mechanism |
| Acceleration | `-enable-kvm -cpu host`, cfg-gated to Linux (WHPX for Windows later) |

### Why 9p instead of virtiofs

Researched directly: `virtiofsd` is a FUSE-based daemon that runs **on the host**, and
FUSE is Linux-only. virtiofs also requires a `memory-backend-memfd` shared-memory
backend, which is Linux-only. The "virtiofs on Windows" documentation that exists is
for Windows as a **guest** (WinFsp + in-guest driver) — the opposite of our case
(Windows host, Linux guest). There is no supported `virtiofsd` for a Windows host.

`virtio-9p-pci` is built directly into QEMU and works identically on Windows and Linux
hosts, with zero extra background processes. Given GAVMO's goal of one mechanism that
works cross-platform, **9p is the portable default**; virtiofs would only ever be a
Linux-only performance optimization layered in later via the same trait.

### Known 9p tradeoffs (accepted for MVP, to validate in the Phase 0 spike)

- **Performance** on large/busy trees (`node_modules`, big `.git`, builds) — 9p does a
  round-trip per metadata op and can be noticeably slower than virtiofs/native.
- **File watching** — `inotify` over 9p is unreliable; dev servers/watchers inside the
  VM may need polling mode to see host edits.
- **Caching semantics** — `cache=none` (our default) favors coherence over speed.
  `cache=loose` is faster but weaker; revisit only if `cache=none` proves too slow.
- **Permissions/ownership** — `security_model=mapped-xattr` is the starting point;
  git can be sensitive to ownership/exec bits, validate in the spike.

None of these block the MVP. They are explicit risks to test for real in the Phase 0
manual spike, with mitigations (tune `msize`, polling watchers) available if needed.

## 2. The Rust ↔ base-image contract

This is the fixed interface between this crate (host side) and the NixOS base image
(guest side). Everything else on either side is an implementation detail.

| Item | Value |
|---|---|
| workspace share | 9p, `mount_tag=workspace` → guest mounts `/workspace`, `cache=none,msize=1048576,rw` |
| creds share | 9p, `mount_tag=gcloud`, `readonly=on` → guest `/etc/gavmo/gcloud`, read-only; `GOOGLE_APPLICATION_CREDENTIALS` points inside |
| guest OpenCode port | fixed `7080`; guest runs `opencode web --hostname 0.0.0.0 --port 7080` |
| bootstrap | guest reads `/workspace/gavmo.nix` → runs `nixos-rebuild switch` → launches `opencode web` |
| control channel | QMP over a unix socket at `.gavmo/vm/qmp.sock` |

Cache mode, `msize`, and mount tags are **guest-side mount options** baked into the
base NixOS image's fstab — part of this contract, not QEMU arguments on their own.

## 3. Crate layout

```
crates/vm/src/
  lib.rs      # public API: boot/stop/status/health, re-exports
  config.rs   # VmConfig (repo_dir, state_dir, base_image, gcloud_dir, mem, vcpus, host_port)
  share.rs    # ShareBackend trait + NinePShare (cache=none); ShareSpec
  disk.rs     # qcow2 overlay creation via `qemu-img`
  qemu.rs     # pure: build_qemu_args(&VmConfig, &[&dyn ShareBackend]) -> Vec<String>; spawn wrapper
  qmp.rs      # thin wrapper over `qapi` (powerdown, query-status, quit)
  health.rs   # poll http://127.0.0.1:<host_port> until OpenCode answers
  handle.rs   # RunningVm: owns qemu Child + qmp sock + cfg; stop()/status()
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
    fn qemu_args(&self, spec: &ShareSpec) -> Vec<String>;
    async fn start(&self, spec: &ShareSpec) -> Result<Option<Child>>; // None for 9p
}

pub struct NinePShare; // only implementation for now; trait lets virtiofs slot in later

pub struct RunningVm {
    // qemu Child, qmp socket path, cfg
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
3. `backend.start()` for each share (no-op for 9p — no extra process to spawn).
4. Assemble QEMU args (below) and spawn the qemu child, headless
   (`-nographic`, serial redirected to `.gavmo/vm/console.log`).
5. Connect to the QMP unix socket, send `qmp_capabilities`.
6. Poll the forwarded host port for OpenCode health until ready or timeout
   (status is `Booting` in the meantime).
7. Return `RunningVm` (status becomes `Running { url }`).

**`stop()`**
QMP `system_powerdown` (ACPI) → wait for SHUTDOWN event/exit with a timeout → else
`quit`/kill → stop any backend child process → remove sockets. The overlay disk is
always preserved.

**`status()`**
Child process alive? + QMP `query-status` + a health poll → resolves to `VmStatus`.

**Concurrency**
Every VM is fully isolated under its own `.gavmo/vm/` (own disk, sockets, console
log). The daemon allocates a unique `host_port` and QMP socket path per workspace and
holds a `HashMap<workspace_id, RunningVm>`.

## 6. QEMU invocation (final, 9p for both shares)

```
qemu-system-x86_64 \
  -enable-kvm -cpu host -smp <vcpus> -m <mem> \      # kvm/cpu-host cfg-gated to linux
  -drive file=.gavmo/vm/disk.qcow2,if=virtio,format=qcow2 \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:<host_port>-:7080 \
  -device virtio-net-pci,netdev=net0 \
  -fsdev local,id=ws,path=<repo_dir>,security_model=mapped-xattr \
  -device virtio-9p-pci,fsdev=ws,mount_tag=workspace \
  -fsdev local,id=gc,path=<gcloud_dir>,security_model=mapped-xattr,readonly=on \
  -device virtio-9p-pci,fsdev=gc,mount_tag=gcloud \
  -qmp unix:.gavmo/vm/qmp.sock,server,nowait \
  -nographic -serial file:.gavmo/vm/console.log
```

## 7. Dependencies

- `tokio` — async process/io/time.
- `qapi` — QMP client (async/tokio integration).
- `thiserror` — error enum.
- `serde` / `serde_json` — config and QMP payloads.
- `tracing` — logging.

## 8. Testing strategy

- **In the workbench sidecar (available now):** unit-test `build_qemu_args`,
  `NinePShare::qemu_args`, overlay path logic, and QMP message encode/decode — all
  pure functions, no `qemu`/`virtiofsd`/`/dev/kvm` required. The sidecar has cargo
  but **not** qemu or `/dev/kvm`, so this is the right boundary.
- **On the real host (NixOS, KVM available):** one integration test that actually
  boots a VM end-to-end, gated behind an env var or feature flag so `cargo test` in
  the sidecar stays green.

## 9. Open items for later (not MVP)

- Swap in a Linux-only `VirtiofsShare` behind the same `ShareBackend` trait if 9p
  performance proves inadequate after real use.
- Windows host support: WHPX acceleration instead of KVM, cfg-gated alongside the
  existing Linux path.
- `reset` lifecycle action (stop + delete overlay, rebuild from base).
