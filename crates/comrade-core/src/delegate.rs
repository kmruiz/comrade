//! The `delegate` tool: hand a single, self-contained sub-task to another
//! model — typically a cheaper or faster one on a different provider.
//!
//! The main ("tech lead") model keeps orchestrating and committing, but can
//! offload a well-defined piece of work to a developer model configured in
//! `config.toml` under `[[delegates]]`. Unlike plain-chat sub-agents, a
//! delegate runs a **real tool-using sub-agent loop**: it is handed the same
//! repository tools (read/search/write/edit, run_tests/run_task, memory, web
//! search) so it can genuinely do the job — write the file, run the tests, fix
//! failures — instead of returning text the parent must apply by hand.
//!
//! Two design points keep the parent in control:
//! - The delegate's tool registry excludes `git_commit` (only the tech lead
//!   commits), the session/UI tools (`set_plan`, `update_plan`,
//!   `set_step_model`, `finish_plan`, `rename_session`, `set_status_bar`,
//!   `ask_question`) and `delegate` itself (no recursion). See
//!   [`DENIED_FOR_DELEGATES`].
//! - The delegate tool call is not approval-gated by default: delegating runs
//!   directly (like `git_commit`/`run_task`/`run_tests`), and every nested tool
//!   call the delegate makes runs auto-approved (its context has `auto_approve`
//!   set), so a delegate works end-to-end without pausing for a human.
//!   EXCEPTION: a delegate whose `[[delegates]]` entry sets
//!   `approval = "ask"` pauses for human approval before it runs, and one set
//!   to `approval = "deny"` is refused outright (see [`enforce_approval`]).

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use comrade_tool::{PlanStatus, PlanTarget, Tool, ToolContext, ToolRegistry, ToolSpec};
use serde_json::{Value, json};

use crate::agent::{LoopTracker, MAX_LOOP_REFUSALS, allow_read_step, loop_refusal};
use crate::config::{Autonomy, DelegateCfg};
use crate::context::ContextManager;
use crate::llm::{ChatMessage, LlmClient, Role, ToolCallMsg};
use crate::react::{parse_turn, render_observation};
use comrade_tool::AGENT_MODEL;

/// Name of the tool advertised to the tech lead model.
pub const TOOL_NAME: &str = "delegate";

/// Tools a delegate must never see, by spec name. Only the tech lead commits,
/// plans, renames the session or asks the human; `delegate` is excluded so a
/// delegate cannot recurse, `ask_advise` is excluded so a delegate cannot spawn
/// extra model chats of its own, and `set_step_model` is excluded so a delegate
/// cannot reassign its own (or any) plan step while working. Everything else
/// in the main registry — including the mutating tools (write_file,
/// apply_edit/apply_patch, shell, run_task, remember, amend_decision) — is fair
/// game because the delegate runs auto-approved under a one-shot human handoff.
pub const DENIED_FOR_DELEGATES: &[&str] = &[
    "git_commit",
    "delegate",
    "ask_advise",
    "ask_question",
    "rename_session",
    "set_status_bar",
    "set_plan",
    "update_plan",
    "set_step_model",
    "set_step_context",
    "finish_plan",
];

/// A delegated plan step gets one attempt from the delegate; if the parent's
/// verification then fails, the parent may re-delegate the same step with
/// `feedback` so the delegate can fix its work — up to this many fix rounds.
/// Rounds are recorded on the step's note (`working: <model>` for the first
/// attempt, `working: <model> (fix N/5)` for each repair), and further fix
/// requests are refused once the limit is reached so the parent takes the
/// step over and does it itself.
const MAX_FIX_ROUNDS: u64 = 5;

/// Budget/iteration limits for one delegated sub-agent run. Production values
/// come from the main `Config` (wired in comrade-tui main.rs); tests use the
/// defaults.
#[derive(Debug, Clone)]
pub struct DelegateLimits {
    pub max_iterations: usize,
    pub budget_tokens: usize,
    pub max_tool_output_chars: usize,
}

impl Default for DelegateLimits {
    fn default() -> Self {
        Self {
            max_iterations: 30,
            budget_tokens: 6000,
            max_tool_output_chars: 5000,
        }
    }
}

/// One configured delegate model plus the HTTP client that talks to it.
/// Shared (pub(crate)) with the `ask_advise` tool in advise.rs so both tools
/// talk to the same validated `[[delegates]]` entries.
pub(crate) struct Target {
    pub(crate) cfg: DelegateCfg,
    pub(crate) client: LlmClient,
}

/// A tool that runs a tool-using sub-agent loop on one of the configured
/// delegate models.
pub struct DelegateTool {
    spec: ToolSpec,
    targets: Vec<Target>,
    /// Repository tools the delegate may call (main registry minus
    /// [`DENIED_FOR_DELEGATES`]).
    tools: ToolRegistry,
    /// Iteration/token caps for the delegated sub-agent run.
    limits: DelegateLimits,
}

/// One advertising line for the tech lead: `- name` or `- name: description`.
/// The `description` is the config blurb of when to use that delegate — it is
/// what the tech lead reads to pick the right developer for a task — so it must
/// appear everywhere delegates are listed (tool doc, `model` arg, errors).
/// Blank blurbs degrade to the plain `- name` line.
pub(crate) fn delegate_line(name: &str, description: &str) -> String {
    match description.trim() {
        "" => format!("  - {name}"),
        desc => format!("  - {name}: {desc}"),
    }
}

/// One listing line for the tech lead, annotated with the delegate's approval
/// policy so it can tell at a glance which models pause for human approval
/// (`approval = "ask"`) and which are refused entirely (`approval = "deny"`).
/// Used wherever delegates are advertised (tool description, `model` arg, and
/// unknown-model errors) by both `delegate` and `ask_advise`.
pub(crate) fn cfg_line(d: &DelegateCfg) -> String {
    let line = delegate_line(&d.name, &d.description);
    match d.approval {
        Autonomy::Ask => format!("{line} [human approval required before it runs]"),
        Autonomy::Deny => format!("{line} [refused: configured `approval = \"deny\"`]"),
        Autonomy::Auto => line,
    }
}

/// Enforce a delegate's approval policy before a `delegate`/`ask_advise` run
/// starts. Called with the human-facing summary of what is about to run:
/// - [`Autonomy::Auto`] (default): run without asking.
/// - [`Autonomy::Ask`]: pause for [`ToolContext::confirm`] (which skips itself
///   when the context is auto-approved, e.g. `[security] autonomy = "auto"`).
/// - [`Autonomy::Deny`]: refuse to run this model through `delegate` /
///   `ask_advise` at all — a config-level block that even auto-approval does
///   not override.
pub(crate) async fn enforce_approval(
    cfg: &DelegateCfg,
    ctx: &ToolContext,
    summary: String,
    preview: Option<String>,
) -> Result<()> {
    match cfg.approval {
        Autonomy::Auto => Ok(()),
        Autonomy::Ask => ctx.confirm(summary, preview).await,
        Autonomy::Deny => bail!(
            "delegate {:?} is configured `approval = \"deny\"`: refusing to run it via \
             `delegate`/`ask_advise` (reconfigure it to \"ask\" or \"auto\" to use it)",
            cfg.name
        ),
    }
}

/// Cap the preview shown on an approval dialog so it stays readable no matter
/// how long the delegated task/context text is.
pub(crate) fn approval_preview(text: &str, label: &str) -> String {
    let cap = 3000usize;
    let body: String = text.chars().take(cap).collect();
    if text.chars().count() > cap {
        format!("{label}\n\n{body}\n…[preview truncated]")
    } else {
        format!("{label}\n\n{body}")
    }
}

/// Validate a `[[delegates]]` list and build one HTTP client per delegate.
/// Shared by `DelegateTool::new` and `AskAdviseTool::new` (advise.rs) so the
/// two tools always agree on which names are valid and how they are described.
/// Fails on duplicate/blank names or a delegate that cannot build a client.
pub(crate) fn build_targets(delegates: &[DelegateCfg]) -> Result<Vec<Target>> {
    let mut names: Vec<String> = Vec::new();
    let mut targets: Vec<Target> = Vec::with_capacity(delegates.len());
    for (i, cfg) in delegates.iter().enumerate() {
        if cfg.name.trim().is_empty() {
            bail!("delegates[{i}]: every delegate needs a `name`");
        }
        if cfg.name.trim() == AGENT_MODEL {
            bail!(
                "delegates[{i}]: {AGENT_MODEL:?} is reserved for the main agent model in plan \
                 steps; pick a different delegate name"
            );
        }
        if cfg.llm.model.trim().is_empty() {
            bail!(
                "delegates[{i}] ({}): every delegate needs a `model`",
                cfg.name
            );
        }
        if names.iter().any(|n| n == &cfg.name) {
            bail!("delegates: duplicate delegate name {:?}", cfg.name);
        }
        names.push(cfg.name.clone());
        let client = LlmClient::new(&cfg.llm)
            .with_context(|| format!("delegate {:?} failed to build client", cfg.name))?;
        targets.push(Target {
            cfg: cfg.clone(),
            client,
        });
    }
    Ok(targets)
}

