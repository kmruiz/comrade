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
