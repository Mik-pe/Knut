pub use knut_auth as openai_auth;
pub use knut_editor as composer;
mod bench;
pub mod calibration;
pub mod cards;
mod census;
mod completion;
mod concurrency;
mod context;
pub mod decision;
mod edge;
pub mod engine;
mod error;
mod evals;
mod frame;
mod harness;
pub mod headless;
mod judgment;
mod lsp;
mod matrix;
mod mcp;
pub mod model;
mod patch;
pub mod persist;
mod planner;
mod policy;
mod profiles;
mod provider;
mod recovery;
pub mod review;
mod runtime;
mod sandbox;
mod self_update;
pub mod session;
mod system_one;
mod system_zero;
mod tool;
pub mod tree;
mod typesafe;
pub mod verify;
mod workers;
pub mod workspace;

pub use bench::{
    Arm, ArmSummary, BenchReport, BenchTask, PairedComparison, REPORT_VERSION, RunMode,
    RunVersions, TaskKind, TaskRun, TaskWorkspace, pilot_suite, run_task_check,
};
pub use calibration::{
    BoundedRoutingCache, BreakerState, CacheKey, CacheStats, CachedDecision, CalibrationEvidence,
    CalibrationMismatch, CalibrationReport, CircuitBreaker, DEFAULT_CACHE_CAPACITY,
    DecisionObservation, DecisionType, InFlightCoalescer, PolicyCalibration, PolicyStatus,
    ScoreDecision, Scores, Thresholds, classify,
};
pub use cards::{
    ActionCard, CardList, CardState, MAX_CARD_DETAIL, MAX_CARDS, sanitize_for_display,
    summarize_value,
};
pub use census::{CallOutcome, CallPurpose, ModelCallRecord, ModelCallReport, capture_model_calls};
pub use completion::{
    ArtifactRevision, ArtifactVerifier, CompletionRequirements, Evidence, Requirement,
    gather_evidence,
};
pub use composer::{Composer, MAX_COMPOSER_CHARS, MAX_HISTORY, MAX_PASTE_CHARS};
pub use concurrency::{
    Budget, BudgetSnapshot, ItemOutcome, ItemResult, ItemRunner, PoolCapacity, ResourceClass,
    ScheduleOutcome, Scheduler, SkipReason, WorkItem, items_from_plan,
};
pub use context::{
    AcceptanceRequirement, ArtifactIndex, Constraint, ContextReport, DecisionProjection,
    DiagnosticRef, MAX_FRAME_CANDIDATES, MAX_FRAME_TEXT, ReasonerProjection, SourceExcerpt,
    TokenBudget, budget_turn, fits_budget,
};
pub use decision::{Action, Decision, DecisionInput, ModelTier, RetrievalSource, Risk, Route};
pub use edge::{
    EdgeChoice, EdgeJudgment, EdgeRouter, EdgeSelector, EdgeState, MAX_STEP_ATTEMPTS, NodeOutcome,
};
pub use engine::{
    ConnectionChange, ConnectionRequest, Engine, EngineReport, action_label, build_harness,
    build_here, build_with_write_approval, run_engine, run_engine_with_connections, source_label,
};
pub use error::{KnutError, SystemOneFailure, redacted};
pub use evals::{
    Benchmark, BenchmarkComparison, BenchmarkTask, CostModel, Expectation, Metrics,
    ShadowSystemOne, TraceLog, TurnOutcome, TurnTrace,
};
pub use frame::{
    Candidate, CandidateChoice, DecisionFrame, ESCALATE_ID, FRAME_VERSION, FrameKind, FrameRouter,
    MAX_CANDIDATES, StaticFrameRouter, decide_candidate, decide_continuation,
    validate_candidate_answer,
};
pub use harness::{
    ASK_USER_TOOL_ID, AskUserTool, CompletionMonitor, ContextProvider, ContextRead, ContextRecord,
    HarnessSetup, ResourceRef, TaskOptions,
};
pub use headless::{
    ACP_METHODS, ACP_PROTOCOL_VERSION, ACP_UNIMPLEMENTED_METHODS, AcpAdapter, AcpAgentCapabilities,
    AcpInitialize, AcpStopReason, ApprovalRefusal, HeadlessAdapter, HeadlessCommand, HeadlessEvent,
    HeadlessOutcome, HeadlessSession, JSONL_PROTOCOL_VERSION, MAX_LINE_BYTES, PendingPermissions,
    ProtocolError, ProtocolFailures, acp_dispatch, acp_update, classify_engine_error, diagnostic,
    is_protocol_line, normalize_protocol_path, to_jsonl,
};
pub use judgment::{
    Complexity, Handler, IngressJudgments, Judgment, JudgmentRouter, JudgmentSystemOne,
    RetrievalJudgment, StaticJudgments, TierJudgment, YesNo,
};
pub use lsp::{
    Degradation, Diagnostic, DiagnosticSeverity, DiagnosticsState, DocumentVersions,
    LSP_PROTOCOL_VERSION, LanguageServerManager, Location, LspFeature, MAX_NAVIGATION_RESULTS,
    Navigation, Position, PositionEncoding, SUPPORTED_FEATURES, ServerCapabilities, ServerConfig,
    ServerEdit, ServerEditRejected, ServerUnavailable, initialize_request, navigation_candidates,
};
pub use matrix::{
    ApprovedSwitch, CAPABILITIES, CapabilityRow, PricingRecord, PricingTable, ProfileConfig,
    ProviderMatrix, ProviderProfile, ReasoningField, SHARED_CONFORMANCE_FIXTURES, SupportLevel,
    SwitchRefusal, TurnState, adapter_for, all_matrices, all_profiles, can_switch, deepseek_matrix,
    glm_matrix, openai_matrix, render_matrix, switch_provider,
};
pub use mcp::{
    ACCEPTED_REVISIONS, ConfigSource, DeclaredSideEffect, DiscoveredTool, ImportRejection,
    ImportedTool, MAX_RESULT_BYTES, MAX_SKILL_CHARS, MAX_TOOLS_PER_SERVER, MCP_PROTOCOL_REVISION,
    McpServerConfig, McpSessionView, McpTool, ServerState, ServerStatus, SkillMaterial,
    StartupRefusal, Transport, bounded_candidates, changed_schemas, import_tools,
    register_mcp_tools,
};
pub use model::{
    BufferedSink, CascadeOutcome, ComputeCascade, Continuation, ContinuationPart, ExpectedArtifact,
    Model, ModelAttempt, ModelCapabilities, ModelExchange, ModelIdentity, ModelRequest,
    ModelResponse, ModelStreamEvent, ModelStreamSink, ToolCall, ToolResult, Usage,
    VerificationVerdict, Verifier,
};
pub use patch::{
    AppliedOp, ApplyFailure, ApplyOutcome, ApplyPatchTool, MAX_PATCH_OPS, Patch, PatchApplier,
    PatchOp, PatchRejection, RevertOutcome, SkippedRevert, ValidatedOp, ValidatedPatch,
    validate_patch,
};
pub use persist::{
    OperationRecord, PersistedOutcome, ResumeBlocker, ResumePlan, SCHEMA_VERSION, SessionExport,
    SessionStore, SessionSummary, redact_path, session_store_path,
};
pub use planner::{MAX_DEPTH, MAX_NODES, PlanRejection, Planner, PlanningContext, ValidatedPlan};
pub use policy::{
    ApprovalLedger, Authorization, ContentPreconditions, ExecOutcome, ExecutionGate,
    InMemoryJournal, JournalEntry, JournalStore, OutcomeState, Policy, PolicyVerdict,
    ProposedExecution, SideEffectPolicy,
};
pub use profiles::{WorkspaceProfile, workspace_setup};
pub use provider::{
    BillingPath, DEFAULT_CHAT_PATH, ProviderConfig, ProviderModel, ProviderSummary,
    ProviderTransport, ReasoningEffort,
};
pub use review::{
    ApprovalChoice, ApprovalError, ApprovalState, ApprovalView, ChangeKind, ChangeSet, CheckRow,
    FileChange, Hunk, HunkSelection, MAX_FILES, MAX_HUNK_LINES, ReviewFocus, ReviewView,
    RevisedProposal, diff_hunks, revise_proposal,
};
pub use runtime::{DecisionSource, Knut, Routed};
pub use sandbox::{
    BoundedOutput, CAPABILITY as SHELL_CAPABILITY, CommandOutcome, CommandRequest, CommandStatus,
    RunCommandTool, SandboxBackend, SandboxSpec, SandboxUnavailable, Supervisor,
    register_command_tools, sanitize_terminal,
};
pub use session::{
    EVENT_LOG_CAPACITY, MAX_REPLANS_PER_TASK, MAX_TURNS_PER_TASK, QueuedRequest,
    SESSION_PROTOCOL_VERSION, SessionCommand, SessionEvent, SessionRuntime, TaskId, TaskRevision,
    TaskState, TurnId, WaitKind, drive_until_stable,
};
pub use system_one::{StaticSystemOne, SystemOne};
pub use system_zero::{
    ExplicitCapabilityRule, InvalidInputRule, RoutingCache, RuleVerdict, SystemZero,
    SystemZeroOutcome, SystemZeroRule, UnavailableCapabilityRule,
};
pub use tool::{
    SchemaError, SideEffect, Tool, ToolMetadata, ToolRegistry, validate_arguments,
    validate_schema_supported,
};
pub use tree::{
    ArtifactKind, ArtifactRef, ArtifactStore, AskUserHandler, CancelFlag, NodeStatus, PlanError,
    PlanNode, TreeExecutor, TreeRunResult, collect_refs, resolve_input, validate_plan,
};
pub use typesafe::{
    Answer, DEFAULT_BASE_URL, DEFAULT_MODEL, DEFAULT_TIMEOUT, JevSystemOne, Question,
    SystemOneRequest, SystemOneResponse, TypeSafeConfig,
};
pub use verify::{
    AcceptAllVerifier, CheckEvidence, CheckOutcome, CheckProfile, CheckRunner, CheckSpec,
    EvidenceReport, ReviewOutcome, ReviewVerdict, TestCounts, TestSetChange, detect_weakened_tests,
    discover_profiles, parse_review, review_change,
};
pub use workers::{
    CleanupOutcome, ContractRejection, ContractTemplate, DelegationComparison, DelegationContract,
    DelegationRefused, IntegrationPlan, MAX_CONCURRENT_WORKERS, MAX_WORKERS_PER_SESSION,
    PatchConflict, Worker, WorkerKind, WorkerPool, WorkerResult, WorkerState, contract_templates,
    delegation_candidates, reconcile,
};
pub use workspace::{
    ContentRef, INSTRUCTION_FILES, InstructionFile, ListTool, MAX_INSTRUCTION_BYTES, ReadTool,
    SearchTool, Workspace, WriteTool, content_hash, discover_instructions,
    register_workspace_tools,
};

pub use self_update::register_self_update_tools;
