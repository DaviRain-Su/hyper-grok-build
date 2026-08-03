//! comet-tui — the terminal viewport.
//!
//! A peer of the gpui app in `comet-ui`, not a subset of it: both are thin
//! clients over the same typed RPC (ARCHITECTURE §1), and both derive their row
//! order, status dots and gates from the same pure functions in
//! `comet_proto::view`. What differs is the renderer and one deliberate
//! architectural choice — **the TUI never embeds an engine**.
//!
//! ## Why the engine is always a separate process
//!
//! The gpui app embeds an engine when no daemon is listening, because a desktop
//! window and the work it drives have the same lifetime. A terminal does not:
//! you close it, the SSH session drops, the laptop lid shuts. So `comet-tui`
//! attaches to a daemon and starts a detached one if there isn't one
//! ([`daemon`]). Quitting the viewport is then genuinely *detaching* — agents
//! keep running, docs keep syncing, the DeviceRoom stays joined — and
//! reattaching is instant because nothing had to be rebuilt.
//!
//! ## Where the performance comes from
//!
//! Four things, in rough order of how much they matter:
//!
//! 1. **No redraw without a reason, and no more than one per frame budget.** The
//!    loop below has no tick: it sleeps in `select!` until input, an engine
//!    frame, or a deadline it armed on purpose. A burst of streaming commits is
//!    drained and collapsed into a single draw ([`Loop::pump`]).
//! 2. **Layout is cached per message and invalidated by content fingerprint**
//!    ([`transcript`]), so a streaming token re-wraps one message, not the
//!    conversation.
//! 3. **Only the visible window is built** — the transcript renders cached lines
//!    by reference, and the sidebar slices to the pane height.
//! 4. **One `write` per frame** ([`tty`]), over ratatui's cell diff, so an
//!    unchanged screen emits no bytes at all.
//!
//! The idle cost of this app is therefore zero wakeups and zero bytes, and its
//! streaming cost is bounded by the terminal size rather than by the transcript.

pub mod app;
pub mod cli;
pub mod composer;
pub mod daemon;
pub mod keys;
pub mod link;
pub mod loaders;
pub mod render;
pub mod theme;
pub mod transcript;
pub mod tty;
pub mod wrap;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{Event, MouseButton, MouseEventKind};
use tokio::sync::mpsc;

use crate::app::{App, Effects};
use crate::daemon::{Attachment, DaemonConfig};
use crate::keys::{Action, Edit, Focus};
use crate::link::EngineLink;

/// Everything the binary resolves from flags and the environment.
#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub ipc_port: u16,
    /// Explicit `comet` binary for the daemon we may start.
    pub comet_bin: Option<PathBuf>,
    /// Start a daemon when none is listening.
    pub spawn_daemon: bool,
    /// Report scroll-wheel events. Costs drag-to-select.
    pub mouse: bool,
    /// Minimum interval between draws. 16ms ≈ 60fps: fast enough that typing
    /// feels direct, slow enough that a stream emitting hundreds of commits a
    /// second cannot spend the CPU on redraws nobody sees.
    pub frame_budget: Duration,
}

impl Config {
    pub fn daemon(&self) -> DaemonConfig {
        DaemonConfig {
            data_dir: self.data_dir.clone(),
            ipc_port: self.ipc_port,
            comet_bin: self.comet_bin.clone(),
            spawn: self.spawn_daemon,
        }
    }
}

/// What the caller prints after the terminal is restored.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub attachment: Option<Attachment>,
    pub daemon_log: Option<PathBuf>,
}

