//! Persistent PTY management: one native pty per pane, driven by a background
//! reader thread that feeds bytes into the pane's `rtty_vt` terminal. The
//! child keeps running whether or not anything is reading the UI — this is
//! the daemon's core.

use std::any::Any;
use std::borrow::Cow;
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rtty_vt::{ColorScheme, Event, MarkKind, ModeChange, MouseEncoding, MouseMode, Terminal};
use tokio::sync::watch;
use tutti_core::{AgentState, HostColors, Rgb, StateSource};

const SCROLLBACK_LINES: usize = 10_000;
/// Writes waiting for a pane's answer thread: a read's answers, or a
/// light/dark notice. A program that asks and never reads the answers stops
/// getting them past this, rather than stalling its own output behind them.
const ANSWER_QUEUE: usize = 64;
/// The longest sequence held from the terminal and the taps while it could
/// still be one the daemon answers or owns; past it, it goes on to both.
const HOLD_MAX: usize = 256;
/// What a program can leave on in its terminal that the shell after it
/// never asked for: the reports the terminal writes into the pane unbidden —
/// the mouse's (every tracking mode and encoding), focus (1004), light/dark
/// (2031) — and the key encodings the TUI types into it by, xterm's
/// modifyOtherKeys and kitty's flags (every entry popped). A program killed
/// with SIGKILL turns none of them off; a shell drawing its prompt sets its
/// own after the command's end (`Term::drain`). Bracketed paste stays: every
/// shell sets it itself.
const LEFT_ON: &[u8] =
    b"\x1b[?9;1000;1002;1003;1004;1005;1006;1015;1016;2031l\x1b[>4;0m\x1b[<65535u";
/// `LEFT_ON` less what never reaches a tap (`Verdict::Own`).
const LEFT_ON_TAPS: &[u8] = b"\x1b[?9;1000;1002;1003;1004;1005;1006;1015;1016l";

/// What to run in a pane.
#[derive(Debug, Clone)]
pub struct PtySpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Additional environment variables layered onto the inherited environment.
    pub env: Vec<(String, String)>,
    /// Inherited environment variables dropped before `env` is layered on.
    pub env_remove: Vec<String>,
    /// The host terminal's colours as of the spawn, so the program's first
    /// colour query is answered (`PtyPane::set_colors` keeps them current).
    pub colors: Option<HostColors>,
}

impl PtySpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            colors: None,
        }
    }
}

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSize {
    pub rows: u16,
    pub cols: u16,
}

impl PaneSize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }
}

/// How a child terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneExit {
    pub success: bool,
    pub code: u32,
}

/// A point-in-time view of a pane's screen, suitable for serving `pane read`.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// One entry per visible row, top to bottom.
    pub lines: Vec<String>,
    /// The output generation this grid reflects — exactly: both are read under
    /// the parser lock the reader thread bumps it under.
    pub generation: u64,
}

impl Snapshot {
    /// The screen flattened to a single newline-joined string.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// A `pane read` reply: the rows committed to the scrollback from a cursor
/// on, then the live screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// The committed rows past the cursor, oldest first, then the screen
    /// with its trailing blank rows trimmed.
    pub lines: Vec<String>,
    /// The next read's `since`: the rows ever committed, as of this read. It
    /// counts screen rows, not logical lines — `unwrapped` folds each read on
    /// its own, so a line wrapped across two reads comes back in two pieces.
    pub cursor: u64,
    /// Rows past the cursor had already fallen out of the 10k ring.
    pub truncated: bool,
}

/// A `pane read --last-output` reply: the output of the last command the
/// pane's shell finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastOutput {
    /// Its rows, trailing blank rows trimmed.
    pub lines: Vec<String>,
    /// The exit code its OSC 133 `D` carried.
    pub exit: Option<i32>,
    /// Its first rows had already fallen out of the 10k ring.
    pub truncated: bool,
}

/// The broadcast tick's look at a pane's screen (`PtyPane::look`), taken
/// under the parser lock.
pub struct Look {
    pub size: PaneSize,
    pub alt_screen: bool,
    /// The bytes that bring a client that holds the last look level: a
    /// redraw of the rows changed since — or, after a switch of screens or
    /// size, which a redraw cannot follow, a reset and a snapshot.
    pub delta: Vec<u8>,
    /// The bytes that bring a fresh client to the screen, when asked for.
    pub snapshot: Option<Vec<u8>>,
}

/// What a tap (`PtyPane::tap`) is handed, under the parser lock and in
/// stream order: a snapshot of the screen to start from, at its size, then
/// each read's bytes as the child wrote them, less the queries the daemon
/// answered and the sequences it owns (`verdict`) — a tap's own terminal
/// would answer or take them again — a switch of screens or a reset handed
/// on its own between the bytes around it; and after a resize, a fresh
/// snapshot at the new size.
pub enum Feed<'a> {
    Snapshot(PaneSize, Vec<u8>),
    Raw(&'a [u8]),
}

