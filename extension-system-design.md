# Extension system and registry design

> **Status:** implemented.
>
> The workspace contains the declarative command/skill system, isolated programmable runtime, session supervisor, package manager, TUI and CLI management surfaces, signed multi-registry client, static registry generator, and ordinary official Goal and Loop extension sources described here.
>
> This document describes an extension architecture and user experience for tokio-agent. It covers local user-authored commands, skills, model tools, status-bar contributions, session services such as `/goal` and `/loop`, and a future official registry.

## Executive summary

tokio-agent is well positioned for extensions, but the extension mechanism should not load arbitrary Rust plugins into the main process. The recommended design is a capability-based extension system with:

1. **Declarative extensions** for prompt commands and skills.
2. **Sandboxed WebAssembly extensions** for programmable commands, tools, status segments, and session services.
3. **MCP** as a separate compatibility path for imported Claude Code and Codex-style plugins, not as an extension contribution type.
4. A **session supervisor** between the frontend and `Agent`, allowing extensions to react to lifecycle events without receiving direct access to core internals.
5. An **official signed registry hosted in this GitHub repository** for tokio-agent-maintained extensions, plus explicitly configured third-party registries. Installation and enablement remain separate actions, and unofficial registry content must never appear official.

The implementation begins with local Markdown commands and a data-driven command catalog. The programmable runtime and registries can then be built on top of that stable contribution model. `/loop` is the first built-in service migrated to an official extension, followed by `/goal`.

## Implementation status and deployment prerequisites

The architecture in this document is implemented in the workspace:

- `crates/extension-api` contains the serializable command, lifecycle, action, capability, status, dynamic-tool, companion-protocol, manager, and resource-limit DTOs.
- `crates/plugin` owns strict manifest and package validation, loose command and skill discovery, command routing, project/user configuration and exact capability grants, immutable package storage, lockfiles, declarative reload, action/timer/autonomy policy, the session-service supervisor, companion lifecycle, TUF root verification and rotation, signed registry catalogs, and offline caches.
- `crates/extension-host` is the separately distributed Wasmtime Component Model companion. It has no WASI imports, uses one store per extension, and enforces memory, fuel, epoch deadline, input/output, action, and circuit-breaker limits.
- `crates/registry-tool` creates deterministic immutable package archives plus signed root and index metadata. `.github/workflows/registry.yml` builds official components from source and publishes release assets and Pages metadata.
- The CLI provides list, search, info, install, update, remove, trust-store, local check/new/link/dev, enable, and disable operations. Installation never enables a package; source, publisher, and exact capability changes require explicit approval.
- TUI and headless sessions use the same data-driven catalog and router. The TUI has offline Installed, cached Discover, and Updates views, trust labels, details, capability/context review, explicit install/enable actions, and confirmation before unloading autonomous services.
- Dynamic model tools are snapshotted at request boundaries, identify their owning extension in permission requests, and are removed on deactivation. Structured status remains cached host-rendered data.
- Goal and Loop are ordinary official component sources under `registry/extensions`; their old feature-specific core state and command variants have been removed. They are not installed or enabled by default.

Production publication still deliberately requires the operational inputs identified by this design: a root-key ceremony, the public root embedded with `TOKIO_AGENT_OFFICIAL_ROOT_JSON` at release build time, and CI publication credentials. Development and integration testing use explicitly identified fixture roots and do not silently establish official trust.

## Current project architecture

The implemented dependency boundary follows the design:

- `tokio-agent-extension-api` is the small serializable leaf crate.
- `tokio-agent-plugin` owns discovery, packages, registries, companion lifecycle, and the provider/frontend-neutral supervisor without becoming a dependency of core.
- CLI session composition attaches the shared router, lifecycle hook, timer poller, dynamic tool catalog, and isolated runtime to `Agent`.
- The TUI renders generic command, manager, notice, and status DTOs. It contains no Goal/Loop command parsing or extension execution.
- `ToolCallExecutor` combines built-in tools with a synchronized extension-owned dynamic catalog whose schemas are snapshotted at each provider request boundary.
- MCP remains a separate crate and compatibility path rather than an extension contribution.

## Goals

The extension system should let users:

- Create a new slash command with a simple Markdown file.
- Install extensions from the official registry or an explicitly trusted third-party registry inside the TUI.
- Enable or disable an extension globally or for one project.
- Add commands, skills, model-callable tools, and status-line segments.
- Opt into larger features such as `/goal`, `/loop`, and subagents rather than receiving them by default.
- Understand an extension's permissions and model-context cost before enabling it.
- Develop and test local extensions without publishing them.
- Use the same extension capabilities in interactive and future headless modes.

The system should preserve the project's existing principles:

- Disabled extensions have zero model-context cost and effectively zero runtime cost.
- The core remains provider-neutral and UI-neutral.
- Context is only expanded after explicit user opt-in.
- Permission checks remain centralized and visible.
- The application remains responsive if an extension is slow or broken.
- Extensions cannot crash or corrupt the main process.

## Non-goals for the first version

The first version should not attempt to provide:

