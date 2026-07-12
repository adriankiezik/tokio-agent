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
    Approve { id: u64, decision: ApprovalDecision },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowAlways,
    Deny,
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
    pub permission: ToolPermission,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermission {
    Read,
    Edit,
    #[default]
    Execute,
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
pub const COMPANION_PROTOCOL_VERSION: u32 = 1;

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
    ValidateComponent {
        component_path: String,
    },
    Load {
        extension: ExtensionId,
        generation: u64,
        component_path: String,
        capabilities: Vec<Capability>,
        limits: RuntimeLimits,
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
    ComponentValid,
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
