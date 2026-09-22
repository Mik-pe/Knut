//! The workbench's live engine: the real session runtime, driven from the
//! shell.
//!
//! The TUI never executes tools itself. It publishes [`SessionCommand`]s
//! and renders [`SessionEvent`]s; this module is the thread in between,
//! owning a [`SessionRuntime`] built over the same provider, tools, gate
//! and checks that `knut run` uses. Anything the shell has that `run` does
//! not — a spinner, a queue, a decision pane — is a *view* of those
//! events, never a second implementation of the work.
//!
//! Two properties are load-bearing:
//!
//! - **Events reach the UI incrementally.** The runtime emits into its own
//!   bounded log; this driver tails that log by index and forwards only
//!   new events, so the shell paints a live transcript and never
//!   duplicates one.
//! - **Absence is honest.** With no provider configured the driver still
//!   runs and reports exactly what is missing, as an actionable task
//!   failure, rather than passing a scripted demo off as live work.

use std::sync::Arc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::decision::{Action, Decision, ModelTier, Risk, Route};
use crate::error::KnutError;
use crate::session::{SessionCommand, SessionEvent, SessionRuntime, TaskState};
use crate::system_one::StaticSystemOne;
use crate::{
    CheckProfile, CheckRunner, ComputeCascade, ExecutionGate, JevSystemOne, Knut, Planner,
    SideEffectPolicy, Supervisor, SystemZero, ToolRegistry, TypeSafeConfig, Workspace,
    register_workspace_tools,
};

/// What the driver can tell the shell about its own readiness.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineReport {
    /// The reasoner model name, when one is configured.
    pub model: Option<String>,
    /// The endpoint the reasoner talks to.
    pub base_url: Option<String>,
    /// Whether coding-loop System One decisions are available.
    pub frames: bool,
    /// Capabilities the tool registry exposes.
    pub tools: Vec<String>,
    /// Checks that gate completion.
    pub checks: Vec<String>,
    /// Why live tasks are unavailable, when they are.
    pub unavailable: Option<String>,
}
impl EngineReport {
    /// A one-line label for the header.
    pub fn reasoner_label(&self) -> String {
        self.model.clone().unwrap_or_else(|| "offline".to_owned())
    }

    /// The provider label, including the endpoint's host.
    pub fn endpoint_label(&self) -> Option<String> {
        let base = self.base_url.as_deref()?;
        Some(
            base.trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or(base)
                .to_owned(),
        )
    }
}

/// Build the engine for the current working directory.
///
/// The convenience entry point the shell uses; it never panics, because a
/// missing workspace is reported through the runtime rather than aborting
/// the display.
pub fn build_here() -> (Engine, EngineReport) {
    match Workspace::open(".") {
        Ok(workspace) => build(workspace),
        Err(err) => (
            minimal_engine(Some(format!("workspace unavailable: {err}"))),
            EngineReport {
                model: None,
                base_url: None,
                frames: false,
                tools: Vec::new(),
                checks: Vec::new(),
                unavailable: Some(format!("workspace unavailable: {err}")),
            },
        ),
    }
}

/// Describe the registry's capabilities for discovery.
///
/// One entry per capability, described by the tools that back it, so a
/// routed `Discover` can resolve to something real. The list is bounded by
/// the registry itself, which is small and explicit by construction.
fn discovery_candidates(registry: &ToolRegistry) -> Vec<(String, String)> {
    registry
        .capabilities()
        .into_iter()
        .map(|capability| {
            let tools: Vec<String> = registry
                .tools_for_capability(&capability)
                .into_iter()
                .map(|tool| format!("{} ({:?})", tool.id, tool.side_effect))
                .collect();
            let description = if tools.is_empty() {
                capability.clone()
            } else {
                format!("{capability}: {}", tools.join(", "))
            };
            (capability, description)
        })
        .collect()
}

