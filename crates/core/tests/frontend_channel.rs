use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use serde_json::value::to_raw_value;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

use tokio_agent_core::agent::{Agent, AgentEvent, ModelConfig, UiCommand};
use tokio_agent_core::event::{Event, StopReason};
use tokio_agent_core::message::{ContentBlock, Message, Role, ToolCallId};
use tokio_agent_core::message::{ToolOutput, Usage};
use tokio_agent_core::permission::{Decision, Mode, PermissionEngine};
use tokio_agent_core::provider::{BoxFuture, Capabilities, Provider, ProviderError, Request};
use tokio_agent_core::tool::{Action, PermissionRequest, Tool, ToolCtx, ToolDef, ToolResult};

struct ScriptedProvider;

impl Provider for ScriptedProvider {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        let resolved = req.messages.last().is_some_and(|m| m.role == Role::Tool);

        let events = if resolved {
            vec![Event::Done {
                stop: StopReason::EndTurn,
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::Text {
                        text: "done".to_owned(),
                        meta: tokio_agent_core::message::ProviderMetadata::default(),
                    }],
                    usage: None,
                },
            }]
        } else {
            vec![
                Event::TextDelta {
                    text: "working".to_owned(),
                },
                Event::Done {
                    stop: StopReason::ToolUse,
                    message: Message {
                        role: Role::Assistant,
                        blocks: vec![ContentBlock::ToolCall {
                            id: ToolCallId("call_1".to_owned()),
                            name: "bash".to_owned(),
                            args: to_raw_value(&serde_json::json!({"cmd": "ls"})).unwrap(),
                            meta: tokio_agent_core::message::ProviderMetadata::default(),
                        }],
                        usage: None,
                    },
                },
            ]
        };

        Box::pin(async move { Ok(futures::stream::iter(events).boxed()) })
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

struct HostedToolProvider;

impl Provider for HostedToolProvider {
    fn stream<'a>(
        &'a self,
        _req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        let events = vec![
            Event::HostedToolCallStart {
                id: ToolCallId("ws_1".to_owned()),
                name: "web_search".to_owned(),
                summary: "searching the web".to_owned(),
            },
            Event::HostedToolCallEnd {
                id: ToolCallId("ws_1".to_owned()),
                name: "web_search".to_owned(),
                output: "searched: latest models".to_owned(),
                is_error: false,
            },
            Event::Done {
                stop: StopReason::EndTurn,
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::Text {
                        text: "verified answer".to_owned(),
                        meta: tokio_agent_core::message::ProviderMetadata::default(),
                    }],
                    usage: None,
                },
            },
        ];
        Box::pin(async move { Ok(futures::stream::iter(events).boxed()) })
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

struct RunningTool {
    runs: Arc<AtomicUsize>,
}

impl Tool for RunningTool {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "bash".to_owned(),
            description: "run a shell command".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest {
            tool: "bash".to_owned(),
            summary: "run ls".to_owned(),
            action: Action::Execute,
        }
    }

    fn run<'a>(&'a self, _input: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolResult::ok("total 0") })
    }
}

async fn run_turn_collecting(
    commands: &tokio::sync::mpsc::UnboundedSender<UiCommand>,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    input: &str,
    answer: Decision,
) -> Vec<&'static str> {
    commands
        .send(UiCommand::UserMessage(input.to_owned()))
        .unwrap();

    let mut seq = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            AgentEvent::AutomaticTurnStarted(_) => seq.push("automatic"),
            AgentEvent::TextDelta(_) => seq.push("text"),
            AgentEvent::ThinkingDelta(_) => seq.push("thinking"),
            AgentEvent::ToolStarted { .. } => seq.push("tool_start"),
            AgentEvent::ToolFinished { .. } => seq.push("tool_result"),
            AgentEvent::TurnUsage(_) => seq.push("usage"),
            AgentEvent::RequestUsage(_) => {}
            AgentEvent::PermissionNeeded { id, .. } => {
                seq.push("permission");
                commands
                    .send(UiCommand::Approve {
                        id,
                        decision: answer,
                    })
                    .unwrap();
            }
            AgentEvent::TurnDone(result) => {
                assert_eq!(
                    result.unwrap(),
                    StopReason::EndTurn,
                    "turn should complete normally"
                );
                seq.push("done");
                break;
            }
        }
    }
    seq
}

