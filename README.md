# GAVMO

> George's Agent Virtual Machine Orchestrator

## Customizing your repo's VM with `gavmo.nix`

Each repo's VM runs GAVMO's fixed base system by default. If your repo's root
contains a `gavmo.nix` file, GAVMO merges it onto that base config (as a NixOS
module) and activates it **before** `opencode` starts — so the agent, and every
shell it spawns, run inside the environment you declare.

```nix
# gavmo.nix
{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.nodejs pkgs.jq ];
  virtualisation.docker.enable = true;
}
```

Because it's a real NixOS module, `gavmo.nix` isn't limited to project tooling
(language runtimes, CLIs) the way a plain dev shell would be — it can also
declare system-level things a dev shell structurally can't: services/daemons
(like Docker above), kernel modules, sysctls, the system CA trust store, etc.
In a dedicated per-repo VM there's no multi-project contention to isolate
against, so `environment.systemPackages` doubles as "the project's tools" too.

A few things worth knowing:

- **It's additive**, evaluated via the NixOS module system on top of GAVMO's
  base config — list-valued options (like `environment.systemPackages`)
  concatenate; most other options just work as expected. Don't try to redefine
  options the base config already fixes — in particular, the `/workspace` and
  `/etc/gavmo/gcloud` 9p mounts and the `gavmo-opencode` service are owned by
  GAVMO and shouldn't be touched.
- **It runs as root, at VM-boot time**, via an in-guest `nixos-rebuild test`
  (activates immediately, never touches the bootloader). The VM is GAVMO's
  trust boundary — same stance as OpenCode's own wide-open permission policy
  inside the VM — so treat `gavmo.nix` as trusted, repo-owned configuration,
  not sandboxed input.
- **No `gavmo.nix`? Nothing changes.** The rebuild step is skipped entirely and
  the VM behaves exactly as it does today.
- **A broken `gavmo.nix` blocks the agent, deliberately.** If it fails to
  evaluate or build, `opencode` **will not start** — rather than silently
  falling back to the base config and letting the agent reach for tools
  (packages, services like Docker) it believes exist but doesn't have. This is
  detected within seconds (not a multi-minute boot timeout): the GAVMO control
  panel shows the workspace as failed with a **"View build log"** button
  surfacing the real `nixos-rebuild` error inline. Fix `gavmo.nix` and hit
  "Open" again. (Nothing else breaks — the overlay disk and your `/workspace`
  files are untouched.)

See `nix/spike-m2.md` for the full design and the hardware-validated results
behind this mechanism.

