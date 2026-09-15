use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{
    AGENT_MODEL, PlanStatus, PlanStep, PlanStepDraft, PlanTarget, SessionControl, Tool,
    ToolContext, UndoLog, UserIo, UserPrompt, UserReply,
};
use serde_json::json;

use super::{
    AskForm, AskUpwards, SelfFinishPlan, SelfSetPlan, SelfSetStepContext, SelfSetStepModel,
    SelfUpdatePlan,
};

/// A real-enough session: stores the plan and which steps the `delegate`
/// tool has run, exactly like `AgentSession` does.
struct StubSession {
    plan: Mutex<Vec<PlanStep>>,
    delegated: Mutex<HashSet<u64>>,
    /// Handle a delegated sub-agent uses to ask the parent model (`ask_upwards`).
    upward: Option<comrade_tool::Upward>,
}

impl StubSession {
    fn with_plan(steps: Vec<PlanStepDraft>) -> Self {
        let plan = steps
            .into_iter()
            .enumerate()
            .map(|(i, d)| PlanStep {
                id: (i + 1) as u64,
                goal: d.goal,
                verification: d.verification,
                model: d.model,
                context: d.context,
                status: PlanStatus::Pending,
                note: None,
                started_at_ms: None,
                took_ms: None,
            })
            .collect();
        StubSession {
            plan: Mutex::new(plan),
            delegated: Mutex::new(HashSet::new()),
            upward: None,
        }
    }
}

impl SessionControl for StubSession {
    fn set_title(&self, _t: &str) {}
    fn title(&self) -> String {
        "test".into()
    }
    fn set_plan(&self, _s: Vec<PlanStepDraft>) {}
    fn plan(&self) -> Vec<PlanStep> {
        self.plan.lock().unwrap().clone()
    }
    fn update_plan(&self, target: PlanTarget, status: PlanStatus, note: Option<String>) -> bool {
        let mut plan = self.plan.lock().unwrap();
        let Some(step) = plan.iter_mut().find(|s| match &target {
            PlanTarget::Id(id) => s.id == *id,
            PlanTarget::Text(text) => s.goal.contains(text.as_str()),
        }) else {
            return false;
        };
        step.update(status, note);
        true
    }
    fn finish_plan(&self, _summary: Option<String>) {
        let mut plan = self.plan.lock().unwrap();
        for step in plan.iter_mut() {
            if !matches!(step.status, PlanStatus::Done | PlanStatus::Blocked) {
                step.update(PlanStatus::Done, None);
            }
        }
    }
    fn mark_step_delegated(&self, id: u64) {
        self.delegated.lock().unwrap().insert(id);
    }
    fn step_was_delegated(&self, id: u64) -> bool {
        self.delegated.lock().unwrap().contains(&id)
    }
    fn reassign_step_model(
        &self,
        target: &PlanTarget,
        model: &str,
    ) -> std::result::Result<bool, String> {
        let step_id = {
            let mut plan = self.plan.lock().unwrap();
            let Some(step) = plan.iter_mut().find(|s| match &target {
                PlanTarget::Id(id) => s.id == *id,
                PlanTarget::Text(text) => s.goal.contains(text.as_str()),
            }) else {
                return Ok(false);
            };
            if matches!(step.status, PlanStatus::InProgress | PlanStatus::Done) {
                return Err(format!(
                    "plan step {} is {} — a step's model can only be changed while it is \
                         pending, ready or blocked",
                    step.id, step.status
                ));
            }
            step.model = model.trim().to_string();
            step.id
        };
        self.delegated.lock().unwrap().remove(&step_id);
        Ok(true)
    }
    fn set_step_context(
        &self,
        target: &PlanTarget,
        context: &str,
    ) -> std::result::Result<bool, String> {
        let context = context.trim();
        let (step_id, was_ready) = {
            let mut plan = self.plan.lock().unwrap();
            let Some(step) = plan.iter_mut().find(|s| match &target {
                PlanTarget::Id(id) => s.id == *id,
                PlanTarget::Text(text) => s.goal.contains(text.as_str()),
            }) else {
                return Ok(false);
            };
            if matches!(step.status, PlanStatus::InProgress | PlanStatus::Done) {
                return Err(format!(
                    "plan step {} is {} — a step's context can only be changed while it is \
                         pending, ready or blocked",
                    step.id, step.status
                ));
            }
            if context.is_empty() {
                return Err(format!("plan step {} needs a non-empty context", step.id));
            }
            step.context = context.to_string();
            (step.id, step.status == PlanStatus::Ready)
        };
        if was_ready {
            self.update_plan(
                PlanTarget::Id(step_id),
                PlanStatus::Pending,
                Some("ready reset: context changed".into()),
            );
        }
        Ok(true)
    }
    fn set_status(&self, _s: &str) {}
    fn status(&self) -> String {
        String::new()
    }
    fn upward(&self) -> Option<comrade_tool::Upward> {
        self.upward.clone()
    }
}

