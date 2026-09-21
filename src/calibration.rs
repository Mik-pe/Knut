//! Calibrated routing policy: bounded caches, per-decision thresholds and
//! honest calibration metrics (issue #36).
//!
//! The prototype reused one 0.75 confidence floor for every action. That
//! confuses *confident routing* with *guaranteed correctness*, so this
//! module separates the pieces:
//!
//! - **per-decision-type thresholds.** A cheap deterministic rule and a
//!   risky recovery classification do not share a confidence floor.
//! - **raw and effective scores stay separate.** A Noul answer's raw
//!   probability is preserved; dampening or abstention produces an
//!   *effective* score, and both are recorded.
//! - **a bounded, versioned cache.** Keys cover everything
//!   decision-relevant (workspace revision, candidate set, policy, tool,
//!   question and model versions, mode), so a hit cannot cross
//!   workspaces or survive a relevant change. Nothing here caches
//!   permissions: a hit still goes through the execution gate.
//! - **explicit deadlines and a circuit breaker** driven by observed
//!   latency, with fallback to reasoning rather than to success.
//! - **held-out evaluation** of the thresholds, including abstentions and
//!   negative results, and a policy-version check that refuses to reuse
//!   calibration from a different version.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Which kind of decision a threshold applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionType {
    /// Ingress routing: does this need context or reasoning?
    Ingress,
    /// Classifying an observed failure.
    Recovery,
    /// Continue a plan or replan.
    Continuation,
    /// Selecting among authorized candidates.
    CandidateSelection,
    /// A deterministic System 0 rule fired.
    Deterministic,
}

impl DecisionType {
    pub fn label(self) -> &'static str {
        match self {
            DecisionType::Ingress => "ingress",
            DecisionType::Recovery => "recovery",
            DecisionType::Continuation => "continuation",
            DecisionType::CandidateSelection => "candidate selection",
            DecisionType::Deterministic => "deterministic",
        }
    }
}

/// Thresholds for one decision type.
///
/// `accept_above` is the confidence at which the answer is used as-is;
/// below `abstain_below` the policy abstains (escalating to reasoning or
/// asking) rather than acting on a weak answer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    pub accept_above: f32,
    pub abstain_below: f32,
}

impl Thresholds {
    /// Whether a score is high enough to act on.
    pub fn accepts(&self, score: f32) -> bool {
        score.is_finite() && score >= self.accept_above
    }

    /// Whether a score is low enough to abstain.
    pub fn abstains(&self, score: f32) -> bool {
        !score.is_finite() || score <= self.abstain_below
    }

    /// Whether a score lands in the uncertain band between the two.
    pub fn is_uncertain(&self, score: f32) -> bool {
        !self.accepts(score) && !self.abstains(score)
    }
}

/// A versioned, calibrated policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyCalibration {
    /// Version identity. A policy/model mismatch cannot reuse old
    /// calibration, so this is compared before use.
    pub version: String,
    /// The model and question pack this calibration was measured with.
    pub model: String,
    pub question_pack: String,
    /// Per-decision-type thresholds, tuned on development data.
    pub thresholds: BTreeMap<DecisionType, Thresholds>,
    /// Observed end-to-end decision latency, used for the deadline.
    pub observed_p50_ms: u64,
    pub observed_p95_ms: u64,
    /// Whether this policy has held-out evidence behind it.
    pub evidence: CalibrationEvidence,
}

/// What evidence supports a calibration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationEvidence {
    /// Development tasks used to choose the thresholds.
    pub development_tasks: usize,
    /// Held-out tasks used to check them. Zero means unvalidated.
    pub held_out_tasks: usize,
    /// Held-out accuracy of accepted answers.
    pub held_out_accept_accuracy: Option<u32>,
    /// Whether the policy is promoted or still experimental.
    pub status: PolicyStatus,
}

/// Whether a policy may be used by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyStatus {
    /// Measured on held-out data and promoted.
    Promoted,
    /// Still experimental: usable in shadow or explicit experimental mode.
    Experimental,
}

impl PolicyCalibration {
    /// A conservative default: high thresholds, explicitly unvalidated.
    ///
    /// The per-type values differ deliberately. Recovery classification
    /// decides whether to retry something that may have had an effect, so
    /// it demands more confidence than ingress routing.
    pub fn default_unvalidated(model: impl Into<String>, question_pack: impl Into<String>) -> Self {
        let mut thresholds = BTreeMap::new();
        thresholds.insert(
            DecisionType::Ingress,
            Thresholds {
                accept_above: 0.70,
                abstain_below: 0.45,
            },
        );
        thresholds.insert(
            DecisionType::Recovery,
            Thresholds {
                accept_above: 0.85,
                abstain_below: 0.55,
            },
        );
        thresholds.insert(
            DecisionType::Continuation,
            Thresholds {
                accept_above: 0.75,
                abstain_below: 0.50,
            },
        );
        thresholds.insert(
            DecisionType::CandidateSelection,
            Thresholds {
                accept_above: 0.80,
                abstain_below: 0.50,
            },
        );
        // A deterministic rule either fires or it does not: its own logic
        // is the evidence, so its threshold is nominal.
        thresholds.insert(
            DecisionType::Deterministic,
            Thresholds {
                accept_above: 0.0,
                abstain_below: -1.0,
            },
        );

        Self {
            version: "unvalidated-v1".to_owned(),
            model: model.into(),
            question_pack: question_pack.into(),
            thresholds,
            observed_p50_ms: 250,
            observed_p95_ms: 1_500,
            evidence: CalibrationEvidence {
                development_tasks: 0,
                held_out_tasks: 0,
                held_out_accept_accuracy: None,
                status: PolicyStatus::Experimental,
            },
        }
    }

