use serde_json::value::RawValue;
use tokio_agent_core::message::{ContentBlock, Message, ProviderMetadata, Role, ToolCallId};

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_owned()).expect("fixture is valid JSON")
}

const HOSTILE_JSON: &str = r#"{"zeta":"EqMBCkYIBRgCKkDsig==","alpha":2.50,"count":1e3}"#;

#[test]
fn provider_metadata_round_trips_byte_exact() {
    let msg = Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::Thinking {
            text: "…".to_owned(),
            meta: ProviderMetadata::from_provider("anthropic".to_owned(), raw(HOSTILE_JSON)),
        }],
        usage: None,
    };

    let ser1 = serde_json::to_string(&msg).unwrap();
    let de: Message = serde_json::from_str(&ser1).unwrap();
    let ser2 = serde_json::to_string(&de).unwrap();

    assert_eq!(ser1, ser2, "second serialization must be byte-identical");
    assert!(
        ser1.contains(HOSTILE_JSON),
        "metadata bytes must appear verbatim in the serialized Message"
    );

    let ContentBlock::Thinking { meta, .. } = &de.blocks[0] else {
        panic!("expected Thinking block");
    };
    assert_eq!(
        meta.get("anthropic").map(RawValue::get),
        Some(HOSTILE_JSON),
        "read-only access must return the original bytes"
    );
    assert!(meta.get("openai").is_none());
}

#[test]
fn tool_call_args_round_trip_byte_exact() {
    let msg = Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolCall {
            id: ToolCallId("call_1".to_owned()),
            name: "read".to_owned(),
            args: raw(r#"{"path":"a.rs","limit":2000,"offset":0}"#),
            meta: ProviderMetadata::default(),
        }],
        usage: None,
    };

    let ser1 = serde_json::to_string(&msg).unwrap();
    let de: Message = serde_json::from_str(&ser1).unwrap();
    let ser2 = serde_json::to_string(&de).unwrap();

    assert_eq!(ser1, ser2);
    assert!(
        ser1.contains(r#"{"path":"a.rs","limit":2000,"offset":0}"#),
        "args bytes must appear verbatim — a Value round-trip would sort the keys"
    );

    let parsed = de.blocks[0]
        .parsed_args()
        .expect("ToolCall has args")
        .expect("args are valid JSON");
    assert_eq!(parsed["path"], "a.rs");
    assert_eq!(parsed["limit"], 2000);
}
