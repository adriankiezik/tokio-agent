use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};

use tokio_agent_core::{DynamicToolCatalog, SessionHookEffect, Tool, ToolCtx, ToolDef, ToolResult};
use tokio_agent_extension_api::{CommandId, ExtensionId, SessionEvent, ToolId};
use tokio_agent_plugin::{
    ExtensionManifest, RuntimeToolResult, SessionSupervisor, SupervisorEffect, SupervisorPolicy,
};

pub struct ProgrammablePackage {
    pub root: PathBuf,
    pub manifest: ExtensionManifest,
}

#[derive(Clone)]
pub struct ExtensionRuntime {
    tx: mpsc::Sender<Request>,
    generations: Arc<RwLock<BTreeMap<ExtensionId, u64>>>,
    initializing: Arc<AtomicBool>,
    startup_replies: Arc<Mutex<Vec<mpsc::Receiver<Vec<SupervisorEffect>>>>>,
    tools: Arc<RwLock<BTreeMap<(ExtensionId, String), String>>>,
    dynamic: DynamicToolCatalog,
    registered: Arc<Mutex<BTreeMap<ToolId, String>>>,
    pending: Arc<Mutex<Vec<SessionHookEffect>>>,
}

#[derive(Clone)]
struct CommandTarget {
    extension: ExtensionId,
    generation: u64,
    handler: String,
}

enum Request {
    Command {
        id: CommandId,
        arguments: String,
        reply: mpsc::Sender<anyhow::Result<Vec<SupervisorEffect>>>,
    },
    Event {
        event: SessionEvent,
        reply: mpsc::Sender<Vec<SupervisorEffect>>,
    },
    Poll {
        reply: mpsc::Sender<Vec<SupervisorEffect>>,
    },
    Load {
        packages: Vec<ProgrammablePackage>,
        reply: mpsc::Sender<anyhow::Result<()>>,
    },
    Tool {
        extension: ExtensionId,
        generation: u64,
        handler: String,
        arguments: String,
        reply: mpsc::Sender<anyhow::Result<RuntimeToolResult>>,
    },
}