struct NoopIo;
#[async_trait]
impl UserIo for NoopIo {
    async fn ask(&self, _p: UserPrompt) -> Result<UserReply> {
        match _p {
            UserPrompt::Form(spec) => Ok(UserReply::Form(spec.initial_values())),
            UserPrompt::Confirm { .. } => Ok(UserReply::Answer("yes".into())),
        }
    }
}

struct NoopUndo;
#[async_trait]
impl UndoLog for NoopUndo {
    async fn capture(&self, _p: &str, _b: String) -> Result<()> {
        Ok(())
    }
    async fn undo_last(&self) -> Result<usize> {
        Ok(0)
    }
    async fn is_empty(&self) -> bool {
        true
    }
    async fn len(&self) -> usize {
        0
    }
}

fn ctx(session: StubSession) -> ToolContext {
    ToolContext {
        project_root: PathBuf::from("/tmp/x"),
        cwd: PathBuf::from("/tmp/x"),
        session: Arc::new(session),
        user: Arc::new(NoopIo),
        undo: Arc::new(NoopUndo),
        auto_approve: true,
        events: Arc::new(comrade_tool::NoopEvents),
        steer: None,
        compact: None,
        stop: None,
    }
}

fn delegated_step() -> PlanStepDraft {
    PlanStepDraft {
        goal: "write the helper fn".into(),
        verification: "cargo test passes".into(),
        model: "cheap".into(),
        context: String::new(),
    }
}

fn plain_step() -> PlanStepDraft {
    PlanStepDraft {
        goal: "plain step".into(),
        verification: String::new(),
        model: AGENT_MODEL.into(),
        context: String::new(),
    }
}

