use async_trait::async_trait;
use knut::{
    Action, Benchmark, BenchmarkTask, ComputeCascade, CostModel, Decision, DecisionInput,
    DecisionSource, ExpectedArtifact, IngressJudgments, JevSystemOne, Judgment, Knut, KnutError,
    Metrics, ModelIdentity, ModelRequest, ModelResponse, ModelTier, PlanNode, RetrievalJudgment,
    RetrievalSource, Risk, Route, SideEffect, SystemOne, TierJudgment, Tool, ToolMetadata,
    ToolRegistry, TreeExecutor, TreeRunResult, TurnOutcome, TurnTrace, TypeSafeConfig, Usage,
    VerificationVerdict, validate_plan,
};
use serde_json::json;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) struct MockIngress;

fn judgments_for(prompt: &str, capabilities: &[String]) -> IngressJudgments {
    use knut::{Complexity, Handler};

    let trimmed = prompt.trim();
    let handler = if capabilities.iter().any(|c| c == trimmed) {
        Handler::Act
    } else if trimmed.ends_with('?') {
        Handler::Clarify
    } else if trimmed.starts_with("find ") || trimmed.starts_with("search ") {
        Handler::Retrieve
    } else {
        Handler::Generate
    };

    let words = trimmed.split_whitespace().count();

    IngressJudgments {
        handler: Judgment {
            choice: handler,
            confidence: 0.9,
        },
        complexity: Judgment {
            choice: if words > 12 {
                Complexity::MultiStep
            } else {
                Complexity::Routine
            },
            confidence: 0.8,
        },
        retrieval: Judgment {
            choice: match handler {
                Handler::Retrieve => RetrievalJudgment::Files,
                _ => RetrievalJudgment::None,
            },
            confidence: 0.85,
        },
        missing_user_info: Judgment {
            choice: if trimmed.ends_with('?') {
                knut::YesNo::Yes
            } else {
                knut::YesNo::No
            },
            confidence: 0.85,
        },
        parallelizable: Judgment {
            choice: knut::YesNo::No,
            confidence: 0.9,
        },
        risk: Judgment {
            choice: Risk::Low,
            confidence: 0.9,
        },
        model_tier: Judgment {
            choice: if words > 20 {
                TierJudgment::Reasoner
            } else {
                TierJudgment::Fast
            },
            confidence: 0.8,
        },
    }
}

#[async_trait]
impl SystemOne for MockIngress {
    async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError> {
        Ok(judgments_for(&input.prompt, &input.capabilities).to_decision())
    }
}

fn playground_registry() -> ToolRegistry {
    struct CannedTool {
        id: &'static str,
        capability: &'static str,
        description: &'static str,
        output: serde_json::Value,
    }

    #[async_trait]
    impl Tool for CannedTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: self.id.to_owned(),
                tool_version: "1".to_owned(),
                capability: self.capability.to_owned(),
                description: self.description.to_owned(),
                input_schema: json!({ "type": "object" }),
                side_effect: SideEffect::ReadOnly,
            }
        }

        async fn call(&self, _input: serde_json::Value) -> Result<serde_json::Value, KnutError> {
            Ok(self.output.clone())
        }
    }

    let mut registry = ToolRegistry::default();
    registry
        .register(CannedTool {
            id: "get_weather",
            capability: "weather",
            description: "current conditions for a city",
            output: json!({ "city": "Stockholm", "temp_c": 14, "sky": "cloudy" }),
        })
        .unwrap();
    registry
        .register(CannedTool {
            id: "read_note",
            capability: "files",
            description: "read a note from the notebook",
            output: json!({ "note": "Knut routes; Jev judges; Rust decides." }),
        })
        .unwrap();
    registry
}

#[derive(Clone)]
pub(super) enum SystemOneBackend {
    Static(MockIngress),
    Jev(Arc<JevSystemOne>),
}

#[async_trait]
impl SystemOne for SystemOneBackend {
    async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError> {
        match self {
            SystemOneBackend::Static(mock) => mock.decide(input).await,
            SystemOneBackend::Jev(jev) => jev.decide(input).await,
        }
    }
}

