use super::*;

#[test]
fn reads_openai_context_window() {
    let json = r#"{
          "object": "list",
          "data": [
            { "id": "other", "context_length": 4096 },
            { "id": "mistral:latest", "context_length": 32768 }
          ]
        }"#;
    assert_eq!(
        model_context_from_openai(json, "mistral:latest"),
        Some(32768)
    );
    assert_eq!(model_context_from_openai(json, "unknown"), None);
}

#[test]
fn reads_ollama_show_version() {
    let json = r#"{
          "details": {
            "parameter_size": "7.2B",
            "quantization_level": "Q4_K_M"
          }
        }"#;
    assert_eq!(
        model_version_from_ollama_show(json).as_deref(),
        Some("7.2B (Q4_K_M)")
    );
    assert_eq!(model_version_from_ollama_show("{}"), None);
}

#[test]
fn reads_ollama_show_context_length() {
    let json = r#"{
          "model_info": {
            "general.architecture": "llama",
            "llama.context_length": 8192,
            "llama.embedding_length": 4096
          }
        }"#;
    assert_eq!(model_context_from_ollama_show(json), Some(8192));
    assert_eq!(model_context_from_ollama_show("{}"), None);
}

#[test]
fn origins_are_derived() {
    assert_eq!(
        origin_of("http://localhost:11434/v1").as_deref(),
        Some("http://localhost:11434")
    );
    assert_eq!(
        origin_of("https://host.example/foo/bar").as_deref(),
        Some("https://host.example")
    );
}
