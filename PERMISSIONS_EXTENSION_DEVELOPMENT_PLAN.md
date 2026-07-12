# Permissions as an Optional Official Extension

## Status

Development-ready architecture and implementation plan. Product decisions are locked; there are no remaining design choices required before Phase 1.

## Objective

Move `/permissions`, tool approval policy, approval prompts, mode persistence, and permission-related CLI behavior out of the default product and into an official registry extension, `tokio.permissions`.

The required product behavior is:

- `tokio.permissions` is published in the official registry but is **not installed by default**, like `tokio.loop` and `tokio.goal`.
- If no tool-gate extension is installed, every valid tool call runs immediately. There is no permission mode, permission event, approval command, prompt, or permission UI in the active session.
- Installing `tokio.permissions` adds the `suggest`, `auto-edit`, and `full-auto` policies.
- The extension owns `/permissions`, the mode picker, approval prompt content and actions, operation-scoped session approvals, persistence, and any permission-specific startup options.
- The host supplies only generic extension mechanisms: a pre-tool gate, user interactions, extension state/settings, and extension-declared CLI options.
- An installed gate that crashes must fail closed. “Extension absent” and “installed extension unavailable” must not be treated as the same state.
- When the extension is absent, the host shows no permission-specific warning, status, or installation suggestion; the user installs it only if wanted.
- Installing, enabling, disabling, removing, or reloading the extension updates the running session immediately at the tool-admission boundary.

This is an intentional security-default change. It should ship with prominent release notes: upgrading without installing `tokio.permissions` changes the agent from `suggest` to unconditional execution.

## Why this is not a file move

The current extension API cannot implement the feature end to end:

- `ToolCallExecutor` directly depends on `PermissionEngine` and emits `AgentEvent::PermissionNeeded`.
- `Agent`, `UiCommand`, and the TUI use core-specific `Mode`, `Decision`, `PermissionId`, and `PermissionRequest` types.
- `/permissions` is always registered as a built-in command in `crates/plugin/src/commands.rs`.
- The mode picker and tool approval panel are hard-coded in `crates/tui/src/projection.rs`.
- `permission_mode` is a first-class config field in `crates/config/src/lib.rs`.
- `--yolo` is statically defined by Clap in `crates/cli/src/main.rs`.
- Wasm extensions can return notices, status, tools, timers, and prompts, but cannot pause a tool invocation for user input or open a UI surface.
- `storage_user` exists as a declared capability but has no user-state request/action implementation.

The host therefore needs generic APIs that are also useful to future policy, confirmation, and workflow extensions. It must not replace `PermissionEngine` with another host type that merely has a less permission-specific name while retaining the policy in core.

## End-state architecture

### 1. Optional, singleton pre-tool gate

Add a generic extension contribution for intercepting tool calls before execution:

```toml
[tool_gate]
handler = "authorize_tool"

[capabilities]
tool_gate = true
interaction_request = true
storage_user = true
```

Only one tool gate may be active in a session. Reject a second active contribution with a clear installation/startup error. For the first release, `tool_gate` should be a privileged capability allowed only for an official-registry package in the reserved `tokio.*` namespace. This prevents an ordinary extension from silently becoming a global execution authority.

Core should expose an optional, frontend-neutral interface along these lines:

```rust
trait ToolGate: Send + Sync {
    fn authorize<'a>(
        &'a self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, ToolGateResult>;
}

enum ToolGateResult {
    Allow,
    Deny { reason: String },
}
```

`ToolCallExecutor` should hold `Option<Arc<dyn ToolGate>>`:

- `None`: call the tool directly; do not construct an approval request or emit an interaction.
- `Some(gate)`: await `authorize` before calling the tool.
- `Deny`: return a normal denied tool result to the model.
- Cancellation: cancel the gate request and do not run the tool.

`ToolInvocation` should be a generic, serializable description, not a permission policy object:

```rust
struct ToolInvocation {
    invocation_id: String,
    tool_name: String,
    owner: ToolOwner,          // built-in or extension ID/version
    arguments: serde_json::Value,
    effect: ToolEffect,        // read, edit, execute, unknown
    cwd: PathBuf,
    summary_hint: Option<String>,
}
```