/// A subscriber to a pane's raw output. Returning false unsubscribes it.
pub type Tap = Box<dyn FnMut(Feed<'_>) -> bool + Send>;

/// What one read comes to.
#[derive(Default)]
struct Processed {
    /// The read for the taps: less what the daemon answered or owns, the
    /// start of a sequence it may yet answer held until the next read.
    raw: Vec<u8>,
    /// Where `raw` is cut into the reads a tap is handed: around each
    /// sequence that switches screens or resets the terminal (`switches`).
    /// `pane attach` redraws a read that does from its mirror, and so loses
    /// whatever else that read held — a bell, a cursor shape — unless the
    /// switch comes alone.
    cuts: Vec<usize>,
    /// The answers to write back to the pane, in order.
    answers: Vec<u8>,
    signals: Vec<Signal>,
}

/// A pane's terminal behind the parser lock, with the raw-output taps it
/// feeds in the same hold.
struct Term {
    vt: Terminal,
    /// How far into an escape sequence the bytes read so far stand.
    seq: Seq,
    /// The start of the sequence in hand, from an earlier read, while it
    /// could still be one the daemon answers or owns: held from the
    /// terminal and the taps until it ends, as it may reach neither.
    held: Vec<u8>,
    /// The sequence in hand went on to both, too long or plain to be one.
    released: bool,
    /// The last sequence was a string an `ESC` ended, and whether the taps
    /// were kept from it: an `ESC \` next is its terminator, and goes with it.
    string_kept: Option<bool>,
    /// The raw-output subscribers, by id: fed under this lock, so each sees
    /// its snapshot and then every read once, in order.
    taps: Vec<(u64, Tap)>,
    /// The engine panicked (`crash`): `vt` is a blank terminal telling of
    /// it, and nothing the pane writes reaches it.
    crashed: bool,
    /// When the program opened the synchronized update (2026) it has open.
    synced: Option<Instant>,
    /// The last shell mark was a command's end (OSC 133;D).
    ended: bool,
    /// A command's end turned off what it left on (`LEFT_ON`): the taps are
    /// told once the mark has ended, its ST included.
    taps_owed: bool,
}

/// The longest the render tick holds a pane's frame for a synchronized
/// update its program has yet to end: one that never does still draws.
const SYNC_HOLD: Duration = Duration::from_millis(250);

impl Term {
    fn new(size: PaneSize, colors: Option<HostColors>) -> Self {
        let mut term = Self {
            vt: Terminal::new(size.rows, size.cols, SCROLLBACK_LINES),
            seq: Seq::default(),
            held: Vec::new(),
            released: false,
            string_kept: None,
            taps: Vec::new(),
            crashed: false,
            synced: None,
            ended: false,
            taps_owed: false,
        };
        if let Some(colors) = colors {
            term.set_colors(colors);
        }
        term
    }

    /// Put a blank terminal of the same size in the place of an engine that
    /// panicked with `message`, telling of the crash, and hand every tap
    /// that screen to start from. Whatever the pane writes next is dropped.
    fn crash(&mut self, message: &str) {
        let (rows, cols) = self.vt.size();
        self.vt = Terminal::new(rows, cols, 0);
        self.vt
            .advance(format!("\r\ntutti: this pane's terminal crashed: {message}\r\n").as_bytes());
        (self.seq, self.released, self.string_kept, self.crashed) =
            (Seq::Ground, false, None, true);
        self.synced = None;
        self.held.clear();
        let (size, snapshot) = (PaneSize::new(rows, cols), self.vt.snapshot());
        self.taps
            .retain_mut(|(_, tap)| tap(Feed::Snapshot(size, snapshot.clone())));
    }

    /// Take the host terminal's colours, which the colour queries are
    /// answered from. A change of light/dark — or the first report — is
    /// told to a program that asked (mode 2031), as a terminal would.
    fn set_colors(&mut self, colors: HostColors) {
        let rgb =
            |Rgb { r, g, b }: Rgb| rtty_vt::Rgb((r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8);
        self.vt
            .set_host_color(rtty_vt::FOREGROUND, Some(rgb(colors.fg)));
        self.vt
            .set_host_color(rtty_vt::BACKGROUND, Some(rgb(colors.bg)));
        self.vt
            .set_host_color(rtty_vt::CURSOR, colors.cursor.map(rgb));
        let scheme = if colors.dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        self.vt.set_color_scheme(Some(scheme));
    }

    /// Feed one read to the terminal, and pick out what the taps must not
    /// get: the queries it answered, and the sequences the daemon owns.
    ///
    /// `Seq` follows vte through each escape sequence; where one ends,
    /// `settle` gives it its `verdict`. One that might draw an answer ends a
    /// run of bytes fed to the terminal, so the answer — and the cursor a
    /// position report reads — is that query's own; plain text and the
    /// other sequences go in runs as long as the read. A sequence a read cut
    /// off is held from both until a later read ends it.
    fn process(&mut self, bytes: &[u8]) -> Processed {
        if self.crashed {
            return Processed::default();
        }
        #[cfg(test)]
        if bytes
            .windows(tests::PANIC_READ.len())
            .any(|bytes| bytes == tests::PANIC_READ)
        {
            panic!("injected: the engine panics on this read");
        }
        let mut out = Processed {
            raw: Vec::with_capacity(bytes.len()),
            ..Processed::default()
        };
        // The terminal has `bytes[..fed]`; the sequence in hand starts at
        // `start` (after what `held` has of it).
        let (mut fed, mut start, mut i) = (0, 0, 0);
        while i < bytes.len() {
            if self.seq == Seq::Ground {
                let esc = memchr::memchr(0x1b, &bytes[i..]).map_or(bytes.len(), |at| i + at);
                out.raw.extend_from_slice(&bytes[i..esc]);
                (i, start) = (esc, esc);
                if i == bytes.len() {
                    break;
                }
            }
            let (next, ended) = self.seq.step(bytes[i]);
            self.seq = next;
            match ended {
                // An `ESC` ended it and opens the next: the terminal is
                // handed a CAN in its place, which ends a string and aborts
                // the rest as the `ESC` does, until the next is settled.
                Some(Ends::Before) => {
                    fed = self.settle(bytes, fed, start..i, b"\x18", &mut out);
                    start = i;
                }
                Some(Ends::With) => {
                    fed = self.settle(bytes, fed, start..i + 1, b"", &mut out);
                    start = i + 1;
                }
                None if !self.released => {
                    let len = self.held.len() + i + 1 - start;
                    if !self.seq.holds(bytes[i], len) {
                        self.released = true;
                        if !self.held.is_empty() {
                            let held = std::mem::take(&mut self.held);
                            self.vt.advance(&held);
                            out.raw.extend(held);
                        }
                    }
                }
                None => {}
            }
            i += 1;
        }
        let unheld = if self.seq == Seq::Ground || self.released {
            out.raw.extend_from_slice(&bytes[start.min(bytes.len())..]);
            bytes.len()
        } else {
            self.held.extend_from_slice(&bytes[start..]);
            start
        };
        self.vt.advance(&bytes[fed..unheld.max(fed)]);
        self.drain(&mut out);
        out
    }

    /// Settle the sequence vte has ended, `self.held` then `bytes[seq]`: the
    /// terminal is fed up to it, and it by its `verdict` — then `then` — and
    /// the taps get it unless the terminal answered it or the daemon owns
    /// it. Returns how much of `bytes` the terminal now has.
    fn settle(
        &mut self,
        bytes: &[u8],
        mut fed: usize,
        seq: std::ops::Range<usize>,
        then: &[u8],
        out: &mut Processed,
    ) -> usize {
        let held = std::mem::take(&mut self.held);
        let whole: Cow<[u8]> = if held.is_empty() {
            Cow::Borrowed(&bytes[seq.clone()])
        } else {
            Cow::Owned([held.as_slice(), &bytes[seq.clone()]].concat())
        };
        let released = std::mem::take(&mut self.released);
        let verdict = match self.string_kept.take() {
            // A string's ST goes where the string went.
            Some(true) if &*whole == b"\x1b\\" => Verdict::Own,
            Some(false) if &*whole == b"\x1b\\" => Verdict::Pass,
            _ if released => Verdict::Released,
            _ => verdict(&whole),
        };
        // Most sequences leave the terminal to take them with the rest of
        // the read: nothing to feed now.
        let feed = |vt: &mut Terminal, parts: &[&[u8]]| {
            for part in parts.iter().filter(|part| !part.is_empty()) {
                vt.advance(part);
            }
        };
        let kept = match verdict {
            Verdict::Ask | Verdict::Released => {
                feed(&mut self.vt, &[&held, &bytes[fed..seq.end], then]);
                fed = seq.end;
                self.drain(out) && verdict == Verdict::Ask
            }
            Verdict::Own | Verdict::Pass if then.is_empty() => {
                feed(&mut self.vt, &[&held]);
                verdict == Verdict::Own
            }
            Verdict::Own | Verdict::Pass => {
                feed(&mut self.vt, &[&held, &bytes[fed..seq.end], then]);
                fed = seq.end;
                verdict == Verdict::Own
            }
        };
        if !kept {
            let alone = switches(&whole);
            if alone {
                out.cuts.push(out.raw.len());
            }
            out.raw.extend_from_slice(if verdict == Verdict::Released {
                &bytes[seq]
            } else {
                &whole
            });
            if alone {
                out.cuts.push(out.raw.len());
            }
        }
        // A shell's marks are OSCs, drained here; one an `ESC` ended goes
        // on with its `\` first.
        if then.is_empty() && std::mem::take(&mut self.taps_owed) {
            out.raw.extend_from_slice(LEFT_ON_TAPS);
        }
        if !then.is_empty() && matches!(whole.get(1), Some(b']' | b'P')) {
            self.string_kept = Some(kept);
        }
        fed
    }

    /// Take the terminal's events: its answers to write back, and the
    /// signals tutti reads. True when there was an answer.
    fn drain(&mut self, out: &mut Processed) -> bool {
        let mut answered = false;
        for event in self.vt.take_events() {
            match event {
                Event::Reply(answer) => {
                    answered = true;
                    out.answers.extend(answer);
                }
                Event::Mode(ModeChange::SyncUpdate(open)) => {
                    self.synced = open.then(Instant::now);
                }
                // A shell marked the end of the command it ran: what that
                // left on goes off, at the mark — the shell's next prompt
                // sets its own after it (fish its 2031 and modifyOtherKeys).
                // Only at the first of a run of ends: zsh's integration marks
                // one from PROMPT_SP and one from precmd, and a hook between
                // them belongs to the shell.
                Event::Mark(mark) => {
                    let ended = matches!(mark.kind, MarkKind::CommandEnd { .. });
                    let modes = self.vt.modes();
                    let on = modes.mouse != MouseMode::None
                        || modes.mouse_encoding != MouseEncoding::Default
                        || modes.focus_reports
                        || modes.color_scheme_reports
                        || modes.modify_other_keys != 0
                        || self.vt.kitty_keyboard() != 0;
                    if ended && !self.ended && on {
                        self.vt.advance(LEFT_ON);
                        self.taps_owed = true;
                    }
                    self.ended = ended;
                    out.signals.extend(signal(Event::Mark(mark)));
                }
                event => out.signals.extend(signal(event)),
            }
        }
        answered
    }

    /// Whether the program has a synchronized update open, and opened it
    /// less than `SYNC_HOLD` ago.
    fn holding(&self) -> bool {
        self.synced.is_some_and(|at| at.elapsed() < SYNC_HOLD)
    }

    /// One broadcast tick's look; see `PtyPane::look`.
    fn look(&mut self, last: Option<(PaneSize, bool)>, snapshot: bool) -> Look {
        #[cfg(test)]
        if self.vt.title() == tests::PANIC_LOOK {
            panic!("injected: the engine panics drawing the screen");
        }
        let (rows, cols) = self.vt.size();
        let size = PaneSize::new(rows, cols);
        let alt_screen = self.vt.modes().alt_screen;
        let damage = self.vt.take_damage();
        let delta = if last == Some((size, alt_screen)) {
            self.vt.redraw(&damage)
        } else {
            [b"\x1bc".as_slice(), &self.vt.snapshot()].concat()
        };
        Look {
            size,
            alt_screen,
            delta,
            snapshot: snapshot.then(|| self.vt.snapshot()),
        }
    }

    /// The committed rows from absolute index `since` on, then the screen;
    /// see `PtyPane::tail`.
    fn tail(&self, since: u64, lines: Option<usize>, unwrapped: bool) -> Result<Tail> {
        let (top, oldest) = (self.vt.screen_top(), self.vt.oldest_row());
        if since > top {
            bail!(
                "cursor {since} is past this pane's {top}: it came from another pane or an \
                 earlier daemon"
            );
        }
        let mut out = self.lines(since, top + u64::from(self.vt.size().0));
        let committed = (top - since.max(oldest)) as usize;
        while out.len() > committed && out.last().is_some_and(|(text, _)| text.is_empty()) {
            out.pop();
        }
        Ok(Tail {
            lines: fold(out, lines, unwrapped),
            cursor: top,
            truncated: since < oldest,
        })
    }

    /// The last finished command's output; see `PtyPane::last_output`.
    fn last_output(&self, lines: Option<usize>, unwrapped: bool) -> Result<Option<LastOutput>> {
        let oldest = self.vt.oldest_row();
        let marks = self.vt.marks(oldest, u64::MAX);
        // Where a command's output began (`C`): the row of the mark at
        // column 0, else the next; and whether that left the ring — a `D`
        // with no mark held before it finished output that began earlier.
        let mut output = marks
            .first()
            .filter(|mark| oldest > 0 && matches!(mark.kind, MarkKind::CommandEnd { .. }))
            .map(|_| (oldest, true));
        let mut finished = None;
        for mark in &marks {
            match mark.kind {
                MarkKind::OutputStart => output = Some((mark.row + u64::from(mark.col > 0), false)),
                MarkKind::CommandEnd { exit } => {
                    if let Some((start, truncated)) = output.take() {
                        finished = Some((start, mark.row, mark.col, exit, truncated));
                    }
                }
                _ => {}
            }
        }
        let Some((start, end, end_col, exit, truncated)) = finished else {
            return Ok(None);
        };
        if self.vt.modes().alt_screen {
            bail!(
                "a full-screen program holds the pane's alternate screen: the last output is out \
                 of reach until it exits"
            );
        }
        let mut out = self.lines(start, end);
        // The cells before `D` on its own row are output too; what follows
        // there — the next prompt, zsh's `%` — is not. A `D` on the last
        // column takes its cell too: a wrap pending leaves the cursor there.
        let cols = self.vt.size().1;
        let end_col = if end_col + 1 >= cols { cols } else { end_col };
        if end_col > 0 && start <= end {
            out.push((self.vt.text((end, 0), (end, end_col - 1)), false));
        }
        while out.last().is_some_and(|(text, _)| text.is_empty()) {
            out.pop();
        }
        Ok(Some(LastOutput {
            lines: fold(out, lines, unwrapped),
            exit,
            truncated,
        }))
    }

    /// The text of the rows with absolute index in `[from, to)` the terminal
    /// still holds, oldest first, each with its soft-wrap flag.
    fn lines(&self, from: u64, to: u64) -> Vec<(String, bool)> {
        self.vt
            .rows(from, to)
            .map(|(_, row)| (row.text(), row.wrapped()))
            .collect()
    }
}

/// `Term::lines` as lines: with `unwrapped`, each soft-wrapped row joined
/// with the one after it; then only the newest `lines` kept.
fn fold(rows: Vec<(String, bool)>, lines: Option<usize>, unwrapped: bool) -> Vec<String> {
    let mut text: Vec<String> = Vec::with_capacity(rows.len());
    let mut continues = false;
    for (row, wrapped) in rows {
        match text.last_mut() {
            Some(line) if unwrapped && continues => line.push_str(&row),
            _ => text.push(row),
        }
        continues = wrapped;
    }
    if let Some(n) = lines
        && text.len() > n
    {
        text.drain(..text.len() - n);
    }
    text
}

/// What a terminal event says to tutti: an attention signal, the program's
/// word on its state, one of its user vars, or a shell drawing its prompt —
/// within tutti's caps — else nothing.
fn signal(event: Event) -> Option<Signal> {
    match event {
        Event::Bell => Some(Signal::Notify(Notification {
            title: None,
            body: None,
        })),
        Event::Notification { title, body } => {
            let len = title.as_ref().map_or(0, String::len) + body.as_ref().map_or(0, String::len);
            (len <= OSC_CAP).then_some(Signal::Notify(Notification { title, body }))
        }
        Event::Progress { state, .. } => {
            // 1 (normal), 3 (indeterminate) and 4 (paused/warning) are
            // Working; 2 (error) is Done — tutti has no errored state, and
            // Done queues it for attention; 0 clears.
            let state = match state {
                0 => None,
                2 => Some(AgentState::Done),
                _ => Some(AgentState::Working),
            };
            Some(Signal::Status(StatusReport {
                source: StateSource::Progress,
                state,
                app: None,
                msg: None,
            }))
        }
        Event::ProgramStatus(body) if body.len() <= OSC_CAP => {
            parse_program_status(&body).map(Signal::Status)
        }
        Event::UserVar { name, value }
            if name.len() <= USER_VAR_NAME_MAX && value.len() <= USER_VAR_VALUE_MAX =>
        {
            Some(Signal::UserVar { name, value })
        }
        Event::Mark(mark) if mark.kind == MarkKind::PromptStart => Some(Signal::Prompt),
        _ => None,
    }
}

/// Where the bytes read so far stand within an escape sequence — as much
/// of vte's state machine as `Term::process` needs to find where each
/// sequence starts and ends. It persists across reads, as a sequence can
/// straddle two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Seq {
    #[default]
    Ground,
    /// Just after `ESC`.
    Esc,
    /// After `ESC` and an intermediate byte: nothing but a plain escape.
    EscInter,
    /// Inside `ESC [`, up to its final byte.
    Csi,
    /// Inside `ESC ]`, up to its BEL or the `ESC` of its ST.
    Osc,
    /// Inside `ESC P`, up to the byte that hooks it.
    Dcs,
    /// A DCS's data, after its hook.
    DcsData,
    /// SOS, PM or APC, which nothing reads.
    Skipped,
}

/// Where a byte ends the sequence in hand.
enum Ends {
    /// With it.
    With,
    /// Before it: an `ESC`, which opens the next.
    Before,
}

impl Seq {
    /// Step over `b`, as vte does: only `ESC` opens a sequence (a raw C1
    /// byte is not one), and any `ESC` ends the one in hand — a string's
    /// with its ST to come, the others cut short; CAN and SUB end any; other
    /// C0 controls execute inside one without ending it.
    fn step(self, b: u8) -> (Seq, Option<Ends>) {
        use Seq::*;
        match (self, b) {
            (Ground, 0x1b) => (Esc, None),
            (_, 0x1b) => (Esc, Some(Ends::Before)),
            (Ground, _) => (Ground, None),
            (_, 0x18 | 0x1a) | (Osc, 0x07) | (DcsData, 0x9c) => (Ground, Some(Ends::With)),
            (Esc, b'[') => (Csi, None),
            (Esc, b']') => (Osc, None),
            (Esc, b'P') => (Dcs, None),
            (Esc, b'X' | b'^' | b'_') => (Skipped, None),
            (Esc | EscInter, 0x20..=0x2f) => (EscInter, None),
            (Esc | EscInter, 0x30..=0x7e) | (Csi, 0x40..=0x7e) => (Ground, Some(Ends::With)),
            (Dcs, 0x40..=0x7e) => (DcsData, None),
            (seq, _) => (seq, None),
        }
    }

    /// Whether the sequence in hand, `len` bytes long with `b` its last,
    /// could still be one the daemon answers or owns (`verdict`), so it is
    /// held from the terminal and the taps until it ends. An `ESC` and a C0
    /// control is no query: the control goes to the terminal at once.
    fn holds(self, b: u8, len: usize) -> bool {
        len <= HOLD_MAX
            && match self {
                Seq::Esc => b == 0x1b,
                Seq::Csi | Seq::Dcs | Seq::DcsData => true,
                // The colour queries are digits, `;` and `?` (OSC 4, 10–12).
                Seq::Osc => b == b']' || b.is_ascii_digit() || b == b';' || b == b'?',
                Seq::Ground | Seq::EscInter | Seq::Skipped => false,
            }
    }
}

/// What the daemon does with a sequence, by its bytes from `ESC` to its
/// end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// One the terminal might answer: fed at once, kept from the taps if it
    /// was answered — a tap's own terminal would answer it again, into the
    /// pane — and passed on if not, for theirs to answer.
    Ask,
    /// Kept from the taps alone: mode 2031 set or reset — the daemon tells a
    /// pane of light/dark changes itself (`PtyPane::set_colors`); the
    /// keyboard protocols, xterm's modifyOtherKeys (`CSI > 4 ; n m`) and
    /// kitty's push, pop and set (`CSI > u`, `CSI < u`, `CSI = u`), which the
    /// TUI encodes a pane's keys by, but a `pane attach` terminal that took
    /// either would encode the prefix where the detach cannot see it, and
    /// keep it on past the detach (nvim and vim ask for modifyOtherKeys level
    /// 2, Emacs for 1; nvim pushes kitty flags once its query, an `Ask`, is
    /// answered); and the ST of a string kept from them.
    Own,
    /// Anything else: on to both.
    Pass,
    /// One that went on to both before it ended, too long or plain to be a
    /// query: what else it had goes on too.
    Released,
}

fn verdict(seq: &[u8]) -> Verdict {
    match seq {
        [0x1b, b'[', body @ .., end @ 0x40..=0x7e] => {
            // C0 controls inside execute as vte executes them.
            let params = || body.iter().copied().filter(|&b| b >= 0x20);
            let private = params().next().filter(|b| b"<=>?".contains(b));
            let first = params()
                .skip(usize::from(private.is_some()))
                .take_while(u8::is_ascii_digit)
                .fold(0u32, |n, b| n.saturating_mul(10) + u32::from(b - b'0'));
            match (private, end) {
                (Some(b'?'), b'u') => Verdict::Ask,
                (Some(_), b'u') | (Some(b'>'), b'm') if *end == b'u' || first == 4 => Verdict::Own,
                _ if params().eq(*b"?2031") && matches!(end, b'h' | b'l') => Verdict::Own,
                (Some(_), b'm') | (_, b'c' | b'n' | b'p' | b'q' | b't') => Verdict::Ask,
                _ => Verdict::Pass,
            }
        }
        [0x1b, b'Z'] | [0x1b, b']' | b'P', ..] => Verdict::Ask,
        _ => Verdict::Pass,
    }
}

/// Whether a sequence switches screens (`CSI ? 47|1047|1049 h|l`, any
/// among others) or resets the terminal (`ESC c`).
fn switches(seq: &[u8]) -> bool {
    match seq {
        [0x1b, b'c'] => true,
        [0x1b, b'[', b'?', params @ .., b'h' | b'l'] => params
            .split(|&b| b == b';')
            .any(|mode| matches!(mode, b"47" | b"1047" | b"1049")),
        _ => false,
    }
}

