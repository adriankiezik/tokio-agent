interface GoalState {
  objective: string;
  active: boolean;
  paused: boolean;
}

let state: GoalState = { objective: "", active: false, paused: false };

function status(text: string): ExtensionAction {
  return tokio.actions.setStatus({
    id: "goal",
    text,
    tone: "normal",
    side: "left",
    priority: 100,
    min_width: 8,
  });
}

function persist(): ExtensionAction {
  return tokio.actions.persistSessionJson([state.objective, state.active, state.paused]);
}

function restore(bytes: Uint8Array): void {
  const saved = tokio.storage.decodeJson<Json>(bytes, null);
  if (Array.isArray(saved)
      && typeof saved[0] === "string"
      && typeof saved[1] === "boolean"
      && typeof saved[2] === "boolean") {
    state = { objective: saved[0], active: saved[1], paused: saved[2] };
  } else {
    state = { objective: "", active: false, paused: false };
  }
}

function continuation(): string {
  return `Continue working toward the active goal. Verify progress and keep going until complete or genuinely blocked.\n\nGoal: ${state.objective}`;
}

function deactivate(message: string): ExtensionAction[] {
  return [
    tokio.actions.unregisterTool("tokio.goal:update_goal"),
    tokio.actions.clearStatus("goal"),
    tokio.actions.notice("info", message),
    tokio.actions.persistSessionBytes([]),
    tokio.actions.releaseAutonomy(),
  ];
}

tokio.defineExtension({
  loadState({ sessionState }) { restore(sessionState); },
  restoreSessionState(bytes) { restore(bytes); },

  onCommand(_handler, commandInput) {
    const input = commandInput.trim();
    if (input === "cancel" || input === "clear") {
      state = { objective: "", active: false, paused: false };
      return deactivate("goal: cancelled");
    }
    if (input === "pause" && state.active) {
      state.paused = true;
      return [status("goal: paused"), persist()];
    }
    if (input === "resume" && state.active) {
      state.paused = false;
      return [tokio.actions.submitPrompt(continuation(), true), status("goal: active"), persist()];
    }
    if (input.length === 0) {
      return [tokio.actions.notice("error", "Usage: /goal <objective> | pause | resume | cancel")];
    }
    if (input === "pause" || input === "resume") {
      return [tokio.actions.notice("error", `Cannot ${input}; no goal is active`)];
    }

    state = { objective: input, active: true, paused: false };
    return [
      tokio.actions.submitPrompt(
        `Work autonomously toward this goal:\n\n${input}\n\nContinue until verified complete or genuinely blocked.`,
        true,
      ),
      tokio.actions.registerTool({
        id: "tokio.goal:update_goal",
        name: "update_goal",
        description: "Mark the active goal complete or blocked",
        inputSchema: {
          type: "object",
          properties: { status: { type: "string", enum: ["complete", "blocked"] } },
          required: ["status"],
          additionalProperties: false,
        },
        effect: "read",
      }),
      status("goal: active"),
      persist(),
    ];
  },

  onEvent(event) {
    if (!state.active) return [];
    if (event.type === "turn_finished" && !state.paused) {
      return [tokio.actions.submitPrompt(continuation(), true)];
    }
    if (event.type === "interrupted") {
      state.paused = true;
      return [status("goal: paused"), persist()];
    }
    return [];
  },

  onTool(_handler, toolInput) {
    const outcome = toolInput && typeof toolInput === "object" && !Array.isArray(toolInput)
      ? (toolInput as Record<string, Json>).status
      : undefined;
    if (outcome !== "complete" && outcome !== "blocked") {
      return { content: "status must be complete or blocked", is_error: true };
    }
    state.active = false;
    state.paused = false;
    return {
      content: `goal marked ${outcome}`,
      is_error: false,
      actions: deactivate(`goal: ${outcome}`),
    };
  },
});
