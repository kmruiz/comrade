//! Web search with reduced results.
//!
//! Primary backend is Bing's HTML search (returns results from datacenter IPs
//! that DuckDuckGo's HTML endpoint flags with a 202 anomaly page); DuckDuckGo
//! HTML is used as a fallback. Results are reduced to what an agent needs: the
//! URL, the page title and a short description. No HTML is ever returned.

use anyhow::{Context as _, Result};
use scraper::{Html, Selector};

/// One reduced search result.
#[derive(Debug, Clone)]
pub struct WebResult {
    pub url: String,
    pub title: String,
    pub description: String,
}

const DDG_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const BING_ENDPOINT: &str = "https://www.bing.com/search";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

fn client() -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in [
        (
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
        ("accept-language", "en-US,en;q=0.9"),
        ("upgrade-insecure-requests", "1"),
        (
            "sec-ch-ua",
            "\"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\"",
        ),
        ("sec-ch-ua-mobile", "?0"),
        ("sec-ch-ua-platform", "\"Linux\""),
    ] {
        if let (Ok(k), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            headers.insert(k, v);
        }
    }
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build http client")
}

/// Perform a web search and return the first `max_results` results reduced to
/// (url, title, description). Tries Bing first, then DuckDuckGo HTML.
pub async fn search(query: &str, max_results: usize) -> Result<Vec<WebResult>> {
    if query.trim().is_empty() {
        anyhow::bail!("query must not be empty");
    }
    let client = client()?;

    // 1) Bing (reliable from datacenter IPs).
    let bing = bing_search(&client, query).await;
    if let Ok(mut results) = bing
        && !results.is_empty()
    {
        results.truncate(max_results.max(1));
        return Ok(results);
    }

    // 2) DuckDuckGo HTML fallback.
    let ddg = fetch_engine(&client, DDG_ENDPOINT, "q", query, parse_results).await;
    match ddg {
        Ok(results) => Ok(results.into_iter().take(max_results.max(1)).collect()),
        Err(e) => Err(e),
    }
}

