//! Bounded subagents with isolated workspaces (issue #38).
//!
//! A reasoner may delegate genuinely independent work to a child, but the
//! delegation is an explicit *contract*: goal, allowed context and tools,
//! expected artifacts, acceptance checks and budget. Jev may choose among
//! known contracts or decline; it never invents a subagent's instructions.
//!
//! Rules carried through:
//! - **read-only first.** Investigation and review workers are the
//!   default. A write worker exists only in its own isolated tree with a
//!   recorded source revision, and a worktree is *not* a sandbox: #25's
//!   execution restrictions still apply inside it.
//! - **children cannot expand anything.** Shared parent/session limits, a
//!   small concurrency cap, no recursive spawning, and no tool, provider
//!   or egress permission a child was not granted.
//! - **the parent owns integration.** A child returns evidence or a
//!   proposed patch; completion does not merge code. Conflicting patches
//!   need explicit reconciliation and fresh combined checks.
//! - **cancellation reaches children.** Cancelling the parent stops queued
//!   and active children, so no hidden background spend continues.
//! - **cleanup is safe.** A crash does not silently delete a dirty
//!   directory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::KnutError;

/// What a child is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    /// Investigate and report evidence; no writes anywhere.
    Investigation,
    /// Review a proposed change; no writes anywhere.
    Review,
    /// Make changes, only inside its own isolated tree.
    WriteInIsolatedTree,
}

impl WorkerKind {
    pub fn label(self) -> &'static str {
        match self {
            WorkerKind::Investigation => "investigation (read-only)",
            WorkerKind::Review => "review (read-only)",
            WorkerKind::WriteInIsolatedTree => "write (isolated tree)",
        }
    }

    pub fn writes(self) -> bool {
        matches!(self, WorkerKind::WriteInIsolatedTree)
    }
}

/// A bounded delegation contract.
///
/// This is the *only* thing a child may be given: a reasoner writes it, the
/// parent approves it, and the child cannot exceed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationContract {
    /// Who the reasoner says this child is for, for the task view.
    pub purpose: String,
    pub kind: WorkerKind,
    /// The goal, bounded.
    pub goal: String,
    /// Workspace-relative files or directories the child may see.
    pub allowed_paths: Vec<String>,
    /// Tool ids the child may call. Anything else is refused.
    pub allowed_tools: Vec<String>,
    /// What the child must produce.
    pub expected_artifacts: Vec<String>,
    /// Checks that decide whether the child succeeded.
    pub acceptance_checks: Vec<String>,
    /// Token budget for this child only.
    pub token_budget: u64,
    /// Wall-clock budget.
    pub time_budget_secs: u64,
    /// Capability id for the child's work; it cannot reach any other.
    pub capability: String,
    /// Provider/model the parent permits. A child may not use another.
    pub provider: String,
}

/// Maximum characters in a contract's free-text fields.
pub const MAX_CONTRACT_TEXT: usize = 600;
/// Maximum allowed paths in one contract.
pub const MAX_ALLOWED_PATHS: usize = 32;
/// Maximum allowed tools in one contract.
pub const MAX_ALLOWED_TOOLS: usize = 16;
/// Maximum children one parent may run at once.
pub const MAX_CONCURRENT_WORKERS: usize = 3;
/// Maximum children per session, so cost cannot grow without bound.
pub const MAX_WORKERS_PER_SESSION: usize = 8;

impl DelegationContract {
    pub fn new(
        purpose: impl Into<String>,
        kind: WorkerKind,
        goal: impl Into<String>,
        capability: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            purpose: bound(purpose.into(), MAX_CONTRACT_TEXT),
            kind,
            goal: bound(goal.into(), MAX_CONTRACT_TEXT),
            allowed_paths: Vec::new(),
            allowed_tools: Vec::new(),
            expected_artifacts: Vec::new(),
            acceptance_checks: Vec::new(),
            token_budget: 50_000,
            time_budget_secs: 600,
            capability: capability.into(),
            provider: provider.into(),
        }
    }

    pub fn with_paths(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_paths = paths
            .into_iter()
            .map(Into::into)
            .take(MAX_ALLOWED_PATHS)
            .collect();
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_tools = tools
            .into_iter()
            .map(Into::into)
            .take(MAX_ALLOWED_TOOLS)
            .collect();
        self
    }

    pub fn with_artifacts(
        mut self,
        artifacts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.expected_artifacts = artifacts
            .into_iter()
            .map(|a| bound(a.into(), MAX_CONTRACT_TEXT))
            .collect();
        self
    }

    pub fn with_checks(mut self, checks: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.acceptance_checks = checks
            .into_iter()
            .map(|c| bound(c.into(), MAX_CONTRACT_TEXT))
            .collect();
        self
    }

    pub fn with_budget(mut self, tokens: u64, secs: u64) -> Self {
        self.token_budget = tokens;
        self.time_budget_secs = secs;
        self
    }

    /// Whether the contract is complete enough to delegate.
    pub fn validate(&self) -> Result<(), ContractRejection> {
        if self.purpose.trim().is_empty() {
            return Err(ContractRejection::EmptyPurpose);
        }
        if self.goal.trim().is_empty() {
            return Err(ContractRejection::EmptyGoal);
        }
        if self.capability.trim().is_empty() {
            return Err(ContractRejection::EmptyCapability);
        }
        if self.provider.trim().is_empty() {
            return Err(ContractRejection::EmptyProvider);
        }
        if self.expected_artifacts.is_empty() {
            // A child with no expected artifact cannot be reviewed.
            return Err(ContractRejection::NoExpectedArtifacts);
        }
        if self.token_budget == 0 {
            return Err(ContractRejection::ZeroBudget);
        }
        if self.kind.writes() && self.allowed_paths.is_empty() {
            // A write worker with no declared scope could touch anything.
            return Err(ContractRejection::WriteWithoutScope);
        }
        Ok(())
    }

    /// Whether a capability is within the contract.
    pub fn allows_capability(&self, capability: &str) -> bool {
        self.capability == capability
    }

    /// Whether a tool is within the contract.
    pub fn allows_tool(&self, tool_id: &str) -> bool {
        self.allowed_tools.iter().any(|allowed| allowed == tool_id)
    }

    /// Whether a path is within the contract's allowed context.
    pub fn allows_path(&self, path: &str) -> bool {
        if self.allowed_paths.is_empty() {
            return false;
        }
        self.allowed_paths.iter().any(|allowed| {
            path == allowed || path.starts_with(&format!("{allowed}/")) || allowed == "."
        })
    }

    /// Whether a provider is permitted.
    pub fn allows_provider(&self, provider: &str) -> bool {
        self.provider == provider
    }
}

