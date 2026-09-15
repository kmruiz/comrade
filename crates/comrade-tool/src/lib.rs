//! Shared contracts for Comrade tools.
//!
//! This crate stays slim on purpose: every concrete tool crate (`comrade-tool-*`)
//! and the agent core (`comrade-core`) depend only on the traits and value types
//! defined here, never on each other.

pub mod ask;
pub mod decl;
pub mod form;
pub mod plan;
pub mod policy;
pub mod repo;
pub mod task_runner;
pub mod tool;

pub use ask::{Upward, UpwardAsk, Verdict};
pub use decl::{declarations, removed_declarations};

pub use form::{DiffOption, FieldKind, FormField, FormSpec, truthy};
pub use plan::{AGENT_MODEL, PlanStatus, PlanStep, PlanStepDraft, PlanTarget, SessionControl};
pub use policy::{SecurityPolicy, check_command, confine, policy, set_policy, with_policy};
pub use repo::changed_files_abs;
pub use task_runner::{TaskRun, TaskRunner};
pub use tool::{
    ActivityEvents, CompactRequest, NoopEvents, Steer, Tool, ToolContext, ToolRegistry, ToolSpec,
    UndoLog, UserIo, UserPrompt, UserReply,
};
