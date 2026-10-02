//! The workbench shell: terminal setup, input handling and the event loop
//! (issue #27).
//!
//! Kept deliberately thin. All state transitions live in
//! [`crate::tui_state`] and all drawing in [`crate::tui_render`]; this
//! module only turns keystrokes into updates and session commands.
//!
//! Terminal correctness is a requirement, not a nicety: raw mode and the
//! alternate screen are restored on normal exit, error and panic, and
//! nothing logs to stdout while the display is active.

use std::io::Stdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::connection::{
    ConnectionAction, ConnectionJob, ConnectionOutcome, ConnectionPage, ConnectionPanel,
};
use crate::session::{SessionCommand, SessionEvent};
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
                panel.status =
                    "Choose your connection and model. Changes are saved for the next start."
                        .to_owned();
            } else {
                state.connection = None;
            }
            return ShellAction::Continue;
        }
        if !panel.busy {
            match key.code {
                KeyCode::Up => panel.selection = panel.selection.saturating_sub(1),
                KeyCode::Down => {
                    panel.selection =
                        (panel.selection + 1).min(panel.choices().len().saturating_sub(1))
                }
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
    if ctrl && key.code == KeyCode::Char('u') && state.usage_limit {
        return ShellAction::PaletteCommand("usage");
    }
    if ctrl && key.code == KeyCode::Char('c') {
        if state.help
            || state.palette_open()
            || state.review.is_some()
            || state.approval_open
            || *tab != Tab::Timeline
        {
            state.help = false;
            state.approval_open = false;
            state.close_palette();
            state.review = None;
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
            KeyCode::Up => {
                state.palette_selection = state.palette_selection.saturating_sub(1);
                ShellAction::Continue
            }
            KeyCode::Down => {
                state.palette_selection = (state.palette_selection + 1)
                    .min(state.palette_results().len().saturating_sub(1));
                ShellAction::Continue
            }
            KeyCode::Backspace => {
                if let Some(query) = state.palette.as_mut() {
                    query.pop();
                }
                state.palette_selection = 0;
                ShellAction::Continue
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Some(query) = state.palette.as_mut() {
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
                    Some(command) if command.is_available() => {
                        state.close_palette();
                        palette_action(state, tab, command.id)
                    }
                    _ => ShellAction::Continue,
                }
            }
            _ => ShellAction::Continue,
        };
    }
    if key.code == KeyCode::F(2) {
        return ShellAction::PaletteCommand("settings");
    }
    if key.code == KeyCode::F(1)
        || (key.code == KeyCode::Char('?')
            && !ctrl
            && !alt
            && (state.composer.text().is_empty() || state.review.is_some()))
    {
        state.help = true;
        state.help_scroll = 0;
        return ShellAction::Continue;
    }
    if ctrl {
        return match key.code {
            KeyCode::Left if state.review.is_none() => {
                state.composer.word_left();
                ShellAction::Continue
            }
            KeyCode::Right if state.review.is_none() => {
                state.composer.word_right();
                ShellAction::Continue
            }
            KeyCode::Char('w') | KeyCode::Backspace if state.review.is_none() => {
                state.composer.delete_word_left();
                ShellAction::Continue
            }
            KeyCode::Char('p') | KeyCode::Char('k') => {
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
            KeyCode::Char('b') => {
                state.approval_open = false;
                state.detail_scroll = 0;
                *tab = if *tab == Tab::Inspector {
                    Tab::Timeline
                } else {
                    Tab::Inspector
                };
                state.focus = Focus::Composer;
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
            KeyCode::Char('a') if state.review.is_none() => {
                state.composer.home();
                ShellAction::Continue
            }
            KeyCode::Char('e') if state.review.is_none() => {
                state.composer.end();
                ShellAction::Continue
            }
            KeyCode::Char('j') if state.review.is_none() => {
                state.composer.insert_newline();
                ShellAction::Continue
            }
            KeyCode::Char('z') if state.review.is_none() => {
                state.composer.undo();
                ShellAction::Continue
            }
            KeyCode::Char('y') if state.review.is_none() => {
                state.composer.redo();
                ShellAction::Continue
            }
            KeyCode::End => {
                state.resume_follow();
                ShellAction::Continue
            }
            _ => ShellAction::Continue,
        };
    }
    if alt {
        if state.review.is_none() {
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
        state.review = None;
        *tab = Tab::Timeline;
        state.focus = Focus::Composer;
        state.resume_follow();
        return ShellAction::Continue;
    }
    if let Some(review) = state.review.as_mut() {
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
        "motion" => {
            state.theme.reduced_motion = !state.theme.reduced_motion;
            state.status = Some(if state.theme.reduced_motion {
                "Reduced motion on".to_owned()
            } else {
                "Animations on".to_owned()
            });
            ShellAction::Continue
        }
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

/// A terminal session guard that always restores the terminal.
///
/// Restoration happens on normal exit, on error and on panic, because a
/// shell that leaves a terminal in raw mode has done real damage to the
/// user's session.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
}

impl TerminalGuard {
    /// Enter the alternate screen and raw mode.
    ///
    /// The viewport is taken from the terminal size rather than a cursor
    /// position query: some environments (background PTYs, recorded
    /// sessions, CI) never answer that query, and a shell that refuses to
    /// start there is not usable.
    pub fn enter() -> std::io::Result<Self> {
        let mut stdout = std::io::stdout();
        enable_raw_mode()?;
        execute!(
            stdout,
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
        let backend = CrosstermBackend::new(stdout);
        // A fixed viewport means no cursor-position query is needed, and
        // the full-screen layout still comes from the real terminal size.
        let (cols, rows) = terminal::size()?;
        let area = ratatui::layout::Rect::new(0, 0, cols, rows);
        let options = ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(area),
        };
        let terminal = Terminal::with_options(backend, options)?;
        // `Terminal::clear` snapshots the cursor position (a terminal
        // query that headless/background PTYs do not answer), so the
        // region is cleared directly instead.
        execute!(std::io::stdout(), terminal::Clear(terminal::ClearType::All))?;

        // Restore before the panic message prints, so the message is
        // readable and the terminal is usable.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            previous(info);
        }));

        Ok(Self {
            terminal,
            restored: false,
        })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    /// Restore the terminal explicitly (also happens on drop).
    pub fn restore(&mut self) {
        if !self.restored {
            let _ = restore_terminal();
            self.restored = true;
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn restore_terminal() -> std::io::Result<()> {
    disable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal::disable_raw_mode()?;
    Ok(())
}

/// Ask the process to exit on SIGINT/SIGTERM.
///
/// The handler only flips a process-wide atomic: nothing allocates, locks
/// or reads a `String` in signal context, and the loop observes the flag
/// between polls. Ctrl+C is also handled as a key event, because raw mode
/// delivers it that way.
fn install_signal_handler(flag: Arc<AtomicBool>) -> std::io::Result<()> {
    static SHUTDOWN: AtomicBool = AtomicBool::new(false);
    // A single process-wide flag keeps the signal handler trivial; the
    // caller's own flag is folded in by the loop below.
    INSTALLED.call_once(|| unsafe {
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
    });
    // Mirror the shared flag into the signal-safe one so both paths agree.
    if flag.load(Ordering::SeqCst) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// Set by the signal handler when the process was asked to stop.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Reset the shutdown flag (so a later shell in the same process starts
/// clean).
pub fn clear_shutdown() {
    SHUTDOWN.store(false, Ordering::SeqCst);
}

extern "C" fn handle_signal(_signal: libc::c_int) {
    // Async-signal-safe: one relaxed store, no allocation, no locks.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Set once when the handlers are installed.
static INSTALLED: std::sync::Once = std::sync::Once::new();

/// The process-wide shutdown flag, signal-safe to set.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Whether an on-demand check run is in flight.
///
/// A tiny state machine rather than a shared `Option` behind a lock: the
/// shell is single-threaded over its state, so a plain flag is enough and
/// two `v` presses cannot start two check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckGate {
    running: bool,
}

impl CheckGate {
    fn idle() -> Self {
        Self { running: false }
    }

    fn get(&self) -> Option<()> {
        self.running.then_some(())
    }

    fn start(&mut self) {
        self.running = true;
    }

    fn finish(&mut self) {
        self.running = false;
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
/// `run_all` is async and spawns processes; the shell is a separate
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
        let revision = runner
            .current_revision("workspace")
            .map_err(|err| format!("cannot identify the current revision: {err}"))?;
        let evidence = runtime.block_on(runner.run_all(&revision));
        let current = runner
            .current_revision("workspace")
            .map_err(|err| format!("cannot identify the checked revision: {err}"))?;
        Ok::<_, String>((
            evidence
                .iter()
                .map(|check| crate::review::CheckRow::from_evidence(check, &current.revision))
                .collect::<Vec<_>>(),
            current.revision,
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
    clear_shutdown();
    install_signal_handler(Arc::new(AtomicBool::new(false)))?;

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
    let mut checks = CheckGate::idle();
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
            checks.finish();
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
                            ));
                        }
                        ShellAction::PaletteCommand("account" | "model") => {
                            state.connection = Some(ConnectionPanel::open());
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
                            start_connection(state, action, &connections, &mut connection_job);
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
                                panel.status = if panel.status.starts_with("Signed in.")
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
                            if checks.get().is_none() {
                                checks.start();
                                state.status = Some("running checks…".to_owned());
                                let tx = check_tx.clone();
                                tokio::spawn(async move {
                                    let outcome = run_checks_blocking().await;
                                    let _ = tx.send(outcome);
                                });
                            }
                        }
                        ShellAction::Review => {
                            // Review opens over whatever the session has
                            // actually recorded; with nothing recorded it
                            // says so rather than opening an empty diff.
                            // Review shows what the session actually
                            // recorded: its checks always, and its diff
                            // when a change was validated. With neither,
                            // it says so rather than opening an empty pane.
                            match state.review.take() {
                                Some(_) => {}
                                None => {
                                    let checks = state.known_checks();
                                    let files = state.review_changes();
                                    if checks.is_empty() && files.is_empty() {
                                        state.status =
                                            Some("no changes or checks to review yet".to_owned());
                                    } else {
                                        let mut view = crate::review::ReviewView::new(files);
                                        view.checks = checks;
                                        state.review = Some(view);
                                    }
                                }
                            }
                        }
                        // The remaining palette entries are honest no-ops
                        // in this increment: they either map to a command
                        // the binary has (handled above) or are reported
                        // as unavailable by the palette itself.
                        ShellAction::PaletteCommand(_) | ShellAction::Continue => {}
                    }
                }
                Event::Paste(text) => {
                    if !state.help
                        && !state.palette_open()
                        && state.review.is_none()
                        && state.connection.is_none()
                    {
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
        && action != ConnectionAction::ManageUsage
    {
        panel.error =
            Some("Finish or cancel the current task before changing the connection.".to_owned());
        return;
    }
    panel.busy = true;
    panel.error = None;
    panel.cancellable = matches!(
        action,
        ConnectionAction::Login(_)
            | ConnectionAction::SelectAccount(_)
            | ConnectionAction::Models
            | ConnectionAction::ManageUsage
    );
    panel.status = match &action {
        ConnectionAction::Login(_) => "Continue with ChatGPT in your browser. Waiting for sign-in…",
        ConnectionAction::Models | ConnectionAction::SelectAccount(_) => {
            "Loading models available to this account…"
        }
        ConnectionAction::SelectModel(_) => "Connecting the model…",
        ConnectionAction::Logout => "Signing out…",
        ConnectionAction::ManageUsage => "Opening ChatGPT usage…",
        ConnectionAction::Acknowledge => "Saving…",
        ConnectionAction::Environment => "Saving environment configuration…",
        ConnectionAction::OpenAccount => unreachable!("navigation was handled above"),
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
            notice,
            error,
        }) => {
            panel.models = models;
            panel.page = if notice {
                ConnectionPage::Welcome
            } else {
                ConnectionPage::Models
            };
            panel.selection = 0;
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
            state.unavailable = if state.checks == 0 {
                Some("Connected. Configure repository checks before tasks can complete.".to_owned())
            } else {
                None
            };
            state.status = Some(
                if plan {
                    "Using ChatGPT plan · F2 settings"
                } else {
                    "Using environment configuration · F2 settings"
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
        let previous = state.theme.reduced_motion;
        assert_eq!(
            palette_action(&mut state, &mut tab, "motion"),
            ShellAction::Continue
        );
        assert_ne!(state.theme.reduced_motion, previous);
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
        handle_key(&mut state, ctrl('b'), &mut tab);
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
    fn an_unavailable_palette_entry_cannot_be_selected() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;

        // Search for a command that is explicitly unavailable.
        state.open_palette();
        for c in "spawnsubagent".chars() {
            handle_key(&mut state, key(KeyCode::Char(c)), &mut tab);
        }
        assert!(state.palette_results().is_empty());
        let action = handle_key(&mut state, key(KeyCode::Enter), &mut tab);
        // No palette command fires: the entry is honestly unavailable.
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
            handle_key(&mut state, ctrl('u'), &mut tab),
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
        state.connection = Some(ConnectionPanel::settings(false, None));
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
