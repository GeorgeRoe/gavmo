# Example `gavmo.nix`: gives the agent Docker (build/run containers with no
# `sudo`) plus a project CLI tool (`jq`). Copy/merge this into your repo's
# root - see the main README's "Customizing your repo's VM with `gavmo.nix`"
# section for how `gavmo.nix` merging works (additive NixOS-module config, not
# a template to fill in).
#
# How it works
# ------------
# - `virtualisation.docker.enable = true` brings up the Docker daemon as a
#   root-run systemd service at VM boot - it does the privileged container
#   work, so the agent never needs `sudo` itself.
# - `users.users.agent.extraGroups = [ "docker" ]` adds the unprivileged
#   `agent` user (which runs opencode and every shell it spawns) to the
#   `docker` group, so `docker build` / `docker run` work without `sudo`.
#   Without this, the agent can't reach the daemon's socket. See the main
#   README's "Granting the agent more" section for the general pattern.
# - `environment.systemPackages` is where a repo's own tools go - `jq` here is
#   just an illustrative CLI. In a dedicated per-repo VM there's no
#   multi-project contention to isolate against, so `systemPackages` doubles
#   as "the project's tools" (language runtimes, CLIs, ...) - no devShell
#   needed.
#
# Using it
# --------
# Once booted, ask the agent to use Docker directly, e.g.: "build the
# Dockerfile in this repo and run it, then curl the service it exposes." The
# first `docker` command after boot may pause briefly while the daemon
# finishes starting.
#
# Caveats
# -------
# - Enabling Docker pulls its daemon + dependencies into the image; the
#   persistent overlay disk (see `crates/vm/src/disk.rs`) needs headroom for
#   that plus any images you pull/build.
# - Group membership takes effect for processes started after the rebuild
#   applies - a fresh boot picks it up cleanly.

{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.jq ];

  virtualisation.docker.enable = true;
  users.users.agent.extraGroups = [ "docker" ];
}
