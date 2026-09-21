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
use crate::tui_render::{Tab, render};
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
}

/// Apply one keystroke to the state, returning the next action.
///
/// Pure and synchronous, so keyboard behaviour is unit-testable without a
/// terminal.
pub fn handle_key(state: &mut WorkbenchState, key: KeyEvent, tab: &mut Tab) -> ShellAction {
    // Only key presses act; a release/repeat must not double-apply.
    if key.kind != KeyEventKind::Press {
        return ShellAction::Continue;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl {
        return match key.code {
            // Raw mode delivers Ctrl+C as a key rather than a signal in
            // most terminals; both paths lead here.
            KeyCode::Char('c') | KeyCode::Char('C') => ShellAction::Quit,
            KeyCode::Char('j') => {
                // Newline in the composer without submitting.
                state.composer.insert_newline();
                ShellAction::Continue
            }
            // Undo/redo: Ctrl+Z / Ctrl+Y.
            KeyCode::Char('z') => {
                state.composer.undo();
                ShellAction::Continue
            }
            KeyCode::Char('y') => {
                state.composer.redo();
                ShellAction::Continue
            }
            KeyCode::Char('p') => ShellAction::Command(SessionCommand::Pause),
            KeyCode::Char('r') => ShellAction::Command(SessionCommand::Resume),
            _ => ShellAction::Continue,
        };
    }

    if state.help {
        // While help is open, only dismissal keys act.
        if matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?')) {
            state.help = false;
        }
        return ShellAction::Continue;
    }

    // The palette owns the keyboard while it is open: a submit cannot
    // fire from inside a command dialog.
    if state.palette_open() {
        return match key.code {
            KeyCode::Esc => {
                state.close_palette();
                ShellAction::Continue
            }
            KeyCode::Backspace => {
                if let Some(query) = state.palette.as_mut() {
                    query.pop();
                }
                ShellAction::Continue
            }
            KeyCode::Char(c) => {
                if let Some(query) = state.palette.as_mut() {
                    query.push(c);
                }
                ShellAction::Continue
            }
            KeyCode::Enter => {
                // Only an implemented command can be selected; the palette
                // never pretends an unavailable entry worked.
                let selected = state
                    .palette_results()
                    .into_iter()
                    .find(|command| command.is_available());
                state.close_palette();
                match selected {
                    Some(command) => ShellAction::PaletteCommand(command.id),
                    None => ShellAction::Continue,
                }
            }
            _ => ShellAction::Continue,
        };
    }

    match key.code {
        KeyCode::F(1) | KeyCode::Char('?') if state.focus != Focus::Composer => {
            state.help = true;
            ShellAction::Continue
        }
        // The palette: every listed command states whether it exists.
        KeyCode::Char(':') if state.focus != Focus::Composer => {
            state.open_palette();
            ShellAction::Continue
        }
        // Review navigation when a review is open: the review owns the
        // keyboard, so a hunk decision cannot be confused with composing.
        KeyCode::Char('j')
        | KeyCode::Char('k')
        | KeyCode::Char('n')
        | KeyCode::Char('p')
        | KeyCode::Char('r')
        | KeyCode::Char('y')
            if state.review.is_some() && state.focus != Focus::Composer =>
        {
            let Some(review) = state.review.as_mut() else {
                return ShellAction::Continue;
            };
            match key.code {
                KeyCode::Char('j') => review.next_hunk(),
                KeyCode::Char('k') => review.previous_hunk(),
                KeyCode::Char('n') => review.next_file(),
                KeyCode::Char('p') => review.previous_file(),
                // `r` rejects the current hunk; `y` keeps it again.
                KeyCode::Char('r') => {
                    review.reject_current_hunk();
                }
                KeyCode::Char('y') => {
                    review.keep_current_hunk();
                }
                _ => {}
            }
            ShellAction::Continue
        }
        KeyCode::Char('v') if state.focus != Focus::Composer => {
            state.review = None;
            ShellAction::Continue
        }
        KeyCode::Esc => {
            state.focus = Focus::Timeline;
            ShellAction::Continue
        }
        KeyCode::Tab => {
            // Cycle focus: composer -> timeline -> inspector -> composer.
            state.focus = match state.focus {
                Focus::Composer => Focus::Timeline,
                Focus::Timeline => Focus::Inspector,
                Focus::Inspector => Focus::Composer,
            };
            ShellAction::Continue
        }
        KeyCode::Char('i') if state.focus != Focus::Composer => {
            state.focus = Focus::Composer;
            ShellAction::Continue
        }
        // A plain quit key, independent of terminal signal handling.
        KeyCode::Char('q') if state.focus != Focus::Composer => ShellAction::Quit,
        KeyCode::Char('c') if state.focus != Focus::Composer => {
            ShellAction::Command(SessionCommand::Cancel)
        }
        KeyCode::Char('p') if state.focus != Focus::Composer => {
            ShellAction::Command(SessionCommand::Pause)
        }
        KeyCode::Char('r') if state.focus != Focus::Composer => {
            ShellAction::Command(SessionCommand::Resume)
        }
        KeyCode::Char('a') if state.focus != Focus::Composer => match &state.pending {
            Some(pending) => match &pending.kind {
                crate::session::WaitKind::Approval { approval_key } => {
                    ShellAction::Command(SessionCommand::Approve {
                        approval_key: approval_key.clone(),
                    })
                }
                crate::session::WaitKind::Question => ShellAction::Continue,
            },
            None => ShellAction::Continue,
        },
        KeyCode::Char('d') if state.focus != Focus::Composer => match &state.pending {
            Some(pending) => match &pending.kind {
                crate::session::WaitKind::Approval { approval_key } => {
                    ShellAction::Command(SessionCommand::Deny {
                        approval_key: approval_key.clone(),
                    })
                }
                crate::session::WaitKind::Question => ShellAction::Continue,
            },
            None => ShellAction::Continue,
        },
        KeyCode::Up => {
            if state.focus == Focus::Composer {
                state.composer.up();
            } else {
                state.follow = false;
                state.selection = state.selection.saturating_sub(1);
            }
            ShellAction::Continue
        }
        KeyCode::Down => {
            if state.focus == Focus::Composer {
                state.composer.down();
            } else {
                state.selection = (state.selection + 1).min(state.timeline.len().saturating_sub(1));
                if state.selection + 1 >= state.timeline.len() {
                    state.follow = true;
                }
            }
            ShellAction::Continue
        }
        KeyCode::Left => {
            if state.focus == Focus::Composer {
                state.composer.left();
            }
            ShellAction::Continue
        }
        KeyCode::Right => {
            if state.focus == Focus::Composer {
                state.composer.right();
            }
            ShellAction::Continue
        }
        KeyCode::Home => {
            if state.focus == Focus::Composer {
                state.composer.home();
            }
            ShellAction::Continue
        }
        KeyCode::End => {
            if state.focus == Focus::Composer {
                state.composer.end();
            }
            ShellAction::Continue
        }
        KeyCode::Delete => {
            if state.focus == Focus::Composer {
                state.composer.delete();
            }
            ShellAction::Continue
        }
        KeyCode::Backspace => {
            if state.focus == Focus::Composer {
                state.composer.backspace();
            }
            ShellAction::Continue
        }
        KeyCode::Enter => {
            if state.focus != Focus::Composer {
                return ShellAction::Continue;
            }
            let Some(text) = state.composer.take_submission() else {
                return ShellAction::Continue;
            };
            // A pending question is answered. Otherwise: while a task is
            // active, a short directive steers it and anything else is
            // queued for the next task — the user chose which, and the
            // session revision mechanism is what validates the steering.
            match &state.pending {
                Some(pending) if pending.kind == crate::session::WaitKind::Question => {
                    ShellAction::Command(SessionCommand::Answer { value: text })
                }
                Some(_) => ShellAction::Continue,
                None => {
                    if state.task_state.is_some_and(|s| !s.is_terminal()) {
                        ShellAction::Command(SessionCommand::SteerOrQueue { text })
                    } else {
                        ShellAction::Command(SessionCommand::Submit { prompt: text })
                    }
                }
            }
        }
        KeyCode::Char(c) if state.focus == Focus::Composer => {
            state.composer.insert(&c.to_string());
            ShellAction::Continue
        }
        // On narrow layouts, digits select panes directly.
        KeyCode::Char(c) if tabbed_shortcut(c).is_some() => {
            if let Some(selected) = tabbed_shortcut(c) {
                *tab = selected;
            }
            ShellAction::Continue
        }
        _ => ShellAction::Continue,
    }
}

