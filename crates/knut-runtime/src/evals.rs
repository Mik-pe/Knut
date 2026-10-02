use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::judgment::IngressJudgments;
use crate::{
    Action, Decision, DecisionInput, DecisionSource, EdgeJudgment, KnutError, ModelTier, Route,
    SystemOne,
};

/// The end state of one executed turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Success,
    Failure,
    Blocked,
    Denied,
    Cancelled,
    Clarified,
}

/// Gold labels for offline scoring; absent means unlabelled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expectation {
    /// Expected route (e.g. the task needed a tool).
    pub route: Option<Route>,
    /// Expected capability for Act turns.
    pub capability: Option<String>,
}

/// Everything worth knowing about one turn, serializable for replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnTrace {
    pub prompt: String,
    pub source: DecisionSource,
    /// Raw judgments when the batched ingress path produced the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgments: Option<IngressJudgments>,
    pub decision: Decision,
    pub action: Action,
    /// What a shadow router would have done, when one ran. A shadow
    /// recommendation is *observable disagreement*: it is not evidence
    /// that the executed decision was wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Decision>,
    /// Post-node edge decisions, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<EdgeJudgment>,
    /// Why each escalation happened, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escalation_reasons: Vec<String>,
    /// Total wall-clock for the turn, kept for compatibility. Prefer
    /// `phases` for analysis: total HTTP time is not inference time.
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub outcome: TurnOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Expectation>,

    /// The exact routing input this decision was made over, so
    /// re-evaluation can reproduce it rather than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_input: Option<RoutingInput>,
    /// Versions of the things that shaped the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<TraceVersions>,
    /// Every model attempt actually made, oldest first. Cost and usage are
    /// summed from these, not from the initially selected tier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<TraceAttempt>,
    /// Separated latency phases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<LatencyPhases>,
    /// Whether this record is complete enough to re-evaluate.
    #[serde(default)]
    pub replayable: bool,
    /// Why it is not replayable, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_replayable_reason: Option<String>,
}

impl TurnTrace {
    /// A minimal trace, with the analysis fields left unset.
    ///
    /// Traces built this way are explicitly *not* replayable: they lack
    /// the exact routing input, so re-evaluating them would run over a
    /// different world. Call [`TurnTrace::with_routing_input`] to make one
    /// replayable.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        prompt: impl Into<String>,
        source: DecisionSource,
        decision: Decision,
        action: Action,
        latency_ms: u64,
        input_tokens: u64,
        output_tokens: u64,
        outcome: TurnOutcome,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            source,
            judgments: None,
            decision,
            action,
            shadow: None,
            edges: Vec::new(),
            escalation_reasons: Vec::new(),
            latency_ms,
            input_tokens,
            output_tokens,
            outcome,
            expected: None,
            routing_input: None,
            versions: None,
            attempts: Vec::new(),
            phases: None,
            replayable: false,
            not_replayable_reason: Some(
                "no routing input was recorded, so re-evaluation would run over a different world"
                    .to_owned(),
            ),
        }
    }

    /// Attach the exact routing input, making the record replayable.
    pub fn with_routing_input(mut self, input: &DecisionInput) -> Self {
        self.routing_input = Some(RoutingInput::from_decision_input(input));
        self.replayable = self
            .routing_input
            .as_ref()
            .is_some_and(RoutingInput::is_complete);
        self.not_replayable_reason = if self.replayable {
            None
        } else {
            Some("the recorded routing input is incomplete".to_owned())
        };
        self
    }

    /// Attach the versions that shaped the decision.
    pub fn with_versions(mut self, versions: TraceVersions) -> Self {
        self.versions = Some(versions);
        self
    }

    /// Attach the attempts actually made.
    pub fn with_attempts(mut self, attempts: Vec<TraceAttempt>) -> Self {
        self.attempts = attempts;
        self
    }

    /// Attach separated latency phases.
    pub fn with_phases(mut self, phases: LatencyPhases) -> Self {
        self.phases = Some(phases);
        self
    }

    /// Sum usage across the attempts actually made.
    ///
    /// A tier that was never attempted contributes nothing; a provider
    /// that reported no usage contributes *unknown*, not zero.
    pub fn summed_usage(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for attempt in &self.attempts {
            totals.add(attempt.input_tokens, attempt.output_tokens);
            if attempt.cached_input_tokens.is_some() {
                totals.cached_known = true;
                totals.cached_input_tokens = Some(
                    totals.cached_input_tokens.unwrap_or(0)
                        + attempt.cached_input_tokens.unwrap_or(0),
                );
            }
            if attempt.reasoning_tokens.is_some() {
                totals.reasoning_known = true;
                totals.reasoning_tokens = Some(
                    totals.reasoning_tokens.unwrap_or(0) + attempt.reasoning_tokens.unwrap_or(0),
                );
            }
        }
        if self.attempts.is_empty() {
            // A trace without attempts falls back to its own totals, which
            // older records carry.
            totals.add(Some(self.input_tokens), Some(self.output_tokens));
        }
        totals
    }

    /// Whether the record carries enough evidence for re-evaluation.
    pub fn is_replayable(&self) -> bool {
        self.replayable
            && self
                .routing_input
                .as_ref()
                .is_some_and(RoutingInput::is_complete)
    }
}

/// Summed token usage across attempts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// Whether any attempt reported cache usage at all.
    pub cached_known: bool,
    /// Whether any attempt reported reasoning usage at all.
    pub reasoning_known: bool,
}

