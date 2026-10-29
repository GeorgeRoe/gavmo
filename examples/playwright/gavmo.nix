# Example `gavmo.nix`: lets the agent drive headless Chromium through the
# Playwright MCP server's browser tools. Copy/merge this AND `opencode.json`
# into your repo root; see this directory's README for how it works.

{ pkgs, ... }:

let
  # Pinned only for reproducibility; it can't match nixpkgs' playwright
  # version anyway (README: "Version matching"). Bump freely.
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

  # Wrapper instead of a `PATH` session var, which `/etc/profile` discards
  # (main README: "Installing tools from outside Nix").
  playwrightMcpWrapper = pkgs.writeShellScriptBin "playwright-mcp" ''
    exec "${npmGlobalPrefix}/bin/playwright-mcp" "$@"
  '';
in
{
  environment.systemPackages = [
    pkgs.nodejs
    playwrightMcpWrapper
  ];

  environment.sessionVariables = {
    # Launch the Nix-built Chromium directly, bypassing Playwright's
    # revision-based browser lookup.
    PLAYWRIGHT_MCP_EXECUTABLE_PATH = chromiumExecutable;
    # Stop the `playwright` package's postinstall from downloading browsers
    # and validating the (unrecognized NixOS) host, which can hang the install.
    PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
    PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
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
