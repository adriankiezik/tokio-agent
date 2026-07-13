use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_agent_extension_api::{
    CommandDescriptor, CommandId, ExtensionId, ExtensionSummary, StatusSegment,
};
use tokio_util::sync::CancellationToken;

use crate::context::ContextAssembler;
use crate::event::{Event, StopReason};
use crate::message::{ContentBlock, Message, ProviderMetadata, Role, ToolOutput, Usage};
use crate::provider::{Provider, ProviderError, Request};
use crate::tool::{
    FrontendCapabilities, InteractionBroker, InteractionRequest, InteractionResponse, Tool,
    ToolGate, ToolGateSlot, ToolResult,
};
use crate::tool_execution::{DynamicToolCatalog, ToolCallExecutor};

#[derive(Debug)]
pub enum AgentEvent {
    AutomaticTurnStarted(String),
    TextDelta(String),
    ThinkingDelta(String),
    ToolStarted {
        id: crate::message::ToolCallId,
        name: String,
        summary: String,
    },
    ToolOutputDelta {
        id: crate::message::ToolCallId,
        text: String,
    },
    ToolFinished {
        id: crate::message::ToolCallId,
        name: String,
        result: ToolResult,
    },
    TurnUsage(Usage),
    RequestUsage(Usage),
    InteractionRequested(InteractionRequest),
    InteractionCancelled {
        id: tokio_agent_extension_api::InteractionId,
    },
    StatusSegments(Vec<StatusSegment>),
    CommandCatalog(Vec<CommandDescriptor>),
    ExtensionCatalog(Vec<ExtensionSummary>),
    CommandHandled(Result<Option<String>, String>),
    TurnDone(Result<StopReason, AgentError>),
}

#[derive(Debug)]
pub enum UiCommand {
    UserMessage(String),
    AutomaticMessage { source: ExtensionId, text: String },
    Steer(String),
    InvokeCommand { id: CommandId, arguments: String },
    CommandHandled(Option<String>),
    Clear,
    SetModel(String),
    SetReasoningEffort(Option<String>),
    Interrupt,
    RespondToInteraction(InteractionResponse),
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
    #[error("command failed: {0}")]
    Command(String),
}

pub struct ModelConfig {
    pub model: String,
    pub system: String,
    pub max_tokens: u32,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Default)]
pub struct AgentState {
    transcript: Vec<Message>,
    last_input_tokens: u64,
}

impl AgentState {
    #[must_use]
    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }
}

pub struct Agent<P: Provider> {
    provider: P,
    tools: ToolCallExecutor,
    interactions: InteractionBroker,
    gate: ToolGateSlot,
    context: ContextAssembler,
    reasoning_effort_supported: bool,
    provider_name: String,
    cwd: PathBuf,
    context_window: Option<u64>,
    last_input_tokens: u64,
    compaction_available: bool,
    command_catalog: Vec<CommandDescriptor>,
    command_router: Option<Arc<CommandRouteFn>>,
    interaction_responder: Option<Arc<InteractionResponseFn>>,
    extension_catalog: Vec<ExtensionSummary>,
    session_hook: Option<Arc<SessionHookFn>>,
    session_poll: Option<Arc<dyn Fn() -> Vec<SessionHookEffect> + Send + Sync>>,
    hook_pending: Arc<Mutex<Vec<SessionHookEffect>>>,
    shutdown_hook: ShutdownHook,
}

#[derive(Debug, Clone)]
pub enum SessionHookEffect {
    SubmitPrompt {
        text: String,
        automatic: bool,
        source: Option<ExtensionId>,
    },
    StatusSegments(Vec<StatusSegment>),
    CommandCatalog(Vec<CommandDescriptor>),
    ExtensionCatalog(Vec<ExtensionSummary>),
    InteractionRequested(InteractionRequest),
    InteractionCancelled(tokio_agent_extension_api::InteractionId),
    Notice(String),
}

type CommandRouteFn = dyn Fn(CommandId, String) -> Result<UiCommand, String> + Send + Sync;
type InteractionResponseFn = dyn Fn(InteractionResponse) -> Result<(), String> + Send + Sync;
type SessionHookFn =
    dyn Fn(tokio_agent_extension_api::SessionEvent) -> Vec<SessionHookEffect> + Send + Sync;

