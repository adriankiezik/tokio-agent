const SECOND_MS = 1_000;
const MINUTE_MS = 60 * SECOND_MS;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;
const TIMER_ID = "loop";

interface LoopState {
  intervalMs: number;
  remainingMs: number;
  scheduledMs: number;
  prompt: string;
}

let state: LoopState = emptyState();

function emptyState(): LoopState {
  return { intervalMs: 0, remainingMs: 0, scheduledMs: 0, prompt: "" };
}

function status(text: string): ExtensionAction {
  return tokio.actions.setStatus({
    id: "loop",
    text,
    tone: "normal",
    side: "left",
    priority: 90,
    min_width: 8,
  });
}

function beginRun(): ExtensionAction[] {
  state.remainingMs = 0;
  state.scheduledMs = 0;
  return [
    tokio.actions.cancelTimer(TIMER_ID),
    tokio.actions.submitPrompt(state.prompt, true),
    status("loop: running"),
  ];
}

function countdownStep(): ExtensionAction[] {
  const unit = state.remainingMs <= MINUTE_MS
    ? SECOND_MS
    : state.remainingMs <= HOUR_MS
      ? MINUTE_MS
      : state.remainingMs <= DAY_MS ? HOUR_MS : DAY_MS;
  state.scheduledMs = Math.min(state.remainingMs, unit);
  return [
    tokio.actions.scheduleTimer(TIMER_ID, state.scheduledMs),
    status(`loop: next in ${formatDuration(state.remainingMs)}`),
  ];
}

function armCountdown(): ExtensionAction[] {
  state.remainingMs = state.intervalMs;
  return countdownStep();
}

function parseInterval(value: string): number | undefined {
  const match = /^(\d+)(s|m|h)$/.exec(value);
  if (!match) return undefined;
  const multiplier = match[2] === "s" ? SECOND_MS : match[2] === "m" ? MINUTE_MS : HOUR_MS;
  const milliseconds = Number(match[1]) * multiplier;
  return Number.isSafeInteger(milliseconds) && milliseconds >= 10_000 ? milliseconds : undefined;
}

function formatDuration(milliseconds: number): string {
  if (milliseconds < MINUTE_MS) return `${Math.ceil(milliseconds / SECOND_MS)}s`;
  if (milliseconds < HOUR_MS) return `${Math.ceil(milliseconds / MINUTE_MS)}m`;
  if (milliseconds < DAY_MS) return `${Math.ceil(milliseconds / HOUR_MS)}h`;
  return `${Math.ceil(milliseconds / DAY_MS)}d`;
}

function restore(bytes: Uint8Array): void {
  const saved = tokio.storage.decodeJson<Json>(bytes, null);
  if (Array.isArray(saved) && typeof saved[0] === "number" && typeof saved[1] === "string") {
    state = { intervalMs: saved[0], remainingMs: saved[0], scheduledMs: 0, prompt: saved[1] };
  } else if (saved && typeof saved === "object" && !Array.isArray(saved)) {
    const value = saved as Record<string, Json>;
    if (typeof value.intervalMs === "number" && typeof value.prompt === "string") {
      state = { intervalMs: value.intervalMs, remainingMs: value.intervalMs, scheduledMs: 0, prompt: value.prompt };
      return;
    }
    state = emptyState();
  } else {
    state = emptyState();
  }
}

tokio.defineExtension({
  loadState({ sessionState }) { restore(sessionState); },
  restoreSessionState(bytes) { restore(bytes); },

  onCommand(_handler, commandInput) {
    const input = commandInput.trim();
    if (["cancel", "clear", "stop"].includes(input)) {
      state = emptyState();
      return [
        tokio.actions.cancelTimer(TIMER_ID),
        tokio.actions.clearStatus("loop"),
        tokio.actions.notice("info", "loop: stopped"),
        tokio.actions.releaseAutonomy(),
        tokio.actions.persistSessionBytes([]),
      ];
    }
    const separator = input.search(/\s/);
    if (separator < 0) {
      return [tokio.actions.notice("error", "Usage: /loop <10s|5m|2h> <prompt> | cancel")];
    }
    const intervalMs = parseInterval(input.slice(0, separator));
    const prompt = input.slice(separator).trim();
    if (intervalMs === undefined || prompt.length === 0) {
      return [tokio.actions.notice("error", "Invalid interval; use at least 10s, for example 5m")];
    }
    state = { intervalMs, remainingMs: 0, scheduledMs: 0, prompt };
    return [...beginRun(), tokio.actions.persistSessionJson({ intervalMs, prompt })];
  },

  onEvent(event) {
    if (state.intervalMs === 0) return [];
    if (event.type === "timer_fired" && event.value.id === TIMER_ID) {
      state.remainingMs = Math.max(0, state.remainingMs - state.scheduledMs);
      return state.remainingMs === 0 ? beginRun() : countdownStep();
    }
    if (["turn_finished", "interrupted", "session_started"].includes(event.type)) {
      return armCountdown();
    }
    if (event.type === "session_stopping") {
      state = emptyState();
      return [
        tokio.actions.cancelTimer(TIMER_ID),
        tokio.actions.releaseAutonomy(),
        tokio.actions.clearStatus("loop"),
        tokio.actions.persistSessionBytes([]),
      ];
    }
    return [];
  },
});
