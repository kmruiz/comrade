//! Persistent project memory: ADR-style decisions + a project glossary, both
//! under `.comrade/memory/`.
//!
//! Decisions (ADR): one numbered Markdown file per decision, recorded only
//! when an important choice was made that will affect the architecture,
//! design or product on the long term. Each entry records when it happened,
//! context/rationale, the decision, alternatives considered, scope and impact.
//!
//! - `record_adr` records a new ADR decision (runs directly without approval).
//! - `find_adr` searches summaries + bodies and returns a cheap ranked
//!   list (id · status · title · excerpt) - never full bodies.
//! - `read_adr` returns a full entry by id.
//! - `amend_adr` updates status or appends a note (runs directly).
//!
//! Glossary: a single `.comrade/memory/glossary.md` mapping project keywords
//! to their meaning and to references in code or documentation.
//!
//! - `record_glossary` adds or updates a term (runs directly without approval).
//! - `find_glossary` searches terms and returns name + one-line meaning.
//! - `read_glossary` returns one term's entry, or the whole file without a
//!   term argument.

mod glossary;
mod store;

use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub use store::{list, read as read_entry, search};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(RecordAdr),
        Box::new(FindAdr),
        Box::new(ReadAdr),
        Box::new(AmendAdr),
        Box::new(RecordGlossary),
        Box::new(FindGlossary),
        Box::new(ReadGlossary),
    ]
}

fn default_limit() -> usize {
    8
}

// ---------------------------------------------------------------------------
// record_adr
// ---------------------------------------------------------------------------

struct RecordAdr;

static RECORD_ADR_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "record_adr".into(),
    description: "Record an ADR decision (.comrade/memory/) for an important long-term architectural/design choice: date, context/rationale, decision, alternatives, scope, impact. Do NOT persist small operational notes or how-tos. Runs directly without approval.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "description": "Short decision title, e.g. \"Prefer pom_run_task for batch rewrites\"." },
            "summary": { "type": "string", "description": "One-line summary shown in search results: the decision in one sentence." },
            "context": { "type": "string", "description": "Background: what happened and why a decision was needed now." },
            "decision": { "type": "string", "description": "What was decided." },
            "rationale": { "type": "string", "description": "Why this choice over the alternatives." },
            "alternatives": { "type": "string", "description": "Alternatives considered and why they were rejected." },
            "scope": { "type": "string", "description": "What this decision covers, and what it deliberately does not." },
            "impact": { "type": "string", "description": "Expected consequences, trade-offs and follow-ups." },
            "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags for filtering." }
        },
        "required": ["title"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for RecordAdr {
    fn spec(&self) -> &ToolSpec {
        &RECORD_ADR_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            title: String,
            #[serde(default)]
            summary: Option<String>,
            #[serde(default)]
            context: Option<String>,
            #[serde(default)]
            decision: Option<String>,
            #[serde(default)]
            rationale: Option<String>,
            #[serde(default)]
            alternatives: Option<String>,
            #[serde(default)]
            scope: Option<String>,
            #[serde(default)]
            impact: Option<String>,
            #[serde(default)]
            tags: Vec<String>,
        }
        let args: Args = serde_json::from_value(args)?;

        let (id, rel) = store::plan_path(&ctx.project_root, &args.title)?;
        let abs = ctx.project_root.join(".comrade").join("memory").join(&rel);
        let before = std::fs::read_to_string(&abs).unwrap_or_default();
        ctx.undo
            .capture(&format!(".comrade/memory/{rel}"), before)
            .await?;

        let draft = store::DraftDecision {
            title: args.title,
            summary: args.summary.unwrap_or_default(),
            context: args.context,
            decision: args.decision,
            rationale: args.rationale,
            alternatives: args.alternatives,
            scope: args.scope,
            impact: args.impact,
            date: None,
            tags: args.tags,
        };
        let written = store::write(&ctx.project_root, draft)?;
        debug_assert_eq!(written, id);
        Ok(format!("Recorded decision #{id} in .comrade/memory/{rel}"))
    }
}

