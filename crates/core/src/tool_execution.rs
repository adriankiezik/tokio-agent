use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::context::PendingToolCall;
use crate::tool::{
    FrontendCapabilities, InteractionBroker, Tool, ToolCtx, ToolDef, ToolGateResult, ToolGateSlot,
    ToolGateState, ToolInvocation, ToolResult,
};

#[derive(Clone, Default)]
pub struct DynamicToolCatalog {
    tools: Arc<RwLock<BTreeMap<String, DynamicTool>>>,
    reserved: Arc<BTreeSet<String>>,
}

#[derive(Clone)]
struct DynamicTool {
    owner: String,
    tool: Arc<dyn Tool>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DynamicToolError {
    #[error("tool name `{0}` is already registered")]
    Collision(String),
}

impl DynamicToolCatalog {
    pub fn register(
        &self,
        owner: impl Into<String>,
        tool: Arc<dyn Tool>,
    ) -> Result<(), DynamicToolError> {
        let name = tool.schema().name;
        let mut tools = self
            .tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.reserved.contains(&name) || tools.contains_key(&name) {
            return Err(DynamicToolError::Collision(name));
        }
        tools.insert(
            name,
            DynamicTool {
                owner: owner.into(),
                tool,
            },
        );
        Ok(())
    }

    pub fn unregister(&self, owner: &str, name: &str) -> bool {
        let mut tools = self
            .tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tools.get(name).is_some_and(|tool| tool.owner == owner) {
            tools.remove(name);
            true
        } else {
            false
        }
    }

    pub fn disable(&self, owner: &str) {
        self.tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, tool| tool.owner != owner);
    }

    fn snapshot(&self) -> BTreeMap<String, Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(name, tool)| (name.clone(), tool.tool.clone()))
            .collect()
    }
}

pub(crate) struct ToolCallExecutor {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    dynamic: DynamicToolCatalog,
    lifecycle: Option<Arc<dyn Fn(String, bool) + Send + Sync>>,
    cwd: PathBuf,
    gate: ToolGateSlot,
    interactions: InteractionBroker,
    frontend: FrontendCapabilities,
}

impl ToolCallExecutor {
    pub(crate) fn new(
        tools: Vec<Arc<dyn Tool>>,
        cwd: PathBuf,
        gate: ToolGateSlot,
        interactions: InteractionBroker,
        frontend: FrontendCapabilities,
    ) -> Self {
        let tools: BTreeMap<_, _> = tools
            .into_iter()
            .map(|tool| (tool.schema().name, tool))
            .collect();
        let dynamic = DynamicToolCatalog {
            reserved: Arc::new(tools.keys().cloned().collect()),
            ..DynamicToolCatalog::default()
        };
        Self {
            tools,
            dynamic,
            lifecycle: None,
            cwd,
            gate,
            interactions,
            frontend,
        }
    }

    pub(crate) fn with_lifecycle_callback(
        mut self,
        callback: Arc<dyn Fn(String, bool) + Send + Sync>,
    ) -> Self {
        self.lifecycle = Some(callback);
        self
    }

    pub(crate) fn set_frontend_capabilities(&mut self, capabilities: FrontendCapabilities) {
        self.frontend = capabilities;
    }

    pub(crate) fn dynamic_catalog(&self) -> DynamicToolCatalog {
        self.dynamic.clone()
    }

    pub(crate) fn schemas(&self) -> Vec<ToolDef> {
        let mut schemas: BTreeMap<_, _> = self
            .tools
            .iter()
            .map(|(name, tool)| (name.clone(), tool.schema()))
            .collect();
        schemas.extend(
            self.dynamic
                .snapshot()
                .into_iter()
                .map(|(name, tool)| (name, tool.schema())),
        );
        schemas.into_values().collect()
    }

