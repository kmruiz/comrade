use comrade_tool::PlanStepDraft;
use comrade_tool::ToolContext;
use comrade_tool::tool::{UserIo, UserPrompt, UserReply};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::*;
use crate::MemoryUndo;
use crate::config::{Autonomy, Config, DelegateCfg, LlmCfg, Protocol};
use crate::session::AgentSession;

/// The delegate tool never touches the session/user, so a no-op IO double
/// is all the tests need.
struct NoopIo;
#[async_trait]
impl UserIo for NoopIo {
    async fn ask(&self, _prompt: UserPrompt) -> Result<UserReply> {
        Ok(UserReply::Answer(String::new()))
    }
}

fn test_ctx() -> ToolContext {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    ToolContext {
        project_root: "/tmp/x".into(),
        cwd: "/tmp/x".into(),
        session: Arc::new(AgentSession::new(tx)).as_control(),
        user: Arc::new(NoopIo),
        undo: Arc::new(MemoryUndo::new("/tmp/x".into())),
        auto_approve: true,
        events: Arc::new(comrade_tool::NoopEvents),
        steer: None,
        compact: None,
        stop: None,
    }
}

/// A context whose confirmations actually reach the UserIo (auto_approve
/// false), for exercising the per-delegate approval gate.
fn ask_ctx(user: Arc<dyn UserIo>) -> ToolContext {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    ToolContext {
        project_root: "/tmp/x".into(),
        cwd: "/tmp/x".into(),
        session: Arc::new(AgentSession::new(tx)).as_control(),
        user,
        undo: Arc::new(MemoryUndo::new("/tmp/x".into())),
        auto_approve: false,
        events: Arc::new(comrade_tool::NoopEvents),
        steer: None,
        compact: None,
        stop: None,
    }
}

/// Records every confirm prompt's title and answers each one with `answer`
/// ("yes" approves; anything else denies).
struct RecordingIo {
    titles: Arc<Mutex<Vec<String>>>,
    answer: String,
}
#[async_trait]
impl UserIo for RecordingIo {
    async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
        if let UserPrompt::Confirm { title, .. } = prompt {
            self.titles.lock().unwrap().push(title);
        }
        Ok(UserReply::Answer(self.answer.clone()))
    }
}

/// Fails the test if the context ever asks the human: used to prove that
/// auto/ungated delegates run without an approval prompt.
struct MustNotAsk;
#[async_trait]
impl UserIo for MustNotAsk {
    async fn ask(&self, _prompt: UserPrompt) -> Result<UserReply> {
        panic!("auto/ungated delegate must not prompt for approval");
    }
}

/// Build a delegate tool with an empty registry (no repository tools) so
/// tests exercise the sub-agent loop without touching the real filesystem.
fn mk_delegate(delegates: &[DelegateCfg]) -> Result<Option<DelegateTool>> {
    DelegateTool::new(delegates, ToolRegistry::new(), DelegateLimits::default())
}

/// Spawn a fake OpenAI-compatible `/v1/chat/completions` server that
/// answers every request with `content`. Returns its base URL.
fn fake_chat_server(content: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let mut used = 0usize;
        loop {
            let n = stream.read(&mut buf[used..]).unwrap();
            if n == 0 {
                break;
            }
            used += n;
            if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let body = format!(
            "{{\"choices\":[{{\"message\":{{\"content\":\"{}\"}}}}]}}",
            content.replace('"', "\\\"")
        );
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).unwrap();
    });
    format!("http://127.0.0.1:{port}/v1")
}

/// Like [`fake_chat_server`], but also ships the raw request body to a
/// channel so tests can assert what the delegate was actually asked.
fn request_spy() -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut data = Vec::new();
        let mut tmp = [0u8; 2048];
        let mut body_len: Option<usize> = None;
        let mut header_end: Option<usize> = None;
        // Read until the whole body (per Content-Length) is buffered.
        while body_len.is_none_or(|len| header_end.unwrap_or(0) + 4 + len > data.len()) {
            let n = stream.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            data.extend_from_slice(&tmp[..n]);
            if header_end.is_none()
                && let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n")
            {
                header_end = Some(p);
                let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
                body_len = head.lines().find_map(|l| {
                    l.trim()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse().ok())
                });
            }
        }
        let body = match (header_end, body_len) {
            (Some(he), Some(len)) => {
                let start = he + 4;
                let end = (start + len).min(data.len());
                String::from_utf8_lossy(&data[start..end]).into_owned()
            }
            _ => String::new(),
        };
        let _ = tx.send(body);
        let resp_body = "{\"choices\":[{\"message\":{\"content\":\"ok\"}}]}";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            resp_body.len(),
            resp_body
        );
        stream.write_all(resp.as_bytes()).unwrap();
    });
    (format!("http://127.0.0.1:{port}/v1"), rx)
}

fn delegate(name: &str, base_url: &str) -> DelegateCfg {
    DelegateCfg {
        name: name.into(),
        description: format!("{name} test delegate"),
        enabled: true,
        approval: Autonomy::Auto,
        llm: LlmCfg {
            base_url: base_url.into(),
            model: "delegate-model".into(),
            ..LlmCfg::default()
        },
    }
}

/// A delegate stuck waiting for its model must abort when the run's cancel
/// token fires, instead of holding the whole run at "working" (the user
/// cannot interrupt it — Esc is dead until the request returns).
#[tokio::test]
async fn cancelled_run_aborts_a_delegate_stuck_waiting_for_the_model() {
    // Server accepts the request, reads it, then never answers: the
    // delegate's model request stays in flight until cancelled.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let mut used = 0usize;
        loop {
            let n = stream.read(&mut buf[used..]).unwrap();
            if n == 0 {
                break;
            }
            used += n;
            if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // Hold the connection open without replying.
        std::thread::sleep(std::time::Duration::from_secs(60));
    });
    let base = format!("http://127.0.0.1:{port}/v1");
    let mut d = delegate("silent", &base);
    // Short client timeout so a regression fails in seconds, not in 600.
    d.llm.timeout_secs = 5;
    let client = LlmClient::new(&d.llm).unwrap();

    let mut ctx = test_ctx();
    let stop = tokio_util::sync::CancellationToken::new();
    ctx.stop = Some(stop.clone());
    // Cancel shortly after the request is in flight (300ms in).
    let stopper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            stop.cancel();
        })
    };

    let started = std::time::Instant::now();
    let res = run_delegate_subagent(
        &client,
        &ToolRegistry::new(),
        &ctx,
        "You are a test delegate.".into(),
        "do the thing".into(),
        "silent",
        false,
        &DelegateLimits::default(),
        DELEGATE_READ_NUDGE,
    )
    .await;
    stopper.join().unwrap();

    let err = res
        .expect_err("a cancelled delegate run must fail, not hang")
        .to_string();
    assert!(err.contains("interrupted"), "unexpected error: {err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "delegate took {:?} to abort — cancel is not reaching it",
        started.elapsed()
    );
}

#[test]
fn empty_delegate_list_yields_no_tool() {
    let tool = mk_delegate(&[]).unwrap();
    assert!(tool.is_none());
}

