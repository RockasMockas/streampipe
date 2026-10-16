//! Forward error correction for the video of an Aggressive Low Latency viewer.
//!
//! A resend ([`crate::rtp::TAG_NACK`]) costs a round trip, which a low latency
//! viewer can only afford on a short path: it plays with no buffer, so a packet
//! that arrives after its hole was given up on is useless.
//!
//!  This fec scheme is plain XOR parity, interleaved within a frame. The `n` RTP
//! packets of a frame (a [`Block`] of consecutive sequence numbers, ended by
//! the marker bit, at most [`FEC_MAX_BLOCK`] packets) are dealt into
//! `m = ceil(n / FEC_GROUP)` groups, packet `j` into group `j % m`, and each
//! group gets one parity packet: the XOR of its members, each prefixed with its
//! length and zero padded to the longest. Any loss with at most one packet
//! missing per group is repaired, so a burst of up to `m` consecutive packets
//! is too, for about `1 / FEC_GROUP` more video on the wire.

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::rtp::{self, RtpInfo};

/// How many packets one parity packet protects: about 10% more video.
pub const FEC_GROUP: usize = 10;
/// The most packets in one [`Block`]; a frame larger than this (a keyframe at
/// a very high bitrate) is protected as several blocks.
pub const FEC_MAX_BLOCK: usize = 1000;
/// The length of the parity framing after the tag: base, count, groups,
/// index, ssrc.
pub const FEC_HEADER_LEN: usize = 10;

/// The consecutive video packets of one frame, ready for [`encode`].
#[derive(Debug, Clone)]
pub struct Block {
    /// The sequence number of the first packet.
    pub base: u16,
    /// The RTP stream all the packets belong to.
    pub ssrc: u32,
    /// The packets, in sequence order from `base`.
    pub packets: Vec<Bytes>,
}

/// Collects the video packets the host forwards into [`Block`]s.
#[derive(Debug, Default)]
pub struct FecEncoder {
    base: u16,
    ssrc: u32,
    packets: Vec<Bytes>,
}

impl FecEncoder {
    /// Add a forwarded video packet, `info` being its header.
    ///
    /// Returns the block it completes: at the marker bit that ends a frame, or
    /// when the block is full.
    pub fn push(&mut self, info: &RtpInfo, packet: Bytes) -> Option<Block> {
        let next = self.base.wrapping_add(self.packets.len() as u16);
        if !self.packets.is_empty() && (info.seq != next || info.ssrc != self.ssrc) {
            // A gap or a reordered packet on the publisher's leg: the block no
            // longer maps onto consecutive sequence numbers of one stream.
            // Leave it unprotected rather than send parity a viewer would
            // rebuild wrong.
            self.packets.clear();
        }
        if self.packets.is_empty() {
            self.base = info.seq;
            self.ssrc = info.ssrc;
        }
        self.packets.push(packet);
        if info.marker || self.packets.len() >= FEC_MAX_BLOCK {
            return Some(Block {
                base: self.base,
                ssrc: self.ssrc,
                packets: std::mem::take(&mut self.packets),
            });
        }
        None
    }

    /// Forget the frame in progress: a new publisher session starts a new
    /// sequence space.
    pub fn clear(&mut self) {
        self.packets.clear();
    }
}

/// The parity packets of a block, without the tag.
pub fn encode(block: &Block) -> Vec<Vec<u8>> {
    let n = block.packets.len().min(FEC_MAX_BLOCK);
    if n == 0 {
        return Vec::new();
    }
    let groups = n.div_ceil(FEC_GROUP);
    (0..groups)
        .map(|g| {
            let members = || (g..n).step_by(groups).map(|j| &block.packets[j]);
            let longest = members().map(|p| p.len()).max().unwrap_or(0);
            let mut buf = Vec::with_capacity(FEC_HEADER_LEN + 2 + longest);
            buf.extend_from_slice(&block.base.to_be_bytes());
            buf.extend_from_slice(&(n as u16).to_be_bytes());
            buf.push(groups as u8);
            buf.push(g as u8);
            buf.extend_from_slice(&block.ssrc.to_be_bytes());
            buf.resize(FEC_HEADER_LEN + 2 + longest, 0);
            for packet in members() {
                xor_into(&mut buf[FEC_HEADER_LEN..], packet);
            }
            buf
        })
        .collect()
}

