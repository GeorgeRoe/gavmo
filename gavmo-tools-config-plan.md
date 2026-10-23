# GAVMO tool configuration & secrets — design notes

> **Status: ✅ built (project-scoped slice), including the encrypted vault and
> credential URL-binding hardening.**
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
> The *hardening* seams once listed here as deferred — the encrypted vault and
> credential URL-binding — are now both built (see §3, §5, §10); the only
> remaining deferred item is a friendlier "your OS keychain isn't set up" UX
> (§11).

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

## 3. Secret storage — decision: Option B (encrypted vault, built)

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

**Built: Option B** (encrypted vault keyed by a single keystore entry) —
`secrets::VaultStore` (+ the lazily-opened `secrets::LazyVaultStore` the daemon
actually wires up). The original Option A (`secrets::KeyringStore`, one entry per
secret) remains in the crate as a valid `SecretStore` impl but is no longer the
daemon's backend. Both sit behind the same `SecretStore` trait, so no caller
changed when the backend was swapped.

Implementation notes (as built):
- **One AEAD-encrypted file** (`~/.local/share/gavmo/secrets.vault`,
  `0600` on Unix) holds the whole secret map; it's rewritten wholesale
  (atomic temp-file + rename) on each `set`/`delete`. The map is tiny (a few
  integrations per workspace), so whole-file re-encrypt beats incremental
  crypto on simplicity.
- **Data-key in the keystore.** A single `keyring` entry (`service=gavmo`,
  name=`vault-data-key`, base64 value) holds the 32-byte AEAD key, minted on
  first run. That's the *only* keyring entry GAVMO keeps now (vs. Option A's
  N). `keyring` is pinned + feature-flagged as before (see `crates/secrets/
  Cargo.toml`).
- **AEAD: XChaCha20-Poly1305** (`chacha20poly1305` crate, pinned exactly
  `=0.11.0`). Pure-Rust (no OpenSSL), 24-byte random nonce generated per
  write — no counter/reuse bookkeeping. No home-grown crypto.
- **Fail-closed, lazy.** The data-key lives *only* in the keystore; an
  unreachable keystore (no/locked Secret Service, no D-Bus session bus — the
  common stock-WSL case) makes every vault op return a backend error rather
  than degrading to plaintext. The vault is opened *lazily* (first secret
  access, not daemon startup) via `LazyVaultStore`, so such a host can still
  start the daemon and manage VMs — only tool-secret operations fail on it.
