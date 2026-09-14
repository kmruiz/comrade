use super::*;

#[test]
fn deepseek_models_get_the_full_1m_window() {
    assert_eq!(heuristic_context("deepseek-v4-flash"), Some(1_000_000));
    assert_eq!(heuristic_context("deepseek-v4-pro"), Some(1_000_000));
    assert_eq!(
        heuristic_context("deepseek-v4-flash-vision-exp"),
        Some(1_000_000)
    );
    assert_eq!(heuristic_context("deepseek-chat"), Some(1_000_000));
}

#[test]
fn k_suffixes_are_recognized() {
    assert_eq!(heuristic_context("something-128k"), Some(131_072));
    assert_eq!(heuristic_context("foo-32k"), Some(32_768));
}

#[test]
fn unknown_models_return_none() {
    assert_eq!(heuristic_context("totally-unknown-model"), None);
}

#[test]
fn mistral_family_gets_128k() {
    assert_eq!(heuristic_context("mistral-large-latest"), Some(131_072));
    assert_eq!(heuristic_context("mistral-small-latest"), Some(131_072));
    assert_eq!(heuristic_context("devstral-small-2507"), Some(131_072));
    assert_eq!(heuristic_context("pixtral-large-latest"), Some(131_072));
    assert_eq!(heuristic_context("ministral-8b-latest"), Some(131_072));
    assert_eq!(heuristic_context("codestral-latest"), Some(32_768));
}

#[test]
fn claude_family_gets_200k() {
    assert_eq!(heuristic_context("claude-sonnet-4-20250514"), Some(200_000));
    assert_eq!(heuristic_context("claude-opus-4-1"), Some(200_000));
    assert_eq!(heuristic_context("claude-3-5-haiku-latest"), Some(200_000));
}
