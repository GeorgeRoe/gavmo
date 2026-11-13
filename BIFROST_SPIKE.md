# Spike: Bifrost behind the GAVMO daemon (2026-11-09)

Verdict: GO, with the "daemon in front" design. Bifrost cannot be the auth
boundary itself (see 2); the daemon must be.

## 1. Shipping
- Bifrost v2.5.0. Static Linux x86_64 Go binary (no dynamic linking), 152 MB.
- From https://downloads.getmaxim.ai/bifrost/<tag>/linux/amd64/bifrost-http,
  with a `.sha256` file next to it. Not on GitHub releases. This is what
  `npx @maximhq/bifrost` downloads. Latest tag: https://getbifrost.ai/latest-release.
- About 170-190 MB resident.
- Startup: 0.3 s with local datasheets, about 6-10 s when fetching them.
- No `-version` flag. Flags: -app-dir -host -port -base-path -log-level -log-style.

## 2. Security model (key finding)
- File-only mode (`config_store.enabled: false`) cannot use virtual keys or
  inference auth. With `governance` set it fails to start with "config store
  is required". Without it, auth middleware is skipped, so inference is
  anonymous.
- In file-only mode the admin API is readable without auth (`/api/config`,
  `/api/providers`, `/metrics`, the dashboard at `/`). Writes fail with
  "config store not initialized".
- Conclusion: anything that can reach Bifrost's port can spend the user's
  credentials. Guests must not be able to reach it, so the daemon is the only
  door: per-VM token check, then forward. Bind Bifrost to 127.0.0.2.
  - Here, binding 127.0.0.2:18080 left 127.0.0.1:18080 unreachable.
  - Still to verify on a real host: a guest cannot reach host 127.0.0.2.
    slirp only maps 10.0.2.2 to host 127.0.0.1, and a guest's 127.x traffic
    stays inside the guest. Host test: run `nc -l 127.0.0.2 9999` on the host
    and `curl 10.0.2.2:9999` and `curl 127.0.0.2:9999` in a guest; both must
    fail.
- Using SQLite config store mode instead would add DB state, the
  admin-account/setup-token dance and VK lifecycle calls, for nothing gained
  once the daemon fronts it. Not recommended.

## 3. Network / offline
- At startup it downloads https://getbifrost.ai/datasheet (pricing, 3.4 MB),
  /datasheet/model-parameters (24 MB) and /mcp-library. If the pricing
  download fails it exits (FTL), after about 60 s of retries.
- Fix: `framework.pricing.pricing_url` / `model_parameters_url` /
  `mcp_library_url` accept `file://`, and `mcp_library_sync_interval: 0`.
  Tested offline: starts in 0.3 s. Either ship the datasheets or accept the
  startup fetch.
- Plugins active by default: telemetry (local Prometheus `/metrics`; no
  outbound telemetry seen), compat, model-catalog-resolver.
- The only steady-state outbound connection seen was to Google.

## 4. Vertex via host ADC
- `auth_credentials: ""` plus `GOOGLE_APPLICATION_CREDENTIALS` pointing at an
  authorized_user ADC file works.
- Listing models fails ("requires a quota project") and it falls back to its
  static catalog.
- Inference works. Model ids look like `vertex/claude-haiku-5-5@default`.

## 5. opencode 1.18.35 end to end (guest config, no Google env/creds)
```json
"enabled_providers": ["gavmo"],
"model": "gavmo/claude-sonnet-5-5",
"small_model": "gavmo/claude-haiku-5-5",
"provider": { "gavmo": {
  "name": "GAVMO", "npm": "@ai-sdk/anthropic",
  "options": { "baseURL": "http://10.0.2.2:7071/llm/anthropic/v1",
               "apiKey": "{env:GAVMO_MCP_TOKEN}" },
  "models": {
    "claude-sonnet-5-5": { "id": "vertex/claude-sonnet-5-5@default", "name": "Claude Sonnet 5.5",
      "reasoning": true, "tool_call": true, "attachment": true, "temperature": true,
      "limit": { "context": 200000, "output": 64000 } },
    "claude-haiku-5-5": { "id": "vertex/claude-haiku-5-5@default", "...": "same" } } } }
```
- `@ai-sdk/anthropic` is bundled in opencode, so nothing is installed at runtime.
- The token is sent as `x-api-key`.
- Ran a real `opencode run`: it read, edited and ran shell commands over 8
  streamed turns, all 200, and fixed the bug.
- Requests carry `anthropic-beta: thinking-binding-controls-2026-08-01`
  (plus `structured-outputs-2025-11-13` on some), adaptive `thinking`,
  3 `cache_control` markers and 10 tools.
