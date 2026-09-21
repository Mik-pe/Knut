//! One event-driven session runtime: commands in, ordered events out
//! (issue #18).
//!
//! A single `SessionRuntime` owns task state. CLI, TUI and future editor
//! adapters submit [`SessionCommand`]s and consume [`SessionEvent`]s;
//! none of them implements its own execution loop. The runtime wires the
//! existing abstractions — `Knut` routing (System 0 -> System One),
//! `Planner` (System Two), `TreeExecutor`, the mandatory `ExecutionGate`
//! and evidence-gated completion — instead of maintaining a second
//! orchestrator.
//!
//! Invariants:
//! - every tool invocation passes through the gate; nothing the model
//!   controls can bypass policy, approval or replay semantics;
//! - `Done` is only ever emitted when deterministic completion
//!   requirements are satisfied by evidence bound to the current
//!   revision (#16); uncertainty never becomes completion;
//! - events are published before awaiting more work; the bounded event
//!   log drops cosmetic stream updates before it ever drops approval
//!   requests, terminal state or artifact events;
//! - steering bumps the task revision, invalidating stale decisions;
//!   pause stops dispatch of new work; cancel propagates to in-flight
//!   work and produces exactly one terminal outcome.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::planner::{Planner, ValidatedPlan};
use crate::runtime::{DecisionSource, Knut, Routed};
use crate::tool::ToolRegistry;
use crate::tree::{CancelFlag, NodeStatus, PlanNode, TreeExecutor};
use crate::{
    Action, CompletionRequirements, Evidence, KnutError, ModelRequest, ModelTier, Risk, SystemOne,
    Verifier,
};

/// Protocol version for commands and events. Bump on breaking changes.
pub const SESSION_PROTOCOL_VERSION: u32 = 1;

/// Bounded event log capacity. When full, the oldest *droppable* events
/// (stream/cosmetic updates) are evicted first; critical events
/// (approvals, terminal states, artifacts) are never dropped.
pub const EVENT_LOG_CAPACITY: usize = 4096;

/// Upper bound on model turns per task within one session.
pub const MAX_TURNS_PER_TASK: usize = 8;

/// Upper bound on replans per task.
pub const MAX_REPLANS_PER_TASK: usize = 2;

/// A command submitted to the session runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionCommand {
    /// Submit a new task; becomes the active task.
    Submit { prompt: String },
    /// Change the active task's direction; bumps the task revision.
    Steer { prompt: String },
    /// Stop dispatching new work; in-flight work completes.
    Pause,
    /// Resume a paused task.
    Resume,
    /// Answer a pending user question.
    Answer { value: String },
    /// Approve one exact pending action by its approval key.
    Approve { approval_key: String },
    /// Deny one exact pending action by its approval key.
    Deny { approval_key: String },
    /// Cancel the active task; in-flight work is cancelled.
    Cancel,
    /// Mid-task input: steer the active task when it is a directive, or
    /// queue the request for the next task. The runtime owns the choice so
    /// a queued request never silently modifies the active task.
    SteerOrQueue { text: String },
}

/// Stable identifier for one task within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(pub u64);

/// Stable identifier for one turn (routing decision cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TurnId(pub u64);

/// Stable identifier for one node execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u64);

/// A monotonically increasing task revision: bumps on steering. Decisions
/// recorded against an older revision are stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TaskRevision(pub u64);

/// The state of the active task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Accepted, not yet working.
    Queued,
    /// Actively routing/executing.
    Running,
    /// Waiting for the user: a question or an approval.
    Waiting,
    /// Paused by the user; no new work dispatches.
    Paused,
    /// Finished successfully (evidence-gated).
    Completed,
    /// Terminal failure.
    Failed,
    /// Cancelled by the user; exactly one terminal outcome.
    Cancelled,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    }
}

/// Why the runtime is waiting on the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    /// A question only the user can answer.
    Question,
    /// An exact-action approval request (fingerprint shown).
    Approval { approval_key: String },
}

/// One ordered event on the shared session stream.
///
/// Events are published *before* awaiting more work; ordering within one
/// session is total and identifiers are stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Emitted once at runtime start.
    SessionStarted {
        protocol_version: u32,
    },

    TaskStarted {
        task: TaskId,
        prompt: String,
    },

    /// A steering command was accepted; subsequent stale decisions are
    /// invalid.
    TaskSteered {
        task: TaskId,
        revision: TaskRevision,
        prompt: String,
    },

    /// A routing decision was made (System 0 or System One).
    Routed {
        task: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        source: DecisionSource,
        action: Action,
        confidence: f32,
    },

    /// The reasoner is generating a plan or content.
    Generating {
        task: TaskId,
        turn: TurnId,
        revision: TaskRevision,
    },

    /// A validated plan is starting.
    PlanStarted {
        task: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        node_count: usize,
    },

    /// One node's execution outcome.
    NodeResult {
        task: TaskId,
        turn: TurnId,
        node: NodeId,
        node_label: String,
        status: NodeStatus,
        /// Bounded output summary; full artifacts stay in the tree run.
        output: Value,
    },

    /// A request was queued for the next task rather than applied to the
    /// running one.
    RequestQueued {
        text: String,
    },

    /// An action card began: the UI's view of one unit of work, with a
    /// stable id and an explicit lifecycle state.
    CardStarted {
        task: TaskId,
        turn: TurnId,
        card_id: String,
        title: String,
    },

    /// An action card finished, with its terminal state, a one-line
    /// summary, bounded detail and elapsed time. Cancellation and timeout
    /// are distinct from failure.
    CardFinished {
        task: TaskId,
        turn: TurnId,
        card_id: String,
        state: crate::cards::CardState,
        summary: String,
        detail: String,
        elapsed_ms: u64,
    },

    /// Incremental assistant text within one turn.
    ///
    /// Droppable: losing a delta must never lose the final artifact. What
    /// the user sees mid-turn is a stream; what the task acts on is the
    /// completed response published as a `NodeResult`.
    TextDelta {
        task: TaskId,
        turn: TurnId,
        node: NodeId,
        text: String,
    },

    /// A tool call the model requested, arguments already complete and
    /// parsed. Partial argument JSON is never published here: a partial
    /// stream is not an executable command.
    ToolCallProposed {
        task: TaskId,
        turn: TurnId,
        call_id: String,
        name: String,
        arguments: Value,
    },

    /// Edge decision after a meaningful node result.
    EdgeDecided {
        task: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        proposed: crate::EdgeChoice,
        effective: crate::EdgeChoice,
        overridden: bool,
    },

    /// The task needs the user before it can proceed.
    WaitingForUser {
        task: TaskId,
        turn: TurnId,
        wait: WaitKind,
        message: String,
    },

    /// An answer or approval resolved the wait.
    WaitResolved {
        task: TaskId,
        turn: TurnId,
    },

    Paused {
        task: TaskId,
    },
    Resumed {
        task: TaskId,
    },

    /// Terminal outcomes; exactly one per task.
    TaskCompleted {
        task: TaskId,
        summary: String,
    },
    TaskFailed {
        task: TaskId,
        reason: String,
    },
    TaskCancelled {
        task: TaskId,
    },

    /// A bounded System One decision was requested at a coding-loop
    /// boundary, with everything needed to audit it: which frame version
    /// and question pack, what was chosen, the raw answer distribution
    /// and whether the runtime had to override it.
    FrameDecided {
        task: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        question_kind: crate::FrameKind,
        frame_version: u32,
        question_pack: Vec<String>,
        choice: String,
        confidence: f64,
        distribution: Value,
        overridden: bool,
    },

    /// An unexpected runtime error that did not terminate the task.
    RuntimeError {
        task: Option<TaskId>,
        message: String,
    },
}

impl SessionEvent {
    /// Whether this event may be dropped when the bounded log is full.
    /// Critical events (approvals, terminal states, artifacts, routing)
    /// are never droppable.
    fn droppable(&self) -> bool {
        matches!(
            self,
            SessionEvent::RuntimeError { .. }
                | SessionEvent::TextDelta { .. }
                | SessionEvent::CardFinished { .. }
        )
    }

    /// The task an event belongs to, when it has one.
    pub fn task(&self) -> Option<TaskId> {
        match self {
            SessionEvent::SessionStarted { .. } => None,
            SessionEvent::RuntimeError { task, .. } => *task,
            SessionEvent::TaskStarted { task, .. }
            | SessionEvent::TaskSteered { task, .. }
            | SessionEvent::Routed { task, .. }
            | SessionEvent::Generating { task, .. }
            | SessionEvent::PlanStarted { task, .. }
            | SessionEvent::EdgeDecided { task, .. }
            | SessionEvent::WaitingForUser { task, .. }
            | SessionEvent::WaitResolved { task, .. }
            | SessionEvent::Paused { task, .. }
            | SessionEvent::Resumed { task, .. }
            | SessionEvent::TaskCompleted { task, .. }
            | SessionEvent::TaskFailed { task, .. }
            | SessionEvent::TaskCancelled { task } => Some(*task),
            SessionEvent::NodeResult { task, .. }
            | SessionEvent::TextDelta { task, .. }
            | SessionEvent::ToolCallProposed { task, .. }
            | SessionEvent::FrameDecided { task, .. }
            | SessionEvent::CardStarted { task, .. }
            | SessionEvent::CardFinished { task, .. } => Some(*task),
            SessionEvent::RequestQueued { .. } => None,
        }
    }
}

