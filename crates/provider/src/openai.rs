use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::{RawValue, to_raw_value};
use tokio_agent_auth::{ChatGptAuth, ORIGINATOR};
use tokio_agent_core::event::{Event, RawFrame, StopReason};
use tokio_agent_core::message::{
    ContentBlock, Message, ProviderMetadata, Role, ToolCallId, ToolOutput, Usage,
};
use tokio_agent_core::provider::{BoxFuture, Capabilities, Provider, ProviderError, Request};
use tokio_util::sync::CancellationToken;

use crate::sse::FrameAssembler;
use crate::transport;

const PROVIDER: &str = "openai";
const DEFAULT_BASE: &str = "https://api.openai.com";
const CHATGPT_BASE: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_EFFORT: &str = "medium";

enum Auth {
    ApiKey(String),
    ChatGpt(Arc<ChatGptAuth>),
}

pub struct OpenAi {
    client: reqwest::Client,
    auth: Auth,
    base: String,
}

impl OpenAi {
    #[must_use]
    pub fn new(api_key: String, api_base: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth: Auth::ApiKey(api_key),
            base: api_base.unwrap_or_else(|| DEFAULT_BASE.to_owned()),
        }
    }

    pub fn chatgpt(auth: Arc<ChatGptAuth>, api_base: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth: Auth::ChatGpt(auth),
            base: api_base.unwrap_or_else(|| CHATGPT_BASE.to_owned()),
        }
    }

    async fn open(
        &self,
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Event>, ProviderError> {
        let body = build_body(req, matches!(self.auth, Auth::ApiKey(_)));
        let path = match self.auth {
            Auth::ApiKey(_) => "/v1/responses",
            Auth::ChatGpt(_) => "/responses",
        };
        let url = format!("{}{}", self.base.trim_end_matches('/'), path);

        let mut request = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);
        request = match &self.auth {
            Auth::ApiKey(key) => request.bearer_auth(key),
            Auth::ChatGpt(auth) => {
                let token = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        return Ok(transport::interrupted_stream(Assembler::new()));
                    }
                    token = auth.access_token() => token,
                }
                .map_err(|e| ProviderError::fatal(e.to_string()))?;
                request = request
                    .bearer_auth(token)
                    .header("originator", ORIGINATOR)
                    .header("accept", "text/event-stream");
                if let Some(account_id) = auth.account_id() {
                    request = request.header("chatgpt-account-id", account_id);
                }
                request
            }
        };

        transport::open_event_stream(request, Assembler::new(), cancel).await
    }
}

impl Provider for OpenAi {
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
            if matches!(self.auth, Auth::ChatGpt(_)) {
                return Err(ProviderError::fatal(
                    "exact live counting is unavailable for ChatGPT subscription sessions",
                ));
            }
            let mut body = serde_json::to_value(build_body(req, false))
                .map_err(|error| ProviderError::fatal(error.to_string()))?;
            if let Some(object) = body.as_object_mut() {
                for key in ["stream", "store", "include", "max_output_tokens"] {
                    object.remove(key);
                }
            }
            let path = "/v1/responses/input_tokens";
            let url = format!("{}{}", self.base.trim_end_matches('/'), path);
            let mut request = self
                .client
                .post(url)
                .header("content-type", "application/json")
                .json(&body);
            request = match &self.auth {
                Auth::ApiKey(key) => request.bearer_auth(key),
                Auth::ChatGpt(auth) => {
                    let token = auth
                        .access_token()
                        .await
                        .map_err(|error| ProviderError::fatal(error.to_string()))?;
                    let mut request = request.bearer_auth(token).header("originator", ORIGINATOR);
                    if let Some(account_id) = auth.account_id() {
                        request = request.header("chatgpt-account-id", account_id);
                    }
                    request
                }
            };
            let response: TokenCount = transport::send_json(request, cancel).await?;
            Ok(response.input_tokens)
        })
    }
}

#[derive(Deserialize)]
struct TokenCount {
    input_tokens: u64,
}

