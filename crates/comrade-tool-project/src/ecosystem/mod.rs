//! Ecosystem abstraction for the `pom_*` tools.
//!
//! The project tools go through an [`Ecosystem`] backend so the SAME tools work
//! for every supported build system: Cargo (Rust) and npm (Node/TypeScript)
//! today, others (Maven, Go, ...) later. A backend knows:
//!
//! - its manifest (the discriminator [`detect_all`]/[`detect`] use),
//! - how to render a compact project model (`pom_model`),
//! - how to map a logical verb (`build`/`check`/`test`/`fmt`/...) to a command,
//! - how to type-check (`check_command`), parse diagnostics
//!   (`parse_diagnostics`) and summarize test output (`simplify_tests`).
//!
//! A repository may host several ecosystems at once (e.g. a Rust workspace with
//! a `package.json` frontend); [`detect_all`] returns all of them and [`pick`]
//! chooses one. Adding a new ecosystem means implementing this trait and one
//! arm in [`detect_all`]; no `pom_*` tool needs to change.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::node;
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
    /// Reduce raw test-runner output to a model-readable summary. The default
    /// keeps the generic test-result lines; when there are none it falls back to
    /// this backend's diagnostics — a suite that reports no results almost always
    /// failed to BUILD, and a model reads "no summary lines" as "the tests are
    /// fine". Backends with a richer test format override this to parse their
    /// runner's output and hand the result to [`Ecosystem::compose_test_summary`].
    fn simplify_tests(&self, raw: &str) -> String {
        self.compose_test_summary(raw, simplify_test_output(raw))
    }

    /// Finish a test summary, adding the shared "the tests did not build" fallback:
    /// a runner that prints no results at all (empty `summary`) plus parseable
    /// diagnostics means the build failed, so hand the model the errors instead of
    /// a message it reads as success.
    fn compose_test_summary(&self, raw: &str, summary: String) -> String {
        if !summary.trim().is_empty() {
            return summary;
        }
        let (errors, _) = self.parse_diagnostics(raw, 20);
        if !errors.is_empty() {
            return format!(
                "the tests could not build - fix these errors first, then rerun. Each error names \
                 the file:line it comes from; edit exactly there:\n{}",
                errors.join("\n")
            );
        }
        "test run produced no summary lines (check timeout or exit code)".to_string()
    }
    /// Whether `dir` is a root this backend owns, i.e. it holds the backend's
    /// manifest. Used to reject an invented `subproject` before a command is
    /// attempted in it. Works for every backend because it is derived from
    /// [`Ecosystem::manifest`].
    fn is_project_dir(&self, dir: &Path) -> bool {
        dir.join(self.manifest()).is_file()
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
/// Every [`Ecosystem`] present at `root`, in priority order (Cargo first). A
/// repository may host several at once — e.g. a Rust workspace with a
/// `package.json` for its frontend — so this returns ALL matching backends; a
/// caller picks one via [`pick`] when more than one is present.
pub fn detect_all(root: &Path) -> Vec<Box<dyn Ecosystem>> {
    let mut out: Vec<Box<dyn Ecosystem>> = Vec::new();
    if root.join(Cargo.manifest()).is_file() {
        out.push(Box::new(Cargo));
    }
    if root.join(Node.manifest()).is_file() {
        out.push(Box::new(Node));
    }
    out
}

/// The default ambient [`Ecosystem`] for `root`, erroring when the directory
/// belongs to no supported build system. When several are present the first
/// (highest priority) wins; use [`pick`] to choose explicitly or by verb.
pub fn detect(root: &Path) -> Result<Box<dyn Ecosystem>> {
    if let Some(eco) = detect_all(root).into_iter().next() {
        return Ok(eco);
    }
    anyhow::bail!(
        "unsupported project at {}: no manifest found (looked for Cargo.toml, package.json; supported ecosystems: {})",
        root.display(),
        supported().join(", ")
    )
}

/// The list of supported ecosystems, for error messages.
pub fn supported() -> &'static [&'static str] {
    &["cargo", "npm"]
}

