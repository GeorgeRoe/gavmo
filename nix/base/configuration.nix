{ lib, pkgs, config, ... }:

let
  # The host daemon's guest-facing gateway (MCP tools at /mcp, models at
  # /llm). 10.0.2.2 is the host under QEMU user-mode networking. The port is
  # the daemon's `--mcp-port`, which it writes into each boot's token.env
  # (`GAVMO_HOST_PORT`, next to `GAVMO_MCP_TOKEN`), so the image doesn't have to
  # match it. opencode substitutes `{env:...}` anywhere in a config string.
  hostGateway = "http://10.0.2.2:{env:GAVMO_HOST_PORT}";

  # opencode web UI source patches, verified against opencode v1.18.35.
  #
  # Default-on UI toggles: these Settings > General toggles exist only in the
  # browser's localStorage (seeded from `defaultSettings.general` in
  # packages/app/src/context/settings.tsx); there is no config-file equivalent.
  # We patch the source before the vite build because minified names churn
  # between releases. Each substitution must match exactly once or the build
  # fails. `showCustomAgents` also needs its first-run auto-init forced on,
  # since that effect overwrites the default; later manual toggles still stick.
  #
  # Workspace auto-registration: opencode's client keeps the project list in
  # localStorage and won't list a non-git /workspace, so prior sessions vanish
  # from the home page. We make `DirectoryDataProvider` (where gavmo's "Open
  # UI" link lands) call `server.projects.open`/`touch` for local directories.
  # Together with the stable host port in crates/gavmo/src/registry.rs (same
  # origin -> same localStorage), this keeps sessions across VM restarts.
  opencodeWeb =
    assert lib.assertMsg (pkgs.opencode.version == "1.18.35") ''
      gavmo: opencode changed from 1.18.35, the version the web-UI source
      patches below (directory-layout.tsx project auto-registration,
      settings.tsx default flips) were verified against. Re-check both
      files' anchors at https://github.com/anomalyco/opencode for the new
      version, update the patches if they moved, then bump the version
      string in this assert (nix/base/configuration.nix).
    '';
    pkgs.opencode.overrideAttrs (old: {
      nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.perl ];

      postPatch = (old.postPatch or "") + ''
        settings=$(find . -type f -path '*/packages/app/src/context/settings.tsx')
        settingsCount=$(printf '%s\n' "$settings" | grep -c .)
        if [ "$settingsCount" != "1" ]; then
          echo "gavmo: expected exactly one" \
               "packages/app/src/context/settings.tsx in the opencode source," \
               "found $settingsCount: [$settings] - opencode's web UI source" \
               "layout has changed; update the opencodeWeb override in" \
               "nix/base/configuration.nix" >&2
          exit 1
        fi

        dirLayout=$(find . -type f -path '*/packages/app/src/pages/directory-layout.tsx')
        dirLayoutCount=$(printf '%s\n' "$dirLayout" | grep -c .)
        if [ "$dirLayoutCount" != "1" ]; then
          echo "gavmo: expected exactly one" \
               "packages/app/src/pages/directory-layout.tsx in the opencode" \
               "source, found $dirLayoutCount: [$dirLayout] - opencode's web" \
               "UI source layout has changed; update the opencodeWeb" \
               "override in nix/base/configuration.nix" >&2
          exit 1
        fi

        # Substitute $1 -> $2 in file $3; fails unless $1 occurs exactly once.
        patchOnce() {
          FROM="$1" TO="$2" FILE="$3" perl -0777 -i -pe '
            my $from = $ENV{FROM};
            my $to   = $ENV{TO};
            my $count = () = /\Q$from\E/g;
            if ($count != 1) {
              die "gavmo: expected exactly one occurrence of \"$from\" in " .
                  "$ENV{FILE}, found $count instead - opencode'"'"'s web UI " .
                  "source has likely changed upstream; update the " .
                  "opencodeWeb override in nix/base/configuration.nix\n";
            }
            s/\Q$from\E/$to/;
          ' "$3"
        }

        patchOnce 'showFileTree: false,' 'showFileTree: true,' "$settings"
        patchOnce 'showSearch: false,' 'showSearch: true,' "$settings"
        patchOnce 'showStatus: false,' 'showStatus: true,' "$settings"

        # The second patch forces the first-run auto-init to resolve on.
        patchOnce 'showCustomAgents: false,' 'showCustomAgents: true,' "$settings"
        patchOnce 'return existing || previousVersion !== undefined' 'return true' "$settings"

        patchOnce 'showReasoningSummaries: false,' 'showReasoningSummaries: true,' "$settings"
        patchOnce 'shellToolPartsExpanded: false,' 'shellToolPartsExpanded: true,' "$settings"
        patchOnce 'editToolPartsExpanded: false,' 'editToolPartsExpanded: true,' "$settings"

        # Workspace auto-registration in DirectoryDataProvider.
        patchOnce \
          'import type { ServerConnection } from "@/context/server"' \
          'import type { ServerConnection } from "@/context/server"
import { useServer } from "@/context/server"' \
          "$dirLayout"

        patchOnce \
          'const serverSync = useServerSync()' \
          'const serverSync = useServerSync()
  const server = useServer()' \
          "$dirLayout"

        # Skips drafts and remote servers, mirroring the sibling effect.
        patchOnce \
          '  createResource(' \
          '  createEffect(() => {
    if (props.draftID || props.server?.()) return
    const d = directory()
    if (!d) return
    server.projects.open(d)
    server.projects.touch(d)
  })

  createResource(' \
          "$dirLayout"
      '';
    });

  # `opencode web` tries to open a browser on startup; a missing `xdg-open`
  # (ENOENT) can crash it into a restart loop. Prepended to PATH only in
  # gavmo-opencode's script, not installed system-wide.
  xdgOpenShim = pkgs.writeShellScriptBin "xdg-open" "exit 0";

  # gproc: runs long-lived processes (dev servers, watchers) detached from
  # opencode's shell tool, which blocks on and eventually kills its commands.
  #
  # Uses setsid + pidfile rather than systemd: there's no `systemd --user` for
  # the agent and gavmo-opencode's sandbox blocks `systemd-run`. Children stay
  # in gavmo-opencode's cgroup, so they die with opencode, but their own
  # session/process group survives the shell tool's group-kill on timeout.
  # State lives in /workspace/.gavmo/proc so it is host-visible.
  # Installed via systemPackages: gavmo-opencode rebuilds PATH from
  # /etc/profile, so a PATH session variable would not reach the agent.
  gproc = pkgs.writeShellApplication {
    name = "gproc";
    runtimeInputs = [ pkgs.coreutils pkgs.util-linux pkgs.procps ];
    text = ''
      root="''${GPROC_ROOT:-/workspace/.gavmo/proc}"

      usage() {
        cat >&2 <<'EOF'
      gproc - manage long-lived processes (dev servers, watchers, …) that
      must outlive a single shell-tool call.

      Usage:
        gproc start <name> -- <cmd> [args...]   start a detached process
        gproc list                              list managed processes
        gproc logs <name> [-n N]                print its log (or last N lines)
        gproc stop <name>                       stop it (SIGTERM, then SIGKILL)
        gproc restart <name>                    stop, then start its saved cmd
        gproc rm <name>                         remove a stopped process's state

      Logs live at <root>/<name>/log (root: $GPROC_ROOT or
      /workspace/.gavmo/proc), host-visible and persistent across VM resets.
      EOF
        exit 2
      }

      # A missing/empty/dead pidfile (e.g. left from a previous boot) counts
      # as not running.
      is_running() {
        local dir="$1" pid
        [ -f "$dir/pid" ] || return 1
        pid="$(cat "$dir/pid" 2>/dev/null || true)"
        [ -n "$pid" ] || return 1
        kill -0 "$pid" 2>/dev/null
      }

      cmd_start() {
        local name="$1"; shift
        [ -n "$name" ] || usage
        [ "''${1:-}" = "--" ] || usage
        shift
        [ "$#" -gt 0 ] || usage

        local dir="$root/$name"
        if is_running "$dir"; then
          echo "gproc: '$name' is already running (pid $(cat "$dir/pid"))." >&2
          echo "gproc: stop it first, or use 'gproc restart $name'." >&2
          exit 1
        fi

        mkdir -p "$dir"
        # One arg per line, so `restart` can re-run args containing spaces.
        : > "$dir/cmd"
        local a
        for a in "$@"; do printf '%s\n' "$a" >> "$dir/cmd"; done
        # Clear stale state first so the launch poll can't mistake the
        # previous run for this one.
        rm -f "$dir/pid" "$dir/status"

        # The wrapper records its pid (atomically via mv, so readers never see
        # a partial file) and later the exit status, for `list`.
        setsid sh -c '
          echo "$$" > "'"$dir"'/pid.tmp"
          mv "'"$dir"'/pid.tmp" "'"$dir"'/pid"
          "$@"
          echo "$?" > "'"$dir"'/status"
        ' sh "$@" >> "$dir/log" 2>&1 &

        # Wait until the process is live or has already exited. Polling real
        # liveness, not just pidfile presence, avoids racing a fast restart.
        local i=0
        while ! is_running "$dir" && [ ! -f "$dir/status" ] && [ "$i" -lt 50 ]; do
          sleep 0.1; i=$((i + 1))
        done

        if is_running "$dir"; then
          echo "gproc: started '$name' (pid $(cat "$dir/pid"))."
          echo "gproc: logs at $dir/log (view with: gproc logs $name; re-run for newer output)"
        else
          # Show the log tail inline and fail, so the agent sees why at once.
          echo "gproc: '$name' exited immediately. Last log lines:" >&2
          tail -n 20 "$dir/log" >&2 || true
          echo "gproc: full log at $dir/log" >&2
          exit 1
        fi
      }

      cmd_list() {
        if [ ! -d "$root" ] || [ -z "$(ls -A "$root" 2>/dev/null || true)" ]; then
          echo "gproc: no managed processes."
          return 0
        fi
        printf '%-16s %-8s %-10s %s\n' NAME PID STATUS COMMAND
        local dir name pid status cmd
        for dir in "$root"/*/; do
          [ -d "$dir" ] || continue
          name="$(basename "$dir")"
          if is_running "$dir"; then
            pid="$(cat "$dir/pid")"; status="running"
          else
            pid="-"
            if [ -f "$dir/status" ]; then
              status="exited($(cat "$dir/status"))"
            else
              status="stopped"
            fi
          fi
          cmd="$(tr '\n' ' ' < "$dir/cmd" 2>/dev/null || true)"
          printf '%-16s %-8s %-10s %s\n' "$name" "$pid" "$status" "$cmd"
        done
      }

      # Snapshot only: the agent can't send Ctrl-C, so follow mode would hang
      # until the shell tool times out. Args are parsed here rather than
      # forwarded to `tail` so `-f` can't slip through.
      cmd_logs() {
        local name="$1"; shift || true
        [ -n "''${name:-}" ] || usage
        local dir="$root/$name"

        local lines=""
        while [ "$#" -gt 0 ]; do
          case "$1" in
            -f|-F|--follow)
              echo "gproc: 'logs $name -f' (follow) is not supported - it would block forever." >&2
              echo "gproc: re-run 'gproc logs $name' for the latest output, or 'gproc logs $name -n <N>' for the last N lines." >&2
              exit 2
              ;;
            -n)
              shift
              lines="''${1:-}"
              [ -n "$lines" ] || { echo "gproc: '-n' needs a line count, e.g. 'gproc logs $name -n 50'." >&2; exit 2; }
              case "$lines" in
                *[!0-9]*) echo "gproc: '-n' needs a number, got '$lines'." >&2; exit 2 ;;
              esac
              ;;
            -n*)
              lines="''${1#-n}"
              case "$lines" in
                *[!0-9]*) echo "gproc: '-n' needs a number, got '$lines'." >&2; exit 2 ;;
              esac
              ;;
            *)
              echo "gproc: unknown option '$1' for logs (supported: -n <N>)." >&2
              exit 2
              ;;
          esac
          shift
        done

        [ -f "$dir/log" ] || { echo "gproc: no log for '$name'." >&2; exit 1; }
        if [ -n "$lines" ]; then
          tail -n "$lines" "$dir/log"
        else
          cat "$dir/log"
        fi
      }

      cmd_stop() {
        local name="$1"
        [ -n "''${name:-}" ] || usage
        local dir="$root/$name"
        if ! is_running "$dir"; then
          echo "gproc: '$name' is not running." >&2
          exit 1
        fi
        local pid; pid="$(cat "$dir/pid")"
        # Negative pid signals the whole process group `setsid` created, so
        # child processes a dev server spawned go down with it.
        kill -TERM "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
        local i=0
        while is_running "$dir" && [ "$i" -lt 50 ]; do
          sleep 0.1; i=$((i + 1))
        done
        if is_running "$dir"; then
          kill -KILL "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
          sleep 0.2
        fi
        echo "gproc: stopped '$name'."
      }

      cmd_restart() {
        local name="$1"
        [ -n "''${name:-}" ] || usage
        local dir="$root/$name"
        [ -f "$dir/cmd" ] || { echo "gproc: no saved command for '$name'." >&2; exit 1; }
        if is_running "$dir"; then cmd_stop "$name"; fi
        local args=()
        while IFS= read -r line; do args+=("$line"); done < "$dir/cmd"
        cmd_start "$name" -- "''${args[@]}"
      }

      cmd_rm() {
        local name="$1"
        [ -n "''${name:-}" ] || usage
        local dir="$root/$name"
        [ -d "$dir" ] || { echo "gproc: no such process '$name'." >&2; exit 1; }
        if is_running "$dir"; then
          echo "gproc: '$name' is still running - stop it first." >&2
          exit 1
        fi
        rm -rf "$dir"
        echo "gproc: removed '$name'."
      }

      sub="''${1:-}"; shift || true
      case "$sub" in
        start)   cmd_start "$@" ;;
        list|ls) cmd_list "$@" ;;
        logs)    cmd_logs "$@" ;;
        stop)    cmd_stop "$@" ;;
        restart) cmd_restart "$@" ;;
        rm)      cmd_rm "$@" ;;
        ""|-h|--help|help) usage ;;
        *) echo "gproc: unknown command '$sub'" >&2; usage ;;
      esac
    '';
  };
in
{
  # GAVMO guest image: `opencode web` running as an unprivileged agent against
  # the virtiofs-shared /workspace, with a managed opencode config.

  # virtiofs shares, plus the balloon device (crates/vm/src/qemu.rs) that the
  # control panel's Memory stat reads over QMP. Loaded explicitly rather than
  # relying on autoload in a minimal image.
  boot.kernelModules = [
    "virtiofs"
    "virtio_balloon"
  ];

  # Needed by the in-guest `nixos-rebuild --flake` (gavmo-rebuild).
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  # Security boundary: `agent` can't reach the nix-daemon. This is enforceable
  # only because `agent` has no sudo and so can't act as root. root is always
  # allowed, so gavmo-rebuild is unaffected. A repo's gavmo.nix can opt in with
  # `nix.settings.allowed-users = [ "agent" ]` (list options concatenate).
  nix.settings.allowed-users = lib.mkDefault [ "root" ];

  # Rust <-> image contract: the virtiofs mount tags ("workspace", "mcp") must match the MOUNT_TAG_* constants in
  # crates/vm/src/handle.rs.
  fileSystems."/workspace" = {
    device = "workspace";
    fsType = "virtiofs";
    options = [
      "rw"
      "nofail"
      "x-systemd.device-timeout=10s"
    ];
  };

  # Per-workspace MCP bearer token (see `VmConfig::mcp_dir` in
  # crates/vm/src/config.rs for why it's a separate share). gavmo-opencode
  # loads token.env via EnvironmentFile, which root reads before dropping to
  # `agent`, so uid mapping doesn't matter here.
  fileSystems."/etc/gavmo/mcp" = {
    device = "mcp";
    fsType = "virtiofs";
    options = [
      "ro"
      "nofail"
      "x-systemd.device-timeout=10s"
    ];
  };

  # Unprivileged user that opencode and its commands run as. uid/gid must
  # match GUEST_AGENT_UID/GID in crates/vm/src/share.rs (virtiofsd's uid
  # mapping); keep them in sync by hand. Deliberately no groups (not wheel)
  # and no sudo - a repo's gavmo.nix can add what its agent needs.
  users.users.agent = {
    isNormalUser = true;
    uid = 1000;
    group = "agent";
    createHome = true;
    home = "/home/agent";
  };
  users.groups.agent.gid = 1000;

  # Nix's fetcher/eval-cache SQLite DBs use `synchronous = off`, so a VM kill
  # mid-rebuild can corrupt them; on the persistent disk that poisons every
  # later rebuild ("file is not a database"). tmpfs starts empty each boot and
  # shadows any corrupt copy underneath. /nix/store and opencode's (crash-safe)
  # session DB stay persistent.
  fileSystems."/root/.cache/nix" = {
    device = "tmpfs";
    fsType = "tmpfs";
    # size is a cap, not a reservation. tmpfs defaults to mode 1777.
    options = [
      "size=512M"
      "mode=0700"
    ];
  };

  # opencode shells out to git for diffs/commits.
  environment.systemPackages = [
    opencodeWeb
    pkgs.git
    gproc
  ];

  # Managed OpenCode config. /etc/opencode is opencode's highest-priority,
  # non-overridable location (https://opencode.ai/docs/config/#managed-settings),
  # so a repo's own opencode.json can't redirect the provider or MCP URL.
  environment.etc."opencode/opencode.json".text = builtins.toJSON {
    "$schema" = "https://opencode.ai/config.json";

    instructions = [ "/etc/gavmo/AGENTS.md" ];

    # No session data leaves the VM.
    share = "disabled";

    # Version is controlled by the image build, not a runtime self-update.
    autoupdate = false;

    # Start in read-only Plan mode; the user can still switch to Build.
    default_agent = "plan";

    # Models are served by the host: the `gavmo` provider talks to the
    # daemon's /llm route (see `hostGateway`), authenticated with this boot's
    # workspace token. The daemon forwards to its model gateway
    # (Bifrost), which holds the user's provider credentials - the guest has
    # none. `id` is the model name Bifrost routes (provider/model).
    model = "gavmo/claude-sonnet-5-5";
    small_model = "gavmo/claude-haiku-5-5";
    # The provider pin is the security boundary: only the host-served
    # provider can be used, whatever a repo's own opencode.json says.
    enabled_providers = [ "gavmo" ];
    provider.gavmo =
      let
        claude = id: name: {
          inherit id name;
          reasoning = true;
          tool_call = true;
          attachment = true;
          temperature = true;
          # Conservative; drives opencode's auto-compaction threshold.
          limit = {
            context = 200000;
            output = 64000;
          };
        };
      in
      {
        name = "GAVMO";
        # Bundled with opencode: nothing is fetched at runtime.
        npm = "@ai-sdk/anthropic";
        options = {
          baseURL = "${hostGateway}/llm/anthropic/v1";
          apiKey = "{env:GAVMO_MCP_TOKEN}";
        };
        models = {
          claude-sonnet-5-5 = claude "vertex/claude-sonnet-5-5@default" "Claude Sonnet 5.5";
          claude-haiku-5-5 = claude "vertex/claude-haiku-5-5@default" "Claude Haiku 5.5";
          claude-opus-5-5 = claude "vertex/claude-opus-5-5@default" "Claude Opus 5.5";
        };
      };

    # Host-side tool gateway (see `hostGateway`). Always enabled: the gateway
    # filters tools per bearer token, returning an empty list rather than an
    # error. GAVMO_MCP_TOKEN and GAVMO_HOST_PORT come from gavmo-opencode's
    # EnvironmentFile.
    mcp.gavmo-tools = {
      type = "remote";
      url = "${hostGateway}/mcp";
      enabled = true;
      headers.Authorization = "Bearer {env:GAVMO_MCP_TOKEN}";
    };

    # Wide open on purpose: with bash allowed, per-category denies are
    # bypassable via shell and only add prompt friction. The git-remote denies
    # are just a tripwire (the VM holds no git credentials). The real boundary
    # is gavmo-opencode's kernel-enforced sandbox (User=agent,
    # ProtectSystem=strict, ...), which bash can't bypass.
    permission = {
      "*" = "allow";
      bash = {
        "*" = "allow";
        "git push*" = "deny";
        "git pull*" = "deny";
        "git fetch*" = "deny";
        "git remote add*" = "deny";
        "git remote remove*" = "deny";
        "git remote set-url*" = "deny";
        "git rebase *" = "deny";
      };
    };
  };

  # Agent orientation only; deliberately no host/VM plumbing detail.
  environment.etc."gavmo/AGENTS.md".text = ''
    # GAVMO

    > George's Agent Virtual Machine Orchestrator

    You are an AI coding agent running inside a disposable Linux VM managed
    by GAVMO. The project you are working on is mounted at `/workspace` -
    treat it as the repository root.

    Changes you make to files under `/workspace` are written directly back
    to the user's real project on their machine.

    This VM runs NixOS, and you run as an unprivileged user (not root) with
    no `sudo`. You can read anything, but can only write `/workspace`, your
    own home directory, and `/tmp`. There is no `apt`/`yum` - to add a
    system package, service, or anything else system-wide, ask the user to
    add it to the repo's `gavmo.nix` (or edit it yourself if it already
    exists) rather than reaching for `sudo`, which will not work. Per-user
    tools (`nix profile install`, `cargo install`, `pipx`, etc.) work
    normally without any elevated privileges.

    ## Long-running processes

    Your shell tool blocks on, and eventually kills, any command it runs -
    so it is the wrong tool for a process that is *meant* to run
    indefinitely (a dev server like `nuxt dev`/`vite`, a test watcher, a
    background worker). Use `gproc` for those instead:

    - `gproc start <name> -- <cmd...>` - start it detached; the call returns
      immediately while the process keeps running in the background.
    - `gproc list` - see what is running and each one's log path.
    - `gproc logs <name>` - print its output so far (add `-n <N>` for just
      the last N lines). This is a one-shot snapshot, not a live stream:
      run it again whenever you want to see newer output. There is no
      follow/`-f` mode - that would block forever (you can't interrupt it).
    - `gproc stop <name>` / `gproc restart <name>` - stop or restart it.

    Example: `gproc start web -- npm run dev`, then `gproc logs web` to check
    it came up, then carry on with other work. Logs live at
    `/workspace/.gavmo/proc/<name>/log`.
  '';

  # Per-repo environment via gavmo.nix.
  # Copies this flake directory into the image at build time as the in-guest
  # rebuild target. Its flake.lock pins the same nixpkgs the image was built
  # from, so rebuilds reuse the existing /nix/store instead of refetching.
  environment.etc."gavmo/system".source = ./.;

  # Applies /workspace/gavmo.nix once per boot, before gavmo-opencode, so the
  # agent sees the customized environment from the start. Skipped cleanly via
  # ConditionPathExists when the repo has none.
  #
  # `nixos-rebuild test`, not `switch`: activates without touching the
  # bootloader, so every boot starts from the base system and re-applies
  # gavmo.nix - nothing to roll back. `--impure` is needed to read gavmo.nix
  # from outside the flake; acceptable since the VM is the trust boundary.
  #
  # gavmo-opencode requires this unit: a broken gavmo.nix must block opencode
  # rather than let the agent run without the tools it expects.
  #
  # Host-visible failure signal (Rust <-> image contract): writes to
  # /workspace/.gavmo/vm (`VmConfig::guest_output_dir` in crates/vm/src/config.rs;
  # it holds only this guest output - host VM state lives outside the repo)
  #   - rebuild-<UTC timestamp>.log: full nixos-rebuild output
  #   - rebuild-failed: on failure only, holding the log's filename; crates/vm
  #     races it against opencode's port so failures surface in seconds
  #     instead of after BOOT_HEALTH_TIMEOUT.
  # On success all rebuild logs and the marker are deleted.
  systemd.services.gavmo-rebuild = {
    description = "GAVMO: rebuild the guest system from the repo's gavmo.nix";
    # Needs real connectivity (network-online, unlike gavmo-opencode) to fetch
    # packages missing from the base store. A wait-online stall (~90s) only
    # eats into the 5-minute BOOT_HEALTH_TIMEOUT.
    wants = [ "network-online.target" ];
    after = [
      "workspace.mount"
      "network-online.target"
    ];
    requires = [ "workspace.mount" ];
    wantedBy = [ "multi-user.target" ];
    unitConfig.ConditionPathExists = "/workspace/gavmo.nix";
    # Pinned so Nix's cache is guaranteed to be the tmpfs /root/.cache/nix.
    environment.HOME = "/root";
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # Journal only: build output would just clutter the serial console,
      # which nobody is watching yet at this point in boot.
      StandardOutput = "journal";
      StandardError = "journal";
    };
    script = ''
      set -u
      state_dir="/workspace/.gavmo/vm"
      marker="$state_dir/rebuild-failed"
      log="$state_dir/rebuild-$(date -u +%Y-%m-%dT%H-%M-%SZ).log"

      # The daemon normally creates this; a manually booted VM may not.
      mkdir -p "$state_dir"

      echo "gavmo: /workspace/gavmo.nix found - rebuilding guest system..."

      # Process substitution rather than a pipe, so the exit status is
      # nixos-rebuild's own without needing pipefail.
      if /run/current-system/sw/bin/nixos-rebuild test \
          --flake /etc/gavmo/system#gavmo --impure \
          > >(tee "$log") 2>&1
      then
        echo "gavmo: rebuild complete."
        rm -f "$state_dir"/rebuild-*.log "$marker"
      else
        echo "gavmo: rebuild FAILED - see $log"
        basename "$log" > "$marker"
        exit 1
      fi
    '';
  };

  # opencode's shell tool kills commands after 2 minutes by default, too short
  # for builds and test suites. A sessionVariable with mkDefault, so a repo's
  # gavmo.nix can override it (unlike gavmo-opencode's managed vars).
  environment.sessionVariables.OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS =
    lib.mkDefault "300000";

  # opencode web, running as the unprivileged `agent` user.
  systemd.services.gavmo-opencode = {
    description = "GAVMO opencode web server";
    # network.target, not network-online: listening needs no connectivity,
    # and waiting on wait-online risks stalling boot. gavmo-rebuild is
    # required so a broken gavmo.nix blocks opencode.
    after = [
      "workspace.mount"
      "network.target"
      "gavmo-rebuild.service"
    ];
    requires = [
      "workspace.mount"
      "gavmo-rebuild.service"
    ];
    wantedBy = [ "multi-user.target" ];
    # sessionVariables are delivered via PAM, which this service bypasses, so
    # merge them here. Managed vars come second so they win on collision.
    environment = config.environment.sessionVariables // {
      # safe.directory=* via env, since nixpkgs' git doesn't reliably read
      # /etc/gitconfig. Covers paths whose host uid doesn't map to `agent`.
      GIT_CONFIG_COUNT = "1";
      GIT_CONFIG_KEY_0 = "safe.directory";
      GIT_CONFIG_VALUE_0 = "*";
      # HOME (and so the regenerable cache) stays on the overlay disk.
      HOME = "/home/agent";
      # Session DB on the share so history survives a VM reset (which wipes
      # the overlay) and is host-visible. Relies on virtiofs locking
      # semantics; 9p caused SQLITE_IOERR here.
      XDG_DATA_HOME = "/workspace/.gavmo/opencode";
    };
    serviceConfig = {
      ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p /workspace/.gavmo/opencode /home/agent/.local/share /home/agent/.config /home/agent/.cache";
      WorkingDirectory = "/workspace";

      # Kernel-enforced sandbox: opencode and every command it spawns can
      # write only /workspace, /home/agent and a private /tmp. This is a
      # second layer under the VM boundary.
      User = "agent";
      Group = "agent";

      # Hardening is mkDefault: a strict default that a repo's gavmo.nix can
      # loosen with lib.mkForce (see the README).
      ProtectSystem = lib.mkDefault "strict";
      # No ProtectHome: it hides /home even with /home/agent listed here,
      # breaking ExecStartPre's mkdir. ProtectSystem=strict already keeps
      # everything else read-only.
      ReadWritePaths = [
        "/workspace"
        "/home/agent"
      ];
      PrivateTmp = lib.mkDefault true;
      # Blocks sudo (setuid gain). Enabling sudo in a repo also requires
      # widening CapabilityBoundingSet (see the README).
      NoNewPrivileges = lib.mkDefault true;
      # Hard capability ceiling regardless of uid, so even an escalation
      # can't regain e.g. CAP_SYS_ADMIN. Repos should add only what they need.
      CapabilityBoundingSet = lib.mkDefault [ ];
      RestrictNamespaces = lib.mkDefault true;
      ProtectKernelModules = lib.mkDefault true;
      ProtectKernelTunables = lib.mkDefault true;
      ProtectControlGroups = lib.mkDefault true;
      ProtectClock = lib.mkDefault true;
      # Not mkDefault: only blocks creating setuid files, not running sudo.
      RestrictSUIDSGID = true;

      # GAVMO_MCP_TOKEN (for both the tool gateway and the model provider)
      # and GAVMO_HOST_PORT (the gateway's host port), read as root before
      # dropping privileges. `-` makes a missing file non-fatal: there's no
      # ordering on the mcp mount, so the service may start tokenless and pick
      # it up on a later restart.
      EnvironmentFile = "-/etc/gavmo/mcp/token.env";
      Restart = "always";
      RestartSec = "1";
      # Journal only; on the console opencode's banner is just clutter.
      StandardOutput = "journal";
      StandardError = "journal";
    };
    # Sources /etc/profile like a login shell, since systemd's default PATH
    # lacks /run/current-system/sw/bin and gavmo.nix's systemPackages would
    # otherwise be invisible to the agent. Runs after gavmo-rebuild, so it
    # sees the merged generation. /etc/profile resets PATH, so the xdg-open
    # shim is prepended afterwards.
    script = ''
      # Before `set -u`: /etc/profile is NixOS-generated and may reference
      # unset variables.
      . /etc/profile
      set -u
      export PATH="${xdgOpenShim}/bin:$PATH"
      exec ${opencodeWeb}/bin/opencode web --hostname 0.0.0.0 --port 7080
    '';
  };

  # GUEST_OPENCODE_PORT (crates/vm/src/config.rs) must be reachable; NixOS's
  # firewall blocks it by default otherwise.
  networking.firewall.allowedTCPPorts = [ 7080 ];

  # Debug access: root auto-login on the virtual and serial consoles, as there
  # is no SSH or guest agent. Reconsider if the VM stops being disposable.
  services.getty.autologinUser = "root";

  # Hold the login prompt until gavmo-rebuild finishes, so the shell sees the
  # final environment. `after` only, so a failed rebuild still gets a console.
  systemd.services."getty@tty1".after = [ "gavmo-rebuild.service" ];
  systemd.services."serial-getty@ttyS0".after = [ "gavmo-rebuild.service" ];

  # Saves significant build time.
  documentation.enable = lib.mkDefault false;

  system.stateVersion = "24.11";
}
