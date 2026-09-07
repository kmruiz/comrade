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
/// Strategy: the system message (index 0) is sacred; when over budget we drop
/// the oldest non-system messages first, then hard-truncate any oversized tool
/// observations. No semantic summarization yet — a hook can be added later.
pub struct ContextManager {
    budget_tokens: usize,
    max_tool_output_chars: usize,
    history: Vec<ChatMessage>,
    /// Number of messages evicted so far (informational).
    pub evicted: usize,
}

impl ContextManager {
    pub fn new(budget_tokens: usize, max_tool_output_chars: usize) -> Self {
        Self {
            budget_tokens,
            max_tool_output_chars,
            history: Vec::new(),
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

    /// Drop oldest messages until within budget. Never drops index 0 (system).
    pub fn enforce_budget(&mut self) {
        loop {
            if self.history.len() <= 1 {
                break;
            }
            let total: usize = self
                .history
                .iter()
                .map(|m| estimate_tokens(&m.content))
                .sum();
            if total <= self.budget_tokens {
                break;
            }
            self.history.remove(1);
            self.evicted += 1;
        }
    }

    pub fn total_tokens(&self) -> usize {
        self.history
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, Role};

    #[test]
    fn budget_evicts_oldest_non_system() {
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
        assert!(cm.total_tokens() <= 20);
        assert!(cm.evicted >= 1);
    }

    #[test]
    fn observation_truncation() {
        let cm = ContextManager::new(1000, 10);
        let out = cm.truncate_observation(&"x".repeat(50));
        assert!(out.contains("truncated"));
    }
}
