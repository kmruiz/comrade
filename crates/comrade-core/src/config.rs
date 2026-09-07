use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Deserialize;

/// How mutating tools get approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    #[default]
    Ask,
    /// Apply mutating operations without asking.
    Auto,
    /// Refuse mutating operations unless the model explicitly asks.
    Deny,
}

impl Autonomy {
    pub fn as_str(self) -> &'static str {
        match self {
            Autonomy::Ask => "ask",
            Autonomy::Auto => "auto",
            Autonomy::Deny => "deny",
        }
    }
}

/// How the agent talks to the model: native OpenAI-style tool calls when
/// possible, text ReAct, or auto (native with ReAct fallback per turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Auto,
    Native,
    React,
}

impl Protocol {
    /// Whether to advertise native `tools` on requests.
    pub fn native_enabled(self) -> bool {
        !matches!(self, Protocol::React)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmCfg {
    /// OpenAI-compatible base URL, e.g. `http://localhost:11434/v1`. When
    /// omitted and `provider` is set, the provider's preset URL is used.
    pub base_url: String,
    /// Optional API key; sent as `Authorization: Bearer` when set.
    pub api_key: Option<String>,
    /// Model identifier, e.g. `devstral-small-2`.
    pub model: String,
    /// Named provider preset (ollama, openai, deepseek, mistral, openrouter,
    /// groq, together). Sets `base_url` unless one is given explicitly.
    pub provider: Option<String>,
    pub temperature: f32,
    /// Seconds to wait for a response.
    pub timeout_secs: u64,
    /// Tool-calling protocol: auto | native | react.
    pub protocol: Protocol,
    /// Model context window in tokens. When `None` the app tries to detect it
    /// from the provider and falls back to the configured context budget.
    pub context_window: Option<usize>,
    /// Optional display identity/version string detected from the provider
    /// (e.g. Ollama "7B (Q4_K_M)").
    pub model_version: Option<String>,
}

impl Default for LlmCfg {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            model: "devstral-small-2".into(),
            provider: None,
            temperature: 0.2,
            timeout_secs: 600,
            protocol: Protocol::Auto,
            context_window: None,
            model_version: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentCfg {
    pub max_iterations: usize,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self { max_iterations: 30 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CtxCfg {
    /// Rolling budget for the visible chat history in tokens.
    pub budget_tokens: usize,
    /// Hard cap on how many characters of a tool result are injected back.
    pub max_tool_output_chars: usize,
}

impl Default for CtxCfg {
    fn default() -> Self {
        Self {
            budget_tokens: 6000,
            max_tool_output_chars: 5000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SecurityCfg {
    pub autonomy: Autonomy,
}

impl Default for SecurityCfg {
    fn default() -> Self {
        Self {
            autonomy: Autonomy::Ask,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub llm: LlmCfg,
    pub agent: AgentCfg,
    pub context: CtxCfg,
    pub security: SecurityCfg,
}

/// Config loaded from disk with defaults layered underneath.
pub struct LoadedConfig {
    pub config: Config,
    /// Path the config was read from, if any.
    pub source: Option<PathBuf>,
}

pub fn default_config_path() -> PathBuf {
    std::env::var_os("COMRADE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::home_dir().unwrap_or_default().join(".config"));
            base.join("comrade").join("config.toml")
        })
}

impl Config {
    /// Load configuration, layering an optional TOML file over defaults.
    pub fn load(path: Option<&Path>) -> Result<LoadedConfig> {
        let path = match path {
            Some(p) => Some(p.to_path_buf()),
            None => {
                let p = default_config_path();
                if p.exists() { Some(p) } else { None }
            }
        };

        let config = if let Some(p) = &path {
            let raw = std::fs::read_to_string(p)
                .with_context(|| format!("cannot read config {}", p.display()))?;
            let mut config = toml::from_str::<Config>(&raw)
                .with_context(|| format!("bad config in {}", p.display()))?;
            apply_provider(&raw, &mut config)?;
            config
        } else {
            Config::default()
        };
        Ok(LoadedConfig {
            config,
            source: path,
        })
    }

    pub fn auto_approve(&self) -> bool {
        matches!(self.security.autonomy, Autonomy::Auto)
    }

    /// Token budget to manage the history against: the model's real context
    /// window when known, otherwise the configured (conservative) budget.
    pub fn effective_budget(&self) -> usize {
        self.llm
            .context_window
            .unwrap_or(self.context.budget_tokens)
    }
}

/// Base URL for a named provider, or `None` for unknown names.
pub fn provider_base_url(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "ollama" | "local" => Some("http://localhost:11434/v1"),
        "openai" => Some("https://api.openai.com/v1"),
        "deepseek" => Some("https://api.deepseek.com/v1"),
        "mistral" => Some("https://api.mistral.ai/v1"),
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        "groq" => Some("https://api.groq.com/openai/v1"),
        "together" => Some("https://api.together.xyz/v1"),
        _ => None,
    }
}

/// When `llm.provider` is set and the config did not explicitly set
/// `llm.base_url`, fill the provider's base URL from the preset.
fn apply_provider(raw_toml: &str, config: &mut Config) -> Result<()> {
    let Some(provider) = config.llm.provider.as_deref() else {
        return Ok(());
    };
    let base_url_explicit = match toml::from_str::<toml::Value>(raw_toml) {
        Ok(v) => v.get("llm").and_then(|l| l.get("base_url")).is_some(),
        Err(_) => false,
    };
    if base_url_explicit {
        return Ok(());
    }
    match provider_base_url(provider) {
        Some(url) => {
            config.llm.base_url = url.to_string();
            Ok(())
        }
        None => anyhow::bail!(
            "unknown llm.provider {provider:?}. Known providers: ollama, openai, deepseek, mistral, openrouter, groq, together"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(toml: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "comrade-config-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, toml).unwrap();
        p
    }

    #[test]
    fn deepseek_provider_fills_base_url() {
        let p = write_tmp(
            "[llm]\nprovider = \"deepseek\"\napi_key = \"sk-test\"\nmodel = \"deepseek-chat\"\n",
        );
        let c = Config::load(Some(&p)).unwrap().config;
        assert_eq!(c.llm.base_url, "https://api.deepseek.com/v1");
        assert_eq!(c.llm.api_key.as_deref(), Some("sk-test"));
        assert_eq!(c.llm.model, "deepseek-chat");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn explicit_base_url_wins_over_provider() {
        let p = write_tmp(
            "[llm]\nprovider = \"deepseek\"\nbase_url = \"http://proxy.example/v1\"\nmodel = \"x\"\n",
        );
        let c = Config::load(Some(&p)).unwrap().config;
        assert_eq!(c.llm.base_url, "http://proxy.example/v1");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let p = write_tmp("[llm]\nprovider = \"skynet\"\nmodel = \"x\"\n");
        assert!(Config::load(Some(&p)).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn preset_lookup_covers_deepseek() {
        assert_eq!(
            provider_base_url("DeepSeek"),
            Some("https://api.deepseek.com/v1")
        );
    }
}