impl DelegateTool {
    /// Whether `name` is a tool a delegate must never see (see
    /// [`DENIED_FOR_DELEGATES`]). Kept as a method so comrade-tui's registry
    /// builder and the tool itself share one source of truth.
    pub fn denied_for_delegates(name: &str) -> bool {
        DENIED_FOR_DELEGATES.contains(&name)
    }

    /// Build the delegate tool from the configured `[[delegates]]` entries.
    /// Returns `Ok(None)` when no delegates are configured (the tool is then
    /// not advertised at all).
    ///
    /// `tools` is the registry the delegate may call: pass a filtered view of
    /// the main registry (everything except [`DENIED_FOR_DELEGATES`]) so the
    /// delegate can actually write files, run tests and search while still
    /// being unable to commit.
    pub fn new(
        delegates: &[DelegateCfg],
        tools: ToolRegistry,
        limits: DelegateLimits,
    ) -> Result<Option<Self>> {
        if delegates.is_empty() {
            return Ok(None);
        }
        let targets = build_targets(delegates)?;
        let names: Vec<String> = targets.iter().map(|t| t.cfg.name.clone()).collect();

        let listing = delegates
            .iter()
            .map(cfg_line)
            .collect::<Vec<_>>()
            .join("\n");
        let description = format!(
            "\
Run one single, self-contained piece of work on another model while you keep \
planning and orchestrating. The delegate runs a real sub-agent WITH TOOLS: it \
gets the repository tools (read/search, write_file/apply_edit, run_tests, \
remember, web_search and more) minus git_commit, so it can do the job itself — \
write the file, run the tests, fix failures — instead of returning text you \
must apply by hand. Delegates cannot commit; only you can.

Delegating normally needs no human approval: the tool call runs directly and \
every tool call the delegate makes runs auto-approved, so a delegate works \
end-to-end on its own. EXCEPTION — some delegates are configured \
`approval = \"ask\"` (marked \"[human approval required before it runs]\" in \
the listing below): calling one pauses for human approval before it runs. \
Delegates configured `approval = \"deny\"` are refused entirely. Keep using \
`delegate` for well-bounded jobs, but expect the delegate to iterate on the \
repo with its own tools.

To execute one of your plan steps, pass `step` (the plan step id): the task and \
context then come from that step and `model` must match the step's model. \
Otherwise delegate ad-hoc work with `model` + `task` (+ optional `context`). \
Plan steps you will run yourself carry the reserved model \"self\" and cannot be \
delegated via `step`.

The plan shows who is working: delegating a step marks it in_progress with a \
`working: <model>` note, and fix rounds show up as `(fix N/5)`. Verification is \
joint: the delegate self-checks and closes with a `VERIFICATION:` line, but it \
runs under its own tools, so that line is never proof. After the delegate \
replies, run the step's verification yourself with your tools (e.g. \
run_tests); if it fails, re-delegate the SAME step with `feedback` set to the \
failure output so the delegate fixes it — up to 5 fix rounds per step. After 5 \
the tool refuses further fix requests and you must do the step yourself. \
Running a plan step through this tool is what entitles it to be marked done: \
update_plan and finish_plan refuse to close a step assigned a delegate model \
until the delegate tool has run it, so you cannot complete delegated work \
yourself. Pick up a step only once it is `ready` — ask its delegate first via \
ask_advise step = <id> (that marks the step `ready` when the delegate confirms \
the context suffices; enrich with set_step_context and re-ask until it does).

Several delegate calls issued in one message run in PARALLEL: split \
independent sub-tasks into separate calls and batch them together instead of \
delegating one at a time and waiting. Every call must be fully self-contained: \
delegates cannot see each other's work, so never make one depend on another's \
result, and never delegate the same plan step twice in one batch. Because \
delegates now write files and share the repo/session/undo, do not batch two \
delegates that will touch the same files — parallel delegates manage their \
own conflicts.

Configured delegates — pick the one whose description best fits the task:
{listing}"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "step": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Plan step id to execute. The task comes from the step's goal (+verification) and the context from the step's summarised context; the step's own model is used."
                },
                "model": {
                    "type": "string",
                    "enum": names,
                    "description": format!(
                        "Which configured delegate model should do the work (must match the step's model when `step` is given). Choose the delegate whose description best fits the task:\n{listing}"
                    )
                },
                "task": {
                    "type": "string",
                    "description": "The exact, self-contained job for the delegate, with all needed details (paths, code, identifiers, expected output). Mutually exclusive with `step`."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background material the delegate should consider (existing code, error logs, constraints). Mutually exclusive with `step`."
                },
                "feedback": {
                    "type": "string",
                    "description": "Verification failure output from the parent for a delegate that previously attempted `step`: the delegate must fix its deliverable until it passes. Counts as one fix round (max 5 per step, tracked on the step's note). The feedback must contain an actionable plan so the delegate model knows what is wrong and what do to to fix it."
                }
            },
            "oneOf": [
                { "required": ["step"] },
                { "required": ["model", "task"] }
            ],
            "additionalProperties": false
        });

        Ok(Some(Self {
            spec: ToolSpec {
                name: TOOL_NAME.into(),
                description,
                json_schema: schema,
            },
            targets,
            tools,
            limits,
        }))
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        let step_id = args.get("step").and_then(Value::as_u64);
        let model_arg = args
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let feedback = args
            .get("feedback")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !feedback.is_empty() && step_id.is_none() {
            bail!(
                "`feedback` is only valid with `step`: it reports a failed verification to a \
                 delegate that previously attempted that plan step"
            );
        }

        // When a plan step is delegated, remember its previous status so the
        // plan can be restored if the delegate call itself fails.
        let mut delegated_step: Option<(u64, PlanStatus)> = None;
        // The `working: <model>` note to apply once the run is approved. Kept
        // separate so the plan is only touched after the approval gate passes.
        let mut delegated_note: Option<String> = None;

        // Resolve what to run: either an explicit ad-hoc task, or one plan step
        // (whose goal/verification/context/model all come from the plan).
        let (task, context, model) = match step_id {
            Some(id) => {
                for (key, label) in [("task", "task"), ("context", "context")] {
                    let present = args
                        .get(key)
                        .map(|v| v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(true))
                        .unwrap_or(false);
                    if present {
                        bail!(
                            "cannot pass `{label}` together with `step`: the {label} comes from the plan step"
                        );
                    }
                }
                let found = ctx
                    .session
                    .plan()
                    .into_iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| anyhow::anyhow!("no plan step with id {id}"))?;
                if !model_arg.is_empty() && model_arg != found.model {
                    bail!(
                        "`model` {model_arg:?} does not match the model assigned to plan step {id} \
                         ({:?})",
                        found.model
                    );
                }
                if found.model.trim().is_empty() {
                    bail!(
                        "plan step {id} has no delegate model assigned; it runs on the main model"
                    );
                }
                if found.model.trim() == AGENT_MODEL {
                    bail!(
                        "plan step {id} is assigned to the main agent model ({AGENT_MODEL:?}), not \
                         a delegate — do the step yourself instead of delegating it"
                    );
                }
                let goal = found.goal.trim();
                let verify = found.verification.trim();
                let task = if verify.is_empty() {
                    goal.to_string()
                } else {
                    format!("{goal}\n\nVerify your work: {verify}")
                };

                // Compute the `working: <model>` note the plan will show once
                // the run is approved, and count the fix rounds. A step gets one
                // attempt from the delegate, then up to MAX_FIX_ROUNDS repairs
                // requested via `feedback`; past that the parent must take over.
                let fixes = fix_rounds_in_note(found.note.as_deref(), &found.model);
                if fixes >= MAX_FIX_ROUNDS {
                    bail!(
                        "plan step {id} already had {MAX_FIX_ROUNDS} failed fix round(s) with \
                         delegate {:?}; stop delegating and do the step yourself",
                        found.model
                    );
                }
                let note = if feedback.is_empty() {
                    if fixes == 0 {
                        format!("working: {}", found.model)
                    } else {
                        // a bare re-run keeps the fix count intact
                        format!("working: {} (fix {fixes}/{MAX_FIX_ROUNDS})", found.model)
                    }
                } else {
                    format!(
                        "working: {} (fix {}/{MAX_FIX_ROUNDS})",
                        found.model,
                        fixes + 1
                    )
                };
                delegated_step = Some((id, found.status));
                // The plan is marked working after the approval gate (below):
                // a denied run must leave the step in its previous status.
                delegated_note = Some(note);

                (task, found.context, found.model)
            }
            None => {
                let task = args
                    .get("task")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if task.trim().is_empty() {
                    bail!("`task` must not be empty (or pass `step` to delegate a plan step)");
                }
                let context = args
                    .get("context")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                (task, context, model_arg)
            }
        };

        let Some(target) = self.targets.iter().find(|t| t.cfg.name == model) else {
            let listed = self
                .targets
                .iter()
                .map(|t| cfg_line(&t.cfg))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("unknown delegate model {model:?}. Configured delegates:\n{listed}");
        };

        // Per-delegate approval policy. `approval = "ask"` pauses for the human
        // before this delegate runs (cheap/trusted delegates default to "auto"
        // and keep running directly); `approval = "deny"` refuses outright.
        let scope = match step_id {
            Some(id) => format!("Delegate plan step {id} to delegate {model}?"),
            None => format!("Delegate a task to delegate {model}?"),
        };
        let preview = match step_id {
            Some(_) => approval_preview(&task, "Plan step to delegate:"),
            None => approval_preview(&task, "Task to delegate:"),
        };
        enforce_approval(&target.cfg, ctx, scope, Some(preview)).await?;

        // Reflect the delegation in the plan only after approval passed, so a
        // denial or a deny-gated model never leaves the step half-claimed.
        if let (Some(id), Some(note)) = (step_id, delegated_note.as_deref()) {
            ctx.session.update_plan(
                PlanTarget::Id(id),
                PlanStatus::InProgress,
                Some(note.to_string()),
            );
        }

        let user_prompt = if feedback.is_empty() {
            if context.trim().is_empty() {
                format!("Task:\n{task}")
            } else {
                format!("Context:\n{context}\n\nTask:\n{task}")
            }
        } else {
            // A fix round: the parent ran the verification, it failed, and the
            // delegate must repair its earlier deliverable.
            let header = if context.trim().is_empty() {
                format!("Task:\n{task}")
            } else {
                format!("Context:\n{context}\n\nTask:\n{task}")
            };
            format!(
                "{header}\n\nYour previous attempt did not pass the parent's verification of \
                 this step. The parent ran the verification and observed:\n{feedback}\n\n\
                 Fix the deliverable so it passes, and reply with the complete corrected version \
                 followed by the VERIFICATION: line."
            )
        };
        let display = target.cfg.llm.display();
        let native = target.cfg.llm.protocol.native_enabled();
        let system =
            delegate_system_prompt(&ctx.project_root.to_string_lossy(), &self.tools, native);
        let reply = run_delegate_subagent(
            &target.client,
            &self.tools,
            ctx,
            system,
            user_prompt,
            &target.cfg.name,
            native,
            &self.limits,
            DELEGATE_READ_NUDGE,
        )
        .await
        .with_context(|| format!("delegate {model} ({display}) failed"));
        let reply = match reply {
            Ok(reply) => reply,
            Err(err) => {
                // The delegate never finished: pull the step back from
                // "working" so the plan does not claim a delegate is on the job.
                if let Some((id, previous)) = delegated_step {
                    ctx.session.update_plan(
                        PlanTarget::Id(id),
                        previous,
                        Some(format!("delegate {model} failed to run")),
                    );
                }
                return Err(err);
            }
        };

        // The delegate produced a reply: record that this plan step really ran
        // on a delegate, so the root cannot later complete it "itself" without
        // delegating (enforced by update_plan/finish_plan).
        if let Some((id, _)) = delegated_step {
            ctx.session.mark_step_delegated(id);
        }

        Ok(format!("delegate {model} ({display}) replied:\n{reply}"))
    }
}

