use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use comrade_tool::ToolSpec;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::LlmCfg;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A function call the assistant made (native tool-calling). Serialized in the
/// OpenAI wire format with `arguments` as a JSON string.
#[derive(Debug, Clone, PartialEq)]
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

impl<'de> serde::Deserialize<'de> for ToolCallMsg {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawToolCall {
            id: String,
            #[serde(rename = "type")]
            _type: Option<String>,
            function: RawFunction,
        }

        #[derive(Deserialize)]
        struct RawFunction {
            name: String,
            arguments: String,
        }

        let raw = RawToolCall::deserialize(deserializer)?;
        let arguments = serde_json::from_str::<serde_json::Value>(&raw.function.arguments)
            .unwrap_or(serde_json::Value::Null);
        Ok(ToolCallMsg {
            id: raw.id,
            name: raw.function.name,
            arguments,
        })
    }
}

#[derive(Serialize)]
struct SerializedFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Assistant tool calls (native mode).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_calls: Option<Vec<ToolCallMsg>>,
    /// Links a `Role::Tool` result to the call that produced it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
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
        let stream_options = if stream {
            Some(StreamOptions {
                include_usage: true,
            })
        } else {
            None
        };
        Self {
            model: model.to_string(),
            messages,
            temperature,
            stream,
            tools,
            tool_choice,
            stream_options,
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
    /// Native tool calls in a NON-streaming response.
    #[serde(default)]
    tool_calls: Option<Vec<ChatResponseToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ChatResponseToolCall {
    id: String,
    function: ChatResponseFunction,
}

#[derive(Debug, Deserialize)]
struct ChatResponseFunction {
    name: String,
    arguments: String,
}

/// A tool call the model asked for.
#[derive(Debug, Clone)]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string.
    pub arguments: String,
}

/// Real token usage reported by the model API for one request.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: usize,
    #[serde(default)]
    pub completion_tokens: usize,
    #[serde(default)]
    pub total_tokens: usize,
}

/// The result of one chat request: streamed text plus any native tool calls.
#[derive(Debug, Clone)]
pub struct LlmTurn {
    pub content: String,
    pub tool_calls: Vec<ModelToolCall>,
    /// Real usage reported by the endpoint, when available.
    pub usage: Option<Usage>,
}

/// A single streaming chunk.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
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

/// `Authorization: Bearer <api_key>` headers when a key is configured, so every
/// outbound request (chat and provider probes alike) authenticates the same way.
fn auth_headers(cfg: &LlmCfg) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(key) = &cfg.api_key
        && let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
    {
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    headers
}

/// Short-timeout HTTP client for lightweight provider probes (model version,
/// context window). Carries the API key so authenticated `/models` endpoints
/// (e.g. DeepSeek) answer instead of replying 401.
fn probe_client(cfg: &LlmCfg) -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(5))
        .default_headers(auth_headers(cfg))
        .build()
        .ok()
}

