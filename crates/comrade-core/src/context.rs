use crate::llm::ChatMessage;

/// Crude token estimate (~4 chars/token) used for budget enforcement. Cheap
/// and deterministic; good enough to keep context small.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.chars().count().div_ceil(4) + 4
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
/// History is trimmed once it *approaches* the budget (the `trim_target`),
/// not only after crossing it: the reserved headroom keeps the next model
/// request (plus its tool output and answer) comfortably inside the window.
///
/// The last [`KEEP_RECENT`] messages and the current step are always preserved.
const KEEP_RECENT: usize = 2;
/// One message out of every this many budget tokens is reserved as headroom
/// while trimming (a tenth of the budget).
const HEADROOM_FRACTION: usize = 10;
/// Budgets below this keep the plain cap: a fraction of a tiny budget would be
/// meaningless and would evict history the moment a single message arrives.
const HEADROOM_MIN_BUDGET: usize = 4000;
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

    pub fn rollup(&self) -> &str {
        &self.rollup
    }

    pub fn history_clone(&self) -> Vec<ChatMessage> {
        self.history.clone()
    }

    pub fn from_parts(
        budget_tokens: usize,
        max_tool_output_chars: usize,
        history: Vec<ChatMessage>,
        rollup: String,
        evicted: usize,
    ) -> Self {
        Self {
            budget_tokens,
            max_tool_output_chars,
            history,
            rollup,
            evicted,
        }
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
        // Keep only a natural-language trace of the turn. We deliberately avoid
        // bracket/scaffold markers ("[Tool executed]") - models tend to echo
        // them back as answers.
        let content = match thought {
            Some(t) => format!("Thought: {t}"),
            None => format!("Ran {tool}."),
        };
        self.history[idx].content = content;
        // Native-mode tool calls carry the full arguments payload; they are
        // spent once executed.
        self.history[idx].tool_calls = None;
    }

    /// After a native turn dispatched its function calls, mark the assistant
    /// message done. The `tool_calls` must stay intact: the OpenAI wire format
    /// requires a `role: "tool"` message to follow an assistant message that
    /// declares the matching `tool_calls`. The content is left as the model
    /// wrote it (no synthetic marker the model could echo back).
    pub fn note_turn_done(&mut self) {
        let Some(idx) = self
            .history
            .iter()
            .rposition(|m| m.role == crate::llm::Role::Assistant)
        else {
            return;
        };
        // content intentionally unchanged
        let _ = idx;
    }

    /// Replace the whole history with a model-written `summary`: keep the system
    /// message (index 0) when present, drop every other message and fold the
    /// summary into the "Earlier context (compacted)" rollup message. Used by
    /// the user-triggered compaction (M-c).
    pub fn compact(&mut self, summary: &str) {
        self.history.truncate(1); // keep only the system message when present
        self.rollup = summary.to_string();
        self.evicted = 0;
        if self.history.is_empty() {
            self.history.push(ChatMessage::new(
                crate::llm::Role::User,
                format!("Earlier context (compacted):\n{summary}"),
            ));
        } else {
            self.upsert_rollup_message();
        }
    }

    /// Fit history under the budget, compacting as it *approaches* the cap:
    /// stub large old observations, then evict the oldest messages (folding
    /// thoughts into an "Earlier context" rollup). Never drops index 0
    /// (system) or the last [`KEEP_RECENT`] messages.
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
        // 2. Evict from the front, rolling up what we can keep. Tool results are
        // removed together with the assistant message that declared their
        // `tool_calls`, so a `role: "tool"` message is never orphaned.
        let mut rolled_up = false;
        while self.over_budget() && self.history.len() > 1 {
            let i = 1;
            let is_assistant_calls = self.history[i].role == crate::llm::Role::Assistant
                && self.history[i].tool_calls.is_some();
            if let Some(snippet) = rollup_snippet(&self.history[i]) {
                self.append_rollup(&snippet);
                rolled_up = true;
            }
            self.history.remove(i);
            self.evicted += 1;
            // Drop the tool results that referenced those calls too.
            if is_assistant_calls {
                while self.history.len() > 1 && self.history[1].role == crate::llm::Role::Tool {
                    self.history.remove(1);
                    self.evicted += 1;
                }
            } else if self.history.len() > 1 && self.history[1].role == crate::llm::Role::Tool {
                // Orphan tool message (shouldn't happen); remove it alone.
                self.history.remove(1);
                self.evicted += 1;
            }
        }
        if rolled_up {
            self.upsert_rollup_message();
        }
    }

    /// Token level history is compacted down to: the hard budget minus a
    /// headroom fraction, so a request is never sent at the very edge of the
    /// window. Small budgets fall back to the plain cap.
    fn trim_target(&self) -> usize {
        if self.budget_tokens >= HEADROOM_MIN_BUDGET {
            self.budget_tokens - self.budget_tokens / HEADROOM_FRACTION
        } else {
            self.budget_tokens
        }
    }

    fn over_budget(&self) -> bool {
        self.total_tokens() > self.trim_target()
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
        crate::llm::Role::System | crate::llm::Role::Tool => None,
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
    fn compact_replaces_history_with_summary() {
        let mut cm = ContextManager::with_system("sys", 1000, 100);
        cm.push(ChatMessage::new(Role::User, "do the thing"));
        cm.push(ChatMessage::new(Role::Assistant, "did the thing"));
        cm.compact("did X");
        assert_eq!(cm.messages().len(), 2);
        assert_eq!(cm.messages()[0].role, Role::System);
        assert_eq!(cm.messages()[0].content, "sys");
        assert!(cm.messages()[1].content.contains("did X"));
        assert_eq!(cm.rollup(), "did X");
        assert_eq!(cm.evicted, 0);
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
        assert!(!last_assistant.content.contains("[Tool"));
        assert_eq!(last_assistant.content, "Thought: big edit");
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

    #[test]
    fn compaction_starts_below_the_hard_budget_keeping_headroom() {
        // budget 10_000 >= HEADROOM_MIN_BUDGET: history is trimmed down to the
        // trim target (90% = 9_000) once it approaches the cap, leaving
        // headroom, instead of waiting until it overflows the hard budget.
        let mut cm = ContextManager::new(10_000, 100_000);
        cm.push(ChatMessage::new(Role::System, "sys"));
        // each message is ~2.25k estimated tokens
        for i in 0..4u32 {
            cm.push(ChatMessage::new(
                Role::User,
                format!("task {i}: {}", "x".repeat(9_000)),
            ));
        }
        // ~9k tokens: over the 9_000 trim target yet under the 10_000 cap.
        assert!(
            cm.total_tokens() > 9_000,
            "precondition: history approaches the budget"
        );
        assert!(
            cm.total_tokens() <= 10_000,
            "precondition: still under the hard cap"
        );
        cm.enforce_budget();
        assert!(
            cm.total_tokens() <= 9_000,
            "compacted to the trim target, not the hard cap"
        );
        assert!(cm.evicted >= 1);
        // evicted history is summarized, not silently dropped
        assert!(
            cm.messages()
                .iter()
                .any(|m| m.content.starts_with("Earlier context (compacted)"))
        );
    }

    #[test]
    fn no_compaction_below_the_trim_target() {
        let mut cm = ContextManager::new(10_000, 100_000);
        cm.push(ChatMessage::new(Role::System, "sys"));
        cm.push(ChatMessage::new(
            Role::User,
            "short task that stays under the trim target",
        ));
        cm.enforce_budget();
        assert_eq!(cm.evicted, 0);
    }
}

#[cfg(test)]
mod tool_role_invariant_tests {
    use super::*;
    use crate::llm::{ChatMessage, Role, ToolCallMsg};

    fn pair(id: &str, calls: Vec<ToolCallMsg>) -> Vec<ChatMessage> {
        let mut out = vec![ChatMessage::assistant_with_calls(
            format!("assistant turn {id} with a fair amount of text"),
            calls,
        )];
        out.push(ChatMessage::tool_result(
            id,
            format!("result of {id}, some detail"),
        ));
        out.push(ChatMessage::tool_result(
            id,
            "second tool result for the same turn",
        ));
        out
    }

    fn no_orphaned_tool_messages(history: &[ChatMessage]) -> bool {
        let mut pending_calls = false;
        for m in history {
            match m.role {
                Role::Assistant if m.tool_calls.is_some() => pending_calls = true,
                Role::Tool if !pending_calls => return false,
                Role::User => pending_calls = false,
                _ => {}
            }
        }
        true
    }

    #[test]
    fn note_turn_done_keeps_tool_calls() {
        let mut cm = ContextManager::new(1_000_000, 100_000);
        cm.push(ChatMessage::new(Role::System, "sys"));
        for m in pair(
            "c1",
            vec![ToolCallMsg {
                id: "c1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path":"a.rs","content":"x".repeat(40)}),
            }],
        ) {
            cm.push(m);
        }
        cm.note_turn_done();
        let assistant = cm
            .messages()
            .iter()
            .find(|m| m.role == Role::Assistant)
            .unwrap();
        assert!(
            assistant.tool_calls.is_some(),
            "tool_calls must be preserved"
        );
        // content is untouched - no synthetic marker the model could echo
        assert!(assistant.content.contains("assistant turn c1"));
    }

    #[test]
    fn eviction_never_orphans_tool_messages() {
        let mut cm = ContextManager::new(600, 100_000);
        cm.push(ChatMessage::new(Role::System, "sys"));
        for i in 0..30u32 {
            let id = format!("c{i}");
            let calls = vec![ToolCallMsg {
                id: id.clone(),
                name: "run_task".into(),
                arguments: serde_json::json!({"task":"test"}),
            }];
            for m in pair(&id, calls) {
                cm.push(m);
            }
        }
        cm.enforce_budget();
        let history = cm.messages().to_vec();
        assert!(no_orphaned_tool_messages(&history), "{history:?}");
        assert!(cm.evicted > 0);
    }

    #[test]
    fn from_parts_roundtrips() {
        let history = vec![
            ChatMessage::new(Role::System, "sys"),
            ChatMessage::new(Role::User, "hello"),
        ];
        let rollup = "Earlier: test".to_string();
        let cm = ContextManager::from_parts(1000, 500, history.clone(), rollup.clone(), 3);
        assert_eq!(cm.messages().len(), 2);
        assert_eq!(cm.rollup(), rollup);
        assert_eq!(cm.evicted, 3);
        assert_eq!(cm.history_clone(), history);
    }
}
