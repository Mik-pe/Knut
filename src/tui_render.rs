//! Rendering for the workbench shell (issue #27).
//!
//! Pure functions over [`WorkbenchState`]: given state and a frame area,
//! produce widgets. No state mutation, no I/O, no terminal control beyond
//! the frame it is handed — so every layout can be snapshot-tested with
//! `TestBackend` at any size.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::session::TaskState;
use crate::tui_state::{Focus, TimelineKind, WorkbenchState};

/// Width below which panes collapse into a single tabbed column.
pub const NARROW_WIDTH: u16 = 80;
/// Height below which the inspector is dropped entirely.
pub const SHORT_HEIGHT: u16 = 20;

/// Which pane is shown when the terminal is too narrow to split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Timeline,
    Inspector,
    Tasks,
}

/// The layout chosen for one frame: useful panes only, and never a
/// permanent empty dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPlan {
    pub header: Rect,
    pub timeline: Rect,
    pub inspector: Option<Rect>,
    pub composer: Rect,
    pub footer: Rect,
    /// Whether the terminal is too narrow to split panes.
    pub tabbed: bool,
}

/// Compute the layout for an area.
pub fn plan_layout(area: Rect, show_inspector: bool) -> LayoutPlan {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(3),    // body
            Constraint::Length(composer_height(area.height)),
            Constraint::Length(1), // footer
        ])
        .split(area);

    let header = vertical[0];
    let body = vertical[1];
    let composer = vertical[2];
    let footer = vertical[3];

    let tabbed = area.width < NARROW_WIDTH;
    let inspector_visible = show_inspector && !tabbed && area.height >= SHORT_HEIGHT;

    if inspector_visible {
        let horizontal = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(38)])
            .split(body);
        LayoutPlan {
            header,
            timeline: horizontal[0],
            inspector: Some(horizontal[1]),
            composer,
            footer,
            tabbed: false,
        }
    } else {
        LayoutPlan {
            header,
            timeline: body,
            inspector: None,
            composer,
            footer,
            tabbed,
        }
    }
}

/// The composer grows with the terminal but stays modest.
fn composer_height(height: u16) -> u16 {
    match height {
        0..=16 => 3,
        17..=30 => 4,
        _ => 6,
    }
}

/// Render the whole shell.
pub fn render(frame: &mut Frame, state: &WorkbenchState, tab: Tab) {
    let plan = plan_layout(frame.area(), true);

    render_header(frame, state, plan.header);
    match (plan.tabbed, tab) {
        (false, _) | (_, Tab::Timeline) => render_timeline(frame, state, plan.timeline),
        (true, Tab::Inspector) => render_inspector(frame, state, plan.timeline),
        (true, Tab::Tasks) => render_tasks(frame, state, plan.timeline),
    }
    if let Some(inspector) = plan.inspector {
        render_inspector(frame, state, inspector);
    }
    render_composer(frame, state, plan.composer);
    render_footer(frame, state, plan.footer, plan.tabbed, tab);

    if state.help {
        render_help(frame, frame.area());
    }

    // The cursor belongs to the composer, and only when the composer is
    // focused: a text cursor floating over a read-only pane is a lie.
    if state.focus == Focus::Composer {
        let area = plan.composer;
        let row = (area.y + 1 + state.cursor_row as u16).min(area.bottom().saturating_sub(1));
        let col = (area.x + 2 + state.cursor_col as u16).min(area.right().saturating_sub(1));
        frame.set_cursor_position((col, row));
    }
}

/// Text/icon marker for a task state, so status never relies on color.
fn state_marker(state: Option<TaskState>) -> (&'static str, &'static str) {
    match state {
        None => ("·", "idle"),
        Some(TaskState::Queued) => ("…", "queued"),
        Some(TaskState::Running) => (">", "running"),
        Some(TaskState::Waiting) => ("?", "waiting"),
        Some(TaskState::Paused) => ("=", "paused"),
        Some(TaskState::Completed) => ("+", "completed"),
        Some(TaskState::Failed) => ("!", "failed"),
        Some(TaskState::Cancelled) => ("x", "cancelled"),
    }
}