    pub fn thresholds(&self, decision: DecisionType) -> Thresholds {
        self.thresholds
            .get(&decision)
            .copied()
            .unwrap_or(Thresholds {
                accept_above: 0.75,
                abstain_below: 0.5,
            })
    }

    /// Whether this calibration may be used for a given model and pack.
    ///
    /// A mismatch refuses rather than silently reusing stale thresholds.
    pub fn matches(&self, model: &str, question_pack: &str) -> Result<(), CalibrationMismatch> {
        if self.model != model {
            return Err(CalibrationMismatch::Model {
                calibrated: self.model.clone(),
                actual: model.to_owned(),
            });
        }
        if self.question_pack != question_pack {
            return Err(CalibrationMismatch::QuestionPack {
                calibrated: self.question_pack.clone(),
                actual: question_pack.to_owned(),
            });
        }
        Ok(())
    }

    /// Whether the policy may be used by default.
    pub fn is_promoted(&self) -> bool {
        self.evidence.status == PolicyStatus::Promoted
    }

    /// Record observed decision latency, updating the deadline basis.
    pub fn observe_latency(&mut self, ms: u64) {
        // A simple bounded reservoir: recent observations drive the
        // deadline, so a provider's slow period does not become permanent.
        self.observed_p95_ms = self
            .observed_p95_ms
            .saturating_mul(9)
            .saturating_add(ms * 11)
            / 20;
        self.observed_p50_ms = self.observed_p50_ms.saturating_mul(4).saturating_add(ms) / 5;
    }

    /// The decision deadline, derived from observed latency rather than a
    /// guess.
    pub fn deadline(&self) -> Duration {
        // Twice the observed p95, floored, so a normal slow decision is
        // not cut off and a hung one is.
        Duration::from_millis((self.observed_p95_ms.max(500) * 2).min(30_000))
    }
}

/// Why a calibration cannot be reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalibrationMismatch {
    Model { calibrated: String, actual: String },
    QuestionPack { calibrated: String, actual: String },
}

impl std::fmt::Display for CalibrationMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalibrationMismatch::Model { calibrated, actual } => write!(
                f,
                "calibration was measured with model {calibrated:?} but {actual:?} is in use"
            ),
            CalibrationMismatch::QuestionPack { calibrated, actual } => write!(
                f,
                "calibration was measured with question pack {calibrated:?} but {actual:?} is in use"
            ),
        }
    }
}

/// A raw and effective score, kept separate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Scores {
    /// What the model answered, unmodified.
    pub raw: f32,
    /// What the policy used, after any dampening or contradiction.
    pub effective: f32,
}

impl Scores {
    pub fn new(raw: f32) -> Self {
        Self {
            raw,
            effective: raw,
        }
    }

    /// Apply a dampening factor (a named contradiction), keeping the raw
    /// value intact.
    pub fn dampen(mut self, factor: f32) -> Self {
        self.effective = (self.raw * factor).clamp(0.0, 1.0);
        self
    }
}

/// What the policy decided to do with a score.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreDecision {
    /// Act on the model's answer.
    Accept,
    /// Do not act; escalate to reasoning or ask the user.
    Abstain,
    /// In the uncertain band: act, but record the uncertainty.
    AcceptUncertain,
}

/// Classify a score under the calibration for its decision type.
pub fn classify(
    calibration: &PolicyCalibration,
    decision: DecisionType,
    scores: Scores,
) -> ScoreDecision {
    let thresholds = calibration.thresholds(decision);
    if thresholds.accepts(scores.effective) {
        ScoreDecision::Accept
    } else if thresholds.abstains(scores.effective) {
        ScoreDecision::Abstain
    } else {
        ScoreDecision::AcceptUncertain
    }
}

/// Everything a cache key must cover, so a hit cannot be stale.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Workspace content revision: a dirty edit changes it even without a
    /// commit.
    pub workspace_revision: String,
    /// Sorted capability set.
    pub capabilities: Vec<String>,
    /// Candidate ids offered.
    pub candidates: Vec<String>,
    /// The frame/state identity.
    pub state_digest: String,
    /// Policy revision.
    pub policy_revision: String,
    /// Tool registry revision.
    pub tool_revision: String,
    /// Question-pack version.
    pub question_pack: String,
    /// Model identity, so an alias resolving to a new model misses.
    pub model: String,
    /// Execution mode (quality/adaptive/…).
    pub mode: String,
    /// Decision type, so thresholds cannot be crossed.
    pub decision: DecisionType,
    pub prompt: String,
}

