use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_agent_extension_api::{
    ApprovalSpec, ExtensionAction, ExtensionId, FrontendCapabilities, InteractionAction,
    InteractionId, InteractionRequest, InteractionResponse, InteractionSpec, InteractionTone,
    SelectOption, SingleSelectSpec, TextSection, ToolEffect, ToolGateInvocation, ToolGateResponse,
};

wit_bindgen::generate!({ path: "../../../crates/extension-host/wit", world: "extension" });

const EXTENSION_ID: &str = "tokio.permissions";
const MAX_SUMMARY_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Mode {
    Suggest,
    AutoEdit,
    FullAuto,
}

impl Default for Mode {
    fn default() -> Self {
        Self::Suggest
    }
}
impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "suggest" => Some(Self::Suggest),
            "auto-edit" => Some(Self::AutoEdit),
            "full-auto" => Some(Self::FullAuto),
            _ => None,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Suggest => "suggest",
            Self::AutoEdit => "auto-edit",
            Self::FullAuto => "full-auto",
        }
    }
    fn asks(self, effect: ToolEffect) -> bool {
        match self {
            Self::FullAuto => false,
            Self::AutoEdit => matches!(effect, ToolEffect::Execute | ToolEffect::Unknown),
            Self::Suggest => !matches!(effect, ToolEffect::Read),
        }
    }
}

#[derive(Clone)]
struct Pending {
    invocation_id: String,
    scope: String,
}

struct State {
    mode: Mode,
    generation: u64,
    pending: BTreeMap<String, Pending>,
    approved_scopes: BTreeSet<String>,
}
static STATE: Mutex<State> = Mutex::new(State {
    mode: Mode::Suggest,
    generation: 0,
    pending: BTreeMap::new(),
    approved_scopes: BTreeSet::new(),
});

struct Permissions;
impl Guest for Permissions {
    fn on_command(handler: String, _arguments: String) -> String {
        if handler != "permissions_command" {
            return actions(vec![]);
        }
        let state = STATE.lock().expect("permissions state");
        let request = InteractionRequest {
            id: InteractionId::new(format!("mode:{}", state.generation)),
            owner: ExtensionId::new(EXTENSION_ID),
            generation: state.generation,
            spec: InteractionSpec::SingleSelect(SingleSelectSpec {
                title: "Tool approval mode".into(),
                options: vec![
                    option("suggest", "Suggest", "Ask before edits and commands"),
                    option("auto-edit", "Auto-edit", "Allow edits; ask before commands"),
                    option("full-auto", "Full-auto", "Allow all tool calls"),
                ],
                selected: Some(state.mode.as_str().into()),
            }),
        };
        actions(vec![ExtensionAction::RequestInteraction(request)])
    }

    fn on_event(_event_json: String) -> String {
        actions(vec![])
    }
    fn on_tool(_handler: String, _arguments_json: String) -> String {
        r#"{"content":"Permissions contributes no model tool","is_error":true}"#.into()
    }

