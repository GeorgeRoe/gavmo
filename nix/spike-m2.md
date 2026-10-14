# M2 spike runbook — per-repo `gavmo.nix` (in-guest rebuild)

> Companion to `gavmo-mvp-plan.md`'s **M2** milestone. This is a manual, interactive
> spike run on real NixOS/KVM hardware (no QEMU/KVM in the dev sandbox or workbench
> sidecar) to prove the in-guest `nixos-rebuild` mechanism before committing to it as
> the M2 implementation.
>
> **Status: ✅ spike complete, all rungs green on real NixOS/KVM hardware.** See §9
> for the recorded results. §§1–7 are kept as-written (the plan as it was run);
> §8's implementation has since been built — see `nix/base/configuration.nix`'s
> `gavmo-rebuild` service and `environment.etc."gavmo/system"`.

## 0. Decisions already made (not re-litigated by this spike)

| Area | Decision | Why |
|---|---|---|
| Repo-facing contract | **`gavmo.nix` only** — a per-repo NixOS module, no `flake.nix` devShell support | In a dedicated per-repo VM, `environment.systemPackages` in `gavmo.nix` *is* the project environment — no multi-project contention to isolate against. A devShell can't do system-level things (daemons, kernel modules, sysctls, trust store) anyway; `gavmo.nix` covers both devShell's job and system config in one mechanism. devShell support can be added later cheaply if ever needed. |
| Mechanism | **Flake + in-guest `nixos-rebuild test --impure`** | `test` activates without touching the bootloader or the default boot entry — removes the single biggest "wedge the guest" risk. A baked flake with nixpkgs pinned to the base image's rev means the rebuild reuses the guest's warm store instead of re-fetching nixpkgs. |

### When `gavmo.nix`-level system config is actually needed (not just "nice")

Context for why this is worth the rebuild cost at all, rather than stopping at a
devShell:

- **Docker / container-based tests** — `virtualisation.docker.enable` needs a root
  daemon + cgroups; a devShell cannot start `dockerd`.
- **Corporate CA / TLS-intercepting proxy** — needs `security.pki.certificateFiles`
  (system trust store), not just a `SSL_CERT_FILE` env var scoped to a shell.
- **Kernel-level work** — kernel modules, eBPF, nested KVM, custom `kernelParams`.
- **sysctls / hard resource limits** — e.g. `vm.max_map_count` for Elasticsearch,
  `vm.overcommit_memory` for Redis, PAM-level `nofile` limits.
- **The repo *is* system/infra code** — e.g. GAVMO's own repo, which needs to actually
  *activate* NixOS config to verify it, not just evaluate it.

