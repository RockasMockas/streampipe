//! RTP framing for the media tunnel.
//!
//! The host side of [`crate::stream`] receives parsed RTP packets from str0m
//! (`Event::RtpPacket` hands out a parsed header and a headerless payload) and
//! forwards them verbatim to viewers over QUIC datagrams, so a viewer is a
//! plain UDP forwarder towards a local media player.
//!
//! Every datagram on the tunnel starts with a one byte tag:
//!
//! * [`TAG_SESSION`]: the session header, see [`crate::sdp::SessionHeader`].
//! * [`TAG_EPOCH`]: the next media packet is the start of a keyframe.
//! * [`TAG_VIDEO`]: a complete RTP packet for the video media.
//! * [`TAG_AUDIO`]: a complete RTP packet for the audio media.
//! * [`TAG_KEYFRAME_REQ`]: viewer to host, please ask the publisher for a
//!   keyframe.
//! * [`TAG_FRAGMENT`]: a fragment of a datagram that was too large for a
//!   single QUIC datagram, see [`fragment_all`] and [`Reassembler`].
//! * [`TAG_NACK`]: viewer to host, please resend these lost packets, see
//!   [`encode_nack`] and [`History`].
//!
//! RTP header extensions and CSRCs are dropped when re-framing, the generated
//! player SDP advertises neither.

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use data_encoding::{BASE64, HEXLOWER};

/// Session header, repeated periodically and on every change.
pub const TAG_SESSION: u8 = 0;
/// The next forwarded media packet starts a keyframe.
pub const TAG_EPOCH: u8 = 1;
/// An RTP packet of the video media.
pub const TAG_VIDEO: u8 = 2;
/// An RTP packet of the audio media.
pub const TAG_AUDIO: u8 = 3;
/// A viewer asking the host to request a keyframe from the publisher.
pub const TAG_KEYFRAME_REQ: u8 = 4;
/// A fragment of a datagram that did not fit into a single QUIC datagram.
///
/// The host fragments such a datagram instead of dropping it: on a local
/// network the path MTU is large and nothing is ever fragmented, but across a
/// continent QUIC's datagram limit settles near 1100 bytes and full-size RTP
/// packets would otherwise be lost on every frame.
pub const TAG_FRAGMENT: u8 = 5;
/// A viewer asking the host to resend lost RTP packets.
///
/// The framing is `tag, media, seq(2)...`, media being [`NACK_VIDEO`] or
/// [`NACK_AUDIO`] and the sequence numbers the RTP sequence numbers to resend.
pub const TAG_NACK: u8 = 6;
/// The [`TAG_NACK`] media byte for video.
pub const NACK_VIDEO: u8 = 0;
/// The [`TAG_NACK`] media byte for audio.
pub const NACK_AUDIO: u8 = 1;
/// The most sequence numbers in one [`TAG_NACK`] datagram, so it always fits
/// the smallest datagram limit QUIC allows.
pub const NACK_MAX: usize = 256;

/// Frame a NACK for `seqs` of one media, at most [`NACK_MAX`] of them.
pub fn encode_nack(media: u8, seqs: &[u16]) -> Vec<u8> {
    let seqs = &seqs[..seqs.len().min(NACK_MAX)];
    let mut buf = Vec::with_capacity(2 + 2 * seqs.len());
    buf.push(TAG_NACK);
    buf.push(media);
    for seq in seqs {
        buf.extend_from_slice(&seq.to_be_bytes());
    }
    buf
}

/// Parse a [`TAG_NACK`] datagram into its media byte and sequence numbers.
pub fn decode_nack(buf: &[u8]) -> Option<(u8, Vec<u16>)> {
    let (&tag, rest) = buf.split_first()?;
    let (&media, seqs) = rest.split_first()?;
    if tag != TAG_NACK || seqs.len() % 2 != 0 || seqs.len() / 2 > NACK_MAX {
        return None;
    }
    let (seqs, _) = seqs.as_chunks::<2>();
    Some((media, seqs.iter().map(|s| u16::from_be_bytes(*s)).collect()))
}