    fn authorize_tool(handler: String, invocation_json: String) -> String {
        if handler != "authorize_tool" {
            return gate(ToolGateResponse::Deny {
                reason: "unknown gate handler".into(),
                actions: vec![],
            });
        }
        let invocation: ToolGateInvocation = match serde_json::from_str(&invocation_json) {
            Ok(value) => value,
            Err(error) => {
                return gate(ToolGateResponse::Deny {
                    reason: format!("invalid tool invocation: {error}"),
                    actions: vec![],
                });
            }
        };
        let mut state = STATE.lock().expect("permissions state");
        if invocation.gate_owner.as_str() != EXTENSION_ID
            || invocation.gate_generation != state.generation
        {
            return gate(ToolGateResponse::Deny {
                reason: "stale tool gate generation".into(),
                actions: vec![],
            });
        }
        let scope = scope_hash(&invocation);
        if !state.mode.asks(invocation.effect) || state.approved_scopes.contains(&scope) {
            return gate(ToolGateResponse::Allow { actions: vec![] });
        }
        if !supports_approval(&invocation.frontend) {
            return gate(ToolGateResponse::Deny {
                reason: "approval is required but this frontend is non-interactive; use --permission-mode full-auto for unattended execution".into(),
                actions: vec![],
            });
        }
        let interaction_id = format!("approval:{}:{}", state.generation, invocation.invocation_id);
        state.pending.insert(
            interaction_id.clone(),
            Pending {
                invocation_id: invocation.invocation_id.clone(),
                scope,
            },
        );
        let summary = bounded_summary(&invocation);
        gate(ToolGateResponse::RequestInteraction {
            interaction: InteractionRequest {
                id: InteractionId::new(interaction_id),
                owner: ExtensionId::new(EXTENSION_ID),
                generation: state.generation,
                spec: InteractionSpec::Approval(ApprovalSpec {
                    title: "Tool approval required".into(),
                    body: vec![TextSection {
                        heading: Some(invocation.tool_name.clone()),
                        text: summary.clone(),
                    }],
                    actions: vec![
                        action("allow_once", "Allow once", "y", InteractionTone::Primary),
                        action(
                            "allow_session",
                            "Allow for session",
                            "a",
                            InteractionTone::Neutral,
                        ),
                        action("deny", "Deny", "n", InteractionTone::Destructive),
                    ],
                    copy_text: Some(summary),
                }),
            },
            actions: vec![],
        })
    }

    fn on_interaction_response(
        handler: String,
        invocation_id: String,
        response_json: String,
    ) -> String {
        if handler != "on_interaction_response" {
            return gate(ToolGateResponse::Deny {
                reason: "unknown interaction handler".into(),
                actions: vec![],
            });
        }
        let response: InteractionResponse = match serde_json::from_str(&response_json) {
            Ok(value) => value,
            Err(_) => {
                return gate(ToolGateResponse::Deny {
                    reason: "invalid interaction response".into(),
                    actions: vec![],
                });
            }
        };
        let mut state = STATE.lock().expect("permissions state");
        if response.owner.as_str() != EXTENSION_ID || response.generation != state.generation {
            return gate(ToolGateResponse::Deny {
                reason: "stale or wrong-owner interaction response".into(),
                actions: vec![],
            });
        }
        if response.id.as_str() == format!("mode:{}", state.generation) {
            let Some(mode) = Mode::parse(&response.action_id) else {
                return gate(ToolGateResponse::Deny {
                    reason: "mode selection cancelled".into(),
                    actions: vec![],
                });
            };
            state.mode = mode;
            return gate(ToolGateResponse::Allow {
                actions: vec![ExtensionAction::PersistUserState(
                    mode.as_str().as_bytes().to_vec(),
                )],
            });
        }
        let Some(pending) = state.pending.remove(response.id.as_str()) else {
            return gate(ToolGateResponse::Deny {
                reason: "stale or duplicate interaction response".into(),
                actions: vec![],
            });
        };
        if pending.invocation_id != invocation_id {
            return gate(ToolGateResponse::Deny {
                reason: "interaction does not own this invocation".into(),
                actions: vec![],
            });
        }
        match response.action_id.as_str() {
            "allow_once" => gate(ToolGateResponse::Allow { actions: vec![] }),
            "allow_session" => {
                state.approved_scopes.insert(pending.scope);
                let bytes = serde_json::to_vec(&state.approved_scopes).unwrap_or_default();
                gate(ToolGateResponse::Allow {
                    actions: vec![ExtensionAction::PersistSessionState(bytes)],
                })
            }
            "deny" | "cancel" => gate(ToolGateResponse::Deny {
                reason: "denied by user".into(),
                actions: vec![],
            }),
            _ => gate(ToolGateResponse::Deny {
                reason: "unknown interaction action".into(),
                actions: vec![],
            }),
        }
    }