- Arbitrary Ratatui widgets or complete custom screens.
- Native dynamic libraries loaded into the main process.
- Arbitrary hooks into every internal function.
- Automatic background extension updates.
- A paid marketplace, billing, or rankings system.
- Silent automatic activation based on model guesses.
- A stable Rust ABI between the host and extensions.
- Unrestricted shell, filesystem, network, environment, or credential access.

## Recommended architecture

Introduce a session supervisor around the current agent:

```text
                         ┌──────────────────────┐
 TUI / headless ────────▶│ SessionSupervisor    │
                         │                      │
                         │ command registry     │
                         │ extension manager    │
                         │ timers/action queue  │
                         └───────┬────────┬─────┘
                                 │        │
                       commands  │        │ lifecycle events
                                 ▼        ▼
                            ┌────────┐  ┌────────────────┐
                            │ Agent  │  │ Extension hosts│
                            └────────┘  └────────────────┘
```

The supervisor should be provider-neutral and frontend-neutral. It would:

- Route built-in commands.
- Route extension commands.
- Forward normal user messages to the agent.
- Broadcast sanitized session events to subscribed extensions.
- Validate extension actions before sending them to the agent.
- Own timers and prioritization of automatic turns.
- Expose current command and status catalogs to the TUI.
- Make the same features available to headless mode later.

The concrete extension loader and registry can remain in `tokio-agent-plugin`. Public protocol types should live in the small leaf crate `tokio-agent-extension-api`, so packages and SDKs do not depend on the entire application.

The core must not depend on `tokio-agent-plugin`; otherwise the current dependency inversion would be lost.

### Proposed crate responsibilities

```text
crates/
  extension-api/   # Stable IDs, manifests, contribution DTOs, events, actions, WIT-facing types
  plugin/          # Discovery, loading, registry client, package store, runtime, SessionSupervisor
  core/            # Agent loop, provider-neutral messages/tools, no concrete plugin loader
  tui/             # Displays generic command catalogs, status segments, and extension manager UI
  cli/             # Composes provider, Agent, extension manager, supervisor, and frontend
```

`extension-api` should remain small and serializable. It should avoid dependencies on Ratatui, provider implementations, config storage, or the concrete WebAssembly runtime.

## Command and event protocols

Frontend-facing commands should become data rather than TUI-specific enums.

```rust
struct CommandDescriptor {
    id: CommandId,              // "tokio.goal:goal"
    name: String,               // "/goal"
    description: String,
    usage: Option<String>,
    source: CommandSource,      // BuiltIn | Extension { id, version }
    available_while_running: bool,
}

enum SessionCommand {
    SubmitMessage(String),
    InvokeCommand {
        id: CommandId,
        arguments: String,
    },
    Interrupt,
    Approve {
        // Existing permission ID and decision.
    },
    Shutdown,
}
```

Extensions should return a limited set of typed actions instead of mutating the agent:

```rust
enum ExtensionAction {
    SubmitPrompt {
        text: String,
        automatic: bool,
    },
    Steer {
        text: String,
    },
    ShowNotice {
        level: NoticeLevel,
        text: String,
    },
    SetStatusSegment(StatusSegment),
    RegisterTool(ToolDescriptor),
    UnregisterTool(ToolId),
    ScheduleTimer {
        id: TimerId,
        after: Duration,
    },
    CancelTimer(TimerId),
    PersistSessionState(Vec<u8>),
}
```

Every sensitive action should require a declared capability and host validation.

User submissions must outrank automatic extension submissions. Only one automatic turn should be admitted at a time, preventing extensions from flooding the queue.

### Session lifecycle events

Session-service extensions need selected lifecycle events:

```rust
enum SessionEvent {
    SessionStarted,
    UserMessageSubmitted,
    AutomaticTurnStarted {
        source: ExtensionId,
    },
    TurnFinished {
        stop: StopReason,
        usage: Usage,
    },
    Interrupted,
    ToolFinished {
        name: String,
        is_error: bool,
    },
    SessionStopping,
    TimerFired {
        id: TimerId,
    },
}
```

Events should contain only what an extension needs. Extensions should not automatically receive the complete transcript, provider metadata, tool output, environment variables, or secrets.

The supervisor should assign sequence numbers to lifecycle events and extension actions where ordering matters. This makes tests deterministic and helps reject stale actions produced after an extension has been disabled.

## Extension types

Users should see one concept—an **extension**—while packages can contribute several capabilities.

### Prompt commands

The easiest way for a user to add a command should be a Markdown file:

```text
~/.config/tokio-agent/commands/review.md
<project>/.tokio-agent/commands/review.md
```

For example:

```markdown
---
description: Review the current changes
argument-hint: "[focus]"
---

Review the current working-tree changes.

Focus requested by the user:
{{ arguments }}
```

This should automatically create `/review` without requiring compilation or a full package manifest.

Internally, loose command files can be represented as synthetic local extensions. This also enables importing Claude and Codex command files as the roadmap intends.

Prompt commands:

- Require no executable code.
- Have no ambient filesystem or network access.
- Add no model context until invoked.
- Can be hot-reloaded during development.
- Should be the recommended starting point for users.

The first template language should remain deliberately small. It can support:

- `{{ arguments }}` for the complete argument string.
- Optional positional arguments such as `{{ arg.1 }}` later.
- Explicit host-provided values such as `{{ cwd }}`.