/// A runtime with no tools and no model: enough to accept commands and
/// report why they cannot be carried out.
fn minimal_engine(reason: Option<String>) -> Engine {
    let gate = Arc::new(ExecutionGate::new(
        SideEffectPolicy::new().allow(crate::SideEffect::ReadOnly),
    ));
    let registry = ToolRegistry::default();
    let runtime = SessionRuntime::new(
        Arc::new(deterministic_router()),
        Planner::new(Arc::new(ComputeCascade::empty())),
        Arc::new(registry),
        gate,
        Arc::new(ComputeCascade::empty()),
        Arc::new(crate::AcceptAllVerifier),
    );
    let _ = reason;
    Engine::new(runtime, None)
}

/// Build the engine over this workspace, or explain why it cannot be
/// built.
///
/// Configuration is discovered, never invented: a missing key produces a
/// report that says so, so the shell can fail each task with an
/// actionable message instead of answering from a canned script.
pub fn build(workspace: Workspace) -> (Engine, EngineReport) {
    build_with_write_approval(workspace, false)
}

pub fn build_with_write_approval(
    workspace: Workspace,
    approve_writes: bool,
) -> (Engine, EngineReport) {
    let provider_result = crate::ProviderConfig::from_env();
    let provider_error = provider_result.as_ref().err().map(ToString::to_string);
    let provider_config = provider_result.ok();

    // Whether the configured provider can actually build an adapter is the
    // question that decides "configured" vs "offline": a base URL and key
    // that produce no usable client must not be reported as ready.
    let model_ready = provider_config
        .as_ref()
        .is_some_and(|config| crate::OpenAiCompatibleModel::new(config.clone()).is_ok());

    let mut report = EngineReport {
        model: None,
        base_url: None,
        frames: false,
        tools: Vec::new(),
        checks: Vec::new(),
        unavailable: None,
    };

    // Tools are the real bounded read/search/write set for this workspace.
    let mut registry = ToolRegistry::default();
    if let Err(err) = register_workspace_tools(&mut registry, workspace.clone()) {
        report.unavailable = Some(format!("workspace tools unavailable: {err}"));
    }
    report.tools = registry.capabilities();
    let profile = CheckProfile::for_workspace(&workspace);
    if let Ok(profile) = &profile {
        report.checks = profile
            .checks
            .iter()
            .map(|check| check.name.clone())
            .collect();
    } else if let Err(error) = &profile {
        report.unavailable = Some(error.to_string());
    }

    match (&provider_config, model_ready) {
        (Some(config), true) => {
            report.model = Some(config.model().to_owned());
            report.base_url = Some(config.base_url().to_owned());
        }
        _ => {
            report.unavailable = Some(format!(
                "reasoner unavailable: {}. Set KNUT_PROVIDER_API_KEY or ZAI_API_KEY and valid provider settings, then restart",
                provider_error.unwrap_or_else(|| "adapter could not be built".to_owned())
            ));
        }
    }

    // One configured model serves every tier the cascade can escalate
    // through. Leaving the fast and standard tiers empty would make a
    // routed *generation* fail with "no model configured for any tier"
    // even though a reasoner is present — the endpoint is the same either
    // way; only the request's effort differs. Each cascade needs its own
    // instance, so the adapter is rebuilt from its (cheap) configuration.
    let build_cascade = |config: &crate::ProviderConfig| {
        let model = |config: crate::ProviderConfig| crate::OpenAiCompatibleModel::new(config).ok();
        let mut cascade = ComputeCascade::empty();
        if let Some(model) = model(config.clone()) {
            cascade = cascade.with_fast(model);
        }
        if let Some(model) = model(config.clone()) {
            cascade = cascade.with_standard(model);
        }
        if let Some(model) = model(config.clone()) {
            cascade = cascade.with_reasoner(model);
        }
        cascade
    };

    let cascade = match &provider_config {
        Some(config) if report.model.is_some() => build_cascade(config),
        _ => ComputeCascade::empty(),
    };
    let planning_cascade = match &provider_config {
        Some(config) if report.model.is_some() => build_cascade(config),
        _ => ComputeCascade::empty(),
    };

    // The gate: the shell is interactive, so a write asks rather than
    // assuming consent. Pre-approving writes belongs to scripted runs.
    let policy = SideEffectPolicy::new().allow(crate::SideEffect::ReadOnly);
    let gate = Arc::new(ExecutionGate::new(if approve_writes {
        policy.allow(crate::SideEffect::IdempotentWrite)
    } else {
        policy.require_approval(crate::SideEffect::IdempotentWrite)
    }));

    // Routing: a live System One when a key is configured, otherwise the
    // deterministic router. A prompt that asks about the workspace must be
    // able to reach the workspace tools, and only a real router knows how.
    let jev = match std::env::var("TYPESAFE_API_KEY") {
        Ok(_) => match TypeSafeConfig::from_env()
            .ok()
            .and_then(|config| JevSystemOne::new(config).ok())
        {
            Some(jev) => {
                report.frames = true;
                Some(jev)
            }
            // A key that is present but unusable is reported rather than
            // quietly ignored: the deterministic path still runs.
            None => {
                report
                    .unavailable
                    .get_or_insert_with(|| "TYPESAFE_API_KEY is set but unusable".to_owned());
                None
            }
        },
        Err(_) => None,
    };
    let jev = jev.map(Arc::new);
    let frames = jev.clone();
    let router = Arc::new(Knut::new(LiveRouter::new(jev)).with_system_zero(SystemZero::empty()));

    let supervisor = Arc::new(Supervisor::new(workspace.clone()));
    let checks = profile
        .ok()
        .map(|profile| Arc::new(CheckRunner::new(workspace.clone(), supervisor, profile)));

    let discovery = discovery_candidates(&registry);
    let runtime = SessionRuntime::new(
        router,
        Planner::new(Arc::new(planning_cascade)),
        Arc::new(registry),
        gate,
        Arc::new(cascade),
        Arc::new(crate::AcceptAllVerifier),
    )
    .with_discovery(crate::session::DiscoveryCandidates {
        // Real capabilities, described by the tools that actually back
        // them. An empty set makes discovery ask the user for a capability
        // the harness already has.
        candidates: discovery,
    });
    let runtime = match checks {
        Some(checks) => runtime.with_checks(checks),
        None => runtime.with_requirements(crate::CompletionRequirements::none().require(
            "repository-checks",
            "configure checks for this repository",
            true,
        )),
    };
    let runtime = match frames {
        Some(frames) => runtime.with_frames(frames),
        None => runtime,
    };
    (Engine::new(runtime, Some(workspace)), report)
}

