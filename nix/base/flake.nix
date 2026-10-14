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

      # Shared between the host-built qcow image and the in-guest M2 rebuild
      # target below, so "the repo's gavmo.nix merged onto the base" can never
      # silently diverge from what the image itself ships. See
      # `nix/spike-m2.md` for the full design rationale.
      commonModules = [ ./configuration.nix ];
    in
    {
      packages.${system}.default = nixos-generators.nixosGenerate {
        inherit system;
        format = "qcow";
        modules = commonModules;
      };

      # --- M2: per-repo environment customization --------------------------
      # The in-guest `nixos-rebuild` target. NOT built on the host - this is
      # evaluated/built *inside* the running guest (see
      # `systemd.services.gavmo-rebuild` in configuration.nix, once Phase 3
      # lands), against this same flake baked into the image at
      # /etc/gavmo/system.
      #
      # `./disk.nix` replicates the subset of `nixos-generators`' qcow format
      # module (`formats/qcow.nix`) that matters for a *running* system
      # (root fs, boot/grub, console) - deliberately NOT included in
      # `commonModules` above, since `nixos-generators` already injects its
      # own copy of those same options when building the image, and
      # redefining e.g. `fileSystems."/".device` again here would conflict.
      #
      # The repo's `/workspace/gavmo.nix` is imported conditionally (it may
      # not exist) via `builtins.pathExists`, which requires `--impure` at
      # rebuild time - safe here because the VM is the trust boundary, same
      # stance as the rest of this image's config (see `configuration.nix`'s
      # OpenCode permission comments).
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
