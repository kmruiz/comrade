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
