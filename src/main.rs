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

    // The workbench shell polls the terminal on the main task, so session
    // work runs on a separate worker: a slow provider must never block
    // typing. Other commands do not need threads, but the cost is
    // negligible and one runtime keeps the binary simple.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
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
        "doctor" => doctor(live_from_flags(&positionals)).await,
        "verify" => verify_workspace(verbose, as_json).await,
        "tui" | "workbench" => tui().await,
        "sessions" => sessions(&positionals, as_json).await,
        "bench" => bench().await,
        "lsp" => lsp_status().await,
        "jsonl" => headless_jsonl(&positionals).await,
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
  knut doctor [--live]
  knut verify [--json]     run the workspace's real checks for the current revision
  knut tui                 open the workbench shell (Ratatui)
  knut bench               run the pilot benchmark and write an inspectable report
  knut lsp                 report language-server availability and negotiated features
  knut jsonl [prompt]      headless JSONL: commands on stdin, events on stdout
  knut sessions list       list stored sessions
  knut sessions show <id>  replay a stored transcript (state only)
  knut sessions export <id> [--raw]  export without executing anything
  knut sessions plan <id>  report whether a session can be resumed

System One backends (--backend):
  static (default)   deterministic mock, fully offline
  jev                live TypeSafe System One API; reads TYPESAFE_API_KEY,
                     optional TYPESAFE_BASE_URL / TYPESAFE_MODEL

Reasoner provider (knut doctor, and KNUT_PROVIDER_* env):
  KNUT_PROVIDER_API_KEY   required for live reasoner calls (never logged)
  KNUT_PROVIDER_BASE_URL  default https://api.z.ai/api/coding/paas/v4
  KNUT_PROVIDER_MODEL     default glm-5.3-flash
  KNUT_PROVIDER_TIER      fast | standard | reasoner"
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

/// `tui`: the workbench shell over the shared session event stream.
///
/// The shell renders events the runtime publishes; it never executes
/// tools or runs its own agent loop. Session work happens on a background
/// task, so a slow provider cannot block typing or navigation.
async fn tui() -> Result<(), KnutError> {
    let workspace = knut::Workspace::open(".")?;
    let mut state = knut::WorkbenchState::new(workspace.root().to_string_lossy().into_owned());
    state.mode = std::env::var("KNUT_MODE").unwrap_or_else(|_| "quality".to_owned());
    state.model = std::env::var("KNUT_PROVIDER_MODEL").ok().or_else(|| {
        std::env::var("KNUT_PROVIDER_API_KEY")
            .ok()
            .map(|_| "configured".to_owned())
    });

    // A demo session drives the shell when no live backend is configured,
    // so the workbench is useful (and testable) without credentials.
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        let _ = event_tx.send(knut::SessionEvent::SessionStarted {
            protocol_version: knut::SESSION_PROTOCOL_VERSION,
        });

        // The shell is over scripted events by default; a live session
        // replaces this loop when providers are configured (#18/#21).
        while let Some(command) = command_rx.recv().await {
            match command {
                knut::SessionCommand::Submit { prompt } => {
                    let _ = event_tx.send(knut::SessionEvent::TaskStarted {
                        task: knut::TaskId(1),
                        prompt: prompt.clone(),
                    });
                    let _ = event_tx.send(knut::SessionEvent::Routed {
                        task: knut::TaskId(1),
                        turn: knut::TurnId(1),
                        revision: knut::TaskRevision(1),
                        source: knut::DecisionSource::SystemOne,
                        action: knut::Action::Discover,
                        confidence: 0.9,
                    });
                    let _ = event_tx.send(knut::SessionEvent::WaitingForUser {
                        task: knut::TaskId(1),
                        turn: knut::TurnId(1),
                        wait: knut::WaitKind::Question,
                        message:
                            "no reasoner configured: set KNUT_PROVIDER_API_KEY to run tasks end to end"
                                .to_owned(),
                    });
                    let _ = prompt;
                }
                knut::SessionCommand::Cancel => {
                    let _ = event_tx.send(knut::SessionEvent::TaskCancelled {
                        task: knut::TaskId(1),
                    });
                }
                _ => {}
            }
        }
    });

    knut::run_shell(state, event_rx, command_tx)
        .await
        .map_err(|err| KnutError::Tool(format!("terminal: {err}")))
}

