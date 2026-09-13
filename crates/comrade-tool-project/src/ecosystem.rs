//! Ecosystem abstraction for the `pom_*` tools.
//!
//! The project tools were Cargo-only. They now go through an [`Ecosystem`]
//! backend so the SAME tools work for Cargo today and other build ecosystems
//! (npm, Maven, Go, ...) later. A backend knows:
//!
//! - its manifest (the discriminator [`detect`] uses),
//! - how to render a compact project model (`pom_model`),
//! - how to map a logical verb (`build`/`check`/`test`/`fmt`/...) to a command,
//! - how to type-check (`check_command`), parse diagnostics
//!   (`parse_diagnostics`) and summarize test output (`simplify_tests`).
//!
//! Adding a new ecosystem means implementing this trait and one arm in
//! [`detect`]; no `pom_*` tool needs to change.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::pom;
use crate::tasks::{self, CommandLine, Resolved};

/// Logical verbs the tools expose, mapped by each backend onto real commands.
pub const VERBS: &[&str] = &[
    "build", "run", "check", "clippy", "fmt", "doc", "bench", "release",
];

/// A build ecosystem a project can belong to.
pub trait Ecosystem: Send + Sync {
    /// Short id, e.g. `cargo`.
    fn name(&self) -> &'static str;
    /// Root manifest file name, e.g. `Cargo.toml`.
    fn manifest(&self) -> &'static str;
    /// Render a compact project model (dependencies, subprojects, tasks).
    fn model(&self, root: &Path) -> Result<String>;
    /// Whether this backend understands `verb` (a logical verb or a configured
    /// task alias) for the project at `root`.
    fn supports(&self, root: &Path, verb: &str) -> bool;
    /// Resolve a logical `verb` (+ extra args, optional subproject) to a command.
    fn resolve(
        &self,
        root: &Path,
        verb: &str,
        subproject: &Option<String>,
        extra: &[String],
    ) -> Result<Resolved>;
    /// The command that resets the working tree to a canonical format.
    fn format_command(&self, root: &Path) -> Result<Resolved>;
    /// A command that type-checks WITHOUT running tests, when the ecosystem has
    /// one. `None` means "this ecosystem has no separate check step".
    fn check_command(
        &self,
        root: &Path,
        subproject: &Option<String>,
        all_targets: bool,
        extra: &[String],
    ) -> Result<Option<CommandLine>> {
        let _ = (root, subproject, all_targets, extra);
        Ok(None)
    }
    /// Parse compiler diagnostics from raw output: `(formatted errors, total)`.
    /// The default keeps the first `max` lines that look like errors, which is a
    /// reasonable fallback for toolchains without a structured diagnostic format.
    fn parse_diagnostics(&self, raw: &str, max: usize) -> (Vec<String>, usize) {
        generic_error_lines(raw, max)
    }
    /// Reduce raw test-runner output to a model-readable summary.
    fn simplify_tests(&self, raw: &str) -> String {
        raw.to_string()
    }
    /// Whether a resolved command runs tests. `pom_run_task` uses this to keep
    /// tests out of its raw-output path (they belong to `pom_run_tests`).
    fn is_test_command(&self, _line: &CommandLine) -> bool {
        false
    }
}

/// Pick the [`Ecosystem`] for `root`. Cargo is the only backend today; the
/// presence of its manifest is the discriminator. New backends are added here
/// in priority order (e.g. `package.json`, `pom.xml`, `go.mod`).
pub fn detect(root: &Path) -> Result<Box<dyn Ecosystem>> {
    let eco = Cargo;
    if root.join(eco.manifest()).is_file() {
        return Ok(Box::new(eco));
    }
    anyhow::bail!(
        "unsupported project at {}: no Cargo.toml (supported ecosystems: {})",
        root.display(),
        supported().join(", ")
    )
}

/// The list of supported ecosystems, for error messages.
pub fn supported() -> &'static [&'static str] {
    &["cargo"]
}

// ---------------------------------------------------------------------------
// Cargo backend
// ---------------------------------------------------------------------------

/// The Cargo backend (Rust). Delegates project parsing to [`crate::pom`] and
/// command resolution to [`crate::tasks::resolve`].
pub struct Cargo;