#[test]
fn disabled_delegate_is_hidden_and_unselectable() {
    let on = delegate("on", "http://x/v1");
    let mut off = delegate("off", "http://x/v1");
    off.enabled = false;
    let tool = mk_delegate(&[on, off]).unwrap().unwrap();
    let schema = &tool.spec().json_schema;
    let models = schema["properties"]["model"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(models, vec!["on"]);
    assert!(
        !tool.spec().description.contains("off"),
        "disabled delegate must not be advertised:\n{}",
        tool.spec().description
    );
}

#[test]
fn all_delegates_disabled_yields_no_tool() {
    let mut off = delegate("off", "http://x/v1");
    off.enabled = false;
    assert!(mk_delegate(&[off]).unwrap().is_none());
}

#[test]
fn disabled_delegate_is_not_validated() {
    // A disabled entry may be blank/duplicate without failing the build,
    // as long as at least one enabled delegate remains.
    let dup_disabled = DelegateCfg {
        enabled: false,
        ..delegate("dup", "http://x/v1")
    };
    let tool = mk_delegate(&[delegate("dup", "http://x/v1"), dup_disabled]).unwrap();
    assert!(tool.is_some());
}

#[test]
fn duplicate_or_blank_names_are_rejected() {
    let dup = vec![delegate("a", "http://x/v1"), delegate("a", "http://x/v1")];
    assert!(mk_delegate(&dup).is_err());

    let blank = vec![DelegateCfg {
        name: "  ".into(),
        ..delegate("ignored", "http://x/v1")
    }];
    assert!(mk_delegate(&blank).is_err());

    let no_model = vec![DelegateCfg {
        llm: LlmCfg {
            model: "".into(),
            ..LlmCfg::default()
        },
        ..delegate("m", "http://x/v1")
    }];
    assert!(mk_delegate(&no_model).is_err());
}

#[test]
fn schema_advertises_models_and_required_args() {
    let cfg = Config {
        delegates: vec![
            delegate("groq-fast", "http://x/v1"),
            delegate("mistral", "http://x/v1"),
        ],
        ..Config::default()
    };
    let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
    assert_eq!(tool.spec().name, "delegate");
    let schema = &tool.spec().json_schema;
    let models = schema["properties"]["model"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(models, vec!["groq-fast", "mistral"]);
    // The tool description advertises the delegates with their
    // descriptions, so the tech lead picks a delegate by what the blurb
    // says fits the task. The `model` arg schema stays small (see below).
    let spec_desc = &tool.spec().description;
    assert!(spec_desc.contains("Configured delegates"), "{spec_desc}");
    assert!(
        spec_desc.contains("groq-fast: groq-fast test delegate"),
        "{spec_desc}"
    );
    assert!(
        spec_desc.contains("mistral: mistral test delegate"),
        "{spec_desc}"
    );
    let model_desc = schema["properties"]["model"]["description"]
        .as_str()
        .unwrap();
    // The delegate listing lives once (in the tool description) to keep the
    // model arg schema small; the `model` arg itself carries only a pointer.
    assert!(
        !model_desc.contains("mistral test delegate"),
        "model arg must not duplicate the listing:\n{model_desc}"
    );
    assert!(schema["properties"]["task"].is_object());
    assert!(schema["properties"]["step"].is_object());
    // `step` alone, or `model` + `task` (oneOf), are the two call shapes.
    let one_of = schema["oneOf"].as_array().unwrap();
    let requires = |needle: &str| {
        one_of.iter().any(|o| {
            o["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|v| v.as_str() == Some(needle)))
        })
    };
    assert!(requires("step"));
    assert!(requires("model") && requires("task"));
}

#[test]
fn blank_blurbs_degrade_to_bare_name_lines() {
    let cfg = Config {
        delegates: vec![
            delegate("with-blurb", "http://x/v1"),
            DelegateCfg {
                description: "   ".into(),
                ..delegate("bare", "http://x/v1")
            },
        ],
        ..Config::default()
    };
    let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
    let desc = tool.spec().description.clone();
    assert!(
        desc.contains("with-blurb: with-blurb test delegate"),
        "{desc}"
    );
    assert!(desc.trim_end().ends_with("  - bare"), "{desc}");
}

#[test]
fn delegate_system_prompt_substitutes_tokens_from_markdown() {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(StubTool {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }));
    for native in [false, true] {
        let prompt = delegate_system_prompt("/repo/root", &reg, native);
        assert!(prompt.starts_with("You are a developer sub-agent on Comrade's team."));
        assert!(
            prompt.contains("Working directory: /repo/root."),
            "{prompt}"
        );
        assert!(prompt.contains("Available tools:"), "{prompt}");
        if native {
            // native mode: the tool schemas carry the descriptions, so the
            // listing is names only to keep the prompt small
            assert!(prompt.contains("- fs_write_file\n"), "{prompt}");
            assert!(!prompt.contains("stub fs_write_file"), "{prompt}");
        } else {
            assert!(
                prompt.contains("- fs_write_file — stub fs_write_file"),
                "{prompt}"
            );
        }
        assert!(
            !prompt.contains("{project_root}")
                && !prompt.contains("{tool_lines}")
                && !prompt.contains("{protocol}"),
            "unsubstituted token in:\n{prompt}"
        );
        assert!(
            prompt.trim_end().ends_with("whether it passes."),
            "{prompt}"
        );
    }
}

#[tokio::test]
async fn invoke_queries_the_chosen_delegate() {
    let base = fake_chat_server("here is the finished function");
    let cfg = Config {
        delegates: vec![
            delegate("cheap", &base),
            delegate("other", "http://127.0.0.1:1/v1"),
        ],
        ..Config::default()
    };
    let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
    let ctx = test_ctx();
    let out = tool
        .invoke(
            &ctx,
            json!({
                "model": "cheap",
                "task": "Write a double() function.",
                "context": "Rust, must be pure."
            }),
        )
        .await
        .unwrap();
    assert!(out.contains("delegate cheap"));
    assert!(out.contains("here is the finished function"));
}

#[tokio::test]
async fn invoke_rejects_unknown_model_and_empty_task() {
    let base = fake_chat_server("ignored");
    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
    let ctx = test_ctx();
    let err = tool
        .invoke(&ctx, json!({"model": "nope", "task": "x"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown delegate model"));
    // The error repeats the pick list (name + description) so a wrong
    // guess teaches the tech lead which delegate fits next time.
    assert!(
        err.to_string().contains("cheap: cheap test delegate"),
        "{err}"
    );

    let err = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "   "}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("task"));
}

#[tokio::test]
async fn plan_step_delegation_sends_goal_verification_and_context() {
    let (base, spy) = request_spy();
    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
    let ctx = test_ctx();
    ctx.session.set_plan(vec![PlanStepDraft {
        goal: "Write a double() function.".into(),
        verification: "cargo test double passes".into(),
        model: "cheap".into(),
        context: "Pure Rust, no dependencies.".into(),
    }]);

    let out = tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
    assert!(out.contains("delegate cheap"), "{out}");

    let body = spy.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(body.contains("Write a double() function."), "{body}");
    assert!(
        body.contains("Verify your work: cargo test double passes"),
        "{body}"
    );
    assert!(body.contains("Pure Rust, no dependencies."), "{body}");
}

#[tokio::test]
async fn self_assigned_plan_steps_cannot_be_delegated() {
    let (base, _spy) = request_spy();
    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
    let ctx = test_ctx();
    ctx.session.set_plan(vec![PlanStepDraft {
        goal: "do it myself".into(),
        verification: "".into(),
        model: comrade_tool::AGENT_MODEL.into(),
        context: "".into(),
    }]);
    let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
    assert!(err.to_string().contains("main agent model"), "{err}");
}

#[tokio::test]
async fn plan_step_delegation_validates_args() {
    let (base, _spy) = request_spy();
    let cfg = Config {
        delegates: vec![
            delegate("cheap", &base),
            delegate("other", "http://127.0.0.1:1/v1"),
        ],
        ..Config::default()
    };
    let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
    let ctx = test_ctx();
    ctx.session.set_plan(vec![
        PlanStepDraft {
            goal: "run on cheap".into(),
            verification: "".into(),
            model: "cheap".into(),
            context: "".into(),
        },
        PlanStepDraft {
            goal: "run on main".into(),
            verification: "".into(),
            model: "".into(),
            context: "".into(),
        },
    ]);

    // `model` must match the step's assigned model.
    let err = tool
        .invoke(&ctx, json!({"step": 1, "model": "other"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");

    // A step without an assigned delegate cannot be delegated.
    let err = tool.invoke(&ctx, json!({"step": 2})).await.unwrap_err();
    assert!(
        err.to_string().contains("no delegate model assigned"),
        "{err}"
    );

    // `step` is lenient about a duplicate `task`: the step's own task wins and
    // the extra argument is ignored (a small model passes both routinely).
    let ok = tool.invoke(&ctx, json!({"step": 1, "task": "nope"})).await;
    assert!(ok.is_ok(), "{:?}", ok.err());

    // Unknown step id.
    let err = tool.invoke(&ctx, json!({"step": 99})).await.unwrap_err();
    assert!(err.to_string().contains("no plan step with id 99"), "{err}");
}

#[tokio::test]
async fn plan_step_delegation_marks_step_in_progress_with_working_note() {
    let (base, _spy) = request_spy();
    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = mk_delegate(&cfg.delegates).unwrap().unwrap();
    let ctx = test_ctx();
    ctx.session.set_plan(vec![PlanStepDraft {
        goal: "Write double()".into(),
        verification: "cargo test double passes".into(),
        model: "cheap".into(),
        context: "".into(),
    }]);

    tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
    let step = &ctx.session.plan()[0];
    assert_eq!(step.status, PlanStatus::InProgress);
    let note = step.note.as_deref().unwrap_or_default();
    assert!(note.contains("working: cheap"), "{note}");
    assert!(!note.contains("fix"), "{note}");
}

#[tokio::test]
async fn ask_gated_delegate_pauses_for_approval_then_runs() {
    let (base, _spy) = request_spy();
    let mut expensive = delegate("expensive", &base);
    expensive.approval = Autonomy::Ask;
    let tool = mk_delegate(&[expensive]).unwrap().unwrap();
    let titles = Arc::new(Mutex::new(Vec::new()));
    let ctx = ask_ctx(Arc::new(RecordingIo {
        titles: titles.clone(),
        answer: "yes".into(),
    }));

    let out = tool
        .invoke(&ctx, json!({"model": "expensive", "task": "add a test"}))
        .await
        .unwrap();
    let held = titles.lock().unwrap();
    assert_eq!(held.len(), 1, "exactly one approval prompt expected");
    assert!(
        held[0].contains("delegate expensive"),
        "confirm title should name the delegate: {:?}",
        held[0]
    );
    drop(held);
    assert!(out.contains("delegate expensive"), "{out}");
}

#[tokio::test]
async fn denied_approval_aborts_delegate_and_leaves_plan_untouched() {
    let (base, _spy) = request_spy();
    let mut expensive = delegate("expensive", &base);
    expensive.approval = Autonomy::Ask;
    let tool = mk_delegate(&[expensive]).unwrap().unwrap();
    let titles = Arc::new(Mutex::new(Vec::new()));
    let ctx = ask_ctx(Arc::new(RecordingIo {
        titles: titles.clone(),
        answer: "".into(), // not affirmative -> denied
    }));
    ctx.session.set_plan(vec![PlanStepDraft {
        goal: "Write double()".into(),
        verification: "".into(),
        model: "expensive".into(),
        context: "".into(),
    }]);

    let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
    assert!(err.to_string().contains("user denied"), "{err}");
    // The step was never claimed: it stays pending with no working note.
    let step = &ctx.session.plan()[0];
    assert_eq!(step.status, PlanStatus::Pending, "{step:?}");
    assert!(step.note.is_none(), "{step:?}");
    assert_eq!(titles.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn approved_plan_step_delegation_still_marks_working_after_the_gate() {
    let (base, _spy) = request_spy();
    let mut expensive = delegate("expensive", &base);
    expensive.approval = Autonomy::Ask;
    let tool = mk_delegate(&[expensive]).unwrap().unwrap();
    let titles = Arc::new(Mutex::new(Vec::new()));
    let ctx = ask_ctx(Arc::new(RecordingIo {
        titles: titles.clone(),
        answer: "yes".into(),
    }));
    ctx.session.set_plan(vec![PlanStepDraft {
        goal: "Write double()".into(),
        verification: "".into(),
        model: "expensive".into(),
        context: "".into(),
    }]);

    tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
    let held = titles.lock().unwrap();
    assert!(
        held[0].contains("plan step 1") && held[0].contains("expensive"),
        "{:?}",
        held[0]
    );
    drop(held);
    let step = &ctx.session.plan()[0];
    assert_eq!(step.status, PlanStatus::InProgress);
    assert_eq!(step.note.as_deref(), Some("working: expensive"));
}

#[tokio::test]
async fn deny_gated_delegate_is_refused_without_running() {
    // No server is spawned: a deny-gated delegate must never be contacted.
    let mut guarded = delegate("guarded", "http://127.0.0.1:1/v1");
    guarded.approval = Autonomy::Deny;
    let tool = mk_delegate(&[guarded]).unwrap().unwrap();
    let ctx = test_ctx();

    let err = tool
        .invoke(&ctx, json!({"model": "guarded", "task": "any task"}))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("approval = \"deny\"") && err.to_string().contains("guarded"),
        "{err}"
    );
}

#[tokio::test]
async fn auto_gated_default_delegate_runs_without_any_prompt() {
    let (base, _spy) = request_spy();
    let tool = mk_delegate(&[delegate("cheap", &base)]).unwrap().unwrap();
    let ctx = ask_ctx(Arc::new(MustNotAsk));
    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "add a test"}))
        .await
        .unwrap();
    assert!(out.contains("delegate cheap"), "{out}");
}

fn cheap_tool(base_url: &str) -> DelegateTool {
    let cfg = Config {
        delegates: vec![delegate("cheap", base_url)],
        ..Config::default()
    };
    mk_delegate(&cfg.delegates).unwrap().unwrap()
}

#[tokio::test]
async fn fix_feedback_is_forwarded_and_rounds_are_counted_on_the_step() {
    let ctx = test_ctx();
    ctx.session.set_plan(vec![PlanStepDraft {
        goal: "Write double()".into(),
        verification: "cargo test double passes".into(),
        model: "cheap".into(),
        context: "".into(),
    }]);
    let note = || ctx.session.plan()[0].note.clone();

    // first attempt: no feedback, note carries no round yet
    let (base1, spy1) = request_spy();
    let tool = cheap_tool(&base1);
    tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
    let body1 = spy1
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        !body1.contains("did not pass the parent's verification"),
        "{body1}"
    );
    assert_eq!(note().as_deref(), Some("working: cheap"));

    // fix round 1: feedback reaches the delegate and the note counts it
    let (base2, spy2) = request_spy();
    let tool = cheap_tool(&base2);
    tool.invoke(
        &ctx,
        json!({"step": 1, "feedback": "cargo test fails: double(0) returned 1"}),
    )
    .await
    .unwrap();
    let body2 = spy2
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        body2.contains("did not pass the parent's verification"),
        "{body2}"
    );
    assert!(body2.contains("double(0) returned 1"), "{body2}");
    assert_eq!(note().as_deref(), Some("working: cheap (fix 1/5)"));

    // fix round 2 keeps counting
    let (base3, _spy3) = request_spy();
    let tool = cheap_tool(&base3);
    tool.invoke(&ctx, json!({"step": 1, "feedback": "still failing"}))
        .await
        .unwrap();
    assert_eq!(note().as_deref(), Some("working: cheap (fix 2/5)"));
}

#[tokio::test]
async fn delegate_is_refused_after_five_fix_rounds() {
    let ctx = test_ctx();
    ctx.session.set_plan(vec![PlanStepDraft {
        goal: "Write double()".into(),
        verification: "".into(),
        model: "cheap".into(),
        context: "".into(),
    }]);
    // simulate five failed fix rounds already recorded on the step
    ctx.session.update_plan(
        PlanTarget::Id(1),
        PlanStatus::InProgress,
        Some("working: cheap (fix 5/5)".into()),
    );

    let (base, _spy) = request_spy();
    let tool = cheap_tool(&base);
    let err = tool
        .invoke(&ctx, json!({"step": 1, "feedback": "still red"}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("5 failed fix round(s)"), "{msg}");
    assert!(msg.contains("do the step yourself"), "{msg}");

    // a bare re-run is refused too once the limit is reached
    let (base2, _spy2) = request_spy();
    let tool = cheap_tool(&base2);
    let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
    assert!(err.to_string().contains("do the step yourself"), "{err}");
}

#[tokio::test]
async fn feedback_without_a_step_is_rejected() {
    let (base, _spy) = request_spy();
    let tool = cheap_tool(&base);
    let ctx = test_ctx();
    let err = tool
        .invoke(
            &ctx,
            json!({"model": "cheap", "task": "write double()", "feedback": "nope"}),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("`feedback` is only valid with `step`"),
        "{err}"
    );
}

#[test]
fn deny_list_keeps_commit_and_session_tools_away_but_not_work_tools() {
    assert!(DelegateTool::denied_for_delegates("git_commit"));
    assert!(DelegateTool::denied_for_delegates("delegate"));
    assert!(DelegateTool::denied_for_delegates("ask_form"));
    assert!(DelegateTool::denied_for_delegates("self_set_plan"));
    assert!(DelegateTool::denied_for_delegates("self_update_plan"));
    assert!(DelegateTool::denied_for_delegates("self_set_step_model"));
    assert!(DelegateTool::denied_for_delegates("self_finish_plan"));
    assert!(DelegateTool::denied_for_delegates("self_rename_session"));
    assert!(DelegateTool::denied_for_delegates("self_set_status_bar"));
    // The delegate must keep every tool that does the actual work...
    for name in [
        "fs_write_file",
        "fs_edit",
        "shell",
        "pom_run_task",
        "pom_run_tests",
        "record_adr",
        "amend_adr",
        "find_adr",
        "record_glossary",
        "find_glossary",
        "read_glossary",
        "fs_read_file",
        "fs_rgrep",
        "ts_list_symbols",
        "git_status",
        "web_search",
    ] {
        assert!(!DelegateTool::denied_for_delegates(name), "{name}");
    }
}

/// A recording stub tool standing in for a real repository tool (e.g.
/// fs_write_file). Lets tests prove the delegate sub-agent actually dispatched
/// a tool call without touching the real filesystem.
struct StubTool {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Tool for StubTool {
    fn spec(&self) -> &ToolSpec {
        &STUB_WRITE_SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("stub fs_write_file executed".to_string())
    }
}

static STUB_WRITE_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
    name: "fs_write_file".into(),
    description: "stub fs_write_file".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "content": { "type": "string" }
        },
        "required": ["path", "content"]
    }),
});