/// `bench`: run the pilot task suite with real checks and write a report.
///
/// Offline by construction: the checks are real, the "model" side is the
/// local harness. The report says so, and the numbers are harness
/// measurements rather than live provider results.
async fn bench() -> Result<(), KnutError> {
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
        model: std::env::var("KNUT_PROVIDER_MODEL")
            .unwrap_or_else(|_| "not configured (offline harness)".to_owned()),
        reasoning_effort: "not applicable offline".to_owned(),
        question_pack: "frame-v1".to_owned(),
        pricing_source: "illustrative example, not a provider price list".to_owned(),
        pricing_as_of: "2026-09-21".to_owned(),
        mode: knut::RunMode::Offline,
        provider_region: None,
    };

    let mut runs = Vec::new();
    for task in knut::pilot_suite() {
        for arm in [knut::Arm::Baseline, knut::Arm::Hybrid] {
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
                arm,
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
                control_decisions: if arm == knut::Arm::Hybrid { 1 } else { 0 },
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

/// `jsonl`: the headless adapter over the same session runtime.
///
/// Contract: stdout carries protocol messages only; diagnostics go to
/// stderr. A single prompt argument runs one scripted session, otherwise
/// commands are read line by line from stdin.
async fn headless_jsonl(positionals: &[String]) -> Result<(), KnutError> {
    let mut adapter = knut::HeadlessAdapter::new(format!("headless-{}", std::process::id()));
    emit(&knut::HeadlessEvent::Ready {
        protocol_version: knut::JSONL_PROTOCOL_VERSION,
        session: adapter.session.id.clone(),
    });

    // A one-shot prompt from the command line, for scripting.
    if !positionals.is_empty() {
        let prompt = positionals.join(" ");
        let event = knut::SessionEvent::TaskStarted {
            task: knut::TaskId(1),
            prompt: prompt.clone(),
        };
        emit(&adapter.translate(&event));
        let routed = adapter.outcome(&knut::SessionEvent::TaskFailed {
            task: knut::TaskId(1),
            reason: "no reasoner configured: set KNUT_PROVIDER_API_KEY to run tasks headlessly"
                .to_owned(),
        });
        if let Some(routed) = routed {
            emit(&routed);
        }
        return Ok(());
    }

    // Otherwise read commands from stdin, one JSON object per line.
    use std::io::BufRead;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|err| KnutError::Tool(format!("reading stdin: {err}")))?;
        if line.trim().is_empty() {
            continue;
        }
        for event in adapter.handle_line(&line) {
            emit(&event);
        }
    }

    // stdin closed: the session stops.
    for event in adapter.handle_line(r#"{"type":"close"}"#) {
        emit(&event);
    }
    Ok(())
}

/// Write one protocol message to stdout, reporting write failures on
/// stderr rather than corrupting the stream.
fn emit(event: &knut::HeadlessEvent) {
    match knut::to_jsonl(event) {
        Ok(line) => println!("{line}"),
        Err(err) => eprintln!("{}", knut::diagnostic(&format!("{err:?}"))),
    }
}

/// `lsp`: report language-server availability without starting anything
/// implicitly.
///
/// Honest by design: an untrusted or missing server is reported, not
/// silently installed, and the session degrades to the workspace search
/// tools.
async fn lsp_status() -> Result<(), KnutError> {
    println!("language intelligence (LSP {})", knut::LSP_PROTOCOL_VERSION);
    println!(
        "  supported features: {}",
        knut::SUPPORTED_FEATURES
            .iter()
            .map(|feature| feature.label())
            .collect::<Vec<_>>()
            .join(", ")
    );

    for config in [knut::ServerConfig::rust(), knut::ServerConfig::typescript()] {
        let availability = {
            let manager = knut::LanguageServerManager::new(config.clone());
            manager.availability()
        };
        match availability {
            Ok(()) => println!("  {}: ready ({})", config.language, config.program),
            Err(unavailable) => {
                println!("  {}: unavailable — {unavailable}", config.language);
            }
        }
    }

    println!(
        "  note: servers are not started implicitly; trust one with ServerConfig::trust(true) \
         and a verified executable"
    );
    Ok(())
}

