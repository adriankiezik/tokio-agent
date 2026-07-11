use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue, to_raw_value};
use serde_json::{Value, json};
use tokio_agent_core::event::{Event, RawFrame, StopReason};
use tokio_agent_core::message::{
    ContentBlock, Message, ProviderMetadata, Role, ToolCallId, ToolOutput, Usage,
};
use tokio_agent_core::provider::{BoxFuture, Capabilities, Provider, ProviderError, Request};
use tokio_util::sync::CancellationToken;

use crate::sse::FrameAssembler;
use crate::transport;

const PROVIDER: &str = "anthropic";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_BASE: &str = "https://api.anthropic.com";

pub struct Anthropic {
    client: reqwest::Client,
    api_key: String,
    base: String,
}

impl Anthropic {
    #[must_use]
    pub fn new(api_key: String, api_base: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base: api_base.unwrap_or_else(|| DEFAULT_BASE.to_owned()),
        }
    }

    async fn open(
        &self,
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Event>, ProviderError> {
        let body = build_body(req);
        let url = format!("{}/v1/messages", self.base.trim_end_matches('/'));

        let request = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body);
        transport::open_event_stream(request, Assembler::new(), cancel).await
    }
}

impl Provider for Anthropic {
    fn stream<'a>(
        &'a self,
        req: &'a Request,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Event>, ProviderError>> {
        Box::pin(self.open(req, cancel))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            streaming: true,
            caching: true,
            vision: true,
        }
    }

    fn count_tokens<'a>(
        &'a self,
        req: &'a Request,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<u64, ProviderError>> {
        Box::pin(async move {
            let mut body = serde_json::to_value(build_body(req))
                .map_err(|error| ProviderError::fatal(error.to_string()))?;
            if let Some(object) = body.as_object_mut() {
                object.remove("stream");
                object.remove("max_tokens");
            }
            let url = format!(
                "{}/v1/messages/count_tokens",
                self.base.trim_end_matches('/')
            );
            let request = self
                .client
                .post(url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json")
                .json(&body);
            let response: TokenCount = transport::send_json(request, cancel).await?;
            Ok(response.input_tokens)
        })
    }
}

#[derive(Deserialize)]
struct TokenCount {
    input_tokens: u64,
}

enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    Unknown,
}

