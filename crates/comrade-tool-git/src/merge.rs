//! `git_merge_session`: hand a session's isolated worktree back to the
//! repository's main branch, once the work is committed and verified.
//!
//! A session edits its own worktree on its own branch, so the repository is
//! untouched while it works. This tool is how that work is published: the target
//! branch is folded into the session branch first (the only place a conflict can
//! appear, and it is aborted there), then the target is fast-forwarded. See
//! [`comrade_core::worktree::Worktree::merge_branch_into_target`].

use anyhow::{Result, bail};
use async_trait::async_trait;
use comrade_core::worktree::Worktree;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde_json::{Value, json};

pub(crate) struct GitMergeSession;

static GIT_MERGE_SESSION_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| {
    ToolSpec {
    name: "git_merge_session".into(),
    description: "Merge this session's isolated worktree branch into the repository's main branch once the work is committed and verified. The main branch is folded into your branch first, so a conflict is resolved inside your worktree before the main branch is touched; the main branch is then fast-forwarded. Refuses while the worktree has uncommitted changes - run git_commit first.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for GitMergeSession {
    fn spec(&self) -> &ToolSpec {
        &GIT_MERGE_SESSION_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, _args: Value) -> Result<String> {
        if !Worktree::is_linked(&ctx.project_root) {
            bail!(
                "git_merge_session: this session is not running in an isolated worktree, so \
                 there is nothing to merge."
            );
        }
        let repo = Worktree::repo_root(&ctx.project_root)?;
        let branch = Worktree::current_branch(&ctx.project_root)?;
        let target = Worktree::current_branch(&repo)?;
        // Publishing to the repository is a mutating operation: ask first.
        ctx.confirm(format!("Merge branch `{branch}` into `{target}`?"), None)
            .await?;
        let msg = Worktree::merge_branch_into_target(&repo, &ctx.project_root, &branch, &target)?;
        Ok(crate::clamp(msg))
    }
}
