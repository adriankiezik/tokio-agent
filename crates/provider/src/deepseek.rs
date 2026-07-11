use std::collections::BTreeMap;

use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio_agent_core::event::{Event, RawFrame, StopReason};
use tokio_agent_core::message::{
    ContentBlock, Message, ProviderMetadata, Role, ToolCallId, ToolOutput, Usage,
};
use tokio_agent_core::provider::{BoxFuture, Capabilities, Provider, ProviderError, Request};
use tokio_util::sync::CancellationToken;

use crate::sse::FrameAssembler;
use crate::transport;

const PROVIDER: &str = "deepseek";
const DEFAULT_BASE: &str = "https://api.deepseek.com";

pub struct DeepSeek {
    client: reqwest::Client,
    api_key: String,
    base: String,
}

impl DeepSeek {
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
        let url = format!("{}/chat/completions", self.base.trim_end_matches('/'));
        let request = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&build_body(req));
        transport::open_event_stream(request, Assembler::new(), cancel).await
    }
}

impl Provider for DeepSeek {
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
            vision: false,
        }
    }
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    args: String,
    started: bool,
    ended: bool,
}

pub struct Assembler {
    text: String,
    reasoning: String,
    calls: BTreeMap<usize, ToolCall>,
    usage: Usage,
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
            text: String::new(),
            reasoning: String::new(),
            calls: BTreeMap::new(),
            usage: Usage::default(),
            stop: StopReason::EndTurn,
        }
    }

    pub fn push(&mut self, data: &str) -> Vec<Event> {
        if data.trim() == "[DONE]" {
            return vec![self.done()];
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return vec![Self::unknown(data)];
        };
        let mut events = Vec::new();
        if let Some(usage) = value.get("usage") {
            self.usage = Usage {
                input_tokens: usage
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read_tokens: usage
                    .get("prompt_cache_hit_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_write_tokens: usage
                    .get("prompt_cache_miss_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            };
            events.push(Event::Usage(self.usage));
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
        else {
            return events;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = match reason {
                "tool_calls" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            };
            for call in self
                .calls
                .values_mut()
                .filter(|call| call.started && !call.ended)
            {
                call.ended = true;
                events.push(Event::ToolCallEnd {
                    id: ToolCallId(call.id.clone()),
                });
            }
        }
        let Some(delta) = choice.get("delta") else {
            return events;
        };
        if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
            self.reasoning.push_str(text);
            events.push(Event::ThinkingDelta {
                text: text.to_owned(),
            });
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
            events.push(Event::TextDelta {
                text: text.to_owned(),
            });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for wire in calls {
                let index = wire.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let call = self.calls.entry(index).or_default();
                if let Some(id) = wire.get("id").and_then(Value::as_str) {
                    call.id.push_str(id);
                }
                if let Some(function) = wire.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        call.name.push_str(name);
                    }
                    if !call.started && !call.id.is_empty() && !call.name.is_empty() {
                        call.started = true;
                        events.push(Event::ToolCallStart {
                            id: ToolCallId(call.id.clone()),
                            name: call.name.clone(),
                        });
                    }
                    if let Some(fragment) = function.get("arguments").and_then(Value::as_str) {
                        call.args.push_str(fragment);
                        events.push(Event::ToolCallArgs {
                            id: ToolCallId(call.id.clone()),
                            fragment: fragment.to_owned(),
                        });
                    }
                }
            }
        }
        events
    }

    fn message(&self) -> Message {
        let mut blocks = Vec::new();
        if !self.reasoning.is_empty() {
            blocks.push(ContentBlock::Thinking {
                text: self.reasoning.clone(),
                meta: ProviderMetadata::default(),
            });
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: self.text.clone(),
                meta: ProviderMetadata::default(),
            });
        }
        for call in self.calls.values() {
            let args =
                RawValue::from_string(if serde_json::from_str::<Value>(&call.args).is_ok() {
                    call.args.clone()
                } else {
                    "{}".to_owned()
                })
                .expect("static JSON is valid");
            blocks.push(ContentBlock::ToolCall {
                id: ToolCallId(call.id.clone()),
                name: call.name.clone(),
                args,
                meta: ProviderMetadata::default(),
            });
        }
        Message {
            role: Role::Assistant,
            blocks,
            usage: Some(self.usage),
        }
    }

    fn done(&self) -> Event {
        Event::Done {
            stop: self.stop,
            message: self.message(),
        }
    }

    fn unknown(data: &str) -> Event {
        Event::Unknown(RawFrame {
            provider: PROVIDER.to_owned(),
            payload: data.to_owned(),
        })
    }
}

