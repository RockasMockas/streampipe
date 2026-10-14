//! Sender-side pacing of the media tunnel.
//!
//! A publisher hands the host each video frame as one burst: OBS emits all the
//! RTP packets of a frame back to back, and a keyframe at 20 Mbit/s is 200-400 KB.
//! Forwarded as fast as it arrives, that burst leaves the host far faster than a
//! home uplink (or the viewer's downlink) can drain it, the bottleneck queue
//! overflows and tail-drops a run of consecutive packets — the `RTP: missed 17
//! packets` a player reports, followed by a smeared picture until the next IDR.
//! QUIC's own pacer does not prevent this: it paces at a multiple of
//! `cwnd / rtt`, which on an app-limited media flow is far above the link rate.
//!
//! [`Pacer`] spreads the video out instead, like a WebRTC sender's pacer: a token
//! bucket that refills at a multiple of the measured media bitrate. A P frame
//! goes out within a few milliseconds; a keyframe is stretched over a few tens
//! of milliseconds, which a bottleneck that keeps up with the stream on average
//! can absorb without dropping anything.
//!
//! With the media shaped here, QUIC's congestion controller is replaced by
//! [`MediaCongestion`], which lets it through instead of throttling it on loss.

use std::time::{Duration, Instant};

/// How much faster than the measured media bitrate the video is paced.
///
/// Pacing at exactly the media rate would queue every keyframe behind the
/// frames that follow it; a multiple drains a keyframe in a fraction of a GOP
/// while never sending faster than this factor over the stream itself, so a
/// link with this much headroom over the stream never queues it at all.
pub const PACE_FACTOR: f64 = 2.0;

/// The slowest the pacer ever goes, in bytes per second (24 Mbit/s).
///
/// Covers the start of a session, before the media rate is known, and keeps a
/// low-bitrate stream from being stretched for no benefit: its bursts are too
/// small to overflow any queue.
pub const PACE_FLOOR: f64 = 3_000_000.0;

/// The most bytes sent back to back, at line rate.
///
/// Small enough that a bottleneck queue never notices it, large enough that the
/// 1 ms timer granularity of the runtime does not throttle the pacing rate.
pub const PACE_BURST: f64 = 24.0 * 1024.0;

/// The longest a packet waits for the pacer before it is sent regardless.
///
/// Draining a keyframe takes well under this; it only binds while the media
/// rate estimate lags behind a sudden jump in bitrate, and then bounds the
/// latency the pacer can add.
pub const PACE_MAX_DELAY: Duration = Duration::from_millis(250);

/// The window over which the media bitrate is measured.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// The first window, shorter so the pacer leaves its floor quickly.
const FIRST_RATE_WINDOW: Duration = Duration::from_millis(250);

/// Weight of a new window in the smoothed media bitrate.
const RATE_ALPHA: f64 = 0.3;

/// A token bucket that paces bytes at a multiple of the rate they arrive at.
#[derive(Debug)]
pub struct Pacer {
    /// Bytes that may be sent right now.
    tokens: f64,
    /// When `tokens` was last refilled.
    refilled: Instant,
    /// Start of the current rate measurement window.
    window_start: Instant,
    /// Bytes that arrived in the current window.
    window_bytes: u64,
    /// The smoothed media rate in bytes per second, once a window has closed.
    media_rate: Option<f64>,
}

impl Pacer {
    pub fn new(now: Instant) -> Self {
        Pacer {
            tokens: PACE_BURST,
            refilled: now,
            window_start: now,
            window_bytes: 0,
            media_rate: None,
        }
    }

    /// Account for `bytes` of media arriving to be paced.
    pub fn on_arrival(&mut self, bytes: usize, now: Instant) {
        self.window_bytes += bytes as u64;
        let elapsed = now.duration_since(self.window_start);
        let window = match self.media_rate {
            Some(_) => RATE_WINDOW,
            None => FIRST_RATE_WINDOW,
        };
        if elapsed >= window {
            let sample = self.window_bytes as f64 / elapsed.as_secs_f64();
            self.media_rate = Some(match self.media_rate {
                Some(rate) => rate + RATE_ALPHA * (sample - rate),
                None => sample,
            });
            self.window_start = now;
            self.window_bytes = 0;
        }
    }

    /// The current pacing rate in bytes per second.
    pub fn rate(&self) -> f64 {
        self.media_rate
            .map_or(PACE_FLOOR, |rate| (rate * PACE_FACTOR).max(PACE_FLOOR))
    }

    /// Take the tokens for `bytes` if they are available.
    ///
    /// Returns `None` when the bytes may go now (the tokens are spent), or how
    /// long to wait before asking again.
    pub fn take(&mut self, bytes: usize, now: Instant) -> Option<Duration> {
        let rate = self.rate();
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate).min(PACE_BURST);
        self.refilled = now;
        // Never ask for more than the bucket holds, or the wait would not end.
        let bytes = (bytes as f64).min(PACE_BURST);
        if self.tokens >= bytes {
            self.tokens -= bytes;
            return None;
        }
        // Rounded up, so asking again after the wait always succeeds.
        let nanos = ((bytes - self.tokens) / rate * 1e9).ceil() as u64 + 1;
        Some(Duration::from_nanos(nanos))
    }
}

/// The congestion window of [`MediaCongestion`], in bytes.
///
/// Large enough never to bind: a 100 Mbit/s stream over a 500 ms round trip
/// keeps about 6 MB in flight.
pub const MEDIA_WINDOW: u64 = 16 * 1024 * 1024;

/// A QUIC congestion controller for a live media connection: a fixed window
/// that does not back off on loss.
///
/// A loss-based controller (QUIC's default Cubic) caps its rate at roughly
/// `1.2 * mss / (rtt * sqrt(loss))`: at 40 ms and 0.1% random loss that is about
/// 9 Mbit/s. The publisher keeps encoding at its own constant bitrate no matter
/// what QUIC decides, so a throttled connection does not slow the stream down,
/// it queues it: the datagram send buffer fills up (seconds of lag), then sheds
/// its oldest datagrams in bulk (the player's `RTP: missed 500 packets`).
///
/// The media is a constant-bitrate flow that [`Pacer`] already shapes to at most
/// [`PACE_FACTOR`] times its own rate, so this controller only lets it through:
/// like RTP over plain UDP, a stream the path cannot carry loses packets instead
/// of piling up latency.
#[derive(Debug, Clone, Copy, Default)]
pub struct MediaCongestion;

impl noq::congestion::Controller for MediaCongestion {
    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        _is_ecn: bool,
        _lost_bytes: u64,
        _largest_lost_pn: u64,
    ) {
    }

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        MEDIA_WINDOW
    }

    fn clone_box(&self) -> Box<dyn noq::congestion::Controller> {
        Box::new(*self)
    }

    fn initial_window(&self) -> u64 {
        MEDIA_WINDOW
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

impl noq::congestion::ControllerFactory for MediaCongestion {
    fn build(
        self: std::sync::Arc<Self>,
        _now: Instant,
        _current_mtu: u16,
    ) -> Box<dyn noq::congestion::Controller> {
        Box::new(*self)
    }
}
