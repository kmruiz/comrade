//! Declarative interactive forms ("custom components") shown in the chat.
//!
//! An agent describes a form as JSON; the UI renders each field as an
//! interactive component (text box, number spinner, date picker, select list,
//! checkbox). When the human submits, the answers come back keyed by field id
//! and the agent receives them as `id = value` lines.
//!
//! This module is the shared contract: the tool crates build a [`FormSpec`],
//! the TUI renders it, and helpers here seed default values and format answers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A whole form: a heading plus an ordered list of fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FormSpec {
    /// Heading shown as the dialog title.
    #[serde(default)]
    pub title: String,
    /// Optional explanatory line shown under the heading.
    #[serde(default)]
    pub description: Option<String>,
    /// Fields in display order. Each must carry a unique `id`.
    #[serde(default)]
    pub fields: Vec<FormField>,
}

/// One input component: a stable `id`, a human label, and a kind + parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FormField {
    /// Identifier the agent receives the answer under.
    pub id: String,
    /// Human-readable label shown next to the component.
    pub label: String,
    /// The component type and its parameters (flattened into this object).
    #[serde(flatten)]
    pub kind: FieldKind,
    /// When true the form cannot be submitted while this field is empty.
    #[serde(default)]
    pub required: bool,
    /// Optional initial value (as text).
    #[serde(default)]
    pub default: Option<String>,
    /// Optional suggested value: prefills the field (shown as "recommended")
    /// and is what auto mode / a sub-agent submits. Takes precedence over
    /// `default`.
    #[serde(default)]
    pub recommended: Option<String>,
}

/// The kind of interactive component, tagged in JSON by `"kind"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldKind {
    /// Single-line free text.
    Text {
        #[serde(default)]
        placeholder: Option<String>,
    },
    /// Numeric spinner; `step` defaults to 1.
    Number {
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
        #[serde(default)]
        step: Option<f64>,
    },
    /// Date picker; the value is an ISO `YYYY-MM-DD` string.
    Date,
    /// One choice out of a fixed list.
    Select { options: Vec<String> },
    /// Boolean toggle; the value is `"true"` or `"false"`.
    Checkbox,
    /// Pick one of several code diffs (e.g. two competing patches). Rendered as
    /// the diffs themselves; the value is the chosen option's `label`.
    DiffChoice { options: Vec<DiffOption> },
}

/// One candidate in a [`FieldKind::DiffChoice`]: a label plus the diff text to
/// show for it (a unified diff, or plain lines where `+`/`-` mark changes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffOption {
    /// Human-readable label handed back as the answer when this option is chosen.
    pub label: String,
    /// The code diff shown for this option.
    pub diff: String,
}

impl FormSpec {
    /// Default answers for every field, in field order.
    ///
    /// Each value is the field's recommended value when it has one, else its
    /// `default`, else a kind-appropriate seed. Used both to prefill the UI and
    /// to auto-answer a form from a context that cannot show one (a sub-agent's
    /// `UserIo`, or headless/auto mode).
    pub fn initial_values(&self) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|f| (f.id.clone(), f.initial_value()))
            .collect()
    }

    /// True when every required field has a non-empty answer.
    ///
    /// A field with no entry at all counts as empty.
    pub fn is_complete(&self, answers: &BTreeMap<String, String>) -> bool {
        self.fields
            .iter()
            .all(|f| !f.required || answers.get(&f.id).is_some_and(|v| !v.trim().is_empty()))
    }

    /// Render `answers` as the `id = value` lines handed back to the agent.
    ///
    /// Fields are emitted in spec order; ids without a field are ignored and
    /// fields without an answer are skipped.
    pub fn answer_lines(&self, answers: &BTreeMap<String, String>) -> String {
        self.fields
            .iter()
            .filter_map(|f| answers.get(&f.id).map(|v| format!("{} = {}", f.id, v)))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl FormField {
    /// The value this field starts with: its `recommended` value if set, else
    /// its `default`, else a kind-appropriate seed (number's `min`, a select's
    /// or diff-choice's first option, checkbox off).
    pub fn initial_value(&self) -> String {
        // A recommended value wins over the plain default (and is checked
        // against min/max-free parsing like a default).
        if let Some(r) = self.recommended.as_deref() {
            return match self.kind {
                FieldKind::Checkbox => truthy(r).to_string(),
                _ => r.to_string(),
            };
        }
        match (&self.kind, self.default.as_deref()) {
            (FieldKind::Checkbox, Some(d)) => truthy(d).to_string(),
            (FieldKind::Checkbox, None) => "false".to_string(),
            (_, Some(d)) => d.to_string(),
            (FieldKind::Number { min, .. }, None) => {
                min.map(fmt_num).unwrap_or_else(|| "0".to_string())
            }
            (FieldKind::Select { options }, None) => options.first().cloned().unwrap_or_default(),
            (FieldKind::DiffChoice { options }, None) => {
                options.first().map(|o| o.label.clone()).unwrap_or_default()
            }
            _ => String::new(),
        }
    }

    /// Whether this field carries a non-blank recommended value.
    pub fn has_recommended(&self) -> bool {
        self.recommended
            .as_ref()
            .is_some_and(|r| !r.trim().is_empty())
    }
}

/// Interpret a textual value as a boolean (for checkbox defaults).
pub fn truthy(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "y" | "on"
    )
}