/// Choose the [`Ecosystem`] for `root` to run `verb`, resolving a polyglot
/// (multi-ecosystem) repository unambiguously:
///
/// - `ecosystem`, when given, names the backend (`"cargo"`/`"npm"`); an unknown
///   or absent name errors with the available list.
/// - otherwise, when the project has exactly one backend, that one is used;
/// - otherwise the single backend that `supports` `verb` wins; if several do (or
///   none), the error asks the caller to pass `ecosystem` explicitly.
pub fn pick(root: &Path, ecosystem: Option<&str>, verb: &str) -> Result<Box<dyn Ecosystem>> {
    let all = detect_all(root);
    if all.is_empty() {
        anyhow::bail!(
            "unsupported project at {}: no manifest found (looked for Cargo.toml, package.json; supported ecosystems: {})",
            root.display(),
            supported().join(", ")
        );
    }
    if let Some(name) = ecosystem.map(str::trim).filter(|n| !n.is_empty()) {
        return all
            .into_iter()
            .find(|e| e.name().eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                let have: Vec<&str> = detect_all(root).iter().map(|e| e.name()).collect();
                anyhow::anyhow!(
                    "ecosystem {name:?} is not present at this project; available: {}",
                    have.join(", ")
                )
            });
    }
    if all.len() == 1 {
        return Ok(all.into_iter().next().unwrap());
    }
    let supporting: Vec<Box<dyn Ecosystem>> =
        all.into_iter().filter(|e| e.supports(root, verb)).collect();
    match supporting.len() {
        0 => {
            let have: Vec<&str> = detect_all(root).iter().map(|e| e.name()).collect();
            anyhow::bail!(
                "no ecosystem at this project supports task {verb:?}; available: {}",
                have.join(", ")
            );
        }
        1 => Ok(supporting.into_iter().next().unwrap()),
        _ => {
            let names: Vec<&str> = supporting.iter().map(|e| e.name()).collect();
            anyhow::bail!(
                "task {verb:?} is ambiguous across ecosystems ({}); pass ecosystem = \"cargo\" or \"npm\"",
                names.join(", ")
            );
        }
    }
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

    fn is_test_command(&self, line: &CommandLine) -> bool {
        matches!(
            line,
            CommandLine::Program { program, args }
                if program == "cargo" && args.first().map(String::as_str) == Some("test")
        )
    }
}

// ---------------------------------------------------------------------------
// Node / npm backend
// ---------------------------------------------------------------------------

/// The Node/npm backend (`package.json` + npm scripts). Maps a logical verb onto
/// an existing npm script and runs it with `npm run <script>`.
pub struct Node;

/// The npm script a logical verb should run: a script literally named the verb
/// wins, otherwise a small set of conventional aliases.
fn npm_script_for(m: &node::NodeModel, verb: &str) -> Option<String> {
    let has = |name: &str| m.root_package.scripts.iter().any(|s| s.name == name);
    if has(verb) {
        return Some(verb.to_string());
    }
    let candidates: &[&str] = match verb {
        "run" => &["start", "dev", "serve"],
        "build" => &["build", "compile"],
        "test" => &["test"],
        "check" => &["typecheck", "check", "lint"],
        "fmt" => &["format", "fmt", "prettier"],
        "doc" => &["doc", "docs"],
        "bench" => &["bench", "benchmark"],
        "release" => &["release", "publish"],
        "clippy" => &["lint", "typecheck"],
        _ => &[],
    };
    candidates.iter().find(|c| has(c)).map(|c| (*c).to_string())
}

