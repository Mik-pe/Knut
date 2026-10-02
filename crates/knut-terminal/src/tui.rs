use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::connection::{
    ConnectionAction, ConnectionJob, ConnectionOutcome, ConnectionPage, ConnectionPanel,
    ModelSource,
};
use crate::session::{SessionCommand, SessionEvent};
use crate::terminal::{TerminalGuard, install_signal_handler, shutdown_requested};
use crate::tui_render::Tab;
use crate::tui_state::{Focus, WorkbenchState};

/// What the shell should do after handling input.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellAction {
    /// Keep running.
    Continue,
    /// Leave the shell.
    Quit,
    /// Send a command to the session runtime.
    Command(SessionCommand),
    /// Run an implemented palette command by id.
    PaletteCommand(&'static str),
    /// Run the workspace's real checks and show what they proved.
    Verify,
    /// Open the review workspace over the session's recorded changes.
    Review,
    Connection(ConnectionAction),
    CancelConnection,
}

/// Apply one keystroke to the state, returning the next action.
///
/// Pure and synchronous, so keyboard behaviour is unit-testable without a
/// terminal.
pub fn handle_key(state: &mut WorkbenchState, key: KeyEvent, tab: &mut Tab) -> ShellAction {
    if key.kind == KeyEventKind::Release {
        return ShellAction::Continue;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if key.kind == KeyEventKind::Repeat && (ctrl || alt || key.code == KeyCode::Enter) {
        return ShellAction::Continue;
    }
    if let Some(panel) = &mut state.connection {
        if matches!(key.code, KeyCode::Esc) || (ctrl && key.code == KeyCode::Char('c')) {
            if panel.busy {
                return if panel.cancellable {
                    ShellAction::CancelConnection
                } else {
                    ShellAction::Continue
                };
            }
            if panel.page != panel.home && panel.page != ConnectionPage::Welcome {
                panel.page = panel.home;
                panel.selection = 0;
                panel.clear_query();
                panel.status = "Changes apply now and are saved on this device.".to_owned();
            } else {
                state.connection = None;
            }
            return ShellAction::Continue;
        }
        if !panel.busy {
            if panel.page == ConnectionPage::Models {
                match key.code {
                    KeyCode::Char('r') if ctrl => {
                        return ShellAction::Connection(panel.refresh_action());
                    }
                    KeyCode::Char('u') if alt && panel.model_source == ModelSource::ChatGpt => {
                        return ShellAction::Connection(ConnectionAction::ManageUsage);
                    }
                    KeyCode::Char('u') if ctrl => panel.clear_query(),
                    KeyCode::Backspace => panel.pop_query(),
                    KeyCode::Char(character) if !ctrl && !alt => panel.push_query(character),
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Up | KeyCode::BackTab => panel.move_selection(-1),
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    panel.move_selection(-1)
                }
                KeyCode::Down | KeyCode::Tab => panel.move_selection(1),
                KeyCode::Char('p') if ctrl => panel.move_selection(-1),
                KeyCode::Char('n') if ctrl => panel.move_selection(1),
                KeyCode::PageUp => panel.move_selection(-8),
                KeyCode::PageDown => panel.move_selection(8),
                KeyCode::Home => panel.selection = 0,
                KeyCode::End => panel.selection = panel.choices().len().saturating_sub(1),
                KeyCode::Enter => {
                    if let Some((_, action)) = panel.choices().get(panel.selection) {
                        return ShellAction::Connection(action.clone());
                    }
                }
                _ => {}
            }
        }
        return ShellAction::Continue;
    }
    if alt && key.code == KeyCode::Char('u') && state.usage_limit {
        return ShellAction::PaletteCommand("usage");
    }
    if key.code == KeyCode::F(2) || (ctrl && key.code == KeyCode::Char(',')) {
        state.help = false;
        state.close_palette();
        return ShellAction::PaletteCommand("settings");
    }
    if key.code == KeyCode::F(4) {
        state.help = false;
        state.close_palette();
        return ShellAction::PaletteCommand("model");
    }
    if ctrl && key.code == KeyCode::Char('c') {
        if state.help
            || state.palette_open()
            || state.review_open
            || state.approval_open
            || *tab != Tab::Timeline
        {
            state.help = false;
            state.approval_open = false;
            state.close_palette();
            state.review_open = false;
            *tab = Tab::Timeline;
            state.focus = Focus::Composer;
        } else if state.queue_edit.is_some() {
            state.restore_draft();
        } else if state.task_state.is_some_and(|s| !s.is_terminal()) {
            return ShellAction::Command(SessionCommand::Cancel);
        } else if !state.composer.is_empty() {
            state.clear_composer();
            state.status = Some("Draft cleared · Ctrl+C again to quit".to_owned());
        } else {
            return ShellAction::Quit;
        }
        return ShellAction::Continue;
    }
    if state.help {
        match key.code {
            KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') => state.help = false,
            KeyCode::Up => state.help_scroll = state.help_scroll.saturating_sub(1),
            KeyCode::PageUp => state.help_scroll = state.help_scroll.saturating_sub(4),
            KeyCode::Down | KeyCode::PageDown => {
                let step = if key.code == KeyCode::PageDown { 4 } else { 1 };
                state.help_scroll = (state.help_scroll + step)
                    .min(crate::tui_render::shortcuts(state).len().saturating_sub(1));
            }
            _ => {}
        }
        return ShellAction::Continue;
    }
    if state.palette_open() {
        return match key.code {
            KeyCode::Esc => {
                state.close_palette();
                ShellAction::Continue
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                state.palette_selection = state.palette_selection.saturating_sub(1);
                ShellAction::Continue
            }
            KeyCode::Up | KeyCode::BackTab => {
                state.palette_selection = state.palette_selection.saturating_sub(1);
                ShellAction::Continue
            }
            KeyCode::Down | KeyCode::Tab => {
                state.palette_selection = (state.palette_selection + 1)
                    .min(state.palette_results().len().saturating_sub(1));
                ShellAction::Continue
            }
            KeyCode::Char('p') if ctrl => {
                state.palette_selection = state.palette_selection.saturating_sub(1);
                ShellAction::Continue
            }
            KeyCode::Char('n') if ctrl => {
                state.palette_selection = (state.palette_selection + 1)
                    .min(state.palette_results().len().saturating_sub(1));
                ShellAction::Continue
            }
            KeyCode::Home => {
                state.palette_selection = 0;
                ShellAction::Continue
            }
            KeyCode::End => {
                state.palette_selection = state.palette_results().len().saturating_sub(1);
                ShellAction::Continue
            }
            KeyCode::PageUp => {
                state.palette_selection = state.palette_selection.saturating_sub(5);
                ShellAction::Continue
            }
            KeyCode::PageDown => {
                state.palette_selection = (state.palette_selection + 5)
                    .min(state.palette_results().len().saturating_sub(1));
                ShellAction::Continue
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(query) = state.palette.as_mut() {
                    query.clear();
                }
                state.palette_selection = 0;
                ShellAction::Continue
            }
            KeyCode::Backspace => {
                if let Some(query) = state.palette.as_mut()
                    && let Some((index, _)) = query.grapheme_indices(true).next_back()
                {
                    query.truncate(index);
                }
                state.palette_selection = 0;
                ShellAction::Continue
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Some(query) = state.palette.as_mut()
                    && query.chars().count() < 256
                {
                    query.push(c);
                }
                state.palette_selection = 0;
                ShellAction::Continue
            }
            KeyCode::Enter => {
                let selected = state
                    .palette_results()
                    .get(state.palette_selection)
                    .cloned();
                match selected {
                    Some(command) => {
                        state.close_palette();
                        palette_action(state, tab, command.id)
                    }
                    _ => ShellAction::Continue,
                }
            }
            _ => ShellAction::Continue,
        };
    }
    if key.code == KeyCode::F(3) {
        state.approval_open = false;
        state.detail_scroll = 0;
        *tab = if *tab == Tab::Inspector {
            Tab::Timeline
        } else {
            Tab::Inspector
        };
        return ShellAction::Continue;
    }
    if key.code == KeyCode::F(1)
        || (key.code == KeyCode::Char('?')
            && !ctrl
            && !alt
            && (state.composer.text().is_empty() || state.review_open))
    {
        state.help = true;
        state.help_scroll = 0;
        return ShellAction::Continue;
    }
    if ctrl {
        return match key.code {
            KeyCode::Left if !state.review_open && state.focus == Focus::Composer => {
                state.composer.word_left();
                ShellAction::Continue
            }
            KeyCode::Right if !state.review_open && state.focus == Focus::Composer => {
                state.composer.word_right();
                ShellAction::Continue
            }
            KeyCode::Char('w') | KeyCode::Backspace
                if !state.review_open && state.focus == Focus::Composer =>
            {
                state.composer.delete_word_left();
                ShellAction::Continue
            }
            KeyCode::Delete if !state.review_open && state.focus == Focus::Composer => {
                state.composer.delete_word_right();
                ShellAction::Continue
            }
            KeyCode::Char('u') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.delete_to_line_start();
                ShellAction::Continue
            }
            KeyCode::Char('k') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.delete_to_line_end();
                ShellAction::Continue
            }
            KeyCode::Char('p') => {
                state.open_palette();
                ShellAction::Continue
            }
            KeyCode::Char('r') => ShellAction::Review,
            KeyCode::Char('o') => {
                state.approval_open = false;
                state.detail_scroll = 0;
                *tab = if *tab == Tab::Tasks {
                    Tab::Timeline
                } else {
                    Tab::Tasks
                };
                state.focus = Focus::Composer;
                ShellAction::Continue
            }
            KeyCode::Char('b') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.left();
                ShellAction::Continue
            }
            KeyCode::Char('f') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.right();
                ShellAction::Continue
            }
            KeyCode::Char('d')
                if state.composer.is_empty()
                    && !state.task_state.is_some_and(|s| !s.is_terminal()) =>
            {
                ShellAction::Quit
            }
            KeyCode::Char('q') => {
                if state.queue_edit.is_some() {
                    state.status =
                        Some("Enter saves the queue edit; Esc restores your draft".to_owned());
                    ShellAction::Continue
                } else if state.task_state.is_some_and(|task| !task.is_terminal()) {
                    state.status = Some("Ctrl+C stops the active task before closing".to_owned());
                    ShellAction::Continue
                } else {
                    ShellAction::Quit
                }
            }
            KeyCode::Char('a') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.home();
                ShellAction::Continue
            }
            KeyCode::Char('e') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.end();
                ShellAction::Continue
            }
            KeyCode::Char('j') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.insert_newline();
                ShellAction::Continue
            }
            KeyCode::Char('z' | 'Z')
                if !state.review_open
                    && state.focus == Focus::Composer
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                state.composer.redo();
                ShellAction::Continue
            }
            KeyCode::Char('z') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.undo();
                ShellAction::Continue
            }
            KeyCode::Char('y') if !state.review_open && state.focus == Focus::Composer => {
                state.composer.redo();
                ShellAction::Continue
            }
            KeyCode::End => {
                state.resume_follow();
                ShellAction::Continue
            }
            KeyCode::Char('l') => {
                state.review_open = false;
                state.approval_open = false;
                state.focus = Focus::Composer;
                state.resume_follow();
                *tab = Tab::Timeline;
                ShellAction::Continue
            }
            _ => ShellAction::Continue,
        };
    }
    if alt {
        if key.code == KeyCode::Enter && !state.review_open && state.focus == Focus::Composer {
            state.composer.insert_newline();
            return ShellAction::Continue;
        }
        if !state.review_open && state.focus == Focus::Composer {
            match key.code {
                KeyCode::Char('b') | KeyCode::Left => {
                    state.composer.word_left();
                    return ShellAction::Continue;
                }
                KeyCode::Char('f') | KeyCode::Right => {
                    state.composer.word_right();
                    return ShellAction::Continue;
                }
                KeyCode::Backspace => {
                    state.composer.delete_word_left();
                    return ShellAction::Continue;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Char('s')
            && state.queue_edit.is_none()
            && state.task_state.is_some_and(|state| !state.is_terminal())
        {
            state.steer_draft = !state.steer_draft;
            state.approval_open = false;
            state.status = Some(
                if state.steer_draft {
                    "Steer current task · Alt+S switches to queue"
                } else {
                    "Queue next task · Alt+S switches to steer"
                }
                .to_owned(),
            );
            return ShellAction::Continue;
        }
        if *tab == Tab::Tasks
            && let Some(request) = state.queued.get(state.queued_selection)
        {
            match key.code {
                KeyCode::Char('e') => {
                    state.edit_queued();
                    return ShellAction::Continue;
                }
                KeyCode::Char('x') => {
                    return ShellAction::Command(SessionCommand::RemoveQueued { id: request.id });
                }
                KeyCode::Char('r') => {
                    return ShellAction::Command(SessionCommand::RunQueued { id: request.id });
                }
                _ => {}
            }
        }
        if let Some(pending) = &state.pending
            && let crate::session::WaitKind::Approval { approval_key } = &pending.kind
        {
            return match key.code {
                KeyCode::Char('v') => {
                    state.approval_open = !state.approval_open;
                    ShellAction::Continue
                }
                KeyCode::Char('a') => ShellAction::Command(SessionCommand::Approve {
                    approval_key: approval_key.clone(),
                }),
                KeyCode::Char('d') => ShellAction::Command(SessionCommand::Deny {
                    approval_key: approval_key.clone(),
                }),
                _ => ShellAction::Continue,
            };
        }
        return ShellAction::Continue;
    }
    if key.code == KeyCode::Esc {
        state.approval_open = false;
        state.restore_draft();
        state.review_open = false;
        *tab = Tab::Timeline;
        state.focus = Focus::Composer;
        state.resume_follow();
        return ShellAction::Continue;
    }
    if state.review_open
        && let Some(review) = state.review.as_mut()
    {
        match key.code {
            KeyCode::PageUp => review.scroll = review.scroll.saturating_sub(10),
            KeyCode::PageDown => review.scroll = review.scroll.saturating_add(10),
            KeyCode::Down | KeyCode::Char('j') => review.next_hunk(),
            KeyCode::Up | KeyCode::Char('k') => review.previous_hunk(),
            KeyCode::Right | KeyCode::Char('n') => review.next_file(),
            KeyCode::Left | KeyCode::Char('p') => review.previous_file(),
            _ => {}
        }
        return ShellAction::Continue;
    }
    match key.code {
        KeyCode::PageUp if state.approval_open => {
            state.approval_scroll = state.approval_scroll.saturating_sub(10);
        }
        KeyCode::PageDown if state.approval_open => {
            state.approval_scroll = state.approval_scroll.saturating_add(10);
        }
        KeyCode::PageUp if *tab != Tab::Timeline => {
            state.detail_scroll = state.detail_scroll.saturating_sub(10);
        }
        KeyCode::PageDown if *tab != Tab::Timeline => {
            state.detail_scroll = state.detail_scroll.saturating_add(10);
        }
        KeyCode::PageUp => {
            state.follow = false;
            state.transcript_scroll = state.transcript_scroll.saturating_add(10);
        }
        KeyCode::PageDown => {
            state.transcript_scroll = state.transcript_scroll.saturating_sub(10);
            if state.transcript_scroll == 0 {
                state.resume_follow();
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            state.focus = if state.focus == Focus::Composer {
                Focus::Timeline
            } else {
                Focus::Composer
            };
            *tab = Tab::Timeline;
        }
        KeyCode::Up if *tab == Tab::Tasks && state.queue_edit.is_none() => {
            state.queued_selection = state.queued_selection.saturating_sub(1);
            state.detail_scroll = state.queued_selection.saturating_sub(3) as u16;
        }
        KeyCode::Down if *tab == Tab::Tasks && state.queue_edit.is_none() => {
            state.queued_selection =
                (state.queued_selection + 1).min(state.queued.len().saturating_sub(1));
            state.detail_scroll = state.queued_selection.saturating_sub(3) as u16;
        }
        KeyCode::Up if state.focus != Focus::Composer => {
            state.follow = false;
            state.selection = state.selection.saturating_sub(1);
        }
        KeyCode::Down if state.focus != Focus::Composer => {
            state.selection = (state.selection + 1).min(state.timeline.len().saturating_sub(1));
            state.follow = state.selection + 1 >= state.timeline.len();
        }
        KeyCode::Home if state.focus != Focus::Composer => {
            state.follow = false;
            state.selection = 0;
        }
        KeyCode::End if state.focus != Focus::Composer => state.resume_follow(),
        KeyCode::Enter if state.focus != Focus::Composer => {
            state.focus = Focus::Composer;
            *tab = Tab::Timeline;
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Delete | KeyCode::Backspace
            if state.focus != Focus::Composer => {}
        KeyCode::Up => state.composer.up(),
        KeyCode::Down => state.composer.down(),
        KeyCode::Left => state.composer.left(),
        KeyCode::Right => state.composer.right(),
        KeyCode::Home => state.composer.home(),
        KeyCode::End => state.composer.end(),
        KeyCode::Delete => state.composer.delete(),
        KeyCode::Backspace => state.composer.backspace(),
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.composer.insert_newline()
        }
        KeyCode::Enter => {
            *tab = Tab::Timeline;
            state.focus = Focus::Composer;
            return submit_composer(state);
        }
        KeyCode::Char('/') if state.composer.is_empty() => state.open_palette(),
        KeyCode::Char(c) => {
            state.focus = Focus::Composer;
            *tab = Tab::Timeline;
            state.composer.insert(&c.to_string());
        }
        _ => {}
    }
    ShellAction::Continue
}

fn submit_composer(state: &mut WorkbenchState) -> ShellAction {
    if let Some((id, _)) = &state.queue_edit {
        let id = *id;
        if !state.queued.iter().any(|request| request.id == id) {
            state.status = Some(
                "This request is no longer queued. Copy your edit or Esc to restore your draft."
                    .to_owned(),
            );
            return ShellAction::Continue;
        }
        let prompt = state.composer.text();
        if prompt.trim().is_empty() {
            return ShellAction::Continue;
        }
        state.status = Some("Saving queued request...".to_owned());
        return ShellAction::Command(SessionCommand::UpdateQueued {
            id,
            prompt,
            options: state
                .queued
                .iter()
                .find(|request| request.id == id)
                .map(|request| request.options.clone())
                .unwrap_or_default(),
        });
    }
    if state
        .pending
        .as_ref()
        .is_some_and(|p| matches!(p.kind, crate::session::WaitKind::Approval { .. }))
        && !state.steer_draft
    {
        state.status = Some("Approval needed: Alt+A allow · Alt+D deny. Draft kept.".to_owned());
        return ShellAction::Continue;
    }
    let Some(text) = state.composer.take_submission() else {
        return ShellAction::Continue;
    };
    state.resume_follow();
    if state.pending.is_some() && !state.steer_draft {
        ShellAction::Command(SessionCommand::Answer { value: text })
    } else if state.task_state.is_some_and(|s| !s.is_terminal()) {
        if std::mem::take(&mut state.steer_draft) {
            ShellAction::Command(SessionCommand::Steer { prompt: text })
        } else {
            ShellAction::Command(SessionCommand::Queue {
                prompt: text,
                options: Default::default(),
            })
        }
    } else {
        ShellAction::Command(SessionCommand::Submit {
            prompt: text,
            options: Default::default(),
        })
    }
}

fn palette_action(state: &mut WorkbenchState, tab: &mut Tab, id: &'static str) -> ShellAction {
    match id {
        "motion" => ShellAction::Connection(ConnectionAction::ToggleMotion),
        "steer" | "queue" => {
            state.steer_draft =
                id == "steer" && state.task_state.is_some_and(|state| !state.is_terminal());
            state.approval_open = false;
            state.focus = Focus::Composer;
            *tab = Tab::Timeline;
            state.status = Some(
                if state.steer_draft {
                    "Enter steers the current task"
                } else {
                    "Enter queues a new task"
                }
                .to_owned(),
            );
            ShellAction::Continue
        }
        "submit" => {
            *tab = Tab::Timeline;
            state.focus = Focus::Composer;
            submit_composer(state)
        }
        "review" => ShellAction::Review,
        "verify" => ShellAction::Verify,
        "jobs" => {
            *tab = Tab::Tasks;
            ShellAction::Continue
        }
        "decisions" => {
            *tab = Tab::Inspector;
            ShellAction::Continue
        }
        "help" => {
            state.help = true;
            state.help_scroll = 0;
            ShellAction::Continue
        }
        _ => ShellAction::PaletteCommand(id),
    }
}

/// The result of an on-demand check run.
///
/// Deliberately a plain message rather than a shared future: the shell
/// renders it in the review pane and the status line, and a failed run is
/// reported as a failure rather than as an empty set of checks.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckOutcome {
    /// Real checks ran; each row is one check with its evidence.
    Ran {
        rows: Vec<crate::review::CheckRow>,
        revision: String,
    },
    /// The checks could not run at all, with the reason.
    Unavailable(String),
}

/// Run the workspace's checks on a blocking thread.
///
/// `run_report` is async and spawns processes; the shell is a separate
/// runtime task, so this hops to a blocking worker and returns a plain
/// message.
async fn run_checks_blocking() -> CheckOutcome {
    let result = tokio::task::spawn_blocking(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("cannot start the check runtime: {err}"))?;
        let workspace =
            crate::Workspace::open(".").map_err(|err| format!("no workspace to check: {err}"))?;
        let supervisor = Arc::new(crate::Supervisor::new(workspace.clone()));
        let profile = crate::CheckProfile::for_workspace(&workspace)
            .map_err(|err| format!("checks unavailable: {err}"))?;
        let runner = crate::CheckRunner::new(workspace, supervisor, profile);
        let report = runtime
            .block_on(runner.run_report())
            .map_err(|err| format!("cannot verify the workspace: {err}"))?;
        Ok::<_, String>((
            report
                .checks
                .iter()
                .map(|check| {
                    crate::review::CheckRow::from_evidence(check, &report.revision.revision)
                })
                .collect::<Vec<_>>(),
            report.revision.revision,
        ))
    })
    .await;

    match result {
        Ok(Ok((rows, revision))) => CheckOutcome::Ran { rows, revision },
        Ok(Err(reason)) => CheckOutcome::Unavailable(reason),
        Err(err) => CheckOutcome::Unavailable(format!("the check task failed: {err}")),
    }
}

