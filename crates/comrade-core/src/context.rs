use crate::llm::ChatMessage;

/// Crude token estimate (~4 chars/token) used for budget enforcement. Cheap
/// and deterministic; good enough to keep context small.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    (text.chars().count() + 3) / 4 + 4
}

/// Holds the rolling message history and enforces a token budget.
///
/// The system message (index 0) is sacred. To fit a budget the manager:
///
/// 1. eagerly strips executed tool arguments via [`Self::note_tool_done`]
///    (a spent `apply_edit` payload must not be re-sent every turn);
/// 2. stubs the oldest *large* tool observations (keeping a head + marker);
/// 3. as a last resort evicts the oldest messages while folding their thought
///    lines into a compact "Earlier context" rollup, so long sessions degrade
///    to summaries instead of silently losing history.
///
/// The last [`KEEP_RECENT`] messages and the current step are always preserved.
const KEEP_RECENT: usize = 2;
/// Observations larger than this many chars are stubbing candidates.
const STUB_MIN_CHARS: usize = 600;
/// Characters of the observation head kept when stubbing.
const STUB_HEAD_CHARS: usize = 140;
/// Upper bound on the accumulated "Earlier context" rollup.
const ROLLUP_MAX_CHARS: usize = 2000;

pub struct ContextManager {
    budget_tokens: usize,
    max_tool_output_chars: usize,
    history: Vec<ChatMessage>,
    /// Compacted summary of evicted turns ("Earlier context").
    rollup: String,
    /// Number of messages evicted so far (informational).
    pub evicted: usize,
}

impl ContextManager {
    pub fn new(budget_tokens: usize, max_tool_output_chars: usize) -> Self {
        Self {
            budget_tokens,
            max_tool_output_chars,
            history: Vec::new(),
            rollup: String::new(),
            evicted: 0,
        }
    }

    pub fn with_system(
        system: impl Into<String>,
        budget_tokens: usize,
        max_tool_output_chars: usize,
    ) -> Self {
        let mut m = Self::new(budget_tokens, max_tool_output_chars);
        m.push(ChatMessage::new(crate::llm::Role::System, system));
        m
    }

    pub fn push(&mut self, msg: ChatMessage) {
        self.history.push(msg);
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.history
    }

    pub fn truncate_observation(&self, text: &str) -> String {
        let limit = self.max_tool_output_chars;
        let chars = text.chars().count();
        if chars <= limit {
            return text.to_string();
        }
        let mut out: String = text.chars().take(limit).collect();
        out.push_str(&format!("\n... (truncated, {chars} chars -> {limit})"));
        out
    }

    /// Condense the last assistant message (the tool call we just executed) so
    /// its arguments stop consuming context. Keeps a short thought when there
    /// is one.
    pub fn note_tool_done(&mut self, tool: &str, thought: Option<&str>) {
        let Some(idx) = self
            .history
            .iter()
            .rposition(|m| m.role == crate::llm::Role::Assistant)
        else {
            return;
        };
        let thought = thought
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| t.chars().take(160).collect::<String>());
        let content = match thought {
            Some(t) => format!("Thought: {t}\n[Tool: {tool} executed]"),
            None => format!("[Tool: {tool} executed]"),
        };
        self.history[idx].content = content;
    }

    /// Fit history under the token budget: stub large old observations, then
    /// evict the oldest messages (folding thoughts into an "Earlier context"
    /// rollup). Never drops index 0 (system) or the last [`KEEP_RECENT`]
    /// messages.
    pub fn enforce_budget(&mut self) {
        // 1. Stub oversized observations from the past (not the recent window).
        loop {
            if !self.over_budget() {
                break;
            }
            let Some(idx) = self.oldest_stubbable_observation() else {
                break;
            };
            self.stub_observation(idx);
        }
        // 2. Evict from the front, rolling up what we can keep.
        let mut rolled_up = false;
        while self.over_budget() && self.history.len() > 1 {
            let i = 1;
            if let Some(snippet) = rollup_snippet(&self.history[i]) {
                self.append_rollup(&snippet);
                rolled_up = true;
            }
            self.history.remove(i);
            self.evicted += 1;
        }
        if rolled_up {
            self.upsert_rollup_message();
        }
    }

    fn over_budget(&self) -> bool {
        self.total_tokens() > self.budget_tokens
    }

    /// Index of the oldest large tool observation that is outside the protected
    /// recent window, if any.
    fn oldest_stubbable_observation(&self) -> Option<usize> {
        let protect_from = self.history.len().saturating_sub(KEEP_RECENT);
        (1..protect_from).find(|&i| {
            let m = &self.history[i];
            m.role == crate::llm::Role::User
                && m.content.starts_with("Observation (result of")
                && m.content.chars().count() > STUB_MIN_CHARS
        })
    }

    fn stub_observation(&mut self, idx: usize) {
        let full = std::mem::take(&mut self.history[idx].content);
        let chars = full.chars().count();
        let head: String = full.chars().take(STUB_HEAD_CHARS).collect();
        let mut stub = head;
        stub.push_str(&format!("… [{chars} chars trimmed]"));
        self.history[idx].content = stub;
    }

    fn append_rollup(&mut self, snippet: &str) {
        let add = snippet.chars().count() + 2; // newline + bullet
        if self.rollup.chars().count() + add > ROLLUP_MAX_CHARS {
            // Keep only the most recent snippet rather than silently dropping.
            self.rollup = snippet.to_string();
            self.rollup.push_str(" [older context dropped]");
            return;
        }
        if !self.rollup.is_empty() {
            self.rollup.push('\n');
        }
        self.rollup.push_str(snippet);
    }

    fn upsert_rollup_message(&mut self) {
        let text = format!("Earlier context (compacted):\n{}", self.rollup);
        if let Some(idx) = self.history.iter().position(|m| {
            m.role == crate::llm::Role::User && m.content.starts_with("Earlier context (compacted)")
        }) {
            self.history[idx].content = text;
        } else {
            self.history
                .insert(1, ChatMessage::new(crate::llm::Role::User, text));
        }
    }

    pub fn total_tokens(&self) -> usize {
        self.history
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum()
    }
}

