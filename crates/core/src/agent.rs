use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::context::ContextAssembler;
use crate::event::{Event, StopReason};
use crate::message::{Message, Usage};
use crate::permission::{Decision, Mode, PermissionEngine, PermissionId};
use crate::provider::{Provider, ProviderError, Request};
use crate::tool::{PermissionRequest, Tool, ToolResult};
use crate::tool_execution::ToolCallExecutor;

#[derive(Debug)]
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolStarted {
        id: crate::message::ToolCallId,
        name: String,
        summary: String,
    },
    ToolFinished {
        id: crate::message::ToolCallId,
        name: String,
        result: ToolResult,
    },
    TurnUsage(Usage),
    RequestUsage(Usage),
    PermissionNeeded {
        id: PermissionId,
        request: PermissionRequest,
    },
    TurnDone(Result<StopReason, AgentError>),
}

#[derive(Debug)]
pub enum UiCommand {
    UserMessage(String),
    Clear,
    SetPermissionMode(Mode),
    Interrupt,
    Approve {
        id: PermissionId,
        decision: Decision,
    },
    Shutdown,
}

const MAX_ITERATIONS: u32 = 100;

#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("exceeded the maximum of {0} tool iterations in a single turn")]
    IterationLimit(u32),
    #[error("provider interrupted the response before the turn completed")]
    UnexpectedInterrupt,
}

pub struct ModelConfig {
    pub model: String,
    pub system: String,
    pub max_tokens: u32,
    pub reasoning_effort: Option<String>,
}

pub struct Agent<P: Provider> {
    provider: P,
    tools: ToolCallExecutor,
    permissions: PermissionEngine,
    context: ContextAssembler,
    reasoning_effort_supported: bool,
    cwd: PathBuf,
    context_window: Option<u64>,
}

impl<P: Provider> Agent<P> {
    pub fn new(
        provider: P,
        tools: Vec<Arc<dyn Tool>>,
        gate: PermissionEngine,
        model: ModelConfig,
        cwd: PathBuf,
    ) -> Self {
        let ModelConfig {
            model,
            system,
            max_tokens,
            reasoning_effort,
        } = model;
        Self {
            provider,
            tools: ToolCallExecutor::new(tools, cwd.clone()),
            permissions: gate,
            context: ContextAssembler::new(model, system, max_tokens)
                .with_reasoning_effort(reasoning_effort),
            reasoning_effort_supported: true,
            cwd,
            context_window: None,
        }
    }

    #[must_use]
    pub fn with_reasoning_effort_support(mut self, supported: bool) -> Self {
        self.reasoning_effort_supported = supported;
        self
    }

    #[must_use]
    pub fn with_context_window(mut self, context_window: Option<u64>) -> Self {
        self.context_window = context_window;
        self
    }

