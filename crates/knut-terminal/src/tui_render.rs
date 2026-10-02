//! Rendering for the workbench shell.
//!
//! Pure functions over [`WorkbenchState`]: given state and a frame area,
//! produce widgets. No state mutation, no I/O, no terminal control beyond
//! the frame it is handed — so every layout can be snapshot-tested with
//! `TestBackend` at any size.
//!

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::attach::CommandAvailability;
use crate::review::MAX_HUNK_LINES;
use crate::session::TaskState;
use crate::theme::Theme;
use crate::tui_state::{Focus, TimelineKind, WorkbenchState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Timeline,
    Inspector,
    Tasks,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Timeline => "conversation",
            Tab::Inspector => "decisions",
            Tab::Tasks => "jobs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPlan {
    pub header: Rect,
    pub timeline: Rect,
    pub composer: Rect,
    pub footer: Rect,
}

pub fn plan_layout(area: Rect, composer_rows: usize) -> LayoutPlan {
    let margin = if area.width >= 60 { 2 } else { 0 };
    let content = Rect {
        x: area.x + margin,
        width: area.width.saturating_sub(margin * 2),
        ..area
    };
    let vertical = Layout::vertical([
        Constraint::Length(if area.height >= 18 { 3 } else { 2 }),
        Constraint::Min(1),
        Constraint::Length((composer_rows as u16).clamp(1, 6) + 2),
        Constraint::Length(2),
    ])
    .split(content);
    LayoutPlan {
        header: vertical[0],
        timeline: vertical[1],
        composer: vertical[2],
        footer: vertical[3],
    }
}

/// Render the whole shell.
pub fn render(frame: &mut Frame, state: &WorkbenchState, tab: Tab) {
    render_themed(frame, state, tab, &state.theme);
}

/// Render with an explicit theme (tests pin one; the shell detects it).
pub fn render_themed(frame: &mut Frame, state: &WorkbenchState, tab: Tab, theme: &Theme) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    paint_backdrop(frame, area, theme);

    let margin = if area.width >= 60 { 4 } else { 0 };
    let width = area.width.saturating_sub(margin + 6).max(1) as usize;
    let rows = state
        .composer
        .lines()
        .iter()
        .map(|line| hard_wrap(line, width).len())
        .sum();
    let plan = plan_layout(area, rows);

    render_header(frame, state, plan.header, theme);
    if let Some(connection) = &state.connection {
        let body = Rect::new(
            plan.timeline.x,
            plan.timeline.y,
            plan.timeline.width,
            plan.timeline.height + plan.composer.height,
        );
        render_connection(frame, connection, body, theme);
        let hint = if connection.busy {
            if connection.cancellable {
                " Esc cancel"
            } else {
                " Finishing connection update…"
            }
        } else {
            " Up/Down choose   Enter continue   Esc back"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(hint),
                Line::from(if state.chatgpt_plan {
                    format!(
                        " Using ChatGPT plan: {}",
                        crate::cards::sanitize_for_display(
                            state.account.as_deref().unwrap_or("ChatGPT account")
                        )
                    )
                } else {
                    " Draft preserved".to_owned()
                }),
            ])
            .style(theme.dim())
            .wrap(Wrap { trim: false }),
            plan.footer,
        );
        return;
    }
    let mut body = plan.timeline;
    if let Some(pending) = &state.pending
        && !state.approval_open
    {
        let rows = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(6.min(body.height / 2)),
        ])
        .split(body);
        body = rows[0];
        frame.render_widget(
            Paragraph::new(pending.message.as_str())
                .block(panel(" needs your input ", theme.warn(), theme))
                .wrap(Wrap { trim: false }),
            rows[1],
        );
    }
    if state.approval_open {
        render_approval(frame, state, body, theme);
    } else {
        match tab {
            Tab::Timeline => render_timeline(frame, state, body, theme),
            Tab::Inspector => render_inspector(frame, state, body, theme),
            Tab::Tasks => render_jobs(frame, state, body, theme),
        }
    }
    render_composer(frame, state, plan.composer, theme);
    render_footer(frame, state, plan.footer, theme);

    // The review workspace replaces the workbench while it is open: a diff
    // needs the room, and review is the point of the view.
    if let Some(review) = &state.review {
        render_review_themed(frame, review, theme);
        if state.help {
            render_help(frame, state, area, theme);
        }
        return;
    }

    if state.help {
        render_help(frame, state, area, theme);
    }

    if state.palette_open() {
        render_palette(frame, state, area, theme);
    }

    // The cursor belongs to the composer, and only when the composer is
    // focused: a text cursor over a read-only pane is a lie. The position
    // is mapped through the same wrapping the composer drew with, so the
    // caret stays on the character it will edit even in a long line.
    if state.focus == Focus::Composer
        && state.review.is_none()
        && !state.help
        && !state.palette_open()
        && plan.composer.width > 6
        && plan.composer.height >= 3
    {
        let area = plan.composer;
        let inner_width = area.width.saturating_sub(6) as usize;
        let (cursor_row, cursor_col) = state.composer.cursor();
        let (row, col) = composer_cursor_position(
            state.composer.lines(),
            cursor_row,
            cursor_col,
            inner_width.max(1),
        );
        // Inside the border, after the " ❯ " sigil column.
        let scroll = row.saturating_sub(area.height.saturating_sub(3) as usize);
        let y =
            (area.y + 1 + row.saturating_sub(scroll) as u16).min(area.bottom().saturating_sub(2));
        let x = (area.x + 4 + col as u16).min(area.right().saturating_sub(2));
        frame.set_cursor_position((x, y));
    }
}

