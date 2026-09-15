use super::*;

mod cases {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use comrade_tool::{
        PlanStepDraft, ToolContext,
        tool::{UserIo, UserPrompt, UserReply},
    };

    use super::*;
    use crate::MemoryUndo;
    use crate::config::{Autonomy, DelegateCfg, LlmCfg};
    use crate::session::AgentSession;

    /// The advise tool never touches the session/user (nothing is handed off),
    /// so a no-op IO double is all the tests need.
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

    /// Build an advise tool with an empty registry (no repository tools) so
    /// tests exercise the sub-agent loop without touching the real filesystem.
    fn mk_advise(delegates: &[DelegateCfg]) -> Result<Option<AskAdviseTool>> {
        AskAdviseTool::new(delegates, ToolRegistry::new(), DelegateLimits::default())
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
                content
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
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
    /// channel so tests can assert what the advisor was actually asked.
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

    #[test]
    fn empty_delegate_list_yields_no_tool() {
        let tool = mk_advise(&[]).unwrap();
        assert!(tool.is_none());
    }

    #[test]
    fn disabled_delegate_is_hidden_and_unselectable() {
        let on = delegate("on", "http://127.0.0.1:1/v1");
        let mut off = delegate("off", "http://127.0.0.1:1/v1");
        off.enabled = false;
        let tool = mk_advise(&[on, off]).unwrap().unwrap();
        let models = tool.spec().json_schema["properties"]["model"]["enum"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(models, vec!["on"]);
        assert!(!tool.spec().description.contains("off test delegate"));
    }

    #[test]
    fn all_delegates_disabled_yields_no_tool() {
        let mut off = delegate("off", "http://127.0.0.1:1/v1");
        off.enabled = false;
        assert!(mk_advise(&[off]).unwrap().is_none());
    }

    #[test]
    fn spec_advertises_model_enum_and_requires_question() {
        let tool = mk_advise(&[
            delegate("planner", "http://127.0.0.1:1/v1"),
            delegate("reviewer", "http://127.0.0.1:1/v1"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(tool.spec().name, "ask_advise");
        let schema = &tool.spec().json_schema;
        // Either a free-form consult (model + question) or a readiness check on
        // a plan step (step alone).
        assert_eq!(
            schema["oneOf"],
            serde_json::json!([
                { "required": ["step"] },
                { "required": ["model", "question"] }
            ])
        );
        let models = schema["properties"]["model"]["enum"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(models, vec!["planner", "reviewer"]);
    }

    #[test]
    fn read_only_for_advice_allows_only_read_only_tools() {
        // Exactly the main loop's read-only classification: advisors may browse
        // but must never be able to change the workspace or session.
        for name in [
            "fs_read_file",
            "fs_read_ranges",
            "fs_rgrep",
            "fs_list_dir",
            "git_diff",
            "git_log",
            "pom_model",
            "find_adr",
            "web_search",
        ] {
            assert!(
                AskAdviseTool::read_only_for_advice(name),
                "{name} should be browsable"
            );
        }
        for name in [
            "fs_write_file",
            "fs_edit",
            "ts_rename",
            "shell",
            "pom_run_task",
            "pom_run_tests",
            "pom_format_code",
            "git_commit",
            "record_adr",
            "amend_adr",
            "ask_advise",
            "delegate",
            "self_set_plan",
        ] {
            assert!(
                !AskAdviseTool::read_only_for_advice(name),
                "{name} must not be browsable"
            );
        }
    }

    #[tokio::test]
    async fn invoke_returns_advice_from_the_chosen_delegate() {
        let base = fake_chat_server("split the task into three small steps");
        let tool = mk_advise(&[
            delegate("cheap", &base),
            delegate("other", "http://127.0.0.1:1/v1"),
        ])
        .unwrap()
        .unwrap();
        let ctx = test_ctx();
        let out = tool
            .invoke(
                &ctx,
                json!({
                    "model": "cheap",
                    "question": "How should I plan this task?",
                    "context": "A Rust refactor touching two crates."
                }),
            )
            .await
            .unwrap();
        assert!(out.contains("advice from cheap"), "{out}");
        assert!(
            out.contains("split the task into three small steps"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn the_question_and_context_reach_the_advisor_verbatim() {
        let (base, spy) = request_spy();
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = test_ctx();
        tool.invoke(
            &ctx,
            json!({
                "model": "cheap",
                "question": "How should I plan this task?",
                "context": "Background the advisor cannot discover."
            }),
        )
        .await
        .unwrap();
        let body = spy.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(body.contains("How should I plan this task?"), "{body}");
        assert!(
            body.contains("Background the advisor cannot discover."),
            "{body}"
        );
        // The advisor system prompt frames it as advice, not delegated work.
        assert!(body.contains("ADVICE"), "{body}");
    }

    #[tokio::test]
    async fn unknown_model_and_empty_question_are_rejected() {
        let base = fake_chat_server("ignored");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = test_ctx();
        let err = tool
            .invoke(&ctx, json!({"model": "nope", "question": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown delegate model"));
        assert!(
            err.to_string().contains("cheap: cheap test delegate"),
            "{err}"
        );

        let err = tool
            .invoke(&ctx, json!({"model": "cheap", "question": "   "}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("question"));
    }

    #[tokio::test]
    async fn ask_gated_advisor_pauses_for_approval_then_advises() {
        let base = fake_chat_server("split the task into three small steps");
        let mut expensive = delegate("expensive", &base);
        expensive.approval = Autonomy::Ask;
        let tool = mk_advise(&[expensive]).unwrap().unwrap();
        let titles = Arc::new(Mutex::new(Vec::new()));
        let ctx = ask_ctx(Arc::new(RecordingIo {
            titles: titles.clone(),
            answer: "yes".into(),
        }));

        let out = tool
            .invoke(
                &ctx,
                json!({"model": "expensive", "question": "Is my plan sound?"}),
            )
            .await
            .unwrap();
        let held = titles.lock().unwrap();
        assert_eq!(held.len(), 1, "exactly one approval prompt expected");
        assert!(
            held[0].contains("Ask delegate expensive"),
            "confirm title should name the delegate: {:?}",
            held[0]
        );
        drop(held);
        assert!(out.contains("advice from expensive"), "{out}");
    }

    #[tokio::test]
    async fn denied_approval_aborts_the_consultation() {
        let base = fake_chat_server("ignored");
        let mut expensive = delegate("expensive", &base);
        expensive.approval = Autonomy::Ask;
        let tool = mk_advise(&[expensive]).unwrap().unwrap();
        let titles = Arc::new(Mutex::new(Vec::new()));
        let ctx = ask_ctx(Arc::new(RecordingIo {
            titles: titles.clone(),
            answer: "".into(), // not affirmative -> denied
        }));

        let err = tool
            .invoke(
                &ctx,
                json!({"model": "expensive", "question": "Is my plan sound?"}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("user denied"), "{err}");
        assert_eq!(titles.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deny_gated_advisor_is_refused_without_running() {
        let mut guarded = delegate("guarded", "http://127.0.0.1:1/v1");
        guarded.approval = Autonomy::Deny;
        let tool = mk_advise(&[guarded]).unwrap().unwrap();
        let ctx = test_ctx();

        let err = tool
            .invoke(
                &ctx,
                json!({"model": "guarded", "question": "Is my plan sound?"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("approval = \"deny\"") && err.to_string().contains("guarded"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn auto_advisor_runs_without_any_prompt() {
        let base = fake_chat_server("your plan is fine");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = ask_ctx(Arc::new(MustNotAsk));
        let out = tool
            .invoke(
                &ctx,
                json!({"model": "cheap", "question": "Is my plan sound?"}),
            )
            .await
            .unwrap();
        assert!(out.contains("advice from cheap"), "{out}");
    }

    /// A context whose session already holds one delegate-assigned plan step
    /// (id 1) so readiness checks have something to consult about.
    fn step_ctx() -> ToolContext {
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "refactor the helper fn".into(),
            verification: "cargo test passes".into(),
            model: "cheap".into(),
            context: "old signature lives in crates/x/src/lib.rs".into(),
        }]);
        ctx
    }

    #[tokio::test]
    async fn step_mode_marks_step_ready_when_delegate_confirms() {
        let base = fake_chat_server("That is enough for me.\nVERDICT: READY");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        let out = tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        assert!(out.contains("READY to pick up"), "{out}");
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::Ready);
        assert_eq!(
            step.note.as_deref(),
            Some("ready: cheap confirmed the context")
        );
    }

    #[tokio::test]
    async fn step_mode_stays_pending_and_reports_request_when_delegate_needs_more() {
        let base = fake_chat_server(
            "I need more detail.\nVERDICT: NEEDS_MORE: the exact fn signature of the helper",
        );
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        let out = tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        assert!(out.contains("needs more context for plan step 1"), "{out}");
        assert!(
            out.contains("the exact fn signature of the helper"),
            "{out}"
        );
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::Pending);
        assert_eq!(
            step.note.as_deref(),
            Some("awaiting context: the exact fn signature of the helper")
        );
    }

    #[tokio::test]
    async fn step_mode_readiness_prompt_reaches_the_delegate() {
        let (base, spy) = request_spy();
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let body = spy.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            body.contains("Context readiness check for plan step 1"),
            "{body}"
        );
        assert!(body.contains("refactor the helper fn"), "{body}");
        assert!(
            body.contains("old signature lives in crates/x/src/lib.rs"),
            "{body}"
        );
        // The advisor must close with an explicit verdict line.
        assert!(body.contains("VERDICT: NEEDS_MORE"), "{body}");
    }

    #[tokio::test]
    async fn step_mode_without_a_verdict_is_never_marked_ready() {
        // request_spy's canned answer ("ok") carries no VERDICT: line; the step
        // must stay pending rather than being optimistically confirmed.
        let (base, _spy) = request_spy();
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let step = &ctx.session.plan()[0];
        assert_eq!(step.status, PlanStatus::Pending);
        let note = step.note.as_deref().unwrap_or_default();
        assert!(note.contains("awaiting context"), "note was {note:?}");
    }

    #[tokio::test]
    async fn step_mode_ignores_extra_prose_but_rejects_a_mismatched_model() {
        let base = fake_chat_server("ignored");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();

        // `step` together with `question`/`context` is NOT an error: a small
        // model routinely echoes them, and both are derived from the plan step.
        tool.invoke(
            &ctx,
            json!({"step": 1, "question": "is it enough?", "context": "extra"}),
        )
        .await
        .expect("step mode ignores question/context");

        // The consulted delegate must be the step's own assigned delegate.
        let err = tool
            .invoke(&ctx, json!({"step": 1, "model": "other"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match the delegate assigned"),
            "{err}"
        );

        // Unknown step id.
        let err = tool.invoke(&ctx, json!({"step": 42})).await.unwrap_err();
        assert!(err.to_string().contains("no plan step with id 42"), "{err}");
    }

    #[tokio::test]
    async fn step_mode_refuses_steps_without_a_delegate_or_already_working() {
        let base = fake_chat_server("ignored");
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();

        // A step assigned to the main model has no delegate to consult.
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "my own work".into(),
            verification: String::new(),
            model: AGENT_MODEL.into(),
            context: String::new(),
        }]);
        let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
        assert!(err.to_string().contains("not a delegate"), "{err}");

        // An in-progress step is already being worked: no readiness check.
        let ctx = test_ctx();
        ctx.session.set_plan(vec![PlanStepDraft {
            goal: "delegated work".into(),
            verification: String::new(),
            model: "cheap".into(),
            context: String::new(),
        }]);
        ctx.session
            .update_plan(PlanTarget::Id(1), PlanStatus::InProgress, None);
        let err = tool.invoke(&ctx, json!({"step": 1})).await.unwrap_err();
        assert!(err.to_string().contains("in_progress"), "{err}");
    }

    /// The readiness check asks the delegate for SUFFICIENCY, not correctness: it
    /// must tell it to trust the context it was given instead of re-running the
    /// lead's reconnaissance to confirm it.
    #[tokio::test]
    async fn step_mode_readiness_prompt_asks_for_sufficiency_not_correctness() {
        let (base, spy) = request_spy();
        let tool = mk_advise(&[delegate("cheap", &base)]).unwrap().unwrap();
        let ctx = step_ctx();
        tool.invoke(&ctx, json!({"step": 1})).await.unwrap();
        let body = spy.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(body.contains("SUFFICIENT"), "{body}");
        assert!(body.contains("not whether it is correct"), "{body}");
        assert!(
            body.contains("Do NOT re-run the lead's reconnaissance"),
            "{body}"
        );
        assert!(!body.contains("browse the repository"), "{body}");
    }
}