/// The router the shell runs on.
///
/// A live System One (Jev) is used when it is configured, because routing
/// is the one decision the harness must not invent: a prompt asking about
/// the workspace should reach the workspace tools, not a text-only
/// generation. Without a key the deterministic router below is used, and
/// its limits are reported rather than papered over.
pub struct LiveRouter {
    live: Option<Arc<JevSystemOne>>,
    fallback: StaticSystemOne,
}

impl LiveRouter {
    fn new(live: Option<Arc<JevSystemOne>>) -> Self {
        Self {
            live,
            fallback: StaticSystemOne::new(Decision {
                route: Route::Act,
                confidence: 0.9,
                retrieval: None,
                capability: Some("files".to_owned()),
                model_tier: ModelTier::Reasoner,
                risk: Risk::Low,
                parallelizable: false,
            }),
        }
    }
}

#[async_trait::async_trait]
impl crate::SystemOne for LiveRouter {
    async fn decide(&self, input: &crate::DecisionInput) -> Result<Decision, KnutError> {
        match &self.live {
            // A live router that fails must not silently become a
            // different decision: the failure is surfaced so the task
            // reports it.
            Some(live) => live.decide(input).await,
            None => self.fallback.decide(input).await,
        }
    }
}

/// The deterministic router for an offline shell: no System 0 shortcuts,
/// one confident generation decision.
fn deterministic_router() -> Knut<LiveRouter> {
    Knut::new(LiveRouter::new(None)).with_system_zero(SystemZero::empty())
}

