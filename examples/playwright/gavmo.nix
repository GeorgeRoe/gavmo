# Example `gavmo.nix`: lets the agent drive headless Chromium through the
# Playwright MCP server's browser tools (navigate/click/type/snapshot/...) -
# imperative, one action at a time, with nothing written to the repo - to
# validate web work in-VM (e.g. hitting a Nuxt dev server). Copy/merge this
# AND `opencode.json` from this same directory into your repo's root - see
# the main README's "Customizing your repo's VM with `gavmo.nix`" section
# for how `gavmo.nix` merging works (additive NixOS-module config, not a
# template), and this directory's own README for the `opencode.json` half.
#
# How it works
# ------------
# - The MCP server (`@playwright/mcp`) is installed globally via `npm`, and
#   registered as a `type: "local"` MCP server in `opencode.json`. Since
#   opencode itself runs *inside* this VM (the `gavmo-opencode` service),
#   a "local" MCP server is spawned as opencode's own child process - so the
#   MCP server AND the Chromium it drives run entirely inside the VM, as the
#   same unprivileged `agent` user, under the same systemd sandbox. Nothing
#   new is exposed on the host.
# - Browsers come from `pkgs.playwright-driver.browsers` - prebuilt and
#   patched for NixOS. A plain `npx playwright install` would instead
#   download unpatched binaries that simply fail to exec on NixOS (missing
#   dynamic linker paths).
# - `.browsers` defaults to Chromium *and* Firefox *and* WebKit - WebKit in
#   particular drags in an entire GStreamer/GTK stack, multiple GiB
#   unpacked. Scoped down here to Chromium-only via `.override { withFirefox
#   = false; withWebkit = false; }` (confirmed against nixpkgs'
#   `pkgs/development/web/playwright/driver.nix`), rather than the built-in
#   `pkgs.playwright-driver.browsers-chromium` passthru - that passthru also
#   sets `withChromiumHeadlessShell = false`, but Playwright's default
#   `headless: true` launches actually use the separate headless-shell
#   binary, not full Chromium, so disabling it would break exactly the
#   headless validation this recipe is for.
# - Version matching is handled differently here than a plain Playwright CLI
#   install would need, and deliberately so: `@playwright/mcp` has never
#   published a release pinned to a *stable* `playwright-core` version in
#   its history (checked npm's registry metadata across every published
#   version) - it always tracks an alpha/nightly build of the next
#   playwright-core release. That means there is no npm version of
#   `@playwright/mcp` that will ever exactly match the stable
#   `pkgs.playwright-driver.version` nixpkgs ships. So instead of trying to
#   pin versions together, this recipe sets `PLAYWRIGHT_MCP_EXECUTABLE_PATH`
#   to the exact Nix-built Chromium binary - this makes Playwright launch
#   that binary directly rather than looking it up by revision folder under
#   `PLAYWRIGHT_BROWSERS_PATH`, sidestepping the version-matching problem
#   entirely. `@playwright/mcp` is still pinned to a fixed npm version below,
#   purely for reproducibility (so a boot doesn't silently pick up a new
#   release) - bump it independently of nixpkgs whenever you like.
# - The installed MCP binary lives outside the Nix store (under the agent's
#   own home, since the store and rest of the system are read-only to it -
#   see the README's sandboxing notes), so it's exposed on `PATH` via a
#   small wrapper package added to `environment.systemPackages`, NOT via
#   `environment.sessionVariables.PATH` - the latter doesn't work here,
#   because `gavmo-opencode`'s startup script sources `/etc/profile`, which
#   rebuilds `PATH` from scratch from the system profile
#   (`/run/current-system/sw/bin`, i.e. `systemPackages`) and would discard
#   any `PATH` session variable instead. Other (non-`PATH`) session
#   variables, like `PLAYWRIGHT_MCP_EXECUTABLE_PATH` below, don't have this
#   problem and reach the agent (and the MCP server it spawns) normally.
# - Chromium's own internal sandbox can't initialize under GAVMO's default
#   systemd sandbox for `gavmo-opencode` (`NoNewPrivileges`,
#   `RestrictNamespaces`, an empty `CapabilityBoundingSet` - see the README's
#   "Granting the agent more"). Rather than loosening that sandbox (which
#   would weaken the agent's own isolation), `opencode.json` launches the MCP
#   server with `--no-sandbox` instead - the VM itself is already the real
#   security boundary, so this costs nothing in practice. `--headless` is
#   required too: the VM has no display.
# - The install itself runs as a `preStart` step on `gavmo-opencode` itself
#   (see that option below), NOT a separate systemd unit. This was a real
#   bug in an earlier version of this recipe: GAVMO applies `gavmo.nix` via
#   an in-guest `nixos-rebuild test` *mid-boot* (see `gavmo-rebuild` in the
#   base config), and `gavmo-opencode`'s own start job is already enqueued
#   in the *initial* boot transaction before that merged generation (where a
#   separate install unit would first exist) is even loaded. A brand-new
#   unit added this way - regardless of `wantedBy` target - never gets
#   pulled into that already-in-flight transaction, so it would sit
#   `enabled` yet permanently `inactive (dead)`, confirmed in practice.
#   `preStart` instead becomes an intrinsic step of `gavmo-opencode`'s own
#   startup (via `serviceConfig.ExecStartPre`), which *does* run every time
#   using the merged unit definition - no separate unit, no transaction gap.
#
# Using it
# --------
# Once booted (and with `opencode.json` from this directory also copied to
# your repo root - see this directory's README), the agent has a `playwright`
# MCP server with browser tools: `browser_navigate`, `browser_click`,
# `browser_type`, `browser_fill_form`, `browser_snapshot`,
# `browser_take_screenshot`, `browser_wait_for`, etc. - each a single,
# stateful tool call against a persistent browser session, not a script file.
# Ask the agent to use these tools directly, e.g.: "start the dev server,
# then use the browser tools to open http://localhost:3000 and check the
# homepage renders - don't write any Playwright scripts to disk."
#
# Caveats
# -------
# - The global `npm install` needs working network and runs at every boot
#   (idempotently - it's a fast version check once already installed, not a
#   re-download). It deliberately ends in `|| true` so a failure (e.g. no
#   network yet) can NEVER fail `gavmo-opencode` itself starting - the
#   agent still starts, just without a working MCP server until the next
#   boot/restart.
# - This installs Chromium only (see the header comment). For Firefox/WebKit
#   too, drop the `.override { withFirefox = false; withWebkit = false; }`
#   below, add `--browser firefox`/`webkit` to `opencode.json`, and expect a
#   much larger image/download as a result.
# - Even Chromium-only, the browser bundle is a few hundred MB on top of
#   `nodejs` and your project's own `node_modules` - make sure the guest's
#   disk has enough headroom (GAVMO's overlay disk; see `crates/vm/src/disk.rs`).