fn script_names(m: &node::NodeModel) -> String {
    let names: Vec<&str> = m
        .root_package
        .scripts
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

impl Ecosystem for Node {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn manifest(&self) -> &'static str {
        "package.json"
    }

    fn model(&self, root: &Path) -> Result<String> {
        Ok(node::render(&node::load(root)?))
    }

    fn supports(&self, root: &Path, verb: &str) -> bool {
        if verb == "test" {
            return true;
        }
        let Ok(m) = node::load(root) else {
            return false;
        };
        npm_script_for(&m, verb).is_some()
    }

    fn resolve(
        &self,
        root: &Path,
        verb: &str,
        subproject: &Option<String>,
        extra: &[String],
    ) -> Result<Resolved> {
        let m = node::load(root)?;
        let script = npm_script_for(&m, verb).ok_or_else(|| {
            anyhow::anyhow!(
                "no npm script for task {verb:?}; runnable scripts: {}",
                script_names(&m)
            )
        })?;
        let mut args: Vec<String> = Vec::new();
        if let Some(dir) = subproject {
            let dir = dir.trim_end_matches('/');
            if dir.contains("..") {
                anyhow::bail!("subproject {dir:?} escapes the project root");
            }
            args.push("--prefix".to_string());
            args.push(dir.to_string());
        }
        args.push("run".to_string());
        args.push(script);
        if !extra.is_empty() {
            args.push("--".to_string());
            args.extend(extra.iter().cloned());
        }
        let line = CommandLine::Program {
            program: "npm".to_string(),
            args,
        };
        let describe = line.describe();
        Ok(Resolved {
            cwd: root.to_path_buf(),
            line,
            describe,
        })
    }

    fn format_command(&self, root: &Path) -> Result<Resolved> {
        let line = CommandLine::Program {
            program: "npx".to_string(),
            args: vec![
                "--no-install".to_string(),
                "prettier".to_string(),
                "--write".to_string(),
                ".".to_string(),
            ],
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
        let _ = (subproject, all_targets);
        if !root.join("tsconfig.json").is_file() {
            return Ok(None);
        }
        let mut args = vec![
            "--no-install".to_string(),
            "tsc".to_string(),
            "--noEmit".to_string(),
        ];
        args.extend(extra.iter().cloned());
        Ok(Some(CommandLine::Program {
            program: "npx".to_string(),
            args,
        }))
    }

    fn parse_diagnostics(&self, raw: &str, max: usize) -> (Vec<String>, usize) {
        parse_tsc_diagnostics(raw, max)
    }

    fn simplify_tests(&self, raw: &str) -> String {
        self.compose_test_summary(raw, simplify_js_tests(raw))
    }

    fn is_test_command(&self, line: &CommandLine) -> bool {
        match line {
            CommandLine::Program { program, args } if program == "npm" => {
                if args.iter().any(|a| a == "test") {
                    return true;
                }
                args.iter()
                    .position(|a| a == "run")
                    .and_then(|i| args.get(i + 1))
                    .map(|s| s == "test")
                    .unwrap_or(false)
            }
            _ => false,
        }
    }
}

/// Parse `tsc --noEmit` output (`path(line,col): error TSxxxx: msg`) into
/// `path:line:col: error[TSxxxx]: msg`, capped at `max`. Falls back to a plain
/// error-line scan when nothing matches.
fn parse_tsc_diagnostics(raw: &str, max: usize) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut total = 0usize;
    for line in raw.lines() {
        if let Some(d) = format_tsc_line(line.trim_end()) {
            total += 1;
            if out.len() < max {
                out.push(d);
            }
        }
    }
    if total == 0 {
        return generic_error_lines(raw, max);
    }
    (out, total)
}

/// Format one tsc diagnostic line, or `None` when it is not a located error.
fn format_tsc_line(line: &str) -> Option<String> {
    let idx = line.find("): error ")?;
    let left = &line[..idx + 1]; // "path(line,col)"
    let rest = &line[idx + 1..]; // ": error TSxxxx: message"
    let open = left.rfind('(')?;
    let path = left[..open].trim();
    let inner = &left[open + 1..left.len() - 1];
    let (l, c) = inner.split_once(',').unwrap_or((inner, "0"));
    if path.is_empty() {
        return None;
    }
    Some(format!("{path}:{l}:{c}{rest}"))
}