- **No migration** from Option A's per-secret entries: `keyring` has no list
  API, so enumerating prior entries isn't possible; a user re-enters each
  secret once via the Tools panel.

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
- **URL-binding of credentials (built).** A stored credential records the
  canonical `base_url` it was approved for, so it is only ever released to that
  exact destination. A repo changing `base_url` afterward (or a value with no
  binding) fails **closed** at release time and the control panel shows a
  "destination changed from X to Y — re-confirm" warning, so the user re-approves
  *seeing the new URL*. See §8a for the mechanism. (Implemented by **storing the
  canonical URL alongside the credential inside the encrypted vault value**, not
  by hashing it into the key as this section originally sketched — now that GAVMO
  owns the vault format, store-alongside is simpler *and* lets the UI show the old
  URL, which a one-way hash couldn't.)

The irreducible residue is **first-use social engineering** ("paste your prod-DB
token for `evil.com`") at initial setup — mitigated by the shown destination +
live test, not eliminated.

## 6. Workspace identity (for keying secrets)

Secrets are keyed by `(workspace identity, secret name)`. Identity = the
absolute host path (`gavmo-mvp-plan.md:82`, `WorkspaceRecord.path`), composed
with the secret name via `secrets::secret_key`. The brief warns against
path-keying (`gavmo-chatgpt-plan.md:509`); the options considered were:

1. **Keep absolute path (chosen — permanent).** Simplest; aliases on reclone /
   path reuse.
2. Normalized git remote URL (survives reclone; edge cases: no remote, multiple
   remotes, forks). — **rejected.**
3. Git remote if present, else fall back to path, and always show the resolved
   identity to the user before they save a token. — **rejected.**

**Decision: keep the absolute host path as the workspace identity (option 1),
permanently.** The git-remote-derived options (2/3) were considered and
declined: a repo moving/re-cloning to a different path losing its stored
secrets is accepted as the user's responsibility, which doesn't justify the
URL-normalization (`git@host:` vs `https://host/` forms, trailing `.git`,
multi-remote selection) plus one-time secret-migration/re-prompt complexity the
derivation would add.

## 7. Tool types

A single generic **host-locked HTTP forwarder** (`type: http`) covers Jira,
Supabase, etc. with zero per-service code. The agent supplies method + path (+
optional query/body); the host prepends `base_url`, host-locks the result,
injects the credential, forwards via `reqwest`, size-caps the response, and
returns it. **Any HTTP method is allowed**; read-only is delegated to the
credential's own scopes (e.g. a read-only PAT). This replaces the earlier idea of
typed per-service tools (`jira_search`/…) and "built-in types only".

## 8. Host-lock (the one security-relevant primitive, built)

`toolspec::lock_url(base_url, path)` resolves an agent-supplied path against an
integration's `base_url` and returns the full URL **only if it stays on the exact
same scheme + host + port**. Any attempt to reach a different origin — an
absolute URL, a scheme-relative `//other-host`, a different port, a userinfo
confusion trick — is rejected, so the host never forwards the credential
off-origin. A `base_url` path prefix (e.g. an Atlassian gateway base) is
preserved. This is tool correctness, not optional hardening — it is always on.

## 8a. URL-binding (built)

Host-lock stops a request mid-flight from leaving the integration's current
origin, but it can't tell that the *current* origin isn't the one the user
approved — a repo that silently changes `base_url` from `trusted` to `evil`
would pass host-lock (the request does stay on `evil`). URL-binding closes that:

- **Canonical form.** `toolspec::canonical_base_url(base_url)` produces a stable
  destination string — scheme + host + effective port + normalized path prefix —
  reusing the *same* normalization as `lock_url`, so binding and host-locking can
  never disagree on "same destination". It captures only what determines *where*
  a credential goes (not `auth`/`header_name`/`description`, which would cause
  pointless re-prompts), and binds the **path prefix** too, so a same-host tenant
  swap (`/ex/jira/GOOD` → `/ex/jira/EVIL`) is caught, not just a host change.
- **Stored alongside the credential.** `set_tool_secret` wraps the credential in
  a `{ bound_url, credential }` envelope (`crate::integrations::wrap_stored_secret`)
  inside the encrypted vault value; saving a credential *is* the user approving
  the destination the control panel just showed them.
- **Enforced at release (the hard boundary).** `integrations::forward` — the one
  path both `call_tool` and the "Test" button go through — calls `check_binding`
  before attaching the credential: it canonicalizes the tool's *current*
  `base_url` and refuses (fail-closed, tool-level error naming both URLs) on a
  mismatch, on an unbound/legacy value, or if the current `base_url` can't be
  canonicalized. This holds regardless of what any running VM was told at boot.
- **Reflected in resolution + UI.** `classify_binding` gives a tri-state
  (`NotConfigured` / `Configured` / `DestinationChanged`). Boot-time resolution
  advertises only `Configured` tools (so a tampered tool isn't even offered —
  defense-in-depth above the release check); the control panel shows a
  "destination changed from X to Y — re-enter to confirm" warning for
  `DestinationChanged`, with Save re-binding to the new URL.

## 9. Data shapes & local state

**Project `gavmo.yaml`** — see §5 (requests/pointers only, untrusted).

**Local host state:**
- *Secrets* — the Option-B encrypted vault file
  (`~/.local/share/gavmo/secrets.vault`), an XChaCha20-Poly1305 map keyed by
  `secret_key(workspace_id, name)` = `"{workspace_id}\x1f{name}"` (the `\x1f`
  Unit Separator can't appear in a path or name, so the two can't collide). The
  vault's 32-byte data-key is the single `vault-data-key` keyring entry. Each
  value is a `{ bound_url, credential }` envelope (URL-binding — see §8a), so the
  destination a credential was approved for travels *with* it.

## 9a. Crate layout

- **`crates/secrets`** (ssr-only, mechanism): `SecretStore` trait (`get`/`set`/
  `delete` by key) + the encrypted `VaultStore`/`LazyVaultStore` backend (AEAD
  file + `KeyProvider`-sourced data-key) + the legacy `KeyringStore` +
  in-memory backend for tests + the `secret_key(workspace_id, name)` composer.
  No knowledge of Jira/workspaces.
- **`crates/toolspec`** (pure): the `gavmo.yaml` YAML schema (`GavmoConfig` /
  `ToolDef` / `ToolKind` / `AuthScheme`), parse/validate, the host-lock
  `lock_url`, and the URL-binding `canonical_base_url` (same normalization as
  `lock_url`). Bytes-in, no I/O. *(Renamed from `teamspec` — it no longer models
  a "team" spec, only the per-repo tool-request schema.)*
- **`crates/mcp`** (extend): the daemon-implemented `ToolExecutor` trait
  (`list_tools`/`call_tool`) so `reqwest` + credentials never enter this crate;
  `WorkspaceScope` carries the workspace id and the executor looks integrations
  up by it.
- **`crates/gavmo`** (extend, ssr-only): owns policy — resolves identity, reads
  `gavmo.yaml`, implements `ToolExecutor` (vault lookup → host-locked HTTPS
  call → size-capped result), plus the control-panel UI (Leptos `#[server]` fns +
  the Tools panel in `app.rs`). All `SecretStore` calls run on `spawn_blocking`
  threads (the vault does file I/O + an occasional keystore unlock).

## 10. What's built

- **`crates/secrets`** — the encrypted Option-B vault (`VaultStore`: one
  XChaCha20-Poly1305 file, data-key held as the single `vault-data-key` keyring
  entry; `LazyVaultStore` for deferred, fail-closed open) + the legacy
  `KeyringStore` + in-memory backends, all behind the `SecretStore` trait, plus
  the `secret_key` composer. Vault AEAD/round-trip/tamper/wrong-key/fail-closed/
  `0600`-perms paths unit-tested via an in-memory `KeyProvider` (no live keystore
  needed).
- **`crates/toolspec`** — `gavmo.yaml` schema + parse/validate + host-lock
  `lock_url` + URL-binding `canonical_base_url`, exhaustively unit-tested.
- **`ToolExecutor` seam in `crates/mcp`** — transport-neutral
  `ToolDescriptor`/`ToolOutcome`; `gavmo_ping` ported onto it.
- **`crate::integrations`** — resolution (`gavmo.yaml` → candidate tools →
  enabled subset via a batched secret fetch + `classify_binding`, so only tools
  whose credential is present *and* bound to the current `base_url` are enabled),
  wired into `AppState`, resolved on boot / cleared on stop+reset.
- **URL-binding** (§8a) — `canonical_base_url` + the `{bound_url, credential}`
  vault envelope (`wrap_stored_secret`/`parse_stored_secret`), `check_binding`
  enforced at release in `forward`, and the `classify_binding` tri-state driving
  both boot-time resolution and the UI. Unit-tested (canonical equivalence/
  distinctness, envelope round-trip, `forward` reject-on-mismatch/unbound,
  tri-state classification).
- **`integrations::forward`** — real `reqwest` call: host-lock → method →
  **URL-binding check** → credential injection (Basic / Bearer / custom header) →
  size-capped response; shared `http_client`; secret fetched at call time.
- **Control-panel UI** — a Tools panel in the workspace modal
  (`WorkspaceTools`/`ToolRow` in `app.rs`) listing each requested tool with its
  destination URL and a tri-state status (configured / not configured /
  **destination changed**, the last with a "was X, now Y — re-enter to confirm"
  warning); per-tool configure form (Basic shows username+token, Bearer/Header
  show token), plus **Save / Update / Test / Clear**. Backed by
  `list_tool_configs` / `set_tool_secret` / `clear_tool_secret` / `test_tool`.

## 11. Deferred hardening (seams preserved)

Most of the originally-deferred hardening is now built; what's left:

- ~~**Encrypted Option-B vault** (§3)~~ — **built** (`secrets::VaultStore`); see
  §3/§10.
- ~~**URL-binding of credentials** (§5)~~ — **built** (§8a): a credential is
  released only to the exact destination it was approved for; a changed `base_url`
  fails closed and the UI re-prompts visibly.
- **Legible keystore-unavailable UX** — a broken/absent OS keystore (stock WSL)
  currently surfaces as the raw backend error from a fail-closed vault op, not a
  friendly "your keychain isn't set up" banner. Deliberately unshipped; the
  fail-closed behavior itself is correct, only its presentation is terse.

(The team tier — remote team-spec fetch, signing/minisign, TOFU key-pinning — is
**not** deferred; it was removed from scope. See the status note at the top.)

## 12. Security properties this design gives

- A malicious project repo can only *request*; it can never supply or select a
  real credential.
- A tampered `base_url` can't send the token anywhere but that URL mid-request
  (host-lock), the destination is shown + testable before a token is saved, and
  (URL-binding, §8a) an already-saved credential is *only* released to the exact
  destination it was approved for — a later `base_url` change fails closed and
  re-prompts rather than silently redirecting the token.
- Secrets are bound to one workspace; a secret stored for `/other` can't enable a
  different repo.
- **Residual, accepted:** first-use social engineering (persuading a user to
  paste a token for an attacker URL at initial setup); mitigated by showing
  destination URLs + live test calls, not eliminated. The §11 data-exfil caveat
  from `gavmo-chatgpt-plan.md` also still applies (a legitimately-enabled tool
  can return data the agent then writes out) — handled separately via read-only
  credentials / result caps.

## 13. Open questions

1. ~~**Workspace identity:** move from absolute path to git-remote-else-path (§6)?~~
   **Resolved — won't do.** The absolute host path stays the permanent identity;
   a moved/re-cloned repo losing its secrets is accepted as the user's problem
   (see §6 for the rationale).
2. ~~**AEAD crate** for the eventual vault: pick + pin a vetted construction.~~
   **Resolved — done.** XChaCha20-Poly1305 via `chacha20poly1305`, pinned
   exactly (`=0.11.0`); pure-Rust, random 24-byte nonce per write. The vault is
   built (§3/§10).
3. ~~**Rename `crates/teamspec`** now that it no longer models a team spec.~~
   **Resolved — done.** Renamed to `crates/toolspec` (it only models the
   per-repo tool-request schema + host-lock, not a "team" spec).
4. ~~**URL-binding** of credentials (§5): bind a stored credential to the URL it
   was approved for.~~ **Resolved — built** (§8a). Deviated from the originally
   sketched `hash(workspace, canonical_url, name)` key: the canonical URL is
   stored *alongside* the credential in the vault envelope instead, which (now
   that GAVMO owns the vault format) is simpler and lets the UI show the old URL
   on a mismatch — impossible with a one-way hash.

## 14. Relationship to existing docs

- `crates/mcp/plan.md` — the as-built P4.0 gateway + the original P4.1–P4.4
  sketch. This document refines that "What's next" section.
- `gavmo-chatgpt-plan.md` §11 (Jira wiring), §13 (security principles), §15
  (phase plan) — the originating brief.
- `gavmo-mvp-plan.md` — workspace identity = absolute host path (permanent; the
  git-remote alternative was considered and declined — see §6).