/// The driver's handle on the runtime, plus the bookkeeping needed to
/// stream new events exactly once.
pub struct Engine {
    runtime: SessionRuntime<LiveRouter>,
    /// How many events of the runtime log have already been forwarded.
    cursor: usize,
    live_events: UnboundedReceiver<SessionEvent>,
    /// The workspace, retained so on-demand checks run against the real
    /// tree rather than a copy.
    workspace: Option<Workspace>,
}

impl Engine {
    fn new(runtime: SessionRuntime<LiveRouter>, workspace: Option<Workspace>) -> Self {
        let (sender, live_events) = tokio::sync::mpsc::unbounded_channel();
        Self {
            runtime: runtime.with_event_sink(sender),
            workspace,
            cursor: 0,
            live_events,
        }
    }

    /// Run the workspace's real checks for the current revision and return
    /// them as review rows, ready for the review pane.
    ///
    /// This is the same [`CheckRunner`] the completion gate consults, so
    /// what the user reads in review is what the gate will believe.
    pub async fn run_checks(&self) -> Result<Vec<crate::review::CheckRow>, KnutError> {
        let Some(workspace) = self.workspace.clone() else {
            return Err(KnutError::Tool(
                "no workspace is open, so no checks can run".to_owned(),
            ));
        };
        let supervisor = Arc::new(Supervisor::new(workspace.clone()));
        let profile = CheckProfile::for_workspace(&workspace)?;
        let runner = CheckRunner::new(workspace, supervisor, profile);
        let revision = runner.current_revision("workspace")?;
        let evidence = runner.run_all(&revision).await;
        Ok(evidence
            .iter()
            .map(|check| crate::review::CheckRow::from_evidence(check, &revision.revision))
            .collect())
    }

    /// Forward any new runtime events to the shell.
    ///
    /// Returns `false` when the shell's receiver is gone, which is the
    /// signal to stop driving: nobody is watching.
    ///
    /// Events are read from the runtime's own bounded log, which is the
    /// ordered transcript; the cursor makes this idempotent, so polling
    /// twice without new work forwards nothing.
    pub fn pump(&mut self, tx: &UnboundedSender<SessionEvent>) -> bool {
        while let Ok(event) = self.live_events.try_recv() {
            if tx.send(event).is_err() {
                return false;
            }
            self.cursor += 1;
        }
        !tx.is_closed()
    }

    /// How many events have been forwarded so far (test seam).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the runtime currently has an active task.
    pub fn task_state(&self) -> Option<TaskState> {
        self.runtime.task_state()
    }

    /// Total model calls observed by the runtime.
    pub fn model_calls(&self) -> u64 {
        self.runtime.model_calls()
    }

    /// The runtime's own cascade, exposed so a caller (or a test) can check
    /// that a configured model is actually reachable from the tiers a
    /// routed action may ask for.
    pub fn cascade(&self) -> &ComputeCascade {
        self.runtime.cascade()
    }

    /// Apply one command and drive the task forward.
    ///
    /// The loop is the one every client uses: `drive` performs one unit of
    /// work per tick and stops when the task needs the user or reaches a
    /// terminal state.
    ///
    /// Events are published *after every tick*, not once at the end: a
    /// shell that only saw the finished task would show a blank transcript
    /// for the whole run, which is exactly the opposite of a live view. The
    /// optional sink is called between ticks so the caller can forward what
    /// just happened while the task keeps working.
    pub async fn handle_with(
        &mut self,
        command: SessionCommand,
        mut on_tick: impl FnMut(&mut Self),
    ) {
        let described = describe(&command);
        if let Err(err) = self.runtime.command(command).await {
            let message = format!("{described} refused: {err}");
            self.runtime.emit_runtime_error(message);
            on_tick(self);
            return;
        }
        on_tick(self);
        for _ in 0..crate::MAX_TURNS_PER_TASK.max(1) {
            let Some(state) = self.runtime.drive().await else {
                break;
            };
            on_tick(self);
            if state.is_terminal() || state == TaskState::Waiting {
                break;
            }
        }
    }

