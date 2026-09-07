use anyhow::Result;
use comrade_tool::ToolRegistry;
use serde_json::Value;

/// The parsed shape of one assistant turn.
#[derive(Debug, Clone)]
pub struct ParsedTurn {
    /// Leading "Thought:" prose, when present.
    pub thought: Option<String>,
    /// The requested tool call, if the model asked for one.
    pub tool_call: Option<ToolCall>,
    /// When `tool_call` is `None`, this is the model's final answer.
    pub final_text: String,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

/// Build the system prompt that sets up the ReAct loop for a model.
pub fn build_system_prompt(project_root: &str, tools: &ToolRegistry, budget: usize) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are Comrade, a software engineering agent that works in a code repository \
         through tools. Keep your context footprint small: only read what you need, prefer \
         precise small edits, and never dump whole files back into the conversation.\n\n",
    );
    prompt.push_str(&format!("Working directory: {project_root}\n"));
    prompt.push_str(&format!(
        "Context budget is about {budget} tokens. Be terse.\n\n"
    ));

    prompt.push_str("## Tools\n");
    prompt.push_str("You can use the following tools, one per turn:\n");
    for tool in tools.iter() {
        prompt.push_str(&render_tool(tool.spec()));
        prompt.push('\n');
    }

    prompt.push_str(
        "\n## Protocol\n\
         Think and act step by step using this exact format, one tool per turn:\n\
         \n\
         Thought: <what you are doing and why, one or two short lines>\n\
         Tool: <tool_name>\n\
         Args: <JSON object with the tool's arguments>\n\
         \n\
         After each tool call you will receive:\n\
         \n\
         Observation: <the tool result>\n\
         \n\
         Then continue with another Thought/Tool/Args turn. Do not repeat a Thought you already sent. \
         If a tool fails, read the error and adapt.\n\
         When the task is fully done, reply with ONLY your final summary message to the user — no Tool line. \
         Do not claim work is done unless you have actually verified it via tool output.\n",
    );
    prompt
}

fn render_tool(spec: &comrade_tool::ToolSpec) -> String {
    let mut out = String::new();
    out.push_str(&format!("### {}\n", spec.name));
    out.push_str(&format!("{}\n", spec.description));
    if let Some(props) = spec
        .json_schema
        .get("properties")
        .and_then(Value::as_object)
    {
        let fields: Vec<String> = props
            .iter()
            .map(|(k, v)| {
                let ty = v.get("type").and_then(Value::as_str).unwrap_or("any");
                let req = spec
                    .json_schema
                    .get("required")
                    .and_then(Value::as_array)
                    .is_some_and(|r| r.iter().any(|x| x.as_str() == Some(k)));
                if req {
                    format!("{k}: {ty} (required)")
                } else {
                    format!("{k}: {ty} (optional)")
                }
            })
            .collect();
        out.push_str(&format!("Args: {{ {} }}\n", fields.join(", ")));
    } else {
        out.push_str("Args: {}\n");
    }
    out
}

/// Parse an assistant message into a thought + (optional) tool call.
///
/// Accepts the canonical multiline form and a compact single-line form. When no
/// `Tool:` marker is present the whole message is the final answer.
pub fn parse_turn(text: &str) -> Result<ParsedTurn> {
    let trimmed = text.trim();
    let thought = extract_thought(trimmed);

    let Some(tool_idx) = find_marker(trimmed, "Tool:") else {
        return Ok(ParsedTurn {
            thought,
            tool_call: None,
            final_text: trimmed.to_string(),
        });
    };

    let name_seg = &trimmed[tool_idx + "Tool:".len()..];
    let name = name_seg
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(':')
        .to_string();
    if name.is_empty() {
        anyhow::bail!("found a Tool: marker with no tool name");
    }

    // JSON object starts after the "Args:" marker that follows the tool name.
    let args = match find_marker(&trimmed[tool_idx..], "Args:") {
        Some(offset) => {
            let seg = &trimmed[tool_idx + offset + "Args:".len()..];
            match seg.find('{') {
                Some(open) => {
                    let balanced = balanced_object(seg, open)?;
                    serde_json::from_str(balanced)
                        .map_err(|e| anyhow::anyhow!("Args is not valid JSON: {e}"))?
                }
                None => Value::Object(Default::default()),
            }
        }
        None => Value::Object(Default::default()),
    };

    Ok(ParsedTurn {
        thought,
        tool_call: Some(ToolCall { name, args }),
        final_text: trimmed.to_string(),
    })
}

/// Extract the prose right after a leading `Thought:` marker (first line only).
fn extract_thought(text: &str) -> Option<String> {
    let idx = find_marker(text, "Thought:")?;
    let seg = &text[idx + "Thought:".len()..];
    let line = seg.split('\n').next().unwrap_or("");
    let content = line.trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

fn find_marker(text: &str, marker: &str) -> Option<usize> {
    text.find(marker)
}

/// Return the balanced `{...}` substring starting at `open` (an index into `s`).
fn balanced_object<'a>(s: &'a str, open: usize) -> Result<&'a str> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    for i in open..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&s[open..=i]);
                }
            }
            _ => {}
        }
    }
    anyhow::bail!("unbalanced braces in Args JSON")
}

/// Wrap a tool result as the "Observation" the model sees next.
pub fn render_observation(tool_name: &str, output: &str) -> String {
    format!(
        "Observation (result of `{tool_name}`):\n{output}\n\nContinue with Thought/Tool/Args, or give your final answer if the task is done."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_tool_call() {
        let turn = parse_turn(
            "Thought: I should read the main file.\nTool: read_file\nArgs: {\"path\": \"src/main.rs\"}",
        )
        .unwrap();
        assert_eq!(
            turn.thought.as_deref(),
            Some("I should read the main file.")
        );
        let call = turn.tool_call.unwrap();
        assert_eq!(call.name, "read_file");
        assert_eq!(call.args["path"], "src/main.rs");
    }

    #[test]
    fn parses_multiline_json() {
        let turn = parse_turn(
            "Thought: rename the fn\nTool: rename\nArgs: {\n  \"symbol\": \"foo\",\n  \"new\": \"bar\"\n}",
        )
        .unwrap();
        let call = turn.tool_call.unwrap();
        assert_eq!(call.name, "rename");
        assert_eq!(call.args["symbol"], "foo");
        assert_eq!(call.args["new"], "bar");
    }

    #[test]
    fn no_tool_means_final_answer() {
        let turn = parse_turn("All done. I renamed the function.").unwrap();
        assert!(turn.tool_call.is_none());
        assert_eq!(turn.final_text, "All done. I renamed the function.");
    }

    #[test]
    fn missing_args_defaults_to_empty() {
        let turn = parse_turn("Thought: ok\nTool: git_status").unwrap();
        let call = turn.tool_call.unwrap();
        assert_eq!(call.name, "git_status");
        assert!(call.args.as_object().unwrap().is_empty());
    }

    #[test]
    fn thought_not_required() {
        let turn = parse_turn("Tool: list_dir\nArgs: {\"path\": \".\"}").unwrap();
        assert!(turn.thought.is_none());
        assert!(turn.tool_call.is_some());
    }
}
