# GAVMO

> George's Agent Virtual Machine Orchestrator

## Architecture

A host daemon boots one QEMU/KVM NixOS VM per repository and runs
`opencode web` inside it. Linux x86-64 hosts only.

Crates (`crates/`):

- `gavmo` - daemon + Leptos control panel (full hydration, `#[server]` API).
  cargo-leptos builds it as `ssr` (native) and `hydrate` (wasm32), so
  host-only deps must stay optional and `ssr`-only. Owns all policy.
- `vm` - pure QEMU mechanism from a `VmConfig`: qcow2 overlay, one
  `virtiofsd` per share, QMP, health polling, stats.
- `mcp` - host-side MCP tool gateway (`rmcp`, stateless Streamable HTTP);
  the daemon injects auth/tools via `TokenAuthority`/`ToolExecutor`.
- `secrets` - encrypted vault behind a `SecretStore` trait.
- `toolspec` - pure `gavmo.yaml` parsing plus host-lock (`lock_url`).

Workspaces and VMs:

- Workspace identity is the repo's absolute host path (registry:
  `~/.local/share/gavmo/workspaces.json`; running VMs are in-memory only).
- The repo is shared read-write into the guest, so nothing a host process
  opens lives in it. Each workspace's persistent qcow2 overlay and console
  log are in `~/.local/share/gavmo/vms/<sha256 of repo path>/` (owner-only),
  on a read-only base image built from `nix/base` (`GAVMO_BASE_IMAGE`); its
  QMP and virtiofsd sockets are in `$XDG_RUNTIME_DIR/gavmo/<id>/` (else
  `/tmp/gavmo-<uid>/`). qemu is given the base image explicitly rather than
  trusting the overlay's header. An overlay only fits the base it was created
  on, so a rebuilt base image needs "Reset". `<repo>/.gavmo/vm/` now holds
  only guest build output; VM files left there by older versions are unused
  (the daemon logs a hint) and can be deleted.
- virtiofs shares: repo -> `/workspace` (rw), gcloud ADC ->
  `/etc/gavmo/gcloud` (ro), MCP token dir -> `/etc/gavmo/mcp` (ro).
  Unprivileged `virtiofsd` maps the host owner to guest `agent` (uid 1000).
- QEMU user-mode NAT forwards a free `127.0.0.1` port to opencode on guest
  port 7080; the guest reaches host loopback via `10.0.2.2`.

Inside the guest (`nix/base/configuration.nix`):

- A repo `gavmo.nix` is applied by `gavmo-rebuild.service` (in-guest
  `nixos-rebuild test` of the flake baked at `/etc/gavmo/system`). On failure
  it writes a `rebuild-failed` marker + log to `.gavmo/vm/`, which the host
  races against the health probe to fail fast.
- `gavmo-opencode` requires that rebuild and runs opencode as the
  unprivileged `agent` user in a systemd sandbox; session data lives in
  `/workspace/.gavmo/opencode`.
- Managed `/etc/opencode/opencode.json` (not repo-overridable) pins the
  Vertex provider, allows all permissions (the VM is the trust boundary), and
  registers the `gavmo-tools` MCP server.

Tool gateway and secrets:

- The MCP gateway is `127.0.0.1:7071/mcp`. Each boot mints a bearer token
  into the token share (outside the repo); Stop/Reset revoke it.
- `gavmo.yaml` is an untrusted request naming tools and secret references;
  the user enters credentials in the UI. Credentials are host-locked to and
  bound to the approved `base_url`; a changed URL fails closed.
- The vault is one XChaCha20-Poly1305 file whose key is a single OS keystore
  entry; it opens lazily and fails closed without a keystore.
- Guests can reach the UI port (`:3000`) too, so every route requires a
  session cookie from a host-only token; the daemon prints the login URL.

## Running the daemon locally

The daemon drives QEMU directly, and each VM's filesystem shares run over
**virtiofs**, which needs a `virtiofsd` binary on `PATH` in addition to
`qemu-system-x86_64` - both must be present when you launch it, e.g.:

```sh
nix-shell -p qemu virtiofsd --command 'env \
  GAVMO_BASE_IMAGE="$(readlink -f result/nixos.qcow2)" \
  cargo leptos watch --project gavmo'
```

Forgetting `virtiofsd` fails a VM boot immediately with `failed to spawn
"virtiofsd": No such file or directory`, before qemu ever starts (so no
`console.log` is written for that attempt either) - if you see that, this is
almost certainly why.

This repo's own `gavmo.nix` sets up a GAVMO VM for developing GAVMO itself
(Rust toolchain, `cargo-leptos`, `qemu`, `virtiofsd`). Builds and `cargo test`
work anywhere; actually booting VMs from inside that VM is nested
virtualization and needs the outer VM to expose KVM to be usable. The
`wasm-bindgen` CLI it installs must match the `wasm-bindgen` crate version in
`Cargo.lock` exactly, so bump `wasmBindgenVersion` in `gavmo.nix` whenever
the lock changes.