#[tokio::test]
async fn update_plan_with_a_non_matching_text_lists_the_steps() {
    let c = ctx(StubSession::with_plan(vec![plain_step()]));
    let err = SelfUpdatePlan
        .invoke(&c, json!({"text": "something unrelated", "status": "done"}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("Existing steps"), "{msg}");
    assert!(msg.contains("1: plain step"), "{msg}");
    assert_eq!(c.session.plan()[0].status, PlanStatus::Pending);
}

#[tokio::test]
async fn cannot_mark_delegated_step_done_before_delegate_ran() {
    let c = ctx(StubSession::with_plan(vec![delegated_step()]));
    let err = SelfUpdatePlan
        .invoke(&c, json!({"index": 1, "status": "done"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("delegate"), "{err}");
    assert_eq!(c.session.plan()[0].status, PlanStatus::Pending);
}

#[tokio::test]
async fn can_mark_delegated_step_done_after_delegate_ran() {
    let c = ctx(StubSession::with_plan(vec![delegated_step()]));
    c.session.mark_step_delegated(1);
    let out = SelfUpdatePlan
        .invoke(&c, json!({"index": 1, "status": "done"}))
        .await
        .unwrap();
    assert!(out.contains("Step marked done"), "{out}");
    assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
}

#[tokio::test]
async fn can_still_mark_a_plain_step_done() {
    // A step the main model runs itself ("self") is the lead's own work:
    // it can be marked done without any `delegate` run.
    let c = ctx(StubSession::with_plan(vec![plain_step()]));
    SelfUpdatePlan
        .invoke(&c, json!({"index": 1, "status": "done"}))
        .await
        .unwrap();
    assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
}

#[tokio::test]
async fn finish_plan_allows_open_self_steps() {
    let c = ctx(StubSession::with_plan(vec![plain_step()]));
    SelfFinishPlan.invoke(&c, json!({})).await.unwrap();
    assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
}

#[tokio::test]
async fn set_plan_requires_a_model_per_step() {
    let c = ctx(StubSession::with_plan(vec![]));
    let err = SelfSetPlan
        .invoke(
            &c,
            json!({ "steps": [{ "goal": "do a thing", "verification": "x", "model": "  " }] }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`model`"), "{err}");
    assert!(err.to_string().contains("self"), "{err}");
}

#[tokio::test]
async fn set_plan_accepts_self_and_delegate_models() {
    let c = ctx(StubSession::with_plan(vec![]));
    let out = SelfSetPlan
        .invoke(
            &c,
            json!({ "steps": [
                    { "goal": "I do this", "model": "self" },
                    { "goal": "delegate does this", "model": "cheap" }
                ] }),
        )
        .await
        .unwrap();
    assert!(out.contains("2 step(s)"), "{out}");
}

#[tokio::test]
async fn finish_plan_refuses_open_never_delegated_steps() {
    let c = ctx(StubSession::with_plan(vec![delegated_step(), plain_step()]));
    let err = SelfFinishPlan.invoke(&c, json!({})).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("step 1"), "{msg}");
    assert_eq!(c.session.plan()[0].status, PlanStatus::Pending);
}

#[tokio::test]
async fn finish_plan_allows_delegated_steps_that_ran() {
    let c = ctx(StubSession::with_plan(vec![delegated_step()]));
    c.session.mark_step_delegated(1);
    SelfFinishPlan.invoke(&c, json!({})).await.unwrap();
    assert_eq!(c.session.plan()[0].status, PlanStatus::Done);
}

#[tokio::test]
async fn set_step_model_reassigns_a_pending_self_step_to_a_delegate() {
    let c = ctx(StubSession::with_plan(vec![plain_step()]));
    let out = SelfSetStepModel
        .invoke(&c, json!({ "index": 1, "model": "cheap" }))
        .await
        .unwrap();
    assert!(out.contains("\"cheap\""), "{out}");
    assert_eq!(c.session.plan()[0].model, "cheap");
}

#[tokio::test]
async fn set_step_model_takes_a_delegate_step_back_onto_self() {
    // Reassigning clears the delegation record: the "cheap" run no longer
    // counts, so the step cannot be marked done without a new delegate run.
    let c = ctx(StubSession::with_plan(vec![delegated_step()]));
    c.session.mark_step_delegated(1);
    assert!(c.session.step_was_delegated(1));
    let out = SelfSetStepModel
        .invoke(&c, json!({ "index": 1, "model": "self" }))
        .await
        .unwrap();
    assert!(out.contains("cleared"), "{out}");
    assert_eq!(c.session.plan()[0].model, AGENT_MODEL);
    assert!(!c.session.step_was_delegated(1));
}

#[tokio::test]
async fn set_step_model_reassigns_by_goal_text_and_allows_blocked_steps() {
    let c = ctx(StubSession::with_plan(vec![
        plain_step(),
        delegated_step(),
        plain_step(),
    ]));
    // block step 2 (goal "write the helper fn")
    c.session
        .update_plan(PlanTarget::Id(2), PlanStatus::Blocked, None);
    let out = SelfSetStepModel
        .invoke(&c, json!({ "text": "helper fn", "model": "groq" }))
        .await
        .unwrap();
    assert!(out.contains("\"groq\""), "{out}");
    assert_eq!(c.session.plan()[1].model, "groq");
}

#[tokio::test]
async fn set_step_model_refuses_in_progress_and_done_steps() {
    let c = ctx(StubSession::with_plan(vec![plain_step(), plain_step()]));
    c.session
        .update_plan(PlanTarget::Id(1), PlanStatus::InProgress, None);
    c.session
        .update_plan(PlanTarget::Id(2), PlanStatus::Done, None);
    let err = SelfSetStepModel
        .invoke(&c, json!({ "index": 1, "model": "cheap" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("in_progress"), "{err}");
    assert!(
        err.to_string().contains("pending, ready or blocked"),
        "{err}"
    );
    let err = SelfSetStepModel
        .invoke(&c, json!({ "index": 2, "model": "cheap" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("done"), "{err}");
    assert_eq!(c.session.plan()[0].model, AGENT_MODEL);
    assert_eq!(c.session.plan()[1].model, AGENT_MODEL);
}

#[tokio::test]
async fn set_step_model_rejects_blank_models_and_unknown_targets() {
    let c = ctx(StubSession::with_plan(vec![plain_step()]));
    let err = SelfSetStepModel
        .invoke(&c, json!({ "index": 1, "model": "   " }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`model`"), "{err}");
    assert!(err.to_string().contains("self"), "{err}");
    let err = SelfSetStepModel
        .invoke(&c, json!({ "index": 99, "model": "cheap" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no step matched"), "{err}");
    let err = SelfSetStepModel
        .invoke(&c, json!({ "model": "cheap" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`index`"), "{err}");
}

#[tokio::test]
async fn set_step_model_is_a_no_op_when_the_model_is_unchanged() {
    let c = ctx(StubSession::with_plan(vec![delegated_step()]));
    let out = SelfSetStepModel
        .invoke(&c, json!({ "index": 1, "model": "cheap" }))
        .await
        .unwrap();
    assert!(out.contains("already assigned"), "{out}");
    assert_eq!(c.session.plan()[0].model, "cheap");
}

#[tokio::test]
async fn set_step_model_accepts_an_unknown_model_value() {
    // Delegate names are validated by the `delegate` tool, not here (mirrors
    // set_plan): an arbitrary non-blank model is stored verbatim.
    let c = ctx(StubSession::with_plan(vec![plain_step()]));
    SelfSetStepModel
        .invoke(&c, json!({ "index": 1, "model": "claude" }))
        .await
        .unwrap();
    assert_eq!(c.session.plan()[0].model, "claude");
}

#[tokio::test]
async fn set_step_context_replaces_the_context_of_a_pending_step() {
    let c = ctx(StubSession::with_plan(vec![delegated_step()]));
    let out = SelfSetStepContext
        .invoke(
            &c,
            json!({ "index": 1, "context": "the helper lives in crates/x" }),
        )
        .await
        .unwrap();
    assert!(out.contains("Context of step 1 replaced"), "{out}");
    assert_eq!(c.session.plan()[0].context, "the helper lives in crates/x");
}

#[tokio::test]
async fn set_step_context_targets_by_goal_text() {
    let c = ctx(StubSession::with_plan(vec![plain_step(), delegated_step()]));
    SelfSetStepContext
        .invoke(&c, json!({ "text": "helper fn", "context": "new ctx" }))
        .await
        .unwrap();
    assert_eq!(c.session.plan()[1].context, "new ctx");
    assert_eq!(c.session.plan()[0].context, String::new());
}

#[tokio::test]
async fn set_step_context_refuses_in_progress_and_done_steps() {
    let c = ctx(StubSession::with_plan(vec![
        delegated_step(),
        delegated_step(),
    ]));
    c.session
        .update_plan(PlanTarget::Id(1), PlanStatus::InProgress, None);
    c.session
        .update_plan(PlanTarget::Id(2), PlanStatus::Done, None);
    let err = SelfSetStepContext
        .invoke(&c, json!({ "index": 1, "context": "x" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("in_progress"), "{err}");
    let err = SelfSetStepContext
        .invoke(&c, json!({ "index": 2, "context": "x" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("done"), "{err}");
}

#[tokio::test]
async fn set_step_context_drops_a_ready_step_back_to_pending() {
    let c = ctx(StubSession::with_plan(vec![delegated_step()]));
    c.session.update_plan(
        PlanTarget::Id(1),
        PlanStatus::Ready,
        Some("ready: ok".into()),
    );
    assert_eq!(c.session.plan()[0].status, PlanStatus::Ready);
    SelfSetStepContext
        .invoke(&c, json!({ "index": 1, "context": "new enriched ctx" }))
        .await
        .unwrap();
    assert_eq!(c.session.plan()[0].context, "new enriched ctx");
    assert_eq!(c.session.plan()[0].status, PlanStatus::Pending);
    assert_eq!(
        c.session.plan()[0].note.as_deref(),
        Some("ready reset: context changed")
    );
}

#[tokio::test]
async fn set_step_context_rejects_blank_context_and_unknown_targets() {
    let c = ctx(StubSession::with_plan(vec![delegated_step()]));
    let err = SelfSetStepContext
        .invoke(&c, json!({ "index": 1, "context": "   " }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("non-empty"), "{err}");
    let err = SelfSetStepContext
        .invoke(&c, json!({ "index": 99, "context": "x" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no step matched"), "{err}");
    let err = SelfSetStepContext
        .invoke(&c, json!({ "context": "x" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`index`"), "{err}");
}

#[tokio::test]
async fn ask_form_returns_id_equals_value_lines() {
    use std::collections::BTreeMap;

    // A UserIo that "fills" the form with chosen values.
    struct FormStubIo;
    #[async_trait]
    impl UserIo for FormStubIo {
        async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
            match prompt {
                UserPrompt::Form(_) => Ok(UserReply::Form(BTreeMap::from([
                    ("guests".into(), "3".into()),
                    ("room".into(), "double".into()),
                    ("breakfast".into(), "true".into()),
                ]))),
                other => panic!("unexpected prompt: {other:?}"),
            }
        }
    }

    let ctx = ToolContext {
        project_root: PathBuf::from("/tmp/x"),
        cwd: PathBuf::from("/tmp/x"),
        session: Arc::new(StubSession::with_plan(vec![])),
        user: Arc::new(FormStubIo),
        undo: Arc::new(NoopUndo),
        auto_approve: true,
        events: Arc::new(comrade_tool::NoopEvents),
        steer: None,
        compact: None,
        stop: None,
    };

    let json = json!({
        "title": "Room Booking",
        "description": "Choose your room and meal plan",
        "fields": [
            { "id": "guests", "label": "Number of guests", "kind": "number", "min": 1, "max": 4 },
            { "id": "room", "label": "Room type", "kind": "select", "options": ["single", "double"] },
            { "id": "breakfast", "label": "Breakfast", "kind": "checkbox", "default": "false" }
        ]
    });

    let out = AskForm.invoke(&ctx, json).await.unwrap();
    assert_eq!(out, "guests = 3\nroom = double\nbreakfast = true");
}

#[tokio::test]
async fn ask_form_accepts_a_diff_choice_field() {
    use comrade_tool::FieldKind;
    use std::collections::BTreeMap;

    struct PickIo;
    #[async_trait]
    impl UserIo for PickIo {
        async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
            match prompt {
                UserPrompt::Form(spec) => {
                    // The diff_choice parsed with both candidate diffs.
                    match spec.fields[0].kind {
                        FieldKind::DiffChoice { ref options } => {
                            assert_eq!(options.len(), 2);
                            assert_eq!(options[1].label, "B");
                        }
                        ref other => panic!("expected diff_choice, got {other:?}"),
                    }
                    Ok(UserReply::Form(BTreeMap::from([(
                        "pick".into(),
                        "B".into(),
                    )])))
                }
                other => panic!("unexpected prompt: {other:?}"),
            }
        }
    }

    let ctx = ToolContext {
        project_root: PathBuf::from("/tmp/x"),
        cwd: PathBuf::from("/tmp/x"),
        session: Arc::new(StubSession::with_plan(vec![])),
        user: Arc::new(PickIo),
        undo: Arc::new(NoopUndo),
        auto_approve: true,
        events: Arc::new(comrade_tool::NoopEvents),
        steer: None,
        compact: None,
        stop: None,
    };

    let out = AskForm
        .invoke(
            &ctx,
            json!({
                "title": "Pick a patch",
                "fields": [
                    { "id": "pick", "label": "Which patch?", "kind": "diff_choice",
                      "options": [
                          { "label": "A", "diff": "-let x = 1;\n+let x = 2;" },
                          { "label": "B", "diff": "-let x = 1;\n+let x = 3;" }
                      ] }
                ]
            }),
        )
        .await
        .unwrap();
    assert_eq!(out, "pick = B");
}

/// A fake tech lead: records the question it was asked and answers with a
/// fixed sentence, so the tool can be tested without a model.
struct FakeLead {
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl comrade_tool::UpwardAsk for FakeLead {
    async fn ask(&self, question: &str) -> Result<String> {
        self.seen.lock().unwrap().push(question.to_string());
        Ok("Put the function below the existing one.".to_string())
    }
}

#[tokio::test]
async fn ask_upwards_returns_the_parents_answer_with_the_plan_context() {
    let mut session = StubSession::with_plan(vec![plain_step()]);
    let lead = Arc::new(FakeLead {
        seen: Mutex::new(Vec::new()),
    });
    session.upward = Some(lead.clone());
    let c = ctx(session);
    let out = AskUpwards
        .invoke(&c, json!({ "question": "where do I put the new function?" }))
        .await
        .unwrap();
    assert!(out.contains("Answer from your tech lead"), "{out}");
    assert!(out.contains("Put the function below"), "{out}");
    // The parent received the plan so it could answer in context.
    let seen = lead.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].contains("plain step"), "{}", seen[0]);
    assert!(seen[0].contains("where do I put"), "{}", seen[0]);
}

#[tokio::test]
async fn ask_upwards_without_a_parent_tells_the_agent_to_decide() {
    let c = ctx(StubSession::with_plan(vec![plain_step()]));
    let out = AskUpwards
        .invoke(&c, json!({ "question": "what now?" }))
        .await
        .unwrap();
    assert!(out.contains("Decide for yourself"), "{out}");
}

#[test]
fn plan_step_tools_advertise_a_flat_step_selector() {
    // A local OpenAI-compatible server compiles the tool schema into a grammar;
    // `oneOf` (index | text) made ministral-3-3b omit the selector entirely, so
    // the call failed with "no step identified". The model-facing schema therefore
    // requires `index`; `text` stays a runtime fallback for the caller.
    for spec in [
        SelfUpdatePlan.spec(),
        SelfSetStepModel.spec(),
        SelfSetStepContext.spec(),
    ] {
        let s = &spec.json_schema;
        assert!(s.get("oneOf").is_none(), "{}: {s}", spec.name);
        assert!(s.get("anyOf").is_none(), "{}: {s}", spec.name);
        assert!(
            s["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some("index")),
            "{}: {s}",
            spec.name
        );
    }
}

#[tokio::test]
async fn ask_upwards_refuses_an_empty_question() {
    let c = ctx(StubSession::with_plan(vec![]));
    let err = AskUpwards
        .invoke(&c, json!({ "question": "   " }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must not be empty"), "{err}");
}
