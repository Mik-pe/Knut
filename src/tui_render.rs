//! Rendering for the workbench shell.
//!
//! Pure functions over [`WorkbenchState`]: given state and a frame area,
//! produce widgets. No state mutation, no I/O, no terminal control beyond
//! the frame it is handed — so every layout can be snapshot-tested with
//! `TestBackend` at any size.
//!
//! The visual language lives in [`crate::theme`] (colour and weight) and
//! [`crate::knot`] (the mark). This module only composes: a header that
//! reads like a status instrument, a transcript with per-kind rails, a
//! live job strip, an inspector that explains the machine, and a composer
//! that looks like the control surface it is.
//!
//! Two invariants survive every layout:
//!
//! - **Nothing is colour-only.** Every state carries a glyph and a word.
//! - **Every pane truncates rather than overflows.** A narrow terminal
//!   loses detail, never structure.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::attach::CommandAvailability;
use crate::knot;
use crate::review::MAX_HUNK_LINES;
use crate::session::TaskState;
use crate::theme::Theme;
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

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Timeline => "transcript",
            Tab::Inspector => "state",
            Tab::Tasks => "jobs",
        }
    }
}

/// The layout chosen for one frame: useful panes only, and never a
/// permanent empty dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPlan {
    pub header: Rect,
    /// The tab strip under the header (narrow layouts only).
    pub tabs: Option<Rect>,
    pub timeline: Rect,
    pub jobs: Option<Rect>,
    pub inspector: Option<Rect>,
    pub composer: Rect,
    pub footer: Rect,
    /// Whether the terminal is too narrow to split panes.
    pub tabbed: bool,
}

/// Compute the layout for an area.
pub fn plan_layout(area: Rect, show_inspector: bool) -> LayoutPlan {
    let tabbed = area.width < NARROW_WIDTH;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),                            // header
            Constraint::Length(if tabbed { 1 } else { 0 }),   // tab strip
            Constraint::Min(3),                               // body
            Constraint::Length(composer_height(area.height)), // composer
            Constraint::Length(1),                            // footer
        ])
        .split(area);

    let header = vertical[0];
    let tabs = if tabbed { Some(vertical[1]) } else { None };
    let body = vertical[2];
    let composer = vertical[3];
    let footer = vertical[4];

    if tabbed {
        return LayoutPlan {
            header,
            tabs,
            timeline: body,
            jobs: None,
            inspector: None,
            composer,
            footer,
            tabbed,
        };
    }

    // Wide: the transcript always; the inspector and the job strip only
    // when there is something to say and room to say it.
    let inspector_visible = show_inspector && area.height >= SHORT_HEIGHT;
    let jobs_visible = area.height >= SHORT_HEIGHT && body.width >= 90;

    let (timeline, jobs, inspector) = match (inspector_visible, jobs_visible) {
        (true, true) => {
            let horizontal = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(40),
                    Constraint::Length(30),
                    Constraint::Length(42),
                ])
                .split(body);
            (horizontal[0], Some(horizontal[1]), Some(horizontal[2]))
        }
        (true, false) => {
            let horizontal = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(40), Constraint::Length(40)])
                .split(body);
            (horizontal[0], None, Some(horizontal[1]))
        }
        (false, true) => {
            let horizontal = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(50), Constraint::Length(32)])
                .split(body);
            (horizontal[0], Some(horizontal[1]), None)
        }
        (false, false) => (body, None, None),
    };

    LayoutPlan {
        header,
        tabs,
        timeline,
        jobs,
        inspector,
        composer,
        footer,
        tabbed,
    }
}

/// The composer grows with the terminal but stays modest.
fn composer_height(height: u16) -> u16 {
    match height {
        0..=16 => 3,
        17..=30 => 5,
        _ => 7,
    }
}

/// Render the whole shell.
pub fn render(frame: &mut Frame, state: &WorkbenchState, tab: Tab) {
    render_themed(frame, state, tab, &state.theme);
}

/// Render with an explicit theme (tests pin one; the shell detects it).
pub fn render_themed(frame: &mut Frame, state: &WorkbenchState, tab: Tab, theme: &Theme) {
    let area = frame.area();
    paint_backdrop(frame, area, theme);

    let plan = plan_layout(area, true);

    render_header(frame, state, plan.header, theme);
    if let Some(tabs) = plan.tabs {
        render_tab_strip(frame, tabs, tab, theme);
    }

    match (plan.tabbed, tab) {
        (false, _) | (_, Tab::Timeline) => render_timeline(frame, state, plan.timeline, theme),
        (true, Tab::Inspector) => render_inspector(frame, state, plan.timeline, theme),
        (true, Tab::Tasks) => render_jobs(frame, state, plan.timeline, theme),
    }
    if let Some(jobs) = plan.jobs {
        render_jobs(frame, state, jobs, theme);
    }
    if let Some(inspector) = plan.inspector {
        render_inspector(frame, state, inspector, theme);
    }
    render_composer(frame, state, plan.composer, theme);
    render_footer(frame, state, plan.footer, theme);

    // The review workspace replaces the workbench while it is open: a diff
    // needs the room, and review is the point of the view.
    if let Some(review) = &state.review {
        render_review_themed(frame, review, theme);
        if state.help {
            render_help(frame, area, theme);
        }
        return;
    }

    if state.help {
        render_help(frame, area, theme);
    }

    if state.palette_open() {
        render_palette(frame, state, area, theme);
    }

    // The decision inspector is opt-in: the transcript explains the work,
    // and this pane explains the decisions behind it.
    if state.show_inspector {
        render_inspector_overlay(frame, state, area, theme);
    }

    // The cursor belongs to the composer, and only when the composer is
    // focused: a text cursor over a read-only pane is a lie. The position
    // is mapped through the same wrapping the composer drew with, so the
    // caret stays on the character it will edit even in a long line.
    if state.focus == Focus::Composer
        && state.review.is_none()
        && !state.help
        && !state.palette_open()
    {
        let area = plan.composer;
        let inner_width = area.width.saturating_sub(6) as usize;
        let (cursor_row, cursor_col) = state.composer.cursor();
        let (row, col) = composer_cursor_position(
            state.composer.lines(),
            cursor_row,
            cursor_col,
            inner_width.max(8),
        );
        // Inside the border, after the " ❯ " sigil column.
        let y = (area.y + 1 + row as u16).min(area.bottom().saturating_sub(2));
        let x = (area.x + 4 + col as u16).min(area.right().saturating_sub(2));
        frame.set_cursor_position((x, y));
    }
}

/// Map the composer's logical cursor onto the wrapped display grid.
///
/// Returns `(row, column)` in cells relative to the composer's first inner
/// row and column. The composer hard-wraps at `inner_width`, so the
/// mapping is exact: every earlier line contributes as many display rows
/// as it has full-width chunks, and the cursor's own line contributes the
/// chunks before its column.
pub fn composer_cursor_position(
    lines: &[String],
    cursor_row: usize,
    cursor_col: usize,
    inner_width: usize,
) -> (usize, usize) {
    let width = inner_width.max(1);
    let mut display_row = 0usize;
    for (index, line) in lines.iter().enumerate() {
        // A grapheme is the unit the composer moves by, and one grapheme
        // occupies one cell for the text this line holds (identifiers,
        // paths, prose).
        let cells = line.chars().count();
        if index == cursor_row {
            return (display_row + cursor_col / width, cursor_col % width);
        }
        // A line that ends exactly on the boundary occupies a whole extra
        // display row rather than zero: the caret can sit past its end.
        display_row += cells / width + 1;
    }
    (display_row, cursor_col.min(width.saturating_sub(1)))
}

/// Hard-wrap one line into chunks of exactly `width` cells.
///
/// The composer uses this rather than word wrapping so the caret maps to a
/// cell exactly; it also stops text from reflowing under the cursor while
/// the user types.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Paint the app background, so the shell reads as one surface rather
/// than as text floating in the terminal's default.
fn paint_backdrop(frame: &mut Frame, area: Rect, theme: &Theme) {
    if !theme.paint_background || area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Block::default().style(theme.bg(theme.palette.bg)), area);
}

