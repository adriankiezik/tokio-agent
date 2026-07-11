use serde::{Deserialize, Serialize};

use crate::message::{Message, ToolCallId, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawFrame {
    pub provider: String,
    pub payload: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolCallStart {
        id: ToolCallId,
        name: String,
    },
    ToolCallArgs {
        id: ToolCallId,
        fragment: String,
    },
    ToolCallEnd {
        id: ToolCallId,
    },
    HostedToolCallStart {
        id: ToolCallId,
        name: String,
        summary: String,
    },
    HostedToolCallEnd {
        id: ToolCallId,
        name: String,
        output: String,
        is_error: bool,
    },
    Usage(Usage),
    Done {
        stop: StopReason,
        message: Message,
    },
    Failed {
        retryable: bool,
        error: String,
        message: Message,
    },
    Unknown(RawFrame),
}
