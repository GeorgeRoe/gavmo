# GAVMO

> George's Agent Virtual Machine Orchestrator

## Installing

GAVMO runs on Linux x86-64 hosts with KVM and is installed with Nix, which
also brings the right qemu and virtiofsd. Nix works on any distribution
(Ubuntu, Fedora, Arch, ...), alongside the system package manager.

1. **Install Nix** 2.26 or newer with flakes enabled, if you don't have it.
   The [Determinate Systems installer](https://determinate.systems/nix-installer/)
   does both:
   ```sh
   curl -fsSL https://install.determinate.systems/nix | sh -s -- install
   ```
   Open a new shell afterwards.
2. **Make sure you can use KVM.** `ls -l /dev/kvm` should exist and be
   accessible to you; on most distributions that means being in the `kvm`
   group (`sudo usermod -aG kvm "$USER"`, then log out and back in).
3. **Install GAVMO:**
   ```sh
   nix profile install github:GeorgeRoe/gavmo
   ```
   This downloads the latest release: the daemon, the [Bifrost](https://github.com/maximhq/bifrost)
   model gateway, and the VM base image (about 570 MB compressed, 2.6 GB on
   disk).
4. **Run it:** `gavmo`. It prints a login URL for the control panel
   (`http://localhost:3000/login?token=...`; see "Logging in to the control
   panel"). Stop it with Ctrl-C.

Upgrade with `nix profile upgrade gavmo`. If a release changes the base
image, existing workspaces show "base image updated - reset needed" until
you Reset them; the repo and opencode sessions are kept.

What else the host needs:

- **An OS keyring** (Secret Service, e.g. GNOME Keyring or KWallet, as on
  any desktop) to store tool credentials. Without one GAVMO still runs, but
  can't save credentials for `gavmo.yaml` tools.
- **Models** come from Vertex AI through your Google Cloud credentials (see
  "Models" under Architecture): run `gcloud auth application-default login`,
  then start GAVMO with your project, e.g.
  `GAVMO_GCP_PROJECT=my-project gavmo`. Without that, VMs still boot and
  opencode opens, but model requests fail with "model gateway is not
  configured". On the first start with a project set, Bifrost downloads its
  model datasheets (about 27 MB) from the internet.

To build everything from source instead (compiles the daemon, opencode and
the VM image; slow), use `nix profile install github:GeorgeRoe/gavmo#gavmo-from-source`.
Releases are made by `.github/workflows/release.yml`; see TESTING.md.

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
  trusting the overlay's header.
- An overlay only fits the base it was created on. The base image is
  identified by its SHA-256 (hashed once per image; cached against its
  path/inode/size/mtime in `~/.local/share/gavmo/base-image.json`), and each
  overlay records the id it was made from in `vms/<hash>/base.json`. Booting
  an overlay against a different base is refused (status "base image
  updated - reset needed"); "Reset" rebuilds it from the current image,
  keeping the repo and opencode sessions. Overlays from before this record
  existed are assumed to match the current base the first time they boot.
- `<repo>/.gavmo/vm/` now holds only guest build output; VM files left there
  by older versions are unused (the daemon logs a hint) and can be deleted.
- virtiofs shares: repo -> `/workspace` (rw), MCP token dir ->
  `/etc/gavmo/mcp` (ro). No credentials are shared into VMs.
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
  host-served `gavmo` provider (see "Models"), allows all permissions (the VM is the trust boundary), and
  registers the `gavmo-tools` MCP server.

Models:

- Guests hold no model credentials. opencode's `gavmo` provider calls
  `http://10.0.2.2:7071/llm/anthropic/v1` with the boot's workspace token
  (the same one as the tool gateway). The daemon checks it, strips it and any
  `x-bf-*` header, and streams the request to its model gateway: a supervised
  [Bifrost](https://github.com/maximhq/bifrost) process holding the user's
  Google Application Default Credentials and doing the Vertex translation.
  Only `/llm/anthropic/v1/*` is forwarded, never Bifrost's admin API.
- Bifrost runs with a generated file-only config
  (`~/.local/share/gavmo/bifrost/config.json`, log `bifrost.log` beside
  it): no database, no request logging. In that mode it has no inbound auth
  and a readable admin API, so it binds `127.0.0.2`, which guests can't
  reach (QEMU maps their `10.0.2.2` only to host `127.0.0.1`). It dies with
  the daemon and is restarted (with backoff) if it exits.
- Configuration, for now from the environment:
  - `GAVMO_BIFROST`: absolute path to the `bifrost-http` binary.
  - `GAVMO_GCP_PROJECT`: the Google Cloud project for Vertex AI.
  - `GAVMO_GCP_REGION`: optional, default `global`.
  - `GAVMO_BIFROST_DATA_DIR`: optional dir with Bifrost's `datasheet.json`
    and `model-parameters.json`, so it starts without downloading them.
  Credentials are `application_default_credentials.json` in `GAVMO_GCLOUD_DIR`
  (default `~/.config/gcloud`), written by
  `gcloud auth application-default login`. Without the first two the daemon
  still runs, and model requests fail with the reason.
- The guest's models are declared in `nix/base/configuration.nix`
  (`provider.gavmo.models`), mapping opencode names to Bifrost ones
  (`vertex/claude-sonnet-5-5@default`).

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
  Request bodies there are capped at 64 KiB and must arrive within 10 s in
  total; handlers themselves are never timed out, since stop/reset tear
  VMs down inline.

## Running the daemon locally

The daemon drives QEMU directly, and each VM's filesystem shares run over
**virtiofs**, which needs a `virtiofsd` binary on `PATH` in addition to
`qemu-system-x86_64` - both must be present when you launch it. (The
installed `gavmo` command brings both.) On a Nix host, the flake's dev shell
has them, the Rust/Leptos toolchain and `GAVMO_BIFROST`:

```sh
nix build .#base-image
nix develop --command env \
  GAVMO_BASE_IMAGE="$(readlink -f result/nixos.qcow2)" \
  cargo leptos watch --project gavmo
```

Forgetting `virtiofsd` fails a VM boot immediately with `failed to spawn
"virtiofsd": No such file or directory`, before qemu ever starts (so no
`console.log` is written for that attempt either) - if you see that, this is
almost certainly why.

`virtiofsd` must be **1.13.0 or newer**, the first release with
`--translate-uid` and `--readonly`. Older ones, such as Ubuntu 24.04's
package, are refused before qemu starts with `virtiofsd X is too old`;
use nixpkgs' (the dev shell's, or `nix-shell -p virtiofsd`) or `cargo
install virtiofsd` instead.

This repo's own `gavmo.nix` sets up a GAVMO VM for developing GAVMO itself
(Rust toolchain, `cargo-leptos`, `wasm-bindgen-cli`, `binaryen`, headless
Firefox for the browser tests, `qemu`, `virtiofsd`) and cargo settings that fit
the VM's memory (see TESTING.md). Builds and `cargo test` work anywhere;
actually booting VMs from inside that VM is nested virtualization and needs
the outer VM to expose KVM to be usable. The `wasm-bindgen` CLI comes from
nixpkgs and must match the `wasm-bindgen` crate version in `Cargo.lock`
exactly; `gavmo.nix` reads the lock and warns in the rebuild log if they
drift. It likewise warns if nixpkgs' `rustc` differs from the version
pinned in `rust-toolchain.toml`, which CI uses. TESTING.md describes the test levels and how to run each.

## Logging in to the control panel

On start the daemon prints a login URL,
`http://localhost:3000/login?token=<token>`. The token is persisted in
`~/.local/share/gavmo/ui_token` and never leaves the host.

- Open the panel at **`localhost`**, not `127.0.0.1`. Every VM's opencode UI
  is served at `127.0.0.1:<port>`, and browsers share cookies across ports,
  so a session cookie set for `127.0.0.1` would be sent to every guest. A
  login at `127.0.0.1` is redirected to `localhost`; other host names are
  refused.
- Logging in creates a session: the cookie holds a random session id (never
  the token) and lasts 30 days. Sessions are kept in
  `~/.local/share/gavmo/sessions.json`, so they survive daemon restarts.
- "Log out everywhere" ends every session, in every browser.
- To replace the token, delete `ui_token` and restart; a new one is minted
  and printed. Existing sessions don't depend on it: also "Log out
  everywhere".

## Tool integrations (`gavmo.yaml`)

A repo can ask for host-side HTTP tools, which the agent calls through the
MCP gateway. The repo only names the tool and where it sends requests; you
enter the credential in the control panel (Tools, in the workspace's
modal), and it never enters the VM.

```yaml
# gavmo.yaml
tools:
  jira:
    type: http
    base_url: https://example.atlassian.net
    auth: basic            # basic | bearer | header
    secret: jira-token     # names the credential; not the credential
    description: Jira Cloud REST API
    instructions: Create an API token at id.atlassian.com.
    test_path: /rest/api/3/myself   # what the panel's "Test" button GETs
  search:
    type: http
    base_url: https://api.example.com
    auth: header
    header_name: X-Api-Key
    secret: search-key
```

`gavmo.yaml` is untrusted (the agent can edit it), so it is checked
strictly, and a file that fails any check is ignored entirely, with the
reason shown in the panel:

- Unknown keys are errors, so a typo doesn't silently disable something.
- So are duplicate keys (two `jira:` entries), YAML merge keys (`<<`),
  custom tags and more than one document. Anchors and aliases work, but
  parsing has limits sized to a tool list, so a file can't expand into
  more than 512 KiB of values.
- Tool ids are 1-64 of `[A-Za-z0-9_-]` and can't start with `gavmo_`; at
  most 64 tools.
- `base_url` must be http(s) with a host and no `user:pass@`, query or
  fragment.
- `secret` is 1-128 of `[A-Za-z0-9_.-]`.
- `header_name` is required with `auth: header` and allowed only there. It
  must be a valid header name, and not one that controls framing, routing or
  connections or carries other credentials (`Host`, `Content-Length`,
  `Cookie`, `Proxy-*`, ...).

Saving a credential approves both its destination (`base_url`) and how it
is sent (`auth`/`header_name`). If either later changes in `gavmo.yaml`,
the credential is withheld until you re-enter it: a VM started after the
change doesn't offer the tool, a running VM's calls are refused, and the
panel says what changed. Every request is locked
to the approved origin, and redirects are never followed: the agent gets
the status and `Location` and can request that path itself, if it's on the
same integration. Which tools a VM offers is fixed when it starts; the
credential itself is read fresh on every call.

## VM resources

Each workspace's modal has a **Resources** section: disk size, memory and
vCPUs. They are set in the panel, not in the repo, because they decide how
much of your machine a VM may use and the repo is writable by the agent.
They are saved in `workspaces.json`; an empty field means the default
(32 GiB disk, 4 GiB memory, 4 vCPUs).

- Memory and vCPUs can be changed any time and apply on the next start, up
  to the host's RAM and CPU count.
- The disk (16-1024 GiB) can only be changed while the VM is stopped, and
  only upward. On the next start the overlay is grown in place
  (`qemu-img resize`) and the guest grows its partition and filesystem to
  fit, so nothing is lost. The disk is sparse: its size is a cap, not space
  taken up front.
- To make a disk smaller, Reset the workspace (this deletes the VM disk,
  including `/nix` and anything installed outside `/workspace`); the next
  start creates a fresh disk at the saved size.

## Upgrading from earlier versions

One-time changes when moving to this version:

- **VM state moved out of the repo** to `~/.local/share/gavmo/vms/`, so
  each workspace's first boot builds a fresh disk. Afterwards delete the old
  `<repo>/.gavmo/vm/disk.qcow2` (tens of GB).
- **The control-panel token is rotated** on the first start, because
  earlier versions put it in the cookie. Use the newly printed URL.
- **Tool credentials must be re-entered once.** Credentials saved before
  destination and delivery binding are withheld until you confirm them.
- **The vault is upgraded to v2** (its header is now authenticated)
  automatically on first use; the original is kept as
  `secrets.vault.v1.bak`, which can be deleted once everything works.

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
  `/etc/gavmo/mcp` virtiofs mounts and the `gavmo-opencode` service are
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
  disk (32 GiB by default; grow it per workspace, see "VM resources").

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