impl<P: Provider> Agent<P> {
    pub fn new(
        provider: P,
        tools: Vec<Arc<dyn Tool>>,
        gate: Option<Arc<dyn ToolGate>>,
        model: ModelConfig,
        cwd: PathBuf,
    ) -> Self {
        let ModelConfig {
            model,
            system,
            max_tokens,
            reasoning_effort,
        } = model;
        let interactions = InteractionBroker::default();
        let gate = ToolGateSlot::new(gate);
        Self {
            provider,
            tools: ToolCallExecutor::new(
                tools,
                cwd.clone(),
                gate.clone(),
                interactions.clone(),
                FrontendCapabilities::default(),
            ),
            interactions,
            gate,
            context: ContextAssembler::new(model, system, max_tokens)
                .with_reasoning_effort(reasoning_effort),
            reasoning_effort_supported: true,
            provider_name: String::new(),
            cwd,
            context_window: None,
            last_input_tokens: 0,
            compaction_available: true,
            command_catalog: Vec::new(),
            command_router: None,
            interaction_responder: None,
            extension_catalog: Vec::new(),
            session_hook: None,
            session_poll: None,
            hook_pending: Arc::new(Mutex::new(Vec::new())),
            shutdown_hook: ShutdownHook(None),
        }
    }

    #[must_use]
    pub fn with_command_router<F>(mut self, catalog: Vec<CommandDescriptor>, router: F) -> Self
    where
        F: Fn(CommandId, String) -> Result<UiCommand, String> + Send + Sync + 'static,
    {
        self.command_catalog = catalog;
        self.command_router = Some(Arc::new(router));
        self
    }

    #[must_use]
    pub fn with_interaction_responder<F>(mut self, responder: F) -> Self
    where
        F: Fn(InteractionResponse) -> Result<(), String> + Send + Sync + 'static,
    {
        self.interaction_responder = Some(Arc::new(responder));
        self
    }