/// Format an f64 the way a spinner shows it (whole numbers without `.0`).
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> FormSpec {
        serde_json::from_value(value).expect("valid spec")
    }

    #[test]
    fn parses_every_field_kind() {
        let spec = parse(json!({
            "title": "Booking",
            "description": "Pick the details",
            "fields": [
                { "id": "name", "label": "Name", "kind": "text", "placeholder": "Jane" },
                { "id": "guests", "label": "Guests", "kind": "number", "min": 1, "max": 8, "step": 1 },
                { "id": "date", "label": "Date", "kind": "date" },
                { "id": "room", "label": "Room", "kind": "select", "options": ["single", "double"] },
                { "id": "breakfast", "label": "Breakfast", "kind": "checkbox" }
            ]
        }));
        assert_eq!(spec.title, "Booking");
        assert_eq!(spec.fields.len(), 5);
        assert_eq!(spec.fields[0].id, "name");
        assert!(matches!(spec.fields[0].kind, FieldKind::Text { .. }));
        assert!(matches!(
            spec.fields[1].kind,
            FieldKind::Number { min: Some(_), .. }
        ));
        assert!(matches!(spec.fields[2].kind, FieldKind::Date));
        assert!(matches!(spec.fields[3].kind, FieldKind::Select { .. }));
        assert!(matches!(spec.fields[4].kind, FieldKind::Checkbox));
    }

    #[test]
    fn initial_values_seed_each_kind() {
        let spec = parse(json!({
            "fields": [
                { "id": "guests", "label": "Guests", "kind": "number", "min": 2 },
                { "id": "room", "label": "Room", "kind": "select", "options": ["double", "single"] },
                { "id": "breakfast", "label": "Breakfast", "kind": "checkbox", "default": "yes" },
                { "id": "notes", "label": "Notes", "kind": "text", "default": "none" },
                { "id": "date", "label": "Date", "kind": "date" }
            ]
        }));
        let v = spec.initial_values();
        assert_eq!(v["guests"], "2");
        assert_eq!(v["room"], "double");
        assert_eq!(v["breakfast"], "true");
        assert_eq!(v["notes"], "none");
        assert_eq!(v["date"], "");
    }

    #[test]
    fn required_fields_gate_submission() {
        let spec = parse(json!({
            "fields": [
                { "id": "name", "label": "Name", "kind": "text", "required": true },
                { "id": "notes", "label": "Notes", "kind": "text" }
            ]
        }));
        let mut answers = spec.initial_values();
        assert!(!spec.is_complete(&answers));
        answers.insert("name".into(), "Jane".into());
        assert!(spec.is_complete(&answers));
    }

    #[test]
    fn answer_lines_follows_spec_order() {
        let spec = parse(json!({
            "fields": [
                { "id": "guests", "label": "Guests", "kind": "number" },
                { "id": "room", "label": "Room", "kind": "select", "options": ["single"] },
                { "id": "notes", "label": "Notes", "kind": "text" }
            ]
        }));
        let mut answers = spec.initial_values();
        answers.insert("guests".into(), "3".into());
        answers.insert("notes".into(), "late arrival".into());
        assert_eq!(
            spec.answer_lines(&answers),
            "guests = 3\nroom = single\nnotes = late arrival"
        );
    }

    #[test]
    fn recommended_value_wins_over_default() {
        let spec = parse(json!({
            "fields": [
                { "id": "guests", "label": "Guests", "kind": "number", "min": 1, "default": "1", "recommended": "4" },
                { "id": "room", "label": "Room", "kind": "select", "options": ["single", "double"], "recommended": "double" },
                { "id": "breakfast", "label": "Breakfast", "kind": "checkbox", "recommended": "yes" },
                { "id": "date", "label": "Date", "kind": "date" },
                { "id": "notes", "label": "Notes", "kind": "text", "default": "none" }
            ]
        }));
        let v = spec.initial_values();
        assert_eq!(v["guests"], "4");
        assert_eq!(v["room"], "double");
        assert_eq!(v["breakfast"], "true");
        assert_eq!(v["date"], "");
        assert_eq!(v["notes"], "none");
    }

    #[test]
    fn parses_diff_choice_field() {
        let spec = parse(json!({
            "fields": [
                { "id": "pick", "label": "Choose a patch", "kind": "diff_choice",
                  "options": [
                      { "label": "A", "diff": "-old\n+new" },
                      { "label": "B", "diff": "-old\n+other" }
                  ] }
            ]
        }));
        assert!(matches!(spec.fields[0].kind, FieldKind::DiffChoice { .. }));
        // Without a recommended value the first option's label is the seed.
        assert_eq!(spec.fields[0].initial_value(), "A");

        // A recommended label wins over the first option.
        let spec = parse(json!({
            "fields": [
                { "id": "pick", "label": "Choose a patch", "kind": "diff_choice",
                  "recommended": "B",
                  "options": [
                      { "label": "A", "diff": "-old\n+new" },
                      { "label": "B", "diff": "-old\n+other" }
                  ] }
            ]
        }));
        assert_eq!(spec.fields[0].initial_value(), "B");
    }

    #[test]
    fn has_recommended_reports_presence() {
        let spec = parse(json!({
            "fields": [
                { "id": "a", "label": "A", "kind": "text", "recommended": "hi" },
                { "id": "b", "label": "B", "kind": "text", "recommended": "  " },
                { "id": "c", "label": "C", "kind": "text" }
            ]
        }));
        assert!(spec.fields[0].has_recommended());
        assert!(!spec.fields[1].has_recommended());
        assert!(!spec.fields[2].has_recommended());
    }
}
