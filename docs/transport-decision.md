# M0 — Transport Decision: server → client pane content

**Status:** Decided. **Recommendation:** Option C (hybrid framing) with
server-computed `vt100` escape-sequence deltas as the pane-output payload.

This is the gate before any protocol code. It fixes how a pane's screen content
travels from `tutti-server` (which owns every PTY and a per-pane `vt100::Parser`)
to a `tutti` client (a ratatui 0.30 TUI attached over a Unix domain socket). The
decision hardens into the Phase 1 wire protocol.

## Constraints that drive the choice

From the plan and the alpha/beta targets:

- The server **already** runs a `vt100::Parser` per pane, with scrollback, because
  it must serve `pane read` and run agent-state heuristics on screen content. That
  parser exists no matter which transport we pick — so the question is really
  *"what does the server do with the grid it already has?"*
- Alpha: single attached client, 60 fps redraw with 8 busy panes, `pane read` on a
  10k-line scrollback in < 50 ms, input latency indistinguishable from a raw terminal.
- Beta: multiple simultaneous clients, smallest-client-wins sizing (tmux model).
- Control plane is newline-delimited JSON (NDJSON).
- The client renders into ratatui — it draws a **grid**, it is not a real terminal.
  So the fidelity ceiling of anything the client shows is whatever a `vt100` model
  captures, regardless of transport. This matters: it means no option can render
  *more* than the server's parser understands, which removes "raw fidelity" as a
  reason to prefer byte forwarding.

## What vt100 0.16 actually gives us

Verified against docs.rs for `vt100` 0.16:

- `Parser::new(rows, cols, scrollback_len) -> Self`, `process(&mut self, &[u8])`,
  `screen(&self) -> &Screen`, `screen_mut(&mut self) -> &mut Screen`. There is **no**
  constructor that accepts a screen snapshot — a parser only starts empty.
- `Screen::contents_formatted(&self) -> Vec<u8>` — the whole visible screen encoded
  as escape sequences that reproduce it when written to a terminal *or fed to another
  `vt100::Parser`*.
- `Screen::contents_diff(&self, prev: &Screen) -> Vec<u8>` — the minimal escape
  sequences that transform `prev` into `self`. This is a damage delta, produced for
  us, in a compact wire-ready encoding.
- `Screen::rows_formatted(start, width) -> impl Iterator<Item = Vec<u8>>`,
  `contents() -> String`, `contents_between(...)`, `cell(row, col) -> Option<&Cell>`,
  `cursor_position() -> (u16, u16)`, `size() -> (u16, u16)`, `set_size(rows, cols)`,
  `scrollback()`/`set_scrollback(rows)`.
- `Cell` exposes `contents()`, `fgcolor()`, `bgcolor()`, and the attribute getters
  (`bold`, `italic`, `underline`, `inverse`, …) needed to build a ratatui cell.

Two consequences fall straight out of this:

1. **A parser can be seeded from a snapshot** even though no snapshot constructor
   exists: `let mut p = Parser::new(r, c, sb); p.process(&snapshot_formatted);`
   reconstructs the exact screen. Attach/reattach becomes a snapshot, never a replay.
2. **The delta encoding is already written.** `contents_diff` *is* the damage-rectangle
   serializer that Option B proposes to hand-roll — in fewer bytes and symmetric on
   both ends.

## The three options

### A. Raw byte forwarding
Server tees each pane's raw PTY output to the client; the client runs its own
`vt100::Parser` per visible pane. The server keeps its parser for `pane read`/heuristics.

- **Attach/reattach:** you cannot replay a pane's entire PTY history (unbounded, and
  the raw bytes aren't retained anyway). The honest implementation is: server sends a
  `contents_formatted()` snapshot from its parser, *then* switches to live raw tail.
  So even "raw" forwarding needs the snapshot path — it just adds a raw firehose on top.
- **Scrollback:** the client would need its own scrollback ring, sized and managed
  independently of the server's — two sources of truth for the same history.
- **Double parse:** **two full `vt100` instances per pane** (server + client), each
  chewing the *entire* raw stream. For 8 panes that's 16 parsers over full volume.
- **Bandwidth:** whatever the PTYs emit. One `cargo build` or log-spew pane bursts
  1–10+ MB/s; a `yes`-class pane saturates the socket. Wire volume is **decoupled from
  the 60 fps of useful updates** — you pay for output the user never sees change.

### B. Server-rendered grid deltas as JSON
Server serializes screen state (full grids or damage rectangles of styled cells) into
JSON events on the control channel.

- **Bandwidth (arithmetic below):** JSON styled cells are the fattest possible encoding
  and put a large serialize/parse load on both ends every frame.
- **Redundant work:** damage-rectangle JSON is a worse-encoded reimplementation of
  `contents_diff`, plus a bespoke client-side cell deserializer to maintain.
- Its one virtue — a self-describing text format — is not worth the cost when the
  client is a `vt100` consumer anyway.

