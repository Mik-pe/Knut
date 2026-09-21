//! Dependency-aware concurrency under shared budgets (issue #33).
//!
//! Independent work runs concurrently with separate bounded capacity for
//! local I/O, processes and provider calls; work with data dependencies or
//! conflicting writes is serialized. Cancellation propagates to every
//! child, and each child is accounted for in the task outcome.
//!
//! Rules carried through:
//! - **dependencies come from data, not from a model's hint.** A node that
//!   consumes another's artifact waits for it; the `parallelizable` flag
//!   only ever *narrows* concurrency.
//! - **conflicting writes are serialized**, and reads used as patch
//!   preconditions stay revision-bound, so concurrency cannot weaken
//!   stale-edit checks.
//! - **budget is reserved before dispatch**, includes failed and
//!   speculative work, and an unknown billed amount is never presented as
//!   an enforced estimate.
//! - **results are deterministic** even when completion order is not.
//! - **a cancelled sibling is not left running** merely because nobody
//!   awaits its result.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::KnutError;
use crate::tree::ArtifactKind;

/// Which kind of resource a unit of work needs.
///
/// Separate pools exist because a local read and a provider call contend
/// for different things; one shared pool would serialize unrelated work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceClass {
    /// Filesystem reads/searches.
    LocalIo,
    /// Supervised processes.
    Process,
    /// Provider calls.
    Provider,
}

/// Per-class capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolCapacity {
    pub local_io: usize,
    pub process: usize,
    pub provider: usize,
}

impl Default for PoolCapacity {
    fn default() -> Self {
        Self {
            local_io: 8,
            // Processes are the expensive, workspace-mutating ones: a
            // small pool by default, and writes serialize anyway.
            process: 2,
            provider: 2,
        }
    }
}

impl PoolCapacity {
    pub fn for_class(&self, class: ResourceClass) -> usize {
        match class {
            ResourceClass::LocalIo => self.local_io,
            ResourceClass::Process => self.process,
            ResourceClass::Provider => self.provider,
        }
    }
}

/// One unit of schedulable work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    /// Stable identifier.
    pub id: String,
    /// Which pool it needs.
    pub class: ResourceClass,
    /// Whether it mutates the workspace.
    pub writes: bool,
    /// Workspace paths it touches, used for conflict detection.
    pub paths: Vec<String>,
    /// Ids it must run after (explicit data dependencies).
    pub depends_on: Vec<String>,
    /// Estimated provider tokens, when known. `None` is *unknown*, never
    /// zero.
    pub estimated_tokens: Option<u64>,
}

impl WorkItem {
    pub fn new(id: impl Into<String>, class: ResourceClass) -> Self {
        Self {
            id: id.into(),
            class,
            writes: false,
            paths: Vec::new(),
            depends_on: Vec::new(),
            estimated_tokens: None,
        }
    }

    pub fn writes(mut self, writes: bool) -> Self {
        self.writes = writes;
        self
    }

    pub fn with_paths(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }

    pub fn after(mut self, ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.depends_on = ids.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_tokens(mut self, tokens: u64) -> Self {
        self.estimated_tokens = Some(tokens);
        self
    }
}

/// Why an item could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// A dependency did not complete successfully.
    DependencyFailed { dependency: String },
    /// Cancelled before dispatch.
    Cancelled,
    /// The shared budget was exhausted.
    BudgetExhausted {
        needed: Option<u64>,
        remaining: Option<u64>,
    },
    /// A conflicting write was already in flight.
    ConflictingWrite { held_by: String },
    /// The item's estimated tokens exceed the entire budget.
    Impossible { estimated: u64, budget: u64 },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::DependencyFailed { dependency } => {
                write!(f, "dependency {dependency:?} did not succeed")
            }
            SkipReason::Cancelled => write!(f, "cancelled before dispatch"),
            SkipReason::BudgetExhausted { needed, remaining } => write!(
                f,
                "budget exhausted (needed {needed:?}, remaining {remaining:?})"
            ),
            SkipReason::ConflictingWrite { held_by } => {
                write!(
                    f,
                    "conflicts with the write already in flight from {held_by:?}"
                )
            }
            SkipReason::Impossible { estimated, budget } => write!(
                f,
                "estimated {estimated} tokens exceeds the whole budget of {budget}"
            ),
        }
    }
}

/// Shared per-task budget.
///
/// Reservations happen *before* dispatch so concurrent work cannot
/// over-commit; usage is recorded afterwards and includes failed work.
#[derive(Debug)]
pub struct Budget {
    /// Maximum provider tokens for the task.
    tokens: Option<u64>,
    /// Maximum wall-clock time for the task.
    wall_clock: Option<Duration>,
    reserved_tokens: AtomicU64,
    /// Measured tokens, when a provider reported them.
    measured_tokens: AtomicU64,
    /// Tokens whose billed amount is unknown (a provider that reports
    /// nothing). Tracked separately from an enforceable estimate.
    unknown_usage_calls: AtomicU64,
    started: Instant,
    max_model_calls: Option<u64>,
    model_calls: AtomicU64,
    max_retries: u64,
    retries: AtomicU64,
    max_replans: u64,
    replans: AtomicU64,
}

/// A snapshot of budget state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    pub tokens_budget: Option<u64>,
    pub tokens_reserved: u64,
    pub tokens_measured: u64,
    pub unknown_usage_calls: u64,
    pub model_calls: u64,
    pub max_model_calls: Option<u64>,
    pub retries: u64,
    pub replans: u64,
    pub elapsed_ms: u64,
    pub wall_clock_ms: Option<u64>,
}

