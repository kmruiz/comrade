//! Comrade agent core.
//!
//! Everything needed to *drive* an agent session against a local/OpenAI
//! compatible model: configuration, the LLM client, the context/token-budget
//! manager, the ReAct text-protocol adapter, the observable session state, and
//! the agent loop itself. The core depends only on `comrade-tool` contracts and
//! is agnostic to the concrete tool crates and the UI.

pub mod advise;
pub mod agent;
pub mod compact;
pub mod config;
pub mod context;
pub mod delegate;
pub mod hooks;
pub mod instructions;
pub mod llm;
pub mod react;
pub mod redact;
pub mod session;
pub mod summarise;
pub mod undo;
pub mod upward;
pub mod worktree;

pub use advise::AskAdviseTool;
pub use agent::{build_session_context, run_agent, run_agent_with_history};
pub use compact::{CompactReport, compact_history};
pub use config::{
    Autonomy, Config, DelegateCfg, LoadedConfig, McpAuth, McpConfig, McpServerCfg, McpTransport,
    SensorCfg, SensorMode, expand_env_value,
};
pub use context::{ContextManager, estimate_tokens};
pub use delegate::{DelegateLimits, DelegateTool};
pub use hooks::Hooks;
pub use instructions::load_project_instructions;
pub use llm::{ChatMessage, LlmClient, Role};
pub use redact::Redactor;
pub use session::{AgentEvent, AgentSession};
pub use summarise::SummariseTool;
pub use undo::MemoryUndo;
pub use upward::ParentAsk;
pub use worktree::Worktree;
