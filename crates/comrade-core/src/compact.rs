//! User-triggered context compaction: ask the model to summarise the running
//! conversation and replace the history with that summary (M-c in the TUI).
//! Complements the automatic budget trimming in [`crate::context`].
use anyhow::{Context as _, Result};

use crate::context::ContextManager;
use crate::llm::{ChatMessage, LlmClient, Role};

/// What a compaction did, for reporting to the UI.
#[derive(Debug, Clone, Copy)]
pub struct CompactReport {
    pub before_messages: usize,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub summary_chars: usize,
}

/// The summariser's own system prompt: it must not answer the user, only write
/// terse notes the agent can resume from.
const SUMMARY_SYSTEM_PROMPT: &str = "You compact an AI coding agent's conversation so it can keep working with far less context. Read the transcript and write terse notes covering: (1) the user's goal and any constraints or preferences stated; (2) what has been done so far - files created or edited, commands run and their results; (3) key decisions and findings; (4) what is unresolved and the next steps. Preserve exact file paths, identifiers, commands and error messages that still matter. Drop small talk and settled detail. Write notes the agent can resume from; do not address the user and add no commentary.";

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Flatten the message history into a plain-text transcript. The system prompt
/// is skipped: it is the agent's own instructions, not work done.
fn render_transcript(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        if m.role == Role::System {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(role_label(m.role));
        out.push_str(": ");
        out.push_str(m.content.trim());
    }
    out
}

/// Summarise `history` with `client` and replace it in place with the summary,
/// returning what changed so the UI can report it.
pub async fn compact_history(
    client: &LlmClient,
    history: &mut ContextManager,
) -> Result<CompactReport> {
    let before_messages = history.messages().len();
    let before_tokens = history.total_tokens();
    let transcript = render_transcript(history.messages());
    // A fresh, minimal request: the summariser sees the transcript as data, not
    // as a conversation to continue (no tools, no tool-role messages).
    let request = vec![
        ChatMessage::new(Role::System, SUMMARY_SYSTEM_PROMPT),
        ChatMessage::new(
            Role::User,
            format!("Conversation so far:\n\n{transcript}\n\nNow write the summary notes."),
        ),
    ];
    let summary = client.chat(&request).await.context("summarising context")?;
    let summary = summary.trim();
    anyhow::ensure!(!summary.is_empty(), "the model returned an empty summary");
    history.compact(summary);
    Ok(CompactReport {
        before_messages,
        before_tokens,
        after_tokens: history.total_tokens(),
        summary_chars: summary.chars().count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_transcript_skips_system_and_labels_roles() {
        let messages = vec![
            ChatMessage::new(Role::System, "you are an agent"),
            ChatMessage::new(Role::User, "add a feature"),
            ChatMessage::new(Role::Assistant, "done"),
        ];
        let out = render_transcript(&messages);
        assert!(out.contains("user: add a feature"));
        assert!(out.contains("assistant: done"));
        assert!(!out.contains("you are an agent"));
        assert!(!out.contains("system:"));
    }

    #[test]
    fn render_transcript_separates_entries() {
        let messages = vec![
            ChatMessage::new(Role::User, "one"),
            ChatMessage::new(Role::User, "two"),
        ];
        assert_eq!(render_transcript(&messages), "user: one\n\nuser: two");
    }
}