impl UsageTotals {
    fn add(&mut self, input: Option<u64>, output: Option<u64>) {
        match (self.input_tokens, input) {
            (Some(a), Some(b)) => self.input_tokens = Some(a + b),
            (None, Some(b)) => self.input_tokens = Some(b),
            (Some(a), None) => self.input_tokens = Some(a),
            (None, None) => {}
        }
        match (self.output_tokens, output) {
            (Some(a), Some(b)) => self.output_tokens = Some(a + b),
            (None, Some(b)) => self.output_tokens = Some(b),
            (Some(a), None) => self.output_tokens = Some(a),
            (None, None) => {}
        }
    }

    /// Whether any usage was reported at all.
    pub fn is_known(&self) -> bool {
        self.input_tokens.is_some() || self.output_tokens.is_some()
    }
}

/// The exact input a routing decision was made over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingInput {
    pub prompt: String,
    pub capabilities: Vec<String>,
    pub state: serde_json::Value,
}

impl RoutingInput {
    pub fn from_decision_input(input: &DecisionInput) -> Self {
        Self {
            prompt: input.prompt.clone(),
            capabilities: input.capabilities.clone(),
            state: input.state.clone(),
        }
    }

    pub fn to_decision_input(&self) -> DecisionInput {
        DecisionInput {
            prompt: self.prompt.clone(),
            capabilities: self.capabilities.clone(),
            state: self.state.clone(),
        }
    }

    /// Whether this input is complete enough to reproduce a decision.
    ///
    /// An empty capability set is legitimate; a *missing* one is not, and
    /// is what made the old replay re-run over a different world.
    pub fn is_complete(&self) -> bool {
        !self.prompt.is_empty()
    }
}

/// Versions of the moving parts behind a decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceVersions {
    /// Question-pack/frame version used by the control layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_pack: Option<String>,
    /// Tool registry revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_registry: Option<String>,
    /// Policy revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

/// One model attempt, with its own usage and cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceAttempt {
    /// Tier the attempt ran on.
    pub tier: crate::ModelTier,
    /// Requested model id.
    pub requested_model: String,
    /// Model that actually answered, when the provider resolved one.
    pub resolved_model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Provider-reported cache hits, when supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Provider-reported reasoning tokens, when supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    pub latency_ms: u64,
    /// Whether the attempt failed (a failure still consumes budget).
    pub failed: bool,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// Latency split by phase.
///
/// Provider inference time is never inferred from total HTTP time: only
/// observed phases are recorded, and unknown ones stay `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyPhases {
    /// Time spent scheduling locally (routing, gating, planning).
    pub local_scheduling_ms: Option<u64>,
    /// Full HTTP round trip, including connection and streaming.
    pub http_round_trip_ms: Option<u64>,
    /// Time to the first visible output (first streamed token/event).
    pub time_to_first_output_ms: Option<u64>,
    /// Time spent in tool execution.
    pub tool_ms: Option<u64>,
    /// End-to-end time from command to verified result.
    pub end_to_end_verified_ms: Option<u64>,
}

/// Ordered trace log with JSON import/export.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraceLog {
    traces: Vec<TurnTrace>,
}

impl TraceLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, trace: TurnTrace) {
        self.traces.push(trace);
    }

    pub fn iter(&self) -> impl Iterator<Item = &TurnTrace> {
        self.traces.iter()
    }

    pub fn len(&self) -> usize {
        self.traces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    pub fn to_json(&self) -> Result<String, KnutError> {
        serde_json::to_string(&self.traces).map_err(|e| KnutError::SystemOne(e.to_string()))
    }

    pub fn from_json(json: &str) -> Result<Self, KnutError> {
        let traces = serde_json::from_str(json).map_err(|e| KnutError::SystemOne(e.to_string()))?;
        Ok(Self { traces })
    }

    /// Re-evaluate routing over the *recorded* inputs.
    ///
    /// Uses the exact capabilities and state the decision was made over.
    /// A trace that lacks them is refused rather than silently re-run
    /// against empty context, which would compare two different worlds.
    pub async fn replay<S: SystemOne>(&self, router: &S) -> Result<Vec<Decision>, KnutError> {
        let mut decisions = Vec::with_capacity(self.traces.len());
        for trace in &self.traces {
            let Some(input) = &trace.routing_input else {
                return Err(KnutError::SystemOne(format!(
                    "trace {:?} has no recorded routing input; re-evaluation would run over a \
                     different world. Re-record it with TurnTrace::with_routing_input",
                    trace.prompt
                )));
            };
            if !input.is_complete() {
                return Err(KnutError::SystemOne(format!(
                    "trace {:?} recorded an incomplete routing input",
                    trace.prompt
                )));
            }
            decisions.push(router.decide(&input.to_decision_input()).await?);
        }
        Ok(decisions)
    }

    /// Whether every trace carries enough evidence for re-evaluation.
    pub fn is_replayable(&self) -> bool {
        self.traces.iter().all(TurnTrace::is_replayable)
    }

    /// Traces that cannot be re-evaluated, with the reason.
    pub fn non_replayable(&self) -> Vec<(&str, &str)> {
        self.traces
            .iter()
            .filter(|trace| !trace.is_replayable())
            .map(|trace| {
                (
                    trace.prompt.as_str(),
                    trace
                        .not_replayable_reason
                        .as_deref()
                        .unwrap_or("incomplete record"),
                )
            })
            .collect()
    }

    /// Aggregate metrics over the log.
    pub fn metrics(&self, cost: &CostModel) -> Metrics {
        Metrics::compute(self.traces.clone(), cost)
    }
}