/// Why a contract cannot be delegated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractRejection {
    EmptyPurpose,
    EmptyGoal,
    EmptyCapability,
    EmptyProvider,
    NoExpectedArtifacts,
    ZeroBudget,
    WriteWithoutScope,
}

impl std::fmt::Display for ContractRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContractRejection::EmptyPurpose => write!(f, "the delegation has no stated purpose"),
            ContractRejection::EmptyGoal => write!(f, "the delegation has no goal"),
            ContractRejection::EmptyCapability => {
                write!(f, "the delegation names no capability")
            }
            ContractRejection::EmptyProvider => write!(f, "the delegation names no provider"),
            ContractRejection::NoExpectedArtifacts => write!(
                f,
                "the delegation expects no artifacts, so its result cannot be reviewed"
            ),
            ContractRejection::ZeroBudget => write!(f, "the delegation has no budget"),
            ContractRejection::WriteWithoutScope => {
                write!(f, "a write worker must declare the paths it may change")
            }
        }
    }
}

/// A child's state, for the task view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Queued,
    Running {
        action: String,
    },
    /// Returned evidence or a proposed patch.
    Completed {
        artifacts: Vec<String>,
    },
    Failed {
        reason: String,
    },
    Cancelled,
}

impl WorkerState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            WorkerState::Completed { .. } | WorkerState::Failed { .. } | WorkerState::Cancelled
        )
    }

    pub fn label(&self) -> String {
        match self {
            WorkerState::Queued => "queued".to_owned(),
            WorkerState::Running { action } => format!("running: {action}"),
            WorkerState::Completed { artifacts } => {
                format!("completed ({} artifact(s))", artifacts.len())
            }
            WorkerState::Failed { reason } => format!("failed: {reason}"),
            WorkerState::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// A child's result, returned to the parent.
///
/// Either evidence (read-only work) or a proposed patch. Notably absent: a
/// merged change. Integration is the parent's job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerResult {
    /// Evidence from investigation or review.
    Evidence {
        summary: String,
        references: Vec<String>,
    },
    /// A patch proposal for the parent to review.
    ProposedPatch {
        /// Patch identity, so the parent can reconcile conflicts.
        identity: String,
        summary: String,
        /// Files the proposal would change.
        paths: Vec<String>,
        /// The revision the child worked from.
        base_revision: String,
    },
}

impl WorkerResult {
    /// The paths this result concerns.
    pub fn paths(&self) -> &[String] {
        match self {
            WorkerResult::Evidence { .. } => &[],
            WorkerResult::ProposedPatch { paths, .. } => paths,
        }
    }

    /// Whether the result proposes a change that needs integration.
    pub fn proposes_change(&self) -> bool {
        matches!(self, WorkerResult::ProposedPatch { .. })
    }
}

/// One child worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Worker {
    /// Stable child id.
    pub id: String,
    pub purpose: String,
    pub kind: WorkerKind,
    pub state: WorkerState,
    pub contract: DelegationContract,
    /// Isolated working tree for a write worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The revision the child started from.
    pub base_revision: String,
    /// Tokens the child has consumed.
    pub tokens_used: u64,
    /// The parent's task revision when the child was created: if the
    /// parent's revision advances, this child's results are stale.
    pub parent_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<WorkerResult>,
}

impl Worker {
    /// Whether the child stayed inside its contract.
    pub fn permits(&self, capability: &str, tool_id: &str, provider: &str) -> bool {
        self.contract.allows_capability(capability)
            && self.contract.allows_tool(tool_id)
            && self.contract.allows_provider(provider)
    }

    /// Whether the child is stale relative to the parent's revision.
    pub fn is_stale(&self, parent_revision: u64) -> bool {
        self.parent_revision != parent_revision
    }
}

/// Why a delegation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationRefused {
    Invalid(ContractRejection),
    /// The concurrency cap is reached.
    ConcurrencyCap {
        cap: usize,
    },
    /// The session-wide worker cap is reached.
    SessionCap {
        cap: usize,
    },
    /// The requested budget exceeds what the parent has left.
    InsufficientBudget {
        requested: u64,
        remaining: u64,
    },
    /// Recursive delegation is not implemented.
    RecursiveNotSupported,
    /// The parent is cancelled.
    ParentCancelled,
}

