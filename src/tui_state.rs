//! The workbench shell's UI state: a pure reducer over the shared session
//! event stream (issue #27).
//!
//! The boundary is deliberate, following the Elm-style split the roadmap
//! references: this module owns *state transitions only*. It performs no
//! I/O, no rendering and no session work, so a slow or disconnected
//! provider can never block typing, navigation or inspection. Rendering
//! lives in [`crate::tui_render`]; the effects live in the binary.
//!
//! Timeline entries are fed from [`crate::SessionEvent`]s, so the TUI
//! shows exactly what the engine published — it never runs its own loop
//! or invents progress the runtime did not report.

use crate::session::{SessionEvent, TaskState, WaitKind};
use crate::tree::NodeStatus;
use crate::{EdgeChoice, FrameKind};

/// Which pane has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Timeline,
    Inspector,
}

/// One line of the conversation/action timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEntry {
    pub kind: TimelineKind,
    pub text: String,
    /// Whether this entry is part of the streaming tail (coalesced).
    pub streaming: bool,
}

/// The kinds of timeline entry the shell renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineKind {
    User,
    Assistant,
    Routing,
    Decision,
    Action,
    Evidence,
    Wait,
    Terminal,
    Error,
}

impl TimelineKind {
    /// Short, non-color label so status is readable without color.
    pub fn label(self) -> &'static str {
        match self {
            TimelineKind::User => "you",
            TimelineKind::Assistant => "knut",
            TimelineKind::Routing => "route",
            TimelineKind::Decision => "decide",
            TimelineKind::Action => "action",
            TimelineKind::Evidence => "check",
            TimelineKind::Wait => "wait",
            TimelineKind::Terminal => "done",
            TimelineKind::Error => "error",
        }
    }
}

/// A pending interaction the user must resolve.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPrompt {
    pub kind: WaitKind,
    pub message: String,
}

/// The whole shell state.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkbenchState {
    /// Workspace label (path or name), shown in the header.
    pub workspace: String,
    /// Branch label, when known.
    pub branch: Option<String>,
    /// Execution mode label (e.g. "quality").
    pub mode: String,
    /// Provider/model label, when configured.
    pub model: Option<String>,
    pub task_state: Option<TaskState>,
    pub focus: Focus,
    /// The composer's text, as a sequence of lines.
    pub composer: Vec<String>,
    /// Cursor position within the composer.
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub timeline: Vec<TimelineEntry>,
    /// Number of entries dropped from the front (virtualization).
    pub timeline_offset: usize,
    /// Index of the selected timeline entry.
    pub selection: usize,
    /// Whether the timeline is scrolled to the newest entry.
    pub follow: bool,
    pub pending: Option<PendingPrompt>,
    /// Whether a help overlay is open.
    pub help: bool,
    /// Transient status message.
    pub status: Option<String>,
    /// Counters for the inspector.
    pub stats: WorkbenchStats,
}

/// Small counters shown in the inspector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkbenchStats {
    pub turns: usize,
    pub node_results: usize,
    pub decisions: usize,
    pub text_deltas: usize,
    pub dropped: u64,
}

/// Maximum retained timeline entries: an unbounded history is a memory
/// leak, so old entries are dropped from the front.
pub const MAX_TIMELINE: usize = 2000;

/// Maximum characters kept per timeline entry after coalescing.
pub const MAX_ENTRY_CHARS: usize = 4000;