    /// Apply one command and drive it to a stop, with no intermediate
    /// publication. Used by tests and non-streaming callers.
    pub async fn handle(&mut self, command: SessionCommand) {
        self.handle_with(command, |_| {}).await;
    }
}

/// The driver loop: owns the engine, drains commands and pumps events
/// until the shell drops its receiver.
pub async fn run_engine(
    mut engine: Engine,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
) {
    let _ = events.send(SessionEvent::SessionStarted {
        protocol_version: crate::SESSION_PROTOCOL_VERSION,
    });
    let (_, replacement) = tokio::sync::mpsc::unbounded_channel();
    let mut live = std::mem::replace(&mut engine.live_events, replacement);
    let mut pending = std::collections::VecDeque::new();
    let mut drive = false;
    loop {
        if let Some(command) = pending.pop_front() {
            let description = describe(&command);
            if let Err(error) = engine.runtime.command(command).await {
                engine
                    .runtime
                    .emit_runtime_error(format!("{description} refused: {error}"));
            }
            drive = engine.runtime.is_runnable();
        }
        let mut disconnected = false;
        let mut cancelled = false;
        if drive {
            let tick = engine.runtime.drive();
            tokio::pin!(tick);
            loop {
                tokio::select! {
                    biased;
                    event = live.recv() => {
                        if let Some(event) = event && events.send(event).is_err() {
                            disconnected = true;
                            break;
                        }
                    }
                    state = &mut tick => {
                        drive = matches!(state, Some(TaskState::Running | TaskState::Queued));
                        break;
                    }
                    command = commands.recv() => {
                        match command {
                            Some(SessionCommand::Cancel) => { cancelled = true; break; }
                            Some(command) => pending.push_back(command),
                            None => { disconnected = true; break; }
                        }
                    }

                }
            }
        } else if pending.is_empty() {
            tokio::select! {
                event = live.recv() => {
                    if let Some(event) = event && events.send(event).is_err() { disconnected = true; }
                }
                command = commands.recv() => {
                    match command {
                        Some(command) => pending.push_back(command),
                        None => disconnected = true,
                    }
                }
            }
        }
        if cancelled || disconnected {
            if engine
                .task_state()
                .is_some_and(|state| !state.is_terminal())
            {
                let _ = engine.runtime.command(SessionCommand::Cancel).await;
            }
            drive = false;
            while let Ok(event) = live.try_recv() {
                let _ = events.send(event);
            }
        }
        if disconnected {
            break;
        }
    }
}

/// A short label for a command, used when reporting a refusal.
fn describe(command: &SessionCommand) -> &'static str {
    match command {
        SessionCommand::Submit { .. } => "submit",
        SessionCommand::Steer { .. } => "steer",
        SessionCommand::SteerOrQueue { .. } => "steer",
        SessionCommand::Answer { .. } => "answer",
        SessionCommand::Approve { .. } => "approve",
        SessionCommand::Deny { .. } => "deny",
        SessionCommand::Pause => "pause",
        SessionCommand::Resume => "resume",
        SessionCommand::Cancel => "cancel",
    }
}

/// A short label for a routed action, for the status line and timeline.
pub fn action_label(action: &Action) -> String {
    match action {
        Action::Generate(tier) => match tier {
            ModelTier::Fast => "generate·fast".to_owned(),
            ModelTier::Standard => "generate·standard".to_owned(),
            ModelTier::Reasoner => "generate·reasoner".to_owned(),
        },
        Action::Retrieve(source) => format!("retrieve·{source:?}"),
        Action::Tool { capability } => format!("tool·{capability}"),
        Action::Discover => "discover".to_owned(),
        Action::AskUser => "ask".to_owned(),
    }
}

