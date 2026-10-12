//! The GUI log ring: a non-blocking writer, a drain thread, a bounded ring.
//!
//! The engine worker that logs must never perform I/O or lock a mutex, so lines
//! go through an unbounded channel to one background thread that owns the ring.

use std::io::Write;
use std::sync::mpsc;

use streampipe::gui::log::{new_ring, push, snapshot, TeeWriter, MAX_LINES};

#[test]
fn writer_splits_lines_into_ring() {
    let (tx, rx) = mpsc::channel::<String>();
    let mut w = TeeWriter {
        tx: tx.clone(),
        buf: Vec::new(),
    };
    w.write_all(b"first\nsecond").unwrap();
    w.flush().unwrap();
    // Drop every sender to close the channel so collecting terminates.
    drop(tx);
    drop(w);
    let texts: Vec<String> = rx.iter().collect();
    assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
}

/// The whole pipeline: writer to channel to drain thread to ring.
#[test]
fn drained_lines_land_in_the_ring() {
    let ring = new_ring();
    let (tx, rx) = mpsc::channel::<String>();
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
    assert_eq!(
        texts,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn ring_is_bounded() {
    let ring = new_ring();
    for i in 0..(MAX_LINES + 50) {
        push(&ring, format!("line {i}"));
    }
    let snap = snapshot(&ring);
    assert_eq!(snap.len(), MAX_LINES);
    assert_eq!(snap.last().unwrap().0, format!("line {}", MAX_LINES + 49));
}