/// Digit shortcuts for the tabbed (narrow) layout.
fn tabbed_shortcut(c: char) -> Option<Tab> {
    match c {
        '1' => Some(Tab::Timeline),
        '2' => Some(Tab::Inspector),
        '3' => Some(Tab::Tasks),
        _ => None,
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
        execute!(stdout, EnterAlternateScreen)?;
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
    execute!(stdout, LeaveAlternateScreen)?;
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

    loop {
        // Drain every event already published, then draw once: coalescing
        // high-frequency updates instead of painting per event.
        while let Ok(event) = events.try_recv() {
            state.apply(&event);
        }

        guard.terminal().draw(|frame| render(frame, &state, tab))?;

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
                        ShellAction::PaletteCommand("submit") => {
                            if let Some(text) = state.composer.take_submission() {
                                let _ = commands.send(SessionCommand::Submit { prompt: text });
                            }
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
                        ShellAction::PaletteCommand("steer") => {
                            if let Some(text) = state.composer.take_submission() {
                                let _ = commands.send(SessionCommand::SteerOrQueue { text });
                            }
                        }
                        ShellAction::PaletteCommand("help") => {
                            state.help = true;
                        }
                        // The remaining palette entries are honest no-ops
                        // in this increment: they either map to a command
                        // the binary has (handled above) or are reported
                        // as unavailable by the palette itself.
                        ShellAction::PaletteCommand(_) | ShellAction::Continue => {}
                    }
                }
                Event::Resize(_, _) => {
                    // The next draw uses the new area; nothing to do here.
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
            handle_key(&mut state, key(KeyCode::Char('a')), &mut tab),
            ShellAction::Command(SessionCommand::Approve {
                approval_key: "fingerprint-1".to_owned()
            })
        );
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('d')), &mut tab),
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
    fn keys_pressed_while_not_composing_do_not_type() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;

        for c in "cancel and resume".chars() {
            handle_key(&mut state, key(KeyCode::Char(c)), &mut tab);
        }
        // Nothing landed in the composer: those were commands.
        assert!(state.is_composer_empty());
    }

    #[test]
    fn cancel_pause_and_resume_reach_the_session() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;

        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('c')), &mut tab),
            ShellAction::Command(SessionCommand::Cancel)
        );
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('p')), &mut tab),
            ShellAction::Command(SessionCommand::Pause)
        );
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char('r')), &mut tab),
            ShellAction::Command(SessionCommand::Resume)
        );
        assert_eq!(
            handle_key(&mut state, ctrl('p'), &mut tab),
            ShellAction::Command(SessionCommand::Pause)
        );
        assert_eq!(
            handle_key(&mut state, ctrl('r'), &mut tab),
            ShellAction::Command(SessionCommand::Resume)
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
    fn focus_cycles_through_the_panes() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Composer;
        let mut tab = Tab::Timeline;

        handle_key(&mut state, key(KeyCode::Tab), &mut tab);
        assert_eq!(state.focus, Focus::Timeline);
        handle_key(&mut state, key(KeyCode::Tab), &mut tab);
        assert_eq!(state.focus, Focus::Inspector);
        handle_key(&mut state, key(KeyCode::Tab), &mut tab);
        assert_eq!(state.focus, Focus::Composer);

        // `i` returns to the composer from a read-only pane.
        handle_key(&mut state, key(KeyCode::Tab), &mut tab);
        handle_key(&mut state, key(KeyCode::Char('i')), &mut tab);
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
    fn numeric_shortcuts_select_tabs_on_narrow_layouts() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;
        handle_key(&mut state, key(KeyCode::Char('3')), &mut tab);
        assert_eq!(tab, Tab::Tasks);
        handle_key(&mut state, key(KeyCode::Char('2')), &mut tab);
        assert_eq!(tab, Tab::Inspector);
    }

    #[test]
    fn the_palette_owns_the_keyboard_and_never_fake_succeeds() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;

        // Open with ':'; typing filters the catalog.
        assert_eq!(
            handle_key(&mut state, key(KeyCode::Char(':')), &mut tab),
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
    fn a_submit_inside_the_palette_does_not_fire() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Composer;
        let mut tab = Tab::Timeline;
        state.composer.insert("a queued prompt");

        // Open the palette, then press Enter: nothing is submitted and the
        // composer keeps its text.
        state.open_palette();
        handle_key(&mut state, key(KeyCode::Enter), &mut tab);
        assert_eq!(state.composer_text(), "a queued prompt");

        // Escape closes it and the composer still has the text.
        handle_key(&mut state, key(KeyCode::Esc), &mut tab);
        assert!(!state.palette_open());
        assert_eq!(state.composer_text(), "a queued prompt");
    }

    #[test]
    fn an_unavailable_palette_entry_cannot_be_selected() {
        let mut state = WorkbenchState::new("/tmp/ws");
        state.focus = Focus::Timeline;
        let mut tab = Tab::Timeline;

        // Search for a command that is explicitly unavailable.
        state.open_palette();
        for c in "review".chars() {
            handle_key(&mut state, key(KeyCode::Char(c)), &mut tab);
        }
        let action = handle_key(&mut state, key(KeyCode::Enter), &mut tab);
        // No palette command fires: the entry is honestly unavailable.
        assert_ne!(action, ShellAction::PaletteCommand("review"));
    }
}