/// The packets the host keeps per media for resending, a power of two so the
/// slots line up across the 16 bit sequence number wrap.
///
/// About two seconds of video at 20 Mbit/s: far more than a NACK ever reaches
/// back, since a viewer gives up on a hole within half a second.
pub const HISTORY_VIDEO: usize = 4096;
/// See [`HISTORY_VIDEO`]; audio runs at 50 packets a second.
pub const HISTORY_AUDIO: usize = 512;

/// The recently forwarded RTP packets of one media, by sequence number, so a
/// viewer can ask for a lost one again ([`TAG_NACK`]).
pub struct History {
    slots: Vec<Option<(u16, bytes::Bytes)>>,
}

impl History {
    /// A history of `capacity` packets, which must be a power of two.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two() && capacity <= 1 << 16);
        History {
            slots: vec![None; capacity],
        }
    }

    /// Remember a forwarded packet, replacing the one `capacity` packets older.
    pub fn insert(&mut self, seq: u16, packet: bytes::Bytes) {
        let slot = seq as usize % self.slots.len();
        self.slots[slot] = Some((seq, packet));
    }

    /// The packet with this sequence number, if it is still remembered.
    pub fn get(&self, seq: u16) -> Option<bytes::Bytes> {
        match &self.slots[seq as usize % self.slots.len()] {
            Some((s, packet)) if *s == seq => Some(packet.clone()),
            _ => None,
        }
    }

    /// Forget everything: a new publisher session starts a new sequence space.
    pub fn clear(&mut self) {
        self.slots.iter_mut().for_each(|s| *s = None);
    }
}

/// The length of the fragment framing: tag, id, index, total.
pub const FRAG_HEADER_LEN: usize = 5;

/// How long a partial fragment set survives before it is discarded.
///
/// Realtime media that arrives later than this is useless anyway, and the
/// keyframe gate recovers the stream at the next IDR.
pub const FRAG_TIMEOUT: Duration = Duration::from_secs(2);

/// The number of datagrams the [`Reassembler`] tracks at once.
///
const FRAG_PENDING: usize = 1024;

/// Split a datagram that is too large into fragments that fit into `max`
/// bytes each.
///
/// Returns `None` if the datagram cannot be framed (an absurd `max`) or would
/// need more fragments than the one byte index can count.
pub fn fragment_all(buf: &[u8], id: u16, max: usize) -> Option<Vec<Vec<u8>>> {
    if max <= FRAG_HEADER_LEN {
        return None;
    }
    let chunk = max - FRAG_HEADER_LEN;
    let total = buf.len().div_ceil(chunk);
    if total == 0 || total > u8::MAX as usize {
        return None;
    }
    Some(
        buf.chunks(chunk)
            .enumerate()
            .map(|(i, part)| fragment(id, i as u8, total as u8, part))
            .collect(),
    )
}

/// Frame one fragment of a datagram.
///
/// The framing is `tag, id(2), index(1), total(1)` followed by the chunk. The
/// id groups the fragments of one datagram, so a reassembler can track several
/// datagrams at once and tolerate fragments arriving out of order.
pub fn fragment(id: u16, index: u8, total: u8, chunk: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(FRAG_HEADER_LEN + chunk.len());
    buf.push(TAG_FRAGMENT);
    buf.extend_from_slice(&id.to_be_bytes());
    buf.push(index);
    buf.push(total);
    buf.extend_from_slice(chunk);
    buf
}

/// The fragments collected for one datagram id.
struct Assembling {
    /// The number of fragments the datagram was split into.
    total: usize,
    /// The parts seen so far, `None` where a fragment is still missing.
    parts: Vec<Option<Vec<u8>>>,
    /// How many parts are filled.
    got: usize,
    /// When the first fragment of this datagram arrived.
    at: Instant,
}