async fn run_single_turn<P: Provider + 'static>(
    agent: Agent<P>,
    input: &str,
) -> (
    Result<StopReason, tokio_agent_core::AgentError>,
    Vec<AgentEvent>,
) {
    let (command_tx, command_rx) = unbounded_channel();
    let (event_tx, mut event_rx) = unbounded_channel();
    let session = tokio::spawn(agent.run(command_rx, event_tx));
    command_tx
        .send(UiCommand::UserMessage(input.to_owned()))
        .unwrap();
    let mut events = Vec::new();
    let result = loop {
        match event_rx
            .recv()
            .await
            .expect("session ended before TurnDone")
        {
            AgentEvent::TurnDone(result) => break result,
            event => events.push(event),
        }
    };
    command_tx.send(UiCommand::Shutdown).unwrap();
    session.await.unwrap();
    (result, events)
}

#[tokio::test]
async fn hosted_tools_are_visible_without_entering_the_local_tool_loop() {
    let agent = Agent::new(
        HostedToolProvider,
        vec![],
        PermissionEngine::new(Mode::Suggest),
        ModelConfig {
            model: "test".to_owned(),
            system: String::new(),
            max_tokens: 100,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );

    let (result, events) = run_single_turn(agent, "latest models").await;
    assert_eq!(result.unwrap(), StopReason::EndTurn);
    assert!(matches!(
        &events[0],
        AgentEvent::ToolStarted { id, name, .. } if id.0 == "ws_1" && name == "web_search"
    ));
    assert!(matches!(
        &events[1],
        AgentEvent::ToolFinished { id, result, .. }
            if id.0 == "ws_1" && !result.is_error
    ));
}

#[tokio::test]
async fn permission_prompt_flows_over_the_channel_and_allow_always_is_remembered() {
    let runs = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(RunningTool { runs: runs.clone() })];

    let agent = Agent::new(
        ScriptedProvider,
        tools,
        PermissionEngine::new(Mode::Suggest),
        ModelConfig {
            model: "m".to_owned(),
            system: "s".to_owned(),
            max_tokens: 256,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );
    let (command_tx, command_rx) = unbounded_channel();
    let (event_tx, mut event_rx) = unbounded_channel();
    let session = tokio::spawn(agent.run(command_rx, event_tx));

    let first =
        run_turn_collecting(&command_tx, &mut event_rx, "run ls", Decision::AllowAlways).await;
    assert_eq!(
        first,
        vec!["text", "tool_start", "permission", "tool_result", "done"],
        "first turn must prompt for permission before running the tool"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the tool must have run once"
    );

    let second =
        run_turn_collecting(&command_tx, &mut event_rx, "run ls again", Decision::Deny).await;
    assert_eq!(
        second,
        vec!["text", "tool_start", "tool_result", "done"],
        "an allow-always decision must suppress the second permission prompt"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the remembered allow-always must let the tool run again without asking"
    );
    command_tx.send(UiCommand::Shutdown).unwrap();
    session.await.unwrap();
}

struct InterruptingProvider {
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Provider for InterruptingProvider {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        self.requests.lock().unwrap().push(req.clone());
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let message = if call == 0 {
                cancel.cancelled().await;
                Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::ToolCall {
                        id: ToolCallId("interrupted_call".to_owned()),
                        name: "bash".to_owned(),
                        args: to_raw_value(&serde_json::json!({"cmd": "sleep 10"})).unwrap(),
                        meta: tokio_agent_core::message::ProviderMetadata::default(),
                    }],
                    usage: None,
                }
            } else {
                Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::Text {
                        text: "recovered".to_owned(),
                        meta: tokio_agent_core::message::ProviderMetadata::default(),
                    }],
                    usage: None,
                }
            };
            let stop = if call == 0 {
                StopReason::Interrupted
            } else {
                StopReason::EndTurn
            };
            Ok(futures::stream::once(async move { Event::Done { stop, message } }).boxed())
        })
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