impl LlmClient {
    pub fn new(cfg: &LlmCfg) -> Result<Self> {
        let endpoint = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(15))
            .default_headers(auth_headers(cfg))
            .build()
            .context("failed to build http client")?;
        Ok(Self {
            http,
            cfg: cfg.clone(),
            endpoint,
        })
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// Try to fetch a short display identity/version for the model (Ollama's
    /// `/api/show` details like "7B (Q4_K_M)"). Returns `None` when the endpoint
    /// is not Ollama or does not provide details.
    pub async fn fetch_model_version(&self) -> Option<String> {
        if !self.is_ollama() {
            return None;
        }
        let origin = origin_of(&self.cfg.base_url)?;
        let short = probe_client(&self.cfg)?;
        let body = serde_json::json!({ "name": self.cfg.model });
        let resp = short
            .post(format!("{origin}/api/show"))
            .json(&body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let text = resp.text().await.ok()?;
        model_version_from_ollama_show(&text)
    }

    /// True when the configured endpoint is DeepSeek (provider preset or host).
    pub fn is_deepseek(&self) -> bool {
        if let Some(p) = self.cfg.provider.as_deref() {
            return p.eq_ignore_ascii_case("deepseek");
        }
        origin_of(&self.cfg.base_url)
            .unwrap_or_default()
            .contains("deepseek.com")
    }

    /// Fetch the account balance from DeepSeek's `/user/balance` endpoint,
    /// returned as a short display string (e.g. "110.50 CNY").
    pub async fn fetch_account_balance(&self) -> Option<String> {
        if !self.is_deepseek() {
            return None;
        }
        let origin = origin_of(&self.cfg.base_url)?;
        let url = format!("{origin}/user/balance");
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let text = resp.text().await.ok()?;
        parse_balance(&text)
    }

    /// Try to detect the model's context window (tokens). Best effort:
    /// 1. OpenAI-compatible `GET /models` (`context_length`/`context_window`),
    ///    sent with the configured API key so authenticated providers (e.g.
    ///    DeepSeek) don't reject the probe with 401;
    /// 2. Ollama's native `GET /api/show` (`model_info...context_length`) -
    ///    only probed when the endpoint actually looks like Ollama;
    /// 3. a model-name heuristic (e.g. deepseek-chat -> 128K).
    ///
    /// Returns `None` only if nothing is known.
    pub async fn fetch_context_window(&self) -> Option<usize> {
        let Some(short) = probe_client(&self.cfg) else {
            return heuristic_context(&self.cfg.model);
        };
        // 1) OpenAI-compatible models list.
        let base = self.cfg.base_url.trim_end_matches('/');
        let models_url = format!("{base}/models");
        if let Ok(resp) = short.get(&models_url).send().await
            && resp.status().is_success()
            && let Ok(text) = resp.text().await
            && let Some(n) = model_context_from_openai(&text, &self.cfg.model)
        {
            return Some(n);
        }
        // 2) Ollama native show (only for real Ollama endpoints).
        if self.is_ollama()
            && let Some(origin) = origin_of(&self.cfg.base_url)
        {
            let show_url = format!("{origin}/api/show");
            let body = serde_json::json!({ "name": self.cfg.model });
            if let Ok(resp) = short.post(&show_url).json(&body).send().await
                && resp.status().is_success()
                && let Ok(text) = resp.text().await
                && let Some(n) = model_context_from_ollama_show(&text)
            {
                return Some(n);
            }
        }
        // 3) Name-based heuristic fallback for cloud providers.
        heuristic_context(&self.cfg.model)
    }

    /// Best guess whether the configured endpoint is an Ollama instance.
    fn is_ollama(&self) -> bool {
        if let Some(p) = self.cfg.provider.as_deref() {
            return matches!(p.to_ascii_lowercase().as_str(), "ollama" | "local");
        }
        let host = origin_of(&self.cfg.base_url).unwrap_or_default();
        host.contains("localhost") || host.contains("127.0.0.1") || host.contains("11434")
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

    /// One non-streaming chat round-trip with optional native tools. Unlike
    /// [`chat`] the reply keeps any `tool_calls` the model made, so the caller
    /// can run tools and feed results back — the delegate sub-agent loop uses
    /// this because it needs tools but has no UI to stream tokens to.
    pub async fn chat_turn_once(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
    ) -> Result<LlmTurn> {
        let body = ChatRequest::new(
            &self.cfg.model,
            messages,
            false,
            tools,
            self.cfg.temperature,
        );
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
        let message = parsed.choices.into_iter().next().map(|c| c.message);
        let Some(message) = message else {
            return Ok(LlmTurn {
                content: String::new(),
                tool_calls: Vec::new(),
                usage: None,
            });
        };
        let tool_calls = message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ModelToolCall {
                id: tc.id,
                name: tc.function.name,
                arguments: tc.function.arguments,
            })
            .collect();
        Ok(LlmTurn {
            content: message.content.unwrap_or_default().trim().to_string(),
            tool_calls,
            usage: None,
        })
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
        let mut usage: Option<Usage> = None;

        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.context("error while streaming llm response")?;
            for event in decoder.push(&chunk) {
                match event {
                    SseEvent::Done => {
                        return Ok(finish_turn(full, tool_acc, usage));
                    }
                    SseEvent::Data(json) => {
                        let Ok(parsed) = serde_json::from_str::<StreamChunk>(&json) else {
                            continue;
                        };
                        // usage may arrive on a final chunk with empty choices
                        if let Some(u) = parsed.usage {
                            usage = Some(u);
                        }
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
                                    entry.id.get_or_insert(id);
                                }
                                if let Some(name) =
                                    tc.function.as_ref().and_then(|f| f.name.clone())
                                {
                                    entry.name.get_or_insert(name);
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
        Ok(finish_turn(full, tool_acc, usage))
    }
}

fn finish_turn(
    full: String,
    tool_acc: BTreeMap<usize, ToolAccum>,
    usage: Option<Usage>,
) -> LlmTurn {
    let tool_calls = tool_acc
        .into_values()
        .filter_map(|acc| {
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
        usage,
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
        while let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') {
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

/// Parse DeepSeek `/user/balance` into a short display string.
fn parse_balance(json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(json).ok()?;
    let infos = value.get("balance_infos")?.as_array()?;
    let mut parts = Vec::new();
    for info in infos {
        let currency = info.get("currency").and_then(Value::as_str)?;
        let total = info.get("total_balance").and_then(Value::as_str)?;
        parts.push(format!("{total} {currency}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" + "))
    }
}

/// Name-based context-window fallback for cloud providers whose `/models`
/// endpoint does not advertise context (e.g. DeepSeek).
fn heuristic_context(model: &str) -> Option<usize> {
    let m = model.to_lowercase();
    let suffix = [
        ("1.5m", 1_500_000usize),
        ("1m", 1_000_000),
        ("128k", 131_072),
        ("64k", 65_536),
        ("32k", 32_768),
        ("16k", 16_384),
        ("8k", 8_192),
        ("4k", 4_096),
        ("2k", 2_048),
    ];
    for (needle, size) in suffix {
        if m.contains(needle) {
            return Some(size);
        }
    }
    // DeepSeek models expose a ~1M token context window, but its `/models`
    // endpoint does not advertise it - fall back to the full window.
    if m.contains("deepseek") {
        return Some(1_000_000);
    }
    if m.contains("gpt-4o") {
        return Some(128_000);
    }
    // Mistral AI models: the `/models` endpoint advertises max_context_length,
    // but fall back to the family default when the probe is unavailable.
    if m.contains("codestral") {
        return Some(32_768); // Codestral (2405): 32K
    }
    if m.contains("mistral")
        || m.contains("devstral")
        || m.contains("pixtral")
        || m.contains("ministral")
        || m.contains("magistral")
    {
        return Some(131_072); // 128K (Large/Medium/Small, Devstral, Nemo, ...)
    }
    None
}

/// Extract the model's context window from an OpenAI-compatible `/models`
/// JSON body, matching by model id.
fn model_context_from_openai(json: &str, model: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(json).ok()?;
    let data = value.get("data")?.as_array()?;
    for entry in data {
        if entry.get("id").and_then(Value::as_str) != Some(model) {
            continue;
        }
        for key in [
            "context_length",
            "context_window",
            "max_context_length",
            "max_model_len",
            "context_size",
        ] {
            if let Some(n) = entry.get(key).and_then(Value::as_u64)
                && n > 0
            {
                return Some(n as usize);
            }
        }
    }
    None
}

/// Extract the context window from an Ollama `/api/show` body by scanning
/// `model_info` for any `...context_length` integer.
fn model_context_from_ollama_show(json: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(json).ok()?;
    let info = value
        .get("model_info")
        .or_else(|| value.get("model"))
        .or(Some(&value))?;
    let mut found: Option<usize> = None;
    fn walk(v: &Value, last_key: Option<&str>, out: &mut Option<usize>) {
        match v {
            Value::Number(n) => {
                if last_key.is_some_and(|k| k.ends_with("context_length"))
                    && let Some(u) = n.as_u64()
                    && u > 0
                {
                    *out = Some(u as usize);
                }
            }
            Value::Object(map) => {
                for (k, val) in map {
                    if out.is_some() {
                        return;
                    }
                    walk(val, Some(k.as_str()), out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    if out.is_some() {
                        return;
                    }
                    walk(item, last_key, out);
                }
            }
            _ => {}
        }
    }
    walk(info, None, &mut found);
    found
}

/// `scheme://host[:port]` from a base URL such as `http://localhost:11434/v1`.
fn origin_of(base_url: &str) -> Option<String> {
    let (scheme, rest) = base_url.split_once("://")?;
    let host = rest.split('/').next().unwrap_or(rest);
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
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
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7,\"total_tokens\":49}}\n\n",
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

        // the final chunk reported real usage
        let usage = turn.usage.expect("usage should be reported");
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 49);
    }
}

/// Extract a display identity from an Ollama `/api/show` body's `details`
/// object, e.g. "7B (Q4_K_M)".
fn model_version_from_ollama_show(json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(json).ok()?;
    let details = value.get("details")?.as_object()?;
    let size = details
        .get("parameter_size")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let quant = details
        .get("quantization_level")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (size, quant) {
        (Some(size), Some(quant)) => Some(format!("{size} ({quant})")),
        (Some(size), None) => Some(size.to_string()),
        (None, Some(quant)) => Some(quant.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod detect_tests {
    use super::*;

    #[test]
    fn reads_openai_context_window() {
        let json = r#"{
          "object": "list",
          "data": [
            { "id": "other", "context_length": 4096 },
            { "id": "mistral:latest", "context_length": 32768 }
          ]
        }"#;
        assert_eq!(
            model_context_from_openai(json, "mistral:latest"),
            Some(32768)
        );
        assert_eq!(model_context_from_openai(json, "unknown"), None);
    }

    #[test]
    fn reads_ollama_show_version() {
        let json = r#"{
          "details": {
            "parameter_size": "7.2B",
            "quantization_level": "Q4_K_M"
          }
        }"#;
        assert_eq!(
            model_version_from_ollama_show(json).as_deref(),
            Some("7.2B (Q4_K_M)")
        );
        assert_eq!(model_version_from_ollama_show("{}"), None);
    }

    #[test]
    fn reads_ollama_show_context_length() {
        let json = r#"{
          "model_info": {
            "general.architecture": "llama",
            "llama.context_length": 8192,
            "llama.embedding_length": 4096
          }
        }"#;
        assert_eq!(model_context_from_ollama_show(json), Some(8192));
        assert_eq!(model_context_from_ollama_show("{}"), None);
    }

    #[test]
    fn origins_are_derived() {
        assert_eq!(
            origin_of("http://localhost:11434/v1").as_deref(),
            Some("http://localhost:11434")
        );
        assert_eq!(
            origin_of("https://host.example/foo/bar").as_deref(),
            Some("https://host.example")
        );
    }
}

#[cfg(test)]
mod heuristic_tests {
    use super::*;

    #[test]
    fn deepseek_models_get_the_full_1m_window() {
        assert_eq!(heuristic_context("deepseek-v4-flash"), Some(1_000_000));
        assert_eq!(heuristic_context("deepseek-v4-pro"), Some(1_000_000));
        assert_eq!(
            heuristic_context("deepseek-v4-flash-vision-exp"),
            Some(1_000_000)
        );
        assert_eq!(heuristic_context("deepseek-chat"), Some(1_000_000));
    }

    #[test]
    fn k_suffixes_are_recognized() {
        assert_eq!(heuristic_context("something-128k"), Some(131_072));
        assert_eq!(heuristic_context("foo-32k"), Some(32_768));
    }

    #[test]
    fn unknown_models_return_none() {
        assert_eq!(heuristic_context("totally-unknown-model"), None);
    }

    #[test]
    fn mistral_family_gets_128k() {
        assert_eq!(heuristic_context("mistral-large-latest"), Some(131_072));
        assert_eq!(heuristic_context("mistral-small-latest"), Some(131_072));
        assert_eq!(heuristic_context("devstral-small-2507"), Some(131_072));
        assert_eq!(heuristic_context("pixtral-large-latest"), Some(131_072));
        assert_eq!(heuristic_context("ministral-8b-latest"), Some(131_072));
        assert_eq!(heuristic_context("codestral-latest"), Some(32_768));
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use std::io::{Read, Write};

    /// Serve one HTTP request on a loopback listener: reply 200 with an
    /// OpenAI-compatible model list when the request carries the API key,
    /// otherwise 401. Returns whether the request was authenticated.
    fn serve_once_reply() -> (std::net::SocketAddr, std::thread::JoinHandle<bool>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = stream.read(&mut buf).unwrap();
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&head).to_lowercase();
            let authed = head.contains("authorization: bearer sk-test");
            let (status, reason, body) = if authed {
                (
                    "200",
                    "OK",
                    r#"{"object":"list","data":[{"id":"deepseek-chat","context_length":131072}]}"#,
                )
            } else {
                ("401", "Unauthorized", r#"{"error":"missing key"}"#)
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            authed
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn context_window_probe_authenticates_deepseek_models_request() {
        // DeepSeek's `/models` endpoint requires `Authorization: Bearer`; the
        // probe must send the configured key or it is 401ed and the advertised
        // context window can never be read from the provider API.
        let (addr, server) = serve_once_reply();
        let cfg = LlmCfg {
            base_url: format!("http://{addr}/v1"),
            api_key: Some("sk-test".into()),
            model: "deepseek-chat".into(),
            ..Default::default()
        };
        let client = LlmClient::new(&cfg).unwrap();
        assert_eq!(client.fetch_context_window().await, Some(131_072));
        assert!(server.join().unwrap(), "probe request was unauthenticated");
    }
}

#[cfg(test)]
mod balance_tests {
    use super::*;

    #[test]
    fn parses_deepseek_balance() {
        let json = r#"{
          "is_available": true,
          "balance_infos": [
            { "currency": "CNY", "total_balance": "110.50", "granted_balance": "10.00", "topped_up_balance": "100.50" }
          ]
        }"#;
        assert_eq!(parse_balance(json).as_deref(), Some("110.50 CNY"));
        assert_eq!(parse_balance("{}"), None);
    }
}

#[cfg(test)]
mod serde_tests {
    use super::*;

    #[test]
    fn chat_message_serde_roundtrip() {
        let sys = ChatMessage::new(Role::System, "You are a helpful assistant");
        let sys_json = serde_json::to_string(&sys).unwrap();
        let sys_de: ChatMessage = serde_json::from_str(&sys_json).unwrap();
        assert_eq!(sys.role, sys_de.role);
        assert_eq!(sys.content, sys_de.content);
        assert_eq!(sys.tool_calls, sys_de.tool_calls);
        assert_eq!(sys.tool_call_id, sys_de.tool_call_id);

        // Test assistant with tool calls
        let assistant = ChatMessage::assistant_with_calls(
            "x".to_string(),
            vec![ToolCallMsg {
                id: "c1".into(),
                name: "fs_read".into(),
                arguments: serde_json::json!({"path": "x"}),
            }],
        );
        let assistant_json = serde_json::to_string(&assistant).unwrap();
        let assistant_de: ChatMessage = serde_json::from_str(&assistant_json).unwrap();
        assert_eq!(assistant.role, assistant_de.role);
        assert_eq!(assistant.content, assistant_de.content);
        assert_eq!(
            assistant.tool_calls.as_ref().unwrap().len(),
            assistant_de.tool_calls.as_ref().unwrap().len()
        );
        let call = &assistant.tool_calls.as_ref().unwrap()[0];
        let call_de = &assistant_de.tool_calls.as_ref().unwrap()[0];
        assert_eq!(call.id, call_de.id);
        assert_eq!(call.name, call_de.name);
        assert_eq!(call.arguments, call_de.arguments);

        // Test tool result
        let tool_result = ChatMessage::tool_result("c1", "ok");
        let tool_result_json = serde_json::to_string(&tool_result).unwrap();
        let tool_result_de: ChatMessage = serde_json::from_str(&tool_result_json).unwrap();
        assert_eq!(tool_result.role, tool_result_de.role);
        assert_eq!(tool_result.content, tool_result_de.content);
        assert_eq!(tool_result.tool_call_id, tool_result_de.tool_call_id);
    }
}
