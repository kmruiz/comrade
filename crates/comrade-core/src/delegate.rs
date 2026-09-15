//! The `delegate` tool: hand a single, self-contained sub-task to another
//! model — typically a cheaper or faster one on a different provider.
//!
//! The companion `delegate_parallel` tool fans out SEVERAL such tasks at once
//! (a single call, so it works in both the native and ReAct protocols) and
//! returns every reply together.
//!
//! The main ("tech lead") model keeps orchestrating and committing, but can
//! offload a well-defined piece of work to a developer model configured in
//! `config.toml` under `[[delegates]]`. Unlike plain-chat sub-agents, a
//! delegate runs a **real tool-using sub-agent loop**: it is handed the same
//! repository tools (read/search/write/edit, pom_run_tests/pom_run_task, memory, web
//! search) so it can genuinely do the job — write the file, run the tests, fix
//! failures — instead of returning text the parent must apply by hand.
//!
//! Two design points keep the parent in control:
//! - The delegate's tool registry excludes `git_commit` (only the tech lead
//!   commits), the session/UI tools (`self_set_plan`, `self_update_plan`,
//!   `self_set_step_model`, `self_finish_plan`, `self_rename_session`,
//!   `self_set_status_bar`, `ask_form`) and `delegate` itself (no recursion).
//!   See [`DENIED_FOR_DELEGATES`].
//! - The delegate tool call is not approval-gated by default: delegating runs
//!   directly (like `git_commit`/`pom_run_task`/`pom_run_tests`), and every nested tool
//!   call the delegate makes runs auto-approved (its context has `auto_approve`
//!   set), so a delegate works end-to-end without pausing for a human.
//!   EXCEPTION: a delegate whose `[[delegates]]` entry sets
//!   `approval = "ask"` pauses for human approval before it runs, and one set
//!   to `approval = "deny"` is refused outright (see [`enforce_approval`]).

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use comrade_tool::{
    PlanStatus, PlanTarget, Tool, ToolContext, ToolRegistry, ToolSpec, Verdict, declarations,
    removed_declarations,
};
use futures_util::future::join_all;
use serde::Deserialize;
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
/// extra model chats of its own, and `self_set_step_model` is excluded so a
/// delegate cannot reassign its own (or any) plan step while working. Everything
/// else in the main registry — including the mutating tools (fs_write_file,
/// fs_edit, shell, pom_run_task, record_adr, amend_adr) — is fair
/// game because the delegate runs auto-approved under a one-shot human handoff.
pub const DENIED_FOR_DELEGATES: &[&str] = &[
    "git_commit",
    "git_stash",
    "git_branch",
    "git_checkout",
    "delegate",
    "delegate_parallel",
    "ask_advise",
    "summarise",
    "ask_form",
    "self_rename_session",
    "self_set_status_bar",
    "self_set_plan",
    "self_update_plan",
    "self_set_step_model",
    "self_set_step_context",
    "self_finish_plan",
    "delegate_parallel",
    "run_bg",
    "bg_status",
    "bg_tail",
    "bg_kill",
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
    /// Wall-clock budget for the whole delegated run. Every model request and
    /// tool call is bounded by what is left of it; when it runs out the delegate
    /// is stopped and returns whatever it had gathered, so a slow or stuck
    /// delegate cannot hang the parent. [`Duration::ZERO`] disables the limit.
    pub timeout: Duration,
}