    #[must_use]
    pub fn with_shutdown_hook<F>(mut self, hook: F) -> Self
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        self.shutdown_hook = ShutdownHook(Some(Box::new(hook)));
        self
    }

    #[must_use]
    pub fn command_catalog(&self) -> Vec<CommandDescriptor> {
        self.command_catalog.clone()
    }

    #[must_use]
    pub fn with_session_hook<F>(mut self, hook: F) -> Self
    where
        F: Fn(tokio_agent_extension_api::SessionEvent) -> Vec<SessionHookEffect>
            + Send
            + Sync
            + 'static,
    {
        let hook: Arc<SessionHookFn> = Arc::new(hook);
        let lifecycle_hook = Arc::clone(&hook);
        let pending = Arc::clone(&self.hook_pending);
        self.tools = self
            .tools
            .with_lifecycle_callback(Arc::new(move |name, is_error| {
                let effects =
                    lifecycle_hook(tokio_agent_extension_api::SessionEvent::ToolFinished {
                        name,
                        is_error,
                    });
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(effects);
            }));
        self.session_hook = Some(hook);
        self
    }

    #[must_use]
    pub fn with_session_poll<F>(mut self, poll: F) -> Self
    where
        F: Fn() -> Vec<SessionHookEffect> + Send + Sync + 'static,
    {
        self.session_poll = Some(Arc::new(poll));
        self
    }

    #[must_use]
    pub fn with_extension_catalog(mut self, catalog: Vec<ExtensionSummary>) -> Self {
        self.extension_catalog = catalog;
        self
    }

    #[must_use]
    pub fn extension_catalog(&self) -> Vec<ExtensionSummary> {
        self.extension_catalog.clone()
    }

    #[must_use]
    pub fn dynamic_tools(&self) -> DynamicToolCatalog {
        self.tools.dynamic_catalog()
    }

    #[must_use]
    pub fn with_frontend_capabilities(mut self, capabilities: FrontendCapabilities) -> Self {
        self.tools.set_frontend_capabilities(capabilities);
        self
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

    #[must_use]
    pub fn with_provider_name(mut self, provider_name: impl Into<String>) -> Self {
        self.provider_name = provider_name.into();
        self
    }

    #[must_use]
    pub fn with_state(mut self, state: AgentState) -> Self {
        self.context.replace_transcript(state.transcript);
        self.last_input_tokens = state.last_input_tokens;
        self
    }

    fn into_state(self) -> AgentState {
        AgentState {
            transcript: self.context.transcript().to_vec(),
            last_input_tokens: self.last_input_tokens,
        }
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
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

    pub fn context_before_compaction(&self) -> Option<u64> {
        self.context_window.map(|window| {
            compaction_threshold(
                &self.provider_name,
                self.provider.supports_native_compaction(),
                window,
                self.context.max_tokens(),
            )
        })
    }

    pub fn max_output_tokens(&self) -> u32 {
        self.context.max_tokens()
    }

    #[must_use]
    pub fn tool_gate_slot(&self) -> ToolGateSlot {
        self.gate.clone()
    }

    pub async fn run(
        mut self,
        mut commands: UnboundedReceiver<UiCommand>,
        events: UnboundedSender<AgentEvent>,
    ) -> AgentState {
        let mut queued: VecDeque<(String, bool, Option<ExtensionId>)> = VecDeque::new();
        apply_session_hook(
            self.session_hook.as_ref(),
            tokio_agent_extension_api::SessionEvent::SessionStarted,
            &mut queued,
            &events,
        );
        loop {
            let (input, automatic, automatic_source) = match queued.pop_front() {
                Some(input) => input,
                None => match self.next_idle_input(&mut commands, &events).await {
                    Some(input) => input,
                    None => {
                        apply_session_hook(
                            self.session_hook.as_ref(),
                            tokio_agent_extension_api::SessionEvent::SessionStopping,
                            &mut queued,
                            &events,
                        );
                        return self.into_state();
                    }
                },
            };
            if !automatic {
                apply_session_hook(
                    self.session_hook.as_ref(),
                    tokio_agent_extension_api::SessionEvent::UserMessageSubmitted,
                    &mut queued,
                    &events,
                );
            }
            let _sleep_inhibitor = crate::sleep::SleepInhibitor::acquire();
            if automatic {
                let _ = events.send(AgentEvent::AutomaticTurnStarted(input.clone()));
                if let Some(source) = automatic_source {
                    apply_session_hook(
                        self.session_hook.as_ref(),
                        tokio_agent_extension_api::SessionEvent::AutomaticTurnStarted { source },
                        &mut queued,
                        &events,
                    );
                }
            }

            let cancel = CancellationToken::new();
            let interactions = self.interactions.clone();
            let interaction_responder = self.interaction_responder.clone();
            let command_router = self.command_router.clone();
            let mut steering = false;
            let mut shutting_down = false;
            let result = {
                let turn = self.drive(input, &events, cancel.clone());
                tokio::pin!(turn);
                loop {
                    tokio::select! {
                        result = &mut turn => break result,
                        command = commands.recv() => match command {
                            Some(UiCommand::Interrupt) => cancel.cancel(),
                            Some(UiCommand::RespondToInteraction(response)) => {
                                if !interactions.respond(response.clone()) {
                                    if let Some(responder) = &interaction_responder { let _ = responder(response); }
                                }
                            }
                            Some(UiCommand::Steer(input)) => {
                                queued.push_back((input, false, None));
                                steering = true;
                                cancel.cancel();
                            }
                            Some(UiCommand::UserMessage(input)) => queued.push_back((input, false, None)),
                            Some(UiCommand::AutomaticMessage { source, text }) => queued.push_back((text, true, Some(source))),
                            Some(UiCommand::InvokeCommand { id, arguments }) => {
                                let Some(router) = command_router.as_ref() else {
                                    let _ = events.send(AgentEvent::CommandHandled(Err(
                                        "command routing is unavailable".to_owned(),
                                    )));
                                    continue;
                                };
                                match router(id, arguments) {
                                    Ok(UiCommand::UserMessage(input)) => {
                                        queued.push_back((input, false, None));
                                    }
                                    Ok(UiCommand::AutomaticMessage { source, text }) => {
                                        queued.push_back((text, true, Some(source)));
                                    }
                                    Ok(UiCommand::Interrupt) => cancel.cancel(),
                                    Ok(UiCommand::RespondToInteraction(response)) => {
                                        if !interactions.respond(response.clone()) {
                                            if let Some(responder) = &interaction_responder { let _ = responder(response); }
                                        }
                                    }
                                    Ok(UiCommand::CommandHandled(message)) => {
                                        let _ = events.send(AgentEvent::CommandHandled(Ok(message)));
                                    }
                                    Ok(UiCommand::InvokeCommand { .. })
                                    | Ok(UiCommand::SetModel(_))
                                    | Ok(UiCommand::SetReasoningEffort(_))
                                    | Ok(UiCommand::Clear)
                                    | Ok(UiCommand::Shutdown)
                                    | Ok(UiCommand::Steer(_)) => {}
                                    Err(error) => {
                                        let _ = events.send(AgentEvent::CommandHandled(Err(error)));
                                    }
                                }
                            },
                            Some(UiCommand::CommandHandled(message)) => {
                                let _ = events.send(AgentEvent::CommandHandled(Ok(message)));
                            }
                            Some(UiCommand::SetModel(_))
                            | Some(UiCommand::SetReasoningEffort(_))
                            | Some(UiCommand::Clear) => {}
                            Some(UiCommand::Shutdown) | None => {
                                cancel.cancel();
                                let result = (&mut turn).await;
                                shutting_down = true;
                                break result;
                            }
                        }
                    }
                }
            };

            if shutting_down {
                apply_session_hook(
                    self.session_hook.as_ref(),
                    tokio_agent_extension_api::SessionEvent::SessionStopping,
                    &mut queued,
                    &events,
                );
                return self.into_state();
            }
            if steering {
                continue;
            }

            let pending_effects = std::mem::take(
                &mut *self
                    .hook_pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            apply_session_effects(pending_effects, &mut queued, &events);

            let interrupted = matches!(result, Ok(StopReason::Interrupted));
            let lifecycle = if interrupted {
                tokio_agent_extension_api::SessionEvent::Interrupted
            } else {
                let usage = self
                    .context
                    .transcript()
                    .iter()
                    .rev()
                    .find_map(|message| message.usage)
                    .unwrap_or_default();
                tokio_agent_extension_api::SessionEvent::TurnFinished {
                    stop: extension_stop_reason(result.as_ref().ok()),
                    usage: tokio_agent_extension_api::Usage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                    },
                }
            };
            apply_session_hook(self.session_hook.as_ref(), lifecycle, &mut queued, &events);
            let _ = events.send(AgentEvent::TurnDone(result));
        }
    }

    async fn next_idle_input(
        &mut self,
        commands: &mut UnboundedReceiver<UiCommand>,
        events: &UnboundedSender<AgentEvent>,
    ) -> Option<(String, bool, Option<ExtensionId>)> {
        loop {
            let command = if self.session_poll.is_some() {
                tokio::select! {
                    biased;
                    command = commands.recv() => command,
                    () = tokio::time::sleep(Duration::from_millis(100)) => {
                        if let Some(input) = poll_session(self.session_poll.as_ref(), events) { return Some(input); }
                        continue;
                    }
                }
            } else {
                commands.recv().await
            };

            let mut command = command;
            loop {
                match command {
                    Some(UiCommand::UserMessage(input)) => return Some((input, false, None)),
                    Some(UiCommand::AutomaticMessage { source, text }) => {
                        return Some((text, true, Some(source)));
                    }
                    Some(UiCommand::Steer(input)) => return Some((input, true, None)),
                    Some(UiCommand::InvokeCommand { id, arguments }) => {
                        let Some(router) = self.command_router.as_ref() else {
                            let _ = events.send(AgentEvent::CommandHandled(Err(
                                "command routing is unavailable".to_owned(),
                            )));
                            break;
                        };
                        match router(id, arguments) {
                            Ok(routed) => {
                                command = Some(routed);
                                continue;
                            }
                            Err(error) => {
                                let _ = events.send(AgentEvent::CommandHandled(Err(error)));
                                break;
                            }
                        }
                    }
                    Some(UiCommand::Interrupt) => {}
                    Some(UiCommand::CommandHandled(message)) => {
                        let _ = events.send(AgentEvent::CommandHandled(Ok(message)));
                    }
                    Some(UiCommand::Clear) => {
                        self.context.clear();
                        let _ = events.send(AgentEvent::CommandHandled(Ok(None)));
                    }
                    Some(UiCommand::SetModel(model)) => {
                        self.context_window = known_context_window(&self.provider_name, &model);
                        self.context.set_model(model);
                    }
                    Some(UiCommand::SetReasoningEffort(effort)) => {
                        self.context.set_reasoning_effort(effort);
                    }
                    Some(UiCommand::RespondToInteraction(response)) => {
                        if !self.interactions.respond(response.clone())
                            && let Some(responder) = &self.interaction_responder
                        {
                            let _ = responder(response);
                        }
                    }
                    Some(UiCommand::Shutdown) | None => return None,
                }
                break;
            }
        }
    }

    fn tool_schemas(&self) -> Vec<crate::tool::ToolDef> {
        self.tools.schemas()
    }

    async fn drive(
        &mut self,
        user_input: String,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<StopReason, AgentError> {
        self.compact_if_needed(cancel.clone()).await?;
        let mut req = self.context.begin_turn(user_input, self.tool_schemas());
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
            self.last_input_tokens = request_usage.input_tokens;
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

            let results = self.tools.execute(&calls, events, cancel.clone()).await;
            self.context.accept_tool_results(calls, results);

            if cancel.is_cancelled() {
                self.context.close_open_tool_calls();
                return Ok(StopReason::Interrupted);
            }
            self.compact_if_needed(cancel.clone()).await?;
            req = self.context.build_request(self.tool_schemas());
        }

        Err(AgentError::IterationLimit(MAX_ITERATIONS))
    }

    async fn compact_if_needed(&mut self, cancel: CancellationToken) -> Result<(), AgentError> {
        let Some(context_window) = self.context_window else {
            return Ok(());
        };
        if !self.compaction_available || self.context.transcript().is_empty() {
            return Ok(());
        }
        let native = self.provider.supports_native_compaction();
        let threshold = compaction_threshold(
            &self.provider_name,
            native,
            context_window,
            self.context.max_tokens(),
        );
        let estimated = self.context.estimated_input_tokens();
        if self.last_input_tokens.max(estimated) < threshold {
            return Ok(());
        }

        if native {
            let request = self.context.build_request(self.tool_schemas());
            match self.provider.compact(&request, cancel).await? {
                Some(messages) => self.context.replace_transcript(messages),
                None => self.compaction_available = false,
            }
        } else if let Some(messages) = self.local_compact(cancel).await? {
            self.context.replace_transcript(messages);
        } else {
            self.compaction_available = false;
        }
        self.last_input_tokens = self.context.estimated_input_tokens();
        Ok(())
    }

    async fn local_compact(
        &self,
        cancel: CancellationToken,
    ) -> Result<Option<Vec<Message>>, AgentError> {
        let keep_tokens = self
            .context_window
            .unwrap_or_default()
            .saturating_sub(u64::from(self.context.max_tokens()).min(20_000))
            .saturating_div(4)
            .clamp(2_000, 8_000);
        let Some((head, recent)) = split_for_local_compaction(
            self.context.transcript(),
            usize::try_from(keep_tokens.saturating_mul(4)).unwrap_or(usize::MAX),
        ) else {
            return Ok(None);
        };
        let request = Request {
            model: self.context.model().to_owned(),
            system: LOCAL_COMPACTION_SYSTEM_PROMPT.to_owned(),
            messages: vec![Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: format!("{head}\n\n{LOCAL_COMPACTION_OUTPUT_TEMPLATE}"),
                    meta: ProviderMetadata::default(),
                }],
                usage: None,
            }],
            tools: Vec::new(),
            max_tokens: self.context.max_tokens().min(4_096),
            reasoning_effort: self.context.reasoning_effort().map(str::to_owned),
        };
        let mut stream = self.provider.stream(&request, cancel).await?;
        let mut summary = None;
        while let Some(event) = stream.next().await {
            match event {
                Event::Done { message, .. } => summary = message_text(&message),
                Event::Failed {
                    retryable, error, ..
                } => {
                    return Err(AgentError::Provider(ProviderError {
                        retryable,
                        message: error,
                    }));
                }
                _ => {}
            }
        }
        let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) else {
            return Ok(None);
        };
        let mut compacted = vec![Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: format!("<compacted-context>\n{summary}\n</compacted-context>"),
                meta: ProviderMetadata::default(),
            }],
            usage: None,
        }];
        compacted.extend(recent);
        Ok(Some(compacted))
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

