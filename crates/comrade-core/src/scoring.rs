//! Feature scoring: before the lead gathers requirements it scores the feature
//! with Jev, so the scores drive how much to gather and when to raise a concern.
//!
//! `score_feature` asks Jev for customer value, technical challenge and UX
//! challenge (each a 0-3 `score` question) plus the risk of a negative
//! architecture or product impact (two `noul` probabilities). The result is DATA
//! (the numbers + level labels); the `## Requirements` prompt turns them into
//! decisions: a high architecture/product risk is raised with the user at once,
//! a low customer value questions whether to build it, a high technical/UX
//! challenge means gather more information before planning.

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::jev::{Jev, noul, score, top_level};

/// Name of the tool advertised to the tech lead.
pub const TOOL_NAME: &str = "score_feature";

const VALUE_LEVELS: [&str; 4] = ["no value", "marginal value", "clear value", "high value"];
const TECH_LEVELS: [&str; 4] = ["trivial", "easy", "moderate", "hard"];
const UX_LEVELS: [&str; 4] = ["invisible", "minor", "moderate", "major"];

pub struct ScoreFeatureTool {
    jev: Jev,
}

impl ScoreFeatureTool {
    pub fn new(jev: Jev) -> Self {
        Self { jev }
    }
}

static SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: TOOL_NAME.into(),
    description: "Score a feature BEFORE planning (Jev, 0-3 each): customer value, technical challenge and UX challenge, plus the risk of a NEGATIVE architecture or product impact (0-1). Use it to decide how much to gather - raise a high architecture/product risk with the user at once, question a low customer value, and gather more information when the technical/UX challenge is high. Returns scores only."
        .into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "feature": { "type": "string", "description": "The feature/bug request, verbatim." }
        },
        "required": ["feature"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ScoreFeatureTool {
    fn spec(&self) -> &ToolSpec {
        &SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            feature: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let feature = args.feature.trim().to_string();
        if feature.is_empty() {
            anyhow::bail!("`feature` must not be empty");
        }

        let score_q = |instructions: &str, levels: &[&str; 4]| json!({ "type": "score", "instructions": instructions, "criteria": levels });
        let risk_q = |instructions: &str| {
            json!({
                "type": "noul",
                "instructions": instructions,
                "criteria": { "true": "A real negative impact", "false": "No real negative impact" }
            })
        };
        let questions = json!({
            "customer_value": score_q(
                "How much CUSTOMER value does this feature deliver? 0 = no value, 3 = high/critical \
                 value.",
                &VALUE_LEVELS,
            ),
            "technical_challenge": score_q(
                "How HARD is it to build this correctly and to maintain - considering the codebase \
                 and its constraints? 0 = trivial, 3 = hard.",
                &TECH_LEVELS,
            ),
            "ux_challenge": score_q(
                "How much user-facing / UX work does this need? 0 = invisible to users, 3 = major UX \
                 work.",
                &UX_LEVELS,
            ),
            "architecture_risk": risk_q(
                "Would implementing this NEGATIVELY impact the ARCHITECTURE - violate a pattern or \
                 an ADR, add coupling, or fight the design?",
            ),
            "product_risk": risk_q(
                "Would it NEGATIVELY impact the PRODUCT - confuse users, break an existing \
                 workflow, or add scope the product does not want?",
            ),
        });
        let state = json!({ "feature": feature });
        let answers = self.jev.evaluate(state, questions).await?;

        let value = score(&answers, "customer_value").unwrap_or(0.0);
        let tech = score(&answers, "technical_challenge").unwrap_or(0.0);
        let ux = score(&answers, "ux_challenge").unwrap_or(0.0);
        let arch = noul(&answers, "architecture_risk").unwrap_or(0.0);
        let product = noul(&answers, "product_risk").unwrap_or(0.0);

        let label = |id: &str, v: f64, levels: &[&str; 4]| -> String {
            top_level(&answers, id)
                .map(|(_, l)| l)
                .unwrap_or_else(|| levels[nearest_level(v)].to_string())
        };
        let scores = [
            (
                "customer value",
                value,
                label("customer_value", value, &VALUE_LEVELS),
            ),
            (
                "technical challenge",
                tech,
                label("technical_challenge", tech, &TECH_LEVELS),
            ),
            ("ux challenge", ux, label("ux_challenge", ux, &UX_LEVELS)),
        ];
        let risks = [("architecture", arch), ("product", product)];
        Ok(render_scores(&scores, &risks))
    }
}

/// The nearest 0-3 level index for a weighted score (used when the response has
/// no probability legend).
fn nearest_level(v: f64) -> usize {
    v.round().clamp(0.0, 3.0) as usize
}

/// The DATA the lead reads. No imperative - the `## Requirements` prompt turns
/// the numbers into decisions. Pure, so it is unit-tested without a network.
fn render_scores(scores: &[(&str, f64, String)], risks: &[(&str, f64)]) -> String {
    let mut s = "Feature score (Jev, 0-3):\n".to_string();
    for (name, v, label) in scores {
        s.push_str(&format!("  {name}: {v:.1}/3 ({label})\n"));
    }
    s.push_str("Negative-impact risk (Jev, 0-1):\n");
    for (name, p) in risks {
        s.push_str(&format!("  {name}: {p:.2}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_render_as_data() {
        let scores = [
            ("customer value", 2.4, "clear value".to_string()),
            ("technical challenge", 3.1, "hard".to_string()),
            ("ux challenge", 1.2, "minor".to_string()),
        ];
        let risks = [("architecture", 0.75), ("product", 0.10)];
        let out = render_scores(&scores, &risks);
        assert!(out.contains("customer value: 2.4/3 (clear value)"), "{out}");
        assert!(out.contains("technical challenge: 3.1/3 (hard)"), "{out}");
        assert!(out.contains("architecture: 0.75"), "{out}");
        assert!(out.contains("product: 0.10"), "{out}");
        // Data-only: no directive.
        assert!(!out.contains("raise"), "{out}");
    }

    #[test]
    fn nearest_level_maps_the_weighted_score() {
        assert_eq!(VALUE_LEVELS[nearest_level(0.1)], "no value");
        assert_eq!(TECH_LEVELS[nearest_level(2.6)], "hard");
        assert_eq!(UX_LEVELS[nearest_level(1.2)], "minor");
        assert_eq!(VALUE_LEVELS[nearest_level(3.0)], "high value");
    }
}
