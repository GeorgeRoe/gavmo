{ lib, pkgs, ... }:

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
  opencodeWeb = pkgs.opencode.overrideAttrs (old: {
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

      # Applies one exact textual substitution inside $settings. Refuses to
      # proceed unless $from occurs in the file EXACTLY once.
      patchOnce() {
        FROM="$1" TO="$2" FILE="$settings" perl -0777 -i -pe '
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
        ' "$settings"
      }

      # File tree / Command palette / Server status: plain default-value
      # flips (false -> true) in `defaultSettings.general`.
      patchOnce 'showFileTree: false,' 'showFileTree: true,'
      patchOnce 'showSearch: false,' 'showSearch: true,'
      patchOnce 'showStatus: false,' 'showStatus: true,'

      # Show agent: default-value flip, plus forcing the first-run
      # auto-init branch (see comment above) to always resolve on.
      patchOnce 'showCustomAgents: false,' 'showCustomAgents: true,'
      patchOnce 'return existing || previousVersion !== undefined' 'return true'

      # Timeline display prefs: pure default-value flips, no auto-init
      # override to worry about (plain `withFallback` accessors).
      patchOnce 'showReasoningSummaries: false,' 'showReasoningSummaries: true,'
      patchOnce 'shellToolPartsExpanded: false,' 'shellToolPartsExpanded: true,'
      patchOnce 'editToolPartsExpanded: false,' 'editToolPartsExpanded: true,'
    '';
  });
in
{
  # Milestone 1 image: real `opencode web`, authenticated to Google Vertex
  # AI, running against the live 9p-shared /workspace. Builds directly on
  # the milestone-0 "plumbing proof" (QEMU + 9p + port-forward + health +
  # QMP stop), which is unchanged below. The only things new in M1 are:
  #   - installing `opencode` + `git`,
  #   - a managed (non-overridable) /etc/opencode/opencode.json, so a repo
  #     cannot swap the provider/model allowlist out from under GAVMO,
  #   - a minimal /etc/gavmo/AGENTS.md baked in via `instructions`,
  #   - the `gavmo-opencode` systemd service replacing the M0 dummy server.
  #
  # No Rust changes: the `vm` crate and crates/vm/examples/boot.rs are
  # unchanged from M0 - same guest port, same mounts, same command.
  #
  # See nix/base/flake.nix for how this gets built into a qcow2, and
  # crates/vm/examples/boot.rs for the runner that boots it.

  # Kernel modules for the 9p shares. Not strictly required (kmod can often
  # autoload filesystem modules on demand), but explicit here so the mounts
  # don't depend on autodetection in a minimal image.
  boot.kernelModules = [
    "9p"
    "9pnet"
    "9pnet_virtio"
  ];

  # --- The Rust <-> image contract -----------------------------------------
  # Mount tags here MUST match MOUNT_TAG_WORKSPACE / MOUNT_TAG_GCLOUD in
  # crates/vm/src/handle.rs, and cache=none matches the daemon's share spec
  # decision (favor coherence over speed) from crates/vm/plan.md.
  fileSystems."/workspace" = {
    device = "workspace";
    fsType = "9p";
    options = [
      "trans=virtio"
      "version=9p2000.L"
      "cache=none"
      "msize=1048576"
      "rw"
      "nofail"
      "x-systemd.device-timeout=10s"
    ];
  };

  fileSystems."/etc/gavmo/gcloud" = {
    device = "gcloud";
    fsType = "9p";
    options = [
      "trans=virtio"
      "version=9p2000.L"
      "cache=none"
      "msize=1048576"
      "ro"
      "nofail"
      "x-systemd.device-timeout=10s"
    ];
  };

  # opencode itself (patched - see `opencodeWeb` above), plus git since
  # opencode shells out to it for diffs/commits/etc.
  environment.systemPackages = [
    opencodeWeb
    pkgs.git
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

    # The VM is the security boundary, not opencode's permission system -
    # once `bash` is allow-all, per-category denies (read/external_directory/
    # etc.) are bypassable via a shell command anyway (e.g. a denied
    # external_directory read is just `cat <path>` through bash), so they'd
    # only add prompting friction without adding real protection. Hence a
    # top-level catch-all rather than allowing some categories and not
    # others. The git-remote denies below are a tripwire only (equally
    # bypassable, e.g. via a wrapper script) - harmless to keep since there
    # are no git credentials in the VM for them to work anyway.
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
  '';

  # --- opencode web -----------------------------------------------------
  # Replaces the M0 dummy server on the same fixed guest port.
  #
  # Session state lives on the VM's own overlay disk (HOME=/root), NOT on
  # the 9p-shared /workspace. This was tried first (state in
  # /workspace/.gavmo/local, host-visible, surviving a VM reset) but
  # OpenCode's SQLite session database threw `disk I/O error`
  # (SQLITE_IOERR) on startup every time - 9p's locking/IO semantics don't
  # satisfy SQLite, even with cache=none. Native ext4 on the overlay disk
  # works reliably. The tradeoff: session history persists across VM
  # stop/restart (the overlay persists) but is wiped on a full VM reset,
  # and isn't visible from the host. Your actual project files in
  # /workspace are unaffected - only OpenCode's internal DB moved.
  systemd.services.gavmo-opencode = {
    description = "GAVMO milestone-1 opencode web server";
    # Only `network.target` (not `network-online.target`): opencode doesn't
    # need working network to start listening, only later for Vertex calls,
    # and depending on "online" risks stalling on NixOS's wait-online
    # plumbing. Mirrors what the M0 dummy service used successfully.
    after = [
      "workspace.mount"
      "network.target"
    ];
    requires = [ "workspace.mount" ];
    wantedBy = [ "multi-user.target" ];
    # `opencode web` attempts to open a browser on startup; without this,
    # a headless VM has no `xdg-open` and the resulting ENOENT can crash
    # the process into a restart loop. No-op it.
    path = [ (pkgs.writeShellScriptBin "xdg-open" "exit 0") ];
    environment = {
      GOOGLE_CLOUD_PROJECT = "redacted-gcp-project";
      GOOGLE_APPLICATION_CREDENTIALS = "/etc/gavmo/gcloud/application_default_credentials.json";
      VERTEX_LOCATION = "global";
      # Force `safe.directory = *` via git's env-var config injection rather
      # than /etc/gitconfig (nixpkgs' git doesn't reliably read that) - this
      # guarantees it applies to every git call opencode makes, sidestepping
      # "dubious ownership" errors git can raise over 9p-shared repos.
      GIT_CONFIG_COUNT = "1";
      GIT_CONFIG_KEY_0 = "safe.directory";
      GIT_CONFIG_VALUE_0 = "*";
      # Native filesystem (the overlay disk), not the 9p /workspace share -
      # see the comment above this service for why.
      HOME = "/root";
    };
    serviceConfig = {
      ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p /root/.local/share /root/.config /root/.cache";
      ExecStart = "${opencodeWeb}/bin/opencode web --hostname 0.0.0.0 --port 7080";
      WorkingDirectory = "/workspace";
      Restart = "always";
      RestartSec = "1";
      # Route to the serial console (in addition to the journal) so
      # console.log actually shows opencode's real startup output/errors -
      # the M1 boot hang was invisible without this.
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
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

  # Skip building the NixOS manual; not needed for a debug image and saves
  # a meaningful chunk of build time.
  documentation.enable = lib.mkDefault false;

  system.stateVersion = "24.11";
}