struct ShutdownHook(Option<Box<dyn FnOnce() + Send + Sync>>);

impl Drop for ShutdownHook {
    fn drop(&mut self) {
        if let Some(hook) = self.0.take() {
            hook();
        }
    }
}

fn apply_session_hook(
    hook: Option<&Arc<SessionHookFn>>,
    event: tokio_agent_extension_api::SessionEvent,
    queued: &mut VecDeque<(String, bool, Option<ExtensionId>)>,
    events: &UnboundedSender<AgentEvent>,
) {
    let Some(hook) = hook else { return };
    apply_session_effects(hook(event), queued, events);
}

fn apply_session_effects(
    effects: Vec<SessionHookEffect>,
    queued: &mut VecDeque<(String, bool, Option<ExtensionId>)>,
    events: &UnboundedSender<AgentEvent>,
) {
    for effect in effects {
        match effect {
            SessionHookEffect::SubmitPrompt {
                text,
                automatic,
                source,
            } => {
                if automatic {
                    queued.push_back((text, true, source));
                } else {
                    queued.push_front((text, false, source));
                }
            }
            SessionHookEffect::StatusSegments(segments) => {
                let _ = events.send(AgentEvent::StatusSegments(segments));
            }
            SessionHookEffect::CommandCatalog(catalog) => {
                let _ = events.send(AgentEvent::CommandCatalog(catalog));
            }
            SessionHookEffect::ExtensionCatalog(catalog) => {
                let _ = events.send(AgentEvent::ExtensionCatalog(catalog));
            }
            SessionHookEffect::InteractionRequested(request) => {
                let _ = events.send(AgentEvent::InteractionRequested(request));
            }
            SessionHookEffect::InteractionCancelled(id) => {
                let _ = events.send(AgentEvent::InteractionCancelled { id });
            }
            SessionHookEffect::Notice(text) => {
                let _ = events.send(AgentEvent::CommandHandled(Ok(Some(text))));
            }
        }
    }
}

