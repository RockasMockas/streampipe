//! Datagram fragmentation and reassembly for the media tunnel.
//!
//! A payload that does not fit `max_datagram_size()` is split into
//! `TAG_FRAGMENT` datagrams on the host and put back together on the viewer.
//! The parts may arrive out of order, interleaved with other datagrams, and
//! duplicated; a set that is not completed in time is abandoned.

use std::time::Instant;

use streampipe::rtp::{fragment_all, Reassembler, FRAG_HEADER_LEN, FRAG_TIMEOUT};

#[test]
fn fragments_roundtrip() {
    // A 100 byte datagram split at a 40 byte limit is three fragments,
    // each within the limit, and they reassemble to exactly the original.
    let original: Vec<u8> = (0..100u8).collect();
    let frags = fragment_all(&original, 7, 40).expect("fragments");
    assert_eq!(frags.len(), 3);
    assert!(frags.iter().all(|f| f.len() <= 40));

    let mut r = Reassembler::default();
    let now = Instant::now();
    assert!(r.push(&frags[0], now).is_none());
    assert!(
        r.push(&frags[2], now).is_none(),
        "the last fragment is still missing"
    );
    assert_eq!(r.push(&frags[1], now).expect("complete"), original);
}

#[test]
fn fragments_tolerate_order_dupes_and_interleaving() {
    let original: Vec<u8> = (0..50u8).collect();
    let other: Vec<u8> = (200..250u8).collect();
    let frags = fragment_all(&original, 1, 25).expect("fragments");
    let other_frags = fragment_all(&other, 2, 25).expect("fragments");
    assert_eq!(frags.len(), 3);

    let mut r = Reassembler::default();
    let now = Instant::now();
    // out of order and interleaved with another datagram, with a dupe
    assert!(r.push(&other_frags[0], now).is_none());
    assert!(r.push(&frags[2], now).is_none());
    assert!(
        r.push(&frags[2], now).is_none(),
        "a dupe is not complete twice"
    );
    assert!(r.push(&other_frags[1], now).is_none());
    assert!(r.push(&frags[0], now).is_none());
    assert_eq!(r.push(&frags[1], now).expect("complete"), original);
    assert_eq!(
        r.push(&other_frags[1], now),
        None,
        "the dupe completed nothing"
    );
    assert_eq!(r.push(&other_frags[2], now).expect("complete"), other);
}

#[test]
fn stale_fragments_are_dropped() {
    let frags = fragment_all(b"0123456789", 1, 8).expect("fragments");
    let mut r = Reassembler::default();
    let now = Instant::now();
    assert!(r.push(&frags[0], now).is_none());
    // past the timeout the partial set is gone, and the remaining
    // fragments must not complete a lie
    assert!(r.push(&frags[1], now + FRAG_TIMEOUT).is_none());
    assert!(r.push(&frags[2], now + FRAG_TIMEOUT).is_none());
    assert!(
        r.push(&frags[3], now + FRAG_TIMEOUT).is_none(),
        "the evicted first fragment must not be forgotten as present"
    );
}

#[test]
fn clear_forgets_partial_datagrams() {
    // 10 bytes at a 8 byte limit is four fragments of three bytes.
    let frags = fragment_all(b"0123456789", 1, 8).expect("fragments");
    assert_eq!(frags.len(), 4);
    let mut r = Reassembler::default();
    let now = Instant::now();
    assert!(r.push(&frags[0], now).is_none());
    r.clear();
    // after the clear the old fragment is gone and must not count:
    assert!(r.push(&frags[1], now).is_none());
    assert!(r.push(&frags[2], now).is_none());
    assert!(r.push(&frags[3], now).is_none(), "fragment 0 was cleared");
    // ... and the datagram completes only when it arrives again
    assert_eq!(
        r.push(&frags[0], now).expect("complete"),
        b"0123456789".to_vec()
    );
}

#[test]
fn fragment_rejects_absurd_limits() {
    assert!(fragment_all(b"12345", 0, FRAG_HEADER_LEN).is_none());
    assert!(fragment_all(&vec![0u8; 300], 0, 6).is_none(), "total > 255");
}