Project-tooling needs (language runtimes, CLIs, LSPs — the devShell's usual job) are
also just `environment.systemPackages` entries in `gavmo.nix` for the MVP.

## 1. Goal & verdicts

- **V1 — evaluate+build:** `nixosConfigurations.gavmo` (base `configuration.nix` +
  optional `/workspace/gavmo.nix`) builds in-guest, reusing the guest's warm store
  (no full nixpkgs re-fetch).
- **V2 — activate safely:** `switch-to-configuration test` applies it without
  touching the bootloader or wedging the guest.
- **V3 — real effect:** a sample `gavmo.nix` (extra package +
  `virtualisation.docker.enable`) actually lands and its systemd services come up
  live.
- **V4 — timings:** cold vs. warm (overlay) rebuild, and a one-line `gavmo.nix`
  change → informs the boot timeout and whether a "rebuilding" status signal is
  needed beyond just widening it.

## 2. Execution model

Runs on the **real NixOS/KVM host** — the agent sandbox and workbench sidecar have
no QEMU/KVM. The agent guides read-only; the developer runs each step and pastes
output; branch on results. Throwaway overlay disk, so a bad activation is just
`rm /tmp/gavmo-spike.qcow2` and recreate.

## 3. Pre-spike host edit (temporary, to get something to test)

The current `nix/base/flake.nix` only outputs the image. Add a second output
sharing the module list (this is the real refactor M2 needs anyway):

```nix
# nix/base/flake.nix  — add to outputs
let commonModules = [ ./configuration.nix ]; in {
  packages.${system}.default = nixos-generators.nixosGenerate {
    inherit system; format = "qcow"; modules = commonModules;
  };
  nixosConfigurations.gavmo = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = commonModules ++ [
      ({ lib, ... }: {
        imports = lib.optional (builtins.pathExists "/workspace/gavmo.nix")
                    /workspace/gavmo.nix;
      })
    ];
  };
}
```

No image rebuild is required to *start* the spike — the guest runs `nixos-rebuild`
against this flake directly. (Baking the flake into the image + a boot service is
the M2 productionization step, after the verdicts are in — see §7.)

## 4. Setup — interactive guest shell, `/workspace` = the gavmo repo

```sh
ls -L result/
BASE=$(readlink -f result/nixos.qcow2)
qemu-img create -f qcow2 -F qcow2 -b "$BASE" /tmp/gavmo-spike.qcow2

qemu-system-x86_64 -enable-kvm -cpu host -smp 4 -m 4096 \
  -drive file=/tmp/gavmo-spike.qcow2,if=virtio,format=qcow2 \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:7099-:7080 \
  -device virtio-net-pci,netdev=net0 \
  -fsdev local,id=ws,path=$(pwd),security_model=mapped-xattr \
  -device virtio-9p-pci,fsdev=ws,mount_tag=workspace \
  -nographic
```

(`/workspace` = the gavmo repo itself, so the guest can see `nix/base/` and we can
drop a sample `gavmo.nix` at its root. Root auto-logins on the console. Exit QEMU
with `Ctrl-a x`.)

## 5. Phase 0 — baseline facts (in guest)

```sh
nixos-version; nix --version
nix config show | grep -i experimental-features || echo "flakes OFF by default"
ls /workspace/nix/base && mount | grep 9p
df -h / /workspace      # ENOSPC headroom — likeliest false "no-go" for a rebuild
nix --extra-experimental-features nix-command \
  store ping --store https://cache.nixos.org && echo NET_OK
```

**Confirms:** flakes on/off by default (the spike passes
`--extra-experimental-features` explicitly either way; production bakes
`nix.settings.experimental-features`), `nix/base` visible over 9p, **free space**
on the overlay disk, substituter reachable.

## 6. Phase R — the rebuild ladder (go/no-go at each rung)

To mirror the baked-flake production setup and dodge 9p/git-tree noise (flakes
copy the git-tracked tree; an in-progress edit to `flake.nix` on a dirty tree
behaves differently than a clean one), copy the flake to native fs first:

```sh
cp -r /workspace/nix/base /tmp/system        # native ext4, non-git => clean path: semantics
NR="nixos-rebuild --extra-experimental-features 'nix-command flakes'"
```

- **R1 — sanity:** `nix --extra-experimental-features 'nix-command flakes' build nixpkgs#hello`
  → nix + net + build work at all. *(Record time.)*

- **R2 — build the real system:**
  ```sh
  $NR build --flake /tmp/system#gavmo --impure
  ```
  **Expected first failure:** missing `fileSystems."/"` / bootloader config —
  `configuration.nix` doesn't carry the disk/boot bits that `nixos-generators`'
  qcow format module injects at image-build time. **This is the predicted crux of
  the whole spike.**

  Fix (**R2b**): inspect `nixos-generators`'s `formats/qcow*.nix` (and whatever
  profile it pulls in, e.g. a qemu-guest profile), factor the equivalent root-fs +
  `boot.loader.grub` + `boot.growPartition` settings into a shared
  `nix/base/disk.nix`, add it to `commonModules`, re-run. Iterate until it builds.
  *(Record cold build time.)*

- **R3 — activate safely:**
  ```sh
  sudo ./result/bin/switch-to-configuration test
  ```
  → services re-run, **no bootloader write, no reboot, guest stays up**. Verify
  `systemctl` is healthy and `opencode web` is still reachable on the host-forwarded
  port (`curl http://127.0.0.1:7099`).

- **R4 — real repo effect:** create a sample module and rebuild:
  ```nix
  # /workspace/gavmo.nix
  { pkgs, ... }: {
    environment.systemPackages = [ pkgs.jq ];
    virtualisation.docker.enable = true;   # Tier-B poster child: needs a root daemon
  }
  ```
  ```sh
  $NR build --flake /tmp/system#gavmo --impure && sudo ./result/bin/switch-to-configuration test
  ```
  → confirm `jq` is on `PATH` **and** `systemctl status docker` + `docker run
  hello-world` work. This is the case a devShell genuinely could not cover.

- **R5 — timings:** warm rebuild (no change) vs. R2's cold build, then a one-line
  `gavmo.nix` edit → informs `vm::handle::BOOT_HEALTH_TIMEOUT` and whether a
  "rebuilding" status signal is needed beyond just widening that timeout.

## 7. Decision gate

