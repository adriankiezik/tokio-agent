use tokio_agent_core::Event;
use tokio_agent_core::event::StopReason;
use tokio_agent_core::message::ContentBlock;
use tokio_agent_provider::conformance::{
    assemble_anthropic, assemble_openai, assembled_message, parse_sse_fixture,
};

const ANTHROPIC_FIXTURE: &str = include_str!("../fixtures/anthropic/signed_thinking_tool_use.sse");
const OPENAI_FIXTURE: &str = include_str!("../fixtures/openai/tool_call.sse");

#[test]
fn anthropic_fixture_parses() {
    let frames = parse_sse_fixture(ANTHROPIC_FIXTURE);
    assert_eq!(frames.len(), 11);
    let events: Vec<_> = frames.iter().map(|f| f.event.as_deref()).collect();
    assert_eq!(events[0], Some("message_start"));
    assert_eq!(events[10], Some("message_stop"));
    assert!(
        frames.iter().any(|f| f.data.contains("signature_delta")),
        "fixture must exercise signed thinking"
    );
}

#[test]
fn openai_fixture_parses() {
    let frames = parse_sse_fixture(OPENAI_FIXTURE);
    assert_eq!(frames.len(), 9);
    let events: Vec<_> = frames.iter().map(|f| f.event.as_deref()).collect();
    assert_eq!(events[0], Some("response.created"));
    assert_eq!(events[8], Some("response.completed"));
    assert!(
        frames
            .iter()
            .any(|f| f.data.contains("reasoning_summary_text.delta")),
        "fixture must exercise reasoning summaries"
    );
    assert!(
        frames
            .iter()
            .any(|f| f.data.contains("function_call_arguments.delta")),
        "fixture must exercise fragmented tool-call args"
    );
}