It should not support arbitrary code evaluation.

### Skills

A skill is an instruction bundle activated explicitly by a command or another extension action.

Installed and enabled skills should not automatically add their full instructions to the system prompt. That would conflict with the project's context policy.

Initial activation should be explicit, for example:

```text
/testing use this project's integration-test workflow
```

Automatic skill discovery can come later through a small catalog or discovery tool once its model-context cost is understood.

The important states are:

- Installed: package is present locally.
- Enabled: its command or activation mechanism is available.
- Active for a turn/session: its instructions and tools are currently in context.

The UI does not need to expose “loaded” as a separate user-managed state.

### Model-callable tools

Model-callable tools contributed by tokio-agent extensions must be self-contained WebAssembly components. Extensions must not install, configure, or launch MCP servers.

MCP remains a separate future ecosystem-compatibility feature. A future Claude Code or Codex-style plugin importer may import MCP configuration as part of compatibility with those ecosystems, but that configuration is not a tokio-agent extension contribution and must use its own trust and permission flow.

Enabled tool schemas enter model context; disabled tools do not. This is the unavoidable context cost of making a model-callable tool available and should be shown in extension details.

The core tool catalog is dynamic. `ToolCallExecutor` combines its fixed built-in vector with an extension-owned catalog, and the request assembler takes a current snapshot of enabled schemas at each request boundary.

Changes during an active provider request should apply at the next request boundary rather than mutating a request already in flight.

Extension tools should use the existing permission vocabulary where possible:

- Read
- Edit
- Execute

The permission request should identify both the tool and owning extension. A prompt should therefore say, for example:

```text
“Deploy Helper” wants to execute:
  kubectl apply -f deployment.yaml
```

rather than presenting the action as if it came from a built-in tool.

### Status segments

Extensions should not receive a Ratatui frame or return arbitrary widgets. The initial UI contribution surface is deliberately limited to commands, structured status segments, notices, and host-rendered extension-management details. Extensions cannot replace the application theme, layout, transcript, composer, or screens, and cannot make tokio-agent look like a different application. Status contributions use plain structured data:

```rust
struct StatusSegment {
    id: String,
    text: String,
    tone: StatusTone,
    side: StatusSide,
    priority: i16,
    min_width: u16,
}
```

The TUI remains responsible for layout and styling.

Status computation must be asynchronous and cached. The render loop must never call extension code. An extension updates its segment after relevant events or a rate-limited timer, and the TUI renders the latest value.

Recommended footer behavior:

```text
 model · cwd   [goal: turn 4] [tests: passing]       ━━━───── 38%
```

When space is limited:

1. Preserve approval and error notices.
2. Preserve the context meter.
3. Drop extension segments by priority.
4. Truncate remaining segments.
5. Never allow ANSI escape sequences or multiline output.

A status segment should have a strict maximum length. Updates should be rate-limited and coalesced so a broken extension cannot force excessive TUI redraws.

### Session services

Session services are the capability required for `/loop`, `/goal`, and later subagents.

A service can subscribe to selected session events and return host actions. High-risk abilities must be separately declared:

- `session.observe`
- `session.submit_automatic`
- `session.schedule`
- `tools.dynamic`
- `status.write`
- `storage.session`
- `storage.user`
- `subagents.spawn`

An ordinary command extension should not automatically receive these capabilities.

The host should enforce global policies around autonomous services:

- Maximum automatic turns.
- Maximum automatic submissions per time window.
- Only one queued continuation per extension.
- User messages have priority.
- Interrupt suppresses pending automatic work.
- A global emergency stop can disable autonomous extensions.
- Incompatible autonomous services cannot silently compete for control.

## Runtime choice

### Native dynamic libraries should not be used

Loading `.so`, `.dylib`, or `.dll` plugins would be a poor fit because:

- Rust has no stable plugin ABI.
- A plugin panic or memory bug can crash or corrupt the main process.
- The workspace denies unsafe code.
- Packages would need platform-specific builds.
- Unloading and upgrading are unreliable.
- Registry code would run with the agent's full authority.

### Arbitrary subprocesses should not be the official registry format

A JSON-RPC subprocess protocol is useful for MCP and local development, but an arbitrary executable has ambient access to the user's files, environment, credentials, processes, and network unless every platform has a robust OS sandbox.

Native executable extensions are outside this extension system. Subprocess protocols remain reserved for separately designed compatibility features such as MCP; registry extensions must not use a “trusted native extension” escape hatch.

### Use WebAssembly for programmable registry extensions

The recommended executable package format is the WebAssembly Component Model with a small versioned WIT interface.

Benefits:

- One portable package across macOS, Linux, and Windows.
- No stable Rust ABI requirement.
- Memory isolation.
- Fuel, memory, and execution-time limits.
- No filesystem, network, environment, clock, or process access unless exposed by the host.
- SDKs can eventually exist for Rust and other component-model languages.
- Components can be started lazily only when invoked or subscribed.

Components should not receive general WASI access. Sensitive operations should use host APIs that pass through the existing permission model.

The main tradeoff is binary size, compile time, and companion-process startup cost. During implementation, measure:

