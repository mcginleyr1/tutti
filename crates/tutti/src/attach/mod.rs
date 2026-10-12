//! The attachable TUI. `run` owns terminal setup/teardown and the event loop;
//! the pieces it drives — connection, app state, input mapping, layout, and
//! rendering — live in the submodules and are unit-tested there. `raw` is the
//! TUI-less view: one pane, streamed raw to the terminal's own emulator.

mod app;
mod connection;
mod host;
mod input;
mod launcher;
mod layout;
mod navigate;
pub mod raw;
mod render;
mod sidebar;
mod theme;

#[cfg(test)]
pub(crate) mod fixtures;

pub use app::App;
use app::{control, tagged};

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use tutti_core::{PaneId, Request};

use connection::Connection;

use crate::config::Config;

const POLL: Duration = Duration::from_millis(16);

/// Attach the interactive TUI to `session`, auto-starting the daemon. Restores
/// the terminal on exit and on panic, returning once the user detaches or the
/// server goes away. `config` supplies the prefix chord, direct bindings, and
/// the master mouse switch. `first_run` (a prefill dir) arms the first-run
/// new-project prompt on an empty session; `notice` posts a one-shot status
/// line (e.g. a startup-project error) once attached.
pub fn run(
    session: &str,
    config: Config,
    first_run: Option<String>,
    notice: Option<String>,
) -> Result<()> {
    let mut conn = Connection::open(session)?;
    // macOS names the GUI app a process was launched under; watchers raise it.
    // Tagged, so the reply shows whether the daemon echoes a request's `req`.
    let terminal = std::env::var("__CFBundleIdentifier").ok();
    conn.send(&tagged(&Request::Attach { terminal }, 0))
        .context("send attach request")?;

    // Asked before crossterm owns input. It stays the attach-time answer: the
    // terminal's later light/dark reports (mode 2031, `CSI ? 997 ; 1|2 n`)
    // are not asked for, as crossterm 0.29 takes `CSI ?` ending in anything
    // but `u` or `c` for a sequence still arriving — it would sit on every
    // key typed after one until a `u` or `c` came.
    if let Some(colors) = theme::host_colors(config.theme) {
        conn.send(&control(&Request::HostColors(colors)))
            .context("send the terminal's colours")?;
    }
    let mut terminal = host::init().context("take the terminal")?;
    install_panic_hook();
    if config.mouse {
        let _ = execute!(io::stdout(), EnableMouseCapture);
    }
    // The terminal's focus, for the panes that ask to hear of theirs.
    let _ = execute!(io::stdout(), EnableFocusChange);
    // A terminal that speaks kitty's keyboard protocol tells keys apart that
    // plain bytes cannot — Ctrl+I from Tab, Esc from Alt — for the panes
    // that asked for them (`input::encode_key`). Its own stack takes the
    // flags, popped on the way out.
    if terminal::supports_keyboard_enhancement().unwrap_or(false) {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
        let pushed = execute!(io::stdout(), PushKeyboardEnhancementFlags(flags)).is_ok();
        KEYS_PUSHED.store(pushed, Ordering::SeqCst);
    }

    let result = event_loop(&mut terminal, &mut conn, config, first_run, notice);

    leave();
    ratatui::restore();
    result
}

/// Whether the terminal's keyboard flags were pushed, for `leave` to pop.
static KEYS_PUSHED: AtomicBool = AtomicBool::new(false);

/// Hand back what the TUI changed in the terminal beyond what
/// `ratatui::restore` does: mouse capture, focus reports, the cursor's shape
/// and the keyboard flags. Safe to run twice.
fn leave() {
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableFocusChange);
    if KEYS_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    emit_terminal(b"\x1b[0 q");
}