/// A persistent pty running a single child process.
pub struct PtyPane {
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Shared with the answer thread, which writes back what the daemon says
    /// on the pane's terminal's behalf.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// The answer thread's queue, which the reader holds too.
    answers: SyncSender<Vec<u8>>,
    term: Arc<Mutex<Term>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// The child's OS process id, the root of the process-tree walk that agent
    /// detection matches against. `None` if the platform did not report one.
    pid: Option<u32>,
    size: Mutex<PaneSize>,
    exit_rx: watch::Receiver<Option<PaneExit>>,
    output_rx: watch::Receiver<u64>,
    /// Bells and OSC 9 / 777 desktop notifications from the pane's terminal,
    /// drained by the broadcast tick — interleaved, in stream order, with
    /// the program's own status reports (OSC 9;4 / 7501), the one signal
    /// here that does feed pane state (under the source ranking), its user
    /// vars (OSC 1337) and its shell's prompts (OSC 133;A).
    notifications: Arc<Mutex<Vec<Signal>>>,
    reader_thread: Option<JoinHandle<()>>,
    wait_thread: Option<JoinHandle<()>>,
}

impl PtyPane {
    /// Spawn `spec` on a fresh native pty of the given size. The child runs
    /// detached from any UI; output is fed to its terminal by a background
    /// thread from the moment this returns.
    pub fn spawn(spec: PtySpec, size: PaneSize) -> Result<Self> {
        // portable-pty silently substitutes $HOME when the requested cwd is not
        // a directory, which would launch an agent against the wrong tree.
        if let Some(cwd) = &spec.cwd
            && !cwd.is_dir()
        {
            bail!("working directory does not exist: {}", cwd.display());
        }
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        for key in &spec.env_remove {
            cmd.env_remove(key);
        }
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn {}", spec.program))?;
        // Drop our copy of the slave so the master reader sees EOF once the
        // child's own descriptors close.
        drop(pair.slave);

        let killer = child.clone_killer();
        let pid = child.process_id();
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let writer = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .context("failed to take pty writer")?,
        ));

        let term = Arc::new(Mutex::new(Term::new(size, spec.colors)));

        let (output_tx, output_rx) = watch::channel(0u64);
        let (exit_tx, exit_rx) = watch::channel(None);
        let notifications = Arc::new(Mutex::new(Vec::new()));

        // Answers go out on a thread of their own, off the parser lock: a
        // program that asks and never reads them fills its input and blocks
        // that thread, never the reader — which would leave its output
        // undrained. The thread ends once the reader and the pane have both
        // let go of its queue.
        let (answers, answers_rx) = mpsc::sync_channel::<Vec<u8>>(ANSWER_QUEUE);
        let answer_writer = Arc::clone(&writer);
        std::thread::Builder::new()
            .name("tutti-pty-answers".into())
            .spawn(move || {
                for answer in answers_rx {
                    let mut writer = lock(&answer_writer);
                    let _ = writer.write_all(&answer).and_then(|()| writer.flush());
                }
            })
            .context("failed to spawn pty answer thread")?;

        let reader_term = Arc::clone(&term);
        let reader_notifications = Arc::clone(&notifications);
        let reader_answers = answers.clone();
        let reader_thread = std::thread::Builder::new()
            .name("tutti-pty-reader".into())
            .spawn(move || {
                read_loop(
                    reader,
                    reader_term,
                    pid,
                    output_tx,
                    reader_notifications,
                    reader_answers,
                )
            })
            .context("failed to spawn pty reader thread")?;

        let wait_thread = std::thread::Builder::new()
            .name("tutti-pty-wait".into())
            .spawn(move || {
                let mut child = child;
                let exit = match child.wait() {
                    Ok(status) => PaneExit {
                        success: status.success(),
                        code: status.exit_code(),
                    },
                    Err(_) => PaneExit {
                        success: false,
                        code: u32::MAX,
                    },
                };
                let _ = exit_tx.send(Some(exit));
            })
            .context("failed to spawn pty wait thread")?;

        Ok(Self {
            master: Mutex::new(pair.master),
            writer,
            answers,
            term,
            killer: Mutex::new(killer),
            pid,
            size: Mutex::new(size),
            exit_rx,
            output_rx,
            notifications,
            reader_thread: Some(reader_thread),
            wait_thread: Some(wait_thread),
        })
    }

    /// Run `f` on the pane's terminal, under its lock. The pane is the unit
    /// of failure: a panic in its engine is caught inside the lock hold — so
    /// the lock the hub, the reader and every caller take is never poisoned
    /// — and ends this pane alone (`crashed`); `f` then runs on the blank
    /// terminal that tells of the crash.
    fn with<R>(&self, mut f: impl FnMut(&mut Term) -> R) -> R {
        let mut term = lock(&self.term);
        match catch_unwind(AssertUnwindSafe(|| f(&mut term))) {
            Ok(out) => out,
            Err(panic) => {
                crashed(&mut term, &*panic, self.pid, &self.notifications);
                f(&mut term)
            }
        }
    }

    /// Take the host terminal's colours for the pane's colour queries. A
    /// change of light/dark — or the first report — is told to a program
    /// that asked (mode 2031), as a terminal would: `CSI ? 997 ; 1|2 n`.
    pub fn set_colors(&self, colors: HostColors) {
        let answers = self.with(|term| {
            term.set_colors(colors);
            let mut out = Processed::default();
            term.drain(&mut out);
            out.answers
        });
        if !answers.is_empty() {
            let _ = self.answers.try_send(answers);
        }
    }

    /// Forward raw keystrokes to the child.
    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = lock(&self.writer);
        writer.write_all(bytes).context("write to pty failed")?;
        writer.flush().context("flush pty failed")?;
        Ok(())
    }

    /// Resize both the kernel pty and the terminal, which reflows its
    /// primary screen. A zero dimension is refused. Each tap is handed a
    /// fresh snapshot at the new size: a terminal reflows its screen its own
    /// way, and this puts it level with the grid again before the program's
    /// redraw lands.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        if rows == 0 || cols == 0 {
            bail!("pane size must be at least 1x1, got {rows}x{cols}");
        }
        lock(&self.master)
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty failed")?;
        let size = PaneSize { rows, cols };
        self.with(|term| {
            term.vt.resize(rows, cols);
            if !term.taps.is_empty() {
                let snapshot = term.vt.snapshot();
                term.taps
                    .retain_mut(|(_, tap)| tap(Feed::Snapshot(size, snapshot.clone())));
            }
        });
        *lock(&self.size) = size;
        Ok(())
    }

    /// Stream the pane's raw output to `tap`, as subscriber `id`. Under one
    /// hold of the parser lock it is handed a snapshot of the screen as it
    /// stands and joins the reader, which hands it each read in the same
    /// hold that parses it — so nothing is lost or repeated between the two.
    pub fn tap(&self, id: u64, tap: Tap) {
        let mut tap = Some(tap);
        self.with(|term| {
            let (rows, cols) = term.vt.size();
            let snapshot = term.vt.snapshot();
            if let Some(mut tap) = tap.take()
                && tap(Feed::Snapshot(PaneSize::new(rows, cols), snapshot))
            {
                term.taps.push((id, tap));
            }
        });
    }

    /// Unsubscribe tap `id`, if it still is.
    pub fn untap(&self, id: u64) {
        lock(&self.term).taps.retain(|(tap, _)| *tap != id);
    }

    /// The size the pane was spawned at or last resized to.
    pub fn size(&self) -> PaneSize {
        *lock(&self.size)
    }

    /// Current screen contents, cursor position, and size.
    pub fn snapshot(&self) -> Snapshot {
        self.with(|term| {
            let (rows, cols) = term.vt.size();
            let (cursor_row, cursor_col) = term.vt.cursor_position();
            let lines = (0..rows).map(|r| term.vt.screen_row(r).text()).collect();
            Snapshot {
                rows,
                cols,
                cursor_row,
                cursor_col,
                lines,
                generation: *self.output_rx.borrow(),
            }
        })
    }

    /// The broadcast tick's look at the screen, the one consumer of its
    /// damage: given the size and screen of the last look, if any, the
    /// bytes that bring a client holding it level, and with `snapshot` the
    /// bytes for a client holding none.
    pub fn look(&self, last: Option<(PaneSize, bool)>, snapshot: bool) -> Look {
        self.with(|term| term.look(last, snapshot))
    }

    /// Whether the render tick holds the pane's frame: its program has a
    /// synchronized update (2026) open — a frame drawn now would show it
    /// half done — and opened it less than `SYNC_HOLD` ago.
    pub fn holding(&self) -> bool {
        self.with(|term| term.holding())
    }

    /// A client snapshot of the screen scrolled `offset` rows back into the
    /// scrollback, with its size: the rows from there drawn, no cursor.
    pub fn scrolled_snapshot(&self, offset: usize) -> (PaneSize, Vec<u8>) {
        self.with(|term| {
            let (rows, cols) = term.vt.size();
            let snapshot = term.vt.scrolled_snapshot(offset as u64);
            (PaneSize::new(rows, cols), snapshot)
        })
    }

    /// The live screen's text, soft-wrapped rows joined: the classifier
    /// scans it.
    pub fn contents(&self) -> String {
        self.with(|term| term.vt.contents())
    }

    /// The rows committed to the primary scrollback from absolute index
    /// `since` on (clamped to the oldest the ring still holds), then the live
    /// screen with its trailing blank rows trimmed — `since: 0` reads it all.
    /// `lines` keeps only the newest that many; `unwrapped` joins soft-wrapped
    /// rows back into logical lines. While the alternate screen is up, the
    /// screen read is that one, after the primary's committed rows. Formats
    /// under the parser lock, in time proportional to the rows returned. A
    /// cursor past the pane's count — from another pane, or an earlier
    /// daemon — is refused. A resize reflows the rows, and can number the
    /// rows it keeps afresh: a cursor from before it may see a row again,
    /// never skip one.
    pub fn tail(&self, since: u64, lines: Option<usize>, unwrapped: bool) -> Result<Tail> {
        self.with(|term| term.tail(since, lines, unwrapped))
    }

    /// The output of the last command the pane's shell finished, between the
    /// OSC 133 `C` and `D` marks `tutti shell-init` emits around it, which
    /// travel with their rows through a reflow: the rows the ring and screen
    /// still hold, trailing blank rows trimmed, `lines` and `unwrapped` as
    /// for `tail`, and the exit code its `D` carried. A command still
    /// running leaves the one before it. `None` when no command has finished
    /// under shell integration; an error while the alternate screen is up,
    /// which covers the primary's screen.
    pub fn last_output(&self, lines: Option<usize>, unwrapped: bool) -> Result<Option<LastOutput>> {
        self.with(|term| term.last_output(lines, unwrapped))
    }

    /// The working directory the pane's shell last reported (OSC 7).
    pub fn cwd(&self) -> Option<String> {
        self.with(|term| term.vt.cwd().map(str::to_string))
    }

    /// The child's OS process id, if the platform reported one at spawn.
    pub fn child_pid(&self) -> Option<u32> {
        self.pid
    }

    /// The child's exit status, or `None` if it is still running.
    pub fn exit_status(&self) -> Option<PaneExit> {
        *self.exit_rx.borrow()
    }

    /// Await child termination, resolving immediately if it has already exited.
    pub async fn wait(&self) -> PaneExit {
        let mut rx = self.exit_rx.clone();
        loop {
            let current = *rx.borrow();
            if let Some(exit) = current {
                return exit;
            }
            if rx.changed().await.is_err() {
                return rx.borrow().unwrap_or(PaneExit {
                    success: false,
                    code: u32::MAX,
                });
            }
        }
    }

    /// A receiver that fires whenever new output is folded into the grid.
    pub fn output_receiver(&self) -> watch::Receiver<u64> {
        self.output_rx.clone()
    }

    /// Drain the bells, desktop notifications and status reports seen since
    /// the last call, in stream order.
    pub fn take_notifications(&self) -> Vec<Signal> {
        std::mem::take(&mut *lock(&self.notifications))
    }

    /// A receiver that resolves to `Some(exit)` when the child terminates.
    pub fn exit_receiver(&self) -> watch::Receiver<Option<PaneExit>> {
        self.exit_rx.clone()
    }

    /// Terminate the child and every descendant sharing its pty.
    ///
    /// portable-pty makes the child a session/process-group leader (`setsid`
    /// before exec), so its pid doubles as its process-group id. We SIGKILL the
    /// whole group: a descendant that inherited the slave pty (a backgrounded
    /// grandchild, say) would otherwise outlive the direct child — leaking, and
    /// on Linux holding the master reader open past EOF so the reader-thread
    /// join in `Drop` hangs. portable-pty's own killer only SIGHUPs the direct
    /// child, which a descendant can ignore or a grandchild never receives.
    pub fn kill(&self) -> Result<()> {
        if let Some(pid) = self.pid {
            signal_group(pid, SIGKILL);
            return Ok(());
        }
        // No pid was reported (not expected on Unix): fall back to the direct
        // child so we at least terminate it.
        lock(&self.killer).kill().context("kill child failed")
    }
}

impl Drop for PtyPane {
    fn drop(&mut self) {
        // Kill the whole process group (see `kill`) so a lingering descendant
        // cannot keep the reader blocked and hang the joins below.
        let _ = self.kill();
        if let Some(handle) = self.reader_thread.take() {
            join_or_detach(handle);
        }
        if let Some(handle) = self.wait_thread.take() {
            join_or_detach(handle);
        }
    }
}

/// SIGKILL: the child called `setsid`, so its pid is its process-group id and a
/// negative pid signals the whole group. We bind `kill(2)` directly rather than
/// via the `libc` crate, which is not a declared dependency — mirroring how
/// `tutti-core` binds `getuid`.
const SIGKILL: i32 = 9;

/// Upper bound on how long `Drop` waits for a pty thread before detaching it, so
/// a pathological descendant that survives SIGKILL while holding the slave open
/// cannot hang shutdown indefinitely.
const JOIN_BACKSTOP: Duration = Duration::from_secs(2);

/// SIGKILL the process group led by `pid` (a negative pid targets the group).
fn signal_group(pid: u32, sig: i32) {
    let _ = libc_kill(-(pid as i32), sig);
}

