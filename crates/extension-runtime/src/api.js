(() => {
  "use strict";
  let identity;
  const freeze = Object.freeze;
  const action = (type, value) => freeze(value === undefined ? { type } : { type, value });
  const string = (value, name) => {
    if (typeof value !== "string") throw new TypeError(`${name} must be a string`);
    return value;
  };
  const integer = (value, name, minimum = 0) => {
    if (!Number.isSafeInteger(value) || value < minimum) {
      throw new TypeError(`${name} must be a safe integer >= ${minimum}`);
    }
    return value;
  };
  const bytes = (value, name = "state") => {
    if (value instanceof Uint8Array) return Array.from(value);
    if (Array.isArray(value) && value.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)) {
      return value.slice();
    }
    throw new TypeError(`${name} must be a Uint8Array or byte array`);
  };
  const encodeUtf8 = (text) => {
    const encoded = encodeURIComponent(text);
    const result = [];
    for (let index = 0; index < encoded.length; index += 1) {
      if (encoded[index] === "%") {
        result.push(Number.parseInt(encoded.slice(index + 1, index + 3), 16));
        index += 2;
      } else {
        result.push(encoded.charCodeAt(index));
      }
    }
    return result;
  };
  const decodeUtf8 = (value) => decodeURIComponent(
    bytes(value).map((byte) => `%${byte.toString(16).padStart(2, "0")}`).join(""),
  );
  const requireIdentity = () => {
    if (!identity) throw new Error("extension identity is unavailable before loadState");
    return identity;
  };

  const interactionRequest = (id, spec) => {
    const current = requireIdentity();
    return freeze({
      id: string(id, "id"),
      owner: current.extensionId,
      generation: current.generation,
      spec,
    });
  };

  const actions = freeze({
    submitPrompt: (text, automatic = false) => action("submit_prompt", {
      text: string(text, "text"),
      automatic: Boolean(automatic),
    }),
    steer: (text) => action("steer", { text: string(text, "text") }),
    notice: (level, text) => action("show_notice", {
      level: string(level, "level"),
      text: string(text, "text"),
    }),
    setStatus: (segment) => action("set_status_segment", segment),
    clearStatus: (id) => action("clear_status_segment", string(id, "id")),
    registerTool: (tool) => action("register_tool", {
      id: string(tool.id, "tool.id"),
      name: string(tool.name, "tool.name"),
      description: string(tool.description, "tool.description"),
      input_schema: tool.inputSchema,
      owner: requireIdentity().extensionId,
      effect: tool.effect ?? "unknown",
    }),
    unregisterTool: (id) => action("unregister_tool", string(id, "id")),
    scheduleTimer: (id, afterMs) => action("schedule_timer", {
      id: string(id, "id"),
      after: integer(afterMs, "afterMs"),
    }),
    cancelTimer: (id) => action("cancel_timer", string(id, "id")),
    persistSessionBytes: (value) => action("persist_session_state", bytes(value)),
    persistUserBytes: (value) => action("persist_user_state", bytes(value)),
    persistSessionJson: (value) => action("persist_session_state", encodeUtf8(JSON.stringify(value))),
    persistUserJson: (value) => action("persist_user_state", encodeUtf8(JSON.stringify(value))),
    persistSessionText: (value) => action("persist_session_state", encodeUtf8(string(value, "value"))),
    persistUserText: (value) => action("persist_user_state", encodeUtf8(string(value, "value"))),
    requestInteraction: (id, spec) => action("request_interaction", interactionRequest(id, spec)),
    fetch: (id, url) => action("fetch", {
      id: string(id, "id"),
      url: string(url, "url"),
    }),
    releaseAutonomy: () => action("release_autonomy"),
  });

  const storage = freeze({
    decodeText(value) { return decodeUtf8(value); },
    decodeJson(value, fallback = null) {
      if (value.length === 0) return fallback;
      return JSON.parse(decodeUtf8(value));
    },
  });

  const api = freeze({
    actions,
    interactions: freeze({ request: interactionRequest }),
    context: freeze({
      get extensionId() { return requireIdentity().extensionId; },
      get generation() { return requireIdentity().generation; },
    }),
    storage,
    defineExtension(extension) {
      if (globalThis.__tokio) throw new Error("defineExtension may only be called once");
      const encode = (value, fallback) => JSON.stringify(value ?? fallback);
      const callbacks = freeze({
        onCommand: (handler, input) => encode(extension.onCommand?.(handler, input), []),
        onEvent: (eventJson) => encode(extension.onEvent?.(JSON.parse(eventJson)), []),
        onTool: (handler, argumentsJson) => encode(
          extension.onTool?.(handler, JSON.parse(argumentsJson)),
          { content: "No tool handler was registered", is_error: true },
        ),
        authorizeTool: (handler, invocationJson) => encode(
          extension.authorizeTool?.(handler, JSON.parse(invocationJson)),
          { decision: "deny", reason: "No authorization handler was registered", actions: [] },
        ),
        onInteractionResponse: (handler, invocationId, responseJson) => encode(
          extension.onInteractionResponse?.(handler, invocationId, JSON.parse(responseJson)),
          { decision: "deny", reason: "No interaction handler was registered", actions: [] },
        ),
        loadState: (userStateJson, sessionStateJson, settingsJson, startupSettingsJson, hostJson) => {
          const host = JSON.parse(hostJson);
          identity = freeze({
            extensionId: string(host.extensionId, "extensionId"),
            generation: integer(host.generation, "generation"),
          });
          extension.loadState?.({
            userState: Uint8Array.from(JSON.parse(userStateJson)),
            sessionState: Uint8Array.from(JSON.parse(sessionStateJson)),
            settings: JSON.parse(settingsJson),
            startupSettings: JSON.parse(startupSettingsJson),
          });
          return "";
        },
        restoreSessionState: (stateJson) => {
          extension.restoreSessionState?.(Uint8Array.from(JSON.parse(stateJson)));
          return "";
        },
      });
      Object.defineProperty(globalThis, "__tokio", {
        value: callbacks,
        configurable: false,
        enumerable: false,
        writable: false,
      });
    },
  });
  Object.defineProperty(globalThis, "tokio", {
    value: api,
    configurable: false,
    enumerable: true,
    writable: false,
  });
})();
