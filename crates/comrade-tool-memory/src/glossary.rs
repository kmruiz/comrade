//! Project glossary store: one file, `.comrade/memory/glossary.md`, mapping
//! project keywords to their meaning and to references in code or
//! documentation. Mirrors the decision store's location but keeps every term
//! in a single Markdown file.
//!
//! File layout (parser is tolerant of hand edits; `upsert` normalises order):
//!
//! ```text
//! # Project glossary
//!
//! ## term-name
//! > one-line meaning used by search
//!
//! full meaning / notes, free markdown
//!
//! **References:**
//! - `src/lib.rs`
//! - `docs/architecture.md`
//! ```
//!
//! Terms are delimited by `## ` headings at column 0; the first `> ` quote
//! after a heading is the excerpt used by `find_glossary`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

pub const FILE_NAME: &str = "glossary.md";

/// A single glossary term: the heading text plus its parsed section body.
#[derive(Debug, Clone)]
pub struct Term {
    pub term: String,
    /// First `> ` quote line inside the section, used as a one-line meaning.
    pub excerpt: String,
    /// Full section body (everything after the `## term` heading).
    pub body: String,
}

pub fn path(root: &Path) -> PathBuf {
    root.join(".comrade")
        .join(super::store::DIR_NAME)
        .join(FILE_NAME)
}

/// Make sure the memory dir exists and the glossary file has a header. Never
/// overwrites an existing file.
pub fn ensure(root: &Path) -> Result<()> {
    let d = root.join(".comrade").join(super::store::DIR_NAME);
    std::fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
    let p = path(root);
    if !p.exists() {
        let header = "\
# Project glossary

Project keywords and their meaning, with references to the code or \
documentation where they appear. One `## term` section per keyword, sorted \
alphabetically. Look terms up with read_glossary, search with find_glossary, \
add or update with record_glossary.

";
        std::fs::write(&p, header).with_context(|| format!("cannot write {}", p.display()))?;
    }
    Ok(())
}

/// Whole glossary file as text, or `""` when it does not exist yet.
pub fn read_whole(root: &Path) -> Result<String> {
    let p = path(root);
    if !p.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&p).with_context(|| format!("cannot read {}", p.display()))
}

/// Text before the first `## ` section heading (title + comments), preserved
/// verbatim across an `upsert` rewrite.
fn header_prefix(text: &str) -> &str {
    match text.find("\n## ") {
        Some(i) => &text[..i + 1],
        None => text,
    }
}

/// Split the section body (text starting at the first `## ` heading) into
/// `(term, raw_section_text)` pairs. Lines before any heading are ignored.
fn split_sections(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(String, String)> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some(pair) = cur.take() {
                out.push(pair);
            }
            cur = Some((rest.trim().to_string(), String::new()));
        } else if let Some((_, text)) = cur.as_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    if let Some(pair) = cur.take() {
        out.push(pair);
    }
    out
}

/// Parse a raw section body into a [`Term`], deriving the excerpt from the
/// first `> ` quote line.
fn parse_term(term: String, raw: &str) -> Term {
    let excerpt = raw
        .lines()
        .find_map(|l| l.trim().strip_prefix("> "))
        .unwrap_or_default()
        .trim()
        .to_string();
    Term {
        term,
        excerpt,
        body: raw.to_string(),
    }
}

/// All terms, alphabetical (case-insensitive).
pub fn terms(root: &Path) -> Result<Vec<Term>> {
    let text = read_whole(root)?;
    let prefix = header_prefix(&text);
    Ok(split_sections(&text[prefix.len()..])
        .into_iter()
        .map(|(t, raw)| parse_term(t, &raw))
        .collect())
}

/// Read the full entry for a term (case-insensitive heading match), if any.
pub fn read_term(root: &Path, term: &str) -> Result<Option<Term>> {
    let needle = term.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(None);
    }
    Ok(terms(root)?
        .into_iter()
        .find(|t| t.term.to_lowercase() == needle))
}

/// Search terms: a query matching a term name ranks above one matching only
/// the meaning body. No query returns everything (alphabetical), capped at
/// `limit`.
pub fn find(root: &Path, query: Option<&str>, limit: usize) -> Result<Vec<Term>> {
    let all = terms(root)?;
    let Some(q) = query
        .map(|q| q.trim().to_lowercase())
        .filter(|q| !q.is_empty())
    else {
        return Ok(all.into_iter().take(limit.max(1)).collect());
    };
    let mut scored: Vec<(usize, Term)> = Vec::new();
    for t in all {
        let hay = format!("{} {}", t.term, t.body).to_lowercase();
        if !hay.contains(&q) {
            continue;
        }
        let term_score = if t.term.to_lowercase().contains(&q) {
            2
        } else {
            0
        };
        let body_score = if t.body.to_lowercase().contains(&q) {
            1
        } else {
            0
        };
        scored.push((term_score + body_score, t));
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.term.to_lowercase().cmp(&b.1.term.to_lowercase()))
    });
    Ok(scored
        .into_iter()
        .take(limit.max(1))
        .map(|(_, t)| t)
        .collect())
}

