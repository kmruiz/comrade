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
//! Two hard limits keep the parent in control:
//! - The delegate's tool registry excludes `git_commit` (only the tech lead
//!   commits), the session/UI tools (`set_plan`, `update_plan`, `finish_plan`,
//!   `rename_session`, `set_status_bar`, `ask_question`) and `delegate` itself
//!   (no recursion). See [`DENIED_FOR_DELEGATES`].
//! - The delegate tool call is approval-gated like any mutation: the human
//!   approves handing the task off once, then every nested tool call the
//!   delegate makes runs auto-approved (its context has `auto_approve` set).

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use comrade_tool::{PlanStatus, PlanTarget, Tool, ToolContext, ToolRegistry, ToolSpec};
use serde_json::{Value, json};

use crate::config::DelegateCfg;
use crate::context::ContextManager;
use crate::llm::{ChatMessage, LlmClient, Role, ToolCallMsg};
use crate::react::{parse_turn, render_observation};

/// Name of the tool advertised to the tech lead model.
pub const TOOL_NAME: &str = "delegate";

/// Tools a delegate must never see, by spec name. Only the tech lead commits,
/// plans, renames the session or asks the human; `delegate` is excluded so a
/// delegate cannot recurse. Everything else in the main registry — including
/// the mutating tools (write_file, apply_edit/apply_patch, shell, run_task,
/// remember, amend_decision) — is fair game because the delegate runs
/// auto-approved under a one-shot human handoff.
pub const DENIED_FOR_DELEGATES: &[&str] = &[
    "git_commit",
    "delegate",
    "ask_question",
    "rename_session",
    "set_status_bar",
    "set_plan",
    "update_plan",
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
struct Target {
    cfg: DelegateCfg,
    client: LlmClient,
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
fn delegate_line(name: &str, description: &str) -> String {
    match description.trim() {
        "" => format!("  - {name}"),
        desc => format!("  - {name}: {desc}"),
    }
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
    /// not advertised at all). Fails on duplicate/blank names or a delegate
    /// that cannot build an HTTP client.
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

        let mut names: Vec<String> = Vec::new();
        let mut targets: Vec<Target> = Vec::with_capacity(delegates.len());
        for (i, cfg) in delegates.iter().enumerate() {
            if cfg.name.trim().is_empty() {
                bail!("delegates[{i}]: every delegate needs a `name`");
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

        let listing = delegates
            .iter()
            .map(|d| delegate_line(&d.name, &d.description))
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

The tool call itself is approval-gated (one human approval for the handoff); \
once approved, every tool call the delegate makes runs auto-approved. Keep \
using `delegate` for well-bounded jobs, but expect the delegate to iterate on \
the repo with its own tools.

To execute one of your plan steps, pass `step` (the plan step id): the task and \
context then come from that step and `model` must match the step's model. \
Otherwise delegate ad-hoc work with `model` + `task` (+ optional `context`).

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
yourself.

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
                    "description": "Verification failure output from the parent for a delegate that previously attempted `step`: the delegate must fix its deliverable until it passes. Counts as one fix round (max 5 per step, tracked on the step's note). Only valid with `step`."
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
                let goal = found.goal.trim();
                let verify = found.verification.trim();
                let task = if verify.is_empty() {
                    goal.to_string()
                } else {
                    format!("{goal}\n\nVerify your work: {verify}")
                };

                // Reflect the delegation in the plan so the UI shows which
                // delegate is working, and count the fix rounds. A step gets one
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
                // Handoff approval: the human approves the delegation once
                // (auto-approve skips this) BEFORE the step is marked working,
                // so a denial leaves the plan untouched.
                ctx.confirm(
                    format!("Delegate plan step {id} to {model}?", model = found.model),
                    Some(format!("Task:\n{goal}")),
                )
                .await?;
                ctx.session
                    .update_plan(PlanTarget::Id(id), PlanStatus::InProgress, Some(note));

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
                .map(|t| delegate_line(&t.cfg.name, &t.cfg.description))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("unknown delegate model {model:?}. Configured delegates:\n{listed}");
        };

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
        // Ad-hoc handoff approval (plan steps were approved above, before being
        // marked in-progress).
        if delegated_step.is_none() {
            ctx.confirm(
                format!("Delegate task to {model}?"),
                Some(format!("Task:\n{task}")),
            )
            .await?;
        }

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
            native,
            &self.limits,
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

/// System prompt for a delegated developer sub-agent that has real tools.
/// Unlike the old text-only delegates it must know what it can call (and that
/// it must NOT try to commit), so the prompt lists the delegate-scoped tool
/// registry and explains the working protocol.
fn delegate_system_prompt(project_root: &str, tools: &ToolRegistry, native: bool) -> String {
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
    format!(
        "\
You are a developer sub-agent on Comrade's team. Your tech lead delegated ONE \
self-contained task to you. Working directory: {project_root}. You have REAL \
tools in this repository and are expected to use them to complete the task \
yourself — read, search, edit and write files, run tests, remember decisions. \
Your tool call for this task was approved by the human and every tool you call \
runs auto-approved, so act directly and do not ask for permission.

Hard rule: you CANNOT commit (no `git_commit` tool) — only the tech lead \
commits. Never try to run git commit through other tools.

Available tools:
{tool_lines}
{protocol}

Before replying, verify your own work with the tools (run the tests / re-read \
the code) and close your reply with a single line starting with `VERIFICATION:` \
stating what you checked and whether it passes."
    )
}

/// Run one delegate as a tool-using sub-agent until it produces a final answer.
/// Mirrors the main agent loop but for the delegate's own client, scoped tool
/// registry and limits: native tool calling when the delegate protocol allows
/// it, ReAct-style text calls otherwise (both advertised/parsed exactly like
/// the main loop so any model works). Every nested tool call runs against a
/// clone of the caller context with `auto_approve` forced on: the single human
/// approval happened at the `delegate` handoff.
async fn run_delegate_subagent(
    client: &LlmClient,
    tools: &ToolRegistry,
    parent_ctx: &ToolContext,
    system: String,
    user_prompt: String,
    native: bool,
    limits: &DelegateLimits,
) -> Result<String> {
    // The delegate inherits the session/user/undo of the parent but runs
    // auto-approved (handoff already approved) with a fresh approval slot.
    let mut dctx = parent_ctx.clone();
    dctx.auto_approve = true;
    dctx.approval = Arc::new(Mutex::new(None));

    let mut ctxm =
        ContextManager::with_system(system, limits.budget_tokens, limits.max_tool_output_chars);
    ctxm.push(ChatMessage::new(Role::User, user_prompt));

    for _ in 0..limits.max_iterations {
        ctxm.enforce_budget();
        let specs: Option<Vec<comrade_tool::ToolSpec>> = if native {
            let specs: Vec<_> = tools.iter().map(|t| t.spec().clone()).collect();
            if specs.is_empty() { None } else { Some(specs) }
        } else {
            None
        };

        let turn = client
            .chat_turn_once(ctxm.messages(), specs.as_deref())
            .await?;

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
                let output = match tools.get(&tc.name) {
                    Some(tool) => match tool.invoke(&dctx, args).await {
                        Ok(out) => out,
                        Err(e) => format!("ERROR: {e:#}"),
                    },
                    None => format!("unknown tool {:?}; choose from the listed tools", tc.name),
                };
                let clamped = ctxm.truncate_observation(&output);
                ctxm.push(ChatMessage::tool_result(tc.id, clamped));
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
        let output = match tool.invoke(&dctx, tool_call.args).await {
            Ok(out) => out,
            Err(e) => format!("ERROR: {e:#}"),
        };
        let clamped = ctxm.truncate_observation(&output);
        ctxm.push(ChatMessage::new(
            Role::User,
            render_observation(&tool_call.name, &clamped),
        ));
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
    use crate::config::{Config, DelegateCfg, LlmCfg};
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
            llm: LlmCfg {
                base_url: base_url.into(),
                model: "delegate-model".into(),
                ..LlmCfg::default()
            },
        }
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
}