/// A recording stub that stands in for the read-only `fs_read_file` tool (a
/// read is never a state change, so it feeds the delegate read guard).
struct ReadStubTool {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Tool for ReadStubTool {
    fn spec(&self) -> &ToolSpec {
        &STUB_READ_SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("stub fs_read_file executed".to_string())
    }
}

static STUB_READ_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
    name: "fs_read_file".into(),
    description: "stub fs_read_file".into(),
    json_schema: json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"]
    }),
});

#[test]
fn context_overflow_errors_are_recognised() {
    // The provider wording that killed a real delegate run, quoted verbatim.
    let provider_error = format!(
        "llm error 400 Bad Request: {}",
        r#"{"error":"Context size has been exceeded."}"#
    );
    assert!(is_context_overflow(&anyhow::anyhow!(provider_error)));
    assert!(is_context_overflow(&anyhow::anyhow!(
        "context_length_exceeded"
    )));
    assert!(is_context_overflow(&anyhow::anyhow!(
        "This model's maximum context length is 8192 tokens"
    )));
    // The ReAct parser's "line too long" is a parse error, not an overflow.
    assert!(!is_context_overflow(&anyhow::anyhow!("line too long: 42")));
    assert!(!is_context_overflow(&anyhow::anyhow!("connection refused")));
}

