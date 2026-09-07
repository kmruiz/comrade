//! Persistent project decisions store (ADR-style), at `<root>/.comrade/memory/`.
//!
//! Every decision is a Markdown file with a lightweight header the engine
//! parses directly (no YAML dependency):
//!
//! ```text
//! # 0001 - Some title
//! status: accepted
//! tags: a, b
//! summary: one line used in search results
//!
//! ## Context
//! ...
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

pub const DIR_NAME: &str = "memory";

#[derive(Debug, Clone, serde::Serialize)]
pub struct EntryMeta {
    pub id: u32,
    pub slug: String,
    pub file_name: String,
    pub status: String,
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
    let mut tags = Vec::new();
    let mut summary = String::new();
    for line in lines.by_ref() {
        if line.starts_with("## ") {
            break; // body begins
        }
        if let Some(v) = line.strip_prefix("status:") {
            status = v.trim().to_string();
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
    let _ = summary.is_empty();
    Some(EntryMeta {
        id,
        slug,
        file_name: file_name.to_string(),
        status,
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
        } else if c.is_whitespace() || c == '-' || c == '_' {
            if !out.ends_with('-') {
                out.push('-');
            }
        }
    }
    out.trim_matches('-').to_string()
}

/// Read the memory file for an id.
fn path_for(root: &Path, id: u32) -> Result<PathBuf> {
    for entry in list_files(root)? {
        if let Some(meta) = read_meta(&entry) {
            if meta.id == id {
                return Ok(entry);
            }
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
    metas.sort_by(|a, b| b.id.cmp(&a.id));
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

/// Write a new decision and return its id.
pub fn write(
    root: &Path,
    title: &str,
    summary: &str,
    context: Option<&str>,
    decision: Option<&str>,
    consequences: Option<&str>,
    tags: Vec<String>,
) -> Result<u32> {
    let title = title.trim();
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

    let mut text = format!("# {id:04} - {title}\n");
    text.push_str(&format!("status: accepted\n"));
    if !tags.is_empty() {
        text.push_str(&format!("tags: {}\n", tags.join(", ")));
    }
    if !summary.trim().is_empty() {
        text.push_str(&format!("summary: {}\n", summary.trim()));
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
    section("Context", context);
    section("Decision", decision);
    section("Consequences", consequences);

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
        if let Some(tags) = tags {
            if !tags.iter().all(|t| meta.tags.iter().any(|m| m == t)) {
                continue;
            }
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

    #[test]
    fn write_list_read_roundtrip() {
        let root = scratch();
        let id = write(
            &root,
            "Prefer run_task for batch rewrites",
            "Batch work goes through run_task.",
            Some("Repeated apply_edit calls are slow."),
            Some("Use run_task check/test instead."),
            None,
            vec!["tooling".into(), "perf".into()],
        )
        .unwrap();
        assert_eq!(id, 1);

        let id2 = write(
            &root,
            "Error handling",
            "Top-level anyhow only",
            None,
            None,
            None,
            vec![],
        )
        .unwrap();
        assert_eq!(id2, 2);

        let metas = list(&root).unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].id, 2); // newest first
        assert_eq!(metas[0].slug, "error-handling");
        assert_eq!(metas[1].tags, vec!["tooling", "perf"]);

        let entry = read(&root, 1).unwrap();
        assert!(entry.body.contains("Prefer run_task"));
        assert!(entry.body.contains("## Context"));
        assert!(entry.body.contains("## Decision"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_ranks_and_filters() {
        let root = scratch();
        write(
            &root,
            "Use serde for config",
            "serde everywhere",
            Some("toml"),
            None,
            None,
            vec!["rust".into()],
        )
        .unwrap();
        write(
            &root,
            "Batch edits",
            "prefer run_task for batch work",
            Some("slow loops"),
            None,
            None,
            vec!["tooling".into()],
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
        write(&root, "Decision A", "summary a", None, None, None, vec![]).unwrap();
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
        let id = write(&root, "First: decide things", "s", None, None, None, vec![]).unwrap();
        assert_eq!(id, pid);
        let _ = std::fs::remove_dir_all(&root);
    }
}
