# GAVMO tool configuration & secrets — design notes

> **Status: ✅ built (project-scoped slice), with security hardening deferred.**
> This captures how GAVMO lets the agent reach context beyond the repo (Jira, a
> project-specific platform, etc.) *safely and easily* for **non-technical users
> on their own Windows/Linux desktops**. It extends the Phase 4 vertical slice in
> `crates/mcp/plan.md` (P4.0: the MCP transport + per-workspace bearer token +
> throwaway `gavmo_ping`).
>
> **Scope note (team tier removed):** an earlier draft modelled *two* tiers —
> a shared "team" scope fed by a remote, (eventually) signed team-spec manifest,
> plus a per-repo "project" scope. The team tier has been **dropped**: it added a
> remote-fetch + signing/TOFU machinery whose value didn't justify its
> complexity, since `gavmo.yaml` already makes per-repo setup trivial and those
> configs rarely change. What remains is the **single, project-scoped** model
> below: tools are declared inline in a repo's `gavmo.yaml`, credentials live
> host-side keyed by workspace, and the host-locked HTTP forwarder does the work.
> The deferred *hardening* seams (encrypted vault, URL-binding) still apply and
> are noted where relevant.

## 1. The problem

The agent often needs context that isn't in the repo: read-only Jira, or a
project-specific platform (e.g. a repo that uses Supabase and wants to give devs
read-only access). We want to grant that **without exposing any token to the
model or the VM**, and in a way a non-technical person can set up once and
forget.

The host-side MCP gateway (`crates/mcp`) already keeps the token on the host and
returns only results. This document is about the layers *above* that: where tool
configuration comes from, how credentials are stored, and — crucially — how we
stop a repo from redirecting a user's token to an attacker.

## 2. Hard constraints

- **Cross-platform:** runs on Windows **and** Linux desktops (not servers).
- **Non-technical users:** "set it once, never think about it again." No master
  passwords to remember, no D-Bus incantations, no editing JSON/YAML by hand.
- **Secrets never enter the VM** (already true via the gateway design).
- **Don't hand-roll crypto** (`gavmo-chatgpt-plan.md:528`).
- **Repo-controlled config is hostile** (`gavmo-chatgpt-plan.md:614`): anything
  in the project repo — including its `gavmo.yaml` — must be treated as
  potentially attacker-controlled.

## 3. Secret storage — decision: Option B (vault deferred; keyring in use today)

Four options were considered for host-side at-rest storage of tokens:

- **A — OS-native store directly** (`keyring` crate; Windows Credential Manager,
  Linux Secret Service). Zero crypto code, but the Linux side needs a running,
  unlocked keyring daemon and creates many small entries.
- **B — Encrypted vault file, with the vault's data-key held as a *single* entry
  in the OS keystore (hybrid).** One encrypted file is the source of truth; only
  one keystore entry to manage (far more robust on flaky Linux); password-free
  for the user; one place to inspect/back up/reset.
- **C — Encrypted vault unlocked by a user master password.** Fully uniform /
  no keystore dependency, but **fails the "non-technical, easy" bar** (forgotten
  password = lost secrets). Rejected as the primary path.
- **D — Layered: OS-native first, automatic vault fallback.** Best-available
  everywhere, most code, two paths to test.

**Target: Option B** (encrypted vault keyed by a single keystore entry).
**As built today: Option A** — the `keyring` crate, one entry per secret (see
§9a). The encrypted Option-B vault lands later as **another `SecretStore` impl
behind the same trait** — callers won't change.

Implementation notes:
- `keyring` crate for the keystore entry (Windows Credential Manager / Linux
  Secret Service); **pin exact versions + confirm per-platform feature flags**
  (this repo pins deps deliberately — see `crates/mcp/plan.md` §2).
- A vetted AEAD for the eventual vault file (e.g. an `age`-style construction or
  XChaCha20-Poly1305). No home-grown crypto. Exact crate TBD.

## 4. One tier — project-scoped tools

Model a generic **integration** = tool type + settings + credential. Jira is the
first instance; a project platform (Supabase, etc.) is another. Each integration
is **bound to one repository**: the repo declares it, and the credential the user
supplies is keyed to that workspace.

### Trust split (who declares vs who grants)

- **The repo's `gavmo.yaml` is an untrusted *request*.** A repo can say "I want
  tool X" and name a secret reference; it can **never** supply or pick an actual
  credential. A malicious clone naming a secret gets nothing.
