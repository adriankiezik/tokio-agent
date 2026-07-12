use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use tokio_agent_extension_api::{
    COMPANION_PROTOCOL_VERSION, ExtensionId, HOST_API_VERSION, HostRequest, HostResponse,
    RuntimeLimits,
};
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::{ManglingAndAbi, Resolve};

#[test]
fn an_infinite_component_is_deadline_limited_and_the_companion_stays_available() {
    let temporary = tempfile::tempdir().unwrap();
    let component_path = temporary.path().join("trap.wasm");
    let mut resolve = Resolve::default();
    let (package, _) = resolve
        .push_path(format!("{}/wit", env!("CARGO_MANIFEST_DIR")))
        .unwrap();
    let world = resolve.select_world(&[package], Some("extension")).unwrap();
    let module = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    let wat = wasmprinter::print_bytes(&module).unwrap();
    let wat = wat
        .replace("(memory (;0;) 0)", "(memory (;0;) 1)")
        .replace(
            "(func (;10;) (type 4) (param i32 i32 i32 i32 i32 i32 i32 i32)\n    unreachable\n  )",
            "(func (;10;) (type 4) (param i32 i32 i32 i32 i32 i32 i32 i32))",
        )
        .replace(
            "(func (;14;) (type 0) (param i32 i32 i32 i32) (result i32)\n    unreachable\n  )",
            "(func (;14;) (type 0) (param i32 i32 i32 i32) (result i32) i32.const 0)",
        );
    let mut module =
        wat::parse_str(wat.replacen("unreachable", "loop br 0 end unreachable", 1)).unwrap();
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    let component = ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap();
    std::fs::write(&component_path, component).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_tokio-agent-extension-host"))
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
            component_path: component_path.to_string_lossy().into_owned(),
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
    assert!(matches!(
        exchange(
            &mut stdin,
            &mut stdout,
            HostRequest::ValidateComponent {
                component_path: component_path.to_string_lossy().into_owned(),
            }
        ),
        HostResponse::ComponentValid
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
