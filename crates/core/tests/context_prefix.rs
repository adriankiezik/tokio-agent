use serde_json::value::RawValue;
use tokio_agent_core::context::ContextAssembler;
use tokio_agent_core::message::{ContentBlock, Message, Role, ToolCallId};
use tokio_agent_core::tool::ToolResult;

fn tool_call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        id: ToolCallId(id.to_owned()),
        name: name.to_owned(),
        args: RawValue::from_string("{\"path\":\"README.md\"}".to_owned()).unwrap(),
        meta: tokio_agent_core::message::ProviderMetadata::default(),
    }
}

fn assistant(blocks: Vec<ContentBlock>) -> Message {
    Message {
        role: Role::Assistant,
        blocks,
        usage: None,
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
        meta: tokio_agent_core::message::ProviderMetadata::default(),
    }
}

fn serialized_messages(ctx: &ContextAssembler) -> String {
    let req = ctx.build_request(Vec::new());
    serde_json::to_string(&req.messages).unwrap()
}

#[test]
fn request_prefix_is_byte_identical_across_turns() {
    let mut ctx = ContextAssembler::with_budget("model-x".into(), "system".into(), 1024, 100);

    ctx.push_user("read the readme".into());
    let turn1 = serialized_messages(&ctx);

    ctx.push_assistant(assistant(vec![text("reading"), tool_call("c1", "read")]));
    ctx.push_tool_result(ToolCallId("c1".into()), ToolResult::ok("x".repeat(1000)));
    let turn2 = serialized_messages(&ctx);

    ctx.push_assistant(assistant(vec![text("done")]));
    ctx.push_user("now summarize".into());
    let turn3 = serialized_messages(&ctx);

    assert!(
        turn2.starts_with(&turn1[..turn1.len() - 1]),
        "turn 1 messages must be a prefix of turn 2"
    );
    assert!(
        turn3.starts_with(&turn2[..turn2.len() - 1]),
        "turn 2 messages must be a prefix of turn 3 (including the truncated tool result)"
    );

    let req = ctx.build_request(Vec::new());
    let stored = serde_json::to_string(&req.messages).unwrap();
    assert!(stored.contains("bytes elided"));
}
