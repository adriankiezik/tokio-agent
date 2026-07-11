# `/goal` and `/loop`: upstream analysis and tokio-agent port design

Research baseline: OpenAI Codex commit `3e7baa00e43419967d90d6ad9cef40f58d5ac89f` and the Claude Code changelog through 2.1.173.

## Executive summary

`/goal` and `/loop` are related but intentionally different autonomy primitives:

- `/goal` is **completion-driven**. The user declares an objective or completion condition. The runtime keeps starting continuation turns until the model explicitly proves completion, becomes genuinely blocked, is paused, or reaches a resource limit.
- `/loop` is **time-driven**. The user schedules a prompt or slash command at a recurring interval. Each firing is independent; success does not end the schedule.

They should not be implemented as aliases. A goal needs lifecycle state, completion authority, resource accounting, interruption semantics, and model-visible control tools. A loop needs a monotonic timer, recurring wakeups, cancellation, and safe interaction with active turns.

For tokio-agent, both belong primarily in the provider-neutral core agent task. The TUI should parse commands and project state, but it must not own scheduling or autonomous continuation. This preserves the existing `UiCommand` / `AgentEvent` seam and leaves room for a future headless frontend.

## Codex CLI `/goal`

### User-facing command surface

Codex exposes `/goal` as “set or view the goal for a long-running task.” The current TUI behavior supports:

- `/goal <objective>` — create or replace a goal, with confirmation before replacing unfinished work.
- `/goal --tokens <budget> <objective>` — optional positive token budget; compact suffixes such as `98.5K` are accepted.
- `/goal` — show objective, status, elapsed time, tokens used, optional token budget, and relevant commands.
- `/goal edit`
- `/goal pause`
- `/goal resume`
- `/goal clear`

The status line distinguishes active, paused, blocked, usage-limited, budget-limited, and complete goals. Interrupting an active goal pauses it rather than silently abandoning it. Resuming a saved thread with a stopped-but-resumable goal prompts the user to resume or leave it paused.

### Persistence and state model

Codex persists one goal per saved thread. The stored record includes:

- a stable goal ID,
- thread ID,
- objective,
- status,
- optional token budget,
- tokens used,
- elapsed active time,
- creation/update timestamps.

Ephemeral threads cannot use goals. Creating a second goal through the model tool is rejected; the user-facing command owns replacement policy. Completed goals can be replaced without confirmation, while unfinished goals require confirmation.

The important design point is that the goal is not merely another user message. It is durable thread metadata with an explicit state machine.

### Model/runtime contract

Codex gives the model three internal tools when goals are enabled:

- `get_goal` — inspect current objective, lifecycle state, and budgets.
- `create_goal` — create a goal only when explicitly requested.
- `update_goal` — mark the existing goal `complete` or `blocked`.

The model is deliberately not allowed to pause, resume, clear, or budget-limit a goal. Those transitions belong to the user or runtime. This separation prevents the model from escaping autonomous work by changing policy state.

The continuation prompt is strict:

- preserve the full objective across turns,
- verify completion against current repository state,
- do not redefine success around partial progress,
- only mark complete when every requirement is supported by evidence,
- retry blockers before declaring the goal blocked,
- do not mark complete merely because a budget is nearly exhausted.

Codex currently requires a repeated blocker across at least three consecutive goal turns before the model may mark the goal blocked. That avoids terminating on the first recoverable error.

### Continuation scheduling

At the end of a completed turn, the runtime checks whether the persisted goal remains active. If the session is idle, it starts another turn using an internal goal-continuation context message. Continuations are suppressed in plan mode and when an automatic continuation made no counted autonomous progress, preventing hot no-op loops.

An objective edited during an active turn is injected as steering. Resuming an active goal while idle also schedules continuation. A goal remains active across normal assistant turn boundaries; “the assistant answered” is not a stop condition.

### Accounting and limits

Codex separately accounts active wall-clock time and token deltas. Accounting begins from the exact usage baseline at goal activation, including activation in the middle of a turn. Tool completion and turn completion flush progress so a crash loses as little accounting as practical.

When a token budget is reached, the runtime transitions to `budget_limited`, injects a final steering message, prevents new substantive work, and asks the model to summarize progress and remaining work. Provider/account usage limits produce a distinct `usage_limited` state. Neither is treated as successful completion.