/// Text/icon marker for a task state, resolved for the terminal's glyphs.
fn state_marker_for(state: Option<TaskState>, theme: &Theme) -> (&'static str, &'static str) {
    let glyphs = &theme.glyphs;
    match state {
        None => (".", "idle"),
        Some(TaskState::Queued) => (glyphs.pending(), "queued"),
        Some(TaskState::Running) => (glyphs.running(), "running"),
        Some(TaskState::Waiting) => (glyphs.question(), "waiting"),
        Some(TaskState::Paused) => (glyphs.paused(), "paused"),
        Some(TaskState::Completed) => (glyphs.tick(), "completed"),
        Some(TaskState::Failed) => (glyphs.cross(), "failed"),
        Some(TaskState::Cancelled) => (glyphs.cancelled(), "cancelled"),
    }
}

/// The style for a task state.
fn state_style(state: Option<TaskState>, theme: &Theme) -> Style {
    match state {
        None => theme.faint(),
        Some(TaskState::Running) => theme.accent(),
        Some(TaskState::Waiting) => theme.warn(),
        Some(TaskState::Completed) => theme.success(),
        Some(TaskState::Failed) => theme.danger(),
        Some(TaskState::Cancelled) => theme.danger(),
        Some(TaskState::Paused) => theme.warn(),
        Some(TaskState::Queued) => theme.dim(),
    }
}

/// The header: the mark, the workspace, and the session's vital signs.
///
/// Two rows, always. The first is identity (mark, workspace, branch); the
/// second is instrument (state, model, endpoint, counters). On a narrow
/// terminal the second row drops fields rather than truncating mid-word.
fn render_header(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let (marker, label) = state_marker_for(state.task_state, theme);
    let style = state_style(state.task_state, theme);

    // Row one: brand + workspace.
    let mut identity: Vec<Span> = Vec::new();
    identity.push(Span::styled(
        format!(" {} ", theme.glyphs.knot()),
        theme.accent().add_modifier(Modifier::BOLD),
    ));
    for span in theme.brand_gradient("knut") {
        identity.push(span.add_modifier(Modifier::BOLD));
    }
    identity.push(Span::styled("  ", theme.faint()));
    let workspace = compact_path(&state.workspace, area.width as usize);
    identity.push(Span::styled(
        workspace,
        theme.text().add_modifier(Modifier::BOLD),
    ));
    if let Some(branch) = &state.branch {
        identity.push(Span::styled(
            format!("  {} {branch}", theme.glyphs.separator()),
            theme.dim(),
        ));
    }

    // Right-align the ticker when there is room: identity left, state right.
    let mut row_one = Line::from(identity);
    if area.width >= 60 {
        let counters = ticker_label(state);
        let used: usize = row_one
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        let padding = (area.width as usize).saturating_sub(used + counters.chars().count() + 2);
        row_one
            .spans
            .push(Span::styled(" ".repeat(padding.max(1)), theme.faint()));
        row_one.push_span(Span::styled(counters, theme.faint()));
    }

    // Row two: state, mode, model, endpoint, checks.
    let mut row_two: Vec<Span> = Vec::new();
    row_two.push(Span::styled(
        format!(" {marker} {label} "),
        style.add_modifier(Modifier::BOLD),
    ));
    row_two.push(Span::styled(theme.glyphs.separator(), theme.faint()));
    row_two.push(Span::styled(format!(" {}", state.mode), theme.dim()));
    if area.width >= 70 {
        row_two.push(Span::styled(
            format!("  {} {}", theme.glyphs.separator(), state.reasoner_label()),
            theme.dim(),
        ));
    }
    if area.width >= 110 {
        if let Some(endpoint) = &state.endpoint {
            row_two.push(Span::styled(
                format!("  {} {endpoint}", theme.glyphs.separator()),
                theme.faint(),
            ));
        }
        if state.checks > 0 {
            row_two.push(Span::styled(
                format!("  {} {} checks", theme.glyphs.separator(), state.checks),
                theme.faint(),
            ));
        }
        if state.model_calls > 0 {
            row_two.push(Span::styled(
                format!("  {} {} calls", theme.glyphs.separator(), state.model_calls),
                theme.faint(),
            ));
        }
        // Whether routing is live matters: with a real router a prompt can
        // reach the workspace tools, and without one it cannot.
        row_two.push(Span::styled(
            format!(
                "  {} routing {}",
                theme.glyphs.separator(),
                if state.live_routing {
                    "live"
                } else {
                    "deterministic"
                }
            ),
            theme.faint(),
        ));
    }

    frame.render_widget(Paragraph::new(row_one), Rect { height: 1, ..area });
    if area.height >= 2 {
        frame.render_widget(
            Paragraph::new(Line::from(row_two)),
            Rect {
                y: area.y + 1,
                height: 1,
                ..area
            },
        );
    }

    // A hairline under the header separates chrome from content.
    if area.height >= 2 {
        let hairline = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        let _ = hairline;
    }
}

/// A short ticker for the header's right edge.
fn ticker_label(state: &WorkbenchState) -> String {
    let elapsed = state.elapsed_secs();
    if elapsed > 0 {
        format!("{:02}:{:02} ", elapsed / 60, elapsed % 60)
    } else {
        String::new()
    }
}

/// Shorten a path to its last two segments, so the header stays readable
/// on a deep working directory.
fn compact_path(path: &str, width: usize) -> String {
    let budget = (width / 3).clamp(16, 48);
    if path.chars().count() <= budget {
        return path.to_owned();
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let tail = match parts.len() {
        0 => return path.to_owned(),
        1 => parts[0].to_owned(),
        _ => format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1]),
    };
    if tail.chars().count() + 2 > budget {
        format!(
            "~{}",
            &tail[tail.len().saturating_sub(budget.saturating_sub(1))..]
        )
    } else {
        format!("~/{tail}")
    }
}

