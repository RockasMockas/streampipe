//! A tee log writer for the GUI.
//!
//! The CLI installs a plain stderr subscriber. The GUI installs this instead:
//! every formatted log line goes to stderr (exactly what the CLI shows) *and*
//! into a bounded ring buffer the log panel renders. This is a `MakeWriter`,
//! not a `registry` `Layer`, because `tracing-subscriber` is compiled without
//! the `registry` feature.
//!
//! The writers handed out here are **wait-free**: they only push to an
//! unbounded channel. They never touch `stderr` and never lock the ring,
//! because the callers are the tokio workers of the engine runtime, and a
//! stall there is a media outage: the WHIP host polls the ICE socket from one
//! of those threads, so a worker parked on a Windows GUI `stderr`, or behind
//! a mutex the UI thread re-acquires at 60 fps, overruns the OS UDP receive
//! buffer during a keyframe burst and the viewer sees decode errors. One
//! background thread drains the channel to `stderr` and the ring.

use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::mpsc::Sender,
    sync::{Arc, Mutex},
    time::Instant,
};

use tracing_subscriber::{fmt::MakeWriter, EnvFilter};

/// The most log lines kept in the ring for the GUI log panel.
const MAX_LINES: usize = 2000;

/// A bounded, thread-safe ring of log lines, each tagged with the instant it was
/// added so the log panel can fade a new line in over the first ~200 ms instead of
/// popping it in.
pub type LogRing = Arc<Mutex<VecDeque<(String, Instant)>>>;

/// Create an empty ring.
pub fn new_ring() -> LogRing {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// A snapshot of the ring for rendering (cloned so the lock is never held by
/// the UI), with each line's arrival instant so the panel can fade new lines in.
pub fn snapshot(ring: &LogRing) -> Vec<(String, Instant)> {
    ring.lock()
        .map(|ring| ring.iter().cloned().collect())
        .unwrap_or_default()
}

/// Push a manual line into the ring (used for GUI-originated messages that are
/// not produced by `tracing`), tagged with the current instant for the fade-in.
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
pub(crate) struct TeeWriter {
    tx: Sender<String>,
    buf: Vec<u8>,
}

impl TeeWriter {
    /// Move any complete lines from the buffer to the logging thread.
    fn drain_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches('\n')
                .to_string();
            // A closed channel means the process is shutting down; ignoring the
            // error keeps logging cheap and infallible on the media hot path.
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
        // Surface any trailing partial line without a newline.
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
/// `RUST_LOG` wins if set, matching the CLI. ANSI is disabled so the ring
/// holds plain text for the log panel.
///
/// The formatted lines go through an unbounded channel to one background
/// thread, which is the only place that writes to `stderr` or locks the ring.
/// The thread ends when the subscriber (and with it the last `Sender`) is
/// dropped, so no explicit teardown is needed.
pub fn install(ring: LogRing, verbosity: u8) {
    let filter = EnvFilter::builder()
        .with_default_directive(crate::log_level(verbosity).into())
        .from_env_lossy();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let drain_ring = ring.clone();
    std::thread::spawn(move || {
        while let Ok(line) = rx.recv() {
            // Mirror to stderr so the GUI process shows the same output as the
            // CLI, from a thread whose blocking we do not care about.
            eprintln!("{line}");
            push(&drain_ring, line);
        }
    });
    let tee = TeeMakeWriter { tx };
    // Ignore the "already installed" error: the GUI installs once, but a second
    // call (e.g. a stray re-init) must not panic.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(tee)
        .with_ansi(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_splits_lines_into_ring() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let mut w = TeeWriter {
            tx: tx.clone(),
            buf: Vec::new(),
        };
        w.write_all(b"first\nsecond").unwrap();
        w.flush().unwrap();
        // The writer only hands lines to the drain thread; dropping every
        // sender closes the channel so collecting terminates.
        drop(tx);
        drop(w);
        let texts: Vec<String> = rx.iter().collect();
        assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
    }

    /// The whole pipeline: writer to channel to drain thread to ring.
    #[test]
    fn drained_lines_land_in_the_ring() {
        let ring = new_ring();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let drain_ring = ring.clone();
        let drainer = std::thread::spawn(move || {
            while let Ok(line) = rx.recv() {
                push(&drain_ring, line);
            }
        });
        let mut w = TeeWriter {
            tx,
            buf: Vec::new(),
        };
        w.write_all(b"a\nb\nc").unwrap();
        w.flush().unwrap();
        drop(w);
        drainer.join().unwrap();
        let texts: Vec<String> = snapshot(&ring).into_iter().map(|(s, _)| s).collect();
        assert_eq!(texts, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn ring_is_bounded() {
        let ring = new_ring();
        for i in 0..(MAX_LINES + 50) {
            push(&ring, format!("line {i}"));
        }
        let snap = snapshot(&ring);
        assert_eq!(snap.len(), MAX_LINES);
        assert_eq!(
            snap.last().unwrap().0,
            format!("line {}", MAX_LINES + 49)
        );
    }
}
