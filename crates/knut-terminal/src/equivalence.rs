use knut_runtime::*;
use serde::{Deserialize, Serialize};
/// Equivalence check between the three adapters.
///
/// The acceptance test for #40: one scripted task must produce equivalent
/// execution outcomes whether observed through the TUI event reducer,
/// JSONL or ACP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterEquivalence {
    pub tui_outcome: Option<TaskState>,
    pub jsonl_outcome: Option<HeadlessOutcome>,
    pub acp_stop_reason: Option<AcpStopReason>,
    /// Whether all three agree on the terminal outcome.
    pub agrees: bool,
    /// Where they disagree, when they do.
    pub disagreement: Option<String>,
}

impl AdapterEquivalence {
    /// Compare the three adapters' readings of one event stream.
    pub fn assess(events: &[SessionEvent]) -> Self {
        let mut tui_state = crate::tui_state::WorkbenchState::new("headless");
        let mut adapter = HeadlessAdapter::new("s1");
        let mut acp = AcpAdapter::initialize(&AcpInitialize {
            protocol_version: ACP_PROTOCOL_VERSION,
            client_capabilities: serde_json::json!({}),
        })
        .expect("supported protocol");
        acp.session_id = "s1".to_owned();

        let mut jsonl_outcome = None;
        for event in events {
            tui_state.apply(event);
            let _ = adapter.translate(event);
            let _ = acp.session_update(event);
            if let Some(HeadlessEvent::Outcome { state, .. }) = adapter.outcome(event) {
                jsonl_outcome = Some(state);
            }
            if let SessionEvent::TaskCompleted { .. } = event {
                acp.finish(TaskState::Completed);
            }
            if let SessionEvent::TaskFailed { .. } = event {
                acp.finish(TaskState::Failed);
            }
            if let SessionEvent::TaskCancelled { .. } = event {
                acp.finish(TaskState::Cancelled);
            }
        }

        let tui_outcome = tui_state.task_state.filter(|state| state.is_terminal());
        let acp_stop_reason = acp.last_stop_reason;

        // The three must agree, translated through their own vocabularies.
        let jsonl_reading = jsonl_outcome;
        let mut disagreement = None;
        if let Some(tui) = tui_outcome {
            if let Some(jsonl) = jsonl_reading
                && HeadlessOutcome::from_task_state(tui) != Some(jsonl)
            {
                disagreement = Some(format!("TUI {tui:?} vs JSONL {jsonl:?}"));
            }
            if let Some(acp_reason) = acp_stop_reason
                && AcpStopReason::from_task_state(tui) != Some(acp_reason)
            {
                disagreement = Some(format!("TUI {tui:?} vs ACP {acp_reason:?}"));
            }
        }

        Self {
            tui_outcome,
            jsonl_outcome: jsonl_reading,
            acp_stop_reason,
            agrees: disagreement.is_none(),
            disagreement,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use knut_runtime::session::NodeId;
    fn started() -> SessionEvent {
        SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "fix the failing test".into(),
        }
    }
    fn completed() -> SessionEvent {
        SessionEvent::TaskCompleted {
            task: TaskId(1),
            summary: "the fix verified".into(),
        }
    }
    #[test]
    fn a_scripted_task_produces_equivalent_outcomes_through_all_three_adapters() {
        // The acceptance test for #40: the same event stream, read by the
        // TUI reducer, the JSONL adapter and the ACP adapter.
        let events = vec![
            started(),
            SessionEvent::Routed {
                task: TaskId(1),
                turn: TurnId(1),
                revision: TaskRevision(1),
                source: crate::DecisionSource::SystemOne,
                action: crate::Action::Generate(crate::ModelTier::Reasoner),
                confidence: 0.9,
            },
            SessionEvent::TextDelta {
                task: TaskId(1),
                turn: TurnId(1),
                node: NodeId(1),
                text: "working".to_owned(),
            },
            completed(),
        ];

        let equivalence = AdapterEquivalence::assess(&events);
        assert!(equivalence.agrees, "{equivalence:?}");
        assert_eq!(equivalence.tui_outcome, Some(TaskState::Completed));
        assert_eq!(equivalence.jsonl_outcome, Some(HeadlessOutcome::Completed));
        assert_eq!(equivalence.acp_stop_reason, Some(AcpStopReason::EndTurn));
    }
    #[test]
    fn all_three_adapters_agree_on_failure_and_cancellation_too() {
        for (terminal, state, headless, acp) in [
            (
                SessionEvent::TaskFailed {
                    task: TaskId(1),
                    reason: "the check failed".to_owned(),
                },
                TaskState::Failed,
                HeadlessOutcome::Failed,
                AcpStopReason::Refusal,
            ),
            (
                SessionEvent::TaskCancelled { task: TaskId(1) },
                TaskState::Cancelled,
                HeadlessOutcome::Cancelled,
                AcpStopReason::Cancelled,
            ),
        ] {
            let events = vec![started(), terminal];
            let equivalence = AdapterEquivalence::assess(&events);
            assert!(equivalence.agrees, "{equivalence:?}");
            assert_eq!(equivalence.tui_outcome, Some(state));
            assert_eq!(equivalence.jsonl_outcome, Some(headless));
            assert_eq!(equivalence.acp_stop_reason, Some(acp));
        }
    }
}
