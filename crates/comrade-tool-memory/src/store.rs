//! Persistent project decision store (ADRs), at `<root>/.comrade/memory/`.
//!
//! Every decision is a Markdown file with a lightweight header the engine
//! parses directly (no YAML dependency):
//!
//! ```text
//! # 0001 - Some title
//! status: accepted
//! date: 2026-09-09
//! tags: a, b
//! summary: one line used in search results
//!
//! ## Context
//! ...
//! ## Decision
//! ...
//! ## Rationale
//! ...
//! ## Alternatives considered
//! ...
//! ## Scope
//! ...
//! ## Impact
//! ...
//! ```
//!
//! Decisions are recorded only when an important choice was made that will
//! affect the architecture, design or product on the long term. Older entries
//! written before this layout remain parseable (their header simply has no
//! `date:` line and their body sections are whatever they were).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

pub const DIR_NAME: &str = "memory";

#[derive(Debug, Clone, serde::Serialize)]
pub struct EntryMeta {
    pub id: u32,
    pub slug: String,
    pub file_name: String,
    pub status: String,
    /// When the decision was made, `YYYY-MM-DD` (absent on pre-ADR entries).
    pub date: Option<String>,
    pub tags: Vec<String>,
    pub summary: String,
}

impl EntryMeta {
    /// First line of the summary, capped, for cheap listings.
    pub fn excerpt(&self) -> String {
        let s = self.summary.trim();
        if s.is_empty() {
            return String::new();
        }
        s.chars().take(160).collect()
    }
}

/// Full decision entry: metadata + markdown body.
#[derive(Debug, Clone)]
pub struct Entry {
    pub meta: EntryMeta,
    pub body: String,
}

fn dir(root: &Path) -> PathBuf {
    root.join(".comrade").join(DIR_NAME)
}

/// Make sure the store directory exists.
pub fn ensure_dir(root: &Path) -> Result<()> {
    let d = dir(root);
    std::fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
    Ok(())
}