The extension makes all policy decisions. The host only supplies facts. Preserve useful summaries currently produced by built-in tools as `summary_hint`, but remove `Tool::permission()`, `PermissionRequest`, and permission evaluation from the normal execution API. Replace extension `ToolPermission` with `ToolEffect` as part of the host API 2.0 break. Keep the initial effect model behaviorally equivalent to today's `Action`: `read`, `edit`, `execute`, plus a conservative `unknown` default. In particular, web search remains a read effect rather than introducing new network-policy behavior in this migration.

The gate adapter belongs at the CLI/plugin integration boundary, not in `tokio-agent-core`: it forwards `ToolInvocation` to the owning extension through the supervisor and implements the generic core trait.

### 2. Two-phase gate callback; never block a Wasm callback on UI

The extension host currently invokes callbacks with short deadlines. A callback must not stay open while a human considers a prompt.

Use this lifecycle:

1. Executor calls the gate adapter with a unique invocation ID.
2. Host invokes the extension’s `authorize_tool` handler.
3. The extension immediately returns one of:
   - `allow`
   - `deny { reason }`
   - `request_interaction { interaction }`
4. For an interaction, the host emits a generic interaction event and waits outside the Wasm callback.
5. The frontend returns a generic interaction response.
6. Host invokes `on_interaction_response` with the invocation ID and selected action.
7. The extension immediately returns final `allow` or `deny`, plus optional state-persistence actions.
8. Executor continues or returns a denied result.

IDs must be opaque and generation-scoped. Reject stale, duplicate, wrong-owner, and already-cancelled responses. On interrupt, shutdown, extension reload/removal, or extension failure, resolve all pending requests as denied and emit a generic `InteractionCancelled` event so every frontend closes the surface.

### 3. Generic interaction surfaces, with the definition bundled by the extension

Do not allow Wasm to execute Ratatui code or draw arbitrary terminal cells. That would break sandboxing, accessibility, headless support, and frontend independence.

Instead, add extension-owned declarative interactions that the host renders with generic widgets:

```rust
enum InteractionSpec {
    Approval(ApprovalSpec),
    SingleSelect(SingleSelectSpec),
}

struct ApprovalSpec {
    title: String,
    body: Vec<TextSection>,
    actions: Vec<InteractionAction>,
    copy_text: Option<String>,
}

struct SingleSelectSpec {
    title: String,
    options: Vec<SelectOption>,
    selected: Option<String>,
}

struct InteractionAction {
    id: String,
    label: String,
    key_hint: Option<String>,
    tone: InteractionTone,
}
```

The extension bundles the title, explanatory text, options, labels, action IDs, preferred keys, and response behavior. The TUI only owns a generic renderer, focus/navigation, terminal-safe wrapping, escape/cancel behavior, and key-conflict resolution. This satisfies “the menu is bundled with the extension” without granting an extension direct terminal access.

The same mechanism supports both permission interfaces:

- `/permissions` returns an action to open a `SingleSelect` surface. Selecting an option is delivered back to `tokio.permissions`, which updates policy and state.
- A gated tool returns an `Approval` surface with actions such as `allow_once`, `allow_session`, and `deny`. The extension interprets those IDs.

Replace permission-specific core messages with generic equivalents:

- `AgentEvent::PermissionNeeded` -> `AgentEvent::InteractionRequested`
- `UiCommand::Approve` -> `UiCommand::RespondToInteraction`
- `SessionCommand::Approve` / `ApprovalDecision` -> generic response DTOs
- add `InteractionCancelled` for owner removal, generation changes, interruption, and shutdown
- permission panel fields in `Projection` -> an interaction stack or one active modal

Keep one active blocking interaction initially. Queue concurrent tool approvals in deterministic tool-call order rather than rendering competing prompts.

### 4. Extension-owned policy

Implement the current behavior in `registry/extensions/permissions`:

- `suggest`: `read` effects are allowed; `edit`, `execute`, and `unknown` ask.
- `auto-edit`: `read` and `edit` effects are allowed; `execute` and `unknown` ask.
- `full-auto`: all effects are allowed.
- `allow_once`: allows only the pending invocation.
- `allow_session`: records a narrow operation scope in extension session state and allows only subsequent invocations matching that scope.
- `deny`: denies the invocation.

Do not implement session approval as a tool-wide allow-list. A Bash approval must never approve every future Bash command. Compute a deterministic scope key from stable owner identity, tool name, working directory, and operation-specific arguments:

