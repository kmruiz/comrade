use super::*;

impl AskAdviseTool {
    /// Whether `name` is a read-only repository tool an advisor may browse.
    /// Advisors get exactly the tools the main agent loop classifies as
    /// read-only (agent.rs), so the advice can never change state. Kept as a
    /// method so comrade-tui's registry builder and this tool share one
    /// source of truth.
    pub fn read_only_for_advice(name: &str) -> bool {
        crate::agent::is_read_only(name)
    }

    /// Build the advise tool from the configured `[[delegates]]` entries.
    /// Returns `Ok(None)` when no delegates are configured (the tool is then
    /// not advertised at all).
    ///
    /// `tools` is the registry the advisor may call: pass a view of the main
    /// registry filtered to [`AskAdviseTool::read_only_for_advice`] tools so
    /// the advice can be grounded in the code without any side effects.
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
            "Ask one of the configured delegate models for ADVICE - a second opinion - while you",
            "keep the task yourself. Two modes:",
            "",
            "1. Free-form advice: pass `model` + a self-contained `question` (+ optional `context`).",
            "Nothing is handed off and nothing runs; the advisor only answers. You get advice back -",
            "you still decide.",
            "",
            "2. Readiness check for a delegated plan step: pass `step` = a plan step id (do NOT",
            "pass `model`, `question` or `context`). The step's OWN delegate is consulted about",
            "whether the step's context suffices for it to pick the step up. VERDICT: READY marks",
            "the step `ready` to delegate; NEEDS_MORE keeps it `pending` with an \"awaiting",
            "context: ...\" note. Enrich with set_step_context, then re-ask until `ready`. Fire",
            "these checks in PARALLEL (one ask_advise step = <id> per delegated step, batched).",
            "",
            "The advisor is READ-ONLY: it can read/search files and git history and use memory/web",
            "tools, but has no write/edit/shell/run/commit/plan tools and cannot change anything.",
            "Consulting normally needs no approval; a delegate configured `approval = \"ask\"`",
            "pauses for human approval first, and `approval = \"deny\"` is refused.",
        ]
        .join("\n");
        let description = format!(
            "{body}\n\nConfigured delegates — pick the one whose description best fits the advice you need:\n{listing}"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "step": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Plan step id for a readiness check: the step's own delegate is consulted about whether its context suffices. VERDICT: READY marks it `ready`; NEEDS_MORE keeps it `pending`. The step supplies the question, context and delegate model, so pass `step` alone."
                },
                "model": {
                    "type": "string",
                    "enum": names,
                    "description": "Which configured delegate model should give the advice."
                },
                "question": {
                    "type": "string",
                    "description": "The advice you want, self-contained: what you are about to do, what you need judged (e.g. how to plan/split a task, whether an approach is sound)."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background the advisor cannot discover itself: plan draft, design notes, error output, constraints."
                }
            },
            "oneOf": [
                { "required": ["step"] },
                { "required": ["model", "question"] }
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
impl Tool for AskAdviseTool {
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
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let context = args
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        // Two mutually exclusive modes: a readiness check on a plan step, or a
        // free-form advice consult.
        let (model, user_prompt, approval_title) = match step_id {
            Some(id) => {
                // A small model routinely echoes `question`/`context` next to
                // `step`; both are DERIVED from the plan step, so ignore them
                // rather than spending an iteration on an avoidable error (the
                // delegate tool already tolerates the same redundancy). A
                // `model` that CONTRADICTS the step is still an error, below.
                let found = ctx
                    .session
                    .plan()
                    .into_iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| anyhow::anyhow!("no plan step with id {id}"))?;
                if !model_arg.is_empty() && model_arg != found.model {
                    bail!(
                        "`model` {model_arg:?} does not match the delegate assigned to plan step \
                         {id} ({:?}) — a step's readiness is checked with its own delegate",
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
                         a delegate — there is no delegate to confirm its readiness"
                    );
                }
                if matches!(found.status, PlanStatus::InProgress | PlanStatus::Done) {
                    bail!(
                        "plan step {id} is {} — readiness is checked while the step is pending, \
                         ready or blocked, before a delegate picks it up",
                        found.status
                    );
                }
                let goal = found.goal.trim();
                let verify = found.verification.trim();
                let step_ctx = found.context.trim();
                let prompt = format!(
                    "Context readiness check for plan step {id} (delegate {}):\n\
                     Goal: {goal}\n\
                     Verification: {verify}\n\
                     Context: {}\n\n\
                     You are the delegate that will execute this step. Working directory: {} — \
                     browse the repository read-only if you need more to judge. Tell the tech lead \
                     whether the context above is ENOUGH for you to accomplish the goal, or \
                     exactly what is missing.\n\n\
                     Reply with your verdict as the FINAL line, exactly one of:\n\
                     VERDICT: READY\n\
                     VERDICT: NEEDS_MORE: <exactly what extra context you need>",
                    found.model,
                    if step_ctx.is_empty() {
                        "(none)"
                    } else {
                        step_ctx
                    },
                    ctx.project_root.display(),
                );
                let model = found.model.clone();
                (
                    model.clone(),
                    prompt,
                    format!("Ask delegate {model} about readiness of plan step {id}?"),
                )
            }
            None => {
                if question.is_empty() {
                    bail!(
                        "`question` must not be empty: tell the delegate what you want advice on"
                    );
                }
                let model = model_arg.clone();
                let prompt = if context.is_empty() {
                    format!("Question:\n{question}")
                } else {
                    format!("Context:\n{context}\n\nQuestion:\n{question}")
                };
                (
                    model,
                    prompt,
                    format!("Ask delegate {model_arg} for advice?"),
                )
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

        // Per-delegate approval policy, same as `delegate`: "ask" pauses for
        // the human before the advice runs, "deny" refuses outright.
        enforce_approval(
            &target.cfg,
            ctx,
            approval_title,
            Some(approval_preview(&user_prompt, "Question:")),
        )
        .await?;

        let display = target.cfg.llm.display();
        let native = target.cfg.llm.protocol.native_enabled();
        let system = render_subagent_system(
            include_str!("../../prompts/advise-system.md"),
            &ctx.project_root.to_string_lossy(),
            &self.tools,
            native,
        );
        let reply = run_delegate_subagent(
            &target.client,
            &self.tools,
            ctx,
            system,
            user_prompt,
            &target.cfg.name,
            native,
            &self.limits,
            ADVICE_READ_NUDGE,
        )
        .await
        .with_context(|| format!("delegate {model} ({display}) failed to give advice"))?;

        // A readiness check resolves the delegate's reply into a plan status:
        // an explicit final VERDICT: READY marks the step ready to pick up;
        // anything else leaves it pending with an "awaiting context" note.
        if let Some(id) = step_id {
            match readiness_verdict(&reply) {
                Readiness::Ready => {
                    ctx.session.update_plan(
                        PlanTarget::Id(id),
                        PlanStatus::Ready,
                        Some(format!("ready: {model} confirmed the context")),
                    );
                    Ok(format!(
                        "delegate {model} ({display}) confirmed plan step {id} is READY to pick \
                         up — the context suffices.\n\n{reply}"
                    ))
                }
                Readiness::NeedsMore(request) => {
                    ctx.session.update_plan(
                        PlanTarget::Id(id),
                        PlanStatus::Pending,
                        Some(format!("awaiting context: {request}")),
                    );
                    Ok(format!(
                        "delegate {model} ({display}) needs more context for plan step {id}:\n\
                         {request}\n\nAdd it with `set_step_context` (index = {id}), then re-run \
                         ask_advise step = {id} until the step is `ready`.\n\n{reply}"
                    ))
                }
            }
        } else {
            Ok(format!("advice from {model} ({display}):\n{reply}"))
        }
    }
}
