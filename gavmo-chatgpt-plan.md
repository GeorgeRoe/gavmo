# Local AI Coding Workspace --- Project Brief

> **Purpose:** A local-first, cross-platform application that lets
> non-technical teammates open a repository and use an AI coding agent
> in an isolated Linux workspace, without needing to install or
> understand Docker, VMs, or agent tooling.
>
> **Status:** Architecture direction, not a final implementation
> specification. Validate the risky assumptions with small prototypes
> before committing to interfaces.

## 1. Executive summary

Build a Rust daemon and a tiny CLI. The CLI asks the daemon to open a
repository/workspace; the daemon manages a local VM, project
environment, OpenCode, configured tools, and host-side secrets. OpenCode
supplies the agent runtime and, ideally, its existing web UI. The
repository remains on the user's machine and is shared into the VM
rather than uploaded or synced to a cloud service.

The current preferred division of responsibilities is:

-   **Rust daemon:** local control plane, workspace/VM lifecycle, API,
    configuration, tool routing, secret storage, and browser launching.
-   **VM:** isolation boundary for the coding agent and its
    shell/processes.
-   **Nix:** declarative project development environment inside the VM.
-   **OpenCode:** agent runtime, model/tool loop, sessions, streaming,
    and potentially the user-facing web UI.
-   **`.agent.yaml` (name provisional):** repository-level declaration
    of which platform tools are enabled and how they are configured.
-   **Host-side tool gateway (likely MCP):** exposes narrowly scoped
    capabilities such as read-only Jira access without putting
    credentials in the VM or model context.
-   **Host project directory:** remains the source of truth for project
    files.

The guiding idea is:

> **The daemon manages machines; Nix manages the environment; OpenCode
> manages the agent; the tool gateway manages privileged capabilities;
> the host secret store manages credentials.**

## 2. Original problem and existing setup

The previous system uses Bash scripts to launch Docker containers based
on images stored/built in GitLab. The current working directory is
mounted into the container, and OpenCode runs inside it. Authentication
currently involves putting the user's gcloud authentication
configuration into the container.

There was also a `workbench.Dockerfile` convention: a repository could
define a workbench sidecar container, and the scripts would launch it
with a FIFO-based messaging mechanism between containers. That made it
possible to give the AI access to selected tools---for example, npm---or
project-specific helpers.

Problems to address:

-   Teammates should not need Docker or command-line/VM knowledge.
-   It should work on Windows and Linux on x86-64 Intel/AMD machines.
-   Project files must remain on the client computer; do not sync
    project files to a cloud service.
-   The UI should be friendly to non-technical users.
-   Teams need repo-specific environments and tools.
-   Credentials must not be exposed to the model or arbitrary commands.
-   Minimize the amount of custom agent UI and environment-provisioning
    code to maintain.

## 3. User experience goal

The intended happy path is approximately:

``` bash
agent .
```

The CLI asks the local daemon to open/create a workspace for the current
directory. The daemon prepares or reuses the workspace and then opens a
browser to the agent UI.

Desired experience:

1.  User installs/downloads the application; no Docker installation or
    manual VM setup.
2.  User opens a repository with `agent .`.
3.  The daemon identifies the repository and reads its configuration.
4.  A VM is created or started.
5.  The repository is shared into the guest at `/workspace`.
6.  The repo's Nix environment is made available.
7.  OpenCode starts, preferably with its web interface.
8.  The browser opens the workspace UI.
9.  The user can have multiple conversations/sessions in the same
    workspace.
10. The user can stop, restart, or reset the VM without losing the
    source files.

A user should not need to understand which components are involved.

## 4. High-level architecture