pub struct Assembler {
    blocks: Vec<ContentBlock>,
    call_ids: HashMap<String, String>,
    citations: HashMap<String, Vec<Citation>>,
    fallback_sources: Vec<Citation>,
    hosted_started: HashSet<String>,
    hosted_finished: HashSet<String>,
    citation_marker_open: bool,
    saw_tool_call: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
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
            call_ids: HashMap::new(),
            citations: HashMap::new(),
            fallback_sources: Vec::new(),
            hosted_started: HashSet::new(),
            hosted_finished: HashSet::new(),
            citation_marker_open: false,
            saw_tool_call: false,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            stop: StopReason::EndTurn,
        }
    }

    pub fn push(&mut self, data: &str) -> Vec<Event> {
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return vec![Self::unknown(data)];
        };
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_item.added") => self.on_item_added(&value),
            Some("response.web_search_call.in_progress")
            | Some("response.web_search_call.searching") => self.on_hosted_start(&value),
            // The dedicated completion event only carries an item ID. The later
            // output_item.done contains the action and actual query used.
            Some("response.web_search_call.completed") => Vec::new(),
            Some("response.output_text.delta") => self.on_text_delta(&value),
            Some("response.output_text.annotation.added") => self.on_annotation(&value),
            Some("response.reasoning_summary_text.delta") => {
                delta_event(&value, |text| Event::ThinkingDelta { text })
            }
            Some("response.function_call_arguments.delta") => self.on_args_delta(&value),
            Some("response.output_item.done") => self.on_item_done(&value),
            Some("response.completed") => self.on_completed(&value),
            Some("response.incomplete") => vec![self.done(StopReason::MaxTokens)],
            Some("response.failed") => vec![self.done(StopReason::Interrupted)],
            _ => Vec::new(),
        }
    }

    #[must_use]
    pub fn done(&self, stop: StopReason) -> Event {
        Event::Done {
            stop,
            message: self.assembled(),
        }
    }

    fn on_item_added(&mut self, value: &Value) -> Vec<Event> {
        let item = value.get("item");
        if item.and_then(|i| i.get("type")).and_then(Value::as_str) == Some("web_search_call") {
            return self.on_hosted_start(item.expect("item was just matched"));
        }
        if item.and_then(|i| i.get("type")).and_then(Value::as_str) != Some("function_call") {
            return Vec::new();
        }
        let item = item.unwrap();
        let call_id = str_field(item, "call_id");
        let name = str_field(item, "name");
        if let Some(fc_id) = item.get("id").and_then(Value::as_str) {
            self.call_ids.insert(fc_id.to_owned(), call_id.clone());
        }
        self.saw_tool_call = true;
        vec![Event::ToolCallStart {
            id: ToolCallId(call_id),
            name,
        }]
    }

    fn on_args_delta(&mut self, value: &Value) -> Vec<Event> {
        let fragment = str_field(value, "delta");
        let item_id = str_field(value, "item_id");
        let call_id = self.call_ids.get(&item_id).cloned().unwrap_or(item_id);
        vec![Event::ToolCallArgs {
            id: ToolCallId(call_id),
            fragment,
        }]
    }

    fn on_text_delta(&mut self, value: &Value) -> Vec<Event> {
        let delta = str_field(value, "delta");
        let text = filter_citation_markers(&delta, &mut self.citation_marker_open);
        if text.is_empty() {
            Vec::new()
        } else {
            vec![Event::TextDelta { text }]
        }
    }

    fn on_annotation(&mut self, value: &Value) -> Vec<Event> {
        let item_id = str_field(value, "item_id");
        if let Some(citation) = value.get("annotation").and_then(parse_url_citation) {
            push_unique_citation(self.citations.entry(item_id).or_default(), citation);
        }
        Vec::new()
    }

    fn on_item_done(&mut self, value: &Value) -> Vec<Event> {
        let Some(item) = value.get("item") else {
            return Vec::new();
        };
        match item.get("type").and_then(Value::as_str) {
            Some("web_search_call") => self.on_hosted_end(item),
            Some("message") => {
                let item_id = str_field(item, "id");
                let (mut text, mut citations) = output_text(item);
                if let Some(streamed) = self.citations.remove(&item_id) {
                    for citation in streamed {
                        push_unique_citation(&mut citations, citation);
                    }
                }
                if citations.is_empty() {
                    citations = std::mem::take(&mut self.fallback_sources);
                } else {
                    self.fallback_sources.clear();
                }
                let sources = citation_sources(&citations);
                text.push_str(&sources);
                if !text.is_empty() {
                    self.blocks.push(ContentBlock::Text {
                        text,
                        meta: ProviderMetadata::default(),
                    });
                }
                if sources.is_empty() {
                    Vec::new()
                } else {
                    vec![Event::TextDelta { text: sources }]
                }
            }
            Some("reasoning") => {
                let text = summary_text(item);
                let meta = reasoning_meta(item);
                if !text.is_empty() || !meta.is_empty() {
                    self.blocks.push(ContentBlock::Thinking { text, meta });
                }
                Vec::new()
            }
            Some("function_call") => {
                let call_id = str_field(item, "call_id");
                self.blocks.push(ContentBlock::ToolCall {
                    id: ToolCallId(call_id.clone()),
                    name: str_field(item, "name"),
                    args: tool_args(&str_field(item, "arguments")),
                    meta: function_call_meta(item),
                });
                self.saw_tool_call = true;
                vec![Event::ToolCallEnd {
                    id: ToolCallId(call_id),
                }]
            }
            _ => Vec::new(),
        }
    }

    fn on_hosted_start(&mut self, value: &Value) -> Vec<Event> {
        let id = hosted_item_id(value);
        if id.is_empty() || !self.hosted_started.insert(id.clone()) {
            return Vec::new();
        }
        vec![Event::HostedToolCallStart {
            id: ToolCallId(id),
            name: "web_search".to_owned(),
            summary: "searching the web".to_owned(),
        }]
    }

    fn on_hosted_end(&mut self, value: &Value) -> Vec<Event> {
        for source in web_search_sources(value) {
            push_unique_citation(&mut self.fallback_sources, source);
        }
        let id = hosted_item_id(value);
        if id.is_empty() || !self.hosted_finished.insert(id.clone()) {
            return Vec::new();
        }
        let mut events = self.on_hosted_start(value);
        events.push(Event::HostedToolCallEnd {
            id: ToolCallId(id),
            name: "web_search".to_owned(),
            output: web_search_summary(value),
            is_error: value.get("status").and_then(Value::as_str) == Some("failed"),
        });
        events
    }

    fn on_completed(&mut self, value: &Value) -> Vec<Event> {
        let usage = value.get("response").and_then(|r| r.get("usage"));
        self.absorb_usage(usage);
        self.stop = if self.saw_tool_call {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        let unfinished: Vec<_> = self
            .hosted_started
            .difference(&self.hosted_finished)
            .cloned()
            .collect();
        let mut events = unfinished
            .into_iter()
            .map(|id| {
                self.hosted_finished.insert(id.clone());
                Event::HostedToolCallEnd {
                    id: ToolCallId(id),
                    name: "web_search".to_owned(),
                    output: "search completed".to_owned(),
                    is_error: false,
                }
            })
            .collect::<Vec<_>>();
        events.push(Event::Usage(self.usage()));
        events.push(self.done(self.stop));
        events
    }

    fn absorb_usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else { return };
        if let Some(v) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = v;
        }
        if let Some(v) = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
        {
            self.cache_read_tokens = v;
        }
    }

    fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: 0,
        }
    }

    fn assembled(&self) -> Message {
        Message {
            role: Role::Assistant,
            blocks: self.blocks.clone(),
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

fn delta_event(value: &Value, make: impl Fn(String) -> Event) -> Vec<Event> {
    match value.get("delta").and_then(Value::as_str) {
        Some(delta) if !delta.is_empty() => vec![make(delta.to_owned())],
        _ => Vec::new(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Citation {
    title: String,
    url: String,
}

fn output_text(item: &Value) -> (String, Vec<Citation>) {
    let mut text = String::new();
    let mut citations = Vec::new();
    if let Some(parts) = item.get("content").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("output_text") {
                text.push_str(&strip_citation_markers(
                    part.get("text").and_then(Value::as_str).unwrap_or_default(),
                ));
                if let Some(annotations) = part.get("annotations").and_then(Value::as_array) {
                    for annotation in annotations {
                        if let Some(citation) = parse_url_citation(annotation) {
                            push_unique_citation(&mut citations, citation);
                        }
                    }
                }
            }
        }
    }
    (text, citations)
}

fn parse_url_citation(annotation: &Value) -> Option<Citation> {
    let value = annotation.get("url_citation").unwrap_or(annotation);
    let is_url_citation = annotation.get("type").and_then(Value::as_str) == Some("url_citation")
        || annotation.get("url_citation").is_some();
    if !is_url_citation {
        return None;
    }
    let url = value.get("url").and_then(Value::as_str)?.trim();
    if url.is_empty() {
        return None;
    }
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(clean_citation_field)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| url.to_owned());
    Some(Citation {
        title,
        url: clean_citation_field(url),
    })
}

fn web_search_sources(value: &Value) -> Vec<Citation> {
    value
        .pointer("/action/sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|source| {
            let url = source.get("url").and_then(Value::as_str)?.trim();
            (!url.is_empty()).then(|| Citation {
                title: source
                    .get("title")
                    .and_then(Value::as_str)
                    .map(clean_citation_field)
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| url.to_owned()),
                url: clean_citation_field(url),
            })
        })
        .collect()
}