- `bash`: exact command string and working directory; ignore scheduling-only fields such as yield and timeout. A changed command, added pipe, redirect, argument, environment assignment, or working directory requires a new approval.
- `write`, `edit`, and `multi_edit`: normalized target path set and operation family. Approval for one file does not approve edits to another file.
- built-in operations with another meaningful target: use that normalized target rather than the whole tool.
- extension-owned or unknown tools: canonicalized complete arguments by default. A future trusted scope-hint API may narrow irrelevant fields, but must never broaden the scope without an explicit protocol contract.

Store only a hash of the canonical scope material in session state so commands and arguments are not retained unnecessarily. Scope comparison must use stable owner identity (`built-in` or extension ID/version), never display name. Path normalization must be lexical and relative to the invocation working directory; it must not perform extra filesystem access merely to ask for approval. Treat `unknown` effects conservatively in `suggest` and `auto-edit`.

The existing implementation calls this choice `AllowAlways`, but it lasts only for the in-memory engine. Rename the UI action to `allow_session` so its actual scope is clear. Persistent approvals can be a later extension feature and should not be implied now.

The extension should create its own summaries from the invocation and use `summary_hint` where available. It must bound displayed argument text and avoid placing secrets or entire files in prompt text or persisted state.

### 5. Failure semantics

Security behavior must be explicit:

| State | Behavior |
|---|---|
| No tool-gate contribution installed | Run tools immediately (full-auto behavior) |
| Gate installed and returns allow | Run tool |
| Gate installed and returns deny | Return denied tool result |
| Gate installed but crashes, times out, or fails protocol validation | Fail closed for the remainder of the session; deny pending and future gated calls and show an error |
| User explicitly disables or removes the gate | Deny pending approval requests, detach the gate immediately, and run subsequent tool calls ungated |
| User explicitly installs/enables a valid gate | Attach it immediately; subsequent tool calls are gated |

Do not silently detach a failed gate and continue ungated. Represent runtime gate state explicitly as `Absent`, `Active`, or `Failed` so failure cannot be confused with absence.

Hot changes are atomic at the tool-admission boundary. Calls already admitted before a successful attach continue unaffected; calls admitted afterward use the gate. On explicit disable/removal, pending approvals are denied rather than replayed ungated, while future calls use the direct path. On install/enable, fully load and validate the extension before switching from `Absent` to `Active`; if loading fails, remain `Absent`. A hot reload of an active gate denies pending requests, validates the new generation, and atomically replaces it; reload failure enters `Failed` rather than detaching protection. Installing a second eligible gate is rejected without disturbing the active gate.

### 6. Extension settings and persistence

Remove permission mode from base configuration:

- `DEFAULT_PERMISSION_MODE`
- `PermissionMode`
- `Config.permission_mode`
- `ResolvedConfig.permission_mode`
- `Layer.permission_mode`
- `store_permission_mode`
- related validation/errors/tests

Add a generic extension settings namespace instead of adding another permission-specific host field:

```toml
[extensions."tokio.permissions"]
mode = "suggest"
```

The extension's first-install default is `suggest`. New-session precedence is: extension default < globally persisted user selection < project extension config < runtime/CLI. `/permissions` always changes the active session immediately and updates the global user selection; a project or CLI override is not rewritten, so it applies again when the next session starts. The host passes the merged JSON/TOML object to the extension at load. The extension validates its own keys and reports an actionable startup error.

Complete the existing `storage_user` capability with bounded, atomic, per-extension state APIs. Use it for the mode selected by `/permissions` so the selection survives restarts. Operation-scope hashes must use session state only and must not leak across projects or sessions.

A clean split is:

- project/user config: administrator/user-declared extension defaults;
- user extension state: last mode selected in the UI;
- session state: `allow_session` entries;
- CLI option: current-process override, never persisted.

#### Breaking configuration removal

Remove `permission_mode` from every base configuration layer in the same release. Do not add a compatibility parser, automatic migration, warning-only fallback, or config rewrite. Because config parsing uses `deny_unknown_fields`, an existing config that still contains `permission_mode` will fail validation until the user removes that key.

Installing `tokio.permissions` starts with the extension's own default mode unless the user explicitly configures `[extensions."tokio.permissions"]` or supplies its session CLI option. Never infer extension state from the removed base setting and never auto-install the extension.

