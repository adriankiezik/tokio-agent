use std::sync::Mutex;
use tokio_agent_extension_api::{
    ExtensionAction, ExtensionId, SessionEvent, StatusSegment, StatusSide, StatusTone,
    ToolDescriptor, ToolId,
};

wit_bindgen::generate!({ path: "../../../crates/extension-host/wit", world: "extension" });

#[derive(Default)]
struct State { objective: String, active: bool, paused: bool }
static STATE: Mutex<State> = Mutex::new(State { objective: String::new(), active: false, paused: false });

struct Goal;
impl Guest for Goal {
    fn on_command(_handler: String, arguments: String) -> String {
        let mut state = STATE.lock().expect("goal state");
        match arguments.trim() {
            "cancel" | "clear" => {
                state.active = false;
                state.objective.clear();
                actions(deactivate("goal: cancelled"))
            }
            "pause" if state.active => {
                state.paused = true;
                actions(vec![
                    ExtensionAction::SetStatusSegment(status("goal: paused")),
                    ExtensionAction::PersistSessionState(persist(&state)),
                ])
            }
            "resume" if state.active => {
                state.paused = false;
                actions(vec![
                    ExtensionAction::SubmitPrompt { text: continuation(&state.objective), automatic: true },
                    ExtensionAction::SetStatusSegment(status("goal: active")),
                    ExtensionAction::PersistSessionState(persist(&state)),
                ])
            }
            "" => notice("Usage: /goal <objective> | pause | resume | cancel"),
            control @ ("pause" | "resume") => notice(&format!("Cannot {control}; no goal is active")),
            objective => {
                state.objective = objective.to_owned();
                state.active = true;
                state.paused = false;
                actions(vec![
                    // Acquire the single autonomous-owner slot before exposing
                    // any other contribution, so a conflict has no partial effects.
                    ExtensionAction::SubmitPrompt { text: format!("Work autonomously toward this goal:\n\n{objective}\n\nContinue until verified complete or genuinely blocked."), automatic: true },
                    ExtensionAction::RegisterTool(tool()),
                    ExtensionAction::SetStatusSegment(status("goal: active")),
                    ExtensionAction::PersistSessionState(persist(&state)),
                ])
            }
        }
    }

    fn on_event(event_json: String) -> String {
        let Ok(event) = serde_json::from_str::<SessionEvent>(&event_json) else { return actions(Vec::new()) };
        let mut state = STATE.lock().expect("goal state");
        if !state.active { return actions(Vec::new()); }
        match event {
            SessionEvent::TurnFinished { .. } if !state.paused => actions(vec![
                ExtensionAction::SubmitPrompt { text: continuation(&state.objective), automatic: true },
            ]),
            SessionEvent::Interrupted => {
                state.paused = true;
                actions(vec![
                    ExtensionAction::SetStatusSegment(status("goal: paused")),
                    ExtensionAction::PersistSessionState(persist(&state)),
                ])
            }
            SessionEvent::SessionStopping => actions(Vec::new()),
            _ => actions(Vec::new()),
        }
    }

    fn on_tool(_handler: String, arguments_json: String) -> String {
        let value: serde_json::Value = serde_json::from_str(&arguments_json).unwrap_or_default();
        let outcome = value.get("status").and_then(serde_json::Value::as_str).unwrap_or("");
        if !matches!(outcome, "complete" | "blocked") {
            return r#"{"content":"status must be complete or blocked","is_error":true}"#.into();
        }
        let mut state = STATE.lock().expect("goal state");
        state.active = false;
        state.paused = false;
        serde_json::json!({
            "content": format!("goal marked {outcome}"),
            "is_error": false,
            "actions": deactivate(&format!("goal: {outcome}")),
        }).to_string()
    }

    fn restore_session_state(bytes: Vec<u8>) {
        let mut state = STATE.lock().expect("goal state");
        if let Ok((objective, active, paused)) =
            serde_json::from_slice::<(String, bool, bool)>(&bytes)
        {
            state.objective = objective;
            state.active = active;
            state.paused = paused;
        } else {
            *state = State::default();
        }
    }
}

fn persist(state: &State) -> Vec<u8> {
    serde_json::to_vec(&(state.objective.as_str(), state.active, state.paused))
        .unwrap_or_default()
}

fn tool() -> ToolDescriptor {
    ToolDescriptor {
        id: ToolId::new("tokio.official.goal:update_goal"),
        name: "update_goal".into(),
        description: "Mark the active goal complete or blocked".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "status": { "type": "string", "enum": ["complete", "blocked"] } },
            "required": ["status"], "additionalProperties": false
        }),
        owner: ExtensionId::new("tokio.official.goal"),
        permission: tokio_agent_extension_api::ToolPermission::Read,
    }
}
fn deactivate(text: &str) -> Vec<ExtensionAction> {
    vec![
        ExtensionAction::UnregisterTool(ToolId::new("tokio.official.goal:update_goal")),
        ExtensionAction::SetStatusSegment(status(text)),
        ExtensionAction::PersistSessionState(Vec::new()),
        ExtensionAction::ReleaseAutonomy,
    ]
}
fn continuation(objective: &str) -> String { format!("Continue working toward the active goal. Verify progress and keep going until complete or genuinely blocked.\n\nGoal: {objective}") }
fn status(text: &str) -> StatusSegment { StatusSegment { id: "goal".into(), text: text.into(), tone: StatusTone::Normal, side: StatusSide::Left, priority: 100, min_width: 8 } }
fn notice(text: &str) -> String { actions(vec![ExtensionAction::ShowNotice { level: tokio_agent_extension_api::NoticeLevel::Error, text: text.into() }]) }
fn actions(values: Vec<ExtensionAction>) -> String { serde_json::to_string(&values).unwrap_or_else(|_| "[]".into()) }

export!(Goal);