    pub fn model(&self) -> &str {
        self.context.model()
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        if self.reasoning_effort_supported {
            self.context.reasoning_effort()
        } else {
            None
        }
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    pub fn permission_mode(&self) -> Mode {
        self.permissions.mode()
    }

    pub async fn run(
        mut self,
        mut commands: UnboundedReceiver<UiCommand>,
        events: UnboundedSender<AgentEvent>,
    ) {
        let mut queued = VecDeque::new();
        loop {
            let input = match queued.pop_front() {
                Some(input) => input,
                None => loop {
                    match commands.recv().await {
                        Some(UiCommand::UserMessage(input)) => break input,
                        Some(UiCommand::Clear) => self.context.clear(),
                        Some(UiCommand::SetPermissionMode(mode)) => {
                            self.permissions.set_mode(mode);
                        }
                        Some(UiCommand::Approve { id, decision }) => {
                            self.permissions.resolve(id, decision);
                        }
                        Some(UiCommand::Interrupt) => {}
                        Some(UiCommand::Shutdown) | None => return,
                    }
                },
            };

            let cancel = CancellationToken::new();
            let permissions = self.permissions.clone();
            let result = {
                let turn = self.drive(input, &events, cancel.clone());
                tokio::pin!(turn);
                loop {
                    tokio::select! {
                        result = &mut turn => break result,
                        command = commands.recv() => match command {
                            Some(UiCommand::Interrupt) => cancel.cancel(),
                            Some(UiCommand::Approve { id, decision }) => permissions.resolve(id, decision),
                            Some(UiCommand::UserMessage(input)) => queued.push_back(input),
                            Some(UiCommand::SetPermissionMode(mode)) => permissions.set_mode(mode),
                            Some(UiCommand::Clear) => {}
                            Some(UiCommand::Shutdown) | None => {
                                cancel.cancel();
                                let _ = (&mut turn).await;
                                return;
                            }
                        }
                    }
                }
            };
            let _ = events.send(AgentEvent::TurnDone(result));
        }
    }

    async fn drive(
        &mut self,
        user_input: String,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<StopReason, AgentError> {
        let mut req = self.context.begin_turn(user_input, self.tools.schemas());
        let mut turn_usage = Usage::default();

        for _ in 0..MAX_ITERATIONS {
            if cancel.is_cancelled() {
                self.context.close_open_tool_calls();
                return Ok(StopReason::Interrupted);
            }

            let (assistant, request_usage, terminal) = self
                .stream_turn(&req, events, cancel.clone(), turn_usage)
                .await?;
            turn_usage = add_usage(turn_usage, request_usage);
            let calls = assistant
                .map(|message| self.context.accept_assistant(message))
                .unwrap_or_default();
            let stop = match terminal {
                Ok(stop) => stop,
                Err(error) => {
                    self.context.close_open_tool_calls();
                    return Err(AgentError::Provider(error));
                }
            };

            if stop == StopReason::Interrupted {
                self.context.close_open_tool_calls();
                return if cancel.is_cancelled() {
                    Ok(StopReason::Interrupted)
                } else {
                    Err(AgentError::UnexpectedInterrupt)
                };
            }

            if cancel.is_cancelled() {
                self.context.close_open_tool_calls();
                return Ok(StopReason::Interrupted);
            }

            if calls.is_empty() {
                return Ok(stop);
            }

            let results = self
                .tools
                .execute(&calls, &self.permissions, events, cancel.clone())
                .await;
            self.context.accept_tool_results(calls, results);

            if cancel.is_cancelled() {
                self.context.close_open_tool_calls();
                return Ok(StopReason::Interrupted);
            }
            req = self.context.build_request(self.tools.schemas());
        }

        Err(AgentError::IterationLimit(MAX_ITERATIONS))
    }

    async fn stream_turn(
        &self,
        req: &Request,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
        prior_usage: Usage,
    ) -> Result<(Option<Message>, Usage, Result<StopReason, ProviderError>), AgentError> {
        const MAX_STREAM_ATTEMPTS: u32 = 3;

        for attempt in 0..MAX_STREAM_ATTEMPTS {
            let mut stream = match self.provider.stream(req, cancel.clone()).await {
                Ok(stream) => stream,
                Err(error) if error.retryable && attempt + 1 < MAX_STREAM_ATTEMPTS => {
                    let delay = std::time::Duration::from_millis(50 * (1_u64 << attempt));
                    tokio::select! {
                        () = tokio::time::sleep(delay) => continue,
                        () = cancel.cancelled() => {
                            return Ok((None, Usage::default(), Ok(StopReason::Interrupted)));
                        }
                    }
                }
                Err(error) => return Err(AgentError::Provider(error)),
            };
            let mut assistant: Option<Message> = None;
            let mut terminal = None;
            let mut request_usage = Usage::default();
            let mut usage_seen = false;

            while let Some(event) = stream.next().await {
                match event {
                    Event::TextDelta { text } => {
                        let _ = events.send(AgentEvent::TextDelta(text));
                    }
                    Event::ThinkingDelta { text } => {
                        let _ = events.send(AgentEvent::ThinkingDelta(text));
                    }
                    Event::Usage(usage) => {
                        request_usage = usage;
                        usage_seen = true;
                        let _ = events.send(AgentEvent::RequestUsage(usage));
                        let _ = events.send(AgentEvent::TurnUsage(add_usage(prior_usage, usage)));
                    }
                    Event::Done { stop, message } => {
                        if !usage_seen && let Some(usage) = message.usage {
                            request_usage = usage;
                            let _ = events.send(AgentEvent::RequestUsage(usage));
                            let _ =
                                events.send(AgentEvent::TurnUsage(add_usage(prior_usage, usage)));
                        }
                        assistant = Some(message);
                        terminal = Some(Ok(stop));
                    }
                    Event::Failed {
                        retryable,
                        error,
                        message,
                    } => {
                        assistant = Some(message);
                        terminal = Some(Err(ProviderError {
                            retryable,
                            message: error,
                        }));
                    }
                    Event::HostedToolCallStart { id, name, summary } => {
                        let _ = events.send(AgentEvent::ToolStarted { id, name, summary });
                    }
                    Event::HostedToolCallEnd {
                        id,
                        name,
                        output,
                        is_error,
                    } => {
                        let result = if is_error {
                            ToolResult::error(output)
                        } else {
                            ToolResult::ok(output)
                        };
                        let _ = events.send(AgentEvent::ToolFinished { id, name, result });
                    }
                    Event::ToolCallStart { .. }
                    | Event::ToolCallArgs { .. }
                    | Event::ToolCallEnd { .. }
                    | Event::Unknown(_) => {}
                }
            }

            let assistant = assistant.ok_or_else(|| {
                AgentError::Provider(ProviderError::retryable(
                    "stream ended without a terminal event",
                ))
            })?;
            let terminal = terminal.expect("terminal message and outcome are set together");
            let should_retry = matches!(&terminal, Err(error) if error.retryable)
                && assistant.blocks.is_empty()
                && attempt + 1 < MAX_STREAM_ATTEMPTS;
            if should_retry {
                let delay = std::time::Duration::from_millis(50 * (1_u64 << attempt));
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = cancel.cancelled() => {
                        return Ok((None, request_usage, Ok(StopReason::Interrupted)));
                    }
                }
                continue;
            }
            return Ok((Some(assistant), request_usage, terminal));
        }
        unreachable!("stream attempts are bounded but non-zero")
    }
}

fn add_usage(left: Usage, right: Usage) -> Usage {
    Usage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        cache_read_tokens: left
            .cache_read_tokens
            .saturating_add(right.cache_read_tokens),
        cache_write_tokens: left
            .cache_write_tokens
            .saturating_add(right.cache_write_tokens),
    }
}
