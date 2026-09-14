use super::*;

#[test]
fn decodes_sse_across_frames() {
    let mut dec = SseDecoder::new();
    // first frame splits an event in the middle
    let mut events = dec.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel");
    events.extend(
        dec.push(b"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n"),
    );
    events.extend(dec.push(b"data: [DONE]\n"));
    let mut contents = Vec::new();
    for e in events {
        match e {
            SseEvent::Data(json) => {
                let chunk: StreamChunk = serde_json::from_str(&json).unwrap();
                contents.push(
                    chunk
                        .choices
                        .into_iter()
                        .next()
                        .and_then(|c| c.delta.content)
                        .unwrap_or_default(),
                );
            }
            SseEvent::Done => contents.push("<DONE>".into()),
            SseEvent::Comment => {}
        }
    }
    assert_eq!(
        contents,
        vec!["Hello".to_string(), " world".to_string(), "<DONE>".into()]
    );
}

#[test]
fn ignores_comments_and_blank_lines() {
    let mut dec = SseDecoder::new();
    let events = dec
        .push(b": keep-alive\n\n: ping\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n");
    let data_count = events
        .iter()
        .filter(|e| matches!(e, SseEvent::Data(_)))
        .count();
    let comment_count = events
        .iter()
        .filter(|e| matches!(e, SseEvent::Comment))
        .count();
    assert_eq!(data_count, 1);
    assert_eq!(comment_count, 2);
}

#[test]
fn messages_serialize_native_wire_format() {
    let assistant = ChatMessage::assistant_with_calls(
        "".into(),
        vec![ToolCallMsg {
            id: "call_1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "a.rs", "content": "x"}),
        }],
    );
    let json = serde_json::to_value(&assistant).unwrap();
    let calls = json["tool_calls"][0].clone();
    assert_eq!(calls["id"], "call_1");
    assert_eq!(calls["type"], "function");
    assert_eq!(calls["function"]["name"], "write_file");
    assert_eq!(
        calls["function"]["arguments"],
        r#"{"content":"x","path":"a.rs"}"#
    );

    let tool = ChatMessage::tool_result("call_1", "wrote a.rs");
    let json = serde_json::to_value(&tool).unwrap();
    assert_eq!(json["role"], "tool");
    assert_eq!(json["tool_call_id"], "call_1");
}

#[test]
fn prompt_caching_marks_the_system_message_and_last_tool_only() {
    let msgs = vec![
        ChatMessage::new(Role::System, "sys"),
        ChatMessage::new(Role::User, "hi"),
    ];
    let tools = vec![
        ToolSpec {
            name: "a".into(),
            description: "".into(),
            json_schema: serde_json::json!({}),
        },
        ToolSpec {
            name: "b".into(),
            description: "".into(),
            json_schema: serde_json::json!({}),
        },
    ];

    let off = serde_json::to_value(ChatRequest::new(
        "m",
        &msgs,
        false,
        Some(&tools),
        0.2,
        false,
    ))
    .unwrap();
    assert!(off["messages"][0].get("cache_control").is_none());
    assert!(off["tools"][1].get("cache_control").is_none());

    let on =
        serde_json::to_value(ChatRequest::new("m", &msgs, false, Some(&tools), 0.2, true)).unwrap();
    assert_eq!(on["messages"][0]["cache_control"]["type"], "ephemeral");
    assert!(on["messages"][1].get("cache_control").is_none());
    assert_eq!(on["tools"][1]["cache_control"]["type"], "ephemeral");
    assert!(on["tools"][0].get("cache_control").is_none());
}
