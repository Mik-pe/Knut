//! A task graph and an honest decision inspector (issue #30).
//!
//! The inspector is *optional*: it exists to answer "why did it do that?"
//! and "what is still outstanding?", not to narrate every match arm. The
//! ordinary transcript stays about useful work.
//!
//! Honesty rules carried through:
//! - a genuine model choice, a deterministic schedule, a cache hit and an
//!   operator decision are four different provenances, and they are never
//!   conflated;
//! - the *proposed* route and the *effective* route are both kept, so a
//!   fallback never inherits the rejected choice's confidence;
//! - router confidence is a routing score, never a percentage chance that
//!   code is correct;
//! - unavailable usage and cost are labelled as such, never shown as
//!   billed totals;
//! - latency is split into the phases it actually has;
//! - context inspection shows which files and revisions were selected,
//!   and hides sensitive payloads by default.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::FrameKind;
use crate::calibration::{DecisionType, Scores};
use crate::session::{SessionEvent, TaskState, WaitKind};
use crate::tree::NodeStatus;

/// Where a decision came from.
///
/// Four different things, deliberately not one enum with a fuzzy "auto".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum DecisionProvenance {
    /// A deterministic rule or schedule decided it.
    Deterministic { rule: String },
    /// The control layer answered a question.
    Model {
        question_kind: DecisionType,
        question_pack: String,
        /// Raw and effective scores, kept separate.
        scores: Scores,
        /// The offered candidates, so the choice is inspectable.
        candidates: Vec<String>,
    },
    /// A cached answer was reused.
    Cached {
        /// The calibration/policy version the cached answer came from.
        policy_version: String,
        question_kind: DecisionType,
    },
    /// An operator (approval, answer, cancel) decided it.
    Operator { action: String },
    /// The routing call failed or abstained; the runtime fell back.
    Fallback { reason: String },
}

impl DecisionProvenance {
    /// A short label for the inspector.
    pub fn label(&self) -> &'static str {
        match self {
            DecisionProvenance::Deterministic { .. } => "deterministic",
            DecisionProvenance::Model { .. } => "model choice",
            DecisionProvenance::Cached { .. } => "cached",
            DecisionProvenance::Operator { .. } => "operator",
            DecisionProvenance::Fallback { .. } => "fallback",
        }
    }

    /// Whether this was a real decision the model made.
    pub fn is_model_choice(&self) -> bool {
        matches!(self, DecisionProvenance::Model { .. })
    }
}

/// One recorded decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub turn: u64,
    pub provenance: DecisionProvenance,
    /// What was proposed, when it differs from what happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed: Option<String>,
    /// What actually happened.
    pub effective: String,
    /// Whether the runtime overrode the proposal, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_reason: Option<String>,
    /// Resolved model identity, when a model was involved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    /// Round-trip duration of the decision request, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_trip_ms: Option<u64>,
    /// Tokens, when the provider reported them. `None` is *unknown*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Estimated cost, labelled as an estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost: Option<f64>,
}

impl DecisionRecord {
    /// Whether this record's usage is unknown.
    pub fn usage_unknown(&self) -> bool {
        self.tokens.is_none()
    }

    /// How the inspector renders this row.
    pub fn row(&self) -> String {
        let mut out = format!(
            "turn {} [{}] {}",
            self.turn,
            self.provenance.label(),
            self.effective
        );
        if let Some(proposed) = &self.proposed
            && proposed != &self.effective
        {
            out.push_str(&format!(" (proposed {proposed}, overridden)"));
        }
        if let Some(model) = &self.resolved_model {
            out.push_str(&format!(" via {model}"));
        }
        if self.usage_unknown() {
            // Never a zero: the row says the usage is unknown.
            out.push_str(" — usage unknown");
        }
        out
    }

    /// A structured, short statement of why stronger reasoning was asked
    /// for.
    ///
    /// Deliberately not a narrative: the runtime explains its own decision,
    /// and never fabricates a model's hidden reasoning.
    pub fn escalation_explanation(&self) -> Option<String> {
        let reason = self.override_reason.as_ref()?;
        Some(match self.provenance {
            DecisionProvenance::Fallback { .. } => {
                format!("stronger reasoning requested: {reason}")
            }
            DecisionProvenance::Model { .. } => {
                format!("escalated from a weak control answer: {reason}")
            }
            _ => format!("escalated: {reason}"),
        })
    }
}