    pub(crate) async fn execute(
        &mut self,
        calls: &[PendingToolCall],
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Vec<ToolResult> {
        let mut results: Vec<Option<ToolResult>> =
            std::iter::repeat_with(|| None).take(calls.len()).collect();
        let mut running = FuturesUnordered::new();

        for (index, call) in calls.iter().enumerate() {
            match self.prepare(call, events, cancel.clone()).await {
                PreparedTool::Immediate(result) => {
                    let _ = events.send(AgentEvent::ToolFinished {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        result: result.clone(),
                    });
                    if let Some(lifecycle) = &self.lifecycle {
                        lifecycle(call.name.clone(), result.is_error);
                    }
                    results[index] = Some(result);
                }
                PreparedTool::Run { tool, args } => {
                    let id = call.id.clone();
                    let name = call.name.clone();
                    let cwd = self.cwd.clone();
                    let tool_cancel = cancel.clone();
                    let progress_events = events.clone();
                    let progress_id = id.clone();
                    running.push(
                        async move {
                            let progress: crate::tool::ToolProgress = Arc::new(move |text| {
                                let _ = progress_events.send(AgentEvent::ToolOutputDelta {
                                    id: progress_id.clone(),
                                    text,
                                });
                            });
                            let ctx = ToolCtx::new(cwd, tool_cancel.clone())
                                .with_progress(progress);
                            let result = tokio::select! {
                                result = tool.run(args, &ctx) => result,
                                () = tool_cancel.cancelled() => ToolResult::error("cancelled by user"),
                            };
                            (index, id, name, result)
                        }
                        .boxed(),
                    );
                }
            }
        }

        while let Some((index, id, name, result)) = running.next().await {
            let _ = events.send(AgentEvent::ToolFinished {
                id,
                name: name.clone(),
                result: result.clone(),
            });
            if let Some(lifecycle) = &self.lifecycle {
                lifecycle(name, result.is_error);
            }
            results[index] = Some(result);
        }

        results
            .into_iter()
            .map(|result| result.unwrap_or_else(|| ToolResult::error("cancelled by user")))
            .collect()
    }

    async fn prepare(
        &mut self,
        call: &PendingToolCall,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> PreparedTool {
        let _ = events.send(AgentEvent::ToolStarted {
            id: call.id.clone(),
            name: call.name.clone(),
            summary: tool_call_summary(&call.name, &call.raw_args),
        });

        let tool = self
            .tools
            .get(&call.name)
            .cloned()
            .or_else(|| self.dynamic.snapshot().remove(&call.name));
        let Some(tool) = tool else {
            return PreparedTool::Immediate(ToolResult::error(format!(
                "unknown tool: {}",
                call.name
            )));
        };

        let args = match serde_json::from_str::<serde_json::Value>(&call.raw_args) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                return PreparedTool::Immediate(ToolResult::error(format!(
                    "tool `{}` was called with arguments that are not a JSON object",
                    call.name
                )));
            }
            Err(err) => {
                return PreparedTool::Immediate(ToolResult::error(format!(
                    "tool `{}` was called with malformed JSON arguments: {err}",
                    call.name
                )));
            }
        };

        let invocation = ToolInvocation {
            invocation_id: call.id.0.clone(),
            tool_name: call.name.clone(),
            owner: tool.owner(),
            arguments: args.clone(),
            effect: tool.effect(),
            cwd: self.cwd.clone(),
            summary_hint: tool
                .summary(&args)
                .or_else(|| Some(tool_call_summary(&call.name, &call.raw_args))),
            frontend: self.frontend.clone(),
        };

        let decision = match self.gate.snapshot() {
            ToolGateState::Absent => ToolGateResult::Allow,
            ToolGateState::Failed(reason) => ToolGateResult::Deny {
                reason: format!("tool gate unavailable: {reason}"),
            },
            ToolGateState::Active(gate) => {
                let lifecycle = self.gate.lifecycle();
                let first = tokio::select! {
                    result = gate.authorize(invocation.clone(), cancel.clone()) => result,
                    () = lifecycle.cancelled() => ToolGateResult::Deny { reason: "tool gate changed while authorization was pending".into() },
                };
                match first {
                    ToolGateResult::RequestInteraction(request) => {
                        let receiver = match self.interactions.register(&request) {
                            Ok(receiver) => receiver,
                            Err(reason) => {
                                return PreparedTool::Immediate(ToolResult::error(format!(
                                    "tool gate protocol error: {reason}"
                                )));
                            }
                        };
                        if events
                            .send(AgentEvent::InteractionRequested(request.clone()))
                            .is_err()
                        {
                            self.interactions.cancel(&request.id);
                            ToolGateResult::Deny {
                                reason: "frontend unavailable".into(),
                            }
                        } else {
                            let response = tokio::select! {
                                response = receiver => response.ok(),
                                () = cancel.cancelled() => None,
                                () = lifecycle.cancelled() => None,
                            };
                            self.interactions.cancel(&request.id);
                            match response {
                                Some(response) => {
                                    gate.respond(invocation.clone(), response, cancel.clone())
                                        .await
                                }
                                None => {
                                    let _ = events
                                        .send(AgentEvent::InteractionCancelled { id: request.id });
                                    ToolGateResult::Deny {
                                        reason: "cancelled by user".into(),
                                    }
                                }
                            }
                        }
                    }
                    result => result,
                }
            }
        };

        match decision {
            ToolGateResult::Allow => PreparedTool::Run { tool, args },
            ToolGateResult::Deny { .. } if cancel.is_cancelled() => {
                PreparedTool::Immediate(ToolResult::error("cancelled by user"))
            }
            ToolGateResult::Deny { reason } => {
                PreparedTool::Immediate(ToolResult::error(format!("denied: {reason}")))
            }
            ToolGateResult::RequestInteraction(_) => PreparedTool::Immediate(ToolResult::error(
                "tool gate protocol error: interaction response did not produce a final decision",
            )),
        }
    }
}

