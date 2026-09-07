//! Web search via DuckDuckGo's HTML endpoint.
//!
//! Results are reduced to what an agent needs: the URL, the page title and a
//! short description. No HTML is ever returned.

use anyhow::{Context as _, Result};
use scraper::{Html, Selector};

/// One reduced search result.
#[derive(Debug, Clone)]
pub struct WebResult {
    pub url: String,
    pub title: String,
    pub description: String,
}

const ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// Perform a DuckDuckGo HTML search and return the first `max_results` results
/// reduced to (url, title, description).
pub async fn search(query: &str, max_results: usize) -> Result<Vec<WebResult>> {
    if query.trim().is_empty() {
        anyhow::bail!("query must not be empty");
    }
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build http client")?;

    let resp = client
        .get(ENDPOINT)
        .query(&[("q", query)])
        .send()
        .await
        .context("duckduckgo request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("duckduckgo returned {status}");
    }
    let html = resp.text().await.context("failed to read response body")?;
    Ok(parse_results(&html)
        .into_iter()
        .take(max_results.max(1))
        .collect())
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
    description: "Search the web via DuckDuckGo. Returns only each result's URL, title and a short description - no HTML, no page content. Use to find docs, APIs, or answers about topics outside the repository.".into(),
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
