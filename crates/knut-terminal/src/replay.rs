use knut_runtime::{KnutError, SessionStore};
/// Rebuild workbench state from a stored transcript.
///
/// The reducer is pure, so replaying is state-only: nothing here can
/// dispatch a tool or a provider call.
pub fn replay_state(
    store: &SessionStore,
    session_id: &str,
    workspace: impl Into<String>,
) -> Result<crate::tui_state::WorkbenchState, KnutError> {
    let mut state = crate::tui_state::WorkbenchState::new(workspace);
    for event in store.events(session_id)? {
        state.apply(&event);
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use knut_runtime::{PersistedOutcome, SessionEvent, TaskId};
    fn store() -> SessionStore {
        SessionStore::in_memory().unwrap()
    }
    fn event(prompt: &str) -> SessionEvent {
        SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: prompt.into(),
        }
    }
    #[test]
    fn replaying_a_session_dispatches_nothing() {
        // The transcript replays state only: a completed write in the
        // transcript produces a UI entry, never a second invocation.
        let mut store = store();
        store
            .start_session("s1", "/ws", "rev-1", "pol-1", "running", 1_000)
            .unwrap();
        store
            .append_event("s1", &event("apply the patch"), 1_001)
            .unwrap();
        store
            .record_intent("s1", "fp", "files", "apply_patch", 1_002)
            .unwrap();
        store
            .record_outcome("fp", PersistedOutcome::Completed, None, 1_003)
            .unwrap();
        store
            .append_event(
                "s1",
                &SessionEvent::TaskCompleted {
                    task: TaskId(1),
                    summary: "applied".to_owned(),
                },
                1_004,
            )
            .unwrap();

        let state = replay_state(&store, "s1", "/ws").unwrap();
        // The state reflects the transcript.
        assert_eq!(state.task_state, Some(crate::session::TaskState::Completed));
        // And nothing was dispatched: the operation row is unchanged and
        // there is no new intent.
        assert_eq!(
            store.operation("fp").unwrap().unwrap().outcome,
            PersistedOutcome::Completed
        );
        assert!(store.unreconciled_operations().unwrap().is_empty());
    }
}
