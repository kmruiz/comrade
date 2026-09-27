//! A thin client for Jev (TypeSafe's SystemOne evaluation endpoint).
//!
//! One HTTP call evaluates a `state` against a map of typed questions and
//! returns one structured answer per question. This is the shared transport for
//! the guardrail ([`crate::guardrails`]) and the test-coverage check
//! ([`crate::tdd`]); each builds its own `state` and questions and reads the
//! answers it asked for.
//!
//! Endpoint: `POST https://api.typesafe.ai/v1/systemone`, `Authorization: Bearer
//! <key>`, body `{ state, model, questions }`, response `{ answers, ... }`.
//! Configured by the `[guardrails]` table (the Jev connection is shared).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::config::GuardrailsCfg;

/// TypeSafe's evaluation endpoint (the "Jev" model API).
pub const DEFAULT_JEV_URL: &str = "https://api.typesafe.ai/v1/systemone";
/// TypeSafe's flagship model alias.
pub const DEFAULT_JEV_MODEL: &str = "jev-latest";

/// A configured Jev client: the shared connection used by the guardrail and the
/// TDD test-coverage check.
#[derive(Clone)]
pub struct Jev {
    client: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
    timeout: Duration,
}

impl Jev {
    pub fn new(
        api_key: impl Into<String>,
        url: Option<String>,
        model: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.unwrap_or_else(|| DEFAULT_JEV_URL.to_string()),
            api_key: api_key.into(),
            model: model.unwrap_or_else(|| DEFAULT_JEV_MODEL.to_string()),
            timeout,
        }
    }

    /// The client from the `[guardrails]` config, or `None` when Jev is disabled
    /// or has no key.
    pub fn from_cfg(cfg: &GuardrailsCfg) -> Option<Jev> {
        if !cfg.is_active() {
            return None;
        }
        let key = cfg.api_key()?.to_string();
        Some(Jev::new(
            key,
            cfg.jev_url.clone(),
            cfg.jev_model.clone(),
            Duration::from_secs(cfg.jev_timeout_secs),
        ))
    }

    /// The model alias this client evaluates with.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Evaluate `state` against `questions` and return the `answers` map, keyed by
    /// the question ids the caller chose.
    pub async fn evaluate(&self, state: Value, questions: Value) -> Result<HashMap<String, Value>> {
        let body = json!({
            "state": state,
            "model": self.model,
            "questions": questions,
        });
        let send = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send();
        let resp = match tokio::time::timeout(self.timeout, send).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => return Err(anyhow::Error::from(e).context("jev request failed")),
            Err(_) => bail!("jev request timed out after {:?}", self.timeout),
        };
        let status = resp.status();
        let text = resp.text().await.context("cannot read the jev response")?;
        if !status.is_success() {
            bail!(
                "jev returned HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let parsed: JevResponse =
            serde_json::from_str(&text).context("jev returned invalid JSON")?;
        Ok(parsed.answers)
    }
}

/// The response body of the evaluation endpoint: one answer per question id.
#[derive(serde::Deserialize)]
struct JevResponse {
    #[serde(default)]
    answers: HashMap<String, Value>,
}

/// The `noul` probability (0..1) of the answer for `id`, when present.
pub fn noul(answers: &HashMap<String, Value>, id: &str) -> Option<f64> {
    answers.get(id)?.get("noul")?.as_f64()
}

/// The `score` of the answer for `id`, when present.
pub fn score(answers: &HashMap<String, Value>, id: &str) -> Option<f64> {
    answers.get(id)?.get("score")?.as_f64()
}

/// The full `probabilities` map of a `choice` (or `score`) answer, when present.
pub fn probabilities(answers: &HashMap<String, Value>, id: &str) -> HashMap<String, f64> {
    answers
        .get(id)
        .and_then(|a| a.get("probabilities"))
        .and_then(|p| p.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_f64().map(|p| (k.clone(), p)))
                .collect()
        })
        .unwrap_or_default()
}

/// The highest-probability level of a `score` answer: its zero-based index and
/// the level's description from the response `legend` (falling back to the level
/// index when no legend is present).
pub fn top_level(answers: &HashMap<String, Value>, id: &str) -> Option<(usize, String)> {
    let answer = answers.get(id)?;
    let probs = answer.get("probabilities")?.as_object()?;
    let legend = answer.get("legend").and_then(|l| l.as_object());
    let (key, _) = probs
        .iter()
        .filter_map(|(k, v)| v.as_f64().map(|p| (k, p)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
    let index: usize = key.parse().ok()?;
    let label = legend
        .and_then(|l| l.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| index.to_string());
    Some((index, label))
}