### C. Hybrid framing  ← recommended
One Unix socket carrying length-prefixed frames with a 1-byte kind tag: **control
frames** (kind = JSON, preserving the NDJSON *semantics* — one JSON object per frame)
and **pane-data frames** (kind = binary: a small header + `vt100` escape bytes). The
pane-data payload is **server-computed `contents_diff` deltas**, coalesced to the frame
tick, with a `contents_formatted` snapshot on attach. The client feeds those bytes into
its own lightweight per-pane `vt100::Parser` and reads cells to build the ratatui buffer.

> **Note on "NDJSON":** pure newline-delimited JSON cannot safely share a byte stream
> with binary pane data, because binary escape bytes contain `0x0A`. Length-prefixing
> every frame keeps the control protocol logically NDJSON (each control frame is exactly
> one JSON object) while making the wire binary-safe. This is the concrete reason
> Option C exists rather than shoving pane bytes through the JSON channel.

## Bandwidth arithmetic

Assumptions (conservative): 8 panes, each **80×24 = 1,920 cells**; target cadence
**60 fps (16.7 ms tick)**.

**Option B — full-grid styled-cell JSON.** A compact per-cell tuple like
`["x",9,0,1]` ≈ 15 B.
- 1,920 cells × 15 B = **28.8 KB / pane / frame**
- × 8 panes = **230 KB / frame**
- × 60 fps = **≈ 13.8 MB/s (~110 Mbit/s)** — plus serializing and parsing
  **≈ 922k cells/s** as JSON. This is CPU-bound long before it is wire-bound.
  Damage-only B lowers the wire cost but still requires a hand-rolled cell-diff and
  per-frame JSON parse of the changed cells.

**Option A — raw bytes.** Wire = the sum of raw PTY output. Busy panes burst
**1–10+ MB/s each**; 8 of them ⇒ **tens of MB/s**, entirely decoupled from the 60 fps
of useful visual change, and the client must run a full `vt100` parse over all of it.

**Option C — `contents_diff` deltas, coalesced per tick.** Wire = Σ diff per pane per
tick, so it is bounded by *visible change × frame rate*, not by raw output volume:
- Steady agent output (a few changed lines/frame ≈ 200 cells; run-length SGR ⇒
  ~0.5–1 KB): 8 × 1 KB × 60 = **≈ 480 KB/s**.
- Pathological full repaint every frame (`contents_formatted` ≈ 4–8 KB/pane):
  8 × 8 KB × 60 = **≈ 3.84 MB/s** worst case.
- Server cost: 8 diffs/tick ≈ **922k cell-compares/s** — negligible. Client parses only
  the coalesced diff bytes.

| Option | Steady wire | Worst-case wire | Client parse input | Bounded by frame rate? |
|---|---|---|---|---|
| A raw | low | **tens of MB/s** | full raw firehose | **No** |
| B JSON | ~13.8 MB/s | ~13.8 MB/s + heavy JSON CPU | changed cells (JSON) | with hand-rolled damage only |
| **C diff** | **~0.48 MB/s** | **~3.84 MB/s** | coalesced diff bytes | **Yes** |

The decisive property is **coalescing**: however hard a pane spews between ticks, the
server emits exactly one diff per pane per tick against the last frame the client saw.
Wire and client-CPU track what actually changed on screen, which is exactly the 60 fps
budget.

## How C scores on the rest

- **Attach / reattach — snapshot, never replay.** On attach the server sends one
  `contents_formatted()` frame per visible pane; the client seeds a fresh parser and is
  instantly current. Reattach is identical and O(screen), independent of how long the
  session ran. Option A needs this same snapshot path *and then* a raw tail on top.
- **Scrollback — one source of truth.** The server's parser owns the scrollback ring
  (it must, for `pane read`). Client scroll is a control request; the server ships the
  scrolled region. The client never maintains a second, divergent history buffer.
- **`pane read`.** A pure control-plane request answered from the same server parser
  via `rows_formatted`/`contents`. No client involvement; works headless for the CLI.
  Option A pays for two parsers per pane *always*; C reuses the one the server needs.
- **Latency / 60 fps.** Input is a tiny binary frame → server writes to the PTY: one
  socket hop, no parsing, indistinguishable from raw. The 16.7 ms tick adds ≤ one tick
  of echo latency; because coalescing is per-pane, the **focused** pane can be flushed
  eagerly (sub-tick) so local echo feels instant while background panes stay coalesced —
  a knob A cannot offer without also uncapping background volume.
- **Multiple clients / different sizes (beta).** The server renders one authoritative
  grid at the smallest-client size (tmux model). It diffs once against the last
  broadcast screen and **fans the identical delta bytes to every client**; a late
  joiner gets a snapshot then falls into the shared cadence. Raw forwarding would make
  each client parse independently and cannot share work; JSON would re-serialize per
  client. C is the only option where N clients cost ~one diff.
