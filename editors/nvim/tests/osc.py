"""Drives a real nvim TUI inside a pty and checks the tutti user vars reach
the terminal: set on start, cleared and set again across a suspend, cleared
on quit.

    python3 -I osc.py <nvim> <editors/nvim dir>
"""

import base64
import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

SET = re.compile(rb"\x1b\]1337;SetUserVar=(TUTTI_\w+)=([A-Za-z0-9+/=]*)\x07")


def spawn(argv, env):
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(argv[0], argv, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    return pid, fd


def answer(fd, chunk):
    """Answers the queries nvim's TUI waits on, as a terminal would: the
    background colour and status report at startup, and the DA1 it sends as a
    barrier before it stops."""
    if b"\x1b]11;?" in chunk:
        os.write(fd, b"\x1b]11;rgb:0000/0000/0000\x1b\\")
    if b"\x1b[5n" in chunk:
        os.write(fd, b"\x1b[0n")
    if b"\x1b[c" in chunk:
        os.write(fd, b"\x1b[?62;22c")


def read_while(fd, more, timeout=10.0):
    """Reads until `more(bytes so far)` is false, EOF, or the timeout."""
    out, deadline = b"", time.monotonic() + timeout
    while more(out) and time.monotonic() < deadline:
        if select.select([fd], [], [], 0.05)[0]:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            answer(fd, chunk)
            out += chunk
    return out


def user_vars(out):
    return [(name.decode(), base64.b64decode(value).decode()) for name, value in SET.findall(out)]


def count_until(n):
    return lambda out: len(user_vars(out)) < n


def check(label, got, expected):
    print(("ok   " if got == expected else "FAIL ") + label + f": {got!r}")
    return got == expected


def run(nvim, plugin, setup):
    """The user vars nvim announces on start, on Ctrl-Z, on `fg` and on quit,
    plus whether its server socket existed while it ran and whether the shell
    saw it stop. nvim runs under a job-control shell, as in a pane: a pty
    child's own process group is orphaned, so its SIGTSTP would be dropped."""
    env = dict(os.environ, TUTTI_PANE="7", TERM="xterm-256color", PS1="$ ", ENV="")
    pid, fd = spawn(["/bin/sh", "-i"], env)
    os.write(fd, f"{nvim} --clean --cmd 'set rtp^={plugin}' --cmd \"lua {setup}\"\r".encode())
    started = user_vars(read_while(fd, count_until(2)))
    server = dict(started).get("TUTTI_EDITOR", "").removeprefix("nvim ")
    listening = os.path.exists(server)
    os.write(fd, b"\x1a")
    out = read_while(fd, lambda out: len(user_vars(out)) < 2 or b"Stopped" not in out)
    suspended, was_stopped = user_vars(out), b"Stopped" in out
    os.write(fd, b"fg\r")
    resumed = user_vars(read_while(fd, count_until(2)))
    os.write(fd, b":qa!\r")
    quit = user_vars(read_while(fd, count_until(2)))
    # Hang up before reaping: an exiting tty session leader waits for its
    # unread output to drain.
    os.close(fd)
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    return started, listening, suspended, was_stopped, resumed, quit


def main(nvim, plugin):
    cleared = [("TUTTI_KEYS", ""), ("TUTTI_EDITOR", "")]
    started, listening, suspended, was_stopped, resumed, quit = run(
        nvim, plugin, "require('tutti').setup({ keys = true })"
    )
    plain = run(nvim, plugin, "vim.g.tutti_plain = true")[0]
    results = [
        check("start claims the keys", started[:1], [("TUTTI_KEYS", "pane")]),
        check("start names a listening server", started[1][1].startswith("nvim /") and listening, True),
        check("suspend clears", suspended, cleared),
        check("suspend stops nvim", was_stopped, True),
        check("resume sets again", resumed, started),
        check("quit clears", quit, cleared),
        check("without keys, the editor alone", [name for name, value in plain if value], ["TUTTI_EDITOR"]),
    ]
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
