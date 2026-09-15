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
    /// Named provider preset (ollama, openai, deepseek, mistral, anthropic,
    /// openrouter, groq, together). Sets `base_url` unless one is given
    /// explicitly. `claude` is an alias for `anthropic`.
    pub provider: Option<String>,
    pub temperature: f32,
    /// Seconds to wait for a response.
    pub timeout_secs: u64,
    /// How many times to retry a request that fails transiently (a connection
    /// reset/refused, a timeout, or a 408/425/429/5xx status) before giving up.
    pub max_retries: u32,
    /// Base delay in milliseconds for the retry backoff. It doubles per attempt
    /// and is capped at 8s, so a flaky provider is not hammered.
    pub retry_backoff_ms: u64,
    /// Tool-calling protocol: auto | native | react.
    pub protocol: Protocol,
    /// Model context window in tokens. When `None` the app tries to detect it
    /// from the provider and falls back to the configured context budget.
    pub context_window: Option<usize>,
    /// Optional display identity/version string detected from the provider
    /// (e.g. Ollama "7B (Q4_K_M)").
    pub model_version: Option<String>,
    /// Send an Anthropic-style `cache_control: ephemeral` marker on the system
    /// message and the last tool definition, so a provider that supports prompt
    /// caching can reuse the (large, stable) prefix across turns. Providers that
    /// do not support it ignore the unknown field.
    pub prompt_caching: bool,
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
            max_retries: 2,
            retry_backoff_ms: 500,
            protocol: Protocol::Auto,
            context_window: None,
            model_version: None,
            prompt_caching: false,
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
    /// Kill a single tool invocation after this many seconds (0 = no limit).
    /// The tool's future is dropped when the timeout fires.
    pub tool_timeout_secs: u64,
    /// Stop a whole agent run after this many seconds of wall-clock time
    /// (0 = no limit). Checked at each rest point, so the run ends gracefully.
    pub run_timeout_secs: u64,
    /// Inactivity budget for one delegated sub-agent run, in seconds: how long a
    /// delegate may complete NOTHING (no model reply, no tool result) before the
    /// harness nudges it to act, and twice that before it is stopped and replies
    /// with whatever it had gathered. Progress resets the clock, so a delegate
    /// that keeps working is never cut off for taking its time, while a slow or
    /// hung one cannot hold the parent run open forever. `0` disables the limit.
    pub delegate_timeout_secs: u64,
    /// How often the tech lead re-reads the transcript of a delegate that is
    /// still running and may steer it back on task. At this interval, and at
    /// most `MAX_DELEGATE_SUPERVISIONS` times per run, the parent model is shown
    /// what the delegate has done and what it intends to do next, and answers
    /// either "OK" (leave it alone) or a short correction that is injected into
    /// the delegate's conversation. `0` disables supervision.
    pub delegate_supervise_secs: u64,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            max_iterations: 30,
            tool_timeout_secs: 0,
            run_timeout_secs: 0,
            delegate_timeout_secs: 300,
            delegate_supervise_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CtxCfg {
    /// Rolling budget for the visible chat history in tokens.
    pub budget_tokens: usize,
    /// Hard cap on how many characters of a tool result are injected back.
    pub max_tool_output_chars: usize,
    /// When true, the agent loop summarises the history automatically once it
    /// grows into the budget headroom (instead of letting the lossy
    /// stub/evict trim run first). Disable to keep compaction manual (M-c).
    pub auto_compact: bool,
}

impl Default for CtxCfg {
    fn default() -> Self {
        Self {
            budget_tokens: 6000,
            max_tool_output_chars: 5000,
            auto_compact: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SecurityCfg {
    pub autonomy: Autonomy,
    /// Scrub secret-looking values (env-var secrets, `sk-…`/`ghp_…` tokens)
    /// out of tool output before it reaches the model or the transcript.
    pub redact_secrets: bool,
    /// Extra directories (absolute, or relative to the project root) the
    /// filesystem tools may read/write, beyond the project root itself.
    pub extra_roots: Vec<String>,
    /// When non-empty, a `shell`/`run_bg` command must START WITH one of these.
    pub shell_allow: Vec<String>,
    /// `shell`/`run_bg` commands CONTAINING any of these are always refused.
    pub shell_deny: Vec<String>,
}

impl Default for SecurityCfg {
    fn default() -> Self {
        Self {
            autonomy: Autonomy::Ask,
            redact_secrets: true,
            extra_roots: Vec::new(),
            shell_allow: Vec::new(),
            shell_deny: Vec::new(),
        }
    }
}

impl SecurityCfg {
    /// Build the process-wide [`comrade_tool::SecurityPolicy`] this config
    /// describes, resolving relative extra roots against `root`.
    pub fn to_policy(&self, root: &std::path::Path) -> comrade_tool::SecurityPolicy {
        comrade_tool::SecurityPolicy {
            extra_roots: self.extra_roots.iter().map(|r| root.join(r)).collect(),
            shell_allow: self.shell_allow.clone(),
            shell_deny: self.shell_deny.clone(),
        }
    }
}

/// What the agent should do when a sensor reports a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensorMode {
    /// Notify the human and ask whether Comrade should tackle the change.
    #[default]
    Ask,
    /// Proactive: handle the change on its own (start a session) without asking.
    Auto,
}

impl SensorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SensorMode::Ask => "ask",
            SensorMode::Auto => "auto",
        }
    }
}