``` text
CLI (`agent .`) ───────┐
                       │ localhost HTTP
Browser ───────────────┤
                       ▼
                ┌──────────────────────┐
                │ Rust daemon (host)   │
                │                      │
                │ Workspace manager    │
                │ VM manager           │
                │ Config/policy        │
                │ Secret store         │
                │ Tool/MCP gateway     │
                │ Local HTTP API       │
                └──────────┬───────────┘
                           │ VM lifecycle / private connection
                           ▼
                ┌──────────────────────┐
                │ Local Linux VM        │
                │                      │
                │ OpenCode + web UI    │
                │ Nix                  │
                │ Project tools        │
                │ `/workspace`         │
                └──────────┬───────────┘
                           │
                           │ host directory sharing
                           ▼
                Host repository directory

OpenCode in VM ── private, workspace-scoped MCP connection ──►
Host daemon/tool gateway ── secret lookup + HTTPS ──► Jira
```

The diagram is conceptual. Exact VM networking, directory sharing, MCP
transport, and process boundaries remain to be proven on both operating
systems.

## 5. Daemon and CLI

### Daemon responsibilities

-   Run as a local long-lived process.
-   Expose a localhost HTTP API; use WebSockets only where useful for
    daemon-level events.
-   Manage workspaces, VM lifecycle, images, and OpenCode
    startup/health.
-   Read and validate repository configuration.
-   Keep workspace metadata in a local database (SQLite is a reasonable
    starting point).
-   Manage secrets and credential references on the host.
-   Expose approved host-side tools through a scoped gateway.
-   Open the browser to the correct workspace UI.
-   Handle upgrades, cleanup, VM restarts, and logs.

### CLI responsibilities

Keep the CLI thin. It should call the daemon's HTTP API, not have a
separate implementation of the business logic.

Possible commands:

``` bash
agent .
agent open .
agent list
agent stop <workspace>
agent reset <workspace>
agent status
```

Exact command names are undecided.

### IPC

No separate IPC mechanism is needed initially. The CLI, browser, and
future integrations can use the same localhost HTTP API. Named pipes or
Unix sockets can be considered later if there is a concrete need. Bind
to loopback (`127.0.0.1`/appropriate IPv6 loopback), not all interfaces,
and design local authentication/CSRF protections deliberately.

## 6. Workspace, VM, and conversation model

-   **One VM per workspace/project**, not one VM per conversation.
-   A workspace can contain multiple OpenCode sessions/conversations.
-   The VM is the isolated execution environment.
-   The source repository stays in the original host directory and is
    shared into the VM at `/workspace`.
-   The VM should ideally be disposable/recreatable. Resetting the VM
    must not delete or overwrite the host project.
-   Persist workspace metadata separately from project contents, e.g. in
    a local SQLite database under the user's application data directory.
-   Use versioned base VM images and an explicit upgrade/reset strategy.

## 7. VM and cross-platform plan

### Target platform

-   Windows x86-64
-   Linux x86-64
-   Intel and AMD CPUs

The goal is a simple distributable Rust application, with no
user-managed Docker or VM installation. This does **not** guarantee that
every locked-down corporate machine will work without prerequisites:
firmware virtualization, OS hypervisor features, endpoint security,
permissions, and filesystem quirks may still matter.

### Candidate: QEMU

QEMU is the leading candidate because it can run an x86-64 Linux guest
on both Windows and Linux hosts. Expected acceleration paths:

-   Linux host: KVM where available.
-   Windows host: Windows Hypervisor Platform (WHPX), where available.
-   Software emulation (QEMU TCG) may work when hardware acceleration is
    unavailable, but can be far too slow for a comfortable coding-agent
    experience.

Do not treat QEMU as a user prerequisite. Package it with the app or
have the app download a pinned, verified version. Be mindful of
licenses, redistribution obligations, code signing, update integrity,
and bundle size.

Firecracker is not a good default for this cross-platform desktop target
because it is Linux/KVM-oriented. Other hypervisors can be evaluated,
but avoid building a custom hypervisor.

### Base VM image

Keep a fixed, versioned Linux image with only baseline components, such
as:

-   Linux OS and boot configuration
-   Nix
-   OpenCode
-   a small guest control/health agent, if required
-   basic networking and filesystem support

Do not rebuild the entire VM for every repo-specific tool. Nix should
supply the project development environment within the VM.

