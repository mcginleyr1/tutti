use std::path::PathBuf;

/// Socket path for a named session: `$XDG_RUNTIME_DIR/tutti/<session>.sock`,
/// falling back to `/tmp/tutti-<uid>/<session>.sock`.
pub fn socket_path(session: &str) -> PathBuf {
    socket_dir().join(format!("{session}.sock"))
}

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

pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("tutti"),
        _ => PathBuf::from(format!("/tmp/tutti-{}", libc_getuid())),
    }
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

unsafe extern "C" {
    #[link_name = "getuid"]
    safe fn libc_getuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_becomes_socket_file() {
        assert!(
            socket_path("tutti")
                .to_string_lossy()
                .ends_with("/tutti.sock")
        );
    }

    #[test]
    fn state_lives_in_a_tutti_dir() {
        assert!(state_dir().is_none_or(|dir| dir.ends_with("tutti")));
    }
}