impl ExtensionRuntime {
    pub fn start(
        packages: Vec<ProgrammablePackage>,
        dynamic: DynamicToolCatalog,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let generations = Arc::new(RwLock::new(BTreeMap::new()));
        let worker_generations = Arc::clone(&generations);
        let initializing = Arc::new(AtomicBool::new(true));
        let worker_initializing = Arc::clone(&initializing);
        let mut tool_handlers = BTreeMap::new();
        for package in &packages {
            let extension = ExtensionId::new(&package.manifest.id);
            for tool in &package.manifest.tools {
                tool_handlers.insert((extension.clone(), tool.name.clone()), tool.handler.clone());
            }
        }
        std::thread::Builder::new()
            .name("extension-supervisor".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let Ok(runtime) = runtime else {
                    worker_initializing.store(false, Ordering::Release);
                    return;
                };
                runtime.block_on(async move {
                    let mut supervisor = SessionSupervisor::new(SupervisorPolicy::default());
                    let mut commands = BTreeMap::new();
                    let mut startup_error = None;
                    let mut loaded_generations = BTreeMap::new();
                    for package in packages {
                        match supervisor
                            .enable_programmable(
                                &package.manifest,
                                &package.root,
                                Default::default(),
                            )
                            .await
                        {
                            Ok(generation) => {
                                let extension = ExtensionId::new(&package.manifest.id);
                                loaded_generations.insert(extension.clone(), generation);
                                for command in package
                                    .manifest
                                    .commands
                                    .iter()
                                    .filter(|command| command.handler.is_some())
                                {
                                    commands.insert(
                                        CommandId::new(format!(
                                            "{}:{}",
                                            package.manifest.id, command.name
                                        )),
                                        CommandTarget {
                                            extension: extension.clone(),
                                            generation,
                                            handler: command.handler.clone().expect("filtered"),
                                        },
                                    );
                                }
                            }
                            Err(error) => {
                                startup_error = Some(error.to_string());
                                break;
                            }
                        }
                    }
                    *worker_generations
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        loaded_generations.clone();
                    worker_initializing.store(false, Ordering::Release);
                    let mut startup_error_reported = false;
                    while let Ok(request) = rx.recv() {
                        match request {
                            Request::Command {
                                id,
                                arguments,
                                reply,
                            } => {
                                let result = if let Some(error) = &startup_error {
                                    Err(anyhow::anyhow!(error.clone()))
                                } else if let Some(target) = commands.get(&id).cloned() {
                                    supervisor
                                        .invoke_programmable_command(
                                            target.extension,
                                            target.generation,
                                            target.handler,
                                            arguments,
                                        )
                                        .await
                                        .map_err(anyhow::Error::new)
                                } else {
                                    Err(anyhow::anyhow!("unknown programmable command `{id}`"))
                                };
                                if let Ok(effects) = &result {
                                    for effect in effects {
                                        if let SupervisorEffect::SubmitPrompt {
                                            automatic: true,
                                            owner,
                                            ..
                                        } = effect
                                        {
                                            supervisor.automatic_admitted(owner);
                                        }
                                    }
                                }
                                let _ = reply.send(result);
                            }
                            Request::Event { event, reply } => {
                                let effects: Vec<_> = if let Some(error) = &startup_error
                                    && !startup_error_reported
                                {
                                    startup_error_reported = true;
                                    vec![SupervisorEffect::Notice {
                                        level: tokio_agent_extension_api::NoticeLevel::Error,
                                        text: format!("Extension failed to start: {error}"),
                                    }]
                                } else {
                                    supervisor
                                        .broadcast(event)
                                        .await
                                        .into_iter()
                                        .map(|result| result.unwrap_or_else(|error| SupervisorEffect::Notice {
                                        level: tokio_agent_extension_api::NoticeLevel::Error,
                                        text: format!("Extension stopped: {error}"),
                                        }))
                                        .collect()
                                };
                                for effect in &effects {
                                    if let SupervisorEffect::SubmitPrompt {
                                        automatic: true,
                                        owner,
                                        ..
                                    } = effect
                                    {
                                        supervisor.automatic_admitted(owner);
                                    }
                                }
                                let _ = reply.send(effects);
                            }
                            Request::Poll { reply } => {
                                let events = supervisor.fire_due(std::time::Instant::now());
                                let mut effects = Vec::new();
                                for event in events {
                                    effects.extend(
                                        supervisor
                                            .deliver(event)
                                            .await
                                            .into_iter()
                                            .map(|result| result.unwrap_or_else(|error| SupervisorEffect::Notice {
                                                level: tokio_agent_extension_api::NoticeLevel::Error,
                                                text: format!("Extension stopped: {error}"),
                                            })),
                                    );
                                }
                                for effect in &effects {
                                    if let SupervisorEffect::SubmitPrompt {
                                        automatic: true,
                                        owner,
                                        ..
                                    } = effect
                                    {
                                        supervisor.automatic_admitted(owner);
                                    }
                                }
                                let _ = reply.send(effects);
                            }
                            Request::Load { packages, reply } => {
                                let mut result = Ok(());
                                for package in packages {
                                    let extension = ExtensionId::new(&package.manifest.id);
                                    if loaded_generations.contains_key(&extension) {
                                        continue;
                                    }
                                    match supervisor
                                        .enable_programmable(
                                            &package.manifest,
                                            &package.root,
                                            Default::default(),
                                        )
                                        .await
                                    {
                                        Ok(generation) => {
                                            loaded_generations
                                                .insert(extension.clone(), generation);
                                            worker_generations
                                                .write()
                                                .unwrap_or_else(
                                                    std::sync::PoisonError::into_inner,
                                                )
                                                .insert(extension.clone(), generation);
                                            for command in package
                                                .manifest
                                                .commands
                                                .iter()
                                                .filter(|command| command.handler.is_some())
                                            {
                                                commands.insert(
                                                    CommandId::new(format!(
                                                        "{}:{}",
                                                        package.manifest.id, command.name
                                                    )),
                                                    CommandTarget {
                                                        extension: extension.clone(),
                                                        generation,
                                                        handler: command
                                                            .handler
                                                            .clone()
                                                            .expect("filtered"),
                                                    },
                                                );
                                            }
                                        }
                                        Err(error) => {
                                            result = Err(anyhow::Error::new(error));
                                            break;
                                        }
                                    }
                                }
                                let _ = reply.send(result);
                            }
                            Request::Tool {
                                extension,
                                generation,
                                handler,
                                arguments,
                                reply,
                            } => {
                                let result = if let Some(error) = &startup_error {
                                    Err(anyhow::anyhow!(error.clone()))
                                } else {
                                    supervisor
                                        .invoke_programmable_tool(
                                            extension, generation, handler, arguments,
                                        )
                                        .await
                                        .map_err(anyhow::Error::new)
                                };
                                if let Ok(result) = &result {
                                    for effect in &result.effects {
                                        if let SupervisorEffect::SubmitPrompt {
                                            automatic: true,
                                            owner,
                                            ..
                                        } = effect
                                        {
                                            supervisor.automatic_admitted(owner);
                                        }
                                    }
                                }
                                let _ = reply.send(result);
                            }
                        }
                    }
                    supervisor.shutdown().await;
                });
            })?;
        Ok(Self {
            tx,
            generations,
            initializing,
            startup_replies: Arc::new(Mutex::new(Vec::new())),
            tools: Arc::new(RwLock::new(tool_handlers)),
            dynamic,
            registered: Arc::new(Mutex::new(BTreeMap::new())),
            pending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn load(&self, packages: Vec<ProgrammablePackage>) -> anyhow::Result<()> {
        {
            let mut tools = self
                .tools
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for package in &packages {
                let extension = ExtensionId::new(&package.manifest.id);
                for tool in &package.manifest.tools {
                    tools.insert((extension.clone(), tool.name.clone()), tool.handler.clone());
                }
            }
        }
        let (reply, receive) = mpsc::channel();
        self.tx.send(Request::Load { packages, reply })?;
        receive.recv()??;
        Ok(())
    }

    pub fn route(
        &self,
        id: &CommandId,
        arguments: String,
    ) -> anyhow::Result<Option<(String, bool)>> {
        let (reply, receive) = mpsc::channel();
        self.tx.send(Request::Command {
            id: id.clone(),
            arguments,
            reply,
        })?;
        let effects = receive.recv()??;
        let mut prompt = None;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for effect in self.process_effects(effects) {
            match effect {
                SessionHookEffect::SubmitPrompt {
                    text, automatic, ..
                } if prompt.is_none() => prompt = Some((text, automatic)),
                other => pending.push(other),
            }
        }
        Ok(prompt)
    }

    pub fn event(&self, event: SessionEvent) -> Vec<SessionHookEffect> {
        let is_session_start = matches!(event, SessionEvent::SessionStarted);
        let (reply, receive) = mpsc::channel();
        if self.tx.send(Request::Event { event, reply }).is_err() {
            return Vec::new();
        }
        if self.initializing.load(Ordering::Acquire) {
            self.startup_replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(receive);
            return if is_session_start {
                vec![SessionHookEffect::StatusSegments(vec![
                    initializing_status_segment(),
                ])]
            } else {
                Vec::new()
            };
        }
        let mut effects = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        effects.extend(self.process_effects(receive.recv().unwrap_or_default()));
        effects
    }

    pub fn poll(&self) -> Vec<SessionHookEffect> {
        let mut effects = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut startup_replies = self
            .startup_replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let had_startup_replies = !startup_replies.is_empty();
        let mut waiting = Vec::new();
        for receive in startup_replies.drain(..) {
            match receive.try_recv() {
                Ok(startup_effects) => effects.extend(self.process_effects(startup_effects)),
                Err(mpsc::TryRecvError::Empty) => waiting.push(receive),
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
        *startup_replies = waiting;
        let waiting_for_startup_events = !startup_replies.is_empty();
        drop(startup_replies);

        if self.initializing.load(Ordering::Acquire) || waiting_for_startup_events {
            return effects;
        }
        if had_startup_replies
            && !effects
                .iter()
                .any(|effect| matches!(effect, SessionHookEffect::StatusSegments(_)))
        {
            effects.push(SessionHookEffect::StatusSegments(Vec::new()));
        }

        let (reply, receive) = mpsc::channel();
        if self.tx.send(Request::Poll { reply }).is_err() {
            return effects;
        }
        effects.extend(self.process_effects(receive.recv().unwrap_or_default()));
        effects
    }

    fn process_effects(&self, effects: Vec<SupervisorEffect>) -> Vec<SessionHookEffect> {
        let mut output = Vec::new();
        for effect in effects {
            match effect {
                SupervisorEffect::SubmitPrompt {
                    text,
                    automatic,
                    owner,
                } => output.push(SessionHookEffect::SubmitPrompt {
                    text,
                    automatic,
                    source: Some(owner),
                }),
                SupervisorEffect::Notice { text, .. } => {
                    output.push(SessionHookEffect::Notice(text))
                }
                SupervisorEffect::Status(segments) => {
                    output.push(SessionHookEffect::StatusSegments(segments))
                }
                SupervisorEffect::RegisterTool(descriptor) => {
                    let key = (descriptor.owner.clone(), descriptor.name.clone());
                    if let Some(handler) = self
                        .tools
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&key)
                        .cloned()
                    {
                        let generation = self
                            .generations
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&descriptor.owner)
                            .copied()
                            .unwrap_or(1);
                        let name = descriptor.name.clone();
                        let tool = Arc::new(ExtensionTool {
                            descriptor: descriptor.clone(),
                            handler,
                            generation,
                            runtime: self.clone(),
                        });
                        if self
                            .dynamic
                            .register(descriptor.owner.to_string(), tool)
                            .is_ok()
                        {
                            self.registered
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(descriptor.id, name);
                        }
                    }
                }
                SupervisorEffect::UnregisterTool { owner, id } => {
                    if let Some(name) = self
                        .registered
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id)
                    {
                        self.dynamic.unregister(owner.as_str(), &name);
                    }
                }
                SupervisorEffect::SessionStateStored { .. }
                | SupervisorEffect::AutonomyReleased { .. } => {}
            }
        }
        output
    }
}

fn initializing_status_segment() -> tokio_agent_extension_api::StatusSegment {
    tokio_agent_extension_api::StatusSegment {
        id: "tokio-agent:extensions-initializing".to_owned(),
        text: "Initializing extensions…".to_owned(),
        tone: tokio_agent_extension_api::StatusTone::Muted,
        side: tokio_agent_extension_api::StatusSide::Right,
        priority: i16::MAX,
        min_width: 24,
    }
}

struct ExtensionTool {
    descriptor: tokio_agent_extension_api::ToolDescriptor,
    handler: String,
    generation: u64,
    runtime: ExtensionRuntime,
}
impl Tool for ExtensionTool {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: self.descriptor.name.clone(),
            description: self.descriptor.description.clone(),
            input_schema: self.descriptor.input_schema.clone(),
        }
    }
    fn permission(&self, _input: &serde_json::Value) -> tokio_agent_core::PermissionRequest {
        tokio_agent_core::PermissionRequest {
            tool: format!("{} ({})", self.descriptor.name, self.descriptor.owner),
            summary: format!("run extension tool owned by {}", self.descriptor.owner),
            action: match self.descriptor.permission {
                tokio_agent_extension_api::ToolPermission::Read => tokio_agent_core::Action::Read,
                tokio_agent_extension_api::ToolPermission::Edit => tokio_agent_core::Action::Edit,
                tokio_agent_extension_api::ToolPermission::Execute => {
                    tokio_agent_core::Action::Execute
                }
            },
        }
    }
    fn run<'a>(
        &'a self,
        input: serde_json::Value,
        _ctx: &'a ToolCtx,
    ) -> tokio_agent_core::provider::BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let runtime = self.runtime.clone();
            let extension = self.descriptor.owner.clone();
            let generation = self.generation;
            let handler = self.handler.clone();
            let arguments = input.to_string();
            match tokio::task::spawn_blocking(move || {
                let (reply, receive) = mpsc::channel();
                runtime.tx.send(Request::Tool {
                    extension,
                    generation,
                    handler,
                    arguments,
                    reply,
                })?;
                let result = receive.recv()??;
                let effects = runtime.process_effects(result.effects.clone());
                runtime
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(effects);
                Ok::<_, anyhow::Error>(result)
            })
            .await
            {
                Ok(Ok(result)) if result.is_error => ToolResult::error(result.content),
                Ok(Ok(result)) => ToolResult::ok(result.content),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(error.to_string()),
            }
        })
    }
}