#[tokio::test]
async fn interrupt_closes_dangling_calls_and_the_next_turn_gets_a_fresh_token() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        InterruptingProvider {
            calls: AtomicUsize::new(0),
            requests: requests.clone(),
        },
        Vec::new(),
        PermissionEngine::new(Mode::Suggest),
        ModelConfig {
            model: "m".into(),
            system: "s".into(),
            max_tokens: 256,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );
    let (command_tx, command_rx) = unbounded_channel();
    let (event_tx, mut event_rx) = unbounded_channel();
    let session = tokio::spawn(agent.run(command_rx, event_tx));

    command_tx
        .send(UiCommand::UserMessage("first".into()))
        .unwrap();
    tokio::task::yield_now().await;
    command_tx.send(UiCommand::Interrupt).unwrap();
    while !matches!(
        event_rx.recv().await,
        Some(AgentEvent::TurnDone(Ok(StopReason::Interrupted)))
    ) {}

    command_tx
        .send(UiCommand::UserMessage("second".into()))
        .unwrap();
    while !matches!(event_rx.recv().await, Some(AgentEvent::TurnDone(_))) {}
    command_tx.send(UiCommand::Shutdown).unwrap();
    session.await.unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "a new turn must stream after interruption"
    );
    assert!(requests[1].messages.iter().any(|message| message.blocks.iter().any(|block| {
        matches!(block, ContentBlock::ToolResult { call, is_error: true, .. } if call.0 == "interrupted_call")
    })), "the interrupted transcript must close every model-issued tool call");
}

struct ParallelProvider {
    calls: AtomicUsize,
    followup: Arc<Mutex<Option<Request>>>,
}

impl Provider for ParallelProvider {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call > 0 {
            *self.followup.lock().unwrap() = Some(req.clone());
        }
        let events = if call == 0 {
            vec![
                Event::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Usage::default()
                }),
                Event::Done {
                    stop: StopReason::ToolUse,
                    message: Message {
                        role: Role::Assistant,
                        blocks: ["slow", "fast"]
                            .into_iter()
                            .map(|name| ContentBlock::ToolCall {
                                id: ToolCallId(format!("{name}_call")),
                                name: name.to_owned(),
                                args: to_raw_value(&serde_json::json!({})).unwrap(),
                                meta: tokio_agent_core::message::ProviderMetadata::default(),
                            })
                            .collect(),
                        usage: None,
                    },
                },
            ]
        } else {
            vec![
                Event::Usage(Usage {
                    input_tokens: 20,
                    output_tokens: 3,
                    ..Usage::default()
                }),
                Event::Done {
                    stop: StopReason::EndTurn,
                    message: Message {
                        role: Role::Assistant,
                        blocks: vec![],
                        usage: None,
                    },
                },
            ]
        };
        Box::pin(async move { Ok(futures::stream::iter(events).boxed()) })
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

struct DelayedTool {
    name: &'static str,
    delay_ms: u64,
}

impl Tool for DelayedTool {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: self.name.to_owned(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        PermissionRequest {
            tool: self.name.to_owned(),
            summary: self.name.to_owned(),
            action: Action::Execute,
        }
    }

    fn run<'a>(&'a self, _input: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            ToolResult::ok(self.name)
        })
    }
}

