//! The `ask_advise` tool: consult one configured delegate model for ADVICE.
//!
//! The main ("tech lead") model keeps a task on its own plate but wants a
//! second opinion before committing to an approach — how to plan or split a
//! task, which delegate fits a piece of work, whether a design/plan is sound,
//! what to watch out for.
//!
//! Besides free-form advice, the tool doubles as the *readiness handshake* for
//! delegated plan steps: pass `step` = a plan step id (instead of `model` +
//! `question`) and the step's OWN delegate is consulted about whether the
//! step's context (goal/verification/context) is enough for it to pick the step
//! up. When the delegate's reply says the context suffices (a final
//! `VERDICT: READY` line) the step is marked [`PlanStatus::Ready`] — ready to
//! be delegated; when the delegate reports it needs more, the step stays
//! `Pending` with an "awaiting context: ..." note and the lead can feed the
//! request back via `set_step_context` and re-ask. This lets the lead run the
//! readiness checks in parallel while doing other work.
//!
//! Unlike [`crate::delegate::DelegateTool`] nothing is handed off: no step is
//! marked working, no fix rounds, and the consulted delegate cannot change the
//! repository. Consultations normally need no approval, but a delegate
//! configured `approval = "ask"` pauses for human approval before the advice
//! runs (and one set to "deny" is refused outright) — same policy as
//! [`crate::delegate`].
//!
//! The advisor gets a READ-ONLY sub-agent loop over the repository (see
//! [`AskAdviseTool::read_only_for_advice`] and the `advise_registry()` builder
//! in comrade-tui): it can list/read/search files, inspect git history, use the
//! project model and the memory/web tools to ground its advice — but it has no
//! write/edit/apply/rename/shell/run/commit/plan tools, so advice can never
//! mutate the workspace or the session. Delegates whose protocol is ReAct-text
//! still run through the same loop as `delegate`; the reply is the advisor's
//! final text, which the lead reads and decides on.

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use comrade_tool::{
    AGENT_MODEL, PlanStatus, PlanTarget, Tool, ToolContext, ToolRegistry, ToolSpec,
};
use serde_json::{Value, json};

use crate::config::DelegateCfg;
use crate::delegate::{
    DelegateLimits, Target, approval_preview, build_targets, cfg_line, enforce_approval,
    render_subagent_system, run_delegate_subagent,
};

/// Name of the tool advertised to the tech lead model.
pub const TOOL_NAME: &str = "ask_advise";

/// Wording of the read-guard nudge for an advisor: unlike a working `delegate`
/// it must not implement — it should stop browsing and give its advice.
const ADVICE_READ_NUDGE: &str = "You have performed {count} read-only calls in a row without \
     answering. You have enough context - give your advice now as your final answer (what to do, \
     in what order, what to avoid). Do not keep reading.";

/// A tool that asks one of the configured delegate models for advice.
pub struct AskAdviseTool {
    spec: ToolSpec,
    targets: Vec<Target>,
    /// Read-only repository tools the advisor may browse.
    tools: ToolRegistry,
    /// Iteration/token caps for the advisor's read-only sub-agent run.
    limits: DelegateLimits,
}

mod tool;
mod readiness;
#[cfg(test)]
mod tests;
pub use tool::*;
pub use readiness::*;