- **The credential is a human-granted *grant*.** The user supplies the token
  once, host-side, via the control panel; it is stored keyed by workspace and
  never leaves the host.

This keeps the one dangerous step — a repo gaining access to real data — a
deliberate human choice (pasting the token into the control panel), while setup
stays a one-time action per repo.

## 5. `gavmo.yaml` — the per-repo tool request

A repo declares the tools it wants inline, keyed by id:

```yaml
tools:
  dafni-jira:
    type: http
    base_url: https://dafni.atlassian.net
    auth: basic                 # basic | bearer | header
    secret: jira-cred           # the *name* of the secret the user supplies
    description: "DAFNI Jira (read via your PAT)"
    instructions: "Create an API token at id.atlassian.net; provide email + token."
    # test_path: rest/api/3/myself   # optional: what the "Test" button GETs
  project-docs:
    type: http
    base_url: https://project.example.com
    auth: bearer
    secret: project-docs-token
```

`gavmo.yaml` holds **references and instructions, never secrets**. The `secret`
field names a credential the user must supply via the control panel; the repo
never carries its value. The file name matches the repo's existing `gavmo.nix` /
`gavmo.*` convention. It is YAML (`serde_yaml`); GAVMO uses no TOML.

### The "repo says where your token goes" risk

Because `gavmo.yaml` lives *inside the untrusted repo*, an attacker landing a PR
/ malicious fork could try to redirect a token (e.g. a Supabase token) by
changing a `base_url`. Mitigations:

- **Host-lock (built, mandatory).** Every forwarded request is locked to the
  integration's `base_url` origin — any request resolving to a different
  scheme/host/port is refused (see §8). A tampered `base_url` can still only send
  the token to *that* `base_url`, never silently to a second host mid-request.
- **Shown destination + live test call (built).** The control panel shows the
  exact URL a credential will be sent to, and a "Test" button makes a live call,
  so a changed destination is visible before the user commits a token.
- **URL-binding of credentials (deferred hardening).** Key each secret by
  `hash(workspace, canonical_url, secret_name)` so a token is only releasable to
  the exact URL it was stored against. A changed URL then fails closed and
  re-prompts the user *seeing the new URL*. Complements the above with zero key
  management; not yet implemented.