/// Reassembles fragmented datagrams.
///
/// Fragments may arrive out of order and duplicates are ignored; a datagram is
/// yielded exactly once, when its last fragment arrives. Incomplete sets are
/// dropped after [`FRAG_TIMEOUT`] or when more than [`FRAG_PENDING`] datagrams
/// are being tracked: the missing fragments are then simply packet loss, and
/// the viewer's keyframe gate recovers from that.
#[derive(Default)]
pub struct Reassembler {
    pending: HashMap<u16, Assembling>,
}

impl Reassembler {
    /// Feed one fragment datagram (starting with [`TAG_FRAGMENT`]).
    ///
    /// Returns the complete datagram, including its original tag byte, when
    /// the last fragment of the datagram arrives.
    pub fn push(&mut self, data: &[u8], now: Instant) -> Option<Vec<u8>> {
        if data.len() < FRAG_HEADER_LEN || data[0] != TAG_FRAGMENT {
            return None;
        }
        let id = u16::from_be_bytes([data[1], data[2]]);
        let index = data[3] as usize;
        let total = data[4] as usize;
        if total == 0 || index >= total {
            return None;
        }
        self.evict(now);
        if !self.pending.contains_key(&id) && self.pending.len() >= FRAG_PENDING {
            self.drop_oldest();
        }
        let entry = self.pending.entry(id).or_insert_with(|| Assembling {
            total,
            parts: vec![None; total],
            got: 0,
            at: now,
        });
        if entry.total != total {
            // A reused id with a different framing, e.g. after the host's
            // MTU estimate changed: start over.
            *entry = Assembling {
                total,
                parts: vec![None; total],
                got: 0,
                at: now,
            };
        }
        if entry.parts[index].is_some() {
            return None;
        }
        entry.parts[index] = Some(data[FRAG_HEADER_LEN..].to_vec());
        entry.got += 1;
        if entry.got < total {
            return None;
        }
        let entry = self.pending.remove(&id)?;
        Some(entry.parts.into_iter().flatten().flatten().collect())
    }

    /// Discard all partial datagrams.
    ///
    /// Called at an epoch: fragments of the old GOP are useless now.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Drop entries that timed out.
    fn evict(&mut self, now: Instant) {
        self.pending
            .retain(|_, e| now.duration_since(e.at) < FRAG_TIMEOUT);
    }

    /// Drop the oldest entry to make room for a new one.
    fn drop_oldest(&mut self) {
        let oldest = self
            .pending
            .iter()
            .min_by_key(|(_, e)| e.at)
            .map(|(id, _)| *id);
        if let Some(id) = oldest {
            self.pending.remove(&id);
        }
    }
}

/// How long a [`Reorder`] holds a packet waiting for a missing earlier one,
/// used when the viewer runs with no player buffer.
///
/// QUIC datagrams are unordered, so the viewer must put RTP packets back in
/// sequence order for the player. This is how long it waits for a hole to be
/// filled before flushing and letting the player conceal the gap — long enough
/// to cover typical reordering, short enough that the stream never stalls.
/// With `--buffer` the window is derived from it instead; see
/// [`reorder_timeout_for`].
pub const REORDER_TIMEOUT: Duration = Duration::from_millis(80);

/// Floor and ceiling for the reorder window derived from a player buffer.
///
/// The window is a fraction of the buffer (see [`reorder_timeout_for`]), kept
/// within these bounds: never below what covers ordinary reordering, never so
/// long that we sit on holes that are really lost packets.
pub const REORDER_TIMEOUT_MIN: Duration = Duration::from_millis(40);
pub const REORDER_TIMEOUT_MAX: Duration = Duration::from_millis(150);

