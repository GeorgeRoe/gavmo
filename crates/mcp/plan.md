# `mcp` crate — GAVMO's host-side tool gateway (brief Phase 4)

> This is the design for GAVMO's Jira + secrets MCP gateway, the brief's
> (`gavmo-chatgpt-plan.md` §11) Phase 4 vertical slice. It's being built as a
> sequence of de-risked sub-milestones (P4.0–P4.4); this doc covers the
> mechanism as it's built, the same "as-built reference" style as
> `crates/vm/plan.md` and `crates/gavmo/plan.md`.
>
> **Status: ✅ P4.0 done — re-verified on real hardware.** The first
> real-hardware pass confirmed the design's single biggest unverified
> assumption — the guest reaches the host gateway via `10.0.2.2` — but surfaced
> a genuine bug: rmcp's default DNS-rebinding `Host`-header allowlist rejected
> every guest request with a bare `403 Forbidden` before any auth/tool code ran
> (see §4.1). That was fixed in `service()`'s config and covered by a regression
> test. A second real-hardware pass then confirmed the fix end-to-end:
> `gavmo_ping` is discovered and callable from a booted workspace, a *current*
> workspace token lists `gavmo_ping` (HTTP 200), and a *stale* token (from a
> stopped VM) returns an empty tool list (`{"tools":[]}`) — confirming
> invalidation — isolated to Stop's `clear_mcp_token` (confirmed with no daemon
> restart). See §4.1 for the full result.

## 1. What this crate is