/// A short label for a routing source.
pub fn source_label(source: crate::DecisionSource) -> &'static str {
    match source {
        crate::DecisionSource::SystemZero => "sys0",
        crate::DecisionSource::Cache => "cache",
        crate::DecisionSource::SystemOne => "sys1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_workspace() -> (Workspace, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "knut-engine-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let workspace = Workspace::open(&root).unwrap();
        (workspace, root)
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn a_configured_reasoner_is_reachable_from_every_tier_it_can_route_to() {
        // The bug this guards: the shell built the model but handed the
        // runtime an *empty* cascade, so a routed generation failed with
        // "no model configured for any tier" while the header said the
        // reasoner was ready. A configured engine must answer for every
        // tier, because a deterministic router may ask for any of them.
        let (workspace, root) = fixture_workspace();
        let (engine, report) = build(workspace);
        if report.model.is_some() {
            let cascade = engine.cascade();
            for tier in [ModelTier::Fast, ModelTier::Standard, ModelTier::Reasoner] {
                assert!(
                    cascade.has_model_for(tier),
                    "a configured engine must serve the {tier:?} tier"
                );
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_submitted_generation_reaches_the_model_rather_than_failing_on_configuration() {
        // End-to-end shape of the same bug: submitting a prompt routes to
        // `Generate`, which needs a model in the runtime's cascade. Either
        // the task gets past configuration, or the engine reports that no
        // reasoner exists — never the contradiction of "configured" plus
        // "no model configured for any tier".
        let (workspace, root) = fixture_workspace();
        let (mut engine, report) = build(workspace);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        block_on(engine.handle(SessionCommand::Submit {
            prompt: "what does src/lib.rs export".to_owned(),
        }));
        engine.pump(&tx);

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        if report.model.is_some() {
            for event in &events {
                if let SessionEvent::TaskFailed { reason, .. } = event {
                    assert!(
                        !reason.contains("no model configured for any tier"),
                        "a configured engine must not fail on its own configuration: {reason}"
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_report_names_real_tools_and_checks() {
        let (workspace, root) = fixture_workspace();
        let (engine, report) = build(workspace);
        drop(engine);
        let _ = std::fs::remove_dir_all(root);

        assert!(report.tools.contains(&"files".to_owned()));
        assert!(report.checks.contains(&"build".to_owned()));
        assert!(report.checks.contains(&"test".to_owned()));
    }

    #[test]
    fn an_unconfigured_reasoner_is_reported_not_hidden() {
        // Whatever this machine's configuration, the report and the label
        // must agree: a model name implies an endpoint, and no model name
        // implies an explanation.
        let (workspace, root) = fixture_workspace();
        let (engine, report) = build(workspace);
        drop(engine);
        let _ = std::fs::remove_dir_all(root);

        if report.model.is_none() {
            assert!(report.unavailable.is_some());
            assert_eq!(report.reasoner_label(), "offline");
            assert!(report.endpoint_label().is_none());
        } else {
            assert!(report.base_url.is_some());
            assert!(report.endpoint_label().is_some());
        }
    }

    #[test]
    fn the_driver_forwards_each_event_exactly_once() {
        let (workspace, root) = fixture_workspace();
        let (mut engine, _) = build(workspace);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // No work yet: the pump publishes nothing and the shell is alive.
        assert!(engine.pump(&tx));
        assert_eq!(engine.cursor(), 0);

        block_on(engine.handle(SessionCommand::Submit {
            prompt: "hello".to_owned(),
        }));

        assert!(engine.pump(&tx), "the shell is still connected");
        let cursor = engine.cursor();
        assert!(cursor >= 1, "the task start must reach the shell");
        // A second pump forwards nothing new: no duplicated transcript.
        assert!(engine.pump(&tx));
        assert_eq!(engine.cursor(), cursor);

        let mut seen = 0;
        while rx.try_recv().is_ok() {
            seen += 1;
        }
        assert_eq!(seen, cursor);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn events_are_published_between_ticks_not_only_at_the_end() {
        // The bug this guards: the driver pumped once after the whole task,
        // so a long task showed an empty transcript while it ran. A shell
        // watching a task must see progress as it happens.
        let (workspace, root) = fixture_workspace();
        let (mut engine, _) = build(workspace);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let mut publications = 0usize;
        let mut seen_at_first_publication = 0usize;
        block_on(engine.handle_with(
            SessionCommand::Submit {
                prompt: "do something".to_owned(),
            },
            |engine| {
                engine.pump(&tx);
                publications += 1;
                if publications == 1 {
                    assert_eq!(engine.model_calls(), 0);
                    assert!(
                        !engine
                            .runtime
                            .events()
                            .events()
                            .any(|event| matches!(event, SessionEvent::Routed { .. }))
                    );
                    let mut count = 0;
                    // The receiver is drained inside the closure only
                    // to observe ordering; the real shell drains it on
                    // its own thread.
                    while rx.try_recv().is_ok() {
                        count += 1;
                    }
                    seen_at_first_publication = count;
                }
            },
        ));

        assert!(
            publications >= 1,
            "the driver must publish at least once per command"
        );
        assert!(
            seen_at_first_publication >= 1,
            "the first publication must already carry the task start, not wait for the end"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_closed_shell_stops_the_driver() {
        let (workspace, root) = fixture_workspace();
        let (mut engine, _) = build(workspace);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        // Nobody is watching: the pump reports it so the loop can stop
        // rather than drive work into the void.
        assert!(!engine.pump(&tx));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_submitted_task_always_reaches_a_terminal_or_waiting_state() {
        let (workspace, root) = fixture_workspace();
        let (mut engine, _) = build(workspace);
        block_on(engine.handle(SessionCommand::Submit {
            prompt: "fix the build".to_owned(),
        }));
        // Whatever the configuration, the shell never sits on a task that
        // silently did nothing: there is a state, and the pump carries the
        // events that explain it.
        let state = engine.task_state().expect("a task is active");
        assert!(
            state.is_terminal() || state == TaskState::Waiting,
            "unexpected state {state:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn action_and_source_labels_are_stable_and_readable() {
        assert_eq!(action_label(&Action::Discover), "discover");
        assert_eq!(action_label(&Action::AskUser), "ask");
        assert_eq!(
            action_label(&Action::Generate(ModelTier::Reasoner)),
            "generate·reasoner"
        );
        assert_eq!(
            action_label(&Action::Tool {
                capability: "files".to_owned()
            }),
            "tool·files"
        );
        assert_eq!(source_label(crate::DecisionSource::SystemZero), "sys0");
        assert_eq!(source_label(crate::DecisionSource::Cache), "cache");
        assert_eq!(source_label(crate::DecisionSource::SystemOne), "sys1");
    }

    #[test]
    fn a_cancelled_task_reaches_the_shell_as_cancelled() {
        let (workspace, root) = fixture_workspace();
        let (mut engine, _) = build(workspace);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        block_on(async {
            engine
                .handle(SessionCommand::Submit {
                    prompt: "do something long".to_owned(),
                })
                .await;
            // Whatever the submit did — completed, failed, or waiting — a
            // task that is still live can be cancelled, and the shell must
            // see exactly one cancellation event.
            if engine
                .task_state()
                .is_some_and(|state| !state.is_terminal())
            {
                engine.handle(SessionCommand::Cancel).await;
            }
        });
        engine.pump(&tx);

        let mut cancelled = false;
        let mut terminal = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                SessionEvent::TaskCancelled { .. } => cancelled = true,
                SessionEvent::TaskCompleted { .. } | SessionEvent::TaskFailed { .. } => {
                    terminal += 1
                }
                _ => {}
            }
        }
        if cancelled {
            assert_eq!(terminal, 0, "cancellation is one terminal outcome");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_command_the_runtime_refuses_is_reported_to_the_shell() {
        let (workspace, root) = fixture_workspace();
        let (mut engine, _) = build(workspace);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // No task is active: pausing cannot succeed, and silently doing
        // nothing would look like a broken key.
        block_on(engine.handle(SessionCommand::Pause));
        engine.pump(&tx);

        let mut reported = false;
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::RuntimeError { message, .. } = event {
                assert!(message.contains("pause"), "unhelpful message: {message}");
                reported = true;
            }
        }
        assert!(reported, "a refused command must be explained");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_run_loop_announces_the_session_and_stops_when_the_shell_leaves() {
        let (workspace, root) = fixture_workspace();
        let (engine, _) = build(workspace);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        block_on(async move {
            // Drop the command sender: the loop must exit rather than hang.
            drop(cmd_tx);
            run_engine(engine, cmd_rx, event_tx).await;
        });

        let first = event_rx.try_recv().expect("session start is announced");
        assert!(matches!(
            first,
            SessionEvent::SessionStarted { protocol_version }
                if protocol_version == crate::SESSION_PROTOCOL_VERSION
        ));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn event_delivery_survives_retained_history_rollover() {
        let mut engine = minimal_engine(None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for index in 0..crate::session::EVENT_LOG_CAPACITY + 20 {
            engine.runtime.emit_runtime_error(format!("event {index}"));
            assert!(engine.pump(&tx));
            assert!(
                matches!(rx.try_recv().unwrap(), SessionEvent::RuntimeError { message, .. } if message == format!("event {index}"))
            );
        }
        assert!(engine.runtime.events().dropped() > 0);
    }

    struct StalledStream;

    #[async_trait::async_trait]
    impl crate::Model for StalledStream {
        fn identity(&self) -> crate::ModelIdentity {
            crate::ModelIdentity {
                provider: "test".to_owned(),
                model: "stalled".to_owned(),
                tier: ModelTier::Reasoner,
            }
        }
        fn capabilities(&self) -> crate::ModelCapabilities {
            crate::ModelCapabilities {
                streaming: true,
                ..crate::ModelCapabilities::buffered_text()
            }
        }
        async fn complete(
            &self,
            _: &crate::ModelRequest,
        ) -> Result<crate::ModelResponse, KnutError> {
            std::future::pending().await
        }
        async fn stream(
            &self,
            _: &crate::ModelRequest,
            sink: &mut (dyn crate::ModelStreamSink + Send),
        ) -> Result<crate::ModelResponse, KnutError> {
            sink.on_event(crate::ModelStreamEvent::TextDelta {
                text: "first fragment".to_owned(),
            });
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn live_stream_reaches_client_and_cancel_interrupts_stalled_provider() {
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(StalledStream));
        let router = LiveRouter {
            live: None,
            fallback: StaticSystemOne::new(Decision {
                route: Route::Generate,
                confidence: 1.0,
                retrieval: None,
                capability: None,
                model_tier: ModelTier::Reasoner,
                risk: Risk::Low,
                parallelizable: false,
            }),
        };
        let runtime = SessionRuntime::new(
            Arc::new(Knut::new(router)),
            Planner::new(cascade.clone()),
            Arc::new(ToolRegistry::default()),
            Arc::new(ExecutionGate::new(SideEffectPolicy::new())),
            cascade,
            Arc::new(crate::AcceptAllVerifier),
        );
        let engine = Engine::new(runtime, None);
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = tokio::spawn(run_engine(engine, command_rx, events));
        commands
            .send(SessionCommand::Submit {
                prompt: "hello".to_owned(),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(event_rx.recv().await.unwrap(), SessionEvent::TextDelta { text, .. } if text == "first fragment") { break; }
            }
        }).await.expect("fragment was buffered behind stalled provider");
        commands.send(SessionCommand::Cancel).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    event_rx.recv().await.unwrap(),
                    SessionEvent::TaskCancelled { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("cancel waited for stalled provider");
        drop(commands);
        driver.await.unwrap();
        while let Ok(event) = event_rx.try_recv() {
            assert!(!matches!(
                event,
                SessionEvent::TaskCompleted { .. }
                    | SessionEvent::TaskFailed { .. }
                    | SessionEvent::TaskCancelled { .. }
            ));
        }
    }
}
