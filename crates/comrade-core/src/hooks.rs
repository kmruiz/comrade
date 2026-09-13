//! Pre/post-tool shell hooks (D1).
//!
//! A hook is a small shell command the user wires to tool calls in the config:
//!
//! ```toml
//! [[hooks.pre_tool]]
//! on = "fs_edit"      # "*" (all), "fs_edit" (exact), or "fs_*" (prefix)
//! run = "cargo fmt --check"
//! ```
//!
//! Each hook runs via `bash -c` in the project root with `COMRADE_TOOL` and
//! `COMRADE_ARGS` (the JSON arguments) exported — and `COMRADE_OK` for post
//! hooks. A pre-hook that exits non-zero ABORTS the tool call; a post-hook's
//! non-zero exit is surfaced as a warning on the result. Hooks fire for the main
//! agent's tool calls (see `agent::Dispatch`); delegate sub-agent runs do not
//! run hooks.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::config::{HookCfg, HooksCfg};

/// The configured hook set, matched per tool call.
#[derive(Clone, Debug, Default)]
pub struct Hooks {
    pre: Vec<HookCfg>,
    post: Vec<HookCfg>,
}

impl Hooks {
    pub fn from_cfg(cfg: &HooksCfg) -> Self {
        Self {
            pre: cfg.pre_tool.clone(),
            post: cfg.post_tool.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }

    /// Run every matching pre-hook. The first non-zero exit aborts the tool.
    pub async fn pre(&self, root: &Path, tool: &str, args: &Value) -> Result<()> {
        for hook in self.pre.iter().filter(|h| matches(&h.on, tool)) {
            let out = run(root, hook, tool, args, None).await?;
            if !out.status.success() {
                anyhow::bail!(
                    "pre-tool hook `{}` failed for {tool}: {}",
                    hook.run,
                    first_line(&out.stderr, &out.stdout)
                );
            }
        }
        Ok(())
    }

    /// Run every matching post-hook. A non-zero exit is returned as a warning
    /// string to append to the tool result (`None` when all hooks passed).
    pub async fn post(&self, root: &Path, tool: &str, args: &Value, ok: bool) -> Option<String> {
        let mut warnings = Vec::new();
        for hook in self.post.iter().filter(|h| matches(&h.on, tool)) {
            match run(root, hook, tool, args, Some(ok)).await {
                Ok(out) if out.status.success() => {}
                Ok(out) => warnings.push(format!(
                    "post-tool hook `{}` failed: {}",
                    hook.run,
                    first_line(&out.stderr, &out.stdout)
                )),
                Err(e) => warnings.push(format!("post-tool hook `{}` error: {e:#}", hook.run)),
            }
        }
        if warnings.is_empty() {
            None
        } else {
            Some(warnings.join("\n"))
        }
    }
}

/// Does a hook's `on` expression match this tool name?
fn matches(on: &str, tool: &str) -> bool {
    match on.trim() {
        "*" => true,
        s if s.ends_with('*') => tool.starts_with(&s[..s.len() - 1]),
        s => s == tool,
    }
}

struct HookOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

async fn run(
    root: &Path,
    hook: &HookCfg,
    tool: &str,
    args: &Value,
    ok: Option<bool>,
) -> Result<HookOutput> {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(&hook.run)
        .current_dir(root)
        .env("COMRADE_TOOL", tool)
        .env(
            "COMRADE_ARGS",
            serde_json::to_string(args).unwrap_or_default(),
        )
        .kill_on_drop(true);
    if let Some(ok) = ok {
        cmd.env("COMRADE_OK", if ok { "1" } else { "0" });
    }
    let out = cmd.output().await?;
    Ok(HookOutput {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

fn first_line(stderr: &str, stdout: &str) -> String {
    let src = if !stderr.is_empty() { stderr } else { stdout };
    src.lines().next().unwrap_or("(no output)").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HookCfg, HooksCfg};

    fn root() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    fn hooks(pre: Vec<HookCfg>, post: Vec<HookCfg>) -> Hooks {
        Hooks::from_cfg(&HooksCfg {
            pre_tool: pre,
            post_tool: post,
        })
    }

    #[test]
    fn matcher_handles_star_exact_and_prefix() {
        assert!(matches("*", "anything"));
        assert!(matches("fs_edit", "fs_edit"));
        assert!(!matches("fs_edit", "fs_read_file"));
        assert!(matches("fs_*", "fs_write_file"));
        assert!(!matches("fs_*", "git_status"));
    }

    #[tokio::test]
    async fn failing_pre_hook_aborts() {
        let h = hooks(
            vec![HookCfg {
                on: "*".into(),
                run: "echo boom 1>&2; exit 3".into(),
            }],
            vec![],
        );
        let err = h.pre(&root(), "fs_edit", &Value::Null).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[tokio::test]
    async fn passing_pre_hook_is_silent_and_post_failure_warns() {
        let h = hooks(
            vec![HookCfg {
                on: "fs_*".into(),
                run: "true".into(),
            }],
            vec![HookCfg {
                on: "fs_edit".into(),
                run: "exit 1".into(),
            }],
        );
        assert!(h.pre(&root(), "fs_edit", &Value::Null).await.is_ok());
        let warn = h.post(&root(), "fs_edit", &Value::Null, true).await;
        assert!(warn.is_some(), "a failing post hook must warn");
        // Non-matching tool: no hook fires.
        assert!(
            h.post(&root(), "git_status", &Value::Null, true)
                .await
                .is_none()
        );
    }
}
