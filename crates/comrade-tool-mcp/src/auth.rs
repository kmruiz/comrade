//! Auth for MCP HTTP servers: static API keys and OIDC (OAuth 2.0
//! authorization-code + PKCE with loopback redirect) resolution into a
//! request header consumed by the rmcp streamable-HTTP transport.

use anyhow::{Context as _, Result, anyhow, bail};
use comrade_core::McpAuth;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::Deserialize;

/// Build the rmcp streamable-HTTP transport config for `url`, attaching an
/// `Authorization`-style header when `auth` resolves to one.
pub async fn http_transport_config(
    url: &str,
    auth: Option<&McpAuth>,
) -> Result<StreamableHttpClientTransportConfig> {
    let mut cfg = StreamableHttpClientTransportConfig::default();
    cfg.uri = url.to_string().into();
    if let Some((name, value)) = resolve_auth_header(auth, url).await? {
        if name.eq_ignore_ascii_case("authorization") {
            cfg.auth_header = Some(value);
        } else {
            let header_name = http::HeaderName::from_bytes(name.as_bytes())?;
            let header_value = http::HeaderValue::from_str(&value)?;
            cfg.custom_headers.insert(header_name, header_value);
        }
    }
    Ok(cfg)
}

/// Produce `(header_name, header_value)` for the configured auth, if any.
///
/// * `api_key` with no custom header → `Authorization: Bearer <key>`.
/// * `api_key` with a custom header → `<header>: <key>` verbatim.
/// * `oidc` → OIDC access token obtained via the authz-code + PKCE flow,
///   sent as `Authorization: Bearer <token>`.
pub async fn resolve_auth_header(
    auth: Option<&McpAuth>,
    url: &str,
) -> Result<Option<(String, String)>> {
    match auth {
        None => Ok(None),
        Some(McpAuth::ApiKey { key, header }) => {
            let value = comrade_core::expand_env_value(key, &|n| std::env::var(n).ok());
            match header {
                Some(h) if !h.is_empty() && !h.eq_ignore_ascii_case("authorization") => {
                    Ok(Some((h.clone(), value)))
                }
                _ => Ok(Some((
                    "Authorization".to_string(),
                    format!("Bearer {value}"),
                ))),
            }
        }
        Some(cfg @ McpAuth::Oidc { .. }) => {
            let token = oidc_access_token(url, cfg).await?;
            Ok(Some((
                "Authorization".to_string(),
                format!("Bearer {}", token.access_token),
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// OIDC / OAuth 2.0 authorization-code + PKCE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct OidcMetadata {
    #[allow(dead_code)]
    issuer: Option<String>,
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: Option<String>,
    #[allow(dead_code)]
    expires_in: Option<u64>,
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

#[derive(Debug, Clone)]
struct OidcToken {
    access_token: String,
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

/// Opens the authorization URL in the system browser. Failures are non-fatal:
/// the URL is already printed to stderr so the user can open it by hand.
fn open_browser(url: &str) {
    if std::env::var_os("COMRADE_MCP_NO_BROWSER").is_some() {
        return;
    }
    if cfg!(target_os = "linux") {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    } else if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("open").arg(url).spawn();
    } else if cfg!(target_os = "windows") {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
}

/// RFC 8414 authorization-server metadata discovery for an MCP endpoint URL.
/// An explicitly configured `issuer` wins; if it does not already point at a
/// `/.well-known/` path, the well-known path is derived from it.
fn metadata_url(url: &str, issuer: &Option<String>) -> Result<String> {
    if let Some(iss) = issuer {
        if iss.contains("/.well-known/") {
            return Ok(iss.clone());
        }
        return Ok(format!(
            "{}/.well-known/oauth-authorization-server",
            iss.trim_end_matches('/')
        ));
    }
    let parsed = url::Url::parse(url)
        .with_context(|| format!("MCP server URL {url:?} is not a valid URL"))?;
    let scheme = parsed.scheme();
    let host = parsed.host_str().context("MCP server URL has no host")?;
    let origin = match parsed.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    };
    Ok(format!("{origin}/.well-known/oauth-authorization-server"))
}

fn random_bytes(bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    // Fallback (non-Linux, tests): xorshift seeded from the clock.
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    for b in buf.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = seed as u8;
    }
    buf
}

/// Base64url (RFC 4648 §5) without padding.
fn base64url(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

fn sha256(input: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().to_vec()
}

/// Wait for the loopback redirect that carries `?code=..&state=..`, answering
/// each request so the browser sees a closing page.
async fn capture_redirect_code(
    listener: &mut tokio::net::TcpListener,
    state: &str,
) -> Result<String> {
    use tokio::io::AsyncReadExt;
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buf = [0u8; 8192];
        let n =
            match tokio::time::timeout(std::time::Duration::from_secs(10), socket.read(&mut buf))
                .await
            {
                Ok(Ok(n)) if n > 0 => n,
                _ => continue,
            };
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
        let Some((_, query)) = path.split_once('?') else {
            respond_http(socket, "<html><body>ok</body></html>").await;
            continue;
        };
        let pairs: std::collections::HashMap<String, String> = query
            .split('&')
            .filter_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                let decoded: String = url::form_urlencoded::parse(format!("k={v}").as_bytes())
                    .map(|(_, val)| val.into_owned())
                    .collect();
                Some((k.to_string(), decoded))
            })
            .collect();
        if pairs.get("state").map(String::as_str) == Some(state) {
            if let Some(code) = pairs.get("code") {
                respond_http(
                    socket,
                    "<html><body><p>Comrade received the code. You can close this tab.</p></body></html>",
                )
                .await;
                return Ok(code.clone());
            }
        }
        respond_http(socket, "<html><body>unexpected callback</body></html>").await;
    }
}

/// Answer a loopback HTTP request so the browser can close the tab.
async fn respond_http(mut socket: tokio::net::TcpStream, body: &str) {
    use tokio::io::AsyncWriteExt;
    let _ = socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
}

/// Full authorization-code + PKCE flow against the MCP server's origin, using
/// `launch` to send the user to the authorization endpoint (the OS browser in
/// production, a test double in tests).
async fn oidc_access_token_with(
    mcp_url: &str,
    cfg: &McpAuth,
    launch: impl Fn(&str),
) -> Result<OidcToken> {
    let McpAuth::Oidc {
        client_id,
        issuer,
        scopes,
        redirect_port,
        audience,
    } = cfg
    else {
        bail!("not an OIDC config");
    };

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building oidc http client")?;

    let meta: OidcMetadata = client
        .get(metadata_url(mcp_url, issuer)?)
        .send()
        .await
        .context("fetching OAuth authorization-server metadata")?
        .error_for_status()
        .context("authorization-server metadata request failed")?
        .json()
        .await
        .context("parsing authorization-server metadata")?;

    // PKCE S256 challenge.
    let code_verifier = base64url(&random_bytes(32));
    let code_challenge = base64url(&sha256(code_verifier.as_bytes()));
    let state = base64url(&random_bytes(16));

    // Loopback listener that receives the redirect.
    let bind_addr = match redirect_port {
        Some(port) => format!("127.0.0.1:{port}"),
        None => "127.0.0.1:0".to_string(),
    };
    let mut listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .context("binding loopback redirect listener")?;
    let actual_port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{actual_port}/callback");

    let mut authz = reqwest::Url::parse(&meta.authorization_endpoint).with_context(|| {
        format!(
            "bad authorization_endpoint {:?}",
            meta.authorization_endpoint
        )
    })?;
    {
        let mut q = authz.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("scope", &scopes.join(" "));
        q.append_pair("state", &state);
        q.append_pair("code_challenge", &code_challenge);
        q.append_pair("code_challenge_method", "S256");
        if let Some(aud) = audience {
            q.append_pair("audience", aud);
        }
    }

    eprintln!("[comrade] OIDC: open this URL in a browser to authorise the MCP server:\n{authz}");
    launch(authz.as_str());

    let code = capture_redirect_code(&mut listener, &state).await?;

    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "authorization_code".into()),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id.clone()),
        ("code_verifier", code_verifier),
    ];
    if let Some(aud) = audience {
        form.push(("audience", aud.clone()));
    }

    let token: TokenResponse = client
        .post(&meta.token_endpoint)
        .form(&form)
        .send()
        .await
        .context("token request failed")?
        .error_for_status()
        .context("token endpoint rejected the exchange")?
        .json()
        .await
        .context("parsing token response")?;

    if token.access_token.is_empty() {
        return Err(anyhow!("token response carried no access_token"));
    }
    Ok(OidcToken {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
    })
}

