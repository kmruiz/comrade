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