### Guest control

A small guest-side service could provide health checks and lifecycle
operations such as starting OpenCode, checking status, and running
controlled management commands. It is not OpenCode itself. It may be
useful to avoid depending on SSH, but its protocol and privileges should
be kept small and secure.

Possible communication is a private guest network and HTTP API. Do not
expose the guest control API to the public network. A QEMU guest address
such as `10.0.2.x` is only an example, not a fixed contract.

### Project directory sharing --- highest-risk VM question

The host path must appear inside Linux as `/workspace` without uploading
or syncing source files.

-   `virtiofs` is attractive on Linux.
-   Windows-host directory sharing and the best QEMU implementation need
    validation; support and operational maturity can differ from Linux.
-   Alternatives include a network filesystem or a narrowly designed
    host/guest file-sharing service, but these have performance and
    security trade-offs.
-   Test symlinks, permissions, file watching, case sensitivity, atomic
    renames, large repositories, node_modules, Git operations, and
    Windows path edge cases.

**Do not assume this part works well until prototyped on real Windows
and Linux hosts.**

### Networking

QEMU user-mode NAT is a candidate for outbound guest access without
requiring a bridged network or administrator setup. The VM will likely
need outbound access for package registries, Git, Vertex AI, and
approved services. Decide how to constrain outbound access and handle
corporate proxies later.

### Packaging and updates

Potential approaches:

1.  Download pinned VM/QEMU assets on first run, verifying
    hashes/signatures.
2.  Bundle them in an installer/package.
3.  Use a hybrid approach.

A single tiny binary may not be practical if it has to carry a complete
VM image and QEMU, so "one binary" should mean a simple install/launch
experience, not necessarily that every asset is embedded in the
executable.

## 8. Nix for repository environments

The repository can use `flake.nix` and `flake.lock` to declare the
project development environment. Example (illustrative only; validate
the current Nix syntax and package names):

``` nix
{
  description = "Project development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          nodejs_22
          git
          ripgrep
          jq
        ];
      };
    };
}
```

The VM is Linux x86-64, so the guest Nix system is `x86_64-linux`
regardless of whether the host is Windows or Linux.

The agent should be started within the project's environment when
practical, so tools such as Node, npm, Python, Go, Rust, etc. are
naturally available to its shell. Confirm the best mechanism
(e.g. `nix develop --command ...`) with the chosen OpenCode deployment.

Nix should describe **what software/environment exists**, not act as the
security boundary. A flake can execute build/evaluation logic and fetch
dependencies; treat repo configuration as untrusted input, and consider
pinning, review/approval, resource limits, and network policy.

The platform should not try to recreate a package manager or a
Dockerfile language. Let Nix handle project packages.

## 9. OpenCode and the UI

OpenCode is the agent runtime, not the underlying model. It orchestrates
model requests, context, tool calls, shell/file operations, and the
agent loop. Vertex AI/Gemini can be the model provider if the chosen
OpenCode version and configuration support the required
authentication/provider flow.

OpenCode's TUI is only one interface. OpenCode has a web
interface/server mode; the current direction is to run OpenCode's own
web UI inside the VM and open it in the user's browser, rather than
immediately building a custom chat UI.

Potential flow:

1.  Daemon boots the VM.
2.  Daemon/guest control starts OpenCode's web/server mode.
3.  Daemon waits for a health check.
4.  Daemon makes the UI safely reachable from the host, e.g. via a local
    forwarded port.
5.  Daemon opens the browser.

OpenCode's server/API should be checked against the exact pinned release
before implementation. Validate: - How web mode is started and
configured. - How authentication/pairing works. - How to bind the server
safely. - Whether sessions can be listed and multiple sessions
shown/managed in the UI as desired. - How server URLs are forwarded from
guest to host. - How to configure MCP servers programmatically. -
Whether the UI and API are suitable for the team's non-technical users.

Do not assume every detail of the UI or authentication behavior remains
constant across versions.

### Streaming and sessions