/// XOR `len(2) ++ packet` into `area`, which must be at least that long.
fn xor_into(area: &mut [u8], packet: &[u8]) {
    let len = (packet.len() as u16).to_be_bytes();
    area[0] ^= len[0];
    area[1] ^= len[1];
    for (a, b) in area[2..].iter_mut().zip(packet) {
        *a ^= b;
    }
}

/// How many received video packets a [`FecDecoder`] keeps to rebuild from, a
/// power of two well above the span of a block.
const RECENT: usize = 4096;
/// How long a received packet counts towards a rebuild. Anything older is
/// past every hole the viewer still holds open, and must never be mistaken for
/// a packet of the current stream after the publisher reconnects.
const RECENT_TTL: Duration = Duration::from_secs(2);
/// How long a parity packet that cannot rebuild anything yet waits for the
/// rest of its group.
const PARITY_TTL: Duration = Duration::from_millis(500);
/// The most parity packets a [`FecDecoder`] waits on at once.
const PARITY_CAP: usize = 256;

/// One received packet, by sequence number modulo [`RECENT`].
struct Slot {
    seq: u16,
    /// When it arrived; `None` for an empty slot.
    at: Option<Instant>,
    packet: Vec<u8>,
}

/// A parity packet, parsed.
struct Parity {
    base: u16,
    count: u16,
    groups: u16,
    index: u16,
    ssrc: u32,
    /// The XOR of the members' `len(2) ++ packet`.
    xor: Vec<u8>,
    /// When it arrived.
    at: Instant,
}

impl Parity {
    fn parse(buf: &[u8], now: Instant) -> Option<Self> {
        if buf.len() < FEC_HEADER_LEN + 2 {
            return None;
        }
        let count = u16::from_be_bytes([buf[2], buf[3]]);
        let groups = buf[4] as u16;
        let index = buf[5] as u16;
        if count == 0 || count as usize > FEC_MAX_BLOCK || groups == 0 || index >= groups {
            return None;
        }
        Some(Parity {
            base: u16::from_be_bytes([buf[0], buf[1]]),
            count,
            groups,
            index,
            ssrc: u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]),
            xor: buf[FEC_HEADER_LEN..].to_vec(),
            at: now,
        })
    }

    /// Whether `seq` is a member of this parity's group.
    fn covers(&self, seq: u16) -> bool {
        let offset = seq.wrapping_sub(self.base);
        offset < self.count && offset % self.groups == self.index
    }

    /// The sequence numbers of the group's members.
    fn members(&self) -> impl Iterator<Item = u16> + '_ {
        (self.index..self.count)
            .step_by(self.groups as usize)
            .map(|j| self.base.wrapping_add(j))
    }
}

/// What a parity packet can do with the packets received so far.
enum Outcome {
    /// Exactly one member was missing, here it is.
    Rebuilt(u16, Vec<u8>),
    /// Nothing to do: every member arrived, or the parity does not fit them.
    Done,
    /// Two or more members are still missing.
    Waiting,
}

/// Rebuilds lost video packets from the host's parity, see the module docs.
pub struct FecDecoder {
    slots: Vec<Slot>,
    /// Parity packets with two or more members still missing.
    pending: Vec<Parity>,
}

impl Default for FecDecoder {
    fn default() -> Self {
        FecDecoder {
            slots: (0..RECENT)
                .map(|_| Slot {
                    seq: 0,
                    at: None,
                    packet: Vec::new(),
                })
                .collect(),
            pending: Vec::new(),
        }
    }
}