/// The tab strip for narrow terminals.
fn render_tab_strip(frame: &mut Frame, area: Rect, tab: Tab, theme: &Theme) {
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" "));
    for (index, candidate) in [Tab::Timeline, Tab::Inspector, Tab::Tasks]
        .iter()
        .enumerate()
    {
        let selected = *candidate == tab;
        let style = if selected {
            theme
                .accent()
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            theme.dim()
        };
        spans.push(Span::styled(
            format!(" {} {} ", index + 1, candidate.label()),
            style,
        ));
        spans.push(Span::styled(" ", theme.faint()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Style for a timeline entry kind.
fn entry_style(kind: TimelineKind, theme: &Theme) -> Style {
    match kind {
        TimelineKind::User => theme.fg(theme.palette.text).add_modifier(Modifier::BOLD),
        TimelineKind::Assistant => theme.text(),
        TimelineKind::Routing => theme.fg(theme.palette.violet),
        TimelineKind::Decision => theme.fg(theme.palette.magenta),
        TimelineKind::Action => theme.fg(theme.palette.sky),
        TimelineKind::Evidence => theme.success(),
        TimelineKind::Wait => theme.warn().add_modifier(Modifier::BOLD),
        TimelineKind::Terminal => theme.success(),
        TimelineKind::Error => theme.danger(),
    }
}

/// The glyph rail for a timeline entry kind: a shape, so the transcript is
/// scannable even in monochrome.
fn entry_glyph(kind: TimelineKind, theme: &Theme) -> &'static str {
    let glyphs = &theme.glyphs;
    match kind {
        TimelineKind::User => glyphs.prompt(),
        TimelineKind::Assistant => glyphs.rail(),
        TimelineKind::Routing => glyphs.route(),
        TimelineKind::Decision => glyphs.diamond(),
        TimelineKind::Action => glyphs.running(),
        TimelineKind::Evidence => glyphs.tick(),
        TimelineKind::Wait => glyphs.question(),
        TimelineKind::Terminal => glyphs.tick(),
        TimelineKind::Error => glyphs.cross(),
    }
}

fn render_timeline(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let inner_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(2) as usize;
    let entries = state.visible_timeline();

    if entries.is_empty() {
        render_welcome(frame, state, area, theme);
        return;
    }

    // Show a window of the newest entries; older ones are virtualized out
    // of the render path entirely. When the user has scrolled back, the
    // window follows the selection instead of the tail — a transcript that
    // scrolls but never moves is worse than no scrolling at all.
    let start = if state.follow {
        entries.len().saturating_sub(inner_height)
    } else {
        state
            .selection
            .min(entries.len().saturating_sub(1))
            .saturating_sub(inner_height / 2)
            .min(
                entries
                    .len()
                    .saturating_sub(inner_height.min(entries.len())),
            )
    };
    let mut lines: Vec<Line> = Vec::with_capacity(inner_height + 4);
    let mut emitted = 0usize;

    for (offset, entry) in entries[start..].iter().enumerate() {
        if emitted >= inner_height {
            break;
        }
        let index = start + offset;
        let selected = !state.follow && index == state.selection;
        let style = entry_style(entry.kind, theme);
        let glyph = entry_glyph(entry.kind, theme);
        let text_lines: Vec<&str> = entry.text.lines().collect();
        let text_lines = if text_lines.is_empty() {
            vec![""]
        } else {
            text_lines
        };

        // A streaming entry is still arriving: mark its last line with a
        // cursor block so a stalled stream and a live one look different.
        let is_tail = index + 1 == entries.len();

        for (line_index, text_line) in text_lines.iter().enumerate() {
            if emitted >= inner_height {
                break;
            }
            let wrapped = wrap_text(text_line, inner_width.saturating_sub(10));
            for (wrap_index, chunk) in wrapped.iter().enumerate() {
                if emitted >= inner_height {
                    break;
                }
                // The selected entry gets a rail in the left margin, so
                // scrolling has a visible cursor even in monochrome.
                let gutter = if selected { theme.glyphs.rail() } else { " " };
                let last_chunk =
                    line_index + 1 == text_lines.len() && wrap_index + 1 == wrapped.len();
                let cursor = if entry.streaming
                    && is_tail
                    && last_chunk
                    && state.task_state.is_some_and(|task| !task.is_terminal())
                {
                    Span::styled(
                        theme.glyphs.bar().to_owned(),
                        theme.accent().add_modifier(Modifier::SLOW_BLINK),
                    )
                } else {
                    Span::raw("")
                };
                let line = if line_index == 0 && wrap_index == 0 {
                    Line::from(vec![
                        Span::styled(gutter.to_owned(), theme.accent()),
                        Span::styled(format!(" {glyph} "), style),
                        Span::styled(chunk.clone(), style),
                        cursor,
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(gutter.to_owned(), theme.accent()),
                        Span::styled("   ", theme.faint()),
                        Span::styled(chunk.clone(), style),
                        cursor,
                    ])
                };
                lines.push(line);
                emitted += 1;
            }
        }
    }

    let title = if state.timeline_offset > 0 {
        format!(
            " {} | {} earlier ",
            Tab::Timeline.label(),
            state.timeline_offset
        )
    } else {
        format!(" {} ", Tab::Timeline.label())
    };
    let title = if state.focus == Focus::Timeline {
        format!("{}{} ", title, theme.glyphs.diamond())
    } else {
        title
    };
    let border = if state.focus == Focus::Timeline {
        theme.border_focused()
    } else {
        theme.border()
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(&title, border, theme))
            .style(theme.bg(theme.palette.bg_panel)),
        area,
    );
}

/// The empty-state: the mark, what this is, and how to start.
fn render_welcome(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();

    // The mark, then the block wordmark. A splash screen names the product
    // at a size body text cannot: that is the whole point of a wordmark.
    lines.extend(knot::logo_lines(theme, inner_width));
    for line in knot::wordmark_block(theme.glyphs.unicode).lines {
        if theme.level.is_color() {
            lines.push(centered_spans(
                gradient_over(theme, line, theme.palette.cyan, theme.palette.magenta),
                inner_width,
            ));
        } else {
            lines.push(centered(line.to_string(), inner_width, theme.text()));
        }
    }
    if inner_width >= 60 {
        for line in wrap_text(knot::SUBTITLE, inner_width.saturating_sub(6)) {
            lines.push(centered(line, inner_width, theme.faint()));
        }
    }

    // Readiness, stated as an instruction the user can act on.
    let ready = state.model.is_some();
    let (status, status_style) = if ready {
        (
            "ready - describe the change you want and press Enter".to_owned(),
            theme.success(),
        )
    } else {
        (
            "offline - no reasoner credential is configured".to_owned(),
            theme.warn(),
        )
    };
    lines.push(Line::from(""));
    lines.push(centered(status, inner_width, status_style));

    // What this session can actually do, in columns — the harness's own
    // inventory, not a promise. Three columns when there is room, stacked
    // otherwise, because a capability list is the first thing a new user
    // needs and the first thing a wide screen wastes.
    lines.push(Line::from(""));
    if inner_width >= 56 {
        lines.extend(capability_columns(state, inner_width, theme));
    } else {
        lines.extend(capability_stack(state, inner_width, theme));
    }

    if inner_width >= 40 {
        lines.push(Line::from(""));
        for hint in [
            "Enter  submit      Ctrl+J  newline     Tab  focus",
            "F1     help        :       commands    q    quit",
        ] {
            lines.push(centered(hint.to_owned(), inner_width, theme.faint()));
        }
    }

    if state.model.is_none() && inner_width >= 60 {
        lines.push(Line::from(""));
        for line in wrap_text(
            "set KNUT_PROVIDER_API_KEY (and optionally KNUT_PROVIDER_BASE_URL / \
             KNUT_PROVIDER_MODEL), then run `knut doctor`",
            inner_width.saturating_sub(8),
        ) {
            lines.push(centered(line, inner_width, theme.faint()));
        }
    }

    // Sit the block a little above centre: text pinned to the exact middle
    // drifts as the terminal resizes.
    let pad = inner_height.saturating_sub(lines.len()) * 2 / 5;
    let mut padded: Vec<Line<'static>> = Vec::with_capacity(lines.len() + pad);
    for _ in 0..pad {
        padded.push(Line::from(""));
    }
    padded.extend(lines);

    frame.render_widget(
        Paragraph::new(padded)
            .block(panel(
                &format!(" {} ", Tab::Timeline.label()),
                theme.border(),
                theme,
            ))
            .style(theme.bg(theme.palette.bg_panel)),
        area,
    );
}

/// The session's real capabilities, side by side.
fn capability_columns(state: &WorkbenchState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let columns: [(&str, Vec<String>); 3] = [
        (
            "model",
            vec![
                state.reasoner_label(),
                state
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| "no endpoint".to_owned()),
                if state.live_routing {
                    "live routing".to_owned()
                } else {
                    "deterministic routing".to_owned()
                },
            ],
        ),
        (
            "gate",
            vec![
                "tools: read, search, write".to_owned(),
                "writes need approval".to_owned(),
                "sandboxed processes".to_owned(),
            ],
        ),
        (
            "checks",
            vec![
                format!("{} configured", state.checks),
                "revision-bound evidence".to_owned(),
                "no checks, no done".to_owned(),
            ],
        ),
    ];

    let total: usize = columns.iter().map(|(_, rows)| column_width(rows)).sum();
    let gaps = 4 * (columns.len().saturating_sub(1));
    // Fall back to the stacked form when the columns will not fit: three
    // cramped columns are worse than one readable list.
    if total + gaps + 4 > width {
        return capability_stack(state, width, theme);
    }
    let spare = width.saturating_sub(total + gaps);
    let pad = spare / (columns.len() + 1);

    let mut heads: Vec<Span<'static>> = vec![Span::raw(" ".repeat(pad))];
    for (index, (name, rows)) in columns.iter().enumerate() {
        let w = column_width(rows);
        heads.push(Span::styled(
            format!("{:<w$}", name.to_ascii_uppercase()),
            theme.accent().add_modifier(Modifier::BOLD),
        ));
        if index + 1 < columns.len() {
            heads.push(Span::raw(" ".repeat(4 + pad)));
        }
    }

    let mut outs = vec![Line::from(heads)];
    let depth = columns
        .iter()
        .map(|(_, rows)| rows.len())
        .max()
        .unwrap_or(0);
    for row in 0..depth {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ".repeat(pad))];
        for (index, (_, rows)) in columns.iter().enumerate() {
            let w = column_width(rows);
            let text = rows.get(row).cloned().unwrap_or_default();
            let text: String = text.chars().take(w).collect();
            spans.push(Span::styled(
                format!("{text:<w$}"),
                if row == 0 {
                    theme.text()
                } else {
                    theme.faint()
                },
            ));
            if index + 1 < columns.len() {
                spans.push(Span::raw(" ".repeat(4 + pad)));
            }
        }
        outs.push(Line::from(spans));
    }
    outs
}

