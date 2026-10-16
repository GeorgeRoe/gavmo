# GAVMO tool configuration & secrets — design notes (deferred work)

> **Status: 🧭 design/thinking only — not scheduled, not built.** This captures a
> design conversation about how GAVMO should let the agent reach context beyond
> the repo (Jira, project-specific platforms, etc.) *safely and easily* for
> **non-technical users on their own Windows/Linux desktops**. It extends the
> Phase 4 vertical slice already sketched in `crates/mcp/plan.md` (P4.0 built:
> the MCP transport + per-workspace bearer token + throwaway `gavmo_ping`). None
> of what follows is implemented. It is pushed back for later work.
>
> Audience: whoever picks up P4.1+. Read `crates/mcp/plan.md` and
> `gavmo-chatgpt-plan.md` §11/§15 first for the existing context.

## 1. The problem

The agent often needs context that isn't in the repo: read-only Jira for the
whole team, or a project-specific platform (e.g. a repo that uses Supabase and
wants to give devs read-only access). We want to grant that **without exposing
any token to the model or the VM**, and in a way a non-technical person can set
up once and forget.

The host-side MCP gateway (`crates/mcp`) already keeps the token on the host and
returns only results. This document is about the layers *above* that: where tool
configuration comes from, how credentials are stored, and — crucially — how we
stop a repo (or a compromised config source) from redirecting a user's token to
an attacker.

## 2. Hard constraints

- **Cross-platform:** runs on Windows **and** Linux desktops (not servers).
- **Non-technical users:** "set it once, never think about it again." No master
  passwords to remember, no D-Bus incantations, no editing JSON/YAML by hand.
- **Secrets never enter the VM** (already true via the gateway design).
- **Don't hand-roll crypto** (`gavmo-chatgpt-plan.md:528`).
- **Repo-controlled config is hostile** (`gavmo-chatgpt-plan.md:614`): anything
  in the project repo — including its `gavmo.yaml` — must be treated as
  potentially attacker-controlled.

## 3. Secret storage — decision: Option B

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

**Decision: Option B**, with a Option-D-style graceful fallback posture if a
Linux box has no Secret Service. It gives the password-free UX non-technical
users need, reduces the Linux keyring dependency to one reliable entry, and
keeps all secret metadata in one inspectable place.

Implementation notes:
- `keyring` crate for the single data-key entry (Windows Credential Manager /
  Linux Secret Service); **pin exact versions + confirm per-platform feature
  flags** (this repo pins deps deliberately — see `crates/mcp/plan.md` §2).
- A vetted AEAD for the vault file (e.g. an `age`-style construction or
  XChaCha20-Poly1305). No home-grown crypto. Exact crate TBD at P4.1.

## 4. Two tiers of context — team vs project

Model a generic **integration** = tool type + settings + credential. Jira is the
first instance; a project platform (Supabase, etc.) is another. The two tiers
are just the **sharing scope** of the resulting credential:

- **Team scope (shared):** configured once per machine, usable by every project.
  Archetype: team Jira at one instance, one per-user token.
- **Project scope (bound):** tied to one repository identity. Archetype: a
  platform hosting info about one specific project.

"Team" here means **per-user-machine, set once** — *not* centrally synced or
admin-provisioned. Each team member configures their own (per-user) token once on
their own machine. A truly distributed/admin-pushed model is explicitly out of
scope (Phase 6+ territory) but the design leaves a seam for it.

### Trust split (who declares vs who grants)

- **Project tier → the repo's `gavmo.yaml` (untrusted *request*).** A repo can
  say "I want tool X" and name a secret reference; it can **never** supply or
  pick an actual credential. A malicious clone naming a secret gets nothing.
- **Team tier → host-side, control-panel-managed (trusted *grant*).** Team
  integrations are shared across repos and involve real credentials, so they
  belong to the human via the control panel + vault, not to any repo.

### Interaction model — decision

