//! The tech lead answering a delegated sub-agent that is stuck.
//!
//! `ask_upwards` gives a small delegate a way out of a decision it cannot make:
//! it asks the model that owns the session. That answer is produced by
//! [`ParentAsk`], a single tool-less chat call to the session's own model, so the
//! escalation cannot recurse into another agent run.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{UpwardAsk, Verdict};

use crate::llm::{ChatMessage, LlmClient, Role};

/// Answers `ask_upwards` questions with the session's own model.
pub struct ParentAsk {
    client: Arc<LlmClient>,
    /// System prose telling the parent what it is answering: it is the tech lead
    /// and one of its sub-agents is stuck.
    system: String,
}

impl ParentAsk {
    pub fn new(client: Arc<LlmClient>, system: impl Into<String>) -> Self {
        Self {
            client,
            system: system.into(),
        }
    }
}

#[async_trait]
impl UpwardAsk for ParentAsk {
    async fn ask(&self, question: &str) -> Result<String> {
        let messages = vec![
            ChatMessage::new(Role::System, self.system.clone()),
            ChatMessage::new(Role::User, question.to_string()),
        ];
        self.client.chat(&messages).await
    }

    async fn approve(&self, title: &str, detail: &str) -> Result<Verdict> {
        let prompt = format!(
            "One of your sub-agents (a developer model) is about to make a DESTRUCTIVE change \
             and asks for your permission first.\n\n\
             Action: {title}\n\
             {detail}\n\n\
             Approve ONLY if deleting that code is genuinely required by the task it was given. \
             Otherwise refuse, and in one line tell it what to do instead — the smallest edit that \
             keeps the existing code.\n\n\
             Reply with exactly one line and nothing else:\n\
             APPROVE\n\
             or\n\
             DENY: <one line: what it must do instead>"
        );
        let messages = vec![
            ChatMessage::new(Role::System, self.system.clone()),
            ChatMessage::new(Role::User, prompt),
        ];
        // Bounded: a sub-agent is itself inside its own budget, so a parent that
        // does not answer promptly must not hold the whole run open. No answer
        // (or a failed request) fails closed.
        match tokio::time::timeout(APPROVAL_TIMEOUT, self.client.chat(&messages)).await {
            Ok(Ok(text)) => Ok(parse_verdict(&text)),
            _ => Ok(Verdict::Unavailable),
        }
    }

    async fn summarise(&self, transcript: &str) -> Result<Option<String>> {
        let prompt = format!(
            "One of your sub-agents (a developer model) has run out of context mid-task. Its \
             transcript so far follows.\n\n--- transcript ---\n{transcript}\n--- end transcript \
             ---\n\nWrite a compact briefing that lets it CONTINUE the same task: (1) the task it \
             was given, restated in one or two lines so it knows what it is still doing; (2) what \
             it has already done, and the exact files it changed; (3) what it learned - the \
             commands it ran and their results, and any error it hit; (4) exactly what remains. \
             Keep exact file paths, identifiers and commands. Do not invent new requirements, do \
             not add advice, and do not address the user. Output only the briefing."
        );
        let messages = vec![
            ChatMessage::new(Role::System, self.system.clone()),
            ChatMessage::new(Role::User, prompt),
        ];
        // Bounded like `approve`: no summary (or a failed request) means the caller
        // keeps its previous behaviour rather than the run dying.
        match tokio::time::timeout(SUMMARY_TIMEOUT, self.client.chat(&messages)).await {
            Ok(Ok(text)) => Ok(usable_summary(&text)),
            _ => Ok(None),
        }
    }
}

/// How long a parent model gets to answer a permission request before the
/// destructive action is refused.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the parent model gets to write a context-overflow summary. Summarising
/// is a real generation, so it needs longer than a one-line verdict - but still
/// bounded, so a sub-agent can never be held open by a parent that stalls.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(120);

/// Read a permission verdict out of the parent's reply. Fail closed: only a reply
/// whose first substantive line OPENS with `APPROVE` approves; `DENY: <why>`
/// refuses with that reason; anything else — a rambling non-answer, an empty
/// reply, markdown noise — is `Unavailable`, which callers treat as a refusal.
pub fn parse_verdict(reply: &str) -> Verdict {
    for raw in reply.lines() {
        let line = raw
            .trim()
            .trim_matches(|c: char| matches!(c, '*' | '#' | '`' | '_' | ' '))
            .trim();
        if line.is_empty() {
            continue;
        }
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("APPROVE") {
            return Verdict::Approved;
        }
        if upper.starts_with("DENY") {
            // The line is ASCII up to here, so byte 4 is a safe split point.
            let why = line[4..]
                .trim_start_matches([':', '-', '.', ' ', '\t'])
                .trim();
            return Verdict::Denied(if why.is_empty() {
                "your tech lead refused this change".to_string()
            } else {
                why.to_string()
            });
        }
        // The first substantive line decided; it was not a verdict.
        break;
    }
    Verdict::Unavailable
}

/// The parent's reply as a usable summary: trimmed, or `None` when it returned
/// nothing usable. Fail open - no summary only means the caller falls back to its
/// own degraded path, so an empty reply must never replace a real transcript.
fn usable_summary(reply: &str) -> Option<String> {
    let text = reply.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_accepts_only_an_explicit_approval() {
        assert_eq!(parse_verdict("APPROVE"), Verdict::Approved);
        assert_eq!(parse_verdict("approve\n"), Verdict::Approved);
        assert_eq!(parse_verdict("**APPROVE**"), Verdict::Approved);
        // A rambling answer is never an approval.
        assert_eq!(
            parse_verdict("I am not sure what you mean by destructive."),
            Verdict::Unavailable
        );
        assert_eq!(parse_verdict(""), Verdict::Unavailable);
        assert_eq!(parse_verdict("\n  \n"), Verdict::Unavailable);
    }

    #[test]
    fn parse_verdict_reads_a_denial_reason() {
        assert_eq!(
            parse_verdict("DENY: use fs_edit and keep greet_works"),
            Verdict::Denied("use fs_edit and keep greet_works".to_string())
        );
        assert_eq!(
            parse_verdict("deny - keep the existing test"),
            Verdict::Denied("keep the existing test".to_string())
        );
        // A bare denial still denies, with a usable default reason.
        assert!(matches!(parse_verdict("DENY"), Verdict::Denied(_)));
        assert!(!parse_verdict("DENY").is_approved());
        assert!(Verdict::Approved.is_approved());
    }

    #[test]
    fn usable_summary_keeps_text_and_rejects_blank() {
        assert_eq!(
            usable_summary("  a summary  "),
            Some("a summary".to_string())
        );
        // A blank reply is not a summary: the caller must keep its transcript.
        assert_eq!(usable_summary("   \n"), None);
        assert_eq!(usable_summary(""), None);
    }
}