#[tokio::test]
async fn tools_finish_concurrently_but_context_and_usage_remain_deterministic() {
    let followup = Arc::new(Mutex::new(None));
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(DelayedTool {
            name: "slow",
            delay_ms: 50,
        }),
        Arc::new(DelayedTool {
            name: "fast",
            delay_ms: 1,
        }),
    ];
    let agent = Agent::new(
        ParallelProvider {
            calls: AtomicUsize::new(0),
            followup: followup.clone(),
        },
        tools,
        PermissionEngine::new(Mode::FullAuto),
        ModelConfig {
            model: "m".into(),
            system: "s".into(),
            max_tokens: 256,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );
    let (command_tx, command_rx) = unbounded_channel();
    let (event_tx, mut event_rx) = unbounded_channel();
    let session = tokio::spawn(agent.run(command_rx, event_tx));
    command_tx
        .send(UiCommand::UserMessage("parallel".into()))
        .unwrap();

    let mut finished = Vec::new();
    let mut usage = Vec::new();
    loop {
        match event_rx.recv().await.unwrap() {
            AgentEvent::ToolFinished { name, .. } => finished.push(name),
            AgentEvent::TurnUsage(value) => usage.push(value),
            AgentEvent::TurnDone(result) => {
                assert_eq!(result.unwrap(), StopReason::EndTurn);
                break;
            }
            _ => {}
        }
    }
    command_tx.send(UiCommand::Shutdown).unwrap();
    session.await.unwrap();

    assert_eq!(
        finished,
        ["fast", "slow"],
        "completion events must reflect actual completion order"
    );
    assert_eq!(
        usage.last().copied(),
        Some(Usage {
            input_tokens: 30,
            output_tokens: 5,
            ..Usage::default()
        })
    );
    let followup = followup.lock().unwrap();
    let tool_message = followup
        .as_ref()
        .unwrap()
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    let calls: Vec<_> = tool_message
        .blocks
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                call,
                output: ToolOutput::Text(_),
                ..
            } => call.0.as_str(),
            _ => panic!("expected only tool results"),
        })
        .collect();
    assert_eq!(
        calls,
        ["slow_call", "fast_call"],
        "context must preserve model-issued order"
    );
}

struct UnexpectedInterruptProvider {
    calls: AtomicUsize,
    followup: Arc<Mutex<Option<Request>>>,
}

impl Provider for UnexpectedInterruptProvider {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if !first {
            *self.followup.lock().unwrap() = Some(req.clone());
        }
        let (stop, blocks) = if first {
            (
                StopReason::Interrupted,
                vec![ContentBlock::ToolCall {
                    id: ToolCallId("partial_call".into()),
                    name: "dangerous".into(),
                    args: to_raw_value(&serde_json::json!({})).unwrap(),
                    meta: tokio_agent_core::message::ProviderMetadata::default(),
                }],
            )
        } else {
            (StopReason::EndTurn, vec![])
        };
        Box::pin(async move {
            Ok(futures::stream::once(async move {
                Event::Done {
                    stop,
                    message: Message {
                        role: Role::Assistant,
                        blocks,
                        usage: None,
                    },
                }
            })
            .boxed())
        })
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

#[tokio::test]
async fn unexpected_interrupted_done_preserves_partial_message_without_executing_tools() {
    let followup = Arc::new(Mutex::new(None));
    let agent = Agent::new(
        UnexpectedInterruptProvider {
            calls: AtomicUsize::new(0),
            followup: followup.clone(),
        },
        Vec::new(),
        PermissionEngine::new(Mode::FullAuto),
        ModelConfig {
            model: "m".into(),
            system: "s".into(),
            max_tokens: 256,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );
    let (command_tx, command_rx) = unbounded_channel();
    let (event_tx, mut event_rx) = unbounded_channel();
    let session = tokio::spawn(agent.run(command_rx, event_tx));
    command_tx
        .send(UiCommand::UserMessage("first".into()))
        .unwrap();
    loop {
        match event_rx.recv().await.unwrap() {
            AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. } => {
                panic!("partial tool calls must never execute")
            }
            AgentEvent::TurnDone(result) => {
                assert!(matches!(
                    result,
                    Err(tokio_agent_core::AgentError::UnexpectedInterrupt)
                ));
                break;
            }
            _ => {}
        }
    }

    command_tx
        .send(UiCommand::UserMessage("recover".into()))
        .unwrap();
    while !matches!(
        event_rx.recv().await,
        Some(AgentEvent::TurnDone(Ok(StopReason::EndTurn)))
    ) {}
    command_tx.send(UiCommand::Shutdown).unwrap();
    session.await.unwrap();

    let followup = followup.lock().unwrap();
    let messages = &followup.as_ref().unwrap().messages;
    assert!(messages.iter().any(|message| message.blocks.iter().any(|block| {
        matches!(block, ContentBlock::ToolResult { call, is_error: true, .. } if call.0 == "partial_call")
    })), "partial assistant metadata must remain resendable with its tool call closed");
}

struct RetryingProvider {
    calls: AtomicUsize,
    fingerprints: Arc<Mutex<Vec<String>>>,
}

impl Provider for RetryingProvider {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        self.fingerprints.lock().unwrap().push(format!(
            "{}\n{}\n{}\n{}",
            req.model,
            req.system,
            req.max_tokens,
            serde_json::to_string(&req.messages).unwrap()
        ));
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        let event = if attempt < 2 {
            Event::Failed {
                retryable: true,
                error: "temporary transport failure".into(),
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![],
                    usage: None,
                },
            }
        } else {
            Event::Done {
                stop: StopReason::EndTurn,
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![],
                    usage: None,
                },
            }
        };
        Box::pin(async move { Ok(futures::stream::once(async move { event }).boxed()) })
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