/// Extract a terse summary of a message about to be evicted, or `None` if it
/// adds nothing worth keeping (e.g. a raw observation whose content is gone).
fn rollup_snippet(msg: &ChatMessage) -> Option<String> {
    let content = msg.content.trim();
    if content.is_empty() {
        return None;
    }
    match msg.role {
        crate::llm::Role::System => None,
        crate::llm::Role::User => {
            if content.starts_with("Observation (result of")
                || content.starts_with("Earlier context (compacted)")
                || content.starts_with("ERROR:")
            {
                None
            } else {
                let head: String = content.chars().take(140).collect();
                Some(format!("- {head}"))
            }
        }
        crate::llm::Role::Assistant => {
            if let Some(t) = content
                .strip_prefix("Thought:")
                .and_then(|rest| rest.split("[Tool:").next())
            {
                let t = t.trim();
                if !t.is_empty() {
                    return Some(format!(
                        "- thought: {}",
                        t.chars().take(140).collect::<String>()
                    ));
                }
            }
            if content.starts_with("[Tool:") {
                let name = content
                    .trim_start_matches("[Tool:")
                    .split_whitespace()
                    .next()
                    .unwrap_or("?");
                return Some(format!("- ran tool {name}"));
            }
            let head: String = content.chars().take(140).collect();
            Some(format!("- {head}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, Role};

    #[test]
    fn budget_evicts_oldest_and_folds_into_rollup() {
        let mut cm = ContextManager::new(20, 100);
        cm.push(ChatMessage::new(Role::System, "sys"));
        cm.push(ChatMessage::new(
            Role::User,
            "hello world hello world hello world",
        ));
        cm.push(ChatMessage::new(
            Role::User,
            "second message that is also reasonably long and chatty",
        ));
        cm.enforce_budget();
        // system survives
        assert_eq!(cm.messages()[0].role, Role::System);
        assert!(cm.evicted >= 1);
        // what was evicted is not silently lost: a compact rollup remains
        assert!(
            cm.messages()
                .iter()
                .any(|m| m.content.starts_with("Earlier context (compacted)"))
        );
        assert!(cm.total_tokens() <= 60);
    }

    #[test]
    fn observation_truncation() {
        let cm = ContextManager::new(1000, 10);
        let out = cm.truncate_observation(&"x".repeat(50));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn executed_tool_args_are_stripped() {
        let mut cm = ContextManager::new(100_000, 100_000);
        cm.push(ChatMessage::new(Role::System, "sys"));
        cm.push(ChatMessage::new(
            Role::Assistant,
            "Thought: big edit\nTool: apply_edit\nArgs: {\"old\": \"xxxxx\", \"new\": \"yyyyy\"}",
        ));
        cm.push(ChatMessage::new(
            Role::User,
            "Observation (result of `apply_edit`):\nEdited src/a.rs.",
        ));
        cm.note_tool_done("apply_edit", Some("big edit"));
        let last_assistant = cm
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .unwrap();
        assert!(!last_assistant.content.contains("Args:"));
        assert!(!last_assistant.content.contains("xxxxx"));
        assert!(
            last_assistant
                .content
                .contains("[Tool: apply_edit executed]")
        );
        assert!(last_assistant.content.contains("Thought: big edit"));
    }

    #[test]
    fn stubs_old_observations_but_keeps_recent_window() {
        let mut cm = ContextManager::new(30, 100_000); // small budget
        cm.push(ChatMessage::new(Role::System, "sys"));
        // old large observation (protected: must not exceed small budget anyway)
        cm.push(ChatMessage::new(
            Role::User,
            format!("Observation (result of `read_file`):\n{}", "a".repeat(1000)),
        ));
        // recent pair must survive
        cm.push(ChatMessage::new(Role::Assistant, "Tool: test executed"));
        cm.push(ChatMessage::new(
            Role::User,
            "Observation (result of `test`):\nok (exit 0)",
        ));
        cm.enforce_budget();
        let roles: Vec<Role> = cm.messages().iter().map(|m| m.role).collect();
        // system, possibly rollup/old-user, assistant, user
        assert_eq!(*roles.last().unwrap(), Role::User);
        assert_eq!(cm.messages()[cm.messages().len() - 2].role, Role::Assistant);
        // the recent observation is intact
        assert!(cm.messages().last().unwrap().content.contains("exit 0"));
        // old read observation, if still present, is stubbed (shorter than before)
        let old = cm
            .messages()
            .iter()
            .find(|m| m.content.contains("a".repeat(10).as_str()))
            .map(|m| m.content.chars().count());
        assert!(
            old.is_none(),
            "oversized observation should be gone/stubbed"
        );
        assert!(cm.total_tokens() <= 40);
    }
}