/// Derive the viewer's reorder window from the player buffer.
///
/// `--buffer` is the player's total jitter/reorder budget; our [`Reorder`] is
/// the small, fast stage in front of it. We take a quarter of the buffer,
/// clamped to [`REORDER_TIMEOUT_MIN`]..=[`REORDER_TIMEOUT_MAX`]: a bigger
/// buffer catches worse long-distance reordering without becoming a latency
/// trap (a hole unfilled after ~150 ms is a lost QUIC datagram). `None` keeps
/// the default [`REORDER_TIMEOUT`].
pub fn reorder_timeout_for(buffer: Option<Duration>) -> Duration {
    buffer
        .map(|b| (b / 4).clamp(REORDER_TIMEOUT_MIN, REORDER_TIMEOUT_MAX))
        .unwrap_or(REORDER_TIMEOUT)
}

/// The longest a [`Reorder`] holds a hole while resends may still fill it.
pub const REORDER_TIMEOUT_NACK_MAX: Duration = Duration::from_millis(500);

/// Slack on top of the round trip for a resend to arrive: timers, and the
/// host's pacer queue.
const NACK_SLACK: Duration = Duration::from_millis(20);

/// How long a [`Reorder`] holds a hole, and how long it waits for a resend
/// before asking again, on a path with round-trip time `rtt`.
///
/// A NACK is answered one round trip later, so a hole is asked for again after
/// 1.5 rtt and held for 2.5 rtt: long enough for the second resend to land
/// too. Never shorter than the window `--buffer` asks for (see
/// [`reorder_timeout_for`]), never longer than [`REORDER_TIMEOUT_NACK_MAX`].
pub fn nack_timing(buffer: Option<Duration>, rtt: Duration) -> (Duration, Duration) {
    let renack = rtt * 3 / 2 + NACK_SLACK;
    let timeout = (rtt * 5 / 2 + NACK_SLACK * 2)
        .max(reorder_timeout_for(buffer))
        .min(REORDER_TIMEOUT_NACK_MAX);
    (timeout, renack)
}

/// The most packets the [`Reorder`] holds per media at once.
const REORDER_CAP: usize = 8192;

/// How often a [`Reorder`] asks the host for one lost packet before giving up.
pub const NACK_TRIES: u8 = 2;

/// The most holes a [`Reorder`] asks the host to fill at once.
///
/// A gap wider than this is an outage or a new sequence space, not a few lost
/// packets: it is left to time out, and the keyframe request recovers the
/// picture.
pub const NACK_PENDING: usize = 1024;

/// How far behind the next expected sequence number a packet may be and still
/// count as late; anything older is a new sequence space (the publisher
/// reconnected) and resets the baseline.
const LATE_WINDOW: i16 = 1024;

/// How many abandoned holes a [`Reorder`] remembers, see [`Reorder::push`].
const ABANDONED_CAP: usize = 512;

/// Puts RTP packets back into sequence order, and finds the lost ones.
///
/// The host forwards one RTP packet per QUIC datagram, and datagrams are
/// unreliable *and unordered*, so packets reach the viewer scrambled even on a
/// local network. Handing them to a player in arrival order makes it read the
/// reordering as packet loss (`missed N packets`, a thrashing jitter buffer,
/// corrupt pictures). The viewer runs one `Reorder` per media (video and audio
/// have separate sequence spaces) and emits strictly in sequence order.
///
/// A packet that arrives when expected is emitted at once; a later one is held
/// until the hole before it is filled. Every hole is reported once by
/// [`Reorder::take_nacks`] so the viewer can ask the host to resend it, and
/// once more if the resend does not arrive in time. If a hole is not filled
/// within the configured timeout, everything held is flushed in order and the
/// hole is abandoned (genuine loss the player conceals).
pub struct Reorder {
    /// The next sequence number to emit, `None` until the first packet.
    next: Option<u16>,
    /// Held packets, `(seq, arrival, packet)`, out of order.
    held: Vec<(u16, Instant, Vec<u8>)>,
    /// How long to hold a hole before flushing. See [`Reorder::with_timeout`].
    timeout: Duration,
    /// The highest sequence number seen, so that each hole is noticed once.
    highest: Option<u16>,
    /// The holes, `(seq, when it was last asked for, how often)`.
    missing: Vec<(u16, Option<Instant>, u8)>,
    /// How long a resend may take before a hole is asked for again.
    renack: Duration,
    /// Holes a flush gave up on, so a very late packet for one of them can
    /// still be handed on, while a duplicate of an emitted packet is dropped.
    abandoned: VecDeque<u16>,
}