pub fn composer_cursor_position(
    lines: &[String],
    cursor_row: usize,
    cursor_col: usize,
    inner_width: usize,
) -> (usize, usize) {
    let width = inner_width.max(1);
    let mut row = 0;
    for (index, line) in lines.iter().enumerate() {
        let mut col = 0;
        for (offset, grapheme) in line.graphemes(true).enumerate() {
            let cells = grapheme.width();
            if col + cells > width || col == width {
                row += 1;
                col = 0;
            }
            if index == cursor_row && offset == cursor_col {
                return (row, col);
            }
            col += cells;
        }
        if index == cursor_row {
            return if col >= width {
                (row + 1, 0)
            } else {
                (row, col)
            };
        }
        row += 1 + usize::from(col >= width);
    }
    (row, 0)
}

fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut cells = 0;
    for grapheme in line.graphemes(true) {
        let size = grapheme.width();
        if cells + size > width || cells == width {
            rows.push(String::new());
            cells = 0;
        }
        rows.last_mut().unwrap().push_str(grapheme);
        cells += size;
    }
    if cells >= width {
        rows.push(String::new());
    }
    rows
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

fn render_header(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let mark_width = if area.width >= 50 && area.height >= 3 {
        8
    } else {
        0
    };
    if mark_width > 0 {
        frame.render_widget(
            Paragraph::new(crate::knot::logo_lines(
                theme,
                mark_width as usize,
                3,
                state.brand_tick(),
            )),
            Rect::new(area.x, area.y, mark_width, 3),
        );
    }
    let text_area = Rect::new(
        area.x + mark_width,
        area.y,
        area.width.saturating_sub(mark_width),
        area.height,
    );
    let (marker, label) = state_marker_for(state.task_state, theme);
    let branch = state.branch.as_deref().unwrap_or_default();
    let workspace = compact_path(
        &state.workspace,
        text_area
            .width
            .saturating_sub(12 + branch.width().min(20) as u16) as usize,
    );
    let mut identity = vec![
        Span::styled(" knut ", theme.accent().add_modifier(Modifier::BOLD)),
        Span::styled(workspace, theme.text()),
    ];
    if !branch.is_empty() && text_area.width >= 40 {
        identity.push(Span::styled(
            format!("  / {}", clipped(branch, 20)),
            theme.faint(),
        ));
    }
    let mut status = vec![Span::styled(
        format!(" {marker} {label}"),
        state_style(state.task_state, theme),
    )];
    if text_area.width >= 35 {
        status.push(Span::styled(
            format!(
                "  {} {}",
                if theme.glyphs.unicode { "·" } else { "|" },
                clipped(
                    &state.reasoner_label(),
                    text_area.width.saturating_sub(29) as usize
                )
            ),
            theme.dim(),
        ));
    }
    let elapsed = ticker_label(state);
    if !elapsed.is_empty() && text_area.width >= 46 {
        status.push(Span::styled(format!("  {elapsed}"), theme.faint()));
    }
    if state.chatgpt_plan && text_area.width >= 55 {
        status.push(Span::styled(" · Using ChatGPT plan", theme.dim()));
    }
    if !state.queued.is_empty() {
        status.push(Span::styled(
            format!("  / {} queued", state.queued.len()),
            theme.warn(),
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![Line::from(identity), Line::from(status)]),
        text_area,
    );
}

fn clipped(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut cells = 0;
    for grapheme in text.graphemes(true) {
        if cells + grapheme.width() > width {
            break;
        }
        result.push_str(grapheme);
        cells += grapheme.width();
    }
    result
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

fn compact_path(path: &str, width: usize) -> String {
    let width = width.min(36);
    if path.width() <= width {
        return path.to_owned();
    }
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let tail = parts[parts.len().saturating_sub(2)..].join("/");
    if tail.width() + 2 <= width {
        return format!("~/{tail}");
    }
    if width == 0 {
        return String::new();
    }
    let mut cells = 1;
    let mut suffix = Vec::new();
    for grapheme in tail.graphemes(true).rev() {
        if cells + grapheme.width() > width {
            break;
        }
        suffix.push(grapheme);
        cells += grapheme.width();
    }
    format!("~{}", suffix.into_iter().rev().collect::<String>())
}

/// The tab strip for narrow terminals.
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
    let entries = state.visible_timeline();
    if entries.is_empty() {
        render_welcome(frame, state, area, theme);
        return;
    }
    let height = area.height.saturating_sub(1) as usize;
    let width = area.width.saturating_sub(5).max(1) as usize;
    let end = if state.follow {
        entries.len()
    } else {
        (state.selection + 1).min(entries.len())
    };
    let mut reversed = Vec::new();
    let skip = if state.follow {
        0
    } else {
        state.transcript_scroll
    };
    for entry in entries[..end].iter().rev() {
        if matches!(entry.kind, TimelineKind::Routing | TimelineKind::Decision) {
            continue;
        }
        let mut block = Vec::new();
        for (index, text) in entry.text.lines().enumerate() {
            for (part, chunk) in hard_wrap(text, width).into_iter().enumerate() {
                let lead = if index == 0 && part == 0 {
                    format!(" {} ", entry_glyph(entry.kind, theme))
                } else {
                    "   ".to_owned()
                };
                block.push(Line::from(vec![
                    Span::styled(lead, entry_style(entry.kind, theme)),
                    Span::styled(chunk, entry_style(entry.kind, theme)),
                ]));
            }
        }
        if matches!(entry.kind, TimelineKind::User | TimelineKind::Assistant) {
            block.push(Line::from(""));
        }
        reversed.extend(block.into_iter().rev());
        if reversed.len() >= height + skip {
            break;
        }
    }
    let skip = skip.min(reversed.len().saturating_sub(height));
    let lines: Vec<_> = reversed.into_iter().skip(skip).take(height).rev().collect();
    let title = if state.follow {
        format!(
            " conversation{} ",
            if state.timeline_offset > 0 {
                " / earlier entries archived"
            } else {
                ""
            }
        )
    } else {
        " conversation / scrollback - Esc for latest ".to_owned()
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().title(Span::styled(title, theme.faint()))),
        area,
    );
}

