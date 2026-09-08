//! Persistent project decisions/memories: ADR-style notes stored under
//! `.comrade/memory/`, plus free-text search.
//!
//! - `remember` writes a new decision (approval-gated).
//! - `find_decisions` searches summaries + bodies and returns a cheap ranked
//!   list (id · status · title · excerpt) - never full bodies.
//! - `read_decision` returns a full entry by id.
//! - `amend_decision` updates status or appends a note (approval-gated).

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
        Box::new(Remember),
        Box::new(FindDecisions),
        Box::new(ReadDecision),
        Box::new(AmendDecision),
    ]
}

fn default_limit() -> usize {
    8
}

// ---------------------------------------------------------------------------
// remember
// ---------------------------------------------------------------------------

struct Remember;

static REMEMBER_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "remember".into(),
    description: "Write what a future session must find, reuse, or avoid as a persistent run book under .comrade/memory/ (find_decisions/read_decision). Format: title + context, numbered steps of ACTION -> VERIFICATION stored as summary/context/decision/consequences. Approval-gated: include Justification.".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "description": "Short decision title, e.g. \"Prefer run_task for batch rewrites\"." },
            "summary": { "type": "string", "description": "One-line summary shown in search results." },
            "context": { "type": "string", "description": "Optional background / why this decision matters." },
            "decision": { "type": "string", "description": "Optional what was decided." },
            "consequences": { "type": "string", "description": "Optional trade-offs / follow-ups." },
            "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags for filtering." }
        },
        "required": ["title"],
        "additionalProperties": false
    }),
}
});

#[async_trait]
impl Tool for Remember {
    fn spec(&self) -> &ToolSpec {
        &REMEMBER_SPEC
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
            consequences: Option<String>,
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
        ctx.confirm(format!("remember #{id}: {}", args.title.trim()), None)
            .await?;

        let written = store::write(
            &ctx.project_root,
            &args.title,
            args.summary.as_deref().unwrap_or(""),
            args.context.as_deref(),
            args.decision.as_deref(),
            args.consequences.as_deref(),
            args.tags.clone(),
        )?;
        debug_assert_eq!(written, id);
        Ok(format!("Recorded decision #{id} in .comrade/memory/{rel}"))
    }
}

// ---------------------------------------------------------------------------
// find_decisions
// ---------------------------------------------------------------------------

struct FindDecisions;

static FIND_DECISIONS_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "find_decisions".into(),
    description: "Free-text search the project's persistent decisions (.comrade/memory/). Returns a cheap ranked list of id, status, title and a one-line excerpt - call read_decision for a full body. Check this before making architectural/behavioral choices you may have already decided.".into(),
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
impl Tool for FindDecisions {
    fn spec(&self) -> &ToolSpec {
        &FIND_DECISIONS_SPEC
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
            out.push_str(&format!(
                "  #{id} [{status}] {title}{excerpt}\n",
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
// read_decision
// ---------------------------------------------------------------------------

struct ReadDecision;

static READ_DECISION_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "read_decision".into(),
    description: "Read the full body of a persistent decision by its #id (see find_decisions). Use when a decision actually applies and you need its details.".into(),
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
impl Tool for ReadDecision {
    fn spec(&self) -> &ToolSpec {
        &READ_DECISION_SPEC
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
// amend_decision
// ---------------------------------------------------------------------------

struct AmendDecision;

static AMEND_DECISION_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec {
    name: "amend_decision".into(),
    description: "Update an existing decision: change its status (proposed/accepted/superseded/rejected) and/or append a Note. Approval-gated: include Justification.".into(),
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
impl Tool for AmendDecision {
    fn spec(&self) -> &ToolSpec {
        &AMEND_DECISION_SPEC
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
            anyhow::bail!("amend_decision requires a status and/or a note");
        }

        let entry = store::read(&ctx.project_root, args.id)?;
        let rel = entry.meta.file_name.clone();
        let abs = ctx.project_root.join(".comrade").join("memory").join(&rel);
        let before = std::fs::read_to_string(&abs).unwrap_or_default();
        ctx.undo
            .capture(&format!(".comrade/memory/{rel}"), before)
            .await?;
        ctx.confirm(
            format!("amend decision #{} ({})", args.id, entry.meta.status),
            None,
        )
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