/// Default OIDC flow: discovery → PKCE → browser → loopback code → token.
async fn oidc_access_token(mcp_url: &str, cfg: &McpAuth) -> Result<OidcToken> {
    oidc_access_token_with(mcp_url, cfg, open_browser).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn api_key_defaults_to_bearer_auth_header_async() {
        let auth = McpAuth::ApiKey {
            key: "sekrit".into(),
            header: None,
        };
        assert_eq!(
            resolve_auth_header(Some(&auth), "https://x.example/mcp")
                .await
                .unwrap()
                .unwrap(),
            ("Authorization".to_string(), "Bearer sekrit".to_string())
        );
    }

    #[tokio::test]
    async fn api_key_with_custom_header_passes_key_verbatim() {
        let auth = McpAuth::ApiKey {
            key: "k123".into(),
            header: Some("x-api-key".into()),
        };
        assert_eq!(
            resolve_auth_header(Some(&auth), "https://x.example/mcp")
                .await
                .unwrap()
                .unwrap(),
            ("x-api-key".to_string(), "k123".to_string())
        );
    }

    #[tokio::test]
    async fn api_key_env_reference_is_expanded() {
        let auth = McpAuth::ApiKey {
            key: "$MISSING_KEY".into(),
            header: None,
        };
        // Unresolvable env var keeps its literal text (config contract).
        let v = resolve_auth_header(Some(&auth), "https://x.example/mcp")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.1, "Bearer $MISSING_KEY");
    }

    /// End-to-end OIDC flow against a mock authorization server: RFC 8414
    /// metadata, an authorize endpoint that 302s to the client's loopback
    /// redirect URI with a code, and a token endpoint that mints tokens.
    async fn spawn_mock_as() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    continue;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 16 * 1024];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let head = req.split("\r\n\r\n").next().unwrap_or("");
                    let mut lines = head.lines();
                    let request_line = lines.next().unwrap_or("");
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("/");
                    let (path, query) = match path.split_once('?') {
                        Some((p, q)) => (p, q),
                        None => (path, ""),
                    };

                    let origin = format!("http://127.0.0.1:{port}");
                    let body;
                    let mut status = "200 OK";
                    let extra_headers = String::new();
                    match (method, path) {
                        ("GET", "/.well-known/oauth-authorization-server") => {
                            body = format!(
                                r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token"}}"#
                            );
                        }
                        ("GET", "/authorize") => {
                            let mut params = std::collections::HashMap::new();
                            for kv in query.split('&') {
                                if let Some((k, v)) = kv.split_once('=') {
                                    let decoded: String =
                                        url::form_urlencoded::parse(format!("k={v}").as_bytes())
                                            .map(|(_, val)| val.into_owned())
                                            .collect();
                                    params.insert(k.to_string(), decoded);
                                }
                            }
                            let redirect_uri =
                                params.get("redirect_uri").cloned().unwrap_or_default();
                            let state = params.get("state").cloned().unwrap_or_default();
                            // Immediate "consent": bounce straight to the loopback.
                            let _ = socket
                                .write_all(
                                    format!(
                                        "HTTP/1.1 302 Found\r\nLocation: {redirect_uri}?code=auth-code-1&state={state}\r\nContent-Length: 0\r\n\r\n"
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            let _ = socket.flush().await;
                            return;
                        }
                        ("POST", "/token") => {
                            let payload = req.split("\r\n\r\n").nth(1).unwrap_or("");
                            if payload.contains("code_verifier=")
                                && payload.contains("code=auth-code-1")
                            {
                                body = format!(
                                    r#"{{"access_token":"access-123","token_type":"Bearer","expires_in":3600,"refresh_token":"refresh-456"}}"#
                                );
                            } else {
                                status = "400 Bad Request";
                                body = r#"{"error":"invalid_grant"}"#.to_string();
                            }
                        }
                        _ => {
                            status = "404 Not Found";
                            body = "{}".to_string();
                        }
                    }
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}{extra_headers}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        (port, handle)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oidc_full_flow_against_mock_authorization_server() -> Result<()> {
        let (port, _as) = spawn_mock_as().await;
        let issuer = format!("http://127.0.0.1:{port}");
        let cfg = McpAuth::Oidc {
            client_id: "comrade-test".into(),
            issuer: Some(issuer.clone()),
            scopes: vec!["openid".into(), "profile".into()],
            redirect_port: None,
            audience: None,
        };

        // In production the OS browser opens the authz URL; here we fetch it
        // (following the 302 back to our loopback redirect) on a background
        // task, the way a browser would.
        let launch = |authz_url: &str| {
            let url = authz_url.to_string();
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                let _ = client.get(url).send().await;
            });
        };

        let fut = oidc_access_token_with(&issuer, &cfg, launch);
        let token = tokio::time::timeout(std::time::Duration::from_secs(20), fut)
            .await
            .context("oidc flow timed out")??;
        assert_eq!(token.access_token, "access-123");
        Ok(())
    }
}