/// Bing search with an explicit English locale so datacenter endpoints do not
/// serve region-specific (e.g. German) results for arbitrary queries.
async fn bing_search(client: &reqwest::Client, query: &str) -> Result<Vec<WebResult>> {
    let resp = client
        .get(BING_ENDPOINT)
        .query(&[
            ("q", query),
            ("setlang", "en"),
            ("cc", "us"),
            ("mkt", "en-US"),
        ])
        .send()
        .await
        .with_context(|| format!("search request to {BING_ENDPOINT} failed"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("search returned {status}");
    }
    let html = resp.text().await.context("failed to read response body")?;
    Ok(parse_bing(&html))
}

async fn fetch_engine<F>(
    client: &reqwest::Client,
    url: &str,
    param: &str,
    query: &str,
    parse: F,
) -> Result<Vec<WebResult>>
where
    F: Fn(&str) -> Vec<WebResult>,
{
    let resp = client
        .get(url)
        .query(&[(param, query)])
        .send()
        .await
        .with_context(|| format!("search request to {url} failed"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("search returned {status}");
    }
    let html = resp.text().await.context("failed to read response body")?;
    Ok(parse(&html))
}

/// Parse Bing results: each `li.b_algo` contributes a title link (h2 a) and a
/// snippet from `.b_caption p`.
pub fn parse_bing(html: &str) -> Vec<WebResult> {
    let doc = Html::parse_document(html);
    let algo = Selector::parse("li.b_algo").expect("static selector");
    let link_sel = Selector::parse("h2 a").expect("static selector");
    let snippet_sel = Selector::parse(".b_caption p").expect("static selector");

    let mut results = Vec::new();
    for el in doc.select(&algo) {
        let Some(link) = el.select(&link_sel).next() else {
            continue;
        };
        let title = text_of(link);
        let url = resolve_bing_url(link.value().attr("href").unwrap_or(""));
        if url.is_empty() || !is_external(&url) {
            continue;
        }
        let description = el
            .select(&snippet_sel)
            .next()
            .map(text_of)
            .unwrap_or_default();
        results.push(WebResult {
            url,
            title,
            description,
        });
    }
    results
}

/// Keep only real external results; drop Bing-internal pages (search, related,
/// redirect stubs) that can slip into organic HTML.
fn is_external(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some(host) if host.eq_ignore_ascii_case("bing.com") => false,
        Some(host) if host.ends_with(".bing.com") => false,
        Some(_) => true,
        None => false,
    }
}

/// Bing organic results point at `bing.com/ck/a?...&u=<base64url>`; decode the
/// `u` param to the real destination.
fn resolve_bing_url(href: &str) -> String {
    if (href.contains("bing.com/ck/a") || href.starts_with("https://www.bing.com/ck/a"))
        && let Ok(parsed) = url::Url::parse(href.trim())
    {
        for (key, value) in parsed.query_pairs() {
            if key == "u"
                && let Some(decoded) = decode_base64_url(&value)
            {
                return decoded;
            }
        }
    }
    href.trim().to_string()
}

fn decode_base64_url(encoded: &str) -> Option<String> {
    use base64::Engine;
    use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
    // Bing prefixes the payload (e.g. "a1"); try slicing it off progressively.
    for cut in 0..=4 {
        let slice = &encoded[cut..];
        let decoded = URL_SAFE_NO_PAD
            .decode(slice)
            .or_else(|_| URL_SAFE.decode(slice));
        if let Ok(decoded) = decoded
            && let Ok(s) = String::from_utf8(decoded)
            && s.starts_with("http")
        {
            return Some(s);
        }
    }
    None
}

/// Parse DDG HTML results: `.result__a` (link + title) and `.result__snippet`
/// (description). Returns only url/title/description.
pub fn parse_results(html: &str) -> Vec<WebResult> {
    let doc = Html::parse_document(html);
    let link_sel = Selector::parse("a.result__a").expect("static selector");
    let snippet_sel = Selector::parse("a.result__snippet").expect("static selector");

    let mut results = Vec::new();
    for (i, el) in doc.select(&link_sel).enumerate() {
        let title = text_of(el);
        let href = el.value().attr("href").unwrap_or("").to_string();
        let url = real_url(&href);
        if title.is_empty() && url.is_empty() {
            continue;
        }
        let description = doc
            .select(&snippet_sel)
            .nth(i)
            .map(text_of)
            .unwrap_or_default();
        results.push(WebResult {
            url,
            title,
            description,
        });
    }
    results
}

/// Join an element's text nodes, collapsing whitespace.
fn text_of(el: scraper::ElementRef<'_>) -> String {
    el.text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve a DDG redirect link (`//duckduckgo.com/l/?uddg=<encoded>`) to the
/// real target URL, falling back to the raw href.
fn real_url(href: &str) -> String {
    let href = href.trim();
    if href.is_empty() {
        return String::new();
    }
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(parsed) = url::Url::parse(&absolute) {
        for (key, value) in parsed.query_pairs() {
            if key == "uddg" {
                return value.into_owned();
            }
        }
        parsed.to_string()
    } else {
        href.to_string()
    }
}

/// Render results as plain text (url, then title, then description).
pub fn render(results: &[WebResult]) -> String {
    if results.is_empty() {
        return "No results.".to_string();
    }
    let mut out = format!("{} result(s):\n", results.len());
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, r.url));
        if !r.title.is_empty() {
            out.push_str(&format!("   title: {}\n", r.title));
        }
        if !r.description.is_empty() {
            out.push_str(&format!("   description: {}\n", r.description));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reduced_results_only() {
        let html = r#"
<html><body>
<div class="result results_links">
  <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=x">Example <b>Page</b></a>
  <a class="result__snippet" href="//duckduckgo.com/l/?uddg=...">A <b>short</b> description here.</a>
</div>
<div class="result">
  <a class="result__a" href="https://second.example/">Second result</a>
  <a class="result__snippet">Second description.</a>
</div>
</body></html>
"#;
        let results = parse_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://example.com/page");
        assert_eq!(results[0].title, "Example Page");
        assert_eq!(results[0].description, "A short description here.");
        assert_eq!(results[1].url, "https://second.example/");
        // never contains markup tags
        let flat = format!("{results:?}");
        assert!(!flat.contains("<b>"));
        assert!(!flat.contains("</a>"));
    }

    #[test]
    fn resolves_redirects() {
        assert_eq!(
            real_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fx&amp;rut=1"),
            "https://example.com/x"
        );
        assert_eq!(real_url("https://plain.example/"), "https://plain.example/");
    }
}

// ---------------------------------------------------------------------------
// web_search tool
// ---------------------------------------------------------------------------

use std::sync::LazyLock;

use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![Box::new(WebSearch)]
}

struct WebSearch;

static WEB_SEARCH_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "web_search".into(),
    description: "Search the web (Bing-backed, DuckDuckGo fallback). Returns only each result's URL, title and short description - no page content. Use to find docs, APIs or answers outside the repo.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query." },
            "max_results": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5, "description": "Max results to return." }
        },
        "required": ["query"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> &ToolSpec {
        &WEB_SEARCH_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            #[serde(default = "default_max")]
            max_results: usize,
        }
        fn default_max() -> usize {
            5
        }
        let _ = ctx;
        let args: Args = serde_json::from_value(args)?;
        let results = search(&args.query, args.max_results).await?;
        Ok(render(&results))
    }
}

#[cfg(test)]
mod bing_tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn decodes_bing_redirect_url() {
        let target = "https://rust-lang.org/";
        let enc = format!("a1{}", URL_SAFE_NO_PAD.encode(target));
        let href = format!("https://www.bing.com/ck/a?x=1&u={enc}&ntb=1");
        assert_eq!(resolve_bing_url(&href), target);
    }
}