#[test]
fn history_is_material_needs_an_assistant_turn() {
    let mut ctxm = ContextManager::with_system("sys".to_string(), 6000, 2000);
    ctxm.push(ChatMessage::new(Role::User, "task"));
    // Only the fixed prompt is present, so a summary would have nothing to say
    // and the overflow must be treated as the fixed-footprint case.
    assert!(!history_is_material(&ctxm));
    ctxm.push(ChatMessage::new(Role::Assistant, "working"));
    assert!(history_is_material(&ctxm));
}

/// A fake chat server that answers N sequential requests with N distinct raw
/// JSON bodies, each with its own HTTP status, so a test can script a provider
/// error (e.g. a 400 context-overflow) followed by successful turns.
fn scripted_http(responses: Vec<(u16, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let mut used = 0usize;
            loop {
                let n = stream.read(&mut buf[used..]).unwrap();
                if n == 0 {
                    break;
                }
                used += n;
                if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}

/// A fake chat server that answers N sequential requests with N distinct
/// raw JSON bodies. The delegate sub-agent loop makes one request per turn,
/// so this models a tool round (turn 1: model asks for a tool) followed by
/// a final answer (turn 2).
fn scripted_server(responses: Vec<String>) -> String {
    scripted_http(responses.into_iter().map(|body| (200, body)).collect())
}

// ---------------------------------------------------------------------------
// Context-overflow recovery
// ---------------------------------------------------------------------------

/// The provider wording a real delegate run died on, surfaced by the client's
/// `LlmHttpError` as `llm error 400 Bad Request: <body>`.
fn overflow_response() -> (u16, String) {
    (
        400,
        r#"{"error":"Context size has been exceeded."}"#.to_string(),
    )
}

/// A native turn asking the stub to make change number `n`.
fn edit_turn(n: u32) -> String {
    json!({"choices":[{"message":{"content":"","tool_calls":[{
        "id": format!("call_{n}"),
        "function": {"name": "fs_edit", "arguments": json!({
            "path": format!("src/f{n}.rs"), "old": "a", "new": "b"
        }).to_string()}
    }]}}]})
    .to_string()
}

/// A native turn that ends the run with `text`.
fn answer_turn(text: &str) -> String {
    json!({"choices":[{"message":{"content": text}}]}).to_string()
}

/// A parent model that answers every `summarise` call with a fixed briefing and
/// counts how often it was asked, so a test can prove a recovery ran (or did
/// not). Its `approve` never authorises anything.
struct SummarisingParent {
    summary: String,
    called: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl UpwardAsk for SummarisingParent {
    async fn ask(&self, _question: &str) -> Result<String> {
        Ok(String::new())
    }

    async fn approve(&self, _title: &str, _detail: &str) -> Result<Verdict> {
        Ok(Verdict::Unavailable)
    }

    async fn summarise(&self, _transcript: &str) -> Result<Option<String>> {
        self.called
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(self.summary.clone()))
    }
}

/// A recording stub `fs_edit`. The name matters: `fs_edit` is a MUTATING tool, so
/// the no-progress guard clears its refusal counter on every run and the stub may
/// legitimately be called more than once in a test.
struct EditStubTool {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

static STUB_EDIT_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
    name: "fs_edit".into(),
    description: "stub fs_edit".into(),
    json_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "old": { "type": "string" },
            "new": { "type": "string" }
        },
        "required": ["path", "old", "new"]
    }),
});

#[async_trait::async_trait]
impl Tool for EditStubTool {
    fn spec(&self) -> &ToolSpec {
        &STUB_EDIT_SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("stub fs_edit executed".to_string())
    }
}

/// Drive one delegate run against `script` and report the tool's reply plus how
/// often the parent was asked to summarise the delegate's context.
async fn overflow_run(script: Vec<(u16, String)>) -> (String, usize) {
    let base = scripted_http(script);
    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EditStubTool {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }));
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let session = Arc::new(AgentSession::new(tx));
    let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    session.set_upward(Arc::new(SummarisingParent {
        summary: "BRIEFING: the delegate was changing a to b in src/f1.rs; that work is unfinished"
            .to_string(),
        called: called.clone(),
    }));

    let mut ctx = test_ctx();
    ctx.session = session.as_control();
    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "change a to b"}))
        .await
        .unwrap();
    (out, called.load(std::sync::atomic::Ordering::SeqCst))
}

/// A delegate that has done work and then overflows must be summarised by the
/// parent and CONTINUE, not die with the provider's error.
#[tokio::test]
async fn delegate_recovers_from_a_context_overflow() {
    let (out, summaries) = overflow_run(vec![
        (200, edit_turn(1)),
        overflow_response(),
        (200, answer_turn("finished")),
    ])
    .await;

    assert_eq!(summaries, 1, "the parent must be asked for one summary");
    assert!(
        out.contains("finished"),
        "the run must continue past the overflow: {out}"
    );
}

/// Recovery is capped at three per run: past that the run hands back what it had
/// instead of summarising again, so a model that cannot fit its task cannot keep
/// the parent answering forever.
#[tokio::test]
async fn delegate_stops_compacting_after_three_recoveries() {
    // Each overflow is preceded by a successful turn, so the history is material
    // again and a recovery is genuinely available - the cap is what stops it.
    let (out, summaries) = overflow_run(vec![
        (200, edit_turn(1)),
        overflow_response(),
        (200, edit_turn(2)),
        overflow_response(),
        (200, edit_turn(3)),
        overflow_response(),
        (200, edit_turn(4)),
        overflow_response(),
        overflow_response(),
        overflow_response(),
    ])
    .await;

    assert_eq!(summaries, 3, "at most three recoveries per run");
    assert!(
        out.contains("compacted 3 time(s)"),
        "an exhausted run must return its partial answer, not fail: {out}"
    );
}

/// When nothing but the fixed prompt and the tool schemas fit the window, a
/// summary has nothing to say: the fixed-footprint retry must handle it WITHOUT
/// spending one of the three recovery rounds.
#[tokio::test]
async fn delegate_does_not_spend_a_compaction_on_a_fixed_footprint_overflow() {
    let (out, summaries) =
        overflow_run(vec![overflow_response(), (200, answer_turn("finished"))]).await;

    assert_eq!(
        summaries, 0,
        "a fixed-footprint overflow must not consume a recovery"
    );
    assert!(
        out.contains("finished"),
        "the slim retry must let the run continue: {out}"
    );
}