impl std::fmt::Display for DelegationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelegationRefused::Invalid(rejection) => write!(f, "{rejection}"),
            DelegationRefused::ConcurrencyCap { cap } => write!(
                f,
                "at most {cap} worker(s) may run at once; wait for one to finish"
            ),
            DelegationRefused::SessionCap { cap } => {
                write!(f, "this session has already started {cap} worker(s)")
            }
            DelegationRefused::InsufficientBudget {
                requested,
                remaining,
            } => write!(
                f,
                "the delegation asks for {requested} tokens but only {remaining} remain"
            ),
            DelegationRefused::RecursiveNotSupported => write!(
                f,
                "a child may not spawn its own workers in this implementation"
            ),
            DelegationRefused::ParentCancelled => {
                write!(f, "the parent task is cancelled")
            }
        }
    }
}

/// The parent's worker supervisor.
///
/// Owns the caps, the shared budget, the isolated trees and the
/// parent-revision binding that makes stale child results detectable.
pub struct WorkerPool {
    workers: Vec<Worker>,
    concurrent_cap: usize,
    session_cap: usize,
    started: usize,
    /// The parent's token budget, shared with children.
    parent_token_budget: u64,
    /// The parent's task revision.
    parent_revision: u64,
    cancelled: bool,
    next_id: u64,
    /// Root directory for isolated trees.
    tree_root: PathBuf,
    /// Whether this pool belongs to a child (recursion is refused).
    is_child_pool: bool,
}

impl WorkerPool {
    /// A pool for a parent task.
    pub fn new(parent_token_budget: u64, tree_root: impl Into<PathBuf>) -> Self {
        Self {
            workers: Vec::new(),
            concurrent_cap: MAX_CONCURRENT_WORKERS,
            session_cap: MAX_WORKERS_PER_SESSION,
            started: 0,
            parent_token_budget,
            parent_revision: 1,
            cancelled: false,
            next_id: 1,
            tree_root: tree_root.into(),
            is_child_pool: false,
        }
    }

    pub fn with_caps(mut self, concurrent: usize, session: usize) -> Self {
        self.concurrent_cap = concurrent.max(1);
        self.session_cap = session.max(1);
        self
    }

    pub fn workers(&self) -> &[Worker] {
        &self.workers
    }

