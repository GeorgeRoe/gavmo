{ lib, pkgs, modulesPath, ... }:

# Disk/boot settings for the in-guest rebuild target (`nixosConfigurations.gavmo`)
# only. Never add this to the image's `commonModules`: nixos-generators' qcow
# format already injects these options there, and redefining them conflicts.
#
# A standalone `nixosSystem` has nobody supplying root fs, bootloader and
# console, so this copies the runtime-relevant subset of nixos-generators'
# `formats/qcow.nix`. Without it the rebuild fails to evaluate or activates a
# system that disagrees with how the guest actually booted. If the image format
# or partition layout changes (e.g. qcow-efi), update this to match.
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