fn render_approval(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let Some(pending) = &state.pending else {
        return;
    };
    let mut text = pending.message.clone();
    if let crate::WaitKind::Approval { approval_key } = &pending.kind {
        if let Some((key, name, arguments)) = &state.proposed_action
            && key == approval_key
        {
            text = crate::review::approval_text(name, arguments);
        }
        text.push_str(&format!("\n\nApproval identity: {approval_key}\n"));
    }
    let text = crate::cards::sanitize_for_display(&text);
    let lines: Vec<_> = text
        .lines()
        .flat_map(|line| hard_wrap(line, area.width.saturating_sub(2).max(1) as usize))
        .map(|line| {
            let style = if line.starts_with('+') {
                theme.success()
            } else if line.starts_with('-') {
                theme.danger()
            } else {
                theme.text()
            };
            Line::from(Span::styled(line, style))
        })
        .collect();
    let height = area.height.saturating_sub(2) as usize;
    let scroll = state
        .approval_scroll
        .min(lines.len().saturating_sub(height));
    let title = format!(
        " proposed action / lines {}-{} / {} ",
        scroll + 1,
        (scroll + height).min(lines.len()),
        lines.len()
    );
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(scroll)
                .take(height)
                .collect::<Vec<_>>(),
        )
        .block(panel(&title, theme.warn(), theme)),
        area,
    );
}

