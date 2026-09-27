//! Challenging the approach: before the lead commits to a feature it ranks the
//! candidate approaches with Jev, so it can reason with the user and let the
//! user decide.
//!
//! `rank_alternatives` takes the request and a few concrete alternatives (the
//! user's proposal plus the ones the lead found) and asks Jev ONE `choice`
//! question - "which single alternative is best?" - whose full probability
//! distribution is the ranking. The result is DATA (each alternative with its
//! probability, best first); the `## Challenge the approach` prompt tells the
//! lead to present the top 3 to the user with its reasoning and then treat the
//! user's choice as final.

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::jev::{Jev, probabilities};

/// Name of the tool advertised to the tech lead.
pub const TOOL_NAME: &str = "rank_alternatives";

pub struct RankAlternativesTool {
    jev: Jev,
}

impl RankAlternativesTool {
    pub fn new(jev: Jev) -> Self {
        Self { jev }
    }
}

static SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: TOOL_NAME.into(),
    description: "Challenge the approach before committing to it: pass the request and 2-4 concrete ALTERNATIVES (the user's proposal and the ones you found, e.g. via web_search); Jev ranks them by probability that each is the best choice. Returns the ranking as data; present the top 3 to the user with your reasoning (ask_form) and treat the user's choice as final."
        .into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "request": { "type": "string", "description": "The feature/bug request, verbatim." },
            "alternatives": {
                "type": "array",
                "items": { "type": "string" },
                "description": "2-4 concrete alternatives to rank, each one line (what it is + its main trade-off), including the user's proposal."
            }
        },
        "required": ["request", "alternatives"],
        "additionalProperties": false
    }),
}
});

#[derive(Deserialize)]
#[serde(untagged)]
enum AltInput {
    One(String),
    Many(Vec<String>),
}

impl AltInput {
    fn into_vec(self) -> Vec<String> {
        match self {
            AltInput::One(s) => vec![s],
            AltInput::Many(v) => v,
        }
    }
}

#[async_trait]
impl Tool for RankAlternativesTool {
    fn spec(&self) -> &ToolSpec {
        &SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            request: String,
            alternatives: AltInput,
        }
        let args: Args = serde_json::from_value(args)?;
        let request = args.request.trim().to_string();
        if request.is_empty() {
            anyhow::bail!("`request` must not be empty");
        }
        let alternatives: Vec<String> = args
            .alternatives
            .into_vec()
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        if alternatives.len() < 2 {
            anyhow::bail!("give at least two alternatives to rank");
        }

        let mut criteria = serde_json::Map::new();
        for (i, alt) in alternatives.iter().enumerate() {
            criteria.insert(format!("a{i}"), Value::String(alt.clone()));
        }
        let questions = json!({
            "best": {
                "type": "choice",
                "instructions": format!(
                    "Which SINGLE alternative is best to implement this request: \"{request}\"? \
                     Judge soundness (will it actually work and fit this codebase and its \
                     constraints), then simplicity and the cost to build and maintain. Prefer the \
                     correct, least-risky option over the clever one."
                ),
                "criteria": Value::Object(criteria),
            }
        });
        let state = json!({
            "user_request": request,
            "alternatives": alternatives
                .iter()
                .enumerate()
                .map(|(i, a)| json!({ "id": format!("a{i}"), "text": a }))
                .collect::<Vec<_>>()
        });
        let answers = self.jev.evaluate(state, questions).await?;

        let probs = probabilities(&answers, "best");
        let choice = answers
            .get("best")
            .and_then(|a| a.get("choice"))
            .and_then(|c| c.as_str())
            .map(str::to_string);
        let mut ranked: Vec<(f64, String)> = alternatives
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let key = format!("a{i}");
                let p = probs.get(&key).copied().unwrap_or(0.0);
                (p, a.clone())
            })
            .collect();
        // Rank by probability, ties keeping input order; if no distribution came
        // back, put Jev's single picked alternative first.
        if probs.is_empty()
            && let Some(picked) = &choice
            && let Some(idx) = (0..alternatives.len())
                .find(|&i| format!("a{i}") == *picked)
                .and_then(|i| ranked.iter().position(|(_, a)| *a == alternatives[i]))
        {
            ranked[idx].0 = 1.0;
        }
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(render_ranking(&ranked))
    }
}

/// The DATA ranking the lead reads: each alternative with its Jev probability,
/// best first. No imperative - the `## Challenge the approach` prompt decides
/// what to do with it. Pure, so it is unit-tested without a network.
fn render_ranking(ranked: &[(f64, String)]) -> String {
    let mut s = "Jev alternative ranking (probability of being the best choice):\n".to_string();
    for (i, (p, alt)) in ranked.iter().enumerate() {
        s.push_str(&format!("  {}. [{p:.2}] {alt}\n", i + 1));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking_lists_best_first_with_probabilities() {
        let ranked = vec![
            (0.52, "Queue edits offline and sync".to_string()),
            (0.28, "Require connectivity".to_string()),
            (0.14, "Use CRDTs".to_string()),
        ];
        let out = render_ranking(&ranked);
        assert!(
            out.contains("1. [0.52] Queue edits offline and sync"),
            "{out}"
        );
        assert!(out.contains("3. [0.14] Use CRDTs"), "{out}");
        // Data-only: no directive.
        assert!(!out.contains("present the top"), "{out}");
    }

    #[test]
    fn alternatives_may_arrive_as_a_string_or_an_array() {
        assert_eq!(AltInput::One("a".into()).into_vec(), vec!["a".to_string()]);
        assert_eq!(
            AltInput::Many(vec!["a".into(), "b".into()]).into_vec(),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