/// A "proactive mode" sensor: a shell command Comrade polls on an interval to
/// detect external changes (JIRA tickets, GitHub issues, a queue, …). When the
/// polled output changes, the sensor emits a notification and — depending on
/// [`SensorMode`] — Comrade either asks the human what to do or proactively
/// starts a session to handle it. One `[[sensors]]` entry per sensor.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SensorCfg {
    /// Unique name the human uses to recognise this sensor, e.g. `jira-tickets`.
    pub name: String,
    /// Shell command polled via `bash -c`. Its stdout is what is watched for
    /// changes; a non-zero exit is reported as a sensor error, not a change.
    /// Mutually exclusive with `tool` (`tool` wins when both are set).
    pub command: String,
    /// Name of a registered tool to invoke instead of a shell command. Any tool
    /// the harness knows — a built-in, a bridged MCP tool (`mcp_<server>_<tool>`,
    /// e.g. one that lists the sprint's JIRA tickets), or a skill
    /// (`skill_<name>`) — so a sensor can poll a real integration, not just a
    /// shell. Its string result is what is watched for changes.
    pub tool: Option<String>,
    /// JSON arguments passed to `tool` when it is invoked (default `{}`).
    pub args: serde_json::Value,
    /// How often to poll, in seconds. Clamped to a 10s floor at runtime.
    pub interval_secs: u64,
    /// `ask` (notify and wait for the human) or `auto` (handle the change on its
    /// own).
    pub mode: SensorMode,
    /// Optional task prompt handed to the session Comrade opens for a change.
    /// When unset a default prompt describing the detected change is used.
    pub prompt: Option<String>,
    /// Whether this sensor is polled. Defaults to `true`; set `enabled = false`
    /// to keep the entry but switch it off.
    pub enabled: bool,
}

impl Default for SensorCfg {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            tool: None,
            args: serde_json::Value::Object(Default::default()),
            interval_secs: 300,
            mode: SensorMode::Ask,
            prompt: None,
            enabled: true,
        }
    }
}

impl SensorCfg {
    /// The effective poll period, with a 10s floor so a misconfigured `0` (or a
    /// tiny value) cannot hammer the command.
    pub fn effective_interval_secs(&self) -> u64 {
        self.interval_secs.max(10)
    }

    /// The tool this sensor invokes, if it is tool-based (`tool` set, blank
    /// ignored). `None` means it is a shell-command sensor.
    pub fn tool_name(&self) -> Option<&str> {
        self.tool
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }

    /// Whether this sensor is enabled and has something to poll (a tool, or a
    /// non-blank command).
    pub fn is_pollable(&self) -> bool {
        self.enabled && (self.tool_name().is_some() || !self.command.trim().is_empty())
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
    /// Proactive-mode sensors polled for external changes (see [`SensorCfg`]).
    pub sensors: Vec<SensorCfg>,
    /// External MCP servers whose tools are bridged into the agent.
    pub mcp: McpConfig,
    /// Pre/post-tool shell hooks run around every tool invocation.
    pub hooks: HooksCfg,
}

/// Shell hooks run before and/or after a tool call. See [`HookCfg`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HooksCfg {
    /// Run before the tool executes. A non-zero exit aborts the call.
    pub pre_tool: Vec<HookCfg>,
    /// Run after the tool executes. A non-zero exit only warns.
    pub post_tool: Vec<HookCfg>,
}

/// One hook: when to fire (`on`) and the shell command to run (`run`).
#[derive(Debug, Clone, Deserialize)]
pub struct HookCfg {
    /// Match expression: `*` (every tool), an exact tool name (`fs_edit`), or a
    /// prefix with a trailing `*` (`fs_*`).
    pub on: String,
    /// Shell command run via `bash -c`. The tool name and its JSON arguments
    /// are exported as `COMRADE_TOOL` and `COMRADE_ARGS`.
    pub run: String,
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
    /// Project-level `.comrade.toml` layered on top, if one was found.
    pub repo_source: Option<PathBuf>,
}