- Release binary size.
- Cold start with no extensions.
- First extension invocation latency.
- Idle memory usage.
- Component-model support.
- Fuel and resource-limit behavior.

Programmable components run through a separate `tokio-agent-extension-host` companion process using Wasmtime. The main process must not embed Wasmtime. This keeps the main CLI smaller, adds a process-isolation boundary around the component runtime, and allows declarative commands to remain available if the companion host is unavailable.

The host process must apply the same default-deny policy and resource limits described below. It must receive only sanitized protocol values and explicitly granted capabilities; it must never inherit provider credentials or expose ambient WASI authority. Failure or termination of the companion process must not terminate the agent session.

`tokio-agent-extension-host` is built and distributed alongside the main executable. The application locates a sibling binary first and then `PATH`, starts it lazily on the first programmable contribution, and performs a versioned host-API handshake. One companion serves a session while each extension receives a separate Wasmtime store and limits. Declarative contributions continue if the companion is absent. The application may restart a crashed companion once; repeated failure trips the session circuit breaker and leaves programmable extensions disabled for that session.

## Package format

A registry package could look like:

```text
extension.toml
README.md
LICENSE
commands/
  goal.md
component/
  extension.wasm
assets/
```

Example manifest:

```toml
manifest_version = 1

id = "tokio.official.goal"
name = "Goal"
version = "1.0.0"
description = "Continue working until an objective is verified complete"
license = "MIT"
host_api = ">=1.0, <2.0"

[runtime]
component = "component/extension.wasm"

[[commands]]
name = "goal"
description = "Work autonomously until an objective is complete"
usage = "/goal <objective> | pause | resume | cancel"
handler = "goal_command"

[[tools]]
name = "update_goal"
description = "Mark the active goal complete or blocked"
handler = "update_goal"
activation = "dynamic"

[[status]]
id = "goal"
side = "left"
priority = 100
handler = "goal_status"

[capabilities]
session_observe = true
session_submit_automatic = true
tools_dynamic = true
status_write = true
storage_session = true
```

The manifest must be readable without launching the component. Search results, command autocomplete, permission review, and package inspection should not execute extension code.

### IDs and command aliases

Internal IDs are namespaced even when visible commands are short:

```text
tokio.official.goal:goal → /goal
```

Built-in command names should be reserved. Extension command collisions should prevent enablement until the user chooses an alias; silently overriding commands would be dangerous.

Package IDs and versions should be immutable after publication. A compromised or broken release can be yanked, but its contents should never be replaced under the same version.

Registry identity is part of package identity. Internally, a registry package is identified by `(registry root identity, extension ID)`, and lockfiles always retain both. The same extension ID from another registry is a different package, not an update. Changing source requires an explicit source-change or uninstall/reinstall flow and renewed approval. Visible command and tool namespaces remain global, so conflicting enabled contributions are rejected rather than resolved by registry priority. The `tokio.official.*` namespace is reserved by the host and cannot be enabled from third-party registries.

## Installation, enablement, and loading

These states should be distinct:

- **Installed:** The package is present in the local package store.
- **Enabled:** User or project configuration allows its contributions.
- **Loaded:** Its runtime is currently instantiated. This is an internal lazy state, not something users need to manage.

Installing should not automatically enable an extension.

Suggested locations:

```text
User configuration and trusted registries:
  ~/.config/tokio-agent/extensions.toml

Project configuration and registry references:
  <project>/.tokio-agent/extensions.toml

Downloaded package cache:
  ~/.local/share/tokio-agent/extensions/<id>/<version>/

Project lock:
  <project>/.tokio-agent/extensions.lock
```

Platform-specific XDG-equivalent paths should be selected through `dirs`.

A project setting should override the user setting, including explicitly disabling a globally enabled extension.

The lock file should contain:

- Exact version.
- Source registry.
- Package digest.
- Host API version.
- Granted capabilities.
Package dependencies are not supported in the initial format; manifests containing dependency declarations must be rejected.

It should be suitable for committing when a project depends on specific extensions.

### Live enable and disable behavior

Commands and status segments can update immediately.

Tool changes should take effect at the next model-request boundary. If an extension tool is currently running, disabling the extension should ask whether to cancel the operation or wait for completion.

Disabling a service extension should:

1. Stop its timers.
2. Reject stale actions from previous invocations.
3. Cancel or pause owned autonomous work according to the extension contract.
4. Remove its dynamic tools before the next request.
5. Remove its commands and status segments.
6. Shut down its runtime instance.

## TUI user experience

Add `/extensions` as a built-in management command.

### Main screen

```text
Extensions

 Search: goal_

 Discover  Installed  Updates

 > Goal                         tokio.official.goal
   Continue until verified complete
   Official · v1.2.0 · Not installed

   Loop                         tokio.official.loop
   Run prompts on an interval
   Official · v1.0.3 · Installed · Enabled for this project

 ↑↓ select   Enter details   i install   e enable/disable   Esc close
```

Search should merge the official registry with explicitly configured third-party registries, use cached indexes immediately, and refresh them in the background. Every result must display its source registry and trust classification. Losing network connectivity must not affect installed extensions.

The Installed tab should work completely offline.

### Extension detail screen

