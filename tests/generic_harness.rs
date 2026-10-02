use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use knut::*;
use serde_json::{Value, json};

#[derive(Default)]
struct World {
    document: Mutex<String>,
    reads: AtomicUsize,
    writes: AtomicUsize,
}

struct WorldTool {
    id: &'static str,
    effect: SideEffect,
    world: Arc<World>,
}

#[async_trait]
impl Tool for WorldTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: self.id.to_owned(),
            tool_version: "1".to_owned(),
            capability: if self.id == "lookup" {
                "records"
            } else {
                "documents"
            }
            .to_owned(),
            description: self.id.to_owned(),
            input_schema: if self.id == "save" {
                json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
            } else {
                json!({"type":"object"})
            },
            side_effect: self.effect,
        }
    }

    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        if self.id == "save" {
            *self.world.document.lock().unwrap() = input["text"].as_str().unwrap().to_owned();
            self.world.writes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"saved":true}))
        } else {
            self.world.reads.fetch_add(1, Ordering::SeqCst);
            Ok(if self.id == "lookup" {
                json!({"balance":42})
            } else {
                json!({"text":*self.world.document.lock().unwrap()})
            })
        }
    }
}

struct Step {
    text: &'static str,
    calls: Vec<(&'static str, &'static str, Value)>,
}

fn answer(text: &'static str) -> Step {
    Step {
        text,
        calls: Vec::new(),
    }
}

fn call(id: &'static str, tool: &'static str, input: Value) -> Step {
    Step {
        text: "",
        calls: vec![(id, tool, input)],
    }
}

struct ScriptedModel {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl Model for ScriptedModel {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: "fixture".to_owned(),
            model: "fixture".to_owned(),
            tier: ModelTier::Reasoner,
        }
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            ..ModelCapabilities::buffered_text()
        }
    }

    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError> {
        self.requests.lock().unwrap().push(request.clone());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| answer("finished"));
        let mut response = ModelResponse::text(
            step.text,
            self.identity(),
            Usage::known(10, 5),
            Duration::ZERO,
        );
        response.tool_calls = step
            .calls
            .into_iter()
            .map(|(id, tool, arguments)| ToolCall {
                id: id.to_owned(),
                name: request
                    .tools
                    .iter()
                    .find(|metadata| metadata.id == tool)
                    .map(ToolMetadata::function_name)
                    .unwrap_or_else(|| tool.to_owned()),
                arguments,
            })
            .collect();
        Ok(response)
    }
}

fn runtime(
    steps: Vec<Step>,
) -> (
    SessionRuntime<StaticSystemOne>,
    Arc<ScriptedModel>,
    Arc<World>,
) {
    let world = Arc::new(World::default());
    let mut tools = ToolRegistry::default();
    for (id, effect) in [
        ("read", SideEffect::ReadOnly),
        ("lookup", SideEffect::ReadOnly),
        ("save", SideEffect::NonIdempotentWrite),
    ] {
        tools
            .register(WorldTool {
                id,
                effect,
                world: world.clone(),
            })
            .unwrap();
    }
    tools.register(AskUserTool).unwrap();
    let model = Arc::new(ScriptedModel {
        steps: Mutex::new(steps.into()),
        requests: Mutex::new(Vec::new()),
    });
    let router = Knut::new(StaticSystemOne::new(Decision {
        route: Route::Generate,
        confidence: 1.0,
        retrieval: None,
        capability: None,
        model_tier: ModelTier::Reasoner,
        risk: Risk::Low,
        parallelizable: false,
    }));
    let runtime = SessionRuntime::new(
        Arc::new(router),
        Arc::new(tools),
        Arc::new(ExecutionGate::new(
            SideEffectPolicy::new()
                .allow(SideEffect::ReadOnly)
                .require_approval(SideEffect::NonIdempotentWrite),
        )),
        Arc::new(ComputeCascade::empty().with_reasoner(model.clone())),
        Arc::new(AcceptAllVerifier),
    );
    (runtime, model, world)
}

fn submit(prompt: &str, options: TaskOptions) -> SessionCommand {
    SessionCommand::Submit {
        prompt: prompt.to_owned(),
        options,
    }
}

fn record(uri: &str, description: &str) -> ContextRecord {
    ContextRecord {
        source: ResourceRef {
            uri: uri.to_owned(),
            revision: "v1".to_owned(),
        },
        description: description.to_owned(),
        content: json!({"text":description}),
    }
}