/// Per-token cost by tier, with separate input/output rates.
///
/// A price is a *claim about a moment*: where it came from and when are
/// recorded, so a stale price cannot masquerade as current.
#[derive(Debug, Clone, PartialEq)]
pub struct CostModel {
    /// Input price per 1k tokens by tier.
    pub fast_input: f64,
    pub standard_input: f64,
    pub reasoner_input: f64,
    /// Output price per 1k tokens by tier.
    pub fast_output: f64,
    pub standard_output: f64,
    pub reasoner_output: f64,
    /// Where these prices came from.
    pub source: String,
    /// When they were captured.
    pub as_of: String,
    /// Configuration version that produced them.
    pub config_version: String,
}

impl CostModel {
    /// A model with the same rates for input and output.
    pub fn flat(fast: f64, standard: f64, reasoner: f64) -> Self {
        Self {
            fast_input: fast,
            fast_output: fast,
            standard_input: standard,
            standard_output: standard,
            reasoner_input: reasoner,
            reasoner_output: reasoner,
            source: "unspecified".to_owned(),
            as_of: "unspecified".to_owned(),
            config_version: "unspecified".to_owned(),
        }
    }

    /// Rates for one tier.
    pub fn rates(&self, tier: crate::ModelTier) -> (f64, f64) {
        match tier {
            crate::ModelTier::Fast => (self.fast_input, self.fast_output),
            crate::ModelTier::Standard => (self.standard_input, self.standard_output),
            crate::ModelTier::Reasoner => (self.reasoner_input, self.reasoner_output),
        }
    }

    /// Cost of one attempt, or `None` when its usage is unknown.
    ///
    /// Unknown usage never becomes a $0 line item.
    pub fn cost_of(&self, attempt: &TraceAttempt) -> Option<f64> {
        let input = attempt.input_tokens?;
        let output = attempt.output_tokens?;
        let (input_rate, output_rate) = self.rates(attempt.tier);
        Some((input as f64 / 1000.0) * input_rate + (output as f64 / 1000.0) * output_rate)
    }
}

impl Default for CostModel {
    fn default() -> Self {
        Self::flat(0.0, 0.0, 0.0)
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn tier_rank(tier: ModelTier) -> u8 {
    match tier {
        ModelTier::Fast => 0,
        ModelTier::Standard => 1,
        ModelTier::Reasoner => 2,
    }
}

/// Offline metrics over recorded turns.
///
/// Naming is deliberately careful. A shadow router's answer is
/// **observable disagreement**, not proof of a mistake: nobody ran the
/// cheaper path, so its outcome is unknown. Counts that *would* be
/// counterfactuals are named as disagreements, and a matched executed
/// comparison (issue #35) is what turns them into evidence.
///
/// Usage and cost are summed from the attempts actually made — including
/// failed and repaired ones — never from the initially selected tier.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    pub turns: usize,
    pub task_success_rate: f64,
    pub clarification_rate: f64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Calls whose provider reported no usage at all: their cost is
    /// *unknown*, not zero.
    pub calls_with_unknown_usage: usize,
    /// Cost of attempts whose usage was reported, using the documented
    /// price source. Unknown-usage attempts contribute nothing here and
    /// are counted above.
    pub estimated_cost: f64,
    /// Turns that executed on the reasoner tier.
    pub reasoner_turns: usize,
    /// Turns where a shadow router would have chosen a cheaper tier and
    /// the turn succeeded. This is *disagreement*, not a measured saving:
    /// the cheaper path was never executed.
    pub shadow_cheaper_disagreements: usize,
    /// Turns where a shadow would have routed stronger and the turn
    /// failed. Also a disagreement: the stronger path was never executed.
    pub shadow_stronger_failures: usize,
    /// The former name, kept so old dashboards can be migrated visibly.
    #[serde(rename = "unnecessary_escalations_legacy")]
    pub unnecessary_escalations: usize,
    #[serde(rename = "under_routing_failures_legacy")]
    pub under_routing_failures: usize,
    /// Tool-selection accuracy over labelled Act turns.
    pub tool_selection_accuracy: Option<f64>,
}

impl Metrics {
    pub fn compute(traces: Vec<TurnTrace>, cost: &CostModel) -> Metrics {
        let turns = traces.len();

        let successes = traces
            .iter()
            .filter(|t| t.outcome == TurnOutcome::Success)
            .count();
        let clarifications = traces
            .iter()
            .filter(|t| t.action == Action::AskUser)
            .count();

        let mut latencies: Vec<u64> = traces.iter().map(|t| t.latency_ms).collect();
        latencies.sort_unstable();

        // Usage and cost come from the attempts that actually ran, so a
        // cascaded turn is not billed as its first selected tier.
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut calls_with_unknown_usage = 0usize;
        let mut estimated_cost = 0.0f64;
        for trace in &traces {
            let totals = trace.summed_usage();
            total_input += totals.input_tokens.unwrap_or(0);
            total_output += totals.output_tokens.unwrap_or(0);

            if trace.attempts.is_empty() {
                // A trace with no attempt detail cannot attribute usage:
                // the turn's own totals are unknown, not zero.
                calls_with_unknown_usage += 1;
                continue;
            }

            for attempt in &trace.attempts {
                match cost.cost_of(attempt) {
                    Some(attempt_cost) => estimated_cost += attempt_cost,
                    // A failed attempt was still paid for; it is as
                    // unknown as any other unreported call.
                    None => calls_with_unknown_usage += 1,
                }
            }
        }

        let reasoner_turns = traces
            .iter()
            .filter(|t| t.action == Action::Generate(ModelTier::Reasoner))
            .count();

        let mut unnecessary_escalations = 0usize;
        let mut under_routing_failures = 0usize;
        for trace in &traces {
            let Some(shadow) = trace.shadow.clone() else {
                continue;
            };

            let executed_reasoner = trace.action == Action::Generate(ModelTier::Reasoner);
            let shadow_cheaper = tier_rank(shadow.model_tier) < tier_rank(ModelTier::Reasoner);

            // Named as disagreement: the cheaper path was never executed,
            // so nothing here proves the escalation was unnecessary.
            if executed_reasoner && shadow_cheaper && trace.outcome == TurnOutcome::Success {
                unnecessary_escalations += 1;
            }

            let shadow_stronger_route =
                matches!(shadow.route, Route::Act | Route::Clarify | Route::Retrieve)
                    && matches!(trace.action, Action::Generate(_));
            let shadow_stronger_tier = !shadow_cheaper && !executed_reasoner;

            if trace.outcome == TurnOutcome::Failure
                && (shadow_stronger_route || shadow_stronger_tier)
            {
                under_routing_failures += 1;
            }
        }

        let labelled: Vec<&TurnTrace> = traces
            .iter()
            .filter(|t| matches!(t.action, Action::Tool { .. }))
            .filter(|t| t.expected.as_ref().is_some_and(|e| e.capability.is_some()))
            .collect();

        let tool_selection_accuracy = if labelled.is_empty() {
            None
        } else {
            let correct = labelled
                .iter()
                .filter(|t| match (&t.action, t.expected.as_ref()) {
                    (Action::Tool { capability }, Some(expected)) => {
                        expected.capability.as_deref() == Some(capability)
                    }
                    _ => false,
                })
                .count();
            Some(correct as f64 / labelled.len() as f64)
        };

        Metrics {
            turns,
            task_success_rate: if turns == 0 {
                0.0
            } else {
                successes as f64 / turns as f64
            },
            clarification_rate: if turns == 0 {
                0.0
            } else {
                clarifications as f64 / turns as f64
            },
            p50_latency_ms: percentile(&latencies, 50.0),
            p95_latency_ms: percentile(&latencies, 95.0),
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            calls_with_unknown_usage,
            estimated_cost,
            reasoner_turns,
            shadow_cheaper_disagreements: unnecessary_escalations,
            shadow_stronger_failures: under_routing_failures,
            unnecessary_escalations,
            under_routing_failures,
            tool_selection_accuracy,
        }
    }
}

/// A System One that records what it *would* do while returning a fixed
/// baseline decision, so execution is provably unaffected.
///
/// This is shadow mode: the recorded decisions land in traces for offline
/// threshold tuning; the baseline keeps the loop deterministic.
pub struct ShadowSystemOne<S: 'static> {
    inner: Arc<S>,
    baseline: Decision,
    /// Outcomes, including explicit unavailability.
    recorded_outcomes: Arc<Mutex<Vec<ShadowOutcome>>>,
    /// How many shadow calls were spawned, so a caller can tell whether
    /// any are still running.
    spawned: Arc<AtomicU64>,
    /// Bounded budget for one shadow call.
    shadow_timeout: std::time::Duration,
    /// Session cancellation, so a shadow does not outlive its task.
    cancel: Arc<AtomicBool>,
}