impl Default for Reorder {
    fn default() -> Self {
        Self {
            next: None,
            held: Vec::new(),
            timeout: REORDER_TIMEOUT,
            highest: None,
            missing: Vec::new(),
            renack: REORDER_TIMEOUT / 2,
            abandoned: VecDeque::new(),
        }
    }
}

impl Reorder {
    /// A reorder that waits `timeout` for a hole before flushing.
    ///
    /// Use [`reorder_timeout_for`] to size this from the player's `--buffer` so
    /// the two reorder stages stay in tandem.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            renack: timeout / 2,
            ..Default::default()
        }
    }

    /// Change how long a hole is held, and how long a resend may take before
    /// the hole is asked for again. See [`nack_timing`].
    pub fn set_timing(&mut self, timeout: Duration, renack: Duration) {
        self.timeout = timeout;
        self.renack = renack;
    }

    /// Feed one packet by its RTP sequence number.
    ///
    /// Returns the packets to hand to the player now: the packet itself if it
    /// closes the gap, plus any held packets it unblocks. A packet for a hole
    /// that was already abandoned is passed through untouched, so a downstream
    /// jitter buffer can still recover it; a duplicate of a packet that was
    /// already emitted (a resend racing a delayed original) is dropped.
    pub fn push(&mut self, seq: u16, packet: Vec<u8>, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some(next) = self.next else {
            // First packet: it is the baseline, emit it and wait for the next.
            self.next = Some(seq.wrapping_add(1));
            self.highest = Some(seq);
            out.push(packet);
            return out;
        };
        let ahead = seq.wrapping_sub(next) as i16;
        if ahead < -LATE_WINDOW {
            // Far older than anything still in flight: the sequence space
            // changed under us. Start over from this packet.
            self.clear();
            return self.push(seq, packet, now);
        }
        if ahead < 0 {
            if let Some(i) = self.abandoned.iter().position(|s| *s == seq) {
                self.abandoned.remove(i);
                out.push(packet);
            }
            return out;
        }
        self.note(seq);
        if ahead == 0 {
            out.push(packet);
            self.next = Some(next.wrapping_add(1));
            self.drain(&mut out);
        } else if !self.held.iter().any(|(s, _, _)| *s == seq) {
            // A future packet: hold it until the hole before it is filled.
            if self.held.len() >= REORDER_CAP {
                self.flush(&mut out);
            }
            self.held.push((seq, now, packet));
        }
        self.expire(now, &mut out);
        out
    }

    /// Flush on the timeout even when no packet arrives to trigger it.
    pub fn poll(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.expire(now, &mut out);
        out
    }

    /// The holes to ask the host to resend now.
    ///
    /// A new hole is returned on the first call after it appears, then again
    /// if its resend has not arrived within the renack interval, up to
    /// [`NACK_TRIES`] times in all.
    pub fn take_nacks(&mut self, now: Instant) -> Vec<u16> {
        let Some(next) = self.next else {
            return Vec::new();
        };
        // Forget the holes a flush has passed.
        self.missing
            .retain(|(s, _, _)| s.wrapping_sub(next) as i16 >= 0);
        let mut seqs = Vec::new();
        for (seq, asked, tries) in &mut self.missing {
            let due = asked.is_none_or(|at| now.duration_since(at) >= self.renack);
            if *tries < NACK_TRIES && due {
                *asked = Some(now);
                *tries += 1;
                seqs.push(*seq);
            }
        }
        seqs
    }

    /// Forget everything: called at an epoch, where the sequence baseline resets.
    pub fn clear(&mut self) {
        self.next = None;
        self.held.clear();
        self.highest = None;
        self.missing.clear();
        self.abandoned.clear();
    }

    /// Track the holes `seq` opens or fills.
    fn note(&mut self, seq: u16) {
        let Some(highest) = self.highest else {
            self.highest = Some(seq);
            return;
        };
        let jump = seq.wrapping_sub(highest) as i16;
        if jump <= 0 {
            // At or below the highest seen: it may fill a hole.
            self.missing.retain(|(s, _, _)| *s != seq);
            return;
        }
        self.highest = Some(seq);
        let gap = jump as usize - 1;
        if gap > 0 && self.missing.len() + gap <= NACK_PENDING {
            let first = highest.wrapping_add(1);
            self.missing
                .extend((0..gap as u16).map(|i| (first.wrapping_add(i), None, 0)));
        }
    }

    /// Emit held packets that are now consecutive with the next expected.
    fn drain(&mut self, out: &mut Vec<Vec<u8>>) {
        while let Some(next) = self.next {
            match self.held.iter().position(|(s, _, _)| *s == next) {
                Some(i) => {
                    let (s, _, p) = self.held.remove(i);
                    out.push(p);
                    self.next = Some(s.wrapping_add(1));
                }
                None => break,
            }
        }
    }

    /// Flush everything held if the oldest has waited past the timeout, so a
    /// lost packet can never stall the stream.
    fn expire(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        if self
            .held
            .iter()
            .map(|(_, at, _)| now.duration_since(*at))
            .max()
            .is_some_and(|wait| wait >= self.timeout)
        {
            self.flush(out);
        }
    }

    /// Release all held packets in sequence order, abandoning the holes.
    fn flush(&mut self, out: &mut Vec<Vec<u8>>) {
        let mut expect = self.next.unwrap_or(0);
        self.held
            .sort_by_key(|(s, _, _)| s.wrapping_sub(expect) as i16);
        for (s, _, p) in std::mem::take(&mut self.held) {
            let gap = (s.wrapping_sub(expect) as i16).max(0) as usize;
            for i in 0..gap.min(ABANDONED_CAP) {
                if self.abandoned.len() >= ABANDONED_CAP {
                    self.abandoned.pop_front();
                }
                self.abandoned.push_back(expect.wrapping_add(i as u16));
            }
            expect = s.wrapping_add(1);
            out.push(p);
        }
        self.next = Some(expect);
    }
}

