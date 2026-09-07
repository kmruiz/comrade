use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::config::LlmCfg;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    stream: bool,
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

/// A single streaming chunk: `choices[0].delta.content`.
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
}

/// A thin, synchronous-to-call OpenAI-compatible chat client.
///
/// v1 uses plain text completion (no native function-calling); the ReAct
/// protocol renders tool use in-band. The struct is deliberately small so a
/// native `tool_calls` mode can be layered on later.
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

    /// Send the whole conversation and return the assistant's reply.
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<String> {
        let body = ChatRequest {
            model: &self.cfg.model,
            messages,
            temperature: self.cfg.temperature,
            stream: false,
        };
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

    /// Stream a chat completion, invoking `on_delta` with each content piece as
    /// it arrives (SSE). Returns the fully accumulated reply so the caller gets
    /// both a live feed and the final string.
    pub async fn chat_stream<F>(&self, messages: &[ChatMessage], on_delta: F) -> Result<String>
    where
        F: FnMut(&str) + Send,
    {
        let body = ChatRequest {
            model: &self.cfg.model,
            messages,
            temperature: self.cfg.temperature,
            stream: true,
        };
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

        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.context("error while streaming llm response")?;
            for event in decoder.push(&chunk) {
                match event {
                    SseEvent::Done => return Ok(full.trim().to_string()),
                    SseEvent::Data(json) => {
                        if let Ok(parsed) = serde_json::from_str::<StreamChunk>(&json) {
                            if let Some(delta) = parsed
                                .choices
                                .into_iter()
                                .next()
                                .and_then(|c| c.delta.content)
                            {
                                on_delta(&delta);
                                full.push_str(&delta);
                            }
                        }
                    }
                    SseEvent::Comment => {}
                }
            }
        }
        Ok(full.trim().to_string())
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
}