Pure mechanism, same spirit as `vm`: it knows how to run an MCP server over
Streamable HTTP and gate every call behind a per-workspace bearer token, but
has no idea what a "workspace", a "secret", or `gavmo.toml` actually are —
those are the daemon's job (`crates/gavmo`), injected via the
[`TokenAuthority`] trait so this crate has zero dependency on `gavmo` (same
responsibility split as `vm`'s `VmConfig`/daemon boundary).

## 2. Decisions

| Area | Decision |
|---|---|
| MCP server implementation | `rmcp` (official Rust SDK), pinned exactly (`=3.5.1`) — its HTTP-transport internals are young and have shifted across releases; an unpinned bump risks silently changing the `ServerHandler`/`StreamableHttpService` shapes this crate relies on |
| Transport | Streamable HTTP, **stateless** (`legacy_session_mode(false)`) — every call is independently authorized by its own bearer token, so there's no conversation-scoped session state worth keeping |
| Auth | Per-call bearer token (`Authorization: Bearer <token>`), resolved via [`TokenAuthority`] — **not** axum middleware; `rmcp`'s `StreamableHttpService` already injects `http::request::Parts` into every `RequestContext`'s extensions, so the token is read directly inside `list_tools`/`call_tool` |
| Token delivery to the guest | A dedicated read-only 9p share (`mcp_dir`, mount tag `mcp`) parallel to the existing `gcloud` share — **not** a file under `/workspace` (would land the capability token inside the user's, gitignored-but-still-present, repo dir) |
| Listener | A **dedicated** host port (`GAVMO_MCP_PORT = 7071`), separate from the UI's `:3000` — see §3 |
| "Enabled" posture | Always-on, empty-by-default: the managed `mcp.gavmo-tools` config entry and the token share exist for *every* workspace; an unauthenticated or tool-less workspace just sees an empty tool list, not a connection failure |
| P4.0 scope | Exactly one tool, `gavmo_ping` — proves the whole pipeline (guest reachability, auth, OpenCode wiring) before any Jira/secrets logic is written |

## 3. Why a dedicated listener, not the existing UI port

The daemon's existing axum/leptos server (`crates/gavmo/src/main.rs`) binds
`127.0.0.1:3000` for the control-panel UI and its `#[server]` functions.
QEMU's user-mode networking means the guest can already reach **any** host
loopback port via `10.0.2.2` — so the guest can already reach `:3000` too.
This was a **pre-existing exposure** at the time this crate was written, not
something it introduced — and has since been **closed** by
`crates/gavmo/src/auth.rs`'s session-cookie gate (see §6 for the resolution).

Putting the MCP gateway on its own port (`7071`) rather than nesting it onto
the existing router keeps the two concerns separable going forward: the UI
port could later be restricted from guest access (e.g. only bind it after
checking the request's source, or firewall it in the guest) without touching
MCP reachability, since they're independent listeners. It costs one extra
`TcpListener`/`axum::serve` task in `main.rs` — negligible.

## 4. P4.0 — prove the pipeline (current milestone)

The narrowest possible slice that exercises every risky, unverified part of
the design before any Jira/secrets logic exists:

- `crates/mcp`: `GavmoMcpHandler` implements `rmcp::ServerHandler` directly
  (no `#[tool]`/`#[tool_router]` macros — full control over per-call auth),
  exposing one tool, `gavmo_ping`, which echoes back the calling workspace's
  id. `list_tools` returns an empty list for an invalid/missing token (not an
  error); `call_tool` returns a tool-level `CallToolResult::error` for the
  same case (per `rmcp`'s own error-handling convention: `Err(McpError)` is
  for "can't route this at all", not "you're not authorized" — see its
  README).
- `crates/vm`: a third read-only 9p share (`VmConfig::mcp_dir`, mount tag
  `mcp`) alongside `repo_dir`/`gcloud_dir`.
- `crates/gavmo`:
  - `AppState` implements `mcp::TokenAuthority` against an in-memory
    `std::sync::Mutex<HashMap<token, workspace_id>>` (`registry.rs`).
  - `open_workspace` mints a fresh UUID token and writes
    `<mcp_dir>/token.env` (`GAVMO_MCP_TOKEN=<uuid>`) before building
    `VmConfig` — but **only** on an actual new boot, never on the
    already-booting/running early-return path (silently rotating a
    *running* VM's token would invalidate the one its already-started
    `gavmo-opencode` already loaded, breaking an otherwise-healthy VM).
  - `stop_workspace`/`reset_workspace` both invalidate the workspace's
    current token up front, before touching anything else — a stopped VM
    must never be able to authorize a gateway call again.
  - `main.rs` runs a second `axum::serve` task bound to
    `127.0.0.1:7071` (`mcp::GAVMO_MCP_PORT`), nesting `mcp::service(app_state)`
    at `/mcp`.
- `nix/base/configuration.nix`:
  - `fileSystems."/etc/gavmo/mcp"` — the read-only mount, identical options
    to the existing `gcloud` mount.
  - `gavmo-opencode`'s `serviceConfig.EnvironmentFile =
    "-/etc/gavmo/mcp/token.env"` — loads `GAVMO_MCP_TOKEN` straight from the
    share, no guest-side parsing script needed. Leading `-` makes a missing
    file non-fatal (mirrors the `nofail` posture of the 9p mounts
    themselves); `Restart = "always"` (pre-existing) means a lost
    mount-vs-service-start race self-heals on the next restart.
  - The managed `opencode.json` gets a new `mcp.gavmo-tools` entry: `type =
    "remote"`, `url = "http://10.0.2.2:7071/mcp"` (`10.0.2.2` is QEMU
    user-mode networking's host address), `enabled = true` unconditionally,
    `headers.Authorization = "Bearer {env:GAVMO_MCP_TOKEN}"` (OpenCode's own
    `{env:VAR}` header substitution — see
    [opencode.ai/docs/mcp-servers](https://opencode.ai/docs/mcp-servers/)).
    Baked into the **managed**, non-overridable config (same rationale as the
    provider/model allowlist above it) so a repo's own `opencode.json`
    cannot redirect this at an attacker-controlled URL.

### P4.0's hardware gate

This milestone is **not done** until, on real NixOS/KVM hardware:

1. A booted workspace's OpenCode discovers `gavmo_ping` (confirms the whole
   chain: 9p mount → `EnvironmentFile` → `{env:...}` substitution → remote
   MCP handshake against `10.0.2.2:7071`).
2. Calling `gavmo_ping` from inside that session returns the pong with the
   correct workspace id.
3. Stopping that workspace and confirming `gavmo_ping` is no longer callable
   (manually, e.g. via `curl` with the old token) proves token invalidation
   actually works end-to-end, not just in the sidecar's in-memory-map tests.

If step 1 fails because `10.0.2.2` isn't reachable from the guest for some
reason, the documented fallback is QEMU `-netdev user,...,guestfwd=...`
(forwarding a guest-side connection to a host-side handler) instead of
relying on the generic NAT gateway address.

#### 4.1 First real-hardware run: result

Ran on a real NixOS/KVM host. Findings, diagnosed via `curl` both
host-side (`http://127.0.0.1:7071/mcp`) and guest-side
(`http://10.0.2.2:7071/mcp`), plus `cat`ing the guest's `token.env`:

- **Step 1's core risk — guest reachability — is confirmed working.**
  `curl -v http://10.0.2.2:7071/mcp` from inside the guest successfully
  established a TCP connection to the host daemon. This was the design's
  single biggest unverified assumption (§6) and it holds; no `guestfwd`
  fallback is needed.
- **Token delivery is confirmed working.** `/etc/gavmo/mcp/token.env`
  contained a well-formed `GAVMO_MCP_TOKEN=<uuid>` inside the guest,
  matching what the daemon minted.
- **A real bug was found:** every guest request got back a bare
  `HTTP/1.1 403 Forbidden` / `Forbidden: Host header is not allowed`,
  before reaching any of this crate's auth or tool code. Root cause:
  `rmcp`'s `StreamableHttpService` has built-in DNS-rebinding protection —
  it validates the request's `Host` header against
  `StreamableHttpServerConfig::allowed_hosts`, which **defaults to
  loopback names only** (`localhost`/`127.0.0.1`/`::1`). The guest's
  requests carry `Host: 10.0.2.2:7071` (the address it actually connects
  to), which was never on that list.
- **Fixed** in `service()` (`crates/mcp/src/lib.rs`):
  `.with_allowed_hosts(["localhost", "127.0.0.1", "::1", "10.0.2.2"])` —
  widens the allowlist rather than disabling the check entirely (keeps
  DNS-rebinding protection against arbitrary origins, just also accepts
  the one address the guest legitimately uses). Covered by a sidecar
  regression test (`service_allows_both_loopback_and_the_qemu_guest_host_address`)
  asserting both loopback and `10.0.2.2` are present in the built
  service's config, so a future refactor that drops this can't regress
  silently.
- **Re-verified (second real-hardware pass):** the fix above now holds
  end-to-end.
  - **Steps 1–2 (discovery + call):** a booted workspace's OpenCode discovers
    `gavmo_ping` and calling it returns the pong with the correct workspace id
    — the `403` is gone.
  - **Step 3 (invalidation):** with the daemon running, `curl`ing the gateway
    (`POST http://127.0.0.1:7071/mcp`, `tools/list`) with a workspace's
    *current* token returns HTTP 200 and lists `gavmo_ping`; with a *stale*
    token (captured before stopping that VM) it returns HTTP 200 with an empty
    list (`{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}`). The empty list is
    the gateway's "always-on, empty-by-default" response to a token that no
    longer resolves to a workspace (`list_tools` returns `vec![]` for an
    unauthorized caller — see `src/lib.rs`), i.e. invalidation working.
  - **Observed:** each Open mints a fresh per-session token; a stale token stays
    rejected even after the same VM is brought back up (the old token is never
    re-added to the daemon's in-memory map).
  - **Isolated to Stop:** invalidation was attributed to Stop's
    `clear_mcp_token` specifically (not merely a daemon restart wiping the map):
    with the daemon left running, a workspace's current token listed `gavmo_ping`
    (HTTP 200, non-empty), and after stopping that VM — with **no** daemon
    restart — the *same* token flipped to the empty list. Since the in-memory map
    was never wiped, `clear_mcp_token` on stop is what dropped it.
- **Operational note for whoever re-runs the gate:** this fix is
  daemon-only — no VM image rebuild needed. But restarting the daemon
  wipes its in-memory token map, so a VM that's still running (or whose
  guest still has an old `token.env`) will hold a token the new daemon no
  longer recognizes. After rebuilding/restarting the daemon, **Stop then
  Open** the workspace (not Reset — no need to touch the overlay/image) so
  a fresh token is minted and delivered that matches the running daemon.

## 5. Testing strategy

Same split as `vm`'s and `gavmo`'s own testing strategies: pure logic in the
sidecar, transport/hardware behavior verified for real.

- **In the sidecar (`cargo test -p mcp`, `cargo test -p vm`,
  `cargo test -p gavmo --features ssr`):**
  - `bearer_token_from_headers` — pure `http::HeaderMap` parsing, exhaustively
    unit-tested (missing header, wrong scheme, non-UTF-8, well-formed,
    empty-after-prefix) with no `rmcp`/transport machinery involved.
  - `ping_tool()`'s shape (name, empty-object schema).
  - A fake `TokenAuthority` proving the trait's basic known/unknown-token
    contract, independent of any real token store.
  - `vm`'s `build_qemu_args` covering all three shares (workspace rw, gcloud
    ro, mcp ro).
  - `gavmo`'s `mcp_share_dir_name` (deterministic, filesystem-safe,
    collision-free for distinct ids) and the full `mint_mcp_token` /
    `TokenAuthority::resolve` / `clear_mcp_token` round trip: minting writes
    a well-formed `token.env` and makes the token resolve; re-minting for the
    same workspace invalidates the previous token; clearing invalidates
    immediately; an unknown token never resolves.
  - **Deliberately not attempted in the sidecar:** driving the actual
    `rmcp::ServerHandler` methods through a hand-constructed
    `RequestContext`/HTTP request. `RequestContext` has no public
    test-friendly constructor (it carries a real `Peer`), and hand-rolling
    the stateless-mode JSON-RPC wire format (per-request `_meta` protocol
    signals, etc.) well enough to trust a test built that way is a
    significant undertaking for a stateless SDK whose exact wire contract is
    exactly the piece that needs real-world (not reimplemented-by-us)
    verification anyway — see the hardware gate above instead.
- **On the real NixOS host:** §4's three-point gate — **passed** (§4.1). The
  first run confirmed reachability + token delivery and found/fixed one bug
  (the `Host`-header allowlist); a second run re-verified the fix end-to-end:
  `gavmo_ping` discovery + call, a current token lists the tool, and a stopped
  VM's token is invalidated (isolated to Stop's `clear_mcp_token`).

