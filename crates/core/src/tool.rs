use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::message::ToolOutput;
use crate::provider::BoxFuture;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Edit,
    Execute,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool: String,
    pub summary: String,
    pub action: Action,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub output: ToolOutput,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            output: ToolOutput::Text(text.into()),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            output: ToolOutput::Text(text.into()),
            is_error: true,
        }
    }
}

pub struct ToolCtx {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
}

pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolDef;
    fn permission(&self, input: &Value) -> PermissionRequest;
    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult>;
}