fn render_header(frame: &mut Frame, state: &WorkbenchState, area: Rect) {
    if area.height == 0 {
        return;
    }
    let (marker, label) = state_marker(state.task_state);
    let branch = state.branch.as_deref().unwrap_or("-");
    let model = state.model.as_deref().unwrap_or("not configured");

    // On narrow terminals the header drops detail rather than truncating
    // important text mid-word.
    let line = if area.width < NARROW_WIDTH {
        Line::from(vec![
            Span::styled(
                format!(" {marker} {label} "),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(state.mode.clone(), Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format!(" {} ", state.workspace),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("({branch}) "), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("[{marker} {label}] "), Style::default()),
            Span::styled(
                format!("{} ", state.mode),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(model.to_owned(), Style::default().fg(Color::DarkGray)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Style for a timeline entry kind.
fn entry_style(kind: TimelineKind) -> Style {
    match kind {
        TimelineKind::User => Style::default().fg(Color::Cyan),
        TimelineKind::Assistant => Style::default(),
        TimelineKind::Routing | TimelineKind::Decision => Style::default().fg(Color::DarkGray),
        TimelineKind::Action => Style::default().fg(Color::Blue),
        TimelineKind::Evidence => Style::default().fg(Color::Green),
        TimelineKind::Wait => Style::default().add_modifier(Modifier::BOLD),
        TimelineKind::Terminal => Style::default().fg(Color::Green),
        TimelineKind::Error => Style::default().fg(Color::Red),
    }
}

fn render_timeline(frame: &mut Frame, state: &WorkbenchState, area: Rect) {
    if area.height == 0 {
        return;
    }
    let inner_height = area.height.saturating_sub(2) as usize;
    let entries = state.visible_timeline();

    // Show a window of the newest entries; older ones are virtualized out
    // of the render path entirely.
    let start = entries.len().saturating_sub(inner_height);

    let mut lines: Vec<Line> = Vec::with_capacity(inner_height);
    for entry in &entries[start..] {
        let label = entry.kind.label();
        let style = entry_style(entry.kind);
        for (index, text_line) in entry.text.lines().enumerate() {
            if index == 0 {
                lines.push(Line::from(vec![
                    Span::styled(format!("{label:>6} "), style.fg(Color::DarkGray)),
                    Span::styled(text_line.to_owned(), style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::styled(text_line.to_owned(), style),
                ]));
            }
        }
    }

    let title = if state.timeline_offset > 0 {
        format!(" conversation ({} earlier) ", state.timeline_offset)
    } else {
        " conversation ".to_owned()
    };
    let title = if state.focus == Focus::Timeline {
        format!("{title}[focused] ")
    } else {
        title
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_inspector(frame: &mut Frame, state: &WorkbenchState, area: Rect) {
    if area.height == 0 {
        return;
    }
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "session",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(format!("  mode     {}", state.mode)));
    lines.push(Line::from(format!(
        "  model    {}",
        state.model.as_deref().unwrap_or("not configured")
    )));
    let (marker, label) = state_marker(state.task_state);
    lines.push(Line::from(format!("  state    {marker} {label}")));
    lines.push(Line::from(format!("  cwd      {}", state.workspace)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "decisions",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(format!("  turns    {}", state.stats.turns)));
    lines.push(Line::from(format!("  system1  {}", state.stats.decisions)));
    lines.push(Line::from(format!(
        "  nodes    {}",
        state.stats.node_results
    )));
    lines.push(Line::from(format!(
        "  deltas   {}",
        state.stats.text_deltas
    )));
    if state.stats.dropped > 0 {
        lines.push(Line::from(format!("  dropped  {}", state.stats.dropped)));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" inspector "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_tasks(frame: &mut Frame, state: &WorkbenchState, area: Rect) {
    if area.height == 0 {
        return;
    }
    let mut lines = Vec::new();
    if let Some(pending) = &state.pending {
        lines.push(Line::from(Span::styled(
            "waiting for you",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for line in pending.message.lines() {
            lines.push(Line::from(format!("  {line}")));
        }
        match &pending.kind {
            crate::session::WaitKind::Approval { .. } => {
                lines.push(Line::from("  [a] approve   [d] deny"));
            }
            crate::session::WaitKind::Question => {
                lines.push(Line::from("  answer in the composer and press Enter"));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "no pending work",
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" jobs "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_composer(frame: &mut Frame, state: &WorkbenchState, area: Rect) {
    if area.height == 0 {
        return;
    }
    let focused = state.focus == Focus::Composer;
    let style = if focused {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let lines: Vec<Line> = if focused {
        state
            .composer
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect()
    } else {
        state
            .composer
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect()
    };

    let hint = if state.pending.is_some() {
        " answer, or [a]pprove / [d]eny "
    } else if focused {
        " compose · Enter submits · Ctrl+J newline "
    } else {
        " press i or Tab to compose "
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(hint)
                    .border_style(style),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame, state: &WorkbenchState, area: Rect, tabbed: bool, tab: Tab) {
    if area.height == 0 {
        return;
    }
    let mut spans = vec![
        Span::styled(" ^C ", Style::default().fg(Color::DarkGray)),
        Span::raw("cancel/quit  "),
        Span::styled(" F1 ", Style::default().fg(Color::DarkGray)),
        Span::raw("help  "),
        Span::styled(" q ", Style::default().fg(Color::DarkGray)),
        Span::raw("quit  "),
    ];
    if tabbed {
        spans.push(Span::styled(" Tab ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw(format!("pane [{tab:?}]  ")));
    } else {
        spans.push(Span::styled(" Tab ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw("focus  "));
    }
    spans.push(Span::styled(
        " focus ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::raw(format!("{:?}  ", state.focus)));
    if let Some(status) = &state.status {
        spans.push(Span::raw("· "));
        spans.push(Span::styled(
            status.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    // A centered overlay, sized to the terminal and never larger than it.
    let width = area.width.saturating_sub(8).min(64);
    let height = area.height.saturating_sub(4).min(16);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let lines = [
        "keyboard",
        "",
        "  i / Tab     focus the composer",
        "  Enter       submit the composer",
        "  Ctrl+J      newline in the composer",
        "  a / d       approve / deny a pending action",
        "  c           cancel the running task",
        "  p / r       pause / resume",
        "  ↑ / ↓       move through the timeline",
        "  F1 / ?      toggle this help",
        "  Esc         close help",
        "  q           quit (restores the terminal)",
        "  Ctrl+C      quit (restores the terminal)",
        "",
        "mouse is optional; status is shown as text, not only color",
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .block(Block::default().borders(Borders::ALL).title(" help "))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionEvent, TaskId, TaskRevision, TurnId};
    use crate::tui_state::{MAX_TIMELINE, TimelineKind, WorkbenchState};
    use crate::{Action, DecisionSource, ModelTier, NodeStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Render one frame at a given size and return the visible text.
    fn snapshot(state: &WorkbenchState, width: u16, height: u16, tab: Tab) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state, tab)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn populated_state() -> WorkbenchState {
        let mut state = WorkbenchState::new("/home/user/日本語のプロジェクト");
        state.branch = Some("main".to_owned());
        state.model = Some("glm-5.3-flash".to_owned());
        state.apply(&SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "fix the failing test in src/lib.rs".to_owned(),
        });
        state.apply(&SessionEvent::Routed {
            task: TaskId(1),
            turn: TurnId(1),
            revision: TaskRevision(1),
            source: DecisionSource::SystemOne,
            action: Action::Tool {
                capability: "files".to_owned(),
            },
            confidence: 0.91,
        });
        state.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(1),
            node: crate::session::NodeId(1),
            node_label: "read".to_owned(),
            status: NodeStatus::Succeeded,
            output: serde_json::json!({ "path": "src/überlegungen.rs" }),
        });
        state.apply(&SessionEvent::FrameDecided {
            task: TaskId(1),
            turn: TurnId(1),
            revision: TaskRevision(1),
            question_kind: crate::FrameKind::Recovery,
            frame_version: 1,
            question_pack: vec!["failure_class".to_owned()],
            choice: "verification".to_owned(),
            confidence: 0.73,
            distribution: serde_json::json!({ "verification": 0.73 }),
            overridden: false,
        });
        state
    }

    #[test]
    fn renders_at_eighty_by_twenty_four() {
        let state = populated_state();
        let text = snapshot(&state, 80, 24, Tab::Timeline);
        // Wide (CJK) glyphs occupy two cells, so the rendered buffer holds
        // them with interleaved blanks: assert on the workspace path
        // prefix and the stable labels instead of the exact glyph run.
        assert!(text.contains("/home/user/"));
        assert!(text.contains("running"));
        assert!(text.contains("conversation"));
        // The composer and footer are present.
        assert!(text.contains("Enter submits"));
        assert!(text.contains("Ctrl+C") || text.contains("^C"));
    }

    #[test]
    fn narrow_windows_collapse_to_tabs_without_losing_content() {
        let state = populated_state();
        // 60x24: below NARROW_WIDTH, so panes become tabs.
        let timeline = snapshot(&state, 60, 24, Tab::Timeline);
        assert!(timeline.contains("conversation"));
        assert!(timeline.contains("fix the failing test"));

        let tasks = snapshot(&state, 60, 24, Tab::Tasks);
        assert!(tasks.contains("jobs"));

        // A very narrow terminal still renders something useful rather
        // than panicking or showing only decoration.
        let tiny = snapshot(&state, 30, 12, Tab::Timeline);
        assert!(!tiny.trim().is_empty());
    }

    #[test]
    fn large_layouts_show_the_inspector() {
        let state = populated_state();
        let text = snapshot(&state, 140, 40, Tab::Timeline);
        assert!(text.contains("inspector"));
        assert!(text.contains("glm-5.3-flash"));
        assert!(text.contains("system1"));
    }

    #[test]
    fn short_windows_drop_the_inspector_instead_of_cramping() {
        let _ = populated_state();
        let plan = plan_layout(Rect::new(0, 0, 120, 14), true);
        assert!(plan.inspector.is_none());
        // The composer and footer still get their rows.
        assert!(plan.composer.height >= 3);
        assert_eq!(plan.footer.height, 1);
    }

    #[test]
    fn resize_between_sizes_is_stable() {
        let state = populated_state();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, &state, Tab::Timeline))
            .unwrap();
        terminal.backend_mut().resize(140, 40);
        terminal
            .draw(|frame| render(frame, &state, Tab::Timeline))
            .unwrap();
        terminal.backend_mut().resize(40, 10);
        terminal
            .draw(|frame| render(frame, &state, Tab::Timeline))
            .unwrap();
        // No panic across sizes, and the buffer matches the last size.
        assert_eq!(terminal.backend().buffer().area.width, 40);
    }

    #[test]
    fn unicode_and_long_paths_render_without_panicking() {
        let mut state = WorkbenchState::new("/very/long/path/".to_owned() + &"nested/".repeat(30));
        state.apply(&SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "日本語のタスク ünïcödé 🎉".to_owned(),
        });
        for width in [20, 40, 80, 200] {
            let text = snapshot(&state, width, 24, Tab::Timeline);
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn long_histories_are_virtualized_in_the_render_path() {
        let mut state = populated_state();
        for i in 0..(MAX_TIMELINE * 2) {
            state.apply(&SessionEvent::NodeResult {
                task: TaskId(1),
                turn: TurnId(1),
                node: crate::session::NodeId(i as u64),
                node_label: format!("node-{i}"),
                status: NodeStatus::Succeeded,
                output: serde_json::Value::Null,
            });
        }
        // Only the newest window is drawn; rendering stays cheap.
        let started = std::time::Instant::now();
        let text = snapshot(&state, 100, 30, Tab::Timeline);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(text.contains("earlier"));
    }

    #[test]
    fn a_waiting_task_shows_the_prompt_and_its_keys() {
        let mut state = populated_state();
        state.apply(&SessionEvent::WaitingForUser {
            task: TaskId(1),
            turn: TurnId(2),
            wait: crate::session::WaitKind::Approval {
                approval_key: "fp".to_owned(),
            },
            message: "the write needs your approval".to_owned(),
        });
        // 120x30 is wide enough to split, so the jobs pane is reached
        // through the tabbed layout at a narrower width.
        let text = snapshot(&state, 70, 30, Tab::Tasks);
        assert!(text.contains("waiting for you"));
        assert!(text.contains("approve"));
        assert!(text.contains("deny"));

        // Status is textual, not color-only.
        let timeline = snapshot(&state, 70, 30, Tab::Timeline);
        assert!(timeline.contains("waiting"));
    }

    #[test]
    fn help_overlay_renders_inside_the_terminal() {
        let mut state = populated_state();
        state.help = true;
        for (width, height) in [(80, 24), (40, 12), (120, 40)] {
            let text = snapshot(&state, width, height, Tab::Timeline);
            assert!(text.contains("keyboard"));
        }
    }

    #[test]
    fn terminal_states_are_shown_as_text_markers() {
        for (state_value, marker) in [
            (crate::session::TaskState::Running, "running"),
            (crate::session::TaskState::Failed, "failed"),
            (crate::session::TaskState::Completed, "completed"),
        ] {
            let mut state = populated_state();
            state.task_state = Some(state_value);
            let text = snapshot(&state, 120, 30, Tab::Timeline);
            assert!(text.contains(marker), "missing {marker} in:\n{text}");
        }
    }

    #[test]
    fn timeline_entry_kinds_have_distinct_non_color_labels() {
        let labels: Vec<&str> = [
            TimelineKind::User,
            TimelineKind::Assistant,
            TimelineKind::Routing,
            TimelineKind::Decision,
            TimelineKind::Action,
            TimelineKind::Evidence,
            TimelineKind::Wait,
            TimelineKind::Terminal,
            TimelineKind::Error,
        ]
        .iter()
        .map(|kind| kind.label())
        .collect();
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "labels are not distinct");
    }

    /// Input-to-paint target from the roadmap: p95 <= 50 ms on a
    /// documented synthetic long session. This measures reducer + render
    /// only (no model, network or filesystem work), which is the part the
    /// shell owns.
    #[test]
    fn input_to_paint_stays_within_the_target_on_a_long_session() {
        // A documented synthetic fixture: a long session with mixed event
        // kinds, including streaming text.
        let mut state = WorkbenchState::new("/workspace/knut");
        for i in 0..1000 {
            state.apply(&SessionEvent::TaskStarted {
                task: TaskId(i),
                prompt: format!("task {i}: fix the failing test in src/module_{i}.rs"),
            });
            state.apply(&SessionEvent::Routed {
                task: TaskId(i),
                turn: TurnId(1),
                revision: TaskRevision(1),
                source: DecisionSource::SystemOne,
                action: Action::Generate(ModelTier::Reasoner),
                confidence: 0.8,
            });
            for chunk in ["working", " on", " the", " fix"] {
                state.apply(&SessionEvent::TextDelta {
                    task: TaskId(i),
                    turn: TurnId(1),
                    node: crate::session::NodeId(1),
                    text: chunk.to_owned(),
                });
            }
            state.apply(&SessionEvent::NodeResult {
                task: TaskId(i),
                turn: TurnId(1),
                node: crate::session::NodeId(1),
                node_label: format!("check_{i}"),
                status: NodeStatus::Succeeded,
                output: serde_json::json!({ "ok": true, "check": i }),
            });
        }

        let mut timings = Vec::new();
        for _ in 0..200 {
            // One keystroke, then a paint.
            let started = std::time::Instant::now();
            let mut local = state.clone();
            local.composer[0].push('x');
            let _ = snapshot(&local, 120, 40, Tab::Timeline);
            timings.push(started.elapsed());
        }
        timings.sort();
        let p95 = timings[(timings.len() as f64 * 0.95) as usize];
        assert!(
            p95 <= std::time::Duration::from_millis(50),
            "input-to-paint p95 was {p95:?}, over the 50 ms target"
        );
        eprintln!("input-to-paint p95: {p95:?} over {} samples", timings.len());
    }

    /// A slow provider must not block the UI: the reducer and render path
    /// never await anything.
    #[test]
    fn rendering_never_awaits_and_has_no_io() {
        // This is a compile-time property enforced by the pure signatures
        // (render takes &WorkbenchState and returns nothing), asserted
        // here by exercising the path with no runtime at all.
        let mut state = WorkbenchState::new("/workspace/knut");
        state.apply(&SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "x".to_owned(),
        });
        let text = snapshot(&state, 100, 30, Tab::Timeline);
        assert!(text.contains("x"));
    }
}