### 7. CLI behavior owned by installed extensions

Remove the host’s static `--yolo` flag. Without a gate it is redundant, and with a gate it improperly bypasses extension policy from outside the extension.

Add generic manifest-declared startup options, for example:

```toml
[[cli_options]]
long = "permission-mode"
value_name = "MODE"
values = ["suggest", "auto-edit", "full-auto"]
description = "Set the tool approval mode for this session"
handler = "permission_mode_option"
```

The CLI should use two-phase parsing for normal agent startup:

1. Parse only bootstrap fields needed to identify the command and working directory.
2. Load manifests for enabled extensions without executing Wasm.
3. Validate and add declared options to a dynamic Clap `Command`.
4. Parse the complete arguments.
5. Deliver parsed values to the extension during `Load` as ephemeral startup settings.

Constraints:

- Only installed/enabled extensions contribute options; without `tokio.permissions`, `--permission-mode` is unknown.
- Extension options cannot shadow built-ins or each other.
- Names are globally validated and a conflict rejects the contributing extension option with a clear error; no alternate permission-specific fallback name is built into the host.
- Manifest parsing, `--help`, and shell completion do not execute extension code.
- Extension management commands should remain parseable even if an installed manifest is broken.
- Secret-valued options must be rejected or explicitly marked and redacted.

The replacement is `--permission-mode full-auto`, not `--yolo`. Remove `--yolo` outright; do not provide a compatibility alias.

For non-interactive runs, an extension mode that requires approval cannot display a TUI prompt. Include frontend capabilities in the gate context. `tokio.permissions` should deny with an actionable message directing the user to `--permission-mode full-auto` (or another non-prompting mode). It must never hang waiting for input. The current permission-specific branch in `crates/cli/src/headless.rs` is then removed; headless handles a generic denied result or unsupported interaction.

### 8. Registry package

Create `registry/extensions/permissions` following the `loop` and `goal` package layout:

- `Cargo.toml` and lockfile
- `extension.toml`
- `src/lib.rs`
- `README.md`
- `LICENSE`
- generated component at publication time

Suggested identity and contributions:

```toml
manifest_version = 1
id = "tokio.permissions"
name = "Permissions"
description = "Ask before selected tool actions and manage approval policy"
host_api = ">=1.0, <2.0"

[runtime]
component = "component/extension.wasm"

[[commands]]
name = "permissions"
description = "Select how the agent asks before tool actions"
handler = "permissions_command"
available_while_running = true

[tool_gate]
handler = "authorize_tool"

[capabilities]
tool_gate = true
interaction_request = true
storage_user = true
storage_session = true
```

The package must not be added to any default install list. Publish it through the same official registry signing/index workflow as `tokio.loop` and `tokio.goal`.

## Concrete code changes

### `crates/core`

- Add generic `ToolInvocation`, `ToolEffect`, `ToolOwner`, `ToolGate`, and `ToolGateResult` types.
- Make `ToolCallExecutor` accept an optional gate and bypass it completely when absent.
- Remove `permission.rs` in the same changeover release.
- Remove permission fields and methods from `Agent`.
- Replace permission-specific events and UI commands with generic interaction events/responses.
- Change `Tool::permission()` to generic invocation metadata or summary hooks; update all built-in tools.
- Ensure malformed arguments are rejected before gate interaction, as they are today.
- Preserve cancellation and ordered parallel-tool behavior.

### `crates/extension-api`

- Add tool-gate request/response DTOs and generic interaction DTOs.
- Add `InteractionId` and generation/owner information.
- Add generic session command/host response variants for interaction responses.
- Add capability values for `ToolGate` and `InteractionRequest`.
- Add user-state load/persist DTOs and startup-settings DTOs.
- Keep new protocol fields bounded and `deny_unknown_fields` where appropriate.
- Release this as host API 2.0: update `HOST_API_VERSION`, the WIT package/world contract, and the companion protocol version; remove the old approval DTOs and `ToolPermission` rather than carrying deprecated permission-specific wire types.
- Update and rebuild `tokio.loop` and `tokio.goal` against host API 2.0 in the same release, including their manifest requirements and generated components.

### `crates/extension-host`

- Extend WIT/JSON dispatch with `authorize-tool` and `on-interaction-response` exports.
- Add user-state restoration and startup settings to `Load`.
- Enforce callback deadlines independently for both short callbacks.
- Never keep a component call open while waiting for frontend input.