impl Outcome {
    /// The detach notice. Printed *after* the alternate screen is gone, so it
    /// survives in the user's scrollback — the whole point being that they know
    /// work is still happening and how to get back to it.
    pub fn farewell(&self) -> String {
        let mut out = String::new();
        match &self.attachment {
            Some(Attachment::Spawned { pid }) => {
                out.push_str(&format!(
                    "Detached. The engine is still running (pid {pid}).\n"
                ));
            }
            Some(Attachment::Attached) => {
                out.push_str("Detached. The engine is still running.\n");
            }
            None => return "Exited without reaching an engine.\n".to_string(),
        }
        out.push_str("  reattach   comet-tui\n");
        if let Some(log) = &self.daemon_log {
            out.push_str(&format!("  logs       {}\n", log.display()));
        }
        out.push_str("  stop       comet daemon stop\n");
        out
    }
}

/// Run the viewport until the user detaches. The terminal is restored before
/// returning, including on error and on panic.
pub async fn run(config: Config) -> anyhow::Result<Outcome> {
    let mut terminal = tty::Terminal::init(config.mouse)?;
    let outcome = Loop::new(&config).run(&mut terminal).await;
    // Explicit: the farewell must print to a restored terminal, and `terminal`
    // would otherwise live until the end of the function.
    drop(terminal);
    outcome
}

struct Loop {
    app: App,
    link: EngineLink,
    events: mpsc::UnboundedReceiver<Event>,
    frame_budget: Duration,
    daemon_log: PathBuf,
    /// A draw is owed.
    dirty: bool,
    last_draw: Instant,
    last_spin: Instant,
}

impl Loop {
    fn new(config: &Config) -> Self {
        let (event_tx, events) = mpsc::unbounded_channel();
        tty::spawn_reader(event_tx);
        Self {
            app: App::new(),
            link: link::spawn(config.daemon()),
            events,
            frame_budget: config.frame_budget,
            daemon_log: config.daemon().log_path(),
            // Draw immediately: the first frame is the "starting the engine…"
            // gate, and the user should see it before the probe finishes.
            dirty: true,
            last_draw: Instant::now() - config.frame_budget,
            last_spin: Instant::now(),
        }
    }

    async fn run(mut self, terminal: &mut tty::Terminal) -> anyhow::Result<Outcome> {
        let mut signals = Signals::install();

        loop {
            if self.dirty && self.last_draw.elapsed() >= self.frame_budget {
                let app = &mut self.app;
                terminal.draw(|frame| render::draw(frame, app))?;
                self.dirty = false;
                self.last_draw = Instant::now();
            }
            if self.app.quit {
                break;
            }

            let deadline = self.next_deadline();
            let mut effects: Effects = Vec::new();

            tokio::select! {
                // Input first: a keystroke must never queue behind a burst of
                // doc frames.
                biased;

                event = self.events.recv() => match event {
                    Some(event) => self.on_event(event, &mut effects),
                    // The reader thread is gone (terminal closed under us).
                    None => break,
                },
                update = self.link.updates.recv() => match update {
                    Some(update) => {
                        effects.extend(self.app.apply(update));
                        self.dirty = true;
                    }
                    None => break,
                },
                _ = sleep_until(deadline) => self.on_deadline(),
                // SIGHUP (the terminal window closed) or SIGTERM (a service
                // manager stopping us). Either way: restore the terminal and
                // leave the engine alone — that separation is the whole design.
                _ = signals.terminating() => break,
            }

            // Drain whatever else is already queued before drawing. This is the
            // coalescing that makes a stream cheap: N frames arriving inside one
            // frame budget produce one layout pass and one draw.
            self.pump(&mut effects);

            for command in effects {
                self.link.send(command);
            }
        }

        Ok(Outcome {
            attachment: self.app.attachment.clone(),
            daemon_log: Some(self.daemon_log.clone()),
        })
    }

    /// Non-blocking drain of both channels.
    fn pump(&mut self, effects: &mut Effects) {
        while let Ok(event) = self.events.try_recv() {
            self.on_event(event, effects);
        }
        while let Ok(update) = self.link.updates.try_recv() {
            effects.extend(self.app.apply(update));
            self.dirty = true;
        }
    }