/// `sessions`: local session management over the SQLite store.
///
/// Every subcommand here is read-only unless it explicitly writes a new
/// session: none of them dispatches a tool or a provider call.
async fn sessions(args: &[String], as_json: bool) -> Result<(), KnutError> {
    let store_path = session_store_path();
    let mut store = knut::SessionStore::open(&store_path)?;
    let subcommand = args.first().map(String::as_str).unwrap_or("list");

    match subcommand {
        "list" | "" => {
            let sessions = store.sessions()?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&sessions)
                        .map_err(|err| KnutError::Tool(format!("serialize: {err}")))?
                );
            } else if sessions.is_empty() {
                println!("no stored sessions ({})", store_path.display());
            } else {
                println!("stored sessions ({})", store_path.display());
                for session in sessions {
                    println!(
                        "  {}  {}  {} events  {}",
                        session.id, session.task_state, session.event_count, session.workspace
                    );
                }
            }
            Ok(())
        }
        "show" | "replay" => {
            let Some(id) = args.get(1) else {
                return Err(KnutError::Tool(
                    "sessions show needs a session id".to_owned(),
                ));
            };
            // Replaying rebuilds state only: it never dispatches.
            let state = knut::replay_state(&store, id, "stored")?;
            println!("session {id} replayed (state only; nothing was executed)");
            println!("  task state: {:?}", state.task_state);
            println!("  timeline entries: {}", state.timeline.len());
            for entry in state.timeline.iter().rev().take(10).rev() {
                println!("  [{}] {}", entry.kind.label(), entry.text);
            }
            Ok(())
        }
        "export" => {
            let Some(id) = args.get(1) else {
                return Err(KnutError::Tool(
                    "sessions export needs a session id".to_owned(),
                ));
            };
            let include_raw = args.iter().any(|arg| arg == "--raw");
            let export = store.export(id, include_raw)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&export)
                    .map_err(|err| KnutError::Tool(format!("serialize: {err}")))?
            );
            if !include_raw {
                eprintln!("note: paths are redacted; pass --raw to include them explicitly");
            }
            Ok(())
        }
        "plan" | "resume" => {
            let Some(id) = args.get(1) else {
                return Err(KnutError::Tool(
                    "sessions plan needs a session id".to_owned(),
                ));
            };
            let workspace = knut::Workspace::open(".")?;
            let supervisor = std::sync::Arc::new(knut::Supervisor::new(workspace.clone()));
            let revision =
                knut::CheckRunner::new(workspace.clone(), supervisor, knut::CheckProfile::rust())
                    .current_revision("workspace")
                    .map(|revision| revision.revision)
                    .unwrap_or_else(|_| "unknown".to_owned());

            let plan = store.plan_resume(id, &revision, "side-effect-default")?;
            match plan {
                knut::ResumePlan::Continue => {
                    println!("session {id} can continue");
                }
                knut::ResumePlan::Reconcile(blockers) => {
                    println!("session {id} needs reconciliation before continuing:");
                    for blocker in blockers {
                        println!("  - {blocker}");
                    }
                }
                knut::ResumePlan::Refuse(blockers) => {
                    println!("session {id} cannot be resumed as-is:");
                    for blocker in blockers {
                        println!("  - {blocker}");
                    }
                }
            }
            let _ = &mut store;
            Ok(())
        }
        other => Err(KnutError::Tool(format!(
            "unknown sessions subcommand {other:?}; expected list, show, export or plan"
        ))),
    }
}

/// Where stored sessions live: alongside the user's local state, never in
/// the repository.
fn session_store_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("KNUT_SESSION_STORE") {
        return std::path::PathBuf::from(path);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    base.join("knut").join("sessions.db")
}

/// `verify`: run the workspace's real checks and report revision-bound
/// evidence. This is the same path completion uses, not a demo.
async fn verify_workspace(verbose: bool, as_json: bool) -> Result<(), KnutError> {
    let workspace = knut::Workspace::open(".")?;
    let profiles = knut::discover_profiles(&workspace);
    if profiles.is_empty() {
        return Err(KnutError::Tool(
            "no check profile fits this workspace (looked for Cargo.toml / package.json)"
                .to_owned(),
        ));
    }

    let supervisor = Arc::new(knut::Supervisor::new(workspace.clone()));
    let mut all_green = true;

    for profile in profiles {
        if !profile.tool_available() {
            println!(
                "profile {}: toolchain not installed; checks reported unavailable",
                profile.name
            );
        }
        let runner = knut::CheckRunner::new(workspace.clone(), Arc::clone(&supervisor), profile);
        let revision = runner.current_revision("workspace")?;
        let checks = runner.run_all(&revision).await;
        let report =
            knut::EvidenceReport::build(&runner.requirements(), revision, checks, Vec::new(), None);

        if as_json {
            let serialized = serde_json::to_string_pretty(&report)
                .map_err(|err| KnutError::Tool(format!("serialize report: {err}")))?;
            println!("{serialized}");
        } else {
            print!("{}", report.summary());
            if verbose {
                for check in &report.checks {
                    println!("  $ {}", check.command.join(" "));
                    if !check.output.is_empty() {
                        for line in check.output.lines().take(12) {
                            println!("      {line}");
                        }
                    }
                }
            }
        }

        // A weakened test set is never green, even with passing checks.
        all_green &= report.is_green();
    }

    if all_green {
        println!("verified: all blocking checks passed for this revision");
        Ok(())
    } else {
        Err(KnutError::Tool(
            "verification did not pass; see the evidence above".to_owned(),
        ))
    }
}