/// Run the shell over a channel of session events.
///
/// `events` is drained without blocking: a slow provider cannot stop
/// typing, navigation or inspection, because the loop never waits on the
/// runtime to paint.
pub async fn run_shell(
    mut state: WorkbenchState,
    mut events: tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    commands: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    connections: tokio::sync::mpsc::UnboundedSender<crate::ConnectionRequest>,
) -> std::io::Result<()> {
    // Signal-driven shutdown, so a Ctrl+C delivered as a signal (or a
    // closing terminal) still restores the display.
    install_signal_handler();

    let mut preferences = match crate::TerminalPreferences::load() {
        Ok(preferences) => preferences,
        Err(error) => {
            state.status = Some(format!("Appearance settings unavailable: {error}"));
            crate::TerminalPreferences::default()
        }
    };
    state.theme = preferences.apply(state.theme);

    let mut memory = match crate::persist::EditorMemory::open(
        &crate::persist::session_store_path(),
        std::path::Path::new(&state.workspace),
    ) {
        Ok((memory, composer)) => {
            if !composer.text().is_empty() {
                state.status = Some("Draft restored - Up/Down recalls prompt history".to_owned());
            }
            state.composer = composer;
            Some(memory)
        }
        Err(err) => {
            state.status = Some(format!("Draft saving unavailable: {err}"));
            None
        }
    };

    let mut guard = TerminalGuard::enter()?;
    // Crossterm also reads NO_COLOR; use the theme's already-resolved explicit override.
    crossterm::style::force_color_output(state.theme.level.is_color());
    let outcome = run_loop(
        &mut state,
        &mut events,
        commands,
        connections,
        &mut guard,
        memory.as_mut(),
        &mut preferences,
    )
    .await;
    guard.restore();
    if let Some(mut memory) = memory {
        let composer = state
            .queue_edit
            .as_ref()
            .map_or(&state.composer, |(_, draft)| draft);
        memory.checkpoint(composer, true);
        if let Some(error) = memory.finish().await {
            eprintln!("Draft saving stopped: {error}");
        }
    }
    outcome
}

