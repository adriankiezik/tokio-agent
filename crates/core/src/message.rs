use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type ProviderName = String;

pub type ProviderMetadata = BTreeMap<ProviderName, Box<serde_json::value::RawValue>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutput {
    Text(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        meta: ProviderMetadata,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        meta: ProviderMetadata,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        args: serde_json::Value,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        meta: ProviderMetadata,
    },
    ToolResult {
        call: ToolCallId,
        output: ToolOutput,
        is_error: bool,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        meta: ProviderMetadata,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}
