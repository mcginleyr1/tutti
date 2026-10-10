//! `tutti-server --session <name> [--foreground]`.
//!
//! Without `--foreground` the process re-execs a detached copy of itself (its
//! own process group, stdin/stdout to /dev/null, stderr appended to the
//! session's log) with `--foreground` appended and exits, leaving the daemon
//! running past the launching shell.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tutti_core::{log_path, socket_path, state_dir};
use tutti_server::PaneSize;

/// Once the log has grown past this, it is trimmed as the daemon starts...
const LOG_CAP: u64 = 4 << 20;
/// ...to its newest this many bytes.
const LOG_KEEP: usize = 1 << 20;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut session = "tutti".to_string();
    let mut foreground = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                i += 1;
                session.clone_from(args.get(i).context("--session needs a value")?);
            }
            "--foreground" => foreground = true,
            other => bail!("unknown argument {other:?}"),
        }
        i += 1;
    }

    if !foreground {
        return daemonize(&session);
    }

    let path = socket_path(&session);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(tutti_server::run(path, PaneSize::new(24, 80), state_dir()))
}

fn daemonize(session: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().context("resolve current executable")?;
    Command::new(exe)
        .arg("--session")
        .arg(session)
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(open_log(&log_path(session))?)
        .process_group(0)
        .spawn()
        .context("spawn detached server")?;
    Ok(())
}

/// Open the session's log for the detached daemon's stderr — appended, so the
/// panics and crash notes of earlier runs survive — first trimming it to its
/// newest `LOG_KEEP` bytes, from a line start, once it outgrows `LOG_CAP`.
fn open_log(path: &Path) -> Result<File> {
    let dir = path.parent().context("log path has no parent directory")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > LOG_CAP) {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let tail = &bytes[bytes.len() - LOG_KEEP..];
        let tail = tail
            .iter()
            .position(|&b| b == b'\n')
            .map_or(tail, |at| &tail[at + 1..]);
        std::fs::write(path, tail).with_context(|| format!("trim {}", path.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open log {}", path.display()))
}