fn event_loop(
    terminal: &mut ratatui::Terminal<host::Host>,
    conn: &mut Connection,
    config: Config,
    first_run: Option<String>,
    notice: Option<String>,
) -> Result<()> {
    let mut captured = config.mouse;
    let mut app = App::with_config(config);
    if let Some(prefill) = first_run {
        app.start_first_run_prompt(prefill);
    }
    if let Some(message) = notice {
        app.note(message);
    }
    let mut dirty = true;
    let mut cursor_shape = 0;
    let mut whichkey = false;
    let mut spinner = app.spinner_frame();
    // The issues panel's fetches report back from their threads, and so do
    // the editors the changes panel hands files to.
    let (issues_tx, issues_rx) = std::sync::mpsc::channel();
    let (editor_tx, editor_rx) = std::sync::mpsc::channel::<(PaneId, String, Result<(), String>)>();

    loop {
        match conn.drain() {
            Ok(frames) => {
                if !frames.is_empty() {
                    for frame in frames {
                        for reply in app.handle_frame(frame) {
                            let _ = conn.send(&reply);
                        }
                    }
                    dirty = true;
                }
            }
            Err(_) => break, // server closed the connection
        }
        while let Ok((generation, index, fetched)) = issues_rx.try_recv() {
            app.issues_loaded(generation, index, fetched);
            dirty = true;
        }
        while let Ok((pane, file, opened)) = editor_rx.try_recv() {
            for frame in app.editor_opened(pane, &file, opened) {
                let _ = conn.send(&frame);
            }
            dirty = true;
        }

        // The which-key popup appears purely on elapsed time; redraw when its
        // visibility flips even without any input event.
        if app.whichkey_visible() != whichkey {
            whichkey = app.whichkey_visible();
            dirty = true;
        }

        // While an agent is working, advance the shared spinner on its own tick
        // so the animation runs without any input event.
        let frame = app.spinner_frame();
        if app.has_working_agent() && frame != spinner {
            spinner = frame;
            dirty = true;
        }

        if dirty {
            let area = terminal.draw(|frame| render::draw(frame, &app))?.area;
            for frame in app.sync_sizes(area) {
                let _ = conn.send(&frame);
            }
            dirty = false;
        }
        if app.cursor_shape() != cursor_shape {
            cursor_shape = app.cursor_shape();
            emit_terminal(format!("\x1b[{cursor_shape} q").as_bytes());
        }

        if event::poll(POLL).context("poll terminal input")? {
            loop {
                match event::read().context("read terminal input")? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        for frame in app.on_key(key) {
                            conn.send(&frame)?;
                        }
                        dirty = true;
                    }
                    Event::Mouse(mouse) => {
                        for frame in app.on_mouse_event(mouse) {
                            conn.send(&frame)?;
                        }
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    Event::FocusGained => app.host_focus(true),
                    Event::FocusLost => app.host_focus(false),
                    _ => {}
                }
                if !event::poll(Duration::ZERO).context("poll terminal input")? {
                    break;
                }
            }
        }

        // An opening issues panel fetches each source on a thread of its own:
        // `gh` and `curl` must never stall the loop.
        if let Some((generation, dir, sources)) = app.take_issue_fetch() {
            for (index, source) in sources.into_iter().enumerate() {
                let (tx, dir) = (issues_tx.clone(), dir.clone());
                std::thread::spawn(move || {
                    let fetched = crate::issues::list(&source, &dir).map_err(|e| format!("{e:#}"));
                    let _ = tx.send((generation, index, fetched));
                });
            }
        }
        // So does the editor's own CLI, which `e` in the changes panel runs.
        if let Some(job) = app.take_editor_job() {
            let tx = editor_tx.clone();
            std::thread::spawn(move || {
                let opened = job.open();
                let _ = tx.send((job.pane, job.file, opened));
            });
        }

        // The mouse-toggle key releases/re-grabs capture so the terminal's own
        // selection and copy work while released.
        if app.mouse_capture() != captured {
            captured = app.mouse_capture();
            if captured {
                let _ = execute!(io::stdout(), EnableMouseCapture);
            } else {
                let _ = execute!(io::stdout(), DisableMouseCapture);
            }
        }

        if let Some(frame) = app.focus_change() {
            let _ = conn.send(&frame);
        }
        for frame in app.focus_reports() {
            let _ = conn.send(&frame);
        }
        if app.take_bell() {
            emit_terminal(b"\x07");
        }
        // Re-emit pane notifications (bell + OSC 9) to the user's real terminal
        // so it raises a desktop notification for background panes.
        for seq in app.take_terminal_out() {
            emit_terminal(&seq);
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Write raw bytes to the real terminal. Bells and OSC escapes are non-printing,
/// so they do not disturb the grid the next draw restores.
fn emit_terminal(bytes: &[u8]) {
    use std::io::Write;
    let mut out = io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Restore the terminal — raw mode and the alternate screen left, mouse
/// capture and the cursor's shape handed back — before any panic prints. A
/// pane frame that panics its terminal is the exception: that panic is
/// caught and the pane reseeded (`App::handle_frame`), so the TUI and its
/// screen are left as they are.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if app::feeding() {
            return;
        }
        // Capture may have been toggled at runtime; releasing when it was never
        // grabbed is harmless, and so is the default cursor shape.
        leave();
        ratatui::restore();
        previous(info);
    }));
}