Prefer OpenCode's own structured agent events and web client rather than
implementing our own token/tool streaming UI. The daemon should
initially care about VM health, workspace lifecycle, and launch
URL---not model token streams, tool-call rendering, or chat
presentation.

A workspace is an environment; a session is a conversation. Multiple
sessions should reuse the workspace VM.

## 10. Repository tool configuration

The old `workbench.Dockerfile`/FIFO approach demonstrated a useful
requirement: repositories should be able to declare the tools the agent
can use. Preserve the capability model, but do not make Docker
containers or FIFO the public abstraction.

A provisional file is `.agent.yaml` (name not final). It might look
like:

``` yaml
version: 1

tools:
  jira:
    enabled: true
    base_url: "https://ourteam.atlassian.net"
    secret: "jira-read-token"
```

Or a list form could be used; choose one format and validate it with a
schema. The example is illustrative, not a finalized schema.

Distinguish:

-   **Environment/packages:** managed by Nix.
-   **Tools/capabilities:** actions the agent can invoke, such as Jira
    search or issue lookup.
-   **Services:** processes such as Postgres/Redis that may need to run
    in the workspace.
-   **Permissions/policy:** what the VM and each tool are actually
    allowed to do.

The repo can request capabilities, but it must not be allowed to grant
itself host permissions or unrestricted access to secrets.

### Built-in and custom tools

Built-in tools could include Jira, GitHub, GitLab, or other internal
APIs. Repo-local tools may be a later feature, but they require a clear
trust model. A repository-provided tool script is executable code and
should be treated as untrusted. Avoid automatically running arbitrary
host-side scripts from a cloned repository.

The platform's tool registry should define tool implementations,
supported operations, configuration schema, and permissions. The repo
config selects/configures an implementation; it should not be able to
redefine the trust boundary.

## 11. Jira read-only tool --- recommended wiring

### Problem

The existing Jira CLI does not work with newer tokens in the desired
way, and the available option apparently requires broader read/write
access. MCP cannot currently be set up by the user due to lack of
administrator access. A simple Bash script that calls `curl` and
prepends the company's Jira base URL has worked.

The goal is a small read-only Jira capability that can be enabled per
repo, with different Jira credentials per repo, without exposing tokens
to the AI or VM.

### Preferred design: host-side tool gateway, likely MCP

Implement a Jira tool in the host daemon/tool runtime. The daemon
exposes a workspace-scoped MCP server to OpenCode inside the VM. The
Jira token remains on the host; the tool gateway makes the authenticated
HTTPS request and returns only the result.

``` text
OpenCode in VM
    │
    │ jira_search / jira_get_issue / jira_get_comments
    ▼
Workspace-scoped MCP endpoint on host daemon
    │
    ├── verify workspace identity
    ├── verify Jira tool is enabled
    ├── validate requested operation and arguments
    ├── resolve the configured secret reference
    ├── call Jira REST API with credential
    └── return limited/sanitized response
         │
         ▼
      Jira API
```

The token must not be: - Put in `.agent.yaml` or Git. - Passed as an
OpenCode environment variable. - Written into a file inside the VM. -
Included in model prompts or tool descriptions. - Returned in MCP
results or logs.

The token is used by the host tool implementation to make the HTTP
request. The model sees only the result, e.g. issue key, summary,
status, assignee, or comments.

### Repo config and host secret store

The repository declares a **secret name/reference**, not the secret
value:

``` yaml
tools:
  jira:
    enabled: true
    base_url: "https://ourteam.atlassian.net"
    secret: "jira-read-token"
```

The host secret store namespaces entries by a stable repository identity
plus secret name:

``` text
(repository identity, secret name) → secret value
```

For example:

``` text
github.com/mycompany/project-a / jira-read-token → token A
github.com/mycompany/project-b / jira-read-token → token B
```

Do not key secrets solely by the directory name. Prefer a normalized,
verified Git remote URL as repository identity, while handling local
repos with missing or ambiguous remotes. Be careful with URL
normalization, SSH-vs-HTTPS remote forms, forks, multiple remotes, and
spoofing. The daemon should make the resolved repository identity
visible to the user and support explicit binding/approval.

