//! The attachable TUI. `run` owns terminal setup/teardown and the event loop;
//! the pieces it drives — connection, app state, input mapping, layout, and
//! rendering — live in the submodules and are unit-tested there.

mod app;
mod connection;
mod input;
mod layout;
mod render;
mod sidebar;

pub use app::App;

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use tutti_core::{Frame as WireFrame, Request};

use connection::Connection;

use crate::config::Config;

const POLL: Duration = Duration::from_millis(16);

/// Attach the interactive TUI to `session`, auto-starting the daemon. Restores
/// the terminal on exit and on panic, returning once the user detaches or the
/// server goes away. `config` supplies the prefix chord, direct bindings, and
/// the master mouse switch.
pub fn run(session: &str, config: Config) -> Result<()> {
    let mut conn = Connection::open(session)?;
    conn.send(&control(&Request::Attach))
        .context("send attach request")?;

    let mouse = config.mouse;
    let mut terminal = ratatui::init();
    if mouse {
        let _ = execute!(io::stdout(), EnableMouseCapture);
    }
    install_panic_hook(mouse);

    let result = event_loop(&mut terminal, &mut conn, config);

    if mouse {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, conn: &mut Connection, config: Config) -> Result<()> {
    let mut app = App::with_config(config);
    let mut dirty = true;
    let mut whichkey = false;

    loop {
        match conn.drain() {
            Ok(frames) => {
                if !frames.is_empty() {
                    for frame in frames {
                        app.handle_frame(frame);
                    }
                    dirty = true;
                }
            }
            Err(_) => break, // server closed the connection
        }

        // The which-key popup appears purely on elapsed time; redraw when its
        // visibility flips even without any input event.
        if app.whichkey_visible() != whichkey {
            whichkey = app.whichkey_visible();
            dirty = true;
        }

        if dirty {
            let area = terminal.draw(|frame| render::draw(frame, &app))?.area;
            let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
            for frame in app.sync_sizes(content) {
                let _ = conn.send(&frame);
            }
            dirty = false;
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
                        for frame in app.on_mouse(mouse.kind, mouse.column, mouse.row) {
                            conn.send(&frame)?;
                        }
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
                if !event::poll(Duration::ZERO).context("poll terminal input")? {
                    break;
                }
            }
        }

        if let Some(frame) = app.focus_change() {
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

fn install_panic_hook(mouse: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if mouse {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        previous(info);
    }));
}

fn control(request: &Request) -> WireFrame {
    WireFrame::Control(serde_json::to_vec(request).expect("serialize request"))
}
