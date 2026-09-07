use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use comrade_tool::ToolSpec;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::LlmCfg;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A function call the assistant made (native tool-calling). Serialized in the
/// OpenAI wire format with `arguments` as a JSON string.
#[derive(Debug, Clone)]
pub struct ToolCallMsg {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl Serialize for ToolCallMsg {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let arguments = serde_json::to_string(&self.arguments).unwrap_or_default();
        let mut s = serializer.serialize_struct("tool_call", 2)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("type", "function")?;
        s.serialize_field(
            "function",
            &SerializedFunction {
                name: &self.name,
                arguments: &arguments,
            },
        )?;
        s.end()
    }
}

#[derive(Serialize)]
struct SerializedFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Assistant tool calls (native mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallMsg>>,
    /// Links a `Role::Tool` result to the call that produced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_with_calls(content: String, calls: Vec<ToolCallMsg>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: Some(calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(id.into()),
        }
    }
}

#[derive(Debug, Serialize)]
struct FunctionDef<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Debug, Serialize)]
struct ToolDef<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionDef<'a>,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: String,
    messages: &'a [ChatMessage],
    temperature: f32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDef<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

impl<'a> ChatRequest<'a> {
    fn new(
        model: &str,
        messages: &'a [ChatMessage],
        stream: bool,
        tools: Option<&'a [ToolSpec]>,
        temperature: f32,
    ) -> Self {
        let tools = tools.map(|specs| {
            specs
                .iter()
                .map(|spec| ToolDef {
                    kind: "function",
                    function: FunctionDef {
                        name: &spec.name,
                        description: &spec.description,
                        parameters: &spec.json_schema,
                    },
                })
                .collect()
        });
        let tool_choice = if tools.is_some() { Some("auto") } else { None };
        Self {
            model: model.to_string(),
            messages,
            temperature,
            stream,
            tools,
            tool_choice,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatResponseMessage {
    #[serde(default)]
    content: Option<String>,
}

/// A tool call the model asked for.
#[derive(Debug, Clone)]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string.
    pub arguments: String,
}

/// The result of one chat request: streamed text plus any native tool calls.
#[derive(Debug, Clone)]
pub struct LlmTurn {
    pub content: String,
    pub tool_calls: Vec<ModelToolCall>,
}

/// A single streaming chunk.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Accumulator for a partial streaming tool call.
#[derive(Debug, Default)]
struct ToolAccum {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// A thin OpenAI-compatible chat client.
///
/// Supports both plain-text completion (ReAct) and native function-calling via
/// [`LlmClient::chat_turn`], which streams content and accumulates `tool_calls`.
pub struct LlmClient {
    http: reqwest::Client,
    cfg: LlmCfg,
    endpoint: String,
}

impl LlmClient {
    pub fn new(cfg: &LlmCfg) -> Result<Self> {
        let endpoint = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(15));
        if let Some(key) = &cfg.api_key {
            builder = builder.default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
                    h.insert(reqwest::header::AUTHORIZATION, v);
                }
                h
            });
        }
        let http = builder.build().context("failed to build http client")?;
        Ok(Self {
            http,
            cfg: cfg.clone(),
            endpoint,
        })
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// Send the whole conversation (non-streaming) and return the reply text.
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<String> {
        let body = ChatRequest::new(&self.cfg.model, messages, false, None, self.cfg.temperature);
        let resp = self
            .http
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("request to {} failed", self.endpoint))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("llm error {status}: {text}");
        }
        let parsed: ChatResponse = resp.json().await.context("malformed llm response")?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok(content)
    }

    /// Stream a chat completion without native tools; returns the reply text.
    pub async fn chat_stream<F>(&self, messages: &[ChatMessage], on_delta: F) -> Result<String>
    where
        F: FnMut(&str) + Send,
    {
        Ok(self.chat_turn(messages, None, on_delta).await?.content)
    }

    /// Stream a chat completion, invoking `on_delta` with each content piece as
    /// it arrives. When `tools` is provided the request advertises them and the
    /// model may respond with native `tool_calls`, which are accumulated across
    /// streaming chunks and returned alongside the final text.
    pub async fn chat_turn<F>(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        on_delta: F,
    ) -> Result<LlmTurn>
    where
        F: FnMut(&str) + Send,
    {
        let body = ChatRequest::new(&self.cfg.model, messages, true, tools, self.cfg.temperature);
        let resp = self
            .http
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("request to {} failed", self.endpoint))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("llm error {status}: {text}");
        }

        let mut on_delta = on_delta;
        let mut decoder = SseDecoder::new();
        let mut full = String::new();
        let mut tool_acc: BTreeMap<usize, ToolAccum> = BTreeMap::new();

        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.context("error while streaming llm response")?;
            for event in decoder.push(&chunk) {
                match event {
                    SseEvent::Done => {
                        return Ok(finish_turn(full, tool_acc));
                    }
                    SseEvent::Data(json) => {
                        let Ok(parsed) = serde_json::from_str::<StreamChunk>(&json) else {
                            continue;
                        };
                        let Some(delta) = parsed.choices.into_iter().next().map(|c| c.delta) else {
                            continue;
                        };
                        if let Some(content) = delta.content {
                            on_delta(&content);
                            full.push_str(&content);
                        }
                        if let Some(calls) = delta.tool_calls {
                            for tc in calls {
                                let index = tc.index.unwrap_or(0);
                                let entry = tool_acc.entry(index).or_default();
                                if let Some(id) = tc.id {
                                    entry.id.get_or_insert_with(|| id);
                                }
                                if let Some(name) =
                                    tc.function.as_ref().and_then(|f| f.name.clone())
                                {
                                    entry.name.get_or_insert_with(|| name);
                                }
                                if let Some(args) =
                                    tc.function.as_ref().and_then(|f| f.arguments.clone())
                                {
                                    entry.arguments.push_str(&args);
                                }
                            }
                        }
                    }
                    SseEvent::Comment => {}
                }
            }
        }
        Ok(finish_turn(full, tool_acc))
    }
}