fn poll_session(
    poll: Option<&Arc<dyn Fn() -> Vec<SessionHookEffect> + Send + Sync>>,
    events: &UnboundedSender<AgentEvent>,
) -> Option<(String, bool, Option<ExtensionId>)> {
    let mut prompt = None;
    for effect in poll?.as_ref()() {
        match effect {
            SessionHookEffect::SubmitPrompt {
                text,
                automatic,
                source,
            } if prompt.is_none() => {
                prompt = Some((text, automatic, source));
            }
            SessionHookEffect::StatusSegments(segments) => {
                let _ = events.send(AgentEvent::StatusSegments(segments));
            }
            SessionHookEffect::CommandCatalog(catalog) => {
                let _ = events.send(AgentEvent::CommandCatalog(catalog));
            }
            SessionHookEffect::ExtensionCatalog(catalog) => {
                let _ = events.send(AgentEvent::ExtensionCatalog(catalog));
            }
            SessionHookEffect::InteractionRequested(request) => {
                let _ = events.send(AgentEvent::InteractionRequested(request));
            }
            SessionHookEffect::InteractionCancelled(id) => {
                let _ = events.send(AgentEvent::InteractionCancelled { id });
            }
            SessionHookEffect::Notice(text) => {
                let _ = events.send(AgentEvent::CommandHandled(Ok(Some(text))));
            }
            SessionHookEffect::SubmitPrompt { .. } => {}
        }
    }
    prompt
}

