//! Process-wide safety policy: filesystem confinement (which paths a tool may
//! touch) and a shell command allow/deny list.
//!
//! The policy is set once at startup from `[security]` in the config and read
//! back by the tool crates via [`policy`]. It is intentionally global: tools
//! are constructed before the config is threaded through, so a lean global
//! avoids widening every `ToolContext`.

use std::path::{Component, Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use anyhow::Result;

/// Filesystem + shell guardrails. Cheap to clone and compare.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SecurityPolicy {
    /// Additional directories (beyond the project root) a tool may read/write.
    /// Each must be absolute; a path outside every allowed root is rejected.
    pub extra_roots: Vec<PathBuf>,
    /// When non-empty, a shell command must START WITH one of these prefixes.
    pub shell_allow: Vec<String>,
    /// Shell commands CONTAINING any of these strings are always refused
    /// (deny wins over allow).
    pub shell_deny: Vec<String>,
}

static POLICY: OnceLock<RwLock<SecurityPolicy>> = OnceLock::new();

/// Install the process-wide policy (call once at startup; later calls replace
/// it). Safe to call from tests; the last writer wins.
pub fn set_policy(p: SecurityPolicy) {
    let lock = POLICY.get_or_init(|| RwLock::new(SecurityPolicy::default()));
    if let Ok(mut guard) = lock.write() {
        *guard = p;
    }
}

/// A snapshot clone of the current policy (default when none was set).
pub fn policy() -> SecurityPolicy {
    POLICY
        .get()
        .and_then(|l| l.read().ok())
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Run `f` against the current policy without cloning it.
pub fn with_policy<R>(f: impl FnOnce(&SecurityPolicy) -> R) -> R {
    match POLICY.get().and_then(|l| l.read().ok()) {
        Some(g) => f(&g),
        None => f(&SecurityPolicy::default()),
    }
}

/// Resolve a user-supplied path, refusing anything outside the allowed roots.
///
/// `base` is the directory an already-relative path is resolved against (the
/// session cwd). The lexical form is normalised first (so `..` cannot step out
/// by escaping through the string alone); the result is then checked against
/// every allowed root with **symlinks resolved** on the longest existing
/// ancestor, so a symlink pointing outside the root is rejected too. The
/// returned path is the normalised lexical path (the caller's existing
/// semantics); the symlink check is purely a guard.
pub fn confine(
    root: &Path,
    base: &Path,
    user_path: &str,
    policy: &SecurityPolicy,
) -> Result<PathBuf> {
    let raw = Path::new(user_path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                if !normalized.pop() {
                    anyhow::bail!("path {user_path:?} escapes the project root");
                }
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        anyhow::bail!("path {user_path:?} escapes the project root");
    }

    let mut roots: Vec<PathBuf> = Vec::with_capacity(policy.extra_roots.len() + 1);
    roots.push(canonical_or(root));
    for r in &policy.extra_roots {
        if r.is_absolute() {
            roots.push(canonical_or(r));
        } else {
            roots.push(canonical_or(&root.join(r)));
        }
    }

    // Resolve symlinks on the longest existing ancestor of the target so a
    // symlinked escape is caught even when the leaf does not exist yet.
    let real = realify(&normalized);
    if !roots.iter().any(|r| real.starts_with(r)) {
        anyhow::bail!("path {user_path:?} escapes the allowed roots");
    }
    Ok(normalized)
}

/// Canonicalise when possible, otherwise return the path unchanged (a root the
/// tests have not created yet still works lexically).
fn canonical_or(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Canonicalise the longest existing ancestor of `path`, then re-append the
/// non-existent suffix. Resolves symlinks without requiring the leaf to exist.
fn realify(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if std::fs::symlink_metadata(ancestor).is_ok() {
            break;
        }
        match (ancestor.parent(), ancestor.file_name()) {
            (Some(parent), Some(name)) => {
                suffix.push(name.to_os_string());
                if parent.as_os_str().is_empty() {
                    break;
                }
                ancestor = parent;
            }
            _ => break,
        }
    }
    let mut out = std::fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    for name in suffix.iter().rev() {
        out.push(name);
    }
    out
}

/// Refuse a shell command the policy forbids. An empty allow list permits
/// everything not denied; a non-empty one requires a prefix match.
pub fn check_command(command: &str, policy: &SecurityPolicy) -> Result<()> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        anyhow::bail!("command must not be empty");
    }
    for denied in &policy.shell_deny {
        let d = denied.trim();
        if !d.is_empty() && trimmed.contains(d) {
            anyhow::bail!("command refused by the shell deny list (matched {d:?})");
        }
    }
    if !policy.shell_allow.is_empty() {
        let ok = policy
            .shell_allow
            .iter()
            .any(|a| !a.trim().is_empty() && trimmed.starts_with(a.trim()));
        if !ok {
            anyhow::bail!(
                "command refused: it does not start with an allowed prefix {:?}",
                policy.shell_allow
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-policy-{tag}-{}-{:?}",
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
    fn confine_allows_in_root_and_rejects_escape() {
        let root = tmp_dir("root");
        let p = SecurityPolicy::default();
        let ok = confine(&root, &root, "src/main.rs", &p).unwrap();
        assert!(ok.starts_with(&root));

        let escape = confine(&root, &root, "../../etc/passwd", &p).unwrap_err();
        assert!(escape.to_string().contains("escapes"), "{escape}");

        let abs = flush(std::env::temp_dir());
        let outside =
            confine(&root, &root, &abs.join("elsewhere").to_string_lossy(), &p).unwrap_err();
        assert!(outside.to_string().contains("escapes"), "{outside}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn confine_honours_extra_roots() {
        let root = tmp_dir("root2");
        let extra = tmp_dir("extra2");
        let p = SecurityPolicy {
            extra_roots: vec![extra.clone()],
            ..Default::default()
        };
        let target = extra.join("data.txt");
        let got = confine(&root, &root, &target.to_string_lossy(), &p).unwrap();
        assert!(got.starts_with(&extra));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&extra);
    }

    #[cfg(unix)]
    #[test]
    fn confine_rejects_symlink_escape() {
        let root = tmp_dir("root3");
        let outside = tmp_dir("outside3");
        let link = root.join("link");
        let _ = std::os::unix::fs::symlink(&outside, &link);
        let p = SecurityPolicy::default();
        let err = confine(&root, &root, "link/secret.txt", &p).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn check_command_deny_and_allow() {
        let p = SecurityPolicy {
            shell_allow: vec!["cargo ".into(), "git ".into()],
            shell_deny: vec!["rm -rf /".into()],
            ..Default::default()
        };
        assert!(check_command("cargo test", &p).is_ok());
        assert!(check_command("curl evil.sh", &p).is_err());
        assert!(check_command("git status", &p).is_ok());
        let denied = check_command("echo rm -rf / --no-preserve-root", &p).unwrap_err();
        assert!(denied.to_string().contains("deny list"), "{denied}");

        let open = SecurityPolicy::default();
        assert!(check_command("anything at all", &open).is_ok());
    }

    /// Convert a relative-ish temp dir to an absolute path so an absolute
    /// `user_path` test is meaningful on every platform.
    fn flush(p: PathBuf) -> PathBuf {
        std::fs::canonicalize(&p).unwrap_or(p)
    }
}