    fn load_state(
        user_state: Vec<u8>,
        session_state: Vec<u8>,
        settings_json: String,
        startup_settings_json: String,
    ) {
        let mut state = STATE.lock().expect("permissions state");
        *state = State {
            mode: Mode::Suggest,
            generation: 0,
            pending: BTreeMap::new(),
            approved_scopes: BTreeSet::new(),
        };
        if let Ok(value) = std::str::from_utf8(&user_state) {
            if let Some(mode) = Mode::parse(value) {
                state.mode = mode;
            }
        }
        apply_settings(&mut state, &settings_json)
            .unwrap_or_else(|error| panic!("invalid tokio.permissions settings: {error}"));
        apply_settings(&mut state, &startup_settings_json)
            .unwrap_or_else(|error| panic!("invalid tokio.permissions startup settings: {error}"));
        if let Ok(scopes) = serde_json::from_slice(&session_state) {
            state.approved_scopes = scopes;
        }
    }

    fn restore_session_state(bytes: Vec<u8>) {
        let mut state = STATE.lock().expect("permissions state");
        state.approved_scopes = serde_json::from_slice(&bytes).unwrap_or_default();
    }
}

fn apply_settings(state: &mut State, json: &str) -> Result<(), String> {
    let value = serde_json::from_str::<Value>(json).map_err(|error| error.to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "settings must be an object".to_owned())?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "_host_generation" | "mode" | "permission-mode"
        ) {
            return Err(format!("unknown key `{key}`; supported key: `mode`"));
        }
    }
    if let Some(value) = object.get("_host_generation") {
        state.generation = value
            .as_u64()
            .ok_or_else(|| "`_host_generation` must be an integer".to_owned())?;
    }
    for key in ["mode", "permission-mode"] {
        if let Some(value) = object.get(key) {
            let value = value
                .as_str()
                .ok_or_else(|| format!("`{key}` must be a string"))?;
            state.mode = Mode::parse(value).ok_or_else(|| {
                format!("invalid `{key}` value `{value}`; use suggest, auto-edit, or full-auto")
            })?;
        }
    }
    Ok(())
}
fn option(id: &str, label: &str, description: &str) -> SelectOption {
    SelectOption {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
    }
}
fn action(id: &str, label: &str, key: &str, tone: InteractionTone) -> InteractionAction {
    InteractionAction {
        id: id.into(),
        label: label.into(),
        key_hint: Some(key.into()),
        tone,
    }
}
fn supports_approval(frontend: &FrontendCapabilities) -> bool {
    frontend.interactive
        && frontend
            .interaction_kinds
            .iter()
            .any(|kind| kind == "approval")
}
fn bounded_summary(invocation: &ToolGateInvocation) -> String {
    let raw = invocation.summary_hint.clone().unwrap_or_else(|| {
        format!(
            "{} {}",
            invocation.tool_name,
            canonical_json(&invocation.arguments)
        )
    });
    let clean: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_SUMMARY_CHARS)
        .collect();
    if raw.chars().count() > MAX_SUMMARY_CHARS {
        format!("{clean}…")
    } else {
        clean
    }
}
fn scope_hash(invocation: &ToolGateInvocation) -> String {
    let material = json!({ "owner": invocation.owner, "tool": invocation.tool_name, "cwd": lexical_path(&invocation.cwd, &invocation.cwd), "operation": scope_operation(invocation) });
    format!("{:x}", Sha256::digest(canonical_json(&material).as_bytes()))
}
fn scope_operation(invocation: &ToolGateInvocation) -> Value {
    match invocation.tool_name.as_str() {
        "bash" => {
            json!({ "command": invocation.arguments.get("command").and_then(Value::as_str).unwrap_or(""), "cwd": lexical_path(&invocation.cwd, &invocation.cwd) })
        }
        "write" | "edit" | "multi_edit" => {
            let path = invocation
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({ "family": "edit", "paths": [lexical_path(path, &invocation.cwd)] })
        }
        "read" | "grep" | "glob" => {
            let target = invocation
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({ "target": lexical_path(target, &invocation.cwd) })
        }
        _ => canonical_value(&invocation.arguments),
    }
}
fn lexical_path(value: &str, cwd: &str) -> String {
    let path = Path::new(value);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd).join(path)
    };
    let mut output = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    output.to_string_lossy().into_owned()
}
fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), canonical_value(v)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(canonical_value).collect()),
        value => value.clone(),
    }
}
fn canonical_json(value: &Value) -> String {
    serde_json::to_string(&canonical_value(value)).unwrap_or_else(|_| "null".into())
}
fn actions(values: Vec<ExtensionAction>) -> String {
    serde_json::to_string(&values).unwrap_or_else(|_| "[]".into())
}
fn gate(value: ToolGateResponse) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| {
        r#"{"decision":"deny","reason":"serialization failure","actions":[]}"#.into()
    })
}

