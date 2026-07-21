//! The attach socket: a background thread decodes inbound frames onto a
//! channel while the event loop writes frames from the foreground. Keeping the
//! read side on its own thread lets the loop block on terminal input without
//! stalling pane updates.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use tutti_core::Frame;

use crate::client;

pub struct Connection {
    stream: UnixStream,
    rx: Receiver<Frame>,
    reader: Option<JoinHandle<()>>,
}

impl Connection {
    /// Connect to the session's daemon (auto-starting it), spawning a reader
    /// thread that decodes inbound frames onto a channel.
    pub fn open(session: &str) -> Result<Self> {
        let stream = client::open(session)?;
        let reader_stream = stream.try_clone().context("clone attach socket")?;
        let (tx, rx) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("tutti-attach-reader".into())
            .spawn(move || read_loop(reader_stream, &tx))
            .context("spawn attach reader thread")?;
        Ok(Self {
            stream,
            rx,
            reader: Some(reader),
        })
    }

    pub fn send(&mut self, frame: &Frame) -> Result<()> {
        self.stream
            .write_all(&frame.encode())
            .context("write attach frame")?;
        self.stream.flush().context("flush attach frame")
    }

    /// Every frame the reader has decoded since the last call. `Err` means the
    /// server closed the connection and no frames remain.
    pub fn drain(&self) -> Result<Vec<Frame>> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(frame) => out.push(frame),
                Err(TryRecvError::Empty) => return Ok(out),
                Err(TryRecvError::Disconnected) => {
                    if out.is_empty() {
                        bail!("tutti-server closed the attach connection");
                    }
                    // Deliver the final frames; the next call reports the close.
                    return Ok(out);
                }
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn read_loop(mut stream: UnixStream, tx: &mpsc::Sender<Frame>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match Frame::decode(&buf) {
            Ok(Some((frame, consumed))) => {
                buf.drain(..consumed);
                if tx.send(frame).is_err() {
                    return;
                }
                continue;
            }
            Ok(None) => {}
            Err(_) => return,
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}