impl Default for DelegateLimits {
    fn default() -> Self {
        Self {
            max_iterations: 30,
            budget_tokens: 6000,
            max_tool_output_chars: 5000,
            timeout: Duration::from_secs(60),
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
        if !cfg.enabled {
            continue;
        }
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
        if targets.is_empty() {
            return Ok(None);
        }
        let names: Vec<String> = targets.iter().map(|t| t.cfg.name.clone()).collect();

        let listing = delegates
            .iter()
            .filter(|d| d.enabled)
            .map(cfg_line)
            .collect::<Vec<_>>()
            .join("\n");
        let body = [
            "Hand ONE self-contained task to another model - a delegate sub-agent WITH tools",
            "(read/search, fs_write_file, pom_run_tests, memory, web) minus git_commit; only you commit.",
            "",
            "Delegating runs without human approval. A delegate configured `approval = \"ask\"`",
            "pauses for approval first; `approval = \"deny\"` refuses it.",
            "",
            "To run one of your plan steps, pass `step`: the task, context and model then come from",
            "the step, and `model` must match the step's model. Otherwise pass `model` + `task`",
            "(+ optional `context`) for ad-hoc work. Plan steps you run yourself carry the reserved",
            "model \"self\" and cannot be delegated via `step`.",
            "",
            "Delegating a step marks it in_progress with a `working: <model>` note. After the",
            "delegate replies, run the step's verification yourself with your tools; on failure",
            "re-delegate the SAME step with `feedback` so it fixes its work (up to 5 fix rounds,",
            "then do the step yourself). The delegate closes with a VERIFICATION: line, but that is",
            "never proof - you verify.",
            "",
            "Several independent delegate calls issued in one message run in PARALLEL - split",
            "independent sub-tasks into separate calls and batch them together.",
        ]
        .join("\n");
        let description = format!(
            "{body}\n\nConfigured delegates — pick the one whose description best fits the task:\n{listing}"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "step": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Plan step id to execute. The task, context and model come from the step."
                },
                "model": {
                    "type": "string",
                    "enum": names,
                    "description": "Which configured delegate model does the work. Must match the step's model when `step` is given."
                },
                "task": {
                    "type": "string",
                    "description": "The exact, self-contained job for the delegate: paths, code, identifiers, expected output. Mutually exclusive with `step`."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background for the delegate: existing code, error logs, constraints. Mutually exclusive with `step`."
                },
                "feedback": {
                    "type": "string",
                    "description": "Your verification failure output for a step this delegate already attempted; it must fix its work until it passes. One fix round (max 5 per step). Must contain an actionable plan."
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
                // A small model often passes `step` together with a `task` (or a
                // `context`) that just restates the step. Be lenient: the step's
                // own task/context win and the duplicate is ignored, rather than
                // failing the call and making the model burn a turn retrying.
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
        // In native mode the tool schemas already carry each tool's description,
        // so the listing is names only - keeps the delegate prompt small. In
        // ReAct mode the listing line is the delegate's only source of "what is
        // this tool for", so it keeps a short description.
        let line = if native {
            format!("- {}\n", tool.spec().name)
        } else {
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
            format!("- {} — {desc}\n", tool.spec().name)
        };
        tool_lines.push_str(&line);
    }
    let protocol = if native {
        "You call tools natively (function calling). Before each tool call, write \
         one short sentence in the message content saying what you are about to do \
         and why - your tech lead reads it as your reasoning. When the task is done and \
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

/// Refuse a destructive tool call unless the delegate's tech lead approves it.
///
/// A small delegate makes a one-line change by rewriting the whole file with
/// `fs_write_file` — and silently deletes the code it was not asked to touch. In
/// the smoke trial that removed the crate's own pre-existing test and the
/// delegate then reported success. So a write that would REMOVE declarations the
/// file already had is stopped right here: the parent model (the tech lead, over
/// the `ask_upwards` channel) is asked for permission first, and when there is
/// nobody to ask the write is refused outright. Destruction fails closed.
///
/// Returns the message to hand back as the tool result, or `None` when the call
/// may proceed. Any other tool, and a write with nothing to lose, is never asked
/// about: the gate costs nothing until a deletion actually appears.
async fn refuse_destructive(
    dctx: &ToolContext,
    author: &str,
    name: &str,
    args: &Value,
    args_pretty: &str,
) -> Option<String> {
    if name != "fs_write_file" {
        return None;
    }
    let rel = args.get("path").and_then(Value::as_str)?.trim();
    let after = args.get("content").and_then(Value::as_str)?;
    if rel.is_empty() {
        return None;
    }
    // The model passes either a project-relative or an absolute path.
    let given = std::path::Path::new(rel);
    let path = if given.is_absolute() {
        given.to_path_buf()
    } else {
        dctx.project_root.join(given)
    };
    // A new file has nothing to lose; a read that fails is not our business here
    // (the tool itself reports a missing file far better than this gate could).
    let before = tokio::fs::read_to_string(&path).await.ok()?;
    let lost = removed_declarations(&before, after);
    if lost.is_empty() {
        return None;
    }

    let names = lost.join(", ");
    let kept = declarations(after);
    let title = format!("overwrite {rel} (deletes {names})");
    let detail = format!(
        "{rel} defines: {}. After the write only {} would be left, so writing it DELETES: {names}. \
         ({} lines -> {} lines.)",
        join_or(&declarations(&before), "(no functions)"),
        join_or(&kept, "no functions"),
        before.lines().count(),
        after.lines().count(),
    );
    let verdict = match dctx.session.upward() {
        Some(parent) => parent
            .approve(&title, &detail)
            .await
            .unwrap_or(Verdict::Unavailable),
        None => Verdict::Unavailable,
    };
    let refusal = match verdict {
        // The tech lead approved the deletion: let the write proceed.
        Verdict::Approved => return None,
        Verdict::Denied(why) => format!(
            "REFUSED by your tech lead: {why}\nNothing was written. Do NOT retry this whole-file \
             rewrite: make the change with `fs_edit`, keeping every existing line the task does \
             not change."
        ),
        Verdict::Unavailable => format!(
            "REFUSED: this `fs_write_file` would DELETE code that is already in {rel}: {names}. \
             Nothing was written.\nMake the change with `fs_edit` instead: copy the lines you need \
             to change into `old` and set `new` to those same lines plus your addition, so every \
             other line in the file stays exactly as it is. Never retype a file to make a small \
             change."
        ),
    };
    // Nothing else is emitted for a call that never reaches its tool, so without
    // this the refusal would be invisible in the delegate's sub-chat.
    dctx.events.tool_call(author, name, args_pretty).await;
    dctx.events.tool_result(author, name, &refusal, false).await;
    Some(refusal)
}

/// `items` as a comma-separated list, or `fallback` when it is empty.
fn join_or(items: &[String], fallback: &str) -> String {
    if items.is_empty() {
        fallback.to_string()
    } else {
        items.join(", ")
    }
}

/// Escalation cap for one delegate run: `ask_upwards` is a recovery move, not a
/// way to keep the parent model answering forever.
const MAX_UPWARD_ASKS: usize = 3;

/// Cap the `ask_upwards` tool per delegate run. Returns the refusal message for
/// the call that exceeds the cap, `None` for every other call (including the
/// first [`MAX_UPWARD_ASKS`] upward questions).
fn refuse_upward(name: &str, used: &mut usize) -> Option<String> {
    if name != "ask_upwards" {
        return None;
    }
    *used += 1;
    if *used > MAX_UPWARD_ASKS {
        return Some(format!(
            "You have already asked your tech lead {MAX_UPWARD_ASKS} questions. Stop asking: decide \
             now, make the smallest reasonable change, verify it, and reply with your final answer."
        ));
    }
    None
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
#[allow(clippy::too_many_arguments)]
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
    // auto-approved, so its nested tool calls never pause for a human
    // confirmation.
    let mut dctx = parent_ctx.clone();
    dctx.auto_approve = true;

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
    // Escalation budget: how many `ask_upwards` questions this run has spent.
    let mut upward_asks = 0usize;
    // Wall-clock budget for the whole run (see [`DelegateLimits::timeout`]): every
    // model request and tool call is bounded by what is left, so a single slow or
    // hung request (or a long chain of turns) cannot hold the parent run open
    // forever. On expiry the delegate answers with whatever it has.
    let deadline = if limits.timeout.is_zero() {
        None
    } else {
        Some(Instant::now() + limits.timeout)
    };
    let budget_left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
    // The most recent non-empty assistant text, returned as a best-effort answer
    // if the budget runs out before the delegate produces a final answer.
    let mut last_text = String::new();
    // The run's cancel token, set by the main agent loop (agent.rs). When the
    // human interrupts the parent, a delegate stuck waiting on its model
    // request must abort instead of holding the whole run at "working".
    let stop = parent_ctx.stop.clone();

    for _ in 0..limits.max_iterations {
        if let Some(stop) = &stop
            && stop.is_cancelled()
        {
            bail!("delegate interrupted: the run was cancelled");
        }
        // A steer typed while the delegate owned the loop reaches the delegate's
        // own conversation at its next rest point (drained from the shared bus
        // cloned into `dctx`).
        crate::agent::drain_steer(dctx.steer.as_ref(), &mut ctxm).await;
        ctxm.enforce_budget();
        // One-shot nudges mirrored from the main loop, so a small delegate does
        // not edit forever without testing, or keep re-verifying after it has
        // finished. Injected as a user turn so the next request sees it.
        if tracker.needs_verify_nudge() {
            ctxm.push_user_merged(crate::agent::VERIFY_NUDGE);
        } else if tracker.needs_stall_nudge() {
            ctxm.push_user_merged(crate::agent::STALL_NUDGE);
        }
        let specs: Option<Vec<comrade_tool::ToolSpec>> = if native {
            let specs: Vec<_> = tools.iter().map(|t| t.spec().clone()).collect();
            if specs.is_empty() { None } else { Some(specs) }
        } else {
            None
        };

        // Every model request is bounded by what is left of the budget.
        let model_call = async {
            match &stop {
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
            }
        };
        let Some(res) = invoke_within(budget_left(), model_call).await else {
            return Ok(timeout_answer(author, limits.timeout, &last_text));
        };
        let turn = res?;
        if !turn.content.trim().is_empty() {
            last_text = turn.content.clone();
        }

        // Native tool calls: dispatch all of them like the main loop does.
        if !turn.tool_calls.is_empty() {
            // The delegate's own reasoning text for this turn (the prose it emits
            // alongside its tool calls): surface it so the chat shows what the
            // delegate is thinking, not only which tools it ran.
            if !turn.content.trim().is_empty() {
                dctx.events.reasoning(author, turn.content.trim()).await;
            }
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
                if let Some(msg) = refuse_upward(&tc.name, &mut upward_asks) {
                    let clamped = ctxm.truncate_observation(&msg);
                    ctxm.push(ChatMessage::tool_result(tc.id, clamped));
                    continue;
                }
                if let Some(msg) =
                    refuse_destructive(&dctx, author, &tc.name, &args, &args_pretty).await
                {
                    // Destructive write without the tech lead's permission:
                    // answer the call so the history stays API-valid and let the
                    // delegate try a smaller, non-destructive edit.
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
                        // Bound the tool call by the remaining budget too, so a
                        // hanging tool cannot outlive the delegate's deadline.
                        let Some(ran) =
                            invoke_within(budget_left(), tool.invoke(&dctx, args)).await
                        else {
                            return Ok(timeout_answer(author, limits.timeout, &last_text));
                        };
                        let r = match ran {
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
        if let Some(t) = turn_p.thought.as_deref() {
            let t = t.trim();
            if !t.is_empty() {
                dctx.events.reasoning(author, t).await;
            }
        }
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
        if let Some(msg) = refuse_upward(&tool_call.name, &mut upward_asks) {
            let obs = ctxm.truncate_observation(&msg);
            ctxm.push(ChatMessage::new(
                Role::User,
                render_observation(&tool_call.name, &obs),
            ));
            continue;
        }
        if let Some(msg) = refuse_destructive(
            &dctx,
            author,
            &tool_call.name,
            &tool_call.args,
            &args_pretty,
        )
        .await
        {
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
        let Some(ran) = invoke_within(budget_left(), tool.invoke(&dctx, tool_call.args)).await
        else {
            return Ok(timeout_answer(author, limits.timeout, &last_text));
        };
        let (output, ok) = match ran {
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

/// Run a future under an optional remaining budget: `Some(left)` bounds it and
/// yields `None` if it does not finish in time, `None` runs it unbounded. Gives
/// every model request and tool call inside a delegate its slice of the
/// delegate's wall-clock budget (see [`DelegateLimits::timeout`]).
async fn invoke_within<F: std::future::Future>(
    left: Option<Duration>,
    fut: F,
) -> Option<F::Output> {
    match left {
        Some(left) => tokio::time::timeout(left, fut).await.ok(),
        None => Some(fut.await),
    }
}

/// The best-effort reply a delegate returns when it runs out of its wall-clock
/// budget: whatever partial answer it had, or a clear notice that it did not
/// finish, so the parent can carry on instead of waiting forever.
fn timeout_answer(author: &str, timeout: Duration, last: &str) -> String {
    let secs = timeout.as_secs();
    let head = format!(
        "[delegate {author}: stopped after {secs}s without a final answer — it reached its time \
         budget]"
    );
    let last = last.trim();
    if last.is_empty() {
        format!(
            "{head} It had not produced an answer yet, so treat its result as unavailable and \
             continue without it (re-delegate a smaller, tightly-scoped task if you still need it)."
        )
    } else {
        format!("{head}\n\nBest effort with what it had gathered so far:\n\n{last}")
    }
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

mod parallel;
pub use parallel::*;
#[cfg(test)]
mod tests;