```text
Goal  v1.2.0                         Official · tokio-agent registry

Continue working until an objective is verified complete.

Contributes:
  /goal
  update_goal model tool, only while a goal is active
  goal status segment

Permissions:
  Observe turn lifecycle
  Start automatic model turns
  Register a model-visible tool
  Store session state

Context cost:
  No cost while idle
  96-token tool schema while a goal is active

[i] Install
```

After installation:

```text
Installed. Enable for:

> This project
  All projects
  Not now
```

Disabling a service extension with active work should ask for confirmation:

```text
Goal currently has active work.
Disabling it will pause and unload the goal.

Disable anyway?  [Disable] [Cancel]
```

### Slash autocomplete

Autocomplete should merge built-in and extension commands and show their source:

```text
/goal       Continue until complete             Goal extension
/loop       Run on an interval                   Loop extension
/model      Switch model                         Built in
```

If a user types an installed but disabled command, provide a useful local response:

```text
/goal is provided by “Goal”, which is installed but disabled.
Enable for this project? [Enable] [Cancel]
```

Unknown slash commands should not be sent to the model by default. Doing so turns typos into expensive prompts and makes disabled extensions confusing. The UI should instead report that the command is unknown and offer registry search when appropriate.

### Update experience

Updates should appear in an Updates tab. Each entry should distinguish:

- Patch/minor update with unchanged permissions.
- Update requiring new capabilities.
- Update requiring a newer host version.
- Yanked or deprecated installed version.

New capabilities require explicit approval before updating.

### Error experience

Extension failures should not terminate the agent session. The user should receive a concise notice such as:

```text
Goal extension stopped: execution exceeded its time limit.

[Restart] [Disable] [View details]
```

Detailed diagnostics belong in debug logs and the extension detail screen, not in the normal transcript unless they affect the user's active task.

## CLI parity and local development

Registry management should also work without the TUI:

```text
tokio-agent extension list
tokio-agent extension search goal
tokio-agent extension info tokio.official.goal
tokio-agent extension install tokio.official.goal
tokio-agent extension enable tokio.official.goal --project
tokio-agent extension disable tokio.official.goal --project
tokio-agent extension update
tokio-agent extension remove tokio.official.goal

tokio-agent extension new my-extension
tokio-agent extension check ./my-extension
tokio-agent extension link ./my-extension
tokio-agent extension dev ./my-extension
```

`link` and `dev` should support local authoring without publishing.

Suggested authoring flow:

```text
$ tokio-agent extension new deploy-helper
Created deploy-helper/
  extension.toml
  commands/deploy.md
  README.md

$ tokio-agent extension check ./deploy-helper
✓ manifest is valid
✓ /deploy has no command conflict
✓ no executable capabilities requested

$ tokio-agent extension link ./deploy-helper
Linked and enabled for this project.
```

For most personal commands, the user should not need these commands at all; adding a Markdown file to `.tokio-agent/commands` should be sufficient.

## Registry design

The tokio-agent project operates one official registry rather than a marketplace. Its index, package source, review policy, and static-registry generation tooling live in this GitHub repository. The official registry contains only extensions published and maintained by the tokio-agent project, such as Goal and Loop. External publishers do not submit arbitrary packages to the official registry.

Users may add third-party registries. Each registry is independently operated and signed, and is untrusted until the user explicitly adds its URL and trusts its root signing identity. Adding a registry is not equivalent to installing or enabling any extension from it.

When a registry is added, the client downloads its TUF root metadata and displays the registry name, origin URL, claimed operator, HTTPS status, and root-key fingerprint. The user must explicitly confirm that fingerprint before the root enters the user trust store. A project may reference a registry URL and expected fingerprint, but opening a project must never trust it automatically: interactive mode asks for approval and headless mode fails with instructions for explicit trust.

Removing a registry stops refresh, installation, and updates from it but does not delete already installed packages. Those packages remain identified as originating from an unavailable registry and must never be silently replaced by same-named packages from another source.

The UI must clearly distinguish:

- **Official:** Published by the tokio-agent project through the built-in official registry.
- **Third-party registry:** Published by an external registry, with the registry name, origin, and publisher identity displayed.
- **Local:** A loose command or linked development package that did not come from a registry.

No package can claim official status through its manifest. Official status derives only from the built-in official registry identity. Search results, details, permission prompts, installed lists, and update notices must preserve this classification.

Every registry must provide a signed, cacheable metadata index containing:

- Package ID and immutable version.
- Description, keywords, README URL, and license.
- Host API compatibility.
- Contributions and requested capabilities.
- Package digest.
- Registry identity, publisher identity, and verification status.
- Yanking and deprecation information.
- Minimum supported application version.

Installation flow:

1. Identify the source registry and verify that its trusted root matches the configured registry identity.
2. Resolve a compatible immutable version.
3. Download it to a temporary location.
4. Verify signed metadata and package digest.
5. Validate the manifest and WebAssembly imports.
6. Show the registry, publisher, and capability review.
7. Atomically move the package into the package store.
8. Update lock and configuration only after success.

### Official package publication

Official extension publication uses this repository and its CI rather than accepting prebuilt contributor binaries:

1. Official extension source, manifest, documentation, license, and tests are committed under the repository's registry source tree.
2. GitHub Actions validates manifests, capabilities, command and tool collisions, host API compatibility, and package paths.
3. CI builds WebAssembly components from source, rejects undeclared imports, and runs conformance and resource-limit tests.
4. CI creates an immutable package archive and publishes it as a GitHub Release asset rather than adding generated binaries to ordinary Git history.
5. The generated GitHub Pages registry references the immutable asset URL and SHA-256 digest.
6. TUF metadata signs the package identity, version, URL, digest, publisher, capabilities, and compatibility information.
7. Production signing keys are managed outside the repository. Tests and local fixtures use clearly marked non-production keys.

Third-party registry operators may choose their own build pipeline, but clients apply exactly the same TUF, immutable-version, package-digest, manifest, import, compatibility, and capability checks. A valid third-party signature authenticates that registry's publication; it does not confer official status.

HTTPS plus a checksum served by the same registry is not enough protection against registry compromise. Signed metadata must protect package integrity, version rollback, malicious index replacement, and root-key rotation. Registries use TUF-style metadata. The built-in official root is distributed with tokio-agent; third-party roots are explicitly accepted by fingerprint when a registry is added. Repository review alone does not protect clients after a registry or hosting-account compromise.

Do not auto-update enabled extensions in the background. Show updates and let the user review capability changes. A new capability requires fresh approval.

### Registry policy

The official registry initially publishes only tokio-agent-maintained:

- Declarative command and skill packages.
- Sandboxed WebAssembly components using supported host APIs.

Third-party registries use the same package format, validation rules, signing protocol, and host capability restrictions. Operating a separate registry does not grant additional runtime authority.

It should initially reject:

- Native dynamic libraries.
- Packages that download and execute code after installation.
- Undeclared network or process access.
- Mutable package URLs without immutable digests.
- Licenses incompatible with project distribution policy.

Official packages such as Goal and Loop are identified by the built-in official registry rather than a self-asserted manifest flag, but they still use the same manifest, package, permissions, and runtime APIs available to third-party authors and registries. This prevents the public extension system from becoming a facade over hidden special cases. After migration, Goal and Loop are discoverable in `/extensions` but are neither installed nor enabled by default.

## Security and capability model

Extensions are untrusted input, including extensions from an official registry.

### Default-deny runtime

A WebAssembly component should receive no ambient:

- Filesystem access.
- Network access.
- Environment variables.
- Process execution.
- Credentials.
- Wall clock.
- Randomness.
- Transcript access.

It receives only declared host interfaces. Host operations can then apply scope checks and the existing permission engine.

### Capability review

Capabilities should be presented in user language rather than only technical identifiers:

```text
This extension can:
  • Add the /deploy command
  • Read files in the current project when you approve
  • Run kubectl commands when you approve
  • Connect to api.example.com

It cannot:
  • Read files outside the project
  • Access provider API keys
  • Start autonomous model turns
```

Capabilities should distinguish between authority granted at enable time and actions that still require per-use approval.

For example, `process.request` permits an extension to ask the host to run a process. It should not necessarily bypass the normal Execute permission prompt.

### Resource limits

The host should enforce:

- Maximum component memory.
- Fuel/instruction limits per callback.
- Wall-time deadlines.
- Maximum returned payload size.
- Maximum status update rate.
- Maximum timer count and minimum interval.
- Maximum pending actions.
- Maximum log volume.
- Cancellation on disable and shutdown.

Repeated failures should trip a circuit breaker and disable the runtime for the current session while preserving its installed/enabled configuration for user review.

### Secrets

Extensions should never receive provider API keys.

If a future extension needs a secret, it should request a named extension-owned secret from a host secret store. The value should only be available to that extension ID and should not be written into manifests, lock files, logs, or model context.

### Package updates

A package update must not silently:

- Add capabilities.
- Change publisher identity.
- Change package contents under an existing version.
- Change command aliases in a way that creates collisions.
- Increase context contribution without showing it in details.

## Migrating `/loop` and `/goal`

These should become the first official service extensions because they exercise the important APIs.

### Migrate `/loop` first

`/loop` needs:

- A slash command.
- Session-scoped state.
- A monotonic timer.
- Automatic prompt submission.
- Interrupt and cancel events.
- A status segment.

It does not require a model tool, so it is the simpler proof that scheduling and automatic turns work outside `Agent`.

Goal and Loop remained built in until the companion runtime, registry client, extension manager, dynamic tools, and service safety policies were in place. Their feature-specific core implementations were then removed together; the official component packages are now the only implementations.

Once migrated:

- Remove `SetLoop` from `UiCommand`.
- Remove `LoopSchedule` and timer selection from `Agent`.
- Let the extension schedule through the supervisor.
- Preserve fixed-delay behavior.
- Do not accumulate missed-tick backlog.
- Do not overlap foreground turns.
- Cancel pending work cleanly on interrupt or disable.

The Loop manifest would require approximately:

```toml
[capabilities]
session_observe = true
session_submit_automatic = true
session_schedule = true
status_write = true
storage_session = true
```

### Migrate `/goal` second

`/goal` needs:

