use std::path::PathBuf;
use std::sync::{Arc, RwLock};

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

pub use tokio_agent_extension_api::{
    FrontendCapabilities, InteractionRequest, InteractionResponse, ToolEffect, ToolOwner,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub invocation_id: String,
    pub tool_name: String,
    pub owner: ToolOwner,
    pub arguments: Value,
    pub effect: ToolEffect,
    pub cwd: PathBuf,
    pub summary_hint: Option<String>,
    pub frontend: FrontendCapabilities,
}

#[derive(Debug, Clone)]
pub enum ToolGateResult {
    Allow,
    Deny { reason: String },
    RequestInteraction(InteractionRequest),
}

pub trait ToolGate: Send + Sync {
    fn authorize<'a>(
        &'a self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
    ) -> crate::provider::BoxFuture<'a, ToolGateResult>;

    fn respond<'a>(
        &'a self,
        invocation: ToolInvocation,
        response: InteractionResponse,
        cancel: CancellationToken,
    ) -> crate::provider::BoxFuture<'a, ToolGateResult>;
}

#[derive(Clone, Default)]
pub struct InteractionBroker {
    pending: Arc<std::sync::Mutex<std::collections::BTreeMap<String, PendingInteraction>>>,
}

struct PendingInteraction {
    owner: tokio_agent_extension_api::ExtensionId,
    generation: u64,
    sender: tokio::sync::oneshot::Sender<InteractionResponse>,
}

impl InteractionBroker {
    pub fn register(
        &self,
        request: &InteractionRequest,
    ) -> Result<tokio::sync::oneshot::Receiver<InteractionResponse>, String> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.contains_key(request.id.as_str()) {
            return Err("duplicate interaction ID".into());
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        pending.insert(
            request.id.to_string(),
            PendingInteraction {
                owner: request.owner.clone(),
                generation: request.generation,
                sender,
            },
        );
        Ok(receiver)
    }

    pub fn respond(&self, response: InteractionResponse) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let valid = pending.get(response.id.as_str()).is_some_and(|entry| {
            entry.owner == response.owner && entry.generation == response.generation
        });
        if !valid {
            return false;
        }
        pending
            .remove(response.id.as_str())
            .is_some_and(|entry| entry.sender.send(response).is_ok())
    }

    pub fn cancel(&self, id: &tokio_agent_extension_api::InteractionId) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id.as_str())
            .is_some()
    }

    pub fn cancel_all(&self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[derive(Clone, Default)]
pub struct ToolGateSlot {
    state: Arc<RwLock<ToolGateState>>,
    lifecycle: Arc<std::sync::Mutex<CancellationToken>>,
}

#[derive(Clone)]
pub enum ToolGateState {
    Absent,
    Active(Arc<dyn ToolGate>),
    Failed(String),
}

impl Default for ToolGateState {
    fn default() -> Self {
        Self::Absent
    }
}

impl ToolGateSlot {
    #[must_use]
    pub fn new(gate: Option<Arc<dyn ToolGate>>) -> Self {
        Self {
            state: Arc::new(RwLock::new(
                gate.map_or(ToolGateState::Absent, ToolGateState::Active),
            )),
            lifecycle: Arc::new(std::sync::Mutex::new(CancellationToken::new())),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> ToolGateState {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn lifecycle(&self) -> CancellationToken {
        self.lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn transition(&self, state: ToolGateState) {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.cancel();
        *lifecycle = CancellationToken::new();
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }

    pub fn attach(&self, gate: Arc<dyn ToolGate>) {
        self.transition(ToolGateState::Active(gate));
    }
    pub fn detach(&self) {
        self.transition(ToolGateState::Absent);
    }
    pub fn fail(&self, reason: impl Into<String>) {
        self.transition(ToolGateState::Failed(reason.into()));
    }
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
    fn effect(&self) -> ToolEffect {
        ToolEffect::Unknown
    }
    fn owner(&self) -> ToolOwner {
        ToolOwner::BuiltIn
    }
    fn summary(&self, _input: &Value) -> Option<String> {
        None
    }
    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult>;
}