The secret name is only a reference. The daemon must not trust
repository config to access arbitrary credentials. The user should
explicitly authorize which credential is bound to which repo/tool. A
malicious repository should not be able to select a production/admin
secret just by naming it.

### Secret storage

A host-side secret store is preferred over committing credentials to a
repository. Investigate OS-native credential facilities (Windows
Credential Manager/DPAPI; Linux Secret Service/keyring, with a fallback
story for headless environments). A local encrypted database may be
possible, but its key management must be thought through. Do not invent
a home-grown crypto scheme.

The user may enter the Jira token once in a daemon-managed setup flow,
and it is stored locally against the repo identity and secret name. The
daemon can prompt when a repository requests a secret that is not
configured.

### Read-only is enforced by implementation

Do not rely on a prompt telling the model not to write. Implement only
read operations in the Jira tool, such as: - `jira_search` -
`jira_get_issue` - `jira_get_comments`

Use appropriate Jira REST endpoints and HTTP methods, validate
arguments, limit result sizes, and do not implement
create/update/delete/comment-write operations. If possible, use a
genuinely read-only Jira token as well. Tool-level read-only design and
server-side credential permissions are complementary.

### MCP design

A single generic `agent-tools` MCP server per workspace is likely
cleaner than a separate MCP server per tool:

``` text
OpenCode
  └── agent-tools MCP
        ├── jira_search
        ├── jira_get_issue
        └── jira_get_comments
```

Only tools enabled for that workspace should be advertised. The daemon
should validate workspace binding and tool authorization on every call,
not just at connection time.

The guest connects to the host daemon through a private/forwarded
channel. Avoid exposing a no-auth MCP endpoint on the LAN. Use a
per-workspace unguessable token or another robust local channel-auth
mechanism, restrict the endpoint to the intended guest/workspace, and
plan for token rotation/revocation. Never treat "it is localhost" or "it
is on the VM network" as sufficient authentication by itself.

The daemon could expose the MCP server over a supported HTTP transport,
but the exact MCP transport and OpenCode configuration should be
confirmed against the pinned versions. Do not hard-code the illustrative
URLs from earlier discussion.

### Why not a custom OpenCode tool inside the VM?

A custom tool inside the VM would need access to the Jira credential,
creating pressure to inject it into the guest or its environment. A
host-side MCP gateway avoids that and gives one place to enforce
workspace-scoped tool policy and credential lookup.

### Important security qualification

A host-side gateway prevents the token itself from being given to the
model/VM, but it does **not** prevent the agent from using the Jira
capability in unexpected ways. The model can still read any data that
the enabled tool allows and may include it in prompts, files, or
responses. Therefore: - Return only the minimum data needed. - Keep the
API strictly read-only. - Limit result size and query scope where
appropriate. - Consider audit logs without logging secrets or sensitive
payloads. - Obtain user approval for especially sensitive tools. - Treat
tool output as untrusted data. - Ensure a malicious repository cannot
silently enable privileged tools or bind itself to a credential.

## 12. Google Vertex AI / model authentication

Vertex AI/Gemini is a likely model backend. The existing setup copies
gcloud authentication configuration into a container; that approach
should be revisited.

Prefer a narrowly scoped, short-lived, or otherwise limited
authentication mechanism rather than exposing a user's broad gcloud
configuration to an arbitrary agent process. Determine the exact
authentication mechanism supported by the pinned OpenCode/provider
version and how to keep credentials out of the guest if possible. This
is an open security/design item; no final mechanism has been selected.

## 13. Security principles

-   The VM is the primary isolation boundary for untrusted agent
    execution.
-   Treat model-generated commands and repo-controlled configuration as
    potentially hostile.
-   Share only the project directory needed by the agent; do not mount
    the whole home directory.
-   Do not expose SSH keys, browser cookies, broad cloud credentials, or
    host secrets to the VM.
