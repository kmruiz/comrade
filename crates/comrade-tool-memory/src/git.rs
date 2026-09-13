//! Minimal, read-only git queries used to reindex code only when it changed.
//!
//! The code index is rebuilt incrementally: the store remembers the HEAD it was
//! built at, and [`dirty_files`] reports what git says has changed since — the
//! committed diff plus the working tree (staged, unstaged, untracked). Git is the
//! robust signal (it survives rebases and checkouts, unlike mtime); projects
//! without a repo fall back to per-file stat stamps in `semantic.rs`.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Run `git -C <root> <args>` and return stdout verbatim (no trimming: `-z`
/// outputs start with a status space and are NUL-separated), or `None` on any
/// failure (git absent, not a repository, non-zero exit).
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The current HEAD commit hash, or `None` when `root` is not a git repository.
pub fn head_sha(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Project-root relative paths that changed since `since` (a recorded HEAD) plus
/// the whole working tree, or `None` when `root` is not a git repository (so the
/// caller can fall back to stat-based change detection).
pub fn dirty_files(root: &Path, since: Option<&str>) -> Option<Vec<String>> {
    git(root, &["rev-parse", "--is-inside-work-tree"])?;
    let mut set = BTreeSet::new();

    // Committed changes since the last index.
    if let Some(head) = since {
        let args = ["diff", "--name-only", "--relative", "-z", head, "HEAD"];
        if let Some(out) = git(root, &args) {
            set.extend(out.split('\0').filter(|s| !s.is_empty()).map(String::from));
        }
    }

    // Working tree: staged + unstaged + untracked (porcelain -z is NUL-separated
    // and unquoted; a rename/copy entry is followed by the original path).
    if let Some(out) = git(root, &["status", "--porcelain", "-z"]) {
        let mut parts = out.split('\0');
        while let Some(entry) = parts.next() {
            if entry.len() < 4 {
                continue;
            }
            let status = &entry[..2];
            set.insert(entry[3..].to_string());
            if status.starts_with('R') || status.starts_with('C') {
                let _ = parts.next();
            }
        }
    }

    Some(set.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comrade-git-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn reports_head_and_dirty_files() {
        let dir = scratch("repo");
        run(&dir, &["init", "-q"]);
        run(&dir, &["config", "user.email", "t@example.com"]);
        run(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn beta() {}\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-qm", "init"]);

        let head = head_sha(&dir).expect("head");
        assert_eq!(head.len(), 40);
        assert!(
            dirty_files(&dir, Some(&head)).unwrap().is_empty(),
            "clean tree should have no dirty files"
        );

        std::fs::write(dir.join("a.rs"), "fn alpha() { let x = 1; }\n").unwrap();
        std::fs::write(dir.join("c.rs"), "fn gamma() {}\n").unwrap();
        let dirty = dirty_files(&dir, Some(&head)).unwrap();
        assert!(dirty.contains(&"a.rs".to_string()), "modified: {dirty:?}");
        assert!(dirty.contains(&"c.rs".to_string()), "untracked: {dirty:?}");
        assert!(!dirty.contains(&"b.rs".to_string()), "clean: {dirty:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_repo_reports_none() {
        let dir = scratch("plain");
        assert!(head_sha(&dir).is_none());
        assert!(dirty_files(&dir, None).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