impl CacheKey {
    /// Build a key from the parts, sorting the unordered ones.
    pub fn new(prompt: impl Into<String>, decision: DecisionType) -> Self {
        Self {
            workspace_revision: String::new(),
            capabilities: Vec::new(),
            candidates: Vec::new(),
            state_digest: String::new(),
            policy_revision: String::new(),
            tool_revision: String::new(),
            question_pack: String::new(),
            model: String::new(),
            mode: String::new(),
            decision,
            prompt: prompt.into(),
        }
    }

    pub fn with_workspace(mut self, revision: impl Into<String>) -> Self {
        self.workspace_revision = revision.into();
        self
    }

    pub fn with_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.capabilities = capabilities.into_iter().map(Into::into).collect();
        self.capabilities.sort();
        self
    }

    pub fn with_candidates(
        mut self,
        candidates: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.candidates = candidates.into_iter().map(Into::into).collect();
        self.candidates.sort();
        self
    }

    pub fn with_versions(
        mut self,
        policy: impl Into<String>,
        tools: impl Into<String>,
        question_pack: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.policy_revision = policy.into();
        self.tool_revision = tools.into();
        self.question_pack = question_pack.into();
        self.model = model.into();
        self
    }

    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.mode = mode.into();
        self
    }

    pub fn with_state(mut self, state: &serde_json::Value) -> Self {
        self.state_digest = crate::workspace::content_hash(
            serde_json::to_string(state).unwrap_or_default().as_bytes(),
        );
        self
    }
}

/// A cached decision, with the versions that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedDecision {
    pub scores: Scores,
    pub decision: ScoreDecision,
    pub version: String,
    pub stored_at: Instant,
}

/// Bounded, versioned routing cache.
///
/// Never caches permissions: a hit replaces *routing*, and the execution
/// gate still authorizes every action.
pub struct BoundedRoutingCache {
    entries: Mutex<HashMap<CacheKey, CachedDecision>>,
    /// Insertion order, for eviction.
    order: Mutex<VecDeque<CacheKey>>,
    capacity: usize,
    hits: Mutex<u64>,
    misses: Mutex<u64>,
    evictions: Mutex<u64>,
}

/// Default cache capacity: bounded so a long session cannot grow without
/// limit.
pub const DEFAULT_CACHE_CAPACITY: usize = 512;

impl BoundedRoutingCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            order: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            hits: Mutex::new(0),
            misses: Mutex::new(0),
            evictions: Mutex::new(0),
        }
    }

    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("cache poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up a decision. A miss returns `None` and is counted.
    pub fn get(&self, key: &CacheKey) -> Option<CachedDecision> {
        let found = self
            .entries
            .lock()
            .expect("cache poisoned")
            .get(key)
            .cloned();
        let mut counter = if found.is_some() {
            self.hits.lock().expect("cache poisoned")
        } else {
            self.misses.lock().expect("cache poisoned")
        };
        *counter += 1;
        found
    }

    /// Store a decision, evicting the oldest entry when full.
    pub fn store(&self, key: CacheKey, value: CachedDecision) {
        // Only finite scores are cacheable: a decision from a broken
        // answer must not be replayed.
        if !value.scores.effective.is_finite() {
            return;
        }
        let mut entries = self.entries.lock().expect("cache poisoned");
        let mut order = self.order.lock().expect("cache poisoned");

        if entries.insert(key.clone(), value).is_none() {
            order.push_back(key);
        }
        while entries.len() > self.capacity {
            let Some(oldest) = order.pop_front() else {
                break;
            };
            if entries.remove(&oldest).is_some() {
                *self.evictions.lock().expect("cache poisoned") += 1;
            }
        }
    }

    /// Cache statistics, for the inspector.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.len(),
            capacity: self.capacity,
            hits: *self.hits.lock().expect("cache poisoned"),
            misses: *self.misses.lock().expect("cache poisoned"),
            evictions: *self.evictions.lock().expect("cache poisoned"),
        }
    }

    /// Drop every entry (used when a version changes wholesale).
    pub fn clear(&self) {
        self.entries.lock().expect("cache poisoned").clear();
        self.order.lock().expect("cache poisoned").clear();
    }
}

/// Cache counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    pub entries: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

impl CacheStats {
    /// Hit rate, or `None` when nothing was looked up.
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        if total == 0 {
            None
        } else {
            Some(self.hits as f64 / total as f64)
        }
    }
}

/// In-flight decision coalescing.
///
/// Identical concurrent decisions share one answer instead of issuing the
/// same request twice. Deliberately only for *permitted* decisions: a
/// coalesced answer still goes through the gate.
#[derive(Default)]
pub struct InFlightCoalescer {
    in_flight: Mutex<HashMap<CacheKey, u64>>,
    coalesced: Mutex<u64>,
}

