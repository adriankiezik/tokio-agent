use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::context::PendingToolCall;
use crate::permission::{Outcome, PermissionEngine};
use crate::tool::{Tool, ToolCtx, ToolDef, ToolResult};

pub(crate) struct ToolCallExecutor {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    cwd: PathBuf,
}

impl ToolCallExecutor {
    pub(crate) fn new(tools: Vec<Arc<dyn Tool>>, cwd: PathBuf) -> Self {
        let tools = tools
            .into_iter()
            .map(|tool| (tool.schema().name, tool))
            .collect();
        Self { tools, cwd }
    }

    pub(crate) fn schemas(&self) -> Vec<ToolDef> {
        self.tools.values().map(|tool| tool.schema()).collect()
    }

    pub(crate) async fn execute(
        &mut self,
        calls: &[PendingToolCall],
        permissions: &PermissionEngine,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Vec<ToolResult> {
        let mut results: Vec<Option<ToolResult>> =
            std::iter::repeat_with(|| None).take(calls.len()).collect();
        let mut running = FuturesUnordered::new();

        for (index, call) in calls.iter().enumerate() {
            match self
                .prepare(call, permissions, events, cancel.clone())
                .await
            {
                PreparedTool::Immediate(result) => {
                    let _ = events.send(AgentEvent::ToolFinished {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        result: result.clone(),
                    });
                    results[index] = Some(result);
                }
                PreparedTool::Run { tool, args } => {
                    let id = call.id.clone();
                    let name = call.name.clone();
                    let cwd = self.cwd.clone();
                    let tool_cancel = cancel.clone();
                    running.push(
                        async move {
                            let ctx = ToolCtx {
                                cwd,
                                cancel: tool_cancel.clone(),
                            };
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
                name,
                result: result.clone(),
            });
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
        permissions: &PermissionEngine,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> PreparedTool {
        let _ = events.send(AgentEvent::ToolStarted {
            id: call.id.clone(),
            name: call.name.clone(),
            summary: tool_call_summary(&call.name, &call.raw_args),
        });

        let Some(tool) = self.tools.get(&call.name).cloned() else {
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

        let req = tool.permission(&args);
        let outcome = permissions
            .decide(&req, cancel.clone(), |id, request| {
                events
                    .send(AgentEvent::PermissionNeeded { id, request })
                    .is_ok()
            })
            .await;

        match outcome {
            Outcome::Deny if cancel.is_cancelled() => {
                PreparedTool::Immediate(ToolResult::error("cancelled by user"))
            }
            Outcome::Deny => PreparedTool::Immediate(ToolResult::error("denied by user")),
            Outcome::Run => PreparedTool::Run { tool, args },
        }
    }
}

fn tool_call_summary(name: &str, raw_args: &str) -> String {
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
