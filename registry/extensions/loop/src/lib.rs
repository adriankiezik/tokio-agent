use std::sync::Mutex;
use tokio_agent_extension_api::{
    DurationDto, ExtensionAction, NoticeLevel, SessionEvent, StatusSegment, StatusSide, StatusTone,
    TimerId,
};

wit_bindgen::generate!({ path: "../../../crates/extension-host/wit", world: "extension" });

const LOOP_TIMER: &str = "loop";
const SECOND_MS: u64 = 1_000;
const MINUTE_MS: u64 = 60 * SECOND_MS;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

#[derive(Default)]
struct State {
    interval_ms: u64,
    remaining_ms: u64,
    scheduled_ms: u64,
    prompt: String,
}
static STATE: Mutex<State> = Mutex::new(State {
    interval_ms: 0,
    remaining_ms: 0,
    scheduled_ms: 0,
    prompt: String::new(),
});

struct Loop;
impl Guest for Loop {
    fn authorize_tool(_handler: String, _invocation_json: String) -> String {
        r#"{"decision":"deny","reason":"loop is not a tool gate","actions":[]}"#.into()
    }
    fn on_interaction_response(
        _handler: String,
        _invocation_id: String,
        _response_json: String,
    ) -> String {
        r#"{"decision":"deny","reason":"loop has no interactions","actions":[]}"#.into()
    }
    fn load_state(
        _user_state: Vec<u8>,
        session_state: Vec<u8>,
        _settings_json: String,
        _startup_settings_json: String,
    ) {
        Self::restore_session_state(session_state);
    }
    fn on_command(_handler: String, arguments: String) -> String {
        let mut state = STATE.lock().expect("loop state");
        let argument = arguments.trim();
        if matches!(argument, "cancel" | "clear" | "stop") {
            *state = State::default();
            return actions(vec![
                ExtensionAction::CancelTimer(TimerId::new(LOOP_TIMER)),
                ExtensionAction::ClearStatusSegment("loop".into()),
                ExtensionAction::ShowNotice {
                    level: NoticeLevel::Info,
                    text: "loop: stopped".into(),
                },
                ExtensionAction::ReleaseAutonomy,
                ExtensionAction::PersistSessionState(Vec::new()),
            ]);
        }
        let Some((interval, prompt)) = argument.split_once(char::is_whitespace) else {
            return actions(vec![ExtensionAction::ShowNotice {
                level: NoticeLevel::Error,
                text: "Usage: /loop <10s|5m|2h> <prompt> | cancel".into(),
            }]);
        };
        let Some(interval_ms) = parse_interval(interval) else {
            return actions(vec![ExtensionAction::ShowNotice {
                level: NoticeLevel::Error,
                text: "Invalid interval; use at least 10s, for example 5m".into(),
            }]);
        };
        state.interval_ms = interval_ms;
        state.prompt = prompt.trim().to_owned();
        let prompt = state.prompt.clone();
        let mut result = begin_run(&mut state, prompt);
        result.push(ExtensionAction::PersistSessionState(
            serde_json::to_vec(&(interval_ms, &state.prompt)).unwrap_or_default(),
        ));
        actions(result)
    }