// ---------------------------------------------------------------------------
// find_adr
// ---------------------------------------------------------------------------

struct FindAdr;

static FIND_ADR_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "find_adr".into(),
    description: "Free-text search persistent ADR decisions (.comrade/memory/). Returns a ranked list of id, status, title and a one-line excerpt - call read_adr for the full body. Check before architectural choices.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Free-text search terms (optional; omit to list the latest decisions)." },
            "tags": { "type": "array", "items": { "type": "string" }, "description": "Only entries with all these tags." },
            "limit": { "type": "integer", "minimum": 1, "maximum": 30, "default": 8, "description": "Max results." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for FindAdr {
    fn spec(&self) -> &ToolSpec {
        &FIND_ADR_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            query: Option<String>,
            #[serde(default)]
            tags: Vec<String>,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        let args: Args = serde_json::from_value(args)?;
        let hits = store::search(
            &ctx.project_root,
            args.query.as_deref(),
            if args.tags.is_empty() {
                None
            } else {
                Some(&args.tags)
            },
            args.limit,
        )?;
        if hits.is_empty() {
            return Ok("No matching decisions found.".to_string());
        }
        let mut out = format!("{} decision(s):\n", hits.len());
        for m in hits {
            let excerpt = m.excerpt();
            let when = m
                .date
                .as_deref()
                .map(|d| format!(" {d}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  #{id} [{status}]{when} {title}{excerpt}\n",
                id = m.id,
                status = m.status,
                title = if excerpt.is_empty() {
                    String::new()
                } else {
                    format!(" - {excerpt}")
                },
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// read_adr
// ---------------------------------------------------------------------------

struct ReadAdr;

static READ_ADR_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "read_adr".into(),
    description: "Read the full body of a persistent decision by its #id (see find_adr). Use when a decision actually applies and you need its details.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "id": { "type": "integer", "minimum": 1, "description": "Decision id, e.g. 3 for #3." }
        },
        "required": ["id"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ReadAdr {
    fn spec(&self) -> &ToolSpec {
        &READ_ADR_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: u32,
        }
        let args: Args = serde_json::from_value(args)?;
        let entry = store::read(&ctx.project_root, args.id)?;
        Ok(entry.body)
    }
}

// ---------------------------------------------------------------------------
// amend_adr
// ---------------------------------------------------------------------------

struct AmendAdr;

static AMEND_ADR_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "amend_adr".into(),
    description: "Update an existing decision: change its status (proposed/accepted/superseded/rejected) and/or append a Note. Runs directly without approval.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "id": { "type": "integer", "minimum": 1, "description": "Decision id to amend." },
            "status": { "type": "string", "enum": ["proposed", "accepted", "superseded", "rejected"], "description": "New status." },
            "note": { "type": "string", "description": "Text to append as a new ## Note section (e.g. why it was superseded)." }
        },
        "required": ["id"],
        "anyOf": [
            { "required": ["status"] },
            { "required": ["note"] }
        ],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for AmendAdr {
    fn spec(&self) -> &ToolSpec {
        &AMEND_ADR_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: u32,
            #[serde(default)]
            status: Option<String>,
            #[serde(default)]
            note: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        if args.status.is_none() && args.note.as_deref().map(str::trim).unwrap_or("").is_empty() {
            anyhow::bail!("amend_adr requires a status and/or a note");
        }

        let entry = store::read(&ctx.project_root, args.id)?;
        let rel = entry.meta.file_name.clone();
        let abs = ctx.project_root.join(".comrade").join("memory").join(&rel);
        let before = std::fs::read_to_string(&abs).unwrap_or_default();
        ctx.undo
            .capture(&format!(".comrade/memory/{rel}"), before)
            .await?;

        store::amend(
            &ctx.project_root,
            args.id,
            args.status.as_deref(),
            args.note.as_deref(),
        )?;
        Ok(format!("Amended decision #{}.", args.id))
    }
}

// ---------------------------------------------------------------------------
// record_glossary
// ---------------------------------------------------------------------------

struct RecordGlossary;

static RECORD_GLOSSARY_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "record_glossary".into(),
    description: "Add or update one keyword in the project glossary (.comrade/memory/glossary.md): term -> meaning + references. Call when you meet a project-specific term the next session should understand. Runs directly without approval.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "term": { "type": "string", "description": "The keyword, e.g. \"ADR\" or \"ToolSpec\"." },
            "meaning": { "type": "string", "description": "What the keyword means for this project." },
            "references": { "type": "array", "items": { "type": "string" }, "description": "File paths or docs where the term is defined or used, e.g. [\"crates/comrade-tool-memory/src/store.rs\"]." },
            "notes": { "type": "string", "description": "Optional extra context beyond the meaning." }
        },
        "required": ["term", "meaning"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for RecordGlossary {
    fn spec(&self) -> &ToolSpec {
        &RECORD_GLOSSARY_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            term: String,
            meaning: String,
            #[serde(default)]
            references: Vec<String>,
            #[serde(default)]
            notes: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;

        let rel = format!(".comrade/{}/{}", store::DIR_NAME, glossary::FILE_NAME);
        let abs = ctx
            .project_root
            .join(".comrade")
            .join(store::DIR_NAME)
            .join(glossary::FILE_NAME);
        let before = std::fs::read_to_string(&abs).unwrap_or_default();
        ctx.undo.capture(&rel, before).await?;

        let was_new = glossary::upsert(
            &ctx.project_root,
            &args.term,
            &args.meaning,
            &args.references,
            args.notes.as_deref(),
        )?;
        Ok(format!(
            "{} glossary term {:?} in {rel}",
            if was_new { "Added" } else { "Updated" },
            args.term.trim()
        ))
    }
}

// ---------------------------------------------------------------------------
// find_glossary
// ---------------------------------------------------------------------------

struct FindGlossary;

static FIND_GLOSSARY_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "find_glossary".into(),
    description: "Search the project glossary for keywords; returns each term + one-line meaning. Read a full entry with read_glossary, add/update with record_glossary.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Substring to match against term names or meanings (optional; omit to list glossary terms)." },
            "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 25, "description": "Max results." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for FindGlossary {
    fn spec(&self) -> &ToolSpec {
        &FIND_GLOSSARY_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            query: Option<String>,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        let args: Args = serde_json::from_value(args)?;
        let hits = glossary::find(&ctx.project_root, args.query.as_deref(), args.limit)?;
        if hits.is_empty() {
            if glossary::read_whole(&ctx.project_root)?.trim().is_empty() {
                return Ok("The glossary is empty (no .comrade/memory/glossary.md yet). Define keywords with record_glossary.".to_string());
            }
            return Ok("No matching glossary terms found.".to_string());
        }
        let mut out = format!("{} glossary term(s):\n", hits.len());
        for t in hits {
            let excerpt = t.excerpt.trim();
            let line = if excerpt.is_empty() {
                format!("## {}", t.term)
            } else {
                format!("## {} — {excerpt}", t.term)
            };
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// read_glossary
// ---------------------------------------------------------------------------

struct ReadGlossary;

static READ_GLOSSARY_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "read_glossary".into(),
    description: "Read a glossary entry: pass a term for its full entry (meaning + references), or omit the term to read the WHOLE .comrade/memory/glossary.md file.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "term": { "type": "string", "description": "Keyword to read (optional). Omit to return the whole glossary file." }
        },
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for ReadGlossary {
    fn spec(&self) -> &ToolSpec {
        &READ_GLOSSARY_SPEC
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            term: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        match args
            .term
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(term) => match glossary::read_term(&ctx.project_root, term)? {
                Some(t) => Ok(format!("## {}\n{}", t.term, t.body)),
                None => Ok(format!(
                    "No glossary term {:?}. See find_glossary to search, record_glossary to add it.",
                    term
                )),
            },
            None => {
                let whole = glossary::read_whole(&ctx.project_root)?;
                if whole.trim().is_empty() {
                    Ok("The glossary is empty (no .comrade/memory/glossary.md yet). Define keywords with record_glossary.".to_string())
                } else {
                    Ok(whole)
                }
            }
        }
    }
}