/// The length of a fixed RTP header without extensions or CSRCs.
pub const RTP_HEADER_LEN: usize = 12;

/// The fields of an RTP header that we preserve when re-framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpInfo {
    /// RTP marker bit.
    pub marker: bool,
    /// Payload type.
    pub pt: u8,
    /// 16 bit sequence number as it appears on the wire.
    pub seq: u16,
    /// Media timestamp in the clock rate of the codec.
    pub timestamp: u32,
    /// Synchronization source identifier.
    pub ssrc: u32,
}

/// Serialize a minimal, self-contained RTP packet.
///
/// The header is written as `V=2, P=0, X=0, CC=0` plus the marker bit, payload
/// type, sequence number, timestamp and ssrc, followed by the payload. Header
/// extensions and CSRCs are deliberately not represented.
pub fn serialize_rtp(info: &RtpInfo, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
    buf.push(0x80);
    buf.push((if info.marker { 0x80 } else { 0 }) | (info.pt & 0x7f));
    buf.extend_from_slice(&info.seq.to_be_bytes());
    buf.extend_from_slice(&info.timestamp.to_be_bytes());
    buf.extend_from_slice(&info.ssrc.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Parse the fixed part of an RTP header.
///
/// Returns the header fields and the offset of the payload, so a receiver can
/// route the packet by payload type and detect sequence number gaps. Header
/// extensions and CSRCs are skipped, padding is left in the payload.
///
/// Returns `None` if the buffer is not a version 2 RTP packet.
pub fn parse_header(buf: &[u8]) -> Option<(RtpInfo, usize)> {
    if buf.len() < RTP_HEADER_LEN || buf[0] >> 6 != 2 {
        return None;
    }
    let has_extension = buf[0] & 0x10 != 0;
    let csrc_count = (buf[0] & 0x0f) as usize;
    let mut offset = RTP_HEADER_LEN + csrc_count * 4;
    if offset > buf.len() {
        return None;
    }
    if has_extension {
        if offset + 4 > buf.len() {
            return None;
        }
        let words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        offset += 4 + words * 4;
        if offset > buf.len() {
            return None;
        }
    }
    let info = RtpInfo {
        marker: buf[1] & 0x80 != 0,
        pt: buf[1] & 0x7f,
        seq: u16::from_be_bytes([buf[2], buf[3]]),
        timestamp: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        ssrc: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
    };
    Some((info, offset))
}

/// Prefix a payload with a tunnel tag.
pub fn tagged(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(tag);
    buf.extend_from_slice(payload);
    buf
}

/// The codecs we know how to detect keyframes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264 / AVC.
    H264,
    /// VP8.
    Vp8,
    /// Opus.
    Opus,
    /// Anything else, treated as always decodable.
    Other,
}

impl Codec {
    /// Map an SDP codec name to a [`Codec`].
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Codec::H264,
            "vp8" => Codec::Vp8,
            "opus" => Codec::Opus,
            _ => Codec::Other,
        }
    }
}

