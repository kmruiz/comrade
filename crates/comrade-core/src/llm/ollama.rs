use super::*;

/// Extract a display identity from an Ollama `/api/show` body's `details`
/// object, e.g. "7B (Q4_K_M)".
pub(crate) fn model_version_from_ollama_show(json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(json).ok()?;
    let details = value.get("details")?.as_object()?;
    let size = details
        .get("parameter_size")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let quant = details
        .get("quantization_level")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (size, quant) {
        (Some(size), Some(quant)) => Some(format!("{size} ({quant})")),
        (Some(size), None) => Some(size.to_string()),
        (None, Some(quant)) => Some(quant.to_string()),
        _ => None,
    }
}