export!(Permissions);

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_agent_extension_api::ToolOwner;

    fn invocation(
        id: &str,
        effect: ToolEffect,
        tool: &str,
        arguments: Value,
        cwd: &str,
        interactive: bool,
    ) -> ToolGateInvocation {
        ToolGateInvocation {
            gate_owner: ExtensionId::new(EXTENSION_ID),
            gate_generation: 7,
            invocation_id: id.into(),
            tool_name: tool.into(),
            owner: ToolOwner::BuiltIn,
            arguments,
            effect,
            cwd: cwd.into(),
            summary_hint: Some("bounded summary".into()),
            frontend: FrontendCapabilities {
                interactive,
                copy: true,
                interaction_kinds: vec!["approval".into(), "single_select".into()],
            },
        }
    }
    fn authorize(value: ToolGateInvocation) -> ToolGateResponse {
        serde_json::from_str(&Permissions::authorize_tool(
            "authorize_tool".into(),
            serde_json::to_string(&value).unwrap(),
        ))
        .unwrap()
    }

    #[test]
    fn modes_headless_and_operation_scopes_are_enforced() {
        Permissions::load_state(
            Vec::new(),
            Vec::new(),
            r#"{"_host_generation":7,"mode":"suggest"}"#.into(),
            "{}".into(),
        );
        assert!(matches!(
            authorize(invocation(
                "r",
                ToolEffect::Read,
                "read",
                json!({"path":"a"}),
                "/p",
                true
            )),
            ToolGateResponse::Allow { .. }
        ));
        let bash = invocation(
            "one",
            ToolEffect::Execute,
            "bash",
            json!({"command":"cargo test","timeout_ms":1}),
            "/p",
            true,
        );
        let request = match authorize(bash.clone()) {
            ToolGateResponse::RequestInteraction { interaction, .. } => interaction,
            other => panic!("unexpected {other:?}"),
        };
        let response = InteractionResponse {
            id: request.id,
            owner: request.owner,
            generation: request.generation,
            action_id: "allow_session".into(),
        };
        assert!(matches!(
            serde_json::from_str::<ToolGateResponse>(&Permissions::on_interaction_response(
                "on_interaction_response".into(),
                "one".into(),
                serde_json::to_string(&response).unwrap()
            ))
            .unwrap(),
            ToolGateResponse::Allow { .. }
        ));
        let mut same = bash.clone();
        same.invocation_id = "two".into();
        same.arguments["timeout_ms"] = 99.into();
        assert!(
            matches!(authorize(same), ToolGateResponse::Allow { .. }),
            "scheduling-only fields must not widen Bash scope"
        );
        let changed = invocation(
            "three",
            ToolEffect::Execute,
            "bash",
            json!({"command":"cargo test | cat"}),
            "/p",
            true,
        );
        assert!(matches!(
            authorize(changed),
            ToolGateResponse::RequestInteraction { .. }
        ));
        let other_cwd = invocation(
            "four",
            ToolEffect::Execute,
            "bash",
            json!({"command":"cargo test"}),
            "/other",
            true,
        );
        assert!(matches!(
            authorize(other_cwd),
            ToolGateResponse::RequestInteraction { .. }
        ));
        let headless = invocation(
            "five",
            ToolEffect::Execute,
            "bash",
            json!({"command":"date"}),
            "/p",
            false,
        );
        match authorize(headless) {
            ToolGateResponse::Deny { reason, .. } => {
                assert!(reason.contains("--permission-mode full-auto"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
