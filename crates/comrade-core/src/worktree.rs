//! Git worktree isolation (D2).
//!
//! A parallel delegate can be given its own detached worktree so two jobs that
//! edit the same tree cannot clobber each other. The worktree shares the repo's
//! object store (cheap) but has an independent working directory. The caller
//! decides whether to keep it (to review/merge the diff) or remove it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// A detached git worktree rooted at `<repo>/.comrade/worktrees/<id>`.
#[derive(Clone, Debug)]
pub struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

impl Worktree {
    /// The worktree's filesystem path (use it as a job's project root).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create a detached worktree of `repo` at HEAD. Fails if `repo` is not a
    /// git repository. `id` only names the directory (pass a unique value).
    pub async fn create(repo: &Path, id: u64) -> Result<Self> {
        if !is_git_repo(repo).await {
            bail!("{} is not a git repository; cannot isolate", repo.display());
        }
        let path = repo.join(".comrade").join("worktrees").join(id.to_string());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // A stale dir from a previous run would make `worktree add` fail.
        let _ = std::fs::remove_dir_all(&path);
        let out = git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                &path.to_string_lossy(),
                "HEAD",
            ],
        )
        .await?;
        if !out.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Self {
            repo: repo.to_path_buf(),
            path,
        })
    }

    /// `true` when the worktree has uncommitted changes (so it is worth keeping).
    pub async fn has_changes(&self) -> bool {
        match git(&self.path, &["status", "--porcelain"]).await {
            Ok(out) => !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            Err(_) => false,
        }
    }

    /// Remove the worktree and prune it from the repo's admin files.
    pub async fn remove(&self) -> Result<()> {
        let out = git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        )
        .await?;
        if !out.status.success() {
            bail!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

async fn is_git_repo(repo: &Path) -> bool {
    matches!(git(repo, &["rev-parse", "--git-dir"]).await, Ok(o) if o.status.success())
}

async fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("running git {args:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_sync(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn scratch_repo() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "comrade-worktree-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        git_sync(&root, &["init", "-q", "-b", "main"]);
        git_sync(&root, &["config", "user.email", "t@example.com"]);
        git_sync(&root, &["config", "user.name", "t"]);
        git_sync(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("readme.txt"), "hello\n").unwrap();
        git_sync(&root, &["add", "-A"]);
        git_sync(&root, &["commit", "-qm", "init"]);
        root
    }

    #[tokio::test]
    async fn isolated_writes_do_not_touch_the_main_repo() {
        let repo = scratch_repo();
        let wt = Worktree::create(&repo, 1).await.unwrap();
        assert!(
            wt.path()
                .starts_with(repo.join(".comrade").join("worktrees"))
        );

        std::fs::write(wt.path().join("scratch.txt"), "inside\n").unwrap();
        assert!(wt.path().join("scratch.txt").exists());
        assert!(
            !repo.join("scratch.txt").exists(),
            "a worktree write must not appear in the main tree"
        );
        assert!(wt.has_changes().await);

        wt.remove().await.unwrap();
        assert!(!wt.path().exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn non_git_dir_is_refused() {
        let dir = std::env::temp_dir().join(format!("comrade-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Worktree::create(&dir, 7).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
