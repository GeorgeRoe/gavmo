# `mcp` crate — GAVMO's host-side tool gateway (brief Phase 4)

> This is the design for GAVMO's Jira + secrets MCP gateway, the brief's
> (`gavmo-chatgpt-plan.md` §11) Phase 4 vertical slice. It's being built as a
> sequence of de-risked sub-milestones (P4.0–P4.4); this doc covers the
> mechanism as it's built, the same "as-built reference" style as
> `crates/vm/plan.md` and `crates/gavmo/plan.md`.
>
> **Status: 🔜 P4.0 in progress — hardware gate run once, found and fixed a
> real bug, fix not yet re-verified.** First real-hardware pass confirmed the
> design's single biggest unverified assumption — the guest reaches the host
> gateway via `10.0.2.2` — but surfaced a genuine bug: rmcp's default
> DNS-rebinding `Host`-header allowlist rejected every guest request with a
> bare `403 Forbidden` before any auth/tool code ran (see §4.1). Fixed in
> `service()`'s config and covered by a regression test, but **not yet
> re-verified against real hardware** — do not consider P4.0 done until a
> fresh run confirms `gavmo_ping` actually works end-to-end and token
> invalidation after stop is confirmed too.

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
- **Not yet re-verified:** the fix above has not been re-run against real
  hardware. Steps 1–3 above still need a fresh pass confirming `gavmo_ping`
  actually succeeds (not just that the TCP connection and token delivery
  work) and that token invalidation after stop holds.
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
- **On the real NixOS host:** §4's three-point gate. Run once (§4.1) —
  reachability and token delivery confirmed, one bug found and fixed; the
  fix itself has not yet been re-verified — see status banner at the top of
  this doc.

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

## 7. What's next (not yet built)

See `gavmo-chatgpt-plan.md` §11/§15 Phase 4 and the plan discussion that
produced this crate for the full sequencing:

- **P4.1** — host secret store (`keyring` crate) + `gavmo.toml` schema
  (TOML, not the brief's originally-sketched `.agent.yaml`) + parser.
- **P4.2** — the actual Jira tool (`jira_search`/`jira_get_issue`/
  `jira_get_comments`), read-only by construction, via `reqwest`.
- **P4.3** — control-panel UI: per-workspace requested-tools panel, a
  "configure secret" form (supplying the secret *is* the user's approval —
  a repo cannot self-enable a tool).
- **P4.4** — security tests + the full Jira flow verified on real hardware,
  asserting the real Jira token never reaches any VM-visible surface (files,
  env, prompts, MCP results, logs).