/// The H.264 parameter sets seen in a stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParameterSets {
    /// The most recent SPS, including the NAL header byte.
    pub sps: Option<Vec<u8>>,
    /// The most recent PPS, including the NAL header byte.
    pub pps: Option<Vec<u8>>,
}

impl ParameterSets {
    /// Whether both parameter sets are known.
    pub fn complete(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }

    /// Render the parameter sets as an SDP `sprop-parameter-sets` value.
    ///
    /// The NAL units are base64 encoded, separated by a comma, exactly as they
    /// appear on the wire (emulation prevention bytes included).
    pub fn to_sprop(&self) -> Option<String> {
        let sps = self.sps.as_deref()?;
        let pps = self.pps.as_deref()?;
        Some(format!("{},{}", BASE64.encode(sps), BASE64.encode(pps)))
    }

    /// Parse an `sprop-parameter-sets` value given as hex encoded bytes.
    ///
    /// The tunnel transports the parameter sets as hex to keep the session
    /// header free of base64 punctuation.
    pub fn from_sprop_hex(sps_hex: &str, pps_hex: &str) -> Self {
        let set = |hex: &str| -> Option<Vec<u8>> {
            if hex.is_empty() || hex == "-" {
                None
            } else {
                HEXLOWER.decode(hex.as_bytes()).ok()
            }
        };
        ParameterSets {
            sps: set(sps_hex),
            pps: set(pps_hex),
        }
    }

    /// The tunnel representation of the parameter sets.
    pub fn to_sprop_hex(&self) -> String {
        format!(
            "{}:{}",
            self.sps
                .as_deref()
                .map(|b| HEXLOWER.encode(b))
                .unwrap_or_default(),
            self.pps
                .as_deref()
                .map(|b| HEXLOWER.encode(b))
                .unwrap_or_default(),
        )
    }
}

/// H.264 nal unit types we care about.
pub mod nal {
    /// Coded slice of an IDR picture.
    pub const IDR: u8 = 5;
    /// Sequence parameter set.
    pub const SPS: u8 = 7;
    /// Picture parameter set.
    pub const PPS: u8 = 8;
    /// Single-time aggregation packet, type A.
    pub const STAP_A: u8 = 24;
    /// Fragmentation unit, type A.
    pub const FU_A: u8 = 28;
}

