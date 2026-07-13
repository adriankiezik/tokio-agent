use std::cell::RefCell;

use rquickjs::{CatchResultExt, Context, Runtime as JsRuntime};

wit_bindgen::generate!({ path: "../../wit", world: "extension" });

thread_local! {
    static VM: RefCell<Option<JsVm>> = const { RefCell::new(None) };
}

struct ExtensionRuntime;

struct JsVm {
    context: Context,
    _runtime: JsRuntime,
}

impl Guest for ExtensionRuntime {
    fn validate_source(source: String) -> String {
        let runtime = match JsRuntime::new() {
            Ok(runtime) => runtime,
            Err(error) => return error.to_string(),
        };
        runtime.set_memory_limit(8 * 1024 * 1024);
        runtime.set_max_stack_size(128 * 1024);
        let context = match Context::full(&runtime) {
            Ok(context) => context,
            Err(error) => return error.to_string(),
        };
        context.with(|context| {
            if let Err(error) = context
                .eval::<(), _>(include_str!("api.js"))
                .catch(&context)
            {
                return error.to_string();
            }
            context
                .eval::<(), _>(source)
                .catch(&context)
                .err()
                .map_or_else(String::new, |error| error.to_string())
        })
    }

    fn on_command(handler: String, arguments: String) -> String {
        invoke("onCommand", &[handler, arguments])
    }

    fn on_event(event_json: String) -> String {
        invoke("onEvent", &[event_json])
    }

    fn on_tool(handler: String, arguments_json: String) -> String {
        invoke("onTool", &[handler, arguments_json])
    }

    fn authorize_tool(handler: String, invocation_json: String) -> String {
        invoke("authorizeTool", &[handler, invocation_json])
    }

    fn on_interaction_response(
        handler: String,
        invocation_id: String,
        response_json: String,
    ) -> String {
        invoke(
            "onInteractionResponse",
            &[handler, invocation_id, response_json],
        )
    }

    fn load_state(
        user_state: Vec<u8>,
        session_state: Vec<u8>,
        settings_json: String,
        startup_settings_json: String,
    ) {
        let mut settings = serde_json::from_str::<serde_json::Value>(&settings_json)
            .expect("settings must be valid JSON");
        let source = take_host_string(&mut settings, "_tokio_source")
            .expect("host must provide extension source");
        let extension_id = take_host_string(&mut settings, "_tokio_extension_id")
            .expect("host must provide extension identity");
        let generation = settings
            .get("_host_generation")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        settings
            .as_object_mut()
            .expect("settings must be an object")
            .remove("_host_generation");
        let settings_json = settings.to_string();

        VM.with_borrow_mut(|slot| {
            let runtime = JsRuntime::new().expect("creating the QuickJS runtime");
            runtime.set_memory_limit(8 * 1024 * 1024);
            runtime.set_max_stack_size(128 * 1024);
            let context = Context::full(&runtime).expect("creating the QuickJS context");
            context.with(|context| {
                context
                    .eval::<(), _>(include_str!("api.js"))
                    .catch(&context)
                    .unwrap_or_else(|error| panic!("loading the Tokio extension API: {error}"));
                context
                    .eval::<(), _>(source)
                    .catch(&context)
                    .unwrap_or_else(|error| panic!("evaluating extension JavaScript: {error}"));
            });
            *slot = Some(JsVm {
                context,
                _runtime: runtime,
            });
        });

        let host_json = serde_json::json!({
            "extensionId": extension_id,
            "generation": generation,
        })
        .to_string();
        invoke_void(
            "loadState",
            &[
                json_bytes(&user_state),
                json_bytes(&session_state),
                settings_json,
                startup_settings_json,
                host_json,
            ],
        );
    }

    fn restore_session_state(state: Vec<u8>) {
        invoke_void("restoreSessionState", &[json_bytes(&state)]);
    }
}

fn take_host_string(settings: &mut serde_json::Value, key: &str) -> Option<String> {
    settings
        .as_object_mut()?
        .remove(key)?
        .as_str()
        .map(str::to_owned)
}

fn json_bytes(bytes: &[u8]) -> String {
    serde_json::to_string(bytes).expect("serializing state")
}

fn invoke(name: &str, arguments: &[String]) -> String {
    evaluate(name, arguments).unwrap_or_else(|error| panic!("{name} failed: {error}"))
}

fn invoke_void(name: &str, arguments: &[String]) {
    let expression =
        expression(name, arguments).unwrap_or_else(|error| panic!("{name} failed: {error}"));
    VM.with_borrow(|slot| {
        let vm = slot.as_ref().expect("extension JavaScript is not loaded");
        vm.context.with(|context| {
            context
                .eval::<(), _>(expression)
                .catch(&context)
                .unwrap_or_else(|error| panic!("{name} failed: {error}"));
        });
    });
}

fn evaluate(name: &str, arguments: &[String]) -> anyhow::Result<String> {
    let expression = expression(name, arguments)?;
    VM.with_borrow(|slot| {
        let vm = slot.as_ref().expect("extension JavaScript is not loaded");
        vm.context.with(|context| {
            context
                .eval::<String, _>(expression)
                .catch(&context)
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
    })
}

fn expression(name: &str, arguments: &[String]) -> anyhow::Result<String> {
    let arguments = arguments
        .iter()
        .map(|argument| serde_json::to_string(argument))
        .collect::<Result<Vec<_>, _>>()?
        .join(",");
    Ok(format!("globalThis.__tokio.{name}({arguments})"))
}

export!(ExtensionRuntime);