/// A node in the task graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: String,
    pub label: String,
    pub state: NodeStatus,
    /// Ids this node waits for.
    pub depends_on: Vec<String>,
    /// The model or tool that ran it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// Bounded output summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The turn that produced it, so a failure links to *its* output.
    pub turn: u64,
    /// Diagnostics for a failed node, taken from its own output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

impl TaskNode {
    /// Whether this node is still outstanding.
    pub fn is_active(&self) -> bool {
        matches!(self.state, NodeStatus::Pending | NodeStatus::Running)
    }

    pub fn row(&self) -> String {
        let marker = match self.state {
            NodeStatus::Pending => "·",
            NodeStatus::Running => ">",
            NodeStatus::Succeeded => "+",
            NodeStatus::Failed => "!",
            NodeStatus::Blocked => "?",
        };
        format!("{marker} {} [{}]", self.label, self.state_label())
    }

    fn state_label(&self) -> &'static str {
        match self.state {
            NodeStatus::Pending => "planned",
            NodeStatus::Running => "running",
            NodeStatus::Succeeded => "completed",
            NodeStatus::Failed => "failed",
            NodeStatus::Blocked => "waiting",
        }
    }
}

/// An acceptance requirement and whether it is met.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementStatus {
    pub check: String,
    pub description: String,
    pub satisfied: bool,
}

/// What is still outstanding for the task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutstandingWork {
    pub requirements: Vec<RequirementStatus>,
    pub waiting_on_user: Option<String>,
    pub active_nodes: Vec<String>,
}

impl OutstandingWork {
    /// Whether anything is outstanding.
    pub fn is_empty(&self) -> bool {
        self.requirements
            .iter()
            .all(|requirement| requirement.satisfied)
            && self.waiting_on_user.is_none()
            && self.active_nodes.is_empty()
    }

    /// A short rendering.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let unmet: Vec<&RequirementStatus> = self
            .requirements
            .iter()
            .filter(|requirement| !requirement.satisfied)
            .collect();
        if unmet.is_empty() {
            out.push_str("requirements: all met\n");
        } else {
            out.push_str("requirements outstanding:\n");
            for requirement in unmet {
                out.push_str(&format!(
                    "  {} — {}\n",
                    requirement.check, requirement.description
                ));
            }
        }
        if let Some(waiting) = &self.waiting_on_user {
            out.push_str(&format!("waiting on you: {waiting}\n"));
        }
        if !self.active_nodes.is_empty() {
            out.push_str(&format!("active: {}\n", self.active_nodes.join(", ")));
        }
        out
    }
}

/// Latency split by phase, so UI responsiveness is not confused with
/// provider latency.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyBreakdown {
    pub ui_ms: Option<u64>,
    pub decision_overhead_ms: Option<u64>,
    pub reasoning_ms: Option<u64>,
    pub tool_ms: Option<u64>,
    pub end_to_end_ms: Option<u64>,
}

impl LatencyBreakdown {
    /// A labelled rendering; unknown phases stay unknown.
    pub fn render(&self) -> String {
        let show = |label: &str, value: Option<u64>| match value {
            Some(ms) => format!("{label} {ms}ms"),
            None => format!("{label} unknown"),
        };
        [
            show("ui", self.ui_ms),
            show("decision", self.decision_overhead_ms),
            show("reasoning", self.reasoning_ms),
            show("tool", self.tool_ms),
            show("end-to-end", self.end_to_end_ms),
        ]
        .join("  ")
    }
}

/// What context was selected, and what was withheld.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextInspection {
    pub selected: Vec<SelectedContext>,
    pub truncated: Vec<String>,
    /// Sensitive payloads are hidden by default.
    pub hidden_payloads: usize,
}

/// One piece of selected context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedContext {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Content identity, so a stale excerpt is visible.
    pub revision: String,
    /// Whether the excerpt is known to be current.
    pub fresh: bool,
    /// The payload, hidden unless the operator reveals it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

impl ContextInspection {
    /// A rendering that says when a revision is stale.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for context in &self.selected {
            out.push_str(&format!(
                "{}:{}-{} [{}]{}\n",
                context.path,
                context.start_line,
                context.end_line,
                if context.fresh { "current" } else { "STALE" },
                match &context.preview {
                    Some(preview) => format!(" — {preview}"),
                    None => " — payload hidden".to_owned(),
                }
            ));
        }
        for truncated in &self.truncated {
            out.push_str(&format!("{truncated} (truncated)\n"));
        }
        if self.hidden_payloads > 0 {
            out.push_str(&format!("{} payload(s) hidden\n", self.hidden_payloads));
        }
        out
    }
}