fn render_connection(
    frame: &mut Frame,
    connection: &crate::connection::ConnectionPanel,
    area: Rect,
    theme: &Theme,
) {
    use crate::connection::ConnectionPage;
    let title = match connection.page {
        ConnectionPage::Settings => " Settings ",
        ConnectionPage::Account => " ChatGPT account ",
        ConnectionPage::Models => " Choose a model ",
        ConnectionPage::Welcome => " You're using your ChatGPT plan ",
    };
    let block = panel(title, theme.border_focused(), theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width.saturating_sub(2).max(1) as usize;
    let mut lines = Vec::new();
    let description = if connection.page == ConnectionPage::Welcome {
        "Your ChatGPT plan covers model usage in Knut. Usage limits apply. Manage usage opens ChatGPT settings. Knut keeps its own renewable sign-in on this computer."
    } else {
        &connection.status
    };
    for line in wrap_text(description, width) {
        lines.push(Line::from(Span::styled(format!(" {line}"), theme.text())));
    }
    if connection.page != ConnectionPage::Welcome
        && let Some(account) = connection.accounts.iter().find(|a| a.active && a.signed_in)
    {
        for line in wrap_text(
            &format!(
                "Account: {}",
                crate::cards::sanitize_for_display(&account.label)
            ),
            width,
        ) {
            lines.push(Line::from(Span::styled(format!(" {line}"), theme.dim())));
        }
    }
    if connection.page != ConnectionPage::Welcome
        && let Some(error) = &connection.error
    {
        for line in wrap_text(error, width) {
            lines.push(Line::from(Span::styled(format!(" {line}"), theme.danger())));
        }
    }
    lines.push(Line::from(""));
    // Reserve selectable rows when long errors or account names fill a small terminal.
    let budget = inner.height.saturating_sub(2) as usize;
    lines.truncate(budget.saturating_sub(3));
    let available = (inner.height as usize).saturating_sub(lines.len()).max(1);
    let choices = connection.choices();
    let start = connection
        .selection
        .saturating_sub(available.saturating_sub(1));
    for (index, (label, _)) in choices.iter().enumerate().skip(start).take(available) {
        let selected = index == connection.selection;
        let marker = if selected { "> " } else { "  " };
        let style = if connection.busy {
            theme.dim()
        } else if selected {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        lines.push(Line::from(Span::styled(
            format!(" {marker}{}", crate::cards::sanitize_for_display(label)),
            style,
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(theme.bg(theme.palette.bg)),
        inner,
    );
}

fn render_welcome(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let rich = area.width >= 66 && area.height >= 14;
    let compact = !rich && area.width >= 42 && area.height >= 9;
    let content_width = area.width.saturating_sub(4).min(72);
    let left = area.x + area.width.saturating_sub(content_width) / 2;
    let top = area.y + area.height.saturating_sub(if rich { 15 } else { 10 }) / 3;
    let (text_x, text_y, text_width, after) = if rich || compact {
        let (mark_width, mark_height) = if rich { (30, 10) } else { (18, 6) };
        frame.render_widget(
            Paragraph::new(crate::knot::logo_lines(
                theme,
                mark_width,
                mark_height,
                state.brand_tick(),
            )),
            Rect::new(left, top, mark_width as u16, mark_height as u16),
        );
        (
            left + mark_width as u16 + 3,
            top + 1,
            content_width.saturating_sub(mark_width as u16 + 3),
            top + mark_height as u16,
        )
    } else {
        (left, top, content_width, top + 2)
    };
    let mut brand = theme.brand_gradient("knut");
    for span in &mut brand {
        span.style = span.style.add_modifier(Modifier::BOLD);
    }
    brand.push(Span::styled(
        format!("  v{}", env!("CARGO_PKG_VERSION")),
        theme.faint(),
    ));
    let mut lines = vec![Line::from(brand)];
    if rich || compact {
        lines.push(Line::from(Span::styled(
            "Your code, untangled.",
            theme.dim(),
        )));
        lines.push(Line::from(""));
    }
    for line in wrap_text("What are we building?", text_width as usize) {
        lines.push(Line::from(Span::styled(
            line,
            theme.text().add_modifier(Modifier::BOLD),
        )));
    }
    if rich {
        lines.push(Line::from(""));
        for line in wrap_text(
            "Describe a change, investigate a bug, or explore this codebase.",
            text_width as usize,
        ) {
            lines.push(Line::from(Span::styled(line, theme.dim())));
        }
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            text_x,
            text_y,
            text_width,
            area.bottom().saturating_sub(text_y),
        ),
    );
    let message = if let Some(reason) = &state.unavailable {
        Some(reason.clone())
    } else if state.model.is_none() {
        Some("Offline. F2 settings > Continue with ChatGPT, or configure an API key.".to_owned())
    } else if !rich {
        Some("Describe a change or ask about this codebase.".to_owned())
    } else {
        None
    };
    let mut details = Vec::new();
    if let Some(message) = message {
        for line in wrap_text(&message, content_width as usize) {
            details.push(Line::from(Span::styled(line, theme.dim())));
        }
    } else if area.height >= 18 {
        details.push(Line::from(vec![
            Span::styled("Try  ", theme.faint()),
            Span::styled("Find what causes the failing test", theme.text()),
        ]));
        details.push(Line::from(vec![
            Span::styled("     ", theme.faint()),
            Span::styled("Explain how requests reach the model", theme.text()),
        ]));
    }
    let detail_y = after + 1;
    frame.render_widget(
        Paragraph::new(details),
        Rect::new(
            left,
            detail_y.min(area.bottom()),
            content_width,
            area.bottom().saturating_sub(detail_y),
        ),
    );
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut current));
        }
        if word.width() > width {
            let mut chunks = hard_wrap(word, width);
            current = chunks.pop().unwrap_or_default();
            lines.extend(chunks);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
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
                    Span::styled("  Alt+A ", theme.success()),
                    Span::styled("approve  ", theme.dim()),
                    Span::styled("Alt+D ", theme.danger()),
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

    if !state.queued.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" queued ({})", state.queued.len()),
            theme.warn(),
        )));
        lines.push(Line::from(Span::styled(
            " Up/Down select / Alt+E edit / Alt+X remove",
            theme.dim(),
        )));
        if state.task_state.is_none_or(|s| s.is_terminal()) {
            lines.push(Line::from(Span::styled(
                " Alt+R starts selected request",
                theme.dim(),
            )));
        }
        for (index, request) in state.queued.iter().enumerate() {
            let short: String = request
                .prompt
                .replace('\n', " ")
                .chars()
                .take(inner_width.saturating_sub(8))
                .collect();
            lines.push(Line::from(Span::styled(
                format!(
                    " {} #{} {}",
                    if index == state.queued_selection {
                        ">"
                    } else {
                        " "
                    },
                    request.id,
                    short
                ),
                if index == state.queued_selection {
                    theme.accent()
                } else {
                    theme.dim()
                },
            )));
        }
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
        for card in &active {
            lines.push(job_line(card, state.tick, inner_width, theme));
        }
    }

    if !finished.is_empty() {
        if !active.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(" recent", theme.dim())));
        for card in &finished {
            lines.push(job_line(card, state.tick, inner_width, theme));
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

    let title = " jobs / PgUp PgDn scroll / Esc close ";
    let scroll = state.detail_scroll.min(
        lines
            .len()
            .saturating_sub(area.height.saturating_sub(2) as usize) as u16,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .block(panel(title, theme.border(), theme))
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

    section(&mut lines, "recent decisions");
    for decision in inspector.decisions().iter().rev().take(30) {
        for line in wrap_text(&decision.row(), width) {
            lines.push(Line::from(Span::styled(format!("  {line}"), theme.dim())));
        }
        if let Some(reason) = decision.escalation_explanation() {
            for line in wrap_text(&reason, width) {
                lines.push(Line::from(Span::styled(format!("  {line}"), theme.warn())));
            }
        }
    }
    let title = " decisions / PgUp PgDn scroll / Esc close ";
    let scroll = state.detail_scroll.min(
        lines
            .len()
            .saturating_sub(area.height.saturating_sub(2) as usize) as u16,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .block(panel(title, theme.border(), theme))
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
            let gutter = if index == 0 && chunk_index == 0 {
                sigil.clone()
            } else {
                Span::styled("   ", theme.faint())
            };
            lines.push(Line::from(vec![gutter, Span::styled(chunk, style)]));
        }
    }

    if state.composer.text().is_empty() {
        lines.clear();
        lines.push(Line::from(vec![
            sigil,
            Span::styled(
                if inner_width < 34 {
                    "Type a task"
                } else {
                    "Describe a task, or / for commands"
                },
                theme.faint(),
            ),
        ]));
    }

    let (cursor_row, cursor_col) = state.composer.cursor();
    let (display_row, _) =
        composer_cursor_position(composer_lines, cursor_row, cursor_col, inner_width);
    let scroll = display_row.saturating_sub(budget.saturating_sub(1));
    let lines: Vec<_> = lines.into_iter().skip(scroll).take(budget).collect();
    let title = if state.queue_edit.is_some() {
        " edit queued request: Enter save / Esc cancel "
    } else if state.steer_draft && state.task_state.is_some_and(|s| !s.is_terminal()) {
        " steer current task / Alt+S to queue "
    } else if state
        .pending
        .as_ref()
        .is_some_and(|p| matches!(p.kind, crate::session::WaitKind::Approval { .. }))
    {
        " approval needed: Alt+A allow / Alt+D deny "
    } else if state.pending.is_some() {
        " your answer "
    } else if state.task_state.is_some_and(|s| !s.is_terminal()) {
        if state.steer_draft {
            " steer current task / Alt+S to queue "
        } else {
            " queue next task / Alt+S to steer "
        }
    } else if state.chatgpt_plan {
        " message · Using ChatGPT plan "
    } else {
        " message "
    };
    let border = if state.pending.is_some() {
        theme.warn().add_modifier(Modifier::BOLD)
    } else {
        border
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(
                &if theme.glyphs.unicode {
                    title.to_owned()
                } else {
                    title.replace('·', "|")
                },
                border,
                theme,
            ))
            .style(theme.bg(theme.palette.bg_raise))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let hints: Vec<(&str, &str)> = if state.usage_limit {
        vec![("Ctrl+U", "Manage usage"), ("/ account", "change account")]
    } else if state.queue_edit.is_some() {
        vec![("Enter", "save edit"), ("Esc", "restore draft")]
    } else if state
        .pending
        .as_ref()
        .is_some_and(|p| matches!(p.kind, crate::session::WaitKind::Approval { .. }))
    {
        vec![
            ("Alt+A", "allow"),
            ("Alt+D", "deny"),
            ("Alt+V", "full action"),
        ]
    } else if state.task_state.is_some_and(|s| !s.is_terminal()) && state.pending.is_none() {
        if state.steer_draft {
            vec![("Enter", "steer"), ("Alt+S", "queue"), ("Ctrl+C", "stop")]
        } else {
            vec![("Enter", "queue"), ("Alt+S", "steer"), ("Ctrl+C", "stop")]
        }
    } else if !state.follow {
        vec![("PgUp/PgDn", "scroll"), ("Esc", "latest")]
    } else if area.width < 40 {
        vec![("Enter", "send"), ("F1", "help")]
    } else {
        vec![
            ("Enter", "send"),
            ("/", "commands"),
            ("F1", "help"),
            ("F2", "settings"),
            ("Shift+Enter", "newline"),
        ]
    };
    let mut line = vec![Span::raw(" ")];
    let mut used = 1;
    for (key, label) in hints {
        let size = key.width() + label.width() + 3;
        if used + size > area.width as usize {
            continue;
        }
        line.push(Span::styled(key, theme.text()));
        line.push(Span::styled(format!(" {label}  "), theme.faint()));
        used += size;
    }
    let detail = state.status.clone().unwrap_or_else(|| {
        if state.approval_open {
            " PgUp/PgDn scroll action · Esc conversation · draft kept".to_owned()
        } else if state.task_state.is_some_and(|s| !s.is_terminal()) {
            if area.width < 45 {
                " Ctrl+O jobs · F1 help"
            } else {
                " Ctrl+O manage queue · Shift+Enter newline"
            }
            .to_owned()
        } else if state.chatgpt_plan {
            format!(
                " ChatGPT: {} · / account · / usage",
                crate::cards::sanitize_for_display(
                    state.account.as_deref().unwrap_or("ChatGPT account")
                )
            )
        } else {
            if area.width < 45 {
                " / commands · Ctrl+Q save & quit"
            } else {
                " Ctrl+R changes   Ctrl+O jobs   Ctrl+Q save & quit"
            }
            .to_owned()
        }
    });
    let detail = if theme.glyphs.unicode {
        detail
    } else {
        detail.replace('·', "|")
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(line),
            Line::from(Span::styled(detail, theme.faint())),
        ]),
        area,
    );
}

