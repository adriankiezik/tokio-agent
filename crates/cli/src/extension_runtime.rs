use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

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
    commands: Arc<BTreeMap<CommandId, CommandTarget>>,
    tools: Arc<BTreeMap<(ExtensionId, String), String>>,
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
        target: CommandTarget,
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
    ) -> anyhow::Result<Option<Self>> {
        if packages.is_empty() {
            return Ok(None);
        }
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
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
                    let _ = ready_tx.send(Err(anyhow::anyhow!("starting extension runtime")));
                    return;
                };
                runtime.block_on(async move {
                    let mut supervisor = SessionSupervisor::new(SupervisorPolicy::default());
                    let mut commands = BTreeMap::new();
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
                                let _ = ready_tx.send(Err(anyhow::Error::new(error)));
                                return;
                            }
                        }
                    }
                    if ready_tx.send(Ok(commands)).is_err() {
                        return;
                    }
                    while let Ok(request) = rx.recv() {
                        match request {
                            Request::Command {
                                target,
                                arguments,
                                reply,
                            } => {
                                let result = supervisor
                                    .invoke_programmable_command(
                                        target.extension,
                                        target.generation,
                                        target.handler,
                                        arguments,
                                    )
                                    .await
                                    .map_err(anyhow::Error::new);
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
                                let effects: Vec<_> = supervisor
                                    .broadcast(event)
                                    .await
                                    .into_iter()
                                    .map(|result| result.unwrap_or_else(|error| SupervisorEffect::Notice {
                                        level: tokio_agent_extension_api::NoticeLevel::Error,
                                        text: format!("Extension stopped: {error}"),
                                    }))
                                    .collect();
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
                            Request::Tool {
                                extension,
                                generation,
                                handler,
                                arguments,
                                reply,
                            } => {
                                let result = supervisor
                                    .invoke_programmable_tool(
                                        extension, generation, handler, arguments,
                                    )
                                    .await
                                    .map_err(anyhow::Error::new);
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
        let commands = Arc::new(
            ready_rx
                .recv()
                .map_err(|_| anyhow::anyhow!("extension supervisor stopped during startup"))??,
        );
        Ok(Some(Self {
            tx,
            commands,
            tools: Arc::new(tool_handlers),
            dynamic,
            registered: Arc::new(Mutex::new(BTreeMap::new())),
            pending: Arc::new(Mutex::new(Vec::new())),
        }))
    }

    pub fn route(
        &self,
        id: &CommandId,
        arguments: String,
    ) -> anyhow::Result<Option<(String, bool)>> {
        let Some(target) = self.commands.get(id).cloned() else {
            return Ok(None);
        };
        let (reply, receive) = mpsc::channel();
        self.tx.send(Request::Command {
            target,
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
        let (reply, receive) = mpsc::channel();
        if self.tx.send(Request::Event { event, reply }).is_err() {
            return Vec::new();
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
        let (reply, receive) = mpsc::channel();
        if self.tx.send(Request::Poll { reply }).is_err() {
            return Vec::new();
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
                    if let Some(handler) = self.tools.get(&key).cloned() {
                        let generation = self
                            .commands
                            .values()
                            .find(|target| target.extension == descriptor.owner)
                            .map_or(1, |target| target.generation);
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