async fn run_loop(
    state: &mut WorkbenchState,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    commands: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    connections: tokio::sync::mpsc::UnboundedSender<crate::ConnectionRequest>,
    guard: &mut TerminalGuard,
    mut memory: Option<&mut crate::persist::EditorMemory>,
    preferences: &mut crate::TerminalPreferences,
) -> std::io::Result<()> {
    let mut tab = Tab::Timeline;
    if state.chatgpt_plan && crate::openai_auth::needs_plan_notice().unwrap_or(false) {
        let mut panel = ConnectionPanel::open();
        panel.page = ConnectionPage::Welcome;
        panel.return_to_conversation = true;
        state.connection = Some(panel);
    }
    let mut connection_job: Option<ConnectionJob> = None;

    // Checks run on demand and are *not* on the keystroke path: the loop
    // stays responsive, and a finished run arrives as a message.
    let (check_tx, mut check_rx) = tokio::sync::mpsc::unbounded_channel::<CheckOutcome>();
    let mut checks_running = false;
    let animation_start = std::time::Instant::now();
    let mut redraw = true;
    let mut last_elapsed = 0;

    loop {
        // Drain every event already published, then draw once: coalescing
        // high-frequency updates instead of painting per event.
        while let Ok(event) = events.try_recv() {
            state.apply(&event);
            redraw = true;
        }
        while let Ok(outcome) = check_rx.try_recv() {
            checks_running = false;
            state.apply_check_outcome(outcome);
            redraw = true;
        }

        if let Some(job) = &mut connection_job {
            while let Ok(status) = job.progress.try_recv() {
                if let Some(panel) = &mut state.connection {
                    panel.status = status.to_owned();
                    redraw = true;
                }
            }
        }
        if connection_job
            .as_ref()
            .and_then(|job| job.handle.as_ref())
            .is_some_and(|handle| handle.is_finished())
        {
            let handle = connection_job.take().unwrap().handle.take().unwrap();
            let outcome = handle.await.unwrap_or_else(|_| {
                Err(crate::KnutError::Model(
                    "Connection task interrupted; retry".to_owned(),
                ))
            });
            finish_connection(state, outcome);
            redraw = true;
        }

        if let Some(memory) = memory.as_mut() {
            let composer = state
                .queue_edit
                .as_ref()
                .map_or(&state.composer, |(_, draft)| draft);
            memory.checkpoint(composer, false);
            if let Some(error) = memory.error() {
                state.status = Some(format!("Draft saving stopped: {error}"));
                redraw = true;
            }
        }

        // Input can wake the loop early; animation speed must not depend on typing speed.
        state.tick = (animation_start.elapsed().as_millis() / 50) as u64;
        let elapsed = state.elapsed_secs();
        if redraw || state.animating() || elapsed != last_elapsed {
            guard
                .terminal()
                .draw(|frame| crate::tui_render::render_themed(frame, state, tab, &state.theme))?;
            redraw = false;
            last_elapsed = elapsed;
        }

        if crossterm::event::poll(Duration::from_millis(50))? {
            redraw = true;
            match crossterm::event::read()? {
                Event::Key(key) => {
                    // In raw mode the terminal does not translate Ctrl+C
                    // into a signal, so the quit path must handle it — and
                    // a signal delivered from outside must break the loop
                    // too, or the shell would keep running after Ctrl+C.
                    if shutdown_requested() {
                        break;
                    }
                    match handle_key(state, key, &mut tab) {
                        ShellAction::Quit => break,
                        ShellAction::Command(command) => {
                            let _ = commands.send(command);
                        }
                        ShellAction::PaletteCommand("pause") => {
                            let _ = commands.send(SessionCommand::Pause);
                        }
                        ShellAction::PaletteCommand("resume") => {
                            let _ = commands.send(SessionCommand::Resume);
                        }
                        ShellAction::PaletteCommand("cancel") => {
                            let _ = commands.send(SessionCommand::Cancel);
                        }
                        ShellAction::PaletteCommand("settings") => {
                            state.connection = Some(ConnectionPanel::settings(
                                state.chatgpt_plan,
                                state.model.as_deref(),
                                &state.theme,
                            ));
                        }
                        ShellAction::PaletteCommand("account") => {
                            state.connection = Some(ConnectionPanel::open());
                        }
                        ShellAction::PaletteCommand("model") => {
                            let mut panel = ConnectionPanel::settings(
                                state.chatgpt_plan,
                                state.model.as_deref(),
                                &state.theme,
                            );
                            panel.home = ConnectionPage::Models;
                            let source = panel.current_source;
                            state.connection = Some(panel);
                            start_connection(
                                state,
                                match source {
                                    ModelSource::ChatGpt => ConnectionAction::Models,
                                    ModelSource::Environment => ConnectionAction::EnvironmentModels,
                                },
                                &connections,
                                &mut connection_job,
                            );
                        }
                        ShellAction::PaletteCommand("usage") => {
                            state.connection = Some(ConnectionPanel::open());
                            start_connection(
                                state,
                                ConnectionAction::ManageUsage,
                                &connections,
                                &mut connection_job,
                            );
                        }
                        ShellAction::Connection(action) => {
                            if matches!(
                                action,
                                ConnectionAction::ToggleMotion | ConnectionAction::ToggleBackground
                            ) {
                                apply_appearance(state, &action, preferences);
                            } else {
                                start_connection(state, action, &connections, &mut connection_job);
                            }
                        }
                        ShellAction::CancelConnection => {
                            connection_job = None;
                            if let Some(panel) = &mut state.connection {
                                panel.busy = false;
                                match crate::openai_auth::account_list() {
                                    Ok(accounts) => panel.accounts = accounts,
                                    Err(error) => panel.error = Some(error.to_string()),
                                }
                                panel.selection = 0;
                                panel.status = if panel.page == ConnectionPage::Models {
                                    "Model loading cancelled. Press Ctrl+R to retry."
                                } else if panel.status.starts_with("Signed in.")
                                    || panel.status.starts_with("Account selected.")
                                {
                                    "Stopped loading models. Open Choose model to continue."
                                } else {
                                    "Sign-in cancelled. You can try again."
                                }
                                .to_owned();
                            }
                        }
                        ShellAction::PaletteCommand("doctor") => {
                            // The shell already knows its own engine
                            // configuration; reporting it needs no
                            // subprocess and no invented task.
                            state.show_setup();
                            tab = Tab::Timeline;
                        }
                        ShellAction::Verify => {
                            // The checks are the binary's real ones, run
                            // off the paint path; the result comes back as
                            // a message, never as a blocking call here.
                            if !checks_running {
                                checks_running = true;
                                state.status = Some("running checks…".to_owned());
                                let tx = check_tx.clone();
                                tokio::spawn(async move {
                                    let outcome = run_checks_blocking().await;
                                    let _ = tx.send(outcome);
                                });
                            }
                        }
                        ShellAction::Review => state.toggle_review(),
                        ShellAction::PaletteCommand(_) | ShellAction::Continue => {}
                    }
                }
                Event::Paste(text) => {
                    if let Some(panel) = &mut state.connection {
                        if panel.page == ConnectionPage::Models && !panel.busy {
                            panel.set_query(format!("{}{}", panel.query, text.trim()));
                        }
                    } else if let Some(query) = state.palette.as_mut() {
                        query.extend(
                            text.trim()
                                .chars()
                                .take(256usize.saturating_sub(query.chars().count())),
                        );
                        state.palette_selection = 0;
                    } else if !state.help && !state.review_open {
                        state.composer.paste(&text);
                        if state.composer.last_paste_truncated {
                            state.status = Some(
                                "Paste shortened · Ctrl+Z undo · review before sending".to_owned(),
                            );
                        }
                        state.focus = Focus::Composer;
                        tab = Tab::Timeline;
                    }
                }
                Event::Resize(cols, rows) => {
                    guard
                        .terminal()
                        .resize(ratatui::layout::Rect::new(0, 0, cols, rows))?;
                }
                _ => {}
            }
        }

        // An external interrupt (SIGINT/SIGTERM, a terminal closing) must
        // exit cleanly so the guard restores the terminal.
        if shutdown_requested() {
            break;
        }
    }
    Ok(())
}

