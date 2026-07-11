use std::collections::HashSet;
use std::fmt::Write;

use crate::message::{ContentBlock, Message, Role, ToolCallId, ToolOutput};
use crate::provider::Request;
use crate::tool::{ToolDef, ToolResult};

pub const DEFAULT_TOOL_RESULT_BUDGET: usize = 64_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub raw_args: String,
}

pub struct ContextAssembler {
    model: String,
    system: String,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    budget: usize,
    transcript: Vec<Message>,
}

impl ContextAssembler {
    #[must_use]
    pub fn new(model: String, system: String, max_tokens: u32) -> Self {
        Self::with_budget(model, system, max_tokens, DEFAULT_TOOL_RESULT_BUDGET)
    }

    #[must_use]
    pub fn with_budget(model: String, system: String, max_tokens: u32, budget: usize) -> Self {
        Self {
            model,
            system,
            max_tokens,
            reasoning_effort: None,
            budget,
            transcript: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    #[must_use]
    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    pub fn clear(&mut self) {
        self.transcript.clear();
    }

    pub fn push_user(&mut self, text: String) {
        self.transcript.push(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text,
                meta: crate::message::ProviderMetadata::default(),
            }],
            usage: None,
        });
    }

    pub fn begin_turn(&mut self, text: String, tools: Vec<ToolDef>) -> Request {
        self.push_user(text);
        self.build_request(tools)
    }

    pub fn push_assistant(&mut self, message: Message) {
        self.transcript.push(message);
    }

    pub fn accept_assistant(&mut self, message: Message) -> Vec<PendingToolCall> {
        let calls = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { id, name, args, .. } => Some(PendingToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    raw_args: args.get().to_owned(),
                }),
                _ => None,
            })
            .collect();
        self.push_assistant(message);
        calls
    }

    pub fn push_tool_result(&mut self, call: ToolCallId, result: ToolResult) {
        let ToolOutput::Text(text) = result.output;
        let block = ContentBlock::ToolResult {
            call,
            output: ToolOutput::Text(truncate_middle(text, self.budget)),
            is_error: result.is_error,
            meta: crate::message::ProviderMetadata::default(),
        };

        match self.transcript.last_mut() {
            Some(message) if message.role == Role::Tool => message.blocks.push(block),
            _ => self.transcript.push(Message {
                role: Role::Tool,
                blocks: vec![block],
                usage: None,
            }),
        }
    }

    pub fn accept_tool_results(&mut self, calls: Vec<PendingToolCall>, results: Vec<ToolResult>) {
        assert_eq!(calls.len(), results.len(), "one result per tool call");
        for (call, result) in calls.into_iter().zip(results) {
            self.push_tool_result(call.id, result);
        }
    }

    #[must_use]
    pub fn build_request(&self, tools: Vec<ToolDef>) -> Request {
        Request {
            model: self.model.clone(),
            system: self.system.clone(),
            messages: self.transcript.clone(),
            tools,
            max_tokens: self.max_tokens,
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }

    pub fn close_open_tool_calls(&mut self) {
        let mut resolved: HashSet<ToolCallId> = HashSet::new();
        let mut open: Vec<ToolCallId> = Vec::new();

        for message in &self.transcript {
            for block in &message.blocks {
                match block {
                    ContentBlock::ToolResult { call, .. } => {
                        resolved.insert(call.clone());
                    }
                    ContentBlock::ToolCall { id, .. } => open.push(id.clone()),
                    _ => {}
                }
            }
        }

        for id in open {
            if resolved.contains(&id) {
                continue;
            }
            self.push_tool_result(id, ToolResult::error("cancelled by user"));
        }
    }
}