#[tokio::test]
async fn lookup_uses_a_tool_result_before_answering() {
    let (mut runtime, model, world) = runtime(vec![
        call("balance", "lookup", json!({})),
        answer("Balance: 42"),
    ]);
    runtime
        .command(submit("Look up the balance", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 4).await,
        Some(TaskState::Completed)
    );
    assert_eq!(world.reads.load(Ordering::SeqCst), 1);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].exchanges[0].results[0].output,
        json!({"balance":42})
    );
    assert!(runtime.events().events().any(|event| matches!(event,
        SessionEvent::TaskCompleted { summary, .. } if summary == "Balance: 42")));
}

#[tokio::test]
async fn document_approval_resumes_exact_calls_without_repeating_reads() {
    let (mut runtime, model, world) = runtime(vec![
        Step {
            text: "",
            calls: vec![
                ("read", "read", json!({})),
                ("save", "save", json!({"text":"updated"})),
            ],
        },
        answer("Saved"),
    ]);
    runtime
        .command(submit("Update the document", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Waiting));
    assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let WaitKind::Approval { approval_key } = runtime.pending_wait().unwrap().kind.clone() else {
        panic!()
    };
    assert!(
        runtime
            .command(SessionCommand::Approve {
                approval_key: "wrong".to_owned()
            })
            .await
            .is_err()
    );
    assert!(
        runtime
            .command(SessionCommand::Answer {
                value: "yes".to_owned()
            })
            .await
            .is_err()
    );
    runtime
        .command(SessionCommand::Approve { approval_key })
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 4).await,
        Some(TaskState::Completed)
    );
    assert_eq!(world.reads.load(Ordering::SeqCst), 1);
    assert_eq!(world.writes.load(Ordering::SeqCst), 1);
    assert_eq!(*world.document.lock().unwrap(), "updated");
    assert_eq!(
        model.requests.lock().unwrap()[1].exchanges[0].results.len(),
        2
    );
}

#[tokio::test]
async fn malformed_tool_arguments_are_observations_and_never_write() {
    let (mut runtime, model, world) =
        runtime(vec![call("bad", "save", json!({})), answer("Missing text")]);
    runtime
        .command(submit("Update the document", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 8).await,
        Some(TaskState::Failed)
    );
    assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    assert!(
        model.requests.lock().unwrap()[1].exchanges[0].results[0].output["error"]
            .as_str()
            .unwrap()
            .contains("text")
    );
}