fn apply_appearance(
    state: &mut WorkbenchState,
    action: &ConnectionAction,
    preferences: &mut crate::TerminalPreferences,
) {
    let result = match action {
        ConnectionAction::ToggleMotion => preferences.toggled_motion(&state.theme),
        ConnectionAction::ToggleBackground => preferences.toggled_background(&state.theme),
        _ => return,
    }
    .and_then(|updated| {
        updated.save()?;
        Ok(updated)
    });
    match result {
        Ok(updated) => {
            state.theme = updated.apply(state.theme);
            *preferences = updated;
            state.status = Some("Appearance saved".to_owned());
            if let Some(panel) = &mut state.connection {
                panel.sync_from_theme(&state.theme);
                panel.error = None;
                panel.status = "Appearance saved for future sessions.".to_owned();
            }
        }
        Err(error) => {
            state.status = Some(error.to_string());
            if let Some(panel) = &mut state.connection {
                panel.error = Some(error.to_string());
            }
        }
    }
}

fn start_connection(
    state: &mut WorkbenchState,
    action: ConnectionAction,
    connections: &tokio::sync::mpsc::UnboundedSender<crate::ConnectionRequest>,
    job: &mut Option<ConnectionJob>,
) {
    let Some(panel) = &mut state.connection else {
        return;
    };
    if panel.busy {
        return;
    }
    if action == ConnectionAction::OpenAccount {
        panel.page = ConnectionPage::Account;
        panel.selection = 0;
        panel.status = "Connect your ChatGPT plan to Knut's agent harness.".to_owned();
        panel.error = None;
        return;
    }
    if state.task_state.is_some_and(|state| !state.is_terminal())
        && !matches!(
            action,
            ConnectionAction::ManageUsage
                | ConnectionAction::Models
                | ConnectionAction::EnvironmentModels
                | ConnectionAction::Acknowledge
        )
    {
        panel.error =
            Some("Finish or cancel the current task before changing the connection.".to_owned());
        return;
    }
    match action {
        ConnectionAction::Models => panel.begin_models(ModelSource::ChatGpt),
        ConnectionAction::EnvironmentModels => panel.begin_models(ModelSource::Environment),
        ConnectionAction::Login(_) | ConnectionAction::SelectAccount(_) => {
            panel.models.clear();
            panel.clear_query();
            panel.model_source = ModelSource::ChatGpt;
        }
        _ => {}
    }
    panel.busy = true;
    panel.error = None;
    panel.cancellable = matches!(
        action,
        ConnectionAction::Login(_)
            | ConnectionAction::SelectAccount(_)
            | ConnectionAction::Models
            | ConnectionAction::EnvironmentModels
            | ConnectionAction::ManageUsage
    );
    panel.status = match &action {
        ConnectionAction::Login(_) => "Continue with ChatGPT in your browser. Waiting for sign-in…",
        ConnectionAction::Models | ConnectionAction::SelectAccount(_) => {
            "Loading models available to this account…"
        }
        ConnectionAction::EnvironmentModels => "Loading models from your API provider…",
        ConnectionAction::SelectModel(_) | ConnectionAction::SelectEnvironmentModel(_) => {
            "Connecting the model…"
        }
        ConnectionAction::Logout => "Signing out…",
        ConnectionAction::ManageUsage => "Opening ChatGPT usage…",
        ConnectionAction::Acknowledge => "Saving…",
        ConnectionAction::Environment => "Saving environment configuration…",
        ConnectionAction::OpenAccount => unreachable!("navigation was handled above"),
        ConnectionAction::ToggleMotion | ConnectionAction::ToggleBackground => {
            unreachable!("appearance changes are handled locally")
        }
    }
    .to_owned();
    let connections = connections.clone();
    let (progress, updates) = tokio::sync::mpsc::unbounded_channel();
    *job = Some(ConnectionJob {
        progress: updates,
        handle: Some(tokio::spawn(crate::connection::execute(
            action,
            connections,
            state.chatgpt_plan,
            progress,
        ))),
    });
}

