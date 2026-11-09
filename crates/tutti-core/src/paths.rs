use std::path::PathBuf;

use tutti_wire::{socket_dir, socket_path};

/// The detached daemon's log, beside its socket: `<session>.log`. Its stderr
/// lands there, so panics and crash notes outlive the process.
pub fn log_path(session: &str) -> PathBuf {
    socket_dir().join(format!("{session}.log"))
}

/// Where a client writes a first message too long to ride an argument well,
/// beside the socket: `<session>.prompts`, private to the user like the
/// socket's own directory. The daemon removes a file once the program of the
/// pane whose argv names it exits, and the whole directory as it starts and
/// stops.
pub fn prompts_dir(session: &str) -> PathBuf {
    socket_path(session).with_extension("prompts")
}

/// Where a daemon saves its session's layout for the next one to offer back:
/// `$XDG_STATE_HOME/tutti`, falling back to `$HOME/.local/state/tutti`. `None`
/// with neither set — a relative path would land wherever the daemon happened
/// to start — which turns persistence off.
pub fn state_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir).join("tutti")),
        _ => std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".local/state/tutti")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_lives_in_a_tutti_dir() {
        assert!(state_dir().is_none_or(|dir| dir.ends_with("tutti")));
    }
}