fn tool_call_summary(name: &str, raw_args: &str) -> String {
    if matches!(name, "bash_wait" | "bash_kill") {
        let Ok(args) = serde_json::from_str::<serde_json::Value>(raw_args) else {
            return name.to_owned();
        };
        return args
            .get("process_id")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| name.to_owned(), |id| format!("process {id}"));
    }
    let argument = match name {
        "bash" => "command",
        "read" | "write" | "edit" | "multi_edit" => "path",
        "glob" | "grep" => "pattern",
        _ => return name.to_owned(),
    };
    let Ok(args) = serde_json::from_str::<serde_json::Value>(raw_args) else {
        return name.to_owned();
    };
    let Some(value) = args.get(argument).and_then(serde_json::Value::as_str) else {
        return name.to_owned();
    };
    compact_tool_argument(value).unwrap_or_else(|| name.to_owned())
}

fn compact_tool_argument(value: &str) -> Option<String> {
    const MAX_CHARS: usize = 96;

    let value = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if value.trim().is_empty() {
        return None;
    }
    if value.chars().count() <= MAX_CHARS {
        return Some(value);
    }
    Some(format!(
        "{}…",
        value.chars().take(MAX_CHARS - 1).collect::<String>()
    ))
}

enum PreparedTool {
    Immediate(ToolResult),
    Run {
        tool: Arc<dyn Tool>,
        args: serde_json::Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_tool_summaries_use_the_relevant_argument() {
        assert_eq!(
            tool_call_summary("bash", r#"{"command":"ls -la"}"#),
            "ls -la"
        );
        assert_eq!(
            tool_call_summary("read", r#"{"path":"src/main.rs"}"#),
            "src/main.rs"
        );
        assert_eq!(
            tool_call_summary("edit", r#"{"path":"src/lib.rs","old_text":"secret"}"#),
            "src/lib.rs"
        );
        assert_eq!(
            tool_call_summary("grep", r#"{"pattern":"fn main"}"#),
            "fn main"
        );
    }

    #[test]
    fn tool_summaries_fall_back_without_exposing_unknown_arguments() {
        assert_eq!(
            tool_call_summary("custom", r#"{"token":"secret"}"#),
            "custom"
        );
        assert_eq!(tool_call_summary("bash", "not json"), "bash");
    }

    #[test]
    fn tool_summaries_are_single_line_and_bounded() {
        assert_eq!(
            tool_call_summary("bash", r#"{"command":"printf foo\nprintf bar"}"#),
            "printf foo printf bar"
        );
        let command = "x".repeat(120);
        let args = serde_json::json!({ "command": command }).to_string();
        let summary = tool_call_summary("bash", &args);
        assert_eq!(summary.chars().count(), 96);
        assert!(summary.ends_with('…'));
    }
}