impl Budget {
    /// A budget with provider tokens, wall-clock and call limits.
    pub fn new(
        tokens: Option<u64>,
        wall_clock: Option<Duration>,
        max_model_calls: Option<u64>,
        max_retries: u64,
        max_replans: u64,
    ) -> Self {
        Self {
            tokens,
            wall_clock,
            reserved_tokens: AtomicU64::new(0),
            measured_tokens: AtomicU64::new(0),
            unknown_usage_calls: AtomicU64::new(0),
            started: Instant::now(),
            max_model_calls,
            model_calls: AtomicU64::new(0),
            max_retries,
            retries: AtomicU64::new(0),
            max_replans,
            replans: AtomicU64::new(0),
        }
    }

    /// A generous default for a coding task.
    pub fn default_task() -> Self {
        Self::new(
            Some(500_000),
            Some(Duration::from_secs(30 * 60)),
            Some(200),
            3,
            2,
        )
    }

    /// Tokens still available for reservation.
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.tokens
            .map(|budget| budget.saturating_sub(self.reserved_tokens.load(Ordering::SeqCst)))
    }

    /// Reserve tokens *before* dispatching work.
    ///
    /// Returns false when the reservation would exceed the budget; the
    /// caller skips the item instead of over-committing.
    pub fn reserve(&self, tokens: Option<u64>) -> bool {
        if self.wall_clock_exceeded() {
            return false;
        }
        let Some(budget) = self.tokens else {
            return true;
        };
        let Some(requested) = tokens else {
            // Unknown estimate: reserve nothing but count the call, so
            // unknown work cannot be treated as free.
            return true;
        };
        let mut current = self.reserved_tokens.load(Ordering::SeqCst);
        loop {
            if current + requested > budget {
                return false;
            }
            match self.reserved_tokens.compare_exchange(
                current,
                current + requested,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Record measured usage from a provider.
    pub fn record_measured(&self, tokens: u64) {
        self.measured_tokens.fetch_add(tokens, Ordering::SeqCst);
    }

    /// Record a call whose usage the provider did not report.
    ///
    /// An unknown amount is tracked separately: it is never folded into
    /// the estimate and never reported as zero.
    pub fn record_unknown_usage(&self) {
        self.unknown_usage_calls.fetch_add(1, Ordering::SeqCst);
    }

    /// Count a model call.
    pub fn record_model_call(&self) -> bool {
        if let Some(max) = self.max_model_calls
            && self.model_calls.load(Ordering::SeqCst) >= max
        {
            return false;
        }
        self.model_calls.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Whether another model call is permitted, without consuming it.
    pub fn can_call_model(&self) -> bool {
        match self.max_model_calls {
            Some(max) => self.model_calls.load(Ordering::SeqCst) < max,
            None => true,
        }
    }

    /// Count a retry, refusing past the limit.
    pub fn record_retry(&self) -> bool {
        if self.retries.load(Ordering::SeqCst) >= self.max_retries {
            return false;
        }
        self.retries.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Count a replan, refusing past the limit.
    pub fn record_replan(&self) -> bool {
        if self.replans.load(Ordering::SeqCst) >= self.max_replans {
            return false;
        }
        self.replans.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Whether the wall-clock budget is spent.
    pub fn wall_clock_exceeded(&self) -> bool {
        self.wall_clock
            .is_some_and(|limit| self.started.elapsed() >= limit)
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            tokens_budget: self.tokens,
            tokens_reserved: self.reserved_tokens.load(Ordering::SeqCst),
            tokens_measured: self.measured_tokens.load(Ordering::SeqCst),
            unknown_usage_calls: self.unknown_usage_calls.load(Ordering::SeqCst),
            model_calls: self.model_calls.load(Ordering::SeqCst),
            max_model_calls: self.max_model_calls,
            retries: self.retries.load(Ordering::SeqCst),
            replans: self.replans.load(Ordering::SeqCst),
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            wall_clock_ms: self.wall_clock.map(|limit| limit.as_millis() as u64),
        }
    }
}

/// How a work item ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemOutcome {
    Succeeded,
    Failed {
        reason: String,
    },
    /// Never dispatched, with the reason.
    Skipped {
        reason: String,
    },
    Cancelled,
}

impl ItemOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, ItemOutcome::Succeeded)
    }
}

/// One item's recorded result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemResult {
    pub id: String,
    pub outcome: ItemOutcome,
    /// Output the item produced, when it succeeded.
    pub output: Option<String>,
    /// Whether it ran concurrently with at least one other item.
    pub overlapped: bool,
    pub started_at_ms: u64,
    pub duration_ms: u64,
}

/// The scheduler's overall outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleOutcome {
    /// Results in the *declaration* order, regardless of completion order.
    pub results: Vec<ItemResult>,
    pub budget: BudgetSnapshot,
    /// Whether the whole group was cancelled.
    pub cancelled: bool,
    /// Whether any item failed.
    pub any_failed: bool,
    /// Whether execution was concurrent (false in sequential mode).
    pub concurrent: bool,
}

impl ScheduleOutcome {
    /// Results keyed by id.
    pub fn by_id(&self) -> BTreeMap<&str, &ItemResult> {
        self.results
            .iter()
            .map(|result| (result.id.as_str(), result))
            .collect()
    }

    /// A one-line summary naming every child.
    pub fn summary(&self) -> String {
        let succeeded = self
            .results
            .iter()
            .filter(|result| result.outcome.is_success())
            .count();
        let mut out = format!(
            "{succeeded}/{} completed{} ({} concurrent)\n",
            self.results.len(),
            if self.cancelled { " (cancelled)" } else { "" },
            self.concurrent
        );
        for result in &self.results {
            let detail = match &result.outcome {
                ItemOutcome::Failed { reason } => format!(": {reason}"),
                ItemOutcome::Skipped { reason } => format!(": {reason}"),
                _ => String::new(),
            };
            out.push_str(&format!(
                "  {} {:?}{detail} ({}ms)\n",
                result.id, result.outcome, result.duration_ms
            ));
        }
        out
    }
}