/// Reduce raw jest/vitest/mocha output to a readable summary: the summary lines,
/// failing-test markers and key errors, capped. When nothing is recognizable the
/// last non-empty lines are returned instead.
/// Reduce raw jest/vitest/mocha output to a small, failure-first summary: the
/// totals lines followed by the failing-test / error lines. Passing-only lines
/// are dropped and the result is bounded, so it is never truncated downstream.
fn simplify_js_tests(raw: &str) -> String {
    /// Totals/summary lines (checked first).
    const SUMMARY: &[&str] = &[
        "tests:",
        "test suites:",
        "test files",
        "snapshots:",
        "time:",
        "duration",
    ];
    /// Lines that report a failure or an error.
    const FAIL: &[&str] = &[
        "✕",
        "✗",
        "✘",
        "×",
        "fail",
        "error",
        "assertionerror",
        "expected",
        "received",
        "expect(",
    ];
    /// Hard cap on the whole summary (well under the agent observation cap).
    const MAX_CHARS: usize = 3000;

    let mut summary: Vec<&str> = Vec::new();
    let mut fails: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            continue;
        }
        let l = t.to_lowercase();
        if SUMMARY.iter().any(|k| l.contains(k)) {
            summary.push(t);
        } else if FAIL.iter().any(|k| l.contains(k)) {
            fails.push(t);
        }
    }

    let mut out = String::new();
    for s in &summary {
        out.push_str(s);
        out.push('\n');
    }
    if !fails.is_empty() {
        out.push_str("failures:\n");
        for f in &fails {
            out.push_str(f);
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        // Nothing recognizable: keep the tail rather than nothing at all.
        let mut tail: Vec<&str> = raw
            .lines()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(20)
            .collect();
        tail.reverse();
        return trim_chars(tail.join("\n"), MAX_CHARS);
    }
    trim_chars(out, MAX_CHARS)
}

/// Whether one raw line is a REAL compiler-diagnostic header, as opposed to
/// noise that merely mentions the word "error". A bare substring test on
/// "error" matched unrelated lines - notably a PASSING test whose name
/// contains it (`test foo::errors_are_recognised ... ok`) - which made
/// `compose_test_summary` tell the model its green suite had failed to build.
/// A real diagnostic has a known shape: `error:` / `error[E0425]:` (rustc,
/// clang), a diagnostic code, or one of the toolchain's stock failure phrases.
fn is_error_header(line: &str) -> bool {
    let t = line.trim();
    // Test-runner output can contain "error" and is never a diagnostic.
    if t.starts_with("test ") || t.starts_with("test result:") {
        return false;
    }
    let l = t.to_lowercase();
    // rustc/clang style header: `error: ...` or `error[E0425]: ...`.
    if l.starts_with("error:") || l.starts_with("error[") {
        return true;
    }
    // Prefixed diagnostics, e.g. `src/x.rs:1:1: error[E0308]: ...`.
    if l.contains("error[e") {
        return true;
    }
    l.starts_with("cannot find")
        || l.starts_with("mismatched types")
        || l.contains("could not compile")
        || l.contains("failed to compile")
}

