# M2 spike runbook — per-repo `gavmo.nix` (in-guest rebuild)

> Companion to `gavmo-mvp-plan.md`'s **M2** milestone. This is a manual, interactive
> spike run on real NixOS/KVM hardware (no QEMU/KVM in the dev sandbox or workbench
> sidecar) to prove the in-guest `nixos-rebuild` mechanism before committing to it as
> the M2 implementation.

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
   console/journal (and a daemon-visible status — don't fail silently), and either
   degrade to the base config or mark the workspace failed (UX choice to make once
   we see real failure modes).
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
</content>