    pub fn get(&self, id: &str) -> Option<&Worker> {
        self.workers.iter().find(|worker| worker.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Worker> {
        self.workers.iter_mut().find(|worker| worker.id == id)
    }

    /// Tokens the children have consumed.
    pub fn tokens_used(&self) -> u64 {
        self.workers.iter().map(|worker| worker.tokens_used).sum()
    }

    /// Tokens still available to delegate.
    pub fn remaining_budget(&self) -> u64 {
        self.parent_token_budget.saturating_sub(self.tokens_used())
    }

    /// How many children are active.
    pub fn active(&self) -> usize {
        self.workers
            .iter()
            .filter(|worker| !worker.state.is_terminal())
            .count()
    }

    /// The parent's revision changed: every earlier child result is stale.
    pub fn parent_revision_advanced(&mut self, revision: u64) {
        self.parent_revision = revision;
        // A stale child is stopped: its work was for a superseded task.
        for worker in &mut self.workers {
            if !worker.state.is_terminal() {
                worker.state = WorkerState::Cancelled;
            }
        }
    }

    /// Cancel every child (the parent was cancelled or paused).
    pub fn cancel_all(&mut self) {
        self.cancelled = true;
        for worker in &mut self.workers {
            if !worker.state.is_terminal() {
                worker.state = WorkerState::Cancelled;
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Delegate one bounded piece of work.
    pub fn delegate(
        &mut self,
        contract: DelegationContract,
        base_revision: impl Into<String>,
    ) -> Result<String, DelegationRefused> {
        contract.validate().map_err(DelegationRefused::Invalid)?;

        if self.is_child_pool {
            return Err(DelegationRefused::RecursiveNotSupported);
        }
        if self.cancelled {
            return Err(DelegationRefused::ParentCancelled);
        }
        if self.active() >= self.concurrent_cap {
            return Err(DelegationRefused::ConcurrencyCap {
                cap: self.concurrent_cap,
            });
        }
        if self.started >= self.session_cap {
            return Err(DelegationRefused::SessionCap {
                cap: self.session_cap,
            });
        }
        let remaining = self.remaining_budget();
        if contract.token_budget > remaining {
            return Err(DelegationRefused::InsufficientBudget {
                requested: contract.token_budget,
                remaining,
            });
        }

        let id = format!("worker-{}", self.next_id);
        self.next_id += 1;
        self.started += 1;

        // A write worker gets its own isolated tree, created up front so
        // the parent can inspect it.
        let worktree = if contract.kind.writes() {
            let path = self.tree_root.join(&id);
            std::fs::create_dir_all(&path)
                .map_err(|_| DelegationRefused::Invalid(ContractRejection::WriteWithoutScope))
                .ok();
            Some(path.to_string_lossy().into_owned())
        } else {
            None
        };

        self.workers.push(Worker {
            id: id.clone(),
            purpose: contract.purpose.clone(),
            kind: contract.kind,
            state: WorkerState::Running {
                action: "starting".to_owned(),
            },
            contract,
            worktree,
            base_revision: base_revision.into(),
            tokens_used: 0,
            parent_revision: self.parent_revision,
            result: None,
        });
        Ok(id)
    }

    /// Record a child's current action, for the task view.
    pub fn record_action(&mut self, id: &str, action: impl Into<String>) {
        if let Some(worker) = self.get_mut(id) {
            worker.state = WorkerState::Running {
                action: action.into(),
            };
        }
    }

    /// Record a child's consumption against the shared budget.
    pub fn record_usage(&mut self, id: &str, tokens: u64) {
        if let Some(worker) = self.get_mut(id) {
            worker.tokens_used = worker.tokens_used.saturating_add(tokens);
        }
    }

    /// Check an action against a child's contract.
    ///
    /// A child cannot invoke an ungranted tool, reach another capability or
    /// send context to another provider.
    pub fn authorizes(&self, id: &str, capability: &str, tool_id: &str, provider: &str) -> bool {
        self.get(id)
            .map(|worker| worker.permits(capability, tool_id, provider))
            .unwrap_or(false)
    }

    /// Complete a child with its result.
    ///
    /// The result is returned to the parent; nothing is merged.
    pub fn complete(&mut self, id: &str, result: WorkerResult) -> Result<(), DelegationRefused> {
        let Some(worker) = self.get_mut(id) else {
            return Err(DelegationRefused::Invalid(ContractRejection::EmptyPurpose));
        };
        if worker.state.is_terminal() {
            return Err(DelegationRefused::ParentCancelled);
        }
        // A read-only worker must not return a patch: that would be a
        // write by another name.
        if !worker.kind.writes() && result.proposes_change() {
            worker.state = WorkerState::Failed {
                reason: "a read-only worker returned a patch proposal".to_owned(),
            };
            return Err(DelegationRefused::Invalid(
                ContractRejection::WriteWithoutScope,
            ));
        }
        let artifacts = match &result {
            WorkerResult::Evidence { references, .. } => references.clone(),
            WorkerResult::ProposedPatch { paths, .. } => paths.clone(),
        };
        worker.state = WorkerState::Completed { artifacts };
        worker.result = Some(result);
        Ok(())
    }

    /// Fail a child.
    pub fn fail(&mut self, id: &str, reason: impl Into<String>) {
        if let Some(worker) = self.get_mut(id) {
            worker.state = WorkerState::Failed {
                reason: reason.into(),
            };
        }
    }

    /// Results that the parent may consider, excluding stale ones.
    pub fn fresh_results(&self) -> Vec<&Worker> {
        self.workers
            .iter()
            .filter(|worker| {
                !worker.is_stale(self.parent_revision)
                    && matches!(worker.state, WorkerState::Completed { .. })
            })
            .collect()
    }

    /// Stale results, which the parent must not act on.
    pub fn stale_results(&self) -> Vec<&Worker> {
        self.workers
            .iter()
            .filter(|worker| {
                worker.is_stale(self.parent_revision)
                    && matches!(worker.state, WorkerState::Completed { .. })
            })
            .collect()
    }

    /// A one-line summary for the task view.
    pub fn view_summary(&self) -> String {
        let mut out = format!(
            "{} worker(s), {} active, {} token(s) used\n",
            self.workers.len(),
            self.active(),
            self.tokens_used()
        );
        for worker in &self.workers {
            out.push_str(&format!(
                "  {} [{}] {} — {}\n",
                worker.purpose,
                worker.kind.label(),
                worker.state.label(),
                worker.contract.goal.chars().take(60).collect::<String>()
            ));
        }
        out
    }

    /// Clean up an isolated tree after its work is integrated or rejected.
    ///
    /// A *dirty* tree is never deleted silently: it may hold work the
    /// parent has not seen.
    pub fn cleanup_tree(&self, id: &str) -> Result<CleanupOutcome, KnutError> {
        let Some(worker) = self.get(id) else {
            return Err(KnutError::Tool(format!("no worker {id:?}")));
        };
        let Some(worktree) = &worker.worktree else {
            return Ok(CleanupOutcome::NoTree);
        };
        let path = Path::new(worktree);
        if !path.exists() {
            return Ok(CleanupOutcome::AlreadyGone);
        }
        // "Dirty" here means the tree has files: a child that wrote
        // something may have produced unreviewed work.
        let entries = std::fs::read_dir(path)
            .map_err(|err| KnutError::Tool(format!("reading {worktree}: {err}")))?
            .count();
        if entries > 0 {
            return Ok(CleanupOutcome::KeptDirty {
                path: worktree.clone(),
            });
        }
        std::fs::remove_dir_all(path)
            .map_err(|err| KnutError::Tool(format!("removing {worktree}: {err}")))?;
        Ok(CleanupOutcome::Removed)
    }
}

/// What happened when a tree was cleaned up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOutcome {
    Removed,
    /// Kept because it holds work that has not been reviewed.
    KeptDirty {
        path: String,
    },
    AlreadyGone,
    NoTree,
}

/// A conflict between two proposed patches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchConflict {
    pub left: String,
    pub right: String,
    pub paths: Vec<String>,
}

/// Reconcile proposed patches from several children.
///
/// Two children proposing changes to the same file conflict and require
/// explicit reconciliation: the parent cannot merge them silently, and
/// fresh combined checks are required afterwards.
pub fn reconcile(
    results: &[(String, WorkerResult)],
) -> (Vec<(String, WorkerResult)>, Vec<PatchConflict>) {
    let mut accepted: Vec<(String, WorkerResult)> = Vec::new();
    let mut conflicts = Vec::new();
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();

    for (worker_id, result) in results {
        let mut conflict_paths = BTreeSet::new();
        for path in result.paths() {
            if let Some(owner) = claimed.get(path)
                && owner != worker_id
            {
                conflict_paths.insert(path.clone());
            }
        }
        if conflict_paths.is_empty() {
            for path in result.paths() {
                claimed.insert(path.clone(), worker_id.clone());
            }
            accepted.push((worker_id.clone(), result.clone()));
        } else {
            conflicts.push(PatchConflict {
                left: claimed
                    .get(conflict_paths.iter().next().unwrap())
                    .cloned()
                    .unwrap_or_default(),
                right: worker_id.clone(),
                paths: conflict_paths.into_iter().collect(),
            });
        }
    }

    (accepted, conflicts)
}

/// The combined checks a parent must run after integrating child work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationPlan {
    /// Patches accepted for integration, in order.
    pub accepted: Vec<String>,
    /// Conflicting patches needing explicit reconciliation.
    pub conflicts: Vec<PatchConflict>,
    /// Checks that must pass on the *combined* result.
    pub combined_checks: Vec<String>,
    /// A statement of what was and was not verified.
    pub note: String,
}

impl IntegrationPlan {
    /// Build an integration plan from child results.
    pub fn build(results: &[(String, WorkerResult)], acceptance_checks: Vec<String>) -> Self {
        let (accepted, conflicts) = reconcile(results);
        let note = if conflicts.is_empty() {
            format!(
                "{} proposal(s) accepted; combined checks must pass before the parent applies them",
                accepted.len()
            )
        } else {
            format!(
                "{} proposal(s) accepted and {} conflict(s) require explicit reconciliation; \
                 combined checks must pass on the reconciled result",
                accepted.len(),
                conflicts.len()
            )
        };
        Self {
            accepted: accepted.into_iter().map(|(id, _)| id).collect(),
            conflicts,
            combined_checks: acceptance_checks,
            note,
        }
    }

    /// Whether integration may proceed without human or model reconciliation.
    pub fn is_unambiguous(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Compare delegated work against doing it in the parent alone.
///
/// Reports total cost and quality alongside wall-clock, and refuses to
/// claim a win that the numbers do not support.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegationComparison {
    pub single_agent_wall_clock_ms: u64,
    pub delegated_wall_clock_ms: u64,
    pub single_agent_tokens: u64,
    pub delegated_tokens: u64,
    pub single_agent_quality: f64,
    pub delegated_quality: f64,
    pub workers: usize,
    /// A conservative reading of the comparison.
    pub verdict: String,
}

impl DelegationComparison {
    /// Assess a comparison, including when delegation lost.
    pub fn assess(
        single_agent_wall_clock_ms: u64,
        single_agent_tokens: u64,
        single_agent_quality: f64,
        delegated_wall_clock_ms: u64,
        delegated_tokens: u64,
        delegated_quality: f64,
        workers: usize,
    ) -> Self {
        let wall_clock_win = delegated_wall_clock_ms < single_agent_wall_clock_ms;
        let cost_win = delegated_tokens < single_agent_tokens;
        let quality_ok = delegated_quality >= single_agent_quality - 0.02;

        let verdict = if !quality_ok {
            "delegation lost on quality; do not use it for this fixture".to_owned()
        } else if wall_clock_win && cost_win {
            format!(
                "delegation was faster and cheaper ({}ms vs {}ms, {} vs {} tokens)",
                delegated_wall_clock_ms,
                single_agent_wall_clock_ms,
                delegated_tokens,
                single_agent_tokens
            )
        } else if wall_clock_win {
            format!(
                "delegation was faster but cost more ({} vs {} tokens); coordination did not pay \
                 for itself",
                delegated_tokens, single_agent_tokens
            )
        } else {
            format!(
                "delegation was not faster ({}ms vs {}ms) and cost {} vs {} tokens; for {} \
                 worker(s) the coordination cost exceeded the parallelism",
                delegated_wall_clock_ms,
                single_agent_wall_clock_ms,
                delegated_tokens,
                single_agent_tokens,
                workers
            )
        };

        Self {
            single_agent_wall_clock_ms,
            delegated_wall_clock_ms,
            single_agent_tokens,
            delegated_tokens,
            single_agent_quality,
            delegated_quality,
            workers,
            verdict,
        }
    }

    /// Whether the comparison supports using delegation for this fixture.
    pub fn supports_delegation(&self) -> bool {
        self.delegated_quality + 0.02 >= self.single_agent_quality
            && self.delegated_wall_clock_ms < self.single_agent_wall_clock_ms
            && self.delegated_tokens < self.single_agent_tokens
    }
}

/// Bound a string to a character limit.
fn bound(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    text.chars().take(max).collect()
}

/// Known delegation contracts a control layer may choose among.
///
/// A decision frame offers these by id; Jev selects or declines. It never
/// writes a new contract's instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractTemplate {
    pub id: String,
    pub description: String,
    pub kind: WorkerKind,
}

/// The initial, deliberately small template set.
pub fn contract_templates() -> Vec<ContractTemplate> {
    vec![
        ContractTemplate {
            id: "investigate".to_owned(),
            description: "Read-only investigation of a bounded area, returning evidence".to_owned(),
            kind: WorkerKind::Investigation,
        },
        ContractTemplate {
            id: "review".to_owned(),
            description: "Read-only review of a proposed change against the task".to_owned(),
            kind: WorkerKind::Review,
        },
        ContractTemplate {
            id: "implement-isolated".to_owned(),
            description:
                "Implement a bounded change in an isolated tree, returning a patch proposal"
                    .to_owned(),
            kind: WorkerKind::WriteInIsolatedTree,
        },
    ]
}

/// Candidates a decision frame may offer for delegation.
pub fn delegation_candidates() -> Vec<crate::frame::Candidate> {
    let mut candidates: Vec<crate::frame::Candidate> = contract_templates()
        .into_iter()
        .map(|template| crate::frame::Candidate::new(template.id, template.description))
        .collect();
    // Delegation must be declinable, like every other frame.
    candidates.push(crate::frame::Candidate::new(
        crate::frame::ESCALATE_ID,
        "Do this work in the parent instead of delegating",
    ));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> WorkerPool {
        let root = std::env::temp_dir().join(format!(
            "knut-workers-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        WorkerPool::new(100_000, root)
    }

    fn read_only_contract(purpose: &str) -> DelegationContract {
        DelegationContract::new(
            purpose,
            WorkerKind::Investigation,
            "find where the parser is defined",
            "files",
            "glm-5.3-flash",
        )
        .with_paths(["src"])
        .with_tools(["read", "search"])
        .with_artifacts(["a summary of the call sites"])
        .with_checks(["the summary names every call site"])
        .with_budget(5_000, 120)
    }

    #[test]
    fn a_contract_is_the_only_thing_a_child_receives() {
        let mut pool = pool();
        let contract = read_only_contract("find the parser");
        let id = pool.delegate(contract.clone(), "rev-1").unwrap();

        let worker = pool.get(&id).unwrap();
        assert_eq!(worker.contract.goal, contract.goal);
        assert_eq!(worker.contract.token_budget, 5_000);
        assert_eq!(worker.purpose, "find the parser");
        assert!(worker.state.label().contains("running"));
    }

    #[test]
    fn an_incomplete_contract_is_refused() {
        let mut pool = pool();
        let mut contract = read_only_contract("x");
        contract.expected_artifacts.clear();
        assert!(matches!(
            pool.delegate(contract, "rev-1").unwrap_err(),
            DelegationRefused::Invalid(ContractRejection::NoExpectedArtifacts)
        ));

        // A write worker must declare its scope.
        let write = DelegationContract::new(
            "edit",
            WorkerKind::WriteInIsolatedTree,
            "fix the bug",
            "files",
            "m",
        )
        .with_artifacts(["a patch"]);
        assert!(matches!(
            pool.delegate(write, "rev-1").unwrap_err(),
            DelegationRefused::Invalid(ContractRejection::WriteWithoutScope)
        ));
    }

    #[test]
    fn a_child_cannot_invoke_an_ungranted_tool_or_reach_another_provider() {
        let mut pool = pool();
        let id = pool.delegate(read_only_contract("scout"), "rev-1").unwrap();

        // Granted capability, tool and provider.
        assert!(pool.authorizes(&id, "files", "read", "glm-5.3-flash"));
        // An ungranted tool.
        assert!(!pool.authorizes(&id, "files", "apply_patch", "glm-5.3-flash"));
        // Another capability.
        assert!(!pool.authorizes(&id, "shell", "read", "glm-5.3-flash"));
        // Another provider: a child cannot send context elsewhere.
        assert!(!pool.authorizes(&id, "files", "read", "some-other-provider"));
    }

    #[test]
    fn two_write_workers_cannot_touch_the_same_parent_file() {
        let mut pool = pool();
        let contract = |purpose: &str| {
            DelegationContract::new(
                purpose,
                WorkerKind::WriteInIsolatedTree,
                "change the parser",
                "files",
                "m",
            )
            .with_paths(["src/parser.rs"])
            .with_tools(["apply_patch"])
            .with_artifacts(["a patch"])
        };

        let first = pool.delegate(contract("worker a"), "rev-1").unwrap();
        let second = pool.delegate(contract("worker b"), "rev-1").unwrap();

        // Both work in *isolated trees*, so neither mutates the parent.
        let first_tree = pool.get(&first).unwrap().worktree.clone().unwrap();
        let second_tree = pool.get(&second).unwrap().worktree.clone().unwrap();
        assert_ne!(first_tree, second_tree);

        // Their proposals conflict on the same logical file, and the
        // parent's integration plan says so rather than merging blindly.
        pool.complete(
            &first,
            WorkerResult::ProposedPatch {
                identity: "patch-a".to_owned(),
                summary: "a".to_owned(),
                paths: vec!["src/parser.rs".to_owned()],
                base_revision: "rev-1".to_owned(),
            },
        )
        .unwrap();
        pool.complete(
            &second,
            WorkerResult::ProposedPatch {
                identity: "patch-b".to_owned(),
                summary: "b".to_owned(),
                paths: vec!["src/parser.rs".to_owned()],
                base_revision: "rev-1".to_owned(),
            },
        )
        .unwrap();

        let results: Vec<(String, WorkerResult)> = pool
            .workers()
            .iter()
            .filter_map(|worker| {
                worker
                    .result
                    .as_ref()
                    .map(|result| (worker.id.clone(), result.clone()))
            })
            .collect();
        let plan = IntegrationPlan::build(&results, vec!["cargo test".to_owned()]);

        assert!(!plan.is_unambiguous());
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].paths, vec!["src/parser.rs".to_owned()]);
        assert!(plan.note.contains("reconciliation"));
        // Combined checks are required on the reconciled result.
        assert_eq!(plan.combined_checks, vec!["cargo test".to_owned()]);
    }

    #[test]
    fn a_read_only_worker_returning_a_patch_is_a_violation() {
        let mut pool = pool();
        let id = pool.delegate(read_only_contract("scout"), "rev-1").unwrap();

        let refused = pool.complete(
            &id,
            WorkerResult::ProposedPatch {
                identity: "p".to_owned(),
                summary: "sneaky".to_owned(),
                paths: vec!["src/a.rs".to_owned()],
                base_revision: "rev-1".to_owned(),
            },
        );
        assert!(refused.is_err());
        // The worker is marked failed, and no result is recorded.
        assert!(matches!(
            pool.get(&id).unwrap().state,
            WorkerState::Failed { .. }
        ));
        assert!(pool.get(&id).unwrap().result.is_none());
    }

    #[test]
    fn parent_cancellation_stops_queued_and_active_children() {
        let mut pool = pool();
        let ids: Vec<String> = (0..3)
            .map(|i| {
                pool.delegate(read_only_contract(&format!("worker {i}")), "rev-1")
                    .unwrap()
            })
            .collect();
        assert_eq!(pool.active(), 3);

        pool.cancel_all();
        assert!(pool.is_cancelled());
        for id in &ids {
            assert!(matches!(
                pool.get(id).unwrap().state,
                WorkerState::Cancelled
            ));
        }
        assert_eq!(pool.active(), 0);
        // And nothing new can be delegated.
        assert!(matches!(
            pool.delegate(read_only_contract("late"), "rev-1")
                .unwrap_err(),
            DelegationRefused::ParentCancelled
        ));
    }

    #[test]
    fn budget_exhaustion_stops_further_delegation() {
        let mut pool = WorkerPool::new(10_000, std::env::temp_dir().join("knut-workers-budget"));

        let contract = || read_only_contract("scout").with_budget(6_000, 60);
        pool.delegate(contract(), "rev-1").unwrap();
        pool.record_usage("worker-1", 6_000);
        assert_eq!(pool.remaining_budget(), 4_000);

        // A second worker asking for more than remains is refused with the
        // numbers named.
        let err = pool.delegate(contract(), "rev-1").unwrap_err();
        match err {
            DelegationRefused::InsufficientBudget {
                requested,
                remaining,
            } => {
                assert_eq!(requested, 6_000);
                assert_eq!(remaining, 4_000);
            }
            other => panic!("expected a budget refusal, got {other}"),
        }
    }

    #[test]
    fn concurrency_and_session_caps_are_enforced() {
        let mut pool = pool().with_caps(2, 3);
        pool.delegate(read_only_contract("a"), "rev-1").unwrap();
        pool.delegate(read_only_contract("b"), "rev-1").unwrap();

        let err = pool.delegate(read_only_contract("c"), "rev-1").unwrap_err();
        assert!(matches!(err, DelegationRefused::ConcurrencyCap { cap: 2 }));

        // Finishing one frees a slot.
        pool.complete(
            "worker-1",
            WorkerResult::Evidence {
                summary: "done".to_owned(),
                references: Vec::new(),
            },
        )
        .unwrap();
        let third = pool.delegate(read_only_contract("c"), "rev-1").unwrap();
        pool.complete(
            &third,
            WorkerResult::Evidence {
                summary: "done".to_owned(),
                references: Vec::new(),
            },
        )
        .unwrap();
        pool.complete(
            "worker-2",
            WorkerResult::Evidence {
                summary: "done".to_owned(),
                references: Vec::new(),
            },
        )
        .unwrap();

        // The session cap then applies.
        let err = pool.delegate(read_only_contract("d"), "rev-1").unwrap_err();
        assert!(matches!(err, DelegationRefused::SessionCap { cap: 3 }));
    }

    #[test]
    fn recursive_delegation_is_not_supported() {
        let mut pool = pool();
        pool.is_child_pool = true;
        let err = pool
            .delegate(read_only_contract("nested"), "rev-1")
            .unwrap_err();
        assert!(matches!(err, DelegationRefused::RecursiveNotSupported));
        assert!(format!("{err}").contains("may not spawn"));
    }

    #[test]
    fn steering_the_parent_invalidates_stale_child_results() {
        let mut pool = pool();
        let id = pool.delegate(read_only_contract("scout"), "rev-1").unwrap();
        pool.complete(
            &id,
            WorkerResult::Evidence {
                summary: "found it".to_owned(),
                references: vec!["src/parser.rs:10".to_owned()],
            },
        )
        .unwrap();
        assert_eq!(pool.fresh_results().len(), 1);

        // The parent is steered: revision 2.
        pool.parent_revision_advanced(2);
        // The child's result is now stale and must not be acted on.
        assert!(pool.fresh_results().is_empty());
        assert_eq!(pool.stale_results().len(), 1);
        assert!(pool.get(&id).unwrap().is_stale(2));
    }

    #[test]
    fn a_dirty_isolated_tree_is_never_deleted_silently() {
        let mut first_pool = pool();
        let contract = DelegationContract::new(
            "implement",
            WorkerKind::WriteInIsolatedTree,
            "fix the bug",
            "files",
            "m",
        )
        .with_paths(["src"])
        .with_tools(["apply_patch"])
        .with_artifacts(["a patch"]);

        let id = first_pool.delegate(contract, "rev-1").unwrap();

        // The tree is empty: cleanup removes it.
        assert_eq!(
            first_pool.cleanup_tree(&id).unwrap(),
            CleanupOutcome::Removed
        );

        // A dirty tree is kept, with the path reported.
        let mut pool = pool();
        let id = pool
            .delegate(
                DelegationContract::new(
                    "implement",
                    WorkerKind::WriteInIsolatedTree,
                    "x",
                    "files",
                    "m",
                )
                .with_paths(["src"])
                .with_tools(["apply_patch"])
                .with_artifacts(["a patch"]),
                "rev-1",
            )
            .unwrap();
        let tree = pool.get(&id).unwrap().worktree.clone().unwrap();
        std::fs::write(Path::new(&tree).join("unreviewed.rs"), "// work\n").unwrap();

        match pool.cleanup_tree(&id).unwrap() {
            CleanupOutcome::KeptDirty { path } => assert_eq!(path, tree),
            other => panic!("a dirty tree must be kept, got {other:?}"),
        }
        // The work is still there.
        assert!(Path::new(&tree).join("unreviewed.rs").exists());
        let _ = std::fs::remove_dir_all(tree);
    }

    #[test]
    fn the_task_view_shows_purpose_action_budget_and_state() {
        let mut pool = pool();
        let id = pool
            .delegate(read_only_contract("audit the parser"), "rev-1")
            .unwrap();
        pool.record_action(&id, "searching src/parser.rs");
        pool.record_usage(&id, 1_200);

        let view = pool.view_summary();
        assert!(view.contains("audit the parser"));
        assert!(view.contains("read-only"));
        assert!(view.contains("searching src/parser.rs"));
        assert!(view.contains("1200") || view.contains("1,200"));
    }

    #[test]
    fn non_conflicting_proposals_can_be_integrated_together() {
        let results = vec![
            (
                "worker-1".to_owned(),
                WorkerResult::ProposedPatch {
                    identity: "p1".to_owned(),
                    summary: "a".to_owned(),
                    paths: vec!["src/a.rs".to_owned()],
                    base_revision: "rev-1".to_owned(),
                },
            ),
            (
                "worker-2".to_owned(),
                WorkerResult::ProposedPatch {
                    identity: "p2".to_owned(),
                    summary: "b".to_owned(),
                    paths: vec!["src/b.rs".to_owned()],
                    base_revision: "rev-1".to_owned(),
                },
            ),
        ];
        let plan = IntegrationPlan::build(&results, vec!["cargo test".to_owned()]);
        assert!(plan.is_unambiguous());
        assert_eq!(plan.accepted.len(), 2);
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn the_delegation_comparison_reports_a_loss_honestly() {
        // Delegation was slower and cost more: the verdict says so.
        let comparison = DelegationComparison::assess(1_000, 10_000, 1.0, 1_500, 18_000, 1.0, 3);
        assert!(!comparison.supports_delegation());
        assert!(comparison.verdict.contains("not faster"));
        // The costs are named, so the loss is concrete rather than vague.
        assert!(comparison.verdict.contains("18000"));
        assert!(comparison.verdict.contains("10000"));

        // Delegation was faster but cost more: also reported plainly.
        let comparison = DelegationComparison::assess(2_000, 10_000, 1.0, 800, 14_000, 1.0, 2);
        assert!(!comparison.supports_delegation());
        assert!(comparison.verdict.contains("cost more"));

        // Quality regressed: refused outright.
        let comparison = DelegationComparison::assess(2_000, 10_000, 1.0, 500, 5_000, 0.5, 2);
        assert!(comparison.verdict.contains("lost on quality"));

        // A genuine win is supported.
        let comparison = DelegationComparison::assess(2_000, 20_000, 0.9, 900, 12_000, 0.9, 2);
        assert!(comparison.supports_delegation());
        assert!(comparison.verdict.contains("faster and cheaper"));
    }

    #[test]
    fn delegation_candidates_are_known_contracts_with_a_decline_path() {
        let candidates = delegation_candidates();
        assert_eq!(candidates.len(), contract_templates().len() + 1);
        // The control layer may always decline.
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.id == crate::frame::ESCALATE_ID)
        );
        // And every offered contract is a known template.
        let templates = contract_templates();
        let known: BTreeSet<&str> = templates
            .iter()
            .map(|template| template.id.as_str())
            .collect();
        for candidate in candidates {
            if candidate.id != crate::frame::ESCALATE_ID {
                assert!(known.contains(candidate.id.as_str()));
            }
        }
    }

    #[test]
    fn context_is_bounded_to_the_allowed_paths() {
        let contract = read_only_contract("scout");
        assert!(contract.allows_path("src"));
        assert!(contract.allows_path("src/parser.rs"));
        assert!(!contract.allows_path("secrets/.env"));
        assert!(!contract.allows_path("../outside"));

        // A contract with no paths allows nothing.
        let mut empty = read_only_contract("scout");
        empty.allowed_paths.clear();
        assert!(!empty.allows_path("src"));
    }
}
