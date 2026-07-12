use std::path::PathBuf;
use std::sync::Arc;

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

pub type ToolProgress = Arc<dyn Fn(String) + Send + Sync>;

pub struct ToolCtx {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    progress: Option<ToolProgress>,
}

impl ToolCtx {
    #[must_use]
    pub fn new(cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            cwd,
            cancel,
            progress: None,
        }
    }

    #[must_use]
    pub fn with_progress(mut self, progress: ToolProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn report_progress(&self, text: impl Into<String>) {
        if let Some(progress) = &self.progress {
            progress(text.into());
        }
    }

    #[must_use]
    pub fn progress_callback(&self) -> Option<ToolProgress> {
        self.progress.clone()
    }
}

pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolDef;
    fn permission(&self, input: &Value) -> PermissionRequest;
    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult>;
}