### `crates/plugin`

- Add and validate singleton `tool_gate` and `cli_options` manifest contributions.
- Restrict `tool_gate` to official-registry extensions in the reserved `tokio.*` namespace, while allowing the existing explicit local development override for testing.
- Extend `SessionSupervisor` with gate invocation and response methods.
- Implement per-extension user storage atomically with size limits.
- Remove `PERMISSIONS_COMMAND_ID`, its built-in catalog entry, `OpenPermissionsPicker`, and routing branches.
- Add lifecycle cleanup and atomic gate-slot updates for install, enable, disable, remove, reload, and crash.
- Refresh the command catalog and extension catalog in the same atomic hot-change transaction so `/permissions` appears and disappears with the gate.
- Reject a second gate contribution; only one may be active in a session.
- Surface the privileged capability clearly in install approval and extension info.

### `crates/cli`

- Start the extension runtime early enough to supply the optional gate to the agent/executor and keep a mutable `Absent`/`Active`/`Failed` gate slot for atomic hot changes.
- Implement the gate adapter between core and the supervisor.
- Remove permission-mode resolution and `apply_yolo_override` from session construction.
- Remove static `--yolo`; implement generic extension-declared option parsing.
- Remove permission-specific headless handling.
- Pass frontend capabilities (`interactive`, `copy`, supported interaction kinds) to extensions.
- Deliver generic interaction request, response, and cancellation events between Agent and frontend.
- Wire extension-manager changes to the running extension runtime so gate and command contributions attach/detach immediately; manifest-declared CLI options naturally become available only on the next process invocation because argument parsing has already completed.

### `crates/config`

- Add a generic per-extension configuration map with layered merge behavior.
- Remove permission-specific types, defaults, storage functions, errors, and parsing immediately.
- Keep `deny_unknown_fields`; configurations that retain `permission_mode` should fail with an unknown-field error.
- Add generic bounded user-state storage if this responsibility remains in config rather than plugin.

### `crates/tui`

- Remove `SlashAction::Permissions`, `PERMISSION_MODES`, `permission_mode`, `permissions_selected`, and all permission-specific key/render helpers.
- Add generic `Approval` and `SingleSelect` interaction projection/rendering.
- Let the interaction spec define labels/action IDs/preferred keys; retain host control of safe layout and navigation.
- Route responses using opaque interaction IDs.
- Remove config persistence from `UiCommand::SetPermissionMode`; persistence is initiated by extension actions.
- Show `/permissions` only through the extension command catalog, so it disappears immediately when the extension is absent.

### `crates/tools`

- Replace each `permission()` implementation with generic effect and summary metadata.
- Preserve argument-aware summaries for bash, file edits, and writes without embedding policy.
- Verify extension-owned tools carry owner and effect metadata into `ToolInvocation`.

### Tests and documentation

- Update README/config examples that mention `permission_mode` or `--yolo`.
- Add the extension README with installation, modes, headless use, uninstall consequences, and failure behavior.
- Update local extension-development documentation to include `permissions`.
- Update registry CI to build and validate the new component.
- Update `loop` and `goal` manifests/components for host API 2.0.

## Implementation sequence

### Phase 1: Generic host primitives

1. Define tool invocation/effect and interaction DTOs in `extension-api` and core.
2. Add generic interaction projection and frontend round-trip tests.
3. Implement user/session state and startup settings APIs.
4. Add manifest validation for `tool_gate` and `cli_options`.
5. Add supervisor callbacks, lifecycle cleanup, and atomic hot attach/detach.
6. Add the optional gate to `ToolCallExecutor`, initially adapting the existing engine in tests if needed.
7. Update the extension protocol to host API 2.0 and port `loop` and `goal`.

Exit criterion: a fixture extension can allow, deny, and request a generic approval for a built-in tool; no permission-specific TUI code is needed for the fixture.

### Phase 2: Build `tokio.permissions`

1. Port mode evaluation to the Wasm extension and implement narrowly scoped, hashed session approvals.
2. Implement `/permissions` using `SingleSelect`.
3. Implement tool prompts using `Approval`.
4. Implement extension-owned persistence and the session CLI override.
5. Add README and registry metadata.

