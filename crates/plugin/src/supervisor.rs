use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use tokio_agent_extension_api::{
    Capability, ExtensionAction, ExtensionId, Sequenced, StatusSegment, TimerId,
};

#[derive(Debug, Clone)]
pub struct SupervisorPolicy {
    pub maximum_automatic_turns: u32,
    pub maximum_automatic_submissions_per_minute: usize,
    pub maximum_pending_actions: usize,
    pub maximum_payload_bytes: usize,
    pub maximum_status_chars: usize,
    pub maximum_status_updates_per_second: usize,
    pub maximum_timers_per_extension: usize,
    pub minimum_timer_interval: Duration,
}

impl Default for SupervisorPolicy {
    fn default() -> Self {
        Self {
            maximum_automatic_turns: 100,
            maximum_automatic_submissions_per_minute: 10,
            maximum_pending_actions: 128,
            maximum_payload_bytes: 256 * 1024,
            maximum_status_chars: 160,
            maximum_status_updates_per_second: 10,
            maximum_timers_per_extension: 32,
            minimum_timer_interval: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ActionError {
    #[error("extension is disabled or action generation is stale")]
    Stale,
    #[error("extension lacks capability `{0:?}`")]
    Capability(Capability),
    #[error("extension action payload exceeds its limit")]
    PayloadLimit,
    #[error("automatic submission limit reached")]
    AutomaticLimit,
    #[error("an automatic submission is already queued for this extension")]
    AlreadyQueued,
    #[error("timer interval or count violates policy")]
    TimerLimit,
    #[error("too many extension actions are pending")]
    QueueLimit,
    #[error("invalid status segment")]
    InvalidStatus,
    #[error("status update rate limit reached")]
    StatusRateLimit,
    #[error("another extension owns autonomous work")]
    AutonomyConflict,
    #[error("extension action used a stale generation or wrong owner")]
    StaleGeneration,
    #[error("invalid or unsafe interaction content")]
    InvalidInteraction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionOutcome {
    PromptQueued { text: String, automatic: bool },
    Steering(String),
    Notice,
    StatusUpdated(StatusSegment),
    StatusCleared(String),
    ToolRegistered,
    ToolUnregistered,
    TimerScheduled { id: TimerId, after: Duration },
    TimerCancelled(TimerId),
    StatePersisted,
    UserStatePersisted,
    InteractionRequested(tokio_agent_extension_api::InteractionRequest),
    AutonomyReleased,
}

#[derive(Debug, Clone)]
struct RuntimeState {
    generation: u64,
    capabilities: BTreeSet<Capability>,
    timers: BTreeSet<TimerId>,
    automatic_queued: bool,
    automatic_turns: u32,
    recent_submissions: VecDeque<Instant>,
    recent_status_updates: VecDeque<Instant>,
}

#[derive(Debug, Clone, Default)]
pub struct SupervisorState {
    policy: SupervisorPolicy,
    runtimes: BTreeMap<ExtensionId, RuntimeState>,
    generations: BTreeMap<ExtensionId, u64>,
    status: BTreeMap<(ExtensionId, String), StatusSegment>,
    pending_actions: usize,
    autonomy_stopped: bool,
    autonomy_owner: Option<ExtensionId>,
    next_sequence: u64,
}

impl SupervisorState {
    #[must_use]
    pub fn new(policy: SupervisorPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    pub fn enable(
        &mut self,
        id: ExtensionId,
        capabilities: impl IntoIterator<Item = Capability>,
    ) -> u64 {
        let generation = self
            .generations
            .get(&id)
            .copied()
            .unwrap_or_default()
            .saturating_add(1);
        self.generations.insert(id.clone(), generation);
        self.runtimes.insert(
            id,
            RuntimeState {
                generation,
                capabilities: capabilities.into_iter().collect(),
                timers: BTreeSet::new(),
                automatic_queued: false,
                automatic_turns: 0,
                recent_submissions: VecDeque::new(),
                recent_status_updates: VecDeque::new(),
            },
        );
        generation
    }

    pub fn disable(&mut self, id: &ExtensionId) {
        self.runtimes.remove(id);
        if self.autonomy_owner.as_ref() == Some(id) {
            self.autonomy_owner = None;
        }
        self.status.retain(|(owner, _), _| owner != id);
    }

    pub fn emergency_stop(&mut self) {
        self.autonomy_stopped = true;
        self.autonomy_owner = None;
        for state in self.runtimes.values_mut() {
            state.automatic_queued = false;
            state.timers.clear();
        }
    }

    pub fn clear_emergency_stop(&mut self) {
        self.autonomy_stopped = false;
    }

    pub fn interrupt(&mut self) {
        self.autonomy_owner = None;
        for state in self.runtimes.values_mut() {
            state.automatic_queued = false;
            state.timers.clear();
        }
    }

    pub fn automatic_turn_admitted(&mut self, id: &ExtensionId) {
        if let Some(state) = self.runtimes.get_mut(id) {
            state.automatic_queued = false;
            state.automatic_turns = state.automatic_turns.saturating_add(1);
        }
    }

    pub fn apply(
        &mut self,
        action: Sequenced<ExtensionAction>,
    ) -> Result<ActionOutcome, ActionError> {
        self.next_sequence = self.next_sequence.max(action.sequence.saturating_add(1));
        if self.pending_actions >= self.policy.maximum_pending_actions {
            return Err(ActionError::QueueLimit);
        }
        let state = self
            .runtimes
            .get_mut(&action.extension)
            .ok_or(ActionError::Stale)?;
        if state.generation != action.generation {
            return Err(ActionError::Stale);
        }
        self.pending_actions += 1;
        let result = apply_action(
            &self.policy,
            self.autonomy_stopped,
            &mut self.autonomy_owner,
            &action.extension,
            state,
            &mut self.status,
            action.value,
        );
        self.pending_actions -= 1;
        result
    }

    #[must_use]
    pub fn autonomy_owner(&self) -> Option<&ExtensionId> {
        self.autonomy_owner.as_ref()
    }

    #[must_use]
    pub fn enabled_extensions(&self) -> Vec<(ExtensionId, u64, BTreeSet<Capability>)> {
        self.runtimes
            .iter()
            .map(|(id, state)| (id.clone(), state.generation, state.capabilities.clone()))
            .collect()
    }

    #[must_use]
    pub fn status_segments(&self) -> Vec<StatusSegment> {
        let mut values: Vec<_> = self.status.values().cloned().collect();
        values.sort_by_key(|segment| std::cmp::Reverse(segment.priority));
        values
    }
}

fn apply_action(
    policy: &SupervisorPolicy,
    autonomy_stopped: bool,
    autonomy_owner: &mut Option<ExtensionId>,
    owner: &ExtensionId,
    state: &mut RuntimeState,
    status: &mut BTreeMap<(ExtensionId, String), StatusSegment>,
    action: ExtensionAction,
) -> Result<ActionOutcome, ActionError> {
    match action {
        ExtensionAction::SubmitPrompt { text, automatic } => {
            payload(policy, &text)?;
            if automatic {
                require(state, Capability::SessionSubmitAutomatic)?;
                if autonomy_stopped || state.automatic_turns >= policy.maximum_automatic_turns {
                    return Err(ActionError::AutomaticLimit);
                }
                if state.automatic_queued {
                    return Err(ActionError::AlreadyQueued);
                }
                if autonomy_owner
                    .as_ref()
                    .is_some_and(|current| current != owner)
                {
                    return Err(ActionError::AutonomyConflict);
                }
                let cutoff = Instant::now() - Duration::from_secs(60);
                while state
                    .recent_submissions
                    .front()
                    .is_some_and(|time| *time < cutoff)
                {
                    state.recent_submissions.pop_front();
                }
                if state.recent_submissions.len() >= policy.maximum_automatic_submissions_per_minute
                {
                    return Err(ActionError::AutomaticLimit);
                }
                state.recent_submissions.push_back(Instant::now());
                state.automatic_queued = true;
                *autonomy_owner = Some(owner.clone());
            }
            Ok(ActionOutcome::PromptQueued { text, automatic })
        }
        ExtensionAction::Steer { text } => {
            require(state, Capability::SessionSubmitAutomatic)?;
            payload(policy, &text)?;
            Ok(ActionOutcome::Steering(text))
        }
        ExtensionAction::ShowNotice { text, .. } => {
            payload(policy, &text)?;
            Ok(ActionOutcome::Notice)
        }
        ExtensionAction::SetStatusSegment(mut segment) => {
            require(state, Capability::StatusWrite)?;
            let cutoff = Instant::now() - Duration::from_secs(1);
            while state
                .recent_status_updates
                .front()
                .is_some_and(|time| *time < cutoff)
            {
                state.recent_status_updates.pop_front();
            }
            if state.recent_status_updates.len() >= policy.maximum_status_updates_per_second {
                return Err(ActionError::StatusRateLimit);
            }
            state.recent_status_updates.push_back(Instant::now());
            if segment.id.is_empty() || segment.text.contains(['\n', '\r', '\x1b']) {
                return Err(ActionError::InvalidStatus);
            }
            segment.text = segment
                .text
                .chars()
                .take(policy.maximum_status_chars)
                .collect();
            status.insert((owner.clone(), segment.id.clone()), segment.clone());
            Ok(ActionOutcome::StatusUpdated(segment))
        }
        ExtensionAction::ClearStatusSegment(id) => {
            require(state, Capability::StatusWrite)?;
            if id.is_empty() {
                return Err(ActionError::InvalidStatus);
            }
            status.remove(&(owner.clone(), id.clone()));
            Ok(ActionOutcome::StatusCleared(id))
        }
        ExtensionAction::RegisterTool(_) => {
            require(state, Capability::ToolsDynamic)?;
            Ok(ActionOutcome::ToolRegistered)
        }
        ExtensionAction::UnregisterTool(_) => {
            require(state, Capability::ToolsDynamic)?;
            Ok(ActionOutcome::ToolUnregistered)
        }
        ExtensionAction::ScheduleTimer { id, after } => {
            require(state, Capability::SessionSchedule)?;
            let after = Duration::from(after);
            if after < policy.minimum_timer_interval
                || (!state.timers.contains(&id)
                    && state.timers.len() >= policy.maximum_timers_per_extension)
            {
                return Err(ActionError::TimerLimit);
            }
            state.timers.insert(id.clone());
            Ok(ActionOutcome::TimerScheduled { id, after })
        }
        ExtensionAction::CancelTimer(id) => {
            require(state, Capability::SessionSchedule)?;
            state.timers.remove(&id);
            Ok(ActionOutcome::TimerCancelled(id))
        }
        ExtensionAction::ReleaseAutonomy => {
            require(state, Capability::SessionSubmitAutomatic)?;
            if autonomy_owner.as_ref() == Some(owner) {
                *autonomy_owner = None;
            }
            state.automatic_queued = false;
            Ok(ActionOutcome::AutonomyReleased)
        }
        ExtensionAction::PersistSessionState(bytes) => {
            require(state, Capability::StorageSession)?;
            if bytes.len() > policy.maximum_payload_bytes {
                return Err(ActionError::PayloadLimit);
            }
            Ok(ActionOutcome::StatePersisted)
        }
        ExtensionAction::PersistUserState(bytes) => {
            require(state, Capability::StorageUser)?;
            if bytes.len() > policy.maximum_payload_bytes {
                return Err(ActionError::PayloadLimit);
            }
            Ok(ActionOutcome::UserStatePersisted)
        }
        ExtensionAction::RequestInteraction(request) => {
            require(state, Capability::InteractionRequest)?;
            if request.owner != *owner || request.generation != state.generation {
                return Err(ActionError::StaleGeneration);
            }
            let encoded = serde_json::to_vec(&request).map_err(|_| ActionError::PayloadLimit)?;
            if encoded.len() > policy.maximum_payload_bytes {
                return Err(ActionError::PayloadLimit);
            }
            if !safe_interaction(&request) {
                return Err(ActionError::InvalidInteraction);
            }
            Ok(ActionOutcome::InteractionRequested(request))
        }
    }
}

fn safe_interaction(request: &tokio_agent_extension_api::InteractionRequest) -> bool {
    fn text(value: &str) -> bool {
        value.chars().count() <= 4_096
            && !value.chars().any(|character| {
                character == '\u{1b}'
                    || (character.is_control() && !matches!(character, '\n' | '\t'))
            })
    }
    fn id(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'.')
            })
    }
    if !id(request.id.as_str()) {
        return false;
    }
    match &request.spec {
        tokio_agent_extension_api::InteractionSpec::Approval(spec) => {
            text(&spec.title)
                && spec.body.len() <= 32
                && spec.body.iter().all(|section| {
                    section.heading.as_deref().is_none_or(text) && text(&section.text)
                })
                && !spec.actions.is_empty()
                && spec.actions.len() <= 16
                && spec.actions.iter().all(|action| {
                    id(&action.id)
                        && text(&action.label)
                        && action.key_hint.as_deref().is_none_or(text)
                })
                && spec.copy_text.as_deref().is_none_or(text)
        }
        tokio_agent_extension_api::InteractionSpec::SingleSelect(spec) => {
            text(&spec.title)
                && !spec.options.is_empty()
                && spec.options.len() <= 64
                && spec.options.iter().all(|option| {
                    id(&option.id)
                        && text(&option.label)
                        && option.description.as_deref().is_none_or(text)
                })
                && spec.selected.as_deref().is_none_or(id)
        }
    }
}

fn require(state: &RuntimeState, capability: Capability) -> Result<(), ActionError> {
    if state.capabilities.contains(&capability) {
        Ok(())
    } else {
        Err(ActionError::Capability(capability))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomaticSubmission {
    pub extension: ExtensionId,
    pub text: String,
}

/// Deterministic session input queues. User work always wins and each
/// extension can own at most one queued automatic submission.
#[derive(Debug, Clone, Default)]
pub struct SessionQueues {
    users: VecDeque<String>,
    automatic: BTreeMap<ExtensionId, String>,
}

impl SessionQueues {
    pub fn submit_user(&mut self, text: String) {
        self.users.push_back(text);
    }

    pub fn submit_automatic(
        &mut self,
        extension: ExtensionId,
        text: String,
    ) -> Result<(), ActionError> {
        if self.automatic.contains_key(&extension) {
            return Err(ActionError::AlreadyQueued);
        }
        self.automatic.insert(extension, text);
        Ok(())
    }

    pub fn dequeue(&mut self) -> Option<QueuedSubmission> {
        self.users
            .pop_front()
            .map(QueuedSubmission::User)
            .or_else(|| {
                let extension = self.automatic.keys().next()?.clone();
                let text = self.automatic.remove(&extension)?;
                Some(QueuedSubmission::Automatic(AutomaticSubmission {
                    extension,
                    text,
                }))
            })
    }

    pub fn interrupt(&mut self) {
        self.automatic.clear();
    }

    pub fn disable(&mut self, extension: &ExtensionId) {
        self.automatic.remove(extension);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueuedSubmission {
    User(String),
    Automatic(AutomaticSubmission),
}

#[derive(Debug, Clone, Default)]
pub struct TimerQueue {
    timers: BTreeMap<(Instant, ExtensionId, TimerId), u64>,
    next_sequence: u64,
}

impl TimerQueue {
    pub fn schedule(
        &mut self,
        extension: ExtensionId,
        generation: u64,
        id: TimerId,
        after: Duration,
    ) {
        self.cancel(&extension, &id);
        self.timers
            .insert((Instant::now() + after, extension, id), generation);
    }

    pub fn cancel(&mut self, extension: &ExtensionId, id: &TimerId) {
        self.timers
            .retain(|(_, owner, timer), _| owner != extension || timer != id);
    }

    pub fn disable(&mut self, extension: &ExtensionId) {
        self.timers.retain(|(_, owner, _), _| owner != extension);
    }

    pub fn due(&mut self, now: Instant) -> Vec<Sequenced<tokio_agent_extension_api::SessionEvent>> {
        let due: Vec<_> = self
            .timers
            .range(
                ..=(
                    now,
                    ExtensionId::new("\u{10ffff}"),
                    TimerId::new("\u{10ffff}"),
                ),
            )
            .map(|(key, generation)| (key.clone(), *generation))
            .collect();
        for (key, _) in &due {
            self.timers.remove(key);
        }
        due.into_iter()
            .map(|((_, extension, id), generation)| {
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.saturating_add(1);
                Sequenced {
                    sequence,
                    extension,
                    generation,
                    value: tokio_agent_extension_api::SessionEvent::TimerFired { id },
                }
            })
            .collect()
    }
}

fn payload(policy: &SupervisorPolicy, value: &str) -> Result<(), ActionError> {
    if value.len() <= policy.maximum_payload_bytes {
        Ok(())
    } else {
        Err(ActionError::PayloadLimit)
    }
}
