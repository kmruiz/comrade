//! Git worktree isolation (D2).
//!
//! A parallel delegate — or a whole TUI session — can be given its own worktree
//! so two agents that edit the same tree cannot clobber each other. The worktree
//! shares the repo's object store (cheap) but has an independent working
//! directory. The caller decides whether to keep it (to review/merge the diff)
//! or remove it.
//!
//! The API is deliberately SYNCHRONOUS: every operation is one short-lived `git`
//! subprocess, and the module is used both from the async delegate path and from
//! the TUI's synchronous key handlers, so a single implementation serves both.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// A git worktree rooted at `<repo>/.comrade/worktrees/<name>`.
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

    /// Create a DETACHED worktree of `repo` at HEAD. Fails if `repo` is not a
    /// git repository. `id` only names the directory (pass a unique value).
    /// Delegate jobs use this; their numeric ids never collide with a session's
    /// `session-<n>` directory.
    pub fn create(repo: &Path, id: u64) -> Result<Self> {
        let path = worktree_path(repo, &id.to_string());
        prepare(repo, &path)?;
        let p = path.to_string_lossy().into_owned();
        run_add(repo, &["--detach", &p, "HEAD"])?;
        Ok(Self {
            repo: repo.to_path_buf(),
            path,
        })
    }

    /// Open — or create — a session worktree on branch `branch`, cut from `base`
    /// when the branch does not exist yet.
    ///
    /// A LEFTOVER worktree or branch from a previous run is REUSED, never
    /// destroyed: it may hold unmerged commits, and both silently dropping them
    /// and silently falling back to the shared directory would lose work. Only a
    /// non-git repository or a real git failure makes this fail.
    pub fn open_on_branch(repo: &Path, name: &str, branch: &str, base: &str) -> Result<Self> {
        if !is_git_repo(repo) {
            bail!("{} is not a git repository; cannot isolate", repo.display());
        }
        let path = worktree_path(repo, name);
        // Still a live worktree of this repository: use it exactly as it stands.
        if path.join(".git").exists() && Self::is_linked(&path) {
            return Ok(Self {
                repo: repo.to_path_buf(),
                path,
            });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // A stale directory (with no live worktree behind it) must go, or
        // `worktree add` refuses.
        let _ = std::fs::remove_dir_all(&path);
        let p = path.to_string_lossy().into_owned();
        if branch_exists(repo, branch) {
            // Re-attach to the surviving branch so its commits are not stranded.
            run_add(repo, &[&p, branch])?;
        } else {
            run_add(repo, &["-b", branch, &p, base])?;
        }
        Ok(Self {
            repo: repo.to_path_buf(),
            path,
        })
    }

    /// Whether `repo` is a git repository, i.e. whether worktree isolation is
    /// possible. A non-git project must fall back to a shared workspace.
    pub fn isolation_available(repo: &Path) -> bool {
        is_git_repo(repo)
    }

    /// `true` when the worktree has uncommitted changes (so it is worth keeping).
    pub fn has_changes(&self) -> bool {
        match git_ok(&self.path, &["status", "--porcelain"]) {
            Ok(out) => !out.trim().is_empty(),
            Err(_) => false,
        }
    }

    /// Remove the worktree and prune it from the repo's admin files.
    pub fn remove(&self) -> Result<()> {
        let out = git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        )?;
        if !out.status.success() {
            bail!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// The branch currently checked out in `dir` (an empty string when the HEAD
    /// is detached, as in a delegate's worktree).
    pub fn current_branch(dir: &Path) -> Result<String> {
        Ok(git_ok(dir, &["branch", "--show-current"])?
            .trim()
            .to_string())
    }

    /// True when `dir` is a LINKED worktree rather than the repository's own
    /// checkout. A linked worktree has its own git dir (`.../.git/worktrees/<n>`)
    /// while the shared common dir stays the repository's `.git`, so the two
    /// differ; in the main checkout they are the same directory.
    pub fn is_linked(dir: &Path) -> bool {
        match (git_dir(dir, "--git-dir"), git_dir(dir, "--git-common-dir")) {
            (Some(own), Some(common)) => own != common,
            _ => false,
        }
    }

    /// The repository root that owns `dir`'s worktree: the parent of the shared
    /// git dir.
    pub fn repo_root(dir: &Path) -> Result<PathBuf> {
        let common = git_dir(dir, "--git-common-dir")
            .context("not a git repository: cannot resolve the repository root")?;
        common
            .parent()
            .map(Path::to_path_buf)
            .context("could not resolve the repository root from the common git dir")
    }

    /// Merge `branch` — the branch of a session worktree at `worktree` — into
    /// `target`, the branch checked out in the repository at `repo`.
    ///
    /// The ORDER is the safety property. The target is first folded INTO the
    /// session branch: that is the only step that can conflict, and it happens
    /// inside the isolated worktree, where aborting it costs nothing. That leaves
    /// `target` an ancestor of `branch`, so updating the repository is a plain
    /// fast-forward which cannot conflict — the repository is therefore never
    /// left mid-merge. A repository holding uncommitted changes makes the
    /// fast-forward refuse, which is equally safe: nothing is touched.
    pub fn merge_branch_into_target(
        repo: &Path,
        worktree: &Path,
        branch: &str,
        target: &str,
    ) -> Result<String> {
        if !git_ok(worktree, &["status", "--porcelain"])?
            .trim()
            .is_empty()
        {
            bail!(
                "the worktree has uncommitted changes: commit them with git_commit before \
                 merging into `{target}`"
            );
        }
        let ahead: usize = git_ok(
            worktree,
            &["rev-list", "--count", &format!("{target}..{branch}")],
        )?
        .trim()
        .parse()
        .unwrap_or(0);
        if ahead == 0 {
            return Ok(format!(
                "nothing to merge: `{branch}` has no commits beyond `{target}`"
            ));
        }
        // Fold the target into the session branch. The only step that can
        // conflict, and it is aborted inside the isolated worktree.
        if let Err(e) = git_ok(worktree, &["merge", "--no-edit", target]) {
            let _ = git_ok(worktree, &["merge", "--abort"]);
            bail!(
                "conflict merging `{target}` into `{branch}`: the merge was aborted inside the \
                 session worktree, so the repository is untouched. Resolve the conflict there, \
                 commit, and call git_merge_session again. ({e:#})"
            );
        }
        // `target` is now an ancestor of `branch`: this cannot conflict.
        git_ok(repo, &["merge", "--ff-only", branch]).map_err(|e| {
            anyhow::anyhow!(
                "could not fast-forward `{target}` to `{branch}`: {e:#}. The repository was not \
                 changed: commit or stash anything in it and retry."
            )
        })?;
        Ok(format!(
            "merged `{branch}` into `{target}` ({ahead} commit(s))"
        ))
    }
}

fn worktree_path(repo: &Path, name: &str) -> PathBuf {
    repo.join(".comrade").join("worktrees").join(name)
}

/// Prepare the worktree's target path: refuse a non-git `repo`, make the parent
/// directory and clear a stale checkout (which would make `worktree add` fail).
fn prepare(repo: &Path, path: &Path) -> Result<()> {
    if !is_git_repo(repo) {
        bail!("{} is not a git repository; cannot isolate", repo.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let _ = std::fs::remove_dir_all(path);
    Ok(())
}

fn run_add(repo: &Path, args: &[&str]) -> Result<()> {
    let mut argv = vec!["worktree", "add"];
    argv.extend_from_slice(args);
    let out = git(repo, &argv)?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn is_git_repo(repo: &Path) -> bool {
    matches!(git(repo, &["rev-parse", "--git-dir"]), Ok(o) if o.status.success())
}

/// Whether `repo` already has a local branch named `branch`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    matches!(
        git(
            repo,
            &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]
        ),
        Ok(o) if o.status.success()
    )
}

/// Resolve one of git's `--git-dir`/`--git-common-dir` answers to a comparable
/// absolute path. Git prints them relative to the working directory in the main
/// checkout (`.git`) and absolute in a linked worktree, so both are resolved
/// against `dir` and canonicalised before they are compared.
fn git_dir(dir: &Path, flag: &str) -> Option<PathBuf> {
    let raw = git_ok(dir, &["rev-parse", flag]).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        dir.join(raw)
    };
    Some(absolute.canonicalize().unwrap_or(absolute))
}