- **R1–R4 all green:** M2 = `gavmo.nix` rebuild, mechanism proven. Proceed to the
  implementation plan below.
- **R2 can't be made to match the booted system:** fall back to driving the image
  *from* `nixosConfigurations.gavmo` directly (one evaluation, so the image and the
  rebuild target can never diverge) or explore building on the host and only
  activating in the guest; re-spike whichever is chosen.
- **R3 wedges the guest, or R4 can't bring up services live:** reconsider `test` vs.
  a reboot-into-new-generation model, or narrow the `gavmo.nix` contract (e.g. defer
  `virtualisation.*`-style service-adding modules to a later pass).

## 8. Contingent M2 implementation (only after green verdicts)

1. **`nix/base/`:** keep the two-output flake (`commonModules`, the new
   `disk.nix` from R2b); add `nix.settings.experimental-features = [ "nix-command"
   "flakes" ]` so no `--extra-experimental-features` flag is needed at runtime.
2. **Bake the flake into the image** at a fixed path (e.g. `/etc/gavmo/system`),
   with its `flake.lock` pinned to the exact nixpkgs rev the image itself was built
   from — this is what makes the in-guest rebuild fast (warm store) instead of a
   from-scratch nixpkgs fetch.
3. **`gavmo-rebuild.service`**, ordered **before** `gavmo-opencode`: runs
   `nixos-rebuild test --flake /etc/gavmo/system#gavmo --impure`. On success, start
   `opencode web` in the now-activated system; on failure, surface the Nix error to
   console/journal (and a daemon-visible status — don't fail silently). **Resolved
   as strict, not degrade:** `gavmo-opencode` both `after`s AND `requires`s this
   service — a broken `gavmo.nix` means opencode never starts at all, rather than
   silently falling back to the base config and letting the agent run with tools
   it believes exist but doesn't (e.g. a `gavmo.nix` enabling Docker that failed to
   build shouldn't leave an agent free to try `docker` commands against a system
   that never got it). The debug console's getty units are also ordered
   (`after`-only, so a failed rebuild is still debuggable) behind this service, so
   a manual QEMU session doesn't drop into a shell mid-rebuild either — purely a
   manual-testing nicety, irrelevant to the real product which has no interactive
   console.
4. **Daemon/UI (M3 surface):** add a "rebuilding env" status distinct from today's
   "`opencode` answering on the forwarded port" ready signal, and widen
   `BOOT_HEALTH_TIMEOUT` per R5's timings. Likely needs a small guest-side status
   file or endpoint for the host's health poller to read mid-rebuild.
5. **Docs — the repo contract:** "add a `gavmo.nix` NixOS module to your repo
   root; it's merged onto GAVMO's base config using GAVMO's pinned nixpkgs."
   Document that it's additive and runs as root at activation time (the VM is the
   trust boundary, same stance as the OpenCode permission policy), and which
   base-owned options it must not clobber (the 9p `fileSystems` entries, the
   `gavmo-opencode` service).
6. **`crates/vm`:** `crates/vm/plan.md` §2 already names this flow aspirationally
   ("guest reads `/workspace/gavmo.nix` → runs `nixos-rebuild switch`") — reconcile
   that doc with what actually ships (`test`, not `switch`; the `vm` crate itself
   likely needs no code change, since the mechanism is entirely guest-side via the
   new systemd service — confirm this during implementation).

## 9. Recorded results (real NixOS/KVM hardware)

All rungs run against the Phase 1 `nix/base` refactor (`commonModules` +
`nixosConfigurations.gavmo` + `disk.nix`), booted from the base image with
`/workspace` = the gavmo repo itself, per §4's Setup.

| Rung | Result |
|---|---|
| R1 (sanity build) | Implicitly covered by R2 succeeding first try — skipped as a separate step. |
| **R2** — `nixos-rebuild build --flake /tmp/system#gavmo --impure` | ✅ **Green on the first attempt.** `disk.nix`'s replicated subset of `nixos-generators`' qcow format module matched the booted system with no iteration needed — the single biggest predicted risk did not materialize. |
| **R3** — `./result/bin/switch-to-configuration test` | ✅ Clean activation. `gavmo-opencode` was **not even restarted** (same PID, continuous uptime), `/workspace` 9p mount stayed intact, and `opencode web` kept answering on the host-forwarded port throughout. No bootloader writes, no wedge. |
| **R4** — sample `gavmo.nix` (`pkgs.jq` + `virtualisation.docker.enable`) | ✅ Full pass. `jq` landed on `PATH`; `docker.service`/`docker.socket` started live (new units, no reboot); `docker run --rm hello-world` succeeded end-to-end (image pull + container run). This is the proof point that justifies `gavmo.nix` over a devShell — a devShell cannot start a root daemon. |
| **R5** — timings | Warm no-op rebuild: **2.3s**. One-line `gavmo.nix` package add: **4.2s build + 0.8s activate**. The R4 Docker-adding rebuild (new service + its full dependency closure): **< 30s total**. All comfortably inside the existing 5-minute `BOOT_HEALTH_TIMEOUT` (`crates/vm/src/handle.rs`) — **no timeout change was needed.** |

