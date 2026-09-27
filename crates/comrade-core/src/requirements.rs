//! Requirements gathering: before the lead plans a feature or a bug, it drafts
//! clarifying questions (usually with a delegate) and has Jev score each one, so
//! it asks the user only good FEATURE-level questions.
//!
//! `evaluate_questions` takes the user's request verbatim and the candidate
//! questions, and asks Jev one `noul` (yes/no probability) per question with the
//! request as the `state`: "is this a good clarifying question to ask the user?"
//! A technical question is only good when the request implies a big
//! architectural change. The result is DATA (each question with its probability
//! and an accepted/rejected label); the `## Requirements` prompt section tells
//! the lead to ask the accepted ones with `ask_form`, always with a suggested
//! answer.

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::jev::{Jev, noul};

/// Name of the tool advertised to the tech lead.
pub const TOOL_NAME: &str = "evaluate_questions";

/// A question at/above this Jev probability is worth asking the user.
const ACCEPT_AT: f64 = 0.6;

pub struct EvaluateQuestionsTool {
    jev: Jev,
}

impl EvaluateQuestionsTool {
    pub fn new(jev: Jev) -> Self {
        Self { jev }
    }
}

static SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: TOOL_NAME.into(),
    description: "Before planning a feature or bug, filter your candidate CLARIFYING questions through Jev: pass the user's request verbatim and the questions; each is scored (0..1) on whether it is a good question to ask the user from a FEATURE standpoint. Ask the user only the accepted ones (with `ask_form`, always with a suggested answer); technical questions score low unless the request implies a big architectural change. Returns scores only."
        .into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "request": { "type": "string", "description": "The user's feature/bug request, verbatim." },
            "questions": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Candidate clarifying questions to score."
            }
        },
        "required": ["request", "questions"],
        "additionalProperties": false
    }),
}
});

/// `questions` may arrive as one string (a small model's slip) or an array.
#[derive(Deserialize)]
#[serde(untagged)]
enum QuestionInput {
    One(String),
    Many(Vec<String>),
}

impl QuestionInput {
    fn into_vec(self) -> Vec<String> {
        match self {
            QuestionInput::One(s) => vec![s],
            QuestionInput::Many(v) => v,
        }
    }
}

#[async_trait]
impl Tool for EvaluateQuestionsTool {
    fn spec(&self) -> &ToolSpec {
        &SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            request: String,
            questions: QuestionInput,
        }
        let args: Args = serde_json::from_value(args)?;
        let request = args.request.trim().to_string();
        if request.is_empty() {
            anyhow::bail!("`request` must not be empty");
        }
        let questions: Vec<String> = args
            .questions
            .into_vec()
            .into_iter()
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty())
            .collect();
        if questions.is_empty() {
            return Ok("No questions to review.".to_string());
        }

        let mut qmap = serde_json::Map::new();
        for (i, q) in questions.iter().enumerate() {
            qmap.insert(
                format!("q{i}"),
                json!({
                    "type": "noul",
                    "instructions": format!(
                        "The user asked for this feature or bug: \"{request}\". Candidate clarifying \
                         question to ask the user: \"{q}\". Is this a GOOD question to ask the user? \
                         It must be answerable from a FEATURE standpoint (what the user wants and \
                         how it should behave). A technical question is good ONLY if the request \
                         implies a big architectural change (a new service or dependency, a data \
                         model or protocol change, or a security/performance trade-off). Reject \
                         questions about implementation detail, and questions the lead could \
                         answer itself from the codebase or memory."
                    ),
                    "criteria": {
                        "true": "Worth asking the user",
                        "false": "Not worth asking (implementation detail, or answerable internally)"
                    }
                }),
            );
        }
        let state = json!({ "user_request": request });
        let answers = self.jev.evaluate(state, Value::Object(qmap)).await?;

        let mut accepted: Vec<(f64, String)> = Vec::new();
        let mut rejected: Vec<(f64, String)> = Vec::new();
        for (i, q) in questions.into_iter().enumerate() {
            let p = noul(&answers, &format!("q{i}")).unwrap_or(0.0);
            if p >= ACCEPT_AT {
                accepted.push((p, q));
            } else {
                rejected.push((p, q));
            }
        }
        Ok(render_review(&accepted, &rejected))
    }
}

/// The DATA line(s) the lead reads: each question with its Jev score and an
/// accepted/rejected label. No imperative - the `## Requirements` prompt decides
/// what to do with it. Pure, so it is unit-tested without a network.
fn render_review(accepted: &[(f64, String)], rejected: &[(f64, String)]) -> String {
    let mut s = format!("Jev question review (accepted >= {ACCEPT_AT:.2}):\n");
    for (p, q) in accepted {
        s.push_str(&format!("  ACCEPTED [{p:.2}] {q}\n"));
    }
    for (p, q) in rejected {
        s.push_str(&format!("  REJECTED [{p:.2}] {q}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_labels_questions_by_threshold() {
        let accepted = vec![(0.92, "When offline, should edits queue?".to_string())];
        let rejected = vec![(0.15, "Should we use Redis?".to_string())];
        let out = render_review(&accepted, &rejected);
        assert!(
            out.contains("ACCEPTED [0.92] When offline, should edits queue?"),
            "{out}"
        );
        assert!(
            out.contains("REJECTED [0.15] Should we use Redis?"),
            "{out}"
        );
        // Data-only: no imperative verbs.
        assert!(!out.contains("ask the user"), "{out}");
    }

    #[test]
    fn questions_may_arrive_as_a_string_or_an_array() {
        assert_eq!(
            QuestionInput::One("q".into()).into_vec(),
            vec!["q".to_string()]
        );
        assert_eq!(
            QuestionInput::Many(vec!["a".into(), "b".into()]).into_vec(),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