## Customizing your repo's VM with `gavmo.nix`

Each repo's VM runs GAVMO's fixed base system by default. If your repo's root
contains a `gavmo.nix` file, GAVMO merges it onto that base config (as a NixOS
module) and activates it **before** `opencode` starts - so the agent, and every
shell it spawns, run inside the environment you declare.

```nix
# gavmo.nix
{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.nodejs pkgs.jq ];
  virtualisation.docker.enable = true;
  users.users.agent.extraGroups = [ "docker" ];
}
```

Because it's a real NixOS module, `gavmo.nix` isn't limited to project tooling
(language runtimes, CLIs) the way a plain dev shell would be - it can also
declare system-level things a dev shell structurally can't: services/daemons
(like Docker above), kernel modules, sysctls, the system CA trust store, etc.
In a dedicated per-repo VM there's no multi-project contention to isolate
against, so `environment.systemPackages` doubles as "the project's tools" too.

A few things worth knowing:

- **It's additive**, evaluated via the NixOS module system on top of GAVMO's
  base config - list-valued options (like `environment.systemPackages`)
  concatenate; most other options just work as expected. Don't try to redefine
  options the base config already fixes - in particular, the `/workspace` and
  `/etc/gavmo/gcloud` virtiofs mounts and the `gavmo-opencode` service are
  owned by GAVMO and shouldn't be touched.
- **It runs as root, at VM-boot time**, via an in-guest `nixos-rebuild test`
  (activates immediately, never touches the bootloader) - so treat `gavmo.nix`
  itself as trusted, repo-owned configuration, not sandboxed input. This is a
  separate claim from what the *agent* can do once booted, though: the agent
  (`opencode` and every `bash` command it runs) runs as an unprivileged,
  non-root `agent` user with no `sudo`, confined by the kernel to writing only
  `/workspace`, its own home, and a private `/tmp` - see "Granting the agent
  more" for how `gavmo.nix` is also the lever for loosening that.
- **No `gavmo.nix`? Nothing changes.** The rebuild step is skipped entirely and
  the VM boots the base config as-is.
- **A broken `gavmo.nix` blocks the agent, deliberately.** If it fails to
  evaluate or build, `opencode` **will not start** - rather than silently
  falling back to the base config and letting the agent reach for tools
  (packages, services like Docker) it believes exist but doesn't have. This is
  detected within seconds (not a multi-minute boot timeout): the GAVMO control
  panel shows the workspace as failed with a **"View build log"** button
  surfacing the real `nixos-rebuild` error inline. Fix `gavmo.nix` and hit
  "Open" again. (Nothing else breaks - the overlay disk and your `/workspace`
  files are untouched.)
- **Disk headroom.** Everything `gavmo.nix` adds (packages, Docker images,
  browser bundles, build caches) lives on the workspace's persistent overlay
  disk; see `crates/vm/src/disk.rs` for its size.

See `examples/` for worked `gavmo.nix` recipes:

- `examples/docker/` - enables the Docker daemon and adds the agent to its
  group, so it can `docker build`/`docker run` with no `sudo`, plus a sample
  project CLI (`jq`). The worked version of "Granting the agent more".
- `examples/playwright/` - a `gavmo.nix` + `opencode.json` that install the
  Playwright MCP server and a Nix-provided Chromium, so the agent can drive a
  real browser - one action at a time, nothing written to the repo - to
  validate its own work (e.g. hitting a Nuxt dev server) instead of just
  reading source.

### Installing tools from outside Nix (npm, cargo, pip, ...)

Some tools aren't in nixpkgs, or not at the version you need, so they have to
be installed with a language package manager at boot. Three gotchas apply
(both `examples/playwright/gavmo.nix` and this repo's own `gavmo.nix` follow
this pattern):

- **Install from `systemd.services.gavmo-opencode.preStart`, not a new
  systemd unit.** `gavmo.nix` is applied mid-boot, after `gavmo-opencode`'s
  start job is already queued in the initial boot transaction, so a brand-new
  unit it adds is never pulled in and sits `enabled` but `inactive (dead)`.
  `preStart` runs every time as part of `gavmo-opencode`'s own startup, as the
  `agent` user with the same sandbox and environment. It's a list-valued
  `ExecStartPre` under the hood, so it merges with the base config's own;
  setting `serviceConfig.ExecStartPre` to a plain string instead fails the
  build.
- **End the install with `|| true` and make it idempotent.** `preStart` runs
  under `set -e`, so any failure (e.g. no network yet) would otherwise stop
  opencode from starting. Check the installed version first so later boots
  skip the reinstall. Install into the agent's home (`/home/agent/...`), the
  only writable prefix besides `/workspace` and `/tmp`.
- **Put the binary on `PATH` with a wrapper in `environment.systemPackages`**
  (e.g. `pkgs.writeShellScriptBin "tool" ''exec /home/agent/.../tool "$@"''`),
  not `environment.sessionVariables.PATH`: `gavmo-opencode` sources
  `/etc/profile`, which rebuilds `PATH` from the system profile and discards a
  `PATH` session variable. Other session variables reach the agent normally.

