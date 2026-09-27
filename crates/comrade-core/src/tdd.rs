//! Test-first (TDD) validation.
//!
//! The tech lead writes the tests for a feature BEFORE the implementation and
//! asks Jev (TypeSafe) how well they cover it. A good score is the green light
//! to delegate the implementation; a poor one means the tests must be
//! strengthened first. This is advisory: the tool returns a verdict, it does
//! not gate the delegate.

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::jev::{Jev, score, top_level};

/// Name of the tool advertised to the tech lead.
pub const TOOL_NAME: &str = "validate_tests";

/// Coverage levels, lowest to highest, asked as one `score` question. The top
/// two ("good", "comprehensive") are enough to proceed.
const LEVELS: [&str; 5] = ["none", "sparse", "partial", "good", "comprehensive"];
/// The minimum weighted score accepted: the top two of five levels.
const ACCEPT_AT: f64 = 3.0;

/// The `validate_tests` tool: score how well the lead's tests cover a feature.
pub struct ValidateTestsTool {
    jev: Jev,
}

impl ValidateTestsTool {
    pub fn new(jev: Jev) -> Self {
        Self { jev }
    }
}

static SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
    name: TOOL_NAME.into(),
    description: "Score how well the tests you wrote COVER a feature (Jev/TypeSafe), before any \
        implementation. For TDD: write the tests first, pass `feature` (what it must do) and \
        `tests` (the test code), and act on the verdict - ACCEPTED means delegate the \
        implementation (the tests must fail first), NOT ENOUGH means strengthen the tests and \
        call again. Advisory; it does not run or gate anything."
        .into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "feature": {
                "type": "string",
                "description": "What the feature must do, in one or two sentences: the behaviour the tests must pin down."
            },
            "tests": {
                "type": "array",
                "items": { "type": "string" },
                "description": "The test code you wrote for the feature (one string per test or test file)."
            }
        },
        "required": ["feature", "tests"],
        "additionalProperties": false
    }),
});

/// `tests` may arrive as one string (a small model's slip) or an array; accept
/// both rather than failing the call.
#[derive(Deserialize)]
#[serde(untagged)]
enum TestInput {
    One(String),
    Many(Vec<String>),
}

impl TestInput {
    fn into_vec(self) -> Vec<String> {
        match self {
            TestInput::One(s) => vec![s],
            TestInput::Many(v) => v,
        }
    }
}

#[async_trait]
impl Tool for ValidateTestsTool {
    fn spec(&self) -> &ToolSpec {
        &SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            feature: String,
            tests: TestInput,
        }
        let args: Args = serde_json::from_value(args)?;
        let feature = args.feature.trim();
        if feature.is_empty() {
            anyhow::bail!("`feature` must not be empty");
        }
        let tests: Vec<String> = args
            .tests
            .into_vec()
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if tests.is_empty() {
            anyhow::bail!("`tests` must not be empty: pass the test code you wrote");
        }

        let questions = json!({
            "coverage": {
                "type": "score",
                "instructions": "How well do these tests cover the feature's required behaviour? \
                    Judge ONLY behavioural coverage of the feature as described, not style or \
                    naming: the happy path, the edge cases and the failure modes a correct \
                    implementation must satisfy. Reward genuine assertions and penalise vacuous \
                    ones. A comprehensive suite follows the test pyramid - more unit tests than \
                    integration tests, and more integration tests than functional (end-to-end) \
                    tests - so judge that balance too.",
                "criteria": LEVELS,
            }
        });
        let state = json!({ "feature": feature, "tests": tests });
        let answers = self.jev.evaluate(state, questions).await?;
        let score =
            score(&answers, "coverage").context("the test-coverage check returned no score")?;
        // Prefer the response's most-likely level; fall back to the weighted score.
        let label = top_level(&answers, "coverage")
            .map(|(_, label)| label)
            .unwrap_or_else(|| nearest_level(score).to_string());
        Ok(verdict(score, &label))
    }
}

/// The nearest level label for a weighted score (used when the response carries
/// no probability legend).
fn nearest_level(score: f64) -> &'static str {
    let idx = score.round().clamp(0.0, (LEVELS.len() - 1) as f64) as usize;
    LEVELS[idx]
}

/// The verdict line the lead reads. It is DATA, not an instruction: the action
/// ("delegate the implementation", "strengthen the tests") comes from the
/// `## Test-first (TDD)` prompt section, so this result never reads as a
/// directive injected through tool output (see ## Trust boundaries). Pure, so it
/// is unit-tested without a network.
fn verdict(score: f64, label: &str) -> String {
    let verdict = if score >= ACCEPT_AT {
        "ACCEPTED"
    } else {
        "NOT ENOUGH"
    };
    format!("Test coverage {score:.1}/4 ({label}): {verdict} (threshold {ACCEPT_AT:.1}/4).")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_good_score_reads_as_an_accepted_verdict() {
        let v = verdict(3.4, "good");
        assert!(v.contains("ACCEPTED"), "{v}");
        assert!(v.contains("3.4/4"), "{v}");
        assert!(v.contains("good"), "{v}");
        // The result is data, not an instruction: no imperative verbs.
        assert!(!v.contains("delegate"), "{v}");
        assert!(!v.contains("refactor"), "{v}");
    }

    #[test]
    fn the_top_level_is_accepted_and_the_boundary_holds() {
        assert!(verdict(4.0, "comprehensive").contains("ACCEPTED"));
        assert!(verdict(3.0, "good").contains("ACCEPTED"));
        // Just below "good" is not enough.
        assert!(verdict(2.9, "partial").contains("NOT ENOUGH"));
        assert!(verdict(0.0, "none").contains("NOT ENOUGH"));
    }

    #[test]
    fn nearest_level_maps_the_weighted_score() {
        assert_eq!(nearest_level(0.1), "none");
        assert_eq!(nearest_level(2.4), "partial");
        assert_eq!(nearest_level(3.4), "good");
        assert_eq!(nearest_level(4.0), "comprehensive");
    }

    #[test]
    fn tests_may_arrive_as_a_string_or_an_array() {
        assert_eq!(TestInput::One("t".into()).into_vec(), vec!["t".to_string()]);
        assert_eq!(
            TestInput::Many(vec!["a".into(), "b".into()]).into_vec(),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
