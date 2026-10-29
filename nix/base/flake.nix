{
  description = "GAVMO base NixOS VM image";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nixos-generators = {
      url = "github:nix-community/nixos-generators";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, nixos-generators, ... }:
    let
      system = "x86_64-linux";

      # Shared by the qcow image and the in-guest rebuild target so the rebuilt
      # system can't diverge from what the image ships.
      commonModules = [ ./configuration.nix ];
    in
    {
      packages.${system}.default = nixos-generators.nixosGenerate {
        inherit system;
        format = "qcow";
        modules = commonModules;
      };

      # In-guest `nixos-rebuild` target, built inside the running guest by
      # gavmo-rebuild against this flake copied to /etc/gavmo/system.
      # `./disk.nix` is not in `commonModules` because nixos-generators already
      # sets those options for the image and redefining them would conflict.
      # The optional repo `gavmo.nix` import needs `--impure`; acceptable since
      # the VM is the trust boundary.
      nixosConfigurations.gavmo = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = commonModules ++ [
          ./disk.nix
          (
            { lib, ... }:
            {
              imports = lib.optional (builtins.pathExists "/workspace/gavmo.nix") /workspace/gavmo.nix;
            }
          )
        ];
      };
    };
}
