pub mod agent;
pub mod context;
pub mod event;
pub mod message;
pub mod provider;
mod sleep;
pub mod tool;
mod tool_execution;

pub use agent::{
    Agent, AgentError, AgentEvent, AgentState, ModelConfig, SessionHookEffect, UiCommand,
};
pub use context::ContextAssembler;
pub use event::Event;
pub use message::{ContentBlock, Message, ProviderMetadata, Role};
pub use provider::{Capabilities, Provider, ProviderError, Request};
pub use tool::{
    FrontendCapabilities, InteractionBroker, InteractionRequest, InteractionResponse, Tool,
    ToolCtx, ToolDef, ToolEffect, ToolGate, ToolGateResult, ToolGateSlot, ToolGateState,
    ToolInvocation, ToolOwner, ToolResult,
};
pub use tool_execution::{DynamicToolCatalog, DynamicToolError};
