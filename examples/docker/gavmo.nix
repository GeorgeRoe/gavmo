# Example `gavmo.nix`: Docker for the agent (no `sudo`) plus a project CLI.
# Copy/merge into your repo root; see this directory's README.

{ pkgs, ... }:
{
  # Your project's own tools go here; `jq` is just an illustration.
  environment.systemPackages = [ pkgs.jq ];

  # The root-run daemon does the privileged work; group membership lets the
  # unprivileged agent reach its socket.
  virtualisation.docker.enable = true;
  users.users.agent.extraGroups = [ "docker" ];
}