/// Fallback diagnostic extraction for toolchains without a structured format:
/// keep the lines that look like errors, capped at `max` — each one FOLLOWED by
/// its location and code-snippet lines when the toolchain prints them on their
/// own lines (Rust: `error: ...` then ` --> src/lib.rs:9:20` then the `|`-prefixed
/// snippet). Dropping those left a small model with "unclosed delimiter" and no
/// idea WHERE, so it rewrote the file blind until it hit max_iterations.
pub fn generic_error_lines(raw: &str, max: usize) -> (Vec<String>, usize) {
    /// Continuation lines kept after one error header (location + snippet).
    const CONTEXT_LINES: usize = 6;
    let lines: Vec<&str> = raw.lines().map(str::trim_end).collect();
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i];
        i += 1;
        if t.trim().is_empty() {
            continue;
        }
        if !is_error_header(t) {
            continue;
        }
        total += 1;
        if out.len() >= max {
            continue;
        }
        out.push(t.to_string());
        // Pull in the location/snippet lines that belong to this error.
        let mut taken = 0usize;
        while i < lines.len() && taken < CONTEXT_LINES && out.len() < max {
            let next = lines[i];
            let nt = next.trim();
            if nt.is_empty() {
                break;
            }
            let is_location = nt.starts_with("-->") || nt.starts_with("|-->");
            let is_snippet =
                next.chars().next().is_some_and(|c| c == ' ' || c == '\t') && nt.contains('|');
            if !is_location && !is_snippet {
                break;
            }
            out.push(next.to_string());
            taken += 1;
            i += 1;
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
        let Some(msg) = v.get("message") else {
            continue;
        };
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
                .find(|s| {
                    s.get("is_primary")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
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
/// Reduce raw `cargo test` output to the failure-only summary the model needs:
/// the per-binary `test result:` totals, the names of the failing tests, and a
/// bounded excerpt of each failure's captured output. Passing tests, build
/// noise and unbounded test stdout are dropped, so the result stays small
/// enough that the agent loop never truncates it.
fn simplify_test_output(raw: &str) -> String {
    /// Lines kept from each failing test's captured output.
    const EXCERPT_LINES: usize = 15;
    /// Hard cap on the whole summary (well under the agent observation cap).
    const MAX_CHARS: usize = 3500;

    let mut results: Vec<String> = Vec::new();
    let mut failing: Vec<String> = Vec::new();
    let mut excerpts: Vec<String> = Vec::new();
    let mut in_excerpt = false;
    let mut excerpt_lines = 0usize;

    for line in raw.lines() {
        let t = line.trim_start();
        if t.starts_with("test result:") {
            in_excerpt = false;
            results.push(line.trim_end().to_string());
            continue;
        }
        // "test NAME ... FAILED" (optionally with a trailing " (1.2s)").
        if let Some(rest) = t.strip_prefix("test ")
            && let Some((name, _)) = rest.split_once(" ... FAILED")
        {
            in_excerpt = false;
            failing.push(name.trim().to_string());
            continue;
        }
        // A captured-output block for one test: "---- NAME stdout ----".
        if t.starts_with("---- ") && t.trim_end().ends_with(" ----") {
            in_excerpt = true;
            excerpt_lines = 0;
            excerpts.push(line.trim_end().to_string());
            continue;
        }
        if in_excerpt {
            if t.starts_with("failures:") {
                in_excerpt = false;
                continue;
            }
            if excerpt_lines < EXCERPT_LINES {
                excerpts.push(line.trim_end().to_string());
                excerpt_lines += 1;
            }
            continue;
        }
        // Everything else (Compiling/Finished/Running/passing tests/…) is noise.
    }

    let mut out = String::new();
    for r in &results {
        out.push_str(r);
        out.push('\n');
    }
    if !failing.is_empty() {
        out.push_str(&format!(
            "{} failing test(s): {}\n",
            failing.len(),
            failing.join(", ")
        ));
    }
    if !excerpts.is_empty() {
        out.push_str("\nfailure output:\n");
        out.push_str(&excerpts.join("\n"));
        out.push('\n');
    }
    let out = trim_chars(out, MAX_CHARS);
    // Empty means "nothing recognizable in this output"; the caller decides what
    // that means for its ecosystem (usually: the build failed).
    out
}

/// Truncate `s` to at most `max` chars, appending a marker when cut.
fn trim_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n...(trimmed)");
    out
}

#[cfg(test)]
mod tests;