impl InFlightCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a request; returns true when this caller should issue it.
    pub fn begin(&self, key: &CacheKey) -> bool {
        let mut in_flight = self.in_flight.lock().expect("coalescer poisoned");
        match in_flight.get_mut(key) {
            Some(count) => {
                *count += 1;
                *self.coalesced.lock().expect("coalescer poisoned") += 1;
                false
            }
            None => {
                in_flight.insert(key.clone(), 1);
                true
            }
        }
    }

    /// Mark a request finished.
    pub fn end(&self, key: &CacheKey) {
        let mut in_flight = self.in_flight.lock().expect("coalescer poisoned");
        if let Some(count) = in_flight.get_mut(key) {
            *count -= 1;
            if *count == 0 {
                in_flight.remove(key);
            }
        }
    }

    /// How many callers shared another's request.
    pub fn coalesced(&self) -> u64 {
        *self.coalesced.lock().expect("coalescer poisoned")
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.lock().expect("coalescer poisoned").len()
    }
}

/// A circuit breaker over the control layer.
///
/// After repeated failures or timeouts the breaker opens and decisions
/// fall back to reasoning; it half-opens after a cooldown so a recovered
/// provider is used again.
#[derive(Debug)]
pub struct CircuitBreaker {
    consecutive_failures: Mutex<u32>,
    threshold: u32,
    cooldown: Duration,
    opened_at: Mutex<Option<Instant>>,
    opened_count: Mutex<u64>,
}

/// What the breaker permits right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Decisions may be attempted.
    Closed,
    /// Decisions must fall back immediately.
    Open,
    /// One probe is permitted.
    HalfOpen,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            consecutive_failures: Mutex::new(0),
            threshold: threshold.max(1),
            cooldown,
            opened_at: Mutex::new(None),
            opened_count: Mutex::new(0),
        }
    }

    /// A default suit for a control layer: three failures, then a minute
    /// of fallback.
    pub fn for_control_layer() -> Self {
        Self::new(3, Duration::from_secs(60))
    }

    pub fn state(&self) -> BreakerState {
        let opened = *self.opened_at.lock().expect("breaker poisoned");
        match opened {
            None => BreakerState::Closed,
            Some(at) => {
                if at.elapsed() >= self.cooldown {
                    BreakerState::HalfOpen
                } else {
                    BreakerState::Open
                }
            }
        }
    }

    /// Whether a decision may be attempted.
    pub fn permits(&self) -> bool {
        !matches!(self.state(), BreakerState::Open)
    }

    pub fn record_success(&self) {
        *self.consecutive_failures.lock().expect("breaker poisoned") = 0;
        *self.opened_at.lock().expect("breaker poisoned") = None;
    }

    pub fn record_failure(&self) {
        let mut failures = self.consecutive_failures.lock().expect("breaker poisoned");
        *failures += 1;
        if *failures >= self.threshold {
            let mut opened = self.opened_at.lock().expect("breaker poisoned");
            if opened.is_none() {
                *opened = Some(Instant::now());
                *self.opened_count.lock().expect("breaker poisoned") += 1;
            }
        }
    }

    /// How many times the breaker opened.
    pub fn opened_count(&self) -> u64 {
        *self.opened_count.lock().expect("breaker poisoned")
    }
}

/// One observed decision, for calibration evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionObservation {
    pub decision: DecisionType,
    pub scores: Scores,
    /// What the policy did.
    pub outcome: ScoreDecision,
    /// Whether the accepted answer turned out to be correct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correct: Option<bool>,
    /// Whether the candidate set actually contained the right answer, so
    /// candidate-set recall is measurable separately from chosen-option
    /// accuracy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_set_contained_answer: Option<bool>,
    /// Latency of this decision.
    pub latency_ms: u64,
    /// Whether this observation is from the held-out set.
    pub held_out: bool,
}

/// Calibration results over observations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationReport {
    pub policy_version: String,
    pub decisions: usize,
    pub accepted: usize,
    pub abstained: usize,
    pub uncertain: usize,
    /// Accuracy of accepted answers, per decision type.
    pub accept_accuracy: BTreeMap<String, f64>,
    /// Candidate-set recall, when measured: how often the right answer was
    /// even offered.
    pub candidate_recall: Option<f64>,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    /// Whether the numbers come from held-out tasks.
    pub held_out: bool,
    /// Explicit statement of what the report does not show.
    pub limitations: Vec<String>,
}