### Safety and reliability lessons

Codex’s implementation highlights several non-obvious requirements:

1. Completion must be an explicit state transition, not inferred from a polite final answer.
2. User-owned and model-owned transitions must be separated.
3. Interrupt means pause for goal work.
4. Autonomous continuations need no-progress and blocker escape hatches.
5. Resource limits are terminal-but-unsuccessful states.
6. Goal input is untrusted user data and must not be interpolated as system-level instructions without delimiting/escaping.
7. Goal tools should be hidden where durable state or appropriate authority is unavailable.

## Claude Code `/goal`

Claude Code added `/goal` in 2.1.139. Publicly documented behavior is “set a completion condition and Claude keeps working across turns until it’s met,” available in interactive mode, print mode, and Remote Control. Its UI shows live elapsed time, turn count, and tokens.

Unlike Codex, Claude Code’s implementation is proprietary, so its detailed state schema and prompts are not inspectable. Changelog evidence still reveals important behavior:

- evaluation waits for background shells and delegated subagents to finish,
- managed-hook restrictions are handled explicitly rather than leaving a hanging indicator,
- idle rendering of the goal chip is throttled,
- the feature crosses ordinary turn boundaries and is not just a stop hook,
- it works in headless and remote frontends, implying the runtime—not the terminal widget—owns goal execution.

The reference to a “goal evaluator” suggests Claude Code may evaluate the completion condition out of band rather than relying only on a model-called completion tool. That is a different implementation strategy from Codex, but it serves the same architectural requirement: completion needs an explicit, auditable decision independent of ordinary turn termination.

### Tool transition vs evaluator

Two valid designs emerge:

- **Codex-style model tool:** cheap, deterministic protocol, and completion is explicit. Its weakness is dependence on the working model remembering and correctly calling the tool.
- **Claude-style evaluator:** can independently judge the latest state and completion condition, but consumes extra model calls, can disagree with the worker, and requires careful exclusion of still-running background work.

For a small provider-neutral agent, the Codex-style tool is the better first implementation. A later evaluator can be layered on as a fallback or verifier.

## Claude Code `/loop`

### Command semantics

Claude Code added `/loop` in 2.1.71:

```text
/loop 5m check the deploy
```

It runs a prompt or slash command on a recurring interval within the current session. `/proactive` is an alias. Claude also exposes cron scheduling tools, but `/loop` is the direct interactive surface.

Changelog behavior establishes these semantics:

- recurring wakeups are timestamped in the transcript,
- a wakeup is visibly labeled as a `/loop` wakeup,
- Esc/Ctrl+C can cancel a pending idle wakeup,
- redundant polling wakeups are avoided when background tasks already provide completion notifications,
- loops work across supported cloud backends and do not depend on telemetry,
- remote sessions do not promote the feature because pending loops do not keep short-lived remote containers alive,
- `CLAUDE_CODE_DISABLE_CRON` provides a process-wide emergency stop.

### Runtime implications

A correct loop scheduler needs:

- monotonic deadlines (`Instant`), not wall-clock arithmetic,
- explicit active/cancelled state,
- no overlapping foreground turns,
- coalescing or skipping missed ticks rather than creating an unbounded backlog,
- cancellation while both idle and running,
- a visible distinction between user submissions and scheduled submissions,
- no assumption that the process or remote container will remain alive.

`/loop` should generally use fixed-delay behavior: schedule the next firing after the previous firing is accepted or completed. Fixed-rate catch-up can flood the queue after sleep, laptop suspend, a long turn, or a permission prompt.

### Slash commands as payloads

Claude allows a prompt or slash command as the recurring payload. That means command dispatch should be reusable: scheduled input must pass through the same parser as interactive input rather than being forced into a plain model message. The initial tokio-agent port can safely limit payloads to prompts and document that limitation, but the internal representation should leave room for typed scheduled commands.

## Comparison