/// Parse a memory file's header + body from its raw text.
fn parse_file(file_name: &str, text: &str) -> Option<EntryMeta> {
    let mut lines = text.lines();
    let title = lines.next()?.trim();
    let (id, slug) = parse_title(title)?;
    let mut status = String::from("accepted");
    let mut date = None;
    let mut tags = Vec::new();
    let mut summary = String::new();
    for line in lines.by_ref() {
        if line.starts_with("## ") {
            break; // body begins
        }
        if let Some(v) = line.strip_prefix("status:") {
            status = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("date:") {
            let v = v.trim();
            if !v.is_empty() {
                date = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("tags:") {
            tags = v
                .split(',')
                .map(|t| t.trim().trim_start_matches('#').trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
        } else if let Some(v) = line.strip_prefix("summary:") {
            summary = v.trim().to_string();
        }
    }
    Some(EntryMeta {
        id,
        slug,
        file_name: file_name.to_string(),
        status,
        date,
        tags,
        summary,
    })
}

fn parse_title(title: &str) -> Option<(u32, String)> {
    let title = title.trim_start_matches('#').trim();
    let (num, rest) = title.split_once('-')?;
    let id: u32 = num.trim().parse().ok()?;
    let slug = slugify(rest);
    Some((id, slug))
}

pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if (c.is_whitespace() || c == '-' || c == '_') && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Read the memory file for an id.
fn path_for(root: &Path, id: u32) -> Result<PathBuf> {
    for entry in list_files(root)? {
        if let Some(meta) = read_meta(&entry)
            && meta.id == id
        {
            return Ok(entry);
        }
    }
    anyhow::bail!("no decision #{id} found in {}", dir(root).display());
}

fn list_files(root: &Path) -> Result<Vec<PathBuf>> {
    let d = dir(root);
    if !d.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&d)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn read_meta(path: &Path) -> Option<EntryMeta> {
    let text = std::fs::read_to_string(path).ok()?;
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    parse_file(&file_name, &text)
}

/// List all stored decisions, newest id first.
pub fn list(root: &Path) -> Result<Vec<EntryMeta>> {
    let mut metas: Vec<EntryMeta> = list_files(root)?
        .iter()
        .filter_map(|p| read_meta(p))
        .collect();
    metas.sort_by_key(|b| std::cmp::Reverse(b.id));
    Ok(metas)
}

/// Read one decision's full body by id.
pub fn read(root: &Path, id: u32) -> Result<Entry> {
    let path = path_for(root, id)?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let meta = parse_file(&file_name, &text)
        .with_context(|| format!("cannot parse header of {}", path.display()))?;
    Ok(Entry { meta, body: text })
}

fn next_id(root: &Path) -> Result<u32> {
    Ok(list_files(root)?
        .iter()
        .filter_map(|p| read_meta(p))
        .map(|m| m.id)
        .max()
        .unwrap_or(0)
        + 1)
}

/// Compute (id, file name) a new decision would get, without writing it. Used
/// to capture undo state and to preview before approval.
pub fn plan_path(root: &Path, title: &str) -> Result<(u32, String)> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("title must not be empty");
    }
    let id = next_id(root)?;
    let slug = slugify(title);
    if slug.is_empty() {
        anyhow::bail!("title cannot be slugified");
    }
    Ok((id, format!("{id:04}-{slug}.md")))
}

/// A new ADR decision: `title` and `summary` are required; everything else is
/// optional prose. `date` defaults to today (UTC) when omitted.
#[derive(Debug, Clone)]
pub struct DraftDecision {
    pub title: String,
    /// One line used in search results.
    pub summary: String,
    /// Background: what happened and why a decision was needed now.
    pub context: Option<String>,
    /// What was decided.
    pub decision: Option<String>,
    /// Why this choice over others.
    pub rationale: Option<String>,
    /// Alternatives considered and why they were rejected.
    pub alternatives: Option<String>,
    /// What this decision covers - and what it deliberately does not.
    pub scope: Option<String>,
    /// Expected consequences, trade-offs and follow-ups.
    pub impact: Option<String>,
    /// When it happened, `YYYY-MM-DD`. Defaults to today when `None`.
    pub date: Option<String>,
    pub tags: Vec<String>,
}

/// Today's date as `YYYY-MM-DD` (UTC, civil calendar) without a date crate.
pub fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Write a new ADR decision and return its id.
pub fn write(root: &Path, draft: DraftDecision) -> Result<u32> {
    let title = draft.title.trim();
    if title.is_empty() {
        anyhow::bail!("title must not be empty");
    }
    let id = next_id(root)?;
    let slug = slugify(title);
    if slug.is_empty() {
        anyhow::bail!("title cannot be slugified");
    }
    let file_name = format!("{id:04}-{slug}.md");
    let path = dir(root).join(&file_name);
    if path.exists() {
        anyhow::bail!("entry {file_name} already exists");
    }

    let date = match draft.date.as_deref() {
        Some(d) if !d.trim().is_empty() => d.trim().to_string(),
        _ => today_iso(),
    };
    let mut text = format!("# {id:04} - {title}\n");
    text.push_str("status: accepted\n");
    text.push_str(&format!("date: {date}\n"));
    if !draft.tags.is_empty() {
        text.push_str(&format!("tags: {}\n", draft.tags.join(", ")));
    }
    if !draft.summary.trim().is_empty() {
        text.push_str(&format!("summary: {}\n", draft.summary.trim()));
    }
    text.push('\n');
    let mut section = |name: &str, body: Option<&str>| {
        if let Some(b) = body {
            let b = b.trim();
            if !b.is_empty() {
                text.push_str(&format!("## {name}\n{b}\n\n"));
            }
        }
    };
    section("Context", draft.context.as_deref());
    section("Decision", draft.decision.as_deref());
    section("Rationale", draft.rationale.as_deref());
    section("Alternatives considered", draft.alternatives.as_deref());
    section("Scope", draft.scope.as_deref());
    section("Impact", draft.impact.as_deref());

    ensure_dir(root)?;
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(id)
}

/// Update an existing decision's status and/or append a note.
pub fn amend(root: &Path, id: u32, status: Option<&str>, note: Option<&str>) -> Result<String> {
    let path = path_for(root, id)?;
    let mut text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    if let Some(status) = status {
        let status = status.trim();
        if !matches!(status, "accepted" | "proposed" | "superseded" | "rejected") {
            anyhow::bail!("unknown status {status:?} (accepted|proposed|superseded|rejected)");
        }
        // rewrite first "status:" line
        let mut done = false;
        let mut lines = Vec::new();
        for line in text.lines() {
            if !done && line.starts_with("status:") {
                lines.push(format!("status: {status}"));
                done = true;
            } else {
                lines.push(line.to_string());
            }
        }
        if !done {
            lines.insert(1, format!("status: {status}"));
        }
        text = lines.join("\n") + "\n";
    }

    if let Some(note) = note {
        let note = note.trim();
        if !note.is_empty() {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("\n## Note\n{note}\n"));
        }
    }

    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned())
}

