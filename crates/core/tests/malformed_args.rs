use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use serde_json::value::to_raw_value;
use tokio_util::sync::CancellationToken;

use tokio_agent_core::agent::{AgentEvent, UiCommand};
use tokio_agent_core::event::{Event, StopReason};
use tokio_agent_core::message::{ContentBlock, Message, Role, ToolCallId, ToolOutput};
use tokio_agent_core::permission::{Mode, PermissionEngine};
use tokio_agent_core::provider::{BoxFuture, Capabilities, Provider, ProviderError, Request};
use tokio_agent_core::tool::{Action, PermissionRequest, Tool, ToolCtx, ToolDef, ToolResult};

struct OneShotProvider {
    message: Message,
    served: AtomicUsize,
}

impl Provider for OneShotProvider {
    fn stream<'a>(
        &'a self,
        _req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        let first = self.served.fetch_add(1, Ordering::SeqCst) == 0;
        let event = if first {
            Event::Done {
                stop: StopReason::ToolUse,
                message: self.message.clone(),
            }
        } else {
            Event::Done {
                stop: StopReason::EndTurn,
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::Text {
                        text: "done".to_owned(),
                        meta: tokio_agent_core::message::ProviderMetadata::default(),
                    }],
                    usage: None,
                },
            }
        };
        Box::pin(async move { Ok(futures::stream::iter(vec![event]).boxed()) })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            streaming: true,
            caching: false,
            vision: false,
        }
    }
}

struct CountingTool {
    runs: Arc<AtomicUsize>,
}

impl Tool for CountingTool {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "read".to_owned(),
            description: "counting".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest {
            tool: "read".to_owned(),
            summary: "read".to_owned(),
            action: Action::Read,
        }
    }

    fn run<'a>(&'a self, _input: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolResult::ok("should not run") })
    }
}

#[tokio::test]
async fn malformed_tool_args_become_is_error_result_without_running_the_tool() {
    let malformed = to_raw_value(r#"{"path":"src/ma"#).unwrap();
    let message = Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolCall {
            id: ToolCallId("call_1".to_owned()),
            name: "read".to_owned(),
            args: malformed,
            meta: tokio_agent_core::message::ProviderMetadata::default(),
        }],
        usage: None,
    };

    let runs = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(CountingTool { runs: runs.clone() })];

    let agent = tokio_agent_core::Agent::new(
        OneShotProvider {
            message,
            served: AtomicUsize::new(0),
        },
        tools,
        PermissionEngine::new(Mode::FullAuto),
        tokio_agent_core::ModelConfig {
            model: "m".to_owned(),
            system: "s".to_owned(),
            max_tokens: 256,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );

    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let session = tokio::spawn(agent.run(command_rx, event_tx));
    command_tx
        .send(UiCommand::UserMessage("go".to_owned()))
        .unwrap();
    let mut events = Vec::new();
    loop {
        match event_rx
            .recv()
            .await
            .expect("session ended before TurnDone")
        {
            AgentEvent::TurnDone(result) => {
                result.unwrap();
                break;
            }
            event => events.push(event),
        }
    }
    command_tx.send(UiCommand::Shutdown).unwrap();
    session.await.unwrap();
    let saw_permission_prompt = events
        .iter()
        .any(|event| matches!(event, AgentEvent::PermissionNeeded { .. }));
    assert!(
        !saw_permission_prompt,
        "full-auto must not raise a permission prompt"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolStarted { id, .. } if id.0 == "call_1"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolFinished { id, .. } if id.0 == "call_1"
    )));

    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "a tool with malformed arguments must never run"
    );

    let result = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolFinished { id, result, .. } if id.0 == "call_1" => Some(result),
            _ => None,
        })
        .expect("a tool result must be emitted");

    assert!(
        result.is_error,
        "malformed arguments must yield an is_error result"
    );
    let ToolOutput::Text(text) = &result.output;
    assert!(
        text.contains("read"),
        "the error should name the offending tool, got: {text}"
    );
}
