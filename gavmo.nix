{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.jq ];
  virtualisation.docker.enable = true;

  environment.variables.GAVMO_TEST_VAR = "hello-from-variables";
  environment.sessionVariables.GAVMO_TEST_SESSION_VAR = "hello-from-sessionVariables";
}