/// Default shadow budget: generous enough to be useful, small enough that
/// a hung provider is abandoned rather than accumulating.
pub const DEFAULT_SHADOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl<S: SystemOne + 'static> ShadowSystemOne<S> {
    pub fn new(inner: S, baseline: Decision) -> Self {
        Self {
            inner: Arc::new(inner),
            baseline,
            recorded_outcomes: Arc::new(Mutex::new(Vec::new())),
            spawned: Arc::new(AtomicU64::new(0)),
            shadow_timeout: DEFAULT_SHADOW_TIMEOUT,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Override the shadow's own budget.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.shadow_timeout = timeout;
        self
    }

    /// Share the session's cancellation token.
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Every recorded outcome, including unavailability.
    pub fn outcomes(&self) -> Vec<ShadowOutcome> {
        self.recorded_outcomes
            .lock()
            .expect("shadow record poisoned")
            .clone()
    }

    /// Decisions the shadow produced, skipping the ones that never
    /// answered.
    pub fn recorded(&self) -> Vec<Decision> {
        self.outcomes()
            .iter()
            .filter_map(ShadowOutcome::decided)
            .cloned()
            .collect()
    }

    /// How many shadow calls failed to produce an answer.
    pub fn unavailable_count(&self) -> usize {
        self.outcomes()
            .iter()
            .filter(|outcome| outcome.unavailable_reason().is_some())
            .count()
    }

    /// Wait (bounded) for in-flight shadow work to settle.
    ///
    /// Used by tests and by a clean shutdown: it never blocks the
    /// execution path, and it gives up rather than hanging.
    pub async fn settle(&self, within: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if !self.in_flight() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    }

    fn in_flight(&self) -> bool {
        self.spawned.load(Ordering::SeqCst) > self.finished()
    }

    fn finished(&self) -> u64 {
        self.recorded_outcomes
            .lock()
            .expect("shadow record poisoned")
            .len() as u64
    }
}

