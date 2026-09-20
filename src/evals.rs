use std::sync::Mutex;

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
    /// What the shadow router would have done, when one was attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Decision>,
    /// Post-node edge decisions, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<EdgeJudgment>,
    /// Why each escalation happened, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escalation_reasons: Vec<String>,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub outcome: TurnOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Expectation>,
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

    /// Replay: rerun routing over the recorded inputs.
    ///
    /// Returns the decisions the supplied router produces for the same
    /// prompts, in order, for comparison against what actually happened.
    pub async fn replay<S: SystemOne>(&self, router: &S) -> Result<Vec<Decision>, KnutError> {
        let mut decisions = Vec::with_capacity(self.traces.len());
        for trace in &self.traces {
            let input = DecisionInput::new(trace.prompt.clone(), vec![]);
            decisions.push(router.decide(&input).await?);
        }
        Ok(decisions)
    }

    /// Aggregate metrics over the log.
    pub fn metrics(&self, cost: &CostModel) -> Metrics {
        Metrics::compute(self.traces.clone(), cost)
    }
}

/// Per-token cost by tier (currency units per 1k tokens).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostModel {
    pub fast: f64,
    pub standard: f64,
    pub reasoner: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            fast: 0.0,
            standard: 0.0,
            reasoner: 0.0,
        }
    }
}

impl CostModel {
    fn tier_cost(&self, tier: ModelTier) -> f64 {
        match tier {
            ModelTier::Fast => self.fast,
            ModelTier::Standard => self.standard,
            ModelTier::Reasoner => self.reasoner,
        }
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
/// Definitions:
/// - *unnecessary escalation*: the turn executed on the reasoner tier
///   while the shadow router would have kept a cheaper tier — and the
///   turn succeeded anyway.
/// - *under-routing failure*: the turn failed while the shadow would have
///   routed stronger (into a tool/clarify/retrieve instead of generate,
///   or onto the reasoner tier).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    pub turns: usize,
    pub task_success_rate: f64,
    pub clarification_rate: f64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub estimated_cost: f64,
    /// Turns that executed on the reasoner tier.
    pub reasoner_turns: usize,
    pub unnecessary_escalations: usize,
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

        let total_input: u64 = traces.iter().map(|t| t.input_tokens).sum();
        let total_output: u64 = traces.iter().map(|t| t.output_tokens).sum();

        let estimated_cost: f64 = traces
            .iter()
            .map(|t| {
                let tokens = (t.input_tokens + t.output_tokens) as f64;
                tokens / 1000.0 * cost.tier_cost(t.decision.model_tier)
            })
            .sum();

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
            estimated_cost,
            reasoner_turns,
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
pub struct ShadowSystemOne<S> {
    inner: S,
    baseline: Decision,
    recorded: Mutex<Vec<Decision>>,
}

impl<S: SystemOne> ShadowSystemOne<S> {
    pub fn new(inner: S, baseline: Decision) -> Self {
        Self {
            inner,
            baseline,
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// What the shadow router chose so far.
    pub fn recorded(&self) -> Vec<Decision> {
        self.recorded
            .lock()
            .expect("shadow record poisoned")
            .clone()
    }
}

#[async_trait]
impl<S: SystemOne> SystemOne for ShadowSystemOne<S> {
    async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError> {
        let would = self.inner.decide(input).await?;
        self.recorded
            .lock()
            .expect("shadow record poisoned")
            .push(would);
        Ok(self.baseline.clone())
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
        TurnTrace {
            prompt: prompt.to_owned(),
            source: DecisionSource::SystemOne,
            judgments: None,
            decision: decision(Route::Generate, 0.9, tier),
            action,
            shadow: None,
            edges: vec![],
            escalation_reasons: vec![],
            latency_ms: 10,
            input_tokens: 100,
            output_tokens: 50,
            outcome,
            expected: None,
        }
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

        let cost = CostModel {
            fast: 0.1,
            standard: 0.0,
            reasoner: 0.0,
        };
        let metrics = log.metrics(&cost);

        assert_eq!(metrics.turns, 3);
        assert!((metrics.task_success_rate - 1.0 / 3.0).abs() < 1e-9);
        assert!((metrics.clarification_rate - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(metrics.p50_latency_ms, 10);
        assert_eq!(metrics.p95_latency_ms, 10);
        assert_eq!(metrics.total_input_tokens, 300);
        assert_eq!(metrics.total_output_tokens, 150);
        // 450 tokens at 0.1 per 1k.
        assert!((metrics.estimated_cost - 0.045).abs() < 1e-9);
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
        let mut log = TraceLog::new();
        log.record(trace(
            "route me",
            Action::AskUser,
            ModelTier::Fast,
            TurnOutcome::Clarified,
        ));
        log.record(trace(
            "and me",
            Action::Generate(ModelTier::Fast),
            ModelTier::Fast,
            TurnOutcome::Success,
        ));

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
        let executed = shadow.decide(&input).await.unwrap();

        // Execution saw the baseline, never the shadow's judgment.
        assert_eq!(executed, baseline);
        assert_eq!(shadow.recorded().len(), 1);
        assert_eq!(shadow.recorded()[0].route, Route::Act);
    }

    #[tokio::test]
    async fn benchmark_compares_hybrid_against_always_reasoner() {
        let benchmark = Benchmark::new()
            .with_task(DecisionInput::new("task 1", vec![]), None)
            .with_task(DecisionInput::new("task 2", vec![]), None)
            .with_task(DecisionInput::new("task 3", vec![]), None);

        let cost = CostModel {
            fast: 0.1,
            standard: 0.0,
            reasoner: 2.0,
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
                (t, TurnOutcome::Success)
            }
        };

        let comparison = benchmark.compare(&cost, routed, baseline).await;

        assert_eq!(comparison.routed.turns, 3);
        assert_eq!(comparison.baseline.turns, 3);
        assert_eq!(comparison.routed.reasoner_turns, 0);
        assert_eq!(comparison.baseline.reasoner_turns, 3);
        assert_eq!(comparison.reasoner_turns_saved, 3);
        // All succeeded in both arms: identical success rates, lower cost.
        assert!((comparison.routed.task_success_rate - 1.0).abs() < 1e-9);
        assert!(comparison.routed.estimated_cost < comparison.baseline.estimated_cost);
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