/// Usage and cost as the inspector shows them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageView {
    /// Reported tokens, when any provider reported them.
    pub tokens: Option<u64>,
    /// Estimated cost, explicitly labelled.
    pub estimated_cost: Option<f64>,
    /// Calls whose usage the provider never reported.
    pub unknown_usage_calls: u64,
    pub request_count: u64,
}

impl UsageView {
    /// A rendering that never presents a guess as a bill.
    pub fn render(&self) -> String {
        let tokens = match self.tokens {
            Some(tokens) => format!("{tokens} tokens"),
            None => "tokens unknown".to_owned(),
        };
        let cost = match self.estimated_cost {
            Some(cost) => format!("~{cost:.4} estimated"),
            None => "cost unknown".to_owned(),
        };
        let unknown = if self.unknown_usage_calls > 0 {
            format!(
                " ({} call(s) with unreported usage)",
                self.unknown_usage_calls
            )
        } else {
            String::new()
        };
        format!(
            "{tokens}, {cost}, {} request(s){unknown}",
            self.request_count
        )
    }
}

/// The inspector's whole state.
///
/// Built from the session's event stream; it performs no I/O and holds no
/// engine state of its own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionInspector {
    decisions: Vec<DecisionRecord>,
    nodes: Vec<TaskNode>,
    outstanding: OutstandingWork,
    latency: LatencyBreakdown,
    context: ContextInspection,
    usage: UsageView,
    task_state: Option<TaskState>,
}