/// Build a delegate whose registry contains one recording stub `fs_write_file`
/// tool, and a scripted model that first calls it natively then answers.
#[tokio::test]
async fn delegate_executes_its_tools_in_a_subagent_loop() {
    let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(StubTool {
        calls: write_calls.clone(),
    }));

    let tool_call_turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn =
        json!({"choices": [{"message": {"content": "done, file written"}}]}).to_string();
    let base = scripted_server(vec![tool_call_turn, final_turn]);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
        .await
        .unwrap();
    assert!(out.contains("done, file written"), "{out}");
    assert_eq!(
        write_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "delegate must have run its fs_write_file tool once"
    );
}

use comrade_tool::{UpwardAsk, Verdict};

/// A parent model that answers permission requests with a fixed verdict and
/// counts how often it was asked.
struct StubParent {
    verdict: Verdict,
    asked: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl UpwardAsk for StubParent {
    async fn ask(&self, _question: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn approve(&self, _title: &str, _detail: &str) -> Result<Verdict> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.verdict.clone())
    }
}

/// Run a delegate that tries to REPLACE src/lib.rs wholesale (which would delete
/// the crate's own `greet_works`), with an optional parent model answering the
/// permission request. Returns (times the write tool ran, the file afterwards).
async fn destructive_trial(verdict: Option<Verdict>, tag: &str) -> (usize, String) {
    let dir = std::env::temp_dir().join(format!("comrade-destructive-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let before = "pub fn greet() {}\n\nfn greet_works() {}\n";
    std::fs::write(dir.join("src/lib.rs"), before).unwrap();

    let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(StubTool {
        calls: write_calls.clone(),
    }));

    let tool_call_turn = json!({"choices":[{"message":{"content":"","tool_calls":[{
        "id":"call_1","function":{"name":"fs_write_file",
        "arguments": json!({"path":"src/lib.rs","content":"pub fn shout() {}\n"}).to_string()
    }}]}}]})
    .to_string();
    let final_turn = json!({"choices":[{"message":{"content":"finished"}}]}).to_string();
    let base = scripted_server(vec![tool_call_turn, final_turn]);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let session = Arc::new(AgentSession::new(tx));
    if let Some(verdict) = verdict {
        session.set_upward(Arc::new(StubParent {
            verdict,
            asked: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
    }
    let mut ctx = test_ctx();
    ctx.project_root = dir.clone();
    ctx.cwd = dir.clone();
    ctx.session = session.as_control();

    let out = tool
        .invoke(
            &ctx,
            json!({"model": "cheap", "task": "replace greet with shout"}),
        )
        .await
        .unwrap();
    assert!(out.contains("finished"), "{out}");
    (
        write_calls.load(std::sync::atomic::Ordering::SeqCst),
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
    )
}

/// A whole-file rewrite that DELETES code must not run unless the tech lead
/// approves it: in the smoke trial the delegate dropped the crate's own test
/// this way and then reported success.
#[tokio::test]
async fn delegate_may_not_delete_code_without_the_tech_leads_permission() {
    let before = "pub fn greet() {}\n\nfn greet_works() {}\n";

    // (1) Nobody to ask: the write is refused outright (fail closed).
    let (calls, after) = destructive_trial(None, "guard-noparent").await;
    assert_eq!(calls, 0, "an unapproved destructive write must not run");
    assert_eq!(after, before, "the file must be untouched");

    // (2) The tech lead refuses: still nothing is written.
    let (calls, after) = destructive_trial(
        Some(Verdict::Denied("use fs_edit, keep greet_works".into())),
        "guard-denied",
    )
    .await;
    assert_eq!(calls, 0, "a refused destructive write must not run");
    assert_eq!(after, before, "the file must be untouched");

    // (3) The tech lead approves: the write runs, so approval is not a dead end.
    let (calls, _) = destructive_trial(Some(Verdict::Approved), "guard-approved").await;
    assert_eq!(calls, 1, "an approved destructive write must run");
}

/// The delegate's sub-agent tool calls must reach the session's event
/// channel (tagged with the delegate's configured name) so the chat can
/// show what the delegate is doing while it works.
#[tokio::test]
async fn delegate_tool_activity_streams_as_chat_events() {
    let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(StubTool {
        calls: write_calls.clone(),
    }));

    let tool_call_turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn =
        json!({"choices": [{"message": {"content": "done, file written"}}]}).to_string();
    let base = scripted_server(vec![tool_call_turn, final_turn]);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();

    // A context wired to a real event channel instead of the no-op sink.
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let mut ctx = test_ctx();
    ctx.events = Arc::new(crate::session::SessionEvents(tx));

    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
        .await
        .unwrap();
    assert!(out.contains("done, file written"), "{out}");

    // Drain the emitted activity: one call and one result, both tagged with
    // the delegate's configured name so the chat can attribute them.
    let mut seen_call = false;
    let mut seen_result = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            crate::session::AgentEvent::DelegateToolCall { model, name, .. } => {
                seen_call = true;
                assert_eq!(model, "cheap");
                assert_eq!(name, "fs_write_file");
            }
            crate::session::AgentEvent::DelegateToolResult {
                model, name, ok, ..
            } => {
                seen_result = true;
                assert_eq!(model, "cheap");
                assert_eq!(name, "fs_write_file");
                assert!(ok);
            }
            _ => {}
        }
    }
    assert!(seen_call, "expected a DelegateToolCall event");
    assert!(seen_result, "expected a DelegateToolResult event");
}

/// The delegate sub-agent's own reasoning text must be forwarded to the
/// session event channel as `AgentEvent::DelegateThought { model, text }`.
#[tokio::test]
async fn delegate_thought_streams_as_a_chat_event() {
    let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(StubTool {
        calls: write_calls.clone(),
    }));

    let first_turn = json!({
        "choices": [{
            "message": {
                "content": "Let me write the file now.",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn = json!({"choices": [{"message": {"content": "done"}}]}).to_string();
    let base = scripted_server(vec![first_turn, final_turn]);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let mut ctx = test_ctx();
    ctx.events = Arc::new(crate::session::SessionEvents(tx));

    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
        .await
        .unwrap();
    assert!(out.contains("done"), "{out}");

    let mut seen = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            crate::session::AgentEvent::DelegateThought { model, text } => {
                seen = true;
                assert_eq!(model, "cheap");
                assert!(text.contains("Let me write the file now"), "{text:?}");
            }
            _ => {}
        }
    }
    assert!(seen, "expected a DelegateThought event");
}

/// A registry that contains exactly one recording stub `fs_write_file` tool.
fn write_stub_registry(calls: Arc<std::sync::atomic::AtomicUsize>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(StubTool { calls }));
    registry
}

/// A native-mode delegate that repeats the SAME tool call with the SAME
/// arguments gets it refused and then aborts instead of looping forever.
/// Sequence: turn 1 executes the call, turns 2-3 are refused as
/// no-progress repeats, turn 4 hits MAX_LOOP_REFUSALS and bails.
#[tokio::test]
async fn repeated_native_tool_call_is_refused_then_aborts() {
    let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = write_stub_registry(write_calls.clone());

    let call = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let base = scripted_server(vec![call.clone(), call.clone(), call.clone(), call]);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let err = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("repeated identical action"), "{text}");
    assert!(text.contains("fs_write_file"), "{text}");
    assert_eq!(
        write_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the repeat must never reach the tool: only the first call runs"
    );
}

/// The same guard works for ReAct-text delegates: an identical
/// Tool:/Args: call repeated with nothing changing in between is refused
/// and the run aborts after MAX_LOOP_REFUSALS repeats.
#[tokio::test]
async fn repeated_react_tool_call_is_refused_then_aborts() {
    let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = write_stub_registry(write_calls.clone());

    let call =
            json!({"choices": [{"message": {"content": "Thought: retry the write\nTool: fs_write_file\nArgs: {\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"}}]})
                .to_string();
    let base = scripted_server(vec![call.clone(), call.clone(), call.clone(), call]);

    let mut d = delegate("cheap", &base);
    d.llm.protocol = Protocol::React;
    let cfg = Config {
        delegates: vec![d],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let err = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("repeated identical action"), "{text}");
    assert_eq!(
        write_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the repeat must never reach the tool: only the first call runs"
    );
}

/// A mutation between two identical calls is real progress, so the second
/// identical call must NOT be refused (no false positive on a legit
/// re-check after an edit).
#[tokio::test]
async fn identical_call_after_a_mutation_is_not_a_loop() {
    let write_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = write_stub_registry(write_calls.clone());

    let call_a = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let call_b = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_2",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"src/b.rs\",\"content\":\"pub fn b(){}\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn =
        json!({"choices": [{"message": {"content": "done, both files written"}}]}).to_string();
    // a executes, b executes (a mutation -> clears refusals), a again is a
    // legitimate re-check after state changed, then a final answer.
    let base = scripted_server(vec![call_a.clone(), call_b, call_a, final_turn]);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "write two files"}))
        .await
        .unwrap();
    assert!(out.contains("done, both files written"), "{out}");
    assert_eq!(
        write_calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "all three fs_write_file calls were legitimate"
    );
}