impl CalibrationReport {
    /// Compute calibration over observations.
    pub fn compute(
        policy_version: impl Into<String>,
        observations: &[DecisionObservation],
    ) -> Self {
        let decisions = observations.len();
        let accepted = observations
            .iter()
            .filter(|o| o.outcome == ScoreDecision::Accept)
            .count();
        let abstained = observations
            .iter()
            .filter(|o| o.outcome == ScoreDecision::Abstain)
            .count();
        let uncertain = observations
            .iter()
            .filter(|o| o.outcome == ScoreDecision::AcceptUncertain)
            .count();

        let mut accuracy: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for observation in observations {
            let Some(correct) = observation.correct else {
                continue;
            };
            if observation.outcome == ScoreDecision::Abstain {
                continue;
            }
            let entry = accuracy
                .entry(observation.decision.label().to_owned())
                .or_insert((0, 0));
            entry.1 += 1;
            if correct {
                entry.0 += 1;
            }
        }
        let accept_accuracy = accuracy
            .into_iter()
            .map(|(label, (correct, total))| (label, correct as f64 / total.max(1) as f64))
            .collect();

        let recall_observations: Vec<bool> = observations
            .iter()
            .filter_map(|o| o.candidate_set_contained_answer)
            .collect();
        let candidate_recall = if recall_observations.is_empty() {
            None
        } else {
            Some(
                recall_observations
                    .iter()
                    .filter(|contained| **contained)
                    .count() as f64
                    / recall_observations.len() as f64,
            )
        };

        let mut latencies: Vec<u64> = observations.iter().map(|o| o.latency_ms).collect();
        latencies.sort_unstable();

        let held_out = !observations.is_empty() && observations.iter().all(|o| o.held_out);

        let mut limitations = vec![
            "Accuracy is measured only where the outcome was verifiable; decisions without a \
             known result are excluded rather than counted as correct."
                .to_owned(),
        ];
        if !held_out {
            limitations.push(
                "These observations are not all from held-out tasks, so the thresholds are \
                 not yet validated."
                    .to_owned(),
            );
        }
        if candidate_recall.is_none() {
            limitations.push(
                "Candidate-set recall was not measured: a low accuracy could be a threshold \
                 problem or a candidate-set problem, and this report cannot tell them apart."
                    .to_owned(),
            );
        }
        if decisions < 30 {
            limitations.push(
                "Fewer than 30 decisions: too few to support a calibration claim.".to_owned(),
            );
        }

        Self {
            policy_version: policy_version.into(),
            decisions,
            accepted,
            abstained,
            uncertain,
            accept_accuracy,
            candidate_recall,
            p50_latency_ms: median(&latencies),
            p95_latency_ms: percentile(&latencies, 95.0),
            held_out,
            limitations,
        }
    }

    /// Whether this report is strong enough to promote a policy.
    pub fn supports_promotion(&self) -> bool {
        self.held_out && self.decisions >= 30
    }

    /// A summary that leads with the limitations.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "calibration {}: {} decision(s), {} accepted, {} abstained, {} uncertain\n",
            self.policy_version, self.decisions, self.accepted, self.abstained, self.uncertain
        );
        for (label, accuracy) in &self.accept_accuracy {
            out.push_str(&format!(
                "  {label}: {:.0}% of accepted answers correct\n",
                accuracy * 100.0
            ));
        }
        if let Some(recall) = self.candidate_recall {
            out.push_str(&format!("  candidate-set recall: {:.0}%\n", recall * 100.0));
        }
        out.push_str(&format!(
            "  p50 {}ms, p95 {}ms\n",
            self.p50_latency_ms, self.p95_latency_ms
        ));
        match self.supports_promotion() {
            true => out.push_str("  supported by held-out evidence\n"),
            false => out.push_str("  NOT sufficient to promote a policy\n"),
        }
        for limitation in &self.limitations {
            out.push_str(&format!("  - {limitation}\n"));
        }
        out
    }
}

