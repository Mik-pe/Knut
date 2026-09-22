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
    if ctrl && key.code == KeyCode::Char('c') {
        if state.help || state.palette_open() || state.review.is_some() || *tab != Tab::Timeline {
            state.help = false;
            state.close_palette();
            state.review = None;
            *tab = Tab::Timeline;
            state.focus = Focus::Composer;
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
        if matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?')) {
            state.help = false;
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
    if key.code == KeyCode::F(1)
        || (key.code == KeyCode::Char('?')
            && !ctrl
            && !alt
            && (state.composer.text().is_empty() || state.review.is_some()))
    {
        state.help = true;
        return ShellAction::Continue;
    }
    if ctrl {
        return match key.code {
            KeyCode::Char('p') | KeyCode::Char('k') => {
                state.open_palette();
                ShellAction::Continue
            }
            KeyCode::Char('r') => ShellAction::Review,
            KeyCode::Char('o') => {
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
        if let Some(pending) = &state.pending
            && let crate::session::WaitKind::Approval { approval_key } = &pending.kind
        {
            return match key.code {
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
    if state
        .pending
        .as_ref()
        .is_some_and(|p| matches!(p.kind, crate::session::WaitKind::Approval { .. }))
    {
        state.status = Some("Approval needed: Alt+A allow · Alt+D deny. Draft kept.".to_owned());
        return ShellAction::Continue;
    }
    let Some(text) = state.composer.take_submission() else {
        return ShellAction::Continue;
    };
    state.resume_follow();
    if state.pending.is_some() {
        ShellAction::Command(SessionCommand::Answer { value: text })
    } else if state.task_state.is_some_and(|s| !s.is_terminal()) {
        ShellAction::Command(SessionCommand::SteerOrQueue { text })
    } else {
        ShellAction::Command(SessionCommand::Submit { prompt: text })
    }
}

fn palette_action(state: &mut WorkbenchState, tab: &mut Tab, id: &'static str) -> ShellAction {
    match id {
        "submit" | "steer" => {
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
        let runner = crate::CheckRunner::new(workspace, supervisor, crate::CheckProfile::rust());
        let revision = runner
            .current_revision("workspace")
            .map_err(|err| format!("cannot identify the current revision: {err}"))?;
        let evidence = runtime.block_on(runner.run_all(&revision));
        Ok::<_, String>((
            evidence
                .iter()
                .map(|check| crate::review::CheckRow::from_evidence(check, &revision.revision))
                .collect::<Vec<_>>(),
            revision.revision,
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
) -> std::io::Result<()> {
    // Signal-driven shutdown, so a Ctrl+C delivered as a signal (or a
    // closing terminal) still restores the display.
    clear_shutdown();
    install_signal_handler(Arc::new(AtomicBool::new(false)))?;

    let mut guard = TerminalGuard::enter()?;
    let mut tab = Tab::Timeline;

    // Checks run on demand and are *not* on the keystroke path: the loop
    // stays responsive, and a finished run arrives as a message.
    let (check_tx, mut check_rx) = tokio::sync::mpsc::unbounded_channel::<CheckOutcome>();
    let mut checks = CheckGate::idle();

    loop {
        // Drain every event already published, then draw once: coalescing
        // high-frequency updates instead of painting per event.
        while let Ok(event) = events.try_recv() {
            state.apply(&event);
        }
        while let Ok(outcome) = check_rx.try_recv() {
            checks.finish();
            state.apply_check_outcome(outcome);
        }

        // The tick drives the spinner and the header clock; state stays
        // pure, so a frame is still a function of (state, tick).
        state.advance();
        guard
            .terminal()
            .draw(|frame| crate::tui_render::render_themed(frame, &state, tab, &state.theme))?;

        if crossterm::event::poll(Duration::from_millis(50))? {
            match crossterm::event::read()? {
                Event::Key(key) => {
                    // In raw mode the terminal does not translate Ctrl+C
                    // into a signal, so the quit path must handle it — and
                    // a signal delivered from outside must break the loop
                    // too, or the shell would keep running after Ctrl+C.
                    if shutdown_requested() {
                        break;
                    }
                    match handle_key(&mut state, key, &mut tab) {
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
                    if !state.help && !state.palette_open() && state.review.is_none() {
                        state.composer.paste(&text);
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

    guard.restore();
    Ok(())
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
                prompt: "fix the test".to_owned()
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
        for c in "model".chars() {
            handle_key(&mut state, key(KeyCode::Char(c)), &mut tab);
        }
        let action = handle_key(&mut state, key(KeyCode::Enter), &mut tab);
        // No palette command fires: the entry is honestly unavailable.
        assert_eq!(action, ShellAction::Continue);
        assert!(state.palette_open());
    }
}