/// Render any sub-agent system prompt whose static prose lives in a markdown
/// file under `crates/comrade-core/prompts/` (embedded with include_str!). The
/// dynamic pieces are substituted at runtime: `{tool_lines}` lists the scoped
/// registry the sub-agent may call (one compact `- name — description` line
/// each), `{protocol}` describes how to issue tool calls for the delegate's
/// protocol (native function calling vs ReAct text), and `{project_root}` is
/// the working directory. Order matters: `{tool_lines}` and `{protocol}` are
/// filled first so tool descriptions that contain braces cannot disturb later
/// substitutions.
pub(crate) fn render_subagent_system(
    body: &str,
    project_root: &str,
    tools: &ToolRegistry,
    native: bool,
) -> String {
    let mut tool_lines = String::new();
    for tool in tools.iter() {
        let desc: String = tool
            .spec()
            .description
            .split('\n')
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(140)
            .collect();
        tool_lines.push_str(&format!("- {} — {desc}\n", tool.spec().name));
    }
    let protocol = if native {
        "You call tools natively (function calling). When the task is done and \
         verified, stop calling tools and reply with your final answer."
    } else {
        "Think and act step by step, one tool per turn:\n\
         Thought: <what you are doing and why>\n\
         Tool: <tool_name>\n\
         Args: <valid strict JSON object>\n\
         After each tool call you receive an Observation; continue until the task \
         is done, then reply with your final answer."
    };
    body.replace("{protocol}", protocol)
        .replace("{tool_lines}", &tool_lines)
        .replace("{project_root}", project_root)
}

/// System prompt for a delegated developer sub-agent that has real tools.
/// Unlike the old text-only delegates it must know what it can call (and that
/// it must NOT try to commit), so the prompt lists the delegate-scoped tool
/// registry and explains the working protocol.
fn delegate_system_prompt(project_root: &str, tools: &ToolRegistry, native: bool) -> String {
    render_subagent_system(
        include_str!("../prompts/delegate-system.md"),
        project_root,
        tools,
        native,
    )
}

/// No-progress guard for the delegate sub-agent loop, mirroring the main agent
/// loop's `LoopTracker` semantics. `tracker.check(sig)` returns a count only
/// when the exact same call ran before with no state change since; such repeats
/// are refused (the returned message tells the model to change tack). After
/// [`MAX_LOOP_REFUSALS`] identical repeats the whole run aborts with an error
/// instead of burning the delegate's iteration budget on the same argument.
fn refuse_repeat(tracker: &mut LoopTracker, sig: &str) -> Result<Option<String>> {
    match tracker.check(sig) {
        None => Ok(None),
        Some(count) if count >= MAX_LOOP_REFUSALS => bail!(
            "delegate repeated identical action `{sig}` {count}x without any state change; \
             aborting the sub-agent loop to avoid repeating forever"
        ),
        Some(_) => {
            let tool = sig.split_whitespace().next().unwrap_or(sig);
            Ok(Some(loop_refusal(tool)))
        }
    }
}

/// Read guard for the delegate sub-agent loop, mirroring the main loop's
/// `allow_read_step` (agent.rs): once the delegate has done twenty consecutive
/// read-only calls with no state change in between, the next read is refused
/// and the delegate is nudged to make progress instead of keep reading. Any
/// non-read action resets the counter. Returns the refusal message when the
/// read must not run, `None` when it may.
///
/// `nudge` is the wording of the refusal and may contain a `{count}`
/// placeholder for the number of consecutive reads; each caller passes wording
/// that fits its sub-agent (a working delegate is told to implement, an
/// advisor consulted via `ask_advise` is told to answer).
fn refuse_reading(name: &str, consecutive_reads: &mut usize, nudge: &str) -> Option<String> {
    if allow_read_step(name, consecutive_reads) {
        return None;
    }
    Some(nudge.replace("{count}", &consecutive_reads.to_string()))
}

/// Wording of the read-guard nudge for a working `delegate` sub-agent: unlike
/// the main loop there is no `update_plan` tool for the delegate to call
/// (session/plan tools are denied), so the nudge points at implementing and
/// answering instead.
const DELEGATE_READ_NUDGE: &str = "You have performed {count} reads in a row with no changes. \
     You have enough context - implement now (write or edit a file) and verify your work, then \
     reply with your final answer. Do not keep reading.";

