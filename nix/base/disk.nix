{ lib, pkgs, modulesPath, ... }:

# --- M2 rebuild-target disk/boot shim ---------------------------------------
#
# This module exists ONLY for `nixosConfigurations.gavmo` in `flake.nix` (the
# in-guest `nixos-rebuild` target) - it must NEVER be added to the host-built
# qcow image's module list (`commonModules` in flake.nix already builds the
# image correctly without it).
#
# Why it's needed: the image is built via `nixos-generators`' `format =
# "qcow"`, whose format module (`formats/qcow.nix` upstream) injects the
# running system's root filesystem, bootloader, and console settings at
# image-build time - options that plain `configuration.nix` never declares,
# because `nixos-generators` normally supplies them. When we instead evaluate
# `nixosConfigurations.gavmo` as a standalone `nixosSystem` (to rebuild
# *inside* an already-running guest), nothing supplies those options unless we
# replicate them here. Without this, the in-guest rebuild fails to evaluate
# (no `fileSystems."/"`) or - worse - activates a system that disagrees with
# how the guest was actually booted.
#
# This is a deliberate copy of the subset of upstream's
# nix-community/nixos-generators `formats/qcow.nix` that affects a *running*
# system (not its build-only attributes like `system.build.qcow`,
# `formatAttr`, `fileExtension`, which are meaningless for an in-guest
# rebuild). If a future base-image format change moves to EFI
# (`qcow-efi`) or a different partition layout, this file must be updated to
# match - verified against the actual booted guest, not guessed from the
# image flake's `format` attribute.
{
  imports = [
    "${toString modulesPath}/profiles/qemu-guest.nix"
  ];

  fileSystems."/" = {
    device = "/dev/disk/by-label/nixos";
    autoResize = true;
    fsType = "ext4";
  };

  boot.growPartition = true;
  boot.kernelParams = [ "console=ttyS0" ];
  boot.loader.grub.device = lib.mkDefault "/dev/vda";
  boot.loader.timeout = 0;
}
