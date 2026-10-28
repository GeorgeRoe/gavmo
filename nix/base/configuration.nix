{ lib, pkgs, config, ... }:

let
  # --- opencode-web UI defaults --------------------------------------------
  # "File tree" / "Command palette" / "Server status" / "Show agent" /
  # "Show reasoning summaries" / "Expand shell tool parts" / "Expand edit
  # tool parts" are off-by-default toggles in opencode's web UI (Settings >
  # General). They are *not* config-file settings - there is no
  # managed-settings or opencode.json equivalent for them (unlike
  # everything else in this file).
  # They live purely in the browser's `localStorage`, under the persisted
  # key `settings.v3`, seeded from a `defaultSettings.general` object in
  # `packages/app/src/context/settings.tsx` of opencode's own source
  # (confirmed against the exact nixpkgs-built version's tag on GitHub).
  #
  # Since there's no config-file lever, we default-enable them by patching
  # that source file *before* nixpkgs' vite build runs (`postPatch`), not
  # the compiled output - the bundled/minified property names churn between
  # opencode releases (verified: they differ between 1.18.31 and 1.18.35),
  # while the readable source identifiers are far more stable. Every
  # substitution *asserts* its target occurs exactly once before touching
  # anything, so a future opencode version that renames/moves these fails
  # the image build loudly (naming the exact string to fix) instead of
  # silently shipping the toggles off.
  #
  # `showCustomAgents` ("Show agent") needs a second patch beyond the
  # default flip: on a fresh browser profile's first web load, a one-time
  # `initializeAgentVisibility` effect unconditionally recomputes and
  # overwrites it (see `initialAgentVisibility` in settings.tsx), ignoring
  # the default. We force that function to always resolve "on" for an
  # uninitialized profile; its `if (initialized === true) return` guard is
  # untouched, so a user's later manual toggle still sticks permanently.
  #
  # --- Auto-register the workspace as an opencode project -----------------
  # opencode scopes every session under a *project*, which it normally
  # auto-derives from a git repo at the working directory. `/workspace`
  # often isn't a git repo (it's just whatever the user's project happens to
  # be), so opencode falls back to its global/"no project" bucket on every
  # boot - the user's prior sessions are still sitting in its SQLite DB (see
  # the "sessions don't survive a restart" investigation in
  # `gavmo-mvp-plan.md`), but the home page never resurfaces them because
  # opencode's *client* - not the server - decides what to show there: the
  # project list and "last project" live in the browser's `localStorage`
  # (persisted key `server.v3`, written by `createServerProjects` in
  # `packages/app/src/context/server.tsx`), and nothing auto-registers the
  # instance's own worktree into that store.
  #
  # Fix: patch `packages/app/src/pages/directory-layout.tsx`'s
  # `DirectoryDataProvider` - the component gavmo's "Open UI" deep-link
  # (`OPENCODE_WORKSPACE_SESSION_PATH` in `crates/gavmo/src/registry.rs`)
  # lands on every boot - so that viewing a *local* (non-draft, non-remote)
  # directory calls `server.projects.open(directory)` (adds it to the home
  # list, a no-op if already present - the same call "New project" makes)
  # and `server.projects.touch(directory)` (marks it last-opened, so a bare
  # `/` visit auto-opens it too). Works for non-git directories exactly as
  # well as git ones - `open`/`touch` just store the worktree path as a
  # string - and writes nothing into the user's project folder. Combined
  # with `crates/gavmo/src/registry.rs` persisting a stable host port across
  # restarts (so the browser keeps the same origin, and therefore this same
  # `localStorage`), this is what makes a session survive a VM Stop/Start.
  #
  # Verified against `anomalyco/opencode` tag `v1.18.35` (the exact version
  # `nixpkgs` at this flake's pinned revision builds - see `flake.lock`).
  # The `assert` below fails the build loudly (not a silent regression) if a
  # future `nixpkgs` update bumps opencode: re-verify the three substitution
  # anchors below against the new version's source before updating the
  # pinned string.
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

        # Applies one exact textual substitution inside $3 (a target file -
        # $settings or $dirLayout below). Refuses to proceed unless $from
        # occurs in that file EXACTLY once.
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

        # File tree / Command palette / Server status: plain default-value
        # flips (false -> true) in `defaultSettings.general`.
        patchOnce 'showFileTree: false,' 'showFileTree: true,' "$settings"
        patchOnce 'showSearch: false,' 'showSearch: true,' "$settings"
        patchOnce 'showStatus: false,' 'showStatus: true,' "$settings"

        # Show agent: default-value flip, plus forcing the first-run
        # auto-init branch (see comment above) to always resolve on.
        patchOnce 'showCustomAgents: false,' 'showCustomAgents: true,' "$settings"
        patchOnce 'return existing || previousVersion !== undefined' 'return true' "$settings"

        # Timeline display prefs: pure default-value flips, no auto-init
        # override to worry about (plain `withFallback` accessors).
        patchOnce 'showReasoningSummaries: false,' 'showReasoningSummaries: true,' "$settings"
        patchOnce 'shellToolPartsExpanded: false,' 'shellToolPartsExpanded: true,' "$settings"
        patchOnce 'editToolPartsExpanded: false,' 'editToolPartsExpanded: true,' "$settings"

        # Auto-register the workspace as an opencode project (see the
        # doc comment above `opencodeWeb`): three substitutions against
        # `DirectoryDataProvider` in $dirLayout.
        #
        # 1. Import the `projects` store's hook alongside the existing
        #    (type-only) `ServerConnection` import.
        patchOnce \
          'import type { ServerConnection } from "@/context/server"' \
          'import type { ServerConnection } from "@/context/server"