- Command handling and argument parsing.
- Session state.
- Turn lifecycle events and usage.
- Automatic continuations.
- A dynamically visible `update_goal` tool.
- Interrupt-to-pause behavior.
- A status segment.

The hidden-tool behavior is preserved: `update_goal` only enters model context while a goal is active. When it marks the goal complete or blocked, the extension unregisters the tool and suppresses further continuation.

The initial autonomy-arbitration policy permits only one autonomous extension to own a session at a time. Activating another autonomous service must visibly ask the user to pause or disable the current owner; autonomous services must never silently compete or rely on priority ordering.

The host, not the extension, should enforce:

- Maximum automatic turns.
- Only one queued automatic continuation.
- User messages take priority.
- No automatic submissions after interrupt.
- No submissions while another extension owns an incompatible autonomous activity.
- A global emergency stop for autonomous extensions.

The Goal extension should own goal policy and state, while the host owns resource and queue safety.

### Future subagents

Subagents can later use the same session-service interface plus an explicit `subagents.spawn` capability.

A subagent extension should submit a typed spawn request containing:

- Task.
- Tool allowlist.
- Optional model/profile selection.
- Budget and concurrency request.
- Result contract.

The host should create and supervise the actual agent loop. An extension should not receive the internal `Agent`, provider, transcript, or permission engine objects.

This keeps the implementation provider-neutral and lets the host enforce concurrency, budget, cancellation, and permission policy.

## Implementation sequence

### Phase 1: Command and contribution foundation

- Extract slash command descriptors from the hard-coded TUI array.
- Replace special TUI parsing with command invocation by stable ID.
- Add built-in and extension command catalogs.
- Implement loose Markdown prompt commands.
- Add command collision and validation rules.
- Keep `/goal` and `/loop` internally implemented temporarily.
- Add targeted tests for discovery, autocomplete, templating, and project/user precedence.

This delivers immediate value with low risk.

### Phase 2: Extension manager and local packages

- Implement manifest parsing and discovery in `tokio-agent-plugin`.
- Add user/project enablement and package state.
- Add `/extensions` with an Installed view.
- Add CLI management and local `link`.
- Add structured status segments.
- Support hot reload for declarative development.
- Add package compatibility and capability validation.

### Phase 3: Programmable runtime

- Prototype and select the WebAssembly runtime.
- Define versioned WIT interfaces.
- Add fuel, memory, time, output, and invocation limits.
- Add capability-gated host calls.
- Integrate extension operations with the permission engine.
- Add dynamic tool availability.
- Add crash and timeout isolation tests.

### Phase 4: Session services

- Introduce the session supervisor and event/action protocol.
- Add deterministic automatic-action prioritization.
- Migrate `/loop`.
- Migrate `/goal`.
- Publish both as local official packages for dogfooding.
- Remove their feature-specific command variants and state from core.

### Phase 5: Official registry

- Implement signed metadata and immutable packages.
- Add search and discovery UI.
- Add install, update, remove, permission review, and compatibility checks.
- Support offline index/cache behavior.
- Add official-registry identity, third-party registry management, publisher verification, and yanking.
- Add lockfile support. Project-scoped registry packages update `.tokio-agent/extensions.lock` only through explicit install, enable, disable, remove, or update operations. Loose Markdown commands are not locked; linked development extensions are recorded as local and non-reproducible; locked registry versions never advance without an explicit update.
- Publish Goal and Loop without installing or enabling them by default.

### Phase 6: Ecosystem compatibility and subagents

- Import Claude and Codex commands and skills.
- Import MCP configurations only as part of future Claude Code and Codex-style plugin compatibility, outside the native tokio-agent extension contribution model.
- Publish extension SDK and templates.
- Add subagent service APIs.
- Add optional hooks only after event semantics are stable.

## Testing strategy

### Manifest and package tests

- Reject malformed or unknown manifest fields.
- Reject invalid IDs and versions.
- Reject path traversal and symlink escapes.
- Reject undeclared component imports.
- Detect command and tool collisions.
- Verify host API compatibility.
- Verify package digest and signed metadata.
- Preserve the previous installation after a failed update.

### Command tests

- Discover user and project Markdown commands.
- Apply project/user precedence deterministically.
- Expand arguments without code execution.
- Keep disabled commands out of autocomplete.
- Explain installed-but-disabled commands.
- Reject unknown commands locally.
- Make command catalogs work in both TUI and headless frontends.

### Runtime tests

- Stop an infinite component with fuel limits.
- Stop an expired callback with a deadline.
- Reject undeclared filesystem, network, and process calls.
- Cancel callbacks on disable and shutdown.
- Prevent stale actions after disable.
- Isolate a component trap from the agent session.
- Enforce output and status update limits.

### Tool and context tests

- Disabled tools never enter request schemas.
- Enabled tools enter schemas only at request boundaries.
- Dynamically active tools disappear after deactivation.
- Extension identity is included in permission prompts.
- Tool results retain existing transcript validity guarantees.
- Status-only and command-only extensions add no model-context cost.

### Service tests

- User input wins over automatic extension work.
- Only one automatic continuation is queued.
- Interrupt cancels or pauses extension-owned autonomy.
- Timers do not overlap turns or build a backlog.
- Disabling a service cancels timers and removes tools/status.
- A failing service cannot stop the base agent session.
- Global autonomy limits cannot be bypassed by an extension.

