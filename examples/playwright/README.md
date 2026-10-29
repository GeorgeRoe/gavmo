# Example: browser automation (Playwright MCP) for web testing

This example lets a repo's agent drive headless Chromium through
[Playwright MCP](https://github.com/microsoft/playwright-mcp)'s browser
tools (`browser_navigate`, `browser_click`, `browser_type`,
`browser_fill_form`, `browser_snapshot`, `browser_take_screenshot`,
`browser_wait_for`, ...) - imperative, one action at a time against a
persistent session, with nothing written into the project - rather than the
agent writing and running one-off Playwright scripts. Useful for validating
a web app's own behavior (e.g. a Nuxt dev server) rather than just reading
its source.

Two files, both copied/merged into your repo root:

- [`gavmo.nix`](./gavmo.nix) - installs the Playwright MCP server and a
  Nix-patched, Chromium-only browser build into the VM.
- [`opencode.json`](./opencode.json) - registers that MCP server with
  opencode as a `local` server.

## Setup

1. Copy/merge [`gavmo.nix`](./gavmo.nix)'s contents into your repo's own
   `gavmo.nix` (see the main README's "Customizing your repo's VM with
   `gavmo.nix`" - it's additive NixOS-module config, not a template to fill
   in).
2. Copy/merge [`opencode.json`](./opencode.json) into your repo's own
   `opencode.json` at the repo root (this merges additively on top of
   GAVMO's managed `/etc/opencode/opencode.json` - a repo-level `mcp.*` entry
   is simply new, not an override of anything managed).
3. Reboot the VM so `gavmo-rebuild` picks up the new `gavmo.nix`.

## Where it actually runs

opencode itself runs *inside* the GAVMO VM (the `gavmo-opencode` service). A
`type: "local"` MCP server is spawned by opencode as its own child process -
so the Playwright MCP server **and** the Chromium it drives run entirely
inside the VM, as the same unprivileged `agent` user, under the same systemd
sandbox as everything else the agent does. Nothing new is exposed on the
host, and no extra port is opened.

## How it works

- **Browsers come from Nix** (`pkgs.playwright-driver.browsers`), prebuilt
  and patched for NixOS. A plain `npx playwright install` downloads
  unpatched binaries that fail to exec on NixOS (missing dynamic linker
  paths).
- **Version matching.** `@playwright/mcp` always tracks an alpha/nightly
  `playwright-core`, so no npm release of it will ever match the stable
  `pkgs.playwright-driver.version`. Instead of matching versions, the recipe
  sets `PLAYWRIGHT_MCP_EXECUTABLE_PATH` to the exact Nix-built Chromium
  binary, so Playwright launches it directly rather than looking it up by
  revision. `@playwright/mcp` is still pinned in `gavmo.nix`, purely so a boot
  doesn't silently pick up a new release; bump it independently of nixpkgs.
- **The MCP server is installed with `npm` at boot** into
  `/home/agent/.npm-global` and put on `PATH` with a wrapper, following the
  main README's "Installing tools from outside Nix" pattern.
- **`--no-sandbox` and `--headless`** (in `opencode.json`). Chromium's own
  sandbox can't initialize under GAVMO's default systemd sandbox for
  `gavmo-opencode` (`NoNewPrivileges`, `RestrictNamespaces`, an empty
  `CapabilityBoundingSet`). Disabling Chromium's sandbox instead of loosening
  the agent's keeps the agent isolated, and the VM itself is the real security
  boundary. The VM has no display, hence `--headless`.

## Using it

Once both files are in place and the VM has rebooted, ask the agent to use
the browser tools directly rather than writing a script, e.g.:

> Start the dev server with `gproc start web -- npm run dev`, then use the
> browser tools to open http://localhost:3000, click the "Login" link, and
> take a snapshot of the result. Don't write any Playwright scripts to disk.

(The dev server has to keep running while the browser tools drive it, which
is exactly what `gproc` is for - see the main README's "Long-lived background
processes". Running `npm run dev` directly through the shell tool would block
until it times out and is killed, so the page would never be up to navigate
to. The agent can check it came up with `gproc logs web` before navigating.)

> Use the browser tools to fill in the signup form at /signup with a test
> email and password, submit it, and confirm the success message appears.

If the agent still reaches for a script, it's worth explicitly reminding it
in the prompt (or your repo's own `AGENTS.md`) that it has Playwright browser
tools available and should prefer them over writing `.spec.ts` files for
ad-hoc checks.

Note: GAVMO's default agent starts every session in **Plan mode**
(read-only); most of the browser tools are not read-only (they interact with
a live page), so the agent may need to switch to Build mode to use them -
the same as for any other mutating action.

## Caveats

- **Chromium only.** Firefox and WebKit are excluded because WebKit in
  particular pulls in a GStreamer/GTK stack of multiple GiB. To add them, drop
  the `.override { withFirefox = false; withWebkit = false; }` in
  `gavmo.nix`, pass `--browser firefox`/`webkit` in `opencode.json`, and
  expect a much larger download.
- **The MCP install runs at every boot** as a `preStart` step of
  `gavmo-opencode` (idempotently - a fast version check once already
  installed) and needs working network. It can't fail `gavmo-opencode`'s own
  startup: if the install fails, the agent still starts, just without a
  working MCP server until the next boot.
- **Disk headroom.** Even Chromium-only, the browser bundle is a few hundred
  MB on top of `nodejs` and your project's own `node_modules`.

## Troubleshooting

The MCP server shows up red / fails to connect in opencode's UI (the on/off
toggle being greyed out there is normal - that's just opencode not letting
you manually toggle a *config-defined* server, unrelated to whether it's
actually working).

1. **Check the install actually ran and produced a binary:**
   ```
   ls -l /home/agent/.npm-global/bin/
   playwright-mcp --version
   ```
   If it's missing:
   - Make sure your `gavmo.nix` installs from
     `systemd.services.gavmo-opencode.preStart`. An install in a separate
     systemd unit (e.g. `playwright-mcp-install.service`) never runs: it
     shows `enabled` but `inactive (dead)` in `systemctl status`, with
     `-- No entries --` from `journalctl -u <unit> -b`.
   - Make sure `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD` and
     `PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS` are set. Without them the
     `playwright` package's postinstall tries to download browsers and
     validate the host OS, which can hang or fail the whole
     `npm install -g`.
   - To fix things without a reboot, run the `npm install -g
     @playwright/mcp@<version>` command from `gavmo.nix`'s `preStart` by hand
     (same env vars), then restart/toggle the MCP in opencode - the binary
     persists on the overlay disk either way.

2. **If the binary exists but opencode still shows it red**, check what
   opencode itself logged (needs the root console - the agent user can't
   read the system journal by default):
   ```
   journalctl -u gavmo-opencode --no-pager | grep -i -A6 -E "playwright|mcp"
   ```
   and try running the exact launch command by hand to see any error
   directly:
   ```
   playwright-mcp --headless --no-sandbox --browser chromium
   ```
   (it should start and sit silently waiting for a client; Ctrl-C to stop.)

3. **Chromium crashes immediately on launch** (common in constrained
   environments, due to a too-small `/dev/shm`): `--disable-dev-shm-usage`
   isn't a bare `playwright-mcp` CLI flag - it has to go through a config
   file's `browser.launchOptions.args`. Add a config file and point
   `opencode.json`'s command at it with `--config`:
   ```json
   // playwright-mcp.config.json
   { "browser": { "launchOptions": { "args": ["--disable-dev-shm-usage"] } } }
   ```
   ```json
   // opencode.json
   "command": ["playwright-mcp", "--headless", "--no-sandbox", "--browser", "chromium", "--config", "playwright-mcp.config.json"]
   ```