fn extension_stop_reason(reason: Option<&StopReason>) -> tokio_agent_extension_api::StopReason {
    match reason {
        Some(StopReason::EndTurn) => tokio_agent_extension_api::StopReason::EndTurn,
        Some(StopReason::MaxTokens) => tokio_agent_extension_api::StopReason::MaxTokens,
        Some(StopReason::ToolUse) => tokio_agent_extension_api::StopReason::ToolUse,
        Some(StopReason::Interrupted) => tokio_agent_extension_api::StopReason::Interrupted,
        None => tokio_agent_extension_api::StopReason::Error,
    }
}

const OPENAI_DEFAULT_COMPACTION_POINT: u64 = 250_000;

fn compaction_threshold(
    provider: &str,
    native: bool,
    context_window: u64,
    max_output_tokens: u32,
) -> u64 {
    if native {
        let threshold = context_window.saturating_mul(9) / 10;
        if provider == "openai" {
            threshold.min(OPENAI_DEFAULT_COMPACTION_POINT)
        } else {
            threshold
        }
    } else {
        context_window.saturating_sub(u64::from(max_output_tokens).min(20_000))
    }
}

fn known_context_window(provider: &str, model: &str) -> Option<u64> {
    match provider {
        "anthropic" if model == "claude-haiku-4-5" => Some(200_000),
        "anthropic" => Some(1_000_000),
        "openai" if model.starts_with("gpt-5.6") => Some(372_000),
        "openai" if model == "gpt-5.4" => Some(1_000_000),
        "openai" => Some(272_000),
        "deepseek" => Some(1_000_000),
        _ => None,
    }
}

