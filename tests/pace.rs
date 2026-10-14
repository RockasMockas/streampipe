//! Sender-side shaping of the media tunnel.
//!
//! The host paces video at a multiple of its measured bitrate so a keyframe
//! burst does not overflow the bottleneck queue, and runs the QUIC connection
//! with a congestion controller that does not throttle the stream on loss.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use noq::congestion::ControllerFactory;
use streampipe::pace::{MediaCongestion, Pacer, MEDIA_WINDOW, PACE_BURST, PACE_FACTOR, PACE_FLOOR};

#[test]
fn pacer_starts_at_its_floor() {
    let pacer = Pacer::new(Instant::now());
    assert_eq!(pacer.rate(), PACE_FLOOR);
}

#[test]
fn pacer_follows_the_media_rate() {
    let t0 = Instant::now();
    let mut pacer = Pacer::new(t0);
    // 10 MB/s of media, in 1 ms steps over the first measurement window.
    for ms in 1..=250 {
        pacer.on_arrival(10_000, t0 + Duration::from_millis(ms));
    }
    let rate = pacer.rate();
    let want = 10_000_000.0 * PACE_FACTOR;
    assert!(
        (rate - want).abs() / want < 0.01,
        "rate {rate}, want {want}"
    );
}

#[test]
fn pacer_spends_a_burst_then_spreads_the_rest() {
    let t0 = Instant::now();
    let mut pacer = Pacer::new(t0);
    let packet = 1200;
    let mut sent = 0;
    while pacer.take(packet, t0).is_none() {
        sent += packet;
    }
    assert!(
        sent as f64 <= PACE_BURST && sent as f64 > PACE_BURST - packet as f64,
        "one burst goes out at once: {sent}"
    );
    // The next packet waits for its tokens at the pacing rate.
    let wait = pacer.take(packet, t0).expect("a wait");
    assert!(wait <= Duration::from_secs_f64(packet as f64 / PACE_FLOOR));
    assert!(
        pacer.take(packet, t0 + wait).is_none(),
        "and then it may go"
    );
}

#[test]
fn pacer_never_waits_forever_on_a_large_packet() {
    let t0 = Instant::now();
    let mut pacer = Pacer::new(t0);
    let huge = 4 * PACE_BURST as usize;
    assert!(pacer.take(huge, t0).is_none(), "a full bucket pays for it");
    let wait = pacer.take(huge, t0).expect("an empty bucket waits");
    assert!(pacer.take(huge, t0 + wait).is_none(), "but not forever");
}

#[test]
fn media_congestion_does_not_back_off_on_loss() {
    let mut cc = Arc::new(MediaCongestion).build(Instant::now(), 1200);
    assert_eq!(cc.window(), MEDIA_WINDOW);
    assert_eq!(cc.initial_window(), MEDIA_WINDOW);
    let now = Instant::now();
    cc.on_congestion_event(now, now, false, false, 1200, 7);
    cc.on_congestion_event(now, now, true, false, 120_000, 99);
    assert_eq!(
        cc.window(),
        MEDIA_WINDOW,
        "loss is not a reason to queue a live stream"
    );
}