pub(super) async fn route_once<S: SystemOne>(
    prompt: String,
    capabilities: Vec<String>,
    confidence_floor: Option<f32>,
    verbose: bool,
    as_json: bool,
    system_one: S,
) -> Result<(), KnutError> {
    let registry = playground_registry();
    let mut available = registry.capabilities();
    available.extend(capabilities);
    available.sort();
    available.dedup();

    let mut runtime = Knut::new(system_one);
    if let Some(floor) = confidence_floor {
        runtime = runtime.with_confidence_floor(floor);
    }

    let started = Instant::now();
    let input = DecisionInput::new(prompt.clone(), available.clone());
    let routed = runtime.route(&input).await?;
    let latency = started.elapsed();

    if as_json {
        let trace = json!({
            "prompt": prompt,
            "source": routed.source,
            "decision": routed.decision,
            "action": routed.action,
            "latency_ms": latency.as_millis() as u64,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&trace).unwrap_or_default()
        );
        return Ok(());
    }

    let source = match routed.source {
        DecisionSource::SystemZero => "system-0",
        DecisionSource::Cache => "cache",
        DecisionSource::SystemOne => "system-1",
    };
    println!(
        "{} -> {:?} (confidence {:.2}, via {source})",
        prompt.trim(),
        routed.action,
        routed.decision.confidence
    );

    if verbose {
        let judgments = judgments_for(&prompt, &available);
        println!("  route:      {:?}", routed.decision.route);
        println!("  tier:       {:?}", routed.decision.model_tier);
        println!("  risk:       {:?}", routed.decision.risk);
        println!(
            "  complexity: {:?}, parallelizable: {}",
            judgments.complexity.choice,
            judgments.parallelizable.choice == knut::YesNo::Yes
        );
        println!("  capabilities: {available:?}");
    }

    Ok(())
}

pub(super) async fn repl<S: SystemOne + Clone>(
    capabilities: Vec<String>,
    confidence_floor: Option<f32>,
    verbose: bool,
    system_one: S,
) -> Result<(), KnutError> {
    println!("knut repl — one prompt per line, empty line quits");
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    loop {
        print!("> ");
        std::io::stdout().flush().ok();
        let Some(line) = lines.next() else { break };
        let prompt = line.map_err(|e| KnutError::SystemOne(e.to_string()))?;
        if prompt.trim().is_empty() {
            break;
        }

        route_once(
            prompt,
            capabilities.clone(),
            confidence_floor,
            verbose,
            false,
            system_one.clone(),
        )
        .await?;
    }

    Ok(())
}

pub(super) async fn demo_tree(verbose: bool) -> Result<(), KnutError> {
    struct EchoModel {
        content: String,
    }

    #[async_trait]
    impl knut::Model for EchoModel {
        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: "canned".to_owned(),
                model: "demo".to_owned(),
                tier: ModelTier::Reasoner,
            }
        }

        async fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse, KnutError> {
            Ok(ModelResponse::text(
                self.content.clone(),
                self.identity(),
                Usage::known(12, 8),
                std::time::Duration::from_millis(2),
            ))
        }
    }

    struct AcceptAll;

    impl knut::Verifier for AcceptAll {
        fn verify(&self, _r: &ModelResponse) -> VerificationVerdict {
            VerificationVerdict::Sufficient
        }
    }

    let registry = Arc::new(playground_registry());
    let cascade = Arc::new(ComputeCascade::empty().with_reasoner(EchoModel {
        content: "{\"summary\": \"Knut routes; Jev judges; Rust decides.\"}".to_owned(),
    }));

    let plan = PlanNode::Sequence {
        id: "demo".to_owned(),
        children: vec![
            PlanNode::Tool {
                id: "note".to_owned(),
                capability: "files".to_owned(),
                tool_id: "read_note".to_owned(),
                input: json!({}),
            },
            PlanNode::Generate {
                id: "summarize".to_owned(),
                instruction: "summarize the note as JSON".to_owned(),
                tier: ModelTier::Reasoner,
                // The read result actually reaches the model instead of
                // being described to it (#19).
                input: json!({ "note": { "$ref": "note", "kind": "json" } }),
            },
            PlanNode::Verify {
                id: "check".to_owned(),
                target: "summarize".to_owned(),
                artifact: ExpectedArtifact::Json,
            },
        ],
    };

    validate_plan(&plan, &registry, &[ModelTier::Reasoner]).map_err(|e| {
        KnutError::PlanRejected {
            errors: vec![e.to_string()],
        }
    })?;

    let validated =
        knut::ValidatedPlan::from_validated(plan, 1).map_err(|e| KnutError::PlanRejected {
            errors: vec![e.to_string()],
        })?;

    let gate = Arc::new(knut::ExecutionGate::new(
        knut::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
    ));
    let executor = TreeExecutor::new(Arc::clone(&registry), gate, cascade, Arc::new(AcceptAll));
    let started = Instant::now();
    let result: TreeRunResult = executor
        .run(
            &validated,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await?;
    let latency = started.elapsed();

    println!(
        "demo tree: {:?} in {} ms",
        result.statuses.get("demo"),
        latency.as_millis()
    );
    for node in ["note", "summarize", "check"] {
        println!("  {node}: {:?}", result.statuses.get(node));
    }

    if verbose {
        for (id, output) in &result.outputs {
            println!("  output[{id}] = {output}");
        }
    }

    Ok(())
}