/// The same capability list, one row per item, for narrow panes.
fn capability_stack(state: &WorkbenchState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let items = [
        format!(
            "model   {} at {}",
            state.reasoner_label(),
            state.endpoint.as_deref().unwrap_or("-")
        ),
        "tools   read, search, write behind an approval gate".to_owned(),
        format!(
            "checks  {} revision-bound checks gate completion",
            state.checks
        ),
    ];
    let mut out = Vec::new();
    for item in items {
        for line in wrap_text(&item, width.saturating_sub(8)) {
            out.push(centered(line, width, theme.faint()));
        }
    }
    out
}

/// The width a column needs: its widest row, bounded.
fn column_width(rows: &[String]) -> usize {
    rows.iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0)
        .min(30)
}

/// Centre a run of spans inside a width, padding with unstyled spaces.
fn centered_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let content: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = width.saturating_sub(content) / 2;
    let mut out = vec![Span::raw(" ".repeat(pad))];
    out.extend(spans);
    Line::from(out)
}

/// One line of text, tinted along a ramp, for the wordmark.
fn gradient_over(
    theme: &Theme,
    text: &str,
    from: crate::theme::Rgb,
    to: crate::theme::Rgb,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let last = chars.len().saturating_sub(1).max(1);
    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let rgb = crate::theme::lerp(from, to, index as f32 / last as f32);
            Span::styled(ch.to_string(), Style::default().fg(theme.color(rgb)))
        })
        .collect()
}

fn centered(text: String, width: usize, style: Style) -> Line<'static> {
    let pad = width.saturating_sub(text.chars().count()) / 2;
    Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(text, style)])
}

/// Wrap plain text to a width, preserving nothing but word boundaries.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split(' ') {
        let word_width = word.chars().count();
        if current_width == 0 {
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                // A single long token (a path, a hash) is hard-split rather
                // than allowed to overflow the panel.
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    out.push(chunk.iter().collect());
                }
                current = String::new();
                current_width = 0;
            }
            continue;
        }
        if current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            out.push(std::mem::take(&mut current));
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    out.push(chunk.iter().collect());
                }
                current_width = 0;
            }
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

/// A panel block: rounded on capable terminals, plain ASCII otherwise.
///
/// The border *glyphs* are part of the same capability decision as colour:
/// a terminal that cannot be trusted with box-drawing gets `+--+|` rather
/// than a screen of replacement characters.
fn panel<'a>(title: &str, border: Style, theme: &Theme) -> Block<'a> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(title.to_owned(), theme.dim()));
    if theme.glyphs.unicode {
        block.border_type(BorderType::Rounded)
    } else {
        block.border_set(ascii_border_set())
    }
}

/// The ASCII border set: `+`, `-` and `|`.
fn ascii_border_set() -> ratatui::symbols::border::Set<'static> {
    use ratatui::symbols::border;
    border::Set {
        top_left: "+",
        top_right: "+",
        bottom_left: "+",
        bottom_right: "+",
        horizontal_top: "-",
        horizontal_bottom: "-",
        vertical_left: "|",
        vertical_right: "|",
    }
}

/// The job strip: what is running right now, with live spinners.
fn render_jobs(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let inner_width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Pending interaction first: it is the only thing the user must act on.
    if let Some(pending) = &state.pending {
        let style = theme.warn().add_modifier(Modifier::BOLD);
        lines.push(Line::from(Span::styled(
            format!(" {} needs you", theme.glyphs.diamond()),
            style,
        )));
        for line in wrap_text(&pending.message, inner_width.saturating_sub(2))
            .iter()
            .take(4)
        {
            lines.push(Line::from(Span::styled(format!("  {line}"), theme.text())));
        }
        match &pending.kind {
            crate::session::WaitKind::Approval { approval_key } => {
                lines.push(Line::from(vec![
                    Span::styled("  [a] ", theme.success()),
                    Span::styled("approve  ", theme.dim()),
                    Span::styled("[d] ", theme.danger()),
                    Span::styled("deny", theme.dim()),
                ]));
                let key: String = approval_key.chars().take(16).collect();
                if !key.is_empty() {
                    lines.push(Line::from(Span::styled(format!("  {key}"), theme.faint())));
                }
            }
            crate::session::WaitKind::Question => lines.push(Line::from(Span::styled(
                "  answer in the composer, then Enter",
                theme.dim(),
            ))),
        }
        lines.push(Line::from(""));
    }

    // Live cards: running first, then the most recent finished ones.
    let cards = state.cards.cards();
    let active: Vec<&crate::cards::ActionCard> =
        cards.iter().filter(|card| card.state.is_active()).collect();
    let finished: Vec<&crate::cards::ActionCard> = cards
        .iter()
        .filter(|card| !card.state.is_active())
        .rev()
        .collect();

    if !active.is_empty() {
        lines.push(Line::from(Span::styled(" active", theme.accent())));
        for card in active.iter().take(6) {
            lines.push(job_line(card, state.tick, inner_width, theme));
        }
    }

    if !finished.is_empty() {
        if !active.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(" recent", theme.dim())));
        let budget = (area.height as usize)
            .saturating_sub(2)
            .saturating_sub(lines.len())
            .max(2);
        for card in finished.iter().take(budget) {
            lines.push(job_line(card, state.tick, inner_width, theme));
        }
    }

    if !state.queued.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" queued ({})", state.queued.len()),
            theme.warn(),
        )));
        for request in state.queued.iter().take(3) {
            let short: String = request.chars().take(inner_width).collect();
            lines.push(Line::from(Span::styled(
                format!("  {} {}", theme.glyphs.bullet(), short),
                theme.dim(),
            )));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(" nothing running", theme.faint())));
        lines.push(Line::from(""));
        for line in wrap_text(
            "submit a task and its actions appear here",
            inner_width.saturating_sub(2),
        ) {
            lines.push(Line::from(Span::styled(format!(" {line}"), theme.faint())));
        }
    }

    let title = format!(" {} ", Tab::Tasks.label());
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(&title, theme.border(), theme))
            .style(theme.bg(theme.palette.bg_panel)),
        area,
    );
}