impl WorkbenchState {
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            branch: None,
            mode: "quality".to_owned(),
            model: None,
            task_state: None,
            focus: Focus::Composer,
            composer: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            timeline: Vec::new(),
            timeline_offset: 0,
            selection: 0,
            follow: true,
            pending: None,
            help: false,
            status: None,
            stats: WorkbenchStats::default(),
        }
    }

    /// Composer contents as one string.
    pub fn composer_text(&self) -> String {
        self.composer.join("\n")
    }

    pub fn is_composer_empty(&self) -> bool {
        self.composer.iter().all(|line| line.trim().is_empty())
    }

    /// Clear the composer after a submit.
    pub fn clear_composer(&mut self) {
        self.composer = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// The entries currently visible, newest last.
    pub fn visible_timeline(&self) -> &[TimelineEntry] {
        &self.timeline
    }

    /// Append an entry, coalescing streaming text and bounding history.
    fn push(&mut self, kind: TimelineKind, text: impl Into<String>, streaming: bool) {
        let text = text.into();

        // Streaming text arrives in fragments; appending one entry per
        // fragment would flood the timeline, so consecutive assistant
        // fragments merge into the entry already in progress.
        if streaming
            && let Some(last) = self.timeline.last_mut()
            && last.streaming
            && last.kind == kind
        {
            if last.text.chars().count() < MAX_ENTRY_CHARS {
                last.text.push_str(&text);
            }
            return;
        }

        let mut text = text;
        if text.chars().count() > MAX_ENTRY_CHARS {
            text = text.chars().take(MAX_ENTRY_CHARS).collect();
            text.push('…');
        }
        self.timeline.push(TimelineEntry {
            kind,
            text,
            streaming,
        });

        // Bound the history; dropped entries are counted so the inspector
        // can show that virtualization happened.
        while self.timeline.len() > MAX_TIMELINE {
            self.timeline.remove(0);
            self.timeline_offset += 1;
        }

        if self.follow {
            self.selection = self.timeline.len().saturating_sub(1);
        }
    }

    /// Apply one session event to the state.
    ///
    /// This is the whole reducer: pure, synchronous and independent of
    /// I/O, so it can be replayed in a test at full speed.
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::SessionStarted { protocol_version } => {
                self.status = Some(format!("session protocol v{protocol_version}"));
            }
            SessionEvent::TaskStarted { prompt, .. } => {
                self.push(TimelineKind::User, prompt.clone(), false);
                self.task_state = Some(TaskState::Running);
                self.pending = None;
            }
            SessionEvent::TaskSteered {
                prompt, revision, ..
            } => {
                self.push(
                    TimelineKind::User,
                    format!("[steered r{}] {prompt}", revision.0),
                    false,
                );
            }
            SessionEvent::Routed {
                source,
                action,
                confidence,
                ..
            } => {
                self.stats.turns += 1;
                self.push(
                    TimelineKind::Routing,
                    format!("{source:?} -> {action:?} ({:.2})", confidence),
                    false,
                );
            }
            SessionEvent::Generating { .. } => {
                self.status = Some("generating…".to_owned());
            }
            SessionEvent::PlanStarted { node_count, .. } => {
                self.push(
                    TimelineKind::Action,
                    format!("plan with {node_count} node(s)"),
                    false,
                );
            }
            SessionEvent::NodeResult {
                node_label,
                status,
                output,
                ..
            } => {
                self.stats.node_results += 1;
                let marker = match status {
                    NodeStatus::Succeeded => "ok",
                    NodeStatus::Failed => "failed",
                    NodeStatus::Blocked => "blocked",
                    NodeStatus::Running => "running",
                    NodeStatus::Pending => "pending",
                };
                let summary = summarize(output);
                self.push(
                    TimelineKind::Action,
                    format!("{node_label}: {marker}{summary}"),
                    false,
                );
            }
            SessionEvent::EdgeDecided {
                proposed,
                effective,
                overridden,
                ..
            } => {
                self.push(
                    TimelineKind::Decision,
                    match (proposed, effective) {
                        (EdgeChoice::Done, EdgeChoice::Done) => "edge: done".to_owned(),
                        _ if *overridden => {
                            format!("edge: {proposed:?} overridden to {effective:?}")
                        }
                        _ => format!("edge: {effective:?}"),
                    },
                    false,
                );
            }
            SessionEvent::FrameDecided {
                question_kind,
                frame_version,
                choice,
                confidence,
                overridden,
                ..
            } => {
                self.stats.decisions += 1;
                let kind = match question_kind {
                    FrameKind::Ingress => "ingress",
                    FrameKind::Recovery => "recovery",
                    FrameKind::Continuation => "continuation",
                    FrameKind::CandidateSelection => "candidate",
                };
                self.push(
                    TimelineKind::Decision,
                    format!(
                        "system one [{kind} v{frame_version}]: {choice} ({confidence:.2}){}",
                        if *overridden { " [overridden]" } else { "" }
                    ),
                    false,
                );
            }
            SessionEvent::WaitingForUser { wait, message, .. } => {
                self.pending = Some(PendingPrompt {
                    kind: wait.clone(),
                    message: message.clone(),
                });
                self.task_state = Some(TaskState::Waiting);
                self.push(TimelineKind::Wait, message.clone(), false);
            }
            SessionEvent::WaitResolved { .. } => {
                self.pending = None;
            }
            SessionEvent::Paused { .. } => {
                self.task_state = Some(TaskState::Paused);
                self.status = Some("paused".to_owned());
            }
            SessionEvent::Resumed { .. } => {
                self.task_state = Some(TaskState::Running);
                self.status = Some("resumed".to_owned());
            }
            SessionEvent::TaskCompleted { summary, .. } => {
                self.task_state = Some(TaskState::Completed);
                self.push(TimelineKind::Terminal, summary.clone(), false);
            }
            SessionEvent::TaskFailed { reason, .. } => {
                self.task_state = Some(TaskState::Failed);
                self.push(TimelineKind::Error, reason.clone(), false);
            }
            SessionEvent::TaskCancelled { .. } => {
                self.task_state = Some(TaskState::Cancelled);
                self.push(TimelineKind::Terminal, "cancelled", false);
            }
            SessionEvent::TextDelta { text, .. } => {
                self.stats.text_deltas += 1;
                self.push(TimelineKind::Assistant, text.clone(), true);
            }
            SessionEvent::ToolCallProposed {
                name, arguments, ..
            } => {
                self.push(
                    TimelineKind::Action,
                    format!("tool call {name} {arguments}"),
                    false,
                );
            }
            SessionEvent::RuntimeError { message, .. } => {
                self.push(TimelineKind::Error, message.clone(), false);
            }
        }
    }

    /// Record that the runtime dropped cosmetic events.
    pub fn note_dropped(&mut self, dropped: u64) {
        self.stats.dropped = dropped;
    }
}

