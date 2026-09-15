use super::*;

// ---------------------------------------------------------------------------
// Ad-hoc parallel delegation: the `jobs` argument of `delegate`
// ---------------------------------------------------------------------------

/// Most jobs one `delegate` call may fan out in a single `jobs` array, so one
/// call cannot spawn an unbounded number of concurrent sub-agent runs.
pub(crate) const MAX_PARALLEL_JOBS: usize = 8;

/// Run the ad-hoc `jobs` of a `delegate` call: several INDEPENDENT delegate
/// tasks at the SAME time, returning every reply together.
///
/// EVERY job is isolated in its own git worktree (a detached checkout under
/// `.comrade/worktrees/`), so two jobs can edit the same files without
/// clobbering each other. Because isolation needs a git repository, a non-git
/// project falls back to the shared workspace with a notice instead of failing.
/// A job that leaves changes keeps its worktree and its path is reported (with
/// the recipe to merge it back); a job that changed nothing has its worktree
/// removed.
pub(crate) async fn run_jobs(
    ctx: &ToolContext,
    targets: &[Target],
    tools: &ToolRegistry,
    limits: &DelegateLimits,
    args: &Value,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Job {
        model: String,
        task: String,
        #[serde(default)]
        context: String,
    }
    #[derive(Deserialize)]
    struct Args {
        jobs: Vec<Job>,
    }
    let args: Args = serde_json::from_value(args.clone())?;
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
    }
    let mut prepared: Vec<Prepared> = Vec::new();
    for (i, job) in args.jobs.iter().enumerate() {
        let model = job.model.trim();
        if job.task.trim().is_empty() {
            bail!("job {}: `task` must not be empty", i + 1);
        }
        let (idx, target) = targets
            .iter()
            .enumerate()
            .find(|(_, t)| t.cfg.name == model)
            .ok_or_else(|| {
                let listed = targets
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
        });
    }

    // Isolation needs a git repo; when the project is not one, fall back to the
    // shared workspace so parallel delegation still works everywhere.
    let can_isolate = crate::worktree::Worktree::isolation_available(&ctx.project_root).await;

    // Gate every job up front so a single denial aborts before anything runs.
    for (i, p) in prepared.iter().enumerate() {
        let cfg = &targets[p.idx].cfg;
        enforce_approval(
            cfg,
            ctx,
            format!(
                "Delegate a task to delegate {} (job {} of {})?",
                p.model,
                i + 1,
                prepared.len()
            ),
            Some(approval_preview(&p.prompt, "Job to delegate:")),
        )
        .await?;
    }

    let results = join_all(prepared.iter().map(|p| {
        let target = &targets[p.idx];
        let isolated = can_isolate;
        async move {
            // An isolated job runs in its own worktree; its tools, cwd and
            // system prompt are rooted there so parallel edits cannot clash.
            let worktree = if isolated {
                Some(
                    crate::worktree::Worktree::create(&ctx.project_root, next_worktree_id())
                        .await
                        .with_context(|| format!("isolating delegate {}", p.model))?,
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

    // Every job failed: report the whole batch as a tool failure (the single-job
    // case then behaves exactly like the plain `delegate` it replaced), so the
    // lead cannot mistake a batch that produced nothing for a success.
    if !results.is_empty() && results.iter().all(|r| r.is_err()) {
        let mut msg = format!("all {} delegate job(s) failed", results.len());
        for (i, r) in results.iter().enumerate() {
            if let Err(err) = r {
                msg.push_str(&format!(
                    "\n- job {} ({}): {err:#}",
                    i + 1,
                    prepared[i].model
                ));
            }
        }
        bail!("{msg}");
    }

    let mut out = format!("{} parallel delegate job(s):\n", prepared.len());
    if !can_isolate {
        out.push_str("[project is not a git repository; jobs ran in the shared workspace]\n");
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

/// Monotonic id for isolated delegate worktrees (unique within this process).
fn next_worktree_id() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    ((std::process::id() as u64) << 20) | n
}