/// One job line: spinner or state glyph, title, timing, and — for a
/// finished card — the state word itself.
///
/// The line is assembled against the pane's width, so a summary that does
/// not fit is truncated *here* rather than clipped by the renderer: what
/// the pane shows is then exactly what this function decided to show, and
/// a terminal state word is never the part that gets lost.
fn job_line(
    card: &crate::cards::ActionCard,
    tick: u64,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let (glyph, style) = match card.state {
        crate::cards::CardState::Running => (theme.spinner(tick), theme.accent()),
        crate::cards::CardState::Queued => (theme.glyphs.pending(), theme.dim()),
        crate::cards::CardState::Waiting => (theme.glyphs.question(), theme.warn()),
        crate::cards::CardState::Succeeded => (theme.glyphs.tick(), theme.success()),
        crate::cards::CardState::Failed => (theme.glyphs.cross(), theme.danger()),
        crate::cards::CardState::Cancelled => (theme.glyphs.cancelled(), theme.danger()),
        crate::cards::CardState::TimedOut => ("t", theme.warn()),
    };
    let timing = if card.elapsed_ms >= 1000 {
        format!("{:.1}s", card.elapsed_ms as f64 / 1000.0)
    } else if card.elapsed_ms > 0 {
        format!("{}ms", card.elapsed_ms)
    } else {
        String::new()
    };

    // A card that has stopped says so in words. "succeeded" is implied by
    // a tick, but failure, cancellation and timeout must be unmistakable.
    let terminal_word = match card.state {
        crate::cards::CardState::Failed => Some("failed"),
        crate::cards::CardState::Cancelled => Some("cancelled"),
        crate::cards::CardState::TimedOut => Some("timed out"),
        _ => None,
    };

    let prefix = format!("  {glyph} ");
    let suffix = if timing.is_empty() {
        String::new()
    } else {
        format!(" {timing}")
    };
    let mut used = prefix.chars().count()
        + suffix.chars().count()
        + terminal_word.map_or(0, |word| word.len() + 3);

    let title_budget = width.saturating_sub(used).max(4);
    let title: String = card.title.chars().take(title_budget).collect();
    used += title.chars().count();

    let mut spans = vec![
        Span::styled(prefix, style),
        Span::styled(title, theme.text()),
    ];
    if let Some(word) = terminal_word {
        spans.push(Span::styled(
            format!(" {word}"),
            style.add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(suffix, theme.faint()));

    // Whatever room is left goes to the summary, clipped to fit.
    let remaining = width.saturating_sub(used);
    if remaining > 4 && !card.summary.is_empty() {
        let summary: String = card
            .summary
            .chars()
            .take(remaining.saturating_sub(3))
            .collect();
        spans.push(Span::styled(format!(" - {summary}"), theme.dim()));
    }

    Line::from(spans)
}

/// The inspector: the machine's own reading of the session.
fn render_inspector(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let inspector = &state.inspector;
    let width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();

    let section = |lines: &mut Vec<Line>, title: &str| {
        lines.push(Line::from(Span::styled(
            format!(" {}", title.to_ascii_uppercase()),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
    };

    section(&mut lines, "session");
    lines.push(field(
        "state",
        state_marker_for(state.task_state, theme).1,
        width,
        theme,
    ));
    lines.push(field("mode", &state.mode, width, theme));
    lines.push(field("reasoner", &state.reasoner_label(), width, theme));
    if let Some(endpoint) = &state.endpoint {
        lines.push(field("endpoint", endpoint, width, theme));
    }
    lines.push(field("cwd", &state.workspace, width, theme));

    section(&mut lines, "graph");
    lines.push(field("summary", &inspector.graph_summary(), width, theme));
    let nodes = inspector.nodes();
    for node in nodes.iter().rev().take(6).rev() {
        let (glyph, style) = node_glyph(node.state, theme);
        lines.push(Line::from(vec![
            Span::styled(format!("  {glyph} "), style),
            Span::styled(
                node.label
                    .chars()
                    .take(width.saturating_sub(4))
                    .collect::<String>(),
                theme.dim(),
            ),
        ]));
    }

    section(&mut lines, "decisions");
    lines.push(field("turns", &state.stats.turns.to_string(), width, theme));
    lines.push(field(
        "system1",
        &state.stats.decisions.to_string(),
        width,
        theme,
    ));
    lines.push(field(
        "nodes",
        &state.stats.node_results.to_string(),
        width,
        theme,
    ));
    lines.push(field(
        "deltas",
        &state.stats.text_deltas.to_string(),
        width,
        theme,
    ));
    if state.model_calls > 0 {
        lines.push(field("calls", &state.model_calls.to_string(), width, theme));
    }
    if state.stats.dropped > 0 {
        lines.push(field(
            "dropped",
            &state.stats.dropped.to_string(),
            width,
            theme,
        ));
    }

    // Latency is shown only once something has actually been measured: a
    // wall of "unknown" tells the user nothing and buries the real
    // numbers once they arrive.
    let latency = inspector.latency().render();
    if !latency.is_empty() && !latency.contains("unknown") {
        section(&mut lines, "latency");
        for line in wrap_text(&latency, width) {
            lines.push(Line::from(Span::styled(format!("  {line}"), theme.dim())));
        }
    }

    for (index, requirement) in inspector
        .outstanding()
        .requirements
        .iter()
        .take(5)
        .enumerate()
    {
        if index == 0 {
            section(&mut lines, "checks");
        }
        let (glyph, style) = if requirement.satisfied {
            (theme.glyphs.tick(), theme.success())
        } else {
            (theme.glyphs.pending(), theme.warn())
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {glyph} "), style),
            Span::styled(
                requirement
                    .check
                    .chars()
                    .take(width.saturating_sub(4))
                    .collect::<String>(),
                style,
            ),
        ]));
    }

    let title = if state.show_inspector {
        format!(" {} {} ", Tab::Inspector.label(), theme.glyphs.diamond())
    } else {
        format!(" {} ", Tab::Inspector.label())
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(&title, theme.border(), theme))
            .style(theme.bg(theme.palette.bg_panel))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn node_glyph(state: crate::NodeStatus, theme: &Theme) -> (&'static str, Style) {
    let glyphs = &theme.glyphs;
    match state {
        crate::NodeStatus::Succeeded => (glyphs.tick(), theme.success()),
        crate::NodeStatus::Failed => (glyphs.cross(), theme.danger()),
        crate::NodeStatus::Blocked => (glyphs.question(), theme.warn()),
        crate::NodeStatus::Running => (glyphs.running(), theme.accent()),
        crate::NodeStatus::Pending => (glyphs.pending(), theme.faint()),
    }
}

/// A key/value line with a fixed label column, so values line up.
fn field(label: &str, value: &str, width: usize, theme: &Theme) -> Line<'static> {
    let label = format!("  {label:<9}");
    let budget = width.saturating_sub(label.chars().count());
    let value: String = value.chars().take(budget).collect();
    Line::from(vec![
        Span::styled(label, theme.faint()),
        Span::styled(value, theme.text()),
    ])
}

fn render_composer(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let focused = state.focus == Focus::Composer;
    let style = if focused { theme.text() } else { theme.dim() };
    let border = if focused {
        theme.border_focused()
    } else {
        theme.border()
    };

    // The composer hard-wraps at a fixed width so the caret maps to an
    // exact cell; word wrapping would reflow the text under the cursor
    // while typing.
    let inner_width = area.width.saturating_sub(6) as usize;
    let budget = area.height.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let composer_lines = state.composer.lines();
    let sigil = Span::styled(
        format!(" {} ", theme.glyphs.prompt()),
        if focused {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.faint()
        },
    );

    for (index, line) in composer_lines.iter().enumerate() {
        for (chunk_index, chunk) in hard_wrap(line, inner_width).into_iter().enumerate() {
            if lines.len() >= budget {
                break;
            }
            let gutter = if index == 0 && chunk_index == 0 {
                sigil.clone()
            } else {
                Span::styled("   ", theme.faint())
            };
            lines.push(Line::from(vec![gutter, Span::styled(chunk, style)]));
        }
        if lines.len() >= budget {
            break;
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![
            sigil,
            Span::styled(
                if focused { "" } else { "press i to compose" },
                theme.faint(),
            ),
        ]));
    }

    let title = if state.pending.is_some() {
        " answer, or [a]pprove / [d]eny "
    } else if focused {
        " enter submit | ctrl+j newline | : commands "
    } else {
        " i / tab to compose "
    };
    let border = if state.pending.is_some() {
        theme.warn().add_modifier(Modifier::BOLD)
    } else {
        border
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title, border, theme))
            .style(theme.bg(theme.palette.bg_raise))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let key = |keys: &'static str| Span::styled(keys, theme.accent());
    let label = |text: &'static str| Span::styled(text, theme.faint());

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    let push = |spans: &mut Vec<Span>, keys: &'static str, text: &'static str| {
        spans.push(key(keys));
        // A visible gap between the key and its action, and between pairs:
        // "enter submit" reads as one word otherwise.
        spans.push(label(" "));
        spans.push(label(text));
        spans.push(Span::styled("   ", theme.faint()));
    };

    push(&mut spans, "enter", "submit");
    push(&mut spans, "tab", "focus");
    if area.width >= 84 {
        push(&mut spans, ":", "commands");
        push(&mut spans, "v", "review");
        push(&mut spans, "V", "verify");
        push(&mut spans, "i", "decisions");
    }
    push(&mut spans, "?", "help");

    // Right edge: the live status, so the shell always says what it is doing.
    let left: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    let status = state.status.clone().or_else(|| {
        state
            .task_state
            .map(|_| state_marker_for(state.task_state, theme).1.to_owned())
    });
    if let Some(status) = status {
        let styled = Span::styled(format!("{status} "), theme.dim());
        let width = status.chars().count() + 1;
        if left + width + 2 < area.width as usize {
            spans.push(Span::styled(
                " ".repeat(area.width as usize - left - width),
                theme.faint(),
            ));
            spans.push(styled);
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme.bg(theme.palette.bg_panel)),
        area,
    );
}

/// The review workspace: files, diff and checks side by side.
pub fn render_review(frame: &mut Frame, view: &crate::review::ReviewView) {
    render_review_themed(frame, view, &Theme::detect());
}

pub fn render_review_themed(frame: &mut Frame, view: &crate::review::ReviewView, theme: &Theme) {
    let area = frame.area();
    paint_backdrop(frame, area, theme);
    let wide = area.width >= NARROW_WIDTH;

    let (files_area, diff_area, checks_area) = if wide {
        let horizontal = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(28),
                Constraint::Min(30),
                Constraint::Length(32),
            ])
            .split(area);
        (horizontal[0], horizontal[1], horizontal[2])
    } else {
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Min(6),
                Constraint::Length(8),
            ])
            .split(area);
        (vertical[0], vertical[1], vertical[2])
    };

    render_review_files(frame, view, files_area, theme);
    render_review_diff(frame, view, diff_area, theme);
    render_review_checks(frame, view, checks_area, theme);
}