#[tokio::test]
async fn restricted_tools_cannot_be_dispatched_by_name() {
    let (mut runtime, model, world) = runtime(vec![
        call("bad", "save", json!({"text":"forbidden"})),
        answer("Unavailable"),
    ]);
    runtime
        .command(submit(
            "Read the document",
            TaskOptions {
                tools: Some(vec!["read".to_owned()]),
                ..TaskOptions::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 4).await,
        Some(TaskState::Completed)
    );
    assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    assert_eq!(model.requests.lock().unwrap()[0].tools.len(), 1);
}

#[tokio::test]
async fn denial_and_cancellation_never_execute_pending_writes() {
    for cancel in [false, true] {
        let (mut runtime, _, world) =
            runtime(vec![call("save", "save", json!({"text":"updated"}))]);
        runtime
            .command(submit("Update the document", TaskOptions::default()))
            .await
            .unwrap();
        assert_eq!(runtime.drive().await, Some(TaskState::Waiting));
        let WaitKind::Approval { approval_key } = runtime.pending_wait().unwrap().kind.clone()
        else {
            panic!()
        };
        runtime
            .command(if cancel {
                SessionCommand::Cancel
            } else {
                SessionCommand::Deny { approval_key }
            })
            .await
            .unwrap();
        assert!(runtime.drive().await.unwrap().is_terminal());
        assert!(runtime.pending_wait().is_none());
        assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn steering_discards_the_pending_action_and_keeps_the_goal() {
    let (mut runtime, model, world) = runtime(vec![
        call("save", "save", json!({"text":"obsolete"})),
        answer("Kept unchanged"),
    ]);
    runtime
        .command(submit("Update the document", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Waiting));
    let WaitKind::Approval { approval_key } = runtime.pending_wait().unwrap().kind.clone() else {
        panic!()
    };
    runtime
        .command(SessionCommand::Steer {
            prompt: "Keep it unchanged".to_owned(),
        })
        .await
        .unwrap();
    assert!(
        runtime
            .command(SessionCommand::Approve { approval_key })
            .await
            .is_err()
    );
    assert_eq!(
        drive_until_stable(&mut runtime, 4).await,
        Some(TaskState::Completed)
    );
    assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    let requests = model.requests.lock().unwrap();
    assert!(requests[1].instruction.contains("Update the document"));
    assert!(requests[1].instruction.contains("Keep it unchanged"));
    assert!(requests[1].exchanges.is_empty());
}

#[tokio::test]
async fn duplicate_call_ids_fail_before_any_tool_executes() {
    let (mut runtime, _, world) = runtime(vec![Step {
        text: "",
        calls: vec![("same", "read", json!({})), ("same", "lookup", json!({}))],
    }]);
    runtime
        .command(submit("Inspect records", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Failed));
    assert_eq!(world.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn queue_retains_options_and_resets_the_previous_task_configuration() {
    let (mut runtime, model, _) = runtime(vec![answer("first"), answer("second")]);
    runtime
        .command(submit(
            "First task",
            TaskOptions {
                tools: Some(vec![]),
                instructions: "First rules".to_owned(),
                context: vec![record("document:first", "first context")],
                ..TaskOptions::default()
            },
        ))
        .await
        .unwrap();
    runtime
        .command(SessionCommand::Queue {
            prompt: "Second task".to_owned(),
            options: TaskOptions {
                tools: Some(vec!["lookup".to_owned()]),
                instructions: "Second rules".to_owned(),
                context: vec![record("record:second", "second context")],
                ..TaskOptions::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Queued));
    assert_eq!(runtime.drive().await, Some(TaskState::Completed));
    let requests = model.requests.lock().unwrap();
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[1].tools[0].id, "lookup");
    assert!(requests[1].instruction.contains("Second rules"));
    assert!(!requests[1].instruction.contains("First rules"));
    assert_eq!(
        requests[1].input["context"][0]["source"]["uri"],
        "record:second"
    );
    assert!(requests[1].exchanges.is_empty());
}

struct DocumentChecks {
    world: Arc<World>,
    change_revision: bool,
    version: AtomicUsize,
}

#[async_trait]
impl CompletionMonitor for DocumentChecks {
    fn requirements(&self) -> CompletionRequirements {
        CompletionRequirements::none().require("document", "the document is updated", true)
    }
    fn current_revision(&self) -> Result<ArtifactRevision, KnutError> {
        Ok(ArtifactRevision::new(
            "document:report",
            format!(
                "{}:{}",
                *self.world.document.lock().unwrap(),
                self.version.load(Ordering::SeqCst)
            ),
        ))
    }
    async fn verify(&self, revision: &ArtifactRevision) -> Vec<Evidence> {
        let passed = *self.world.document.lock().unwrap() == "updated";
        if self.change_revision {
            self.version.fetch_add(1, Ordering::SeqCst);
        }
        vec![Evidence {
            check: "document".to_owned(),
            subject: revision.clone(),
            produced_at: "now".to_owned(),
            passed,
            detail: json!({"actual":*self.world.document.lock().unwrap()}),
        }]
    }
}

struct ChangedDocumentChecks(DocumentChecks);

#[async_trait]
impl CompletionMonitor for ChangedDocumentChecks {
    fn requirements(&self) -> CompletionRequirements {
        self.0.requirements()
    }
    fn verify_unchanged(&self) -> bool {
        false
    }
    fn current_revision(&self) -> Result<ArtifactRevision, KnutError> {
        self.0.current_revision()
    }
    async fn verify(&self, revision: &ArtifactRevision) -> Vec<Evidence> {
        self.0.verify(revision).await
    }
}

#[tokio::test]
async fn unchanged_tasks_can_skip_profile_checks_but_keep_explicit_requirements() {
    for explicit in [false, true] {
        let (mut runtime, _, world) = runtime(vec![answer("This is a report.")]);
        runtime = runtime.with_completion(Arc::new(ChangedDocumentChecks(DocumentChecks {
            world,
            change_revision: false,
            version: AtomicUsize::new(0),
        })));
        let mut options = TaskOptions::default();
        if explicit {
            options.requirements =
                CompletionRequirements::none().require("document", "check the document", true);
        }
        runtime
            .command(submit("Explain the report", options))
            .await
            .unwrap();
        assert_eq!(
            drive_until_stable(&mut runtime, 8).await,
            Some(if explicit {
                TaskState::Failed
            } else {
                TaskState::Completed
            })
        );
        assert_eq!(runtime.events().events().any(|event| matches!(event, SessionEvent::NodeResult {node_label, ..} if node_label == "check/document")), explicit);
    }
}

#[tokio::test]
async fn changed_artifacts_cannot_skip_profile_checks() {
    let (mut runtime, _, world) = runtime(vec![answer("finished")]);
    runtime = runtime.with_completion(Arc::new(ChangedDocumentChecks(DocumentChecks {
        world: world.clone(),
        change_revision: false,
        version: AtomicUsize::new(0),
    })));
    runtime
        .command(submit("Check the report", TaskOptions::default()))
        .await
        .unwrap();
    *world.document.lock().unwrap() = "wrong content".to_owned();
    assert_eq!(
        drive_until_stable(&mut runtime, 8).await,
        Some(TaskState::Failed)
    );
    assert!(runtime.events().events().any(|event| matches!(event, SessionEvent::NodeResult {node_label, status: NodeStatus::Failed, ..} if node_label == "check/document")));
}

#[tokio::test]
async fn general_completion_checks_gate_the_actual_document_revision() {
    let (mut runtime, _, world) = runtime(vec![answer("finished")]);
    *world.document.lock().unwrap() = "updated".to_owned();
    runtime = runtime.with_completion(Arc::new(DocumentChecks {
        world,
        change_revision: false,
        version: AtomicUsize::new(0),
    }));
    runtime
        .command(submit("Check the report", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Completed));
    assert!(runtime.events().events().any(|event| matches!(event,
        SessionEvent::TaskCompleted { summary, .. } if summary.contains("document: passed"))));
}

#[tokio::test]
async fn a_monitor_cannot_complete_with_stale_passing_evidence() {
    let (mut runtime, _, world) = runtime(vec![]);
    *world.document.lock().unwrap() = "updated".to_owned();
    runtime = runtime.with_completion(Arc::new(DocumentChecks {
        world,
        change_revision: true,
        version: AtomicUsize::new(0),
    }));
    runtime
        .command(submit("Check the report", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 8).await,
        Some(TaskState::Failed)
    );
    assert!(
        !runtime
            .events()
            .events()
            .any(|event| matches!(event, SessionEvent::TaskCompleted { .. }))
    );
}

#[tokio::test]
async fn task_requirements_add_to_the_monitor_contract() {
    let (mut runtime, _, world) = runtime(vec![]);
    *world.document.lock().unwrap() = "updated".to_owned();
    runtime = runtime.with_completion(Arc::new(DocumentChecks {
        world,
        change_revision: false,
        version: AtomicUsize::new(0),
    }));
    runtime
        .command(submit(
            "Check the report",
            TaskOptions {
                requirements: CompletionRequirements::none().require(
                    "publication",
                    "publication acknowledged",
                    true,
                ),
                ..TaskOptions::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 8).await,
        Some(TaskState::Failed)
    );
}

#[tokio::test]
async fn context_selection_prioritizes_a_resource_and_records_the_decision() {
    let (mut runtime, model, _) = runtime(vec![answer("summary")]);
    runtime = runtime.with_frames(Arc::new(StaticFrameRouter::choice_over(
        "candidate",
        "1",
        0.95,
        &["0", ESCALATE_ID],
    )));
    runtime
        .command(submit(
            "Summarize the report",
            TaskOptions {
                context: vec![
                    record("https://example.test/background", "background"),
                    record("document:report", "report"),
                ],
                ..TaskOptions::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Completed));
    assert_eq!(
        model.requests.lock().unwrap()[0].input["context"][0]["source"]["uri"],
        "document:report"
    );
    assert!(runtime.events().events().any(|event| matches!(event,
        SessionEvent::FrameDecided { question_kind: FrameKind::ContextSelection, choice, .. } if choice == "1")));
}

#[tokio::test]
async fn unusable_context_decisions_keep_all_evidence() {
    let (mut runtime, model, _) = runtime(vec![answer("summary")]);
    runtime = runtime.with_frames(Arc::new(StaticFrameRouter::choice(
        "candidate",
        "missing",
        0.95,
    )));
    runtime
        .command(submit(
            "Summarize the report",
            TaskOptions {
                context: vec![record("document:one", "one"), record("tool:lookup", "two")],
                ..TaskOptions::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Completed));
    assert_eq!(
        model.requests.lock().unwrap()[0].input["context"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn jsonl_accepts_task_options_and_preserves_prompt_only_clients() {
    let plain = HeadlessCommand::parse(r#"{"type":"submit","prompt":"hello"}"#).unwrap();
    assert!(
        matches!(plain.to_session_command(), Some(SessionCommand::Submit { options, .. }) if options == TaskOptions::default())
    );
    let configured = HeadlessCommand::parse(
        r#"{"type":"queue","prompt":"hello","options":{"tools":[],"instructions":"brief"}}"#,
    )
    .unwrap();
    assert!(
        matches!(configured.to_session_command(), Some(SessionCommand::Queue { options, .. }) if options.tools == Some(vec![]) && options.instructions == "brief")
    );
}

#[tokio::test]
async fn invalid_task_configuration_is_rejected_before_model_work() {
    let (mut runtime, model, _) = runtime(vec![]);
    for options in [
        TaskOptions {
            tools: Some(vec!["missing".to_owned()]),
            ..TaskOptions::default()
        },
        TaskOptions {
            context: vec![record("", "invalid")],
            ..TaskOptions::default()
        },
    ] {
        assert!(
            runtime
                .command(submit("Inspect records", options))
                .await
                .is_err()
        );
    }
    assert!(model.requests.lock().unwrap().is_empty());
    assert!(runtime.task_state().is_none());
}

#[tokio::test]
async fn a_native_question_resumes_with_the_answer_as_a_tool_result() {
    let (mut runtime, model, _) = runtime(vec![
        call(
            "question",
            ASK_USER_TOOL_ID,
            json!({"question":"Which report?"}),
        ),
        answer("Selected report"),
    ]);
    runtime
        .command(submit("Inspect a report", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Waiting));
    assert_eq!(runtime.pending_wait().unwrap().message, "Which report?");
    runtime
        .command(SessionCommand::Answer {
            value: "Annual report".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 4).await,
        Some(TaskState::Completed)
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].exchanges[0].results[0].output,
        json!({"answer":"Annual report"})
    );
}

#[tokio::test]
async fn passing_checks_cannot_hide_a_failed_write() {
    let (mut runtime, _, world) = runtime(vec![call("bad", "save", json!({})), answer("finished")]);
    *world.document.lock().unwrap() = "updated".to_owned();
    runtime = runtime.with_completion(Arc::new(DocumentChecks {
        world: world.clone(),
        change_revision: false,
        version: AtomicUsize::new(0),
    }));
    runtime
        .command(submit("Update the report", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 8).await,
        Some(TaskState::Failed)
    );
    assert_eq!(world.writes.load(Ordering::SeqCst), 0);
    assert!(
        !runtime
            .events()
            .events()
            .any(|event| matches!(event, SessionEvent::TaskCompleted { .. }))
    );
}

#[tokio::test]
async fn failed_write_arguments_can_be_repaired_before_completion() {
    let (mut runtime, _, world) = runtime(vec![
        call("bad", "save", json!({})),
        call("fixed", "save", json!({"text":"updated"})),
        answer("Saved"),
    ]);
    runtime
        .command(submit("Update the document", TaskOptions::default()))
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 4).await,
        Some(TaskState::Waiting)
    );
    let WaitKind::Approval { approval_key } = runtime.pending_wait().unwrap().kind.clone() else {
        panic!()
    };
    runtime
        .command(SessionCommand::Approve { approval_key })
        .await
        .unwrap();
    assert_eq!(
        drive_until_stable(&mut runtime, 4).await,
        Some(TaskState::Completed)
    );
    assert_eq!(world.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn uncertain_context_priority_leaves_resource_order_unchanged() {
    let (mut runtime, model, _) = runtime(vec![answer("summary")]);
    runtime = runtime.with_frames(Arc::new(StaticFrameRouter::choice_over(
        "candidate",
        "1",
        0.6,
        &["0", ESCALATE_ID],
    )));
    runtime
        .command(submit(
            "Summarize the report",
            TaskOptions {
                context: vec![
                    record("document:first", "first"),
                    record("document:second", "second"),
                ],
                ..TaskOptions::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(runtime.drive().await, Some(TaskState::Completed));
    assert!(runtime.events().events().any(|event| matches!(
        event,
        SessionEvent::FrameDecided {
            question_kind: FrameKind::ContextSelection,
            overridden: true,
            ..
        }
    )));
    assert_eq!(
        model.requests.lock().unwrap()[0].input["context"][0]["source"]["uri"],
        "document:first"
    );
}