## 6. Known risks / accepted gaps (carried from the top-level plan)

- **`10.0.2.2` reachability — confirmed working** (§4.1), no longer an open
  risk. (It did surface a *different* real issue — rmcp's `Host`-header
  allowlist — now fixed; see §4.1.)
- **`:3000` UI-port exposure — ✅ closed.** The guest could reach the
  daemon's UI port (`:3000`) and its `#[server]` functions via the same
  `10.0.2.2` path this gateway uses. Putting MCP on its own
  bearer-token-gated port (§3) never worsened this, but didn't fix it either
  — the actual fix landed separately, in `crates/gavmo/src/auth.rs`: every
  route except `/login` and static assets now requires a session cookie
  matching a host-only secret (`~/.local/share/gavmo/ui_token`, never
  written anywhere a guest's 9p shares can reach), delivered to the user via
  a login URL the `gavmo` command prints on startup. Same bearer-token
  posture as this crate's own gateway, applied to the UI's browser-facing
  surface via a cookie instead of a header. See `crates/gavmo/plan.md`'s
  auth section for the full design.
- **`EnvironmentFile` vs. mount-ordering race:** `gavmo-opencode` has no
  explicit systemd ordering against `etc-gavmo-mcp.mount` completing (same
  pre-existing gap as the `gcloud` share, which `GOOGLE_APPLICATION_CREDENTIALS`
  also doesn't order against) — accepted as consistent with existing
  precedent in this file rather than a new regression, with `Restart =
  "always"` as the self-healing backstop.
- **Prompt-injection / data-exfiltration caveat (brief §11):** gating the
  *token* doesn't gate what the agent does with data a real tool (Jira, in
  P4.2+) legitimately returns — mitigated later via minimal fields/result
  caps, not eliminated by this gateway's existence alone.

## 7. What's next

The original P4.1–P4.3 sketch below has since been **built**, though the design
evolved — see `gavmo-tools-config-plan.md` for the as-built project-scoped slice
and how it differs from this early sketch:

- **P4.1 ✅ built** — host secret store (`crates/secrets`, `keyring` backend) +
  the repo tool-request schema. Landed as **`gavmo.yaml` (YAML)**, not the
  `gavmo.toml` sketched here (GAVMO standardized on YAML; see
  `gavmo-tools-config-plan.md` §5), parsed/validated in `crates/teamspec`.
- **P4.2 ✅ built, generalized** — instead of typed per-service Jira tools, a
  single generic **host-locked HTTP forwarder** (`type: http`) covers Jira,
  Supabase, etc. with zero per-service code (`crate::integrations::forward`;
  read-only is delegated to the credential's own scopes). See
  `gavmo-tools-config-plan.md` §7–§8.
- **P4.3 ✅ built** — the control-panel Tools panel (`WorkspaceTools`/`ToolRow`
  in `crates/gavmo/src/app.rs`): per-workspace requested tools, shown
  destination URL, configure-secret form, and Save/Update/Test/Clear. Supplying
  the secret *is* the user's approval — a repo cannot self-enable a tool.

**P4.4 ✅ verified on real hardware** — the token-exfiltration property is
confirmed: a configured integration credential is injected host-side into the
outbound request and reaches **no** VM-visible surface.

- **Test setup.** A temporary `gavmo.yaml` at the repo root declared one tool
  (`exfil-probe`, `type: http`, `auth: bearer`, `base_url: https://example.com`,
  `test_path: /`). `example.com` was used deliberately: it host-locks and
  resolves cleanly, exercising the full inject-credential forward path without
  any real service or real secret. The credential was a distinctive sentinel
  (`testing`) stored via the Tools panel.
- **Forwarder works end-to-end.** Calling `exfil-probe` (GET `/`) through the
  gateway returned `HTTP 200` with only example.com's HTML body — the host
  attached the credential to the *outbound* request and returned only the
  response to the caller; the sentinel was never echoed back to the agent.
- **The credential reaches no guest surface.** Verified from inside the guest:
  - `/etc/gavmo/mcp/token.env` held **only** `GAVMO_MCP_TOKEN=<uuid>` — not the
    sentinel. (`-rw------- agent`, 53 bytes.)
  - The sentinel was **absent** from the opencode process environment
    (`/proc/<pid>/environ`).
  - The sentinel was **absent** from `/etc/gavmo` (token + gcloud shares) and
    from `/workspace` (bar the `gavmo.yaml` itself and the ordinary English word
    "testing" in docs/source — never as a stored credential).
  - opencode's own state (`~/.local/share/opencode/opencode.db*`, `log/`) held
    **no** `"authorization":"Bearer <sentinel>"` field — the credential was never
    recorded as an actual header anywhere in session/tool-call state.
- **Test-harness caveat (important for whoever re-runs this).** Because the
  sentinel was the literal word `testing` and the verification was driven from
  *inside* an opencode session, opencode journaled the grep commands (and this
  agent's own messages) into its WAL — so naive `grep 'Bearer testing'` over
  opencode state produces **self-inflicted** matches that grow every time you
  grep again (a feedback loop). The authoritative check is whether the sentinel
  appears in an actual request-header *field* (it does not), not a raw substring
  count. Next time: use a sentinel that is **not** a common English word and is
  **never typed into a shell command** (e.g. configure it only through the Tools
  panel), and/or inspect opencode state from a context that isn't itself being
  recorded.

- **Deferred hardening** (see `gavmo-tools-config-plan.md` §11): encrypted
  Option-B vault behind the `SecretStore` trait; URL-binding of credentials.
- **Open tidy-ups** (ibid. §13): git-remote-else-path workspace identity; rename
  `crates/teamspec` (no longer a "team" spec); pick + pin an AEAD crate.