import { useServer } from "@/context/server"' \
          "$dirLayout"

        # 2. Grab the store, right next to the sibling `serverSync` hook
        #    this component already calls.
        patchOnce \
          'const serverSync = useServerSync()' \
          'const serverSync = useServerSync()
  const server = useServer()' \
          "$dirLayout"

        # 3. Register (and mark last-opened) the current directory for a
        #    *local* instance - mirrors the existing draft/remote guard on
        #    the sibling effect just above this anchor - inserted right
        #    before the component's only other effect-ish call
        #    (`createResource`), so it runs once `directory()` is known.
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

  # `opencode web` attempts to open a browser on startup; without this, a
  # headless VM has no `xdg-open` and the resulting ENOENT can crash the
  # process into a restart loop. No-op it. Kept out of
  # `environment.systemPackages` deliberately - it's a GAVMO plumbing detail,
  # not a project tool the agent should see on its own `PATH` - and instead
  # prepended directly in `gavmo-opencode`'s startup `script` below (see
  # that service's comment for why it needs re-prepending there instead of
  # just using `systemd.services.*.path`).
  xdgOpenShim = pkgs.writeShellScriptBin "xdg-open" "exit 0";

  # --- gproc: a long-lived-process manager for the agent -------------------
  # opencode's shell tool blocks on, and (past
  # `OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS`, see `gavmo-opencode`
  # below) *kills*, any command it runs - which is exactly wrong for a
  # process that's *supposed* to run indefinitely (a `nuxt dev`/`vite` dev
  # server, a test watcher, …). The agent has no good way to start one of
  # those and keep working: either the command blocks the tool forever, or
  # the timeout reaps it mid-run (surfacing as a confusing `<shell_metadata>`
  # "terminated command after exceeding timeout" note). `gproc` gives the
  # agent a dedicated verb for this: start a process *detached* from the
  # shell tool's lifetime (so the bash call returns instantly while the
  # process keeps running), and always know where its logs are.
  #
  # Mechanism: deliberately NOT systemd. There is no `systemd --user`
  # instance for the `agent` user (no lingering is enabled), and
  # `gavmo-opencode`'s sandbox (`NoNewPrivileges`, `RestrictNamespaces`, an
  # empty `CapabilityBoundingSet` - see that service below) blocks
  # `systemd-run` from spawning a system/scope unit too. So `gproc` uses
  # plain `setsid` + a redirected log + a pidfile, all doable as the
  # unprivileged agent. Spawned processes stay inside `gavmo-opencode`'s own
  # cgroup/mount namespace (incl. its `PrivateTmp`), so they correctly die if
  # opencode itself restarts (a dev server shouldn't outlive the agent) but
  # survive any individual bash tool call. `setsid` also puts each process in
  # its own session/process-group, so the shell tool killing *its* process
  # group on timeout can't take the detached process with it, and
  # `gproc stop` can signal the whole group.
  #
  # State/logs live under `/workspace/.gavmo/proc/<name>/` - the virtiofs
  # share, so they're host-visible (inspectable from the host / control
  # panel), survive a VM reset, and sit alongside the existing
  # `.gavmo/opencode` and `.gavmo/vm` state dirs. Gitignored on the host
  # (see `.gitignore`), so they persist without being committed.
  #
  # `writeShellApplication` runs `shellcheck` at image-build time (so a
  # mistake here fails the build loudly rather than shipping a broken tool)
  # and gives the script a clean `PATH` of exactly its declared
  # `runtimeInputs` - `util-linux` for `setsid`, `procps` for `kill`/liveness
  # checks, `coreutils` for the rest. It's added to
  # `environment.systemPackages` below so it lands in
  # `/run/current-system/sw/bin`, which `gavmo-opencode`'s `/etc/profile`
  # sourcing puts on the agent's `PATH` (same delivery path the playwright
  # example's wrapper relies on - a `PATH` session variable would NOT work,
  # since that startup re-derives `PATH` from scratch).
  gproc = pkgs.writeShellApplication {
    name = "gproc";
    runtimeInputs = [ pkgs.coreutils pkgs.util-linux pkgs.procps ];
    text = ''
      # Root of all gproc state, on the host-visible virtiofs share.
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

      # True if the process recorded for <name> is currently alive. Treats a
      # missing/empty/dead pidfile as not-running, so a stale pidfile left by
      # a previous boot (the pid may now belong to something else, or
      # nothing) never reads as a live process.
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
        # Require the `--` separator so the command is unambiguous.
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
        # Record the command (one arg per line) so `list`/`restart` can show
        # and re-run it. `printf %s\n` per-arg keeps args with spaces intact.
        : > "$dir/cmd"
        local a
        for a in "$@"; do printf '%s\n' "$a" >> "$dir/cmd"; done
        # Clear any leftover pid/status from a previous run of this name
        # (e.g. a `restart`, or a reused name after a reboot) *before*
        # launching, so the poll below can never read stale state and mistake
        # the previous run for this one.
        rm -f "$dir/pid" "$dir/status"

        # Detach: `setsid` gives the child its own session + process group, so
        # it survives this shell returning and the shell tool's group-kill on
        # timeout. A tiny wrapper records its own pid, then (once the command
        # exits) its exit status - so `list` can show "exited (code)" rather
        # than just "stopped". `pid` is written to a temp path and atomically
        # `mv`d into place so a reader never sees a half-written/empty file.
        setsid sh -c '
          echo "$$" > "'"$dir"'/pid.tmp"
          mv "'"$dir"'/pid.tmp" "'"$dir"'/pid"
          "$@"
          echo "$?" > "'"$dir"'/status"
        ' sh "$@" >> "$dir/log" 2>&1 &

        # Wait until the launch resolves one way or the other: the process is
        # actually alive (`is_running` - pidfile written *and* that pid is
        # live), or it already recorded an exit `status`. Polling on real
        # liveness (not just "pidfile non-empty") is what makes a fast
        # `restart` report accurately instead of racing the pidfile write.
        local i=0
        while ! is_running "$dir" && [ ! -f "$dir/status" ] && [ "$i" -lt 50 ]; do
          sleep 0.1; i=$((i + 1))
        done

        if is_running "$dir"; then
          echo "gproc: started '$name' (pid $(cat "$dir/pid"))."
          echo "gproc: logs at $dir/log (view with: gproc logs $name; re-run for newer output)"
        else
          # Exited before we even looked - almost always a bad command or an
          # immediate crash. Surface the tail of its log inline so the failure
          # is visible without a second call, and fail nonzero so the agent
          # doesn't read this as a healthy start.
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

      # Deliberately a *snapshot* read, never a live stream. Follow mode
      # (`tail -f`) is intentionally NOT supported: the agent driving this
      # can't send Ctrl-C, so a `-f` would block until the shell tool's
      # timeout kills it - exactly the hang `gproc` exists to avoid. So this
      # parses only an optional `-n <N>` itself (rather than forwarding `$@`
      # to `tail`, which would let `-f`/`--follow` slip through) and rejects
      # everything else with guidance to just run `logs` again for newer
      # output.
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
        # Re-read the saved command, one arg per line, back into positional
        # params and re-run through cmd_start.
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
  # Milestone 1 image: real `opencode web`, authenticated to Google Vertex
  # AI, running against the live virtiofs-shared /workspace. Builds directly
  # on the milestone-0 "plumbing proof" (originally QEMU + 9p + port-forward
  # + health + QMP stop; the share transport moved from 9p to virtiofs at
  # M3 — see `share.rs::VirtiofsShare`'s doc comment for why — everything
  # else below is otherwise unchanged). The only things new in M1 were:
  #   - installing `opencode` + `git`,
  #   - a managed (non-overridable) /etc/opencode/opencode.json, so a repo
  #     cannot swap the provider/model allowlist out from under GAVMO,
  #   - a minimal /etc/gavmo/AGENTS.md baked in via `instructions`,
  #   - the `gavmo-opencode` systemd service replacing the M0 dummy server.
  #
  # See nix/base/flake.nix for how this gets built into a qcow2, and
  # crates/vm/examples/boot.rs for the runner that boots it.

  # Kernel modules for the virtiofs shares and the memory-stats balloon
  # device (`-device virtio-balloon-pci` in `crates/vm/src/qemu.rs`, queried
  # over QMP by `crates/vm/src/qmp.rs::guest_memory` for the control panel's
  # Memory stat). Not strictly required (kmod can often autoload these on
  # demand), but explicit here so neither depends on autodetection in a
  # minimal image.
  boot.kernelModules = [
    "virtiofs"
    "virtio_balloon"
  ];

  # M2: lets the in-guest `nixos-rebuild --flake ...` (see `flake.nix`'s
  # `nixosConfigurations.gavmo` and `nix/spike-m2.md`) run without needing
  # `--extra-experimental-features` passed by hand every time.
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  # M3: the unprivileged `agent` user (see `users.users.agent` below) must
  # not be able to reach the nix-daemon directly — this is what makes
  # `nix.settings.allowed-users`'s exclusion of `agent` an enforceable
  # boundary rather than a tripwire: with `agent` holding no sudo (see
  # `gavmo-opencode` below), there is no path for it to re-present as a
  # trusted/root caller. `root` is always implicitly allowed/trusted by the
  # daemon regardless of this list — that's what lets `gavmo-rebuild` below
  # keep working unaffected. A repo that genuinely wants its agent to use
  # nix directly can re-add it via its own `gavmo.nix`
  # (`nix.settings.allowed-users = [ "agent" ]` — list options concatenate,
  # see the README's "Customizing your repo's VM").
  nix.settings.allowed-users = lib.mkDefault [ "root" ];

  # --- The Rust <-> image contract -----------------------------------------
  # Mount tags here MUST match MOUNT_TAG_WORKSPACE / MOUNT_TAG_GCLOUD in
  # crates/vm/src/handle.rs. `cache=auto` mirrors the daemon's virtiofsd
  # invocation (`share.rs::build_virtiofsd_args`'s `--cache=auto`) — the two
  # must agree since virtiofs's host and guest sides both derive their
  # coherence behavior from the same cache mode.
  fileSystems."/workspace" = {
    device = "workspace";
    fsType = "virtiofs";
    options = [
      "rw"
      "nofail"
      "x-systemd.device-timeout=10s"
    ];
  };

  fileSystems."/etc/gavmo/gcloud" = {
    device = "gcloud";
    fsType = "virtiofs";
    options = [
      "ro"
      "nofail"
      "x-systemd.device-timeout=10s"
    ];
  };

  # Phase 4 / P4.0 (see `crates/mcp/plan.md`): the per-workspace MCP
  # capability token share. Mirrors the gcloud mount above exactly (same
  # virtiofs options, same read-only posture) - the only dynamic
  # per-workspace value the host needs to get into the guest for the tool
  # gateway is this one bearer token (see `VmConfig::mcp_dir` in
  # `crates/vm/src/config.rs` for why it's a dedicated share and not just a
  # file under `/workspace`). `EnvironmentFile` on `gavmo-opencode` below
  # loads `token.env` from here directly - no guest-side script needed to
  # turn it into an env var. Read by root (systemd's `EnvironmentFile` is
  # processed before the service drops to `agent`), so this share's
  # ownership/uid-mapping doesn't matter for it the way it does for
  # `/workspace`/gcloud.
  fileSystems."/etc/gavmo/mcp" = {
    device = "mcp";
    fsType = "virtiofs";
    options = [
      "ro"
      "nofail"
      "x-systemd.device-timeout=10s"
    ];
  };

  # --- M3: the unprivileged agent user --------------------------------------
  # `opencode` (and every `bash` command it runs) executes as this user, not
  # root (see `gavmo-opencode` below) — see the design discussion that landed
  # this change for the full rationale. Its uid/gid are pinned to fixed
  # numbers (rather than left to NixOS's normal dynamic allocation) because
  # `crates/vm/src/share.rs`'s `GUEST_AGENT_UID`/`GUEST_AGENT_GID` constants
  # must match them exactly for virtiofsd's host-owner-to-guest-agent uid
  # mapping to land on the right guest user — like `GAVMO_MCP_PORT` elsewhere
  # in this codebase, there is no automatic sharing between the Rust constant
  # and this declaration, so a change to one must be mirrored in the other by
  # hand. No `extraGroups` (in particular, **not** `wheel`) and no sudo rule
  # is granted by this base config — a repo's own `gavmo.nix` is the place to
  # add group memberships (e.g. `docker`) or capabilities a specific
  # project's agent genuinely needs (see the README).
  users.users.agent = {
    isNormalUser = true;
    uid = 1000;
    group = "agent";
    createHome = true;
    home = "/home/agent";
  };
  users.groups.agent.gid = 1000;

  # --- M2 hardening: keep Nix's regenerable caches off the persistent disk --
  # Nix runs its fetcher-cache and eval-cache SQLite DBs with `pragma
  # synchronous = off` (durability traded for speed) under `~/.cache/nix` -
  # i.e. `/root/.cache/nix` for the root-run `gavmo-rebuild` service below.
  # That makes them explicitly NOT crash-safe. Discovered the hard way: an
  # abrupt VM kill mid-rebuild (our own fast-fail path on a failed build, or
  # a developer's manual `Ctrl-a x`) can leave one half-written. If that sat
  # on the PERSISTENT overlay disk (as it did before this fix), the
  # corruption survives every later reboot and poisons all future rebuilds
  # with `file is not a database` - even after fixing the `gavmo.nix` that
  # caused the original failure (see `nix/spike-m2.md` §10 for the full
  # incident).
  #
  # Fix: mount this one directory as `tmpfs` (RAM-backed, wiped every boot).
  # An abrupt kill can still leave a half-written file, but it's in RAM and
  # evaporates with the crash - there's never a persistent corrupt copy to
  # poison a later boot. This also means the directory mounts here EMPTY
  # every boot, shadowing (not deleting) whatever junk is already on the
  # overlay underneath - so it self-heals an already-corrupted disk with no
  # manual cleanup needed.
  #
  # Deliberately narrow (not all of `/root/.cache`): only Nix's own caches
  # need this - the `/nix/store` itself (the expensive, valuable cache) and
  # OpenCode's session SQLite (`/home/agent/.local/share` since M3 - see the
  # `gavmo-opencode` comment below) are both NOT touched by this mount and
  # stay persistent on the overlay exactly as before. Unlike Nix's cache,
  # OpenCode's session DB uses normal (crash-safe) SQLite settings, so it was
  # never at risk here.
  fileSystems."/root/.cache/nix" = {
    device = "tmpfs";
    fsType = "tmpfs";
    # A cap, not a reservation - tmpfs only consumes RAM for what's actually
    # written, and this cache is metadata only (built packages live in
    # /nix/store on the overlay, unaffected). `mode=0700` because tmpfs
    # otherwise defaults to world-writable (1777, like /tmp), inappropriate
    # for a dir under /root.
    options = [
      "size=512M"
      "mode=0700"
    ];
  };

  # opencode itself (patched - see `opencodeWeb` above), plus git since
  # opencode shells out to it for diffs/commits/etc.
  environment.systemPackages = [
    opencodeWeb
    pkgs.git
    # `gproc` (defined above): the agent's long-lived-process manager, on the
    # agent's `PATH` via the system profile - see its `let`-binding comment.
    gproc
  ];

  # --- Managed OpenCode config ----------------------------------------------
  # /etc/opencode/ is OpenCode's highest-priority, non-overridable "managed
  # settings" location (see https://opencode.ai/docs/config/#managed-settings).
  # Baking the provider/model allowlist here means a repo's own opencode.json
  # cannot widen it - mirrors DAFNAI's config verbatim (same project, same
  # 4-model Claude whitelist via google-vertex-anthropic).
  environment.etc."opencode/opencode.json".text = builtins.toJSON {
    "$schema" = "https://opencode.ai/config.json";

    instructions = [ "/etc/gavmo/AGENTS.md" ];

    # No session data leaves the VM.
    share = "disabled";

    # Version is controlled by the image build, not a runtime self-update.
    autoupdate = false;

    # Start every session in Plan mode (read-only) rather than OpenCode's
    # built-in Build default - the agent can still switch to build mid
    # session, this just avoids having to flip it manually every time.
    default_agent = "plan";

    model = "google-vertex-anthropic/claude-sonnet-5@default";
    small_model = "google-vertex-anthropic/claude-haiku-4-5@20251001";
    enabled_providers = [ "google-vertex-anthropic" ];

    provider.google-vertex-anthropic.whitelist = [
      "claude-haiku-4-5@20251001"
      "claude-sonnet-5@default"
      "claude-opus-4-8@default"
      "claude-fable-5@default"
    ];

    # Phase 4 (see `crates/mcp/plan.md`): GAVMO's host-side tool gateway.
    # `10.0.2.2` is QEMU user-mode networking's address for the host from
    # inside the guest; `7071` is `mcp::GAVMO_MCP_PORT` - there is no
    # automatic sharing between that Rust constant and this literal, so a
    # change to one must be mirrored here by hand. `enabled = true`
    # unconditionally: the gateway itself decides per-call (via the bearer
    # token) what tools a workspace is actually authorized to see - an
    # unauthenticated or tool-less workspace just gets an empty tool list
    # back, not a connection failure (see `crates/mcp/src/lib.rs`'s
    # `list_tools`). `GAVMO_MCP_TOKEN` is loaded from the read-only
    # `/etc/gavmo/mcp/token.env` share above via this service's
    # `EnvironmentFile` below - baked as a managed config value (not
    # something a repo's own opencode.json could point elsewhere) so a
    # repo can't redirect this at a different, attacker-controlled URL.
    mcp.gavmo-tools = {
      type = "remote";
      url = "http://10.0.2.2:7071/mcp";
      enabled = true;
      headers.Authorization = "Bearer {env:GAVMO_MCP_TOKEN}";
    };

    # opencode's own permission system is deliberately wide open (catch-all
    # allow) rather than denying specific categories - once `bash` is
    # allow-all, per-category denies (read/external_directory/etc.) are
    # bypassable via a shell command anyway (e.g. a denied external_directory
    # read is just `cat <path>` through bash), so they'd only add prompting
    # friction without adding real protection. The git-remote denies below
    # are a tripwire only (equally bypassable, e.g. via a wrapper script) -
    # harmless to keep since there are no git credentials in the VM for them
    # to work anyway.
    #
    # This does NOT mean "anything goes inside the VM", though - that was
    # the M1 stance, superseded at M3: `gavmo-opencode`'s `serviceConfig`
    # (`User=agent` + `ProtectSystem=strict` + friends) is a second,
    # independent, OS-level boundary underneath opencode's own permission
    # layer, and that one is NOT bypassable via bash (it's enforced by the
    # kernel against the whole process tree, not interpreted by opencode).
    # The two layers serve different purposes: this `permission` block is
    # about prompting friction for a cooperative agent; `serviceConfig` is
    # about what the agent process can durably do to the guest system even
    # if it tries.
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

  # Minimal context so the agent knows where it is without any detail about
  # the host/VM plumbing underneath it.
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

  # --- M2: per-repo environment via gavmo.nix --------------------------------
  # Bakes this entire flake directory (flake.nix, flake.lock, configuration.nix,
  # disk.nix) into the image at a fixed path, so the in-guest rebuild below has
  # something to target without needing the virtiofs-shared /workspace to contain
  # GAVMO's own source. `./.` is this file's own directory (nix/base) - a plain
  # path literal, so this is a point-in-time filesystem copy taken at image
  # *build* time, not a live/recursive reference; see `nix/spike-m2.md` for the
  # full design (that doc also records the spike that validated this mechanism
  # end-to-end on real hardware).
  #
  # Crucially, `flake.lock` here is pinned to the exact nixpkgs revision this
  # image itself was built from, so the in-guest `nixos-rebuild` below reuses
  # the guest's already-populated /nix/store instead of re-fetching nixpkgs -
  # confirmed by the spike's R5 timings (a few seconds for a no-op rebuild,
  # under 30s even when a gavmo.nix pulled in an entire new service like
  # Docker).
  environment.etc."gavmo/system".source = ./.;

  # Rebuilds the guest's system from `/workspace/gavmo.nix`, if the repo
  # provides one, merging it onto this same base config
  # (`nixosConfigurations.gavmo` in flake.nix, sharing `commonModules` with the
  # image build so the two can never silently diverge). Runs once per boot,
  # before `gavmo-opencode` starts, so opencode and every shell it spawns see
  # the repo's customized environment from the start.
  #
  # `ConditionPathExists` (not a bash `if`) means a repo with no gavmo.nix
  # shows this unit as cleanly "skipped" in `systemctl status`, not run at all
  # - the base image's behavior is completely unchanged for repos that don't
  # opt in.
  #
  # Uses `nixos-rebuild test`, NOT `switch`: `test` builds and activates the
  # new system immediately but never touches the bootloader or the default
  # boot entry. That removes the single biggest "wedge the guest" risk a bad
  # `switch` could cause - confirmed safe in the spike's R3 (opencode wasn't
  # even restarted by activation) and R4 (a new systemd service, Docker, came
  # up live with no reboot). Since the overlay disk persists across restarts
  # but the *activated* system doesn't (no bootloader change), the base system
  # boots fresh every time and this service re-applies gavmo.nix fresh every
  # time too - fully deterministic, nothing to roll back.
  #
  # `--impure` is required because `/workspace/gavmo.nix` lives outside this
  # baked flake's own source tree, which pure-eval flakes normally forbid
  # reading - acceptable here because the VM is the trust boundary (same
  # stance as the wide-open OpenCode permission policy above), not Nix's
  # eval-purity sandbox.
  #
  # Ordering/failure policy: `gavmo-opencode` (below) both `after`s AND
  # `requires`s this service - deliberately strict. The whole point of
  # `gavmo.nix` is that the agent's tools (packages, services like Docker)
  # come from it; if a repo's `gavmo.nix` fails to build, starting opencode
  # anyway on the unmodified base config would let the agent run and reach
  # for tools it believes exist but doesn't have - worse than not starting
  # at all. So a broken `gavmo.nix` means opencode never starts; the daemon
  # detects this quickly (see below), not via a boot-health timeout.
  #
  # --- Host-visible failure signal (the Rust <-> image contract, part 2) ---
  # Since M1.5's console logging got turned off above (clutter, no live
  # audience), the only channel left to tell the host daemon *why* a boot
  # never came up is the virtiofs-shared `/workspace` itself. This service writes
  # into `/workspace/.gavmo/vm/` - which is `state_dir` in
  # `crates/vm/src/config.rs::VmConfig`, i.e. exactly where the host daemon
  # already keeps this VM's disk overlay/QMP socket/console log:
  #   - `rebuild-<UTC timestamp>.log` - the full `nixos-rebuild` output,
  #     always written for this run.
  #   - `rebuild-failed` - written ONLY on failure, containing that log's
  #     filename. `crates/vm`'s boot wait races this marker's appearance
  #     against opencode's port answering, so a broken `gavmo.nix` is
  #     detected in seconds rather than waiting out `BOOT_HEALTH_TIMEOUT`.
  # On SUCCESS, all `rebuild-*.log` files and the marker are deleted - a
  # successful rebuild makes any prior failure moot, so the state dir only
  # ever holds logs for an *unresolved* failure (`crates/vm` also clears a
  # stale marker before each boot attempt, belt-and-suspenders against a
  # previous run's leftovers). Repos with **no** `gavmo.nix` never run this
  # unit at all (`ConditionPathExists` below), so none of this applies to
  # them - `requires` is satisfied by a clean skip same as by success.
  systemd.services.gavmo-rebuild = {
    description = "GAVMO M2: rebuild the guest system from the repo's gavmo.nix";
    # Unlike `gavmo-opencode` below (which deliberately only waits on the
    # lighter `network.target`, since it doesn't need working network to
    # start listening), this service genuinely needs real connectivity: a
    # repo's `gavmo.nix` can pull in packages not already in the base image's
    # store (confirmed in testing - a sample gavmo.nix adding `jq` +
    # `virtualisation.docker.enable` failed to fetch them when this ran
    # before DHCP/networking was actually up, despite booting fine on
    # `network.target` alone). `network-online.target` blocks until
    # `wait-online` confirms real connectivity. Worst case (no DHCP lease at
    # all) this can stall up to systemd's default wait-online timeout
    # (~90s) - acceptable here because `gavmo-opencode` below only orders
    # after *this service's completion*, not after network-online directly,
    # so a stall just eats into (not blows) the 5-minute
    # `BOOT_HEALTH_TIMEOUT` (`crates/vm/src/handle.rs`).
    wants = [ "network-online.target" ];
    after = [
      "workspace.mount"
      "network-online.target"
    ];
    requires = [ "workspace.mount" ];
    wantedBy = [ "multi-user.target" ];
    unitConfig.ConditionPathExists = "/workspace/gavmo.nix";
    # Pinned explicitly (rather than relying on systemd's implicit root-
    # service default) so Nix's cache path is GUARANTEED to be
    # `/root/.cache/nix` - i.e. exactly the directory mounted as tmpfs above.
    # Belt-and-suspenders: today this already resolves to `/root` without
    # this line, but that fix only works if the two stay in lockstep.
    environment.HOME = "/root";
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # Journal only, not console - a `nixos-rebuild` build log (dozens of
      # "copying path"/"building" lines) would otherwise clutter the serial
      # console for no benefit: the agent is already hard-blocked from
      # starting until this succeeds (`requires`, see `gavmo-opencode` below),
      # and the debug console itself doesn't appear until this finishes
      # either (see the getty units near the bottom of this file) - so there
      # is no live audience for console output here. `journalctl -u
      # gavmo-rebuild` still has the full output for debugging a failure.
      StandardOutput = "journal";
      StandardError = "journal";
    };
    script = ''
      set -u
      state_dir="/workspace/.gavmo/vm"
      marker="$state_dir/rebuild-failed"
      log="$state_dir/rebuild-$(date -u +%Y-%m-%dT%H-%M-%SZ).log"

      # Belt-and-suspenders: the host daemon creates this dir before boot
      # (it's where the disk overlay/QMP socket live too), but don't depend
      # on that for a manually-booted/non-daemon VM.
      mkdir -p "$state_dir"

      echo "gavmo: /workspace/gavmo.nix found - rebuilding guest system..."

      # Not a `cmd | tee` pipe (which would need `pipefail` to preserve the
      # real exit code) - `>` into a process substitution keeps `$?` below
      # as nixos-rebuild's own exit status.
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

  # --- opencode web -----------------------------------------------------
  # Replaces the M0 dummy server on the same fixed guest port.
  #
  # Session state (SQLite DB, message/session history, snapshots) lives on
  # the virtiofs-shared /workspace, under `/workspace/.gavmo/opencode`
  # (`XDG_DATA_HOME`, set in this service's `environment` below), NOT on the
  # VM's overlay disk. `HOME` itself stays on the overlay (/home/agent) - only
  # the *data* dir is redirected - so the regenerable cache (`$HOME/.cache`)
  # and managed config stay off the share.
  #
  # History: this was tried at M1 (state under /workspace) and failed hard -
  # OpenCode's SQLite DB threw `disk I/O error` (SQLITE_IOERR) on startup
  # every time under the *original 9p transport* (9p's locking/IO semantics
  # didn't satisfy SQLite, even with `cache=none`), so state was moved onto
  # the overlay disk as a workaround. The M3 switch to virtiofs (stronger
  # POSIX/locking semantics, `--cache=auto`) is what makes putting it back on
  # /workspace viable - exactly the revisit the old workaround comment
  # anticipated. The payoff: session history now survives a full VM *reset*
  # (which wipes the overlay disk and recreates a fresh one), not just
  # stop/restart, and is visible from the host. Gitignored on the host
  # (`.gitignore`'s `.gavmo/opencode`) so it persists on disk without being
  # committed. Your actual project files in /workspace are unaffected either
  # way - this only concerns OpenCode's own internal state dir.
  #
  # opencode's shell tool kills any command that runs longer than this many
  # milliseconds (upstream default: 2 minutes - see
  # `packages/opencode/src/tool/shell.ts`'s `defaultTimeoutMs` and
  # `runtime-flags.ts`'s `bashDefaultTimeoutMs`, confirmed against the
  # `opencode` v1.18.35 source pinned above). 2 minutes is too low for a dev
  # VM where installs/builds/test suites routinely run longer - confirmed in
  # practice: an `npm run test` run was killed mid-suite, surfaced as a
  # `<shell_metadata>` "terminated command after exceeding timeout" note in
  # the agent's own tool output, easy to mistake for the command having
  # silently hung. 5 minutes is a more realistic default for ordinary dev
  # work while still catching a truly stuck command reasonably fast.
  #
  # Set via `sessionVariables` (merged into `gavmo-opencode`'s `environment`
  # below) rather than that service's managed `environment` block, and with
  # `lib.mkDefault`, specifically so a repo's own `gavmo.nix` can override it
  # with its own `environment.sessionVariables.OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS`
  # (see the README) - unlike the managed block's GOOGLE_*/GIT_*/HOME vars,
  # there's no reason this one shouldn't be repo-adjustable. A single long
  # command can also always request its own larger timeout directly (the
  # shell tool's `timeout` parameter), with no config needed.
  environment.sessionVariables.OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS =
    lib.mkDefault "300000";

  # M3: runs as the unprivileged `agent` user (see `users.users.agent`
  # above), not root. See this service's `serviceConfig` below for the
  # write-confinement this enables, and the README for how a repo's own
  # `gavmo.nix` can grant a specific project's agent more (group
  # memberships, capabilities, even sudo) when it's genuinely needed.
  systemd.services.gavmo-opencode = {
    description = "GAVMO opencode web server";
    # Only `network.target` (not `network-online.target`): opencode doesn't
    # need working network to start listening, only later for Vertex calls,
    # and depending on "online" risks stalling on NixOS's wait-online
    # plumbing. Mirrors what the M0 dummy service used successfully.
    #
    # `gavmo-rebuild.service` is both `after`ed AND `require`d - see that
    # service's comment above for the deliberate strict-failure rationale
    # (a broken gavmo.nix must block opencode, not silently degrade to the
    # base config and risk the agent reaching for tools it doesn't have).
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
    # `environment.variables`/`environment.sessionVariables` from a repo's
    # `gavmo.nix` need to reach the agent too, not just
    # `environment.systemPackages` (see the `script` comment below) -
    # `sessionVariables` specifically because it's delivered via PAM to
    # interactive sessions, which this service's `/etc/profile`-sourcing
    # startup script (below) doesn't go through, so it has to be merged in
    # here instead. Put first so the explicit GAVMO-managed vars below
    # always win on collision (e.g. a `gavmo.nix` shouldn't be able to
    # redirect `GOOGLE_APPLICATION_CREDENTIALS`).
    environment = config.environment.sessionVariables // {
      GOOGLE_CLOUD_PROJECT = "redacted-gcp-project";
      GOOGLE_APPLICATION_CREDENTIALS = "/etc/gavmo/gcloud/application_default_credentials.json";
      VERTEX_LOCATION = "global";
      # Force `safe.directory = *` via git's env-var config injection rather
      # than /etc/gitconfig (nixpkgs' git doesn't reliably read that) - this
      # guarantees it applies to every git call opencode makes. Belt-and-
      # suspenders: virtiofsd's uid mapping (`share.rs::GUEST_AGENT_UID`)
      # should make `/workspace` present as owned by `agent` itself, which
      # alone would already satisfy git's ownership check - this covers any
      # case where a subdirectory doesn't map as cleanly (e.g. paths created
      # by tooling with a different host uid).
      GIT_CONFIG_COUNT = "1";
      GIT_CONFIG_KEY_0 = "safe.directory";
      GIT_CONFIG_VALUE_0 = "*";
      # `HOME` stays on the native overlay disk (`/home/agent`, not `/root`
      # since this runs as the unprivileged `agent` user - see
      # `serviceConfig.User` below), so OpenCode's regenerable cache
      # (`$HOME/.cache`) and anything else HOME-relative stays off the
      # virtiofs share. Only the *data* dir is redirected onto `/workspace`
      # via `XDG_DATA_HOME` below.
      HOME = "/home/agent";
      # OpenCode's session state (SQLite DB, message/session history,
      # snapshots) lives under `$XDG_DATA_HOME/opencode`. Pointing this at
      # the virtiofs-shared `/workspace` means session history survives a
      # full VM *reset* (which wipes the overlay disk - see the comment
      # above this service), not just stop/restart, and is visible from the
      # host. This was tried at M1 and failed with `SQLITE_IOERR` under the
      # *9p* transport, which is why state originally moved to the overlay;
      # virtiofs (M3, `--cache=auto`) has stronger POSIX/locking semantics,
      # so this is the revisit that comment anticipated. Gitignored on the
      # host (`.gitignore`'s `.gavmo/opencode`), so it persists on disk but
      # isn't committed. Only the data dir moves - config stays managed
      # (`/etc/opencode`) and cache stays on the overlay (regenerable).
      XDG_DATA_HOME = "/workspace/.gavmo/opencode";
    };
    serviceConfig = {
      # `/workspace/.gavmo/opencode` is `XDG_DATA_HOME` (see `environment`
      # above) - OpenCode's session state dir, on the virtiofs share so it
      # survives a VM reset. The `/home/agent/.{config,cache}` dirs stay
      # HOME-relative on the overlay (config is managed anyway, cache is
      # regenerable). `/home/agent/.local/share` is no longer where data
      # lands (XDG_DATA_HOME overrides it) but is cheap to keep creating.
      ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p /workspace/.gavmo/opencode /home/agent/.local/share /home/agent/.config /home/agent/.cache";
      WorkingDirectory = "/workspace";

      # --- M3: run as the unprivileged agent user, confined to its own
      # write set ---------------------------------------------------------
      # `User`/`Group` drop this service (and every `bash` child opencode
      # spawns, since they inherit the same process credentials and mount
      # namespace) off root entirely - no `wheel`, no sudo rule granted here.
      # Combined with the sandboxing options below, this makes "the agent
      # can only write /workspace, its own home, and a private /tmp" a real,
      # kernel-enforced boundary rather than a convention - see the design
      # discussion that landed this change for why this supersedes the
      # earlier "the VM alone is the trust boundary" stance (this is a
      # second, independent layer on top of it, not a replacement for it).
      User = "agent";
      Group = "agent";

      # Every option below is `lib.mkDefault`ed so a specific repo's own
      # `gavmo.nix` can loosen exactly what it needs via `lib.mkForce`
      # (see the README's "Customizing your repo's VM" for worked examples:
      # Docker access, low ports, etc.). The base posture here is meant to
      # be the strict default, not a hard ceiling.
      #
      # `ProtectSystem = "strict"` remounts the *entire* visible filesystem
      # read-only (including separately-mounted filesystems like the
      # virtiofs shares) except API pseudo-filesystems (`/proc`, `/sys`,
      # `/dev`) and whatever `ReadWritePaths` exempts below - this, not
      # `User`/`Group` alone, is what stops the agent from durably mutating
      # the guest system (`/etc`, `/nix`, `/var`, …). The read-only virtiofs
      # shares (gcloud, mcp) stay readable - `ProtectSystem` only forbids
      # writes, not reads.
      ProtectSystem = lib.mkDefault "strict";
      # `/workspace` is the live project checkout and must stay writable;
      # `/home/agent` is where opencode's session DB/config/cache live (see
      # `HOME` above) and must too.
      #
      # Deliberately NOT paired with `ProtectHome = true`: that was tried and
      # found to shadow `/home` with an empty, read-only mount *inside this
      # service's namespace* even with `/home/agent` listed here - in
      # practice `ExecStartPre`'s `mkdir -p /home/agent/...` failed outright
      # with "cannot create directory '/home': Permission denied" (the
      # namespace never saw a real `/home` to create `agent`'s subdirectory
      # under). `ProtectSystem = "strict"` alone already remounts everything
      # else read-only, so dropping `ProtectHome` costs only "/root and other
      # users' homes are readable" (there are no other users, and `/root` has
      # nothing sensitive - the gcloud creds live under `/etc/gavmo/gcloud`)
      # - the actual write-confinement goal is unaffected.
      ReadWritePaths = [
        "/workspace"
        "/home/agent"
      ];
      # Gives this service its own private, writable `/tmp` - scratch space
      # the agent can freely use (builds, downloads, package manager caches)
      # that is isolated from the host-visible `/tmp` and from other units,
      # and - since it lives in the service's own ephemeral mount namespace,
      # not under `ReadWritePaths` - vanishes with the process rather than
      # persisting on the overlay disk.
      PrivateTmp = lib.mkDefault true;
      # `NoNewPrivileges` is the knob that would need `lib.mkForce false` in
      # a repo's `gavmo.nix` to enable sudo for its agent (sudo relies on a
      # setuid-root exec gaining privileges it didn't start with, which this
      # option forbids outright) - see the README for the full worked
      # example, including why `CapabilityBoundingSet` below also needs
      # widening for that to actually grant useful privileges.
      NoNewPrivileges = lib.mkDefault true;
      # Strips every Linux capability from this service's bounding set, so
      # even a hypothetical privilege-escalation bug (or a future repo that
      # re-enables sudo without also revisiting this) can't regain
      # `CAP_SYS_ADMIN` (remounting the read-only paths above away) or any
      # other capability - the bounding set is a hard ceiling no exec can
      # exceed regardless of uid. A repo granting itself sudo for, say,
      # Docker should extend this list with exactly the capabilities that
      # workflow needs rather than clearing it.
      CapabilityBoundingSet = lib.mkDefault [ ];
      # Blocks creating new mount/user/etc. namespaces from inside the
      # service (e.g. a sandboxed `unshare`-based tool) - defense in depth
      # alongside the empty capability set above, since namespace creation
      # is itself a capability-gated operation these two overlap on.
      RestrictNamespaces = lib.mkDefault true;
      ProtectKernelModules = lib.mkDefault true;
      ProtectKernelTunables = lib.mkDefault true;
      ProtectControlGroups = lib.mkDefault true;
      ProtectClock = lib.mkDefault true;
      # Blocks *creating* new setuid/setgid files - does not block
      # *executing* an already-setuid binary (e.g. `sudo`, if a repo adds
      # it), so this stays on even for a repo that re-enables sudo.
      RestrictSUIDSGID = true;

      # `GAVMO_MCP_TOKEN` for the managed `mcp.gavmo-tools` config entry
      # above, loaded straight from the read-only MCP token share - no
      # script-side parsing needed. Read as root (systemd processes
      # `EnvironmentFile` before dropping to `User`/`Group` above), so this
      # is unaffected by the agent's own reduced privileges. Leading `-`
      # makes a missing file non-fatal (same `nofail`-style posture as the
      # virtiofs mounts themselves): if `/etc/gavmo/mcp` hasn't finished
      # mounting by the time this starts (no explicit unit ordering against
      # it, same as the gcloud share above), the service still starts with
      # no token, and `Restart = "always"` below means the next restart
      # picks it up once the mount has settled - at the cost of the gateway
      # rejecting calls (empty bearer) until then.
      EnvironmentFile = "-/etc/gavmo/mcp/token.env";
      Restart = "always";
      RestartSec = "1";
      # Journal only, not console: originally routed to console too (in
      # addition to the journal) back in M1, when there was no other way to
      # see opencode's real startup output/errors during the initial debug
      # cycle (a boot hang was invisible without it). Now that boot is
      # proven reliable, the console route is pure clutter - e.g. opencode's
      # banner/ASCII art printing on every boot - with no upside, since
      # `journalctl -u gavmo-opencode` still has everything for debugging a
      # real failure.
      StandardOutput = "journal";
      StandardError = "journal";
    };
    # A plain `ExecStart = "${opencodeWeb}/bin/..."` only gets systemd's
    # minimal default service `PATH` (coreutils/findutils/etc, NOT
    # `/run/current-system/sw/bin`) - so a repo's `gavmo.nix`
    # `environment.systemPackages` (e.g. `pkgs.jq`) would be installed
    # system-wide but invisible to the agent and every `bash`/`sh` command
    # it runs, defeating the README's "Customizing your repo's VM" promise
    # that `environment.systemPackages` doubles as the project's tools. A
    # root login console (the debug getty below) doesn't have this problem
    # - `/etc/profile` puts the system profile on its `PATH` - which is why
    # `jq` appeared there but not for the agent.
    #
    # Fix: source `/etc/profile` ourselves before `exec`ing opencode, same
    # as a login shell would, so the agent gets what that debug console
    # gets: the system profile (`/run/current-system/sw/bin`) and setuid
    # wrappers (`/run/wrappers/bin`) on `PATH`, plus `environment.variables`
    # via `/etc/profile` sourcing `/etc/set-environment`. Since
    # `gavmo-opencode` only starts after `gavmo-rebuild.service` has
    # already switched `/run/current-system` to the merged (base +
    # `gavmo.nix`) generation, this always reflects the *current* repo
    # config, not a stale one. (`environment.sessionVariables` is handled
    # separately above - it's PAM-delivered to interactive sessions, not
    # sourced via `/etc/profile`, so sourcing it here wouldn't pick it up.)
    #
    # Deliberately using NixOS's `script` option (as `gavmo-rebuild` above
    # does) rather than a raw `serviceConfig.ExecStart` string: `script`
    # writes this out as a real generated shell script file invoked with no
    # arguments, sidestepping systemd's own ExecStart command-line
    # quoting/`$VAR`-substitution rules entirely - there's no risk of
    # systemd trying to interpret the `$PATH` or quoting below itself
    # before this ever reaches a shell.
    #
    # `/etc/profile` sourcing resets `PATH` from scratch, which would
    # otherwise silently drop the `xdg-open` shim (previously supplied via
    # this service's own `path` option, now removed) and reintroduce the
    # ENOENT crash loop that shim exists to prevent - so it's re-prepended
    # by hand right after, scoping it to just this service rather than
    # adding it to the system profile everything else (including the
    # agent's own `bash` tool calls) would see it on too.
    script = ''
      # `/etc/profile` sourced *before* `set -u`: it's NixOS-generated, not
      # ours to audit, and a `set -u` tripping on some unrelated unset
      # variable inside it would be a confusing way for this service to
      # fail. Everything below this line is ours, so `set -u` from here on
      # only guards our own code.
      . /etc/profile
      set -u
      export PATH="${xdgOpenShim}/bin:$PATH"
      exec ${opencodeWeb}/bin/opencode web --hostname 0.0.0.0 --port 7080
    '';
  };

  # GUEST_OPENCODE_PORT (crates/vm/src/config.rs) must be reachable; NixOS's
  # firewall blocks it by default otherwise.
  networking.firewall.allowedTCPPorts = [ 7080 ];

  # --- Debug access ---------------------------------------------------------
  # Auto-login on both the virtual console and the serial console
  # (-serial file:console.log in crates/vm/src/qemu.rs), since there's no
  # SSH/guest-agent in the MVP. This is a disposable debug image; do not
  # carry this into the real per-repo image without reconsidering it.
  services.getty.autologinUser = "root";

  # Don't hand out a login prompt on either console until `gavmo-rebuild` has
  # finished (success OR failure) - purely a manual-debugging nicety, so you
  # don't land on a shell mid-rebuild and see e.g. `jq` as "not found" only
  # for it to appear moments later. `after`-only (NOT `requires`): a *failed*
  # rebuild must still leave you a console to debug it from - only the agent
  # (`gavmo-opencode` above) is strictly blocked by a failed rebuild, not
  # this debug shell.
  systemd.services."getty@tty1".after = [ "gavmo-rebuild.service" ];
  systemd.services."serial-getty@ttyS0".after = [ "gavmo-rebuild.service" ];

  # Skip building the NixOS manual; not needed for a debug image and saves
  # a meaningful chunk of build time.
  documentation.enable = lib.mkDefault false;

  system.stateVersion = "24.11";
}
