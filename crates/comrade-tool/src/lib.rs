//! Shared contracts for Comrade tools.
//!
//! This crate stays slim on purpose: every concrete tool crate (`comrade-tool-*`)
//! and the agent core (`comrade-core`) depend only on the traits and value types
//! defined here, never on each other.

pub mod plan;
pub mod tool;

pub use plan::{PlanStatus, PlanStep, PlanTarget, SessionControl};
pub use tool::{Tool, ToolContext, ToolRegistry, ToolSpec, UndoLog, UserIo, UserPrompt, UserReply};