fn finish_connection(
    state: &mut WorkbenchState,
    outcome: Result<ConnectionOutcome, crate::KnutError>,
) {
    let Some(panel) = &mut state.connection else {
        return;
    };
    panel.busy = false;
    match crate::openai_auth::account_list() {
        Ok(accounts) => panel.accounts = accounts,
        Err(error) => panel.error = Some(error.to_string()),
    }
    match outcome {
        Ok(ConnectionOutcome::Models {
            models,
            source,
            notice,
            error,
        }) => {
            panel.begin_models(source);
            if let Some(models) = models {
                panel.models = models;
                panel.select_current();
            }
            panel.page = if notice {
                ConnectionPage::Welcome
            } else {
                ConnectionPage::Models
            };
            panel.clamp_selection();
            panel.error = error;
            panel.status = "Choose a model. Access is checked when a task runs.".to_owned();
        }
        Ok(ConnectionOutcome::Connected {
            model,
            endpoint,
            plan,
            account,
        }) => {
            state.model = Some(model);
            state.endpoint = Some(endpoint);
            state.chatgpt_plan = plan;
            state.usage_limit = false;
            state.account = account;
            state.unavailable = None;
            state.status = Some(
                if plan {
                    "Using ChatGPT plan · F2 settings"
                } else {
                    "Using API connection · F2 settings"
                }
                .to_owned(),
            );
            state.connection = None;
        }
        Ok(ConnectionOutcome::LoggedOut(revoked)) => {
            if state.chatgpt_plan {
                state.model = None;
                state.endpoint = None;
                state.chatgpt_plan = false;
                state.usage_limit = false;
                state.unavailable = Some("Signed out. Open / account to connect again.".to_owned());
            }
            state.account = None;
            panel.page = ConnectionPage::Account;
            panel.selection = 0;
            panel.status = if revoked { "Signed out." } else { "Local credentials removed. Remote revocation was not confirmed; manage connected apps in ChatGPT." }.to_owned();
        }
        Ok(ConnectionOutcome::Acknowledged) => {
            if panel.return_to_conversation {
                state.connection = None;
            } else {
                panel.page = ConnectionPage::Models;
                panel.selection = 0;
            }
        }
        Ok(ConnectionOutcome::BrowserOpened) => {
            panel.status = "ChatGPT usage opened in your browser.".to_owned()
        }
        Err(error) => {
            panel.status = "Try again, or press Esc to return to your conversation.".to_owned();
            panel.error = Some(error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{TaskId, TurnId, WaitKind};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn motion_and_scrolling_help_keep_the_draft_untouched() {
        let mut state = WorkbenchState::new("/workspace");
        state.composer.paste("unfinished 👩‍💻 draft");
        let mut tab = Tab::Timeline;
        assert_eq!(
            palette_action(&mut state, &mut tab, "motion"),
            ShellAction::Connection(ConnectionAction::ToggleMotion)
        );
        handle_key(&mut state, key(KeyCode::F(1)), &mut tab);
        handle_key(&mut state, key(KeyCode::PageDown), &mut tab);
        assert_eq!(state.help_scroll, 4);
        for _ in 0..30 {
            handle_key(&mut state, key(KeyCode::Down), &mut tab);
        }
        assert_eq!(
            state.help_scroll,
            crate::tui_render::shortcuts(&state).len() - 1
        );
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        handle_key(&mut state, key(KeyCode::F(1)), &mut tab);
        assert_eq!(state.help_scroll, 0);
        assert_eq!(state.composer.text(), "unfinished 👩‍💻 draft");
    }

    #[test]
    fn typing_builds_the_composer_and_enter_submits() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Composer;
        let mut tab = Tab::Timeline;

        for c in "fix the test".chars() {
            assert_eq!(
                handle_key(&mut state, key(KeyCode::Char(c)), &mut tab),
                ShellAction::Continue
            );
        }
        assert_eq!(state.composer_text(), "fix the test");

        let action = handle_key(&mut state, key(KeyCode::Enter), &mut tab);
        assert_eq!(
            action,
            ShellAction::Command(SessionCommand::Submit {
                prompt: "fix the test".to_owned(),
                options: Default::default(),
            })
        );
        // The composer is cleared after submitting.
        assert!(state.is_composer_empty());
    }

    #[test]
    fn familiar_line_editing_shortcuts_do_not_open_overlays() {
        let mut state = WorkbenchState::new("/workspace");
        let mut tab = Tab::Timeline;
        state.composer.insert("a\u{030a} tail");
        handle_key(&mut state, ctrl('a'), &mut tab);
        handle_key(&mut state, ctrl('f'), &mut tab);
        assert_eq!(state.composer.cursor(), (0, 1));
        handle_key(&mut state, ctrl('b'), &mut tab);
        assert_eq!(state.composer.cursor(), (0, 0));
        assert_eq!(tab, Tab::Timeline);
        handle_key(&mut state, ctrl('f'), &mut tab);
        handle_key(&mut state, ctrl('k'), &mut tab);
        assert_eq!(state.composer.text(), "a\u{030a}");
        assert!(!state.palette_open());
        handle_key(&mut state, ctrl('z'), &mut tab);
        handle_key(&mut state, ctrl('u'), &mut tab);
        assert_eq!(state.composer.text(), " tail");
        handle_key(&mut state, ctrl('z'), &mut tab);
        handle_key(
            &mut state,
            KeyEvent::new(
                KeyCode::Char('Z'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            &mut tab,
        );
        assert_eq!(state.composer.text(), " tail");
    }

    #[test]
    fn focused_transcript_navigation_never_edits_or_sends_the_draft() {
        let mut state = WorkbenchState::new("/workspace");
        state.composer.insert("keep this draft");
        state.focus = Focus::Timeline;
        let before = state.composer.clone();
        let mut tab = Tab::Timeline;
        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Delete,
            KeyCode::Backspace,
        ] {
            assert_eq!(
                handle_key(&mut state, key(code), &mut tab),
                ShellAction::Continue
            );
        }
        for shortcut in ['a', 'e', 'b', 'f', 'k', 'u', 'w', 'z', 'y', 'j'] {
            assert_eq!(
                handle_key(&mut state, ctrl(shortcut), &mut tab),
                ShellAction::Continue
            );
        }
        assert_eq!(state.composer, before);
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::Continue
        );
        assert_eq!(state.focus, Focus::Composer);
        assert_eq!(state.composer, before);
    }

    #[test]
    fn alternate_enter_adds_a_newline_and_settings_are_global() {
        let mut state = WorkbenchState::new("/workspace");
        state.composer.insert("first");
        let mut tab = Tab::Timeline;
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
                &mut tab
            ),
            ShellAction::Continue
        );
        assert_eq!(state.composer.text(), "first\n");
        state.open_palette();
        assert_eq!(
            handle_key(&mut state, ctrl(','), &mut tab),
            ShellAction::PaletteCommand("settings")
        );
        assert!(!state.palette_open());
        state.help = true;
        assert_eq!(
            handle_key(&mut state, key(KeyCode::F(4)), &mut tab),
            ShellAction::PaletteCommand("model")
        );
        assert!(!state.help);
        assert_eq!(state.composer.text(), "first\n");
    }

    #[test]
    fn question_mark_opens_shortcuts_but_remains_punctuation_in_a_draft() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let mut tab = Tab::Timeline;
        handle_key(&mut state, key(KeyCode::Char('?')), &mut tab);
        assert!(state.help);
        assert!(state.composer.text().is_empty());
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        state.composer.insert("why");
        handle_key(&mut state, key(KeyCode::Char('?')), &mut tab);
        assert_eq!(state.composer.text(), "why?");
        assert!(!state.help);
    }

    #[test]
    fn shift_enter_inserts_a_newline_without_submitting() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let mut tab = Tab::Timeline;
        state.composer.insert("first");
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                &mut tab
            ),
            ShellAction::Continue
        );
        state.composer.insert("second");
        assert_eq!(state.composer.text(), "first\nsecond");
    }

    #[test]
    fn ctrl_j_inserts_a_newline_without_submitting() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Composer;
        let mut tab = Tab::Timeline;
        handle_key(&mut state, key(KeyCode::Char('a')), &mut tab);
        handle_key(&mut state, ctrl('j'), &mut tab);
        handle_key(&mut state, key(KeyCode::Char('b')), &mut tab);

        assert_eq!(state.composer_text(), "a\nb");
        // Ctrl+J moved to a new line, and the following 'b' landed there.
        assert_eq!(state.composer.cursor(), (1, 1));
        assert_eq!(state.composer.lines(), &["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn empty_submissions_are_ignored() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Composer;
        let mut tab = Tab::Timeline;
        for c in "   ".chars() {
            handle_key(&mut state, key(KeyCode::Char(c)), &mut tab);
        }
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::Continue
        );
    }

    #[test]
    fn ctrl_c_quits_from_anywhere() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let mut tab = Tab::Timeline;
        assert_eq!(
            handle_key(&mut state, ctrl('c'), &mut tab),
            ShellAction::Quit
        );
    }

    #[test]
    fn ctrl_q_preserves_the_draft_and_refuses_to_leave_active_work() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let mut tab = Tab::Timeline;
        state.composer.paste("my next task");
        state.composer.left();
        let before = state.composer.clone();
        assert_eq!(
            handle_key(&mut state, ctrl('q'), &mut tab),
            ShellAction::Quit
        );
        assert_eq!(state.composer, before);
        state.task_state = Some(crate::session::TaskState::Running);
        assert_eq!(
            handle_key(&mut state, ctrl('q'), &mut tab),
            ShellAction::Continue
        );
        assert_eq!(state.composer, before);
    }

    #[test]
    fn a_pending_approval_is_approved_by_exact_key() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        state.pending = Some(crate::tui_state::PendingPrompt {
            kind: WaitKind::Approval {
                approval_key: "fingerprint-1".to_owned(),
            },
            message: "approve?".to_owned(),
        });
        let mut tab = Tab::Timeline;

        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
                &mut tab
            ),
            ShellAction::Command(SessionCommand::Approve {
                approval_key: "fingerprint-1".to_owned()
            })
        );
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
                &mut tab
            ),
            ShellAction::Command(SessionCommand::Deny {
                approval_key: "fingerprint-1".to_owned()
            })
        );
    }

    #[test]
    fn a_pending_question_is_answered_from_the_composer() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Composer;
        state.pending = Some(crate::tui_state::PendingPrompt {
            kind: WaitKind::Question,
            message: "which file?".to_owned(),
        });
        let mut tab = Tab::Timeline;

        for c in "src/lib.rs".chars() {
            handle_key(&mut state, key(KeyCode::Char(c)), &mut tab);
        }
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::Command(SessionCommand::Answer {
                value: "src/lib.rs".to_owned()
            })
        );
    }

    #[test]
    fn typing_returns_to_composer_without_command_modes() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Tasks;
        for c in "cancel and resume".chars() {
            assert_eq!(
                handle_key(&mut state, key(KeyCode::Char(c)), &mut tab),
                ShellAction::Continue
            );
        }
        assert_eq!(state.composer_text(), "cancel and resume");
        assert_eq!(tab, Tab::Timeline);
    }

    #[test]
    fn interrupt_cancels_work_then_clears_draft_then_exits() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let mut tab = Tab::Timeline;
        state.composer.insert("keep this");
        state.task_state = Some(crate::session::TaskState::Running);
        assert_eq!(
            handle_key(&mut state, ctrl('c'), &mut tab),
            ShellAction::Command(SessionCommand::Cancel)
        );
        assert_eq!(state.composer_text(), "keep this");
        state.task_state = Some(crate::session::TaskState::Cancelled);
        assert_eq!(
            handle_key(&mut state, ctrl('c'), &mut tab),
            ShellAction::Continue
        );
        assert!(state.composer.is_empty());
        assert_eq!(
            handle_key(&mut state, ctrl('c'), &mut tab),
            ShellAction::Quit
        );
    }

    #[test]
    fn help_opens_and_closes_without_leaking_keystrokes() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;

        handle_key(&mut state, key(KeyCode::F(1)), &mut tab);
        assert!(state.help);

        // Typing while help is open must not reach the composer or the
        // session.
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('x')), &mut tab),
            ShellAction::Continue
        );
        assert!(state.is_composer_empty());

        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        assert!(!state.help);
    }

    #[test]
    fn tab_switches_only_between_conversation_and_input() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let mut tab = Tab::Timeline;
        handle_key(&mut state, key(KeyCode::Tab), &mut tab);
        assert_eq!(state.focus, Focus::Timeline);
        handle_key(&mut state, key(KeyCode::Tab), &mut tab);
        assert_eq!(state.focus, Focus::Composer);
    }

    #[test]
    fn navigation_does_not_change_the_task_or_composer() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        state.apply(&SessionEvent::TaskStarted {
            task: TaskId(1),
            prompt: "x".to_owned(),
        });
        state.apply(&SessionEvent::WaitingForUser {
            task: TaskId(1),
            turn: TurnId(1),
            wait: WaitKind::Question,
            message: "?".to_owned(),
        });
        let before = state.clone();
        let mut tab = Tab::Timeline;

        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Up] {
            handle_key(&mut state, key(code), &mut tab);
        }
        // The user can navigate while a provider is slow: nothing about
        // the session changed.
        assert_eq!(state.task_state, before.task_state);
        assert_eq!(state.pending, before.pending);
    }

    #[test]
    fn detail_shortcuts_work_from_the_composer_and_escape_restores_it() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.composer.insert("draft");
        let mut tab = Tab::Timeline;
        handle_key(&mut state, ctrl('o'), &mut tab);
        assert_eq!(tab, Tab::Tasks);
        handle_key(&mut state, key(KeyCode::F(3)), &mut tab);
        assert_eq!(tab, Tab::Inspector);
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        assert_eq!(tab, Tab::Timeline);
        assert_eq!(state.composer_text(), "draft");
    }

    #[test]
    fn the_palette_owns_the_keyboard_and_never_fake_succeeds() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;

        // Filtering must not leak text into the draft.
        assert_eq!(
            handle_key(&mut state, ctrl('p'), &mut tab),
            ShellAction::Continue
        );
        assert!(state.palette_open());
        for c in "canc".chars() {
            handle_key(&mut state, key(KeyCode::Char(c)), &mut tab);
        }
        assert_eq!(state.palette.as_deref(), Some("canc"));

        // Enter selects the matching implemented command.
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::PaletteCommand("cancel")
        );
        assert!(!state.palette_open());
    }

    #[test]
    fn palette_navigation_and_dismissal_preserve_the_draft() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let mut tab = Tab::Timeline;
        state.composer.insert("a queued prompt");
        handle_key(&mut state, ctrl('p'), &mut tab);
        handle_key(&mut state, key(KeyCode::Down), &mut tab);
        assert_eq!(state.palette_selection, 1);
        let id = state.palette_results()[1].id;
        assert_eq!(id, "steer");
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        assert!(!state.palette_open());
        assert_eq!(state.composer_text(), "a queued prompt");
    }

    #[test]
    fn approval_enter_never_discards_the_draft() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.pending = Some(crate::tui_state::PendingPrompt {
            kind: WaitKind::Approval {
                approval_key: "exact".into(),
            },
            message: "Write file?".into(),
        });
        state.composer.insert("my follow-up");
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut Tab::Timeline),
            ShellAction::Continue
        );
        assert_eq!(state.composer_text(), "my follow-up");
    }

    #[test]
    fn help_owns_editing_shortcuts_and_opens_while_composing() {
        let mut state = WorkbenchState::new("/tmp/ws");
        let mut tab = Tab::Timeline;
        handle_key(&mut state, key(KeyCode::F(1)), &mut tab);
        assert!(state.help);
        handle_key(&mut state, ctrl('j'), &mut tab);
        assert!(state.composer.is_empty());
    }

    #[test]
    fn a_palette_search_without_matches_cannot_be_selected() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;

        state.open_palette();
        for c in "zzzz".chars() {
            handle_key(&mut state, key(KeyCode::Char(c)), &mut tab);
        }
        assert!(state.palette_results().is_empty());
        let action = handle_key(&mut state, key(KeyCode::Enter), &mut tab);
        assert_eq!(action, ShellAction::Continue);
        assert!(state.palette_open());
    }
    #[test]
    fn account_navigation_preserves_drafts_and_cancels_only_abortable_work() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.composer.paste("unfinished task");
        state.connection = Some(ConnectionPanel::open());
        let mut tab = Tab::Timeline;
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::Connection(ConnectionAction::Login(false))
        );
        let panel = state.connection.as_mut().unwrap();
        panel.busy = true;
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Esc), &mut tab),
            ShellAction::CancelConnection
        );
        state.connection.as_mut().unwrap().cancellable = false;
        assert_eq!(
            handle_key(&mut state, ctrl('c'), &mut tab),
            ShellAction::Continue
        );
        assert!(state.connection.is_some());
        state.connection.as_mut().unwrap().busy = false;
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        assert!(state.connection.is_none());
        assert_eq!(state.composer_text(), "unfinished task");
    }

    #[test]
    fn approval_wait_blocks_sign_in_before_any_browser_or_credentials_effect() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.task_state = Some(crate::TaskState::Waiting);
        state.connection = Some(ConnectionPanel::open());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut job = None;
        for action in [
            ConnectionAction::Login(false),
            ConnectionAction::Logout,
            ConnectionAction::SelectModel("model".into()),
        ] {
            start_connection(&mut state, action, &tx, &mut job);
            assert!(job.is_none());
            assert!(rx.try_recv().is_err());
            assert!(
                state
                    .connection
                    .as_ref()
                    .unwrap()
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("current task")
            );
        }
    }

    #[test]
    fn plan_limit_has_a_keyboard_recovery_and_sign_out_preserves_an_api_route() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.chatgpt_plan = true;
        state.apply(&SessionEvent::TaskFailed {
            task: TaskId(1),
            reason:
                "model provider rate limited the request: subscription_sharing_usage_limit_exceeded"
                    .into(),
        });
        assert!(state.usage_limit);
        let mut tab = Tab::Timeline;
        assert_eq!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::ALT),
                &mut tab
            ),
            ShellAction::PaletteCommand("usage")
        );
        state.chatgpt_plan = false;
        state.model = Some("api-model".into());
        state.connection = Some(ConnectionPanel::open());
        finish_connection(&mut state, Ok(ConnectionOutcome::LoggedOut(true)));
        assert_eq!(state.model.as_deref(), Some("api-model"));
    }
    #[test]
    fn settings_are_discoverable_and_account_navigation_returns_to_settings() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.composer.paste("keep this draft");
        let mut tab = Tab::Timeline;
        assert_eq!(
            handle_key(&mut state, key(KeyCode::F(2)), &mut tab),
            ShellAction::PaletteCommand("settings")
        );
        state.connection = Some(ConnectionPanel::settings(false, None, &state.theme));
        let (connections, _) = tokio::sync::mpsc::unbounded_channel();
        let mut job = None;
        start_connection(
            &mut state,
            ConnectionAction::OpenAccount,
            &connections,
            &mut job,
        );
        assert!(job.is_none());
        assert_eq!(
            state.connection.as_ref().unwrap().page,
            ConnectionPage::Account
        );
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        assert_eq!(
            state.connection.as_ref().unwrap().page,
            ConnectionPage::Settings
        );
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        assert!(state.connection.is_none());
        assert_eq!(state.composer_text(), "keep this draft");
    }

    #[test]
    fn active_input_explicitly_queues_or_steers_without_changing_the_draft() {
        let mut state = WorkbenchState::new("/workspace");
        let mut tab = Tab::Timeline;
        state.task_state = Some(crate::TaskState::Running);
        state.composer.insert("short follow-up");
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::Command(SessionCommand::Queue {
                prompt: "short follow-up".into(),
                options: Default::default(),
            })
        );
        state.composer.insert("preserve APIs\nand add a regression");
        let draft = state.composer.text();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT),
            &mut tab,
        );
        assert_eq!(state.composer.text(), draft);
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::Command(SessionCommand::Steer { prompt: draft })
        );
        assert!(!state.steer_draft);
    }

    #[test]
    fn editing_a_queued_request_restores_the_draft_only_after_acknowledgement() {
        let mut state = WorkbenchState::new("/workspace");
        let mut tab = Tab::Tasks;
        state.composer.insert("unfinished draft");
        state.apply(&SessionEvent::RequestQueued {
            request: crate::session::QueuedRequest {
                options: Default::default(),
                id: 4,
                prompt: "queued".into(),
            },
        });
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT),
            &mut tab,
        );
        assert_eq!(state.composer.text(), "queued");
        state.composer.insert(" changed");
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::Command(SessionCommand::UpdateQueued {
                id: 4,
                prompt: "queued changed".into(),
                options: Default::default(),
            })
        );
        assert_eq!(state.composer.text(), "queued changed");
        state.apply(&SessionEvent::RequestUpdated {
            request: crate::session::QueuedRequest {
                options: Default::default(),
                id: 4,
                prompt: "queued changed".into(),
            },
        });
        assert_eq!(state.composer.text(), "unfinished draft");
        assert!(state.queue_edit.is_none());
        state.edit_queued();
        state.apply(&SessionEvent::RequestRemoved { id: 4 });
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Enter), &mut tab),
            ShellAction::Continue
        );
        assert_eq!(state.composer.text(), "queued changed");
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        assert_eq!(state.composer.text(), "unfinished draft");
    }
}