#[async_trait]
impl<S: SystemOne + 'static> SystemOne for ShadowSystemOne<S> {
    /// Answer immediately with the baseline; the shadow runs **outside the
    /// critical path**.
    ///
    /// A slow, failing or cancelled shadow cannot delay the executed
    /// decision, change it, or make the task unavailable — it only
    /// produces a recorded disagreement, or a recorded *absence* of one.
    async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError> {
        let inner = Arc::clone(&self.inner);
        let recorded = Arc::clone(&self.recorded_outcomes);
        let cancel = Arc::clone(&self.cancel);
        let _ = &self.spawned;
        let input = input.clone();
        let deadline = self.shadow_timeout;

        // Fire and forget: the caller is answered from here regardless.
        self.spawned.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            if cancel.load(Ordering::SeqCst) {
                recorded
                    .lock()
                    .expect("shadow record poisoned")
                    .push(ShadowOutcome::Unavailable {
                        reason: "the session was cancelled".to_owned(),
                    });
                return;
            }
            let decided = tokio::time::timeout(deadline, inner.decide(&input)).await;
            let outcome = match decided {
                Ok(Ok(decision)) => ShadowOutcome::Decided(decision),
                Ok(Err(err)) => ShadowOutcome::Unavailable {
                    reason: err.to_string(),
                },
                Err(_) => ShadowOutcome::Unavailable {
                    reason: format!("the shadow exceeded its {deadline:?} budget"),
                },
            };
            recorded
                .lock()
                .expect("shadow record poisoned")
                .push(outcome);
        });

        Ok(self.baseline.clone())
    }
}

/// What a shadow evaluation produced.
///
/// A missing outcome is *reported as missing* rather than treated as
/// agreement: a shadow that never answered says nothing about the
/// decision.
#[derive(Debug, Clone, PartialEq)]
pub enum ShadowOutcome {
    Decided(Decision),
    Unavailable { reason: String },
}

impl ShadowOutcome {
    pub fn decided(&self) -> Option<&Decision> {
        match self {
            ShadowOutcome::Decided(decision) => Some(decision),
            ShadowOutcome::Unavailable { .. } => None,
        }
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            ShadowOutcome::Decided(_) => None,
            ShadowOutcome::Unavailable { reason } => Some(reason),
        }
    }
}

/// One benchmark input with an optional gold label.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchmarkTask {
    pub input: DecisionInput,
    pub expected: Option<Expectation>,
}

/// Benchmark: hybrid routing against an always-reasoner baseline over the
/// same task set.
///
/// Both arms run the same inputs; the caller supplies a runner per arm
/// that wires its own router and produces a trace plus the observed
/// outcome. The comparison reports both metric sets and the reasoner
/// turns saved, so confidence thresholds can be tuned from recorded data.
#[derive(Debug, Clone, Default)]
pub struct Benchmark {
    tasks: Vec<BenchmarkTask>,
}

/// The two metric sets and the delta that matters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BenchmarkComparison {
    pub routed: Metrics,
    pub baseline: Metrics,
    /// Reasoner-turns saved by routing (baseline minus routed).
    pub reasoner_turns_saved: usize,
}

impl Benchmark {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_task(mut self, input: DecisionInput, expected: Option<Expectation>) -> Self {
        self.tasks.push(BenchmarkTask { input, expected });
        self
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    async fn run_arm<F, Fut>(&self, cost: &CostModel, mut runner: F) -> Metrics
    where
        F: FnMut(BenchmarkTask) -> Fut,
        Fut: std::future::Future<Output = (TurnTrace, TurnOutcome)>,
    {
        let mut traces = Vec::with_capacity(self.tasks.len());
        for task in &self.tasks {
            let (mut trace, outcome) = runner(task.clone()).await;
            trace.expected = task.expected.clone();
            trace.outcome = outcome;
            traces.push(trace);
        }
        Metrics::compute(traces, cost)
    }