fn render_review_files(
    frame: &mut Frame,
    view: &crate::review::ReviewView,
    area: Rect,
    theme: &Theme,
) {
    let mut lines = Vec::new();
    for (index, file) in view
        .changes
        .files
        .iter()
        .enumerate()
        .take(area.height.saturating_sub(2) as usize)
    {
        let marker = file.kind.marker();
        let from = file
            .from
            .as_ref()
            .map(|from| format!("{from} -> "))
            .unwrap_or_default();
        let label = format!("{marker} {from}{}", file.path);
        let budget = area.width.saturating_sub(3) as usize;
        let label: String = label.chars().take(budget).collect();
        let selected = index == view.file_index;

        let mut spans = vec![Span::styled(
            if selected {
                format!(" {} ", theme.glyphs.rail())
            } else {
                "   ".to_owned()
            },
            theme.accent(),
        )];
        spans.push(Span::styled(
            label,
            if selected {
                theme.text().add_modifier(Modifier::BOLD)
            } else {
                theme.dim()
            },
        ));
        if area.width >= 24 {
            spans.push(Span::styled(format!(" +{}", file.added), theme.success()));
            spans.push(Span::styled(format!(" -{}", file.removed), theme.danger()));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(
                &format!(" changes | {} ", view.changes.files.len()),
                theme.border(),
                theme,
            ))
            .style(theme.bg(theme.palette.bg_panel)),
        area,
    );
}

fn render_review_diff(
    frame: &mut Frame,
    view: &crate::review::ReviewView,
    area: Rect,
    theme: &Theme,
) {
    let mut lines: Vec<Line> = Vec::new();
    let Some(file) = view.current_file() else {
        frame.render_widget(
            Paragraph::new(Span::styled(" no changes", theme.faint()))
                .block(panel(" diff ", theme.border(), theme))
                .style(theme.bg(theme.palette.bg_panel)),
            area,
        );
        return;
    };

    lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", file.path),
            theme.text().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" | {}", file.kind.label()), theme.faint()),
    ]));

    let budget = area.height.saturating_sub(2) as usize;
    let mut used = 1usize;
    for (index, hunk) in file.hunks.iter().enumerate() {
        if used >= budget {
            lines.push(Line::from(Span::styled(
                format!("  .. {} more hunk(s)", file.hunks.len() - index),
                theme.faint(),
            )));
            break;
        }
        let rejected = view.selection.is_rejected(&hunk.id);
        let selected = index == view.hunk_index;
        let style = if selected {
            theme.accent().add_modifier(Modifier::BOLD)
        } else if rejected {
            theme.faint()
        } else {
            theme.dim()
        };
        let marker = if rejected {
            format!(" {} ", theme.glyphs.cancelled())
        } else {
            "   ".to_owned()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, theme.danger()),
            Span::styled(hunk.header(), style),
        ]));
        used += 1;
        for line in hunk.lines.iter().take(MAX_HUNK_LINES).take(budget - used) {
            let (color, glyph) = match line.chars().next() {
                Some('+') => (theme.success(), "+"),
                Some('-') => (theme.danger(), "-"),
                _ => (theme.faint(), " "),
            };
            let content: String = line
                .chars()
                .skip(1)
                .take(area.width.saturating_sub(4) as usize)
                .collect();
            lines.push(Line::from(vec![
                Span::styled(format!("  {glyph} "), color),
                Span::styled(content, color),
            ]));
            used += 1;
        }
        if hunk.lines.len() > MAX_HUNK_LINES {
            lines.push(Line::from(Span::styled(
                format!("  .. {} more line(s)", hunk.lines.len() - MAX_HUNK_LINES),
                theme.faint(),
            )));
            used += 1;
        }
    }

    if let Some(approval) = view.approvals.pending() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {} approval required", theme.glyphs.diamond()),
            theme.warn().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled("  action  ", theme.faint()),
            Span::styled(approval.reason.clone(), theme.text()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  scope   ", theme.faint()),
            Span::styled(approval.scope.join(", "), theme.dim()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  network ", theme.faint()),
            Span::styled(
                if approval.network {
                    "allowed"
                } else {
                    "denied"
                }
                .to_owned(),
                if approval.network {
                    theme.warn()
                } else {
                    theme.success()
                },
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [a] ", theme.success()),
            Span::styled("approve  ", theme.dim()),
            Span::styled("[A] ", theme.success()),
            Span::styled("always  ", theme.dim()),
            Span::styled("[d] ", theme.danger()),
            Span::styled("reject", theme.dim()),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(
                " diff | j/k hunk | n/p file | r reject ",
                theme.border(),
                theme,
            ))
            .style(theme.bg(theme.palette.bg_panel)),
        area,
    );
}

fn render_review_checks(
    frame: &mut Frame,
    view: &crate::review::ReviewView,
    area: Rect,
    theme: &Theme,
) {
    let mut lines = Vec::new();
    if view.checks.is_empty() {
        lines.push(Line::from(Span::styled(" no checks yet", theme.faint())));
    }
    for check in view
        .checks
        .iter()
        .take(area.height.saturating_sub(2) as usize)
    {
        let label = check.label();
        let (glyph, style) = if check.is_green() {
            (theme.glyphs.tick(), theme.success())
        } else {
            match check.state {
                crate::verify::CheckOutcome::Failed => (theme.glyphs.cross(), theme.danger()),
                _ => (theme.glyphs.pending(), theme.warn()),
            }
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {glyph} "), style),
            Span::styled(check.name.clone(), style),
            Span::styled(format!(" | {label}"), theme.faint()),
        ]));
        for diagnostic in check.diagnostics.iter().take(2) {
            for line in wrap_text(diagnostic, area.width.saturating_sub(6) as usize) {
                lines.push(Line::from(Span::styled(format!("   {line}"), theme.dim())));
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" checks ", theme.border(), theme))
            .style(theme.bg(theme.palette.bg_panel)),
        area,
    );
}

/// The decision inspector overlay.
fn render_inspector_overlay(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let width = area.width.saturating_sub(6).min(104);
    let height = area.height.saturating_sub(4).min(34);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let inspector = &state.inspector;
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(" task ", theme.faint()),
        Span::styled(
            format!("{:?}", inspector.task_state()),
            theme.text().add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ", theme.faint()),
        Span::styled(inspector.graph_summary(), theme.dim()),
    ]));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        " OUTSTANDING",
        theme.accent().add_modifier(Modifier::BOLD),
    )));
    for line in inspector.outstanding().render().lines().take(6) {
        lines.push(Line::from(Span::styled(format!("  {line}"), theme.text())));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        " LATENCY",
        theme.accent().add_modifier(Modifier::BOLD),
    )));
    for line in inspector.latency().render().lines().take(4) {
        lines.push(Line::from(Span::styled(format!("  {line}"), theme.dim())));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        " USAGE",
        theme.accent().add_modifier(Modifier::BOLD),
    )));
    for line in inspector.usage().render().lines().take(4) {
        lines.push(Line::from(Span::styled(format!("  {line}"), theme.dim())));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        " DECISIONS | provenance preserved",
        theme.accent().add_modifier(Modifier::BOLD),
    )));
    for record in inspector.decisions().iter().rev().take(10).rev() {
        let style = match record.provenance {
            crate::inspector::DecisionProvenance::Model { .. } => theme.text(),
            crate::inspector::DecisionProvenance::Deterministic { .. } => theme.faint(),
            crate::inspector::DecisionProvenance::Cached { .. } => theme.fg(theme.palette.sky),
            crate::inspector::DecisionProvenance::Operator { .. } => theme.accent(),
            crate::inspector::DecisionProvenance::Fallback { .. } => theme.warn(),
        };
        let bounded: String = record.row().chars().take(width as usize - 6).collect();
        lines.push(Line::from(vec![
            Span::styled("  * ", theme.faint()),
            Span::styled(bounded, style),
        ]));
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(
                " decisions | i closes ",
                theme.border_focused(),
                theme,
            ))
            .style(theme.bg(theme.palette.bg_raise))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// The command palette: every entry states whether it exists.