/// Run one delegate as a tool-using sub-agent until it produces a final answer.
/// Mirrors the main agent loop but for the delegate's own client, scoped tool
/// registry and limits: native tool calling when the delegate protocol allows
/// it, ReAct-style text calls otherwise (both advertised/parsed exactly like
/// the main loop so any model works). Every nested tool call runs against a
/// clone of the caller context with `auto_approve` forced on, so the delegate
/// works end-to-end without pausing for confirmations (the `delegate` and
/// `ask_advise` tools themselves are not approval-gated).
///
/// `read_nudge` customises the read-guard refusal for the kind of sub-agent
/// being run (see [`refuse_reading`]).
pub(crate) async fn run_delegate_subagent(
    client: &LlmClient,
    tools: &ToolRegistry,
    parent_ctx: &ToolContext,
    system: String,
    user_prompt: String,
    author: &str,
    native: bool,
    limits: &DelegateLimits,
    read_nudge: &str,
) -> Result<String> {
    // The delegate inherits the session/user/undo of the parent but runs
    // auto-approved with a fresh approval slot, so its nested tool calls never
    // pause for a human confirmation.
    let mut dctx = parent_ctx.clone();
    dctx.auto_approve = true;
    dctx.approval = Arc::new(Mutex::new(None));

    let mut ctxm =
        ContextManager::with_system(system, limits.budget_tokens, limits.max_tool_output_chars);
    ctxm.push(ChatMessage::new(Role::User, user_prompt));

    // No-progress guard: the identical tool call repeated with no state change
    // in between is refused, and the run aborts after a few such refusals so a
    // stuck delegate cannot loop forever on the same argument.
    let mut tracker = LoopTracker::default();
    // Read guard: consecutive read-only calls since the last state change; the
    // delegate is nudged to implement once it has read too long without acting.
    let mut consecutive_reads = 0usize;
    // The run's cancel token, set by the main agent loop (agent.rs). When the
    // human interrupts the parent, a delegate stuck waiting on its model
    // request must abort instead of holding the whole run at "working".
    let stop = parent_ctx.stop.clone();

    for _ in 0..limits.max_iterations {
        if let Some(stop) = &stop {
            if stop.is_cancelled() {
                bail!("delegate interrupted: the run was cancelled");
            }
        }
        // A steer typed while the delegate owned the loop reaches the delegate's
        // own conversation at its next rest point (drained from the shared bus
        // cloned into `dctx`).
        crate::agent::drain_steer(dctx.steer.as_ref(), &mut ctxm).await;
        ctxm.enforce_budget();
        let specs: Option<Vec<comrade_tool::ToolSpec>> = if native {
            let specs: Vec<_> = tools.iter().map(|t| t.spec().clone()).collect();
            if specs.is_empty() { None } else { Some(specs) }
        } else {
            None
        };

        let turn = match &stop {
            Some(stop) => {
                tokio::select! {
                    r = client.chat_turn_once(ctxm.messages(), specs.as_deref()) => r,
                    _ = stop.cancelled() => {
                        bail!("delegate interrupted: the run was cancelled while waiting for the model")
                    }
                }
            }
            None => {
                client
                    .chat_turn_once(ctxm.messages(), specs.as_deref())
                    .await
            }
        }?;

        // Native tool calls: dispatch all of them like the main loop does.
        if !turn.tool_calls.is_empty() {
            let calls: Vec<ToolCallMsg> = turn
                .tool_calls
                .iter()
                .map(|tc| ToolCallMsg {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                })
                .collect();
            ctxm.push(ChatMessage::assistant_with_calls(turn.content, calls));
            for tc in turn.tool_calls {
                let args = serde_json::from_str(&tc.arguments).unwrap_or_default();
                let args_pretty = serde_json::to_string(&args).unwrap_or_default();
                let sig = format!("{} {}", tc.name, args_pretty);
                if let Some(msg) = refuse_reading(&tc.name, &mut consecutive_reads, read_nudge) {
                    // Too many reads in a row: nudge to implement. Still answer
                    // the call with a tool result so history stays API-valid.
                    let clamped = ctxm.truncate_observation(&msg);
                    ctxm.push(ChatMessage::tool_result(tc.id, clamped));
                    continue;
                }
                if let Some(msg) = refuse_repeat(&mut tracker, &sig)? {
                    // Refused as a no-progress repeat: still answer the call
                    // with a tool result so the history stays API-valid.
                    let clamped = ctxm.truncate_observation(&msg);
                    ctxm.push(ChatMessage::tool_result(tc.id, clamped));
                    continue;
                }
                let output = match tools.get(&tc.name) {
                    Some(tool) => {
                        dctx.events.tool_call(author, &tc.name, &args_pretty).await;
                        let r = match tool.invoke(&dctx, args).await {
                            Ok(out) => (out, true),
                            Err(e) => (format!("ERROR: {e:#}"), false),
                        };
                        dctx.events.tool_result(author, &tc.name, &r.0, r.1).await;
                        r.0
                    }
                    None => format!("unknown tool {:?}; choose from the listed tools", tc.name),
                };
                let clamped = ctxm.truncate_observation(&output);
                ctxm.push(ChatMessage::tool_result(tc.id, clamped));
                tracker.record(&tc.name, sig);
            }
            continue;
        }

        // Plain text: final answer, or (native fallback / react mode) a text
        // Tool:/Args: call.
        let response = turn.content;
        if response.trim().is_empty() {
            anyhow::bail!("delegate returned an empty response");
        }
        ctxm.push(ChatMessage::new(Role::Assistant, response.clone()));
        let turn_p = match parse_turn(&response) {
            Ok(t) => t,
            Err(e) => {
                let msg = format!(
                    "Your previous message could not be parsed ({e:#}). Resend the tool call \
                     with Args as VALID strict JSON: quote every key and string value."
                );
                let obs = ctxm.truncate_observation(&msg);
                ctxm.push(ChatMessage::new(
                    Role::User,
                    render_observation("model_output", &obs),
                ));
                continue;
            }
        };
        let Some(tool_call) = turn_p.tool_call else {
            return Ok(turn_p.final_text);
        };
        let args_pretty = serde_json::to_string(&tool_call.args).unwrap_or_default();
        let sig = format!("{} {}", tool_call.name, args_pretty);
        if let Some(msg) = refuse_reading(&tool_call.name, &mut consecutive_reads, read_nudge) {
            let obs = ctxm.truncate_observation(&msg);
            ctxm.push(ChatMessage::new(
                Role::User,
                render_observation(&tool_call.name, &obs),
            ));
            continue;
        }
        if let Some(msg) = refuse_repeat(&mut tracker, &sig)? {
            let obs = ctxm.truncate_observation(&msg);
            ctxm.push(ChatMessage::new(
                Role::User,
                render_observation(&tool_call.name, &obs),
            ));
            continue;
        }
        let Some(tool) = tools.get(&tool_call.name) else {
            let msg = format!(
                "unknown tool {:?}; choose from the listed tools",
                tool_call.name
            );
            let obs = ctxm.truncate_observation(&msg);
            ctxm.push(ChatMessage::new(
                Role::User,
                render_observation(&tool_call.name, &obs),
            ));
            continue;
        };
        dctx.events
            .tool_call(author, &tool_call.name, &args_pretty)
            .await;
        let (output, ok) = match tool.invoke(&dctx, tool_call.args).await {
            Ok(out) => (out, true),
            Err(e) => (format!("ERROR: {e:#}"), false),
        };
        dctx.events
            .tool_result(author, &tool_call.name, &output, ok)
            .await;
        let clamped = ctxm.truncate_observation(&output);
        ctxm.push(ChatMessage::new(
            Role::User,
            render_observation(&tool_call.name, &clamped),
        ));
        tracker.record(&tool_call.name, sig);
        ctxm.note_tool_done(&tool_call.name, turn_p.thought.as_deref());
    }

    anyhow::bail!(
        "delegate reached max_iterations ({}) without a final answer",
        limits.max_iterations
    );
}