    /// Run both arms over the same tasks and compare.
    pub async fn compare<FR, FutR, FB, FutB>(
        &self,
        cost: &CostModel,
        routed: FR,
        baseline: FB,
    ) -> BenchmarkComparison
    where
        FR: FnMut(BenchmarkTask) -> FutR,
        FutR: std::future::Future<Output = (TurnTrace, TurnOutcome)>,
        FB: FnMut(BenchmarkTask) -> FutB,
        FutB: std::future::Future<Output = (TurnTrace, TurnOutcome)>,
    {
        let routed_metrics = self.run_arm(cost, routed).await;
        let baseline_metrics = self.run_arm(cost, baseline).await;

        let reasoner_turns_saved = baseline_metrics
            .reasoner_turns
            .saturating_sub(routed_metrics.reasoner_turns);

        BenchmarkComparison {
            routed: routed_metrics,
            baseline: baseline_metrics,
            reasoner_turns_saved,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Risk, StaticSystemOne};

    fn decision(route: Route, confidence: f32, tier: ModelTier) -> Decision {
        Decision {
            route,
            confidence,
            retrieval: None,
            capability: None,
            model_tier: tier,
            risk: Risk::Low,
            parallelizable: false,
        }
    }

    fn trace(prompt: &str, action: Action, tier: ModelTier, outcome: TurnOutcome) -> TurnTrace {
        TurnTrace::new(
            prompt,
            DecisionSource::SystemOne,
            decision(Route::Generate, 0.9, tier),
            action,
            10,
            100,
            50,
            outcome,
        )
    }

    #[test]
    fn metrics_report_rates_latencies_and_cost() {
        let log = TraceLog {
            traces: vec![
                trace(
                    "a",
                    Action::Generate(ModelTier::Fast),
                    ModelTier::Fast,
                    TurnOutcome::Success,
                ),
                trace(
                    "b",
                    Action::Generate(ModelTier::Fast),
                    ModelTier::Fast,
                    TurnOutcome::Failure,
                ),
                trace(
                    "c",
                    Action::AskUser,
                    ModelTier::Fast,
                    TurnOutcome::Clarified,
                ),
            ],
        };

        let cost = CostModel::flat(0.1, 0.0, 0.0);
        let metrics = log.metrics(&cost);

        assert_eq!(metrics.turns, 3);
        assert!((metrics.task_success_rate - 1.0 / 3.0).abs() < 1e-9);
        assert!((metrics.clarification_rate - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(metrics.p50_latency_ms, 10);
        assert_eq!(metrics.p95_latency_ms, 10);
        assert_eq!(metrics.total_input_tokens, 300);
        assert_eq!(metrics.total_output_tokens, 150);
        // Cost comes from the attempts actually made. These traces carry
        // no attempts, so the cost is zero *and* the totals are reported
        // as unknown rather than being billed against the selected tier.
        assert_eq!(metrics.estimated_cost, 0.0);
        assert_eq!(metrics.calls_with_unknown_usage, 3);
        assert_eq!(metrics.tool_selection_accuracy, None);
    }

    #[test]
    fn unnecessary_escalation_needs_shadow_and_success() {
        let mut escalated = trace(
            "hard?",
            Action::Generate(ModelTier::Reasoner),
            ModelTier::Reasoner,
            TurnOutcome::Success,
        );
        escalated.shadow = Some(decision(Route::Generate, 0.9, ModelTier::Fast));

        let mut failed_escalation = escalated.clone();
        failed_escalation.outcome = TurnOutcome::Failure;

        // Escalated without a shadow verdict: not counted (no evidence).
        let no_shadow = trace(
            "plain",
            Action::Generate(ModelTier::Reasoner),
            ModelTier::Reasoner,
            TurnOutcome::Success,
        );

        let metrics = Metrics::compute(
            vec![escalated, failed_escalation, no_shadow],
            &CostModel::default(),
        );

        assert_eq!(metrics.unnecessary_escalations, 1);
    }

    #[test]
    fn under_routing_failure_is_detected_via_shadow_route() {
        let mut misrouted = trace(
            "run the deploy tool",
            Action::Generate(ModelTier::Fast),
            ModelTier::Fast,
            TurnOutcome::Failure,
        );
        misrouted.shadow = Some(Decision {
            route: Route::Act,
            capability: Some("deploy".to_owned()),
            ..decision(Route::Act, 0.9, ModelTier::Fast)
        });

        let metrics = Metrics::compute(vec![misrouted], &CostModel::default());

        assert_eq!(metrics.under_routing_failures, 1);
    }

    #[test]
    fn tool_selection_accuracy_uses_labels() {
        let mut correct = trace(
            "deploy",
            Action::Tool {
                capability: "deploy".to_owned(),
            },
            ModelTier::Fast,
            TurnOutcome::Success,
        );
        correct.expected = Some(Expectation {
            route: Some(Route::Act),
            capability: Some("deploy".to_owned()),
        });

        let mut wrong = trace(
            "deploy too",
            Action::Tool {
                capability: "files".to_owned(),
            },
            ModelTier::Fast,
            TurnOutcome::Success,
        );
        wrong.expected = Some(Expectation {
            route: Some(Route::Act),
            capability: Some("deploy".to_owned()),
        });

        let metrics = Metrics::compute(vec![correct, wrong], &CostModel::default());

        assert_eq!(metrics.tool_selection_accuracy, Some(0.5));
    }

    #[test]
    fn trace_log_round_trips_through_json() {
        let mut log = TraceLog::new();
        log.record(trace(
            "a",
            Action::AskUser,
            ModelTier::Fast,
            TurnOutcome::Clarified,
        ));

        let json = log.to_json().unwrap();
        let back = TraceLog::from_json(&json).unwrap();

        assert_eq!(back, log);
    }

    #[tokio::test]
    async fn replay_reroutes_recorded_prompts() {
        // A record that captured the exact routing input replays over the
        // same world.
        let mut log = TraceLog::new();
        for prompt in ["route me", "and me"] {
            let input = DecisionInput::new(prompt, vec!["files".to_owned()])
                .with_state(serde_json::json!({ "turn": 1 }));
            log.record(
                trace(
                    prompt,
                    Action::AskUser,
                    ModelTier::Fast,
                    TurnOutcome::Clarified,
                )
                .with_routing_input(&input),
            );
        }
        assert!(log.is_replayable());

        let router = StaticSystemOne::new(decision(Route::Retrieve, 0.9, ModelTier::Standard));
        let decisions = log.replay(&router).await.unwrap();

        assert_eq!(decisions.len(), 2);
        assert!(
            decisions
                .iter()
                .all(|d| d.model_tier == ModelTier::Standard)
        );
    }

    #[tokio::test]
    async fn shadow_records_without_affecting_execution() {
        let inner = StaticSystemOne::new(decision(Route::Act, 0.95, ModelTier::Fast));
        let baseline = decision(Route::Generate, 0.95, ModelTier::Reasoner);
        let shadow = ShadowSystemOne::new(inner, baseline.clone());

        let input = DecisionInput::new("hello", vec![]);
        let started = std::time::Instant::now();
        let executed = shadow.decide(&input).await.unwrap();
        let elapsed = started.elapsed();

        // Execution saw the baseline immediately: the shadow ran outside
        // the critical path, so the caller never waited for it.
        assert_eq!(executed, baseline);
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "the shadow delayed the executed decision by {elapsed:?}"
        );

        // The shadow's answer is recorded once it settles.
        assert!(shadow.settle(std::time::Duration::from_secs(2)).await);
        assert_eq!(shadow.recorded().len(), 1);
        assert_eq!(shadow.recorded()[0].route, Route::Act);
        assert_eq!(shadow.unavailable_count(), 0);
    }

    #[tokio::test]
    async fn re_evaluation_refuses_an_incomplete_record() {
        // A record without its routing input cannot be re-run: doing so
        // would compare two different worlds.
        let mut log = TraceLog::new();
        log.record(trace(
            "no input captured",
            Action::Generate(ModelTier::Fast),
            ModelTier::Fast,
            TurnOutcome::Success,
        ));

        assert!(!log.is_replayable());
        let missing = log.non_replayable();
        assert_eq!(missing.len(), 1);
        assert!(missing[0].1.contains("routing input"));

        let router = StaticSystemOne::new(decision(Route::Retrieve, 0.9, ModelTier::Standard));
        let err = log.replay(&router).await.unwrap_err();
        assert!(
            format!("{err}").contains("no recorded routing input"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn replay_uses_the_exact_recorded_capabilities_and_state() {
        /// Records the input it was asked about.
        struct Recorder {
            seen: std::sync::Mutex<Vec<DecisionInput>>,
        }

        #[async_trait]
        impl SystemOne for Recorder {
            async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError> {
                self.seen.lock().unwrap().push(input.clone());
                Ok(decision(Route::Generate, 0.9, ModelTier::Fast))
            }
        }

        let recorder = Recorder {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let input =
            DecisionInput::new("exact prompt", vec!["files".to_owned(), "shell".to_owned()])
                .with_state(serde_json::json!({ "revision": 3 }));
        let mut log = TraceLog::new();
        log.record(
            trace(
                "exact prompt",
                Action::Tool {
                    capability: "files".to_owned(),
                },
                ModelTier::Fast,
                TurnOutcome::Success,
            )
            .with_routing_input(&input),
        );

        log.replay(&recorder).await.unwrap();
        let seen = recorder.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        // Exactly the recorded capabilities and state, not empty ones.
        assert_eq!(
            seen[0].capabilities,
            vec!["files".to_owned(), "shell".to_owned()]
        );
        assert_eq!(seen[0].state, serde_json::json!({ "revision": 3 }));
    }

    #[tokio::test]
    async fn a_slow_or_failing_shadow_cannot_block_or_fail_the_baseline() {
        /// A shadow that never answers.
        struct Hanging;
        #[async_trait]
        impl SystemOne for Hanging {
            async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(decision(Route::Act, 0.9, ModelTier::Fast))
            }
        }
        /// A shadow that always errors.
        struct Failing;
        #[async_trait]
        impl SystemOne for Failing {
            async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
                Err(KnutError::SystemOne("shadow blew up".to_owned()))
            }
        }

        let baseline = decision(Route::Generate, 0.95, ModelTier::Reasoner);
        let input = DecisionInput::new("hello", vec![]);

        // A hanging shadow: the baseline is still returned promptly.
        let shadow = ShadowSystemOne::new(Hanging, baseline.clone())
            .with_timeout(std::time::Duration::from_millis(50));
        let started = std::time::Instant::now();
        let executed = shadow.decide(&input).await.unwrap();
        assert_eq!(executed, baseline);
        assert!(started.elapsed() < std::time::Duration::from_millis(200));

        // Its absence is reported, not treated as agreement.
        assert!(shadow.settle(std::time::Duration::from_secs(2)).await);
        assert_eq!(shadow.unavailable_count(), 1);
        assert!(shadow.recorded().is_empty());

        // A failing shadow likewise.
        let shadow = ShadowSystemOne::new(Failing, baseline.clone());
        assert_eq!(shadow.decide(&input).await.unwrap(), baseline);
        assert!(shadow.settle(std::time::Duration::from_secs(2)).await);
        assert_eq!(shadow.unavailable_count(), 1);
    }

    #[test]
    fn a_multi_attempt_fixture_sums_usage_and_preserves_unknowns() {
        let attempt = |tier: ModelTier,
                       input: Option<u64>,
                       output: Option<u64>,
                       cached: Option<u64>,
                       reasoning: Option<u64>| TraceAttempt {
            tier,
            requested_model: "requested".to_owned(),
            resolved_model: Some("resolved".to_owned()),
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
            reasoning_tokens: reasoning,
            latency_ms: 5,
            failed: false,
            failure: None,
        };

        let mut trace = trace(
            "cascade",
            Action::Generate(ModelTier::Reasoner),
            ModelTier::Reasoner,
            TurnOutcome::Success,
        );
        trace.attempts = vec![
            // Fast attempt that failed and was escalated past.
            TraceAttempt {
                failed: true,
                failure: Some("verifier rejected".to_owned()),
                ..attempt(ModelTier::Fast, Some(100), Some(20), Some(50), Some(0))
            },
            // Reasoner attempt that succeeded, with reasoning tokens.
            attempt(ModelTier::Reasoner, Some(500), Some(120), None, Some(80)),
        ];

        let totals = trace.summed_usage();
        // The failed attempt is included: it was really paid for.
        assert_eq!(totals.input_tokens, Some(600));
        assert_eq!(totals.output_tokens, Some(140));
        // Cache and reasoning usage are reported only where supported.
        assert_eq!(totals.cached_input_tokens, Some(50));
        assert!(totals.cached_known);
        assert_eq!(totals.reasoning_tokens, Some(80));

        // Cost uses separate input/output rates per tier.
        let cost = CostModel {
            fast_input: 1.0,
            fast_output: 2.0,
            standard_input: 0.0,
            standard_output: 0.0,
            reasoner_input: 3.0,
            reasoner_output: 4.0,
            source: "fixture".to_owned(),
            as_of: "2026-09-21".to_owned(),
            config_version: "test".to_owned(),
        };
        // Fast: 100/1000*1 + 20/1000*2 = 0.14
        // Reasoner: 500/1000*3 + 120/1000*4 = 1.98
        let fast_cost = cost.cost_of(&trace.attempts[0]).unwrap();
        let reasoner_cost = cost.cost_of(&trace.attempts[1]).unwrap();
        assert!((fast_cost - 0.14).abs() < 1e-9);
        assert!((reasoner_cost - 1.98).abs() < 1e-9);

        // Unknown usage produces no cost line, never a zero one.
        let unknown = attempt(ModelTier::Reasoner, None, None, None, None);
        assert!(cost.cost_of(&unknown).is_none());
    }

    #[test]
    fn missing_usage_is_counted_as_unknown_not_zero() {
        let mut log = TraceLog::new();
        let mut complete = trace(
            "measured",
            Action::Generate(ModelTier::Fast),
            ModelTier::Fast,
            TurnOutcome::Success,
        );
        complete.attempts = vec![TraceAttempt {
            tier: ModelTier::Fast,
            requested_model: "m".to_owned(),
            resolved_model: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            cached_input_tokens: None,
            reasoning_tokens: None,
            latency_ms: 3,
            failed: false,
            failure: None,
        }];
        log.record(complete);

        let mut unreported = trace(
            "unreported",
            Action::Generate(ModelTier::Fast),
            ModelTier::Fast,
            TurnOutcome::Success,
        );
        unreported.attempts = vec![TraceAttempt {
            tier: ModelTier::Fast,
            requested_model: "m".to_owned(),
            resolved_model: None,
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            reasoning_tokens: None,
            latency_ms: 3,
            failed: false,
            failure: None,
        }];
        log.record(unreported);

        let metrics = log.metrics(&CostModel::flat(1.0, 1.0, 1.0));
        // Only the measured attempt contributes tokens and cost.
        assert_eq!(metrics.total_input_tokens, 100);
        assert!(metrics.estimated_cost > 0.0);
        // The unreported call is visibly unknown.
        assert_eq!(metrics.calls_with_unknown_usage, 1);
    }

    #[tokio::test]
    async fn benchmark_compares_hybrid_against_always_reasoner() {
        let benchmark = Benchmark::new()
            .with_task(DecisionInput::new("task 1", vec![]), None)
            .with_task(DecisionInput::new("task 2", vec![]), None)
            .with_task(DecisionInput::new("task 3", vec![]), None);

        let cost = CostModel::flat(0.1, 0.0, 2.0);

        // Both arms record the attempt they actually made, so the cost
        // comparison is over real attempts rather than selected tiers.
        let attempt = |tier: ModelTier| TraceAttempt {
            tier,
            requested_model: format!("{tier:?}"),
            resolved_model: None,
            input_tokens: Some(1000),
            output_tokens: Some(500),
            cached_input_tokens: None,
            reasoning_tokens: None,
            latency_ms: 5,
            failed: false,
            failure: None,
        };

        let routed = |task: BenchmarkTask| {
            let prompt = task.input.prompt.clone();
            async move {
                let mut t = trace(
                    &prompt,
                    Action::Generate(ModelTier::Fast),
                    ModelTier::Fast,
                    TurnOutcome::Success,
                );
                t.decision = decision(Route::Generate, 0.95, ModelTier::Fast);
                t.attempts = vec![attempt(ModelTier::Fast)];
                (t, TurnOutcome::Success)
            }
        };

        let baseline = |task: BenchmarkTask| {
            let prompt = task.input.prompt.clone();
            async move {
                let mut t = trace(
                    &prompt,
                    Action::Generate(ModelTier::Reasoner),
                    ModelTier::Reasoner,
                    TurnOutcome::Success,
                );
                t.decision = decision(Route::Generate, 0.95, ModelTier::Reasoner);
                t.attempts = vec![attempt(ModelTier::Reasoner)];
                (t, TurnOutcome::Success)
            }
        };

        let comparison = benchmark.compare(&cost, routed, baseline).await;

        assert_eq!(comparison.routed.turns, 3);
        assert_eq!(comparison.baseline.turns, 3);
        assert_eq!(comparison.routed.reasoner_turns, 0);
        assert_eq!(comparison.baseline.reasoner_turns, 3);
        assert_eq!(comparison.reasoner_turns_saved, 3);
        // All succeeded in both arms: identical success rates, lower cost
        // from the attempts actually made.
        assert!((comparison.routed.task_success_rate - 1.0).abs() < 1e-9);
        assert!(
            comparison.routed.estimated_cost < comparison.baseline.estimated_cost,
            "routed {} vs baseline {}",
            comparison.routed.estimated_cost,
            comparison.baseline.estimated_cost
        );
        // Both arms reported usage, so nothing is unknown.
        assert_eq!(comparison.routed.calls_with_unknown_usage, 0);
        assert_eq!(comparison.baseline.calls_with_unknown_usage, 0);
    }

    #[test]
    fn empty_log_gives_zeroed_metrics() {
        let metrics = Metrics::compute(vec![], &CostModel::default());

        assert_eq!(metrics.turns, 0);
        assert_eq!(metrics.task_success_rate, 0.0);
        assert_eq!(metrics.p50_latency_ms, 0);
        assert_eq!(metrics.tool_selection_accuracy, None);
    }
}