#[tokio::test]
async fn empty_retryable_transport_failures_retry_the_identical_request() {
    let fingerprints = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        RetryingProvider {
            calls: AtomicUsize::new(0),
            fingerprints: fingerprints.clone(),
        },
        Vec::new(),
        PermissionEngine::new(Mode::FullAuto),
        ModelConfig {
            model: "m".into(),
            system: "s".into(),
            max_tokens: 256,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );
    let (result, _) = run_single_turn(agent, "retry").await;
    assert_eq!(result.unwrap(), StopReason::EndTurn);

    let fingerprints = fingerprints.lock().unwrap();
    assert_eq!(fingerprints.len(), 3);
    assert!(
        fingerprints.windows(2).all(|pair| pair[0] == pair[1]),
        "retries must resend the identical request"
    );
}

struct DirectRetryProvider {
    calls: AtomicUsize,
    fingerprints: Arc<Mutex<Vec<String>>>,
}

impl Provider for DirectRetryProvider {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        self.fingerprints
            .lock()
            .unwrap()
            .push(serde_json::to_string(&req.messages).unwrap());
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if attempt < 2 {
                Err(ProviderError::retryable("HTTP connection failed"))
            } else {
                Ok(futures::stream::once(async {
                    Event::Done {
                        stop: StopReason::EndTurn,
                        message: Message {
                            role: Role::Assistant,
                            blocks: vec![],
                            usage: None,
                        },
                    }
                })
                .boxed())
            }
        })
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

#[tokio::test]
async fn direct_retryable_stream_errors_are_bounded_and_resend_the_identical_request() {
    let fingerprints = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        DirectRetryProvider {
            calls: AtomicUsize::new(0),
            fingerprints: fingerprints.clone(),
        },
        Vec::new(),
        PermissionEngine::new(Mode::FullAuto),
        ModelConfig {
            model: "m".into(),
            system: "s".into(),
            max_tokens: 256,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );
    let (result, _) = run_single_turn(agent, "retry direct").await;
    assert_eq!(result.unwrap(), StopReason::EndTurn);
    let fingerprints = fingerprints.lock().unwrap();
    assert_eq!(fingerprints.len(), 3);
    assert!(fingerprints.windows(2).all(|pair| pair[0] == pair[1]));
}

struct PartialFailureProvider;

impl Provider for PartialFailureProvider {
    fn stream<'a>(
        &'a self,
        _req: &'a Request,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        let event = Event::Failed {
            retryable: true,
            error: "connection lost after partial response".into(),
            message: Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: ToolCallId("failed_partial_call".into()),
                    name: "must_not_run".into(),
                    args: to_raw_value(&serde_json::json!({})).unwrap(),
                    meta: tokio_agent_core::message::ProviderMetadata::default(),
                }],
                usage: None,
            },
        };
        Box::pin(async move { Ok(futures::stream::once(async move { event }).boxed()) })
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

#[tokio::test]
async fn partial_transport_failure_is_preserved_closed_and_never_retried_or_executed() {
    let agent = Agent::new(
        PartialFailureProvider,
        Vec::new(),
        PermissionEngine::new(Mode::FullAuto),
        ModelConfig {
            model: "m".into(),
            system: "s".into(),
            max_tokens: 256,
            reasoning_effort: None,
        },
        std::env::temp_dir(),
    );
    let (result, events) = run_single_turn(agent, "partial").await;
    assert!(
        matches!(result, Err(tokio_agent_core::AgentError::Provider(error)) if error.retryable)
    );
    assert!(!events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. }
        )
    }));
}
