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
fn auto_compact_defaults_on_and_can_be_disabled() {
    let loaded = Config::load(Some(&write_tmp(""))).unwrap();
    assert!(
        loaded.config.context.auto_compact,
        "auto-compaction is on by default"
    );

    let loaded = Config::load(Some(&write_tmp("[context]\nauto_compact = false\n"))).unwrap();
    assert!(!loaded.config.context.auto_compact);
}

#[test]
fn new_safety_and_agent_knobs_have_sane_defaults_and_parse() {
    let d = Config::load(Some(&write_tmp(""))).unwrap().config;
    assert_eq!(d.agent.tool_timeout_secs, 0);
    assert_eq!(d.agent.run_timeout_secs, 0);
    assert!(d.security.redact_secrets);
    assert!(d.security.extra_roots.is_empty());
    assert!(!d.llm.prompt_caching);
    assert!(d.hooks.pre_tool.is_empty() && d.hooks.post_tool.is_empty());

    let raw = r#"
[llm]
prompt_caching = true

[agent]
tool_timeout_secs = 120
run_timeout_secs = 900

[security]
redact_secrets = false
extra_roots = ["../shared"]
shell_allow = ["cargo "]
shell_deny = ["rm -rf /"]

[[hooks.pre_tool]]
on = "fs_edit"
run = "echo pre"

[[hooks.post_tool]]
on = "fs_*"
run = "echo post"
"#;
    let c = Config::load(Some(&write_tmp(raw))).unwrap().config;
    assert!(c.llm.prompt_caching);
    assert_eq!(c.agent.tool_timeout_secs, 120);
    assert_eq!(c.agent.run_timeout_secs, 900);
    assert!(!c.security.redact_secrets);
    assert_eq!(c.security.extra_roots, vec!["../shared".to_string()]);
    assert_eq!(c.hooks.pre_tool.len(), 1);
    assert_eq!(c.hooks.pre_tool[0].on, "fs_edit");
    assert_eq!(c.hooks.post_tool[0].on, "fs_*");

    let policy = c.security.to_policy(std::path::Path::new("/repo"));
    assert_eq!(
        policy.extra_roots,
        vec![std::path::PathBuf::from("/repo/../shared")]
    );
    assert_eq!(policy.shell_allow, vec!["cargo ".to_string()]);
    assert_eq!(policy.shell_deny, vec!["rm -rf /".to_string()]);
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
fn preset_lookup_covers_anthropic() {
    assert_eq!(
        provider_base_url("anthropic"),
        Some("https://api.anthropic.com/v1")
    );
    assert_eq!(
        provider_base_url("Claude"),
        Some("https://api.anthropic.com/v1")
    );
}

#[test]
fn anthropic_provider_fills_base_url() {
    let p = write_tmp(
        "[llm]\nprovider = \"anthropic\"\napi_key = \"sk-ant-test\"\nmodel = \"claude-sonnet-4-20250514\"\n",
    );
    let c = Config::load(Some(&p)).unwrap().config;
    let _ = std::fs::remove_file(&p);
    assert_eq!(c.llm.base_url, "https://api.anthropic.com/v1");
    assert_eq!(c.llm.api_key.as_deref(), Some("sk-ant-test"));
    assert_eq!(c.llm.model, "claude-sonnet-4-20250514");
    assert_eq!(c.llm.display(), "anthropic/claude-sonnet-4-20250514");
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

/// Create a unique temp directory under std::env::temp_dir().
fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "comrade-config-test-{}-{:?}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        tag
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn repo_config_supersedes_user_fields() {
    let user = write_tmp("[llm]\nprovider=\"ollama\"\nmodel=\"a\"\ntemperature=0.5\n");
    let proj = tmp_dir("supersedes");
    let repo_cfg = proj.join(PROJECT_CONFIG_FILE);
    std::fs::write(&repo_cfg, "[llm]\nmodel=\"b\"\n").unwrap();

    let loaded = Config::load_layered(Some(&user), Some(&proj)).unwrap();

    assert_eq!(loaded.config.llm.model, "b");
    assert_eq!(loaded.config.llm.temperature, 0.5);
    assert_eq!(loaded.repo_source, Some(repo_cfg));

    let _ = std::fs::remove_file(&user);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn repo_config_merges_delegates_by_name() {
    let user = write_tmp(
        r#"
[llm]
provider = "groq"
model = "x"

[[delegates]]
name = "a"
provider = "groq"
model = "x"

[[delegates]]
name = "b"
provider = "groq"
model = "y"
"#,
    );
    let proj = tmp_dir("merge_delegates");
    let repo_cfg = proj.join(PROJECT_CONFIG_FILE);
    std::fs::write(
        &repo_cfg,
        r#"
[[delegates]]
name = "a"
provider = "groq"
model = "z"

[[delegates]]
name = "c"
provider = "groq"
model = "w"
"#,
    )
    .unwrap();

    let loaded = Config::load_layered(Some(&user), Some(&proj)).unwrap();

    let names: Vec<&str> = loaded
        .config
        .delegates
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b", "c"]);

    assert_eq!(loaded.config.delegates[0].llm.model, "z");
    assert_eq!(loaded.config.delegates[1].llm.model, "y");
    assert_eq!(loaded.config.delegates[2].llm.model, "w");

    let _ = std::fs::remove_file(&user);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn no_repo_config_leaves_base_unchanged() {
    let user = write_tmp("[llm]\nprovider=\"ollama\"\nmodel=\"a\"\ntemperature=0.5\n");

    let loaded = Config::load_layered(Some(&user), None).unwrap();

    assert_eq!(loaded.config.llm.model, "a");
    assert_eq!(loaded.config.llm.temperature, 0.5);
    assert!(loaded.repo_source.is_none());

    let _ = std::fs::remove_file(&user);
}

#[test]
fn delegate_timeout_defaults_to_five_minutes_and_parses() {
    let d = Config::load(Some(&write_tmp(""))).unwrap().config;
    assert_eq!(d.agent.delegate_timeout_secs, 300);
    let p = write_tmp("[agent]\ndelegate_timeout_secs = 0\n");
    let c = Config::load(Some(&p)).unwrap().config;
    let _ = std::fs::remove_file(&p);
    assert_eq!(c.agent.delegate_timeout_secs, 0);
}

#[test]
fn delegate_supervise_defaults_to_a_minute_and_parses() {
    let d = Config::load(Some(&write_tmp(""))).unwrap().config;
    assert_eq!(d.agent.delegate_supervise_secs, 60);
    let p = write_tmp("[agent]\ndelegate_supervise_secs = 0\n");
    let c = Config::load(Some(&p)).unwrap().config;
    let _ = std::fs::remove_file(&p);
    assert_eq!(c.agent.delegate_supervise_secs, 0);
}

#[test]
fn sensors_default_to_none() {
    let c = Config::default();
    assert!(c.sensors.is_empty());
    let p = write_tmp("[llm]\nprovider=\"ollama\"\nmodel=\"x\"\n");
    let c = Config::load(Some(&p)).unwrap().config;
    let _ = std::fs::remove_file(&p);
    assert!(c.sensors.is_empty());
}

#[test]
fn sensors_parse_all_fields_and_defaults() {
    let p = write_tmp(
        r#"
        [[sensors]]
        name = "gh-issues"
        command = "gh issue list --state open"
        interval_secs = 120
        mode = "auto"
        prompt = "Triage the new issues."
        enabled = true

        [[sensors]]
        name = "minimal"
        command = "echo hi"
        "#,
    );
    let c = Config::load(Some(&p)).unwrap().config;
    let _ = std::fs::remove_file(&p);
    assert_eq!(c.sensors.len(), 2);

    let gh = &c.sensors[0];
    assert_eq!(gh.name, "gh-issues");
    assert_eq!(gh.command, "gh issue list --state open");
    assert_eq!(gh.interval_secs, 120);
    assert_eq!(gh.mode, SensorMode::Auto);
    assert_eq!(gh.prompt.as_deref(), Some("Triage the new issues."));
    assert!(gh.enabled);

    // Omitted fields fall back to the documented defaults.
    let minimal = &c.sensors[1];
    assert_eq!(minimal.interval_secs, 300);
    assert_eq!(minimal.mode, SensorMode::Ask);
    assert_eq!(minimal.prompt, None);
    assert!(minimal.enabled);
}

#[test]
fn sensor_disabled_flag_parses() {
    let p = write_tmp(
        "[[sensors]]\nname = \"off\"\ncommand = \"true\"\nenabled = false\nmode = \"ask\"\n",
    );
    let c = Config::load(Some(&p)).unwrap().config;
    let _ = std::fs::remove_file(&p);
    assert_eq!(c.sensors.len(), 1);
    assert!(!c.sensors[0].enabled);
    assert_eq!(c.sensors[0].mode, SensorMode::Ask);
}

#[test]
fn sensor_effective_interval_floors_at_ten() {
    let mk = |interval_secs| SensorCfg {
        interval_secs,
        ..Default::default()
    };
    assert_eq!(mk(0).effective_interval_secs(), 10);
    assert_eq!(mk(5).effective_interval_secs(), 10);
    assert_eq!(mk(10).effective_interval_secs(), 10);
    assert_eq!(mk(120).effective_interval_secs(), 120);
}

#[test]
fn sensor_mode_as_str() {
    assert_eq!(SensorMode::Ask.as_str(), "ask");
    assert_eq!(SensorMode::Auto.as_str(), "auto");
}

#[test]
fn a_tool_sensor_parses_a_registered_tool_and_args() {
    let p = write_tmp(
        r#"
        [[sensors]]
        name = "jira-sprint"
        tool = "mcp_jira_list_tickets"
        args = { sprint = "S-42", state = "open" }
        interval_secs = 120
        mode = "auto"
        "#,
    );
    let c = Config::load(Some(&p)).unwrap().config;
    let _ = std::fs::remove_file(&p);
    assert_eq!(c.sensors.len(), 1);
    let s = &c.sensors[0];
    assert_eq!(s.tool_name(), Some("mcp_jira_list_tickets"));
    assert!(s.is_pollable());
    assert_eq!(s.args["sprint"], "S-42");
    assert_eq!(s.args["state"], "open");
    // A blank `command` is fine for a tool sensor; `tool` wins when both are set.
    assert!(s.command.is_empty());
}

#[test]
fn a_command_sensor_has_no_tool_and_is_pollable() {
    let p = write_tmp("[[sensors]]\nname = \"c\"\ncommand = \"echo hi\"\n");
    let c = Config::load(Some(&p)).unwrap().config;
    let _ = std::fs::remove_file(&p);
    let s = &c.sensors[0];
    assert_eq!(s.tool_name(), None);
    assert!(s.is_pollable());
    // An enabled sensor with neither a tool nor a command must not be polled.
    let empty = SensorCfg {
        enabled: true,
        ..Default::default()
    };
    assert!(!empty.is_pollable());
}

#[test]
fn repo_config_merges_sensors_by_name() {
    let user = write_tmp(
        r#"
[llm]
provider = "ollama"
model = "x"

[[sensors]]
name = "a"
command = "echo a"
interval_secs = 60
mode = "ask"
"#,
    );
    let proj = tmp_dir("merge_sensors");
    let repo_cfg = proj.join(PROJECT_CONFIG_FILE);
    std::fs::write(
        &repo_cfg,
        r#"
[[sensors]]
name = "a"
command = "echo a-v2"
interval_secs = 90
mode = "auto"

[[sensors]]
name = "c"
command = "echo c"
"#,
    )
    .unwrap();

    let loaded = Config::load_layered(Some(&user), Some(&proj)).unwrap();

    let names: Vec<&str> = loaded
        .config
        .sensors
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "c"]);
    // The project entry supersedes the user one in place...
    assert_eq!(loaded.config.sensors[0].command, "echo a-v2");
    assert_eq!(loaded.config.sensors[0].interval_secs, 90);
    assert_eq!(loaded.config.sensors[0].mode, SensorMode::Auto);
    // ...and the new name is appended.
    assert_eq!(loaded.config.sensors[1].command, "echo c");

    let _ = std::fs::remove_file(&user);
    let _ = std::fs::remove_dir_all(&proj);
}