{ pkgs, ... }:

let
  # Pinned purely for install reproducibility - see the header comment for
  # why this does NOT need to match `pkgs.playwright-driver.version` (it
  # can't - `@playwright/mcp` never pins a stable `playwright-core`).
  # Bump independently whenever you want a newer MCP server.
  playwrightMcpVersion = "0.0.83";

  # Chromium (+ its dedicated headless-shell binary) only - see the header
  # comment for why not Firefox/WebKit, and why not the `browsers-chromium`
  # passthru.
  chromiumBrowsers = pkgs.playwright-driver.browsers.override {
    withFirefox = false;
    withWebkit = false;
  };

  # The exact Chromium binary inside the Nix browser bundle above, handed to
  # the MCP server directly (see the header comment on version matching).
  # Derived the same way nixpkgs' own `driver.nix`/`chromium.nix` lay out the
  # bundle: a `linkFarm` entry named `chromium-<revision>`, containing a
  # `chrome-linux64/chrome` binary on x86_64 (`chrome-linux-arm64` on
  # aarch64) - see `pkgs/development/web/playwright/chromium.nix`.
  chromiumRevision = pkgs.playwright-driver.browsersJSON.chromium.revision;
  chromeDir =
    {
      x86_64-linux = "chrome-linux64";
      aarch64-linux = "chrome-linux-arm64";
    }
    .${pkgs.stdenv.hostPlatform.system};
  chromiumExecutable = "${chromiumBrowsers}/chromium-${chromiumRevision}/${chromeDir}/chrome";

  # Writable install location: `/home/agent` is the one place besides
  # `/workspace` the agent's own sandbox can write (see the README), and npm
  # needs a real writable prefix for a global install.
  npmGlobalPrefix = "/home/agent/.npm-global";

  # Exposes the npm-installed `playwright-mcp` binary on the system PATH
  # (see the header comment for why this can't just be a `PATH` session
  # variable). `systemPackages` entries land in `/run/current-system/sw/bin`,
  # which `/etc/profile` does put on `gavmo-opencode`'s PATH.
  playwrightMcpWrapper = pkgs.writeShellScriptBin "playwright-mcp" ''
    exec "${npmGlobalPrefix}/bin/playwright-mcp" "$@"
  '';
