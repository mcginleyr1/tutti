use std::path::PathBuf;

/// Socket path for a named session: `$XDG_RUNTIME_DIR/tutti/<session>.sock`,
/// falling back to `/tmp/tutti-<uid>/<session>.sock`.
pub fn socket_path(session: &str) -> PathBuf {
    socket_dir().join(format!("{session}.sock"))
}

pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("tutti"),
        _ => PathBuf::from(format!("/tmp/tutti-{}", libc_getuid())),
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
}
