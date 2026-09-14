use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context as _, Result};
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

/// Anthropic/OpenRouter-style prompt-cache marker. Providers that do not
/// support it ignore the unknown field, so it is safe to send when enabled.
#[derive(Debug, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

const EPHEMERAL: CacheControl = CacheControl { kind: "ephemeral" };

#[derive(Debug, Serialize)]
struct ToolDef<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionDef<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: String,
    messages: Vec<Value>,
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
        prompt_caching: bool,
    ) -> Self {
        // Serialize messages as loose JSON so an opt-in `cache_control` marker
        // can be added to the (stable, large) system message without touching
        // the wire format otherwise.
        let mut messages: Vec<Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect();
        if prompt_caching
            && let Some(sys) = messages
                .iter_mut()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
            && let Some(obj) = sys.as_object_mut()
        {
            obj.insert(
                "cache_control".into(),
                serde_json::to_value(EPHEMERAL).unwrap_or(Value::Null),
            );
        }

        let count = tools.map(<[ToolSpec]>::len).unwrap_or(0);
        let tools = tools.map(|specs| {
            specs
                .iter()
                .enumerate()
                .map(|(i, spec)| ToolDef {
                    kind: "function",
                    function: FunctionDef {
                        name: &spec.name,
                        description: &spec.description,
                        parameters: &spec.json_schema,
                    },
                    // Cache the tool block up to and including its last entry.
                    cache_control: (prompt_caching && i + 1 == count).then_some(EPHEMERAL),
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

/// A non-success HTTP status from the provider, kept typed so the retry loop
/// can tell a transient 429/5xx from a permanent 4xx instead of re-parsing a
/// string.
#[derive(Debug, thiserror::Error)]
#[error("llm error {status}: {body}")]
struct LlmHttpError {
    status: reqwest::StatusCode,
    body: String,
}

/// Whether a provider status is worth retrying: rate limits, request timeouts
/// and server-side hiccups. Client errors (bad key, bad request) are not.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status.as_u16(),
        408 | 425 | 429 | 500 | 502 | 503 | 504 | 529
    )
}

/// Whether a failed request is transient enough to retry: transport failures
/// (connection reset/refused, DNS, timeouts, a truncated body) and retryable
/// HTTP statuses. A malformed response or a permanent 4xx propagates at once.
fn is_retryable(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(http) = cause.downcast_ref::<LlmHttpError>() {
            return is_retryable_status(http.status);
        }
        if let Some(req) = cause.downcast_ref::<reqwest::Error>() {
            return req.is_timeout() || req.is_connect() || req.is_request() || req.is_body();
        }
    }
    false
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

    /// Total tries for one request: the initial attempt plus `max_retries`.
    fn max_attempts(&self) -> u32 {
        self.cfg.max_retries.saturating_add(1).max(1)
    }

    /// Whether a failed 0-based `attempt` may be retried after `err`.
    fn should_retry(&self, attempt: u32, err: &anyhow::Error) -> bool {
        attempt + 1 < self.max_attempts() && is_retryable(err)
    }

    /// Sleep before the next attempt: exponential backoff (doubling per
    /// attempt) capped at 8s, so a flaky provider is not hammered and a
    /// sustained outage still fails within a few seconds.
    async fn backoff(&self, attempt: u32) {
        let base = self.cfg.retry_backoff_ms.max(1);
        let delay = base.saturating_mul(1u64 << attempt.min(6)).min(8_000);
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }

    /// POST one chat request and return the successful response. A non-success
    /// status is consumed into a typed [`LlmHttpError`] the caller may classify
    /// as retryable.
    async fn send_once(&self, body: &ChatRequest<'_>) -> Result<reqwest::Response> {
        let resp = self
            .http
            .post(&self.endpoint)
            .json(body)
            .send()
            .await
            .with_context(|| format!("request to {} failed", self.endpoint))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmHttpError { status, body }.into());
        }
        Ok(resp)
    }

    /// Send a non-streaming request, retrying transient failures with backoff.
    async fn send_with_retry(&self, body: &ChatRequest<'_>) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            match self.send_once(body).await {
                Ok(resp) => return Ok(resp),
                Err(err) if self.should_retry(attempt, &err) => {
                    self.backoff(attempt).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Send the whole conversation (non-streaming) and return the reply text.
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<String> {
        let body = ChatRequest::new(
            &self.cfg.model,
            messages,
            false,
            None,
            self.cfg.temperature,
            self.cfg.prompt_caching,
        );
        let resp = self.send_with_retry(&body).await?;

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
            self.cfg.prompt_caching,
        );
        let resp = self.send_with_retry(&body).await?;

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
        let body = ChatRequest::new(
            &self.cfg.model,
            messages,
            true,
            tools,
            self.cfg.temperature,
            self.cfg.prompt_caching,
        );
        let mut on_delta = on_delta;
        let mut attempt = 0;
        loop {
            // Retry the connect/status phase, and a stream that dies before any
            // token reached the caller: a reset after the first delta would
            // otherwise duplicate what was already shown.
            match self.send_once(&body).await {
                Ok(resp) => {
                    let mut emitted = false;
                    let result = {
                        let mut forward = |piece: &str| {
                            emitted = true;
                            on_delta(piece);
                        };
                        self.stream_turn(resp, &mut forward).await
                    };
                    match result {
                        Ok(turn) => return Ok(turn),
                        Err(err) if !emitted && self.should_retry(attempt, &err) => {
                            self.backoff(attempt).await;
                            attempt += 1;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if self.should_retry(attempt, &err) => {
                    self.backoff(attempt).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Drive one already-connected streaming response to completion, forwarding
    /// each content piece to `on_delta` and accumulating native `tool_calls`.
    async fn stream_turn<F>(&self, resp: reqwest::Response, on_delta: F) -> Result<LlmTurn>
    where
        F: FnMut(&str) + Send,
    {
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

mod ollama;
pub(crate) use ollama::*;
#[cfg(test)]
mod balance_tests;
mod context_window;
#[cfg(test)]
mod detect_tests;
#[cfg(test)]
mod heuristic_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod retry_tests;
#[cfg(test)]
mod serde_tests;
#[cfg(test)]
mod tests;
pub(crate) use context_window::*;