    fn on_event(event_json: String) -> String {
        let Ok(event) = serde_json::from_str::<SessionEvent>(&event_json) else {
            return actions(Vec::new());
        };
        let mut state = STATE.lock().expect("loop state");
        if state.interval_ms == 0 {
            return actions(Vec::new());
        }
        match event {
            SessionEvent::TimerFired { id } if id.as_str() == LOOP_TIMER => {
                state.remaining_ms = state.remaining_ms.saturating_sub(state.scheduled_ms);
                if state.remaining_ms == 0 {
                    let prompt = state.prompt.clone();
                    actions(begin_run(&mut state, prompt))
                } else {
                    actions(schedule_countdown_step(&mut state))
                }
            }
            SessionEvent::TurnFinished { .. } | SessionEvent::Interrupted => {
                actions(arm_countdown(&mut state))
            }
            SessionEvent::SessionStarted => actions(arm_countdown(&mut state)),
            SessionEvent::SessionStopping => {
                *state = State::default();
                actions(vec![
                    ExtensionAction::CancelTimer(TimerId::new(LOOP_TIMER)),
                    ExtensionAction::ReleaseAutonomy,
                    ExtensionAction::ClearStatusSegment("loop".into()),
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
        let mut state = STATE.lock().expect("loop state");
        if let Ok((interval_ms, prompt)) = serde_json::from_slice::<(u64, String)>(&bytes) {
            state.interval_ms = interval_ms;
            state.remaining_ms = interval_ms;
            state.scheduled_ms = 0;
            state.prompt = prompt;
        } else {
            *state = State::default();
        }
    }
}

fn begin_run(state: &mut State, prompt: String) -> Vec<ExtensionAction> {
    state.remaining_ms = 0;
    state.scheduled_ms = 0;
    vec![
        ExtensionAction::CancelTimer(TimerId::new(LOOP_TIMER)),
        ExtensionAction::SubmitPrompt {
            text: prompt,
            automatic: true,
        },
        ExtensionAction::SetStatusSegment(status("loop: running")),
    ]
}

fn arm_countdown(state: &mut State) -> Vec<ExtensionAction> {
    state.remaining_ms = state.interval_ms;
    schedule_countdown_step(state)
}

fn schedule_countdown_step(state: &mut State) -> Vec<ExtensionAction> {
    state.scheduled_ms = countdown_step(state.remaining_ms);
    vec![
        ExtensionAction::ScheduleTimer {
            id: TimerId::new(LOOP_TIMER),
            after: DurationDto(state.scheduled_ms),
        },
        ExtensionAction::SetStatusSegment(status(&format!(
            "loop: next in {}",
            format_duration(state.remaining_ms)
        ))),
    ]
}

fn countdown_step(remaining_ms: u64) -> u64 {
    let unit = if remaining_ms <= MINUTE_MS {
        SECOND_MS
    } else if remaining_ms <= HOUR_MS {
        MINUTE_MS
    } else if remaining_ms <= DAY_MS {
        HOUR_MS
    } else {
        DAY_MS
    };
    remaining_ms.min(unit)
}

fn format_duration(milliseconds: u64) -> String {
    let (amount, suffix) = if milliseconds < MINUTE_MS {
        (milliseconds.div_ceil(SECOND_MS), "s")
    } else if milliseconds < HOUR_MS {
        (milliseconds.div_ceil(MINUTE_MS), "m")
    } else if milliseconds < DAY_MS {
        (milliseconds.div_ceil(HOUR_MS), "h")
    } else {
        (milliseconds.div_ceil(DAY_MS), "d")
    };
    format!("{amount}{suffix}")
}

fn parse_interval(value: &str) -> Option<u64> {
    let split = value.find(|character: char| !character.is_ascii_digit())?;
    let amount = value[..split].parse::<u64>().ok()?;
    let multiplier = match &value[split..] {
        "s" => SECOND_MS,
        "m" => MINUTE_MS,
        "h" => HOUR_MS,
        _ => return None,
    };
    amount
        .checked_mul(multiplier)
        .filter(|milliseconds| *milliseconds >= 10_000)
}
fn status(text: &str) -> StatusSegment {
    StatusSegment {
        id: "loop".into(),
        text: text.into(),
        tone: StatusTone::Normal,
        side: StatusSide::Left,
        priority: 90,
        min_width: 8,
    }
}
fn actions(values: Vec<ExtensionAction>) -> String {
    serde_json::to_string(&values).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(event: SessionEvent) -> Vec<ExtensionAction> {
        let result = Loop::on_event(serde_json::to_string(&event).unwrap());
        serde_json::from_str(&result).unwrap()
    }

    #[test]
    fn status_switches_between_running_and_the_countdown() {
        let result = Loop::on_command(String::new(), "10s check status".into());
        let initial: Vec<ExtensionAction> = serde_json::from_str(&result).unwrap();
        assert!(initial.contains(&ExtensionAction::SetStatusSegment(status("loop: running"))));
        assert!(
            !initial
                .iter()
                .any(|action| matches!(action, ExtensionAction::ScheduleTimer { .. }))
        );

        let finished = event(SessionEvent::TurnFinished {
            stop: tokio_agent_extension_api::StopReason::EndTurn,
            usage: tokio_agent_extension_api::Usage::default(),
        });
        assert!(finished.contains(&ExtensionAction::ScheduleTimer {
            id: TimerId::new(LOOP_TIMER),
            after: DurationDto(SECOND_MS),
        }));
        assert!(finished.contains(&ExtensionAction::SetStatusSegment(status(
            "loop: next in 10s"
        ))));

        for expected in (1..10).rev() {
            let tick = event(SessionEvent::TimerFired {
                id: TimerId::new(LOOP_TIMER),
            });
            assert!(
                tick.contains(&ExtensionAction::SetStatusSegment(status(&format!(
                    "loop: next in {expected}s"
                ))))
            );
        }
        let due = event(SessionEvent::TimerFired {
            id: TimerId::new(LOOP_TIMER),
        });
        assert!(due.contains(&ExtensionAction::SubmitPrompt {
            text: "check status".into(),
            automatic: true,
        }));
        assert!(due.contains(&ExtensionAction::SetStatusSegment(status("loop: running"))));
        assert!(
            !due.iter()
                .any(|action| matches!(action, ExtensionAction::ScheduleTimer { .. }))
        );

        let next = event(SessionEvent::TurnFinished {
            stop: tokio_agent_extension_api::StopReason::EndTurn,
            usage: tokio_agent_extension_api::Usage::default(),
        });
        assert!(next.contains(&ExtensionAction::SetStatusSegment(status(
            "loop: next in 10s"
        ))));
    }

    #[test]
    fn cancel_clears_the_timer_status_and_autonomy() {
        let _ = Loop::on_command(String::new(), "10s check status".into());
        let result = Loop::on_command(String::new(), "cancel".into());
        let cancelled: Vec<ExtensionAction> = serde_json::from_str(&result).unwrap();

        assert!(cancelled.contains(&ExtensionAction::CancelTimer(TimerId::new(LOOP_TIMER))));
        assert!(cancelled.contains(&ExtensionAction::ClearStatusSegment("loop".into())));
        assert!(cancelled.contains(&ExtensionAction::ReleaseAutonomy));
        assert!(cancelled.contains(&ExtensionAction::PersistSessionState(Vec::new())));
        assert!(
            event(SessionEvent::TimerFired {
                id: TimerId::new(LOOP_TIMER),
            })
            .is_empty()
        );
    }

    #[test]
    fn countdown_uses_an_appropriate_display_unit() {
        assert_eq!(format_duration(10_000), "10s");
        assert_eq!(format_duration(90_000), "2m");
        assert_eq!(format_duration(5 * HOUR_MS), "5h");
        assert_eq!(format_duration(49 * HOUR_MS), "3d");
    }
}

export!(Loop);
