//! Comrade agent core.
//!
//! Everything needed to *drive* an agent session against a local/OpenAI
//! compatible model: configuration, the LLM client, the context/token-budget
//! manager, the ReAct text-protocol adapter, the observable session state, and
//! the agent loop itself. The core depends only on `comrade-tool` contracts and
//! is agnostic to the concrete tool crates and the UI.

pub mod agent;
pub mod config;
pub mod context;
pub mod llm;
pub mod react;
pub mod session;
pub mod undo;

pub use agent::run_agent;
pub use config::{Autonomy, Config, LoadedConfig};
pub use context::{ContextManager, estimate_tokens};
pub use llm::{ChatMessage, LlmClient, Role};
pub use session::{AgentEvent, AgentSession};
pub use undo::MemoryUndo;