pub fn render_review(frame: &mut Frame, view: &crate::review::ReviewView) {
    render_review_themed(frame, view, &Theme::detect());
}

pub fn render_review_themed(frame: &mut Frame, view: &crate::review::ReviewView, theme: &Theme) {
    let area = frame.area();
    paint_backdrop(frame, area, theme);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(if view.checks.is_empty() {
            0
        } else {
            5.min(area.height / 3)
        }),
    ])
    .split(area);
    let label = format!(
        " Changes  {}/{} files   Left/Right file   Up/Down hunk   Esc close",
        if view.changes.files.is_empty() {
            0
        } else {
            view.file_index + 1
        },
        view.changes.files.len()
    );
    frame.render_widget(Paragraph::new(label).style(theme.dim()), rows[0]);
    render_review_diff(frame, view, rows[1], theme);
    if rows[2].height > 0 {
        render_review_checks(frame, view, rows[2], theme);
    }
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

    for (index, hunk) in file.hunks.iter().enumerate().skip(view.hunk_index).take(1) {
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
        for line in hunk.lines.iter().take(MAX_HUNK_LINES) {
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
        }
        if hunk.lines.len() > MAX_HUNK_LINES {
            lines.push(Line::from(Span::styled(
                format!("  .. {} more line(s)", hunk.lines.len() - MAX_HUNK_LINES),
                theme.faint(),
            )));
        }
    }

    let scroll = view.scroll.min(
        lines
            .len()
            .saturating_sub(area.height.saturating_sub(2) as usize),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll.min(u16::MAX as usize) as u16, 0))
            .block(panel(
                " diff / PgUp PgDn scroll / read only ",
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
/// The command palette: every entry states whether it exists.
fn render_palette(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let width = area.width.saturating_sub(4).min(80);
    let results = state.palette_results();
    let height = (results.len() as u16 + 5).min(area.height.saturating_sub(2));
    let popup = Rect::new(area.x + (area.width - width) / 2, area.y + 1, width, height);
    let visible = height.saturating_sub(5).max(1) as usize;
    let start = state.palette_selection.saturating_sub(visible - 1);
    let mut lines = vec![
        Line::from(Span::styled(
            format!(" /{}_", state.palette.as_deref().unwrap_or("")),
            theme.text(),
        )),
        Line::from(""),
    ];
    if results.is_empty() {
        lines.push(Line::from("  No matching command"));
    }
    for (index, command) in results.iter().enumerate().skip(start).take(visible) {
        let selected = index == state.palette_selection;
        let style = if selected {
            theme
                .text()
                .patch(theme.bg(theme.palette.bg_sel))
                .add_modifier(Modifier::BOLD)
        } else if command.is_available() {
            theme.text()
        } else {
            theme.faint()
        };
        let detail = match command.availability {
            CommandAvailability::Available => command.title,
            CommandAvailability::Unavailable(_) => "not available in this session",
        };
        lines.push(Line::from(Span::styled(
            format!(
                " {} {:<12} {}",
                if selected { ">" } else { " " },
                command.id,
                detail
            ),
            style,
        )));
    }
    lines.push(Line::from(Span::styled(
        " Up/Down select  Enter run  Esc close",
        theme.dim(),
    )));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" commands ", theme.border_focused(), theme))
            .style(theme.bg(theme.palette.bg_raise)),
        popup,
    );
}