/// A short, single-line summary of structured output.
fn summarize(output: &serde_json::Value) -> String {
    if output.is_null() {
        return String::new();
    }
    let text = output.to_string();
    let bounded: String = text.chars().take(160).collect();
    format!(" {bounded}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{TaskId, TaskRevision, TurnId};
    use crate::{Action, DecisionSource, ModelTier};

    fn task_started(prompt: &str) -> SessionEvent {
        SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: prompt.to_owned(),
        }
    }

    fn text_delta(text: &str) -> SessionEvent {
        SessionEvent::TextDelta {
            task: TaskId(1),
            turn: TurnId(1),
            node: crate::session::NodeId(1),
            text: text.to_owned(),
        }
    }

    #[test]
    fn assistant_fragments_coalesce_into_one_entry() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.apply(&text_delta("Hel"));
        state.apply(&text_delta("lo, "));
        state.apply(&text_delta("world"));

        // One assistant entry, not three: a streaming turn must not flood
        // the timeline.
        assert_eq!(state.timeline.len(), 1);
        assert_eq!(state.timeline[0].text, "Hello, world");
        assert_eq!(state.stats.text_deltas, 3);
    }

    #[test]
    fn a_submitted_prompt_becomes_a_user_entry() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.apply(&task_started("fix the failing test"));
        assert_eq!(state.timeline[0].kind, TimelineKind::User);
        assert_eq!(state.timeline[0].text, "fix the failing test");
        assert_eq!(state.task_state, Some(TaskState::Running));
    }

    #[test]
    fn waiting_then_resolving_clears_the_pending_prompt() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.apply(&SessionEvent::WaitingForUser {
            task: TaskId(1),
            turn: TurnId(2),
            wait: WaitKind::Approval {
                approval_key: "fp".to_owned(),
            },
            message: "needs approval".to_owned(),
        });
        assert!(state.pending.is_some());
        assert_eq!(state.task_state, Some(TaskState::Waiting));

        state.apply(&SessionEvent::WaitResolved {
            task: TaskId(1),
            turn: TurnId(2),
        });
        assert!(state.pending.is_none());
    }

    #[test]
    fn terminal_events_set_the_task_state_and_never_look_running() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.apply(&task_started("x"));
        state.apply(&SessionEvent::TaskFailed {
            task: TaskId(1),
            reason: "checks failed".to_owned(),
        });
        assert_eq!(state.task_state, Some(TaskState::Failed));
        assert_eq!(state.timeline.last().unwrap().kind, TimelineKind::Error);
    }

    #[test]
    fn history_is_bounded_and_counts_virtualized_entries() {
        let mut state = WorkbenchState::new("/tmp/ws");
        for i in 0..(MAX_TIMELINE + 500) {
            state.apply(&SessionEvent::Routed {
                task: TaskId(1),
                turn: TurnId(1),
                revision: TaskRevision(1),
                source: DecisionSource::SystemOne,
                action: Action::Generate(ModelTier::Fast),
                confidence: 0.9,
            });
            let _ = i;
        }
        assert!(state.timeline.len() <= MAX_TIMELINE);
        assert!(state.timeline_offset > 0);
    }

    #[test]
    fn very_long_entries_are_bounded() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let huge = "x".repeat(MAX_ENTRY_CHARS * 2);
        state.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(1),
            node: crate::session::NodeId(1),
            node_label: "read".to_owned(),
            status: NodeStatus::Succeeded,
            output: serde_json::Value::String(huge),
        });
        assert!(state.timeline[0].text.chars().count() <= MAX_ENTRY_CHARS + 200);
    }

    #[test]
    fn the_reducer_performs_no_io() {
        // A pure state transition: applying a long event stream is
        // immediate and testable without a terminal, provider or disk.
        let mut state = WorkbenchState::new("/tmp/ws");
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            state.apply(&text_delta("tok"));
        }
        // Coalescing keeps this trivially fast; the assertion is about
        // the property (no blocking work), not a benchmark.
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(state.timeline.len() <= MAX_TIMELINE);
    }

    #[test]
    fn composer_editing_is_pure_and_testable() {
        let mut state = WorkbenchState::new("/tmp/ws");
        assert!(state.is_composer_empty());

        state.composer = vec!["first line".to_owned(), "second".to_owned()];
        state.cursor_row = 1;
        state.cursor_col = 6;
        assert_eq!(state.composer_text(), "first line\nsecond");
        assert!(!state.is_composer_empty());

        state.clear_composer();
        assert!(state.is_composer_empty());
        assert_eq!(state.cursor_row, 0);
    }

    #[test]
    fn unicode_and_long_paths_survive_the_reducer() {
        let mut state = WorkbenchState::new("/home/user/日本語のプロジェクト");
        state.apply(&text_delta("スカンジナビア"));
        state.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(1),
            node: crate::session::NodeId(1),
            node_label: "src/überlegungen.rs".to_owned(),
            status: NodeStatus::Succeeded,
            output: serde_json::json!({ "path": "src/日本語.md" }),
        });
        assert!(state.timeline[0].text.contains("スカンジナビア"));
        assert!(state.timeline[1].text.contains("überlegungen.rs"));
    }
}