### Registry tests

- Search works from cached metadata offline.
- Install is atomic.
- Rollback and expired metadata are rejected.
- Yanked packages remain identifiable but are not newly selected.
- Capability changes require renewed approval.
- Incompatible updates are not installed.
- Removing one version does not break another project's lock.

## Important design rules

The following should be treated as non-negotiable:

- No native libraries loaded into the main process.
- No direct Ratatui widget access.
- No direct mutable access to `Agent`, transcript, or `ContextAssembler`.
- No arbitrary environment variables or secrets exposed by default.
- No model-context contribution merely because a package is installed.
- No automatic enablement after installation.
- No silent command shadowing.
- No extension execution during TUI rendering.
- No capability expansion during update without renewed approval.
- No extension-owned unbounded turn queue or timer backlog.
- Disabled extensions must have zero model-context cost and effectively zero runtime cost.
- First-party extensions must use the same public package and capability system as third-party extensions.

## Resolved implementation decisions

The following decisions are part of the implementation contract:

1. **WebAssembly runtime:** Use Wasmtime and the WebAssembly Component Model.
2. **Runtime placement:** Run Wasmtime only in a separate `tokio-agent-extension-host` companion process, not in the main application process.
3. **Registry ownership:** Keep the official registry and its generation tooling in this GitHub repository and restrict it to tokio-agent-maintained packages. Support explicitly added, independently signed third-party registries instead of accepting arbitrary community packages into the official registry. The UI derives official status only from the built-in registry identity and labels all other sources prominently.
4. **Registry implementation scope:** Build the multi-registry client, TUF verifier, static-registry generator, official GitHub Pages publication workflow, and local integration fixtures in this repository. The official registry is a static repository workflow rather than a separately deployed marketplace service. Third-party operators can use the same generator and protocol.
5. **Goal and Loop migration:** Keep the existing built-in implementations until the replacement system is complete, then migrate both to ordinary official extension packages. After migration they are discoverable but not installed or enabled by default.
6. **Project lock behavior:** Create or update a project lock only through explicit project-scoped extension operations. Do not lock loose Markdown commands. Record linked development extensions as local and non-reproducible. Never advance a locked registry version implicitly.
7. **Autonomy arbitration:** Permit one autonomous extension owner per session. Starting another requires visible user resolution of the conflict.
8. **Extension dependencies:** Do not support package-to-package dependencies initially. Unknown dependency fields are rejected.
9. **MCP packaging:** Native tokio-agent extensions cannot contribute MCP configuration or servers. MCP import may be added later only through a separate Claude Code or Codex compatibility layer.
10. **Extension manager sequence:** Implement a complete Installed view first. Add Discover, Updates, and explicit third-party registry management when signed static registries are available.
11. **Capability approval storage:** Bind grants to source registry identity, extension ID, publisher identity, and the exact capability set. Record all four in project locks. Registry, publisher, or capability changes require renewed approval.
12. **Initial resource limits:** Start with 64 MiB component memory, 10 million fuel units per callback, a two-second callback deadline, a 256 KiB returned-payload limit, 160 characters per status segment, ten coalesced status updates per second, 32 timers per extension, a 100 ms minimum timer interval, 128 pending actions, and a circuit breaker after three consecutive failures. Limits must be configurable by the host without becoming extension-controlled.
13. **Companion lifecycle:** Distribute the companion beside the main executable, locate a sibling before searching `PATH`, launch lazily, use a versioned handshake, isolate extensions in separate stores, preserve declarative behavior when unavailable, and allow at most one automatic restart per session.
14. **TUI boundaries:** Limit extensions to commands, structured status, notices, and host-rendered management metadata initially. Do not expose Ratatui, raw terminal rendering, theme replacement, arbitrary screens, or application-wide layout control.
15. **Third-party registry trust:** Adding a registry requires explicit confirmation of its TUF root fingerprint in the user trust store. Projects may reference a URL and expected fingerprint but cannot silently establish trust; headless use fails until trust is established explicitly. Removing a registry preserves installed files but prevents refresh, installation, and updates from that source.
16. **Cross-registry identity:** Treat `(registry root identity, extension ID)` as package identity and retain both in locks. A same-ID package from another registry is not an update. Source changes and changed registry identity require explicit action and renewed approval. Reject globally visible command or tool collisions, and reserve `tokio.official.*` to the built-in registry.
17. **Official artifacts:** Build official Wasm packages in GitHub Actions from repository source, publish immutable archives as GitHub Release assets, serve generated signed metadata through GitHub Pages, and keep production private keys outside the repository.

### Remaining implementation details

No product or architecture decision is currently blocking implementation. Ordinary internal representations may be selected where behavior is constrained by this document. Production launch still requires an operational TUF root-key ceremony and GitHub publication credentials, but implementation and fixture-based integration testing do not depend on those external secrets.

## Release operations

Before the first production registry release, perform the root-key ceremony, embed the resulting public root in release builds, configure the registry signing secret and GitHub Pages environment, and run the signed publication workflow. These are operational trust and deployment steps rather than missing implementation.