pub(super) async fn bench() -> Result<(), KnutError> {
    let root = std::env::temp_dir().join(format!(
        "knut-bench-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root)
        .map_err(|err| KnutError::Tool(format!("creating bench root: {err}")))?;

    let versions = knut::RunVersions {
        harness: format!("knut {}", env!("CARGO_PKG_VERSION")),
        report: knut::REPORT_VERSION,
        model: "none (offline checks only)".to_owned(),
        reasoning_effort: "not applicable offline".to_owned(),
        question_pack: "none (offline checks only)".to_owned(),
        pricing_source: "not applicable offline".to_owned(),
        pricing_as_of: "not applicable offline".to_owned(),
        mode: knut::RunMode::Offline,
        provider_region: None,
    };

    let mut runs = Vec::new();
    for task in knut::pilot_suite() {
        let started = std::time::Instant::now();
        let workspace = knut::TaskWorkspace::create(&task, &root)?;
        let revision_before = workspace.revision()?;
        let supervisor = Arc::new(knut::Supervisor::new(workspace.workspace()?));
        let revision = knut::ArtifactRevision::new(&task.id, revision_before.clone());

        // The harness's "work" for this pilot: for a task whose check
        // already passes, the correct action is to change nothing; for
        // a failing one, run the check so the failure is recorded
        // verbatim. The report makes clear that no model produced the
        // patch in offline mode.
        let check = knut::run_task_check(&supervisor, &task, &revision).await?;
        let verified = check.outcome == knut::CheckOutcome::Passed;
        let revision_after = workspace.revision()?;

        runs.push(knut::TaskRun {
            task: task.id.clone(),
            kind: task.kind,
            held_out: task.held_out,
            arm: knut::Arm::Baseline,
            verified,
            checks: vec![check],
            failure: if verified {
                None
            } else {
                Some("the task check did not pass in the offline harness".to_owned())
            },
            time_to_first_action_ms: Some(started.elapsed().as_millis() as u64),
            time_to_verified_ms: if verified {
                Some(started.elapsed().as_millis() as u64)
            } else {
                None
            },
            total_ms: started.elapsed().as_millis() as u64,
            control_decisions: 0,
            control_overhead_ms: 0,
            interventions: 0,
            input_tokens: None,
            output_tokens: None,
            cost: None,
            repairs: 0,
            revision_before,
            revision_after,
        });
    }

    let report = knut::BenchReport::build(versions, runs);
    let directory = std::path::PathBuf::from(
        std::env::var("KNUT_BENCH_OUT").unwrap_or_else(|_| "bench-out".to_owned()),
    );
    let path = report.write(&directory)?;
    println!("{}", report.human_summary());
    println!("report written to {}", path.display());
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

pub(super) async fn eval() -> Result<(), KnutError> {
    let tasks = [
        "weather",
        "read a note please",
        "find my deployment notes",
        "why does the router escalate?",
        "summarize the repository structure in detail with context",
    ];

    let benchmark = tasks.iter().fold(Benchmark::new(), |b, prompt| {
        b.with_task(DecisionInput::new(*prompt, vec![]), None)
    });

    // Illustrative rates for the offline playground only. They are
    // *example* prices with their source recorded, not measured costs, and
    // the token counts behind them come from a scripted mock router.
    let cost = CostModel {
        fast_input: 0.05,
        fast_output: 0.05,
        standard_input: 0.2,
        standard_output: 0.2,
        reasoner_input: 2.0,
        reasoner_output: 2.0,
        source: "illustrative example, not a provider price list".to_owned(),
        as_of: "2026-09-21".to_owned(),
        config_version: "playground-1".to_owned(),
    };

    let routed = |task: BenchmarkTask| {
        let prompt = task.input.prompt.clone();
        async move {
            let runtime = Knut::new(MockIngress);
            let started = Instant::now();
            let action = runtime
                .next(
                    prompt.clone(),
                    vec!["weather".to_owned(), "files".to_owned()],
                )
                .await
                .unwrap_or(Action::Generate(ModelTier::Reasoner));
            let decision = action_decision(&action);
            let trace = trace_for(
                &prompt,
                decision,
                started.elapsed(),
                40,
                20,
                TurnOutcome::Success,
            );
            (trace, TurnOutcome::Success)
        }
    };

    let baseline = |task: BenchmarkTask| {
        let prompt = task.input.prompt.clone();
        async move {
            let decision = always_reasoner_decision();
            let trace = trace_for(
                &prompt,
                decision,
                std::time::Duration::from_millis(90),
                400,
                200,
                TurnOutcome::Success,
            );
            (trace, TurnOutcome::Success)
        }
    };

    let comparison = benchmark.compare(&cost, routed, baseline).await;

    print_metrics("routed (hybrid)", &comparison.routed);
    print_metrics("baseline (reasoner)", &comparison.baseline);
    println!("reasoner turns saved: {}", comparison.reasoner_turns_saved);
    println!(
        "cost routed {:.4} vs baseline {:.4}",
        comparison.routed.estimated_cost, comparison.baseline.estimated_cost
    );

    Ok(())
}

fn always_reasoner_decision() -> Decision {
    Decision {
        route: Route::Generate,
        confidence: 0.99,
        retrieval: None,
        capability: None,
        model_tier: ModelTier::Reasoner,
        risk: Risk::Low,
        parallelizable: false,
    }
}

fn action_decision(action: &Action) -> Decision {
    let (route, tier, capability) = match action {
        Action::Plan => (Route::Plan, ModelTier::Reasoner, None),
        Action::AskUser => (Route::Clarify, ModelTier::Fast, None),
        Action::Retrieve(_) => (Route::Retrieve, ModelTier::Fast, None),
        Action::Discover => (Route::Act, ModelTier::Fast, None),
        Action::Tool { capability } => (Route::Act, ModelTier::Fast, Some(capability.clone())),
        Action::Generate(tier) => (Route::Generate, *tier, None),
    };

    Decision {
        route,
        confidence: 0.9,
        retrieval: None,
        capability,
        model_tier: tier,
        risk: Risk::Low,
        parallelizable: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn trace_for(
    prompt: &str,
    decision: Decision,
    latency: std::time::Duration,
    input_tokens: u64,
    output_tokens: u64,
    outcome: TurnOutcome,
) -> TurnTrace {
    let action = match decision.route {
        Route::Plan => Action::Plan,
        Route::Clarify => Action::AskUser,
        Route::Retrieve => Action::Retrieve(RetrievalSource::Files),
        Route::Act => Action::Tool {
            capability: decision.capability.clone().unwrap_or_default(),
        },
        Route::Generate => Action::Generate(decision.model_tier),
    };

    TurnTrace::new(
        prompt,
        DecisionSource::SystemOne,
        decision,
        action,
        latency.as_millis() as u64,
        input_tokens,
        output_tokens,
        outcome,
    )
}

fn print_metrics(label: &str, metrics: &Metrics) {
    println!("{label}:");
    println!(
        "  success {:.0}%  clarify {:.0}%  p50 {}ms  p95 {}ms",
        metrics.task_success_rate * 100.0,
        metrics.clarification_rate * 100.0,
        metrics.p50_latency_ms,
        metrics.p95_latency_ms
    );
    println!(
        "  tokens in {} out {}  cost {:.4}  reasoner turns {}",
        metrics.total_input_tokens,
        metrics.total_output_tokens,
        metrics.estimated_cost,
        metrics.reasoner_turns
    );
}

pub(super) fn backend(name: &str) -> Result<SystemOneBackend, KnutError> {
    match name {
        "static" => Ok(SystemOneBackend::Static(MockIngress)),
        "jev" => Ok(SystemOneBackend::Jev(Arc::new(JevSystemOne::new(
            TypeSafeConfig::from_env()?,
        )?))),
        other => Err(KnutError::SystemOne(format!(
            "unknown backend {other:?}; expected static or jev"
        ))),
    }
}