/// A bounded, ordered event log.
///
/// When capacity is exceeded, droppable events are evicted first
/// (oldest first). If no droppable events remain, the oldest event is
/// evicted — but that can only happen when the entire log is critical,
/// at which point dropping the oldest is still preferable to unbounded
/// growth, and consumers receive a `RuntimeError` noting the loss.
#[derive(Debug, Clone)]
pub struct EventLog {
    events: VecDeque<SessionEvent>,
    dropped: u64,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLog {
    pub fn new() -> Self {
        Self {
            events: VecDeque::with_capacity(EVENT_LOG_CAPACITY),
            dropped: 0,
        }
    }

    fn push(&mut self, event: SessionEvent) {
        if self.events.len() >= EVENT_LOG_CAPACITY {
            // Evict the oldest droppable event; otherwise the oldest.
            let victim = self
                .events
                .iter()
                .position(SessionEvent::droppable)
                .unwrap_or(0);
            self.events.remove(victim);
            self.dropped += 1;
        }
        self.events.push_back(event);
    }

    pub fn events(&self) -> impl Iterator<Item = &SessionEvent> {
        self.events.iter()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// How the session runtime resolves `Action::Discover` into a concrete
/// capability: bounded candidate discovery, never model-invented.
#[derive(Debug, Clone)]
pub struct DiscoveryCandidates {
    /// Capability id -> description, bounded by the caller.
    pub candidates: Vec<(String, String)>,
}

/// Outcome of one driver tick, internal bookkeeping.
enum Tick {
    /// The task reached a terminal state.
    Terminal(TaskState),
    /// The task is waiting on the user (question or approval).
    Waiting(WaitKind),
    /// Ready for another tick (work performed, task continues).
    Continue,
}

/// One event-driven session runtime over the shared Knut abstractions.
pub struct SessionRuntime<S> {
    router: Arc<Knut<S>>,
    planner: Planner,
    registry: Arc<ToolRegistry>,
    gate: Arc<crate::ExecutionGate>,
    cascade: Arc<crate::ComputeCascade>,
    verifier: Arc<dyn Verifier>,
    /// Completion contract: `Done` requires evidence-gated satisfaction.
    requirements: CompletionRequirements,
    /// Evidence accumulated for the current artifact revision.
    evidence: Vec<Evidence>,
    /// Current artifact revision identity, if any.
    artifact: Option<crate::ArtifactRevision>,
    /// How Discover resolves to capabilities.
    discovery: DiscoveryCandidates,
    /// Bounded System One decisions at coding-loop boundaries. `None`
    /// means no router is configured: discovery then asks the user
    /// rather than guessing.
    frames: Option<Arc<dyn crate::FrameRouter>>,

    // Session state.
    events: EventLog,
    task: Option<ActiveTask>,
    pending_wait: Option<PendingWait>,
    paused: bool,
    /// Queued steering prompts to fold into the next decision.
    next_turn: u64,
    next_node: u64,
    next_task: u64,
    cancel_flag: CancelFlag,
    ids: Arc<IdCounter>,
    model_calls: Arc<AtomicU64>,
}

struct IdCounter {
    task: AtomicU64,
    turn: AtomicU64,
    node: AtomicU64,
}

struct ActiveTask {
    id: TaskId,
    prompt: String,
    revision: TaskRevision,
    state: TaskState,
    turns: usize,
    replans: usize,
    /// Latest routing decision, invalidated on revision bump.
    routed: Option<(TaskRevision, TurnId)>,
    /// A validated plan paused on approval. Approving must resume this
    /// exact plan instead of asking the reasoner for a new one: the
    /// approval is bound to the actions inside it (steering bumps the
    /// revision and discards it, so a stale plan can never be resumed).
    pending_plan: Option<(TaskRevision, ValidatedPlan, String)>,
}

/// The pending wait a task is blocked on: what the user must resolve.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingWait {
    /// The task waiting.
    pub task: TaskId,
    /// The turn that produced the wait.
    pub turn: TurnId,
    /// Question or approval.
    pub kind: WaitKind,
    /// The message shown to the user.
    pub message: String,
}

impl<S> SessionRuntime<S>
where
    S: SystemOne,
{
    /// Assemble the runtime over shared abstractions. The gate is
    /// mandatory: there is no gate-less constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: Arc<Knut<S>>,
        planner: Planner,
        registry: Arc<ToolRegistry>,
        gate: Arc<crate::ExecutionGate>,
        cascade: Arc<crate::ComputeCascade>,
        verifier: Arc<dyn Verifier>,
    ) -> Self {
        let ids = Arc::new(IdCounter {
            task: AtomicU64::new(1),
            turn: AtomicU64::new(1),
            node: AtomicU64::new(1),
        });
        Self {
            router,
            planner,
            registry,
            gate,
            cascade,
            verifier,
            requirements: CompletionRequirements::none(),
            evidence: Vec::new(),
            artifact: None,
            discovery: DiscoveryCandidates {
                candidates: Vec::new(),
            },
            frames: None,
            events: EventLog::new(),
            task: None,
            pending_wait: None,
            paused: false,
            next_turn: 1,
            next_node: 1,
            next_task: 1,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            ids,
            model_calls: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Configure the completion contract for tasks in this session.
    pub fn with_requirements(mut self, requirements: CompletionRequirements) -> Self {
        self.requirements = requirements;
        self
    }

    /// Configure discovery candidates for `Action::Discover`.
    pub fn with_discovery(mut self, discovery: DiscoveryCandidates) -> Self {
        self.discovery = discovery;
        self
    }

    /// Configure the bounded System One router used at coding-loop
    /// decision points (candidate selection, failure classification,
    /// plan continuation).
    pub fn with_frames(mut self, router: Arc<dyn crate::FrameRouter>) -> Self {
        self.frames = Some(router);
        self
    }

    /// Bind completion checks to an explicit artifact revision.
    pub fn with_artifact_revision(mut self, revision: crate::ArtifactRevision) -> Self {
        self.artifact = Some(revision);
        self
    }

    /// Seed evidence for the current artifact revision (test/seam for
    /// evidence-gated completion; real verifiers arrive with #26).
    pub fn seed_evidence(&mut self, evidence: Evidence) {
        self.evidence.push(evidence);
    }

    pub fn set_artifact_revision(&mut self, artifact: crate::ArtifactRevision) {
        self.artifact = Some(artifact);
    }

    pub fn events(&self) -> &EventLog {
        &self.events
    }

    pub fn task_state(&self) -> Option<TaskState> {
        self.task.as_ref().map(|t| t.state)
    }

    pub fn pending_wait(&self) -> Option<&PendingWait> {
        self.pending_wait.as_ref()
    }

    /// Total model calls observed by this runtime (planner runs).
    pub fn model_calls(&self) -> u64 {
        self.model_calls.load(Ordering::SeqCst)
    }

    /// The compute cascade this runtime uses for generation.
    ///
    /// Exposed so a client can verify that a configured model is reachable
    /// from every tier a routed action may ask for, rather than
    /// discovering the gap only when a task fails.
    pub fn cascade(&self) -> &crate::ComputeCascade {
        self.cascade.as_ref()
    }

    fn emit(&mut self, event: SessionEvent) {
        self.events.push(event);
    }

    /// Publish a runtime error from outside the drive loop.
    ///
    /// Used by adapters that own a command path the runtime rejected: the
    /// user must see *why* a keystroke did nothing, and the event log is
    /// the only transcript every client reads.
    pub fn emit_runtime_error(&mut self, message: impl Into<String>) {
        let task = self.task.as_ref().map(|task| task.id);
        self.emit(SessionEvent::RuntimeError {
            task,
            message: message.into(),
        });
    }

    /// The revision identity a completion decision is bound to.
    ///
    /// An explicit artifact revision (set by the caller for the work in
    /// progress) always wins. Otherwise the subject is derived from the
    /// task revision and the step kind, and deliberately *not* from the
    /// turn id: every turn of an unchanged revision is checked against
    /// the same subject, so evidence seeded for that revision is
    /// applicable and fresh evidence legitimately permits completion.
    fn completion_subject(
        &self,
        task_id: TaskId,
        _turn: TurnId,
        revision: TaskRevision,
        kind: &str,
    ) -> crate::ArtifactRevision {
        self.artifact.clone().unwrap_or_else(|| {
            crate::ArtifactRevision::new(
                format!("{kind}:task:{}", task_id.0),
                format!("revision:{}", revision.0),
            )
        })
    }

    fn next_turn_id(&mut self) -> TurnId {
        let id = self.next_turn;
        self.next_turn += 1;
        self.ids.turn.fetch_add(1, Ordering::Relaxed);
        TurnId(id)
    }

    fn next_node_id(&mut self) -> NodeId {
        let id = self.next_node;
        self.next_node += 1;
        self.ids.node.fetch_add(1, Ordering::Relaxed);
        NodeId(id)
    }

    /// Submit a command. Returns an error only for structurally invalid
    /// commands (empty prompts, unknown approval keys); everything else
    /// is accepted and reflected in the event stream.
    pub async fn command(&mut self, command: SessionCommand) -> Result<(), KnutError> {
        match command {
            SessionCommand::Submit { prompt } => {
                if prompt.trim().is_empty() {
                    return Err(KnutError::InvalidArguments {
                        path: "prompt".to_owned(),
                        reason: "task prompt must not be empty".to_owned(),
                    });
                }
                if let Some(task) = &self.task
                    && !task.state.is_terminal()
                {
                    return Err(KnutError::InvalidArguments {
                        path: "task".to_owned(),
                        reason: "a task is already active; cancel or complete it first".to_owned(),
                    });
                }
                let id = TaskId(self.next_task);
                self.next_task += 1;
                self.ids.task.fetch_add(1, Ordering::Relaxed);
                self.cancel_flag.store(false, Ordering::SeqCst);
                self.paused = false;
                self.pending_wait = None;
                self.task = Some(ActiveTask {
                    id,
                    prompt: prompt.clone(),
                    revision: TaskRevision(1),
                    state: TaskState::Queued,
                    turns: 0,
                    replans: 0,
                    routed: None,
                    pending_plan: None,
                });
                self.emit(SessionEvent::TaskStarted { task: id, prompt });
                Ok(())
            }
            SessionCommand::Steer { prompt } => {
                if prompt.trim().is_empty() {
                    return Err(KnutError::InvalidArguments {
                        path: "prompt".to_owned(),
                        reason: "steering prompt must not be empty".to_owned(),
                    });
                }
                let (task_id, is_terminal) = {
                    let Some(task) = self.task.as_mut() else {
                        return Err(KnutError::InvalidArguments {
                            path: "task".to_owned(),
                            reason: "no active task to steer".to_owned(),
                        });
                    };
                    if task.state.is_terminal() {
                        return Err(KnutError::InvalidArguments {
                            path: "task".to_owned(),
                            reason: "task is already terminal".to_owned(),
                        });
                    }
                    (task.id, false)
                };
                let _ = is_terminal;
                let last_turn = self
                    .task
                    .as_ref()
                    .and_then(|t| t.routed)
                    .map(|(_, t)| t)
                    .unwrap_or(TurnId(0));
                let was_waiting = self.pending_wait.is_some();
                self.pending_wait = None;
                if was_waiting {
                    self.emit(SessionEvent::WaitResolved {
                        task: task_id,
                        turn: last_turn,
                    });
                }
                if let Some(task) = self.task.as_mut() {
                    task.revision = TaskRevision(task.revision.0 + 1);
                    task.prompt = prompt.clone();
                    // The steered revision invalidates any plan awaiting
                    // approval: those actions were chosen for old intent.
                    task.pending_plan = None;
                }
                let revision = self
                    .task
                    .as_ref()
                    .map(|t| t.revision)
                    .unwrap_or(TaskRevision(1));
                self.emit(SessionEvent::TaskSteered {
                    task: task_id,
                    revision,
                    prompt,
                });
                Ok(())
            }
            SessionCommand::Pause => {
                let Some(task) = self.task.as_mut() else {
                    return Err(KnutError::InvalidArguments {
                        path: "task".to_owned(),
                        reason: "no active task to pause".to_owned(),
                    });
                };
                if task.state.is_terminal() {
                    return Err(KnutError::InvalidArguments {
                        path: "task".to_owned(),
                        reason: "task is already terminal".to_owned(),
                    });
                }
                self.paused = true;
                if task.state == TaskState::Running || task.state == TaskState::Queued {
                    task.state = TaskState::Paused;
                }
                let task_id = task.id;
                self.emit(SessionEvent::Paused { task: task_id });
                Ok(())
            }
            SessionCommand::Resume => {
                let Some(task) = self.task.as_mut() else {
                    return Err(KnutError::InvalidArguments {
                        path: "task".to_owned(),
                        reason: "no active task to resume".to_owned(),
                    });
                };
                if task.state.is_terminal() {
                    return Err(KnutError::InvalidArguments {
                        path: "task".to_owned(),
                        reason: "task is already terminal".to_owned(),
                    });
                }
                self.paused = false;
                if task.state == TaskState::Paused {
                    task.state = TaskState::Queued;
                }
                let task_id = task.id;
                self.emit(SessionEvent::Resumed { task: task_id });
                Ok(())
            }
            SessionCommand::Answer { value } => {
                let Some(wait) = self.pending_wait.take() else {
                    return Err(KnutError::InvalidArguments {
                        path: "answer".to_owned(),
                        reason: "no pending question to answer".to_owned(),
                    });
                };
                match wait.kind {
                    WaitKind::Question => {
                        // Fold the answer into the task prompt and
                        // continue with a fresh decision.
                        if let Some(task) = self.task.as_mut() {
                            task.prompt = format!("{}\nuser: {value}", task.prompt);
                            task.revision = TaskRevision(task.revision.0 + 1);
                        }
                        self.emit(SessionEvent::WaitResolved {
                            task: wait.task,
                            turn: wait.turn,
                        });
                        Ok(())
                    }
                    WaitKind::Approval { .. } => Err(KnutError::InvalidArguments {
                        path: "answer".to_owned(),
                        reason: "pending wait is an approval, not a question; use approve/deny"
                            .to_owned(),
                    }),
                }
            }
            SessionCommand::Approve { approval_key } => {
                let Some(wait) = self.pending_wait.as_ref() else {
                    return Err(KnutError::InvalidArguments {
                        path: "approve".to_owned(),
                        reason: "no pending approval".to_owned(),
                    });
                };
                let WaitKind::Approval { approval_key: key } = &wait.kind else {
                    return Err(KnutError::InvalidArguments {
                        path: "approve".to_owned(),
                        reason: "pending wait is a question, not an approval".to_owned(),
                    });
                };
                if *key != approval_key {
                    return Err(KnutError::InvalidArguments {
                        path: "approve".to_owned(),
                        reason: "approval key does not match the pending action".to_owned(),
                    });
                }
                let wait = self.pending_wait.take().expect("checked above");
                self.gate.grant_approval(&approval_key);
                self.emit(SessionEvent::WaitResolved {
                    task: wait.task,
                    turn: wait.turn,
                });
                Ok(())
            }
            SessionCommand::Deny { approval_key } => {
                let Some(wait) = self.pending_wait.as_ref() else {
                    return Err(KnutError::InvalidArguments {
                        path: "deny".to_owned(),
                        reason: "no pending approval".to_owned(),
                    });
                };
                let WaitKind::Approval { approval_key: key } = &wait.kind else {
                    return Err(KnutError::InvalidArguments {
                        path: "deny".to_owned(),
                        reason: "pending wait is a question, not an approval".to_owned(),
                    });
                };
                if *key != approval_key {
                    return Err(KnutError::InvalidArguments {
                        path: "deny".to_owned(),
                        reason: "approval key does not match the pending action".to_owned(),
                    });
                }
                let task = wait.task;
                let turn = wait.turn;
                self.pending_wait = None;
                self.emit(SessionEvent::WaitResolved { task, turn });
                // Denial fails the task: the gate will keep refusing the
                // exact action; ask the reasoner is not a safe bypass.
                if let Some(task_state) = self.task.as_mut() {
                    task_state.state = TaskState::Failed;
                }
                self.emit(SessionEvent::TaskFailed {
                    task,
                    reason: "user denied the required approval".to_owned(),
                });
                Ok(())
            }
            SessionCommand::SteerOrQueue { text } => {
                // Mid-task input: a short directive steers the active
                // task; anything else is a *new* request, which must not
                // silently modify the running one.
                let (active, short) = {
                    let Some(task) = self.task.as_ref() else {
                        return Err(KnutError::InvalidArguments {
                            path: "task".to_owned(),
                            reason: "no active task to steer or queue for".to_owned(),
                        });
                    };
                    if task.state.is_terminal() {
                        return Err(KnutError::InvalidArguments {
                            path: "task".to_owned(),
                            reason: "task is already terminal".to_owned(),
                        });
                    }
                    (true, is_steering_directive(&text))
                };

                if active && short {
                    // Steering bumps the revision, which invalidates any
                    // plan or approval bound to the older one. This is the
                    // same body as `Steer`, inlined to keep `command` free
                    // of async recursion.
                    let task_id = self.task.as_ref().map(|task| task.id);
                    let last_turn = self
                        .task
                        .as_ref()
                        .and_then(|task| task.routed)
                        .map(|(_, turn)| turn)
                        .unwrap_or(TurnId(0));
                    let was_waiting = self.pending_wait.is_some();
                    self.pending_wait = None;
                    if let Some(task_id) = task_id
                        && was_waiting
                    {
                        self.emit(SessionEvent::WaitResolved {
                            task: task_id,
                            turn: last_turn,
                        });
                    }
                    let revision = {
                        let Some(task) = self.task.as_mut() else {
                            return Ok(());
                        };
                        task.revision = TaskRevision(task.revision.0 + 1);
                        task.prompt = text.clone();
                        task.pending_plan = None;
                        task.revision
                    };
                    if let Some(task_id) = task_id {
                        self.emit(SessionEvent::TaskSteered {
                            task: task_id,
                            revision,
                            prompt: text,
                        });
                    }
                    return Ok(());
                }

                // Otherwise it is the next request, held for later.
                self.emit(SessionEvent::RequestQueued { text });
                Ok(())
            }
            SessionCommand::Cancel => {
                let Some(task) = self.task.as_ref() else {
                    return Err(KnutError::InvalidArguments {
                        path: "task".to_owned(),
                        reason: "no active task to cancel".to_owned(),
                    });
                };
                if task.state.is_terminal() {
                    return Err(KnutError::InvalidArguments {
                        path: "task".to_owned(),
                        reason: "task is already terminal".to_owned(),
                    });
                }
                // Signal in-flight work, then emit exactly one terminal
                // outcome on the next tick.
                self.cancel_flag.store(true, Ordering::SeqCst);
                let task_id = task.id;
                if let Some(task) = self.task.as_mut() {
                    task.state = TaskState::Cancelled;
                }
                self.emit(SessionEvent::TaskCancelled { task: task_id });
                Ok(())
            }
        }
    }

    /// Drive the session forward until it needs input or reaches a
    /// terminal state. Returns the active task state.
    ///
    /// Each tick performs at most one routing decision and one unit of
    /// work; calling `drive` again continues. This is the single owner
    /// of the execution loop — adapters never implement their own.
    pub async fn drive(&mut self) -> Option<TaskState> {
        // Cancelled tasks stop here: one terminal outcome, no queued
        // writes after cancellation.
        if self
            .task
            .as_ref()
            .is_some_and(|task| task.state == TaskState::Cancelled)
        {
            return Some(TaskState::Cancelled);
        }
        if self.cancel_flag.load(Ordering::SeqCst) {
            if let Some(task) = self.task.as_mut()
                && !task.state.is_terminal()
            {
                task.state = TaskState::Cancelled;
                let id = task.id;
                self.emit(SessionEvent::TaskCancelled { task: id });
            }
            self.cancel_flag.store(false, Ordering::SeqCst);
            return Some(TaskState::Cancelled);
        }

        if self.paused {
            return self.task.as_ref().map(|t| t.state);
        }
        if self.pending_wait.is_some() {
            return self.task.as_ref().map(|t| t.state);
        }
        let task = self.task.as_ref()?;
        if task.state.is_terminal() {
            return Some(task.state);
        }
        if task.turns >= MAX_TURNS_PER_TASK {
            let id = task.id;
            let reason = format!("turn budget of {MAX_TURNS_PER_TASK} exhausted");
            if let Some(task) = self.task.as_mut() {
                task.state = TaskState::Failed;
            }
            self.emit(SessionEvent::TaskFailed { task: id, reason });
            return Some(TaskState::Failed);
        }

        let task_id = task.id;
        let revision = task.revision;
        let prompt = task.prompt.clone();

        if let Some(task) = self.task.as_mut() {
            task.turns += 1;
        }
        let turn = self.next_turn_id();

        let tick = self.tick(task_id, turn, revision, &prompt).await;
        let waiting_kind = match &tick {
            Tick::Waiting(kind) => Some(kind.clone()),
            _ => None,
        };
        if let Some(task) = self.task.as_mut()
            && !task.state.is_terminal()
        {
            match tick {
                Tick::Terminal(state) => task.state = state,
                Tick::Waiting(_) => task.state = TaskState::Waiting,
                Tick::Continue => task.state = TaskState::Running,
            }
        }
        // Record the wait for future commands.
        if let Some(kind) = waiting_kind {
            let message = match &kind {
                WaitKind::Question => "The task needs information only you can provide.".to_owned(),
                WaitKind::Approval { .. } => {
                    "The task needs your approval before proceeding.".to_owned()
                }
            };
            self.pending_wait = Some(PendingWait {
                task: task_id,
                turn,
                kind,
                message,
            });
        }

        self.task.as_ref().map(|t| t.state)
    }

    /// Ask the user to choose among real candidates, naming why we are
    /// asking. This is the safe transition when a bounded decision is
    /// unavailable, declined or unusable — never a guessed capability.
    fn escalate_to_user(
        &mut self,
        task_id: TaskId,
        turn: TurnId,
        candidates: &[(String, String)],
        reason: &str,
    ) -> Tick {
        if candidates.is_empty() {
            self.emit(SessionEvent::WaitingForUser {
                task: task_id,
                turn,
                wait: WaitKind::Question,
                message: format!("{reason}; no capability candidates are available."),
            });
        } else {
            self.emit(SessionEvent::WaitingForUser {
                task: task_id,
                turn,
                wait: WaitKind::Question,
                message: format!(
                    "{reason}. Which capability should act? {}",
                    candidates
                        .iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        Tick::Waiting(WaitKind::Question)
    }

    /// One unit of work: route, then execute the routed action.
    async fn tick(
        &mut self,
        task_id: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        prompt: &str,
    ) -> Tick {
        // Ingress: System 0 fast paths, then System One judgment.
        let input = crate::DecisionInput::new(prompt.to_owned(), self.registry.capabilities());
        let routed: Routed = match self.router.route(&input).await {
            Ok(routed) => routed,
            Err(err) => {
                // Routing failures (blocked prompts, transport errors)
                // surface as an explicit failure, never silent progress.
                let reason = err.to_string();
                if matches!(err, KnutError::ApprovalRequired { .. }) {
                    // Should not happen at ingress, but handle honestly.
                    return Tick::Waiting(WaitKind::Approval {
                        approval_key: String::new(),
                    });
                }
                self.emit(SessionEvent::TaskFailed {
                    task: task_id,
                    reason,
                });
                return Tick::Terminal(TaskState::Failed);
            }
        };

        self.emit(SessionEvent::Routed {
            task: task_id,
            turn,
            revision,
            source: routed.source,
            action: routed.action.clone(),
            confidence: routed.decision.confidence,
        });

        match routed.action {
            Action::AskUser => {
                self.emit(SessionEvent::WaitingForUser {
                    task: task_id,
                    turn,
                    wait: WaitKind::Question,
                    message: "Information is missing that only you can supply.".to_owned(),
                });
                Tick::Waiting(WaitKind::Question)
            }
            Action::Retrieve(_) => {
                // Retrieval is a context-gathering step: fold a bounded
                // note into the prompt and continue. Real workspace tools
                // arrive with #23; the session contract is fixed here.
                let retrieved = format!("{prompt}\n[retrieval completed]");
                if let Some(task) = self.task.as_mut() {
                    task.prompt = retrieved;
                }
                Tick::Continue
            }
            Action::Discover => {
                // Explicit discovery: resolve against bounded candidates.
                // No candidates means ask the user; a single candidate is
                // mechanical; multiple candidates select deterministically
                // by exact prompt match, else ask.
                let candidates: Vec<(String, String)> = self.discovery.candidates.clone();
                let candidates = &candidates;
                match candidates.len() {
                    0 => {
                        self.emit(SessionEvent::WaitingForUser {
                            task: task_id,
                            turn,
                            wait: WaitKind::Question,
                            message: "No capability candidates found; which capability should act?"
                                .to_owned(),
                        });
                        Tick::Waiting(WaitKind::Question)
                    }
                    1 => {
                        // Mechanical singleton: proceed to that capability.
                        let capability = candidates[0].0.clone();
                        self.execute_capability(task_id, turn, revision, &capability)
                            .await
                    }
                    _ => {
                        let exact = candidates
                            .iter()
                            .find(|(id, _)| id == prompt.trim())
                            .map(|(id, _)| id.clone());
                        match exact {
                            Some(capability) => {
                                self.execute_capability(task_id, turn, revision, &capability)
                                    .await
                            }
                            None => {
                                // More than one legal choice: exactly the
                                // case System One exists for. Ask the
                                // frame router over a bounded, versioned
                                // frame, and fall back to asking the user
                                // when the answer is unusable.
                                let frame = crate::DecisionFrame::new(
                                    crate::FrameKind::CandidateSelection,
                                    task_id.0,
                                    revision.0,
                                    prompt.to_owned(),
                                )
                                .with_capabilities(self.registry.capabilities())
                                .with_candidates(
                                    candidates.iter().map(|(id, description)| {
                                        crate::Candidate::new(id.clone(), description.clone())
                                    }),
                                );

                                let decision = match &self.frames {
                                    Some(router) => {
                                        crate::decide_candidate(router.as_ref(), &frame).await
                                    }
                                    None => Ok(crate::CandidateChoice {
                                        id: crate::ESCALATE_ID.to_owned(),
                                        escalate: true,
                                        distribution: Default::default(),
                                        confidence: 0.0,
                                    }),
                                };

                                // Record what was asked and answered, with
                                // the raw distribution preserved.
                                self.emit(SessionEvent::FrameDecided {
                                    task: task_id,
                                    turn,
                                    revision,
                                    question_kind: crate::FrameKind::CandidateSelection,
                                    frame_version: frame.version,
                                    question_pack: frame.questions().keys().cloned().collect(),
                                    choice: decision
                                        .as_ref()
                                        .map(|d| d.id.clone())
                                        .unwrap_or_else(|_| "unavailable".to_owned()),
                                    confidence: decision
                                        .as_ref()
                                        .map(|d| d.confidence)
                                        .unwrap_or(0.0),
                                    distribution: decision
                                        .as_ref()
                                        .map(|d| {
                                            serde_json::to_value(&d.distribution)
                                                .unwrap_or(Value::Null)
                                        })
                                        .unwrap_or(Value::Null),
                                    overridden: decision.is_err(),
                                });

                                match decision {
                                    // A validated, offered candidate:
                                    // dispatch it (the gate still decides
                                    // whether the action may run).
                                    Ok(choice) => match choice.chosen() {
                                        Some(capability) => {
                                            let capability = capability.to_owned();
                                            self.execute_capability(
                                                task_id,
                                                turn,
                                                revision,
                                                &capability,
                                            )
                                            .await
                                        }
                                        // Escalation is a safe transition,
                                        // never a guessed capability.
                                        None => self.escalate_to_user(
                                            task_id,
                                            turn,
                                            candidates,
                                            "the bounded router declined the candidate set",
                                        ),
                                    },
                                    // Transport/protocol failure or no
                                    // router configured: ask, with the
                                    // real candidates listed.
                                    Err(err) => {
                                        eprintln!("DEBUG candidate decision error: {err}");
                                        self.escalate_to_user(
                                            task_id,
                                            turn,
                                            candidates,
                                            "routing the candidate set failed",
                                        )
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Action::Tool { capability } => {
                self.execute_capability(task_id, turn, revision, &capability)
                    .await
            }
            Action::Generate(tier) => self.generate(task_id, turn, revision, prompt, tier).await,
        }
    }

    /// Execute a capability through the planner -> validated plan ->
    /// gated tree executor path.
    async fn execute_capability(
        &mut self,
        task_id: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        capability: &str,
    ) -> Tick {
        // Resuming an approved plan: the user approved the exact actions
        // inside this validated plan, so re-asking the reasoner would
        // silently change what was approved. Reuse it verbatim.
        let resumed = self
            .task
            .as_mut()
            .and_then(|task| match task.pending_plan.take() {
                Some((plan_revision, plan, plan_capability))
                    if plan_revision == revision && plan_capability == capability =>
                {
                    Some(plan)
                }
                _ => None,
            });

        let validated = match resumed {
            Some(validated) => validated,
            None => match self.plan_for(task_id, turn, revision, capability).await {
                Some(validated) => validated,
                None => return Tick::Terminal(TaskState::Failed),
            },
        };

        self.emit(SessionEvent::PlanStarted {
            task: task_id,
            turn,
            revision,
            node_count: validated.node_count,
        });

        // Execute the plan through the gated tree executor.
        let executor = TreeExecutor::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.gate),
            Arc::clone(&self.cascade),
            Arc::clone(&self.verifier),
        );

        let run = match executor
            .run(&validated, Arc::clone(&self.cancel_flag))
            .await
        {
            Ok(run) => run,
            Err(err) => {
                let reason = err.to_string();
                self.emit(SessionEvent::TaskFailed {
                    task: task_id,
                    reason,
                });
                return Tick::Terminal(TaskState::Failed);
            }
        };

        // Publish node results.
        for (label, status) in &run.statuses {
            let node = self.next_node_id();
            let output = run.outputs.get(label).cloned().unwrap_or(Value::Null);
            self.emit(SessionEvent::NodeResult {
                task: task_id,
                turn,
                node,
                node_label: label.clone(),
                status: *status,
                output,
            });
        }

        if run.cancelled {
            self.emit(SessionEvent::TaskCancelled { task: task_id });
            return Tick::Terminal(TaskState::Cancelled);
        }

        // A blocked node means policy/approval/unavailability refused
        // work. Extract the approval key when present to surface the
        // exact pending action to the user.
        let blocked: Vec<String> = run
            .statuses
            .iter()
            .filter(|(_, s)| **s == NodeStatus::Blocked)
            .map(|(label, _)| label.clone())
            .collect();

        if !blocked.is_empty() {
            // Approval-required is the recoverable block: ask the user.
            // The approval binds to the exact action fingerprint in the
            // gate, and the validated plan is retained so approval resumes
            // this plan instead of generating a different one.
            let approval_key = self.extract_approval_key(&validated.plan).await;

            match approval_key {
                Some(key) => {
                    if let Some(task) = self.task.as_mut() {
                        task.pending_plan =
                            Some((revision, validated.clone(), capability.to_owned()));
                    }
                    self.emit(SessionEvent::WaitingForUser {
                        task: task_id,
                        turn,
                        wait: WaitKind::Approval {
                            approval_key: key.clone(),
                        },
                        message: format!(
                            "The action {:?} requires your approval.",
                            blocked.join(", ")
                        ),
                    });
                    Tick::Waiting(WaitKind::Approval { approval_key: key })
                }
                None => {
                    let reason = format!("blocked: {}", blocked.join(", "));
                    self.emit(SessionEvent::TaskFailed {
                        task: task_id,
                        reason,
                    });
                    Tick::Terminal(TaskState::Failed)
                }
            }
        } else if run.statuses.values().all(|s| *s == NodeStatus::Succeeded) {
            // Plan succeeded: completion is checked against a revision
            // derived from the plan actually executed, so evidence bound
            // to an earlier revision can never leak into this decision.
            let subject = self.completion_subject(task_id, turn, revision, "plan");
            let done_permitted = self.requirements.satisfied(&self.evidence, &subject);

            self.emit(SessionEvent::EdgeDecided {
                task: task_id,
                turn,
                revision,
                proposed: if done_permitted {
                    crate::EdgeChoice::Done
                } else {
                    crate::EdgeChoice::Continue
                },
                effective: if done_permitted {
                    crate::EdgeChoice::Done
                } else {
                    crate::EdgeChoice::Continue
                },
                overridden: false,
            });

            if done_permitted {
                let summary = format!(
                    "plan completed: {} of {} nodes succeeded",
                    run.statuses.len(),
                    validated.node_count
                );
                self.emit(SessionEvent::TaskCompleted {
                    task: task_id,
                    summary,
                });
                Tick::Terminal(TaskState::Completed)
            } else {
                // Succeeded but not complete. Outstanding requirements
                // mean more work (or more evidence) is needed: continue
                // to the next decision, never fabricate completion. The
                // turn budget in `drive` bounds the loop, so this cannot
                // spin forever.
                if let Some(task) = self.task.as_mut()
                    && task.replans < MAX_REPLANS_PER_TASK
                {
                    task.replans += 1;
                    let missing = self.requirements.missing(&self.evidence, &subject);
                    task.prompt = format!(
                        "{}\n[plan executed successfully; completion evidence still required: {}]",
                        task.prompt,
                        missing.join("; ")
                    );
                }
                Tick::Continue
            }
        } else {
            // Some node failed. A *verification/schema* failure is the
            // case that can be repaired: the plan's shape was legal, the
            // artifact was not. Feed the exact failing nodes back to the
            // reasoner under a bounded budget; a failed tool execution or
            // an exhausted budget is a terminal failure instead.
            let failed: Vec<String> = run
                .statuses
                .iter()
                .filter(|(_, s)| **s == NodeStatus::Failed)
                .map(|(label, _)| label.clone())
                .collect();

            // Composite nodes only report the failure their children had,
            // so repairability is decided by the failing *leaves*.
            let failed_leaves: Vec<String> = failed
                .iter()
                .filter(|label| is_leaf_node(&validated.plan, label))
                .cloned()
                .collect();
            let repairable = !failed_leaves.is_empty()
                && failed_leaves
                    .iter()
                    .all(|label| is_verification_failure(&validated.plan, label));
            let budget_left = self
                .task
                .as_ref()
                .is_some_and(|task| task.replans < MAX_REPLANS_PER_TASK);

            // A meaningful observation boundary: something failed and the
            // next step is a real choice. Ask System One to classify the
            // failure over a bounded frame; the runtime still owns the
            // decision rules and the budgets.
            let failure_class = self
                .classify_failure(
                    task_id,
                    turn,
                    revision,
                    capability,
                    &failed_leaves,
                    repairable,
                )
                .await;

            if repairable
                && budget_left
                && let Some(task) = self.task.as_mut()
            {
                task.replans += 1;
                // Carry the concrete failure into the next planning round
                // ("carry failed artifacts and verification feedback into
                // bounded repair"): the reasoner sees which node failed.
                task.prompt = format!(
                    "{}\n[failed: {} ({failure_class}); the plan executed but these checks did not pass]",
                    task.prompt,
                    failed_leaves.join(", ")
                );
                return Tick::Continue;
            }

            let reason = format!("nodes failed: {}", failed.join(", "));
            self.emit(SessionEvent::TaskFailed {
                task: task_id,
                reason,
            });
            Tick::Terminal(TaskState::Failed)
        }
    }

    /// Classify an observed failure through a bounded recovery frame.
    ///
    /// Returns a short label for the repair prompt and the audit trail.
    /// The classification is *advisory*: it never grants permission and
    /// never converts a failure into success. When no router is
    /// configured, or the answer is unusable, the runtime falls back to
    /// its own deterministic classification.
    async fn classify_failure(
        &mut self,
        task_id: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        capability: &str,
        failed_leaves: &[String],
        repairable: bool,
    ) -> String {
        // Deterministic default: schema/artifact checks that failed can
        // be repaired; anything else cannot.
        let fallback = if repairable {
            "verification"
        } else {
            "failed_effect"
        }
        .to_owned();

        let Some(router) = self.frames.clone() else {
            return fallback;
        };

        let goal = self
            .task
            .as_ref()
            .map(|task| task.prompt.clone())
            .unwrap_or_default();
        let frame =
            crate::DecisionFrame::new(crate::FrameKind::Recovery, task_id.0, revision.0, goal)
                .with_unit(capability)
                .with_capabilities(self.registry.capabilities())
                .with_observation(serde_json::json!({
                    "failed_nodes": failed_leaves,
                    "repairable_as_check": repairable,
                }))
                .with_remaining(["the failed checks must pass for this task to complete"]);

        let (choice, confidence, distribution, overridden) = match router.ask(&frame).await {
            Ok(response) => {
                match crate::typesafe::answer_choice(&response, "failure_class") {
                    Ok((choice, distribution, confidence)) => {
                        // An unoffered class is unusable: keep the
                        // deterministic answer rather than trusting it.
                        let offered = [
                            "transient",
                            "verification",
                            "wrong_approach",
                            "blocked_by_policy",
                            "unknown",
                        ];
                        if offered.contains(&choice.as_str()) {
                            (choice, confidence, distribution, false)
                        } else {
                            (fallback.clone(), 0.0, Default::default(), true)
                        }
                    }
                    Err(_) => (fallback.clone(), 0.0, Default::default(), true),
                }
            }
            Err(_) => (fallback.clone(), 0.0, Default::default(), true),
        };

        self.emit(SessionEvent::FrameDecided {
            task: task_id,
            turn,
            revision,
            question_kind: crate::FrameKind::Recovery,
            frame_version: frame.version,
            question_pack: frame.questions().keys().cloned().collect(),
            choice: choice.clone(),
            confidence,
            distribution: serde_json::to_value(&distribution).unwrap_or(Value::Null),
            overridden,
        });

        choice
    }

    /// Ask System Two for a validated plan for `capability`, emitting the
    /// standard plan lifecycle events. Returns `None` after emitting the
    /// explicit failure for a rejected plan.
    async fn plan_for(
        &mut self,
        task_id: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        capability: &str,
    ) -> Option<ValidatedPlan> {
        let context = crate::planner::PlanningContext::from_registry(
            format!(
                "Use the {capability} capability to make progress on: {}",
                self.task
                    .as_ref()
                    .map(|t| t.prompt.clone())
                    .unwrap_or_default()
            ),
            &self.registry,
        );

        self.emit(SessionEvent::Generating {
            task: task_id,
            turn,
            revision,
        });

        let plan_result = self
            .planner
            .plan(
                &context,
                &self.registry,
                &[ModelTier::Fast, ModelTier::Standard, ModelTier::Reasoner],
                self.verifier.as_ref(),
            )
            .await;

        match plan_result {
            Ok(validated) => {
                self.model_calls.fetch_add(1, Ordering::SeqCst);
                Some(validated)
            }
            Err(err) => {
                let reason = err.to_string();
                self.emit(SessionEvent::TaskFailed {
                    task: task_id,
                    reason,
                });
                None
            }
        }
    }

    /// Attempt to extract an approval key from a blocked plan by probing
    /// the first tool node's invocation against the gate without
    /// executing it. Returns `None` when the block is not an approval.
    async fn extract_approval_key(&self, plan: &PlanNode) -> Option<String> {
        let (capability, tool_id, input) = find_first_tool_node(plan)?;
        let metadata = self.registry.find_exact(capability, tool_id).ok()?;

        match self.gate.authorize(&metadata, input, None, Risk::Low).await {
            // Authorized now: the block came from something else.
            Ok(_) => None,
            // The exact pending action: surface its fingerprint.
            Err(KnutError::ApprovalRequired { approval_key, .. }) => Some(approval_key),
            Err(_) => None,
        }
    }

    /// Generation path: run the model through the cascade.
    async fn generate(
        &mut self,
        task_id: TaskId,
        turn: TurnId,
        revision: TaskRevision,
        prompt: &str,
        tier: ModelTier,
    ) -> Tick {
        self.emit(SessionEvent::Generating {
            task: task_id,
            turn,
            revision,
        });

        let request = ModelRequest::new(prompt.to_owned(), crate::ExpectedArtifact::Text);

        // Stream the turn through the cascade: incremental text is
        // published as it arrives, while the task only ever acts on the
        // *completed* response. Events are emitted from a sink that
        // copies them into the runtime's own log, so ordering stays in
        // the runtime's hands.
        let node = self.next_node_id();
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let mut sink = SessionSink {
            task: task_id,
            turn,
            node,
            events: Arc::clone(&events),
        };
        let outcome = self
            .cascade
            .run_streaming(&request, tier, self.verifier.as_ref(), &mut sink)
            .await;

        for event in events.lock().expect("sink lock").drain(..) {
            self.emit(event);
        }

        match outcome {
            Ok(outcome) => {
                self.model_calls.fetch_add(1, Ordering::SeqCst);
                let content = outcome.response.content;

                // Tool calls the model requested are surfaced as proposals
                // for *bounded* dispatch; arguments are already complete
                // and parsed here, never a partial JSON fragment.
                for call in &outcome.response.tool_calls {
                    self.emit(SessionEvent::ToolCallProposed {
                        task: task_id,
                        turn,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                }

                // Generation satisfies the turn only when completion is
                // permitted; an *unmet* deterministic requirement keeps
                // the task running (further turns, evidence, or the turn
                // budget's explicit failure) instead of ending the task
                // with a misleading "plan succeeded" failure.
                let subject = self.completion_subject(task_id, turn, revision, "generate");
                let done_permitted = self.requirements.satisfied(&self.evidence, &subject);

                if done_permitted {
                    self.emit(SessionEvent::TaskCompleted {
                        task: task_id,
                        summary: content.chars().take(500).collect(),
                    });
                    Tick::Terminal(TaskState::Completed)
                } else {
                    // Generated content is a turn result; publish it and
                    // continue to the next decision.
                    self.emit(SessionEvent::NodeResult {
                        task: task_id,
                        turn,
                        node,
                        node_label: "generate".to_owned(),
                        status: NodeStatus::Succeeded,
                        output: Value::String(content.chars().take(2000).collect()),
                    });
                    Tick::Continue
                }
            }
            Err(err) => {
                let reason = err.to_string();
                self.emit(SessionEvent::TaskFailed {
                    task: task_id,
                    reason,
                });
                Tick::Terminal(TaskState::Failed)
            }
        }
    }
}

/// Whether mid-task input is a steering directive rather than new work.
///
/// Deliberately simple and documented: a single short line is a
/// directive; anything longer or multi-line is new work that gets queued.
/// The session owns the decision, so the UI cannot silently reinterpret a
/// queued request as an edit to the running task.
fn is_steering_directive(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && !trimmed.contains('\n') && trimmed.chars().count() <= 160
}

/// Whether a failed node is a *check* failure (schema/artifact
/// verification) rather than a failed effect or unavailable tool.
///
/// Only check failures are repairable: retrying a write that may already
/// have happened, or a command that may have had side effects, would risk
/// duplicating real work. The gate's journal already refuses such
/// replays; this keeps the session from even attempting one.
fn is_verification_failure(plan: &PlanNode, node_id: &str) -> bool {
    matches!(find_node(plan, node_id), Some(PlanNode::Verify { .. }))
}

/// Whether a node is a leaf (an actual action or check) rather than a
/// composite whose status is just its children's.
fn is_leaf_node(plan: &PlanNode, node_id: &str) -> bool {
    find_node(plan, node_id).is_some_and(|node| node.children().is_empty())
}

fn find_node<'a>(plan: &'a PlanNode, node_id: &str) -> Option<&'a PlanNode> {
    if plan.id() == node_id {
        return Some(plan);
    }
    plan.children()
        .iter()
        .find_map(|child| find_node(child, node_id))
}

/// Copies stream events into the session's event vocabulary.
///
/// The sink only translates; the runtime decides when the events are
/// published, so the transcript stays totally ordered.
struct SessionSink {
    task: TaskId,
    turn: TurnId,
    node: NodeId,
    events: Arc<Mutex<Vec<SessionEvent>>>,
}

impl crate::ModelStreamSink for SessionSink {
    fn on_event(&self, event: crate::ModelStreamEvent) {
        let translated = match event {
            crate::ModelStreamEvent::TextDelta { text } => Some(SessionEvent::TextDelta {
                task: self.task,
                turn: self.turn,
                node: self.node,
                text,
            }),
            // Reasoning text is provider state: preserved for
            // continuation, never published as if it were assistant
            // output the user or the task should act on.
            crate::ModelStreamEvent::ReasoningDelta { .. } => None,
            // Partial tool arguments are deliberately not published: a
            // fragment is not an executable command. Complete calls are
            // published by the runtime after the turn finishes.
            crate::ModelStreamEvent::ToolCallStarted { .. }
            | crate::ModelStreamEvent::ToolCallArgumentsDelta { .. }
            | crate::ModelStreamEvent::ToolCallEnded { .. } => None,
            crate::ModelStreamEvent::Completed { .. }
            | crate::ModelStreamEvent::Incomplete { .. } => None,
        };

        if let Some(event) = translated
            && let Ok(mut events) = self.events.lock()
        {
            events.push(event);
        }
    }
}

/// Find the first tool node in a plan (pre-order), destructured to its
/// invocation parts.
fn find_first_tool_node(plan: &PlanNode) -> Option<(&str, &str, &Value)> {
    if let PlanNode::Tool {
        capability,
        tool_id,
        input,
        ..
    } = plan
    {
        return Some((capability, tool_id, input));
    }
    for child in plan.children() {
        if let Some(found) = find_first_tool_node(child) {
            return Some(found);
        }
    }
    None
}

/// A convenience driver: run commands against a runtime until the task
/// reaches a terminal state or needs input, bounded by `max_ticks`.
///
/// This is what CLI/TUI adapters call; they never implement loops that
/// execute tools themselves.
pub async fn drive_until_stable<S: SystemOne>(
    runtime: &mut SessionRuntime<S>,
    max_ticks: usize,
) -> Option<TaskState> {
    for _ in 0..max_ticks {
        let state = runtime.drive().await?;
        if state.is_terminal() || state == TaskState::Waiting || state == TaskState::Paused {
            return Some(state);
        }
    }
    runtime.task_state()
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use crate::planner::Planner;
    use crate::runtime::Knut;
    use crate::tool::{SideEffect, Tool, ToolMetadata};
    use crate::{
        ComputeCascade, Decision, DecisionInput, ExecutionGate, KnutError, ModelIdentity,
        ModelRequest, ModelResponse, ModelStreamSink, ModelTier, Risk, Route, SystemOne, Usage,
        VerificationVerdict, Verifier,
    };

    use super::*;

    // --- fakes ---------------------------------------------------------

    struct ScriptedReasoner {
        responses: Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    impl ScriptedReasoner {
        fn with(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl crate::Model for ScriptedReasoner {
        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: "fake".to_owned(),
                model: "scripted".to_owned(),
                tier: ModelTier::Reasoner,
            }
        }

        async fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse, KnutError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut queue = self.responses.lock().unwrap();
            let content = if queue.is_empty() {
                "out of script".to_owned()
            } else {
                queue.remove(0)
            };
            Ok(ModelResponse::text(
                content,
                self.identity(),
                Usage::known(10, 10),
                std::time::Duration::ZERO,
            ))
        }
    }

    struct AcceptAll;
    impl Verifier for AcceptAll {
        fn verify(&self, _r: &ModelResponse) -> VerificationVerdict {
            VerificationVerdict::Sufficient
        }
    }

    /// SystemOne that routes everything to Generate(reasoner).
    struct AlwaysGenerate;

    #[async_trait]
    impl SystemOne for AlwaysGenerate {
        async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
            Ok(Decision {
                route: Route::Generate,
                confidence: 0.95,
                retrieval: None,
                capability: None,
                model_tier: ModelTier::Reasoner,
                risk: Risk::Low,
                parallelizable: false,
            })
        }
    }

    /// A read-only file tool that succeeds.
    struct OkTool {
        id: &'static str,
        capability: &'static str,
        output: Value,
    }

    #[async_trait]
    impl Tool for OkTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: self.id.to_owned(),
                tool_version: "1".to_owned(),
                capability: self.capability.to_owned(),
                description: "test tool".to_owned(),
                input_schema: json!({ "type": "object" }),
                side_effect: SideEffect::ReadOnly,
            }
        }

        async fn call(&self, _input: Value) -> Result<Value, KnutError> {
            Ok(self.output.clone())
        }
    }

    /// A write tool that requires approval.
    struct WriteTool {
        id: &'static str,
        capability: &'static str,
    }

    #[async_trait]
    impl Tool for WriteTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: self.id.to_owned(),
                tool_version: "1".to_owned(),
                capability: self.capability.to_owned(),
                description: "write tool".to_owned(),
                input_schema: json!({ "type": "object" }),
                side_effect: SideEffect::IdempotentWrite,
            }
        }

        async fn call(&self, _input: Value) -> Result<Value, KnutError> {
            Ok(json!({ "written": true }))
        }
    }

    fn registry() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::default();
        registry
            .register(OkTool {
                id: "read",
                capability: "files",
                output: json!({ "content": "hello" }),
            })
            .unwrap();
        registry
            .register(WriteTool {
                id: "write",
                capability: "files",
            })
            .unwrap();
        Arc::new(registry)
    }

    fn plan_json(tool_id: &str) -> String {
        serde_json::to_string(&json!({
            "type": "sequence",
            "id": "root",
            "children": [
                { "type": "tool", "id": "step", "capability": "files", "tool_id": tool_id, "input": {} },
                { "type": "verify", "id": "check", "target": "step", "artifact": "json" }
            ]
        }))
        .unwrap()
    }

    fn runtime_with(
        system_one: Arc<dyn SystemOne>,
        reasoner_responses: Vec<String>,
        policy: crate::SideEffectPolicy,
    ) -> SessionRuntime<Arc<dyn SystemOne>> {
        let (runtime, _) = runtime_with_reasoner(system_one, reasoner_responses, policy);
        runtime
    }

    /// The same runtime, plus a handle on the reasoner so tests can count
    /// model rounds (approval must resume a plan, not re-plan it).
    fn runtime_with_reasoner(
        system_one: Arc<dyn SystemOne>,
        reasoner_responses: Vec<String>,
        policy: crate::SideEffectPolicy,
    ) -> (SessionRuntime<Arc<dyn SystemOne>>, Arc<ScriptedReasoner>) {
        let router = Arc::new(Knut::new(system_one));
        let reasoner = Arc::new(ScriptedReasoner::with(reasoner_responses));
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(Arc::clone(&reasoner)));
        let planner = Planner::new(Arc::clone(&cascade));
        let gate = Arc::new(ExecutionGate::new(policy));
        (
            SessionRuntime::new(
                router,
                planner,
                registry(),
                gate,
                cascade,
                Arc::new(AcceptAll),
            ),
            reasoner,
        )
    }

    /// SystemOne that routes Act(files) — used with a prompt matching the
    /// explicit-capability rule ("files") to hit the plan path.
    struct ActFiles;

    #[async_trait]
    impl SystemOne for ActFiles {
        async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
            Ok(Decision {
                route: Route::Act,
                confidence: 0.95,
                retrieval: None,
                capability: Some("files".to_owned()),
                model_tier: ModelTier::Fast,
                risk: Risk::Low,
                parallelizable: false,
            })
        }
    }

    // --- event log -----------------------------------------------------

    #[test]
    fn event_log_bounded_drops_cosmetic_first() {
        let mut log = EventLog::new();
        for i in 0..EVENT_LOG_CAPACITY + 500 {
            log.push(if i % 7 == 0 {
                SessionEvent::RuntimeError {
                    task: None,
                    message: format!("cosmetic {i}"),
                }
            } else {
                SessionEvent::Resumed {
                    task: TaskId(i as u64),
                }
            });
        }

        assert_eq!(log.len(), EVENT_LOG_CAPACITY);
        assert!(log.dropped() > 0);
        // Terminal/critical events never dropped for cosmetic ones: the
        // retained set is full-capacity with the newest events.
        let last = log.events().last().unwrap();
        assert_eq!(
            *last,
            SessionEvent::Resumed {
                task: TaskId((EVENT_LOG_CAPACITY + 499) as u64)
            }
        );
    }

    #[test]
    fn commands_and_events_serialize() {
        let submit: SessionCommand =
            serde_json::from_str(r#"{"kind": "submit", "prompt": "fix it"}"#).unwrap();
        assert_eq!(
            submit,
            SessionCommand::Submit {
                prompt: "fix it".to_owned()
            }
        );

        // Identifiers are plain integers on the wire: the transcript is
        // consumed by headless tools, so the tagged event envelope plus a
        // scalar id is the smallest stable contract.
        let event: SessionEvent =
            serde_json::from_str(r#"{"kind": "task_started", "task": 1, "prompt": "fix it"}"#)
                .unwrap();
        assert!(matches!(event, SessionEvent::TaskStarted { .. }));
        assert_eq!(
            serde_json::to_string(&SessionEvent::TaskStarted {
                task: TaskId(1),
                prompt: "fix it".to_owned(),
            })
            .unwrap(),
            r#"{"kind":"task_started","task":1,"prompt":"fix it"}"#
        );

        let json = serde_json::to_string(&SessionEvent::TaskCancelled { task: TaskId(7) }).unwrap();
        assert!(json.contains("\"task_cancelled\""));
    }

    // --- the coding session scenario ------------------------------------

    /// One fake coding session: routes, reads, reasons, proposes a write,
    /// waits for approval, then finishes — the full event contract.
    #[tokio::test]
    async fn one_coding_session_routes_reads_waits_and_finishes() {
        let reasoner_plan = plan_json("write");
        let mut runtime = runtime_with(
            Arc::new(ActFiles),
            vec![reasoner_plan],
            // Writes require approval.
            crate::SideEffectPolicy::new()
                .allow(SideEffect::ReadOnly)
                .require_approval(SideEffect::IdempotentWrite),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();

        // First drive: routes to Act(files), plans a write, blocks on
        // approval.
        let state = drive_until_stable(&mut runtime, 10).await.unwrap();
        assert_eq!(state, TaskState::Waiting);

        let wait = runtime.pending_wait().unwrap();
        let WaitKind::Approval { approval_key } = &wait.kind else {
            panic!("expected an approval wait, got {:?}", wait.kind);
        };
        assert!(!approval_key.is_empty());

        // Approve the exact pending action.
        runtime
            .command(SessionCommand::Approve {
                approval_key: approval_key.clone(),
            })
            .await
            .unwrap();

        // The re-run executes the write under the granted approval.
        let state = drive_until_stable(&mut runtime, 10).await.unwrap();
        assert_eq!(state, TaskState::Completed);

        // Transcript: the same events a TUI would render.
        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        let kinds: Vec<String> = events
            .iter()
            .map(|e| {
                serde_json::to_value(e).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert!(kinds.contains(&"task_started".to_owned()));
        assert!(kinds.contains(&"routed".to_owned()));
        assert!(kinds.contains(&"generating".to_owned()));
        assert!(kinds.contains(&"plan_started".to_owned()));
        assert!(kinds.contains(&"waiting_for_user".to_owned()));
        assert!(kinds.contains(&"wait_resolved".to_owned()));
        assert!(kinds.contains(&"task_completed".to_owned()));
    }

    #[tokio::test]
    async fn approval_resumes_the_same_plan_instead_of_replanning() {
        let (mut runtime, reasoner) = runtime_with_reasoner(
            Arc::new(ActFiles),
            // One plan round only. A second round would consume the next
            // entry; the script is exhausted afterwards on purpose.
            vec![plan_json("write")],
            crate::SideEffectPolicy::new()
                .allow(SideEffect::ReadOnly)
                .require_approval(SideEffect::IdempotentWrite),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();

        let state = drive_until_stable(&mut runtime, 10).await.unwrap();
        assert_eq!(state, TaskState::Waiting);
        assert_eq!(reasoner.calls(), 1, "planning happens once");

        let wait = runtime.pending_wait().unwrap();
        let WaitKind::Approval { approval_key } = &wait.kind else {
            panic!("expected approval wait");
        };
        let approval_key = approval_key.clone();

        runtime
            .command(SessionCommand::Approve { approval_key })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 10).await.unwrap();

        assert_eq!(state, TaskState::Completed);
        // The approved plan is re-executed, not regenerated: the user
        // approved exact actions inside one plan.
        assert_eq!(reasoner.calls(), 1, "approval must not re-plan");
    }

    /// A plan whose generate node produces non-JSON text that is then
    /// verified as JSON: the artifact check is the node that fails.
    fn plan_with_failing_check() -> String {
        serde_json::to_string(&json!({
            "type": "sequence",
            "id": "root",
            "children": [
                { "type": "generate", "id": "draft", "instruction": "produce json", "tier": "reasoner" },
                { "type": "verify", "id": "check", "target": "draft", "artifact": "json" }
            ]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn verification_failure_repairs_and_finishes() {
        // The first plan's verify node fails: the model produced text
        // where the plan promised JSON. The session must observe that
        // failure, carry it into a repair round, and finish on the
        // repaired plan — neither declaring success nor giving up after
        // one bad plan.
        let (mut runtime, reasoner) = runtime_with_reasoner(
            Arc::new(ActFiles),
            vec![
                plan_with_failing_check(),
                "not json".to_owned(),
                plan_json("read"),
            ],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();

        let state = drive_until_stable(&mut runtime, 10).await.unwrap();

        assert_eq!(state, TaskState::Completed);
        // Failing plan -> its generation -> repaired plan.
        assert_eq!(reasoner.calls(), 3);

        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::NodeResult { status, node_label, .. }
                    if *status == NodeStatus::Failed && node_label == "check"
            )),
            "the verification failure is observable"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::TaskCompleted { .. }))
        );
    }

    #[tokio::test]
    async fn exhausted_repair_budget_fails_explicitly() {
        // Every plan fails verification: repair is bounded, so the task
        // ends as an explicit failure rather than looping or claiming
        // completion.
        let mut responses = Vec::new();
        for _ in 0..12 {
            responses.push(plan_with_failing_check());
            responses.push("not json".to_owned());
        }

        let mut runtime = runtime_with(
            Arc::new(ActFiles),
            responses,
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();

        let state = drive_until_stable(&mut runtime, 20).await.unwrap();
        assert_eq!(state, TaskState::Failed);
        assert!(runtime
            .events()
            .events()
            .any(|e| matches!(e, SessionEvent::TaskFailed { reason, .. } if reason.contains("nodes failed"))));
    }

    #[tokio::test]
    async fn streaming_deltas_and_tool_call_proposals_reach_the_transcript() {
        // A streaming adapter: fragments become TextDelta events as they
        // arrive, and a completed tool call becomes a proposal with
        // parsed arguments (never a partial JSON fragment).
        struct StreamingReasoner;

        #[async_trait]
        impl crate::Model for StreamingReasoner {
            fn identity(&self) -> ModelIdentity {
                ModelIdentity {
                    provider: "fake".to_owned(),
                    model: "streaming".to_owned(),
                    tier: ModelTier::Reasoner,
                }
            }

            async fn complete(&self, _r: &ModelRequest) -> Result<ModelResponse, KnutError> {
                unimplemented!("this fake streams")
            }

            fn capabilities(&self) -> crate::ModelCapabilities {
                crate::ModelCapabilities {
                    streaming: true,
                    tools: true,
                    reasoning: true,
                    continuation: true,
                    usage: true,
                }
            }

            async fn stream(
                &self,
                _request: &ModelRequest,
                sink: &mut (dyn crate::ModelStreamSink + Send),
            ) -> Result<ModelResponse, KnutError> {
                sink.on_event(crate::ModelStreamEvent::TextDelta {
                    text: "wor".to_owned(),
                });
                sink.on_event(crate::ModelStreamEvent::TextDelta {
                    text: "king".to_owned(),
                });
                sink.on_event(crate::ModelStreamEvent::ToolCallStarted {
                    id: "call_7".to_owned(),
                    name: "read".to_owned(),
                });
                sink.on_event(crate::ModelStreamEvent::ToolCallArgumentsDelta {
                    id: "call_7".to_owned(),
                    delta: "{\"path\":\"a.rs\"}".to_owned(),
                });
                sink.on_event(crate::ModelStreamEvent::ToolCallEnded {
                    id: "call_7".to_owned(),
                });
                sink.on_event(crate::ModelStreamEvent::Completed {
                    usage: Usage::known(5, 2),
                });

                let buffered = crate::BufferedSink::new();
                buffered.on_event(crate::ModelStreamEvent::TextDelta {
                    text: "working".to_owned(),
                });
                buffered.on_event(crate::ModelStreamEvent::ToolCallStarted {
                    id: "call_7".to_owned(),
                    name: "read".to_owned(),
                });
                buffered.on_event(crate::ModelStreamEvent::ToolCallArgumentsDelta {
                    id: "call_7".to_owned(),
                    delta: "{\"path\":\"a.rs\"}".to_owned(),
                });
                buffered.on_event(crate::ModelStreamEvent::Completed {
                    usage: Usage::known(5, 2),
                });
                Ok(buffered
                    .into_response(self.identity(), std::time::Duration::ZERO)
                    .expect("complete stream"))
            }
        }

        let router = Arc::new(Knut::new(Arc::new(AlwaysGenerate)));
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(StreamingReasoner));
        let planner = Planner::new(Arc::clone(&cascade));
        let gate = Arc::new(ExecutionGate::new(
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        ));
        let mut runtime = SessionRuntime::new(
            router,
            planner,
            registry(),
            gate,
            cascade,
            Arc::new(AcceptAll),
        )
        .with_requirements(CompletionRequirements::none().require(
            "tests",
            "tests pass",
            true,
        ));

        runtime
            .command(SessionCommand::Submit {
                prompt: "fix the bug".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 6).await.unwrap();
        assert_ne!(state, TaskState::Completed);

        let events: Vec<&SessionEvent> = runtime.events().events().collect();

        // Incremental text arrived as separate deltas, in order. The
        // unmet requirement keeps the task running, so the stream is
        // per-turn: assert the first turn's deltas exactly.
        let first_turn = runtime
            .events()
            .events()
            .filter_map(|e| match e {
                SessionEvent::Routed { turn, .. } => Some(*turn),
                _ => None,
            })
            .next()
            .expect("a routing decision");
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::TextDelta { text, turn, .. } if *turn == first_turn => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["wor", "king"]);

        // The completed tool call is a proposal with parsed arguments.
        let proposals: Vec<(&str, &Value)> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::ToolCallProposed {
                    call_id, arguments, ..
                } => Some((call_id.as_str(), arguments)),
                _ => None,
            })
            .collect();
        assert!(!proposals.is_empty());
        assert_eq!(proposals[0].0, "call_7");
        assert_eq!(*proposals[0].1, json!({ "path": "a.rs" }));

        // No event ever carries partial argument JSON.
        assert!(!events.iter().any(|e| matches!(
            e,
            SessionEvent::ToolCallProposed { arguments, .. } if arguments.is_null()
        )));
    }

    #[tokio::test]
    async fn dropped_deltas_never_lose_the_terminal_result() {
        // Delta events are droppable under pressure; the completed
        // artifact is not. Force heavy churn and confirm the outcome and
        // node result survive.
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["answer".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_requirements(CompletionRequirements::none().require(
            "tests",
            "tests pass",
            true,
        ));

        runtime
            .command(SessionCommand::Submit {
                prompt: "go".to_owned(),
            })
            .await
            .unwrap();
        runtime.drive().await.unwrap();

        // Flood the log with droppable deltas.
        for i in 0..(EVENT_LOG_CAPACITY * 2) {
            runtime.emit(SessionEvent::TextDelta {
                task: TaskId(1),
                turn: TurnId(1),
                node: NodeId(1),
                text: format!("chunk {i}"),
            });
        }

        assert!(runtime.events().dropped() > 0);
        assert!(
            runtime
                .events()
                .events()
                .any(|e| matches!(e, SessionEvent::NodeResult { .. }))
        );
        assert!(
            !runtime
                .events()
                .events()
                .any(|e| matches!(e, SessionEvent::TextDelta { text, .. } if text == "chunk 0"))
        );
    }

    // --- coding-loop frames (#22) ---------------------------------------

    /// Counts frame calls so tests can prove decisions are batched and
    /// only asked at real boundaries.
    struct CountingFrameRouter {
        inner: crate::StaticFrameRouter,
        calls: Arc<Mutex<Vec<crate::FrameKind>>>,
    }

    #[async_trait]
    impl crate::FrameRouter for CountingFrameRouter {
        async fn ask(
            &self,
            frame: &crate::DecisionFrame,
        ) -> Result<crate::SystemOneResponse, KnutError> {
            self.calls.lock().unwrap().push(frame.kind);
            crate::FrameRouter::ask(&self.inner, frame).await
        }
    }

    #[tokio::test]
    async fn one_coding_session_makes_two_meaningful_frame_decisions() {
        // Discovery chooses a candidate, then a failing check is
        // classified: two decisions inside one read/edit/check session,
        // not merely at ingress.
        // The router returns Act *without* a capability, so the runtime
        // must resolve the capability through bounded candidate
        // discovery, then classify the failing check.
        let (runtime, _reasoner) = runtime_with_reasoner(
            Arc::new(ActNoCapRouter),
            vec![
                plan_with_failing_check(),
                "not json".to_owned(),
                plan_json("read"),
            ],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );

        let calls = Arc::new(Mutex::new(Vec::new()));
        let router = Arc::new(CountingFrameRouter {
            inner: crate::StaticFrameRouter::choice_over(
                "candidate",
                "files",
                0.9,
                &["shell", crate::ESCALATE_ID],
            )
            .and(crate::StaticFrameRouter::choice_over(
                "failure_class",
                "verification",
                0.8,
                &[
                    "transient",
                    "wrong_approach",
                    "blocked_by_policy",
                    "unknown",
                ],
            )),
            calls: Arc::clone(&calls),
        });
        let mut runtime = runtime
            .with_frames(router)
            .with_discovery(DiscoveryCandidates {
                candidates: vec![
                    (
                        "files".to_owned(),
                        "read or write workspace files".to_owned(),
                    ),
                    ("shell".to_owned(), "run a supervised command".to_owned()),
                ],
            });

        runtime
            .command(SessionCommand::Submit {
                prompt: "fix it".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 12).await.unwrap();
        assert_eq!(state, TaskState::Completed);

        // Two different frame kinds were consulted: candidate selection
        // and failure recovery.
        let kinds = calls.lock().unwrap().clone();
        assert!(
            kinds.contains(&crate::FrameKind::CandidateSelection),
            "no candidate decision: {kinds:?}"
        );
        assert!(
            kinds.contains(&crate::FrameKind::Recovery),
            "no recovery decision: {kinds:?}"
        );

        // Both are auditable in the transcript, with the frame version,
        // the question pack and the raw distribution preserved.
        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        let decisions: Vec<(&crate::FrameKind, &str, bool)> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::FrameDecided {
                    question_kind,
                    choice,
                    overridden,
                    ..
                } => Some((question_kind, choice.as_str(), *overridden)),
                _ => None,
            })
            .collect();
        assert!(decisions.len() >= 2, "decisions: {decisions:?}");
        assert!(decisions.iter().all(|(_, _, overridden)| !overridden));
        assert!(decisions.iter().any(|(kind, choice, _)| **kind
            == crate::FrameKind::CandidateSelection
            && *choice == "files"));
        assert!(
            decisions
                .iter()
                .any(|(kind, choice, _)| **kind == crate::FrameKind::Recovery
                    && *choice == "verification")
        );

        // Distributions are stored raw, not collapsed.
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::FrameDecided { distribution, .. }
                if distribution.get("files").is_some()
        )));
    }

    #[tokio::test]
    async fn unambiguous_steps_do_not_consult_system_one() {
        // A single legal candidate is mechanical: no frame decision, no
        // network call.
        let (runtime, _reasoner) = runtime_with_reasoner(
            Arc::new(ActFiles),
            vec![plan_json("read")],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let router = Arc::new(CountingFrameRouter {
            inner: crate::StaticFrameRouter::choice("candidate", "files", 0.9),
            calls: Arc::clone(&calls),
        });
        let mut runtime = runtime
            .with_frames(router)
            .with_discovery(DiscoveryCandidates {
                candidates: vec![("files".to_owned(), "files".to_owned())],
            });

        runtime
            .command(SessionCommand::Submit {
                prompt: "fix it".to_owned(),
            })
            .await
            .unwrap();
        drive_until_stable(&mut runtime, 12).await.unwrap();

        // The routed action was explicit, so nothing needed a bounded
        // decision.
        assert!(
            calls.lock().unwrap().is_empty(),
            "a mechanical step consulted System One: {:?}",
            calls.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn an_unoffered_candidate_answer_cannot_dispatch_anything() {
        // The router answers with a capability the frame never offered:
        // the runtime must treat it as unusable and escalate, not act.
        // A well-formed answer that names an option the frame never
        // offered: the distribution covers the real options, but the
        // selection is unusable.
        let mut probabilities = std::collections::BTreeMap::new();
        probabilities.insert("files".to_owned(), 0.01);
        probabilities.insert("shell".to_owned(), 0.01);
        probabilities.insert(crate::ESCALATE_ID.to_owned(), 0.01);
        probabilities.insert("totally_made_up".to_owned(), 0.97);
        let mut answers = std::collections::BTreeMap::new();
        answers.insert(
            "candidate".to_owned(),
            crate::Answer::Choice {
                choice: "totally_made_up".to_owned(),
                probabilities,
                confidence: 0.97,
            },
        );
        let router = Arc::new(crate::StaticFrameRouter::new(answers));
        let mut runtime = runtime_with(
            Arc::new(ActNoCapRouter),
            vec![],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_frames(router)
        .with_discovery(DiscoveryCandidates {
            candidates: vec![
                ("files".to_owned(), "files".to_owned()),
                ("shell".to_owned(), "shell".to_owned()),
            ],
        });

        runtime
            .command(SessionCommand::Submit {
                prompt: "do something".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 6).await.unwrap();

        // Waiting for the user: no capability was dispatched.
        assert_eq!(state, TaskState::Waiting);
        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::FrameDecided {
                overridden: true,
                ..
            }
        )));
        assert!(!events.iter().any(|e| matches!(
            e,
            SessionEvent::NodeResult { .. } | SessionEvent::Generating { .. }
        )));
    }

    /// Routes Act without a capability, so discovery is exercised.
    struct ActNoCapRouter;

    #[async_trait]
    impl SystemOne for ActNoCapRouter {
        async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
            Ok(Decision {
                route: Route::Act,
                confidence: 0.95,
                retrieval: None,
                capability: None,
                model_tier: ModelTier::Fast,
                risk: Risk::Low,
                parallelizable: false,
            })
        }
    }

    #[tokio::test]
    async fn escalation_answer_routes_to_reasoning_not_a_guess() {
        // The router declines the candidate set: the runtime must not
        // pick a capability anyway.
        let router = Arc::new(crate::StaticFrameRouter::choice_over(
            "candidate",
            crate::ESCALATE_ID,
            0.9,
            &["files", "shell"],
        ));
        let mut runtime = runtime_with(
            Arc::new(ActNoCapRouter),
            vec![],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_frames(router)
        .with_discovery(DiscoveryCandidates {
            candidates: vec![
                ("files".to_owned(), "files".to_owned()),
                ("shell".to_owned(), "shell".to_owned()),
            ],
        });

        runtime
            .command(SessionCommand::Submit {
                prompt: "do something".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 6).await.unwrap();

        assert_eq!(state, TaskState::Waiting);
        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        // The recorded choice names escalation and was not overridden:
        // it is a legitimate answer, not a failure.
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::FrameDecided { choice, overridden: false, .. }
                if choice == crate::ESCALATE_ID
        )));
        assert!(!events.iter().any(|e| matches!(
            e,
            SessionEvent::NodeResult { .. } | SessionEvent::Generating { .. }
        )));
    }

    #[tokio::test]
    async fn only_real_check_evidence_completes_a_coding_task() {
        // The coding slice's completion path is the check runner's
        // revision-bound evidence: a plan that produced a valid artifact
        // (schema-valid JSON, accepted by the shape verifier) still does
        // not complete the task.
        let fixture = std::env::temp_dir().join(format!(
            "knut-session-evidence-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(fixture.join("src")).unwrap();
        std::fs::write(fixture.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(fixture.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        let workspace = crate::Workspace::open(&fixture).unwrap();

        let supervisor = Arc::new(crate::Supervisor::new(workspace.clone()));
        let runner = crate::CheckRunner::new(
            workspace.clone(),
            supervisor,
            crate::CheckProfile {
                name: "fixture".to_owned(),
                checks: vec![crate::CheckSpec::new(
                    "test",
                    "the test suite passes",
                    "/bin/sh",
                    vec![
                        "-c".to_owned(),
                        "echo 'test result: FAILED. 0 passed; 1 failed'; exit 101".to_owned(),
                    ],
                )],
            },
        );

        let revision = runner.current_revision("src").unwrap();
        let checks = runner.run_all(&revision).await;
        assert_eq!(checks[0].outcome, crate::CheckOutcome::Failed);

        // The session's completion contract, fed with that evidence.
        let (runtime, _reasoner) = runtime_with_reasoner(
            Arc::new(AlwaysGenerate),
            vec!["attempt 1".to_owned(), "attempt 2".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );
        let mut runtime = runtime
            .with_requirements(runner.requirements())
            .with_artifact_revision(revision.clone());

        // Even a passing *shape* check cannot satisfy the requirement.
        for check in &checks {
            runtime.seed_evidence(check.to_evidence());
        }

        runtime
            .command(SessionCommand::Submit {
                prompt: "fix the failing test".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 4).await.unwrap();
        assert_ne!(
            state,
            TaskState::Completed,
            "a failing check produced completion"
        );

        // Once the check genuinely passes at the same revision, it does.
        let mut passing_checks = checks.clone();
        passing_checks[0].outcome = crate::CheckOutcome::Passed;
        passing_checks[0].reason = "passed".to_owned();
        passing_checks[0].test_counts = crate::TestCounts {
            passed: Some(1),
            failed: Some(0),
            ignored: Some(0),
        };

        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["done".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_requirements(runner.requirements())
        .with_artifact_revision(revision.clone());
        for check in &passing_checks {
            runtime.seed_evidence(check.to_evidence());
        }
        runtime
            .command(SessionCommand::Submit {
                prompt: "fix the failing test".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 4).await.unwrap();
        assert_eq!(state, TaskState::Completed);

        let _ = std::fs::remove_dir_all(&fixture);
    }

    #[tokio::test]
    async fn a_review_verdict_is_evidence_but_never_replaces_checks() {
        // An accepting review is recorded, and a blocking requirement
        // without checks still blocks: semantic review is additional
        // evidence, not a substitute.
        let review = crate::parse_review(r#"{"verdict":"accept","concerns":[]}"#, "reasoner");
        assert!(review.is_acceptance());

        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["done".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_requirements(CompletionRequirements::none().require("test", "tests pass", true))
        .with_artifact_revision(crate::ArtifactRevision::new("patch", "r1"));

        runtime
            .command(SessionCommand::Submit {
                prompt: "fix it".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 4).await.unwrap();
        assert_ne!(state, TaskState::Completed);
    }

    #[tokio::test]
    async fn steering_while_an_approval_is_pending_invalidates_the_stale_action() {
        // The user steers mid-task while an approval is pending: the
        // approval was minted for the old revision, so it must not be
        // resumable afterwards.
        let (mut runtime, _reasoner) = runtime_with_reasoner(
            Arc::new(ActFiles),
            vec![plan_json("write")],
            crate::SideEffectPolicy::new()
                .allow(SideEffect::ReadOnly)
                .require_approval(SideEffect::IdempotentWrite),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 10).await.unwrap();
        assert_eq!(state, TaskState::Waiting);

        let wait = runtime.pending_wait().unwrap();
        let WaitKind::Approval { approval_key } = &wait.kind else {
            panic!("expected approval wait");
        };
        let stale_key = approval_key.clone();

        // Mid-task input: a short directive steers the task.
        runtime
            .command(SessionCommand::SteerOrQueue {
                text: "use a different file instead".to_owned(),
            })
            .await
            .unwrap();

        // The stale approval is gone: the wait was resolved by the
        // steering, and the old key no longer refers to anything.
        assert!(runtime.pending_wait().is_none());
        let err = runtime
            .command(SessionCommand::Approve {
                approval_key: stale_key,
            })
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("no pending approval"),
            "got {err}"
        );

        // The revision advanced, and the steering is in the transcript.
        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::TaskSteered { revision, .. } if revision.0 >= 2
        )));
    }

    #[tokio::test]
    async fn queuing_does_not_modify_the_active_task() {
        // Mid-task input that is new work is queued: the running task's
        // prompt and revision are untouched.
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["first".to_owned(), "second".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_requirements(CompletionRequirements::none().require("tests", "tests pass", true))
        .with_artifact_revision(crate::ArtifactRevision::new("patch", "r1"));

        runtime
            .command(SessionCommand::Submit {
                prompt: "the original task".to_owned(),
            })
            .await
            .unwrap();
        runtime.drive().await.unwrap();

        let revision_before = runtime
            .events()
            .events()
            .filter_map(|e| match e {
                SessionEvent::TaskStarted { .. } => Some(TaskRevision(1)),
                SessionEvent::TaskSteered { revision, .. } => Some(*revision),
                _ => None,
            })
            .last()
            .unwrap_or(TaskRevision(1));

        // A multi-line request is new work, not a directive.
        runtime
            .command(SessionCommand::SteerOrQueue {
                text: "a much longer request\nwith several lines\nof new work".to_owned(),
            })
            .await
            .unwrap();

        // No steering happened, the request is queued, and the active task
        // still holds its own prompt.
        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::TaskSteered { .. })),
            "queued work was applied as a steering edit"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::RequestQueued { text } if text.contains("several lines")
        )));

        // The running task's revision is unchanged.
        let revision_after = events
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEvent::TaskSteered { revision, .. } => Some(*revision),
                _ => None,
            })
            .unwrap_or(revision_before);
        assert_eq!(revision_before, revision_after);
    }

    #[tokio::test]
    async fn a_directive_steers_and_new_work_queues_in_one_stream() {
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["one".to_owned(), "two".to_owned(), "three".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_requirements(CompletionRequirements::none().require("tests", "tests pass", true))
        .with_artifact_revision(crate::ArtifactRevision::new("patch", "r1"));

        runtime
            .command(SessionCommand::Submit {
                prompt: "do the thing".to_owned(),
            })
            .await
            .unwrap();
        runtime.drive().await.unwrap();

        runtime
            .command(SessionCommand::SteerOrQueue {
                text: "be more careful".to_owned(),
            })
            .await
            .unwrap();
        runtime
            .command(SessionCommand::SteerOrQueue {
                text: "then also rewrite the parser\nand update the docs".to_owned(),
            })
            .await
            .unwrap();

        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SessionEvent::TaskSteered { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, SessionEvent::RequestQueued { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn denial_fails_the_task_without_bypass() {
        let mut runtime = runtime_with(
            Arc::new(ActFiles),
            vec![plan_json("write")],
            crate::SideEffectPolicy::new()
                .allow(SideEffect::ReadOnly)
                .require_approval(SideEffect::IdempotentWrite),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();
        drive_until_stable(&mut runtime, 10).await.unwrap();

        let wait = runtime.pending_wait().unwrap();
        let WaitKind::Approval { approval_key } = &wait.kind else {
            panic!("expected approval wait");
        };

        runtime
            .command(SessionCommand::Deny {
                approval_key: approval_key.clone(),
            })
            .await
            .unwrap();

        assert_eq!(runtime.task_state(), Some(TaskState::Failed));
        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        assert!(events.iter().any(
            |e| matches!(e, SessionEvent::TaskFailed { reason, .. } if reason.contains("denied"))
        ));
    }

    #[tokio::test]
    async fn cancellation_produces_one_terminal_outcome() {
        let mut runtime = runtime_with(
            Arc::new(ActFiles),
            vec![plan_json("read")],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();

        runtime.command(SessionCommand::Cancel).await.unwrap();

        // Driving after cancel: exactly one terminal outcome.
        let first = drive_until_stable(&mut runtime, 10).await.unwrap();
        assert_eq!(first, TaskState::Cancelled);

        let cancelled_count = runtime
            .events()
            .events()
            .filter(|e| matches!(e, SessionEvent::TaskCancelled { .. }))
            .count();
        assert_eq!(cancelled_count, 1);
    }

    #[tokio::test]
    async fn pause_prevents_new_work_and_resume_continues() {
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["answer 1".to_owned(), "answer 2".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "write a poem".to_owned(),
            })
            .await
            .unwrap();
        runtime.command(SessionCommand::Pause).await.unwrap();

        // Driving while paused performs no routing.
        let state = runtime.drive().await.unwrap();
        assert_eq!(state, TaskState::Paused);

        runtime.command(SessionCommand::Resume).await.unwrap();
        let state = drive_until_stable(&mut runtime, 10).await.unwrap();
        // Generation continues without completion evidence (empty
        // requirements are vacuously satisfied here, so generation
        // completes).
        assert_eq!(state, TaskState::Completed);
    }

    #[tokio::test]
    async fn steering_invalidates_stale_decisions() {
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["first answer".to_owned(), "second answer".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        // A blocking requirement without evidence: generation produces a
        // turn result, not completion, so the task stays steered-able.
        .with_requirements(CompletionRequirements::none().require(
            "tests",
            "the test suite passes",
            true,
        ));

        let subject = crate::ArtifactRevision::new("patch", "r1");
        runtime.set_artifact_revision(subject.clone());

        runtime
            .command(SessionCommand::Submit {
                prompt: "original task".to_owned(),
            })
            .await
            .unwrap();
        // One turn of generation: a turn result, not completion.
        let state = runtime.drive().await.unwrap();
        assert_eq!(state, TaskState::Running);

        runtime
            .command(SessionCommand::Steer {
                prompt: "steered task".to_owned(),
            })
            .await
            .unwrap();

        // The event stream records the steering and the revision bump.
        let steered: Vec<_> = runtime
            .events()
            .events()
            .filter(|e| matches!(e, SessionEvent::TaskSteered { revision, .. } if revision.0 >= 2))
            .collect();
        assert_eq!(steered.len(), 1);

        // Fresh evidence for the revision; the next decision runs against
        // the steered prompt and completes.
        runtime.seed_evidence(Evidence {
            check: "tests".to_owned(),
            subject,
            produced_at: "t1".to_owned(),
            passed: true,
            detail: json!({}),
        });
        let state = drive_until_stable(&mut runtime, 10).await.unwrap();
        assert_eq!(state, TaskState::Completed);
    }

    #[tokio::test]
    async fn generation_with_unsatisfied_requirements_does_not_complete() {
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["draft".to_owned(), "draft 2".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_requirements(CompletionRequirements::none().require(
            "tests",
            "the test suite passes",
            true,
        ));

        runtime.set_artifact_revision(crate::ArtifactRevision::new("patch", "r1"));

        runtime
            .command(SessionCommand::Submit {
                prompt: "fix the bug".to_owned(),
            })
            .await
            .unwrap();

        // Never completes: every drive generates and continues.
        let state = drive_until_stable(&mut runtime, MAX_TURNS_PER_TASK + 2)
            .await
            .unwrap();
        assert_eq!(state, TaskState::Failed);
        assert_eq!(runtime.task_state(), Some(TaskState::Failed));

        let events: Vec<&SessionEvent> = runtime.events().events().collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::TaskCompleted { .. }))
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, SessionEvent::TaskFailed { reason, .. } if reason.contains("turn budget"))));
    }

    #[tokio::test]
    async fn completion_requires_fresh_evidence_for_the_revision() {
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["done content".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_requirements(CompletionRequirements::none().require(
            "tests",
            "the test suite passes",
            true,
        ));

        let subject = crate::ArtifactRevision::new("patch", "r1");
        runtime.set_artifact_revision(subject.clone());

        // Stale evidence for another revision proves nothing.
        runtime.seed_evidence(Evidence {
            check: "tests".to_owned(),
            subject: crate::ArtifactRevision::new("patch", "r0"),
            produced_at: "t0".to_owned(),
            passed: true,
            detail: json!({}),
        });

        runtime
            .command(SessionCommand::Submit {
                prompt: "fix the bug".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 5).await.unwrap();
        assert_ne!(state, TaskState::Completed);

        // Fresh passing evidence for the exact revision completes.
        runtime.seed_evidence(Evidence {
            check: "tests".to_owned(),
            subject,
            produced_at: "t1".to_owned(),
            passed: true,
            detail: json!({}),
        });

        // Reset the task (previous one failed by budget) and rerun.
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["done content".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        )
        .with_requirements(CompletionRequirements::none().require(
            "tests",
            "the test suite passes",
            true,
        ));
        runtime.set_artifact_revision(crate::ArtifactRevision::new("patch", "r1"));
        runtime.seed_evidence(Evidence {
            check: "tests".to_owned(),
            subject: crate::ArtifactRevision::new("patch", "r1"),
            produced_at: "t1".to_owned(),
            passed: true,
            detail: json!({}),
        });
        runtime
            .command(SessionCommand::Submit {
                prompt: "fix the bug".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 5).await.unwrap();
        assert_eq!(state, TaskState::Completed);
    }

    #[tokio::test]
    async fn act_without_capability_is_explicit_discovery_not_execution() {
        struct ActNoCap;

        #[async_trait]
        impl SystemOne for ActNoCap {
            async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
                Ok(Decision {
                    route: Route::Act,
                    confidence: 0.95,
                    retrieval: None,
                    capability: None,
                    model_tier: ModelTier::Fast,
                    risk: Risk::Low,
                    parallelizable: false,
                })
            }
        }

        let mut runtime = runtime_with(
            Arc::new(ActNoCap),
            vec![],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "do the thing".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 5).await.unwrap();

        // No candidates: waits for the user rather than guessing.
        assert_eq!(state, TaskState::Waiting);
        assert!(matches!(
            runtime.pending_wait().unwrap().kind,
            WaitKind::Question
        ));
    }

    #[tokio::test]
    async fn same_event_transcript_drives_headless_consumers() {
        // The transcript is pure data: serializable, ordered, with stable
        // identifiers. A headless consumer replays it without executing
        // anything.
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec!["the answer".to_owned()],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "summarize".to_owned(),
            })
            .await
            .unwrap();
        drive_until_stable(&mut runtime, 10).await.unwrap();

        let transcript: Vec<SessionEvent> = runtime.events().events().cloned().collect();
        let serialized = serde_json::to_string(&transcript).unwrap();
        let replayed: Vec<SessionEvent> = serde_json::from_str(&serialized).unwrap();
        assert_eq!(transcript, replayed);

        // Identifiers are stable and unique.
        let task_ids: Vec<TaskId> = replayed.iter().filter_map(|e| e.task()).collect();
        assert!(!task_ids.is_empty());
        assert!(task_ids.iter().all(|t| *t == task_ids[0]));
    }

    #[tokio::test]
    async fn blocking_without_approval_key_fails_explicitly() {
        // Policy denies writes outright: the plan blocks, no approval is
        // possible, the task fails with the blocked node named.
        let mut runtime = runtime_with(
            Arc::new(ActFiles),
            vec![plan_json("write")],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly), // writes denied
        );

        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 10).await.unwrap();
        assert_eq!(state, TaskState::Failed);
    }

    #[tokio::test]
    async fn empty_prompt_submit_is_rejected() {
        let mut runtime = runtime_with(
            Arc::new(AlwaysGenerate),
            vec![],
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        );

        assert!(
            runtime
                .command(SessionCommand::Submit {
                    prompt: "   ".to_owned()
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn fast_repliers_cannot_cause_unbounded_growth() {
        let mut log = EventLog::new();
        // A producer far faster than the consumer: 100k events.
        for i in 0..100_000 {
            log.push(SessionEvent::Resumed {
                task: TaskId(i as u64),
            });
        }
        assert!(log.len() <= EVENT_LOG_CAPACITY);
    }

    #[tokio::test]
    async fn verify_failure_observed_leads_to_explicit_failure() {
        // The plan's verify node fails (artifact is json but the model
        // returns text): the task fails with the failing node named.
        struct TextReasoner;

        #[async_trait]
        impl crate::Model for TextReasoner {
            fn identity(&self) -> ModelIdentity {
                ModelIdentity {
                    provider: "fake".to_owned(),
                    model: "text".to_owned(),
                    tier: ModelTier::Reasoner,
                }
            }

            async fn complete(&self, _r: &ModelRequest) -> Result<ModelResponse, KnutError> {
                Ok(ModelResponse::text(
                    "not json".to_owned(),
                    self.identity(),
                    Usage::default(),
                    std::time::Duration::ZERO,
                ))
            }
        }

        let router = Arc::new(Knut::new(Arc::new(ActFiles)));
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(TextReasoner));
        let planner = Planner::new(Arc::clone(&cascade));
        let gate = Arc::new(ExecutionGate::new(
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        ));
        let mut runtime = SessionRuntime::new(
            router,
            planner,
            registry(),
            gate,
            cascade,
            Arc::new(AcceptAll),
        );

        // The planner's model returns "not json", so planning itself
        // fails: an explicit PlanRejected failure, never a silent
        // success.
        runtime
            .command(SessionCommand::Submit {
                prompt: "files".to_owned(),
            })
            .await
            .unwrap();
        let state = drive_until_stable(&mut runtime, 10).await.unwrap();
        assert_eq!(state, TaskState::Failed);
        assert!(runtime.events().events().any(
            |e| matches!(e, SessionEvent::TaskFailed { reason, .. } if reason.contains("plan"))
        ));
    }
}