impl Ecosystem for Cargo {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn manifest(&self) -> &'static str {
        "Cargo.toml"
    }

    fn model(&self, root: &Path) -> Result<String> {
        Ok(pom::render(&pom::load(root)?))
    }

    fn supports(&self, root: &Path, verb: &str) -> bool {
        if VERBS.contains(&verb) || verb == "test" {
            return true;
        }
        // Configured aliases (`.cargo/config.toml`) count as known tasks too.
        pom::load(root)
            .map(|m| m.aliases.iter().any(|a| a.name == verb))
            .unwrap_or(false)
    }

    fn resolve(
        &self,
        root: &Path,
        verb: &str,
        subproject: &Option<String>,
        extra: &[String],
    ) -> Result<Resolved> {
        tasks::resolve(root, verb, subproject, extra)
    }

    fn format_command(&self, root: &Path) -> Result<Resolved> {
        let line = CommandLine::Program {
            program: "cargo".to_string(),
            args: vec!["fmt".to_string(), "--all".to_string()],
        };
        let describe = line.describe();
        Ok(Resolved {
            cwd: root.to_path_buf(),
            line,
            describe,
        })
    }

    fn check_command(
        &self,
        root: &Path,
        subproject: &Option<String>,
        all_targets: bool,
        extra: &[String],
    ) -> Result<Option<CommandLine>> {
        let _ = root;
        let mut args = vec!["check".to_string()];
        if let Some(m) = tasks::manifest_path_arg(subproject) {
            args.push(m);
        }
        if all_targets {
            args.push("--all-targets".to_string());
        }
        args.extend(extra.iter().cloned());
        // Structured diagnostics so the caller can extract just the errors.
        args.push("--message-format=json".to_string());
        Ok(Some(CommandLine::Program {
            program: "cargo".to_string(),
            args,
        }))
    }

    fn parse_diagnostics(&self, raw: &str, max: usize) -> (Vec<String>, usize) {
        let (errors, total) = parse_check_json(raw, max);
        if total == 0 {
            // No JSON diagnostics (e.g. the build failed before emitting any):
            // fall back to a plain error-line scan.
            return generic_error_lines(raw, max);
        }
        (errors, total)
    }

    fn simplify_tests(&self, raw: &str) -> String {
        simplify_test_output(raw)
    }

    fn is_test_command(&self, line: &CommandLine) -> bool {
        matches!(
            line,
            CommandLine::Program { program, args }
                if program == "cargo" && args.first().map(String::as_str) == Some("test")
        )
    }
}

/// Fallback diagnostic extraction for toolchains without a structured format:
/// keep the non-empty lines that look like errors, capped at `max`.
pub fn generic_error_lines(raw: &str, max: usize) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut total = 0usize;
    for line in raw.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            continue;
        }
        let l = t.to_lowercase();
        let looks_like_error = l.contains("error")
            || l.contains("cannot find")
            || l.contains("mismatched types")
            || l.contains("failed to compile");
        if looks_like_error {
            total += 1;
            if out.len() < max {
                out.push(t.to_string());
            }
        }
    }
    (out, total)
}

/// Parse `cargo check --message-format=json` output: return up to `max` formatted
/// error diagnostics and the total number of errors seen.
fn parse_check_json(raw: &str, max: usize) -> (Vec<String>, usize) {
    let mut errors = Vec::new();
    let mut total = 0usize;
    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        if msg.get("level").and_then(Value::as_str) != Some("error") {
            continue;
        }
        total += 1;
        if errors.len() < max {
            errors.push(format_diagnostic(msg));
        }
    }
    (errors, total)
}

/// Format one JSON diagnostic as `file:line:col: error[CODE]: message`.
fn format_diagnostic(msg: &Value) -> String {
    let text = msg.get("message").and_then(Value::as_str).unwrap_or("");
    let code = msg
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .map(|c| format!("[{c}]"))
        .unwrap_or_default();
    let loc = msg
        .get("spans")
        .and_then(Value::as_array)
        .and_then(|spans| {
            spans
                .iter()
                .find(|s| s.get("is_primary").and_then(Value::as_bool).unwrap_or(false))
                .or_else(|| spans.first())
        })
        .map(|s| {
            let f = s.get("file_name").and_then(Value::as_str).unwrap_or("");
            let l = s.get("line_start").and_then(Value::as_u64).unwrap_or(0);
            let c = s.get("column_start").and_then(Value::as_u64).unwrap_or(0);
            format!("{f}:{l}:{c}")
        })
        .unwrap_or_default();
    if loc.is_empty() {
        format!("error{code}: {text}")
    } else {
        format!("{loc}: error{code}: {text}")
    }
}