fn push_unique_citation(citations: &mut Vec<Citation>, citation: Citation) {
    if !citations
        .iter()
        .any(|existing| existing.url == citation.url)
    {
        citations.push(citation);
    }
}

fn citation_sources(citations: &[Citation]) -> String {
    if citations.is_empty() {
        return String::new();
    }
    let mut output = String::from("\n\nSources:\n");
    for citation in citations {
        output.push_str("- ");
        output.push_str(&citation.title);
        output.push_str(": ");
        output.push_str(&citation.url);
        output.push('\n');
    }
    output.pop();
    output
}

fn clean_citation_field(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_citation_markers(text: &str) -> String {
    let mut marker_open = false;
    filter_citation_markers(text, &mut marker_open)
}

fn filter_citation_markers(text: &str, marker_open: &mut bool) -> String {
    const START: char = '\u{e200}';
    const END: char = '\u{e201}';

    let mut output = String::new();
    for character in text.chars() {
        if *marker_open {
            if character == END {
                *marker_open = false;
            }
        } else if character == START {
            *marker_open = true;
        } else {
            output.push(character);
        }
    }
    output
}

fn summary_text(item: &Value) -> String {
    let mut text = String::new();
    if let Some(parts) = item.get("summary").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("summary_text") {
                text.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
            }
        }
    }
    text
}

