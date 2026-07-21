//! `tutti-server --session <name> [--foreground]`.
//!
//! Without `--foreground` the process re-execs a detached copy of itself (its
//! own process group, stdio to /dev/null) with `--foreground` appended and
//! exits, leaving the daemon running past the launching shell.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tutti_core::socket_path;
use tutti_server::PaneSize;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut session = "tutti".to_string();
    let mut foreground = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                i += 1;
                session = args.get(i).context("--session needs a value")?.clone();
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
    runtime.block_on(tutti_server::run(path, PaneSize::new(24, 80)))
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
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("spawn detached server")?;
    Ok(())
}