- Prompt caching through the chain: 1st call wrote 6303 cache tokens, 2nd
  call read 6303.
- Thinking blocks come back.
- Vertex directly (rawPredict) accepts all of these beta headers, so Bifrost
  filtering them is not a problem today. Whether Bifrost forwards or drops
  them is unverified.
- opencode only adds `interleaved-thinking`/`fine-grained-tool-streaming`
  betas for provider id `anthropic`. A custom id `gavmo` skips them; naming
  the provider `anthropic` would restore them.
- Config can be redirected for tests with `OPENCODE_TEST_MANAGED_CONFIG_DIR`.
  The guest image could also take the provider block per boot via
  `OPENCODE_CONFIG_CONTENT` (env) if we want models set from the host.

## 6. Side finding
The guest currently gets the whole host gcloud dir, not just ADC: it
includes `credentials.db`, `access_tokens.db` and `legacy_credentials/`
(gcloud CLI refresh tokens). Removing the share fixes that as well.

## 7. Decision: GAVMO's settings panel is the only UI (option A)
- Bifrost stays stateless (file-only config) and hidden on 127.0.0.2; its
  dashboard is not exposed to users. In this mode it is read-only, request
  logs are off, and a WSL Windows browser probably can't reach 127.0.0.2.
- The GAVMO settings panel holds provider settings and generates Bifrost's
  `config.json`. Each new provider means a small form plus config mapping in
  GAVMO.
- Rejected option B: Bifrost's own dashboard under `/bifrost` behind GAVMO's
  login. It would need the SQLite config store and an admin password the
  daemon injects. Revisit if many providers are needed.
- Auth for users:
  - Run `gcloud auth application-default login` on the host (inside WSL on
    Windows). This writes a refresh-token ADC file, which Bifrost reads via
    `GOOGLE_APPLICATION_CREDENTIALS`.
  - Recommend `gcloud auth application-default set-quota-project <project>`
    to silence the model-listing warning.
  - Don't paste `print-access-token` output: it expires after 60 minutes.
- Unverified:
  - token refresh after 1 h of runtime;
  - pasting the ADC JSON into `auth_credentials` instead of using the file.

## 8. Packaging decision
Revised 2026-11-11: GAVMO is distributed as a Nix flake, so Bifrost is a
Nix package rather than something embedded in the daemon. The original plan
(a `bundled-bifrost` Cargo feature whose `build.rs` downloaded and embedded
the binary) is dropped: no build-script downloads, and no 152 MB in the
daemon binary.
- `flake.nix`'s `bifrost` package fetches the pinned upstream binary
  (v2.5.0, sha256-pinned) and installs Bifrost's Apache-2.0 `LICENSE` and
  `THIRD_PARTY_NOTICES.md` beside it (upstream has no NOTICE file).
- The wrapped `gavmo` command sets `GAVMO_BIFROST` to it as a default; the
  user's own `GAVMO_BIFROST` still wins. Dev and tests set it by hand (the
  flake's dev shell does too); without it, `/llm` and the settings page
  report why there are no models.
- Datasheets are still fetched by Bifrost at startup (about 6-10 s, see 1)
  unless `GAVMO_BIFROST_DATA_DIR` is set.

## 9. Implementation outline
1. Daemon supervises bifrost-http:
   - binary from `GAVMO_BIFROST` (the Nix package sets it, see 8);
   - `-host 127.0.0.2 -port <free>`, app-dir under
     `~/.local/share/gavmo/bifrost`;
   - generated file-only `config.json` (logs store off, datasheets file://);
   - env `GOOGLE_APPLICATION_CREDENTIALS`;
   - dies with the daemon (PDEATHSIG); restarted when settings change.
2. `/llm/*` route on the :7071 gateway:
   - resolve token via `TokenAuthority` (`x-api-key` or Bearer);
   - strip it; reverse-proxy streaming to Bifrost;
   - body size limit; no timeout on the stream.
3. Settings panel writes provider settings, which regenerate Bifrost's
   config. Only GCP for now. Done (2026-11-12): `/settings` in the control
   panel, `settings.json`, schema-driven provider forms
   (`crates/gavmo/src/providers/`), `GAVMO_GCP_*` as read-only overrides.
4. Guest image:
   - drop the gcloud share/mount and `GOOGLE_*` env;
   - provider endpoint static in the managed config; the model list and
     default models per boot, as `opencode.json` in the token share loaded
     via `OPENCODE_CONFIG` (done 2026-11-12; a repo's own `opencode.json`
     outranks it, so the endpoint stays in the managed file);
   - this is a base-image change, so workspaces get "reset needed".
5. Packaging: a pinned Nix package (see 8).
