# tutti and rtty

How tutti works with [rtty](https://github.com/mcginleyr1/rtty), a terminal
with panes of its own: which of the two is the front end for a tutti session,
how tutti's TUI behaves when it runs inside an rtty pane (the keys it claims
from rtty, and where a move or a resize past tutti's edge goes), and rtty
showing a tutti session's panes itself.

## The two front ends

tutti's TUI is the primary front end for a tutti session. It is the complete
one: the agent sidebar, the approvals popup, the notes and the diffs are
there, and new agent features land there first.

rtty's native tutti panes (the [last section](#rtty-showing-tutti-panes-itself))
are a plain view of a session: rtty draws the panes, shows on the tab's pill
when an agent is working or blocked, and notifies when an agent is blocked or
done. No further agent UI is planned in rtty.

## tutti in rtty

[rtty](https://github.com/mcginleyr1/rtty) is a terminal with panes of its
own. Run the TUI in an rtty pane, and one set of keys moves through Neovim's
windows, tutti's panes and rtty's panes alike: `Ctrl+h/j/k/l` to focus, and
`Alt+h/j/k/l` to resize.

The TUI knows it runs in rtty from its own environment, the one the terminal
you attach from gives it: `RTTY_PANE` set and `RTTY_SOCKET` naming a socket.
XTVERSION cannot tell, because rtty and tutti's own panes both answer as
`rtty-vt`. The daemon's environment never counts, and its panes never see
either variable. In rtty, the TUI does four things:

- **It claims rtty's keys.** On attach it sets the user var `RTTY_KEYS=pane`
  in its rtty pane. Under rtty's `keys = "tutti"`, rtty then hands its
  `Ctrl/Alt+h/j/k/l` to the TUI instead of acting on them.
- **It gives the keys back** (an empty `RTTY_KEYS`) on detach, on a panic,
  and when stopped. The keys can't stop it, since raw mode sends `Ctrl-Z` to
  the pane, but `kill -TSTP` does, and the shell beneath gets rtty's keys
  while it waits. On `fg` the TUI claims them again and redraws.
- **It says whose TUI it is.** Once attached it sets two more user vars in
  its rtty pane: `TUTTI_TUI`, the session it shows, and `TUTTI_TUI_HOST`, the
  ssh destination of a session on another machine (`tutti --host`), empty
  for one on this. rtty reports a pane's user vars, so a window manager
  around rtty can find the window that holds a session's TUI and send it
  `tutti pane focus <id>`. A view sets them too. Both are unset with the
  keys.
- **It hands a move past its own edge on to rtty.** It runs
  `rtty cli pane focus --direction <d> --from $RTTY_PANE` or `rtty cli pane
  resize …`, off its event loop, and gives rtty 500 ms to answer.

Where `Ctrl+h/j/k/l` leads past the focused pane's edge:

| Edge | Goes to |
| --- | --- |
| left | the sidebar when visible, then rtty's pane to the left. At rtty's own edge (`rtty cli` exits 1), the previous tab |
| right | the changes panel when visible, then rtty's pane to the right. At rtty's own edge, the next tab |
| top, bottom | rtty's pane above or below. At rtty's edge, nowhere |

The order follows what is on screen. tutti's panels are part of its screen,
so they come first, as they do outside rtty. rtty's panes are on screen and
tutti's other tabs are not, so rtty's panes come next. The tab wrap is the
fallback at rtty's edge, so `Ctrl+h/l` still reaches every tab.

`Alt+h/j/k/l` resizes tutti's split when the focused pane has one on that
axis, else rtty's (`rtty cli pane resize --direction <d> --from
$RTTY_PANE`).

These stay tutti's alone:
- the prefix's `h/j/k/l`, which only ever move between tutti's panes;
- the changes panel's own `l`, which goes to the next tab.

An editor holding tutti's keys carries the chain on, with the same key at
every step:
1. Neovim with `keys = true`, or Emacs's `tutti-windmove-*`, calls `tutti
   pane focus --direction` at its own edge.
2. The daemon hands that move to the TUI (in rtty, at the top and bottom edges
   too), which goes on as its own key would.
3. So one `Ctrl+k` in Neovim's top window lands in rtty's pane above.

An editor's `tutti pane resize` that no split of tutti's takes moves rtty's
split in the same way.

## rtty showing tutti panes itself

rtty can show the panes of a tutti session as panes of its own, without
tutti's TUI:

- a window that is a whole session: `rtty --tutti <session>`, or `rtty cli
  tutti open` from a pane;
- one tutti pane in rtty's own layout: `rtty --tutti <session>:<pane>`, or
  `rtty cli tutti attach <pane>` from a pane.

tutti's daemon keeps the programs, the session and the agents' states, and
rtty draws, so rtty's own scrollback, search and selection work on the pane.
tutti's TUI can stay attached beside it.

This is the plain view of [the two front ends](#the-two-front-ends): the
tab's pill shows when an agent is working or blocked, and a notification says
when one is blocked or done. tutti's agent chrome (the sidebar, approvals,
notes, the changes panel) stays in the TUI and is not planned in rtty.

rtty does not yet reach sessions on other machines (`tutti --host`). It never
starts a daemon: tutti's daemon must be running, and its wire and rtty's must
work together, each at or above the other's floor (an rtty that speaks wire
revision 20 works with a daemon at 19 or newer; see "Revisions and floors" in
[transport-decision.md](transport-decision.md)).

The feature is rtty's and is documented there, in
[docs/tutti.md](https://github.com/mcginleyr1/rtty/blob/main/docs/tutti.md#tutti-panes).
The frames it uses on tutti's socket are in
[transport-decision.md](transport-decision.md).