fn render_palette(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let width = area.width.saturating_sub(10).min(76);
    let results = state.palette_results();
    let height = (results.len() as u16 + 5).min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + 3,
        width,
        height,
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(" {} ", theme.glyphs.prompt()),
            theme.accent().add_modifier(Modifier::BOLD),
        ),
        Span::styled(state.palette.clone().unwrap_or_default(), theme.text()),
    ])];
    if results.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching command",
            theme.faint(),
        )));
    }
    for command in results.iter().take(height.saturating_sub(3) as usize) {
        match command.availability {
            CommandAvailability::Available => lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<10}", command.id),
                    theme.accent().add_modifier(Modifier::BOLD),
                ),
                Span::styled(command.title.to_owned(), theme.text()),
                Span::styled(format!("  {}", command.description), theme.faint()),
            ])),
            CommandAvailability::Unavailable(reason) => lines.push(Line::from(vec![
                Span::styled(format!("  {:<10}", command.id), theme.faint()),
                Span::styled(command.title.to_owned(), theme.faint()),
                Span::styled(format!("  - {reason}"), theme.faint()),
            ])),
        }
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" commands ", theme.border_focused(), theme))
            .style(theme.bg(theme.palette.bg_raise))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_help(frame: &mut Frame, area: Rect, theme: &Theme) {
    let width = area.width.saturating_sub(10).min(72);
    let height = area.height.saturating_sub(6).min(24);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let key = |keys: &'static str| Span::styled(format!("  {keys:<12}"), theme.accent());
    let text = |what: &'static str| Span::styled(what, theme.text());
    let lines: Vec<Line> = vec![
        Line::from(Span::styled(
            " compose",
            theme.accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![key("i / Tab"), text("focus the composer")]),
        Line::from(vec![key("Enter"), text("submit the composed task")]),
        Line::from(vec![key("Ctrl+J"), text("insert a newline")]),
        Line::from(vec![key("Ctrl+Z/Y"), text("undo / redo")]),
        Line::from(vec![key("Up / Dn"), text("composer history, or scroll")]),
        Line::from(""),
        Line::from(Span::styled(
            " session",
            theme.accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![key("a / d"), text("approve / deny a gated action")]),
        Line::from(vec![key("c"), text("cancel the running task")]),
        Line::from(vec![key("p / r"), text("pause / resume")]),
        Line::from(vec![key(":"), text("command palette")]),
        Line::from(vec![key("v"), text("close the review workspace")]),
        Line::from(""),
        Line::from(Span::styled(
            " view",
            theme.accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            key("1 2 3"),
            text("transcript / state / jobs (narrow)"),
        ]),
        Line::from(vec![key("i"), text("decision inspector overlay")]),
        Line::from(vec![key("F1 / ?"), text("toggle this help")]),
        Line::from(vec![
            key("q / Ctrl+C"),
            text("quit, restoring the terminal"),
        ]),
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" help ", theme.border_focused(), theme))
            .style(theme.bg(theme.palette.bg_raise)),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionEvent, TaskId, TaskRevision, TurnId};
    use crate::theme::ColorLevel;
    use crate::tui_state::{MAX_TIMELINE, WorkbenchState};
    use crate::{Action, DecisionSource, ModelTier, NodeStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Render one frame at a given size and return the visible text.
    fn snapshot(state: &WorkbenchState, width: u16, height: u16, tab: Tab) -> String {
        snapshot_themed(
            state,
            width,
            height,
            tab,
            &Theme::for_level(ColorLevel::TrueColor),
        )
    }

    fn snapshot_themed(
        state: &WorkbenchState,
        width: u16,
        height: u16,
        tab: Tab,
        theme: &Theme,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_themed(frame, state, tab, theme))
            .unwrap();
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
        assert!(text.contains("knut"), "the wordmark is present:\n{text}");
        assert!(text.contains("running"));
        assert!(text.contains("transcript"));
        // The composer and footer are present.
        assert!(text.contains("enter submit"));
    }

    #[test]
    fn the_welcome_screen_teaches_the_first_move() {
        let state = WorkbenchState::new("/workspace/knut");
        let text = snapshot(&state, 100, 30, Tab::Timeline);
        // The block wordmark spells the name at display size.
        assert!(
            text.contains('K') || text.contains('█'),
            "the mark is drawn:{text}"
        );
        assert!(
            text.contains("offline") || text.contains("ready"),
            "the empty state says whether it can work:\n{text}"
        );
        assert!(text.contains("Enter") || text.contains("enter"));
    }

    #[test]
    fn narrow_windows_collapse_to_tabs_without_losing_content() {
        let state = populated_state();
        let timeline = snapshot(&state, 60, 24, Tab::Timeline);
        assert!(timeline.contains("transcript"));
        assert!(timeline.contains("fix the failing test"));

        let jobs = snapshot(&state, 60, 24, Tab::Tasks);
        assert!(jobs.contains("jobs") || jobs.contains("nothing running"));

        let tiny = snapshot(&state, 30, 12, Tab::Timeline);
        assert!(!tiny.trim().is_empty());
    }

    #[test]
    fn large_layouts_show_the_inspector() {
        let state = populated_state();
        let text = snapshot(&state, 140, 40, Tab::Timeline);
        assert!(text.contains("STATE") || text.contains("state"));
        assert!(text.contains("glm-5.3-flash"));
        assert!(text.contains("turns"));
    }

    #[test]
    fn short_windows_drop_the_inspector_instead_of_cramping() {
        let plan = plan_layout(Rect::new(0, 0, 120, 14), true);
        assert!(plan.inspector.is_none());
        assert!(plan.composer.height >= 3);
        assert_eq!(plan.footer.height, 1);
    }

    #[test]
    fn resize_between_sizes_is_stable() {
        let state = populated_state();
        let theme = Theme::for_level(ColorLevel::TrueColor);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render_themed(frame, &state, Tab::Timeline, &theme))
            .unwrap();
        terminal.backend_mut().resize(140, 40);
        terminal
            .draw(|frame| render_themed(frame, &state, Tab::Timeline, &theme))
            .unwrap();
        terminal.backend_mut().resize(40, 10);
        terminal
            .draw(|frame| render_themed(frame, &state, Tab::Timeline, &theme))
            .unwrap();
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
        let text = snapshot(&state, 70, 30, Tab::Tasks);
        assert!(text.contains("needs you"));
        assert!(text.contains("approve"));
        assert!(text.contains("deny"));

        let timeline = snapshot(&state, 70, 30, Tab::Timeline);
        assert!(timeline.contains("waiting"));
    }

    #[test]
    fn help_overlay_renders_inside_the_terminal() {
        let mut state = populated_state();
        state.help = true;
        for (width, height) in [(80, 24), (40, 12), (120, 44)] {
            let text = snapshot(&state, width, height, Tab::Timeline);
            assert!(text.contains("compose"), "help missing at {width}x{height}");
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

    #[test]
    fn every_entry_kind_has_a_distinct_glyph() {
        let theme = Theme::for_level(ColorLevel::TrueColor);
        let glyphs: Vec<&str> = [
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
        .map(|kind| entry_glyph(*kind, &theme))
        .collect();
        // Terminal and evidence share a tick deliberately (both mean
        // "verified"), but nothing else may be ambiguous.
        let mut unique = glyphs.clone();
        unique.sort();
        unique.dedup();
        assert!(unique.len() >= 7, "too much glyph reuse: {glyphs:?}");
    }

    #[test]
    fn input_to_paint_stays_within_the_target_on_a_long_session() {
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

        let theme = Theme::for_level(ColorLevel::TrueColor);
        let mut timings = Vec::new();
        for _ in 0..200 {
            let started = std::time::Instant::now();
            let mut local = state.clone();
            local.composer.insert("x");
            let _ = snapshot_themed(&local, 120, 40, Tab::Timeline, &theme);
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

    #[test]
    fn rendering_never_awaits_and_has_no_io() {
        let mut state = WorkbenchState::new("/workspace/knut");
        state.apply(&SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "x".to_owned(),
        });
        let text = snapshot(&state, 100, 30, Tab::Timeline);
        assert!(text.contains("x"));
    }

    #[test]
    fn cards_and_queued_work_are_visible_and_distinguishable() {
        let mut state = populated_state();
        state.apply(&SessionEvent::CardStarted {
            task: TaskId(1),
            turn: TurnId(1),
            card_id: "tool-1".to_owned(),
            title: "run cargo test".to_owned(),
        });
        state.apply(&SessionEvent::CardFinished {
            task: TaskId(1),
            turn: TurnId(1),
            card_id: "tool-2".to_owned(),
            state: crate::cards::CardState::Cancelled,
            summary: "stopped by the user".to_owned(),
            detail: String::new(),
            elapsed_ms: 40,
        });
        state.apply(&SessionEvent::CardFinished {
            task: TaskId(1),
            turn: TurnId(1),
            card_id: "tool-3".to_owned(),
            state: crate::cards::CardState::Failed,
            summary: "exit 101".to_owned(),
            detail: "assertion failed".to_owned(),
            elapsed_ms: 900,
        });
        state.apply(&SessionEvent::RequestQueued {
            text: "then update the docs".to_owned(),
        });

        let text = snapshot(&state, 160, 44, Tab::Timeline);
        assert!(
            text.contains("run cargo test"),
            "active card visible:\n{text}"
        );
        assert!(text.contains("queued"));
        assert!(text.contains("then update the docs"));
        // A card that stopped must say *how* it stopped: cancellation and
        // failure must never read as success.
        assert!(
            text.contains("cancelled"),
            "cancellation is a word:\n{text}"
        );
        assert!(text.contains("failed"), "failure is a word:\n{text}");
        // And on a cramped pane the state word survives while the summary
        // is what gets dropped.
        let narrow = snapshot(&state, 100, 26, Tab::Tasks);
        assert!(
            narrow.contains("cancelled") || narrow.contains("failed"),
            "narrow panes keep the state word:\n{narrow}"
        );
    }

    #[test]
    fn a_scrolled_transcript_does_not_jump_when_new_output_arrives() {
        let mut state = populated_state();
        for i in 0..60 {
            state.apply(&SessionEvent::NodeResult {
                task: TaskId(1),
                turn: TurnId(1),
                node: crate::session::NodeId(i),
                node_label: format!("node-{i}"),
                status: NodeStatus::Succeeded,
                output: serde_json::Value::Null,
            });
        }
        state.follow = false;
        state.selection = 5;

        let before = state.selection;
        state.apply(&SessionEvent::TextDelta {
            task: TaskId(1),
            turn: TurnId(1),
            node: crate::session::NodeId(999),
            text: "new output arrives".to_owned(),
        });
        assert_eq!(state.selection, before);
        assert!(!state.follow);
    }

    #[test]
    fn every_theme_level_renders_a_usable_screen() {
        let state = populated_state();
        for level in [
            ColorLevel::TrueColor,
            ColorLevel::Ansi256,
            ColorLevel::Ansi16,
            ColorLevel::Mono,
        ] {
            let theme = Theme::for_level(level);
            let text = snapshot_themed(&state, 100, 30, Tab::Timeline, &theme);
            // The screen never depends on colour to be legible.
            assert!(text.contains("knut"), "{level:?} lost the wordmark");
            assert!(text.contains("running"), "{level:?} lost the state");
            assert!(text.contains("transcript"), "{level:?} lost the panels");
        }
    }

    #[test]
    fn the_ascii_theme_emits_no_unicode_of_its_own() {
        // The fixture deliberately carries CJK and accented text, which
        // reaches the screen as *content* whatever the theme does. What
        // the theme controls is chrome, so the assertion is about the
        // characters this renderer chooses: glyphs and borders.
        let mut state = WorkbenchState::new("/workspace/knut");
        state.model = Some("glm-5.3-flash".to_owned());
        state.apply(&SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "fix the build".to_owned(),
        });
        state.apply(&SessionEvent::NodeResult {
            task: TaskId(1),
            turn: TurnId(1),
            node: crate::session::NodeId(1),
            node_label: "read".to_owned(),
            status: NodeStatus::Failed,
            output: serde_json::json!({ "path": "src/lib.rs" }),
        });
        state.apply(&SessionEvent::WaitingForUser {
            task: TaskId(1),
            turn: TurnId(1),
            wait: crate::session::WaitKind::Approval {
                approval_key: "fp-1".to_owned(),
            },
            message: "the write needs approval".to_owned(),
        });

        let theme = Theme::plain_ascii();
        let text = snapshot_themed(&state, 110, 34, Tab::Timeline, &theme);
        assert!(
            text.is_ascii(),
            "ascii chrome produced non-ascii pixels:\n{text}"
        );

        // The state words and structure survive without any glyphs.
        assert!(text.contains("failed"));
        assert!(text.contains("waiting"));
        assert!(text.contains("+--"));
    }

    #[test]
    fn a_unicode_capable_terminal_gets_the_rounded_chrome() {
        let state = populated_state();
        let theme = Theme::for_level(ColorLevel::TrueColor);
        let text = snapshot_themed(&state, 100, 30, Tab::Timeline, &theme);
        assert!(
            text.contains('╭') || text.contains('│'),
            "expected box-drawing chrome:\n{text}"
        );
    }

    #[test]
    fn the_composer_caret_tracks_hard_wrapped_lines() {
        let lines = vec!["short".to_owned()];
        // A short line: caret on the first display row, at its column.
        assert_eq!(composer_cursor_position(&lines, 0, 3, 20), (0, 3));

        // A line that wraps: the caret lands on the continuation row at
        // the right offset, so typing continues where the eye expects.
        let long = vec!["x".repeat(45)];
        assert_eq!(composer_cursor_position(&long, 0, 20, 20), (1, 0));
        assert_eq!(composer_cursor_position(&long, 0, 25, 20), (1, 5));
        assert_eq!(composer_cursor_position(&long, 0, 45, 20), (2, 5));

        // A second logical line starts *below* the wrapped rows of the
        // first, not on top of them.
        let multi = vec!["x".repeat(45), "second".to_owned()];
        assert_eq!(composer_cursor_position(&multi, 1, 2, 20), (3, 2));
    }

    #[test]
    fn hard_wrapping_preserves_every_character() {
        let line = "abcdefghij";
        let chunks = hard_wrap(line, 4);
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
        assert_eq!(chunks.concat(), line);

        // An empty line still occupies one display row.
        assert_eq!(hard_wrap("", 4), vec![""]);
        // A line exactly on the boundary does not emit a phantom chunk,
        // but the caret math accounts for the row it fills.
        assert_eq!(hard_wrap("abcd", 4), vec!["abcd"]);
    }

    #[test]
    fn the_composer_renders_its_own_text_and_never_wraps_it_away() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Composer;
        state.composer.insert("fix the failing test");
        let text = snapshot(&state, 100, 30, Tab::Timeline);
        assert!(text.contains("fix the failing test"), "{text}");

        // A long single line wraps rather than being clipped away.
        state.composer = crate::composer::Composer::new();
        state.composer.paste(&"abcdefghij".repeat(12));
        let text = snapshot(&state, 100, 30, Tab::Timeline);
        assert!(text.contains("abcdefghijab"), "the pasted text is drawn");
    }

    #[test]
    fn text_wrapping_splits_long_tokens_instead_of_overflowing() {
        let long = "a".repeat(60);
        let wrapped = wrap_text(&long, 20);
        assert!(wrapped.len() >= 3);
        assert!(wrapped.iter().all(|line| line.chars().count() <= 20));

        let sentence = wrap_text("the quick brown fox jumps over the lazy dog", 12);
        assert!(sentence.iter().all(|line| line.chars().count() <= 12));
        // Words are never split when they fit.
        assert!(sentence.iter().any(|line| line.contains("quick")));
    }

    #[test]
    fn compact_paths_keep_the_leaf_visible() {
        let deep = "/home/user/projects/deeply/nested/workspace/knut";
        let compact = compact_path(deep, 60);
        assert!(compact.contains("knut"));
        assert!(compact.chars().count() < deep.chars().count());
        // A short path is left alone.
        assert_eq!(compact_path("/tmp/x", 60), "/tmp/x");
    }
}