The irreducible residue is **first-use social engineering** ("paste your prod-DB
token for `evil.com`") at initial setup — mitigated by the shown destination +
live test, not eliminated.

## 6. Workspace identity (for keying secrets)

Secrets are keyed by `(workspace identity, secret name)`. Today identity = the
absolute host path (`gavmo-mvp-plan.md:82`, `WorkspaceRecord.path`), composed
with the secret name via `secrets::secret_key`. The brief warns against
path-keying (`gavmo-chatgpt-plan.md:509`); options for later:

1. Keep absolute path (simplest; aliases on reclone / path reuse — current).
2. Normalized git remote URL (survives reclone; edge cases: no remote, multiple
   remotes, forks).
3. **Git remote if present, else fall back to path, and always show the resolved
   identity to the user before they save a token.** — recommended future state.

## 7. Tool types

A single generic **host-locked HTTP forwarder** (`type: http`) covers Jira,
Supabase, etc. with zero per-service code. The agent supplies method + path (+
optional query/body); the host prepends `base_url`, host-locks the result,
injects the credential, forwards via `reqwest`, size-caps the response, and
returns it. **Any HTTP method is allowed**; read-only is delegated to the
credential's own scopes (e.g. a read-only PAT). This replaces the earlier idea of
typed per-service tools (`jira_search`/…) and "built-in types only".

## 8. Host-lock (the one security-relevant primitive, built)

`teamspec::lock_url(base_url, path)` resolves an agent-supplied path against an
integration's `base_url` and returns the full URL **only if it stays on the exact
same scheme + host + port**. Any attempt to reach a different origin — an
absolute URL, a scheme-relative `//other-host`, a different port, a userinfo
confusion trick — is rejected, so the host never forwards the credential
off-origin. A `base_url` path prefix (e.g. an Atlassian gateway base) is
preserved. This is tool correctness, not optional hardening — it is always on.

## 9. Data shapes & local state

**Project `gavmo.yaml`** — see §5 (requests/pointers only, untrusted).

**Local host state:**
- *Secrets* — today one keyring entry per secret, keyed
  `secret_key(workspace_id, name)` = `"{workspace_id}\x1f{name}"` (the `\x1f`
  Unit Separator can't appear in a path or name, so the two can't collide).
- *Vault (deferred)* — the Option-B encrypted file, tokens keyed by
  `hash(workspace, canonical_url, secret_name)` (URL-binding).

## 9a. Crate layout

- **`crates/secrets`** (ssr-only, mechanism): `SecretStore` trait (`get`/`set`/
  `delete` by key) + `keyring` backend + in-memory backend for tests + the
  `secret_key(workspace_id, name)` composer. No knowledge of Jira/workspaces.
- **`crates/teamspec`** (pure): the `gavmo.yaml` YAML schema (`GavmoConfig` /
  `ToolDef` / `ToolKind` / `AuthScheme`), parse/validate, and the host-lock
  `lock_url`. Bytes-in, no I/O. *(Name is historical — it no longer models a
  "team" spec; renaming it is a possible future tidy-up.)*
- **`crates/mcp`** (extend): the daemon-implemented `ToolExecutor` trait
  (`list_tools`/`call_tool`) so `reqwest` + credentials never enter this crate;
  `WorkspaceScope` carries the workspace id and the executor looks integrations
  up by it.
- **`crates/gavmo`** (extend, ssr-only): owns policy — resolves identity, reads
  `gavmo.yaml`, implements `ToolExecutor` (keyring lookup → host-locked HTTPS
  call → size-capped result), plus the control-panel UI (Leptos `#[server]` fns +
  the Tools panel in `app.rs`).

## 10. What's built

- **`crates/secrets`** — keyring + in-memory backends, `secret_key` composer.
- **`crates/teamspec`** — `gavmo.yaml` schema + parse/validate + host-lock
  `lock_url`, exhaustively unit-tested.
- **`ToolExecutor` seam in `crates/mcp`** — transport-neutral
  `ToolDescriptor`/`ToolOutcome`; `gavmo_ping` ported onto it.
- **`crate::integrations`** — resolution (`gavmo.yaml` → candidate tools →
  enabled subset via a batched keyring presence check), keyring +
  `workspace_integrations` wired into `AppState`, resolved on boot / cleared on
  stop+reset.
- **`integrations::forward`** — real `reqwest` call: host-lock → method →
  credential injection (Basic / Bearer / custom header) → size-capped response;
  shared `http_client`; secret fetched at call time.
- **Control-panel UI** — a Tools panel in the workspace modal
  (`WorkspaceTools`/`ToolRow` in `app.rs`) listing each requested tool with its
  destination URL and configured status; per-tool configure form (Basic shows
  username+token, Bearer/Header show token), plus **Save / Update / Test /
  Clear**. Backed by `list_tool_configs` / `set_tool_secret` / `clear_tool_secret`
  / `test_tool`.

## 11. Deferred hardening (seams preserved)

The built path omits these; their insertion points are preserved so they drop in
without a rewrite:

- **Encrypted Option-B vault** (§3) — another `SecretStore` impl behind the trait.
- **URL-binding of credentials** (§5) — fails a changed URL closed, re-prompts
  visibly.

(The team tier — remote team-spec fetch, signing/minisign, TOFU key-pinning — is
**not** deferred; it was removed from scope. See the status note at the top.)

## 12. Security properties this design gives

- A malicious project repo can only *request*; it can never supply or select a
  real credential.
- A tampered `base_url` can't send the token anywhere but that URL mid-request
  (host-lock), and the destination is shown + testable before a token is saved.
- Secrets are bound to one workspace; a secret stored for `/other` can't enable a
  different repo.
- **Residual, accepted:** first-use social engineering (persuading a user to
  paste a token for an attacker URL at initial setup); mitigated by showing
  destination URLs + live test calls, not eliminated. The §11 data-exfil caveat
  from `gavmo-chatgpt-plan.md` also still applies (a legitimately-enabled tool
  can return data the agent then writes out) — handled separately via read-only
  credentials / result caps.

## 13. Open questions

1. **Workspace identity:** move from absolute path to git-remote-else-path (§6)?
2. **AEAD crate** for the eventual vault: pick + pin a vetted construction.
3. **Rename `crates/teamspec`** now that it no longer models a team spec.

## 14. Relationship to existing docs

- `crates/mcp/plan.md` — the as-built P4.0 gateway + the original P4.1–P4.4
  sketch. This document refines that "What's next" section.
- `gavmo-chatgpt-plan.md` §11 (Jira wiring), §13 (security principles), §15
  (phase plan) — the originating brief.
- `gavmo-mvp-plan.md` — workspace identity = absolute host path (to be revisited
  per §6).