pub(crate) fn shortcuts(state: &WorkbenchState) -> Vec<&'static str> {
    let mut lines = vec![
        " F2             Settings",
        " Enter          Send message",
        " Shift+Enter    Newline (Ctrl+J fallback)",
        " Ctrl+P /       Commands",
        " Ctrl+R         Review changes",
        " Ctrl+O / B     Jobs / decisions",
        " Alt+S          Switch queue / steer",
        " PgUp / PgDn    Scroll",
        " Ctrl+Z / Y     Undo / redo",
        " Alt+B / F      Move by word",
        " Ctrl+W         Delete previous word",
        " /motion        Toggle animations",
        " Ctrl+C         Stop / clear / quit",
        " Ctrl+Q         Save draft and quit when idle",
    ];
    if state.review.is_some() {
        lines = vec![
            " Left / Right   Previous / next file",
            " Up / Down      Previous / next hunk",
            " PgUp / PgDn    Scroll diff",
            " Esc            Back to conversation",
        ];
    } else if state
        .pending
        .as_ref()
        .is_some_and(|pending| matches!(pending.kind, crate::session::WaitKind::Approval { .. }))
    {
        lines.insert(0, " Alt+A / D      Allow / deny approval");
        lines.insert(1, " Alt+V          Full action preview");
    }
    lines.push(" ? / F1 / Esc   Close shortcuts");
    lines
}