/// A delegate that tries to `git_commit` gets an "unknown tool" error and
/// must recover — commit is simply not in its registry.
#[tokio::test]
async fn delegate_cannot_commit_even_if_the_model_asks() {
    // Registry deliberately has no git_commit: only a stub writer exists.
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(StubTool {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }));

    // The model first (wrongly) asks to commit, then realises and answers.
    let commit_turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "git_commit",
                        "arguments": "{\"message\":\"x\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn =
        json!({"choices": [{"message": {"content": "no commit available, done"}}]}).to_string();
    let base = scripted_server(vec![commit_turn, final_turn]);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "commit everything"}))
        .await
        .unwrap();
    // The sub-agent loop must not have crashed: unknown tool became an
    // observation and the model recovered with a final answer.
    assert!(out.contains("no commit available, done"), "{out}");
}

/// The read guard itself (mirror of the main loop's `allow_read_step`, but
/// with delegate wording): the 21st consecutive read-only call is refused,
/// the nudge says implement instead of reading, and it never mentions
/// `update_plan` (delegates have no plan tool).
#[test]
fn delegate_read_guard_nudges_after_20_consecutive_reads() {
    let mut reads = 0usize;
    for i in 0..20 {
        assert!(
            refuse_reading("fs_read_file", &mut reads, DELEGATE_READ_NUDGE).is_none(),
            "read #{i} should be allowed before the threshold"
        );
    }
    assert_eq!(reads, 20);
    let msg = refuse_reading("fs_read_file", &mut reads, DELEGATE_READ_NUDGE)
        .expect("the 21st read is refused");
    assert!(msg.contains("20 reads"), "{msg}");
    assert!(msg.contains("implement now"), "{msg}");
    assert!(!msg.contains("update_plan"), "delegates cannot plan: {msg}");
    // The counter never grows past the threshold: further reads stay refused.
    assert!(refuse_reading("fs_read_file", &mut reads, DELEGATE_READ_NUDGE).is_some());
    assert_eq!(reads, 20);
}

/// Any non-read action resets the delegate's read counter, so a delegate
/// that edits between reads never trips the guard.
#[test]
fn delegate_read_guard_resets_after_an_action() {
    let mut reads = 19usize; // one short of the threshold
    assert!(refuse_reading("fs_write_file", &mut reads, DELEGATE_READ_NUDGE).is_none());
    assert_eq!(reads, 0, "a mutating call resets the read counter");
    assert!(refuse_reading("fs_read_ranges", &mut reads, DELEGATE_READ_NUDGE).is_none());
    assert_eq!(reads, 1);
}

/// `timeout_answer` returns a notice with the partial answer when the delegate
/// had produced text, and a plain "did not finish" notice when it had none.
#[test]
fn timeout_answer_reports_a_partial_or_missing_result() {
    let d = std::time::Duration::from_secs(60);
    let partial = timeout_answer("cheap", d, "I found the bug in fs.rs");
    assert!(partial.contains("60s"), "{partial}");
    assert!(partial.contains("I found the bug in fs.rs"), "{partial}");

    let none = timeout_answer("cheap", d, "   ");
    assert!(none.contains("without a final answer"), "{none}");
    assert!(none.contains("unavailable"), "{none}");
}

/// A delegate whose model never answers must still return (a slow or hung model
/// request used to hold the parent run open indefinitely): it is nudged once at
/// its inactivity budget, then stopped at twice that, replying with a notice
/// instead of blocking forever.
#[tokio::test]
async fn a_delegate_that_never_answers_times_out_within_its_budget() {
    // The server reads the request and then holds the connection open without
    // ever replying: the delegate's single model request would hang.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let mut used = 0usize;
        loop {
            let n = stream.read(&mut buf[used..]).unwrap();
            if n == 0 {
                break;
            }
            used += n;
            if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    });
    let base = format!("http://127.0.0.1:{port}/v1");
    let mut d = delegate("slow", &base);
    // The client timeout must not be what saves us: make it far larger than the
    // delegate budget we are testing.
    d.llm.timeout_secs = 30;
    let cfg = Config {
        delegates: vec![d],
        ..Config::default()
    };
    let limits = DelegateLimits {
        timeout: std::time::Duration::from_millis(150),
        ..DelegateLimits::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, ToolRegistry::new(), limits)
        .unwrap()
        .unwrap();
    let ctx = test_ctx();

    let started = std::time::Instant::now();
    let out = tool
        .invoke(&ctx, json!({"model": "slow", "task": "do the thing"}))
        .await
        .expect("a timed-out delegate returns a notice, not an error");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the delegate must be cut off, took {elapsed:?}"
    );
    assert!(out.contains("inactivity"), "{out}");
}

/// A tool that never returns does not hang the delegate: the call is cut off at
/// the idle gate and answered with an error so the history stays API-valid, and
/// the delegate gets its next turn (here it recovers and answers).
#[tokio::test]
async fn a_hanging_tool_is_cut_off_and_the_delegate_recovers() {
    /// A tool whose `invoke` never resolves.
    struct HangingTool;
    #[async_trait]
    impl Tool for HangingTool {
        fn spec(&self) -> &ToolSpec {
            &HANGING_SPEC
        }
        async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
            std::future::pending::<Result<String>>().await
        }
    }
    static HANGING_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
        name: "fs_read_file".into(),
        description: "hangs forever".into(),
        json_schema: json!({"type": "object"}),
    });

    // Turn 1 asks for the hanging read; after it is cut off the delegate is
    // reached again and answers differently, proving the cut-off left the run
    // usable instead of aborting it.
    let turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_0",
                    "function": { "name": "fs_read_file", "arguments": "{}" }
                }]
            }
        }]
    })
    .to_string();
    let recovered = json!({"choices": [{"message": {"content": "read another way"}}]}).to_string();
    let base = scripted_server(vec![turn, recovered]);
    let mut d = delegate("reader", &base);
    d.llm.protocol = Protocol::Native;
    d.llm.timeout_secs = 30;
    let cfg = Config {
        delegates: vec![d],
        ..Config::default()
    };
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(HangingTool));
    let limits = DelegateLimits {
        timeout: std::time::Duration::from_millis(150),
        ..DelegateLimits::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, limits)
        .unwrap()
        .unwrap();
    let ctx = test_ctx();

    let started = std::time::Instant::now();
    let out = tool
        .invoke(&ctx, json!({"model": "reader", "task": "read the file"}))
        .await
        .expect("a hanging tool is cut off, it does not block the run");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "a hanging tool must be cut off, took {elapsed:?}"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(100),
        "the hang must actually reach the idle gate, took {elapsed:?}"
    );
    assert!(out.contains("read another way"), "{out}");
}

/// A native-mode delegate that does nothing but read different files for 21
/// consecutive turns has its 21st read refused: the tool is only ever
/// reached 20 times, and the run still ends with a normal final answer.
#[tokio::test]
async fn native_delegate_read_guard_stops_a_read_only_run() {
    let read_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadStubTool {
        calls: read_calls.clone(),
    }));

    let mut turns = Vec::new();
    for i in 0..21 {
        turns.push(
            json!({
                "choices": [{
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "id": format!("call_{i}"),
                            "function": {
                                "name": "fs_read_file",
                                "arguments": json!({"path": format!("src/f{i}.rs")}).to_string()
                            }
                        }]
                    }
                }]
            })
            .to_string(),
        );
    }
    let final_turn = json!({"choices": [{"message": {"content": "done reading"}}]}).to_string();
    turns.push(final_turn);
    let base = scripted_server(turns);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "read src/f0.rs"}))
        .await
        .unwrap();
    assert!(out.contains("done reading"), "{out}");
    assert_eq!(
        read_calls.load(std::sync::atomic::Ordering::SeqCst),
        20,
        "the 21st read must be refused before it reaches the tool"
    );
}