impl FecDecoder {
    /// Note a received video packet.
    ///
    /// Returns the packets it lets a waiting parity packet rebuild.
    pub fn on_packet(&mut self, seq: u16, packet: &[u8], now: Instant) -> Vec<(u16, Vec<u8>)> {
        self.store(seq, packet, now);
        self.pending
            .retain(|p| now.duration_since(p.at) < PARITY_TTL);
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if !self.pending[i].covers(seq) {
                i += 1;
                continue;
            }
            match self.attempt(&self.pending[i], now) {
                Outcome::Waiting => i += 1,
                outcome => {
                    self.pending.swap_remove(i);
                    if let Outcome::Rebuilt(seq, packet) = outcome {
                        self.store(seq, &packet, now);
                        out.push((seq, packet));
                    }
                }
            }
        }
        out
    }

    /// Take a parity packet, without its tag.
    ///
    /// Returns the packet it rebuilds when exactly one member of its group is
    /// missing; with more missing it waits for them, see [`Self::on_packet`].
    pub fn on_parity(&mut self, buf: &[u8], now: Instant) -> Option<(u16, Vec<u8>)> {
        let parity = Parity::parse(buf, now)?;
        match self.attempt(&parity, now) {
            Outcome::Rebuilt(seq, packet) => {
                self.store(seq, &packet, now);
                Some((seq, packet))
            }
            Outcome::Done => None,
            Outcome::Waiting => {
                if self.pending.len() >= PARITY_CAP {
                    self.pending.remove(0);
                }
                self.pending.push(parity);
                None
            }
        }
    }

    /// Forget everything: the stream starts over at a keyframe.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            slot.at = None;
        }
        self.pending.clear();
    }

    fn store(&mut self, seq: u16, packet: &[u8], now: Instant) {
        let slot = &mut self.slots[seq as usize % RECENT];
        slot.seq = seq;
        slot.at = Some(now);
        slot.packet.clear();
        slot.packet.extend_from_slice(packet);
    }

    /// The received packet `seq` of stream `ssrc`, if it is recent.
    fn get(&self, seq: u16, ssrc: u32, now: Instant) -> Option<&[u8]> {
        let slot = &self.slots[seq as usize % RECENT];
        let fresh = slot
            .at
            .is_some_and(|at| now.duration_since(at) < RECENT_TTL);
        let ours = slot.packet.get(8..12) == Some(&ssrc.to_be_bytes()[..]);
        (slot.seq == seq && fresh && ours).then_some(&slot.packet[..])
    }

    fn attempt(&self, parity: &Parity, now: Instant) -> Outcome {
        let mut lost = None;
        for seq in parity.members() {
            if self.get(seq, parity.ssrc, now).is_none() {
                if lost.is_some() {
                    return Outcome::Waiting;
                }
                lost = Some(seq);
            }
        }
        let Some(lost) = lost else {
            return Outcome::Done;
        };
        let mut xor = parity.xor.clone();
        for seq in parity.members().filter(|s| *s != lost) {
            let Some(packet) = self.get(seq, parity.ssrc, now) else {
                return Outcome::Done;
            };
            if packet.len() + 2 > xor.len() {
                // Longer than anything the parity covers: not its member.
                return Outcome::Done;
            }
            xor_into(&mut xor, packet);
        }
        let len = u16::from_be_bytes([xor[0], xor[1]]) as usize;
        if len + 2 > xor.len() {
            return Outcome::Done;
        }
        let packet = xor[2..2 + len].to_vec();
        // Check the result: the wrong packets in (a block the host framed
        // differently) is garbage out, and garbage must never reach the
        // player as video.
        match rtp::parse_header(&packet) {
            Some((info, _)) if info.seq == lost && info.ssrc == parity.ssrc => {
                Outcome::Rebuilt(lost, packet)
            }
            _ => Outcome::Done,
        }
    }
}