in
{
  environment.systemPackages = [
    pkgs.nodejs
    playwrightMcpWrapper
  ];

  # Points the MCP server at the exact Nix-built Chromium binary, bypassing
  # Playwright's own revision-based browser lookup entirely - see the header
  # comment on version matching for why this (rather than
  # `PLAYWRIGHT_BROWSERS_PATH` alone) is the right mechanism here.
  environment.sessionVariables = {
    PLAYWRIGHT_MCP_EXECUTABLE_PATH = chromiumExecutable;
    PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
    PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
  };

  # Installs the pinned `@playwright/mcp` into the writable npm prefix above,
  # as a `preStart` step of `gavmo-opencode` itself - see the header comment
  # ("The install itself runs...") for why this, rather than a separate
  # systemd unit, is what actually runs reliably every boot under GAVMO's
  # in-guest rebuild flow.
  #
  # `preStart` compiles down to `serviceConfig.ExecStartPre = [ <generated
  # script> ];` - a *list* value. This matters because `serviceConfig`
  # options merge via nixpkgs' `unitOption` type (see
  # `nixos/lib/systemd-unit-options.nix`): if ANY module's definition for a
  # given key is a list, ALL definitions (including the base config's own
  # plain-string `ExecStartPre` that creates `/home/agent/.local/share`
  # etc.) get coerced to lists and concatenated - both run, in some order,
  # as independent `ExecStartPre=` lines (harmless here since they touch
  # different paths). Setting a plain *string* here instead would instead
  # hit `unitOption`'s other merge branch, which requires exact equality
  # with the base's string and would hard-fail the build.
  #
  # `ExecStartPre` entries inherit the same `User`/`Group` (`agent`),
  # sandbox (`ReadWritePaths` already includes `/home/agent`), and
  # `Environment` (so the `PLAYWRIGHT_SKIP_*` vars from `sessionVariables`
  # above are already present here for free) as `gavmo-opencode`'s main
  # process - no need to redeclare them, unlike a standalone unit would.
  #
  # The trailing `|| true` is what makes a failure here non-fatal to
  # `gavmo-opencode` itself starting (see the "Caveats" note above) -
  # `preStart`'s generated script runs under `set -e`, so without it any
  # failed command (a network hiccup, say) would fail this `ExecStartPre`
  # and, with it, the whole service.
  systemd.services.gavmo-opencode.preStart = ''
    installed="$(${npmGlobalPrefix}/bin/playwright-mcp --version 2>/dev/null || true)"
    if [ "$installed" != "Version ${playwrightMcpVersion}" ]; then
      HOME=/home/agent npm_config_prefix=${npmGlobalPrefix} \
        ${pkgs.nodejs}/bin/npm install -g "@playwright/mcp@${playwrightMcpVersion}" || true
    fi
  '';
}
