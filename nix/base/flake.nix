{
  description = "GAVMO base NixOS VM image (Milestone 0: plumbing proof)";

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
    in
    {
      packages.${system}.default = nixos-generators.nixosGenerate {
        inherit system;
        format = "qcow";
        modules = [ ./configuration.nix ];
      };
    };
}
