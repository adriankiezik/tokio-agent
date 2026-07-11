use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

pub type ProviderName = String;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderMetadata(BTreeMap<ProviderName, Box<RawValue>>);

impl ProviderMetadata {
    pub fn get(&self, provider: &str) -> Option<&RawValue> {
        self.0.get(provider).map(Box::as_ref)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn from_provider(provider: ProviderName, raw: Box<RawValue>) -> Self {
        Self(BTreeMap::from([(provider, raw)]))
    }
}

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
#[serde(rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
        meta: ProviderMetadata,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
        meta: ProviderMetadata,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        args: Box<RawValue>,
        #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
        meta: ProviderMetadata,
    },
    ToolResult {
        call: ToolCallId,
        output: ToolOutput,
        is_error: bool,
        #[serde(default, skip_serializing_if = "ProviderMetadata::is_empty")]
        meta: ProviderMetadata,
    },
}

impl ContentBlock {
    #[must_use]
    pub fn parsed_args(&self) -> Option<Result<serde_json::Value, serde_json::Error>> {
        match self {
            ContentBlock::ToolCall { args, .. } => Some(serde_json::from_str(args.get())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}
