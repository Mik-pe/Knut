use std::io::Stdout;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
}

impl TerminalGuard {
    pub fn enter() -> std::io::Result<Self> {
        let terminal = Self::setup().inspect_err(|_| {
            let _ = restore_terminal();
        })?;
        static PANIC_HOOK: Once = Once::new();
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let _ = restore_terminal();
                previous(info);
            }));
        });
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    fn setup() -> std::io::Result<Terminal<CrosstermBackend<Stdout>>> {
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
        // Background PTYs may never answer the cursor query used by an automatic viewport.
        let (cols, rows) = terminal::size()?;
        let options = ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, cols, rows)),
        };
        let terminal = Terminal::with_options(backend, options)?;
        // Terminal::clear also queries the cursor, so clear the screen directly.
        execute!(std::io::stdout(), terminal::Clear(terminal::ClearType::All))?;
        Ok(terminal)
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

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
    let raw_mode = disable_raw_mode();
    let screen = execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    );
    raw_mode.and(screen)
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub(crate) fn install_signal_handler() {
    static INSTALLED: Once = Once::new();
    SHUTDOWN.store(false, Ordering::SeqCst);
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
}

pub(crate) fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

extern "C" fn handle_signal(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}