/// Free-text search across summaries and bodies. Relevance = number of query
/// words found (title/summary words count double). Returns best matches first.
pub fn search(
    root: &Path,
    query: Option<&str>,
    tags: Option<&[String]>,
    limit: usize,
) -> Result<Vec<EntryMeta>> {
    let words: Vec<String> = query
        .map(|q| q.split_whitespace().map(|w| w.to_lowercase()).collect())
        .unwrap_or_default();

    let mut scored: Vec<(usize, EntryMeta)> = Vec::new();
    for file in list_files(root)? {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let file_name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let Some(meta) = parse_file(&file_name, &text) else {
            continue;
        };
        if let Some(tags) = tags
            && !tags.iter().all(|t| meta.tags.iter().any(|m| m == t))
        {
            continue;
        }
        if words.is_empty() {
            scored.push((0, meta));
            continue;
        }
        let hay = text.to_lowercase();
        let mut score = 0usize;
        for w in &words {
            if meta.summary.to_lowercase().contains(w) {
                score += 2;
            }
            if hay.contains(w) {
                score += 1;
            }
        }
        if score > 0 {
            scored.push((score, meta));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
    scored.truncate(limit.max(1));
    Ok(scored.into_iter().map(|(_, m)| m).collect())
}

/// The title of an entry body: its first `# NNNN - Title` line, title only.
fn title_of(body: &str) -> String {
    let first = body.lines().next().unwrap_or("").trim();
    let first = first.trim_start_matches('#').trim();
    match first.split_once(" - ") {
        Some((_, t)) => t.trim().to_string(),
        None => first.to_string(),
    }
}

/// Merge `sources` into `target`: append each source's body under a
/// `## Merged from #NNNN - title` heading and mark the source `superseded`.
/// Returns a one-line summary of what happened.
pub fn merge(root: &Path, target: u32, sources: &[u32], note: Option<&str>) -> Result<String> {
    if !path_for(root, target)?.exists() {
        anyhow::bail!("no decision #{target}");
    }
    let mut text = read(root, target)?.body;
    let mut merged: Vec<String> = Vec::new();
    for &src in sources {
        if src == target {
            continue;
        }
        let e = read(root, src)?;
        let title = title_of(&e.body);
        let mut src_body = e.body.clone();
        if let Some(i) = src_body.find('\n') {
            src_body = src_body[i + 1..].to_string();
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "\n## Merged from #{src:04} - {title}\n{}\n",
            src_body.trim()
        ));
        amend(
            root,
            src,
            Some("superseded"),
            Some(&format!("merged into #{target:04}")),
        )?;
        merged.push(format!("#{src:04}"));
    }
    if let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("\n## Note\n{n}\n"));
    }
    let path = path_for(root, target)?;
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    if merged.is_empty() {
        Ok(format!("updated #{target:04} (nothing to merge)"))
    } else {
        Ok(format!("merged {} into #{target:04}", merged.join(", ")))
    }
}

/// Backtick-quoted spans that look like filesystem paths (used for staleness).
pub fn extract_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_tick = false;
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '`' {
            if in_tick {
                let t = cur.trim();
                if looks_like_path(t) && !out.iter().any(|p| p == t) {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            in_tick = !in_tick;
            continue;
        }
        if in_tick {
            cur.push(ch);
        }
    }
    out
}

/// A token worth checking for existence: no spaces/globs/placeholders, and it
/// either contains a `/` or ends with a known source/doc extension.
fn looks_like_path(t: &str) -> bool {
    !t.is_empty()
        && !t.contains(' ')
        && !t.contains('*')
        && !t.contains('<')
        && !t.contains('>')
        && !t.contains('{')
        && (t.contains('/')
            || t.ends_with(".rs")
            || t.ends_with(".toml")
            || t.ends_with(".md")
            || t.ends_with(".json"))
}

