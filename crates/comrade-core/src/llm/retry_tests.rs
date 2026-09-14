use super::*;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Loopback server: the first `failures` requests get `fail_status` with an
/// empty body, and the next one gets `ok_body` (after which the thread
/// stops). When `ok_body` is `None` every request fails. Returns the bound
/// address and the number of requests actually served, so a test can assert
/// how many attempts the client made.
fn flaky_server(
    failures: usize,
    fail_status: u16,
    ok_body: Option<&'static str>,
) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&served);
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let idx = counted.fetch_add(1, Ordering::SeqCst);
            drain_request(&mut stream);
            let (status, payload) = if idx < failures {
                (fail_status, String::new())
            } else {
                match ok_body {
                    Some(body) => (200, body.to_string()),
                    None => (fail_status, String::new()),
                }
            };
            let resp = format!(
                "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            if ok_body.is_some() && idx >= failures {
                break;
            }
        }
    });
    (addr, served)
}

/// Read a full HTTP request (headers + Content-Length body) so the client is
/// not reset while it is still writing the request.
fn drain_request(stream: &mut std::net::TcpStream) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    while !data.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
    let head_end = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&data[..head_end]).to_lowercase();
    let want = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut have = data.len() - head_end;
    while have < want {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => have += n,
        }
    }
}

const OK_JSON: &str = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
const OK_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]\n\n";

fn client(addr: std::net::SocketAddr) -> LlmClient {
    let cfg = LlmCfg {
        base_url: format!("http://{addr}/v1"),
        model: "m".into(),
        max_retries: 5,
        retry_backoff_ms: 1,
        ..Default::default()
    };
    LlmClient::new(&cfg).unwrap()
}

#[tokio::test]
async fn chat_retries_transient_5xx_then_succeeds() {
    let (addr, served) = flaky_server(2, 503, Some(OK_JSON));
    let reply = client(addr)
        .chat(&[ChatMessage::new(Role::User, "hi")])
        .await
        .unwrap();
    assert_eq!(reply, "hello");
    assert_eq!(served.load(Ordering::SeqCst), 3, "two 503s then one 200");
}

#[tokio::test]
async fn chat_turn_retries_transient_rate_limit_and_streams() {
    let (addr, served) = flaky_server(1, 429, Some(OK_SSE));
    let mut deltas = String::new();
    let turn = client(addr)
        .chat_turn(&[ChatMessage::new(Role::User, "hi")], None, |d| {
            deltas.push_str(d)
        })
        .await
        .unwrap();
    assert_eq!(turn.content, "hello");
    assert_eq!(deltas, "hello");
    assert_eq!(served.load(Ordering::SeqCst), 2, "one 429 then one 200");
}

#[tokio::test]
async fn permanent_4xx_is_not_retried() {
    let (addr, served) = flaky_server(0, 400, None);
    let err = client(addr)
        .chat(&[ChatMessage::new(Role::User, "hi")])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("400"), "unexpected error: {err}");
    assert_eq!(
        served.load(Ordering::SeqCst),
        1,
        "a 400 must not be retried"
    );
}

#[test]
fn classifies_retryable_statuses() {
    for code in [408, 425, 429, 500, 502, 503, 504, 529] {
        assert!(
            is_retryable_status(reqwest::StatusCode::from_u16(code).unwrap()),
            "{code} should be retryable"
        );
    }
    for code in [400, 401, 403, 404, 422] {
        assert!(
            !is_retryable_status(reqwest::StatusCode::from_u16(code).unwrap()),
            "{code} must not be retryable"
        );
    }
}