/// Run git, failing only when it could not be spawned. Callers that care about
/// the exit status inspect it themselves.
fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {args:?}"))
}

/// Run git and fail on a non-zero exit, folding its stderr into the error.
fn git_ok(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = git(cwd, args)?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

    /// Stage and commit everything in `dir`.
    fn commit_all(dir: &Path, msg: &str) {
        git_sync(dir, &["add", "-A"]);
        git_sync(dir, &["commit", "-qm", msg]);
    }

    /// The commit `dir` is on.
    fn head(dir: &Path) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
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

    #[test]
    fn isolated_writes_do_not_touch_the_main_repo() {
        let repo = scratch_repo();
        let wt = Worktree::create(&repo, 1).unwrap();
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
        assert!(wt.has_changes());

        wt.remove().unwrap();
        assert!(!wt.path().exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn non_git_dir_is_refused() {
        let dir = std::env::temp_dir().join(format!("comrade-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!Worktree::isolation_available(&dir));
        assert!(Worktree::create(&dir, 7).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_on_branch_makes_a_branch_worktree() {
        let repo = scratch_repo();
        let wt = Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        assert!(
            wt.path()
                .starts_with(repo.join(".comrade").join("worktrees"))
        );
        assert_eq!(
            Worktree::current_branch(wt.path()).unwrap(),
            "comrade/session-1"
        );
        // The repository's own checkout is untouched, still on its base branch.
        assert_eq!(Worktree::current_branch(&repo).unwrap(), "main");
        wt.remove().unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn merge_folds_the_target_in_then_fast_forwards_it() {
        let repo = scratch_repo();
        let wt = Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        // The repository moves on while the session works, so the fold is real.
        std::fs::write(repo.join("main-only.txt"), "later\n").unwrap();
        commit_all(&repo, "main moves");
        std::fs::write(wt.path().join("feat.txt"), "work\n").unwrap();
        commit_all(wt.path(), "add feat");

        let msg = Worktree::merge_branch_into_target(&repo, wt.path(), "comrade/session-1", "main")
            .unwrap();
        assert!(msg.contains("comrade/session-1"), "{msg}");
        assert!(msg.contains("1 commit"), "{msg}");

        // The work landed in the repository checkout and both refs now agree.
        assert_eq!(
            std::fs::read_to_string(repo.join("feat.txt")).unwrap(),
            "work\n"
        );
        assert!(repo.join("main-only.txt").exists());
        assert_eq!(head(&repo), head(wt.path()));
        wt.remove().unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_conflict_aborts_and_leaves_the_target_branch_untouched() {
        let repo = scratch_repo();
        let wt = Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        std::fs::write(wt.path().join("readme.txt"), "session edit\n").unwrap();
        commit_all(wt.path(), "session edit");
        // The same file changes on the repository side.
        std::fs::write(repo.join("readme.txt"), "main edit\n").unwrap();
        commit_all(&repo, "main edit");
        let before = head(&repo);

        let err = Worktree::merge_branch_into_target(&repo, wt.path(), "comrade/session-1", "main")
            .unwrap_err();

        assert!(err.to_string().contains("conflict"), "{err}");
        // The repository is exactly as it was: same commit, same file, no merge
        // left in progress.
        assert_eq!(head(&repo), before);
        assert_eq!(
            std::fs::read_to_string(repo.join("readme.txt")).unwrap(),
            "main edit\n"
        );
        assert!(!repo.join(".git").join("MERGE_HEAD").exists());
        // The worktree was cleaned up too, so a retry starts from a clean tree.
        assert!(!wt.has_changes());
        wt.remove().unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn merge_refuses_uncommitted_work_and_changes_nothing() {
        let repo = scratch_repo();
        let wt = Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        std::fs::write(wt.path().join("dirty.txt"), "not committed\n").unwrap();
        let before = head(&repo);

        let err = Worktree::merge_branch_into_target(&repo, wt.path(), "comrade/session-1", "main")
            .unwrap_err();

        assert!(err.to_string().contains("commit"), "{err}");
        assert_eq!(head(&repo), before);
        assert!(!repo.join("dirty.txt").exists());
        wt.remove().unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn merge_with_no_new_commits_is_a_no_op() {
        let repo = scratch_repo();
        let wt = Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        let before = head(&repo);

        let msg = Worktree::merge_branch_into_target(&repo, wt.path(), "comrade/session-1", "main")
            .unwrap();

        assert!(msg.contains("nothing to merge"), "{msg}");
        assert_eq!(head(&repo), before, "nothing to merge means no new commit");
        wt.remove().unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn open_on_branch_reuses_a_leftover_worktree_and_keeps_its_commits() {
        let repo = scratch_repo();
        {
            // A session that ends without merging leaves its worktree behind,
            // exactly as an app exit does.
            let wt =
                Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
            std::fs::write(wt.path().join("wip.txt"), "unmerged\n").unwrap();
            commit_all(wt.path(), "wip");
        }
        // The next run of that session reopens it instead of losing the work.
        let again =
            Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        assert!(again.path().join("wip.txt").exists());
        assert_eq!(
            Worktree::current_branch(again.path()).unwrap(),
            "comrade/session-1"
        );
        // ...and the leftover work can still be published.
        let msg =
            Worktree::merge_branch_into_target(&repo, again.path(), "comrade/session-1", "main")
                .unwrap();
        assert!(msg.contains("1 commit"), "{msg}");
        again.remove().unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn open_on_branch_reattaches_a_branch_whose_checkout_is_gone() {
        let repo = scratch_repo();
        let wt = Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        std::fs::write(wt.path().join("wip.txt"), "unmerged\n").unwrap();
        commit_all(wt.path(), "wip");
        // The checkout goes away but the branch (and its commit) survives.
        wt.remove().unwrap();

        let again =
            Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        assert!(
            again.path().join("wip.txt").exists(),
            "the branch's commits must be checked out again, not recreated empty"
        );
        again.remove().unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn is_linked_tells_a_worktree_from_the_main_checkout() {
        let repo = scratch_repo();
        assert!(
            !Worktree::is_linked(&repo),
            "the main checkout is not linked"
        );
        let wt = Worktree::open_on_branch(&repo, "session-1", "comrade/session-1", "main").unwrap();
        assert!(Worktree::is_linked(wt.path()), "a worktree is linked");
        assert_eq!(
            Worktree::repo_root(wt.path()).unwrap(),
            repo.canonicalize().unwrap()
        );
        wt.remove().unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }
}