/// Entries that reference files which no longer exist on disk, as
/// `(meta, missing_paths)`. Use to spot memory that has drifted from the code.
pub fn stale(root: &Path) -> Result<Vec<(EntryMeta, Vec<String>)>> {
    let mut out = Vec::new();
    for meta in list(root)? {
        let path = dir(root).join(&meta.file_name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let missing: Vec<String> = extract_paths(&text)
            .into_iter()
            .filter(|p| match repo_path(p) {
                // Home-relative (`~/`, `$HOME/`) or absolute (`/`) refs cannot live
                // under the project root, so they are never reported as stale.
                None => false,
                Some(rel) => {
                    !root.join(rel).exists() && !root.join(rel.trim_start_matches("./")).exists()
                }
            })
            .collect();
        if !missing.is_empty() {
            out.push((meta, missing));
        }
    }
    Ok(out)
}

/// The repo-relative path to existence-check for an extracted reference, or
/// `None` when the reference is not repo-relative. A trailing `:line` or
/// `:line:col` location suffix (e.g. `crates/foo/src/lib.rs:251`) is stripped,
/// so a line-pinned reference is checked against the file itself.
fn repo_path(p: &str) -> Option<&str> {
    if p.starts_with('~') || p.starts_with('$') || p.starts_with('/') {
        return None;
    }
    let mut rel = p;
    for _ in 0..2 {
        match rel.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => {
                rel = head;
            }
            _ => break,
        }
    }
    Some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-memory-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn draft(
        title: &str,
        summary: &str,
        context: Option<&str>,
        decision: Option<&str>,
        tags: Vec<String>,
    ) -> DraftDecision {
        DraftDecision {
            title: title.into(),
            summary: summary.into(),
            context: context.map(str::to_string),
            decision: decision.map(str::to_string),
            rationale: None,
            alternatives: None,
            scope: None,
            impact: None,
            date: None,
            tags,
        }
    }

    #[test]
    fn write_list_read_roundtrip() {
        let root = scratch();
        let d = DraftDecision {
            title: "Prefer run_task for batch rewrites".into(),
            summary: "Batch work goes through run_task.".into(),
            context: Some("Repeated apply_edit calls are slow.".into()),
            decision: Some("Use run_task check/test instead.".into()),
            rationale: None,
            alternatives: None,
            scope: None,
            impact: None,
            date: None,
            tags: vec!["tooling".into(), "perf".into()],
        };
        let id = write(&root, d).unwrap();
        assert_eq!(id, 1);

        let d2 = draft(
            "Error handling",
            "Top-level anyhow only",
            None,
            None,
            vec![],
        );
        let id2 = write(&root, d2).unwrap();
        assert_eq!(id2, 2);

        let metas = list(&root).unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].id, 2); // newest first
        assert_eq!(metas[0].slug, "error-handling");
        assert!(metas[0].date.is_some()); // date: defaulted to today
        assert_eq!(metas[1].tags, vec!["tooling", "perf"]);

        let entry = read(&root, 1).unwrap();
        assert!(entry.body.contains("Prefer run_task"));
        assert!(entry.body.contains("## Context"));
        assert!(entry.body.contains("## Decision"));
        assert!(entry.meta.date.as_deref() == Some(today_iso().as_str()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_emits_adr_sections_in_order_and_honours_date() {
        let root = scratch();
        let id = write(
            &root,
            DraftDecision {
                title: "ADR layout".into(),
                summary: "decisions are ADRs".into(),
                context: Some("an important choice was made".into()),
                decision: Some("use the ADR template".into()),
                rationale: Some("traceable long-term choices".into()),
                alternatives: Some("run books; no store at all".into()),
                scope: Some("memory tooling only, not UI".into()),
                impact: Some("entries get longer".into()),
                date: Some("2026-09-09".into()),
                tags: vec!["meta".into()],
            },
        )
        .unwrap();
        let body = read(&root, id).unwrap().body;
        assert!(body.starts_with("# 0001 - ADR layout\n"));
        assert!(body.contains("\nstatus: accepted\ndate: 2026-09-09\n"));
        let ctx = body.find("## Context").unwrap();
        let decision = body.find("## Decision").unwrap();
        let rationale = body.find("## Rationale").unwrap();
        let alt = body.find("## Alternatives considered").unwrap();
        let scope = body.find("## Scope").unwrap();
        let impact = body.find("## Impact").unwrap();
        assert!(ctx < decision && decision < rationale);
        assert!(rationale < alt && alt < scope && scope < impact);
        // Consequences is gone from the new template.
        assert!(!body.contains("## Consequences"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_entries_without_date_still_parse() {
        let root = scratch();
        let text = "# 0007 - Old style\nstatus: accepted\ntags: a\nsummary: pre-ADR note\n\n## Context\nwas a run book\n";
        std::fs::create_dir_all(root.join(".comrade").join("memory")).unwrap();
        std::fs::write(
            root.join(".comrade")
                .join("memory")
                .join("0007-old-style.md"),
            text,
        )
        .unwrap();
        let metas = list(&root).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, 7);
        assert_eq!(metas[0].date, None);
        let entry = read(&root, 7).unwrap();
        assert!(entry.body.contains("run book"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_ranks_and_filters() {
        let root = scratch();
        write(
            &root,
            draft(
                "Use serde for config",
                "serde everywhere",
                Some("toml"),
                None,
                vec!["rust".into()],
            ),
        )
        .unwrap();
        write(
            &root,
            draft(
                "Batch edits",
                "prefer run_task for batch work",
                Some("slow loops"),
                None,
                vec!["tooling".into()],
            ),
        )
        .unwrap();

        // query matches title+body of second entry most
        let hits = search(&root, Some("batch work"), None, 10).unwrap();
        assert_eq!(hits[0].id, 2);

        // tag filter
        let only_rust = search(&root, None, Some(&["rust".to_string()]), 10).unwrap();
        assert_eq!(only_rust.len(), 1);
        assert_eq!(only_rust[0].id, 1);

        // no query -> everything
        assert_eq!(search(&root, None, None, 10).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn amend_changes_status_and_appends_note() {
        let root = scratch();
        write(&root, draft("Decision A", "summary a", None, None, vec![])).unwrap();
        amend(&root, 1, Some("superseded"), Some("Replaced by B.")).unwrap();
        let entry = read(&root, 1).unwrap();
        assert!(entry.body.contains("status: superseded"));
        assert!(entry.body.contains("## Note"));
        assert!(entry.body.contains("Replaced by B."));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_path_and_write_agree() {
        let root = scratch();
        let (pid, rel) = plan_path(&root, "First: decide things").unwrap();
        assert_eq!(pid, 1);
        assert_eq!(rel, "0001-first-decide-things.md");
        let id = write(
            &root,
            draft("First: decide things", "s", None, None, vec![]),
        )
        .unwrap();
        assert_eq!(id, pid);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn today_iso_is_shaped_like_a_date() {
        let d = today_iso();
        let parts: Vec<&str> = d.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].len() == 4 && parts[1].len() == 2 && parts[2].len() == 2);
        assert!(parts[0].chars().all(|c| c.is_ascii_digit()));
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()));
        assert!(parts[2].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn merge_appends_and_supersedes_sources() {
        let root = scratch();
        let a = write(&root, draft("Alpha", "first", None, None, vec![])).unwrap();
        let b = write(&root, draft("Beta", "second", None, None, vec![])).unwrap();
        let msg = merge(&root, a, &[b], Some("consolidated")).unwrap();
        assert!(msg.contains(&format!("#{b:04}")), "{msg}");
        let target = read(&root, a).unwrap();
        assert!(
            target.body.contains(&format!("Merged from #{b:04}")),
            "{}",
            target.body
        );
        assert!(target.body.contains("consolidated"), "{}", target.body);
        let src = read(&root, b).unwrap();
        assert_eq!(src.meta.status, "superseded");
        assert!(src.body.contains(&format!("merged into #{a:04}")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_paths_and_stale_flag_missing_refs() {
        let paths = extract_paths("see `crates/foo/src/lib.rs`, `Foo::bar` and `README.md`");
        assert!(paths.contains(&"crates/foo/src/lib.rs".to_string()));
        assert!(paths.contains(&"README.md".to_string()));
        assert!(!paths.iter().any(|p| p == "Foo::bar"));

        let root = scratch();
        let id = write(
            &root,
            draft(
                "Doc",
                "s",
                Some("refers to `crates/nope/missing.rs`"),
                None,
                vec![],
            ),
        )
        .unwrap();
        let stale_list = stale(&root).unwrap();
        assert_eq!(stale_list.len(), 1, "{stale_list:?}");
        assert_eq!(stale_list[0].0.id, id);
        assert!(
            stale_list[0]
                .1
                .contains(&"crates/nope/missing.rs".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_ignores_location_suffixes_and_non_repo_refs() {
        let root = scratch();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/keep.rs"), "fn x() {}\n").unwrap();
        write(
            &root,
            draft(
                "Doc",
                "s",
                Some("ok `sub/keep.rs:12`, home `~/cache/x`, abs `/etc/hosts`, env `$HOME/y`"),
                None,
                vec![],
            ),
        )
        .unwrap();
        // A `:line` ref to an existing file is not stale, and neither are the
        // home-relative or absolute refs (they cannot live under the root).
        let stale_list = stale(&root).unwrap();
        assert!(stale_list.is_empty(), "{stale_list:?}");

        // Stripping the suffix must not hide drift: the file itself is gone.
        let id = write(
            &root,
            draft("Doc2", "s", Some("gone `sub/gone.rs:12`"), None, vec![]),
        )
        .unwrap();
        let stale_list = stale(&root).unwrap();
        assert_eq!(stale_list.len(), 1, "{stale_list:?}");
        assert_eq!(stale_list[0].0.id, id);
        assert!(stale_list[0].1.contains(&"sub/gone.rs:12".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