/// Join `handle`, but detach rather than block indefinitely if it does not
/// finish within `JOIN_BACKSTOP`. After a process-group kill the reader sees EOF
/// and returns at once; this only guards a descendant that survives the kill
/// while holding the slave open.
fn join_or_detach(handle: JoinHandle<()>) {
    let deadline = std::time::Instant::now() + JOIN_BACKSTOP;
    while !handle.is_finished() {
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = handle.join();
}

/// Lock a pty mutex, recovering it if a panicking thread poisoned it: one
/// pane's panic must not turn every later lock — the render tick's among them
/// — into another.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// End the pane whose engine panicked with `panic`: the panic message to the
/// daemon's log, the blank terminal that tells of it in the engine's place
/// (`Term::crash`), a notification, and the child's process group killed —
/// which closes the pty, ending the reader. Nothing else is touched.
fn crashed(
    term: &mut Term,
    panic: &(dyn Any + Send),
    pid: Option<u32>,
    notifications: &Mutex<Vec<Signal>>,
) {
    let message = panic
        .downcast_ref::<&str>()
        .map(|message| message.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".into());
    eprintln!("tutti: the terminal of a pane (pid {pid:?}) panicked: {message}; ending that pane");
    term.crash(&message);
    lock(notifications).push(Signal::Notify(Notification {
        title: Some("pane crashed".into()),
        body: Some(message),
    }));
    if let Some(pid) = pid {
        signal_group(pid, SIGKILL);
    }
}

fn read_loop(
    mut reader: Box<dyn Read + Send>,
    term: Arc<Mutex<Term>>,
    pid: Option<u32>,
    output_tx: watch::Sender<u64>,
    notifications: Arc<Mutex<Vec<Signal>>>,
    answers: SyncSender<Vec<u8>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Bumped under the parser lock, so a `snapshot` sees a grid and
                // a generation that agree; the signals are queued in the same
                // hold, so the tick that sees the screen they came with sees
                // them too. A panic handling the read is caught inside that
                // hold, so it never poisons the lock, and ends this pane.
                let mut term = lock(&term);
                let handled = catch_unwind(AssertUnwindSafe(|| {
                    let Processed {
                        raw,
                        cuts,
                        answers,
                        signals,
                    } = term.process(&buf[..n]);
                    let mut from = 0;
                    for to in cuts.into_iter().chain([raw.len()]) {
                        let piece = &raw[from..to];
                        if !piece.is_empty() {
                            term.taps.retain_mut(|(_, tap)| tap(Feed::Raw(piece)));
                        }
                        from = to;
                    }
                    (answers, signals)
                }));
                let (answer, signals) = match handled {
                    Ok(handled) => handled,
                    Err(panic) => {
                        crashed(&mut term, &*panic, pid, &notifications);
                        break;
                    }
                };
                if !signals.is_empty() {
                    let mut queue = lock(&notifications);
                    queue.extend(signals);
                    let overflow = queue.len().saturating_sub(NOTIFY_QUEUE_CAP);
                    if overflow > 0 {
                        queue.drain(..overflow);
                    }
                }
                output_tx.send_modify(|generation| *generation = generation.wrapping_add(1));
                drop(term);
                if !answer.is_empty() {
                    let _ = answers.try_send(answer);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // The last screen — a crash's, or the end of a pane its crash killed —
    // goes out with the next tick.
    output_tx.send_modify(|generation| *generation = generation.wrapping_add(1));
}

/// A desktop notification or bell surfaced from a pane's raw output. A bare
/// bell carries no text (both fields `None`); OSC 9 fills `body`; OSC 777 fills
/// `title` and/or `body`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: Option<String>,
    pub body: Option<String>,
}

/// A program's own word on its state, from its output: an OSC 9;4 progress
/// report (`source` Progress) or an OSC 7501 program status (`source`
/// Program). `state: None` is a clear — it ends that source's claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub source: StateSource,
    pub state: Option<AgentState>,
    /// The OSC 7501 `app`: the label the pane shows under where agent state
    /// does, when no agent was detected there.
    pub app: Option<String>,
    /// The decoded OSC 7501 `msg`: the notification body on a move to
    /// done/blocked.
    pub msg: Option<String>,
}

/// What a pane's terminal tells tutti: an attention signal, the program's
/// word on its state, one of its user vars (OSC 1337 `SetUserVar`) — an
/// empty `value` unsets it — or a shell drawing its prompt (OSC 133;A),
/// which says no editor holds the pane any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    Notify(Notification),
    Status(StatusReport),
    UserVar { name: String, value: String },
    Prompt,
}

/// Largest notification (title and body together) and OSC 7501 report
/// taken; a longer one is dropped, so the notification history cannot be
/// filled with megabytes.
const OSC_CAP: usize = 4096;
/// Cap on pending notifications per pane, bounding the queue when a burst
/// arrives with no client draining it.
const NOTIFY_QUEUE_CAP: usize = 256;
/// OSC 7501 limits from the program-status spec: the longest key and `app`,
/// and the base64 `msg`/`title` as (encoded, decoded) bytes.
const KEY_MAX: usize = 16;
const APP_MAX: usize = 32;
const MSG_MAX: (usize, usize) = (2732, 2048);
const TITLE_MAX: (usize, usize) = (256, 192);
/// OSC 1337 user var limits: the longest name, and the longest value once
/// decoded.
const USER_VAR_NAME_MAX: usize = 64;
const USER_VAR_VALUE_MAX: usize = 1024;
/// Standard-alphabet base64 that takes padding or leaves it off, as OSC 7501
/// allows.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Parse an OSC 7501 program-status body: `key=value` pairs joined by `:`
/// (tuios.dev/docs/program-status.md). Only the root record is tracked, so a
/// report naming an `id` is ignored, as is the `?` support probe (answering
/// means writing into the pty). Per the spec, a report with a missing/unknown
/// `state`, a `kind` on a non-blocked state, or a `progress` outside 0–100 is
/// ignored; one breaking a length limit, or whose `msg`/`title` is not base64
/// of control-free UTF-8, is discarded whole. A malformed pair is skipped and
/// the rest still read; a repeated key's last value wins.
fn parse_program_status(body: &str) -> Option<StatusReport> {
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let well_formed = !key.is_empty()
            && key.bytes().all(|b| b.is_ascii_lowercase())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b));
        if !well_formed {
            continue;
        }
        if key.len() > KEY_MAX {
            return None;
        }
        pairs.push((key, value));
    }
    let get = |key: &str| pairs.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v);
    let state = match get("state")? {
        "idle" => Some(AgentState::Idle),
        "working" => Some(AgentState::Working),
        "blocked" => Some(AgentState::Blocked),
        "done" | "error" => Some(AgentState::Done),
        "clear" => None,
        _ => return None,
    };
    let percent =
        |p: &str| p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u8>().is_ok_and(|p| p <= 100);
    if get("id").is_some()
        || (get("kind").is_some() && state != Some(AgentState::Blocked))
        || get("progress").is_some_and(|p| !percent(p))
    {
        return None;
    }
    let app = get("app");
    if app.is_some_and(|app| app.len() > APP_MAX) {
        return None;
    }
    let msg = match get("msg") {
        Some(msg) => Some(decode_text(msg, MSG_MAX)?),
        None => None,
    };
    if let Some(title) = get("title") {
        decode_text(title, TITLE_MAX)?;
    }
    Some(StatusReport {
        source: StateSource::Program,
        state,
        app: app.map(str::to_string),
        msg,
    })
}

/// Decode a base64 `msg`/`title` within its (encoded, decoded) byte caps into
/// control-free UTF-8; `None` discards the report.
fn decode_text(encoded: &str, (encoded_max, decoded_max): (usize, usize)) -> Option<String> {
    if encoded.len() > encoded_max {
        return None;
    }
    let text = String::from_utf8(BASE64.decode(encoded).ok()?).ok()?;
    (text.len() <= decoded_max && !text.chars().any(char::is_control)).then_some(text)
}

