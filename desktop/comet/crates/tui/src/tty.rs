//! Terminal setup and teardown, and the input reader.
//!
//! [`Terminal`] is a guard: raw mode, the alternate screen, bracketed paste and
//! (optionally) mouse reporting all come back off in `Drop`, and a panic hook
//! does the same before unwinding. Leaving a terminal in raw mode is the one
//! failure mode a TUI must not have — the user's shell becomes unusable and the
//! only fix is `reset`.

use std::io::{BufWriter, Stdout, Write, stdout};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{execute, queue};
use tokio::sync::mpsc;

/// Bytes buffered before a flush. One frame of a full-screen repaint on a large
/// terminal is well under this, so a draw is a single `write` syscall instead of
/// hundreds of small ones — the cheapest large win available to a TUI.
const WRITE_BUFFER: usize = 256 * 1024;

pub type Backend = CrosstermBackend<BufWriter<Stdout>>;

pub struct Terminal {
    inner: ratatui::Terminal<Backend>,
    mouse: bool,
}

impl Terminal {
    /// Enter the alternate screen and raw mode.
    ///
    /// `mouse` enables scroll-wheel reporting. It is opt-out because mouse
    /// capture takes drag-to-select away from the terminal; users who live on
    /// selection would rather scroll with the keyboard.
    pub fn init(mouse: bool) -> anyhow::Result<Self> {
        install_panic_hook(mouse);
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
        if mouse {
            execute!(out, EnableMouseCapture)?;
        }
        let backend = CrosstermBackend::new(BufWriter::with_capacity(WRITE_BUFFER, stdout()));
        let inner = ratatui::Terminal::new(backend)?;
        Ok(Self { inner, mouse })
    }

    pub fn draw(&mut self, render: impl FnOnce(&mut ratatui::Frame)) -> std::io::Result<()> {
        self.inner.draw(render)?;
        Ok(())
    }

    /// Undo everything [`Terminal::init`] did. Idempotent; also called from
    /// `Drop` and from the panic hook.
    fn leave(mouse: bool) {
        let mut out = stdout();
        if mouse {
            let _ = queue!(out, DisableMouseCapture);
        }
        let _ = queue!(out, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = out.flush();
        // Raw mode last: it has the widest side effects, so it is the thing to
        // be surest of clearing.
        let _ = disable_raw_mode();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        Self::leave(self.mouse);
    }
}

/// Restore the terminal before a panic prints, so the backtrace is readable and
/// the shell is usable afterwards.
fn install_panic_hook(mouse: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        Terminal::leave(mouse);
        previous(info);
    }));
}

/// Read terminal events on a dedicated OS thread and forward them.
///
/// A blocking `read()` on its own thread costs nothing while idle — no timer, no
/// poll loop, no wakeups. (crossterm's async `EventStream` is the same thread
/// plus a waker, and it needs a feature we'd otherwise not compile.) The channel
/// is unbounded so a paste storm can never block the reader, and the render loop
/// drains it in one pass per frame.
pub fn spawn_reader(events: mpsc::UnboundedSender<Event>) {
    let spawned = std::thread::Builder::new()
        .name("comet-tui-input".into())
        .spawn(move || {
            loop {
                match ratatui::crossterm::event::read() {
                    // A closed channel means the app is shutting down.
                    Ok(event) => {
                        if events.send(event).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "terminal input closed");
                        break;
                    }
                }
            }
        });
    if let Err(err) = spawned {
        tracing::error!(error = %err, "could not start the input thread");
    }
}