/// The nal unit type of a nal unit header byte.
fn nal_type(byte: u8) -> u8 {
    byte & 0x1f
}

/// Iterate over the nal units of an H.264 RTP payload.
///
/// Handles single nal units and STAP-A aggregation packets. FU-A fragments are
/// passed through as they are, they are handled by
/// [`h264_starts_keyframe`] directly.
fn for_each_nal(payload: &[u8], f: &mut impl FnMut(&[u8])) {
    let Some((&first, rest)) = payload.split_first() else {
        return;
    };
    if nal_type(first) != nal::STAP_A {
        f(payload);
        return;
    }
    let mut i = 0;
    while i + 2 <= rest.len() {
        let len = u16::from_be_bytes([rest[i], rest[i + 1]]) as usize;
        let start = i + 2;
        let end = start + len;
        if end > rest.len() {
            break;
        }
        f(&rest[start..end]);
        i = end;
    }
}

/// Whether an H.264 payload starts an independently decodable picture.
///
/// This is the case if the payload contains an SPS (parameter sets are repeated
/// at the start of every GOP by the encoders we target) or the first slice of
/// an IDR picture.
pub fn h264_starts_keyframe(payload: &[u8]) -> bool {
    let mut keyframe = false;
    for_each_nal(payload, &mut |nal| {
        let Some((&first, _)) = nal.split_first() else {
            return;
        };
        match nal_type(first) {
            nal::SPS | nal::IDR => keyframe = true,
            _ => {}
        }
    });
    if keyframe {
        return true;
    }
    // A fragmented IDR: the first fragment of an IDR nal unit.
    if let Some(&first) = payload.first() {
        if nal_type(first) == nal::FU_A && payload.len() > 1 {
            let fu = payload[1];
            return fu & 0x80 != 0 && nal_type(fu) == nal::IDR;
        }
    }
    false
}

/// Collect SPS and PPS nal units from an H.264 RTP payload.
pub fn scan_h264_parameter_sets(payload: &[u8], sets: &mut ParameterSets) {
    for_each_nal(payload, &mut |nal| {
        let Some((&first, _)) = nal.split_first() else {
            return;
        };
        match nal_type(first) {
            nal::SPS => sets.sps = Some(nal.to_vec()),
            nal::PPS => sets.pps = Some(nal.to_vec()),
            _ => {}
        }
    });
}

/// Whether a VP8 payload starts an independently decodable frame.
///
/// The descriptor payload type bit is not used, we only look at the partition
/// structure: a keyframe has `S=1` and `PID=0` in the 1 byte descriptor, and
/// the inverse keyframe flag in the extended descriptor must be unset.
pub fn vp8_starts_keyframe(payload: &[u8]) -> bool {
    let Some(&first) = payload.first() else {
        return false;
    };
    // Bit 0 of the first descriptor octet is the inverse keyframe flag.
    let keyframe = first & 0x01 == 0;
    let extended = first & 0x80 != 0;
    if !extended {
        return keyframe;
    }
    // With X=1 there is one more descriptor octet, and the extended control
    // bits may add a picture id. None of that changes the keyframe flag.
    keyframe
}

/// Whether a payload starts an independently decodable access unit.
///
/// Used by the host to gate the media it forwards to a viewer: a viewer that
/// joins in the middle of a GOP only starts receiving at a keyframe, so the
/// player never has to decode a GOP tail without parameter sets.
pub fn starts_keyframe(payload: &[u8], codec: Codec) -> bool {
    match codec {
        Codec::H264 => h264_starts_keyframe(payload),
        Codec::Vp8 => vp8_starts_keyframe(payload),
        // Opus frames are independently decodable, and unknown codecs are
        // passed through ungated rather than stalling the stream.
        Codec::Opus | Codec::Other => true,
    }
}
