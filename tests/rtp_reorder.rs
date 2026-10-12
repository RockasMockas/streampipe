//! Sequence reordering in front of the player.
//!
//! The host forwards one RTP packet per QUIC datagram, and datagrams are
//! unreliable *and* unordered, so the viewer must put them back in sequence
//! order before the player sees them — a player reads scrambled arrival as
//! packet loss. These check the three cases the `Reorder` has: in order, a hole
//! that gets filled, and a hole that has to be abandoned.

use std::time::{Duration, Instant};

use streampipe::rtp::{
    reorder_timeout_for, Reorder, REORDER_TIMEOUT, REORDER_TIMEOUT_MAX, REORDER_TIMEOUT_MIN,
};

#[test]
fn reorder_emits_in_order_packets_at_once() {
    let mut r = Reorder::default();
    let now = Instant::now();
    assert_eq!(r.push(1, vec![1], now), vec![vec![1]]);
    assert_eq!(r.push(2, vec![2], now), vec![vec![2]]);
    assert_eq!(r.push(3, vec![3], now), vec![vec![3]]);
}

#[test]
fn reorder_holds_a_head_until_the_hole_is_filled() {
    let mut r = Reorder::default();
    let now = Instant::now();
    assert_eq!(r.push(1, vec![1], now), vec![vec![1]]);
    // 3 arrives before 2: hold it, emit nothing.
    assert!(r.push(3, vec![3], now).is_empty(), "2 is still missing");
    // 2 arrives late: it and the held 3 go out in order.
    assert_eq!(r.push(2, vec![2], now), vec![vec![2], vec![3]]);
    assert_eq!(r.push(4, vec![4], now), vec![vec![4]]);
}

#[test]
fn reorder_flushes_held_packets_after_the_timeout() {
    let mut r = Reorder::default();
    let t0 = Instant::now();
    assert_eq!(r.push(1, vec![1], t0), vec![vec![1]]);
    // 2 is genuinely lost; 3 and 4 pile up behind the hole.
    assert!(r.push(3, vec![3], t0).is_empty());
    assert!(r.push(4, vec![4], t0).is_empty());
    // once the oldest held packet has waited past the timeout, the next
    // packet flushes everything in order and abandons the hole at 2.
    let later = t0 + REORDER_TIMEOUT;
    assert_eq!(r.push(5, vec![5], later), vec![vec![3], vec![4], vec![5]]);
    // the stream resumes normally from the new baseline.
    assert_eq!(r.push(6, vec![6], later), vec![vec![6]]);
}

#[test]
fn reorder_emits_packets_past_a_flushed_hole() {
    let mut r = Reorder::default();
    let t0 = Instant::now();
    assert_eq!(r.push(1, vec![1], t0), vec![vec![1]]);
    assert!(r.push(3, vec![3], t0).is_empty());
    // the hole at 2 is abandoned after the timeout
    assert_eq!(
        r.push(4, vec![4], t0 + REORDER_TIMEOUT),
        vec![vec![3], vec![4]]
    );
    // the very late 2 is passed through so a downstream buffer can reorder
    assert_eq!(r.push(2, vec![2], t0 + REORDER_TIMEOUT), vec![vec![2]]);
}

#[test]
fn reorder_clear_resets_the_baseline() {
    let mut r = Reorder::default();
    let now = Instant::now();
    assert_eq!(r.push(5, vec![5], now), vec![vec![5]]);
    assert!(r.push(7, vec![7], now).is_empty());
    r.clear();
    // after an epoch the baseline is gone: a new low sequence number is the
    // start of a fresh GOP, not a late packet to drop.
    assert_eq!(r.push(1, vec![1], now), vec![vec![1]]);
}

#[test]
fn reorder_timeout_tracks_the_player_buffer() {
    // No buffer (lowest latency): the default window.
    assert_eq!(reorder_timeout_for(None), REORDER_TIMEOUT);
    // A quarter of the buffer, clamped to the floor and ceiling.
    assert_eq!(
        reorder_timeout_for(Some(Duration::from_millis(200))),
        Duration::from_millis(50)
    );
    assert_eq!(
        reorder_timeout_for(Some(Duration::from_millis(500))),
        Duration::from_millis(125)
    );
    assert_eq!(
        reorder_timeout_for(Some(Duration::from_millis(1000))),
        REORDER_TIMEOUT_MAX,
        "capped so we never sit on lost packets"
    );
    assert_eq!(
        reorder_timeout_for(Some(Duration::from_millis(40))),
        REORDER_TIMEOUT_MIN,
        "floored to cover ordinary reordering"
    );
}

#[test]
fn reorder_honours_a_custom_timeout() {
    let mut r = Reorder::with_timeout(Duration::from_millis(200));
    let t0 = Instant::now();
    assert_eq!(r.push(1, vec![1], t0), vec![vec![1]]);
    assert!(r.push(3, vec![3], t0).is_empty());
    // still held at 80ms: the default window would have flushed by now.
    assert!(r
        .push(4, vec![4], t0 + Duration::from_millis(80))
        .is_empty());
    // flushed once past the longer, custom window.
    assert_eq!(
        r.push(5, vec![5], t0 + Duration::from_millis(200)),
        vec![vec![3], vec![4], vec![5]]
    );
}
