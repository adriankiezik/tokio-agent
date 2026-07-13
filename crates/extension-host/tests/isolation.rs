use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use tokio_agent_extension_api::{
    COMPANION_PROTOCOL_VERSION, ExtensionAction, ExtensionId, HOST_API_VERSION, HostRequest,
    HostResponse, RuntimeLimits,
};
#[test]
fn scripts_are_isolated_and_official_extensions_run_in_the_shared_runtime() {
    let temporary = tempfile::tempdir().unwrap();
    let script_path = temporary.path().join("trap.js");
    std::fs::write(
        &script_path,
        "tokio.defineExtension({ onCommand() { while (true) {} } });",
    )
    .unwrap();

    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/extension-host-test-cache");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tokio-agent-extension-host"))
        .arg("--cache-dir")
        .arg(cache)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    assert!(matches!(
        exchange(
            &mut stdin,
            &mut stdout,
            HostRequest::Handshake {
                protocol_version: COMPANION_PROTOCOL_VERSION,
                host_api: HOST_API_VERSION.into(),
            }
        ),
        HostResponse::Handshake { .. }
    ));
    let id = ExtensionId::new("example.test.trap");
    let load_response = exchange(
        &mut stdin,
        &mut stdout,
        HostRequest::Load {
            extension: id.clone(),
            generation: 1,
            script_path: script_path.to_string_lossy().into_owned(),
            capabilities: Vec::new(),
            limits: RuntimeLimits {
                callback_deadline_ms: 100,
                fuel_per_callback: u64::MAX,
                ..RuntimeLimits::default()
            },
            user_state: Vec::new(),
            settings: serde_json::json!({}),
            startup_settings: serde_json::json!({}),
        },
    );
    assert!(
        matches!(load_response, HostResponse::Loaded { .. }),
        "{load_response:?}"
    );
    let started = std::time::Instant::now();
    assert!(matches!(
        exchange(
            &mut stdin,
            &mut stdout,
            HostRequest::InvokeCommand {
                extension: id,
                generation: 1,
                handler: "trap".into(),
                arguments: String::new(),
            }
        ),
        HostResponse::Error { .. }
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    let security_path = temporary.path().join("security.js");
    std::fs::write(
        &security_path,
        r#"
          if ([typeof process, typeof require, typeof fetch, typeof WebAssembly]
              .some((value) => value !== "undefined")) throw new Error("unsafe global exposed");
          if (!Object.isFrozen(tokio) || !Object.isFrozen(tokio.actions))
            throw new Error("extension API is mutable");
          tokio.defineExtension({ onCommand() { return [tokio.actions.fetch("data", "https://example.com")]; } });
        "#,
    )
    .unwrap();
    assert!(matches!(
        exchange(
            &mut stdin,
            &mut stdout,
            HostRequest::ValidateScript {
                script_path: security_path.to_string_lossy().into_owned(),
            }
        ),
        HostResponse::ScriptValid
    ));

    let invalid_path = temporary.path().join("invalid.js");
    std::fs::write(&invalid_path, "tokio.defineExtension({").unwrap();
    let invalid = exchange(
        &mut stdin,
        &mut stdout,
        HostRequest::ValidateScript {
            script_path: invalid_path.to_string_lossy().into_owned(),
        },
    );
    assert!(
        matches!(&invalid, HostResponse::Error { message, .. }
            if message.contains("invalid extension JavaScript")
                && message.contains("invalid property name")
                && message.contains("eval_script:1")),
        "{invalid:?}"
    );

    let registry =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../registry/extensions");
    for (name, handler, arguments, expected) in [
        ("loop", "loop_command", "10s keep going", "submit_prompt"),
        ("goal", "goal_command", "ship the feature", "register_tool"),
        (
            "permissions",
            "permissions_command",
            "",
            "request_interaction",
        ),
    ] {
        let extension = ExtensionId::new(format!("tokio.{name}"));
        let script_path = registry.join(name).join("dist/extension.js");
        let loaded = exchange(
            &mut stdin,
            &mut stdout,
            HostRequest::Load {
                extension: extension.clone(),
                generation: 1,
                script_path: script_path.to_string_lossy().into_owned(),
                capabilities: Vec::new(),
                limits: RuntimeLimits::default(),
                user_state: Vec::new(),
                settings: serde_json::json!({}),
                startup_settings: serde_json::json!({}),
            },
        );
        assert!(matches!(loaded, HostResponse::Loaded { .. }), "{loaded:?}");
        let response = exchange(
            &mut stdin,
            &mut stdout,
            HostRequest::InvokeCommand {
                extension,
                generation: 1,
                handler: handler.into(),
                arguments: arguments.into(),
            },
        );
        let HostResponse::Actions(actions) = response else {
            panic!("{name} returned {response:?}");
        };
        assert!(actions.iter().any(|action| matches!(
            (&action.value, expected),
            (ExtensionAction::SubmitPrompt { .. }, "submit_prompt")
                | (ExtensionAction::RegisterTool(_), "register_tool")
                | (
                    ExtensionAction::RequestInteraction(_),
                    "request_interaction"
                )
        )));
    }

    assert!(matches!(
        exchange(
            &mut stdin,
            &mut stdout,
            HostRequest::ValidateScript {
                script_path: script_path.to_string_lossy().into_owned(),
            }
        ),
        HostResponse::ScriptValid
    ));
    let _ = exchange(&mut stdin, &mut stdout, HostRequest::Shutdown);
    assert!(child.wait().unwrap().success());
}

fn exchange(
    stdin: &mut impl Write,
    stdout: &mut impl BufRead,
    request: HostRequest,
) -> HostResponse {
    serde_json::to_writer(&mut *stdin, &request).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    let mut response = String::new();
    stdout.read_line(&mut response).unwrap();
    assert!(
        !response.is_empty(),
        "companion exited before responding to {request:?}"
    );
    serde_json::from_str(&response).unwrap()
}
