use super::*;

/// Name-based context-window fallback for cloud providers whose `/models`
/// endpoint does not advertise context (e.g. DeepSeek).
pub(crate) fn heuristic_context(model: &str) -> Option<usize> {
    let m = model.to_lowercase();
    let suffix = [
        ("1.5m", 1_500_000usize),
        ("1m", 1_000_000),
        ("128k", 131_072),
        ("64k", 65_536),
        ("32k", 32_768),
        ("16k", 16_384),
        ("8k", 8_192),
        ("4k", 4_096),
        ("2k", 2_048),
    ];
    for (needle, size) in suffix {
        if m.contains(needle) {
            return Some(size);
        }
    }
    // DeepSeek models expose a ~1M token context window, but its `/models`
    // endpoint does not advertise it - fall back to the full window.
    if m.contains("deepseek") {
        return Some(1_000_000);
    }
    if m.contains("gpt-4o") {
        return Some(128_000);
    }
    // Mistral AI models: the `/models` endpoint advertises max_context_length,
    // but fall back to the family default when the probe is unavailable.
    if m.contains("codestral") {
        return Some(32_768); // Codestral (2405): 32K
    }
    if m.contains("mistral")
        || m.contains("devstral")
        || m.contains("pixtral")
        || m.contains("ministral")
        || m.contains("magistral")
    {
        return Some(131_072); // 128K (Large/Medium/Small, Devstral, Nemo, ...)
    }
    None
}

/// Extract the model's context window from an OpenAI-compatible `/models`
/// JSON body, matching by model id.
pub(crate) fn model_context_from_openai(json: &str, model: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(json).ok()?;
    let data = value.get("data")?.as_array()?;
    for entry in data {
        if entry.get("id").and_then(Value::as_str) != Some(model) {
            continue;
        }
        for key in [
            "context_length",
            "context_window",
            "max_context_length",
            "max_model_len",
            "context_size",
        ] {
            if let Some(n) = entry.get(key).and_then(Value::as_u64)
                && n > 0
            {
                return Some(n as usize);
            }
        }
    }
    None
}

/// Extract the context window from an Ollama `/api/show` body by scanning
/// `model_info` for any `...context_length` integer.
pub(crate) fn model_context_from_ollama_show(json: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(json).ok()?;
    let info = value
        .get("model_info")
        .or_else(|| value.get("model"))
        .or(Some(&value))?;
    let mut found: Option<usize> = None;
    fn walk(v: &Value, last_key: Option<&str>, out: &mut Option<usize>) {
        match v {
            Value::Number(n) => {
                if last_key.is_some_and(|k| k.ends_with("context_length"))
                    && let Some(u) = n.as_u64()
                    && u > 0
                {
                    *out = Some(u as usize);
                }
            }
            Value::Object(map) => {
                for (k, val) in map {
                    if out.is_some() {
                        return;
                    }
                    walk(val, Some(k.as_str()), out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    if out.is_some() {
                        return;
                    }
                    walk(item, last_key, out);
                }
            }
            _ => {}
        }
    }
    walk(info, None, &mut found);
    found
}

/// `scheme://host[:port]` from a base URL such as `http://localhost:11434/v1`.
pub(crate) fn origin_of(base_url: &str) -> Option<String> {
    let (scheme, rest) = base_url.split_once("://")?;
    let host = rest.split('/').next().unwrap_or(rest);
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}
