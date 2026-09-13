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

impl LlmCfg {
    /// Short display label used in the UI and in tool output to name the model,
    /// e.g. `ollama/devstral-small-2`; the bare model id when no provider is
    /// configured. The chat transcript attributes every message/action to the
    /// model that produced it using this label.
    pub fn display(&self) -> String {
        match &self.provider {
            Some(p) => format!("{p}/{}", self.model),
            None => self.model.clone(),
        }
    }
}

/// An extra model — usually a cheaper/faster one, possibly on another provider
/// — that the main "tech lead" model can delegate self-contained sub-tasks to
/// via the `delegate` tool. One `[[delegates]]` entry per model.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DelegateCfg {
    /// Unique name the tech lead uses to select this model, e.g. `groq`.
    pub name: String,
    /// Short human-readable blurb of when to use this model (shown to the
    /// tech lead so it can pick the right developer for a task).
    pub description: String,
    /// Whether this delegate can be used by the `delegate`/`ask_advise` tools.
    /// Defaults to `true`; set `enabled = false` to keep the entry in the
    /// config but stop the tech lead using it to delegate (it is then not
    /// validated, not advertised and not selectable).
    pub enabled: bool,
    /// Approval policy for running this model via the `delegate` and
    /// `ask_advise` tools. `auto` (default) runs without asking, like any
    /// un-gated delegate; `ask` pauses for human approval before each use
    /// (skipped under `[security] autonomy = "auto"`); `deny` refuses to run
    /// this model through `delegate`/`ask_advise` at all, even auto-approved.
    pub approval: Autonomy,
    /// The model settings for this delegate: `provider`, `model`, `api_key`,
    /// `temperature`, ... written inline at the same level as `name`.
    #[serde(flatten)]
    pub llm: LlmCfg,
}

impl Default for DelegateCfg {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            enabled: true,
            approval: Autonomy::Auto,
            llm: LlmCfg::default(),
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
    /// Extra developer models the tech lead can delegate sub-tasks to (see the
    /// `delegate` tool).
    pub delegates: Vec<DelegateCfg>,
    /// External MCP servers whose tools are bridged into the agent.
    pub mcp: McpConfig,
}

/// Configuration for the built-in MCP (Model Context Protocol) client.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpConfig {
    pub servers: Vec<McpServerCfg>,
}

/// One external MCP server the agent can call tools on.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct McpServerCfg {
    /// Unique name; namespaces this server's tools as `mcp_<name>_<tool>`.
    pub name: String,
    /// How to reach the server (stdio child process or streamable HTTP).
    pub transport: McpTransport,
    /// Optional authentication for HTTP servers.
    #[serde(default)]
    pub auth: Option<McpAuth>,
}

/// Transport for an MCP server connection.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransport {
    /// Spawn a local process and speak JSON-RPC 2.0 over its stdin/stdout.
    Stdio {
        /// Executable to run (e.g. `npx`, `uvx`, a local binary).
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment for the child. Values starting with `$` read the
        /// named variable from Comrade's own environment at connect time.
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
    },
    /// Remote MCP server speaking the streamable HTTP transport.
    Http {
        /// Base URL of the MCP endpoint (e.g. `https://mcp.example.com/mcp`).
        url: String,
    },
}

/// Authentication applied to MCP HTTP requests.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum McpAuth {
    /// Static API key sent with every request.
    #[serde(rename = "api_key")]
    ApiKey {
        /// Literal key, or `$NAME` to read the key from Comrade's environment.
        key: String,
        /// Header to carry the key in. Defaults to `Authorization`, in which
        /// case the value is `Bearer <key>`; any other header gets the raw key.
        #[serde(default)]
        header: Option<String>,
    },
    /// OpenID Connect: OAuth 2.0 authorization-code flow with PKCE.
    #[serde(rename = "oidc")]
    Oidc {
        /// OAuth client id this Comrade install identifies as.
        client_id: String,
        /// Authorization server metadata URL. When unset, discovered from the
        /// transport URL's origin per RFC 8414 (`/.well-known/oauth-authorization-server`).
        #[serde(default)]
        issuer: Option<String>,
        #[serde(default = "default_oidc_scopes")]
        scopes: Vec<String>,
        /// Fixed loopback redirect port; when unset an ephemeral port is used.
        #[serde(default)]
        redirect_port: Option<u16>,
        /// Optional OAuth audience/resource indicator.
        #[serde(default)]
        audience: Option<String>,
    },
}