/// `doctor`: report what is configured, and optionally prove it works.
///
/// Offline by default: it inspects configuration and reports honestly
/// what is missing. `--live` performs one real reasoner call so an
/// operator can verify credentials and transport end to end.
async fn doctor(live: bool) -> Result<(), KnutError> {
    println!("knut doctor");

    // System One (Jev) configuration.
    match std::env::var("TYPESAFE_API_KEY") {
        Ok(key) if !key.trim().is_empty() => {
            let base = std::env::var("TYPESAFE_BASE_URL")
                .unwrap_or_else(|_| knut::DEFAULT_BASE_URL.to_owned());
            let model =
                std::env::var("TYPESAFE_MODEL").unwrap_or_else(|_| knut::DEFAULT_MODEL.to_owned());
            println!("  system one:  configured (base {base}, model {model})");
            if live {
                let system_one = knut::JevSystemOne::new(knut::TypeSafeConfig::from_env()?)?;
                let input = knut::DecisionInput::new(
                    "read the project notes".to_owned(),
                    vec!["files".to_owned()],
                );
                match knut::SystemOne::decide(&system_one, &input).await {
                    Ok(decision) => println!(
                        "    live:      ok (route {:?}, confidence {:.2})",
                        decision.route, decision.confidence
                    ),
                    Err(err) => println!("    live:      FAILED ({err})"),
                }
            }
        }
        _ => println!("  system one:  not configured (TYPESAFE_API_KEY unset)"),
    }

    // Reasoner (BYOK provider) configuration.
    match std::env::var("KNUT_PROVIDER_API_KEY") {
        Ok(key) if !key.trim().is_empty() => {
            let config = knut::ProviderConfig::from_env()?;
            let summary = config.summary();
            println!(
                "  reasoner:    configured (model {}, tier {:?}, billing {:?})",
                summary.model, summary.tier, summary.billing
            );
            println!(
                "               endpoint {} (timeout {:?})",
                summary.base_url, summary.timeout
            );

            let model = knut::OpenAiCompatibleModel::new(config)?;
            let caps = knut::Model::capabilities(&model);
            println!(
                "               capabilities: streaming={} tools={} reasoning={} continuation={} usage={}",
                caps.streaming, caps.tools, caps.reasoning, caps.continuation, caps.usage
            );

            if live {
                let request = knut::ModelRequest::new(
                    "Reply with exactly the word pong.",
                    knut::ExpectedArtifact::Text,
                );
                let mut sink = knut::BufferedSink::new();
                match knut::Model::stream(&model, &request, &mut sink).await {
                    Ok(response) => {
                        let usage =
                            match (response.usage.input_tokens, response.usage.output_tokens) {
                                (Some(i), Some(o)) => format!("{i} in / {o} out"),
                                _ => "unknown".to_owned(),
                            };
                        println!(
                            "    live:      ok (model {}, usage {usage}, reasoning parts {})",
                            response.identity.model,
                            response.continuation.parts.len()
                        );
                    }
                    Err(err) => println!("    live:      FAILED ({err})"),
                }
            }
        }
        _ => println!("  reasoner:    not configured (KNUT_PROVIDER_API_KEY unset)"),
    }

    // The offline playground always works.
    println!("  playground:  ok (static backend, offline)");

    if !live {
        println!("\nrun `knut doctor --live` to verify credentials with one real call");
    }
    Ok(())
}

fn live_from_flags(positionals: &[String]) -> bool {
    positionals.iter().any(|p| p == "--live")
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