/// Reduce raw `cargo test` output to a readable summary for the model: keep
/// totals, failing-test sections and their detail lines; drop compile/build
/// noise and the per-test "... ok" lines.
fn simplify_test_output(raw: &str) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    for line in raw.lines() {
        let t = line.trim_start();
        if t.is_empty()
            || t.starts_with("Compiling")
            || t.starts_with("Finished")
            || t.starts_with("Running")
            || t.starts_with("Doc-tests")
            || t.starts_with("warning: ")
            || t.contains("running 0 tests")
        {
            continue;
        }
        // skip individual passing tests ("test foo ... ok")
        if let Some(rest) = t.strip_prefix("test ")
            && rest.ends_with(" ... ok")
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        kept += 1;
        if kept > 200 {
            out.push_str("... (output trimmed)\n");
            break;
        }
    }
    if out.trim().is_empty() {
        "test run produced no summary lines (check timeout or exit code)".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cargo_and_rejects_unknown() {
        let dir = std::env::temp_dir().join(format!("comrade-eco-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No manifest yet -> unsupported.
        assert!(detect(&dir).is_err());
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let eco = detect(&dir).unwrap();
        assert_eq!(eco.name(), "cargo");
        assert_eq!(eco.manifest(), "Cargo.toml");
        assert!(eco.supports(&dir, "check"));
        assert!(eco.supports(&dir, "test"));
        assert!(!eco.supports(&dir, "nope"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cargo_check_command_is_json() {
        let eco = Cargo;
        let sub = Some("crates/a".to_string());
        let cmd = eco
            .check_command(
                Path::new("/tmp/x"),
                &sub,
                true,
                &["--features".into(), "f".into()],
            )
            .unwrap()
            .unwrap();
        match cmd {
            CommandLine::Program { program, args } => {
                assert_eq!(program, "cargo");
                assert_eq!(args[0], "check");
                assert!(
                    args.contains(&"--manifest-path=crates/a/Cargo.toml".to_string()),
                    "{args:?}"
                );
                assert!(args.contains(&"--all-targets".to_string()), "{args:?}");
                assert!(args.contains(&"--features".to_string()), "{args:?}");
                assert_eq!(args.last().unwrap(), "--message-format=json");
            }
            _ => panic!("expected a program command"),
        }
    }

    #[test]
    fn cargo_is_test_command_only_for_cargo_test() {
        let eco = Cargo;
        assert!(eco.is_test_command(&CommandLine::Program {
            program: "cargo".into(),
            args: vec!["test".into(), "--lib".into()],
        }));
        assert!(!eco.is_test_command(&CommandLine::Program {
            program: "cargo".into(),
            args: vec!["check".into()],
        }));
        assert!(!eco.is_test_command(&CommandLine::Shell {
            script: "cargo test".into(),
        }));
    }

    #[test]
    fn parses_and_caps_json_diagnostics() {
        // Two compiler-message errors + noise; cap at 1.
        let raw = r#"{"reason":"compiler-artifact","package_id":"x"}
{"reason":"compiler-message","message":{"level":"error","message":"cannot find value `a`","code":{"code":"E0425"},"spans":[{"file_name":"src/main.rs","line_start":3,"column_start":5,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"warning","message":"unused","spans":[]}}
{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":null,"spans":[{"file_name":"src/lib.rs","line_start":9,"column_start":1,"is_primary":true}]}}
Compiling foo
"#;
        let (errors, total) = Cargo.parse_diagnostics(raw, 1);
        assert_eq!(total, 2);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0], "src/main.rs:3:5: error[E0425]: cannot find value `a`");

        let (all, total) = Cargo.parse_diagnostics(raw, 10);
        assert_eq!(total, 2);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], "src/lib.rs:9:1: error: mismatched types");

        // Non-JSON output falls back to the generic error-line scan.
        let (generic, total) =
            Cargo.parse_diagnostics("Compiling x\nerror: expected `;`\nwarning: unused\n", 10);
        assert_eq!(total, 1);
        assert_eq!(generic, vec!["error: expected `;`".to_string()]);
    }

    #[test]
    fn test_output_is_simplified() {
        let raw = "\
   Compiling comrade-core v0.1.0
    Finished `dev` profile [unoptimized + debuginfo]
     Running unittests src/lib.rs
running 12 tests
test foo ... ok
test bar ... FAILED

failures:
    bar

---- bar stdout ----
thread 'bar' panicked at src/lib.rs:10
note: run with `RUST_BACKTRACE=1` to see the stack trace

failures:
    bar
test result: FAILED. 11 passed; 1 failed; 0 ignored
";
        let out = Cargo.simplify_tests(raw);
        assert!(out.contains("test result: FAILED"));
        assert!(out.contains("11 passed"));
        assert!(out.contains("panicked at"));
        assert!(!out.contains("Compiling"));
        assert!(!out.contains("Finished"));
    }
}