fn default_oidc_scopes() -> Vec<String> {
    ["openid", "profile", "email"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Expand a `$NAME`-prefixed config value against a lookup (normally
/// `std::env::var`). Literal values pass through untouched; a `$`-reference
/// whose variable is unset keeps its literal text so the misconfiguration is
/// visible rather than silently empty.
pub fn expand_env_value(value: &str, lookup: &impl Fn(&str) -> Option<String>) -> String {
    match value.strip_prefix('$').filter(|n| !n.is_empty()) {
        Some(name) => lookup(name).unwrap_or_else(|| value.to_string()),
        None => value.to_string(),
    }
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
/// `llm.base_url`, fill the provider's base URL from the preset. Delegate
/// entries get the same treatment per entry.
fn apply_provider(raw_toml: &str, config: &mut Config) -> Result<()> {
    let raw = toml::from_str::<toml::Value>(raw_toml).ok();
    let llm_has_explicit_url = raw
        .as_ref()
        .and_then(|v| v.get("llm"))
        .and_then(|l| l.get("base_url"))
        .is_some();
    fill_provider_base_url(
        config.llm.provider.as_deref(),
        llm_has_explicit_url,
        &mut config.llm.base_url,
        "llm",
    )?;

    let explicit: Vec<bool> = raw
        .as_ref()
        .and_then(|v| v.get("delegates"))
        .and_then(|d| d.as_array())
        .map(|arr| arr.iter().map(|e| e.get("base_url").is_some()).collect())
        .unwrap_or_default();
    for (i, delegate) in config.delegates.iter_mut().enumerate() {
        let has_explicit_url = explicit.get(i).copied().unwrap_or(false);
        fill_provider_base_url(
            delegate.llm.provider.as_deref(),
            has_explicit_url,
            &mut delegate.llm.base_url,
            &format!("delegates[{}]", delegate.name),
        )?;
    }
    Ok(())
}

/// Fill `base_url` from a named provider preset unless the config already set
/// one explicitly. Unknown provider names are an error.
fn fill_provider_base_url(
    provider: Option<&str>,
    has_explicit_url: bool,
    base_url: &mut String,
    where_: &str,
) -> Result<()> {
    let Some(provider) = provider else {
        return Ok(());
    };
    if has_explicit_url {
        return Ok(());
    }
    match provider_base_url(provider) {
        Some(url) => {
            *base_url = url.to_string();
            Ok(())
        }
        None => anyhow::bail!(
            "unknown provider {provider:?} in {where_}. Known providers: ollama, openai, deepseek, mistral, openrouter, groq, together"
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
    fn display_label_prefers_provider_qualified_form() {
        let mut cfg = LlmCfg {
            provider: Some("ollama".into()),
            model: "devstral-small-2".into(),
            ..Default::default()
        };
        assert_eq!(cfg.display(), "ollama/devstral-small-2");
        cfg.provider = None;
        assert_eq!(cfg.display(), "devstral-small-2");
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
    fn delegates_parse_with_provider_presets_and_defaults() {
        let p = write_tmp(
            r#"
            [llm]
            provider = "deepseek"
            model = "deepseek-chat"

            [[delegates]]
            name = "groq-fast"
            description = "Groq Llama 3.3 70B - very fast and cheap"
            approval = "ask"
            provider = "groq"
            model = "llama-3.3-70b-versatile"
            api_key = "gsk-x"

            [[delegates]]
            name = "local-tiny"
            enabled = false
            provider = "ollama"
            model = "qwen3:4b"
            "#,
        );
        let c = Config::load(Some(&p)).unwrap().config;
        let _ = std::fs::remove_file(&p);
        assert_eq!(c.delegates.len(), 2);
        let groq = &c.delegates[0];
        assert_eq!(groq.name, "groq-fast");
        assert_eq!(groq.description, "Groq Llama 3.3 70B - very fast and cheap");
        // provider preset fills the base URL...
        assert_eq!(groq.llm.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(groq.llm.model, "llama-3.3-70b-versatile");
        assert_eq!(groq.llm.api_key.as_deref(), Some("gsk-x"));
        // ...and omitted scalar settings fall back to defaults.
        assert_eq!(groq.llm.temperature, 0.2);
        assert_eq!(groq.llm.timeout_secs, 600);
        // `approval` parses and defaults to Auto (ungated) when omitted.
        assert_eq!(groq.approval, Autonomy::Ask);
        assert_eq!(c.delegates[1].approval, Autonomy::Auto);
        // `enabled` defaults to true and parses `false`.
        assert!(groq.enabled);
        assert!(!c.delegates[1].enabled);
        // second delegate has no description and still resolves its provider.
        assert_eq!(c.delegates[1].llm.base_url, "http://localhost:11434/v1");
        assert!(c.delegates[1].description.is_empty());
    }

    #[test]
    fn delegate_explicit_base_url_wins() {
        let p = write_tmp(
            r#"
            [[delegates]]
            name = "proxy"
            provider = "openai"
            base_url = "http://proxy.example/v1"
            model = "gpt-4o-mini"
            "#,
        );
        let c = Config::load(Some(&p)).unwrap().config;
        let _ = std::fs::remove_file(&p);
        assert_eq!(c.delegates.len(), 1);
        assert_eq!(c.delegates[0].llm.base_url, "http://proxy.example/v1");
    }

    #[test]
    fn unknown_delegate_provider_is_rejected() {
        let p = write_tmp(
            "[llm]\nprovider = \"ollama\"\nmodel = \"x\"\n[[delegates]]\nname = \"d\"\nprovider = \"skynet\"\nmodel = \"y\"\n",
        );
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

    #[test]
    fn preset_lookup_covers_mistral() {
        assert_eq!(
            provider_base_url("mistral"),
            Some("https://api.mistral.ai/v1")
        );
        assert_eq!(
            provider_base_url("Mistral"),
            Some("https://api.mistral.ai/v1")
        );
    }

    #[test]
    fn mistral_provider_fills_base_url() {
        let p = write_tmp(
            "[llm]\nprovider = \"mistral\"\napi_key = \"sk-mistral\"\nmodel = \"mistral-large-latest\"\n",
        );
        let c = Config::load(Some(&p)).unwrap().config;
        let _ = std::fs::remove_file(&p);
        assert_eq!(c.llm.base_url, "https://api.mistral.ai/v1");
        assert_eq!(c.llm.api_key.as_deref(), Some("sk-mistral"));
        assert_eq!(c.llm.model, "mistral-large-latest");
        assert_eq!(c.llm.display(), "mistral/mistral-large-latest");
    }

    #[test]
    fn mcp_defaults_to_no_servers() {
        let c = Config::default();
        assert!(c.mcp.servers.is_empty());
        let p = write_tmp("[llm]\nprovider = \"ollama\"\nmodel = \"x\"\n");
        let c = Config::load(Some(&p)).unwrap().config;
        let _ = std::fs::remove_file(&p);
        assert!(c.mcp.servers.is_empty());
    }

    #[test]
    fn mcp_parses_stdio_and_http_with_api_key() {
        let p = write_tmp(
            r#"
            [llm]
            provider = "ollama"
            model = "x"

            [[mcp.servers]]
            name = "fs"
            [mcp.servers.transport]
            type = "stdio"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-filesystem"]
            [mcp.servers.transport.env]
            TOKEN = "$FS_TOKEN"

            [[mcp.servers]]
            name = "remote"
            [mcp.servers.transport]
            type = "http"
            url = "https://mcp.example.com/mcp"
            [mcp.servers.auth]
            type = "api_key"
            key = "$REMOTE_KEY"
            "#,
        );
        let c = Config::load(Some(&p)).unwrap().config;
        let _ = std::fs::remove_file(&p);
        assert_eq!(c.mcp.servers.len(), 2);

        let fs = &c.mcp.servers[0];
        assert_eq!(fs.name, "fs");
        assert_eq!(fs.auth, None);
        let McpTransport::Stdio { command, args, env } = &fs.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(command, "npx");
        assert_eq!(
            args,
            &vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string()
            ]
        );
        assert_eq!(env.get("TOKEN").map(String::as_str), Some("$FS_TOKEN"));

        let remote = &c.mcp.servers[1];
        let McpTransport::Http { url } = &remote.transport else {
            panic!("expected http transport");
        };
        assert_eq!(url, "https://mcp.example.com/mcp");
        assert_eq!(
            remote.auth,
            Some(McpAuth::ApiKey {
                key: "$REMOTE_KEY".into(),
                header: None
            })
        );
    }

    #[test]
    fn mcp_oidc_auth_parses_full_and_defaults_scopes() {
        let p = write_tmp(
            r#"
            [llm]
            provider = "ollama"
            model = "x"

            [[mcp.servers]]
            name = "secure"
            [mcp.servers.transport]
            type = "http"
            url = "https://secure.example/mcp"
            [mcp.servers.auth]
            type = "oidc"
            client_id = "comrade"
            audience = "https://secure.example/api"
            "#,
        );
        let c = Config::load(Some(&p)).unwrap().config;
        let _ = std::fs::remove_file(&p);
        let s = &c.mcp.servers[0];
        match &s.auth {
            Some(McpAuth::Oidc {
                client_id,
                scopes,
                issuer: None,
                redirect_port: None,
                audience: Some(aud),
            }) => {
                assert_eq!(client_id, "comrade");
                assert_eq!(scopes, &["openid", "profile", "email"]);
                assert_eq!(aud, "https://secure.example/api");
            }
            other => panic!("unexpected auth: {other:?}"),
        }
    }

    #[test]
    fn expand_env_value_resolves_only_dollar_names() {
        let env = |name: &str| -> Option<String> {
            match name {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }
        };
        assert_eq!(expand_env_value("$HOME", &env), "/home/me");
        assert_eq!(expand_env_value("plain", &env), "plain");
        assert_eq!(expand_env_value("$", &env), "$");
        // An unset variable keeps its literal text so the mistake is visible.
        assert_eq!(expand_env_value("$MISSING", &env), "$MISSING");
        // Nested lookups are not expanded (single pass).
        assert_eq!(expand_env_value("$HO$ME", &env), "$HO$ME");
    }
}