/// Render one `## term` section body (everything after the heading line) in
/// the canonical layout. Kept separate from the heading so existing
/// hand-written sections (which also exclude the heading) can be re-emitted
/// through the same path. The term itself only appears in the heading.
fn render_section(
    _term: &str,
    meaning: &str,
    references: &[String],
    notes: Option<&str>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    let body_lines: Vec<&str> = meaning.lines().map(str::trim_end).collect();
    if let Some(first) = body_lines.first() {
        if !first.trim().is_empty() {
            lines.push(format!("> {first}"));
        }
    }
    let rest: Vec<&str> = body_lines
        .iter()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();
    if !rest.is_empty() {
        lines.push(String::new());
        lines.extend(rest.iter().map(|l| l.to_string()));
    }
    let refs: Vec<String> = references
        .iter()
        .map(|r| r.trim().trim_matches('`').trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    if !refs.is_empty() {
        lines.push(String::new());
        lines.push("**References:**".into());
        lines.extend(refs.into_iter().map(|r| format!("- `{r}`")));
    }
    if let Some(n) = notes.map(str::trim).filter(|n| !n.is_empty()) {
        lines.push(String::new());
        lines.push("**Notes:**".into());
        lines.extend(n.lines().map(|l| l.trim_end().to_string()));
    }
    lines.push(String::new()); // separator before the next section
    lines.join("\n")
}

/// Add or replace a glossary term, keeping the single file sorted
/// alphabetically. Existing sections are preserved verbatim; only the order
/// and the changed term are rewritten. Returns whether the term was new.
pub fn upsert(
    root: &Path,
    term: &str,
    meaning: &str,
    references: &[String],
    notes: Option<&str>,
) -> Result<bool> {
    let term = term.trim();
    if term.is_empty() {
        anyhow::bail!("term must not be empty");
    }
    if meaning.trim().is_empty() {
        anyhow::bail!("term {term:?} needs a meaning");
    }
    ensure(root)?;
    let p = path(root);
    let text =
        std::fs::read_to_string(&p).with_context(|| format!("cannot read {}", p.display()))?;
    let prefix = header_prefix(&text);
    let sections = split_sections(&text[prefix.len()..]);
    let key = term.to_lowercase();
    let new_section = render_section(term, meaning, references, notes);

    let mut was_new = true;
    let mut kept: Vec<(String, String)> = Vec::with_capacity(sections.len() + 1);
    for (t, raw) in sections {
        if t.to_lowercase() == key {
            if was_new {
                // Keep the pre-existing heading's casing on updates.
                kept.push((t, new_section.clone()));
                was_new = false;
            } // a stray duplicate heading with the same key is dropped
        } else {
            kept.push((t, raw));
        }
    }
    if was_new {
        kept.push((term.to_string(), new_section));
    }
    kept.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut out = String::new();
    out.push_str(prefix.trim_end());
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    for (t, raw) in kept {
        // Re-emit every section with exactly one blank line between entries so
        // repeated upserts never accumulate stray blank lines. `raw` holds
        // everything after the `## term` heading.
        let body = raw.trim_end_matches('\n');
        out.push_str(&format!("## {t}\n"));
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
        out.push('\n');
    }
    std::fs::write(&p, out).with_context(|| format!("cannot write {}", p.display()))?;
    Ok(was_new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-glossary-test-{}-{:?}",
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
    fn ensure_creates_file_with_header_once() {
        let root = scratch();
        ensure(&root).unwrap();
        ensure(&root).unwrap();
        let text = read_whole(&root).unwrap();
        assert!(text.starts_with("# Project glossary"));
        assert!(read_term(&root, "anything").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn upsert_roundtrip_and_alphabetic_order() {
        let root = scratch();
        assert!(upsert(
            &root,
            "ADR",
            "Architecture Decision Record: a dated entry capturing an important long-term choice.",
            &["crates/comrade-tool-memory/src/store.rs".into()],
            None,
        )
        .unwrap());
        assert!(
            upsert(
                &root,
                "tool",
                "A named capability an agent calls, with a JSON schema.",
                &["crates/comrade-tool/src/lib.rs".into()],
                None,
            )
            .unwrap()
        );
        assert!(
            !upsert(
                &root,
                "adr",
                "Updated meaning.",
                &["docs/adr.md".into()],
                Some("case-insensitive dedupe".into()),
            )
            .unwrap()
        );

        let all = terms(&root).unwrap();
        let names: Vec<&str> = all.iter().map(|t| t.term.as_str()).collect();
        assert_eq!(names, vec!["ADR", "tool"]); // sorted, deduped case-insensitively
        let adr = read_term(&root, "adr").unwrap().unwrap();
        assert!(adr.body.contains("Updated meaning."));
        assert!(adr.body.contains("docs/adr.md"));
        assert!(adr.body.contains("**Notes:**"));
        assert!(adr.excerpt.contains("Updated meaning."));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hand_edited_file_is_preserved_and_searchable() {
        let root = scratch();
        let text = "# Project glossary\n\nglossary of words, with meanings.\n\n## zebra\n> a striped animal\n\nsome hand-written detail\n\n**References:**\n- `src/zebra.rs`\n";
        ensure(&root).unwrap();
        std::fs::write(path(&root), text).unwrap();
        let all = terms(&root).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].term, "zebra");
        assert!(all[0].body.contains("hand-written detail"));

        let hits = find(&root, Some("striped"), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].term, "zebra");

        // Upsert keeps the handwritten body intact and adds the new term.
        upsert(&root, "alpha", "first term", &["src/alpha.rs".into()], None).unwrap();
        let all = terms(&root).unwrap();
        assert_eq!(all[0].term, "alpha");
        assert!(all[1].body.contains("hand-written detail"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn read_whole_returns_empty_when_missing() {
        let root = scratch();
        assert_eq!(read_whole(&root).unwrap(), "");
        assert!(upsert(&root, "x", "y", &[], None).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }
}
