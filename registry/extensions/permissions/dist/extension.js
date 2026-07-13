"use strict";
let mode = "suggest";
let pending = new Map();
let approvedScopes = new Set();
function isMode(value) {
    return value === "suggest" || value === "auto-edit" || value === "full-auto";
}
function asks(effect) {
    if (mode === "full-auto")
        return false;
    if (mode === "auto-edit")
        return effect === "execute" || effect === "unknown";
    return effect !== "read";
}
function applySettings(value) {
    if (!value || typeof value !== "object" || Array.isArray(value)) {
        throw new Error("permissions settings must be an object");
    }
    for (const [key, setting] of Object.entries(value)) {
        if (key !== "mode" && key !== "permission-mode") {
            throw new Error(`unknown permissions setting: ${key}`);
        }
        if (!isMode(setting)) {
            throw new Error(`invalid ${key}; use suggest, auto-edit, or full-auto`);
        }
        mode = setting;
    }
}
function canonical(value) {
    if (Array.isArray(value))
        return value.map(canonical);
    if (value && typeof value === "object") {
        return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])]));
    }
    return value;
}
function lexicalPath(value, cwd) {
    const absolute = value.startsWith("/") || /^[A-Za-z]:[\\/]/.test(value);
    const joined = absolute ? value : `${cwd}/${value}`;
    const prefix = joined.startsWith("/") ? "/" : "";
    const parts = [];
    for (const part of joined.split(/[\\/]+/)) {
        if (!part || part === ".")
            continue;
        if (part === "..")
            parts.pop();
        else
            parts.push(part);
    }
    return prefix + parts.join("/");
}
function objectArguments(invocation) {
    return invocation.arguments && typeof invocation.arguments === "object" && !Array.isArray(invocation.arguments)
        ? invocation.arguments
        : {};
}
function scopeOperation(invocation) {
    const args = objectArguments(invocation);
    if (invocation.tool_name === "bash") {
        return { command: typeof args.command === "string" ? args.command : "", cwd: lexicalPath(invocation.cwd, invocation.cwd) };
    }
    if (["write", "edit", "multi_edit"].includes(invocation.tool_name)) {
        const path = typeof args.path === "string" ? args.path : "";
        return { family: "edit", paths: [lexicalPath(path, invocation.cwd)] };
    }
    if (["read", "grep", "glob"].includes(invocation.tool_name)) {
        const path = typeof args.path === "string" ? args.path : "";
        return { target: lexicalPath(path, invocation.cwd) };
    }
    return canonical(invocation.arguments);
}
function scope(invocation) {
    return JSON.stringify(canonical({
        owner: invocation.owner,
        tool: invocation.tool_name,
        cwd: lexicalPath(invocation.cwd, invocation.cwd),
        operation: scopeOperation(invocation),
    }));
}
function summary(invocation) {
    const raw = invocation.summary_hint ?? `${invocation.tool_name} ${JSON.stringify(canonical(invocation.arguments))}`;
    const clean = Array.from(raw, (character) => /[\u0000-\u001f\u007f]/.test(character) ? " " : character)
        .slice(0, 512)
        .join("");
    return Array.from(raw).length > 512 ? `${clean}…` : clean;
}
function approvalSupported(frontend) {
    return frontend.interactive && frontend.interaction_kinds.includes("approval");
}
tokio.defineExtension({
    loadState({ userState, sessionState, settings, startupSettings }) {
        mode = "suggest";
        pending = new Map();
        approvedScopes = new Set();
        if (userState.length > 0) {
            const savedMode = tokio.storage.decodeText(userState);
            if (isMode(savedMode))
                mode = savedMode;
        }
        applySettings(settings);
        applySettings(startupSettings);
        const scopes = tokio.storage.decodeJson(sessionState, []);
        if (Array.isArray(scopes)) {
            approvedScopes = new Set(scopes.filter((value) => typeof value === "string"));
        }
    },
    restoreSessionState(bytes) {
        const scopes = tokio.storage.decodeJson(bytes, []);
        approvedScopes = new Set(Array.isArray(scopes)
            ? scopes.filter((value) => typeof value === "string")
            : []);
    },
    onCommand(handler) {
        if (handler !== "permissions_command")
            return [];
        return [tokio.actions.requestInteraction(`mode:${tokio.context.generation}`, {
                kind: "single_select",
                spec: {
                    title: "Tool approval mode",
                    options: [
                        { id: "suggest", label: "Suggest", description: "Ask before edits and commands" },
                        { id: "auto-edit", label: "Auto-edit", description: "Allow edits; ask before commands" },
                        { id: "full-auto", label: "Full-auto", description: "Allow all tool calls" },
                    ],
                    selected: mode,
                },
            })];
    },
    authorizeTool(handler, value) {
        if (handler !== "authorize_tool" || !value || typeof value !== "object" || Array.isArray(value)) {
            return { decision: "deny", reason: "invalid authorization request", actions: [] };
        }
        const invocation = value;
        if (invocation.gate_owner !== tokio.context.extensionId
            || invocation.gate_generation !== tokio.context.generation) {
            return { decision: "deny", reason: "stale tool gate generation", actions: [] };
        }
        const invocationScope = scope(invocation);
        if (!asks(invocation.effect) || approvedScopes.has(invocationScope)) {
            return { decision: "allow", actions: [] };
        }
        if (!approvalSupported(invocation.frontend)) {
            return {
                decision: "deny",
                reason: "approval is required but this frontend is non-interactive; use --permission-mode full-auto for unattended execution",
                actions: [],
            };
        }
        const interactionId = `approval:${tokio.context.generation}:${invocation.invocation_id}`;
        pending.set(interactionId, { invocationId: invocation.invocation_id, scope: invocationScope });
        const text = summary(invocation);
        return {
            decision: "request_interaction",
            interaction: tokio.interactions.request(interactionId, {
                kind: "approval",
                spec: {
                    title: "Tool approval required",
                    body: [{ heading: invocation.tool_name, text }],
                    actions: [
                        { id: "allow_once", label: "Allow once", key_hint: "y", tone: "primary" },
                        { id: "allow_session", label: "Allow for session", key_hint: "a", tone: "neutral" },
                        { id: "deny", label: "Deny", key_hint: "n", tone: "destructive" },
                    ],
                    copy_text: text,
                },
            }),
            actions: [],
        };
    },
    onInteractionResponse(handler, invocationId, value) {
        if (handler !== "on_interaction_response" || !value || typeof value !== "object" || Array.isArray(value)) {
            return { decision: "deny", reason: "invalid interaction response", actions: [] };
        }
        const response = value;
        if (response.owner !== tokio.context.extensionId || response.generation !== tokio.context.generation) {
            return { decision: "deny", reason: "stale or wrong-owner interaction response", actions: [] };
        }
        if (response.id === `mode:${tokio.context.generation}`) {
            if (!isMode(response.action_id)) {
                return { decision: "deny", reason: "mode selection cancelled", actions: [] };
            }
            mode = response.action_id;
            return { decision: "allow", actions: [tokio.actions.persistUserText(mode)] };
        }
        const request = pending.get(response.id);
        pending.delete(response.id);
        if (!request)
            return { decision: "deny", reason: "stale or duplicate interaction response", actions: [] };
        if (request.invocationId !== invocationId) {
            return { decision: "deny", reason: "interaction does not own this invocation", actions: [] };
        }
        if (response.action_id === "allow_once")
            return { decision: "allow", actions: [] };
        if (response.action_id === "allow_session") {
            approvedScopes.add(request.scope);
            return {
                decision: "allow",
                actions: [tokio.actions.persistSessionJson(Array.from(approvedScopes).sort())],
            };
        }
        if (response.action_id === "deny" || response.action_id === "cancel") {
            return { decision: "deny", reason: "denied by user", actions: [] };
        }
        return { decision: "deny", reason: "unknown interaction action", actions: [] };
    },
});
