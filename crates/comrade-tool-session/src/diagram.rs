//! Deterministic ASCII-art diagram rendering for the `show_diagram` tool.
//!
//! Models explain a concept or a flow far more clearly with a picture, but
//! drawing one by hand is error-prone: boxes drift out of alignment as soon as
//! a label's length changes. The renderers here lay out a *structured*
//! description deterministically, so the result lines up every time. A model
//! that prefers its own art can still pass it through with `kind = "raw"`.

use std::sync::LazyLock;

use anyhow::{Result, bail};
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

/// The arrow drawn between two boxes of a horizontal flow.
const CONN: &str = " --> ";

/// The sentinel line that opens a diagram block in a tool result. The TUI keys
/// off these two strings to render the art verbatim (see `result_rows`).
pub const DIAGRAM_OPEN: &str = "--- diagram (ascii) ---";

/// The sentinel line that closes a diagram block.
pub const DIAGRAM_CLOSE: &str = "--- end diagram ---";

/// The three lines of a single box: top border, labelled middle, bottom border.
/// Every box is padded to the same `inner` content width so a row of them lines
/// up. `inner` is the character length of the longest label in the flow.
fn box_lines(text: &str, inner: usize) -> [String; 3] {
    let border = format!("+{}+", "-".repeat(inner + 2));
    let mid = format!("| {}{} |", text, " ".repeat(inner - text.chars().count()));
    [border.clone(), mid, border]
}

/// Render a sequence of steps as an ASCII flow diagram.
///
/// `vertical` stacks the boxes top-to-bottom; when false the boxes sit side by
/// side and are joined by `CONN` arrows. A horizontal layout wider than `width`
/// falls back to vertical so the diagram never spills off the terminal.
pub fn render_flow(steps: &[String], vertical: bool, width: usize) -> String {
    if steps.is_empty() {
        return String::new();
    }
    let inner = steps.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let box_w = inner + 4;
    let width = width.max(1);
    let vertical = vertical
        || (steps.len() > 1 && box_w * steps.len() + CONN.len() * (steps.len() - 1) > width);
    let boxes: Vec<[String; 3]> = steps.iter().map(|s| box_lines(s, inner)).collect();

    if vertical {
        let col = box_w / 2;
        let bar = format!("{}|", " ".repeat(col));
        let arrow = format!("{}v", " ".repeat(col));
        let mut out: Vec<String> = Vec::new();
        for (i, b) in boxes.iter().enumerate() {
            if i > 0 {
                out.push(bar.clone());
                out.push(arrow.clone());
            }
            out.extend(b.iter().cloned());
        }
        out.join("\n")
    } else {
        let gap = " ".repeat(CONN.len());
        let row = |i: usize| -> String {
            boxes
                .iter()
                .map(|b| b[i].clone())
                .collect::<Vec<_>>()
                .join(&gap)
        };
        let mid = boxes
            .iter()
            .map(|b| b[1].clone())
            .collect::<Vec<_>>()
            .join(CONN);
        format!("{}\n{}\n{}", row(0), mid, row(2))
    }
}

/// Frame model-authored ASCII `art` in a box, with an optional `title` line
/// directly under the top border. Each art line is padded to the widest line so
/// the right border lines up. Empty `art` and no `title` -> empty string.
pub fn render_raw(art: &str, title: Option<&str>) -> String {
    let title = title.filter(|t| !t.is_empty());
    let lines: Vec<&str> = if art.is_empty() {
        Vec::new()
    } else {
        art.split('\n').collect()
    };
    if lines.is_empty() && title.is_none() {
        return String::new();
    }
    let inner = lines
        .iter()
        .map(|l| l.chars().count())
        .chain(title.map(|t| t.chars().count()))
        .max()
        .unwrap_or(0);
    let border = format!("+{}+", "-".repeat(inner + 2));
    let mut out = vec![border.clone()];
    if let Some(t) = title {
        out.push(format!(
            "| {}{} |",
            t,
            " ".repeat(inner - t.chars().count())
        ));
    }
    for l in &lines {
        out.push(format!(
            "| {}{} |",
            l,
            " ".repeat(inner - l.chars().count())
        ));
    }
    out.push(border);
    out.join("\n")
}

/// Build the tool result: the rendered diagram wrapped in the sentinel block
/// the TUI recognises. Shared by [`ShowDiagram::invoke`] and the unit tests so
/// the wire format has exactly one definition.
fn build(
    kind: &str,
    steps: Option<Vec<String>>,
    ascii: Option<String>,
    title: Option<String>,
    direction: Option<String>,
    width: Option<usize>,
) -> Result<String> {
    let body = match kind {
        "flow" => {
            let steps = steps.unwrap_or_default();
            if steps.is_empty() {
                bail!("show_diagram kind=flow needs a non-empty `steps` array");
            }
            let vertical = direction.as_deref() == Some("vertical");
            let width = width.unwrap_or(100).max(1);
            let art = render_flow(&steps, vertical, width);
            match title.filter(|t| !t.is_empty()) {
                Some(t) => format!("{t}\n{art}"),
                None => art,
            }
        }
        "raw" => {
            let art = ascii.unwrap_or_default();
            if art.trim().is_empty() {
                bail!("show_diagram kind=raw needs `ascii`");
            }
            render_raw(&art, title.as_deref())
        }
        other => bail!("unknown kind {other:?}; use \"flow\" or \"raw\""),
    };
    Ok(format!("{DIAGRAM_OPEN}\n{body}\n{DIAGRAM_CLOSE}"))
}