Exit criterion: linking the local extension reproduces current `suggest`, `auto-edit`, `full-auto`, allow-once, deny, cancellation, TUI, and headless behavior, with the deliberate improvement that allow-session is operation-scoped rather than tool-wide.

### Phase 3: Remove built-in permissions

1. Switch session construction to `Option<ToolGate>` based on installed contributions.
2. Remove the built-in command and TUI panels.
3. Remove `PermissionEngine` and permission-specific agent protocol.
4. Remove static `--yolo` and base permission config with no compatibility parser or alias.
5. Update all affected tests and docs.

Exit criterion: a build with no `tokio.permissions` package installed contains no `/permissions` command, warning, or mode indicator; never emits an approval interaction; and executes read/edit/execute/unknown tool calls directly.

### Phase 4: Release hardening

1. Verify that configuration containing removed `permission_mode` fails clearly as an unknown field.
2. Test gate crash, timeout, malformed output, hot install/enable/disable/remove/reload, a conflicting second gate, interrupt, and shutdown.
3. Verify install approval calls out global tool-gating authority.
4. Publish the extension but do not add it to defaults.
5. Add release notes appropriate for the breaking configuration and security-default changes.

## Required verification matrix

### No extension installed

- `/permissions` is absent from completion and routing.
- `--permission-mode` and removed `--yolo` are unknown.
- Read, edit, execute, unknown, and extension-owned tools execute without an interaction.
- Interactive and headless sessions behave the same.
- No permission mode is loaded or persisted.
- No permission-specific warning, status segment, or installation suggestion is shown.

### Extension installed

- Each mode gates every `ToolEffect` as documented.
- Picker selection applies immediately and persists at the intended scope.
- Allow-once does not affect the next call.
- Allow-session matches a stable, narrow operation scope and resets next session.
- Allowing one Bash command does not allow a changed command or the same command in a different working directory.
- Allowing an edit target does not allow a different path.
- Deny returns a useful tool result to the model.
- Prompt text wraps safely and large summaries are bounded.
- Copy does not resolve the prompt.
- Escape/cancel, interrupt, shutdown, and dropped frontend deny the pending invocation.
- Headless prompting denies immediately with an actionable message.
- CLI override applies only to the current process.

### Failure and security

- Installed gate crash/timeout/malformed response fails closed.
- A gate cannot approve a different invocation ID or generation.
- A non-official extension cannot claim the privileged gate capability.
- Two gate contributions cannot become active.
- Reload does not orphan or accidentally approve pending calls.
- Explicit install/enable attaches the gate immediately without affecting already admitted calls, and `/permissions` appears in the live command catalog.
- Explicit disable/remove denies pending approvals, cancels their UI surfaces, removes `/permissions`, detaches immediately, and makes future calls ungated without a permission-specific warning.
- Unexpected gate failure remains `Failed` and cannot become an ungated `Absent` state.
- User-state writes are scoped by extension ID, atomic, size-limited, and not exposed to other extensions.
- Untrusted labels/content cannot inject terminal control sequences.

### Host API break and registry

- Host API 2.0 contains no deprecated permission-specific approval or tool metadata DTOs.
- Updated `tokio.loop` and `tokio.goal` packages load against host API 2.0.
- Configuration containing the removed `permission_mode` fails validation; no value is migrated or ignored.
- Registry build, signing, install, update, remove, link, and dev-override flows cover `tokio.permissions`.

## Acceptance criteria

The work is complete when all of the following are true:

1. A fresh/default installation does not install `tokio.permissions`.
2. With the extension absent, tool execution has a direct allow path and no permission-specific runtime state or UI.
3. `/permissions`, permission modes, approval decisions, menus, persistence, and permission CLI options exist only when `tokio.permissions` is installed.
4. The extension package owns policy and declarative UI content; the host owns only generic safe rendering and transport.
5. The installed extension reproduces current mode behavior and cancellation semantics, with operation-scoped session approval that never grants blanket access to Bash or unrelated targets.
6. Runtime failure of an installed gate cannot silently turn into ungated execution.
7. Explicit install, enable, disable, and removal update the active session immediately at a well-defined tool-admission boundary.
8. Host API 2.0 is clean of the replaced permission-specific protocol, and all official extensions are rebuilt for it.
9. Documentation clearly states that uninstalling or not installing the extension means all tools run automatically; the host displays no permission-specific warning or status when it is absent.