fn finish_turn(full: String, tool_acc: BTreeMap<usize, ToolAccum>) -> LlmTurn {
    let tool_calls = tool_acc
        .into_iter()
        .filter_map(|(_, acc)| {
            let id = acc.id?;
            let name = acc.name?;
            Some(ModelToolCall {
                id,
                name,
                arguments: acc.arguments,
            })
        })
        .collect();
    LlmTurn {
        content: full.trim().to_string(),
        tool_calls,
    }
}

enum SseEvent {
    /// A `data:` payload line.
    Data(String),
    /// The SSE stream terminated.
    Done,
    /// A `:` comment / keep-alive line.
    Comment,
}

/// Incremental SSE parser that tolerates events split across TCP frames.
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feed bytes; returns any complete events found.
    fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        loop {
            let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') else {
                break;
            };
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(event) = parse_sse_line(line) {
                events.push(event);
            }
        }
        events
    }
}

fn parse_sse_line(line: &str) -> Option<SseEvent> {
    if line.is_empty() {
        return None;
    }
    if line.starts_with(":") {
        return Some(SseEvent::Comment);
    }
    let payload = line.strip_prefix("data:")?.trim_start();
    if payload == "[DONE]" {
        return Some(SseEvent::Done);
    }
    Some(SseEvent::Data(payload.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_sse_across_frames() {
        let mut dec = SseDecoder::new();
        // first frame splits an event in the middle
        let mut events = dec.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel");
        events.extend(
            dec.push(b"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n"),
        );
        events.extend(dec.push(b"data: [DONE]\n"));
        let mut contents = Vec::new();
        for e in events {
            match e {
                SseEvent::Data(json) => {
                    let chunk: StreamChunk = serde_json::from_str(&json).unwrap();
                    contents.push(
                        chunk
                            .choices
                            .into_iter()
                            .next()
                            .and_then(|c| c.delta.content)
                            .unwrap_or_default(),
                    );
                }
                SseEvent::Done => contents.push("<DONE>".into()),
                SseEvent::Comment => {}
            }
        }
        assert_eq!(
            contents,
            vec!["Hello".to_string(), " world".to_string(), "<DONE>".into()]
        );
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let mut dec = SseDecoder::new();
        let events = dec.push(
            b": keep-alive\n\n: ping\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
        );
        let data_count = events
            .iter()
            .filter(|e| matches!(e, SseEvent::Data(_)))
            .count();
        let comment_count = events
            .iter()
            .filter(|e| matches!(e, SseEvent::Comment))
            .count();
        assert_eq!(data_count, 1);
        assert_eq!(comment_count, 2);
    }

    #[test]
    fn messages_serialize_native_wire_format() {
        let assistant = ChatMessage::assistant_with_calls(
            "".into(),
            vec![ToolCallMsg {
                id: "call_1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "a.rs", "content": "x"}),
            }],
        );
        let json = serde_json::to_value(&assistant).unwrap();
        let calls = json["tool_calls"][0].clone();
        assert_eq!(calls["id"], "call_1");
        assert_eq!(calls["type"], "function");
        assert_eq!(calls["function"]["name"], "write_file");
        assert_eq!(
            calls["function"]["arguments"],
            r#"{"content":"x","path":"a.rs"}"#
        );

        let tool = ChatMessage::tool_result("call_1", "wrote a.rs");
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
    }
}

#[cfg(test)]
mod live_tests {
    use std::io::{Read, Write};

    use super::*;
    use crate::config::LlmCfg;

    #[tokio::test]
    async fn streams_from_a_real_sse_endpoint() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let mut used = 0usize;
            loop {
                let n = stream.read(&mut buf[used..]).unwrap();
                if n == 0 {
                    break;
                }
                used += n;
                if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n",
                "data: [DONE]\n\n"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });

        let cfg = LlmCfg {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            model: "test".into(),
            ..LlmCfg::default()
        };
        let client = LlmClient::new(&cfg).unwrap();
        let mut seen = String::new();
        let full = client.chat_stream(&[], |d| seen.push_str(d)).await.unwrap();
        assert_eq!(seen, "Hello world!");
        assert_eq!(full, "Hello world!");
    }

    #[tokio::test]
    async fn accumulates_streamed_native_tool_calls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let mut used = 0usize;
            loop {
                let n = stream.read(&mut buf[used..]).unwrap();
                if n == 0 {
                    break;
                }
                used += n;
                if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            // name/id arrive in chunk 1; arguments trickle across chunks 2-3.
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"write_file\",\"arguments\":\"\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\": \\\"a.r\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"s\\\"}\"}}]}}]}\n\n",
                "data: [DONE]\n\n"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });

        let cfg = LlmCfg {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            model: "test".into(),
            ..LlmCfg::default()
        };
        let client = LlmClient::new(&cfg).unwrap();
        let turn = client.chat_turn(&[], None, |_| {}).await.unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        let call = &turn.tool_calls[0];
        assert_eq!(call.id, "c1");
        assert_eq!(call.name, "write_file");
        let args: Value = serde_json::from_str(&call.arguments).unwrap();
        assert_eq!(args["path"], "a.rs");
    }
}
