use super::*;

// ---------------------------------------------------------------------------
// delegate_parallel
// ---------------------------------------------------------------------------

/// Name of the parallel fan-out tool advertised to the tech lead.
pub const TOOL_NAME_PARALLEL: &str = "delegate_parallel";

/// Most jobs one `delegate_parallel` call may fan out, so a single call cannot
/// spawn an unbounded number of concurrent sub-agent runs.
const MAX_PARALLEL_JOBS: usize = 8;

/// Parallel jobs isolate into a git worktree unless a job explicitly opts out.
fn default_isolate() -> bool {
    true
}

/// A tool that runs several INDEPENDENT delegate tasks at the same time and
/// returns every reply together.
///
/// The main loop already parallelises a native batch that is entirely
/// `delegate` calls; this tool gives the same fan-out in ONE call, so it also
/// works under the ReAct protocol (one action per turn) and lets the lead split
/// a job across models without emitting several tool calls.
pub struct DelegateParallelTool {
    spec: ToolSpec,
    targets: Vec<Target>,
    tools: ToolRegistry,
    limits: DelegateLimits,
}

impl DelegateParallelTool {
    /// Build the tool from the configured `[[delegates]]` entries. Returns
    /// `Ok(None)` when no delegates are configured.
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
            "Run SEVERAL independent delegate tasks at the SAME time and return every reply together.",
            "Use it to fan out reviews or checks (e.g. three reviewers on the same diff) or to split a",
            "job into independent sub-tasks across models.",
            "",
            "Each job runs its own delegate sub-agent WITH tools (minus git_commit); every job's `model`",
            "must be a configured delegate. For a single task use `delegate`; for a plan step use",
            "`delegate` with `step` (this tool never touches the plan). A delegate configured",
            "`approval = \"ask\"` pauses for each of its jobs before the batch starts; `approval = \"deny\"`",
            "refuses it.",
            "",
            "Jobs are isolated into their own git worktree by default (a detached checkout under",
            ".comrade/worktrees/), so parallel jobs cannot clobber each other's files; set a job's",
            "`isolate = false` to make it share the workspace instead - then do NOT point two jobs at",
            "the same files. A changed worktree is left in",
            "place for you to merge back before you finish; a job that changed nothing has its",
            "worktree removed. In a non-git project jobs fall back to sharing the workspace.",
        ]
        .join("\n");
        let description = format!("{body}\n\nConfigured delegates:\n{listing}");
        let schema = json!({
            "type": "object",
            "properties": {
                "jobs": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_PARALLEL_JOBS,
                    "description": "The independent tasks to run concurrently.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "model": { "type": "string", "enum": names, "description": "Configured delegate model for this job." },
                            "task": { "type": "string", "description": "Self-contained job for the delegate: paths, code, expected output." },
                            "context": { "type": "string", "description": "Optional background for the delegate." },
                            "isolate": { "type": "boolean", "default": true, "description": "Run this job in its own git worktree (the default) so parallel jobs cannot clobber each other's files; set false to share the workspace. Without a git repo jobs fall back to sharing." }
                        },
                        "required": ["model", "task"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["jobs"],
            "additionalProperties": false
        });
        Ok(Some(Self {
            spec: ToolSpec {
                name: TOOL_NAME_PARALLEL.into(),
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
impl Tool for DelegateParallelTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Job {
            model: String,
            task: String,
            #[serde(default)]
            context: String,
            #[serde(default = "default_isolate")]
            isolate: bool,
        }
        #[derive(Deserialize)]
        struct Args {
            jobs: Vec<Job>,
        }
        let args: Args = serde_json::from_value(args)?;
        if args.jobs.is_empty() {
            bail!("`jobs` must contain at least one job");
        }
        if args.jobs.len() > MAX_PARALLEL_JOBS {
            bail!("at most {MAX_PARALLEL_JOBS} jobs per call");
        }

        struct Prepared {
            idx: usize,
            model: String,
            display: String,
            native: bool,
            prompt: String,
            isolate: bool,
        }
        let mut prepared: Vec<Prepared> = Vec::new();
        for (i, job) in args.jobs.iter().enumerate() {
            let model = job.model.trim();
            if job.task.trim().is_empty() {
                bail!("job {}: `task` must not be empty", i + 1);
            }
            let (idx, target) = self
                .targets
                .iter()
                .enumerate()
                .find(|(_, t)| t.cfg.name == model)
                .ok_or_else(|| {
                    let listed = self
                        .targets
                        .iter()
                        .map(|t| cfg_line(&t.cfg))
                        .collect::<Vec<_>>()
                        .join("\n");
                    anyhow::anyhow!(
                        "job {}: unknown delegate model {model:?}. Configured delegates:\n{listed}",
                        i + 1
                    )
                })?;
            let prompt = if job.context.trim().is_empty() {
                format!("Task:\n{}", job.task)
            } else {
                format!("Context:\n{}\n\nTask:\n{}", job.context, job.task)
            };
            prepared.push(Prepared {
                idx,
                model: model.to_string(),
                display: target.cfg.llm.display(),
                native: target.cfg.llm.protocol.native_enabled(),
                prompt,
                isolate: job.isolate,
            });
        }

        // Isolation needs a git repo; when the project is not one, fall back to
        // the shared workspace so parallel delegation still works.
        let can_isolate = if prepared.iter().any(|p| p.isolate) {
            crate::worktree::Worktree::isolation_available(&ctx.project_root).await
        } else {
            true
        };

        // Gate every job up front so a single denial aborts before anything runs.
        for (i, p) in prepared.iter().enumerate() {
            let cfg = &self.targets[p.idx].cfg;
            enforce_approval(
                cfg,
                ctx,
                format!("Run parallel delegate job {} ({})?", i + 1, p.model),
                Some(approval_preview(&p.prompt, "Job to delegate:")),
            )
            .await?;
        }

        let results = join_all(prepared.iter().map(|p| {
            let target = &self.targets[p.idx];
            let tools = &self.tools;
            let limits = &self.limits;
            let isolated = p.isolate && can_isolate;
            async move {
                // An isolating job runs in its own worktree; its tools, cwd and
                // system prompt are rooted there so parallel edits cannot clash.
                let worktree = if isolated {
                    Some(
                        crate::worktree::Worktree::create(&ctx.project_root, next_worktree_id())
                            .await
                            .with_context(|| format!("isolating delegate {} ", p.model))?,
                    )
                } else {
                    None
                };
                let (job_ctx, root) = match &worktree {
                    Some(w) => {
                        let mut c = ctx.clone();
                        c.project_root = w.path().to_path_buf();
                        c.cwd = w.path().to_path_buf();
                        (c, w.path().to_string_lossy().to_string())
                    }
                    None => (ctx.clone(), ctx.project_root.to_string_lossy().to_string()),
                };
                let system = delegate_system_prompt(&root, tools, p.native);
                let reply = run_delegate_subagent(
                    &target.client,
                    tools,
                    &job_ctx,
                    system,
                    p.prompt.clone(),
                    &p.model,
                    p.native,
                    limits,
                    DELEGATE_READ_NUDGE,
                )
                .await
                .with_context(|| format!("delegate {} ({}) failed", p.model, p.display))?;
                Ok::<_, anyhow::Error>((reply, worktree))
            }
        }))
        .await;

        let mut out = format!("{} parallel delegate job(s):\n", prepared.len());
        if !can_isolate && prepared.iter().any(|p| p.isolate) {
            out.push_str(
                "[project is not a git repository; isolated jobs ran in the shared workspace]\n",
            );
        }
        for (i, (p, res)) in prepared.iter().zip(results).enumerate() {
            match res {
                Ok((reply, worktree)) => {
                    out.push_str(&format!("\n=== job {} ({}) ===\n{reply}\n", i + 1, p.model));
                    if let Some(w) = worktree {
                        if w.has_changes().await {
                            let wt = w.path().display();
                            let repo = ctx.project_root.display();
                            out.push_str(&format!(
                                "[job {} left changes in worktree {wt} — merge them back before you verify:\n    git -C {wt} add -A && git -C {wt} diff --cached --binary | git -C {repo} apply --3way\n  then remove it: git -C {repo} worktree remove --force {wt}]\n",
                                i + 1,
                            ));
                        } else {
                            let _ = w.remove().await;
                            out.push_str(&format!(
                                "[job {} isolated in a worktree; it made no changes, so it was removed]\n",
                                i + 1
                            ));
                        }
                    }
                }
                Err(err) => out.push_str(&format!(
                    "\n=== job {} ({}) FAILED ===\nERROR: {err:#}\n",
                    i + 1,
                    p.model
                )),
            }
        }
        Ok(out)
    }
}

/// Monotonic id for isolated delegate worktrees (unique within this process).
fn next_worktree_id() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    ((std::process::id() as u64) << 20) | n
}
