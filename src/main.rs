//! Knut playground: experiment with routing without writing Rust.
//!
//! Subcommands:
//! - `route <prompt>`   route one prompt, print decision + action
//! - `repl`             route prompts from stdin, one per line
//! - `demo-tree`        execute the canned validated behavior tree
//! - `eval`             mini benchmark: hybrid routing vs always-reasoner
//!
//! Default output is compact; `--verbose` exposes the machinery and
//! `--json` emits the trace as JSON. Live Jev routing lands with the
//! System One HTTP adapter (issue #2); the mock runs fully offline.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;

use knut::{
    Action, Benchmark, BenchmarkTask, ComputeCascade, CostModel, Decision, DecisionInput,
    DecisionSource, ExpectedArtifact, IngressJudgments, JevSystemOne, Judgment, Knut, KnutError,
    Metrics, ModelIdentity, ModelRequest, ModelResponse, ModelTier, PlanNode, RetrievalJudgment,
    RetrievalSource, Risk, Route, SideEffect, SystemOne, TierJudgment, Tool, ToolMetadata,
    ToolRegistry, TreeExecutor, TreeRunResult, TurnOutcome, TurnTrace, TypeSafeConfig, Usage,
    VerificationVerdict, validate_plan,
};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    match runtime.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::ExitCode::from(2)
        }
    }
}

async fn run(args: Vec<String>) -> Result<(), KnutError> {
    let mut verbose = false;
    let mut as_json = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut capabilities: Vec<String> = Vec::new();
    let mut confidence_floor: Option<f32> = None;
    let mut backend = String::from("static");

    let mut iter = args.into_iter();
    let Some(command) = iter.next() else {
        return Err(KnutError::SystemOne(usage()));
    };

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--verbose" | "-v" => verbose = true,
            "--json" => as_json = true,
            "--capability" | "-c" => {
                let value = iter
                    .next()
                    .ok_or_else(|| KnutError::SystemOne("--capability needs a value".to_owned()))?;
                capabilities.push(value);
            }
            "--backend" => {
                backend = iter
                    .next()
                    .ok_or_else(|| KnutError::SystemOne("--backend needs a value".to_owned()))?;
            }
            "--confidence" => {
                let value = iter
                    .next()
                    .ok_or_else(|| KnutError::SystemOne("--confidence needs a value".to_owned()))?;
                confidence_floor = Some(value.parse().map_err(|_| {
                    KnutError::SystemOne(format!("--confidence: {value:?} is not a number"))
                })?);
            }
            other => positionals.push(other.to_owned()),
        }
    }

    let system_one = match backend.as_str() {
        "static" => SystemOneBackend::Static(MockIngress),
        "jev" => SystemOneBackend::Jev(Arc::new(JevSystemOne::new(TypeSafeConfig::from_env()?)?)),
        other => {
            return Err(KnutError::SystemOne(format!(
                "unknown backend {other:?}; expected static or jev"
            )));
        }
    };

    match command.as_str() {
        "route" => {
            let prompt = positionals.join(" ");
            if prompt.trim().is_empty() {
                return Err(KnutError::SystemOne(usage()));
            }
            route_once(
                prompt,
                capabilities,
                confidence_floor,
                verbose,
                as_json,
                system_one,
            )
            .await
        }
        "repl" => repl(capabilities, confidence_floor, verbose, system_one).await,
        "demo-tree" => demo_tree(verbose).await,
        "eval" => eval().await,
        "--help" | "-h" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        other => Err(KnutError::SystemOne(format!(
            "unknown command {other:?}\n{}",
            usage()
        ))),
    }
}

fn usage() -> String {
    "knut playground

USAGE:
  knut route <prompt> [--capability <id>]... [--confidence <f32>] [--verbose] [--json]
  knut repl [--capability <id>]... [--verbose]
  knut demo-tree [--verbose]
  knut eval

System One backends (--backend):
  static (default)   deterministic mock, fully offline
  jev                live TypeSafe System One API; reads TYPESAFE_API_KEY,
                     optional TYPESAFE_BASE_URL / TYPESAFE_MODEL"
        .to_owned()
}

/// Mock System One: full fan-out judgments derived from obvious prompt
/// shape. One call, complete answer set — like the real thing, offline.
#[derive(Clone, Copy)]
struct MockIngress;

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

/// The playground registry: two canned capabilities with read-only tools.
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

/// Selected System One backend for the playground.
#[derive(Clone)]
enum SystemOneBackend {
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

async fn route_once<S: SystemOne>(
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

async fn repl<S: SystemOne + Clone>(
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

/// The canned validated tree: read a note, summarize it, verify JSON.
async fn demo_tree(verbose: bool) -> Result<(), KnutError> {
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
            Ok(ModelResponse {
                content: self.content.clone(),
                identity: self.identity(),
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 8,
                },
                latency: std::time::Duration::from_millis(2),
            })
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

    let gate = Arc::new(knut::ExecutionGate::new(
        knut::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
    ));
    let executor = TreeExecutor::new(Arc::clone(&registry), gate, cascade, Arc::new(AcceptAll));
    let started = Instant::now();
    let result: TreeRunResult = executor
        .run(&plan, Arc::new(std::sync::atomic::AtomicBool::new(false)))
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

/// Mini benchmark: the mock router against an always-reasoner baseline.
async fn eval() -> Result<(), KnutError> {
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

    let cost = CostModel {
        fast: 0.05,
        standard: 0.2,
        reasoner: 2.0,
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

/// Describe the executed action the way a `Decision` would have looked.
fn action_decision(action: &Action) -> Decision {
    let (route, tier, capability) = match action {
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
        Route::Clarify => Action::AskUser,
        Route::Retrieve => Action::Retrieve(RetrievalSource::Files),
        Route::Act => Action::Tool {
            capability: decision.capability.clone().unwrap_or_default(),
        },
        Route::Generate => Action::Generate(decision.model_tier),
    };

    TurnTrace {
        prompt: prompt.to_owned(),
        source: DecisionSource::SystemOne,
        judgments: None,
        action,
        decision,
        shadow: None,
        edges: vec![],
        escalation_reasons: vec![],
        latency_ms: latency.as_millis() as u64,
        input_tokens,
        output_tokens,
        outcome,
        expected: None,
    }
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