/// How many failed fix rounds a plan step has already been through, read from
/// the note this tool writes while a delegate is working (`working: <model>`
/// for the first attempt, `working: <model> (fix N/5)` for each repair).
fn fix_rounds_in_note(note: Option<&str>, model: &str) -> u64 {
    let Some(note) = note else {
        return 0;
    };
    let Some(rest) = note.strip_prefix(&format!("working: {model}")) else {
        return 0;
    };
    let Some(start) = rest.find("(fix ") else {
        return 0;
    };
    rest[start + "(fix ".len()..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use comrade_tool::PlanStepDraft;
    use comrade_tool::ToolContext;
    use comrade_tool::tool::{UserIo, UserPrompt, UserReply};

    use super::*;
    use crate::MemoryUndo;
    use crate::config::{Autonomy, Config, DelegateCfg, LlmCfg, Protocol};
    use crate::session::AgentSession;

    /// The delegate tool never touches the session/user, so a no-op IO double
    /// is all the tests need.
    struct NoopIo;
    #[async_trait]
    impl UserIo for NoopIo {
        async fn ask(&self, _prompt: UserPrompt) -> Result<UserReply> {
            Ok(UserReply::Answer(String::new()))
        }
    }

    fn test_ctx() -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        ToolContext {
            project_root: "/tmp/x".into(),
            cwd: "/tmp/x".into(),
            session: Arc::new(AgentSession::new(tx)).as_control(),
            user: Arc::new(NoopIo),
            undo: Arc::new(MemoryUndo::new("/tmp/x".into())),
            auto_approve: true,
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            stop: None,
        }
    }

    /// A context whose confirmations actually reach the UserIo (auto_approve
    /// false), for exercising the per-delegate approval gate.
    fn ask_ctx(user: Arc<dyn UserIo>) -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        ToolContext {
            project_root: "/tmp/x".into(),
            cwd: "/tmp/x".into(),
            session: Arc::new(AgentSession::new(tx)).as_control(),
            user,
            undo: Arc::new(MemoryUndo::new("/tmp/x".into())),
            auto_approve: false,
            approval: Default::default(),
            events: Arc::new(comrade_tool::NoopEvents),
            steer: None,
            stop: None,
        }
    }

    /// Records every confirm prompt's title and answers each one with `answer`
    /// ("yes" approves; anything else denies).
    struct RecordingIo {
        titles: Arc<Mutex<Vec<String>>>,
        answer: String,
    }
    #[async_trait]
    impl UserIo for RecordingIo {
        async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
            if let UserPrompt::Confirm { title, .. } = prompt {
                self.titles.lock().unwrap().push(title);
            }
            Ok(UserReply::Answer(self.answer.clone()))
        }
    }

    /// Fails the test if the context ever asks the human: used to prove that
    /// auto/ungated delegates run without an approval prompt.
    struct MustNotAsk;
    #[async_trait]
    impl UserIo for MustNotAsk {
        async fn ask(&self, _prompt: UserPrompt) -> Result<UserReply> {
            panic!("auto/ungated delegate must not prompt for approval");
        }
    }

    /// Build a delegate tool with an empty registry (no repository tools) so
    /// tests exercise the sub-agent loop without touching the real filesystem.
    fn mk_delegate(delegates: &[DelegateCfg]) -> Result<Option<DelegateTool>> {
        DelegateTool::new(delegates, ToolRegistry::new(), DelegateLimits::default())
    }

    /// Spawn a fake OpenAI-compatible `/v1/chat/completions` server that
    /// answers every request with `content`. Returns its base URL.
    fn fake_chat_server(content: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let mut used = 0usize;
            loop {
                let n = stream.read(&mut buf[used..]).unwrap();
                if n == 0 {
                    break;
                }
                used += n;
                if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = format!(
                "{{\"choices\":[{{\"message\":{{\"content\":\"{}\"}}}}]}}",
                content.replace('"', "\\\"")
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    /// Like [`fake_chat_server`], but also ships the raw request body to a
    /// channel so tests can assert what the delegate was actually asked.
    fn request_spy() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            let mut tmp = [0u8; 2048];
            let mut body_len: Option<usize> = None;
            let mut header_end: Option<usize> = None;
            // Read until the whole body (per Content-Length) is buffered.
            while body_len.map_or(true, |len| header_end.unwrap_or(0) + 4 + len > data.len()) {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&tmp[..n]);
                if header_end.is_none() {
                    if let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(p);
                        let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
                        body_len = head.lines().find_map(|l| {
                            l.trim()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        });
                    }
                }
            }
            let body = match (header_end, body_len) {
                (Some(he), Some(len)) => {
                    let start = he + 4;
                    let end = (start + len).min(data.len());
                    String::from_utf8_lossy(&data[start..end]).into_owned()
                }
                _ => String::new(),
            };
            let _ = tx.send(body);
            let resp_body = "{\"choices\":[{\"message\":{\"content\":\"ok\"}}]}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });
        (format!("http://127.0.0.1:{port}/v1"), rx)
    }

    fn delegate(name: &str, base_url: &str) -> DelegateCfg {
        DelegateCfg {
            name: name.into(),
            description: format!("{name} test delegate"),
            approval: Autonomy::Auto,
            llm: LlmCfg {
                base_url: base_url.into(),
                model: "delegate-model".into(),
                ..LlmCfg::default()
            },
        }
    }

    /// A delegate stuck waiting for its model must abort when the run's cancel
    /// token fires, instead of holding the whole run at "working" (the user
    /// cannot interrupt it — Esc is dead until the request returns).
    #[tokio::test]
    async fn cancelled_run_aborts_a_delegate_stuck_waiting_for_the_model() {
        // Server accepts the request, reads it, then never answers: the
        // delegate's model request stays in flight until cancelled.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let mut used = 0usize;
            loop {
                let n = stream.read(&mut buf[used..]).unwrap();
                if n == 0 {
                    break;
                }
                used += n;
                if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            // Hold the connection open without replying.
            std::thread::sleep(std::time::Duration::from_secs(60));
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        let mut d = delegate("silent", &base);
        // Short client timeout so a regression fails in seconds, not in 600.
        d.llm.timeout_secs = 5;
        let client = LlmClient::new(&d.llm).unwrap();

        let mut ctx = test_ctx();
        let stop = tokio_util::sync::CancellationToken::new();
        ctx.stop = Some(stop.clone());
        // Cancel shortly after the request is in flight (300ms in).
        let stopper = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                stop.cancel();
            })
        };

        let started = std::time::Instant::now();
        let res = run_delegate_subagent(
            &client,
            &ToolRegistry::new(),
            &ctx,
            "You are a test delegate.".into(),
            "do the thing".into(),
            "silent",
            false,
            &DelegateLimits::default(),
            DELEGATE_READ_NUDGE,
        )
        .await;
        stopper.join().unwrap();

        let err = res
            .expect_err("a cancelled delegate run must fail, not hang")
            .to_string();
        assert!(err.contains("interrupted"), "unexpected error: {err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "delegate took {:?} to abort — cancel is not reaching it",
            started.elapsed()
        );
    }

    #[test]
    fn empty_delegate_list_yields_no_tool() {
        let tool = mk_delegate(&[]).unwrap();
        assert!(tool.is_none());
    }

    #[test]
    fn duplicate_or_blank_names_are_rejected() {
        let dup = vec![delegate("a", "http://x/v1"), delegate("a", "http://x/v1")];
        assert!(mk_delegate(&dup).is_err());

        let blank = vec![DelegateCfg {
            name: "  ".into(),
            ..delegate("ignored", "http://x/v1")
        }];
        assert!(mk_delegate(&blank).is_err());

        let no_model = vec![DelegateCfg {
            llm: LlmCfg {
                model: "".into(),
                ..LlmCfg::default()
            },
            ..delegate("m", "http://x/v1")
        }];
        assert!(mk_delegate(&no_model).is_err());
    }

    #[test]
    fn schema_advertises_models_and_required_args() {
        let cfg = Config {
            delegates: vec![
                delegate("groq-fast", "http://x/v1"),
                delegate("mistral", "http://x/v1"),
            ],
            ..Config::default()
        };
        let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
        assert_eq!(tool.spec().name, "delegate");
        let schema = &tool.spec().json_schema;
        let models = schema["properties"]["model"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(models, vec!["groq-fast", "mistral"]);
        // The tool description and the `model` argument both advertise the
        // delegates with their descriptions, so the tech lead picks a delegate
        // by what the blurb says fits the task.
        let spec_desc = &tool.spec().description;
        assert!(spec_desc.contains("Configured delegates"), "{spec_desc}");
        assert!(
            spec_desc.contains("groq-fast: groq-fast test delegate"),
            "{spec_desc}"
        );
        assert!(
            spec_desc.contains("mistral: mistral test delegate"),
            "{spec_desc}"
        );
        let model_desc = schema["properties"]["model"]["description"]
            .as_str()
            .unwrap();
        assert!(
            model_desc.contains("mistral: mistral test delegate"),
            "{model_desc}"
        );
        assert!(schema["properties"]["task"].is_object());
        assert!(schema["properties"]["step"].is_object());
        // `step` alone, or `model` + `task` (oneOf), are the two call shapes.
        let one_of = schema["oneOf"].as_array().unwrap();
        let requires = |needle: &str| {
            one_of.iter().any(|o| {
                o["required"]
                    .as_array()
                    .map_or(false, |r| r.iter().any(|v| v.as_str() == Some(needle)))
            })
        };
        assert!(requires("step"));
        assert!(requires("model") && requires("task"));
    }

    #[test]
    fn blank_blurbs_degrade_to_bare_name_lines() {
        let cfg = Config {
            delegates: vec![
                delegate("with-blurb", "http://x/v1"),
                DelegateCfg {
                    description: "   ".into(),
                    ..delegate("bare", "http://x/v1")
                },
            ],
            ..Config::default()
        };
        let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
        let desc = tool.spec().description.clone();
        assert!(
            desc.contains("with-blurb: with-blurb test delegate"),
            "{desc}"
        );
        assert!(desc.trim_end().ends_with("  - bare"), "{desc}");
    }

    #[test]
    fn delegate_system_prompt_substitutes_tokens_from_markdown() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(StubTool {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        for native in [false, true] {
            let prompt = delegate_system_prompt("/repo/root", &reg, native);
            assert!(prompt.starts_with("You are a developer sub-agent on Comrade's team."));
            assert!(
                prompt.contains("Working directory: /repo/root."),
                "{prompt}"
            );
            assert!(prompt.contains("Available tools:"), "{prompt}");
            assert!(
                prompt.contains("- write_file — stub write_file"),
                "{prompt}"
            );
            assert!(
                !prompt.contains("{project_root}")
                    && !prompt.contains("{tool_lines}")
                    && !prompt.contains("{protocol}"),
                "unsubstituted token in:\n{prompt}"
            );
            assert!(
                prompt.trim_end().ends_with("whether it passes."),
                "{prompt}"
            );
        }
    }

    #[tokio::test]
    async fn invoke_queries_the_chosen_delegate() {
        let base = fake_chat_server("here is the finished function");
        let cfg = Config {
            delegates: vec![
                delegate("cheap", &base),
                delegate("other", "http://127.0.0.1:1/v1"),
            ],
            ..Config::default()
        };
        let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(
                &ctx,
                json!({
                    "model": "cheap",
                    "task": "Write a double() function.",
                    "context": "Rust, must be pure."
                }),
            )
            .await
            .unwrap();
        assert!(out.contains("delegate cheap"));
        assert!(out.contains("here is the finished function"));
    }

    #[tokio::test]
    async fn invoke_rejects_unknown_model_and_empty_task() {
        let base = fake_chat_server("ignored");
        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        let err = tool
            .invoke(&ctx, json!({"model": "nope", "task": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown delegate model"));
        // The error repeats the pick list (name + description) so a wrong
        // guess teaches the tech lead which delegate fits next time.
        assert!(
            err.to_string().contains("cheap: cheap test delegate"),
            "{err}"
        );

        let err = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "   "}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("task"));
    }

    #[tokio::test]
    async fn plan_step_delegation_sends_goal_verification_and_context() {
        let (base, spy) = request_spy();
        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write a double() function.".into(),
            verification: "cargo test double passes".into(),
            model: "cheap".into(),
            context: "Pure Rust, no dependencies.".into(),
        }]);

        let out = tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        assert!(out.contains("delegate cheap"), "{out}");

        let body = spy.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(body.contains("Write a double() function."), "{body}");
        assert!(
            body.contains("Verify your work: cargo test double passes"),
            "{body}"
        );
        assert!(body.contains("Pure Rust, no dependencies."), "{body}");
    }

    #[tokio::test]
    async fn self_assigned_plan_steps_cannot_be_delegated() {
        let (base, _spy) = request_spy();
        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "do it myself".into(),
            verification: "".into(),
            model: comrade_tool::AGENT_MODEL.into(),
            context: "".into(),
        }]);
        let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
        assert!(err.to_string().contains("main agent model"), "{err}");
    }

    #[tokio::test]
    async fn plan_step_delegation_validates_args() {
        let (base, _spy) = request_spy();
        let cfg = Config {
            delegates: vec![
                delegate("cheap", &base),
                delegate("other", "http://127.0.0.1:1/v1"),
            ],
            ..Config::default()
        };
        let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        ctx.session.set_plan(vec![
            PlanStepDraft {
                goal: "run on cheap".into(),
                verification: "".into(),
                model: "cheap".into(),
                context: "".into(),
            },
            PlanStepDraft {
                goal: "run on main".into(),
                verification: "".into(),
                model: "".into(),
                context: "".into(),
            },
        ]);

        // `model` must match the step's assigned model.
        let err = tool
            .invoke(&ctx, json!({"step": 1, "model": "other"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");

        // A step without an assigned delegate cannot be delegated.
        let err = tool.invoke(&ctx, json!({"step": 2})).await.unwrap_err();
        assert!(
            err.to_string().contains("no delegate model assigned"),
            "{err}"
        );

        // `step` is exclusive with an explicit `task`.
        let err = tool
            .invoke(&ctx, json!({"step": 1, "task": "nope"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot pass `task`"), "{err}");

        // Unknown step id.
        let err = tool.invoke(&ctx, json!({"step": 99})).await.unwrap_err();
        assert!(err.to_string().contains("no plan step with id 99"), "{err}");
    }

    #[tokio::test]
    async fn plan_step_delegation_marks_step_in_progress_with_working_note() {
        let (base, _spy) = request_spy();
        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write double()".into(),
            verification: "cargo test double passes".into(),
            model: "cheap".into(),
            context: "".into(),
        }]);

        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::InProgress);
        let note = step.note.as_deref().unwrap_or_default();
        assert!(note.contains("working: cheap"), "{note}");
        assert!(!note.contains("fix"), "{note}");
    }

    #[tokio::test]
    async fn ask_gated_delegate_pauses_for_approval_then_runs() {
        let (base, _spy) = request_spy();
        let mut expensive = delegate("expensive", &base);
        expensive.approval = Autonomy::Ask;
        let tool = mk_delegate(&[expensive]).unwrap().unwrap();
        let titles = Arc::new(Mutex::new(Vec::new()));
        let ctx = ask_ctx(Arc::new(RecordingIo {
            titles: titles.clone(),
            answer: "yes".into(),
        }));

        let out = tool
            .invoke(&ctx, json!({"model": "expensive", "task": "add a test"}))
            .await
            .unwrap();
        let held = titles.lock().unwrap();
        assert_eq!(held.len(), 1, "exactly one approval prompt expected");
        assert!(
            held[0].contains("delegate expensive"),
            "confirm title should name the delegate: {:?}",
            held[0]
        );
        drop(held);
        assert!(out.contains("delegate expensive"), "{out}");
    }

    #[tokio::test]
    async fn denied_approval_aborts_delegate_and_leaves_plan_untouched() {
        let (base, _spy) = request_spy();
        let mut expensive = delegate("expensive", &base);
        expensive.approval = Autonomy::Ask;
        let tool = mk_delegate(&[expensive]).unwrap().unwrap();
        let titles = Arc::new(Mutex::new(Vec::new()));
        let ctx = ask_ctx(Arc::new(RecordingIo {
            titles: titles.clone(),
            answer: "".into(), // not affirmative -> denied
        }));
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write double()".into(),
            verification: "".into(),
            model: "expensive".into(),
            context: "".into(),
        }]);

        let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
        assert!(err.to_string().contains("user denied"), "{err}");
        // The step was never claimed: it stays pending with no working note.
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::Pending, "{step:?}");
        assert!(step.note.is_none(), "{step:?}");
        assert_eq!(titles.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn approved_plan_step_delegation_still_marks_working_after_the_gate() {
        let (base, _spy) = request_spy();
        let mut expensive = delegate("expensive", &base);
        expensive.approval = Autonomy::Ask;
        let tool = mk_delegate(&[expensive]).unwrap().unwrap();
        let titles = Arc::new(Mutex::new(Vec::new()));
        let ctx = ask_ctx(Arc::new(RecordingIo {
            titles: titles.clone(),
            answer: "yes".into(),
        }));
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write double()".into(),
            verification: "".into(),
            model: "expensive".into(),
            context: "".into(),
        }]);

        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let held = titles.lock().unwrap();
        assert!(
            held[0].contains("plan step 1") && held[0].contains("expensive"),
            "{:?}",
            held[0]
        );
        drop(held);
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::InProgress);
        assert_eq!(step.note.as_deref(), Some("working: expensive"));
    }

    #[tokio::test]
    async fn deny_gated_delegate_is_refused_without_running() {
        // No server is spawned: a deny-gated delegate must never be contacted.
        let mut guarded = delegate("guarded", "http://127.0.0.1:1/v1");
        guarded.approval = Autonomy::Deny;
        let tool = mk_delegate(&[guarded]).unwrap().unwrap();
        let ctx = test_ctx();

        let err = tool
            .invoke(&ctx, json!({"model": "guarded", "task": "any task"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("approval = \"deny\"") && err.to_string().contains("guarded"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn auto_gated_default_delegate_runs_without_any_prompt() {
        let (base, _spy) = request_spy();
        let tool = mk_delegate(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = ask_ctx(Arc::new(MustNotAsk));
        let out = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "add a test"}))
            .await
            .unwrap();
        assert!(out.contains("delegate cheap"), "{out}");
    }

    fn cheap_tool(base_url: &str) -> DelegateTool {
        let cfg = Config {
            delegates: vec![delegate("cheap", base_url)],
            ..Config::default()
        };
        mk_delegate(&cfg.delegates).unwrap().unwrap()
    }

    #[tokio::test]
    async fn fix_feedback_is_forwarded_and_rounds_are_counted_on_the_step() {
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write double()".into(),
            verification: "cargo test double passes".into(),
            model: "cheap".into(),
            context: "".into(),
        }]);
        let note = || ctx.session.plan()[0].note.clone();

        // first attempt: no feedback, note carries no round yet
        let (base1, spy1) = request_spy();
        let tool = cheap_tool(&base1);
        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let body1 = spy1
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            !body1.contains("did not pass the parent's verification"),
            "{body1}"
        );
        assert_eq!(note().as_deref(), Some("working: cheap"));

        // fix round 1: feedback reaches the delegate and the note counts it
        let (base2, spy2) = request_spy();
        let tool = cheap_tool(&base2);
        tool.invoke(
            &ctx,
            json!({"step": 1, "feedback": "cargo test fails: double(0) returned 1"}),
        )
        .await
        .unwrap();
        let body2 = spy2
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            body2.contains("did not pass the parent's verification"),
            "{body2}"
        );
        assert!(body2.contains("double(0) returned 1"), "{body2}");
        assert_eq!(note().as_deref(), Some("working: cheap (fix 1/5)"));

        // fix round 2 keeps counting
        let (base3, _spy3) = request_spy();
        let tool = cheap_tool(&base3);
        tool.invoke(&ctx, json!({"step": 1, "feedback": "still failing"}))
            .await
            .unwrap();
        assert_eq!(note().as_deref(), Some("working: cheap (fix 2/5)"));
    }

    #[tokio::test]
    async fn delegate_is_refused_after_five_fix_rounds() {
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "Write double()".into(),
            verification: "".into(),
            model: "cheap".into(),
            context: "".into(),
        }]);
        // simulate five failed fix rounds already recorded on the step
        ctx.session.update_plan(
            PlanTarget::Id(1),
            PlanStatus::InProgress,
            Some("working: cheap (fix 5/5)".into()),
        );

        let (base, _spy) = request_spy();
        let tool = cheap_tool(&base);
        let err = tool
            .invoke(&ctx, json!({"step": 1, "feedback": "still red"}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("5 failed fix round(s)"), "{msg}");
        assert!(msg.contains("do the step yourself"), "{msg}");

        // a bare re-run is refused too once the limit is reached
        let (base2, _spy2) = request_spy();
        let tool = cheap_tool(&base2);
        let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
        assert!(err.to_string().contains("do the step yourself"), "{err}");
    }

    #[tokio::test]
    async fn feedback_without_a_step_is_rejected() {
        let (base, _spy) = request_spy();
        let tool = cheap_tool(&base);
        let ctx = test_ctx();
        let err = tool
            .invoke(
                &ctx,
                json!({"model": "cheap", "task": "write double()", "feedback": "nope"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("`feedback` is only valid with `step`"),
            "{err}"
        );
    }

    #[test]
    fn deny_list_keeps_commit_and_session_tools_away_but_not_work_tools() {
        assert!(DelegateTool::denied_for_delegates("git_commit"));
        assert!(DelegateTool::denied_for_delegates("delegate"));
        assert!(DelegateTool::denied_for_delegates("ask_question"));
        assert!(DelegateTool::denied_for_delegates("set_plan"));
        assert!(DelegateTool::denied_for_delegates("update_plan"));
        assert!(DelegateTool::denied_for_delegates("set_step_model"));
        assert!(DelegateTool::denied_for_delegates("finish_plan"));
        assert!(DelegateTool::denied_for_delegates("rename_session"));
        assert!(DelegateTool::denied_for_delegates("set_status_bar"));
        // The delegate must keep every tool that does the actual work...
        for name in [
            "write_file",
            "apply_edit",
            "apply_patch",
            "shell",
            "run_task",
            "run_tests",
            "remember",
            "amend_decision",
            "find_decisions",
            "remember_glossary",
            "find_glossary",
            "read_glossary",
            "read_file",
            "rgrep",
            "list_symbols",
            "git_status",
            "web_search",
        ] {
            assert!(!DelegateTool::denied_for_delegates(name), "{name}");
        }
    }

    /// A recording stub tool standing in for a real repository tool (e.g.
    /// write_file). Lets tests prove the delegate sub-agent actually dispatched
    /// a tool call without touching the real filesystem.
    struct StubTool {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn spec(&self) -> &ToolSpec {
            &STUB_WRITE_SPEC
        }

        async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("stub write_file executed".to_string())
        }
    }

    static STUB_WRITE_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
        name: "write_file".into(),
        description: "stub write_file".into(),
        json_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
    });

    /// A recording stub that stands in for the read-only `read_file` tool (a
    /// read is never a state change, so it feeds the delegate read guard).
    struct ReadStubTool {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Tool for ReadStubTool {
        fn spec(&self) -> &ToolSpec {
            &STUB_READ_SPEC
        }

        async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("stub read_file executed".to_string())
        }
    }

    static STUB_READ_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
        name: "read_file".into(),
        description: "stub read_file".into(),
        json_schema: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    });

    /// A fake chat server that answers N sequential requests with N distinct
    /// raw JSON bodies. The delegate sub-agent loop makes one request per turn,
    /// so this models a tool round (turn 1: model asks for a tool) followed by
    /// a final answer (turn 2).
    fn scripted_server(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let mut used = 0usize;
                loop {
                    let n = stream.read(&mut buf[used..]).unwrap();
                    if n == 0 {
                        break;
                    }
                    used += n;
                    if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).unwrap();
            }
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    /// Build a delegate whose registry contains one recording stub `write_file`
    /// tool, and a scripted model that first calls it natively then answers.
    #[tokio::test]
    async fn delegate_executes_its_tools_in_a_subagent_loop() {
        let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool {
            calls: write_calls.clone(),
        }));

        let tool_call_turn = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let final_turn =
            json!({"choices": [{"message": {"content": "done, file written"}}]}).to_string();
        let base = scripted_server(vec![tool_call_turn, final_turn]);

        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
            .await
            .unwrap();
        assert!(out.contains("done, file written"), "{out}");
        assert_eq!(
            write_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "delegate must have run its write_file tool once"
        );
    }

    /// The delegate's sub-agent tool calls must reach the session's event
    /// channel (tagged with the delegate's configured name) so the chat can
    /// show what the delegate is doing while it works.
    #[tokio::test]
    async fn delegate_tool_activity_streams_as_chat_events() {
        let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool {
            calls: write_calls.clone(),
        }));

        let tool_call_turn = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let final_turn =
            json!({"choices": [{"message": {"content": "done, file written"}}]}).to_string();
        let base = scripted_server(vec![tool_call_turn, final_turn]);

        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();

        // A context wired to a real event channel instead of the no-op sink.
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let mut ctx = test_ctx();
        ctx.events = Arc::new(crate::session::SessionEvents(tx));

        let out = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
            .await
            .unwrap();
        assert!(out.contains("done, file written"), "{out}");

        // Drain the emitted activity: one call and one result, both tagged with
        // the delegate's configured name so the chat can attribute them.
        let mut seen_call = false;
        let mut seen_result = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                crate::session::AgentEvent::DelegateToolCall { model, name, .. } => {
                    seen_call = true;
                    assert_eq!(model, "cheap");
                    assert_eq!(name, "write_file");
                }
                crate::session::AgentEvent::DelegateToolResult {
                    model, name, ok, ..
                } => {
                    seen_result = true;
                    assert_eq!(model, "cheap");
                    assert_eq!(name, "write_file");
                    assert!(ok);
                }
                _ => {}
            }
        }
        assert!(seen_call, "expected a DelegateToolCall event");
        assert!(seen_result, "expected a DelegateToolResult event");
    }

    /// A registry that contains exactly one recording stub `write_file` tool.
    fn write_stub_registry(calls: Arc<std::sync::atomic::AtomicUsize>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool { calls }));
        registry
    }

    /// A native-mode delegate that repeats the SAME tool call with the SAME
    /// arguments gets it refused and then aborts instead of looping forever.
    /// Sequence: turn 1 executes the call, turns 2-3 are refused as
    /// no-progress repeats, turn 4 hits MAX_LOOP_REFUSALS and bails.
    #[tokio::test]
    async fn repeated_native_tool_call_is_refused_then_aborts() {
        let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = write_stub_registry(write_calls.clone());

        let call = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let base = scripted_server(vec![call.clone(), call.clone(), call.clone(), call]);

        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();
        let ctx = test_ctx();
        let err = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("repeated identical action"), "{text}");
        assert!(text.contains("write_file"), "{text}");
        assert_eq!(
            write_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the repeat must never reach the tool: only the first call runs"
        );
    }

    /// The same guard works for ReAct-text delegates: an identical
    /// Tool:/Args: call repeated with nothing changing in between is refused
    /// and the run aborts after MAX_LOOP_REFUSALS repeats.
    #[tokio::test]
    async fn repeated_react_tool_call_is_refused_then_aborts() {
        let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = write_stub_registry(write_calls.clone());

        let call =
            json!({"choices": [{"message": {"content": "Thought: retry the write\nTool: write_file\nArgs: {\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"}}]})
                .to_string();
        let base = scripted_server(vec![call.clone(), call.clone(), call.clone(), call]);

        let mut d = delegate("cheap", &base);
        d.llm.protocol = Protocol::React;
        let cfg = Config {
            delegates: vec![d],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();
        let ctx = test_ctx();
        let err = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("repeated identical action"), "{text}");
        assert_eq!(
            write_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the repeat must never reach the tool: only the first call runs"
        );
    }

    /// A mutation between two identical calls is real progress, so the second
    /// identical call must NOT be refused (no false positive on a legit
    /// re-check after an edit).
    #[tokio::test]
    async fn identical_call_after_a_mutation_is_not_a_loop() {
        let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = write_stub_registry(write_calls.clone());

        let call_a = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let call_b = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_2",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"src/b.rs\",\"content\":\"pub fn b(){}\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let final_turn =
            json!({"choices": [{"message": {"content": "done, both files written"}}]}).to_string();
        // a executes, b executes (a mutation -> clears refusals), a again is a
        // legitimate re-check after state changed, then a final answer.
        let base = scripted_server(vec![call_a.clone(), call_b, call_a, final_turn]);

        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "write two files"}))
            .await
            .unwrap();
        assert!(out.contains("done, both files written"), "{out}");
        assert_eq!(
            write_calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "all three write_file calls were legitimate"
        );
    }

    /// A delegate that tries to `git_commit` gets an "unknown tool" error and
    /// must recover — commit is simply not in its registry.
    #[tokio::test]
    async fn delegate_cannot_commit_even_if_the_model_asks() {
        // Registry deliberately has no git_commit: only a stub writer exists.
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));

        // The model first (wrongly) asks to commit, then realises and answers.
        let commit_turn = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "git_commit",
                            "arguments": "{\"message\":\"x\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let final_turn =
            json!({"choices": [{"message": {"content": "no commit available, done"}}]}).to_string();
        let base = scripted_server(vec![commit_turn, final_turn]);

        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "commit everything"}))
            .await
            .unwrap();
        // The sub-agent loop must not have crashed: unknown tool became an
        // observation and the model recovered with a final answer.
        assert!(out.contains("no commit available, done"), "{out}");
    }

    /// The read guard itself (mirror of the main loop's `allow_read_step`, but
    /// with delegate wording): the 21st consecutive read-only call is refused,
    /// the nudge says implement instead of reading, and it never mentions
    /// `update_plan` (delegates have no plan tool).
    #[test]
    fn delegate_read_guard_nudges_after_20_consecutive_reads() {
        let mut reads = 0usize;
        for i in 0..20 {
            assert!(
                refuse_reading("read_file", &mut reads, DELEGATE_READ_NUDGE).is_none(),
                "read #{i} should be allowed before the threshold"
            );
        }
        assert_eq!(reads, 20);
        let msg = refuse_reading("read_file", &mut reads, DELEGATE_READ_NUDGE)
            .expect("the 21st read is refused");
        assert!(msg.contains("20 reads"), "{msg}");
        assert!(msg.contains("implement now"), "{msg}");
        assert!(!msg.contains("update_plan"), "delegates cannot plan: {msg}");
        // The counter never grows past the threshold: further reads stay refused.
        assert!(refuse_reading("read_file", &mut reads, DELEGATE_READ_NUDGE).is_some());
        assert_eq!(reads, 20);
    }

    /// Any non-read action resets the delegate's read counter, so a delegate
    /// that edits between reads never trips the guard.
    #[test]
    fn delegate_read_guard_resets_after_an_action() {
        let mut reads = 19usize; // one short of the threshold
        assert!(refuse_reading("write_file", &mut reads, DELEGATE_READ_NUDGE).is_none());
        assert_eq!(reads, 0, "a mutating call resets the read counter");
        assert!(refuse_reading("read_ranges", &mut reads, DELEGATE_READ_NUDGE).is_none());
        assert_eq!(reads, 1);
    }

    /// A native-mode delegate that does nothing but read different files for 21
    /// consecutive turns has its 21st read refused: the tool is only ever
    /// reached 20 times, and the run still ends with a normal final answer.
    #[tokio::test]
    async fn native_delegate_read_guard_stops_a_read_only_run() {
        let read_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ReadStubTool {
            calls: read_calls.clone(),
        }));

        let mut turns = Vec::new();
        for i in 0..21 {
            turns.push(
                json!({
                    "choices": [{
                        "message": {
                            "content": "",
                            "tool_calls": [{
                                "id": format!("call_{i}"),
                                "function": {
                                    "name": "read_file",
                                    "arguments": json!({"path": format!("src/f{i}.rs")}).to_string()
                                }
                            }]
                        }
                    }]
                })
                .to_string(),
            );
        }
        let final_turn = json!({"choices": [{"message": {"content": "done reading"}}]}).to_string();
        turns.push(final_turn);
        let base = scripted_server(turns);

        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "read src/f0.rs"}))
            .await
            .unwrap();
        assert!(out.contains("done reading"), "{out}");
        assert_eq!(
            read_calls.load(std::sync::atomic::Ordering::SeqCst),
            20,
            "the 21st read must be refused before it reaches the tool"
        );
    }

    /// The same read guard applies to ReAct-text delegates: 21 read-only
    /// Tool:/Args: turns, the 21st refused, then a normal final answer.
    #[tokio::test]
    async fn react_delegate_read_guard_stops_a_read_only_run() {
        let read_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ReadStubTool {
            calls: read_calls.clone(),
        }));

        let mut turns = Vec::new();
        for i in 0..21 {
            turns.push(
                json!({"choices": [{"message": {"content": format!(
                    "Thought: still exploring\nTool: read_file\nArgs: {{\"path\":\"src/f{i}.rs\"}}"
                )}}]})
                .to_string(),
            );
        }
        let final_turn = json!({"choices": [{"message": {"content": "done reading"}}]}).to_string();
        turns.push(final_turn);
        let base = scripted_server(turns);

        let mut d = delegate("cheap", &base);
        d.llm.protocol = Protocol::React;
        let cfg = Config {
            delegates: vec![d],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(&ctx, json!({"model": "cheap", "task": "read src/f0.rs"}))
            .await
            .unwrap();
        assert!(out.contains("done reading"), "{out}");
    }

    /// A fake chat server that answers N sequential requests with N distinct
    /// raw JSON bodies and forwards each raw REQUEST body (read per
    /// Content-Length) to the returned channel first. The delegate loop makes
    /// one request per turn, so tests can assert what the delegate was asked
    /// at every step and steer it between turns.
    fn scripted_spy(responses: Vec<String>) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for resp_body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut data = Vec::new();
                let mut tmp = [0u8; 8192];
                let mut body_len: Option<usize> = None;
                let mut header_end: Option<usize> = None;
                while body_len.map_or(true, |len| header_end.unwrap_or(0) + 4 + len > data.len()) {
                    let n = stream.read(&mut tmp).unwrap();
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&tmp[..n]);
                    if header_end.is_none() {
                        if let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = Some(p);
                            let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
                            body_len = head.lines().find_map(|l| {
                                l.trim()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse().ok())
                            });
                        }
                    }
                }
                let body = match (header_end, body_len) {
                    (Some(he), Some(len)) => {
                        let start = he + 4;
                        let end = (start + len).min(data.len());
                        String::from_utf8_lossy(&data[start..end]).into_owned()
                    }
                    _ => String::new(),
                };
                let _ = tx.send(body);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                stream.write_all(resp.as_bytes()).unwrap();
            }
        });
        (format!("http://127.0.0.1:{port}/v1"), rx)
    }

    /// Wait (with a timeout) for the next request body the fake server sees.
    async fn recv_body(rx: &std::sync::mpsc::Receiver<String>) -> String {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(b) = rx.try_recv() {
                return b;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting for a model request");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// A write_file stub whose invocation signals `called` (a oneshot: safe to
    /// fire before the test awaits it) and then blocks on `release` (a Notify).
    /// Lets a test steer a running delegate at a deterministic moment: the stub
    /// is executing (so the sub-agent is mid-flight) when the steer is sent.
    struct Gate {
        called: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: tokio::sync::Notify,
    }

    struct GatedWriteTool {
        gate: Arc<Gate>,
    }

    #[async_trait]
    impl Tool for GatedWriteTool {
        fn spec(&self) -> &ToolSpec {
            &STUB_WRITE_SPEC
        }

        async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
            if let Some(send) = self.gate.called.lock().unwrap().take() {
                let _ = send.send(());
            }
            self.gate.release.notified().await;
            Ok("stub write_file executed".to_string())
        }
    }

    /// A steering message typed while a DELEGATE owns the loop is delivered to
    /// the delegate: sent while its write_file tool is still executing, it must
    /// appear in the delegate's NEXT model request, and the run must then end
    /// normally.
    #[tokio::test]
    async fn delegate_receives_a_steer_mid_run() {
        const STEER: &str = "change direction now";
        let (called_tx, called_rx) = tokio::sync::oneshot::channel();
        let gate = Arc::new(Gate {
            called: std::sync::Mutex::new(Some(called_tx)),
            release: tokio::sync::Notify::new(),
        });
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(GatedWriteTool { gate: gate.clone() }));

        let tool_call_turn = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let final_turn =
            json!({"choices": [{"message": {"content": "done after the steer"}}]}).to_string();
        let (base, bodies) = scripted_spy(vec![tool_call_turn, final_turn]);

        let cfg = Config {
            delegates: vec![delegate("cheap", &base)],
            ..Config::default()
        };
        let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
            .unwrap()
            .unwrap();

        // The delegate inherits a live steering pipe from its context.
        let (steer, steer_tx) = comrade_tool::Steer::channel();
        let mut ctx = test_ctx();
        ctx.steer = Some(steer);

        let run = tokio::spawn({
            let ctx = ctx.clone();
            async move {
                tool.invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
                    .await
                    .unwrap()
            }
        });

        // Request 1 (its first model turn) must NOT contain the steer yet.
        let first = recv_body(&bodies).await;
        assert!(
            !first.contains(STEER),
            "steer leaked into the first request"
        );

        // The delegate is now executing write_file: steer it, then let the
        // tool finish so the loop reaches its next rest point.
        called_rx.await.unwrap();
        steer_tx.send(STEER.to_string()).unwrap();
        gate.release.notify_one();

        // Request 2 must carry the steer as a user message.
        let second = recv_body(&bodies).await;
        assert!(
            second.contains(STEER),
            "steer missing from the delegate's next request"
        );

        let out = run.await.unwrap();
        assert!(out.contains("done after the steer"), "{out}");
    }
}