pub struct Assembler {
    blocks: Vec<Block>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    stop: StopReason,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            stop: StopReason::EndTurn,
        }
    }

    pub fn push(&mut self, data: &str) -> Vec<Event> {
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return vec![Self::unknown(data)];
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.absorb_usage(value.get("message").and_then(|m| m.get("usage")));
                Vec::new()
            }
            Some("content_block_start") => self.on_block_start(&value),
            Some("content_block_delta") => self.on_block_delta(&value),
            Some("content_block_stop") => self.on_block_stop(&value),
            Some("message_delta") => self.on_message_delta(&value),
            Some("message_stop") => vec![self.done(self.stop)],
            Some("ping") => Vec::new(),
            _ => vec![Self::unknown(data)],
        }
    }

    #[must_use]
    pub fn done(&self, stop: StopReason) -> Event {
        Event::Done {
            stop,
            message: self.assembled(),
        }
    }

    fn on_block_start(&mut self, value: &Value) -> Vec<Event> {
        let cb = value.get("content_block");
        match cb.and_then(|c| c.get("type")).and_then(Value::as_str) {
            Some("text") => {
                self.blocks.push(Block::Text {
                    text: String::new(),
                });
                Vec::new()
            }
            Some("thinking") => {
                self.blocks.push(Block::Thinking {
                    text: String::new(),
                    signature: String::new(),
                });
                Vec::new()
            }
            Some("tool_use") => {
                let id = str_field(cb.unwrap(), "id");
                let name = str_field(cb.unwrap(), "name");
                self.blocks.push(Block::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    json: String::new(),
                });
                vec![Event::ToolCallStart {
                    id: ToolCallId(id),
                    name,
                }]
            }
            _ => {
                self.blocks.push(Block::Unknown);
                vec![Self::unknown(&value.to_string())]
            }
        }
    }

    fn on_block_delta(&mut self, value: &Value) -> Vec<Event> {
        let index = block_index(value);
        let delta = value.get("delta");
        match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
            Some("text_delta") => {
                let text = delta.map(|d| str_field(d, "text")).unwrap_or_default();
                if let Some(Block::Text { text: buf }) = self.blocks.get_mut(index) {
                    buf.push_str(&text);
                }
                vec![Event::TextDelta { text }]
            }
            Some("thinking_delta") => {
                let text = delta.map(|d| str_field(d, "thinking")).unwrap_or_default();
                if let Some(Block::Thinking { text: buf, .. }) = self.blocks.get_mut(index) {
                    buf.push_str(&text);
                }
                vec![Event::ThinkingDelta { text }]
            }
            Some("signature_delta") => {
                let sig = delta.map(|d| str_field(d, "signature")).unwrap_or_default();
                if let Some(Block::Thinking { signature, .. }) = self.blocks.get_mut(index) {
                    signature.push_str(&sig);
                }
                Vec::new()
            }
            Some("input_json_delta") => {
                let fragment = delta
                    .map(|d| str_field(d, "partial_json"))
                    .unwrap_or_default();
                let mut id = None;
                if let Some(Block::ToolUse {
                    id: tool_id, json, ..
                }) = self.blocks.get_mut(index)
                {
                    json.push_str(&fragment);
                    id = Some(tool_id.clone());
                }
                match id {
                    Some(id) => vec![Event::ToolCallArgs {
                        id: ToolCallId(id),
                        fragment,
                    }],
                    None => Vec::new(),
                }
            }
            _ => vec![Self::unknown(&value.to_string())],
        }
    }

    fn on_block_stop(&mut self, value: &Value) -> Vec<Event> {
        let index = block_index(value);
        match self.blocks.get(index) {
            Some(Block::ToolUse { id, .. }) => vec![Event::ToolCallEnd {
                id: ToolCallId(id.clone()),
            }],
            _ => Vec::new(),
        }
    }

    fn on_message_delta(&mut self, value: &Value) -> Vec<Event> {
        if let Some(reason) = value
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(Value::as_str)
        {
            self.stop = map_stop_reason(reason);
        }
        self.absorb_usage(value.get("usage"));
        vec![Event::Usage(self.usage())]
    }

    fn absorb_usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else { return };
        let n = |k: &str| usage.get(k).and_then(Value::as_u64);
        if let Some(v) = n("input_tokens") {
            self.input_tokens = v;
        }
        if let Some(v) = n("output_tokens") {
            self.output_tokens = v;
        }
        if let Some(v) = n("cache_read_input_tokens") {
            self.cache_read_tokens = v;
        }
        if let Some(v) = n("cache_creation_input_tokens") {
            self.cache_write_tokens = v;
        }
    }

    fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
        }
    }

    fn assembled(&self) -> Message {
        let blocks = self
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(ContentBlock::Text {
                    text: text.clone(),
                    meta: ProviderMetadata::default(),
                }),
                Block::Thinking { text, signature } => Some(ContentBlock::Thinking {
                    text: text.clone(),
                    meta: signature_meta(signature),
                }),
                Block::ToolUse { id, name, json } => Some(ContentBlock::ToolCall {
                    id: ToolCallId(id.clone()),
                    name: name.clone(),
                    args: tool_args(json),
                    meta: ProviderMetadata::default(),
                }),
                Block::Unknown => None,
            })
            .collect();

        Message {
            role: Role::Assistant,
            blocks,
            usage: Some(self.usage()),
        }
    }

    fn unknown(payload: &str) -> Event {
        Event::Unknown(RawFrame {
            provider: PROVIDER.to_owned(),
            payload: payload.to_owned(),
        })
    }
}

impl FrameAssembler for Assembler {
    fn push(&mut self, data: &str) -> Vec<Event> {
        Assembler::push(self, data)
    }

    fn interrupted(&self) -> Event {
        self.done(StopReason::Interrupted)
    }

    fn failed(&self, error: ProviderError) -> Event {
        Event::Failed {
            retryable: error.retryable,
            error: error.message,
            message: self.assembled(),
        }
    }
}

fn block_index(value: &Value) -> usize {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or_default()
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}