/// Project-level config file, looked up in the project root and layered on top
/// of the user config (project values supersede it per key).
pub const PROJECT_CONFIG_FILE: &str = ".comrade.toml";

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
        Self::load_layered(path, None)
    }

    /// Load configuration with an optional project-level config layered on top.
    pub fn load_layered(base: Option<&Path>, project_root: Option<&Path>) -> Result<LoadedConfig> {
        // Resolve base path exactly as `load` does today.
        let base_path = match base {
            Some(p) => Some(p.to_path_buf()),
            None => {
                let p = default_config_path();
                if p.exists() { Some(p) } else { None }
            }
        };

        // Read base file or empty string.
        let base_raw = if let Some(ref p) = base_path {
            std::fs::read_to_string(p)
                .with_context(|| format!("cannot read config {}", p.display()))?
        } else {
            String::new()
        };

        // Resolve project-level config.
        let repo_path = project_root
            .map(|r| r.join(PROJECT_CONFIG_FILE))
            .filter(|p| p.exists());
        let repo_raw: Option<String> = repo_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok());

        // Determine source description for error messages.
        let src_desc = match (&base_path, &repo_path) {
            (Some(b), Some(r)) => format!("{} and {}", b.display(), r.display()),
            (Some(b), None) => format!("{}", b.display()),
            (None, Some(r)) => format!("{}", r.display()),
            (None, None) => "defaults".into(),
        };

        // Parse and merge.
        let base_val = parse_toml_value(&base_raw)?;
        let repo_val = match &repo_raw {
            Some(r) => parse_toml_value(r)?,
            None => toml::Value::Table(toml::map::Map::new()),
        };
        let merged = merge_toml_values(base_val, repo_val);

        // Deserialize into Config.
        let mut config: Config = merged
            .clone()
            .try_into()
            .with_context(|| format!("bad config in {}", src_desc))?;
        apply_provider(&merged, &mut config)?;

        Ok(LoadedConfig {
            config,
            source: base_path,
            repo_source: repo_path,
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
        "anthropic" | "claude" => Some("https://api.anthropic.com/v1"),
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        "groq" => Some("https://api.groq.com/openai/v1"),
        "together" => Some("https://api.together.xyz/v1"),
        _ => None,
    }
}

/// When `llm.provider` is set and the config did not explicitly set
/// `llm.base_url`, fill the provider's base URL from the preset. Delegate
/// entries get the same treatment per entry.
fn apply_provider(raw: &toml::Value, config: &mut Config) -> Result<()> {
    let llm_has_explicit_url = raw.get("llm").and_then(|l| l.get("base_url")).is_some();
    fill_provider_base_url(
        config.llm.provider.as_deref(),
        llm_has_explicit_url,
        &mut config.llm.base_url,
        "llm",
    )?;

    let explicit: Vec<bool> = raw
        .get("delegates")
        .and_then(toml::Value::as_array)
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

fn parse_toml_value(raw: &str) -> Result<toml::Value> {
    if raw.trim().is_empty() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }
    Ok(toml::from_str::<toml::Value>(raw)?)
}

/// Layer `over` on `base`: tables merge per key; `delegates`/`servers` merge by
/// `name`; any other value is taken from `over` wholesale.
fn merge_toml_values(base: toml::Value, over: toml::Value) -> toml::Value {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => toml::Value::Table(merge_tables(b, o)),
        (_, over) => over,
    }
}

fn merge_tables(
    mut base: toml::map::Map<String, toml::Value>,
    over: toml::map::Map<String, toml::Value>,
) -> toml::map::Map<String, toml::Value> {
    for (key, value) in over {
        let merged = match base.remove(&key) {
            Some(existing) if matches!(key.as_str(), "delegates" | "servers" | "sensors") => {
                merge_named_lists(existing, value)
            }
            Some(existing) => merge_toml_values(existing, value),
            None => value,
        };
        base.insert(key, merged);
    }
    base
}

fn merge_named_lists(base: toml::Value, over: toml::Value) -> toml::Value {
    let (toml::Value::Array(mut base), toml::Value::Array(over_inner)) = (base, over.clone())
    else {
        return over;
    };
    for entry in over_inner {
        let name = entry.get("name").and_then(toml::Value::as_str);
        match name.and_then(|n| {
            base.iter()
                .position(|e| e.get("name").and_then(toml::Value::as_str) == Some(n))
        }) {
            Some(i) => base[i] = entry,
            None => base.push(entry),
        }
    }
    toml::Value::Array(base)
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
            "unknown provider {provider:?} in {where_}. Known providers: ollama, openai, deepseek, mistral, anthropic (alias claude), openrouter, groq, together"
        ),
    }
}

#[cfg(test)]
mod tests;