## Granting the agent more

The base image's default posture for the `agent` user (that runs `opencode`)
is strict: no `sudo`, no group memberships beyond its own, and a systemd
sandbox (`ProtectSystem=strict`, `NoNewPrivileges`, an empty
`CapabilityBoundingSet`, ...) that confines its writes to `/workspace`, its own
home, and a private `/tmp`. Most "the agent needs more power" asks are solved
declaratively - grant the specific thing a project's agent needs via its own
`gavmo.nix`, rather than the agent escalating privilege at runtime:

- **Docker / containers.** Enable the daemon and add `agent` to its group -
  the agent then runs `docker build`/`docker run` with no `sudo`; the
  root-run daemon does the privileged work.
  ```nix
  virtualisation.docker.enable = true;
  users.users.agent.extraGroups = [ "docker" ];
  ```
- **Binding low ports (<1024)**, e.g. testing on `:80`/`:443`:
  ```nix
  boot.kernel.sysctl."net.ipv4.ip_unprivileged_port_start" = 0;
  ```
- **A service for integration tests** (Postgres, Redis, ...) - declare it, it
  comes up as root at boot, the agent just connects to it:
  ```nix
  services.postgresql.enable = true;
  ```

For the rare case where a repo's actual work is systems programming
(containers, kernel modules, mounts, etc.) and the agent genuinely needs
ad-hoc root at runtime, `gavmo.nix` can re-enable `sudo` for it - this
requires overriding **both** of the sandbox options that would otherwise
block it, not just adding a sudoers rule:

```nix
{ lib, ... }:
{
  systemd.services.gavmo-opencode.serviceConfig = {
    # Sudo relies on a setuid-root exec gaining privileges it didn't start
    # with - NoNewPrivileges forbids exactly that outright.
    NoNewPrivileges = lib.mkForce false;
    # The bounding set is a hard ceiling no exec can exceed regardless of
    # uid; widen it to exactly what the workflow needs rather than clearing
    # it entirely.
    CapabilityBoundingSet = lib.mkForce [ "CAP_SYS_ADMIN" ];
  };
  security.sudo.extraRules = [
    {
      users = [ "agent" ];
      commands = [ { command = "ALL"; options = [ "NOPASSWD" ]; } ];
    }
  ];
}
```

Treat this as a last resort: granting broad `sudo` plus capabilities
re-widens exactly the hole the default posture exists to close, scoped only
to that one repo.

## Long-running shell commands

opencode's shell tool kills any command that runs longer than a configured
timeout (the upstream default is 2 minutes). The base image raises that
default to **5 minutes**, since installs/builds/test suites on a real
project routinely exceed 2 - a killed command surfaces in the agent's own
output as a `<shell_metadata>` "terminated command after exceeding timeout"
note, easy to mistake for the command having silently hung rather than
having been stopped.

If a specific repo's workflow still needs longer (a slow test suite, a big
build), raise it further via that repo's own `gavmo.nix`:

```nix
{ ... }:
{
  environment.sessionVariables.OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS = "1200000"; # 20 min
}
```

Or, for a one-off long command, just ask the agent to run it with a larger
timeout - the shell tool takes a `timeout` (in milliseconds) per call, with
no upper limit, so this needs no `gavmo.nix` change at all.

## Long-lived background processes (`gproc`)

Raising the timeout only helps commands that *eventually finish*. A process
that is **meant to run indefinitely** - a dev server (`nuxt dev`, `vite`), a
file/test watcher, a background worker - is a different problem: the shell
tool blocks on it until the timeout, then kills it, so there's no timeout
value that's ever right. The base image ships a small tool, **`gproc`**, that
the agent uses instead to start such a process *detached* from the shell
tool's lifetime (the bash call returns immediately while the process keeps
running) and with its logs in a known place:

```sh
gproc start web -- npm run dev     # start detached; returns right away
gproc list                         # name, pid, status, command, for each
gproc logs web                     # print its output (add -n N for last N lines)
gproc stop web                     # SIGTERM (then SIGKILL) the process group
gproc restart web                  # stop, then re-run its saved command
```

`gproc logs` is a one-shot snapshot, not a live stream - there is deliberately
no follow/`-f` mode, since the agent can't interrupt one and would hang until
the shell tool's timeout. To see newer output, just run `gproc logs` again.

Logs live on the host-visible virtiofs share at
`/workspace/.gavmo/proc/<name>/log`, so they survive a VM reset and are
inspectable from the host (gitignored, like `.gavmo/opencode`). It's a plain
`setsid`-based supervisor (not systemd - the agent is unprivileged and has no
user systemd instance), so a started process lives in opencode's own cgroup:
it survives individual shell calls but is cleaned up if opencode itself
restarts - exactly the lifetime a dev server wants. The agent is told about
`gproc` in its baked-in `/etc/gavmo/AGENTS.md`, so it should reach for it on
its own; if it still tries to run a dev server directly through the shell
tool, remind it to use `gproc`.
