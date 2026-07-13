#![forbid(unsafe_code)]

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

string_id!(ExtensionId);
string_id!(CommandId);
string_id!(ToolId);
string_id!(TimerId);
string_id!(InteractionId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CommandSource {
    BuiltIn,
    Extension { id: ExtensionId, version: String },
    Local { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDescriptor {
    pub id: CommandId,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    pub source: CommandSource,
    #[serde(default)]
    pub available_while_running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum SessionCommand {
    SubmitMessage(String),
    InvokeCommand { id: CommandId, arguments: String },
    Interrupt,
    RespondToInteraction(InteractionResponse),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionResponse {
    pub id: InteractionId,
    pub owner: ExtensionId,
    pub generation: u64,
    pub action_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionTone {
    Neutral,
    Primary,
    Destructive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionAction {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_hint: Option<String>,
    pub tone: InteractionTone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalSpec {
    pub title: String,
    #[serde(default)]
    pub body: Vec<TextSection>,
    pub actions: Vec<InteractionAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingleSelectSpec {
    pub title: String,
    pub options: Vec<SelectOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "spec")]
pub enum InteractionSpec {
    Approval(ApprovalSpec),
    SingleSelect(SingleSelectSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionRequest {
    pub id: InteractionId,
    pub owner: ExtensionId,
    pub generation: u64,
    pub spec: InteractionSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusTone {
    Normal,
    Muted,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSide {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusSegment {
    pub id: String,
    pub text: String,
    pub tone: StatusTone,
    pub side: StatusSide,
    pub priority: i16,
    pub min_width: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDescriptor {
    pub id: ToolId,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub owner: ExtensionId,
    #[serde(default)]
    pub effect: ToolEffect,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    Read,
    Edit,
    Execute,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ToolOwner {
    BuiltIn,
    Extension { id: ExtensionId, version: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendCapabilities {
    pub interactive: bool,
    pub copy: bool,
    #[serde(default)]
    pub interaction_kinds: Vec<String>,
}

impl Default for FrontendCapabilities {
    fn default() -> Self {
        Self {
            interactive: true,
            copy: true,
            interaction_kinds: vec!["approval".into(), "single_select".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolGateInvocation {
    pub gate_owner: ExtensionId,
    pub gate_generation: u64,
    pub invocation_id: String,
    pub tool_name: String,
    pub owner: ToolOwner,
    pub arguments: serde_json::Value,
    pub effect: ToolEffect,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_hint: Option<String>,
    pub frontend: FrontendCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkRequest {
    pub id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkResponse {
    pub id: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ToolGateResponse {
    Allow {
        #[serde(default)]
        actions: Vec<ExtensionAction>,
    },
    Deny {
        reason: String,
        #[serde(default)]
        actions: Vec<ExtensionAction>,
    },
    RequestInteraction {
        interaction: InteractionRequest,
        #[serde(default)]
        actions: Vec<ExtensionAction>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum ExtensionAction {
    SubmitPrompt { text: String, automatic: bool },
    Steer { text: String },
    ShowNotice { level: NoticeLevel, text: String },
    SetStatusSegment(StatusSegment),
    ClearStatusSegment(String),
    RegisterTool(ToolDescriptor),
    UnregisterTool(ToolId),
    ScheduleTimer { id: TimerId, after: DurationDto },
    CancelTimer(TimerId),
    PersistSessionState(Vec<u8>),
    PersistUserState(Vec<u8>),
    RequestInteraction(InteractionRequest),
    Fetch(NetworkRequest),
    ReleaseAutonomy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DurationDto(pub u64);

impl From<Duration> for DurationDto {
    fn from(value: Duration) -> Self {
        Self(value.as_millis().try_into().unwrap_or(u64::MAX))
    }
}
impl From<DurationDto> for Duration {
    fn from(value: DurationDto) -> Self {
        Duration::from_millis(value.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    Interrupted,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum SessionEvent {
    SessionStarted,
    UserMessageSubmitted,
    AutomaticTurnStarted { source: ExtensionId },
    TurnFinished { stop: StopReason, usage: Usage },
    Interrupted,
    ToolFinished { name: String, is_error: bool },
    SessionStopping,
    TimerFired { id: TimerId },
    NetworkResponse(NetworkResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    SessionObserve,
    SessionSubmitAutomatic,
    SessionSchedule,
    ToolsDynamic,
    StatusWrite,
    StorageSession,
    StorageUser,
    ToolGate,
    InteractionRequest,
    SubagentsSpawn,
    FilesystemRead,
    FilesystemEdit,
    ProcessRequest,
    NetworkRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sequenced<T> {
    pub sequence: u64,
    pub extension: ExtensionId,
    pub generation: u64,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExtensionOrigin {
    OfficialRegistry { registry: String },
    ThirdPartyRegistry { registry: String, operator: String },
    Local { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSummary {
    pub id: ExtensionId,
    pub name: String,
    pub version: String,
    pub description: String,
    pub origin: ExtensionOrigin,
    pub installed: bool,
    #[serde(default)]
    pub local_override: bool,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub status_segments: Vec<String>,
}

pub const HOST_API_VERSION: &str = "1.0.0";
pub const COMPANION_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLimits {
    pub memory_bytes: u64,
    pub fuel_per_callback: u64,
    pub callback_deadline_ms: u64,
    pub maximum_payload_bytes: u64,
    pub maximum_status_chars: u32,
    pub maximum_status_updates_per_second: u32,
    pub maximum_timers: u32,
    pub minimum_timer_interval_ms: u64,
    pub maximum_pending_actions: u32,
    pub circuit_breaker_failures: u32,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            fuel_per_callback: 10_000_000,
            callback_deadline_ms: 2_000,
            maximum_payload_bytes: 256 * 1024,
            maximum_status_chars: 160,
            maximum_status_updates_per_second: 10,
            maximum_timers: 32,
            minimum_timer_interval_ms: 100,
            maximum_pending_actions: 128,
            circuit_breaker_failures: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum HostRequest {
    Handshake {
        protocol_version: u32,
        host_api: String,
    },
    ValidateScript {
        script_path: String,
    },
    Load {
        extension: ExtensionId,
        generation: u64,
        script_path: String,
        capabilities: Vec<Capability>,
        limits: RuntimeLimits,
        #[serde(default)]
        user_state: Vec<u8>,
        #[serde(default)]
        settings: serde_json::Value,
        #[serde(default)]
        startup_settings: serde_json::Value,
    },
    InvokeCommand {
        extension: ExtensionId,
        generation: u64,
        handler: String,
        arguments: String,
    },
    InvokeTool {
        extension: ExtensionId,
        generation: u64,
        handler: String,
        arguments_json: String,
    },
    AuthorizeTool {
        extension: ExtensionId,
        generation: u64,
        handler: String,
        invocation: ToolGateInvocation,
    },
    InteractionResponse {
        extension: ExtensionId,
        generation: u64,
        handler: String,
        invocation_id: String,
        response: InteractionResponse,
    },
    SessionEvent(Sequenced<SessionEvent>),
    RestoreSessionState {
        extension: ExtensionId,
        generation: u64,
        state: Vec<u8>,
    },
    Disable {
        extension: ExtensionId,
        generation: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum HostResponse {
    Handshake {
        protocol_version: u32,
        host_api: String,
    },
    ScriptValid,
    Loaded {
        extension: ExtensionId,
        generation: u64,
    },
    Actions(Vec<Sequenced<ExtensionAction>>),
    ToolResult {
        content: String,
        is_error: bool,
        #[serde(default)]
        actions: Vec<Sequenced<ExtensionAction>>,
    },
    ToolGateResult(ToolGateResponse),
    SessionStateRestored {
        extension: ExtensionId,
        generation: u64,
    },
    Disabled {
        extension: ExtensionId,
        generation: u64,
    },
    Error {
        extension: Option<ExtensionId>,
        message: String,
        retryable: bool,
    },
}