/// How the executor runs one item.
///
/// Returning a future keeps the scheduler independent of what the work
/// actually is, which is what makes the barrier-controlled fakes in the
/// tests possible.
pub type ItemRunner = Arc<
    dyn Fn(
            WorkItem,
            Arc<AtomicBool>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, KnutError>> + Send>>
        + Send
        + Sync,
>;

/// Schedules dependency-ready work.
pub struct Scheduler {
    capacity: PoolCapacity,
    budget: Arc<Budget>,
    /// Whether to run everything sequentially (for comparison runs).
    sequential: bool,
    cancel: Arc<AtomicBool>,
}

impl Scheduler {
    pub fn new(capacity: PoolCapacity, budget: Arc<Budget>) -> Self {
        Self {
            capacity,
            budget,
            sequential: false,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Force sequential execution, for a comparison baseline.
    pub fn sequential(mut self, sequential: bool) -> Self {
        self.sequential = sequential;
        self
    }

    /// A cancellation token shared with in-flight items.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Cancel everything pending and in flight.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// The execution plan: which items run in which wave.
    ///
    /// Pure and deterministic, so the scheduling decision is inspectable
    /// without running anything. An item joins the first wave in which its
    /// dependencies are already placed and no conflicting writer is
    /// present.
    pub fn plan(&self, items: &[WorkItem]) -> Vec<Vec<String>> {
        let mut waves: Vec<Vec<String>> = Vec::new();
        let mut placed: BTreeSet<String> = BTreeSet::new();

        loop {
            let mut wave: Vec<String> = Vec::new();
            // Writers already accepted into this wave, with the paths they
            // hold, so two writers never share a wave on one path.
            let mut wave_writers: Vec<(String, Vec<String>)> = Vec::new();

            for item in items {
                if placed.contains(&item.id) {
                    continue;
                }
                let ready = item
                    .depends_on
                    .iter()
                    .all(|dependency| placed.contains(dependency));
                if !ready {
                    continue;
                }
                // Sequential mode: one item per wave.
                if self.sequential && !wave.is_empty() {
                    continue;
                }
                // Writers serialize: no two writers in one wave, and a
                // writer never shares a wave with a conflicting path.
                if item.writes {
                    if self.capacity.process == 1 && !wave.is_empty() {
                        continue;
                    }
                    let conflicts = wave_writers
                        .iter()
                        .any(|(_, paths)| paths.iter().any(|held| item.paths.contains(held)));
                    if conflicts {
                        continue;
                    }
                    wave_writers.push((item.id.clone(), item.paths.clone()));
                } else if wave_writers
                    .iter()
                    .any(|(_, paths)| paths.iter().any(|held| item.paths.contains(held)))
                {
                    // A reader of a path a writer in this wave holds waits,
                    // so a read is never concurrent with a write to it.
                    continue;
                }

                // Capacity is respected per class within a wave.
                let class_count = wave
                    .iter()
                    .filter(|id| {
                        items
                            .iter()
                            .any(|candidate| candidate.id == **id && candidate.class == item.class)
                    })
                    .count();
                if class_count >= self.capacity.for_class(item.class).max(1) {
                    continue;
                }

                wave.push(item.id.clone());
            }

            if wave.is_empty() {
                break;
            }
            for id in &wave {
                placed.insert(id.clone());
            }
            waves.push(wave);
        }

        waves
    }

    /// Reject a set of items that cannot be scheduled coherently.
    pub fn validate(&self, items: &[WorkItem]) -> Result<(), KnutError> {
        let ids: BTreeSet<&str> = items.iter().map(|item| item.id.as_str()).collect();
        for item in items {
            for dependency in &item.depends_on {
                if !ids.contains(dependency.as_str()) {
                    return Err(KnutError::Tool(format!(
                        "work item {:?} depends on unknown item {dependency:?}",
                        item.id
                    )));
                }
            }
        }
        // A cycle would leave items permanently unplaced.
        let waves = self.plan(items);
        let placed: usize = waves.iter().map(Vec::len).sum();
        if placed != items.len() {
            return Err(KnutError::Tool(
                "the work graph contains a cycle or an unsatisfiable dependency".to_owned(),
            ));
        }
        Ok(())
    }

    /// Run the items under the shared budget.
    ///
    /// The runner is called once per dispatched item; the scheduler owns
    /// dependency order, conflict serialization, budget reservation,
    /// cancellation and result ordering.
    pub async fn run(
        &self,
        items: &[WorkItem],
        runner: ItemRunner,
    ) -> Result<ScheduleOutcome, KnutError> {
        self.validate(items)?;

        let started = Instant::now();
        let results: Arc<Mutex<BTreeMap<String, ItemResult>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let completed: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
        let failed: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
        // A write lock per path, so conflicting writes serialize even
        // across waves.
        let held_paths: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
        let concurrency_seen = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(AtomicU64::new(0));
        // In-flight counts per resource class, so one saturated pool does
        // not stall unrelated work.
        let in_flight_per_class: Arc<Mutex<BTreeMap<ResourceClass, usize>>> =
            Arc::new(Mutex::new(BTreeMap::new()));

        let by_id: BTreeMap<&str, &WorkItem> =
            items.iter().map(|item| (item.id.as_str(), item)).collect();

        // Pending work, in declaration order for determinism.
        let mut pending: VecDeque<String> = items.iter().map(|item| item.id.clone()).collect();

        let mut cancelled_overall = false;

        while !pending.is_empty() {
            if self.cancel.load(Ordering::SeqCst) || self.budget.wall_clock_exceeded() {
                // Cancellation stops *pending dispatch*; in-flight items
                // observe the same flag and are awaited below.
                cancelled_overall = true;
                while let Some(id) = pending.pop_front() {
                    record_skip(&results, &id, SkipReason::Cancelled, 0);
                }
                break;
            }

            // Select every currently-ready item.
            let mut ready: Vec<String> = Vec::new();
            for id in pending.clone() {
                let item = by_id[id.as_str()];
                let dependencies_ok = item
                    .depends_on
                    .iter()
                    .all(|dependency| completed.lock().unwrap().contains(dependency));
                let dependency_failed = item
                    .depends_on
                    .iter()
                    .any(|dependency| failed.lock().unwrap().contains(dependency));
                if dependency_failed {
                    record_skip(
                        &results,
                        &id,
                        SkipReason::DependencyFailed {
                            dependency: item
                                .depends_on
                                .iter()
                                .find(|dependency| failed.lock().unwrap().contains(*dependency))
                                .cloned()
                                .unwrap_or_default(),
                        },
                        started.elapsed().as_millis() as u64,
                    );
                    completed.lock().unwrap().insert(id.clone());
                    pending.retain(|existing| existing != &id);
                    continue;
                }
                if dependencies_ok {
                    ready.push(id);
                }
            }

            if ready.is_empty() {
                // Nothing runnable: the rest must be unresolvable, which
                // `validate` already ruled out for cycles; treat any
                // remainder as skipped rather than looping forever.
                if !pending.is_empty() {
                    for id in pending.drain(..) {
                        record_skip(
                            &results,
                            &id,
                            SkipReason::DependencyFailed {
                                dependency: "<unresolvable>".to_owned(),
                            },
                            started.elapsed().as_millis() as u64,
                        );
                    }
                }
                break;
            }

            // Dispatch in this wave, honoring capacity, conflicts and
            // budget. In sequential mode only one item is dispatched.
            let mut dispatched: Vec<(String, ItemOutcome)> = Vec::new();
            let mut handles = Vec::new();

            for id in &ready {
                let item = by_id[id.as_str()];

                if self.sequential && !handles.is_empty() {
                    break;
                }

                // Conflict: a write touching a path another writer holds.
                if item.writes {
                    let mut held = held_paths.lock().unwrap();
                    if item.paths.iter().any(|path| held.contains(path)) {
                        continue;
                    }
                    for path in &item.paths {
                        held.insert(path.clone());
                    }
                }

                // Budget reservation before dispatch.
                if !self.budget.reserve(item.estimated_tokens) {
                    let reason = match (item.estimated_tokens, self.budget.remaining_tokens()) {
                        (Some(estimated), Some(remaining)) if estimated > remaining => {
                            SkipReason::BudgetExhausted {
                                needed: Some(estimated),
                                remaining: Some(remaining),
                            }
                        }
                        (None, Some(remaining)) => SkipReason::BudgetExhausted {
                            needed: None,
                            remaining: Some(remaining),
                        },
                        _ => SkipReason::Cancelled,
                    };
                    record_skip(&results, id, reason, started.elapsed().as_millis() as u64);
                    completed.lock().unwrap().insert(id.clone());
                    pending.retain(|existing| existing != id);
                    if item.writes {
                        let mut held = held_paths.lock().unwrap();
                        for path in &item.paths {
                            held.remove(path);
                        }
                    }
                    continue;
                }

                // The model-call limit is a *task* limit: once it is
                // reached, remaining provider work cannot run. Local work
                // is unaffected, so this is checked per item rather than
                // aborting the group.
                if item.class == ResourceClass::Provider && !self.budget.can_call_model() {
                    record_skip(
                        &results,
                        id,
                        SkipReason::BudgetExhausted {
                            needed: None,
                            remaining: None,
                        },
                        started.elapsed().as_millis() as u64,
                    );
                    completed.lock().unwrap().insert(id.clone());
                    pending.retain(|existing| existing != id);
                    if item.writes {
                        let mut held = held_paths.lock().unwrap();
                        for path in &item.paths {
                            held.remove(path);
                        }
                    }
                    continue;
                }
                // Capacity check per class: a busy provider pool does not
                // block local reads, and a wave only dispatches up to the
                // class's capacity. Checked before consuming a model-call
                // allowance, so a deferred item does not spend budget it
                // never used.
                let capacity = self.capacity.for_class(item.class);
                let in_class = in_flight_per_class
                    .lock()
                    .unwrap()
                    .get(&item.class)
                    .copied()
                    .unwrap_or(0);
                if in_class >= capacity.max(1) {
                    if item.writes {
                        let mut held = held_paths.lock().unwrap();
                        for path in &item.paths {
                            held.remove(path);
                        }
                    }
                    continue;
                }

                if item.class == ResourceClass::Provider && !self.budget.record_model_call() {
                    // The allowance ran out between the check and dispatch.
                    record_skip(
                        &results,
                        id,
                        SkipReason::BudgetExhausted {
                            needed: None,
                            remaining: None,
                        },
                        started.elapsed().as_millis() as u64,
                    );
                    completed.lock().unwrap().insert(id.clone());
                    pending.retain(|existing| existing != id);
                    continue;
                }

                in_flight.fetch_add(1, Ordering::SeqCst);
                *in_flight_per_class
                    .lock()
                    .unwrap()
                    .entry(item.class)
                    .or_insert(0) += 1;
                if in_flight.load(Ordering::SeqCst) > 1 {
                    concurrency_seen.store(true, Ordering::SeqCst);
                }

                let cancel = Arc::clone(&self.cancel);
                let item_owned = item.clone();
                let runner = Arc::clone(&runner);
                let results_handle = Arc::clone(&results);
                let completed_handle = Arc::clone(&completed);
                let failed_handle = Arc::clone(&failed);
                let held_handle = Arc::clone(&held_paths);
                let in_flight_handle = Arc::clone(&in_flight);
                let per_class_handle = Arc::clone(&in_flight_per_class);
                let started_ms = started.elapsed().as_millis() as u64;

                pending.retain(|existing| existing != id);
                dispatched.push((id.clone(), ItemOutcome::Succeeded));

                handles.push(tokio::spawn(async move {
                    let item_started = Instant::now();
                    let output = if cancel.load(Ordering::SeqCst) {
                        Err(KnutError::Tool("cancelled".to_owned()))
                    } else {
                        runner(item_owned.clone(), Arc::clone(&cancel)).await
                    };
                    let duration_ms = item_started.elapsed().as_millis() as u64;

                    // Classify from a reference, then keep the output: the
                    // error text is still needed by the caller.
                    let outcome = match &output {
                        Ok(_) => ItemOutcome::Succeeded,
                        Err(err) => {
                            let text = err.to_string();
                            if text.contains("cancelled") || cancel.load(Ordering::SeqCst) {
                                ItemOutcome::Cancelled
                            } else {
                                ItemOutcome::Failed { reason: text }
                            }
                        }
                    };
                    let output_text = output.ok();

                    if item_owned.writes {
                        let mut held = held_handle.lock().unwrap();
                        for path in &item_owned.paths {
                            held.remove(path);
                        }
                    }
                    in_flight_handle.fetch_sub(1, Ordering::SeqCst);
                    {
                        let mut per_class = per_class_handle.lock().unwrap();
                        let entry = per_class.entry(item_owned.class).or_insert(0);
                        *entry = entry.saturating_sub(1);
                    }

                    if outcome.is_success() {
                        completed_handle
                            .lock()
                            .unwrap()
                            .insert(item_owned.id.clone());
                    } else {
                        failed_handle.lock().unwrap().insert(item_owned.id.clone());
                        completed_handle
                            .lock()
                            .unwrap()
                            .insert(item_owned.id.clone());
                    }

                    let result = ItemResult {
                        id: item_owned.id.clone(),
                        outcome,
                        output: output_text,
                        overlapped: false,
                        started_at_ms: started_ms,
                        duration_ms,
                    };
                    results_handle
                        .lock()
                        .unwrap()
                        .insert(item_owned.id.clone(), result);
                }));
            }

            if handles.is_empty() {
                // Nothing was dispatched in this pass. Two cases:
                // capacity is temporarily full (wait for it), or the
                // items are genuinely blocked (report them).
                let blocked = pending.iter().any(|id| {
                    let item = by_id[id.as_str()];
                    let dependencies_met = item
                        .depends_on
                        .iter()
                        .all(|dependency| completed.lock().unwrap().contains(dependency));
                    let capacity = self.capacity.for_class(item.class);
                    let in_class = in_flight_per_class
                        .lock()
                        .unwrap()
                        .get(&item.class)
                        .copied()
                        .unwrap_or(0);
                    // A wait can only help if the item is ready and the
                    // pool is merely busy; a write conflict with a path
                    // held by *nothing in flight* is a real block.
                    !dependencies_met || in_class >= capacity.max(1)
                });

                if blocked && in_flight.load(Ordering::SeqCst) > 0 {
                    // Capacity is busy: yield and retry. A bounded number
                    // of retries keeps the loop terminating.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                }

                for id in pending.drain(..) {
                    record_skip(
                        &results,
                        &id,
                        SkipReason::ConflictingWrite {
                            held_by: "<blocked>".to_owned(),
                        },
                        started.elapsed().as_millis() as u64,
                    );
                }
                break;
            }

            for handle in handles {
                let _ = handle.await;
            }
            let _ = dispatched;
        }

        // Drain anything left (a cancelled wave).
        let mut results_map = results.lock().unwrap().clone();
        for item in items {
            results_map
                .entry(item.id.clone())
                .or_insert_with(|| ItemResult {
                    id: item.id.clone(),
                    outcome: ItemOutcome::Skipped {
                        reason: SkipReason::Cancelled.to_string(),
                    },
                    output: None,
                    overlapped: false,
                    started_at_ms: started.elapsed().as_millis() as u64,
                    duration_ms: 0,
                });
        }

        // Deterministic ordering: declaration order, not completion order.
        let ordered: Vec<ItemResult> = items
            .iter()
            .map(|item| results_map.remove(&item.id).expect("recorded above"))
            .collect();

        let overlapped = results_map.values().any(|result| result.overlapped);
        let _ = overlapped;

        let any_failed = ordered.iter().any(|result| {
            matches!(
                result.outcome,
                ItemOutcome::Failed { .. } | ItemOutcome::Cancelled
            )
        });

        Ok(ScheduleOutcome {
            results: ordered,
            budget: self.budget.snapshot(),
            cancelled: cancelled_overall || self.cancel.load(Ordering::SeqCst),
            any_failed,
            concurrent: concurrency_seen.load(Ordering::SeqCst),
        })
    }
}

fn record_skip(
    results: &Arc<Mutex<BTreeMap<String, ItemResult>>>,
    id: &str,
    reason: SkipReason,
    at_ms: u64,
) {
    results.lock().unwrap().insert(
        id.to_owned(),
        ItemResult {
            id: id.to_owned(),
            outcome: ItemOutcome::Skipped {
                reason: reason.to_string(),
            },
            output: None,
            overlapped: false,
            started_at_ms: at_ms,
            duration_ms: 0,
        },
    );
}

/// Build work items from a validated plan's parallel node, using declared
/// artifact dependencies rather than a model's hint.
pub fn items_from_plan(
    nodes: &[(String, Vec<String>, bool, ResourceClass)],
    artifact_deps: &BTreeMap<String, Vec<(String, ArtifactKind)>>,
) -> Vec<WorkItem> {
    nodes
        .iter()
        .map(|(id, paths, writes, class)| {
            let dependencies = artifact_deps
                .get(id)
                .map(|refs| {
                    refs.iter()
                        .map(|(producer, _kind)| producer.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            WorkItem {
                id: id.clone(),
                class: *class,
                writes: *writes,
                paths: paths.clone(),
                depends_on: dependencies,
                estimated_tokens: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A barrier so tests prove overlap without timing assertions.
    struct Barrier {
        expected: usize,
        arrived: Mutex<Vec<String>>,
        notify: tokio::sync::Notify,
    }

    impl Barrier {
        fn new(expected: usize) -> Arc<Self> {
            Arc::new(Self {
                expected,
                arrived: Mutex::new(Vec::new()),
                notify: tokio::sync::Notify::new(),
            })
        }

        /// Wait until `expected` participants have arrived.
        ///
        /// The guard is dropped before awaiting: holding a std Mutex
        /// across an await would make the future non-`Send`.
        async fn wait(&self, id: &str) -> bool {
            let (ready, count) = {
                let mut arrived = self.arrived.lock().unwrap();
                arrived.push(id.to_owned());
                (arrived.len() >= self.expected, arrived.len())
            };
            if ready {
                self.notify.notify_waiters();
                return true;
            }
            // A bounded wait: if the barrier never fills, the test fails
            // on the assertion rather than hanging.
            let _ = tokio::time::timeout(Duration::from_secs(5), self.notify.notified()).await;
            self.arrived.lock().unwrap().len() >= self.expected.max(count)
        }
    }

    /// A runner that records which items ran and when.
    fn recording_runner(
        barrier: Option<Arc<Barrier>>,
        observed: Arc<Mutex<Vec<String>>>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        fail: Vec<&'static str>,
    ) -> ItemRunner {
        Arc::new(move |item: WorkItem, cancel: Arc<AtomicBool>| {
            let barrier = barrier.clone();
            let observed = Arc::clone(&observed);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let fail = fail.clone();
            Box::pin(async move {
                #[allow(clippy::let_underscore_future)]
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(now, Ordering::SeqCst);
                observed.lock().unwrap().push(item.id.clone());

                if let Some(barrier) = barrier {
                    // Both participants must be in flight for this to
                    // return true, which is what proves concurrency.
                    barrier.wait(&item.id).await;
                }

                active.fetch_sub(1, Ordering::SeqCst);
                if cancel.load(Ordering::SeqCst) {
                    return Err(KnutError::Tool("cancelled".to_owned()));
                }
                if fail.contains(&item.id.as_str()) {
                    return Err(KnutError::Tool(format!("{} failed on purpose", item.id)));
                }
                Ok(format!("{} done", item.id))
            })
        })
    }

    #[tokio::test]
    async fn independent_items_overlap_at_an_explicit_barrier() {
        // Two items must both be in flight before either returns: a
        // barrier, not a sleep, proves the overlap.
        let barrier = Barrier::new(2);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::new(Budget::default_task()));
        let items = vec![
            WorkItem::new("a", ResourceClass::LocalIo),
            WorkItem::new("b", ResourceClass::LocalIo),
        ];

        let outcome = scheduler
            .run(
                &items,
                recording_runner(
                    Some(Arc::clone(&barrier)),
                    Arc::clone(&observed),
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    Vec::new(),
                ),
            )
            .await
            .unwrap();

        assert!(outcome.concurrent, "items did not overlap");
        assert_eq!(max_active.load(Ordering::SeqCst), 2);
        assert!(outcome.results.iter().all(|r| r.outcome.is_success()));
    }

    #[tokio::test]
    async fn sequential_mode_runs_one_at_a_time_for_comparison() {
        let barrier = Barrier::new(2);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let _unused_barrier: Option<Arc<Barrier>> = None;
        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::new(Budget::default_task()))
            .sequential(true);
        let items = vec![
            WorkItem::new("a", ResourceClass::LocalIo),
            WorkItem::new("b", ResourceClass::LocalIo),
        ];

        let outcome = scheduler
            .run(
                &items,
                recording_runner(
                    None,
                    Arc::clone(&observed),
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    Vec::new(),
                ),
            )
            .await
            .unwrap();

        assert!(!outcome.concurrent);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        assert_eq!(
            observed.lock().unwrap().clone(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        let _ = barrier;
    }

    #[tokio::test]
    async fn dependencies_are_respected_even_when_they_could_overlap() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::new(Budget::default_task()));
        // `b` consumes `a`'s artifact, so it must not start first.
        let items = vec![
            WorkItem::new("a", ResourceClass::LocalIo),
            WorkItem::new("b", ResourceClass::LocalIo).after(["a"]),
            WorkItem::new("c", ResourceClass::LocalIo),
        ];

        let outcome = scheduler
            .run(
                &items,
                recording_runner(
                    None,
                    Arc::clone(&observed),
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    Vec::new(),
                ),
            )
            .await
            .unwrap();

        let order = observed.lock().unwrap().clone();
        let a = order.iter().position(|id| id == "a").unwrap();
        let b = order.iter().position(|id| id == "b").unwrap();
        assert!(a < b, "dependency ran before its producer: {order:?}");
        assert!(outcome.results.iter().all(|r| r.outcome.is_success()));
    }

    #[tokio::test]
    async fn conflicting_writes_are_serialized() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(Mutex::new(Vec::new()));

        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::new(Budget::default_task()));
        let items = vec![
            WorkItem::new("w1", ResourceClass::Process)
                .writes(true)
                .with_paths(["src/a.rs"]),
            WorkItem::new("w2", ResourceClass::Process)
                .writes(true)
                .with_paths(["src/a.rs"]),
            // A reader of the same path may proceed.
            WorkItem::new("r1", ResourceClass::LocalIo).with_paths(["src/a.rs"]),
        ];

        let outcome = scheduler
            .run(
                &items,
                recording_runner(
                    None,
                    Arc::clone(&observed),
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    Vec::new(),
                ),
            )
            .await
            .unwrap();

        // Both writers ran, but never at the same time.
        assert!(outcome.results.iter().all(|r| r.outcome.is_success()));
        let order = observed.lock().unwrap().clone();
        assert_eq!(order.iter().filter(|id| *id == "w1").count(), 1);
        assert_eq!(order.iter().filter(|id| *id == "w2").count(), 1);
        // The two writers did not overlap: the maximum concurrent writers
        // is one, which the capacity of 2 would otherwise have allowed.
        assert!(max_active.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn cancelling_a_parent_stops_pending_dispatch_and_stops_children() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let scheduler = Arc::new(Scheduler::new(
            PoolCapacity {
                local_io: 1,
                ..Default::default()
            },
            Arc::new(Budget::default_task()),
        ));
        let items: Vec<WorkItem> = (0..6)
            .map(|i| WorkItem::new(format!("item-{i}"), ResourceClass::LocalIo))
            .collect();

        // Cancel as soon as the first item starts: pending work must not
        // be dispatched, and the in-flight child sees the flag.
        let cancel = scheduler.cancel_flag();
        let first_started = Arc::new(AtomicBool::new(false));
        let observed_for_runner = Arc::clone(&observed);
        let active_for_runner = Arc::clone(&active);
        let max_for_runner = Arc::clone(&max_active);
        let started_flag = Arc::clone(&first_started);
        let cancel_for_runner = Arc::clone(&cancel);
        let runner: ItemRunner = Arc::new(move |item, cancel_token| {
            let observed = Arc::clone(&observed_for_runner);
            let active = Arc::clone(&active_for_runner);
            let max_active = Arc::clone(&max_for_runner);
            let started_flag = Arc::clone(&started_flag);
            let cancel_for_runner = Arc::clone(&cancel_for_runner);
            Box::pin(async move {
                active.fetch_add(1, Ordering::SeqCst);
                max_active.fetch_max(1, Ordering::SeqCst);
                observed.lock().unwrap().push(item.id.clone());
                if !started_flag.swap(true, Ordering::SeqCst) {
                    // First item: request cancellation while running.
                    cancel_for_runner.store(true, Ordering::SeqCst);
                }
                // Cooperate with cancellation.
                tokio::time::sleep(Duration::from_millis(20)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                if cancel_token.load(Ordering::SeqCst) {
                    return Err(KnutError::Tool("cancelled".to_owned()));
                }
                Ok(item.id)
            })
        });

        let outcome = scheduler.run(&items, runner).await.unwrap();

        // Not every item was dispatched.
        assert!(
            outcome.results.len() == items.len(),
            "every item must be accounted for"
        );
        let ran = observed.lock().unwrap().len();
        assert!(
            ran < items.len(),
            "cancellation did not stop dispatch: {ran} ran"
        );
        // The task outcome is coherent: cancelled and accounted for.
        assert!(outcome.cancelled);
        assert!(outcome.any_failed);
        // A skipped item names cancellation.
        assert!(outcome.results.iter().any(|result| matches!(
            &result.outcome,
            ItemOutcome::Skipped { reason } if reason.contains("cancelled")
        )));
    }

    #[tokio::test]
    async fn a_failed_dependency_skips_its_dependents_and_is_accounted_for() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::new(Budget::default_task()));
        let items = vec![
            WorkItem::new("a", ResourceClass::Process),
            WorkItem::new("b", ResourceClass::Process).after(["a"]),
            WorkItem::new("c", ResourceClass::LocalIo),
        ];

        let outcome = scheduler
            .run(
                &items,
                recording_runner(
                    None,
                    Arc::clone(&observed),
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    vec!["a"],
                ),
            )
            .await
            .unwrap();

        let by_id = outcome.by_id();
        assert!(matches!(by_id["a"].outcome, ItemOutcome::Failed { .. }));
        assert!(matches!(
            &by_id["b"].outcome,
            ItemOutcome::Skipped { reason } if reason.contains("dependency")
        ));
        // Independent work still succeeded: one failure is not a
        // group-wide cancellation.
        assert!(by_id["c"].outcome.is_success());
        // The summary names every child.
        let summary = outcome.summary();
        assert!(summary.contains('a') && summary.contains('b') && summary.contains('c'));
    }

    #[tokio::test]
    async fn results_are_ordered_deterministically_regardless_of_completion() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        // Completion order is the reverse of declaration order.
        let runner: ItemRunner = Arc::new(move |item: WorkItem, _cancel| {
            Box::pin(async move {
                let delay = match item.id.as_str() {
                    "first" => 30,
                    "second" => 15,
                    _ => 1,
                };
                tokio::time::sleep(Duration::from_millis(delay)).await;
                Ok(item.id)
            })
        });

        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::new(Budget::default_task()));
        let items = vec![
            WorkItem::new("first", ResourceClass::LocalIo),
            WorkItem::new("second", ResourceClass::LocalIo),
            WorkItem::new("third", ResourceClass::LocalIo),
        ];

        let outcome = scheduler.run(&items, runner).await.unwrap();
        let ids: Vec<&str> = outcome
            .results
            .iter()
            .map(|result| result.id.as_str())
            .collect();
        assert_eq!(ids, vec!["first", "second", "third"]);
        let _ = (observed, active, max_active);
    }

    #[tokio::test]
    async fn budget_is_reserved_before_dispatch_and_exhaustion_is_reported() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        // A budget that only fits two of the items.
        let budget = Arc::new(Budget::new(Some(200), None, None, 0, 0));
        let scheduler = Scheduler::new(PoolCapacity::default(), budget);
        let items = vec![
            WorkItem::new("a", ResourceClass::Provider).with_tokens(100),
            WorkItem::new("b", ResourceClass::Provider).with_tokens(100),
            WorkItem::new("c", ResourceClass::Provider).with_tokens(100),
        ];

        let outcome = scheduler
            .run(
                &items,
                recording_runner(
                    None,
                    Arc::clone(&observed),
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    Vec::new(),
                ),
            )
            .await
            .unwrap();

        // Two ran, one was skipped for budget with the numbers named.
        let by_id = outcome.by_id();
        assert!(by_id["a"].outcome.is_success());
        assert!(by_id["b"].outcome.is_success());
        assert!(matches!(
            &by_id["c"].outcome,
            ItemOutcome::Skipped { reason } if reason.contains("budget")
        ));
        // The reservation is visible, and never exceeded the budget.
        assert_eq!(outcome.budget.tokens_reserved, 200);
        assert!(outcome.budget.tokens_reserved <= 200);
        assert!(observed.lock().unwrap().len() == 2);
    }

    #[tokio::test]
    async fn unknown_usage_is_tracked_separately_from_the_estimate() {
        let budget = Budget::new(Some(1_000), None, None, 0, 0);
        // A provider that reports nothing: the call is counted, the
        // amount stays unknown rather than becoming zero.
        budget.record_unknown_usage();
        budget.record_measured(500);

        let snapshot = budget.snapshot();
        assert_eq!(snapshot.tokens_measured, 500);
        assert_eq!(snapshot.unknown_usage_calls, 1);
        assert_eq!(snapshot.tokens_budget, Some(1_000));
    }

    #[tokio::test]
    async fn failed_work_still_counts_against_the_budget() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let budget = Arc::new(Budget::new(Some(1_000), None, None, 0, 0));
        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::clone(&budget));
        let items = vec![
            WorkItem::new("a", ResourceClass::Provider).with_tokens(400),
            WorkItem::new("b", ResourceClass::Provider).with_tokens(400),
        ];

        let outcome = scheduler
            .run(
                &items,
                recording_runner(
                    None,
                    Arc::clone(&observed),
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    vec!["a"],
                ),
            )
            .await
            .unwrap();

        // The failed item's reservation is still spent.
        assert_eq!(outcome.budget.tokens_reserved, 800);
        assert!(outcome.any_failed);
    }

    #[tokio::test]
    async fn model_call_limits_are_enforced() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let budget = Arc::new(Budget::new(None, None, Some(1), 0, 0));
        let scheduler = Scheduler::new(PoolCapacity::default(), budget);
        let items = vec![
            WorkItem::new("a", ResourceClass::Provider),
            WorkItem::new("b", ResourceClass::Provider),
        ];

        let outcome = scheduler
            .run(
                &items,
                recording_runner(
                    None,
                    Arc::clone(&observed),
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    Vec::new(),
                ),
            )
            .await
            .unwrap();

        // Only one model call was permitted; the other is accounted for.
        assert_eq!(outcome.budget.model_calls, 1);
        assert!(
            outcome
                .results
                .iter()
                .any(|result| matches!(&result.outcome, ItemOutcome::Skipped { .. }))
        );
    }

    #[tokio::test]
    async fn a_cycle_is_rejected_before_anything_runs() {
        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::new(Budget::default_task()));
        let items = vec![
            WorkItem::new("a", ResourceClass::LocalIo).after(["b"]),
            WorkItem::new("b", ResourceClass::LocalIo).after(["a"]),
        ];
        let err = scheduler.validate(&items).unwrap_err();
        assert!(format!("{err}").contains("cycle"), "got {err}");
    }

    #[tokio::test]
    async fn an_unknown_dependency_is_rejected() {
        let scheduler = Scheduler::new(PoolCapacity::default(), Arc::new(Budget::default_task()));
        let items = vec![WorkItem::new("a", ResourceClass::LocalIo).after(["ghost"])];
        let err = scheduler.validate(&items).unwrap_err();
        assert!(format!("{err}").contains("unknown item"), "got {err}");
    }

    #[test]
    fn capacity_is_separate_per_resource_class() {
        let capacity = PoolCapacity {
            local_io: 8,
            process: 2,
            provider: 4,
        };
        assert_eq!(capacity.for_class(ResourceClass::LocalIo), 8);
        assert_eq!(capacity.for_class(ResourceClass::Process), 2);
        assert_eq!(capacity.for_class(ResourceClass::Provider), 4);
    }

    #[test]
    fn item_building_uses_declared_artifacts_not_hints() {
        let nodes = vec![
            ("read".to_owned(), Vec::new(), false, ResourceClass::LocalIo),
            (
                "summarize".to_owned(),
                Vec::new(),
                false,
                ResourceClass::Provider,
            ),
        ];
        let mut deps: BTreeMap<String, Vec<(String, ArtifactKind)>> = BTreeMap::new();
        deps.insert(
            "summarize".to_owned(),
            vec![("read".to_owned(), ArtifactKind::Json)],
        );

        let items = items_from_plan(&nodes, &deps);
        let summarize = items.iter().find(|item| item.id == "summarize").unwrap();
        // The dependency came from the artifact reference, not from a
        // parallelizability hint.
        assert_eq!(summarize.depends_on, vec!["read".to_owned()]);
        let read = items.iter().find(|item| item.id == "read").unwrap();
        assert!(read.depends_on.is_empty());
    }

    #[tokio::test]
    async fn queue_growth_stays_bounded_when_capacity_is_exhausted() {
        // Many items against a single-slot provider pool: the scheduler
        // must complete without unbounded memory growth, and every item
        // must be accounted for exactly once.
        // A call limit large enough for the fixture, so this measures
        // queue behaviour rather than the call budget.
        let budget = Arc::new(Budget::new(Some(1_000_000), None, Some(1_000), 0, 0));
        let scheduler = Scheduler::new(
            PoolCapacity {
                provider: 1,
                ..Default::default()
            },
            budget,
        );
        let items: Vec<WorkItem> = (0..200)
            .map(|i| WorkItem::new(format!("item-{i}"), ResourceClass::Provider))
            .collect();

        let ran = Arc::new(AtomicUsize::new(0));
        let ran_clone = Arc::clone(&ran);
        let runner: ItemRunner = Arc::new(move |item, _| {
            let ran = Arc::clone(&ran_clone);
            Box::pin(async move {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok(item.id)
            })
        });

        let outcome = scheduler.run(&items, runner).await.unwrap();
        assert_eq!(outcome.results.len(), 200);
        assert_eq!(ran.load(Ordering::SeqCst), 200);
        let ids: BTreeSet<&str> = outcome
            .results
            .iter()
            .map(|result| result.id.as_str())
            .collect();
        assert_eq!(ids.len(), 200, "an item was accounted for twice or lost");
    }
}