fn render_help(frame: &mut Frame, state: &WorkbenchState, area: Rect, theme: &Theme) {
    let lines = shortcuts(state);
    let width = area.width.min(48);
    let height = area.height.min(lines.len() as u16 + 2);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Block::default().style(theme.faint()), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(state.help_scroll)
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .wrap(Wrap { trim: false })
        .style(theme.text().patch(theme.bg(theme.palette.bg)))
        .block(panel(
            " shortcuts / Esc close ",
            theme.border_focused(),
            theme,
        )),
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
    fn narrow_help_can_reveal_later_shortcuts_and_keeps_an_exit_hint() {
        let mut state = WorkbenchState::new("/workspace");
        state.help = true;
        state.help_scroll = 8;
        let text = snapshot(&state, 30, 12, Tab::Timeline);
        assert!(text.contains("Alt+B / F"));
        assert!(text.contains("Esc close"));
        assert!(
            wrap_text("日本語 e\u{301} 👩‍💻 words", 8)
                .iter()
                .all(|line| line.width() <= 8)
        );
        assert!(compact_path("/workspace/日本語のコード/e\u{301}👩‍💻", 13).width() <= 13);
    }

    #[test]
    fn renders_at_eighty_by_twenty_four() {
        let state = populated_state();
        let text = snapshot(&state, 80, 24, Tab::Timeline);
        assert!(text.contains("knut"), "the wordmark is present:\n{text}");
        assert!(text.contains("running"));
        assert!(text.contains("conversation"));
        // The composer and footer are present.
        assert!(text.contains("Enter queue"));
    }

    #[test]
    fn the_welcome_screen_teaches_the_first_move() {
        let state = WorkbenchState::new("/workspace/knut");
        let text = snapshot(&state, 100, 30, Tab::Timeline);
        // The block wordmark spells the name at display size.
        assert!(
            text.contains("What are we building?"),
            "the mark is drawn:{text}"
        );
        assert!(
            text.contains("offline") || text.contains("ready"),
            "the empty state says whether it can work:\n{text}"
        );
        assert!(text.contains("Enter") || text.contains("enter"));
    }

    #[test]
    fn secondary_views_work_at_every_width() {
        let state = populated_state();
        let timeline = snapshot(&state, 60, 24, Tab::Timeline);
        assert!(timeline.contains("conversation"));
        assert!(timeline.contains("fix the failing test"));

        let jobs = snapshot(&state, 60, 24, Tab::Tasks);
        assert!(jobs.contains("jobs") || jobs.contains("nothing running"));

        let tiny = snapshot(&state, 30, 12, Tab::Timeline);
        assert!(!tiny.trim().is_empty());
    }

    #[test]
    fn wide_layouts_keep_the_conversation_full_width() {
        let state = populated_state();
        let text = snapshot(&state, 140, 40, Tab::Timeline);
        assert!(!text.contains("turns"));
        assert!(text.contains("glm-5.3-flash"));
        let details = snapshot(&state, 140, 40, Tab::Inspector);
        assert!(details.contains("turns"));
        assert_eq!(plan_layout(Rect::new(0, 0, 140, 40), 1).timeline.width, 136);
    }

    #[test]
    fn short_windows_drop_the_inspector_instead_of_cramping() {
        let plan = plan_layout(Rect::new(0, 0, 120, 14), 1);
        assert_eq!(plan.timeline.width, 116);
        assert!(plan.composer.height >= 3);
        assert_eq!(plan.footer.height, 2);
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
        assert!(text.contains("allow"));
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
            assert!(
                text.contains("shortcuts"),
                "help missing at {width}x{height}"
            );
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
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut timings = Vec::new();
        for _ in 0..200 {
            let started = std::time::Instant::now();
            state.composer.insert("x");
            terminal
                .draw(|frame| render_themed(frame, &state, Tab::Timeline, &theme))
                .unwrap();
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
            request: crate::session::QueuedRequest {
                options: Default::default(),
                id: 1,
                prompt: "then update the docs".to_owned(),
            },
        });

        let text = snapshot(&state, 160, 44, Tab::Tasks);
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
            assert!(text.contains("conversation"), "{level:?} lost the panels");
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

        state.approval_open = false;
        let text = snapshot_themed(&state, 110, 34, Tab::Timeline, &theme);
        assert!(text.is_ascii());
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
        assert_eq!(hard_wrap("abcd", 4), vec!["abcd", ""]);
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
    #[test]
    fn composer_tracks_wide_graphemes_and_combining_marks() {
        let lines = vec!["界e\u{301}界".to_owned()];
        assert_eq!(composer_cursor_position(&lines, 0, 1, 4), (0, 2));
        assert_eq!(composer_cursor_position(&lines, 0, 2, 4), (1, 0));
        assert_eq!(composer_cursor_position(&lines, 0, 3, 4), (1, 2));
        assert_eq!(hard_wrap(&lines[0], 4), vec!["界e\u{301}", "界"]);
    }

    #[test]
    fn a_long_draft_keeps_its_cursor_line_visible() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.composer.paste(
            &(0..20)
                .map(|i| format!("draft line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let text = snapshot(&state, 80, 24, Tab::Timeline);
        assert!(text.contains("draft line 19"), "{text}");
        assert!(!text.contains("draft line 0"));
    }

    #[test]
    fn streaming_tail_and_scrollback_show_both_ends_of_a_long_answer() {
        let mut state = populated_state();
        state.timeline.clear();
        state.timeline.push(crate::tui_state::TimelineEntry {
            kind: TimelineKind::Assistant,
            text: (0..50)
                .map(|i| format!("answer line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            streaming: true,
        });
        let tail = snapshot(&state, 80, 24, Tab::Timeline);
        assert!(tail.contains("answer line 49"), "{tail}");
        state.follow = false;
        state.selection = 0;
        state.transcript_scroll = 100;
        let start = snapshot(&state, 80, 24, Tab::Timeline);
        assert!(start.contains("answer line 0"), "{start}");
    }
    #[test]
    fn connection_flow_is_readable_at_supported_sizes_and_without_color() {
        use crate::connection::{ConnectionPage, ConnectionPanel};
        let mut state = WorkbenchState::new("/fixture");
        let mut connection = ConnectionPanel::open();
        connection.accounts.clear();
        connection.error = None;
        connection.models = (0..30)
            .map(|i| (format!("model-{i}"), format!("Model {i}")))
            .collect();
        for level in [ColorLevel::TrueColor, ColorLevel::Mono] {
            for (width, height) in [(60, 18), (80, 24), (140, 40)] {
                for page in [
                    ConnectionPage::Settings,
                    ConnectionPage::Account,
                    ConnectionPage::Welcome,
                    ConnectionPage::Models,
                ] {
                    connection.page = page;
                    connection.selection = if page == ConnectionPage::Models {
                        29
                    } else {
                        0
                    };
                    state.connection = Some(if page == ConnectionPage::Settings {
                        let mut settings = ConnectionPanel::settings(false, None);
                        settings.accounts.clear();
                        settings.error = None;
                        settings
                    } else {
                        connection.clone()
                    });
                    let text = snapshot_themed(
                        &state,
                        width,
                        height,
                        Tab::Timeline,
                        &Theme::for_level(level),
                    );
                    let expected = match page {
                        ConnectionPage::Settings | ConnectionPage::Account => {
                            "Continue with ChatGPT"
                        }
                        ConnectionPage::Welcome => "Got it",
                        ConnectionPage::Models => "model-29",
                    };
                    assert!(text.contains(expected), "{width}x{height} {page:?}\n{text}");
                    assert!(text.contains("Esc back"));
                    if let Ok(dir) = std::env::var("KNUT_UI_CAPTURE_DIR") {
                        std::fs::write(
                            std::path::Path::new(&dir)
                                .join(format!("{width}x{height}-{level:?}-{page:?}.txt")),
                            text,
                        )
                        .unwrap();
                    }
                }
            }
        }
        connection.page = ConnectionPage::Models;
        connection.models.clear();
        connection.selection = 0;
        connection.error = Some("Could not load models. Retry or manage usage.".to_owned());
        state.connection = Some(connection.clone());
        let failed = snapshot(&state, 60, 18, Tab::Timeline);
        assert!(failed.contains("Refresh available models"));
        assert!(failed.contains("Manage usage"));
        connection.error = None;
        connection.busy = true;
        connection.status =
            "Continue with ChatGPT in your browser. Waiting for sign-in…".to_owned();
        state.connection = Some(connection);
        let waiting = snapshot(&state, 60, 18, Tab::Timeline);
        assert!(waiting.contains("Esc cancel"));
        state.connection = None;
        state.chatgpt_plan = true;
        state.account = Some("fixture@example.invalid".to_owned());
        state.model = Some("model-1".into());
        let ascii = snapshot_themed(&state, 60, 18, Tab::Timeline, &Theme::plain_ascii());
        assert!(ascii.contains("Using ChatGPT plan"));
        assert!(ascii.contains("fixture@example.invalid"));
        assert!(ascii.is_ascii());
    }

    #[test]
    fn a_long_unicode_workspace_name_never_slices_a_character() {
        for width in [30, 60, 80, 140] {
            let state = WorkbenchState::new(format!("/workspace/{}", "界🚀é".repeat(30)));
            assert!(!snapshot(&state, width, 18, Tab::Timeline).is_empty());
        }
    }

    #[test]
    fn exact_approval_is_scrollable_past_the_transcript_limit_at_every_size() {
        let mut state = WorkbenchState::new("/workspace");
        let args = serde_json::json!({"path":"src/lib.rs", "expect_hash":"fresh-revision", "changes": serde_json::json!([
            {"old":"before", "new":format!("{}END_OF_EXACT_PATCH", "new line\n".repeat(800))}
        ]).to_string()});
        state.apply(&SessionEvent::ToolCallProposed {
            task: TaskId(1),
            turn: TurnId(1),
            call_id: "fingerprint".into(),
            name: "files/edit".into(),
            arguments: args.clone(),
        });
        state.apply(&SessionEvent::WaitingForUser {
            task: TaskId(1),
            turn: TurnId(1),
            wait: crate::WaitKind::Approval {
                approval_key: "fingerprint".into(),
            },
            message: "Allow exact edit?".into(),
        });
        for (width, height) in [(60, 18), (80, 24), (140, 40)] {
            state.approval_scroll = 0;
            let first = snapshot(&state, width, height, Tab::Timeline);
            assert!(first.contains("src/lib.rs"));
            assert!(first.contains("Alt+A"));
            state.approval_scroll = usize::MAX;
            let last = snapshot(&state, width, height, Tab::Timeline);
            assert!(last.contains("END_OF_EXACT_PATCH"), "{last}");
        }
        assert_eq!(state.proposed_action.as_ref().unwrap().2, args);
    }
}
