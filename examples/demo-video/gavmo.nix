# Example `gavmo.nix`: narrated demo videos of a web app, recorded from headless
# Chromium. Builds on `examples/playwright/gavmo.nix` (the browser tools) and adds
# ffmpeg, a slimmed-down Piper text-to-speech and one pinned Piper voice. The
# `demo-video` skill (.opencode/skills/demo-video) does the rest. See README.md.

{ pkgs, ... }:

let
  # --- From examples/playwright/gavmo.nix (see its README) ------------------

  # Pinned only for reproducibility; it can't match nixpkgs' playwright
  # version anyway. Bump freely.
  playwrightMcpVersion = "0.0.83";

  # Chromium plus its headless-shell binary only. Not the `browsers-chromium`
  # passthru: it drops headless-shell, which `headless: true` launches use.
  chromiumBrowsers = pkgs.playwright-driver.browsers.override {
    withFirefox = false;
    withWebkit = false;
  };

  # Path layout mirrors nixpkgs' pkgs/development/web/playwright/chromium.nix.
  chromiumRevision = pkgs.playwright-driver.browsersJSON.chromium.revision;
  chromeDir =
    {
      x86_64-linux = "chrome-linux64";
      aarch64-linux = "chrome-linux-arm64";
    }
    .${pkgs.stdenv.hostPlatform.system};
  chromiumExecutable = "${chromiumBrowsers}/chromium-${chromiumRevision}/${chromeDir}/chrome";

  # The agent's home is writable; the Nix store isn't.
  npmGlobalPrefix = "/home/agent/.npm-global";

  # Wrapper instead of a `PATH` session var, which `/etc/profile` discards.
  playwrightMcpWrapper = pkgs.writeShellScriptBin "playwright-mcp" ''
    exec "${npmGlobalPrefix}/bin/playwright-mcp" "$@"
  '';

  # --- Demo video additions ---------------------------------------------------

  # Piper (piper1-gpl, the Python rewrite) without its optional extras. The
  # default nixpkgs build includes model *training* (PyTorch, Lightning, ...),
  # Japanese text support, an HTTP server and alignment tools: ~1.7 GB more for
  # features speaking doesn't use (it only needs onnxruntime + numpy). This
  # variant isn't on cache.nixos.org, so it builds locally (a small C extension;
  # its espeak-ng dependency is shared with the cached build). If that build
  # ever fails, replace `piper` with plain `pkgs.piper-tts` below.
  piper =
    (pkgs.piper-tts.override {
      withTrain = false;
      withJapanese = false;
      withHTTP = false;
      withAlignment = false;
    }).overridePythonAttrs
      (_: {
        # The upstream import check includes `piper.train`, and the test suite
        # covers training; both need the extras we removed.
        pythonImportsCheck = [ "piper" ];
        doCheck = false;
      });

  # One Piper voice, pinned to a commit of rhasspy/piper-voices on Hugging Face.
  # Voice licence: the training data is CC BY-SA 4.0 (OpenSLR 83); see README.
  # To use a different voice, change the name, path and both hashes (README:
  # "Changing the voice").
  piperVoiceName = "en_GB-northern_english_male-medium";
  piperVoiceUrl = "https://huggingface.co/rhasspy/piper-voices/resolve/5ec43b186ef915a195d3ddbcfd9916e76c503c3f/en/en_GB/northern_english_male/medium";
  piperVoice = pkgs.linkFarm "piper-voice-${piperVoiceName}" [
    {
      name = "${piperVoiceName}.onnx";
      path = pkgs.fetchurl {
        url = "${piperVoiceUrl}/${piperVoiceName}.onnx";
        hash = "sha256-QRmdVXdsFCwVHgGM93vqgMpfAbe4NPgc14HfW3Ub8vs=";
      };
    }
    {
      # Piper looks for the config next to the model, as `<model>.json`.
      name = "${piperVoiceName}.onnx.json";
      path = pkgs.fetchurl {
        url = "${piperVoiceUrl}/${piperVoiceName}.onnx.json";
        hash = "sha256-AVnUgWCSOOR+MRFEltAEuG17SdH5WoGt+25f91jZkBM=";
      };
    }
  ];
in
{
  environment.systemPackages = [
    pkgs.nodejs
    playwrightMcpWrapper

    # Full ffmpeg: H.264 (libx264) + AAC into .mp4. Playwright's bundled
    # ffmpeg only writes silent VP8 .webm.
    pkgs.ffmpeg

    piper
  ];

  environment.sessionVariables = {
    # Launch the Nix-built Chromium directly, bypassing Playwright's
    # revision-based browser lookup.
    PLAYWRIGHT_MCP_EXECUTABLE_PATH = chromiumExecutable;
    # Stop the `playwright` package's postinstall from downloading browsers
    # and validating the (unrecognized NixOS) host, which can hang the install.
    PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
    PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";

    # Playwright's video recording (the browser server's `browser_start_video`
    # tool) needs Playwright's own ffmpeg build, which it looks for in
    # ~/.cache/ms-playwright by default. The Nix browser bundle ships it next
    # to Chromium.
    PLAYWRIGHT_BROWSERS_PATH = "${chromiumBrowsers}";

    # The voice the demo-video skill narrates with.
    DEMO_VIDEO_PIPER_VOICE = "${piperVoice}/${piperVoiceName}.onnx";
  };

  # preStart, not a separate unit, and `|| true` so a failed install never
  # blocks opencode (main README: "Installing tools from outside Nix").
  systemd.services.gavmo-opencode.preStart = ''
    installed="$(${npmGlobalPrefix}/bin/playwright-mcp --version 2>/dev/null || true)"
    if [ "$installed" != "Version ${playwrightMcpVersion}" ]; then
      HOME=/home/agent npm_config_prefix=${npmGlobalPrefix} \
        ${pkgs.nodejs}/bin/npm install -g "@playwright/mcp@${playwrightMcpVersion}" || true
    fi
  '';
}
