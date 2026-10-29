# Example: Docker (containers) + a project CLI tool for the agent

This example gives a repo's agent **Docker** - it can `docker build` and
`docker run` with no `sudo` - plus an illustrative project CLI (`jq`). It's
the worked version of the pattern from the main README's "Granting the agent
more" section.

One file, copied/merged into your repo root:

- [`gavmo.nix`](./gavmo.nix) - enables the Docker daemon in the VM and adds
  the unprivileged `agent` user to the `docker` group, plus `jq` as an
  example tool.

## Setup

1. Copy/merge [`gavmo.nix`](./gavmo.nix)'s contents into your repo's own
   `gavmo.nix` (see the main README's "Customizing your repo's VM with
   `gavmo.nix`" - it's additive NixOS-module config, not a template to fill
   in).
2. Reboot the VM so `gavmo-rebuild` picks up the new `gavmo.nix`.

## How it works

- The Docker daemon runs as a **root** systemd service at VM boot - it does
  the privileged container work, so the agent never needs `sudo`.
- The `agent` user (which runs opencode and every shell it spawns) is added
  to the `docker` group so it can reach the daemon's socket; `docker build` /
  `docker run` then work unprivileged.
- `environment.systemPackages` is where a repo's own tools go. `jq` here is
  just illustrative - swap in whatever your project needs (language runtimes,
  CLIs, ...). In a dedicated per-repo VM there's no multi-project contention
  to isolate against, so `systemPackages` doubles as "the project's tools"
  with no devShell needed.

## Using it

Once booted, ask the agent to use Docker directly, e.g.:

> Build the Dockerfile in this repo, run it, and curl the service it exposes.

The first `docker` command after boot may pause briefly while the daemon
finishes starting.

## Caveats

- **Disk headroom.** Enabling Docker pulls its daemon + dependencies into the
  image, and any images you build/pull live on the guest's persistent overlay
  disk (see `crates/vm/src/disk.rs`). Make sure there's room.
- **Group membership applies after the rebuild.** A fresh boot picks up the
  `docker` group cleanly; processes started before the rebuild applied won't
  have it.