    /// The next moment we have a reason to wake up: the pacing deadline when a
    /// draw is owed, the spinner while something is running, the notice's
    /// expiry. `None` means sleep until something happens — an idle app arms no
    /// timer at all.
    fn next_deadline(&self) -> Option<Instant> {
        let pace = self.dirty.then(|| self.last_draw + self.frame_budget);
        let spinner = self
            .app
            .animating()
            .then(|| self.last_spin + Duration::from_millis(theme::ANIMATION_TICK_MS));
        let notice = self.app.notice_deadline();
        [pace, spinner, notice].into_iter().flatten().min()
    }

    fn on_deadline(&mut self) {
        if self.app.animating()
            && self.last_spin.elapsed() >= Duration::from_millis(theme::ANIMATION_TICK_MS)
        {
            self.last_spin = Instant::now();
            // The loaders read the clock at draw time, so a tick only has to
            // mark the frame dirty — no state to advance, nothing to rebuild.
            self.dirty = true;
        }
        if self.app.expire_notice() {
            self.dirty = true;
        }
        // A pacing wake-up needs nothing else: `dirty` is already set, and the
        // top of the loop will draw now that the budget has elapsed.
    }

    fn on_event(&mut self, event: Event, effects: &mut Effects) {
        match event {
            Event::Key(key) => {
                if let Some(action) = keys::map(
                    self.app.focus,
                    self.app.overlay_open(),
                    self.app.composer.is_empty(),
                    key,
                ) {
                    effects.extend(self.app.act(action));
                    self.dirty = true;
                }
            }
            // Bracketed paste: a whole clipboard in one event, applied as one
            // edit. Pasting also implies wanting to type, so it takes focus.
            Event::Paste(text) => {
                self.app.focus = Focus::Composer;
                effects.extend(self.app.act(Action::Edit(Edit::Paste(text))));
                self.dirty = true;
            }
            Event::Resize(..) => {
                // The transcript re-lays out on the next draw because its cached
                // width no longer matches the pane.
                self.dirty = true;
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    effects.extend(self.app.act(Action::ScrollUp(3)));
                    self.dirty = true;
                }
                MouseEventKind::ScrollDown => {
                    effects.extend(self.app.act(Action::ScrollDown(3)));
                    self.dirty = true;
                }
                // Resolved against the hit map the last draw built.
                MouseEventKind::Down(MouseButton::Left) => {
                    effects.extend(self.app.click(mouse.column, mouse.row));
                    self.dirty = true;
                }
                MouseEventKind::Down(MouseButton::Right) => {
                    self.app.right_click(mouse.column, mouse.row);
                    self.dirty = true;
                }
                _ => {}
            },
            Event::FocusGained | Event::FocusLost => {}
        }
    }
}

/// `sleep_until` an optional deadline, pending forever when there is none.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// The signals that mean "stop drawing" — and, notably, not "stop working".
///
/// SIGHUP is the interesting one: the kernel sends it to the foreground process
/// group when the controlling terminal goes away. We want it, so the viewport
/// can put the terminal back and print nothing further. The engine does *not*
/// get it, because [`daemon::connect`] put it in its own session.
struct Signals {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hup: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let open = |kind: SignalKind, name: &str| match signal(kind) {
                Ok(stream) => Some(stream),
                Err(err) => {
                    tracing::warn!(error = %err, name, "could not install a signal handler");
                    None
                }
            };
            Self {
                term: open(SignalKind::terminate(), "SIGTERM"),
                hup: open(SignalKind::hangup(), "SIGHUP"),
            }
        }
        #[cfg(not(unix))]
        Self {}
    }

    /// Resolves when a terminating signal arrives; pends forever if we couldn't
    /// install the handlers, or on platforms without them.
    async fn terminating(&mut self) {
        #[cfg(unix)]
        {
            let term = async {
                match &mut self.term {
                    Some(stream) => {
                        stream.recv().await;
                    }
                    None => std::future::pending().await,
                }
            };
            let hup = async {
                match &mut self.hup {
                    Some(stream) => {
                        stream.recv().await;
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = term => {}
                _ = hup => {}
            }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await
    }
}
