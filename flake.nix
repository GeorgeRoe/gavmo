{
  description = "GAVMO - George's Agent Virtual Machine Orchestrator";

  # The base image's own flake is an input, and nixpkgs follows its lock:
  # the in-guest rebuild target (`/etc/gavmo/system`) is `nix/base` with
  # `nix/base/flake.lock`, so the image must be built from that same
  # nixpkgs or every VM with a `gavmo.nix` would rebuild a whole system on
  # its first boot. Bump nixpkgs with `nix flake update --flake ./nix/base`,
  # then `nix flake update base` here. (Relative path inputs need Nix 2.26+.)
  inputs = {
    base.url = "path:./nix/base";
    nixpkgs.follows = "base/nixpkgs";
  };

  outputs =
    { nixpkgs, base, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      inherit (pkgs) lib;

      version = (lib.importTOML ./Cargo.toml).workspace.package.version;

      # The prebuilt release this revision installs by default (written by
      # .github/workflows/release.yml); absent until the first release.
      release = if builtins.pathExists ./release.json then lib.importJSON ./release.json else null;

      # The wasm-bindgen CLI must match the crate in Cargo.lock exactly.
      wasmBindgenVersion =
        (lib.findFirst (p: p.name == "wasm-bindgen") (throw "wasm-bindgen not in Cargo.lock")
          (lib.importTOML ./Cargo.lock).package
        ).version;
      wasmBindgenAttr = "wasm-bindgen-cli_" + lib.replaceStrings [ "." ] [ "_" ] wasmBindgenVersion;
      wasmBindgenCli =
        pkgs.${wasmBindgenAttr} or (throw ''
          gavmo: Cargo.lock uses wasm-bindgen ${wasmBindgenVersion}, but the pinned
          nixpkgs has no ${wasmBindgenAttr}. Bump nixpkgs (see the inputs comment in
          flake.nix) or pin the crate to a version nixpkgs packages.
        '');

      # Bifrost, the host-side model gateway: upstream's static Linux build.
      # Not in nixpkgs; pinned to the version tested in BIFROST_SPIKE.md.
      bifrost =
        let
          bifrostVersion = "2.5.0";
          tag = "transports/v${bifrostVersion}";
          license = pkgs.fetchurl {
            url = "https://raw.githubusercontent.com/maximhq/bifrost/${tag}/LICENSE";
            hash = "sha256-kQjG77cB8ZYUzyrI1kTxRZt9Z7xXD/oVVH5Nwuc0tNE=";
          };
          notices = pkgs.fetchurl {
            url = "https://raw.githubusercontent.com/maximhq/bifrost/${tag}/THIRD_PARTY_NOTICES.md";
            hash = "sha256-ghOS6OjC6CO/alKZgYlMP2L1oA5ItydkzqQrksV/IYE=";
          };
        in
        pkgs.stdenvNoCC.mkDerivation {
          pname = "bifrost-http";
          version = bifrostVersion;
          src = pkgs.fetchurl {
            url = "https://downloads.getmaxim.ai/bifrost/v${bifrostVersion}/linux/amd64/bifrost-http";
            hash = "sha256-1/lpfr9EP4+WHBx1HlgSMyCujHSRcOr2O2RV3U8f3i8=";
          };
          dontUnpack = true;
          # A static Go binary: nothing to patch, and stripping can break it.
          dontStrip = true;
          dontPatchELF = true;
          installPhase = ''
            runHook preInstall
            install -Dm755 $src $out/bin/bifrost-http
            install -Dm644 ${license} $out/share/doc/bifrost/LICENSE
            install -Dm644 ${notices} $out/share/doc/bifrost/THIRD_PARTY_NOTICES.md
            runHook postInstall
          '';
          meta = {
            description = "Bifrost LLM gateway (upstream static build)";
            homepage = "https://github.com/maximhq/bifrost";
            license = lib.licenses.asl20;
            sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
            platforms = [ system ];
            mainProgram = "bifrost-http";
          };
        };

      # The daemon and its control panel's site assets, built from source.
      gavmo-unwrapped = pkgs.rustPlatform.buildRustPackage {
        pname = "gavmo-unwrapped";
        inherit version;
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./crates
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [
          pkgs.cargo-leptos
          wasmBindgenCli
          # `wasm-opt`, for the release wasm bundle.
          pkgs.binaryen
          # The wasm32 target links with lld; nixpkgs' rustc has no rust-lld.
          pkgs.lld
        ];

        # cargo-leptos builds both halves (the ssr binary and the hydrate
        # wasm) in one go, so it replaces the cargo build hook.
        buildPhase = ''
          runHook preBuild
          export HOME=$TMPDIR
          cargo leptos build --release --cargo-frozen
          runHook postBuild
        '';
        # The test suites run in CI (ci.yml), with the toolchain they need.
        doCheck = false;
        installPhase = ''
          runHook preInstall
          install -Dm755 target/release/gavmo $out/bin/gavmo
          mkdir -p $out/share/gavmo
          cp -r target/site $out/share/gavmo/site
          runHook postInstall
        '';

        meta = {
          description = "GAVMO daemon (unwrapped: expects qemu and virtiofsd on PATH)";
          license = lib.licenses.mit;
          platforms = [ system ];
          mainProgram = "gavmo";
        };
      };

      base-image = base.packages.${system}.default;

      # The prebuilt release (see release.json): the static daemon tarball and
      # the zstd-compressed base image from the GitHub release.
      gavmo-bin-unwrapped = pkgs.stdenvNoCC.mkDerivation {
        pname = "gavmo-bin-unwrapped";
        inherit (release) version;
        src = pkgs.fetchurl release.tarball;
        # Static (musl) binary: nothing to patch.
        dontStrip = true;
        dontPatchELF = true;
        installPhase = ''
          runHook preInstall
          mkdir -p $out
          cp -r bin share $out/
          runHook postInstall
        '';
        meta = {
          description = "GAVMO daemon, prebuilt (unwrapped)";
          license = lib.licenses.mit;
          sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
          platforms = [ system ];
          mainProgram = "gavmo";
        };
      };

      base-image-bin =
        pkgs.runCommand "gavmo-base-image-${release.version}"
          {
            src = pkgs.fetchurl release.image;
            nativeBuildInputs = [ pkgs.zstd ];
            # Decompressing is faster than downloading the result from a cache.
            preferLocalBuild = true;
          }
          ''
            mkdir -p $out
            zstd -d --long=27 $src -o $out/nixos.qcow2
          '';

      # The command users run: the daemon with qemu and virtiofsd (nixpkgs',
      # which meet the virtiofsd >= 1.13 requirement) on its PATH, and
      # defaults for everything it otherwise needs from the environment. Each
      # is only a default, so the user's own environment still wins.
      wrap =
        { unwrapped, image }:
        pkgs.runCommand "gavmo-${unwrapped.version}"
          {
            nativeBuildInputs = [ pkgs.makeBinaryWrapper ];
            inherit (unwrapped) version;
            meta = unwrapped.meta // {
              description = "GAVMO: one QEMU/KVM NixOS VM per repository, running opencode";
              mainProgram = "gavmo";
            };
          }
          ''
            makeWrapper ${unwrapped}/bin/gavmo $out/bin/gavmo \
              --prefix PATH : ${
                lib.makeBinPath [
                  pkgs.qemu_kvm
                  pkgs.virtiofsd
                ]
              } \
              --set-default GAVMO_BASE_IMAGE ${image}/nixos.qcow2 \
              --set-default GAVMO_BIFROST ${bifrost}/bin/bifrost-http \
              --set-default LEPTOS_OUTPUT_NAME gavmo \
              --set-default LEPTOS_SITE_ROOT ${unwrapped}/share/gavmo/site \
              --set-default LEPTOS_SITE_PKG_DIR pkg \
              --set-default LEPTOS_ENV PROD
          '';

      gavmo = wrap {
        unwrapped = gavmo-unwrapped;
        image = base-image;
      };

      gavmo-bin = wrap {
        unwrapped = gavmo-bin-unwrapped;
        image = base-image-bin;
      };
    in
    {
      packages.${system} = {
        inherit
          bifrost
          base-image
          gavmo-unwrapped
          ;
        # Everything built from this checkout (slow: compiles the daemon,
        # opencode and the VM image locally).
        gavmo-from-source = gavmo;
        # Installs the prebuilt release named in release.json; before the
        # first release, falls back to building from source.
        default = if release != null then gavmo-bin else gavmo;
      }
      // lib.optionalAttrs (release != null) {
        inherit gavmo-bin base-image-bin;
      };

      # For working on GAVMO on a Nix host. (Inside a GAVMO VM, this repo's
      # gavmo.nix provides the same tools.)
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          pkgs.rustc
          pkgs.cargo
          pkgs.clippy
          pkgs.rustfmt
          pkgs.clang
          pkgs.lld
          pkgs.cargo-leptos
          wasmBindgenCli
          pkgs.binaryen
          pkgs.cargo-llvm-cov
          pkgs.firefox
          pkgs.geckodriver
          pkgs.qemu_kvm
          pkgs.virtiofsd
        ];
        GAVMO_BIFROST = "${bifrost}/bin/bifrost-http";
        shellHook = ''
          echo "gavmo dev shell. Base image: nix build .#base-image, then"
          echo "  GAVMO_BASE_IMAGE=\$(readlink -f result)/nixos.qcow2 cargo leptos watch --project gavmo"
        '';
      };
    };
}