impl DecisionInspector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decisions(&self) -> &[DecisionRecord] {
        &self.decisions
    }

    pub fn nodes(&self) -> &[TaskNode] {
        &self.nodes
    }

    pub fn outstanding(&self) -> &OutstandingWork {
        &self.outstanding
    }

    pub fn latency(&self) -> &LatencyBreakdown {
        &self.latency
    }

    pub fn context(&self) -> &ContextInspection {
        &self.context
    }

    pub fn usage(&self) -> &UsageView {
        &self.usage
    }

    pub fn task_state(&self) -> Option<TaskState> {
        self.task_state
    }

    /// How many decisions are retained.
    pub const MAX_DECISIONS: usize = 500;

    /// Apply one session event.
    ///
    /// Only decisions and nodes that the runtime actually reported appear
    /// here: nothing is inferred or synthesized.
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Routed {
                turn,
                source,
                action,
                confidence,
                ..
            } => {
                let provenance = match source {
                    crate::DecisionSource::SystemZero => DecisionProvenance::Deterministic {
                        rule: "system 0".to_owned(),
                    },
                    crate::DecisionSource::Cache => DecisionProvenance::Cached {
                        // The cache key covers the policy version, so a
                        // hit is only possible for the version that
                        // produced it.
                        policy_version: "session".to_owned(),
                        question_kind: DecisionType::Ingress,
                    },
                    crate::DecisionSource::SystemOne => DecisionProvenance::Model {
                        question_kind: DecisionType::Ingress,
                        question_pack: "frame-v1".to_owned(),
                        scores: Scores::new(*confidence),
                        candidates: Vec::new(),
                    },
                };
                self.push_decision(DecisionRecord {
                    turn: turn.0,
                    provenance,
                    proposed: None,
                    effective: format!("{action:?}"),
                    override_reason: None,
                    resolved_model: None,
                    round_trip_ms: None,
                    tokens: None,
                    estimated_cost: None,
                });
                self.usage.request_count += 1;
            }
            SessionEvent::FrameDecided {
                turn,
                question_kind,
                frame_version,
                choice,
                confidence,
                distribution,
                overridden,
                ..
            } => {
                self.push_decision(DecisionRecord {
                    turn: turn.0,
                    provenance: DecisionProvenance::Model {
                        question_kind: frame_kind_to_decision_type(*question_kind),
                        question_pack: format!("frame-v{frame_version}"),
                        scores: Scores::new(*confidence as f32),
                        candidates: distribution
                            .as_object()
                            .map(|map| map.keys().cloned().collect())
                            .unwrap_or_default(),
                    },
                    proposed: None,
                    effective: choice.clone(),
                    override_reason: overridden.then(|| "the answer was not usable".to_owned()),
                    resolved_model: None,
                    round_trip_ms: None,
                    tokens: None,
                    estimated_cost: None,
                });
            }
            SessionEvent::EdgeDecided {
                turn,
                proposed,
                effective,
                overridden,
                ..
            } => {
                self.push_decision(DecisionRecord {
                    turn: turn.0,
                    provenance: DecisionProvenance::Deterministic {
                        rule: "edge selector".to_owned(),
                    },
                    // Proposed and effective are both kept: a fallback
                    // never inherits the rejected choice's confidence.
                    proposed: Some(format!("{proposed:?}")),
                    effective: format!("{effective:?}"),
                    override_reason: overridden.then(|| "policy override".to_owned()),
                    resolved_model: None,
                    round_trip_ms: None,
                    tokens: None,
                    estimated_cost: None,
                });
            }
            SessionEvent::PlanStarted {
                turn, node_count, ..
            } => {
                for index in 0..*node_count {
                    self.nodes.push(TaskNode {
                        id: format!("plan:{index}"),
                        label: format!("planned node {index}"),
                        state: NodeStatus::Pending,
                        depends_on: Vec::new(),
                        executor: None,
                        output: None,
                        turn: turn.0,
                        diagnostics: Vec::new(),
                    });
                }
            }
            SessionEvent::NodeResult {
                turn,
                node,
                node_label,
                status,
                output,
                ..
            } => {
                let id = format!("node:{}", node.0);
                // A failure's diagnostics come from *its own* output, so
                // the link never points at another turn's text.
                let diagnostics = if *status == NodeStatus::Failed {
                    output
                        .to_string()
                        .lines()
                        .filter(|line| {
                            let lowered = line.to_lowercase();
                            lowered.contains("error") || lowered.contains("failed")
                        })
                        .take(5)
                        .map(str::to_owned)
                        .collect()
                } else {
                    Vec::new()
                };
                let summary: String = output.to_string().chars().take(200).collect();

                // The plan created placeholders by index; the first real
                // result of that turn replaces one, so the graph does not
                // double-count planned and executed nodes.
                if !self.nodes.iter().any(|candidate| candidate.id == id)
                    && let Some(placeholder) = self.nodes.iter_mut().find(|candidate| {
                        candidate.turn == turn.0
                            && candidate.state == NodeStatus::Pending
                            && candidate.id.starts_with("plan:")
                    })
                {
                    placeholder.id = id.clone();
                }

                match self.nodes.iter_mut().find(|candidate| candidate.id == id) {
                    Some(existing) => {
                        existing.state = *status;
                        existing.label = node_label.clone();
                        existing.turn = turn.0;
                        existing.output = Some(summary);
                        existing.diagnostics = diagnostics;
                    }
                    None => self.nodes.push(TaskNode {
                        id,
                        label: node_label.clone(),
                        state: *status,
                        depends_on: Vec::new(),
                        executor: None,
                        output: Some(summary),
                        turn: turn.0,
                        diagnostics,
                    }),
                }
            }
            SessionEvent::WaitingForUser { wait, message, .. } => {
                self.task_state = Some(TaskState::Waiting);
                self.outstanding.waiting_on_user = Some(match wait {
                    WaitKind::Question => message.clone(),
                    WaitKind::Approval { .. } => {
                        format!("approval required: {message}")
                    }
                });
            }
            SessionEvent::WaitResolved { .. } => {
                self.outstanding.waiting_on_user = None;
            }
            SessionEvent::TaskCompleted { .. } => {
                self.task_state = Some(TaskState::Completed);
                for node in &mut self.nodes {
                    if node.state == NodeStatus::Pending {
                        node.state = NodeStatus::Succeeded;
                    }
                }
            }
            SessionEvent::TaskFailed { .. } => {
                self.task_state = Some(TaskState::Failed);
            }
            SessionEvent::TaskCancelled { .. } => {
                self.task_state = Some(TaskState::Cancelled);
            }
            _ => {}
        }

        // Which nodes are still active, for the outstanding view.
        self.outstanding.active_nodes = self
            .nodes
            .iter()
            .filter(|node| node.is_active())
            .map(|node| node.label.clone())
            .collect();
    }

    fn push_decision(&mut self, record: DecisionRecord) {
        self.decisions.push(record);
        // Bounded: a long session cannot grow the inspector without limit.
        if self.decisions.len() > Self::MAX_DECISIONS {
            self.decisions.remove(0);
        }
    }

    /// Record the resolved model and usage of the most recent decision.
    pub fn annotate_last_decision(
        &mut self,
        model: impl Into<String>,
        round_trip_ms: u64,
        tokens: Option<u64>,
        estimated_cost: Option<f64>,
    ) {
        if let Some(record) = self.decisions.last_mut() {
            record.resolved_model = Some(model.into());
            record.round_trip_ms = Some(round_trip_ms);
            record.tokens = tokens;
            record.estimated_cost = estimated_cost;
            match tokens {
                Some(tokens) => {
                    self.usage.tokens = Some(self.usage.tokens.unwrap_or(0) + tokens);
                }
                // Unreported usage is counted, never assumed zero.
                None => self.usage.unknown_usage_calls += 1,
            }
            if let Some(cost) = estimated_cost {
                self.usage.estimated_cost = Some(self.usage.estimated_cost.unwrap_or(0.0) + cost);
            }
        }
    }

    /// Record a latency phase.
    pub fn record_latency(&mut self, phase: LatencyPhase, ms: u64) {
        match phase {
            LatencyPhase::Ui => self.latency.ui_ms = Some(ms),
            LatencyPhase::Decision => self.latency.decision_overhead_ms = Some(ms),
            LatencyPhase::Reasoning => self.latency.reasoning_ms = Some(ms),
            LatencyPhase::Tool => self.latency.tool_ms = Some(ms),
            LatencyPhase::EndToEnd => self.latency.end_to_end_ms = Some(ms),
        }
    }

    /// Record selected context.
    pub fn record_context(&mut self, context: SelectedContext) {
        self.context.selected.push(context);
    }

    /// Note a truncated context item.
    pub fn record_truncation(&mut self, what: impl Into<String>) {
        self.context.truncated.push(what.into());
    }

    /// Hide payloads (the default) or reveal them explicitly.
    pub fn hide_payloads(&mut self) {
        let hidden = self
            .context
            .selected
            .iter()
            .filter(|context| context.preview.is_some())
            .count();
        for context in &mut self.context.selected {
            context.preview = None;
        }
        self.context.hidden_payloads = hidden;
    }

    /// Set requirement satisfaction.
    pub fn set_requirements(&mut self, requirements: Vec<RequirementStatus>) {
        self.outstanding.requirements = requirements;
    }

    /// The node a failure should link to: the *failed* node of that turn,
    /// never another turn's output.
    pub fn failed_node_for_turn(&self, turn: u64) -> Option<&TaskNode> {
        self.nodes
            .iter()
            .find(|node| node.turn == turn && node.state == NodeStatus::Failed)
    }

    /// Whether a router confidence could be mistaken for correctness.
    ///
    /// Always false by construction: the inspector renders confidence as a
    /// routing score and nothing else.
    pub fn renders_confidence_as_correctness(&self) -> bool {
        false
    }

    /// A summary of the graph.
    pub fn graph_summary(&self) -> String {
        if self.nodes.is_empty() {
            // No nodes yet is a fact about the session, not an incomplete
            // sentence: "0 node(s): " reads as a truncated label.
            return "no nodes yet".to_owned();
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for node in &self.nodes {
            *counts.entry(node.state_label()).or_default() += 1;
        }
        let mut out = format!("{} node(s): ", self.nodes.len());
        out.push_str(
            &counts
                .into_iter()
                .map(|(state, count)| format!("{count} {state}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out
    }

    /// A full rendering for the inspector pane.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("task: {:?}\n", self.task_state));
        out.push_str(&format!("{}\n", self.graph_summary()));
        out.push_str(&self.outstanding.render());
        out.push_str(&format!("latency: {}\n", self.latency.render()));
        out.push_str(&format!("usage: {}\n", self.usage.render()));
        out.push_str("decisions:\n");
        for record in self.decisions.iter().rev().take(10).rev() {
            out.push_str(&format!("  {}\n", record.row()));
        }
        out
    }
}

/// Map a frame kind onto the calibration's decision type.
fn frame_kind_to_decision_type(kind: FrameKind) -> DecisionType {
    match kind {
        FrameKind::Ingress => DecisionType::Ingress,
        FrameKind::Recovery => DecisionType::Recovery,
        FrameKind::Continuation => DecisionType::Continuation,
        FrameKind::CandidateSelection => DecisionType::CandidateSelection,
    }
}

/// Which latency phase is being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyPhase {
    Ui,
    Decision,
    Reasoning,
    Tool,
    EndToEnd,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{NodeId, TaskId, TaskRevision, TurnId};
    use crate::{Action, DecisionSource, EdgeChoice, FrameKind, ModelTier};

    fn routed(source: DecisionSource, turn: u64, confidence: f32) -> SessionEvent {
        SessionEvent::Routed {
            task: TaskId(1),
            turn: TurnId(turn),
            revision: TaskRevision(1),
            source,
            action: Action::Generate(ModelTier::Reasoner),
            confidence,
        }
    }

    #[test]
    fn deterministic_jev_cached_overridden_and_reasoner_transitions_are_distinguished() {
        // The acceptance test: a fake session with every provenance kind.
        let mut inspector = DecisionInspector::new();

        // A deterministic System 0 rule.
        inspector.apply(&routed(DecisionSource::SystemZero, 1, 1.0));
        // A Jev/model choice.
        inspector.apply(&SessionEvent::FrameDecided {
            task: TaskId(1),
            turn: TurnId(2),
            revision: TaskRevision(1),
            question_kind: FrameKind::Recovery,
            frame_version: 1,
            question_pack: vec!["failure_class".to_owned()],
            choice: "verification".to_owned(),
            confidence: 0.73,
            distribution: serde_json::json!({
                "verification": 0.73, "transient": 0.1, "unknown": 0.17
            }),
            overridden: false,
        });
        // A cached answer.
        inspector.apply(&routed(DecisionSource::Cache, 3, 0.8));
        // An overridden edge decision.
        inspector.apply(&SessionEvent::EdgeDecided {
            task: TaskId(1),
            turn: TurnId(4),
            revision: TaskRevision(1),
            proposed: EdgeChoice::Retry,
            effective: EdgeChoice::Continue,
            overridden: true,
            reason: String::new(),
        });

        let labels: Vec<&str> = inspector
            .decisions()
            .iter()
            .map(|record| record.provenance.label())
            .collect();
        assert_eq!(
            labels,
            vec!["deterministic", "model choice", "cached", "deterministic"]
        );
        // The model choice carries its question pack and candidates.
        let model_choice = &inspector.decisions()[1];
        match &model_choice.provenance {
            DecisionProvenance::Model {
                question_kind,
                question_pack,
                candidates,
                ..
            } => {
                assert_eq!(*question_kind, DecisionType::Recovery);
                assert_eq!(question_pack, "frame-v1");
                assert_eq!(candidates.len(), 3);
            }
            other => panic!("expected a model choice, got {other:?}"),
        }
        // The override keeps both the proposal and what happened.
        let overridden = &inspector.decisions()[3];
        assert_eq!(overridden.proposed.as_deref(), Some("Retry"));
        assert_eq!(overridden.effective, "Continue");
        assert!(overridden.override_reason.is_some());
    }

    #[test]
    fn a_router_confidence_is_never_rendered_as_correctness() {
        let mut inspector = DecisionInspector::new();
        inspector.apply(&routed(DecisionSource::SystemOne, 1, 0.42));
        inspector.annotate_last_decision("glm-5.3-flash", 250, Some(100), Some(0.001));

        let rendered = inspector.render();
        // The number appears as a routing score, not a percentage of
        // correctness.
        assert!(!rendered.to_lowercase().contains("correct"));
        assert!(!inspector.renders_confidence_as_correctness());
    }

    #[test]
    fn missing_usage_is_unknown_not_zero() {
        let mut inspector = DecisionInspector::new();
        inspector.apply(&routed(DecisionSource::SystemOne, 1, 0.9));
        // The provider reported nothing.
        inspector.annotate_last_decision("some-model", 300, None, None);

        let usage = inspector.usage();
        assert_eq!(usage.tokens, None);
        assert_eq!(usage.unknown_usage_calls, 1);
        assert!(usage.render().contains("tokens unknown"));
        assert!(usage.render().contains("unreported usage"));
        // And the decision row says so.
        assert!(inspector.decisions()[0].row().contains("usage unknown"));
        assert!(inspector.decisions()[0].usage_unknown());
    }

    #[test]
    fn cost_is_labelled_as_an_estimate() {
        let mut inspector = DecisionInspector::new();
        inspector.apply(&routed(DecisionSource::SystemOne, 1, 0.9));
        inspector.annotate_last_decision("m", 100, Some(500), Some(0.0042));
        // Never presented as a bill.
        assert!(inspector.usage().render().contains("estimated"));
        assert!(!inspector.usage().render().contains("billed"));
    }

    #[test]
    fn a_failed_node_links_to_its_own_diagnostics() {
        let mut inspector = DecisionInspector::new();

        // Turn 1 succeeds with output that happens to contain "error".
        inspector.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(1),
            node: NodeId(1),
            node_label: "read".to_owned(),
            status: NodeStatus::Succeeded,
            output: serde_json::json!("no error here"),
        });
        // Turn 2 fails with its own diagnostics.
        inspector.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(2),
            node: NodeId(2),
            node_label: "check".to_owned(),
            status: NodeStatus::Failed,
            output: serde_json::json!("error[E0308]: mismatched types; 1 test failed"),
        });

        let failed = inspector.failed_node_for_turn(2).unwrap();
        assert_eq!(failed.label, "check");
        assert!(failed.diagnostics.iter().any(|d| d.contains("E0308")));
        // Turn 1 has no failed node, so nothing is mis-attributed.
        assert!(inspector.failed_node_for_turn(1).is_none());
    }

    #[test]
    fn the_graph_covers_planned_running_waiting_failed_and_completed() {
        let mut inspector = DecisionInspector::new();
        inspector.apply(&SessionEvent::PlanStarted {
            task: TaskId(1),
            turn: TurnId(1),
            revision: TaskRevision(1),
            node_count: 4,
        });
        // One succeeds, one fails, one blocks.
        inspector.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(1),
            node: NodeId(1),
            node_label: "read".to_owned(),
            status: NodeStatus::Succeeded,
            output: serde_json::json!({}),
        });
        inspector.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(1),
            node: NodeId(2),
            node_label: "check".to_owned(),
            status: NodeStatus::Failed,
            output: serde_json::json!("failed"),
        });
        inspector.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(1),
            node: NodeId(3),
            node_label: "write".to_owned(),
            status: NodeStatus::Blocked,
            output: serde_json::json!(null),
        });

        let summary = inspector.graph_summary();
        assert!(summary.contains("completed"));
        assert!(summary.contains("failed"));
        assert!(summary.contains("waiting"));
        // The fourth planned node is still outstanding.
        assert_eq!(inspector.outstanding().active_nodes.len(), 1);
        assert!(!inspector.outstanding().is_empty());
    }

    #[test]
    fn requirements_are_visible_until_they_are_met() {
        let mut inspector = DecisionInspector::new();
        inspector.set_requirements(vec![
            RequirementStatus {
                check: "test".to_owned(),
                description: "the test suite passes".to_owned(),
                satisfied: false,
            },
            RequirementStatus {
                check: "build".to_owned(),
                description: "the crate compiles".to_owned(),
                satisfied: true,
            },
        ]);

        let rendered = inspector.outstanding().render();
        assert!(rendered.contains("test"));
        // The satisfied one is not listed as outstanding.
        assert!(!rendered.contains("the crate compiles"));
        assert!(!inspector.outstanding().is_empty());

        // Met: nothing outstanding.
        inspector.set_requirements(vec![RequirementStatus {
            check: "test".to_owned(),
            description: "the test suite passes".to_owned(),
            satisfied: true,
        }]);
        assert!(inspector.outstanding().is_empty());
    }

    #[test]
    fn waiting_on_the_user_is_surfaced() {
        let mut inspector = DecisionInspector::new();
        inspector.apply(&SessionEvent::WaitingForUser {
            task: TaskId(1),
            turn: TurnId(1),
            wait: WaitKind::Approval {
                approval_key: "fp".to_owned(),
            },
            message: "writes src/lib.rs".to_owned(),
        });
        assert!(inspector.outstanding().waiting_on_user.is_some());
        assert!(
            inspector
                .outstanding()
                .render()
                .contains("writes src/lib.rs")
        );

        inspector.apply(&SessionEvent::WaitResolved {
            task: TaskId(1),
            turn: TurnId(1),
        });
        assert!(inspector.outstanding().waiting_on_user.is_none());
    }

    #[test]
    fn latency_phases_are_separate_and_unknown_stays_unknown() {
        let mut inspector = DecisionInspector::new();
        inspector.record_latency(LatencyPhase::Ui, 8);
        inspector.record_latency(LatencyPhase::Decision, 250);
        inspector.record_latency(LatencyPhase::EndToEnd, 1_400);

        let rendered = inspector.latency().render();
        assert!(rendered.contains("ui 8ms"));
        assert!(rendered.contains("decision 250ms"));
        assert!(rendered.contains("end-to-end 1400ms"));
        // The unreported phases are labelled unknown, never zero.
        assert!(rendered.contains("reasoning unknown"));
        assert!(rendered.contains("tool unknown"));
        assert!(!rendered.contains("reasoning 0ms"));
    }

    #[test]
    fn context_inspection_hides_payloads_by_default() {
        let mut inspector = DecisionInspector::new();
        inspector.record_context(SelectedContext {
            path: "src/lib.rs".to_owned(),
            start_line: 1,
            end_line: 40,
            revision: "fnv1a:abc".to_owned(),
            fresh: true,
            preview: Some("secret-looking code".to_owned()),
        });
        inspector.record_context(SelectedContext {
            path: "src/old.rs".to_owned(),
            start_line: 1,
            end_line: 5,
            revision: "fnv1a:old".to_owned(),
            fresh: false,
            preview: Some("stale code".to_owned()),
        });
        inspector.record_truncation("src/big.rs");

        // Payloads are hidden unless the operator reveals them.
        inspector.hide_payloads();
        let rendered = inspector.context().render();
        assert!(!rendered.contains("secret-looking code"));
        assert!(rendered.contains("payload hidden"));
        assert!(rendered.contains("hidden"));
        // A stale revision is visible rather than silently trusted.
        assert!(rendered.contains("STALE"));
        assert!(rendered.contains("src/big.rs (truncated)"));
    }

    #[test]
    fn the_history_is_bounded() {
        let mut inspector = DecisionInspector::new();
        for turn in 0..(DecisionInspector::MAX_DECISIONS * 2) {
            inspector.apply(&routed(DecisionSource::SystemOne, turn as u64, 0.9));
        }
        assert_eq!(
            inspector.decisions().len(),
            DecisionInspector::MAX_DECISIONS
        );
        // The newest decisions are the ones retained.
        assert_eq!(
            inspector.decisions().last().unwrap().turn,
            (DecisionInspector::MAX_DECISIONS * 2 - 1) as u64
        );
    }

    #[test]
    fn escalation_explanations_are_structured_not_narrative() {
        let record = DecisionRecord {
            turn: 1,
            provenance: DecisionProvenance::Fallback {
                reason: "the control layer timed out".to_owned(),
            },
            proposed: Some("Fast".to_owned()),
            effective: "Reasoner".to_owned(),
            override_reason: Some("the control layer timed out".to_owned()),
            resolved_model: None,
            round_trip_ms: None,
            tokens: None,
            estimated_cost: None,
        };
        let explanation = record.escalation_explanation().unwrap();
        assert!(explanation.contains("stronger reasoning requested"));
        assert!(explanation.contains("timed out"));

        // A record without an override has no explanation at all: nothing
        // is fabricated.
        let plain = DecisionRecord {
            override_reason: None,
            ..record
        };
        assert!(plain.escalation_explanation().is_none());
    }

    #[test]
    fn the_inspector_renders_a_useful_summary() {
        let mut inspector = DecisionInspector::new();
        inspector.apply(&SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "fix the failing test".to_owned(),
        });
        inspector.apply(&routed(DecisionSource::SystemOne, 1, 0.9));
        inspector.annotate_last_decision("glm-5.3-flash", 200, Some(50), Some(0.0001));
        inspector.set_requirements(vec![RequirementStatus {
            check: "test".to_owned(),
            description: "the test suite passes".to_owned(),
            satisfied: false,
        }]);

        let rendered = inspector.render();
        assert!(rendered.contains("task:"));
        assert!(rendered.contains("requirements outstanding"));
        assert!(rendered.contains("latency:"));
        assert!(rendered.contains("usage:"));
        assert!(rendered.contains("decisions:"));
        assert!(rendered.contains("model choice"));
    }
}