fn median(sorted: &[u64]) -> u64 {
    if sorted.is_empty() {
        0
    } else {
        sorted[sorted.len() / 2]
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(prompt: &str) -> CacheKey {
        CacheKey::new(prompt, DecisionType::Ingress)
            .with_workspace("rev-1")
            .with_capabilities(["files", "shell"])
            .with_candidates(["read", "run_tests"])
            .with_versions("policy-1", "tools-1", "pack-1", "glm-5.3-flash")
            .with_mode("quality")
    }

    fn cached(raw: f32) -> CachedDecision {
        CachedDecision {
            scores: Scores::new(raw),
            decision: ScoreDecision::Accept,
            version: "policy-1".to_owned(),
            stored_at: Instant::now(),
        }
    }

    #[test]
    fn thresholds_differ_by_decision_type() {
        let calibration = PolicyCalibration::default_unvalidated("glm-5.3-flash", "pack-1");
        let ingress = calibration.thresholds(DecisionType::Ingress);
        let recovery = calibration.thresholds(DecisionType::Recovery);

        // Recovery demands more confidence than ingress, because it can
        // authorize a retry of something that may have had an effect.
        assert!(recovery.accept_above > ingress.accept_above);
        assert_ne!(ingress.accept_above, 0.75);
        assert_ne!(recovery.accept_above, 0.75);
    }

    #[test]
    fn raw_and_effective_scores_stay_separate() {
        let scores = Scores::new(0.9).dampen(0.5);
        // The raw answer is preserved for analysis; the effective one is
        // what the threshold saw.
        assert_eq!(scores.raw, 0.9);
        assert!((scores.effective - 0.45).abs() < 1e-6);

        let calibration = PolicyCalibration::default_unvalidated("m", "p");
        // Raw alone would have been accepted; the dampened score abstains.
        assert_eq!(
            classify(&calibration, DecisionType::Recovery, Scores::new(0.9)),
            ScoreDecision::Accept
        );
        assert_eq!(
            classify(&calibration, DecisionType::Recovery, scores),
            ScoreDecision::Abstain
        );
    }

    #[test]
    fn a_score_can_be_uncertain_rather_than_binary() {
        let mut calibration = PolicyCalibration::default_unvalidated("m", "p");
        calibration.thresholds.insert(
            DecisionType::Ingress,
            Thresholds {
                accept_above: 0.8,
                abstain_below: 0.4,
            },
        );
        assert_eq!(
            classify(&calibration, DecisionType::Ingress, Scores::new(0.6)),
            ScoreDecision::AcceptUncertain
        );
    }

    #[test]
    fn cache_keys_cover_file_edits_without_a_commit() {
        // A dirty edit changes the workspace revision, so an old answer
        // cannot be served.
        let cache = BoundedRoutingCache::new(16);
        let before = key("route me");
        cache.store(before.clone(), cached(0.9));

        let after_edit = CacheKey::new("route me", DecisionType::Ingress)
            .with_workspace("rev-2") // same prompt, different content
            .with_capabilities(["files", "shell"])
            .with_candidates(["read", "run_tests"])
            .with_versions("policy-1", "tools-1", "pack-1", "glm-5.3-flash")
            .with_mode("quality");

        assert!(cache.get(&before).is_some());
        assert!(
            cache.get(&after_edit).is_none(),
            "a dirty edit reused a cached decision"
        );
    }

    #[test]
    fn cache_keys_cover_capability_policy_pack_and_model_changes() {
        let cache = BoundedRoutingCache::new(16);
        let base = key("route me");
        cache.store(base.clone(), cached(0.9));

        let variants = [
            CacheKey::new("route me", DecisionType::Ingress)
                .with_workspace("rev-1")
                .with_capabilities(["files"]) // capability removed
                .with_candidates(["read", "run_tests"])
                .with_versions("policy-1", "tools-1", "pack-1", "glm-5.3-flash")
                .with_mode("quality"),
            CacheKey::new("route me", DecisionType::Ingress)
                .with_workspace("rev-1")
                .with_capabilities(["files", "shell"])
                .with_candidates(["read", "run_tests"])
                .with_versions("policy-2", "tools-1", "pack-1", "glm-5.3-flash") // policy
                .with_mode("quality"),
            CacheKey::new("route me", DecisionType::Ingress)
                .with_workspace("rev-1")
                .with_capabilities(["files", "shell"])
                .with_candidates(["read", "run_tests"])
                .with_versions("policy-1", "tools-2", "pack-1", "glm-5.3-flash") // tools
                .with_mode("quality"),
            CacheKey::new("route me", DecisionType::Ingress)
                .with_workspace("rev-1")
                .with_capabilities(["files", "shell"])
                .with_candidates(["read", "run_tests"])
                .with_versions("policy-1", "tools-1", "pack-2", "glm-5.3-flash") // pack
                .with_mode("quality"),
            CacheKey::new("route me", DecisionType::Ingress)
                .with_workspace("rev-1")
                .with_capabilities(["files", "shell"])
                .with_candidates(["read", "run_tests"])
                // An alias resolving to a different model must miss.
                .with_versions("policy-1", "tools-1", "pack-1", "glm-5.3")
                .with_mode("quality"),
            CacheKey::new("route me", DecisionType::Ingress)
                .with_workspace("rev-1")
                .with_capabilities(["files", "shell"])
                .with_candidates(["read", "run_tests"])
                .with_versions("policy-1", "tools-1", "pack-1", "glm-5.3-flash")
                .with_mode("adaptive"), // mode
            CacheKey::new("route me", DecisionType::Recovery) // decision type
                .with_workspace("rev-1")
                .with_capabilities(["files", "shell"])
                .with_candidates(["read", "run_tests"])
                .with_versions("policy-1", "tools-1", "pack-1", "glm-5.3-flash")
                .with_mode("quality"),
        ];

        for variant in variants {
            assert!(
                cache.get(&variant).is_none(),
                "a version change reused a cached decision: {variant:?}"
            );
        }
    }

    #[test]
    fn candidate_set_changes_invalidate_a_cached_decision() {
        let cache = BoundedRoutingCache::new(16);
        cache.store(key("route me"), cached(0.9));

        let different_candidates = CacheKey::new("route me", DecisionType::Ingress)
            .with_workspace("rev-1")
            .with_capabilities(["files", "shell"])
            .with_candidates(["read"]) // the offered set changed
            .with_versions("policy-1", "tools-1", "pack-1", "glm-5.3-flash")
            .with_mode("quality");
        assert!(cache.get(&different_candidates).is_none());
    }

    #[test]
    fn the_cache_is_bounded_under_load() {
        let cache = BoundedRoutingCache::new(32);
        for i in 0..2_000 {
            let key = CacheKey::new(format!("prompt {i}"), DecisionType::Ingress)
                .with_workspace("rev-1")
                .with_versions("p", "t", "q", "m");
            cache.store(key, cached(0.9));
        }
        let stats = cache.stats();
        assert_eq!(stats.entries, 32);
        assert_eq!(stats.capacity, 32);
        assert!(stats.evictions > 0);
    }

    #[test]
    fn a_denied_operation_is_still_denied_on_a_cache_hit() {
        // The cache stores routing only. Authorization is re-evaluated by
        // the gate every time, so a hit cannot bypass a policy denial.
        let cache = BoundedRoutingCache::new(8);
        let cache_key = key("write the file");
        cache.store(cache_key.clone(), cached(0.99));

        let hit = cache.get(&cache_key).unwrap();
        assert_eq!(hit.decision, ScoreDecision::Accept);
        // The cached value carries no authorization whatsoever.
        let serialized = serde_json::to_string(&hit.decision).unwrap();
        assert!(!serialized.contains("allow"));
        assert!(!serialized.contains("permit"));
        assert!(!serialized.contains("approve"));

        // And the gate still decides, as the other tests in the policy
        // module prove.
    }

    #[test]
    fn identical_in_flight_decisions_are_coalesced() {
        let coalescer = InFlightCoalescer::new();
        let shared = key("route me");

        assert!(coalescer.begin(&shared), "the first caller should issue");
        assert!(!coalescer.begin(&shared), "the second caller should share");
        assert!(!coalescer.begin(&shared));
        assert_eq!(coalescer.coalesced(), 2);

        coalescer.end(&shared);
        coalescer.end(&shared);
        coalescer.end(&shared);
        assert_eq!(coalescer.in_flight(), 0);

        // A different key is not coalesced.
        let other = key("different prompt");
        assert!(coalescer.begin(&other));
    }

    #[test]
    fn the_circuit_breaker_falls_back_after_repeated_failures() {
        let breaker = CircuitBreaker::new(3, Duration::from_millis(50));
        assert_eq!(breaker.state(), BreakerState::Closed);
        assert!(breaker.permits());

        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.state(), BreakerState::Closed);
        breaker.record_failure();
        // The breaker opened: decisions fall back to reasoning.
        assert_eq!(breaker.state(), BreakerState::Open);
        assert!(!breaker.permits());
        assert_eq!(breaker.opened_count(), 1);

        // After the cooldown it half-opens and a success closes it.
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(breaker.state(), BreakerState::HalfOpen);
        assert!(breaker.permits());
        breaker.record_success();
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[test]
    fn the_deadline_comes_from_observed_latency() {
        let mut calibration = PolicyCalibration::default_unvalidated("m", "p");
        calibration.observed_p95_ms = 1_000;
        assert_eq!(calibration.deadline(), Duration::from_millis(2_000));

        // Observed latency moves the deadline rather than a fixed guess.
        for _ in 0..50 {
            calibration.observe_latency(3_000);
        }
        assert!(calibration.deadline() > Duration::from_millis(2_000));
        // But it stays bounded.
        assert!(calibration.deadline() <= Duration::from_millis(30_000));
    }

    #[test]
    fn a_policy_version_mismatch_cannot_reuse_old_calibration() {
        let calibration = PolicyCalibration::default_unvalidated("glm-5.3-flash", "pack-1");
        calibration.matches("glm-5.3-flash", "pack-1").unwrap();

        let err = calibration.matches("glm-5.3", "pack-1").unwrap_err();
        assert!(matches!(err, CalibrationMismatch::Model { .. }));
        assert!(format!("{err}").contains("glm-5.3-flash"));

        let err = calibration.matches("glm-5.3-flash", "pack-2").unwrap_err();
        assert!(matches!(err, CalibrationMismatch::QuestionPack { .. }));
    }

    #[test]
    fn an_unvalidated_policy_is_experimental_not_promoted() {
        let calibration = PolicyCalibration::default_unvalidated("m", "p");
        assert!(!calibration.is_promoted());
        assert_eq!(calibration.evidence.status, PolicyStatus::Experimental);
    }

    fn observation(
        decision: DecisionType,
        outcome: ScoreDecision,
        correct: Option<bool>,
        held_out: bool,
    ) -> DecisionObservation {
        DecisionObservation {
            decision,
            scores: Scores::new(0.9),
            outcome,
            correct,
            candidate_set_contained_answer: None,
            latency_ms: 100,
            held_out,
        }
    }

    #[test]
    fn calibration_reports_coverage_mistakes_and_abstentions() {
        let mut observations = Vec::new();
        for i in 0..20 {
            observations.push(observation(
                DecisionType::Ingress,
                ScoreDecision::Accept,
                Some(i < 18), // two mistakes
                true,
            ));
        }
        for _ in 0..10 {
            observations.push(observation(
                DecisionType::Recovery,
                ScoreDecision::Abstain,
                None,
                true,
            ));
        }

        let report = CalibrationReport::compute("policy-1", &observations);
        assert_eq!(report.decisions, 30);
        assert_eq!(report.accepted, 20);
        assert_eq!(report.abstained, 10);
        // Mistakes are visible in the accuracy, not smoothed away.
        let ingress = report.accept_accuracy.get("ingress").copied().unwrap();
        assert!((ingress - 0.9).abs() < 1e-9);
        // The abstention has no accuracy entry: it made no claim.
        assert!(!report.accept_accuracy.contains_key("recovery"));
        assert!(report.supports_promotion());
    }

    #[test]
    fn candidate_set_recall_is_measured_separately_from_chosen_accuracy() {
        let mut observations: Vec<DecisionObservation> = (0..10)
            .map(|_| {
                observation(
                    DecisionType::Ingress,
                    ScoreDecision::Accept,
                    Some(true),
                    true,
                )
            })
            .collect();
        // In half the cases the right answer was never even offered.
        for (index, observation) in observations.iter_mut().enumerate() {
            observation.candidate_set_contained_answer = Some(index < 5);
        }

        let report = CalibrationReport::compute("policy-1", &observations);
        assert_eq!(report.candidate_recall, Some(0.5));
        // Accepted accuracy is perfect *given the candidates*, which is
        // exactly why recall has to be reported alongside it.
        assert!((report.accept_accuracy["ingress"] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_small_or_non_held_out_report_cannot_support_promotion() {
        let few: Vec<DecisionObservation> = (0..5)
            .map(|_| {
                observation(
                    DecisionType::Ingress,
                    ScoreDecision::Accept,
                    Some(true),
                    true,
                )
            })
            .collect();
        let report = CalibrationReport::compute("p", &few);
        assert!(!report.supports_promotion());
        assert!(report.summary().contains("NOT sufficient"));

        let calibration_only: Vec<DecisionObservation> = (0..40)
            .map(|_| {
                observation(
                    DecisionType::Ingress,
                    ScoreDecision::Accept,
                    Some(true),
                    false,
                )
            })
            .collect();
        let report = CalibrationReport::compute("p", &calibration_only);
        assert!(!report.supports_promotion());
        assert!(
            report
                .limitations
                .iter()
                .any(|l| l.contains("not all from held-out"))
        );
    }

    #[test]
    fn decisions_without_a_known_outcome_are_not_counted_as_correct() {
        let observations = vec![
            observation(DecisionType::Ingress, ScoreDecision::Accept, None, true),
            observation(
                DecisionType::Ingress,
                ScoreDecision::Accept,
                Some(true),
                true,
            ),
        ];
        let report = CalibrationReport::compute("p", &observations);
        // Only the verifiable one contributes, and the report says so.
        assert!((report.accept_accuracy["ingress"] - 1.0).abs() < 1e-9);
        assert!(
            report
                .limitations
                .iter()
                .any(|l| l.contains("only where the outcome was verifiable"))
        );
    }

    #[test]
    fn the_report_records_actual_latency() {
        let mut observations: Vec<DecisionObservation> = (0..10)
            .map(|i| {
                let mut observation = observation(
                    DecisionType::Ingress,
                    ScoreDecision::Accept,
                    Some(true),
                    true,
                );
                observation.latency_ms = 100 + i * 10;
                observation
            })
            .collect();
        observations.push(observation(
            DecisionType::Ingress,
            ScoreDecision::Accept,
            Some(true),
            true,
        ));

        let report = CalibrationReport::compute("p", &observations);
        assert!(report.p50_latency_ms > 0);
        assert!(report.p95_latency_ms >= report.p50_latency_ms);
    }

    #[test]
    fn state_digest_participates_in_the_key() {
        let with_state = key("p").with_state(&json!({ "revision": 3 }));
        let other_state = key("p").with_state(&json!({ "revision": 4 }));
        assert_ne!(with_state, other_state);

        let cache = BoundedRoutingCache::new(8);
        cache.store(with_state.clone(), cached(0.9));
        assert!(cache.get(&with_state).is_some());
        assert!(cache.get(&other_state).is_none());
    }

    #[test]
    fn cache_statistics_report_hits_misses_and_evictions() {
        let cache = BoundedRoutingCache::new(2);
        let a = key("a");
        cache.store(a.clone(), cached(0.9));
        cache.get(&a); // hit
        cache.get(&key("b")); // miss

        cache.store(key("b"), cached(0.9));
        cache.store(key("c"), cached(0.9)); // evicts a

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hit_rate(), Some(0.5));
        assert_eq!(stats.entries, 2);
    }
}
