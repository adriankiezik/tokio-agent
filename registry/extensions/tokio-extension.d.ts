type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Bytes = Uint8Array | number[];
type NoticeLevel = "info" | "warning" | "error";
type StatusTone = "normal" | "muted" | "success" | "warning" | "error";
type StatusSide = "left" | "right";
type ToolEffect = "read" | "edit" | "execute" | "unknown";

interface StatusSegment {
  id: string;
  text: string;
  tone: StatusTone;
  side: StatusSide;
  priority: number;
  min_width: number;
}

interface ToolDefinition {
  id: string;
  name: string;
  description: string;
  inputSchema: Json;
  effect?: ToolEffect;
}

interface TextSection { heading?: string; text: string }
interface InteractionAction {
  id: string;
  label: string;
  key_hint?: string;
  tone: "neutral" | "primary" | "destructive";
}
type InteractionSpec =
  | { kind: "approval"; spec: { title: string; body?: TextSection[]; actions: InteractionAction[]; copy_text?: string } }
  | { kind: "single_select"; spec: { title: string; options: Array<{ id: string; label: string; description?: string }>; selected?: string } };

type SessionEvent =
  | { type: "session_started" }
  | { type: "user_message_submitted" }
  | { type: "automatic_turn_started"; value: { source: string } }
  | { type: "turn_finished"; value: { stop: string; usage: { input_tokens: number; output_tokens: number } } }
  | { type: "interrupted" }
  | { type: "tool_finished"; value: { name: string; is_error: boolean } }
  | { type: "session_stopping" }
  | { type: "timer_fired"; value: { id: string } }
  | { type: "network_response"; value: NetworkResponse };

interface NetworkResponse {
  id: string;
  url: string;
  status?: number;
  body?: string;
  error?: string;
}

type ExtensionAction =
  | { type: "submit_prompt"; value: { text: string; automatic: boolean } }
  | { type: "steer"; value: { text: string } }
  | { type: "show_notice"; value: { level: NoticeLevel; text: string } }
  | { type: "set_status_segment"; value: StatusSegment }
  | { type: "clear_status_segment"; value: string }
  | { type: "register_tool"; value: Json }
  | { type: "unregister_tool"; value: string }
  | { type: "schedule_timer"; value: { id: string; after: number } }
  | { type: "cancel_timer"; value: string }
  | { type: "persist_session_state"; value: number[] }
  | { type: "persist_user_state"; value: number[] }
  | { type: "request_interaction"; value: Json }
  | { type: "fetch"; value: { id: string; url: string } }
  | { type: "release_autonomy" };

interface ToolResult { content: string; is_error: boolean; actions?: ExtensionAction[] }
type GateResult =
  | { decision: "allow"; actions?: ExtensionAction[] }
  | { decision: "deny"; reason: string; actions?: ExtensionAction[] }
  | { decision: "request_interaction"; interaction: InteractionRequest; actions?: ExtensionAction[] };
interface LoadState {
  userState: Uint8Array;
  sessionState: Uint8Array;
  settings: Json;
  startupSettings: Json;
}
interface TokioExtension {
  onCommand?(handler: string, arguments: string): ExtensionAction[];
  onEvent?(event: SessionEvent): ExtensionAction[];
  onTool?(handler: string, arguments: Json): ToolResult;
  authorizeTool?(handler: string, invocation: Json): GateResult;
  onInteractionResponse?(handler: string, invocationId: string, response: Json): GateResult;
  loadState?(state: LoadState): void;
  restoreSessionState?(state: Uint8Array): void;
}
interface TokioActions {
  submitPrompt(text: string, automatic?: boolean): ExtensionAction;
  steer(text: string): ExtensionAction;
  notice(level: NoticeLevel, text: string): ExtensionAction;
  setStatus(segment: StatusSegment): ExtensionAction;
  clearStatus(id: string): ExtensionAction;
  registerTool(tool: ToolDefinition): ExtensionAction;
  unregisterTool(id: string): ExtensionAction;
  scheduleTimer(id: string, afterMs: number): ExtensionAction;
  cancelTimer(id: string): ExtensionAction;
  persistSessionBytes(value: Bytes): ExtensionAction;
  persistUserBytes(value: Bytes): ExtensionAction;
  persistSessionJson(value: Json): ExtensionAction;
  persistUserJson(value: Json): ExtensionAction;
  persistSessionText(value: string): ExtensionAction;
  persistUserText(value: string): ExtensionAction;
  requestInteraction(id: string, spec: InteractionSpec): ExtensionAction;
  fetch(id: string, url: string): ExtensionAction;
  releaseAutonomy(): ExtensionAction;
}
interface InteractionRequest {
  id: string;
  owner: string;
  generation: number;
  spec: InteractionSpec;
}
declare const tokio: Readonly<{
  actions: Readonly<TokioActions>;
  interactions: Readonly<{ request(id: string, spec: InteractionSpec): InteractionRequest }>;
  context: Readonly<{ extensionId: string; generation: number }>;
  storage: Readonly<{
    decodeText(value: Bytes): string;
    decodeJson<T extends Json>(value: Bytes, fallback: T): T;
  }>;
  defineExtension(extension: TokioExtension): void;
}>;