- **Implementation complexity.** Server: `contents_diff` per tick + framing — `vt100`
  hands us the payload. Client: `parser.process(delta_bytes)` then read cells — the same
  call as A, and *simpler* than B (no bespoke cell schema). Rendering into ratatui is
  identical across all three options, so it is not a differentiator.

**Honest limitations (accepted).** The client's fidelity is capped by the `vt100`
model — pane-originated OSC 52 clipboard, sixel, and kitty-graphics escapes are not
represented (the same "trapped escape" limitation any grid-rendered multiplexer has).
The plan already defers OSC 52 to beta and handles copy at the tutti layer, so this
costs us nothing at alpha. The server also keeps one cloned "last broadcast" `Screen`
per pane for diffing — modest, bounded memory.

## Recommendation

**Adopt Option C:** length-prefixed frames on the Unix socket; control frames carry one
JSON object each (NDJSON semantics), pane-data frames carry server-computed
`vt100` escape deltas. Attach with a `contents_formatted` snapshot, then stream
`contents_diff` deltas coalesced to a ~16 ms tick. The client renders from its own
per-pane parser fed by those bytes.

Three strongest reasons:

1. **Bounded, coalesced bandwidth and client CPU** — ~0.48 MB/s steady, ~3.84 MB/s
   worst case, versus B's ~13.8 MB/s of JSON and A's unbounded raw firehose — and it
   tracks the 60 fps budget by construction.
2. **The payload is free.** `contents_diff`/`contents_formatted` already emit the exact
   wire format; both ends are symmetric `vt100`. B's cell serializer and A's dual-parser
   firehose are strictly more code for a worse result.
3. **Clean scaling to beta.** Server-authoritative grid ⇒ one diff fans out to all
   clients, smallest-client sizing is natural, scrollback and `pane read` have a single
   source of truth.

## Concrete wire shapes

Transport frame (little-endian):

```
Frame:
  len:     u32   // byte length of (kind + payload)
  kind:    u8    // 0x01 Control(JSON) | 0x02 PaneSnapshot | 0x03 PaneDelta | 0x04 Input
  payload: [u8; len-1]
```

Control payload (`kind = 0x01`), one JSON object per frame:

```json
{"t":"attach","session":"tutti","size":{"rows":50,"cols":200}}
{"t":"attach_ok","layout":{"tabs":[{"id":1,"name":"main","active":true}],
  "panes":[{"id":3,"tab":1,"rect":{"x":0,"y":0,"w":80,"h":24},
            "agent":"claude","state":"working"}]}}
{"t":"pane_state","pane":3,"agent":"claude","state":"blocked"}
{"t":"resize","size":{"rows":50,"cols":200}}
{"t":"scroll","pane":3,"offset":120}
{"t":"read","pane":3,"lines":1000,"unwrapped":true}
{"t":"detach"}
```

Pane-data payload (`kind = 0x02` snapshot / `0x03` delta):

```
[u32 pane_id][u16 rows][u16 cols][u32 seq][escape_bytes...]
  snapshot escape_bytes = screen.contents_formatted()
  delta    escape_bytes = screen.contents_diff(&last_broadcast[pane])
  seq increments per pane; a gap after resize tells the client to expect a fresh snapshot
```

Input payload (`kind = 0x04`, client → server): `[u32 pane_id][raw key bytes...]`,
forwarded verbatim to the PTY (kept binary for the lowest-latency keystroke path).

> `seq` is belt-and-suspenders: the socket is `SOCK_STREAM` (reliable, ordered), so
> deltas cannot be dropped or reordered; `seq` exists to mark resync points (resize,
> reattach) and to assert the invariant in tests.

## Attach handshake

1. Client → `Frame{Control, {"t":"attach","session","size"}}`.
2. Server → `Frame{Control, {"t":"attach_ok","layout":{…tree, geometry, agent+state…}}}`.
3. Server → for each visible pane: `Frame{PaneSnapshot, pane_id, rows, cols, seq=0,
   contents_formatted()}`.
4. Client seeds one `vt100::Parser` per pane by `process`-ing each snapshot, then draws
   grids into ratatui.
5. **Steady state:** every ~16 ms tick, for each pane whose screen changed since its
   last broadcast, Server → `Frame{PaneDelta, pane_id, rows, cols, seq++,
   contents_diff(last_broadcast)}`; client `process`es it, marks the pane damaged, and
   redraws only damaged panes. The focused pane may be flushed sub-tick for instant echo.
6. **Resize:** client → `Frame{Control,{"t":"resize","size"}}`; server recomputes layout
   (smallest-client-wins), `set_size`s the affected parsers, and replies with the new
   layout plus fresh `PaneSnapshot` frames for resized panes.
7. **Scroll:** client → `Frame{Control,{"t":"scroll","pane","offset"}}`; server ships a
   snapshot of the scrolled region from the authoritative scrollback ring.
8. **Detach:** client → `Frame{Control,{"t":"detach"}}`; server drops the client and
   keeps every PTY and parser alive.