/// The same read guard applies to ReAct-text delegates: 21 read-only
/// Tool:/Args: turns, the 21st refused, then a normal final answer.
#[tokio::test]
async fn react_delegate_read_guard_stops_a_read_only_run() {
    let read_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadStubTool {
        calls: read_calls.clone(),
    }));

    let mut turns = Vec::new();
    for i in 0..21 {
        turns.push(
            json!({"choices": [{"message": {"content": format!(
                "Thought: still exploring\nTool: fs_read_file\nArgs: {{\"path\":\"src/f{i}.rs\"}}"
            )}}]})
            .to_string(),
        );
    }
    let final_turn = json!({"choices": [{"message": {"content": "done reading"}}]}).to_string();
    turns.push(final_turn);
    let base = scripted_server(turns);

    let mut d = delegate("cheap", &base);
    d.llm.protocol = Protocol::React;
    let cfg = Config {
        delegates: vec![d],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let out = tool
        .invoke(&ctx, json!({"model": "cheap", "task": "read src/f0.rs"}))
        .await
        .unwrap();
    assert!(out.contains("done reading"), "{out}");
}

/// A fake chat server that answers N sequential requests with N distinct
/// raw JSON bodies and forwards each raw REQUEST body (read per
/// Content-Length) to the returned channel first. The delegate loop makes
/// one request per turn, so tests can assert what the delegate was asked
/// at every step and steer it between turns.
fn scripted_spy(responses: Vec<String>) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for resp_body in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            let mut tmp = [0u8; 8192];
            let mut body_len: Option<usize> = None;
            let mut header_end: Option<usize> = None;
            while body_len.is_none_or(|len| header_end.unwrap_or(0) + 4 + len > data.len()) {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&tmp[..n]);
                if header_end.is_none()
                    && let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    header_end = Some(p);
                    let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
                    body_len = head.lines().find_map(|l| {
                        l.trim()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse().ok())
                    });
                }
            }
            let body = match (header_end, body_len) {
                (Some(he), Some(len)) => {
                    let start = he + 4;
                    let end = (start + len).min(data.len());
                    String::from_utf8_lossy(&data[start..end]).into_owned()
                }
                _ => String::new(),
            };
            let _ = tx.send(body);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), rx)
}

/// Wait (with a timeout) for the next request body the fake server sees.
async fn recv_body(rx: &std::sync::mpsc::Receiver<String>) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(b) = rx.try_recv() {
            return b;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for a model request");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A fs_write_file stub whose invocation signals `called` (a oneshot: safe to
/// fire before the test awaits it) and then blocks on `release` (a Notify).
/// Lets a test steer a running delegate at a deterministic moment: the stub
/// is executing (so the sub-agent is mid-flight) when the steer is sent.
struct Gate {
    called: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: tokio::sync::Notify,
}

struct GatedWriteTool {
    gate: Arc<Gate>,
}

#[async_trait]
impl Tool for GatedWriteTool {
    fn spec(&self) -> &ToolSpec {
        &STUB_WRITE_SPEC
    }

    async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
        if let Some(send) = self.gate.called.lock().unwrap().take() {
            let _ = send.send(());
        }
        self.gate.release.notified().await;
        Ok("stub fs_write_file executed".to_string())
    }
}

/// A steering message typed while a DELEGATE owns the loop is delivered to
/// the delegate: sent while its fs_write_file tool is still executing, it must
/// appear in the delegate's NEXT model request, and the run must then end
/// normally.
#[tokio::test]
async fn delegate_receives_a_steer_mid_run() {
    const STEER: &str = "change direction now";
    let (called_tx, called_rx) = tokio::sync::oneshot::channel();
    let gate = Arc::new(Gate {
        called: std::sync::Mutex::new(Some(called_tx)),
        release: tokio::sync::Notify::new(),
    });
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(GatedWriteTool { gate: gate.clone() }));

    let tool_call_turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"src/a.rs\",\"content\":\"pub fn a(){}\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn =
        json!({"choices": [{"message": {"content": "done after the steer"}}]}).to_string();
    let (base, bodies) = scripted_spy(vec![tool_call_turn, final_turn]);

    let cfg = Config {
        delegates: vec![delegate("cheap", &base)],
        ..Config::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();

    // The delegate inherits a live steering pipe from its context.
    let (steer, steer_tx) = comrade_tool::Steer::channel();
    let mut ctx = test_ctx();
    ctx.steer = Some(steer);

    let run = tokio::spawn({
        let ctx = ctx.clone();
        async move {
            tool.invoke(&ctx, json!({"model": "cheap", "task": "write src/a.rs"}))
                .await
                .unwrap()
        }
    });

    // Request 1 (its first model turn) must NOT contain the steer yet.
    let first = recv_body(&bodies).await;
    assert!(
        !first.contains(STEER),
        "steer leaked into the first request"
    );

    // The delegate is now executing fs_write_file: steer it, then let the
    // tool finish so the loop reaches its next rest point.
    called_rx.await.unwrap();
    steer_tx.send(STEER.to_string()).unwrap();
    gate.release.notify_one();

    // Request 2 must carry the steer as a user message.
    let second = recv_body(&bodies).await;
    assert!(
        second.contains(STEER),
        "steer missing from the delegate's next request"
    );

    let out = run.await.unwrap();
    assert!(out.contains("done after the steer"), "{out}");
}

#[tokio::test]
async fn parallel_delegates_run_every_job_and_merge_replies() {
    let a_url = fake_chat_server("reply-from-alpha");
    let b_url = fake_chat_server("reply-from-beta");
    let cfg = vec![delegate("alpha", &a_url), delegate("beta", &b_url)];
    let tool = DelegateParallelTool::new(&cfg, ToolRegistry::new(), DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let out = tool
        .invoke(
            &ctx,
            json!({"jobs": [
                {"model": "alpha", "task": "do A"},
                {"model": "beta", "task": "do B", "context": "some context"}
            ]}),
        )
        .await
        .unwrap();
    assert!(out.contains("reply-from-alpha"), "{out}");
    assert!(out.contains("reply-from-beta"), "{out}");
    assert!(out.contains("job 1 (alpha)"), "{out}");
    assert!(out.contains("job 2 (beta)"), "{out}");
}

/// A stub `fs_write_file` that really writes into `ctx.project_root`, so an
/// isolating job's write lands in its worktree (not the main repo).
struct WriteIntoRoot;
#[async_trait]
impl Tool for WriteIntoRoot {
    fn spec(&self) -> &ToolSpec {
        &STUB_WRITE_SPEC
    }
    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("out.txt");
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
        std::fs::write(ctx.project_root.join(path), content)?;
        Ok(format!("wrote {path}"))
    }
}

fn scratch_git_repo() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "comrade-delegate-wt-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("readme.txt"), "hi\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    root
}

