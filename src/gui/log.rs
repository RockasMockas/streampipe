//! A tee log writer for the GUI: every formatted line goes to stderr (what the
//! CLI shows) *and* into a bounded ring the log panel renders.
//!
//! This is a `MakeWriter`, not a registry `Layer`, because `tracing-subscriber`
//! is compiled without the `registry` feature.
//!
//! The writers handed out here are **wait-free**: they only push to a channel.
//! The callers are the engine's tokio workers, and a stall there is a media
//! outage — the WHIP host polls the ICE socket from one of those threads, so a
//! worker parked on GUI `stderr` (or behind a mutex the UI thread re-acquires at
//! 60 fps) overruns the OS UDP receive buffer during a keyframe burst and the
//! viewer sees decode errors. One background thread drains the channel to
//! `stderr` and the ring.

use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::mpsc::Sender,
    sync::{Arc, Mutex},
    time::Instant,
};

use tracing_subscriber::{fmt::MakeWriter, EnvFilter};

/// The most log lines kept in the ring.
pub const MAX_LINES: usize = 2000;

/// A bounded, thread-safe ring of log lines, each tagged with the instant it was
/// added so the panel can fade a new line in over the first ~200 ms.
pub type LogRing = Arc<Mutex<VecDeque<(String, Instant)>>>;

/// Create an empty ring.
pub fn new_ring() -> LogRing {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// A snapshot of the ring for rendering (cloned so the lock is never held by the UI).
pub fn snapshot(ring: &LogRing) -> Vec<(String, Instant)> {
    ring.lock()
        .map(|ring| ring.iter().cloned().collect())
        .unwrap_or_default()
}

/// Push a manual line into the ring (for GUI-originated messages that `tracing`
/// doesn't produce), tagged with the current instant for the fade-in.
pub fn push(ring: &LogRing, line: impl Into<String>) {
    if let Ok(mut ring) = ring.lock() {
        ring.push_back((line.into(), Instant::now()));
        while ring.len() > MAX_LINES {
            ring.pop_front();
        }
    }
}

/// A writer that hands complete lines to the background logging thread.
///
/// Non-blocking by design: `write` only appends to a local buffer and pushes
/// finished lines onto a channel, so the engine worker that is logging never
/// performs I/O and never locks a mutex.
pub struct TeeWriter {
    /// Lines go to the single background logging thread.
    pub tx: Sender<String>,
    /// Not-yet-complete bytes, flushed on `flush()`.
    pub buf: Vec<u8>,
}

impl TeeWriter {
    /// Move any complete lines from the buffer to the logging thread.
    fn drain_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches('\n')
                .to_string();
            // A closed channel means shutdown; ignoring the error keeps logging
            // cheap and infallible on the media hot path.
            let _ = self.tx.send(text);
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        self.drain_lines();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Surface any trailing partial line.
        if !self.buf.is_empty() {
            let text = String::from_utf8_lossy(&self.buf).trim_end().to_string();
            let _ = self.tx.send(text);
            self.buf.clear();
        }
        Ok(())
    }
}

/// A `MakeWriter` handing out [`TeeWriter`]s bound to one channel.
pub(crate) struct TeeMakeWriter {
    tx: Sender<String>,
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            tx: self.tx.clone(),
            buf: Vec::new(),
        }
    }
}

/// Install the GUI logging: stderr plus the ring, at the given verbosity.
///
/// `RUST_LOG` wins if set, matching the CLI. ANSI is disabled so the ring holds
/// plain text. Lines go through an unbounded channel to one background thread —
/// the only place that writes to `stderr` or locks the ring. The thread ends when
/// the last `Sender` drops, so no explicit teardown is needed.
pub fn install(ring: LogRing, verbosity: u8) {
    let filter = EnvFilter::builder()
        .with_default_directive(crate::common::log_level(verbosity).into())
        .from_env_lossy();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let drain_ring = ring.clone();
    std::thread::spawn(move || {
        while let Ok(line) = rx.recv() {
            // Mirror to stderr so the GUI shows the same output as the CLI, from a
            // thread whose blocking we don't care about.
            eprintln!("{line}");
            push(&drain_ring, line);
        }
    });
    let tee = TeeMakeWriter { tx };
    // Ignore "already installed": the GUI installs once, but a stray re-init must
    // not panic.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(tee)
        .with_ansi(false)
        .try_init();
}