/// The `show_diagram` tool: render an ASCII diagram into the chat.
pub struct ShowDiagram;

static SHOW_DIAGRAM_SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "show_diagram".into(),
    description: "Render an ASCII-art diagram in the chat so you can explain a concept or a flow \
        visually. kind=\"flow\" lays out a sequence of `steps` (a pipeline, a process, a state \
        machine) as aligned boxes and arrows; kind=\"raw\" frames ASCII art you wrote yourself. \
        The diagram is shown verbatim (never line-wrapped), so keep it within ~100 columns."
        .into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["flow", "raw"],
                "description": "\"flow\" = a sequence of steps; \"raw\" = ASCII you supply in `ascii`."
            },
            "steps": {
                "type": "array",
                "items": { "type": "string" },
                "description": "kind=flow: the ordered steps to lay out as boxes."
            },
            "ascii": {
                "type": "string",
                "description": "kind=raw: the ASCII art to frame (may contain \\n)."
            },
            "title": {
                "type": "string",
                "description": "Optional title shown above the diagram."
            },
            "direction": {
                "type": "string",
                "enum": ["horizontal", "vertical"],
                "description": "kind=flow: layout direction (default horizontal; auto-falls back to vertical when too wide)."
            },
            "width": {
                "type": "integer",
                "description": "kind=flow: max columns for a horizontal row before falling back to vertical (default 100)."
            }
        },
        "required": ["kind"],
        "additionalProperties": false
    }),
});

#[async_trait]
impl Tool for ShowDiagram {
    fn spec(&self) -> &ToolSpec {
        &SHOW_DIAGRAM_SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            kind: String,
            #[serde(default)]
            steps: Option<Vec<String>>,
            #[serde(default)]
            ascii: Option<String>,
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            direction: Option<String>,
            #[serde(default)]
            width: Option<usize>,
        }
        let a: Args = serde_json::from_value(args)?;
        build(&a.kind, a.steps, a.ascii, a.title, a.direction, a.width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(steps: &[&str], vertical: bool, width: usize) -> String {
        let steps: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
        render_flow(&steps, vertical, width)
    }

    #[test]
    fn render_flow_horizontal() {
        assert_eq!(
            flow(&["a", "b"], false, 100),
            "+---+     +---+\n| a | --> | b |\n+---+     +---+"
        );
    }

    #[test]
    fn render_flow_single() {
        assert_eq!(flow(&["a"], false, 100), "+---+\n| a |\n+---+");
    }

    #[test]
    fn render_flow_vertical() {
        assert_eq!(
            flow(&["a", "b"], true, 100),
            "+---+\n| a |\n+---+\n  |\n  v\n+---+\n| b |\n+---+"
        );
    }

    #[test]
    fn render_flow_pads_differing_widths() {
        assert_eq!(
            flow(&["start", "a much longer step"], false, 100),
            "+--------------------+     +--------------------+\n\
             | start              | --> | a much longer step |\n\
             +--------------------+     +--------------------+"
        );
    }

    #[test]
    fn render_flow_falls_back_vertical_when_too_wide() {
        assert_eq!(flow(&["a", "b"], false, 10), flow(&["a", "b"], true, 10));
    }

    #[test]
    fn render_flow_empty() {
        assert_eq!(flow(&[], false, 100), "");
    }

    #[test]
    fn render_raw_plain() {
        assert_eq!(render_raw("x", None), "+---+\n| x |\n+---+");
    }

    #[test]
    fn render_raw_title() {
        assert_eq!(
            render_raw("ab\nc", Some("T")),
            "+----+\n| T  |\n| ab |\n| c  |\n+----+"
        );
    }

    #[test]
    fn render_raw_empty() {
        assert_eq!(render_raw("", None), "");
    }

    #[test]
    fn build_flow_block() {
        assert_eq!(
            build(
                "flow",
                Some(vec!["a".into(), "b".into()]),
                None,
                None,
                None,
                None
            )
            .unwrap(),
            "--- diagram (ascii) ---\n+---+     +---+\n| a | --> | b |\n+---+     +---+\n--- end diagram ---"
        );
    }

    #[test]
    fn build_flow_with_title() {
        assert_eq!(
            build(
                "flow",
                Some(vec!["a".into()]),
                None,
                Some("Flow".into()),
                Some("vertical".into()),
                None
            )
            .unwrap(),
            "--- diagram (ascii) ---\nFlow\n+---+\n| a |\n+---+\n--- end diagram ---"
        );
    }

    #[test]
    fn build_raw_block() {
        assert_eq!(
            build("raw", None, Some("x".into()), None, None, None).unwrap(),
            "--- diagram (ascii) ---\n+---+\n| x |\n+---+\n--- end diagram ---"
        );
    }

    #[test]
    fn build_flow_requires_steps() {
        assert!(build("flow", None, None, None, None, None).is_err());
        assert!(build("flow", Some(vec![]), None, None, None, None).is_err());
    }

    #[test]
    fn build_raw_requires_ascii() {
        assert!(build("raw", None, None, None, None, None).is_err());
        assert!(build("raw", None, Some("   ".into()), None, None, None).is_err());
    }

    #[test]
    fn build_unknown_kind() {
        assert!(build("pie", None, Some("x".into()), None, None, None).is_err());
    }
}