fn signature_meta(signature: &str) -> ProviderMetadata {
    if signature.is_empty() {
        return ProviderMetadata::default();
    }
    let raw = RawValue::from_string(json!({ "signature": signature }).to_string())
        .expect("json object is valid");
    ProviderMetadata::from_provider(PROVIDER.to_owned(), raw)
}

fn tool_args(json: &str) -> Box<RawValue> {
    if json.trim().is_empty() {
        return RawValue::from_string("{}".to_owned()).expect("empty object is valid");
    }
    match RawValue::from_string(json.to_owned()) {
        Ok(raw) => raw,
        Err(_) => to_raw_value(&json).expect("string is always valid json"),
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig<'a>>,
}

#[derive(Serialize)]
struct OutputConfig<'a> {
    effort: &'a str,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: Vec<WireContent<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContent<'a> {
    Text {
        text: &'a str,
    },
    Thinking {
        thinking: &'a str,
        signature: String,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a RawValue,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

#[derive(Serialize)]
struct WireTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

fn build_body(req: &Request) -> WireRequest<'_> {
    let messages = req.messages.iter().filter_map(wire_message).collect();
    let tools = req
        .tools
        .iter()
        .map(|t| WireTool {
            name: &t.name,
            description: &t.description,
            input_schema: &t.input_schema,
        })
        .collect();

    WireRequest {
        model: &req.model,
        max_tokens: req.max_tokens,
        stream: true,
        system: if req.system.is_empty() {
            None
        } else {
            Some(&req.system)
        },
        messages,
        tools,
        output_config: req
            .reasoning_effort
            .as_deref()
            .map(|effort| OutputConfig { effort }),
    }
}

fn wire_message(msg: &Message) -> Option<WireMessage<'_>> {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<WireContent<'_>> = msg.blocks.iter().filter_map(wire_content).collect();
    if content.is_empty() {
        return None;
    }
    Some(WireMessage { role, content })
}

fn wire_content(block: &ContentBlock) -> Option<WireContent<'_>> {
    match block {
        ContentBlock::Text { text, .. } => Some(WireContent::Text { text }),
        ContentBlock::Thinking { text, meta } => {
            signature_of(meta).map(|signature| WireContent::Thinking {
                thinking: text,
                signature,
            })
        }
        ContentBlock::ToolCall { id, name, args, .. } => Some(WireContent::ToolUse {
            id: &id.0,
            name,
            input: args,
        }),
        ContentBlock::ToolResult {
            call,
            output,
            is_error,
            ..
        } => {
            let ToolOutput::Text(content) = output;
            Some(WireContent::ToolResult {
                tool_use_id: &call.0,
                content,
                is_error: *is_error,
            })
        }
        ContentBlock::Compaction { .. } => None,
    }
}

#[derive(serde::Deserialize)]
struct ThinkingMeta {
    signature: String,
}

fn signature_of(meta: &ProviderMetadata) -> Option<String> {
    let raw = meta.get(PROVIDER)?;
    let parsed: ThinkingMeta = serde_json::from_str(raw.get()).ok()?;
    Some(parsed.signature)
}