-   Nix flakes and build steps can execute code and fetch dependencies;
    review/policy decisions may be needed.
-   Keep the daemon API bound to loopback and protect against local
    web/CSRF abuse.
-   Tool gateway operations must be authenticated, workspace-scoped,
    validated, and least-privilege.
-   Repository declarations request capabilities; only host-side
    policy/user approval can grant privileged access.
-   Keep the host tool gateway's exposed operation set narrow and
    explicit.
-   Avoid logging credentials, authorization headers, or unnecessary
    sensitive tool output.
-   Ensure reset/destroy operations cannot delete source files.

## 14. Suggested Rust technology choices

Initial candidates (not mandatory): - Tokio for async runtime. - Axum
for local HTTP API and possibly WebSocket endpoints. - Serde for
serialization/config. - SQLite with `sqlx` for local workspace
metadata. - A YAML parser plus a schema/versioning layer for
`.agent.yaml`. - A keyring/OS credential integration library after
validating Windows and Linux behavior. - QEMU launched as a child
process, with platform-specific host detection/configuration behind a VM
abstraction. - MCP server implementation compatible with the selected
OpenCode/MCP versions.

Keep the code organized around domain boundaries, not just one large
daemon module. For example:

``` text
crates/
  daemon/
  cli/
  vm/
  workspace/
  config/
  secrets/
  tools/
  mcp/
  opencode/
  api/
```

The precise crate structure can evolve; do not over-engineer before the
prototype.

## 15. Development plan: validate risk early

### Phase 1 --- VM proof of concept (highest priority)

Build a small Rust program that: - Locates/uses a pinned QEMU build. -
Starts a Linux x86-64 VM on Windows and Linux. - Uses hardware
acceleration where available. - Gives the guest outbound network
access. - Shares the chosen host directory into the guest as
`/workspace`. - Runs `ls /workspace` and a simple command. - Stops
cleanly and can restart. - Demonstrates that project changes are
immediately visible on both sides.

Test on real Windows and Linux x86-64 machines. Directory sharing is the
main architecture risk.

### Phase 2 --- OpenCode proof of concept

Inside the VM: - Install/configure Nix and OpenCode. - Launch OpenCode
in web/server mode. - Verify the web UI is reachable from the host
through a controlled local forward. - Verify multiple sessions and
session persistence behavior. - Verify Vertex AI authentication. -
Verify the desired tool/MCP configuration works in the pinned OpenCode
version.

### Phase 3 --- Daemon and workspace lifecycle

Implement: - `agent .` → daemon API call. - Workspace identity and
metadata. - VM create/start/stop/reset. - Health checks and OpenCode
startup. - Browser launch. - Versioned image updates and cleanup.

### Phase 4 --- Jira and secrets vertical slice

Build one end-to-end capability: - `.agent.yaml` enables Jira and names
a secret reference. - User configures the secret in the host store for
that repository. - Daemon exposes a workspace-scoped MCP endpoint. -
OpenCode sees only read-only Jira tools. - Jira requests execute on the
host using the credential. - The model receives only structured
issue/search/comment results. - Tests demonstrate the token is absent
from VM files, environment, prompts, results, and logs.

### Phase 5 --- Nix and declarative tool expansion

-   Support repo Nix flakes.
-   Add schema validation and useful errors for `.agent.yaml`.
-   Add more built-in tools.
-   Consider trusted, sandboxed custom tools only after the built-in
    model is solid.
-   Add approvals/policy controls and auditability.

### Phase 6 --- Product hardening

-   Installer and signed updates.
-   VM image verification.
-   Error handling and diagnostics.
-   Windows/Linux filesystem edge cases.
-   Endpoint security and corporate proxy testing.
-   Resource limits and VM cleanup.
-   Upgrade, rollback, and recovery behavior.

## 16. Decisions made so far

-   Target Windows/Linux x86-64, Intel/AMD.
-   Keep project files local; share them into the VM rather than syncing
    to a cloud.