**Team tools set up once, per-project opt-in to use them.** The first time a
given repo wants a team tool, the control panel asks "Allow project
`github.com/org/repo` to use your team Jira?" (one click). This keeps the
dangerous step — a repo gaining access to real team data — a deliberate human
choice, while team setup stays one-time. (The frictionless alternative, where any
opened repo can use team Jira automatically, was rejected because a malicious
repo's agent could then read/exfiltrate team data even under a read-only token.)

## 5. Syncing team config across a team — signed git manifests

The sync mechanism is **a git repo the whole team can access** (no central GAVMO
server needed — everyone already has access-controlled git).

- A **team-spec repo** (e.g. GitLab) holds a YAML manifest declaring the team's
  tools: tool type, URL, and *which secret the user must supply* (plus
  human-readable instructions). It holds **references and instructions, never
  secrets**.
- The project's `gavmo.yaml` **references** that team-spec repo URL, so everyone
  stays up to date from one source.
- The end user supplies their own token (e.g. a Jira PAT) once, into the vault,
  via the control panel.

### Update model — decision: manual, on-demand, reviewed

The daemon **never auto-pulls**. Updating the spec is an explicit human action
(an in-app "Check for updates → review diff → import" button). This turns every
spec change into a reviewed action and closes the silent-poisoning window; the
manual import *is* the pin. A passive "update available" check (reads a version
marker only, applies nothing) can mitigate staleness without auto-pulling.

### Integrity — decision: signed specs (minisign/Ed25519), TOFU on the key

The key risk: a team spec says "send your token to URL X." If the repo is
compromised (or a project points GAVMO at a lookalike spec), X could become
`evil.com`. Mitigations, in layers:

- **Signatures (minisign / Ed25519).** The spec is signed; GAVMO **verifies
  before trusting**. Integrity (any byte change fails) + authenticity (only the
  private-key holder can produce an accepted spec). A compromised repo without
  the key cannot forge an accepted spec.
- **Trust anchor = TOFU on the public key.** GAVMO is a *general* tool (not
  DAFNI-specific), so we can't bake an org key into the installer. Instead the
  public key is **pinned on first import** (like SSH `known_hosts`), fingerprint
  verifiable out-of-band. One human trust decision per team; everything after is
  cryptographic. This is the irreducible single bootstrap — unavoidable.
- **Private-key custody = CI signs on merge (recommended).** The signing key
  lives only in CI secrets; e.g. **GitLab CI signs on merge to a protected
  branch**. Then *who can change tools = who can merge in GitLab* — existing
  permissions govern it, and unsigned direct pushes are rejected. No individual
  holds the key. (Custody is each team's choice; CI-on-merge is best.)
- **Why minisign/signify:** one tiny public key (short base64 string), one
  detached signature, no X.509, no web-of-trust, no keyrings. Rust support
  exists (`rsign2`/`minisign`). SSH signatures (`ssh-keygen -Y`) were considered
  (reuses existing keys) but minisign is the "do one thing well" pick. GPG
  rejected (painful UX for non-technical orgs).
- **Key rotation/revocation:** GAVMO trusts a *set* of pinned keys; rotate by
  importing an updated spec signed with the new key and re-pinning; revoke by
  dropping a key. No separate revocation infrastructure initially.

Because integrity/authenticity come from the signature, the spec repo can even be
**public** — nothing secret lives in it; only the private key must be protected.

### Credential bound to URL (belt-and-braces, no PKI)

Independently of signing, **bind each credential to its canonical URL** in the
vault: key the secret by `hash(scope, canonical_url, secret_name)`. The token is
then only releasable to the *exact* URL it was stored against. A tampered URL
can't resolve the existing token at all — it silently becomes "a different,
not-yet-configured tool," so the agent gets nothing and the user is re-prompted
**seeing the new URL**. Tampering fails closed *and* becomes visible, with zero
key management. This complements signing (which stops a bad spec being accepted
at all).

Human-readable names (`dafni-jira`) provide **legibility**, not security — they
make the pinned URL human-checkable so a change is obvious ("why is DAFNI Jira
suddenly a different address?"). Security comes from pinning the name→URL binding
(URL-binding) and/or signing.

## 6. Project-scoped config has the same problem — same mechanism

Project-scoped secrets have the **same "the repo says where your token goes"
risk — and it's arguably worse**, because the `gavmo.yaml` carrying the request
lives *inside the untrusted repo*. An attacker landing a PR / malicious fork
could redirect, say, a Supabase token.

Key insight: this is **not a separate mechanism**. There is really one concept —
*a signed tool manifest from a change-controlled source* — and "team" vs
"project" is just the **credential's sharing scope**:

- Jira → manifest shared by many repos → credential scope = all referencing repos.
- Supabase → manifest for one repo → credential scope = just that repo.

So `gavmo.yaml` is best reframed as a **pure, untrusted pointer + opt-in list**:
it references signed manifests by URL and opts into tools, but never itself holds
the authoritative URL+secret binding (which lives in a signed, verified
manifest). A project's own tools can live in a signed manifest **in the project
repo, signed by that repo's CI**, TOFU-pinned per project.

Two things soften how much project CI signing is actually required:

- **URL-binding already protects already-configured tokens** with zero CI: a
  changed URL fails closed and re-prompts visibly.
- **The irreducible residue is first-use social engineering** ("paste your
  prod-DB token for `evil.com`"), which signing can't fully remove (same TOFU
  moment). Signing's real value at project scope is ensuring the config came
  through the project's **protected CI/review**, not a random push/fork.

### Options for project scope

1. **Strict unify:** `gavmo.yaml` may *only* reference signed manifests — no
   inline URL+secret ever. Strongest/most consistent; highest friction (every
   project with a tool needs a signed manifest + CI).
2. **Layered (recommended):** `gavmo.yaml` may inline-request a project tool
   unsigned, protected by URL-binding + shown destination + live test-call;
   projects wanting change-controlled config **opt into** project-CI signing
   (same machinery, TOFU-pinned per project). Low friction by default, strong
   where it matters. Builds **one** signing path, applied to both tiers.
3. **Project = team mechanism only:** no inline project tools; a project tool is
   just a signed manifest whose credential scope is one repo. Clean but full
   setup even for a one-off integration.

**Leaning: (2) layered.**

## 7. Workspace identity (for keying secrets)

Secrets are keyed by `(repository identity, secret name)`. Today identity = the
absolute host path (`gavmo-mvp-plan.md:82`, `WorkspaceRecord.path`), but the
brief warns against path-keying (`gavmo-chatgpt-plan.md:509`). Options:

1. Keep absolute path (simplest; aliases on reclone / path reuse — dangerous).
2. Normalized git remote URL (matches brief; survives reclone; edge cases: no
   remote, multiple remotes, forks).
3. **Git remote if present, else fall back to path, and always show the resolved
   identity to the user before they save a token.** — recommended.

## 8. Data shapes (illustrative, for review — not final)

**Team spec** (in the signed team repo, e.g. `team-tools.yaml` +
`team-tools.yaml.minisig`):

```yaml
version: 1
team: DAFNI
tools:
  - id: dafni-jira
    type: jira
    base_url: https://dafni.atlassian.net
    secret:
      name: jira-pat
      instructions: "Create a read-only Jira PAT at …"
```

**Project `gavmo.yaml`** (in the repo, untrusted — *requests/pointers only*):

```yaml
team:
  spec: https://gitlab.com/dafni/gavmo-tools   # import source (+ optional pubkey hint)
  tools:
    - dafni-jira                               # opt into these team tools

tools:
  project-docs:                                # optional project-scoped request
    type: "..."
    base_url: "..."
    secret: project-docs-token
```

Both files are **YAML** (`serde_yaml`); GAVMO uses no TOML. The `gavmo.yaml`
name matches the repo's existing `gavmo.nix` / `gavmo.*` convention.

**Local host state:**

- *Trust store* (plaintext, no secrets): per-spec pinned **public key**, last
  imported+**verified** spec, approvals, name→URL bindings.
- *Vault* (encrypted, Option B): tokens keyed by
  `hash(scope, canonical_url, secret_name)`.

## 9. Tool types

**Built-in tool types only** to start (`jira` first; Supabase/others as added
built-ins), parameterized by URL + secret. Arbitrary "call any endpoint" tools
defined in YAML are powerful but open SSRF / data-exfil and defeat the read-only
guarantee — deferred to a much later, heavily-gated feature.

## 10. Proposed crate layout (keeps the existing mechanism/policy split)

- **`crates/secrets`** (new, mechanism): the Option-B vault — `keyring` + a
  vetted AEAD. No knowledge of Jira/workspaces.
- **`crates/teamspec`** (new, pure): manifest YAML schema, **minisign
  verification**, parsing, diffing. Bytes-in, no I/O.
- **`crates/mcp`** (extend): add a daemon-implemented **`ToolExecutor` trait**
  (sibling to `TokenAuthority`). `list_tools`/`call_tool` dispatch to it so
  **reqwest + credentials never enter this crate**. `WorkspaceScope` grows to
  carry the workspace's enabled tool set.
- **`crates/gavmo`** (extend): owns policy — resolves identity, reads
  `gavmo.yaml`, runs import/trust flow, implements `ToolExecutor` (vault lookup →
  HTTPS call → sanitized result), plus the control-panel UI (Leptos `#[server]`
  fns + components in `app.rs`).

## 11. Suggested sub-milestone sequence (after P4.0)

- **P4.1 — vault + schemas (no network tools yet).** `crates/secrets` (Option B,
  cross-platform round-trip tested), `crates/teamspec` (schema + minisign verify
  + parse, exhaustively unit-tested), `gavmo.yaml` parser, git-remote-else-path
  identity. All sidecar-testable.
- **P4.1b — team-spec import & trust flow.** On-demand fetch, **verify signature
  → TOFU-pin key on first import → diff & approve on later imports**, persist
  verified spec. Control-panel "Add/Update team tools" entry. Written **once**,
  parameterized by scope (shared vs repo-bound) + source (remote repo vs
  in-project file), so it serves both tiers.
- **P4.2 — the Jira tool.** `ToolExecutor` in the daemon: enabled tool → vault
  token (URL-bound) → `reqwest` read-only calls (`jira_search` /
  `jira_get_issue` / `jira_get_comments`) → minimal/sanitized results. `mcp`
  advertises only the workspace's enabled tools.
- **P4.3 — control-panel UX.** Team-spec trust prompt (show fingerprint +
  destination URL), "paste your PAT" form with a live read-only test call,
  per-project opt-in cards, staleness / "update available" indicator.
- **P4.4 — security + hardware.** Tests asserting the token never reaches any
  VM-visible surface (files/env/prompts/results/logs); tampered-spec and
  changed-URL both fail closed; full flow on real hardware (incl. the still-open
  P4.0 token-invalidation check).

## 12. Security properties this is designed to give

- A compromised/public spec repo can't forge a spec (signature) **or** silently
  redirect a token (URL-binding) — both fail closed.
- Who can change tools = **who can merge in GitLab** (CI holds the only signing
  key).
- Exactly **one** human trust decision per team (pin the key once); everything
  after is cryptographic.
- A malicious project repo can only *request*; it can never supply or select a
  real credential.
- **Residual, accepted:** first-use social engineering (persuading a user to
  paste a token for an attacker URL at initial setup) is the irreducible TOFU
  gap; mitigated by showing destination URLs + live test calls + (optional)
  signing that ties config to protected CI, not eliminated. The §11 data-exfil
  caveat from `gavmo-chatgpt-plan.md` also still applies (a legitimately-enabled
  read-only tool can return data the agent then writes out) — handled separately
  via minimal fields / read-only / result caps.

## 13. Open questions to resolve before implementation

1. **GitLab fetch mechanism.** Signing makes integrity independent of transport,
   so the spec *could* be fetched over plain HTTPS (public or deploy-token URL),
   avoiding any git setup for non-technical users. Alternatives: shell to system
   `git` (reuses credential helper, but may be absent on Windows for
   non-technical users), or `git2`/libgit2 (self-contained, fiddly private-repo
   auth). **Lean: HTTPS-fetchable signed spec.** Decide based on where team
   repos live and whether they can be public / deploy-token-readable.
2. **Project-scope posture:** confirm **(2) layered** vs stricter (1)/(3) from §6.
3. **One spec vs. many per project:** allow a list (platform team + sub-team) or
   exactly one? (Affects schema.)
4. **Intentional URL migration** (e.g. Jira changes domain): design a deliberate
   "re-pin, re-verify" path so a legit migration isn't mistaken for an attack.
5. **AEAD crate** for the vault: pick + pin a vetted construction at P4.1.
6. **`gavmo.yaml` as pure pointer/opt-in** (never carrying authoritative
   URL+secret) vs. keeping inline project tool definitions first-class.

## 13a. v1 — functional slice, security deferred (IN PROGRESS)

> Decision (build mode): implement the whole config/tools pipeline **without the
> hardening**, since GAVMO is a small internal tool and security isn't the
> immediate concern. The *architecture* is kept so the deferred features slot in
> later without a rewrite. The irreducible exception kept even in v1 is the
> **base_url host-lock** on the forwarder (see below) — that's tool correctness,
> not hardening.

**Locked v1 decisions:**

- **Secret storage:** `keyring` crate (OS keystore), one entry per secret,
  structured values stored as JSON. *(Encrypted Option-B vault deferred.)*
- **Team spec fetch:** direct raw **HTTPS URL** to the YAML via `reqwest`.
  *(minisign signing / TOFU key-pinning deferred — a no-op `verify` seam is left
  in place.)*
- **Config format:** **YAML** for both `gavmo.yaml` and the team spec
  (`serde_yaml`); no TOML.
- **Jira:** Cloud, modeled as a generic `http` integration (Basic
  `email:api_token`).
- **Tool shape:** one generic **host-locked HTTP forwarder** per enabled
  integration. The agent supplies method + path (+ query/body); the host
  prepends `base_url`, **rejects any request resolving to a different host**
  (mandatory — prevents PAT exfiltration), injects the credential, forwards via
  `reqwest`, size-caps, returns. **Any HTTP method is allowed**; read-only is
  delegated to the PAT's own scopes. *(This replaces the earlier typed
  `jira_search`/`jira_get_issue`/… tools and the "built-in types only" stance in
  §9: one generic `http` type covers Jira, Supabase, etc. with zero per-service
  code.)*
- **Workspace identity:** keep absolute host path for now. *(git-remote identity
  deferred — §7.)*
- **URL-binding of credentials** (§5) deferred.

**Crate changes:**

- `crates/secrets` (new, ssr-only): `SecretStore` trait (`get/set/delete` by
  key) + `keyring` backend + in-memory backend for tests.
- `crates/teamspec` (new, pure): team-spec + `gavmo.yaml` YAML types, parse /
  validate, HTTPS `fetch`, no-op `verify` seam, and the pure host-lock
  request-builder (`resolve_request(base_url, method, path, query)`), all
  unit-testable without a network.
- `crates/mcp` (extend): add a `ToolExecutor` trait (`list_tools(scope)`,
  `async call_tool(scope, name, args)`); `GavmoMcpHandler<A: TokenAuthority +
  ToolExecutor>` delegates; `WorkspaceScope` stays `{workspace_id}` (the executor
  looks integrations up by id). `gavmo_ping` is ported onto the executor.
- `crates/gavmo` (extend, ssr-only): add `reqwest` (rustls), `serde_yaml`,
  `secrets`, `teamspec`; `AppState` implements `ToolExecutor`; new `#[server]`
  fns + control-panel UI.

**v1 sub-milestones:**

- **V1.1 ✅ done** — `crates/secrets` (keyring + in-memory backends) +
  `crates/teamspec` (YAML schema, parsing, host-lock `lock_url`, HTTPS fetch,
  no-op `verify` seam), sidecar-tested.
- **V1.2 ✅ done** — `ToolExecutor` seam in `crates/mcp` (transport-neutral
  `ToolDescriptor`/`ToolOutcome`); `gavmo_ping` ported onto it, gateway behavior
  unchanged.
- **V1.3 ✅ done** — `crate::integrations` resolution (`gavmo.yaml` → enabled
  integrations → scope), keyring + `workspace_integrations` wired into
  `AppState`, resolved on boot / cleared on stop+reset; host-locked stub proved
  end-to-end dispatch.
- **V1.4 ✅ done** — real `integrations::forward` via `reqwest`: host-lock →
  method → credential injection (Basic / Bearer / custom header) → size-capped
  response; shared `http_client`; secret fetched at call time. Unit-tested
  (auth-header injection, response capping, pre-network error branches); the
  live on-the-wire call is left to the hardware gate (same split as P4.0).
- **V1.5 ✅ done** — control-panel UI: a Tools panel in the workspace modal
  (`WorkspaceTools`/`ToolRow` in `app.rs`) listing each requested tool with its
  scope, destination URL, and configured status; per-tool configure form (Basic
  shows username+token, Bearer/Header show token), plus **Save / Update / Test /
  Clear**. Backed by four server fns (`list_tool_configs`, `set_tool_secret`,
  `clear_tool_secret`, `test_tool`) over new `AppState` methods; `integrations`
  refactored to a single `requested_tools` source of truth (with
  `select_integrations` as its configured subset). Verified: ssr build, wasm
  (hydrate) build, clippy on both targets, and the full test suite.

**v1 status: all sub-milestones (V1.1–V1.5) complete.** The end-to-end path works
without the deferred security features (signing/TOFU, encrypted vault,
URL-binding), whose insertion points are preserved.

Everything in §§1–13 remains the target end-state; v1 is the subset that omits
the security features while preserving their insertion points.

## 14. Relationship to existing docs

- `crates/mcp/plan.md` — the as-built P4.0 gateway + the original P4.1–P4.4
  sketch. This document refines and expands that "What's next" section;
  **fold a short pointer into it when this work is scheduled.**
- `gavmo-chatgpt-plan.md` §11 (Jira wiring), §13 (security principles), §15
  (phase plan) — the originating brief.
- `gavmo-mvp-plan.md` — workspace identity = absolute host path (to be revisited
  per §7).
</content>
</invoke>
