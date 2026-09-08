//! Shared contracts for Comrade tools.
//!
//! This crate stays slim on purpose: every concrete tool crate (`comrade-tool-*`)
//! and the agent core (`comrade-core`) depend only on the traits and value types
//! defined here, never on each other.

pub mod plan;
pub mod repo;
pub mod tool;

pub use plan::{AGENT_MODEL, PlanStatus, PlanStep, PlanStepDraft, PlanTarget, SessionControl};
pub use repo::changed_files_abs;
pub use tool::{
    ActivityEvents, ApprovalNotes, NoopEvents, Steer, Tool, ToolContext, ToolRegistry, ToolSpec,
    UndoLog, UserIo, UserPrompt, UserReply,
};