-   Prefer a Rust daemon plus thin CLI and local browser UI.
-   Use localhost HTTP as the initial common interface; no separate IPC
    mechanism is required initially.
-   Prefer one VM per workspace/project, with multiple sessions inside
    it.
-   QEMU is the leading cross-platform VM candidate, subject to proof of
    concept.
-   Use a fixed Linux base image and Nix for repo-specific environments.
-   Prefer OpenCode's existing web UI rather than immediately building a
    custom UI.
-   Let OpenCode own agent execution and streaming initially.
-   Use repo config to declare enabled tools.
-   Keep privileged credentials on the host.
-   Prefer a host-side, workspace-scoped MCP/tool gateway for Jira.
-   Keep Jira read-only operations enforced in the implementation, not
    merely in prompts.
-   Treat the repo's secret reference as a name, not a credential value.
-   Namespace secrets by verified repository identity plus secret name,
    with explicit user authorization.

## 17. Open questions to resolve

1.  **Directory sharing:** What is the most reliable and performant
    host-to-guest filesystem approach on Windows and Linux?
2.  **Hypervisor packaging:** Bundle QEMU or download it on first run?
    What are the licensing, signing, update, and enterprise deployment
    requirements?
3.  **Virtualization prerequisites:** Require hardware virtualization,
    or support a slow fallback?
4.  **Guest control:** Is a custom guest agent necessary, or can the
    first prototype use simpler mechanisms?
5.  **OpenCode web:** Confirm current CLI flags, auth/pairing, safe
    binding, multi-session UX, and browser forwarding for the pinned
    version.
6.  **OpenCode/MCP integration:** Confirm the exact transport and config
    required for a per-workspace daemon-hosted MCP endpoint.
7.  **Vertex AI:** Choose a secure, supported authentication flow
    without exposing broad host credentials to the agent.
8.  **Secret storage:** Choose and test Windows/Linux native credential
    storage and headless behavior.
9.  **Repository identity:** Normalize Git remotes safely and support
    repositories without a remote or with multiple remotes.
10. **Trust/approval:** Decide whether tools requested by `.agent.yaml`
    require user approval and how to prevent malicious repos from
    silently enabling capabilities.
11. **Tool permissions:** Define whether tool-level permissions are
    fixed by built-in tool implementations, configurable by users, or
    both.
12. **Network controls:** Decide whether outbound VM traffic is
    unrestricted initially or filtered.
13. **Nix trust:** Decide how to handle untrusted flakes, downloads,
    evaluation, and build resource limits.
14. **Workspace persistence:** Decide what survives VM
    stop/restart/reset beyond host source files (e.g. OpenCode sessions,
    Nix store/cache, home directory).
15. **Platform UX:** Installer, auto-update, first-run checks, logs,
    diagnostics, and corporate machine compatibility.

## 18. Working principles for future coding agents

When using AI coding agents to implement this project, give them these
constraints:

-   Do not build a custom chat UI until OpenCode's web UI has been
    evaluated against real user needs.
-   Do not invent a custom package/environment manager; use Nix for
    project dependencies.
-   Do not pass host secrets into the VM or model context.
-   Do not assume repo configuration is trusted.
-   Do not implement a broad Jira CLI; start with a small read-only API.
-   Do not build a hypervisor; orchestrate QEMU or another established
    VM technology.
-   Do not treat example config, ports, IP addresses, crate layouts, or
    API names in this document as finalized contracts.
-   Prototype the VM and host-directory sharing before investing in the
    rest of the platform.
-   Pin external component versions and verify their current
    behavior/documentation.
-   Keep interfaces small and versioned; prefer working vertical slices
    over speculative abstractions.
-   Add tests for security properties, especially secret isolation and
    workspace scoping.

------------------------------------------------------------------------

## One-sentence product definition

**A local-first Rust workspace manager that runs OpenCode in an isolated
Linux VM, uses Nix to define repo environments, and exposes approved
host-side tools through a secure, workspace-scoped gateway---without
moving project files or credentials into the cloud or agent VM.**