| Dimension | `/goal` | `/loop` |
|---|---|---|
| Stop condition | objective complete, blocked, paused, cancelled, or limited | explicit cancellation/session exit |
| Trigger | immediately and after each turn | recurring timer deadline |
| Payload | durable objective + internal continuation prompt | repeated prompt or command |
| Model authority | complete/blocked only | none required |
| User authority | create/edit/pause/resume/clear | create/replace/cancel |
| Persistence | valuable and central | optional; usually session-scoped |
| Accounting | turns, elapsed active time, tokens, budget | firings, next deadline |
| Interrupt | pause goal | cancel pending schedule; active turn policy must be explicit |
| Main risk | infinite autonomous no-progress work or false completion | runaway recurring costs/backlog |

## Port design for tokio-agent

### Existing seams

The current project already has the right high-level boundaries:

- `FrontendProjection` parses local slash commands and emits `UiCommand`.
- `Agent::run` is a single provider-neutral task that owns queued turns, cancellation, context, tools, and idle waiting.
- `AgentEvent` projects runtime changes back to any frontend.
- `ContextAssembler` owns the resendable transcript.

Scheduling in the TUI would violate the documented UI-blind core invariant and would not work in future headless mode. The core task must own both feature state machines.

### Proposed goal state

```rust
Goal {
    objective: String,
    status: Active | Paused | Blocked | Complete,
    turns: u32,
    usage: Usage,
    active_elapsed: Duration,
}
```

The first port should be session-scoped because tokio-agent does not yet persist conversation threads. Persistence should be added with transcript/session persistence rather than inventing a separate partial session database.

The core should register an internal, permission-free `update_goal` tool but only expose its schema while a goal is active. Accepted transitions are `complete` and `blocked`. Pause/resume/clear remain `UiCommand`s. Goal continuation messages must be internal context, clearly delimit the objective as untrusted text, and remind the model to verify completion and call the tool.

On normal turn completion:

1. flush usage/turn accounting,
2. emit updated goal state,
3. if still active, enqueue one continuation,
4. if interrupted, pause and do not enqueue,
5. if complete/blocked, stop.

Only one continuation may be queued at a time. User messages should take precedence over an automatic continuation and may steer the active goal without replacing it.

### Proposed loop state

```rust
LoopSchedule {
    interval: Duration,
    prompt: String,
    next_fire: Instant,
    iteration: u64,
}
```

The idle branch of `Agent::run` should `select!` between `commands.recv()` and the current deadline. After a scheduled turn, set `next_fire = Instant::now() + interval`; do not enqueue catch-up firings. Replacing a loop resets the iteration and deadline. Cancelling removes the timer. Interrupt should cancel a pending timer; the UI should offer an explicit `/loop cancel` so cancellation does not depend on keyboard timing.

Reasonable parser units are `s`, `m`, and `h`, with a conservative minimum interval to prevent accidental high-cost hot loops. The first port should reject zero, malformed, or missing intervals and empty prompts.

### Events and UI

Typed events should report lifecycle changes rather than making the TUI infer them:

- goal set/updated/paused/resumed/cleared,
- loop scheduled/fired/cancelled.

The footer can show one compact autonomy indicator, with active goal taking priority over loop. Transcript notices should identify automatic turns so users understand why the agent resumed.

### Required tests

Core tests:

- active goal automatically schedules a continuation,
- `update_goal(complete)` suppresses continuation,
- blocked suppresses continuation,
- interrupt pauses goal,
- user input is not starved by continuation,
- loop fires after its deadline,
- loop does not overlap a turn or accumulate missed ticks,
- cancel prevents a pending firing,
- shutdown cancels timers and active work.

TUI tests:

- `/goal <objective>`, `/goal pause|resume|clear`, and bare `/goal`,
- `/loop <duration> <prompt>` and `/loop cancel`,
- invalid syntax remains in the composer or produces a clear local error,
- slash autocomplete remains functional with argument-bearing commands.

### Deliberate first-port limits

- Session-scoped state only; no resume persistence until thread persistence exists.
- No token budget until the core has a stable cumulative session-usage counter exposed to autonomy state.
- No separate evaluator call; use an explicit model tool first.
- Loop payloads are prompts initially; recurring slash-command dispatch can follow when command parsing is extracted from the TUI into a shared typed command layer.
- A conservative continuation cap/no-progress guard should be added before presenting `/goal` as unattended infinite execution.

These limits preserve the essential semantics without coupling the implementation to unfinished persistence, plugin, or headless subsystems.