#[tokio::test]
async fn isolated_parallel_job_runs_in_its_own_worktree() {
    let root = scratch_git_repo();
    let tool_call_turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"out.txt\",\"content\":\"hi\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn = json!({"choices": [{"message": {"content": "wrote out.txt"}}]}).to_string();
    let base = scripted_server(vec![tool_call_turn, final_turn]);

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(WriteIntoRoot));
    let cfg = vec![delegate("alpha", &base)];
    let tool = DelegateParallelTool::new(&cfg, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let mut ctx = test_ctx();
    ctx.project_root = root.clone();
    ctx.cwd = root.clone();

    let out = tool
        .invoke(
            &ctx,
            json!({"jobs": [{"model": "alpha", "task": "write out.txt", "isolate": true}]}),
        )
        .await
        .unwrap();

    assert!(out.contains("wrote out.txt"), "{out}");
    assert!(out.contains("worktree"), "{out}");
    // The write landed in the worktree, never in the main repo root.
    assert!(
        !root.join("out.txt").exists(),
        "an isolated job must not write into the main tree"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn parallel_jobs_isolate_by_default() {
    let root = scratch_git_repo();
    let tool_call_turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"out.txt\",\"content\":\"hi\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn = json!({"choices": [{"message": {"content": "wrote out.txt"}}]}).to_string();
    let base = scripted_server(vec![tool_call_turn, final_turn]);

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(WriteIntoRoot));
    let cfg = vec![delegate("alpha", &base)];
    let tool = DelegateParallelTool::new(&cfg, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let mut ctx = test_ctx();
    ctx.project_root = root.clone();
    ctx.cwd = root.clone();

    // No `isolate` key: the default must isolate the job in a worktree.
    let out = tool
        .invoke(
            &ctx,
            json!({"jobs": [{"model": "alpha", "task": "write out.txt"}]}),
        )
        .await
        .unwrap();

    assert!(out.contains("worktree"), "{out}");
    assert!(
        !root.join("out.txt").exists(),
        "a default job must isolate and not write into the main tree"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn parallel_jobs_share_when_not_a_git_repo() {
    let root = std::env::temp_dir().join(format!(
        "comrade-delegate-nogit-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let tool_call_turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "fs_write_file",
                        "arguments": "{\"path\":\"out.txt\",\"content\":\"hi\"}"
                    }
                }]
            }
        }]
    })
    .to_string();
    let final_turn = json!({"choices": [{"message": {"content": "wrote out.txt"}}]}).to_string();
    let base = scripted_server(vec![tool_call_turn, final_turn]);

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(WriteIntoRoot));
    let cfg = vec![delegate("alpha", &base)];
    let tool = DelegateParallelTool::new(&cfg, registry, DelegateLimits::default())
        .unwrap()
        .unwrap();
    let mut ctx = test_ctx();
    ctx.project_root = root.clone();
    ctx.cwd = root.clone();

    let out = tool
        .invoke(
            &ctx,
            json!({"jobs": [{"model": "alpha", "task": "write out.txt"}]}),
        )
        .await
        .unwrap();

    assert!(out.contains("not a git repository"), "{out}");
    assert!(
        root.join("out.txt").exists(),
        "without a git repo the job must fall back to the shared workspace"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn parallel_delegates_reject_unknown_model_and_empty_jobs() {
    let url = fake_chat_server("unused");
    let cfg = vec![delegate("alpha", &url)];
    let tool = DelegateParallelTool::new(&cfg, ToolRegistry::new(), DelegateLimits::default())
        .unwrap()
        .unwrap();
    let ctx = test_ctx();
    let empty = tool.invoke(&ctx, json!({"jobs": []})).await.unwrap_err();
    assert!(empty.to_string().contains("at least one job"), "{empty}");
    let unknown = tool
        .invoke(&ctx, json!({"jobs": [{"model": "nope", "task": "x"}]}))
        .await
        .unwrap_err();
    assert!(
        unknown.to_string().contains("unknown delegate model"),
        "{unknown}"
    );
}

#[test]
fn no_delegates_yields_no_parallel_tool() {
    assert!(
        DelegateParallelTool::new(&[], ToolRegistry::new(), DelegateLimits::default())
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_upward_escalation_is_capped_per_run() {
    use super::{MAX_UPWARD_ASKS, refuse_upward};
    let mut used = 0usize;
    // Other tools never touch the escalation budget.
    assert!(refuse_upward("fs_read_file", &mut used).is_none());
    assert_eq!(used, 0);
    for _ in 0..MAX_UPWARD_ASKS {
        assert!(refuse_upward("ask_upwards", &mut used).is_none());
    }
    // Past the cap the delegate is told to decide for itself.
    let msg = refuse_upward("ask_upwards", &mut used).expect("refused past the cap");
    assert!(msg.contains("Stop asking"), "{msg}");
}

#[test]
fn native_subagent_protocol_asks_for_reasoning_before_tool_calls() {
    let tools = ToolRegistry::new();
    let body = "BODY {protocol} {tool_lines} {project_root}";
    let native = render_subagent_system(body, "/tmp/p", &tools, true);
    let lower = native.to_lowercase();
    assert!(
        lower.contains("reasoning"),
        "native protocol must mention reasoning: {native}"
    );
    assert!(
        lower.contains("before each tool call"),
        "native protocol must ask for a sentence before each tool call: {native}"
    );
    let react = render_subagent_system(body, "/tmp/p", &tools, false);
    assert!(
        react.contains("Thought:"),
        "react protocol keeps its Thought line: {react}"
    );
}

/// A delegate that completes nothing for its inactivity budget is nudged to act:
/// the nudge must reach its NEXT model request.
#[tokio::test]
async fn a_frozen_delegate_is_nudged_to_act() {
    /// A tool whose `invoke` never resolves, so the delegate completes nothing.
    struct HangingTool;
    #[async_trait]
    impl Tool for HangingTool {
        fn spec(&self) -> &ToolSpec {
            &IDLE_HANGING_SPEC
        }
        async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
            std::future::pending::<Result<String>>().await
        }
    }
    static IDLE_HANGING_SPEC: std::sync::LazyLock<ToolSpec> =
        std::sync::LazyLock::new(|| ToolSpec {
            name: "fs_read_file".into(),
            description: "hangs forever".into(),
            json_schema: json!({"type": "object"}),
        });

    let turn = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_0",
                    "function": { "name": "fs_read_file", "arguments": "{}" }
                }]
            }
        }]
    })
    .to_string();
    let recovered = json!({"choices": [{"message": {"content": "acted at last"}}]}).to_string();
    let (base, rx) = scripted_spy(vec![turn, recovered]);
    let mut d = delegate("frozen", &base);
    d.llm.protocol = Protocol::Native;
    d.llm.timeout_secs = 30;
    let cfg = Config {
        delegates: vec![d],
        ..Config::default()
    };
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(HangingTool));
    let limits = DelegateLimits {
        timeout: std::time::Duration::from_millis(150),
        ..DelegateLimits::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, limits)
        .unwrap()
        .unwrap();
    let ctx = test_ctx();

    let out = tool
        .invoke(&ctx, json!({"model": "frozen", "task": "read the file"}))
        .await
        .unwrap();
    assert!(out.contains("acted at last"), "{out}");
    // Request 1 is the plain task; request 2 (after the hang was cut off at the
    // idle gate) must carry the nudge telling the delegate to act.
    let first = recv_body(&rx).await;
    assert!(
        !first.contains("Take an action now"),
        "no nudge before the idle gate: {first}"
    );
    let second = recv_body(&rx).await;
    assert!(
        second.contains("Take an action now"),
        "the frozen delegate must be nudged: {second}"
    );
}

/// A delegate that keeps making progress is never cut off, even when the TOTAL
/// run time exceeds the old wall-clock budget: only a gap with no completed
/// request or tool call counts against the inactivity budget.
#[tokio::test]
async fn a_progressing_delegate_is_never_cut_off() {
    /// A read that takes a fixed slice of time, so the total run time can exceed
    /// the budget while every individual gap stays below it.
    struct SlowRead;
    #[async_trait]
    impl Tool for SlowRead {
        fn spec(&self) -> &ToolSpec {
            &SLOW_READ_SPEC
        }
        async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            Ok("read ok".to_string())
        }
    }
    static SLOW_READ_SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
        name: "fs_read_file".into(),
        description: "a slow read".into(),
        json_schema: json!({"type": "object"}),
    });

    // Four slow reads (distinct paths, so the no-progress guard never fires) take
    // ~240ms in total - more than the 100ms budget and its 200ms stop - but every
    // gap is 60ms, below both gates.
    let mut turns = Vec::new();
    for i in 0..4 {
        turns.push(
            json!({
                "choices": [{
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "id": format!("call_{i}"),
                            "function": {
                                "name": "fs_read_file",
                                "arguments": json!({"path": format!("src/f{i}.rs")}).to_string()
                            }
                        }]
                    }
                }]
            })
            .to_string(),
        );
    }
    turns.push(json!({"choices": [{"message": {"content": "all read"}}]}).to_string());
    let base = scripted_server(turns);
    let mut d = delegate("steady", &base);
    d.llm.protocol = Protocol::Native;
    d.llm.timeout_secs = 30;
    let cfg = Config {
        delegates: vec![d],
        ..Config::default()
    };
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(SlowRead));
    let limits = DelegateLimits {
        timeout: std::time::Duration::from_millis(100),
        ..DelegateLimits::default()
    };
    let tool = DelegateTool::new(&cfg.delegates, registry, limits)
        .unwrap()
        .unwrap();
    let ctx = test_ctx();

    let started = std::time::Instant::now();
    let out = tool
        .invoke(&ctx, json!({"model": "steady", "task": "read four files"}))
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(out.contains("all read"), "{out}");
    assert!(
        !out.contains("inactivity"),
        "progress must not be cut off: {out}"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(200),
        "the run must outlast the whole budget to prove the point, took {elapsed:?}"
    );
}

/// The delegate prompt must tell the sub-agent to trust the tech lead's
/// reconnaissance and to judge sufficiency, not correctness: it must not
/// re-run the lead's reads or re-load what the task context already holds.
#[test]
fn delegate_prompt_trusts_the_leads_reconnaissance() {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(StubTool {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }));
    for native in [false, true] {
        let p = delegate_system_prompt("/repo/root", &reg, native).to_lowercase();
        assert!(p.contains("trust the tech lead"), "{p}");
        assert!(p.contains("sufficient"), "{p}");
        assert!(p.contains("do not read it again"), "{p}");
        assert!(p.contains("do not re-run"), "{p}");
        assert!(p.contains("never re-run"), "{p}");
    }
}
