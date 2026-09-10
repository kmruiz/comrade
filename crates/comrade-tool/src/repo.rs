use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

/// Run `git` inside `root`, returning stdout bytes.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("failed to spawn git")?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Absolute paths of every file that differs from `HEAD` under `root`:
/// staged, unstaged, and untracked changes (deletions are excluded since the
/// files no longer exist).
///
/// `root` must be inside a git repository. Works whether `root` is the repo
/// top-level or a subdirectory.
pub fn changed_files_abs(root: &Path) -> Result<HashSet<PathBuf>> {
    let top_raw = git(root, &["rev-parse", "--show-toplevel"])
        .map_err(|e| anyhow::anyhow!("git_modified_only requires a git repository: {e:#}"))?;
    let top = PathBuf::from(String::from_utf8_lossy(&top_raw).trim().to_string());

    let out = git(
        root,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )?;

    let mut files = HashSet::new();
    let mut rename_next = false;
    for field in out.split(|&b| b == 0) {
        if field.is_empty() {
            continue;
        }
        let Ok(s) = std::str::from_utf8(field) else {
            continue;
        };
        if rename_next {
            files.insert(top.join(s));
            rename_next = false;
            continue;
        }
        if s.len() < 3 {
            continue;
        }
        let (xy, rest) = s.split_at(2);
        let x = xy.as_bytes()[0];
        let y = xy.as_bytes()[1];
        let path = rest.trim_start_matches(' ');
        if x == b'D' || y == b'D' {
            continue; // deleted: file no longer exists
        }
        if x == b'R' || x == b'C' {
            // Rename/copy: the old path is in this field; the new path is the
            // following NUL-delimited field.
            rename_next = true;
            continue;
        }
        files.insert(top.join(path));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-repo-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_changes_vs_head() {
        let root = scratch();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "t@example.com"]);
        run(&root, &["config", "user.name", "t"]);
        // Never inherit the developer's global commit signing (e.g. 1Password),
        // which would fail/block a scratch-repo commit.
        run(&root, &["config", "commit.gpgsign", "false"]);

        std::fs::write(root.join("keep.txt"), "k\n").unwrap();
        std::fs::write(root.join("dirty.txt"), "a\n").unwrap();
        std::fs::write(root.join("gone.txt"), "g\n").unwrap();
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-qm", "init"]);

        // modify, delete, and add-untracked
        std::fs::write(root.join("dirty.txt"), "b\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::write(root.join("new.rs"), "fn new() {}\n").unwrap();

        let files = changed_files_abs(&root).unwrap();
        assert!(files.contains(&root.join("dirty.txt")), "{files:?}");
        assert!(files.contains(&root.join("new.rs")), "{files:?}");
        assert!(!files.contains(&root.join("keep.txt")));
        assert!(
            !files.contains(&root.join("gone.txt")),
            "deleted file must be excluded"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn errors_outside_a_repo() {
        let dir = std::env::temp_dir().join("comrade-no-repo-test");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(changed_files_abs(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
