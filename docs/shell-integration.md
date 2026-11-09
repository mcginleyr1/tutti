# Shell integration

The optional shell integration: one line in your shell's rc file that tells
tutti each pane's working directory and where each command and its output
begin and end. This page covers installing it, what it turns on, and what
each shell supports.

Add one line to your shell's rc file, or let the setup panel (`Ctrl+B i`, row
*shell integration*) append it for you, with a backup:

```sh
eval "$(tutti shell-init zsh)"   # ~/.zshrc (bash: ~/.bashrc, `shell-init bash`)
tutti shell-init fish | source   # ~/.config/fish/config.fish
```

Inside a tutti pane the shell then reports its working directory (OSC 7) and
marks each prompt and command (OSC 133); elsewhere it does nothing, and loading
it twice changes nothing. With it:

- a split (`%`, `"`, `tutti pane split`) starts in the directory you are in
  when that lies inside the workspace, and in the workspace's directory
  otherwise;
- `tutti --json pane list` shows each pane's `cwd`;
- `tutti pane read <pane> --last-output` prints the output of the last command
  that finished, and `exit: N` on stderr. While a command runs you get the one
  before it. It is exact to the cell: zsh's `%` marker and a prompt sharing the
  row are not part of it. While a full-screen program holds the alternate
  screen it is refused, and a pane without the integration says `no finished
  command`;
- each prompt clears the editor vars (`TUTTI_KEYS`, `TUTTI_EDITOR`), so an
  editor that crashed without clearing them hands the keys back.
- the end of each command turns off what the command left on in its terminal
  that the shell never asked for. That covers mouse reporting, focus reports
  (1004), light/dark reports (2031) and the key encodings (modifyOtherKeys,
  kitty's flags).
  - **Why:** a program killed with SIGKILL (Neovim after a `kill -9`) leaves
    them all on. The shell would then read the next light/dark report as
    typing, and the TUI would type into it in kitty's encoding, so Ctrl-C
    would no longer interrupt.
  - **What stays:** whatever the shell sets for its own prompt comes after the
    command's end. zsh, bash and fish set bracketed paste themselves, and fish
    also sets 2031 and modifyOtherKeys.
  - **The tty is not touched:** zsh keeps the raw mode a killed program leaves
    the tty in, and `stty sane` restores it.

bash needs 4.4 or newer for `--last-output` (`PS0`); macOS's `/bin/bash` 3.2
reports only the directory. The fish snippet has been run under fish 4.9.3,
which marks prompts and commands itself too; `--last-output` under fish has not
been checked.