**Verdict: V1–V4 all met.** Decision gate (§7) resolves to: proceed with the
`gavmo.nix` rebuild mechanism as designed, implementation per §8 (now built).

## 10. Bugs found validating the *baked* mechanism (Phase 3), and fixes

§9's rungs ran the manual `cp -r /workspace/nix/base /tmp/system` loop (a plain
filesystem copy). Once the real mechanism — `environment.etc."gavmo/system".source
= ./.` baking the flake into the image, plus the `gavmo-rebuild.service` — was
built and tested against an actual `nix build ./nix/base`, more issues surfaced
that the manual loop couldn't have caught (the first two immediately; the third
only once the §11 strict-failure/fast-fail path was exercised against a real
abrupt VM kill):

1. **Untracked files are invisible to flakes.** `disk.nix` was a new, not-yet-
   `git add`ed file. Flakes only copy **git-tracked** files into their source
   tree, so `/etc/gavmo/system` baked without it, and the in-guest rebuild failed
   with `path '…/disk.nix' does not exist`. The manual spike loop used a plain
   `cp -r` (no git involved), so it never hit this. **Fix:** `git add
   nix/base/disk.nix` — staging is sufficient, no commit required. **Lesson:**
   any new file added under `nix/base/` must be staged before `nix build`/the
   in-guest rebuild will see it.