fn reasoning_meta(item: &Value) -> ProviderMetadata {
    let mut meta = serde_json::Map::new();
    if let Some(id) = item.get("id").and_then(Value::as_str) {
        meta.insert("id".to_owned(), Value::String(id.to_owned()));
    }
    if let Some(enc) = item.get("encrypted_content").and_then(Value::as_str) {
        meta.insert(
            "encrypted_content".to_owned(),
            Value::String(enc.to_owned()),
        );
    }
    object_meta(meta)
}

fn function_call_meta(item: &Value) -> ProviderMetadata {
    let mut meta = serde_json::Map::new();
    if let Some(id) = item.get("id").and_then(Value::as_str) {
        meta.insert("id".to_owned(), Value::String(id.to_owned()));
    }
    object_meta(meta)
}

fn object_meta(map: serde_json::Map<String, Value>) -> ProviderMetadata {
    if map.is_empty() {
        return ProviderMetadata::default();
    }
    let raw = RawValue::from_string(Value::Object(map).to_string()).expect("json object is valid");
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

fn str_field(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn hosted_item_id(value: &Value) -> String {
    value
        .get("item_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn web_search_summary(value: &Value) -> String {
    let Some(action) = value.get("action") else {
        return if value.get("status").and_then(Value::as_str) == Some("failed") {
            "search failed".to_owned()
        } else {
            "search completed".to_owned()
        };
    };
    match action.get("type").and_then(Value::as_str) {
        Some("search") => {
            let queries = action
                .get("queries")
                .and_then(Value::as_array)
                .map(|queries| {
                    queries
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|queries| !queries.is_empty())
                .or_else(|| {
                    action
                        .get("query")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            queries.map_or_else(
                || "search completed".to_owned(),
                |q| format!("searched: {q}"),
            )
        }
        Some("open_page") => action
            .get("url")
            .and_then(Value::as_str)
            .map_or_else(|| "page opened".to_owned(), |url| format!("opened: {url}")),
        Some("find_in_page") => action.get("pattern").and_then(Value::as_str).map_or_else(
            || "page searched".to_owned(),
            |pattern| format!("found: {pattern}"),
        ),
        _ => "search completed".to_owned(),
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    input: Vec<WireItem<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    parallel_tool_calls: bool,
    reasoning: Reasoning,
    include: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

#[derive(Serialize)]
struct Reasoning {
    effort: String,
    summary: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireItem<'a> {
    Message {
        role: &'a str,
        content: Vec<WireContent>,
    },
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        summary: Vec<WireSummary<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    FunctionCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
    FunctionCallOutput {
        call_id: &'a str,
        output: &'a str,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContent {
    InputText { text: String },
    OutputText { text: String },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireSummary<'a> {
    SummaryText { text: &'a str },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireTool<'a> {
    Function {
        name: &'a str,
        description: &'a str,
        parameters: &'a Value,
        strict: bool,
    },
    WebSearch {
        external_web_access: bool,
    },
}

fn build_body(req: &Request, include_max_output_tokens: bool) -> WireRequest<'_> {
    let mut input = Vec::new();
    for msg in &req.messages {
        wire_items(msg, &mut input);
    }

    let mut tools: Vec<_> = req
        .tools
        .iter()
        .map(|t| WireTool::Function {
            name: &t.name,
            description: &t.description,
            parameters: &t.input_schema,
            strict: false,
        })
        .collect();
    tools.push(WireTool::WebSearch {
        external_web_access: true,
    });

    WireRequest {
        model: &req.model,
        stream: true,
        store: false,
        instructions: if req.system.is_empty() {
            None
        } else {
            Some(&req.system)
        },
        input,
        tools,
        parallel_tool_calls: true,
        reasoning: Reasoning {
            effort: req
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| DEFAULT_EFFORT.to_owned()),
            summary: "auto",
        },
        include: vec!["reasoning.encrypted_content"],
        max_output_tokens: include_max_output_tokens.then_some(req.max_tokens),
    }
}

fn wire_items<'a>(msg: &'a Message, out: &mut Vec<WireItem<'a>>) {
    match msg.role {
        Role::User => {
            let text = collect_text(msg);
            if !text.is_empty() {
                out.push(WireItem::Message {
                    role: "user",
                    content: vec![WireContent::InputText { text }],
                });
            }
        }
        Role::Assistant => {
            for block in &msg.blocks {
                match block {
                    ContentBlock::Text { text, .. } => out.push(WireItem::Message {
                        role: "assistant",
                        content: vec![WireContent::OutputText { text: text.clone() }],
                    }),
                    ContentBlock::Thinking { text, meta } => {
                        if let Some(encrypted) = meta_field(meta, "encrypted_content") {
                            let summary = if text.is_empty() {
                                Vec::new()
                            } else {
                                vec![WireSummary::SummaryText { text }]
                            };
                            out.push(WireItem::Reasoning {
                                id: meta_field(meta, "id"),
                                summary,
                                encrypted_content: Some(encrypted),
                            });
                        }
                    }
                    ContentBlock::ToolCall {
                        id,
                        name,
                        args,
                        meta,
                    } => out.push(WireItem::FunctionCall {
                        id: meta_field(meta, "id"),
                        call_id: &id.0,
                        name,
                        arguments: args.get(),
                    }),
                    ContentBlock::ToolResult { .. } => {}
                }
            }
        }
        Role::Tool => {
            for block in &msg.blocks {
                if let ContentBlock::ToolResult { call, output, .. } = block {
                    let ToolOutput::Text(content) = output;
                    out.push(WireItem::FunctionCallOutput {
                        call_id: &call.0,
                        output: content,
                    });
                }
            }
        }
    }
}

fn collect_text(msg: &Message) -> String {
    let mut text = String::new();
    for block in &msg.blocks {
        if let ContentBlock::Text { text: fragment, .. } = block {
            text.push_str(fragment);
        }
    }
    text
}

fn meta_field(meta: &ProviderMetadata, key: &str) -> Option<String> {
    let raw = meta.get(PROVIDER)?;
    let value: Value = serde_json::from_str(raw.get()).ok()?;
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;
    use tokio_agent_core::tool::ToolDef;

    fn drive(assembler: &mut Assembler, frames: &[&str]) -> Vec<Event> {
        frames.iter().flat_map(|f| assembler.push(f)).collect()
    }

    #[tokio::test]
    async fn cancellation_before_http_response_returns_interrupted_done() {
        let provider = OpenAi::new("unused".to_owned(), Some("http://127.0.0.1:1".to_owned()));
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
    fn assembler_builds_reasoning_and_tool_call() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
                r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"think"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"think"}],"encrypted_content":"ENC=="}}"#,
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}"#,
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":\"a.rs\"}"}"#,
                r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.rs\"}"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":2},"output_tokens":5}}}"#,
            ],
        );

        assert!(
            matches!(events.first(), Some(Event::ThinkingDelta { text }) if text == "think"),
            "reasoning summary must render before anything else, got {events:?}"
        );

        let Some(Event::Done { stop, message }) = events.last() else {
            panic!("expected terminal Done, got {events:?}");
        };
        assert_eq!(*stop, StopReason::ToolUse);
        assert_eq!(message.blocks.len(), 2);

        match &message.blocks[0] {
            ContentBlock::Thinking { text, meta } => {
                assert_eq!(text, "think");
                assert!(meta.get(PROVIDER).unwrap().get().contains("ENC=="));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
        match &message.blocks[1] {
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
        assert_eq!(usage.cache_read_tokens, 2);
    }

    #[test]
    fn interruption_preserves_partial_reasoning_metadata() {
        let mut asm = Assembler::new();
        drive(
            &mut asm,
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
                r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"partial"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"partial"}],"encrypted_content":"ENC"}}"#,
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
        assert!(meta.get(PROVIDER).is_some());
    }

    #[test]
    fn args_delta_resolves_call_id_from_item_id() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_9","call_id":"call_9","name":"read","arguments":""}}"#,
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_9","delta":"{}"}"#,
            ],
        );
        assert!(matches!(
            &events[1],
            Event::ToolCallArgs { id, .. } if id.0 == "call_9"
        ));
    }

    #[test]
    fn body_uses_responses_shape_with_reasoning_and_roundtrips_items() {
        let req = Request {
            model: "gpt-5.6-sol".to_owned(),
            system: "sys".to_owned(),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    blocks: vec![
                        ContentBlock::Thinking {
                            text: "hmm".to_owned(),
                            meta: ProviderMetadata::from_provider(
                                PROVIDER.to_owned(),
                                RawValue::from_string(
                                    r#"{"id":"rs_1","encrypted_content":"ENC"}"#.to_owned(),
                                )
                                .unwrap(),
                            ),
                        },
                        ContentBlock::ToolCall {
                            id: ToolCallId("call_1".to_owned()),
                            name: "read".to_owned(),
                            args: RawValue::from_string(r#"{"path":"a.rs"}"#.to_owned()).unwrap(),
                            meta: ProviderMetadata::default(),
                        },
                    ],
                    usage: None,
                },
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
            ],
            tools: vec![ToolDef {
                name: "read".to_owned(),
                description: "read".to_owned(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 256,
            reasoning_effort: Some("high".to_owned()),
        };

        let wire = serde_json::to_value(build_body(&req, true)).unwrap();
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["store"], false);
        assert_eq!(wire["instructions"], "sys");
        assert_eq!(wire["reasoning"]["effort"], "high");
        assert_eq!(wire["reasoning"]["summary"], "auto");
        assert_eq!(wire["max_output_tokens"], 256);
        assert_eq!(wire["include"][0], "reasoning.encrypted_content");
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["name"], "read");
        assert_eq!(wire["tools"][1]["type"], "web_search");
        assert_eq!(wire["tools"][1]["external_web_access"], true);

        let input = wire["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "ENC");
        assert_eq!(input[0]["summary"][0]["text"], "hmm");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["arguments"], r#"{"path":"a.rs"}"#);
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "42");
    }

    #[test]
    fn chatgpt_body_omits_unsupported_max_output_tokens() {
        let req = Request {
            model: "gpt-5.6-sol".to_owned(),
            system: String::new(),
            messages: vec![],
            tools: vec![],
            max_tokens: 8192,
            reasoning_effort: None,
        };

        let wire = serde_json::to_value(build_body(&req, false)).unwrap();
        assert!(wire.get("max_output_tokens").is_none());
        assert_eq!(
            wire["tools"],
            json!([{
                "type": "web_search",
                "external_web_access": true
            }])
        );
    }

    #[test]
    fn hosted_search_items_are_server_side_and_do_not_become_local_tool_calls() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"current news"}}}"#,
                r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Search-backed answer"}]}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":3}}}"#,
            ],
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::ToolCallStart { .. }))
        );
        assert!(matches!(
            &events[0],
            Event::HostedToolCallStart { id, name, .. }
                if id.0 == "ws_1" && name == "web_search"
        ));
        assert!(matches!(
            &events[1],
            Event::HostedToolCallEnd { output, .. } if output == "searched: current news"
        ));
        let Some(Event::Done { stop, message }) = events.last() else {
            panic!("expected terminal Done, got {events:?}");
        };
        assert_eq!(*stop, StopReason::EndTurn);
        assert!(matches!(
            &message.blocks[..],
            [ContentBlock::Text { text, .. }] if text == "Search-backed answer"
        ));
    }

    #[test]
    fn citation_markers_are_filtered_across_streaming_delta_boundaries() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"response.output_text.delta","delta":"Fact. ci"}"#,
                r#"{"type":"response.output_text.delta","delta":"teturn0search0 More."}"#,
            ],
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::TextDelta { text } if text == "Fact. "));
        assert!(matches!(&events[1], Event::TextDelta { text } if text == " More."));
    }

    #[test]
    fn structured_url_citations_become_a_readable_sources_section() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"Fact. citeturn0search0"}"#,
                r#"{"type":"response.output_text.annotation.added","item_id":"msg_1","annotation":{"type":"url_citation","title":"OpenAI Models","url":"https://developers.openai.com/api/docs/models","start_index":6,"end_index":31}}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"Fact. citeturn0search0","annotations":[{"type":"url_citation","title":"OpenAI Models","url":"https://developers.openai.com/api/docs/models","start_index":6,"end_index":31}]}]}}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}}"#,
            ],
        );
        let source = "\n\nSources:\n- OpenAI Models: https://developers.openai.com/api/docs/models";
        assert!(matches!(&events[1], Event::TextDelta { text } if text == source));
        let Some(Event::Done { message, .. }) = events.last() else {
            panic!("expected terminal event, got {events:?}");
        };
        assert!(matches!(
            &message.blocks[..],
            [ContentBlock::Text { text, .. }] if text == &format!("Fact. {source}")
        ));
    }

    #[test]
    fn web_search_action_sources_are_used_when_annotations_are_absent() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"response.output_item.added","item":{"type":"web_search_call","id":"ws_3"}}"#,
                r#"{"type":"response.output_item.done","item":{"type":"web_search_call","id":"ws_3","status":"completed","action":{"type":"search","queries":["docs"],"sources":[{"type":"url","url":"https://example.com/docs"}]}}}"#,
                r#"{"type":"response.output_item.done","item":{"type":"message","id":"msg_3","content":[{"type":"output_text","text":"Answer citeturn0search0","annotations":[]}]}}"#,
            ],
        );
        assert!(events.iter().any(|event| matches!(
            event,
            Event::TextDelta { text }
                if text.contains("Sources:\n- https://example.com/docs: https://example.com/docs")
        )));
    }

    #[test]
    fn dedicated_hosted_search_lifecycle_events_are_deduplicated() {
        let mut asm = Assembler::new();
        let events = drive(
            &mut asm,
            &[
                r#"{"type":"response.web_search_call.in_progress","item_id":"ws_2","output_index":0}"#,
                r#"{"type":"response.web_search_call.searching","item_id":"ws_2","output_index":0}"#,
                r#"{"type":"response.web_search_call.completed","item_id":"ws_2","output_index":0}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], Event::HostedToolCallStart { id, .. } if id.0 == "ws_2"));
        assert!(matches!(&events[1], Event::HostedToolCallEnd { id, .. } if id.0 == "ws_2"));
    }

    #[test]
    fn unsigned_reasoning_is_dropped_from_body() {
        let req = Request {
            model: "gpt-5.6-sol".to_owned(),
            system: String::new(),
            messages: vec![Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Thinking {
                    text: "unencrypted".to_owned(),
                    meta: ProviderMetadata::default(),
                }],
                usage: None,
            }],
            tools: vec![],
            max_tokens: 16,
            reasoning_effort: None,
        };
        let wire = serde_json::to_value(build_body(&req, true)).unwrap();
        assert!(
            wire["input"].as_array().unwrap().is_empty(),
            "reasoning without encrypted content cannot be replayed and must be dropped"
        );
        assert!(wire.get("instructions").is_none(), "empty system omitted");
        assert_eq!(
            wire["reasoning"]["effort"], "medium",
            "effort defaults to medium when unset"
        );
    }
}