fn str_field(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio_agent_core::tool::ToolDef;

    fn drive(assembler: &mut Assembler, frames: &[&str]) -> Vec<Event> {
        frames.iter().flat_map(|f| assembler.push(f)).collect()
    }

    #[tokio::test]
    async fn cancellation_before_http_response_returns_interrupted_done() {
        let provider = Anthropic::new("unused".to_owned(), Some("http://127.0.0.1:1".to_owned()));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let req = Request {
            model: "m".to_owned(),
            system: String::new(),
            messages: vec![],
            tools: vec![],
            max_tokens: 1,
            reasoning_effort: None,
        };
        let mut stream = provider
            .open(&req, cancel)
            .await
            .expect("cancellation is data");
        assert!(matches!(
            stream.next().await,
            Some(Event::Done {
                stop: StopReason::Interrupted,
                ..
            })
        ));
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn assembler_builds_text_thinking_and_tool_use_in_order() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me look"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG123"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"checking"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_1","name":"read","input":{}}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.rs\"}"}}"#,
                r#"{"type":"content_block_stop","index":2}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );

        let Some(Event::Done { stop, message }) = events.last() else {
            panic!("expected terminal Done, got {events:?}");
        };
        assert_eq!(*stop, StopReason::ToolUse);
        assert_eq!(message.blocks.len(), 3);

        match &message.blocks[0] {
            ContentBlock::Thinking { text, meta } => {
                assert_eq!(text, "let me look");
                assert!(meta.get(PROVIDER).unwrap().get().contains("SIG123"));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
        assert!(
            matches!(&message.blocks[1], ContentBlock::Text { text, .. } if text == "checking")
        );
        match &message.blocks[2] {
            ContentBlock::ToolCall { id, name, args, .. } => {
                assert_eq!(id.0, "call_1");
                assert_eq!(name, "read");
                assert_eq!(args.get(), r#"{"path":"a.rs"}"#);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }

        let usage = message.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cache_write_tokens, 1);
    }

    #[test]
    fn interruption_preserves_partial_thinking_signature() {
        let mut asm = Assembler::new();
        drive(
            &mut asm,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"partial"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
            ],
        );
        let Event::Done { stop, message } = asm.interrupted() else {
            unreachable!()
        };
        assert_eq!(stop, StopReason::Interrupted);
        let ContentBlock::Thinking { text, meta } = &message.blocks[0] else {
            panic!()
        };
        assert_eq!(text, "partial");
        assert_eq!(signature_of(meta).as_deref(), Some("SIG"));
    }

    #[test]
    fn malformed_tool_args_are_retained_verbatim_and_not_a_json_object() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":".rs"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );

        let Some(Event::Done { message, .. }) = events.last() else {
            panic!("expected terminal Done");
        };
        let parsed = message.blocks[0]
            .parsed_args()
            .expect("block is a ToolCall")
            .expect("wrapped args are themselves valid JSON");
        assert!(
            !parsed.is_object(),
            "malformed model args must not surface as a runnable JSON object"
        );
        assert_eq!(
            parsed.as_str(),
            Some(r#"{"path":"a.rs"#),
            "the exact malformed bytes must be retained verbatim"
        );
    }

    #[test]
    fn unrecognized_frames_become_unknown_events() {
        let mut asm = Assembler::new();
        let events = asm.push(r#"{"type":"widget_frobnicated","index":9}"#);
        assert!(matches!(events.as_slice(), [Event::Unknown(frame)] if frame.provider == PROVIDER));
    }

    #[test]
    fn body_maps_tool_results_to_user_role_and_carries_signature() {
        let req = Request {
            model: "claude-sonnet-5".to_owned(),
            system: "sys".to_owned(),
            messages: vec![
                Message {
                    role: Role::Tool,
                    blocks: vec![ContentBlock::ToolResult {
                        call: ToolCallId("call_1".to_owned()),
                        output: ToolOutput::Text("42".to_owned()),
                        is_error: false,
                        meta: ProviderMetadata::default(),
                    }],
                    usage: None,
                },
                Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::Thinking {
                        text: "hmm".to_owned(),
                        meta: ProviderMetadata::from_provider(
                            PROVIDER.to_owned(),
                            RawValue::from_string(r#"{"signature":"SIG"}"#.to_owned()).unwrap(),
                        ),
                    }],
                    usage: None,
                },
            ],
            tools: vec![ToolDef {
                name: "read".to_owned(),
                description: "read".to_owned(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 256,
            reasoning_effort: Some("high".to_owned()),
        };

        let wire = serde_json::to_value(build_body(&req)).unwrap();
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["output_config"]["effort"], "high");
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["signature"], "SIG");
    }

    #[test]
    fn thinking_without_signature_is_dropped_from_body() {
        let req = Request {
            model: "m".to_owned(),
            system: String::new(),
            messages: vec![Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Thinking {
                    text: "unsigned".to_owned(),
                    meta: ProviderMetadata::default(),
                }],
                usage: None,
            }],
            tools: vec![],
            max_tokens: 16,
            reasoning_effort: None,
        };
        let wire = serde_json::to_value(build_body(&req)).unwrap();
        assert!(
            wire["messages"].as_array().unwrap().is_empty(),
            "a message left with no content must be dropped, not sent as an empty content array"
        );
        assert!(wire.get("system").is_none(), "empty system omitted");
    }
}
