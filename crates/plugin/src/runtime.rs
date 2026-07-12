use std::collections::BTreeMap;
use std::path::Path;

use tokio_agent_extension_api::{
    Capability, ExtensionAction, ExtensionId, HostRequest, HostResponse, NoticeLevel,
    RuntimeLimits, Sequenced, SessionEvent, StatusSegment, ToolDescriptor,
};

use crate::{
    ActionError, ActionOutcome, CompanionError, CompanionManager, ExtensionManifest, SessionQueues,
    SupervisorPolicy, SupervisorState, TimerQueue,
};

#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorEffect {
    SubmitPrompt {
        text: String,
        automatic: bool,
        owner: ExtensionId,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    Status(Vec<StatusSegment>),
    RegisterTool(ToolDescriptor),
    UnregisterTool {
        owner: ExtensionId,
        id: tokio_agent_extension_api::ToolId,
    },
    SessionStateStored {
        owner: ExtensionId,
    },
    AutonomyReleased {
        owner: ExtensionId,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeToolResult {
    pub content: String,
    pub is_error: bool,
    pub effects: Vec<SupervisorEffect>,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Companion(#[from] CompanionError),
    #[error(transparent)]
    Action(#[from] ActionError),
    #[error("extension host returned an unexpected response")]
    Protocol,
    #[error("dynamic tool name `{0}` conflicts with another enabled extension")]
    ToolCollision(String),
    #[error("dynamic tool owner does not match the acting extension")]
    ToolOwner,
}

/// Frontend- and provider-neutral session-service runtime. It owns extension
/// generations, action policy, timers, user-priority queues, cached status and
/// the isolated companion lifecycle. The CLI remains responsible only for
/// translating resulting effects to its concrete `Agent` channel.
pub struct SessionSupervisor {
    state: SupervisorState,
    queues: SessionQueues,
    timers: TimerQueue,
    companion: CompanionManager,
    session_state: BTreeMap<ExtensionId, Vec<u8>>,
    registered_tools: BTreeMap<String, (ExtensionId, tokio_agent_extension_api::ToolId)>,
    next_event_sequence: u64,
}

impl SessionSupervisor {
    #[must_use]
    pub fn new(policy: SupervisorPolicy) -> Self {
        Self {
            state: SupervisorState::new(policy),
            queues: SessionQueues::default(),
            timers: TimerQueue::default(),
            companion: CompanionManager::default(),
            session_state: BTreeMap::new(),
            registered_tools: BTreeMap::new(),
            next_event_sequence: 0,
        }
    }

    #[must_use]
    pub fn with_companion(mut self, companion: CompanionManager) -> Self {
        self.companion = companion;
        self
    }

    pub async fn enable_programmable(
        &mut self,
        manifest: &ExtensionManifest,
        package_root: &Path,
        limits: RuntimeLimits,
    ) -> Result<u64, RuntimeError> {
        let id = ExtensionId::new(&manifest.id);
        let capabilities = manifest.capabilities.as_set();
        let generation = self.state.enable(id.clone(), capabilities.iter().copied());
        let Some(runtime) = &manifest.runtime else {
            return Ok(generation);
        };
        let response = self
            .companion
            .request(&HostRequest::Load {
                extension: id.clone(),
                generation,
                component_path: package_root
                    .join(&runtime.component)
                    .to_string_lossy()
                    .into_owned(),
                capabilities: capabilities.into_iter().collect(),
                limits,
            })
            .await?;
        match response {
            HostResponse::Loaded {
                extension,
                generation: loaded,
            } if extension == id && loaded == generation => Ok(generation),
            _ => {
                self.state.disable(&id);
                Err(RuntimeError::Protocol)
            }
        }
    }

    pub async fn disable(&mut self, id: &ExtensionId) -> Result<(), RuntimeError> {
        let generation = self
            .state
            .enabled_extensions()
            .into_iter()
            .find_map(|(owner, generation, _)| (owner == *id).then_some(generation));
        self.state.disable(id);
        self.queues.disable(id);
        self.timers.disable(id);
        self.session_state.remove(id);
        self.companion.forget_extension(id);
        self.registered_tools.retain(|_, (owner, _)| owner != id);
        if let Some(generation) = generation {
            let _ = self
                .companion
                .request(&HostRequest::Disable {
                    extension: id.clone(),
                    generation,
                })
                .await;
        }
        Ok(())
    }

    pub fn automatic_admitted(&mut self, owner: &ExtensionId) {
        self.state.automatic_turn_admitted(owner);
        self.queues.disable(owner);
    }

    pub fn submit_user(&mut self, text: String) {
        self.queues.submit_user(text);
    }

    pub fn dequeue(&mut self) -> Option<crate::QueuedSubmission> {
        self.queues.dequeue()
    }

    pub fn interrupt(&mut self) {
        self.state.interrupt();
        self.queues.interrupt();
    }

    #[must_use]
    pub fn status_segments(&self) -> Vec<StatusSegment> {
        self.state.status_segments()
    }

    pub async fn invoke_programmable_command(
        &mut self,
        extension: ExtensionId,
        generation: u64,
        handler: String,
        arguments: String,
    ) -> Result<Vec<SupervisorEffect>, RuntimeError> {
        let autonomous = self.state.enabled_extensions().into_iter().any(
            |(id, current_generation, capabilities)| {
                id == extension
                    && current_generation == generation
                    && capabilities.contains(&Capability::SessionSubmitAutomatic)
            },
        );
        if autonomous
            && self
                .state
                .autonomy_owner()
                .is_some_and(|owner| owner != &extension)
        {
            return Err(ActionError::AutonomyConflict.into());
        }
        match self
            .companion
            .request(&HostRequest::InvokeCommand {
                extension,
                generation,
                handler,
                arguments,
            })
            .await?
        {
            HostResponse::Actions(actions) => self.apply_actions(actions),
            _ => Err(RuntimeError::Protocol),
        }
    }

    pub async fn invoke_programmable_tool(
        &mut self,
        extension: ExtensionId,
        generation: u64,
        handler: String,
        arguments_json: String,
    ) -> Result<RuntimeToolResult, RuntimeError> {
        match self
            .companion
            .request(&HostRequest::InvokeTool {
                extension,
                generation,
                handler,
                arguments_json,
            })
            .await?
        {
            HostResponse::ToolResult {
                content,
                is_error,
                actions,
            } => {
                let effects = self.apply_actions(actions)?;
                Ok(RuntimeToolResult {
                    content,
                    is_error,
                    effects,
                })
            }
            _ => Err(RuntimeError::Protocol),
        }
    }

    pub async fn broadcast(
        &mut self,
        event: SessionEvent,
    ) -> Vec<Result<SupervisorEffect, RuntimeError>> {
        let subscribers: Vec<_> = self
            .state
            .enabled_extensions()
            .into_iter()
            .filter(|(_, _, capabilities)| capabilities.contains(&Capability::SessionObserve))
            .collect();
        let mut effects = Vec::new();
        for (extension, generation, _) in subscribers {
            let sequence = self.next_event_sequence;
            self.next_event_sequence = self.next_event_sequence.saturating_add(1);
            let response = self
                .companion
                .request(&HostRequest::SessionEvent(Sequenced {
                    sequence,
                    extension: extension.clone(),
                    generation,
                    value: event.clone(),
                }))
                .await;
            match response {
                Ok(HostResponse::Actions(actions)) => match self.apply_actions(actions) {
                    Ok(batch) => effects.extend(batch.into_iter().map(Ok)),
                    Err(error) => effects.push(Err(error)),
                },
                Ok(_) => effects.push(Err(RuntimeError::Protocol)),
                Err(error) => effects.push(Err(error.into())),
            }
        }
        effects
    }

    pub fn fire_due(&mut self, now: std::time::Instant) -> Vec<Sequenced<SessionEvent>> {
        self.timers.due(now)
    }

    pub async fn deliver(
        &mut self,
        event: Sequenced<SessionEvent>,
    ) -> Vec<Result<SupervisorEffect, RuntimeError>> {
        let enabled = self.state.enabled_extensions().into_iter().any(
            |(extension, generation, capabilities)| {
                extension == event.extension
                    && generation == event.generation
                    && capabilities.contains(&Capability::SessionObserve)
            },
        );
        if !enabled {
            return Vec::new();
        }
        match self
            .companion
            .request(&HostRequest::SessionEvent(event))
            .await
        {
            Ok(HostResponse::Actions(actions)) => match self.apply_actions(actions) {
                Ok(effects) => effects.into_iter().map(Ok).collect(),
                Err(error) => vec![Err(error)],
            },
            Ok(_) => vec![Err(RuntimeError::Protocol)],
            Err(error) => vec![Err(error.into())],
        }
    }

    pub fn apply_actions(
        &mut self,
        actions: Vec<Sequenced<ExtensionAction>>,
    ) -> Result<Vec<SupervisorEffect>, RuntimeError> {
        let state = self.state.clone();
        let queues = self.queues.clone();
        let timers = self.timers.clone();
        let session_state = self.session_state.clone();
        let registered_tools = self.registered_tools.clone();
        let mut effects = Vec::with_capacity(actions.len());
        for action in actions {
            match self.apply(action) {
                Ok(effect) => effects.push(effect),
                Err(error) => {
                    self.state = state;
                    self.queues = queues;
                    self.timers = timers;
                    self.session_state = session_state;
                    self.registered_tools = registered_tools;
                    return Err(error);
                }
            }
        }
        for (owner, state) in &self.session_state {
            if let Some((_, generation, _)) = self
                .state
                .enabled_extensions()
                .into_iter()
                .find(|(extension, _, _)| extension == owner)
            {
                self.companion
                    .remember_session_state(owner.clone(), generation, state.clone());
            }
        }
        Ok(effects)
    }

    pub fn apply(
        &mut self,
        action: Sequenced<ExtensionAction>,
    ) -> Result<SupervisorEffect, RuntimeError> {
        let owner = action.extension.clone();
        let generation = action.generation;
        let value = action.value.clone();
        if let ExtensionAction::RegisterTool(tool) = &value
            && tool.owner != owner
        {
            return Err(RuntimeError::ToolOwner);
        }
        if let ExtensionAction::RegisterTool(tool) = &value
            && (self
                .registered_tools
                .get(&tool.name)
                .is_some_and(|(registered_owner, id)| registered_owner != &owner || id != &tool.id)
                || self
                    .registered_tools
                    .values()
                    .any(|(registered_owner, id)| id == &tool.id && registered_owner != &owner))
        {
            return Err(RuntimeError::ToolCollision(tool.name.clone()));
        }
        if let ExtensionAction::UnregisterTool(id) = &value
            && self
                .registered_tools
                .values()
                .any(|(registered_owner, registered_id)| {
                    registered_id == id && registered_owner != &owner
                })
        {
            return Err(RuntimeError::ToolOwner);
        }
        let outcome = self.state.apply(action)?;
        match (value, outcome) {
            (
                ExtensionAction::SubmitPrompt { text, automatic },
                ActionOutcome::PromptQueued { .. },
            ) => {
                if automatic {
                    self.queues.submit_automatic(owner.clone(), text.clone())?;
                }
                Ok(SupervisorEffect::SubmitPrompt {
                    text,
                    automatic,
                    owner,
                })
            }
            (ExtensionAction::ShowNotice { level, text }, ActionOutcome::Notice) => {
                Ok(SupervisorEffect::Notice { level, text })
            }
            (ExtensionAction::SetStatusSegment(_), ActionOutcome::StatusUpdated(_))
            | (ExtensionAction::ClearStatusSegment(_), ActionOutcome::StatusCleared(_)) => {
                Ok(SupervisorEffect::Status(self.state.status_segments()))
            }
            (ExtensionAction::RegisterTool(tool), ActionOutcome::ToolRegistered) => {
                self.registered_tools
                    .insert(tool.name.clone(), (owner, tool.id.clone()));
                Ok(SupervisorEffect::RegisterTool(tool))
            }
            (ExtensionAction::UnregisterTool(id), ActionOutcome::ToolUnregistered) => {
                self.registered_tools
                    .retain(|_, (registered_owner, registered_id)| {
                        registered_owner != &owner || registered_id != &id
                    });
                Ok(SupervisorEffect::UnregisterTool { owner, id })
            }
            (
                ExtensionAction::ScheduleTimer { id, after },
                ActionOutcome::TimerScheduled { .. },
            ) => {
                self.timers
                    .schedule(owner.clone(), generation, id, after.into());
                Ok(SupervisorEffect::Status(self.state.status_segments()))
            }
            (ExtensionAction::CancelTimer(id), ActionOutcome::TimerCancelled(_)) => {
                self.timers.cancel(&owner, &id);
                Ok(SupervisorEffect::Status(self.state.status_segments()))
            }
            (ExtensionAction::ReleaseAutonomy, ActionOutcome::AutonomyReleased) => {
                Ok(SupervisorEffect::AutonomyReleased { owner })
            }
            (ExtensionAction::PersistSessionState(bytes), ActionOutcome::StatePersisted) => {
                self.session_state.insert(owner.clone(), bytes);
                Ok(SupervisorEffect::SessionStateStored { owner })
            }
            (ExtensionAction::Steer { text }, ActionOutcome::Steering(_)) => {
                Ok(SupervisorEffect::SubmitPrompt {
                    text,
                    automatic: false,
                    owner,
                })
            }
            _ => Err(RuntimeError::Protocol),
        }
    }

    pub async fn shutdown(&mut self) {
        let _ = self.broadcast(SessionEvent::SessionStopping).await;
        self.companion.stop().await;
    }
}