2. **`network.target` isn't real connectivity.** `gavmo-rebuild` initially
   ordered after only `network.target` (matching `gavmo-opencode`'s existing,
   deliberate choice — see that service's comment). But `gavmo-rebuild` actually
   needs to **fetch** packages a `gavmo.nix` adds that aren't already in the base
   image's store (the sample's `jq` + `virtualisation.docker.enable` wasn't).
   Running at ~3.8s into boot, before DHCP completed, it failed with `warning:
   you don't have Internet access`. The manual spike loop ran long after boot
   had settled, so it never hit this either. **Fix:** `gavmo-rebuild.service` now
   `wants`/`after`s `network-online.target` instead. Accepted tradeoff: a worst-
   case DHCP failure could stall this service up to systemd's default
   wait-online timeout (~90s) — bounded and acceptable since `gavmo-opencode`
   only orders after *this service's completion*, not after network-online
   directly, so a stall eats into, not past, the 5-minute `BOOT_HEALTH_TIMEOUT`.
3. **Nix's own cache isn't crash-safe, and lives on the persistent overlay by
   default.** Surfaced only after the §11/strict-mode work landed: Nix runs its
   fetcher-cache and eval-cache SQLite DBs under `~/.cache/nix` with `pragma
   synchronous = off` (durability traded for speed — explicitly **not**
   crash-safe). Our own fast-fail path (`RunningVm::boot` dropping the VM via
   `kill_on_drop` on a detected `RebuildFailed`) — or simply a developer's
   manual `Ctrl-a x` — can abruptly power off the guest mid-write to one of
   these DBs. Because `/root/.cache/nix` sat on the **persistent** overlay disk
   (same as everything else under `/root`), a half-written cache file survived
   reboot and broke **every subsequent rebuild attempt** — including with a
   perfectly valid `gavmo.nix` — with `error: executing SQLite statement
   'pragma synchronous = off': file is not a database`. This is strictly worse
   than the original failure: a workspace could get permanently stuck even
   after the repo's `gavmo.nix` was fixed, with no obvious connection between
   "I fixed my config" and "it's still broken." **Fix:** mount
   `/root/.cache/nix` as `tmpfs` (RAM-backed, wiped every boot) — an abrupt
   kill can still leave a half-written file, but it's in RAM and evaporates
   with the crash, so there's never a persistent corrupt copy to poison a
   later boot. Deliberately scoped to just `/root/.cache/nix`, not all of
   `/root/.cache`: `/nix/store` (the actual expensive build cache) and
   OpenCode's session SQLite (`/root/.local/share` — crash-safe, normal SQLite
   settings, never at risk here) are untouched and stay persistent exactly as
   before. **Self-healing property:** because the tmpfs mounts *over* the
   existing path, it shadows (doesn't need to delete) whatever corrupt file is
   already on an affected overlay — rebuilding the image and re-opening an
   already-broken workspace fixes it with no manual `rm` required.
4. **A successful rebuild doesn't mean the agent can see what it installed.**
   Surfaced via dogfooding, after §11's validation below had already declared
   victory on a `gavmo.nix` adding `pkgs.jq` — that validation only checked
   that `nixos-rebuild test` *succeeded* (i.e. `jq` landed in
   `/run/current-system/sw/bin`), not that the **agent** could actually run
   it. It couldn't: `gavmo-opencode` runs as a systemd service, and services
   get only systemd's minimal default `PATH` (coreutils/findutils/etc), never
   `/run/current-system/sw/bin` — that directory only lands on a shell's
   `PATH` via `/etc/profile`, which only **login** shells source. The debug
   getty (a login shell) saw `jq` fine; the agent (a bare `ExecStart`, not a
   login shell) got `jq: command not found` from every `bash`/`sh` call it
   made, despite `jq` being correctly installed. This defeats the README's
   "Customizing your repo's VM" promise that `environment.systemPackages`
   doubles as the project's tools — the tools were there, just invisible to
   the one consumer that mattered. (`environment.variables`/
   `environment.sessionVariables` from a `gavmo.nix` had the same blind spot,
   for the same underlying reason.) **Fix:** `gavmo-opencode`'s `ExecStart`
   now sources `/etc/profile` itself (via NixOS's `script` option, same
   pattern `gavmo-rebuild` already uses, rather than a raw `ExecStart`
   string with its own `$VAR`/quoting rules) before `exec`ing opencode, then
   re-prepends the `xdg-open` no-op shim that `/etc/profile`'s `PATH` reset
   would otherwise have dropped. `environment.sessionVariables` is merged
   into the service's own `environment=` directly, since it's PAM-delivered
   and `/etc/profile` sourcing wouldn't pick it up. **Lesson:** "the rebuild
   succeeded" and "the agent can use what the rebuild installed" are
   different claims — a spike/test validating M2 needs to check from
   *inside an agent session*, not just the rebuild's own exit code.

All four fixed in `nix/base/configuration.nix`; re-validated with the **actual**
baked image (not the manual `/tmp/system` copy).

## 11. End-to-end validation of the baked mechanism, and a strictness hardening

With both §10 fixes applied, a full boot with a sample `gavmo.nix`
(`pkgs.jq` + `virtualisation.docker.enable`) on the real baked image (not the
manual `/tmp/system` loop) passed completely: `gavmo-rebuild` started after
real connectivity was up, fetched `jq` + moby/docker (~153 MiB) from
`cache.nixos.org`, built the system closure, ran `switch-to-configuration
test`, brought up `docker.socket` live, and **only then** started
`gavmo-opencode` — all in ~24s including the package downloads, comfortably
inside `BOOT_HEALTH_TIMEOUT`. `jq` and `docker` both worked from the shell
afterward.

This also surfaced a product decision that the original §8 plan had left open
("degrade to base, or mark failed"): **resolved as strict** (see §8's updated
text) — `gavmo-opencode` now both `after`s and `requires`s `gavmo-rebuild`, so
a failing `gavmo.nix` blocks the agent entirely rather than silently running
it against an environment missing its declared tools. The debug console's
getty units got the same `after`-only treatment for the same reason a human
manually testing shouldn't land on a shell mid-rebuild either (not relevant to
the real product, which has no interactive console).

**Follow-up debt from strict mode — resolved.** The gap this left (a failing
`gavmo.nix` surfacing only as a generic 5-minute `BOOT_HEALTH_TIMEOUT`, with
the real Nix error unreachable once console logging was turned off) is closed:
`gavmo-rebuild.service` now writes a timestamped log + failure marker into
`/workspace/.gavmo/vm/` (host-visible — see `nix/base/configuration.nix`'s
`gavmo-rebuild` comment block), `crates/vm::RunningVm::boot` races that marker
against the health probe for a fast (seconds, not minutes) failure, and
`crates/gavmo` surfaces it as a `BuildFailed` status with an in-UI "View build
log" action. Full details in `gavmo-mvp-plan.md`'s M2 section.
</content>
