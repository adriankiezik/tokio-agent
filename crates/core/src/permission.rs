use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::tool::{Action, PermissionRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Suggest,
    AutoEdit,
    FullAuto,
}

impl Mode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "suggest" => Some(Mode::Suggest),
            "auto-edit" | "auto_edit" => Some(Mode::AutoEdit),
            "full-auto" | "full_auto" => Some(Mode::FullAuto),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PermissionId(pub(crate) u64);

struct State {
    mode: Mode,
    next_id: u64,
    pending: std::collections::HashMap<PermissionId, oneshot::Sender<Decision>>,
    always_allow: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Run,
    Deny,
}

enum Gate {
    Allow,
    Ask,
}

#[derive(Clone)]
pub struct PermissionEngine {
    state: Arc<Mutex<State>>,
}

impl PermissionEngine {
    #[must_use]
    pub fn new(mode: Mode) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                mode,
                next_id: 0,
                pending: std::collections::HashMap::new(),
                always_allow: HashSet::new(),
            })),
        }
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.state().mode
    }

    pub fn set_mode(&self, mode: Mode) {
        self.state().mode = mode;
    }

    pub fn resolve(&self, id: PermissionId, decision: Decision) {
        if let Some(answer) = self.state().pending.remove(&id) {
            let _ = answer.send(decision);
        }
    }

    pub async fn decide<F>(
        &self,
        req: &PermissionRequest,
        cancel: CancellationToken,
        announce: F,
    ) -> Outcome
    where
        F: FnOnce(PermissionId, PermissionRequest) -> bool,
    {
        let decision = match self.evaluate(req) {
            Gate::Allow => Decision::AllowOnce,
            Gate::Ask => {
                let (id, answer) = self.register();
                if announce(id, req.clone()) {
                    let decision = tokio::select! {
                        answer = answer => answer.unwrap_or(Decision::Deny),
                        () = cancel.cancelled() => Decision::Deny,
                    };
                    self.discard(id);
                    decision
                } else {
                    self.discard(id);
                    Decision::Deny
                }
            }
        };

        match decision {
            Decision::Deny => Outcome::Deny,
            Decision::AllowOnce => Outcome::Run,
            Decision::AllowAlways => {
                self.state().always_allow.insert(req.tool.clone());
                Outcome::Run
            }
        }
    }

    fn evaluate(&self, req: &PermissionRequest) -> Gate {
        if req.action == Action::Read {
            return Gate::Allow;
        }
        if self.state().always_allow.contains(&req.tool) {
            return Gate::Allow;
        }
        match self.mode() {
            Mode::FullAuto => Gate::Allow,
            Mode::AutoEdit if req.action == Action::Edit => Gate::Allow,
            Mode::AutoEdit | Mode::Suggest => Gate::Ask,
        }
    }

    fn register(&self) -> (PermissionId, oneshot::Receiver<Decision>) {
        let mut state = self.state();
        let id = PermissionId(state.next_id);
        state.next_id += 1;
        let (answer, receiver) = oneshot::channel();
        state.pending.insert(id, answer);
        (id, receiver)
    }

    fn discard(&self, id: PermissionId) {
        self.state().pending.remove(&id);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("permission state poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str, action: Action) -> PermissionRequest {
        PermissionRequest {
            tool: tool.to_owned(),
            summary: String::new(),
            action,
        }
    }

    fn panic_ask(_: PermissionId, _: PermissionRequest) -> bool {
        panic!("ask should not have been invoked");
    }

    async fn decide(
        engine: &PermissionEngine,
        req: &PermissionRequest,
        answer: Decision,
    ) -> Outcome {
        let resolver = engine.clone();
        engine
            .decide(req, CancellationToken::new(), move |id, _| {
                resolver.resolve(id, answer);
                true
            })
            .await
    }

    #[tokio::test]
    async fn read_is_auto_allowed_without_ask_in_every_mode() {
        for mode in [Mode::Suggest, Mode::AutoEdit, Mode::FullAuto] {
            let engine = PermissionEngine::new(mode);
            let outcome = engine
                .decide(
                    &req("read", Action::Read),
                    CancellationToken::new(),
                    panic_ask,
                )
                .await;
            assert_eq!(outcome, Outcome::Run);
        }
    }

    #[tokio::test]
    async fn full_auto_allows_edit_and_execute_without_ask() {
        let engine = PermissionEngine::new(Mode::FullAuto);
        assert_eq!(
            engine
                .decide(
                    &req("edit", Action::Edit),
                    CancellationToken::new(),
                    panic_ask
                )
                .await,
            Outcome::Run
        );
        assert_eq!(
            engine
                .decide(
                    &req("bash", Action::Execute),
                    CancellationToken::new(),
                    panic_ask
                )
                .await,
            Outcome::Run
        );
    }

    #[tokio::test]
    async fn auto_edit_allows_edit_but_asks_for_execute() {
        let engine = PermissionEngine::new(Mode::AutoEdit);
        assert_eq!(
            engine
                .decide(
                    &req("edit", Action::Edit),
                    CancellationToken::new(),
                    panic_ask
                )
                .await,
            Outcome::Run
        );

        let outcome = decide(&engine, &req("bash", Action::Execute), Decision::AllowOnce).await;
        assert_eq!(outcome, Outcome::Run);
    }

    #[tokio::test]
    async fn mode_can_be_changed_for_all_engine_clones() {
        let engine = PermissionEngine::new(Mode::Suggest);
        let clone = engine.clone();

        clone.set_mode(Mode::FullAuto);

        assert_eq!(engine.mode(), Mode::FullAuto);
        assert_eq!(
            engine
                .decide(
                    &req("bash", Action::Execute),
                    CancellationToken::new(),
                    panic_ask,
                )
                .await,
            Outcome::Run
        );
    }

    #[tokio::test]
    async fn suggest_asks_for_edit_and_execute() {
        let engine = PermissionEngine::new(Mode::Suggest);
        for action in [Action::Edit, Action::Execute] {
            let outcome = decide(&engine, &req("tool", action), Decision::AllowOnce).await;
            assert_eq!(outcome, Outcome::Run);
        }
    }

    #[tokio::test]
    async fn ask_denies_when_user_denies() {
        let engine = PermissionEngine::new(Mode::Suggest);
        let outcome = decide(&engine, &req("bash", Action::Execute), Decision::Deny).await;
        assert_eq!(outcome, Outcome::Deny);
    }

    #[tokio::test]
    async fn allow_always_records_and_skips_ask_next_time_for_same_tool() {
        let engine = PermissionEngine::new(Mode::Suggest);
        let first = decide(
            &engine,
            &req("bash", Action::Execute),
            Decision::AllowAlways,
        )
        .await;
        assert_eq!(first, Outcome::Run);

        let second = engine
            .decide(
                &req("bash", Action::Execute),
                CancellationToken::new(),
                panic_ask,
            )
            .await;
        assert_eq!(second, Outcome::Run);
    }

    #[tokio::test]
    async fn allow_always_is_scoped_to_the_tool_that_was_allowed() {
        let engine = PermissionEngine::new(Mode::Suggest);
        decide(
            &engine,
            &req("bash", Action::Execute),
            Decision::AllowAlways,
        )
        .await;
        let outcome = decide(&engine, &req("write", Action::Edit), Decision::Deny).await;
        assert_eq!(outcome, Outcome::Deny);
    }

    #[tokio::test]
    async fn allow_once_does_not_record() {
        let engine = PermissionEngine::new(Mode::Suggest);
        decide(&engine, &req("bash", Action::Execute), Decision::AllowOnce).await;
        let outcome = decide(&engine, &req("bash", Action::Execute), Decision::AllowOnce).await;
        assert_eq!(outcome, Outcome::Run);
    }

    #[tokio::test]
    async fn deny_answer_does_not_record() {
        let engine = PermissionEngine::new(Mode::Suggest);
        decide(&engine, &req("bash", Action::Execute), Decision::Deny).await;
        let outcome = decide(&engine, &req("bash", Action::Execute), Decision::Deny).await;
        assert_eq!(outcome, Outcome::Deny);
    }
}
