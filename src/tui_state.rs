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
use crate::theme::Theme;
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
    /// The provider endpoint's host, when configured.
    pub endpoint: Option<String>,
    /// Why live work is unavailable, when it is.
    pub unavailable: Option<String>,
    /// How many completion checks gate this session.
    pub checks: usize,
    /// Whether routing decisions come from a live System One rather than
    /// the deterministic fallback. It matters to the user: live routing is
    /// what lets a prompt reach the workspace tools.
    pub live_routing: bool,
    /// Model calls observed by the runtime.
    pub model_calls: u64,
    /// Wall-clock start of the current task, for the header ticker.
    task_started: Option<std::time::Instant>,
    /// Final duration of the last task, so a finished run still shows one.
    last_duration_secs: u64,
    /// Monotonic frame counter, used to animate spinners without a clock.
    pub tick: u64,
    /// The resolved theme: colour capability and glyph set.
    pub theme: Theme,
    pub task_state: Option<TaskState>,
    pub focus: Focus,
    /// The multiline composer (issue #28): grapheme-aware editing,
    /// undo/redo, history and bracketed paste.
    pub composer: crate::composer::Composer,
    /// Action cards for the current session (issue #28).
    pub cards: crate::cards::CardList,
    /// Requests queued while a task is running (issue #28): these start a
    /// *new* task when the current one ends and never modify it.
    pub queued: Vec<String>,
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
    /// Whether the optional decision inspector is shown.
    pub show_inspector: bool,
    /// Command palette state: the query when open.
    pub palette: Option<String>,
    /// Attachments resolved for the current composer text.
    pub attachments: Vec<crate::attach::Attachment>,
    /// Attachment errors to surface to the user.
    pub attachment_errors: Vec<crate::attach::AttachError>,
    /// The review workspace, when the user has opened one (issue #29).
    pub review: Option<crate::review::ReviewView>,
    /// The optional decision inspector (issue #30). Built from the same
    /// events the timeline uses, so it never narrates anything the
    /// runtime did not report.
    pub inspector: crate::inspector::DecisionInspector,
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
            endpoint: None,
            unavailable: None,
            checks: 0,
            live_routing: false,
            model_calls: 0,
            task_started: None,
            last_duration_secs: 0,
            tick: 0,
            theme: Theme::detect(),
            task_state: None,
            focus: Focus::Composer,
            composer: crate::composer::Composer::new(),
            cards: crate::cards::CardList::new(),
            queued: Vec::new(),
            timeline: Vec::new(),
            timeline_offset: 0,
            selection: 0,
            follow: true,
            pending: None,
            help: false,
            show_inspector: false,
            palette: None,
            attachments: Vec::new(),
            attachment_errors: Vec::new(),
            review: None,
            inspector: crate::inspector::DecisionInspector::new(),
            status: None,
            stats: WorkbenchStats::default(),
        }
    }

    /// A state with an explicit theme: used by tests and by callers that
    /// have already resolved the terminal's capability.
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Advance the animation tick. The shell calls this once per frame;
    /// spinners read from it so rendering stays a pure function of state.
    pub fn advance(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Seconds since the current task started (0 when idle).
    pub fn elapsed_secs(&self) -> u64 {
        match &self.task_started {
            Some(started) => started.elapsed().as_secs(),
            None => self.last_duration_secs,
        }
    }

    /// A short label for the configured reasoner.
    pub fn reasoner_label(&self) -> String {
        self.model.clone().unwrap_or_else(|| "offline".to_owned())
    }

    /// One line describing the engine's configuration, for the footer and
    /// the `doctor` palette entry. Everything here is already known to the
    /// shell, so it costs no calls and cannot drift from what the header
    /// shows.
    pub fn setup_summary(&self) -> String {
        match &self.model {
            Some(model) => {
                let endpoint = self.endpoint.as_deref().unwrap_or("default endpoint");
                format!(
                    "{model} via {endpoint}, {} checks, mode {}",
                    self.checks, self.mode
                )
            }
            None => self
                .unavailable
                .clone()
                .unwrap_or_else(|| "no reasoner configured".to_owned()),
        }
    }

    /// Composer contents as one string.
    pub fn composer_text(&self) -> String {
        self.composer.text()
    }

    pub fn is_composer_empty(&self) -> bool {
        self.composer.is_empty()
    }

    /// Clear the composer after a submit.
    pub fn clear_composer(&mut self) {
        self.composer = crate::composer::Composer::new();
    }

    /// Queue a request for after the current task (never a steering edit).
    pub fn queue_request(&mut self, text: impl Into<String>) {
        self.queued.push(text.into());
    }

    /// Take the next queued request, if any.
    pub fn take_queued(&mut self) -> Option<String> {
        if self.queued.is_empty() {
            None
        } else {
            Some(self.queued.remove(0))
        }
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
        // The inspector consumes the same stream; it performs no I/O and
        // adds nothing the runtime did not report.
        self.inspector.apply(event);
        match event {
            SessionEvent::SessionStarted { protocol_version } => {
                self.status = Some(format!("session protocol v{protocol_version}"));
            }
            SessionEvent::TaskStarted { prompt, .. } => {
                self.push(TimelineKind::User, prompt.clone(), false);
                self.task_state = Some(TaskState::Running);
                self.pending = None;
                // The header ticker measures the *current* task; a finished
                // task keeps its last duration instead of counting forever.
                self.task_started = Some(std::time::Instant::now());
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
            SessionEvent::RequestQueued { text } => {
                // Queued work is shown as queued, never as a change to the
                // running task.
                self.queue_request(text.clone());
                self.push(
                    TimelineKind::Action,
                    format!("queued for next task: {text}"),
                    false,
                );
            }
            SessionEvent::CardStarted { card_id, title, .. } => {
                self.cards.start(card_id.clone(), title.clone());
            }
            SessionEvent::CardFinished {
                card_id,
                state,
                summary,
                detail,
                elapsed_ms,
                ..
            } => {
                self.cards.finish(
                    card_id,
                    *state,
                    summary.clone(),
                    detail.clone(),
                    *elapsed_ms,
                );
            }
            SessionEvent::NodeResult {
                node_label,
                status,
                output,
                node,
                ..
            } => {
                self.stats.node_results += 1;
                // The card mirrors the engine's reported status, sanitized
                // for display.
                let card_id = format!("node:{}", node.0);
                self.cards.start(card_id.clone(), node_label.clone());
                self.cards.finish(
                    &card_id,
                    crate::cards::CardState::from_node_status(*status),
                    crate::cards::summarize_value(output),
                    crate::cards::sanitize_for_display(&output.to_string()),
                    0,
                );
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
                self.finish_timing();
                self.task_state = Some(TaskState::Completed);
                self.push(TimelineKind::Terminal, summary.clone(), false);
            }
            SessionEvent::TaskFailed { reason, .. } => {
                self.finish_timing();
                self.task_state = Some(TaskState::Failed);
                self.push(TimelineKind::Error, reason.clone(), false);
            }
            SessionEvent::TaskCancelled { .. } => {
                self.finish_timing();
                self.task_state = Some(TaskState::Cancelled);
                self.push(TimelineKind::Terminal, "cancelled", false);
            }
            SessionEvent::TextDelta { text, .. } => {
                self.stats.text_deltas += 1;
                // Untrusted model output is sanitized before display.
                self.push(
                    TimelineKind::Assistant,
                    crate::cards::sanitize_for_display(text),
                    true,
                );
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

    /// Open the palette.
    pub fn open_palette(&mut self) {
        self.palette = Some(String::new());
    }

    /// Stop the header ticker and remember how long the task took.
    fn finish_timing(&mut self) {
        if let Some(started) = self.task_started.take() {
            self.last_duration_secs = started.elapsed().as_secs();
        }
    }

    /// Close the palette.
    pub fn close_palette(&mut self) {
        self.palette = None;
    }

    /// Whether the palette is open.
    pub fn palette_open(&self) -> bool {
        self.palette.is_some()
    }

    /// Commands matching the current palette query.
    pub fn palette_results(&self) -> Vec<crate::attach::PaletteCommand> {
        match &self.palette {
            Some(query) => crate::attach::filter_catalog(query),
            None => Vec::new(),
        }
    }

    /// Resolve `@mentions` in the composer against the workspace.
    ///
    /// Resolving is a *local* read through the workspace tools: it grants
    /// no egress permission, and the summary exists so the user sees what
    /// would be attached before submitting.
    pub fn refresh_attachments(&mut self, workspace: &crate::Workspace) {
        let mentions = crate::attach::extract_mentions(&self.composer.text());
        let (attachments, errors) = crate::attach::resolve_mentions(workspace, &mentions);
        self.attachments = attachments;
        self.attachment_errors = errors;
    }

    /// A summary of what the next submission would carry.
    pub fn attachment_summary(&self) -> String {
        crate::attach::attachment_summary(&self.attachments, &self.attachment_errors)
    }

    /// Resume following the newest output (an explicit user action).
    pub fn resume_follow(&mut self) {
        self.follow = true;
        self.selection = self.timeline.len().saturating_sub(1);
    }

    /// Record that the runtime dropped cosmetic events.
    pub fn note_dropped(&mut self, dropped: u64) {
        self.stats.dropped = dropped;
    }

    /// Checks the session has already run, in review-row form.
    ///
    /// These are the *real* rows recorded by a check run; the session never
    /// synthesizes a row for a check that did not happen.
    pub fn known_checks(&self) -> Vec<crate::review::CheckRow> {
        self.review
            .as_ref()
            .map(|view| view.checks.clone())
            .unwrap_or_default()
    }

    /// The change set the session is reviewing, if one was recorded.
    pub fn review_changes(&self) -> crate::review::ChangeSet {
        self.review
            .as_ref()
            .map(|view| view.changes.clone())
            .unwrap_or_else(crate::review::ChangeSet::empty)
    }

    /// Record a validated change set for review.
    pub fn set_review_changes(&mut self, changes: crate::review::ChangeSet) {
        match &mut self.review {
            Some(view) => view.changes = changes,
            None => self.review = Some(crate::review::ReviewView::new(changes)),
        }
    }

    /// Apply the outcome of an on-demand check run.
    ///
    /// A run that could not happen says so; it never becomes "no checks
    /// reported", which would read as success.
    pub fn apply_check_outcome(&mut self, outcome: crate::tui::CheckOutcome) {
        match outcome {
            crate::tui::CheckOutcome::Ran { rows, revision } => {
                self.checks = rows.len();
                let green = rows.iter().filter(|row| row.is_green()).count();
                self.status = Some(format!("checks {green}/{}", rows.len()));
                self.push(
                    TimelineKind::Evidence,
                    format!(
                        "checks for {}: {}",
                        short_revision(&revision),
                        summarize_checks(&rows)
                    ),
                    false,
                );
                match &mut self.review {
                    Some(view) => view.checks = rows,
                    None => {
                        let mut view =
                            crate::review::ReviewView::new(crate::review::ChangeSet::empty());
                        view.checks = rows;
                        self.review = Some(view);
                    }
                }
            }
            crate::tui::CheckOutcome::Unavailable(reason) => {
                self.status = Some("checks unavailable".to_owned());
                self.push(
                    TimelineKind::Error,
                    format!("checks unavailable: {reason}"),
                    false,
                );
            }
        }
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

/// A short label for a revision identity, so the transcript does not carry
/// a full content hash.
fn short_revision(revision: &str) -> String {
    revision.chars().take(12).collect()
}

/// One line summarizing check rows, counting real outcomes only.
fn summarize_checks(rows: &[crate::review::CheckRow]) -> String {
    if rows.is_empty() {
        return "no checks were reported".to_owned();
    }
    let green = rows.iter().filter(|row| row.is_green()).count();
    let failed = rows
        .iter()
        .filter(|row| row.state == crate::verify::CheckOutcome::Failed)
        .count();
    let names: Vec<String> = rows
        .iter()
        .map(|row| format!("{} {}", row.name, row.label()))
        .collect();
    format!(
        "{green}/{} passed{}{} — {}",
        rows.len(),
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        },
        if rows.len() > green + failed {
            format!(", {} other", rows.len() - green - failed)
        } else {
            String::new()
        },
        names.join(", ")
    )
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
    fn the_header_clock_measures_the_task_and_stops_at_its_end() {
        let mut state = WorkbenchState::new("/tmp/ws");
        assert_eq!(state.elapsed_secs(), 0, "idle before any task");

        state.apply(&task_started("x"));
        assert!(state.task_started.is_some());

        state.apply(&SessionEvent::TaskCompleted {
            task: TaskId(1),
            summary: "done".to_owned(),
        });
        // A finished task stops counting: the ticker must not run forever
        // on a task that ended.
        assert!(state.task_started.is_none());
        assert_eq!(state.elapsed_secs(), state.last_duration_secs);
    }

    #[test]
    fn a_check_run_never_fabricates_a_result() {
        let mut state = WorkbenchState::new("/tmp/ws");
        // A run that could not happen says so, and reports no checks.
        state.apply_check_outcome(crate::tui::CheckOutcome::Unavailable(
            "no workspace".to_owned(),
        ));
        assert!(
            state.timeline.iter().any(
                |entry| entry.kind == TimelineKind::Error && entry.text.contains("unavailable")
            ),
            "an unavailable run is reported as an error, not silence"
        );
        assert!(state.known_checks().is_empty());
    }

    #[test]
    fn a_check_run_records_real_rows_and_summarizes_them() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let rows = vec![
            crate::review::CheckRow {
                name: "build".to_owned(),
                state: crate::verify::CheckOutcome::Passed,
                revision: "abc123".to_owned(),
                current_revision: true,
                summary: "ok".to_owned(),
                diagnostics: Vec::new(),
                output_truncated: false,
            },
            crate::review::CheckRow {
                name: "test".to_owned(),
                state: crate::verify::CheckOutcome::Failed,
                revision: "abc123".to_owned(),
                current_revision: true,
                summary: "3 failed".to_owned(),
                diagnostics: vec!["error[E0308]".to_owned()],
                output_truncated: false,
            },
        ];
        state.apply_check_outcome(crate::tui::CheckOutcome::Ran {
            rows,
            revision: "abcdef0123456789".to_owned(),
        });

        assert_eq!(state.checks, 2);
        assert_eq!(state.known_checks().len(), 2);
        // The summary must not report a failing suite as green.
        let summary = state.timeline.last().unwrap();
        assert!(summary.text.contains("1/2"));
        assert!(summary.text.contains("test failed"));
    }

    #[test]
    fn a_review_with_no_content_says_so_rather_than_opening_empty() {
        let state = WorkbenchState::new("/tmp/ws");
        assert!(state.known_checks().is_empty());
        assert!(state.review_changes().is_empty());
    }

    #[test]
    fn the_setup_summary_states_the_configuration_it_has() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.model = Some("glm-5.3-flash".to_owned());
        state.endpoint = Some("api.z.ai".to_owned());
        state.checks = 3;
        let summary = state.setup_summary();
        assert!(summary.contains("glm-5.3-flash"));
        assert!(summary.contains("api.z.ai"));
        assert!(summary.contains('3'));

        // Without a model the summary explains what is missing.
        let mut offline = WorkbenchState::new("/tmp/ws");
        offline.unavailable = Some("set KNUT_PROVIDER_API_KEY".to_owned());
        assert!(offline.setup_summary().contains("KNUT_PROVIDER_API_KEY"));
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

        state.composer.paste("first line\nsecond");
        assert_eq!(state.composer_text(), "first line\nsecond");
        assert!(!state.is_composer_empty());
        // The cursor sits at the end of the pasted text, not mid-buffer.
        assert_eq!(state.composer.cursor(), (1, 6));

        state.clear_composer();
        assert!(state.is_composer_empty());
        assert_eq!(state.composer.cursor(), (0, 0));
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