impl FrameAssembler for Assembler {
    fn push(&mut self, data: &str) -> Vec<Event> {
        self.push(data)
    }

    fn interrupted(&self) -> Event {
        Event::Done {
            stop: StopReason::Interrupted,
            message: self.message(),
        }
    }

    fn failed(&self, error: ProviderError) -> Event {
        Event::Failed {
            retryable: error.retryable,
            error: error.message,
            message: self.message(),
        }
    }
}

#[derive(Serialize)]
struct WireRequest {
    model: String,
    messages: Vec<Value>,
    tools: Vec<Value>,
    stream: bool,
    stream_options: Value,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

fn build_body(req: &Request) -> WireRequest {
    let mut messages = vec![json!({ "role": "system", "content": req.system })];
    for message in &req.messages {
        match message.role {
            Role::User => {
                let content = text_content(message);
                if !content.is_empty() {
                    messages.push(json!({ "role": "user", "content": content }));
                }
            }
            Role::Assistant => {
                let content = text_content(message);
                let reasoning_content: String = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let tool_calls: Vec<Value> = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall { id, name, args, .. } => Some(json!({
                            "id": id.0,
                            "type": "function",
                            "function": { "name": name, "arguments": args.get() }
                        })),
                        _ => None,
                    })
                    .collect();
                let mut wire = json!({ "role": "assistant", "content": content });
                if !reasoning_content.is_empty() {
                    wire["reasoning_content"] = json!(reasoning_content);
                }
                if !tool_calls.is_empty() {
                    wire["tool_calls"] = json!(tool_calls);
                }
                messages.push(wire);
            }
            Role::Tool => {
                for block in &message.blocks {
                    if let ContentBlock::ToolResult {
                        call,
                        output: ToolOutput::Text(text),
                        ..
                    } = block
                    {
                        messages.push(
                            json!({ "role": "tool", "tool_call_id": call.0, "content": text }),
                        );
                    }
                }
            }
        }
    }
    let tools = req.tools.iter().map(|tool| json!({
        "type": "function",
        "function": { "name": tool.name, "description": tool.description, "parameters": tool.input_schema }
    })).collect();
    WireRequest {
        model: req.model.clone(),
        messages,
        tools,
        stream: true,
        stream_options: json!({ "include_usage": true }),
        max_tokens: req.max_tokens,
        reasoning_effort: req.reasoning_effort.clone(),
    }
}

fn text_content(message: &Message) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_agent_core::tool::ToolDef;

    #[test]
    fn builds_chat_completion_request_with_tools() {
        let req = Request {
            model: "deepseek-chat".into(),
            system: "help".into(),
            messages: vec![Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: "read it".into(),
                    meta: ProviderMetadata::default(),
                }],
                usage: None,
            }],
            tools: vec![ToolDef {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: json!({"type":"object"}),
            }],
            max_tokens: 1024,
            reasoning_effort: None,
        };
        let body = serde_json::to_value(build_body(&req)).unwrap();
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["messages"][1]["content"], "read it");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn forwards_reasoning_effort() {
        let req = Request {
            model: "deepseek-v4-pro".into(),
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 1024,
            reasoning_effort: Some("max".into()),
        };
        let body = serde_json::to_value(build_body(&req)).unwrap();
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn assembles_reasoning_text_tool_call_and_usage() {
        let mut assembler = Assembler::new();
        let mut events =
            assembler.push(r#"{"choices":[{"delta":{"reasoning_content":"think "}}]}"#);
        events.extend(assembler.push(r#"{"choices":[{"delta":{"content":"done","tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#));
        events.extend(assembler.push(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#));
        events.extend(assembler.push(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_cache_hit_tokens":4,"prompt_cache_miss_tokens":6}}"#));
        events.extend(assembler.push("[DONE]"));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ThinkingDelta { text } if text == "think "))
        );
        let Event::Done { stop, message } = events.last().unwrap() else {
            panic!("missing Done")
        };
        assert_eq!(*stop, StopReason::ToolUse);
        assert_eq!(message.usage.unwrap().cache_read_tokens, 4);
        assert!(
            matches!(&message.blocks[2], ContentBlock::ToolCall { args, .. } if args.get() == r#"{"path":"a"}"#)
        );
    }
}