fn truncate_middle(text: String, budget: usize) -> String {
    if text.len() <= budget {
        return text;
    }

    let half = budget / 2;
    let head_end = floor_char_boundary(&text, half);
    let mut tail_start = ceil_char_boundary(&text, text.len() - half);
    if tail_start < head_end {
        tail_start = head_end;
    }

    let elided = tail_start - head_end;
    let mut out = String::with_capacity(head_end + (text.len() - tail_start) + 32);
    out.push_str(&text[..head_end]);
    write!(out, "\n[… {elided} bytes elided …]\n").expect("writing to a String cannot fail");
    out.push_str(&text[tail_start..]);
    out
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut i = index;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut i = index;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::value::RawValue;

    fn tool_call(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: ToolCallId(id.to_owned()),
            name: name.to_owned(),
            args: RawValue::from_string("{}".to_owned()).unwrap(),
            meta: crate::message::ProviderMetadata::default(),
        }
    }

    fn assistant_with_calls(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            blocks,
            usage: None,
        }
    }

    fn output_text(block: &ContentBlock) -> &str {
        match block {
            ContentBlock::ToolResult {
                output: ToolOutput::Text(text),
                ..
            } => text,
            _ => panic!("expected a tool result block"),
        }
    }

    #[test]
    fn under_budget_result_is_untouched() {
        let mut ctx = ContextAssembler::with_budget("m".into(), "s".into(), 8, 1000);
        ctx.push_tool_result(ToolCallId("a".into()), ToolResult::ok("small output"));
        assert_eq!(output_text(&ctx.transcript()[0].blocks[0]), "small output");
    }

    #[test]
    fn over_budget_result_is_middle_truncated() {
        let budget = 100;
        let mut ctx = ContextAssembler::with_budget("m".into(), "s".into(), 8, budget);
        let big = "x".repeat(1000);
        ctx.push_tool_result(ToolCallId("a".into()), ToolResult::ok(big));

        let out = output_text(&ctx.transcript()[0].blocks[0]);
        assert!(out.starts_with('x'));
        assert!(out.ends_with('x'));
        assert!(out.contains("bytes elided"));
        assert!(out.len() <= budget + 32);
    }

    #[test]
    fn stored_result_is_immutable_across_later_pushes() {
        let mut ctx = ContextAssembler::with_budget("m".into(), "s".into(), 8, 100);
        ctx.push_tool_result(ToolCallId("a".into()), ToolResult::ok("y".repeat(1000)));
        let captured = output_text(&ctx.transcript()[0].blocks[0]).to_owned();

        ctx.push_user("next".into());
        ctx.push_tool_result(ToolCallId("b".into()), ToolResult::ok("z".repeat(1000)));

        assert_eq!(output_text(&ctx.transcript()[0].blocks[0]), captured);
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        let budget = 41;
        let mut ctx = ContextAssembler::with_budget("m".into(), "s".into(), 8, budget);
        let big = "é".repeat(1000);
        ctx.push_tool_result(ToolCallId("a".into()), ToolResult::ok(big));
        let out = output_text(&ctx.transcript()[0].blocks[0]);
        assert!(out.contains("bytes elided"));
        assert!(out.chars().count() > 0);
    }

    #[test]
    fn close_open_tool_calls_synthesizes_errors_for_dangling_calls_only() {
        let mut ctx = ContextAssembler::new("m".into(), "s".into(), 8);
        ctx.push_user("go".into());
        ctx.push_assistant(assistant_with_calls(vec![
            tool_call("one", "read"),
            tool_call("two", "read"),
        ]));
        ctx.push_tool_result(ToolCallId("one".into()), ToolResult::ok("done"));

        ctx.close_open_tool_calls();

        let mut results: Vec<(String, bool)> = Vec::new();
        for message in ctx.transcript() {
            for block in &message.blocks {
                if let ContentBlock::ToolResult { call, is_error, .. } = block {
                    results.push((call.0.clone(), *is_error));
                }
            }
        }

        assert_eq!(
            results,
            vec![("one".to_owned(), false), ("two".to_owned(), true)]
        );

        ctx.close_open_tool_calls();
        let count = ctx
            .transcript()
            .iter()
            .flat_map(|m| &m.blocks)
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count();
        assert_eq!(count, 2);
    }
}
