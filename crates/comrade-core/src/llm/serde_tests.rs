use super::*;

#[test]
fn chat_message_serde_roundtrip() {
    let sys = ChatMessage::new(Role::System, "You are a helpful assistant");
    let sys_json = serde_json::to_string(&sys).unwrap();
    let sys_de: ChatMessage = serde_json::from_str(&sys_json).unwrap();
    assert_eq!(sys.role, sys_de.role);
    assert_eq!(sys.content, sys_de.content);
    assert_eq!(sys.tool_calls, sys_de.tool_calls);
    assert_eq!(sys.tool_call_id, sys_de.tool_call_id);

    // Test assistant with tool calls
    let assistant = ChatMessage::assistant_with_calls(
        "x".to_string(),
        vec![ToolCallMsg {
            id: "c1".into(),
            name: "fs_read".into(),
            arguments: serde_json::json!({"path": "x"}),
        }],
    );
    let assistant_json = serde_json::to_string(&assistant).unwrap();
    let assistant_de: ChatMessage = serde_json::from_str(&assistant_json).unwrap();
    assert_eq!(assistant.role, assistant_de.role);
    assert_eq!(assistant.content, assistant_de.content);
    assert_eq!(
        assistant.tool_calls.as_ref().unwrap().len(),
        assistant_de.tool_calls.as_ref().unwrap().len()
    );
    let call = &assistant.tool_calls.as_ref().unwrap()[0];
    let call_de = &assistant_de.tool_calls.as_ref().unwrap()[0];
    assert_eq!(call.id, call_de.id);
    assert_eq!(call.name, call_de.name);
    assert_eq!(call.arguments, call_de.arguments);

    // Test tool result
    let tool_result = ChatMessage::tool_result("c1", "ok");
    let tool_result_json = serde_json::to_string(&tool_result).unwrap();
    let tool_result_de: ChatMessage = serde_json::from_str(&tool_result_json).unwrap();
    assert_eq!(tool_result.role, tool_result_de.role);
    assert_eq!(tool_result.content, tool_result_de.content);
    assert_eq!(tool_result.tool_call_id, tool_result_de.tool_call_id);
}

fn png_part() -> comrade_tool::ImagePart {
    comrade_tool::ImagePart::from_bytes("shot.png", b"\x89PNG\r\n\x1a\n").unwrap()
}

/// A message with no image keeps the exact wire shape it always had, so prompt
/// caching and every provider that only understands a plain string still work.
#[test]
fn content_is_a_plain_string_without_images() {
    let m = ChatMessage::user("hello", Vec::new());
    let v = serde_json::to_value(&m).unwrap();
    assert_eq!(v["content"], serde_json::json!("hello"));
    assert_eq!(v["role"], serde_json::json!("user"));
    assert!(v.get("images").is_none(), "no extra field on the wire");
}

/// With an image the same message carries OpenAI-style content parts: the text
/// and the image in ONE message, never two turns.
#[test]
fn text_and_image_travel_as_content_parts() {
    let m = ChatMessage::user("why is this wrong?", vec![png_part()]);
    let v = serde_json::to_value(&m).unwrap();
    let parts = v["content"]
        .as_array()
        .expect("content becomes a parts array");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "why is this wrong?");
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(
        parts[1]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
}

#[test]
fn an_image_without_text_omits_the_empty_text_part() {
    let m = ChatMessage::user("", vec![png_part()]);
    let v = serde_json::to_value(&m).unwrap();
    let parts = v["content"].as_array().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["type"], "image_url");
}

#[test]
fn a_round_trip_keeps_the_text_and_the_image_payload() {
    let m = ChatMessage::user("look at this", vec![png_part()]);
    let json = serde_json::to_string(&m).unwrap();
    let back: ChatMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back.role, Role::User);
    assert_eq!(back.content, "look at this");
    assert_eq!(back.images.len(), 1);
    // The display name is not part of the wire form (no provider understands an
    // unknown field inside `image_url`), so only the payload comes back.
    assert_eq!(back.images[0].mime, m.images[0].mime);
    assert_eq!(back.images[0].base64, m.images[0].base64);
    assert_eq!(back.images[0].data_uri(), m.images[0].data_uri());
}