unsafe extern "C" {
    #[link_name = "kill"]
    safe fn libc_kill(pid: i32, sig: i32) -> i32;
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PtyPane>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader's buffer.
    const READ: usize = 8192;

    /// What makes a pane's engine panic, through the test hooks: a read that
    /// holds these bytes (`Term::process`), or drawing a screen with this
    /// title (`Term::look`).
    pub(super) const PANIC_READ: &[u8] = b"tutti\x01panic";
    pub(super) const PANIC_LOOK: &str = "tutti-panic-look";

    fn signals(input: &[u8]) -> Vec<Signal> {
        Term::new(PaneSize::new(24, 80), None)
            .process(input)
            .signals
    }

    fn notes(signals: Vec<Signal>) -> Vec<Notification> {
        signals
            .into_iter()
            .filter_map(|s| match s {
                Signal::Notify(note) => Some(note),
                _ => None,
            })
            .collect()
    }

    fn scan(input: &[u8]) -> Vec<Notification> {
        notes(signals(input))
    }

    /// The status reports in `input`, one entry per report.
    fn scan_status(input: &[u8]) -> Vec<StatusReport> {
        signals(input)
            .into_iter()
            .filter_map(|s| match s {
                Signal::Status(report) => Some(report),
                _ => None,
            })
            .collect()
    }

    /// Feed `input` in two reads split at `cut`, exercising cross-read state.
    fn signals_split(input: &[u8], cut: usize) -> Vec<Signal> {
        let mut term = Term::new(PaneSize::new(24, 80), None);
        let mut out = term.process(&input[..cut]).signals;
        out.extend(term.process(&input[cut..]).signals);
        out
    }

    fn scan_split(input: &[u8], cut: usize) -> Vec<Notification> {
        notes(signals_split(input, cut))
    }

    fn progress(state: Option<AgentState>) -> StatusReport {
        StatusReport {
            source: StateSource::Progress,
            state,
            app: None,
            msg: None,
        }
    }

    fn program(state: Option<AgentState>, app: Option<&str>, msg: Option<&str>) -> StatusReport {
        StatusReport {
            source: StateSource::Program,
            state,
            app: app.map(Into::into),
            msg: msg.map(Into::into),
        }
    }

    /// Regression: an OSC 9;4 progress report used to surface as an OSC 9
    /// notification with body `4;1;50`.
    #[test]
    fn osc9_4_progress_is_never_a_notification() {
        for input in [
            b"\x1b]9;4;1;50\x07".as_slice(),
            b"\x1b]9;4;0\x1b\\",
            b"\x1b]9;4;9;10\x07",
            b"\x1b]9;4\x07",
            b"\x1b]9;4;\x07",
        ] {
            assert!(scan(input).is_empty(), "{input:?} became a notification");
        }
    }

    /// ConEmu's other numbered subcommands — `9;9;<cwd>`, which shells emit
    /// for Windows Terminal, and the rest — surface nothing either, while a
    /// body that merely starts with digits is still a notification.
    #[test]
    fn osc9_subcommands_are_never_notifications() {
        for input in [
            b"\x1b]9;9;/Users/me/src\x1b\\".as_slice(),
            b"\x1b]9;9;\x07",
            b"\x1b]9;1;500\x07",
            b"\x1b]9;2;hello there\x07",
            b"\x1b]9;10;1\x07",
            b"\x1b]9;12\x07",
        ] {
            assert!(signals(input).is_empty(), "{input:?} surfaced");
        }
        assert_eq!(
            scan(b"\x1b]9;42 tests passed\x07"),
            vec![Notification {
                title: None,
                body: Some("42 tests passed".into()),
            }]
        );
    }

    #[test]
    fn osc9_4_states_map_onto_progress_reports() {
        use AgentState::*;
        let cases: &[(&[u8], Option<StatusReport>)] = &[
            (b"\x1b]9;4;0\x1b\\", Some(progress(None))),
            (b"\x1b]9;4;0;0\x07", Some(progress(None))),
            (b"\x1b]9;4;1;40\x1b\\", Some(progress(Some(Working)))),
            (b"\x1b]9;4;1\x07", Some(progress(Some(Working)))),
            (b"\x1b]9;4;2;100\x07", Some(progress(Some(Done)))),
            (b"\x1b]9;4;3\x07", Some(progress(Some(Working)))),
            (b"\x1b]9;4;4;70\x1b\\", Some(progress(Some(Working)))),
            (b"\x1b]9;4;5\x07", None),
            (b"\x1b]9;4\x07", None),
        ];
        for (input, want) in cases {
            assert_eq!(scan_status(input).pop(), *want, "for {input:?}");
        }
    }

    #[test]
    fn osc9_4_is_incremental_across_every_cut_point() {
        let input = b"a\x1b]9;4;1;40\x1b\\b\x1b]9;4;0\x07";
        let whole = signals(input);
        assert_eq!(
            whole,
            vec![
                Signal::Status(progress(Some(AgentState::Working))),
                Signal::Status(progress(None)),
            ]
        );
        for cut in 0..=input.len() {
            assert_eq!(
                signals_split(input, cut),
                whole,
                "mismatch cutting at {cut}"
            );
        }
    }

    #[test]
    fn osc7501_valid_reports_with_bel_and_st() {
        use AgentState::*;
        // "Compiling" / "Built" in standard base64, padded and not.
        assert_eq!(
            scan_status(b"\x1b]7501;state=working:app=build:progress=40:msg=Q29tcGlsaW5n\x1b\\"),
            vec![program(Some(Working), Some("build"), Some("Compiling"))]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=done:app=build:msg=QnVpbHQ=\x07"),
            vec![program(Some(Done), Some("build"), Some("Built"))]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=done:msg=QnVpbHQ\x07"),
            vec![program(Some(Done), None, Some("Built"))]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=blocked:kind=permission\x07"),
            vec![program(Some(Blocked), None, None)]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=error\x07"),
            vec![program(Some(Done), None, None)]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=idle:title=QnVpbGQ=\x07"),
            vec![program(Some(Idle), None, None)]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=clear\x1b\\"),
            vec![program(None, None, None)]
        );
        // Whitespace is trimmed, unknown keys are ignored, a malformed pair (no
        // `=`, or a byte outside the value set) is skipped while the rest still
        // counts, and a repeated key's last value wins.
        assert_eq!(
            scan_status(b"\x1b]7501; state = idle :future=1:junk:app=a b:state=working\x07"),
            vec![program(Some(Working), None, None)]
        );
        assert_eq!(
            scan_status(b"\x1b]7501;state=done:msg=Zm9v$\x07"),
            vec![program(Some(Done), None, None)]
        );
        assert!(
            scan(b"\x1b]7501;state=working\x07").is_empty(),
            "no notification"
        );
    }

    #[test]
    fn osc7501_invalid_reports_are_dropped() {
        let long_key = format!("\x1b]7501;state=working:{}=1\x07", "k".repeat(KEY_MAX + 1));
        let long_app = format!(
            "\x1b]7501;state=working:app={}\x07",
            "a".repeat(APP_MAX + 1)
        );
        let long_msg = format!(
            "\x1b]7501;state=done:msg={}\x07",
            BASE64.encode("m".repeat(MSG_MAX.1 + 1))
        );
        let long_title = format!(
            "\x1b]7501;state=done:title={}\x07",
            BASE64.encode("t".repeat(TITLE_MAX.1 + 1))
        );
        let control_msg = format!("\x1b]7501;state=done:msg={}\x07", BASE64.encode("a\nb"));
        let non_utf8_msg = format!(
            "\x1b]7501;state=done:msg={}\x07",
            BASE64.encode([0xff, 0xfe])
        );
        let control_title = format!(
            "\x1b]7501;state=done:title={}\x07",
            BASE64.encode("\x1b[2J")
        );
        // Ignored: no/unknown state, kind off blocked, progress outside 0–100,
        // a non-root record, the support probe. Discarded: a broken limit, or a
        // msg/title that is not base64 of control-free UTF-8.
        for input in [
            "\x1b]7501;app=build\x07",
            "\x1b]7501;state=sleeping\x07",
            "\x1b]7501;state=working:kind=permission\x07",
            "\x1b]7501;state=working:progress=101\x07",
            "\x1b]7501;state=working:progress=-1\x07",
            "\x1b]7501;state=working:progress=+5\x07",
            "\x1b]7501;state=working:id=build/test\x07",
            "\x1b]7501;?\x1b\\",
            "\x1b]7501;state=done:msg=Z\x07",
            &long_key,
            &long_app,
            &long_msg,
            &long_title,
            &control_msg,
            &non_utf8_msg,
            &control_title,
        ] {
            let reports = scan_status(input.as_bytes());
            assert!(
                reports.is_empty(),
                "{input:?} should be dropped: {reports:?}"
            );
        }
    }

    #[test]
    fn osc7501_is_incremental_across_every_cut_point() {
        let input = b"x\x1b]7501;state=working:app=build\x1b\\y\x1b]7501;state=clear\x07";
        let whole = signals(input);
        assert_eq!(
            whole,
            vec![
                Signal::Status(program(Some(AgentState::Working), Some("build"), None)),
                Signal::Status(program(None, None, None)),
            ]
        );
        for cut in 0..=input.len() {
            assert_eq!(
                signals_split(input, cut),
                whole,
                "mismatch cutting at {cut}"
            );
        }
    }

    fn user_var(name: &str, value: &str) -> Signal {
        Signal::UserVar {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn osc1337_sets_and_unsets_user_vars() {
        let set = |name: &str, value: &str| {
            format!("\x1b]1337;SetUserVar={name}={}\x07", BASE64.encode(value))
        };
        assert_eq!(
            signals(b"\x1b]1337;SetUserVar=TUTTI_KEYS=cGFuZQ==\x07"),
            vec![user_var("TUTTI_KEYS", "pane")]
        );
        // ST-terminated, unpadded, and an empty value — which unsets.
        assert_eq!(
            signals(
                b"\x1b]1337;SetUserVar=TUTTI_KEYS=cGFuZQ\x1b\\\x1b]1337;SetUserVar=TUTTI_KEYS=\x07"
            ),
            vec![user_var("TUTTI_KEYS", "pane"), user_var("TUTTI_KEYS", "")]
        );
        let name = "N".repeat(USER_VAR_NAME_MAX);
        let value = "é".repeat(USER_VAR_VALUE_MAX / 2);
        assert_eq!(
            signals(set(&name, &value).as_bytes()),
            vec![user_var(&name, &value)],
            "both limits are inclusive"
        );
        for input in [
            set("TUTTI-KEYS", "pane"),
            set("", "pane"),
            set(&"N".repeat(USER_VAR_NAME_MAX + 1), "pane"),
            set("TUTTI_EDITOR", &"v".repeat(USER_VAR_VALUE_MAX + 1)),
            format!("\x1b]1337;SetUserVar=K={}\x07", BASE64.encode([0xff, 0xfe])),
            "\x1b]1337;SetUserVar=K=cGF$ZQ==\x07".into(),
            "\x1b]1337;SetUserVar=K\x07".into(),
            "\x1b]1337;CurrentDir=/tmp\x07".into(),
            "\x1b]1337;SetMark\x07".into(),
        ] {
            assert!(signals(input.as_bytes()).is_empty(), "{input:?} was kept");
        }
    }

    #[test]
    fn a_prompt_start_is_a_signal_and_the_other_marks_are_not() {
        assert_eq!(signals(b"\x1b]133;A\x07"), vec![Signal::Prompt]);
        assert_eq!(signals(b"\x1b]133;A;k=i\x1b\\"), vec![Signal::Prompt]);
        for input in ["\x1b]133;B\x07", "\x1b]133;C\x07", "\x1b]133;D;0\x07"] {
            assert!(signals(input.as_bytes()).is_empty(), "{input:?} was kept");
        }
    }

    #[test]
    fn osc1337_is_incremental_across_every_cut_point() {
        let input = b"x\x1b]1337;SetUserVar=A=b2s=\x1b\\y\x1b]1337;SetUserVar=A=\x07";
        let whole = signals(input);
        assert_eq!(whole, vec![user_var("A", "ok"), user_var("A", "")]);
        for cut in 0..=input.len() {
            assert_eq!(
                signals_split(input, cut),
                whole,
                "mismatch cutting at {cut}"
            );
        }
    }

    #[test]
    fn oversized_osc7501_is_discarded() {
        let mut input = b"\x1b]7501;state=working:app=x:pad=".to_vec();
        input.resize(OSC_CAP + 10, b'a');
        input.push(0x07);
        assert!(scan_status(&input).is_empty());
    }

    fn bell() -> Notification {
        Notification {
            title: None,
            body: None,
        }
    }

    #[test]
    fn bare_bel_is_a_bell() {
        assert_eq!(scan(b"\x07"), vec![bell()]);
        // The terminal makes one bell of a read's bells with no other signal
        // between them.
        assert_eq!(scan(b"ab\x07cd\x07"), vec![bell()]);
        assert_eq!(scan_split(b"ab\x07cd\x07", 3), vec![bell(), bell()]);
    }

    #[test]
    fn osc9_with_bel_terminator() {
        assert_eq!(
            scan(b"\x1b]9;build done\x07"),
            vec![Notification {
                title: None,
                body: Some("build done".into()),
            }]
        );
    }

    #[test]
    fn osc9_with_st_terminator() {
        assert_eq!(
            scan(b"\x1b]9;hi\x1b\\"),
            vec![Notification {
                title: None,
                body: Some("hi".into()),
            }]
        );
    }

    #[test]
    fn osc777_carries_title_and_body() {
        assert_eq!(
            scan(b"\x1b]777;notify;Agent;ready to merge\x07"),
            vec![Notification {
                title: Some("Agent".into()),
                body: Some("ready to merge".into()),
            }]
        );
    }

    #[test]
    fn osc777_body_may_contain_semicolons() {
        assert_eq!(
            scan(b"\x1b]777;notify;T;a;b;c\x1b\\"),
            vec![Notification {
                title: Some("T".into()),
                body: Some("a;b;c".into()),
            }]
        );
    }

    #[test]
    fn bel_terminating_an_osc_is_not_also_a_bell() {
        // One notification, and no spurious bell from the terminating BEL.
        assert_eq!(scan(b"\x1b]9;x\x07").len(), 1);
    }

    #[test]
    fn non_notification_osc_is_ignored() {
        // OSC 0 (window title), and OSC 777 with a non-notify subcommand.
        assert!(scan(b"\x1b]0;my title\x07").is_empty());
        assert!(scan(b"\x1b]777;precmd\x07").is_empty());
    }

    #[test]
    fn oversized_payload_is_discarded_but_the_stream_recovers() {
        let mut input = b"\x1b]9;".to_vec();
        input.resize(input.len() + OSC_CAP + 100, b'x');
        input.push(0x07); // terminates the (overflowed, discarded) OSC
        input.push(0x07); // a following bell still registers
        assert_eq!(
            scan(&input),
            vec![bell()],
            "the huge OSC is dropped, the trailing bell survives"
        );
    }

    #[test]
    fn esc_bel_still_counts_as_a_bell() {
        // A bare `ESC BEL`, and an `ESC <byte> BEL`, both surface the bell that
        // the `Esc` state used to swallow.
        assert_eq!(scan(b"\x1b\x07"), vec![bell()]);
        assert_eq!(scan(b"\x1bx\x07"), vec![bell()]);
    }

    #[test]
    fn esc_bel_is_incremental_across_every_cut_point() {
        for input in [b"\x1b\x07".as_slice(), b"\x1bx\x07".as_slice()] {
            let whole = scan(input);
            assert_eq!(whole, vec![bell()], "whole scan of {input:?}");
            for cut in 0..=input.len() {
                assert_eq!(
                    scan_split(input, cut),
                    whole,
                    "mismatch cutting {input:?} at {cut}"
                );
            }
        }
    }

    #[test]
    fn scanning_is_incremental_across_every_cut_point() {
        let input = b"pre\x07\x1b]9;done\x07mid\x1b]777;notify;A;B\x1b\\post\x07";
        let whole = scan(input);
        assert_eq!(whole.len(), 4, "bell, osc9, osc777, bell");
        for cut in 0..=input.len() {
            assert_eq!(scan_split(input, cut), whole, "mismatch cutting at {cut}");
        }
    }

    /// A missing cwd must fail the spawn: portable-pty would otherwise fall
    /// back to $HOME and quietly run the child against the wrong tree.
    #[test]
    fn spawn_refuses_a_cwd_that_does_not_exist() {
        let mut spec = PtySpec::new("/bin/sh");
        spec.cwd = Some(PathBuf::from("/nonexistent/tutti-test-dir"));
        match PtyPane::spawn(spec, PaneSize::new(24, 80)) {
            Err(err) => assert!(
                err.to_string().contains("does not exist"),
                "unexpected error: {err}"
            ),
            Ok(_) => panic!("spawn accepted a missing cwd"),
        }
    }

    #[test]
    fn resize_refuses_a_zero_dimension() {
        let pane = PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 80)).unwrap();
        assert!(pane.resize(0, 80).is_err());
        assert!(pane.resize(24, 0).is_err());
        assert_eq!(pane.snapshot().rows, 24, "a refused resize leaves the grid");
        pane.kill().unwrap();
    }

    /// Killing a pane must terminate the child's whole process group. A
    /// backgrounded grandchild that ignores SIGHUP and holds the slave pty
    /// survives portable-pty's SIGHUP-to-the-direct-child kill (and, on Linux,
    /// keeps the master reader blocked so `Drop` hangs); only a process-group
    /// kill takes it down. We assert the group is fully reaped, then that
    /// dropping the pane does not hang.
    #[test]
    fn kill_reaps_the_whole_process_group() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Instant;

        let mut spec = PtySpec::new("/bin/sh");
        spec.args = vec![
            "-c".into(),
            "sh -c 'trap \"\" HUP; while :; do sleep 1; done' & exec sleep 30".into(),
        ];
        let pane = PtyPane::spawn(spec, PaneSize::new(24, 80)).unwrap();
        let pid = pane.child_pid().expect("unix reports a child pid");

        // Let the shell fork the backgrounded grandchild into the group.
        std::thread::sleep(Duration::from_millis(250));
        pane.kill().unwrap();

        // The child is its own group leader, so `kill(-pid, 0)` succeeds while
        // any group member (the grandchild) is still alive.
        let reaped = {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if libc_kill(-(pid as i32), 0) != 0 {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        assert!(
            reaped,
            "process group survived the kill: a descendant was left running"
        );

        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        std::thread::spawn(move || {
            drop(pane);
            flag.store(true, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "dropping a killed pane hung: the reader thread never saw EOF"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A `rows`×`cols` terminal fed `bytes` in reads of `chunk` bytes, as the
    /// reader thread would.
    fn fed(rows: u16, cols: u16, bytes: &[u8], chunk: usize) -> Term {
        let mut term = Term::new(PaneSize::new(rows, cols), None);
        for read in bytes.chunks(chunk) {
            term.process(read);
        }
        term
    }

    /// `n` lines `L<first>`, `L<first+1>`, …, each its own row.
    fn numbered(first: usize, n: usize) -> String {
        (first..first + n).map(|i| format!("L{i}\r\n")).collect()
    }

    /// The rows `numbered` printed for `range`, as `tail` returns them.
    fn rows_of(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| format!("L{i}")).collect()
    }

    /// The newest row the ring holds. The count only picks where `tail`
    /// starts, not which row is newest, so `L<k>` here checks a count of k+1.
    fn newest_committed(term: &mut Term) -> String {
        let since = term.vt.screen_top() - 1;
        term.tail(since, None, false).unwrap().lines.remove(0)
    }

    #[test]
    fn scroll_count_matches_the_rows_plain_output_commits() {
        // Five rows: four lines fill the screen, each later one pushes one.
        let mut term = fed(5, 20, numbered(0, 40).as_bytes(), READ);
        assert_eq!(term.vt.screen_top(), 36);
        assert_eq!(newest_committed(&mut term), "L35");
    }

    #[test]
    fn scroll_count_holds_across_a_read_boundary_anywhere() {
        let stream = [
            numbered(0, 12),
            "ABCDEFGHIJKLMNOPQRSTUVWXY\r\n".into(),
            format!("\x1b[?1049h{}\x1b[?1049l", numbered(100, 9)),
            numbered(12, 6),
            "\x1b[2S".into(),
        ]
        .concat();
        // 12 lines push 8; the 25-char line wraps twice on 10 columns and
        // ends, 3; the alternate screen pushes none; 6 lines, 6; the
        // scroll-up, 2.
        assert_eq!(fed(5, 10, stream.as_bytes(), READ).vt.screen_top(), 19);
        for cut in 0..=stream.len() {
            let mut term = Term::new(PaneSize::new(5, 10), None);
            term.process(&stream.as_bytes()[..cut]);
            term.process(&stream.as_bytes()[cut..]);
            assert_eq!(term.vt.screen_top(), 19, "reads split at byte {cut}");
        }
        for chunk in [1, 2, 3, 5, 64] {
            assert_eq!(
                fed(5, 10, stream.as_bytes(), chunk).vt.screen_top(),
                19,
                "{chunk}-byte reads"
            );
        }
    }

    /// Rows pushed just before entering the alternate screen and just after
    /// leaving it, in the same read, count.
    #[test]
    fn scroll_count_skips_the_alternate_screen_within_one_read() {
        let stream = format!(
            "{}\x1b[?1049h{}\x1b[?1049l{}",
            numbered(0, 10),
            numbered(100, 20),
            numbered(10, 7)
        );
        let mut term = fed(5, 20, stream.as_bytes(), READ);
        assert_eq!(term.vt.screen_top(), 13);
        assert_eq!(newest_committed(&mut term), "L12");
    }

    #[test]
    fn scroll_count_skips_the_alternate_screen_across_reads() {
        let mut term = Term::new(PaneSize::new(5, 20), None);
        for read in [
            format!("{}\x1b[?1049h", numbered(0, 10)),
            numbered(100, 20),
            format!("\x1b[?1049l{}", numbered(10, 7)),
        ] {
            term.process(read.as_bytes());
        }
        assert_eq!(term.vt.screen_top(), 13);
        assert_eq!(newest_committed(&mut term), "L12");
    }

    #[test]
    fn scroll_count_stays_exact_once_the_ring_is_full() {
        // 25k lines overflow the 10k ring long before the end; after every
        // 8 KiB read the count still names the newest row.
        let mut term = Term::new(PaneSize::new(24, 80), None);
        for read in numbered(0, 25_000).as_bytes().chunks(READ) {
            term.process(read);
            let top = term.vt.screen_top();
            if top > 0 {
                assert_eq!(newest_committed(&mut term), format!("L{}", top - 1));
            }
        }
        assert_eq!(term.vt.screen_top(), 25_000 - 23);
    }

    #[test]
    fn scroll_count_continues_across_a_resize() {
        let mut term = fed(24, 80, numbered(0, 100).as_bytes(), READ);
        assert_eq!(term.vt.screen_top(), 77);
        // A shrink keeps the cursor in view by pushing the rows above it
        // (L77..L90), and every line after pushes one.
        term.vt.resize(10, 40);
        term.process(numbered(100, 50).as_bytes());
        assert_eq!(term.vt.screen_top(), 141);
        assert_eq!(newest_committed(&mut term), "L140");
        // A growth pulls rows back onto the screen, the cursor going down
        // with them, and the count stays, numbering the rows it kept afresh:
        // a cursor from before sees rows again, and never skips one.
        term.vt.resize(30, 40);
        assert_eq!(term.vt.screen_top(), 141);
        term.process(numbered(150, 20).as_bytes());
        assert_eq!(term.vt.screen_top(), 161);
        term.process(numbered(170, 5).as_bytes());
        assert_eq!(term.vt.screen_top(), 166);
        assert_eq!(
            term.tail(141, None, false).unwrap().lines,
            rows_of(121..175)
        );
    }

    #[test]
    fn scroll_count_handles_scroll_up_regions_and_resets() {
        let mut term = fed(5, 20, numbered(0, 10).as_bytes(), READ);
        assert_eq!(term.vt.screen_top(), 6);
        // A scroll-up pushes up to a screen height per sequence: 3000 in one
        // read push 15k rows.
        term.process("\x1b[100S".repeat(3000).as_bytes());
        assert_eq!(term.vt.screen_top(), 15_006);
        // Inside a scroll region nothing reaches the scrollback.
        term.process(format!("\x1b[2;4r\x1b[4;1H{}", numbered(0, 20)).as_bytes());
        assert_eq!(term.vt.screen_top(), 15_006);
        // A full reset empties the ring; the count carries on past it.
        term.process(format!("\x1b[r\x1bc{}", numbered(0, 10)).as_bytes());
        assert_eq!(term.vt.screen_top(), 15_012);
        assert_eq!(newest_committed(&mut term), "L5");
        assert!(term.tail(0, None, false).unwrap().truncated);
    }

    #[test]
    fn tail_reads_the_rows_committed_since_the_cursor_then_the_screen() {
        // 26 rows committed (L0..L25); the screen holds L26..L29 over a blank
        // cursor row, which is trimmed.
        let mut term = fed(5, 20, numbered(0, 30).as_bytes(), READ);
        let all = term.tail(0, None, false).unwrap();
        assert_eq!(all.lines, rows_of(0..30));
        assert_eq!((all.cursor, all.truncated), (26, false));
        assert_eq!(term.tail(20, None, false).unwrap().lines, rows_of(20..30));
        // Caught up, a read is the screen alone; `lines` keeps the newest.
        assert_eq!(term.tail(26, None, false).unwrap().lines, rows_of(26..30));
        assert_eq!(term.tail(0, Some(3), false).unwrap().lines, rows_of(27..30));
        // More output: a reread from the old cursor starts where it left off.
        term.process(numbered(30, 3).as_bytes());
        let next = term.tail(26, None, false).unwrap();
        assert_eq!(next.lines, rows_of(26..33));
        assert_eq!(next.cursor, 29);
    }

    #[test]
    fn tail_reports_rows_that_left_the_ring() {
        // 10_077 rows committed; the ring keeps the newest 10k, from L77.
        let term = fed(24, 80, numbered(0, 10_100).as_bytes(), READ);
        let tail = term.tail(50, None, false).unwrap();
        assert!(tail.truncated);
        assert_eq!(tail.lines[0], "L77");
        assert_eq!(tail.cursor, 10_077);
        assert!(!term.tail(77, None, false).unwrap().truncated);
    }

    #[test]
    fn tail_reads_the_scrollback_under_the_alternate_screen() {
        let mut term = fed(5, 20, numbered(0, 10).as_bytes(), READ);
        term.process(b"\x1b[?1049hfull screen");
        // The committed rows, then the alternate screen, its cursor on the
        // row it had; the primary's own screen waits under it, and is read
        // once it comes back.
        let up = term.tail(2, None, false).unwrap();
        let mut want = rows_of(2..6);
        want.extend(["", "", "", "", "full screen"].map(String::from));
        assert_eq!(up.lines, want);
        assert_eq!((up.cursor, up.truncated), (6, false));
        term.process(b"\x1b[?1049l");
        let down = term.tail(up.cursor, None, false).unwrap();
        assert_eq!(down.lines, rows_of(6..10));
        assert_eq!(down.cursor, 6);
    }

    #[test]
    fn tail_refuses_a_cursor_past_the_count() {
        let term = fed(5, 20, numbered(0, 10).as_bytes(), READ);
        let err = term.tail(7, None, false).unwrap_err();
        assert!(err.to_string().contains("past this pane's 6"), "{err}");
    }

    #[test]
    fn osc7_reports_the_cwd_decoded_and_skips_a_bad_report() {
        let mut term = fed(5, 20, b"\x1b]7;file://host/Users/me/a%20dir\x07", READ);
        assert_eq!(term.vt.cwd(), Some("/Users/me/a dir"));
        // ST-terminated, an empty host, raw and escaped UTF-8.
        term.process("\x1b]7;file:///tmp/caf%C3%A9/é\x1b\\".as_bytes());
        assert_eq!(term.vt.cwd(), Some("/tmp/café/é"));
        // Not a file URL, no path, a broken escape, an escape that is not
        // UTF-8: each skipped, the last good report kept.
        for bad in [
            "http://host/x",
            "/bare/path",
            "file://host",
            "file://host/a%2",
            "file://host/a%zz",
            "file://host/a%+f",
            "file://host/a%ff",
        ] {
            term.process(format!("\x1b]7;{bad}\x07").as_bytes());
            assert_eq!(term.vt.cwd(), Some("/tmp/café/é"), "{bad}");
        }
        assert_eq!(term.vt.screen_top(), 0, "an OSC draws nothing");
    }

    /// A shell's turn as `tutti shell-init` marks it: the prompt, `cmd`
    /// typed, then `output` between `C` and `D;<exit>`.
    fn turn(cmd: &str, output: &str, exit: i32) -> String {
        format!(
            "\x1b]133;A\x07$ \x1b]133;B\x07{cmd}\r\n\x1b]133;C\x07{output}\x1b]133;D;{exit}\x07"
        )
    }

    /// `last_output` as (lines, exit, truncated), `None` with no command.
    fn last(term: &mut Term, unwrapped: bool) -> Option<(Vec<String>, Option<i32>, bool)> {
        let out = term.last_output(None, unwrapped).unwrap()?;
        Some((out.lines, out.exit, out.truncated))
    }

    /// Output that scrolls past the top of a five-row screen, wraps, and has
    /// a full-screen program in the middle comes back exactly, however the
    /// stream is cut into reads — a mark split across two included.
    #[test]
    fn last_output_reads_between_the_marks_across_a_read_boundary_anywhere() {
        let output = format!(
            "{}\x1b[?1049hfull screen\x1b[?1049lABCDEFGHIJKLMNO\r\n{}",
            numbered(0, 4),
            numbered(4, 4)
        );
        let stream = format!("{}\x1b]133;A\x07$ ", turn("run", &output, 3));
        let mut want = rows_of(0..4);
        want.extend(["ABCDEFGHIJ".into(), "KLMNO".into()]);
        want.extend(rows_of(4..8));
        let mut joined = rows_of(0..4);
        joined.push("ABCDEFGHIJKLMNO".into());
        joined.extend(rows_of(4..8));

        let mut term = fed(5, 10, stream.as_bytes(), READ);
        assert_eq!(last(&mut term, false), Some((want.clone(), Some(3), false)));
        assert_eq!(last(&mut term, true), Some((joined, Some(3), false)));
        for cut in 0..=stream.len() {
            let mut term = Term::new(PaneSize::new(5, 10), None);
            term.process(&stream.as_bytes()[..cut]);
            term.process(&stream.as_bytes()[cut..]);
            assert_eq!(
                last(&mut term, false),
                Some((want.clone(), Some(3), false)),
                "reads split at byte {cut}"
            );
        }
        for chunk in [1, 2, 3, 5, 64] {
            let mut term = fed(5, 10, stream.as_bytes(), chunk);
            assert_eq!(
                last(&mut term, false),
                Some((want.clone(), Some(3), false)),
                "{chunk}-byte reads"
            );
        }
    }

    #[test]
    fn last_output_is_the_previous_command_while_one_runs() {
        let mut term = fed(10, 20, b"$ ", READ);
        assert_eq!(last(&mut term, false), None, "nothing finished yet");
        term.process(turn("echo one", "one\r\n", 0).as_bytes());
        let one = Some((vec!["one".to_string()], Some(0), false));
        assert_eq!(last(&mut term, false), one);
        // An empty command line: a prompt's `D` with no `C` before it.
        term.process(b"\x1b]133;A\x07$ \x1b]133;D;1\x07\x1b]133;A\x07$ ");
        assert_eq!(last(&mut term, false), one);
        // A command still running leaves the one before it.
        term.process(b"\x1b]133;A\x07$ \x1b]133;B\x07sleep\r\n\x1b]133;C\x07partial\r\n");
        assert_eq!(last(&mut term, false), one);
        term.process(b"\x1b]133;D;130\x07");
        assert_eq!(
            last(&mut term, false),
            Some((vec!["partial".to_string()], Some(130), false))
        );
        // A `D` naming no exit code finishes with none.
        term.process(turn("true", "", 0).replace("D;0", "D").as_bytes());
        assert_eq!(last(&mut term, false), Some((Vec::new(), None, false)));
    }

    /// Output ending mid-row shares that row with what the shell draws next
    /// — bash's prompt, zsh's `%` — which is not output.
    #[test]
    fn last_output_stops_at_the_cell_its_d_fell_on() {
        let mut term = fed(5, 20, turn("printf", "a\r\nb", 0).as_bytes(), READ);
        term.process(b"%   \r\n\x1b]133;A\x07$ ");
        assert_eq!(
            last(&mut term, false),
            Some((vec!["a".to_string(), "b".to_string()], Some(0), false))
        );
        term.process(turn("printf", "c", 1).as_bytes());
        term.process(b"\x1b]133;A\x07$ ");
        assert_eq!(
            last(&mut term, false),
            Some((vec!["c".to_string()], Some(1), false)),
            "the prompt on the same row is cut off"
        );
        // A row filled to its last column keeps every cell.
        term.process(turn("printf", "0123456789ABCDEFGHIJ", 0).as_bytes());
        assert_eq!(
            last(&mut term, false),
            Some((vec!["0123456789ABCDEFGHIJ".to_string()], Some(0), false))
        );
    }

    #[test]
    fn last_output_reports_rows_that_left_the_ring() {
        let stream = turn("seq", &numbered(0, 10_100), 0);
        let mut term = fed(24, 80, stream.as_bytes(), READ);
        let (lines, exit, truncated) = last(&mut term, false).unwrap();
        assert!(truncated);
        assert_eq!(exit, Some(0));
        assert_eq!(lines.last().map(String::as_str), Some("L10099"));
        assert_eq!(
            lines.len(),
            10_000 + 23,
            "the ring and the screen above the cursor"
        );
    }

    /// The alternate grid has no rows numbered as the primary's: a mark
    /// there is ignored, and while it is up the last output cannot be read.
    #[test]
    fn last_output_stays_on_the_primary_screen() {
        let mut term = fed(5, 20, turn("ls", "x\r\n", 0).as_bytes(), READ);
        term.process(b"\x1b]133;A\x07$ \x1b]133;B\x07tmux\r\n\x1b]133;C\x07\x1b[?1049h");
        term.process(turn("inner", "y\r\n", 7).as_bytes());
        let err = term.last_output(None, false).unwrap_err();
        assert!(err.to_string().contains("alternate screen"), "{err}");
        term.process(b"\x1b[?1049l\x1b]133;D;0\x07");
        assert_eq!(last(&mut term, false), Some((Vec::new(), Some(0), false)));
    }

    /// An `ESC` that does not start a String Terminator ends the OSC string
    /// as vte ends one, which reads it; every byte after it is parsed as
    /// usual.
    #[test]
    fn an_osc_an_escape_cuts_short_is_read_as_vte_reads_it() {
        let term = fed(
            5,
            20,
            b"\x1b]7;file:///gone\x1b[1mbold\x1b]133;C\x1bc",
            READ,
        );
        assert_eq!(term.vt.cwd(), Some("/gone"));
        assert_eq!(term.vt.contents(), "", "the reset still ran");
    }

    #[test]
    fn unwrapped_joins_exactly_the_soft_wrapped_rows() {
        // Four rows of ten columns. A line of exactly ten fills its row but
        // never wraps — the old full-width guess merged it with the next. The
        // 25-char line wraps onto three rows, and the lines after push all of
        // it into the ring.
        let mut term = fed(
            4,
            10,
            format!(
                "0123456789\r\nnext\r\nABCDEFGHIJKLMNOPQRSTUVWXY\r\n{}",
                numbered(0, 8)
            )
            .as_bytes(),
            READ,
        );
        let mut want = vec!["0123456789", "next", "ABCDEFGHIJKLMNOPQRSTUVWXY"];
        let numbers = rows_of(0..8);
        want.extend(numbers.iter().map(String::as_str));
        assert_eq!(term.tail(0, None, true).unwrap().lines, want);
        assert_eq!(
            term.tail(0, None, false).unwrap().lines[2..5],
            ["ABCDEFGHIJ", "KLMNOPQRST", "UVWXY"]
        );

        // A 45-char line whose first row is committed and the rest on screen
        // joins across that boundary too.
        let long = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRS";
        term.process(long.as_bytes());
        let since = term.vt.screen_top() - 1;
        assert_eq!(term.tail(since, None, true).unwrap().lines, [long]);
        // Folding is per read: from a cursor inside the line, only its rest.
        let since = term.vt.screen_top();
        assert_eq!(term.tail(since, None, true).unwrap().lines, [&long[10..]]);
    }

    /// The plan's `pane read` target — a full 10k-row scrollback in under 50
    /// ms — held here in a debug build, best of three to ride out load.
    #[test]
    fn reading_a_full_ring_takes_under_50ms() {
        let dense: String = (0..12_000)
            .map(|i| {
                format!(
                    "\x1b[3{}m{i:>6}\x1b[0m \x1b[1mINFO\x1b[0m compiling crate {i} of the workspace, step {}\r\n",
                    i % 8,
                    i * 7
                )
            })
            .collect();
        let term = fed(24, 80, dense.as_bytes(), READ);
        let best = (0..3)
            .map(|_| {
                let start = std::time::Instant::now();
                let tail = term.tail(0, None, false).unwrap();
                assert_eq!(tail.lines.len(), 10_000 + 23);
                start.elapsed()
            })
            .min()
            .unwrap();
        assert!(
            best < Duration::from_millis(50),
            "a full read took {best:?}"
        );
    }

    /// Screens stressing what a snapshot must carry: colours and attributes,
    /// wide and combining characters, a soft-wrapped row, a moved and a
    /// hidden cursor, input modes, the alternate screen (with a colour left
    /// set).
    const SCREENS: &[&str] = &[
        "\x1b[1;3;4;7mbold italic under inverse\x1b[0m \x1b[31mred\x1b[38;5;200m idx\x1b[48;2;10;20;30m rgb\x1b[0m",
        "日本語 wide \u{1f600} emoji, e\u{301} combining",
        "a soft-wrapped line that runs well past the eighty columns of this screen, onto the next row",
        "\x1b[5;10Hmoved\x1b[12;40H",
        "\x1b[?25lhidden cursor",
        "\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b=modes",
        "primary\r\n\x1b[?1049h\x1b[2;3Halternate\x1b[32m green",
    ];

    /// A client terminal, as the TUI and `pane attach` hold one, fed `bytes`.
    fn client(rows: u16, cols: u16, bytes: &[u8]) -> Terminal {
        let mut client = Terminal::new(rows, cols, 0);
        client.advance(bytes);
        client
    }

    /// What a terminal shows: each row's cells — their text and rendition —
    /// and soft wrap; the cursor; the modes, which screen is up and whether
    /// the cursor shows among them.
    type Shown = (
        Vec<(
            Vec<(String, rtty_vt::Color, rtty_vt::Color, rtty_vt::Attrs)>,
            bool,
        )>,
        (u16, u16),
        rtty_vt::Modes,
    );

    fn shown(term: &Terminal) -> Shown {
        let (rows, cols) = term.size();
        let row = |r: u16| {
            let row = term.screen_row(r);
            let cells = (0..usize::from(cols))
                .map(|c| {
                    let mut text = String::new();
                    row.push_grapheme(c, &mut text);
                    let cell = row.cells()[c];
                    (text, cell.fg(), cell.bg(), cell.attrs())
                })
                .collect();
            (cells, row.wrapped())
        };
        (
            (0..rows).map(row).collect(),
            term.cursor_position(),
            term.modes().clone(),
        )
    }

    /// `seen` — a client, as the TUI and `pane attach` run one — shows what
    /// `want`, a terminal fed the pane's own bytes, does.
    fn level(seen: &Terminal, want: &Terminal, step: &str) {
        assert_eq!(shown(seen), shown(want), "{step}");
    }

    /// The render tick's contract: a client seeded with one look's snapshot
    /// and fed only each later look's delta stays level with the screen,
    /// step after step, switches of screen and all.
    #[test]
    fn looks_keep_a_client_level_with_the_live_screen() {
        let size = PaneSize::new(24, 80);
        let mut live = Term::new(size, None);
        let first = numbered(0, 30);
        live.process(first.as_bytes());
        let mut reference = client(size.rows, size.cols, first.as_bytes());
        let look = live.look(None, true);
        let mut client = client(size.rows, size.cols, &look.snapshot.unwrap());
        let mut last = (look.size, look.alt_screen);
        let back = "\x1b[?1049l\x1b[?25h\x1b[?1002l\x1b[0mback on the primary\r\n";
        for screen in SCREENS.iter().chain([&back]) {
            live.process(screen.as_bytes());
            reference.advance(screen.as_bytes());
            let look = live.look(Some(last), false);
            client.advance(&look.delta);
            last = (look.size, look.alt_screen);
            level(&client, &reference, &format!("after {screen:?}"));
        }
    }

    /// A shrink to one column leaves no wide character there — rtty-vt
    /// writes one that cannot fit as the replacement character — and a
    /// client replays the snapshot to the same screen.
    #[test]
    fn a_snapshot_after_a_shrink_to_one_column_replays() {
        let mut live = Term::new(PaneSize::new(2, 4), None);
        live.process("日本".as_bytes());
        live.vt.resize(2, 1);
        assert!(live.vt.contents().contains('\u{FFFD}'));
        let replayed = client(2, 1, &live.vt.snapshot());
        assert_eq!(replayed.contents(), live.vt.contents());
    }

    /// Solarized Light, its cursor too.
    fn light() -> HostColors {
        let rgb = |r, g, b| Rgb { r, g, b };
        HostColors {
            fg: rgb(0x6565, 0x7b7b, 0x8383),
            bg: rgb(0xfdfd, 0xf6f6, 0xe3e3),
            cursor: Some(rgb(0x5858, 0x6e6e, 0x7575)),
            dark: false,
        }
    }

    /// Feed `reads` to a fresh 3x20 `Term` holding `colors`, one at a time:
    /// what the taps got and the answers, joined; and the scroll count.
    fn answered(colors: Option<HostColors>, reads: &[&[u8]]) -> (Vec<u8>, Vec<u8>, u64) {
        let mut term = Term::new(PaneSize::new(3, 20), colors);
        let (mut raw, mut answers) = (Vec::new(), Vec::new());
        for read in reads {
            let out = term.process(read);
            raw.extend(out.raw);
            answers.extend(out.answers);
        }
        (raw, answers, term.vt.screen_top())
    }

    /// Every query the terminal answers and every sequence the daemon owns,
    /// wherever reads cut the stream: each answered once and in order — a
    /// cursor report where the cursor stood, a row scrolled and all; mode
    /// 2031 and modifyOtherKeys tracked — and kept from what the taps get,
    /// which is everything else: what goes unanswered, and the `ESC` before
    /// another.
    #[test]
    fn owned_queries_are_answered_and_kept_from_the_taps_across_any_cut() {
        let stream: &[u8] = b"ab\x1b[c\x1b[0cX\x1b[>c\x1b[>0c\r\nL1\r\nL2\r\nL3\x1b[5n\x1b[6n\
            \x1b[?996n\x1b[?2031$p\x1b[?2031h\x1b[?2031$p\x1b[?2031l\
            \x1b[>4;2m\x1b[>4;1m\x1b[>4;0m\x1b[>4m\
            \x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07\
            \x1b[?6n\x1b[1c\x1b[00c\x1b[=c\x1b[6;1n\x1b[?2031;1h\x1b[\x076n\x1b[>4;2;1m\x1b[?4m\
            \x1b]11;?x\x07\x1b]110\x07\x1b]11;rgb:0/0/0\x07\x1b]0;t\x1b\\\
            \x1b]11;?\x1b[c\x1b]1\x1b[5nend\x1b\x1b[6n";
        let raw: &[u8] = b"abX\r\nL1\r\nL2\r\nL3\x1b[1c\x1b[=c\x1b[?2031;1h\
            \x1b]11;?x\x07\x1b]110\x07\x1b]11;rgb:0/0/0\x07\x1b]0;t\x1b\\\
            \x1b]1end\x1b";
        let (da1, da2) = (
            "\x1b[?62;22c",
            format!("\x1b[>1;{};0c", rtty_vt::XTERM_VERSION),
        );
        let answers = [
            da1,
            da1,
            &da2,
            &da2,
            "\x1b[0n\x1b[3;3R\x1b[?997;2n\x1b[?2031;2$y\x1b[?2031;1$y",
            "\x1b]10;rgb:6565/7b7b/8383\x07\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\",
            "\x1b]12;rgb:5858/6e6e/7575\x07\x1b[?3;3;1R",
            da1,
            "\x1b[3;3R\x1b[3;3R\x1b[>4;2m\x1b]11;rgb:0000/0000/0000\x1b\\",
            da1,
            "\x1b[0n\x1b[3;6R",
        ]
        .concat();
        let want = (raw.to_vec(), answers.into_bytes(), 1);
        for cut in 0..=stream.len() {
            let got = answered(Some(light()), &[&stream[..cut], &stream[cut..]]);
            assert_eq!(got, want, "cut at {cut}");
        }
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(answered(Some(light()), &bytes), want, "a byte a read");
    }

    /// Kitty's keyboard protocol and xterm's modifyOtherKeys reach the
    /// terminal — which answers their queries, and whose snapshots and
    /// redraws hand the TUI the flags to encode keys by — but never the
    /// taps, however a read cuts them: a `pane attach` terminal keeps plain
    /// keys.
    #[test]
    fn the_keyboard_protocols_reach_the_terminal_but_not_the_taps() {
        let stream: &[u8] =
            b"\x1b[?u\x1b[>3u\x1b[=1;1u\x1b[>5u\x1b[<u\x1b[?u\x1b[>4;2m\x1b[?4m\x1b[c";
        let answers = b"\x1b[?0u\x1b[?1u\x1b[>4;2m\x1b[?62;22c".to_vec();
        for cut in 0..=stream.len() {
            let mut term = Term::new(PaneSize::new(3, 20), None);
            let (first, second) = (term.process(&stream[..cut]), term.process(&stream[cut..]));
            assert_eq!([first.raw, second.raw].concat(), b"", "cut at {cut}");
            assert_eq!([first.answers, second.answers].concat(), answers);
            assert_eq!(term.vt.kitty_keyboard(), 1);
            assert_eq!(term.vt.modes().modify_other_keys, 2);
            let snapshot = String::from_utf8_lossy(&term.vt.snapshot()).into_owned();
            assert!(
                snapshot.contains("\x1b[>1u") && snapshot.contains("\x1b[>4;2m"),
                "{snapshot:?}"
            );
        }
    }

    /// nvim killed with SIGKILL, as its shell's integration marks it: the
    /// mouse, focus and light/dark reports and the key encodings it left on
    /// go off at the command's end (OSC 133;D), however reads cut the
    /// stream, so the shell neither hears a light/dark flip (a 997 report
    /// zsh's line editor takes for typing) nor gets its keys in kitty's
    /// encoding. Bracketed paste and the alternate screen stay. The taps
    /// are told of the mouse and focus modes, after the mark.
    #[test]
    fn a_commands_end_turns_off_what_it_left_on() {
        let left: &[u8] = b"\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2031h\x1b[?2004h\
            \x1b[>4;2m\x1b[>1u";
        let stream = [
            b"\x1b]133;C\x07",
            left,
            b"\x1b]133;D;137\x1b\\\x1b]133;A\x07$ ",
        ]
        .concat();
        let raw = [
            b"\x1b]133;C\x07\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h\
                \x1b]133;D;137\x1b\\",
            LEFT_ON_TAPS,
            b"\x1b]133;A\x07$ ",
        ]
        .concat();
        for cut in 0..=stream.len() {
            let mut term = Term::new(PaneSize::new(3, 20), None);
            let (first, second) = (term.process(&stream[..cut]), term.process(&stream[cut..]));
            assert_eq!([first.raw, second.raw].concat(), raw, "cut at {cut}");
            let modes = term.vt.modes();
            assert_eq!(
                (modes.mouse, modes.mouse_encoding, modes.focus_reports),
                (MouseMode::None, MouseEncoding::Default, false),
                "cut at {cut}"
            );
            assert!(!modes.color_scheme_reports);
            assert_eq!((modes.modify_other_keys, term.vt.kitty_keyboard()), (0, 0));
            assert!(modes.bracketed_paste && modes.alt_screen);
            term.set_colors(light());
            let mut out = Processed::default();
            term.drain(&mut out);
            assert_eq!(out.answers, b"", "no light/dark report reaches the shell");
        }
    }

    /// What a shell sets after a command's end stays: fish turns on 2031 and
    /// modifyOtherKeys at each prompt, in the read that ends the command.
    /// Only the first of a run of ends turns anything off — zsh's
    /// integration marks one from PROMPT_SP and another from its precmd, and
    /// what a hook between them turns on is the shell's — and an end with
    /// nothing left on tells the taps nothing.
    #[test]
    fn only_a_commands_first_end_turns_anything_off() {
        let mut term = Term::new(PaneSize::new(3, 20), None);
        term.process(b"\x1b]133;C\x07\x1b[?1000h");
        term.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b[?2031h\x1b[>4;1m> ");
        let modes = term.vt.modes();
        assert_eq!(modes.mouse, MouseMode::None);
        assert!(modes.color_scheme_reports && modes.modify_other_keys == 1);

        let mut term = Term::new(PaneSize::new(3, 20), None);
        let quiet = b"\x1b]133;D;0\x07\x1b[?1004h\x1b]133;D;0\x07$ ";
        assert_eq!(term.process(quiet).raw, quiet, "nothing was on");
        assert!(term.vt.modes().focus_reports, "a hook's, between two ends");
        term.process(b"\x1b]133;A\x07$ \x1b]133;C\x07\x1b]133;D;0\x07");
        assert!(!term.vt.modes().focus_reports, "a command ran since");
    }

    /// A synchronized update holds the pane's frame until its program ends
    /// it, or `SYNC_HOLD` has gone by.
    #[test]
    fn a_synchronized_update_holds_the_frame_until_it_ends() {
        let mut term = Term::new(PaneSize::new(3, 20), None);
        assert!(!term.holding());
        term.process(b"\x1b[?2026hhalf");
        assert!(term.holding());
        term.process(b" done\x1b[?2026l");
        assert!(!term.holding());
        term.process(b"\x1b[?2026h");
        term.synced = term.synced.map(|at| at - SYNC_HOLD);
        assert!(!term.holding(), "a program that never ends one still draws");
    }

    /// A switch of screens and a reset each reach the taps alone, however
    /// reads cut the stream — `pane attach` redraws a read that switches, and
    /// would lose the bell, the cursor shape or the text beside one.
    #[test]
    fn a_switch_of_screens_reaches_the_taps_alone() {
        let stream: &[u8] = b"before\x07\x1b[?1049h\x1b[?2026$p\x07\x1b[6 qafter\
            \x1b[?1049;1006l\x1b[?47hon\x1b[?47l\x1bcreset";
        let switches: [&[u8]; 5] = [
            b"\x1b[?1049h",
            b"\x1b[?1049;1006l",
            b"\x1b[?47h",
            b"\x1b[?47l",
            b"\x1bc",
        ];
        let raw = [
            b"before\x07".as_slice(),
            switches[0],
            b"\x07\x1b[6 qafter",
            switches[1],
            switches[2],
            b"on",
            switches[3],
            switches[4],
            b"reset",
        ]
        .concat();
        for cut in 0..=stream.len() {
            let mut term = Term::new(PaneSize::new(5, 20), None);
            let mut pieces: Vec<Vec<u8>> = Vec::new();
            for read in [&stream[..cut], &stream[cut..]] {
                let out = term.process(read);
                let mut from = 0;
                for to in out.cuts.iter().copied().chain([out.raw.len()]) {
                    pieces.push(out.raw[from..to].to_vec());
                    from = to;
                }
            }
            assert_eq!(pieces.concat(), raw, "cut at {cut}");
            for piece in &pieces {
                for switch in switches {
                    let holds = piece.windows(switch.len()).any(|bytes| bytes == switch);
                    assert!(
                        !holds || piece == switch,
                        "cut at {cut}: {piece:?} switches with more"
                    );
                }
            }
        }
    }

    /// With no colours reported the daemon invents none: the colour queries
    /// and the light/dark one go on to the taps unanswered. Without a cursor
    /// colour, so does OSC 12 alone.
    #[test]
    fn colour_queries_without_a_report_go_on_to_the_taps() {
        let queries: &[u8] = b"\x1b[?996n\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07";
        let stream = [queries, b"\x1b[c"].concat();
        let (raw, answers, _) = answered(None, &[&stream]);
        assert_eq!(
            (raw.as_slice(), answers.as_slice()),
            (queries, &b"\x1b[?62;22c"[..])
        );
        let blind = HostColors {
            cursor: None,
            ..light()
        };
        let (raw, answers, _) = answered(Some(blind), &[b"\x1b]12;?\x07\x1b]10;?\x07"]);
        assert_eq!(raw, b"\x1b]12;?\x07");
        assert_eq!(answers, b"\x1b]10;rgb:6565/7b7b/8383\x07");
    }

    /// A cursor report reads the screen the query lands on: a wrap pending
    /// past the last column reports that column, and the alternate screen
    /// its own cursor.
    #[test]
    fn a_cursor_report_reads_the_cursor_where_the_query_found_it() {
        let mut term = Term::new(PaneSize::new(3, 10), None);
        let mut answer = |bytes: &[u8]| term.process(bytes).answers;
        assert_eq!(answer(b"0123456789\x1b[6n"), b"\x1b[1;10R");
        assert_eq!(answer(b"\x1b[?1049h\x1b[2;4H\x1b[6n"), b"\x1b[2;4R");
        assert_eq!(answer(b"\x1b[?1049l\x1b[6n"), b"\x1b[1;10R");
    }

    /// A program that asks and never reads the answers never stalls its own
    /// output behind them. Its input raw, a full input blocks the pty's
    /// writer — the answer thread, which then falls behind until the rest go
    /// unwritten — and the reader reads on.
    #[test]
    fn unread_answers_never_stall_the_output() {
        let flood = "stty -icanon -echo; i=0; while [ $i -lt 5000 ]; do printf '\\033[6n'; \
                     i=$((i+1)); done; echo flood-done; sleep 5";
        let spec = PtySpec {
            args: vec!["-c".into(), flood.into()],
            ..PtySpec::new("/bin/sh")
        };
        let pane = PtyPane::spawn(spec, PaneSize::new(24, 80)).unwrap();
        wait_for(
            || pane.contents().contains("flood-done"),
            "the output stalled behind its answers",
        );
        pane.kill().unwrap();
    }

    /// A terminal seeded with a tap's snapshot shows what the pane shows —
    /// and under the alternate screen it holds the pane's primary, so the
    /// program's own switch back lands both on the same screen.
    #[test]
    fn a_snapshot_seeds_a_terminal_with_both_screens() {
        for screen in SCREENS {
            let stream = format!("{}{screen}", numbered(0, 30));
            let live = fed(24, 80, stream.as_bytes(), READ);
            let mut terminal = client(24, 80, &live.vt.snapshot());
            let mut reference = client(24, 80, stream.as_bytes());
            for next in ["", "\x1b[?1049l after"] {
                terminal.advance(next.as_bytes());
                reference.advance(next.as_bytes());
                level(&terminal, &reference, &format!("{screen:?} then {next:?}"));
            }
        }
    }

    /// A tap joined mid-stream is handed a snapshot, then every read after
    /// it exactly once: the snapshot's numbers run on into the stream's with
    /// none lost or repeated. A resize hands it a fresh snapshot at the new
    /// size; untapped, it is dropped.
    #[test]
    fn a_tap_sees_a_snapshot_then_every_read_once() {
        let spec = PtySpec {
            args: vec!["-c".into(), "seq 1 300000".into()],
            ..PtySpec::new("/bin/sh")
        };
        let pane = PtyPane::spawn(spec, PaneSize::new(24, 80)).unwrap();
        wait_for(|| lock(&pane.term).vt.screen_top() > 1000, "no output");
        let (tx, rx) = std::sync::mpsc::channel();
        pane.tap(
            7,
            Box::new(move |feed| {
                let fed = match feed {
                    Feed::Snapshot(size, bytes) => (Some(size), bytes),
                    Feed::Raw(bytes) => (None, bytes.to_vec()),
                };
                tx.send(fed).is_ok()
            }),
        );
        wait_for(
            || pane.output_receiver().has_changed().is_err(),
            "the output never ended",
        );

        let (size, snapshot) = rx.recv().unwrap();
        assert_eq!(size, Some(PaneSize::new(24, 80)));
        let seeded = client(24, 80, &snapshot);
        let mut text = seeded.contents();
        // The cursor at the start of an empty row: the last line ended. At
        // the start of a written one, a read ended between its CR and LF.
        let (row, col) = seeded.cursor_position();
        if col == 0 && seeded.screen_row(row).text().is_empty() {
            text.push('\n');
        }
        let mut reads = 0;
        while let Ok((None, bytes)) = rx.try_recv() {
            text.push_str(&String::from_utf8(bytes).unwrap().replace('\r', ""));
            reads += 1;
        }
        let numbers: Vec<u64> = text.lines().map(|l| l.parse().unwrap()).collect();
        assert!(reads > 0 && numbers.len() > 24, "joined after the output");
        assert!(
            numbers.windows(2).all(|w| w[1] == w[0] + 1),
            "a read was lost or repeated"
        );
        assert_eq!(numbers.last(), Some(&300_000));

        pane.resize(10, 40).unwrap();
        let (size, snapshot) = rx.recv().unwrap();
        assert_eq!(size, Some(PaneSize::new(10, 40)));
        assert_eq!(client(10, 40, &snapshot).contents(), pane.contents());
        pane.untap(7);
        assert!(rx.recv().is_err(), "an untapped tap is dropped");
    }

    /// Poll `done` for up to three seconds.
    fn wait_for(done: impl Fn() -> bool, failure: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{failure}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A wide character echoed into a one-column pane — which once panicked
    /// the daemon's vt100 on the reader thread — lands as the replacement
    /// character, and the pane runs on.
    #[test]
    fn a_wide_character_in_a_one_column_pane_never_stops_the_reader() {
        let pane = PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 1)).unwrap();
        pane.write_input("日\n".as_bytes()).unwrap();
        wait_for(
            || pane.contents().contains('\u{FFFD}'),
            "the character never landed",
        );
        assert!(!pane.term.is_poisoned());
        pane.resize(24, 80).unwrap();
        pane.write_input(b"still-here\n").unwrap();
        wait_for(
            || pane.snapshot().text().contains("still-here"),
            "output stopped after the wide character",
        );
        pane.kill().unwrap();
    }

    /// A thread panicking while it holds a pane's locks poisons them; every
    /// later lock recovers, so the pane still writes, reads and is looked at.
    #[test]
    fn poisoned_locks_recover() {
        let pane =
            Arc::new(PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 80)).unwrap());
        let poisoner = Arc::clone(&pane);
        let _ = std::thread::spawn(move || {
            let _term = poisoner.term.lock();
            let _writer = poisoner.writer.lock();
            let _notifications = poisoner.notifications.lock();
            panic!("poisoning the pane's locks");
        })
        .join();
        assert!(pane.term.is_poisoned() && pane.writer.is_poisoned());
        pane.write_input(b"after-poison\n").unwrap();
        wait_for(
            || pane.snapshot().text().contains("after-poison"),
            "the pane stopped reading after the poison",
        );
        let tail = pane.tail(0, None, false).unwrap();
        assert!(tail.lines.iter().any(|line| line == "after-poison"));
        let look = pane.look(None, true);
        let client = client(look.size.rows, look.size.cols, &look.snapshot.unwrap());
        assert!(client.contents().contains("after-poison"));
        assert!(pane.take_notifications().is_empty());
        pane.kill().unwrap();
    }

    /// A pane is the unit of failure. Its engine panicking — on a read, or
    /// drawing the screen for the tick — ends that pane alone: its child is
    /// killed, its screen tells of the crash, a notification says so, and its
    /// lock is never poisoned, so whoever takes it next gets an answer. The
    /// pane beside it reads and draws on.
    #[test]
    fn a_panicking_engine_ends_its_pane_alone() {
        let cat = || PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 80)).unwrap();
        let (reading, drawing, beside) = (cat(), cat(), cat());
        let crashed = |pane: &PtyPane| {
            wait_for(
                || pane.exit_status().is_some(),
                "the crashed pane's child lives on",
            );
            assert!(!pane.term.is_poisoned());
            assert!(
                pane.contents().contains("terminal crashed: injected"),
                "{:?}",
                pane.contents()
            );
            let notes = pane.take_notifications();
            assert!(
                notes.iter().any(|signal| matches!(signal,
                    Signal::Notify(note) if note.title.as_deref() == Some("pane crashed"))),
                "{notes:?}"
            );
        };

        reading.write_input(&[PANIC_READ, b"\n"].concat()).unwrap();
        crashed(&reading);

        drawing
            .write_input(format!("\x1b]2;{PANIC_LOOK}\x07\n").as_bytes())
            .unwrap();
        wait_for(
            || lock(&drawing.term).vt.title() == PANIC_LOOK,
            "the title never landed",
        );
        let look = drawing.look(None, true);
        let shown = client(24, 80, &look.snapshot.unwrap());
        assert!(shown.contents().contains("terminal crashed: injected"));
        crashed(&drawing);

        beside.write_input(b"still-here\n").unwrap();
        wait_for(
            || beside.contents().contains("still-here"),
            "the pane beside stopped",
        );
        let look = beside.look(None, true);
        let shown = client(24, 80, &look.snapshot.unwrap());
        assert!(shown.contents().contains("still-here"));
        assert!(beside.exit_status().is_none() && !beside.term.is_poisoned());
        beside.kill().unwrap();
    }
}