const LOCAL_COMPACTION_SYSTEM_PROMPT: &str = "You are an anchored context summarization assistant for coding sessions. Summarize only the conversation history you are given. Preserve exact file paths, identifiers, commands, errors, decisions, constraints, completed work, active work, blockers, and immediate next actions. Do not answer the conversation itself and do not mention the compaction process. Use terse Markdown bullets.";

const LOCAL_COMPACTION_OUTPUT_TEMPLATE: &str = r#"Output exactly this structure:
## Objective
- ...
## Important Details
- ...
## Work State
### Completed
- ...
### Active
- ...
### Blocked
- ...
## Next Move
1. ...
## Relevant Files
- ..."#;

fn split_for_local_compaction(
    messages: &[Message],
    recent_byte_budget: usize,
) -> Option<(String, Vec<Message>)> {
    let user_indices = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == Role::User).then_some(index))
        .collect::<Vec<_>>();
    if user_indices.len() < 2 {
        return None;
    }
    let mut split = *user_indices.last()?;
    for index in user_indices.into_iter().rev() {
        let size = messages[index..]
            .iter()
            .map(serialized_message_len)
            .sum::<usize>();
        if size > recent_byte_budget {
            break;
        }
        split = index;
    }
    if split == 0 {
        return None;
    }
    let head = messages[..split]
        .iter()
        .map(serialize_message_for_compaction)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!head.is_empty()).then(|| (head, messages[split..].to_vec()))
}

fn serialized_message_len(message: &Message) -> usize {
    serialize_message_for_compaction(message).len()
}