#[test]
fn openai_adapter_assembles_reasoning_and_fragmented_tool_call() {
    let frames = parse_sse_fixture(OPENAI_FIXTURE);
    let events = assemble_openai(&frames);

    let render: Vec<&Event> = events
        .iter()
        .filter(|e| !matches!(e, Event::Done { .. }))
        .collect();
    assert!(
        matches!(render.first(), Some(Event::ThinkingDelta { text }) if text == "I should read the file before editing it."),
        "the reasoning summary delta must render before anything else"
    );
    assert!(
        matches!(render.iter().find(|e| matches!(e, Event::ToolCallStart { .. })), Some(Event::ToolCallStart { name, .. }) if name == "read"),
        "a tool-call start must render for `read`"
    );

    let fragments: String = render
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallArgs { fragment, .. } => Some(fragment.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        fragments, r#"{"path":"src/main.rs","limit":2000}"#,
        "streamed args fragments must reassemble to the tool input"
    );
    assert!(
        render
            .iter()
            .any(|e| matches!(e, Event::ToolCallEnd { .. })),
        "the tool call must be closed with ToolCallEnd"
    );
    assert!(
        render.iter().any(|e| matches!(e, Event::Usage(_))),
        "usage must be surfaced as a render Event"
    );

    let Event::Done { stop, .. } = events.last().unwrap() else {
        panic!("stream must terminate in Done");
    };
    assert_eq!(*stop, StopReason::ToolUse);

    let message = assembled_message(&events);
    assert_eq!(message.blocks.len(), 2);

    match &message.blocks[0] {
        ContentBlock::Thinking { text, meta } => {
            assert_eq!(text, "I should read the file before editing it.");
            let raw = meta
                .get("openai")
                .expect("encrypted reasoning preserved")
                .get();
            assert!(
                raw.contains(
                    "EqMBCkYIBRgCKkDsig5o0BqNZQz7hOxc9K8AF2i1EhIvSnr3jY0mPqXW4wcT2LZUdBM6VvE1kAaGwRj0nC7yhIiaAlFvbxxJ2u9FEgxOe6nQhAo4rC1qkc0aDA=="
                ),
                "the reasoning encrypted content must be captured verbatim"
            );
        }
        other => panic!("expected Thinking, got {other:?}"),
    }

    match &message.blocks[1] {
        ContentBlock::ToolCall { id, name, args, .. } => {
            assert_eq!(id.0, "call_h7q1nZ8sVxLmT2");
            assert_eq!(name, "read");
            assert_eq!(args.get(), fragments);
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }

    let usage = message.usage.expect("assembled message carries usage");
    assert_eq!(usage.input_tokens, 472);
    assert_eq!(usage.output_tokens, 89);
    assert_eq!(usage.cache_read_tokens, 128);

    let ser1 = serde_json::to_string(message).unwrap();
    let de: tokio_agent_core::message::Message = serde_json::from_str(&ser1).unwrap();
    let ser2 = serde_json::to_string(&de).unwrap();
    assert_eq!(
        ser1, ser2,
        "assembled Message must round-trip byte-identically"
    );
}

#[test]
fn adapters_assemble_messages_with_verbatim_metadata() {
    let frames = parse_sse_fixture(ANTHROPIC_FIXTURE);
    let events = assemble_anthropic(&frames);

    let render: Vec<&Event> = events
        .iter()
        .filter(|e| !matches!(e, Event::Done { .. }))
        .collect();
    assert!(
        matches!(render.first(), Some(Event::ThinkingDelta { text }) if text == "I should read the file before editing it."),
        "the thinking delta must render before anything else"
    );
    assert!(
        matches!(render.iter().find(|e| matches!(e, Event::ToolCallStart { .. })), Some(Event::ToolCallStart { name, .. }) if name == "read"),
        "a tool-call start must render for `read`"
    );

    let fragments: String = render
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallArgs { fragment, .. } => Some(fragment.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        fragments, r#"{"path":"src/main.rs","limit":2000}"#,
        "streamed args fragments must reassemble to the tool input"
    );
    assert!(
        render
            .iter()
            .any(|e| matches!(e, Event::ToolCallEnd { .. })),
        "the tool call must be closed with ToolCallEnd"
    );
    assert!(
        render.iter().any(|e| matches!(e, Event::Usage(_))),
        "usage must be surfaced as a render Event"
    );

    let Event::Done { stop, .. } = events.last().unwrap() else {
        panic!("stream must terminate in Done");
    };
    assert_eq!(*stop, StopReason::ToolUse);

    let message = assembled_message(&events);
    assert_eq!(message.blocks.len(), 2);

    match &message.blocks[0] {
        ContentBlock::Thinking { text, meta } => {
            assert_eq!(text, "I should read the file before editing it.");
            let raw = meta.get("anthropic").expect("signature preserved").get();
            assert!(
                raw.contains(
                    "EqMBCkYIBRgCKkDsig5o0BqNZQz7hOxc9K8AF2i1EhIvSnr3jY0mPqXW4wcT2LZUdBM6VvE1kAaGwRj0nC7yhIiaAlFvbxxJ2u9FEgxOe6nQhAo4rC1qkc0aDA=="
                ),
                "the thinking signature must be captured verbatim"
            );
        }
        other => panic!("expected Thinking, got {other:?}"),
    }

    match &message.blocks[1] {
        ContentBlock::ToolCall { id, name, args, .. } => {
            assert_eq!(id.0, "toolu_01T1x1fJ34qAmk2tNTrN7Up6");
            assert_eq!(name, "read");
            assert_eq!(args.get(), fragments);
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }

    let usage = message.usage.expect("assembled message carries usage");
    assert_eq!(usage.input_tokens, 472);
    assert_eq!(usage.output_tokens, 89);

    let ser1 = serde_json::to_string(message).unwrap();
    let de: tokio_agent_core::message::Message = serde_json::from_str(&ser1).unwrap();
    let ser2 = serde_json::to_string(&de).unwrap();
    assert_eq!(
        ser1, ser2,
        "assembled Message must round-trip byte-identically"
    );
}
