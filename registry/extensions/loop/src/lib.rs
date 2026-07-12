use std::sync::Mutex;
use tokio_agent_extension_api::{
    DurationDto, ExtensionAction, SessionEvent, StatusSegment, StatusSide, StatusTone, TimerId,
};

wit_bindgen::generate!({ path: "../../../crates/extension-host/wit", world: "extension" });

#[derive(Default)]
struct State {
    interval_ms: u64,
    prompt: String,
}
static STATE: Mutex<State> = Mutex::new(State { interval_ms: 0, prompt: String::new() });

struct Loop;
impl Guest for Loop {
    fn on_command(_handler: String, arguments: String) -> String {
        let mut state = STATE.lock().expect("loop state");
        let argument = arguments.trim();
        if matches!(argument, "cancel" | "clear" | "stop") {
            state.interval_ms = 0;
            state.prompt.clear();
            return actions(vec![
                ExtensionAction::CancelTimer(TimerId::new("loop")),
                ExtensionAction::SetStatusSegment(status("loop: stopped")),
                ExtensionAction::ReleaseAutonomy,
                ExtensionAction::PersistSessionState(Vec::new()),
            ]);
        }
        let Some((interval, prompt)) = argument.split_once(char::is_whitespace) else {
            return actions(vec![ExtensionAction::ShowNotice {
                level: tokio_agent_extension_api::NoticeLevel::Error,
                text: "Usage: /loop <10s|5m|2h> <prompt> | cancel".into(),
            }]);
        };
        let Some(interval_ms) = parse_interval(interval) else {
            return actions(vec![ExtensionAction::ShowNotice {
                level: tokio_agent_extension_api::NoticeLevel::Error,
                text: "Invalid interval; use at least 10s, for example 5m".into(),
            }]);
        };
        state.interval_ms = interval_ms;
        state.prompt = prompt.trim().to_owned();
        actions(vec![
            ExtensionAction::SubmitPrompt { text: state.prompt.clone(), automatic: true },
            ExtensionAction::SetStatusSegment(status(&format!("loop: every {interval}"))),
            ExtensionAction::PersistSessionState(serde_json::to_vec(&(interval_ms, &state.prompt)).unwrap_or_default()),
        ])
    }

    fn on_event(event_json: String) -> String {
        let Ok(event) = serde_json::from_str::<SessionEvent>(&event_json) else { return actions(Vec::new()) };
        let mut state = STATE.lock().expect("loop state");
        if state.interval_ms == 0 { return actions(Vec::new()); }
        match event {
            SessionEvent::TimerFired { id } if id.as_str() == "loop" => actions(vec![
                ExtensionAction::SubmitPrompt { text: state.prompt.clone(), automatic: true },
            ]),
            SessionEvent::TurnFinished { .. } => actions(vec![
                ExtensionAction::ScheduleTimer { id: TimerId::new("loop"), after: DurationDto(state.interval_ms) },
            ]),
            SessionEvent::Interrupted | SessionEvent::SessionStopping => {
                state.interval_ms = 0;
                state.prompt.clear();
                actions(vec![
                    ExtensionAction::CancelTimer(TimerId::new("loop")),
                    ExtensionAction::ReleaseAutonomy,
                ExtensionAction::PersistSessionState(Vec::new()),
                ])
            }
            _ => actions(Vec::new()),
        }
    }

    fn on_tool(_handler: String, _arguments_json: String) -> String {
        r#"{"content":"Loop contributes no model tool","is_error":true}"#.into()
    }

    fn restore_session_state(bytes: Vec<u8>) {
        if let Ok((interval_ms, prompt)) = serde_json::from_slice::<(u64, String)>(&bytes) {
            let mut state = STATE.lock().expect("loop state");
            state.interval_ms = interval_ms;
            state.prompt = prompt;
        }
    }
}

fn parse_interval(value: &str) -> Option<u64> {
    let split = value.find(|character: char| !character.is_ascii_digit())?;
    let amount = value[..split].parse::<u64>().ok()?;
    let multiplier = match &value[split..] { "s" => 1_000, "m" => 60_000, "h" => 3_600_000, _ => return None };
    amount.checked_mul(multiplier).filter(|milliseconds| *milliseconds >= 10_000)
}
fn status(text: &str) -> StatusSegment {
    StatusSegment { id: "loop".into(), text: text.into(), tone: StatusTone::Normal, side: StatusSide::Left, priority: 90, min_width: 8 }
}
fn actions(values: Vec<ExtensionAction>) -> String { serde_json::to_string(&values).unwrap_or_else(|_| "[]".into()) }

export!(Loop);