fn serialize_message_for_compaction(message: &Message) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(format!("[{:?}]: {text}", message.role)),
            ContentBlock::Thinking { text, .. } if !text.is_empty() => {
                Some(format!("[Assistant reasoning]: {text}"))
            }
            ContentBlock::Thinking { .. } | ContentBlock::Compaction { .. } => None,
            ContentBlock::ToolCall { name, args, .. } => {
                Some(format!("[Assistant tool call]: {name}({})", args.get()))
            }
            ContentBlock::ToolResult {
                output, is_error, ..
            } => {
                let ToolOutput::Text(text) = output;
                let text = if text.len() > 2_000 {
                    format!("{}\n[truncated]", &text[..floor_char_boundary(text, 2_000)])
                } else {
                    text.clone()
                };
                Some(format!(
                    "[Tool {}]: {text}",
                    if *is_error { "error" } else { "result" }
                ))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn message_text(message: &Message) -> Option<String> {
    let text = message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, ProviderMetadata, Role};
    use crate::provider::{BoxFuture, Capabilities};
    use futures::stream::BoxStream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn shutdown_hook_runs_when_its_owner_is_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            let _hook = ShutdownHook(Some(Box::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
            })));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct CompactingProvider {
        calls: Arc<AtomicUsize>,
    }

    impl Provider for CompactingProvider {
        fn stream<'a>(
            &'a self,
            _req: &'a Request,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
            Box::pin(async { unreachable!("stream is not used by this test") })
        }

        fn supports_native_compaction(&self) -> bool {
            true
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tools: false,
                streaming: true,
                caching: false,
                vision: false,
            }
        }

        fn compact<'a>(
            &'a self,
            _req: &'a Request,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<Option<Vec<Message>>, ProviderError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Some(vec![Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::Compaction {
                        encrypted_content: "summary".into(),
                        meta: ProviderMetadata::default(),
                    }],
                    usage: None,
                }]))
            })
        }
    }

    struct LocalCompactingProvider {
        calls: Arc<AtomicUsize>,
    }

    impl Provider for LocalCompactingProvider {
        fn stream<'a>(
            &'a self,
            _req: &'a Request,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let message = Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "## Objective\n- preserve the task".into(),
                    meta: ProviderMetadata::default(),
                }],
                usage: None,
            };
            Box::pin(async move {
                Ok(Box::pin(futures::stream::iter(vec![Event::Done {
                    stop: StopReason::EndTurn,
                    message,
                }])) as BoxStream<'static, Event>)
            })
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tools: false,
                streaming: true,
                caching: false,
                vision: false,
            }
        }
    }

    #[test]
    fn state_restores_conversation_into_a_different_agent() {
        let state = AgentState {
            transcript: vec![Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: "keep this context".into(),
                    meta: ProviderMetadata::default(),
                }],
                usage: None,
            }],
            last_input_tokens: 42,
        };
        let agent = Agent::new(
            LocalCompactingProvider {
                calls: Arc::new(AtomicUsize::new(0)),
            },
            Vec::new(),
            None,
            ModelConfig {
                model: "different-model".into(),
                system: String::new(),
                max_tokens: 1_024,
                reasoning_effort: None,
            },
            PathBuf::new(),
        )
        .with_state(state);

        assert_eq!(agent.last_input_tokens, 42);
        assert!(matches!(
            &agent.context.transcript()[0].blocks[0],
            ContentBlock::Text { text, .. } if text == "keep this context"
        ));
    }

    #[test]
    fn openai_compaction_is_capped_only_when_default_threshold_exceeds_250k() {
        assert_eq!(
            compaction_threshold("openai", true, 372_000, 32_000),
            250_000
        );
        assert_eq!(
            compaction_threshold("openai", true, 272_000, 32_000),
            244_800
        );
        assert_eq!(
            compaction_threshold("anthropic", false, 1_000_000, 32_000),
            980_000
        );
    }

    #[tokio::test]
    async fn compacts_at_ninety_percent_of_the_context_window() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            CompactingProvider {
                calls: Arc::clone(&calls),
            },
            Vec::new(),
            None,
            ModelConfig {
                model: "model".into(),
                system: String::new(),
                max_tokens: 1024,
                reasoning_effort: None,
            },
            PathBuf::new(),
        )
        .with_context_window(Some(100));
        agent.context.push_user("x".repeat(360));

        agent
            .compact_if_needed(CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            &agent.context.transcript()[0].blocks[0],
            ContentBlock::Compaction { encrypted_content, .. } if encrypted_content == "summary"
        ));
    }

    #[tokio::test]
    async fn local_compaction_summarizes_old_history_and_preserves_recent_turns() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            LocalCompactingProvider {
                calls: Arc::clone(&calls),
            },
            Vec::new(),
            None,
            ModelConfig {
                model: "model".into(),
                system: String::new(),
                max_tokens: 4_096,
                reasoning_effort: None,
            },
            PathBuf::new(),
        )
        .with_context_window(Some(10_000));
        agent.context.push_user("old".repeat(5_000));
        agent.context.push_assistant(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "old response".repeat(1_000),
                meta: ProviderMetadata::default(),
            }],
            usage: None,
        });
        agent.context.push_user("recent request".into());

        agent
            .compact_if_needed(CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(agent.context.transcript().len(), 2);
        assert!(matches!(
            &agent.context.transcript()[0].blocks[0],
            ContentBlock::Text { text, .. } if text.contains("<compacted-context>")
        ));
        assert!(matches!(
            &agent.context.transcript()[1].blocks[0],
            ContentBlock::Text { text, .. } if text == "recent request"
        ));
    }
}
