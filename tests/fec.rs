//! Forward error correction for Aggressive Low Latency viewers.
//!
//! The host deals the packets of each video frame into interleaved groups and
//! sends one XOR parity packet per group; the viewer rebuilds a lost packet
//! once the rest of its group is in. These check that a rebuild is exact, that
//! it happens whatever order parity and packets arrive in, and that parity
//! never turns the wrong packets into "video".
use std::time::{Duration, Instant};

use bytes::Bytes;
use streampipe::{
    fec::{encode, Block, FecDecoder, FecEncoder, FEC_GROUP, FEC_MAX_BLOCK},
    rtp::{self, RtpInfo},
};

/// An RTP packet whose length and contents vary with its sequence number.
fn packet(seq: u16, ssrc: u32) -> Vec<u8> {
    let info = RtpInfo {
        marker: false,
        pt: 96,
        seq,
        timestamp: 90_000,
        ssrc,
    };
    let len = 200 + (seq as usize * 37) % 1000;
    let payload: Vec<u8> = (0..len).map(|i| (i as u16 ^ seq) as u8).collect();
    rtp::serialize_rtp(&info, &payload)
}

/// A block of `n` consecutive packets from `base`, as the host would frame it.
fn block(base: u16, n: u16) -> Block {
    Block {
        base,
        ssrc: 7,
        packets: (0..n)
            .map(|i| Bytes::from(packet(base.wrapping_add(i), 7)))
            .collect(),
    }
}

/// Deliver a block with the given sequence numbers lost, then its parity.
/// Returns what the decoder rebuilt, sorted by sequence number.
fn deliver(block: &Block, lost: &[u16]) -> Vec<(u16, Vec<u8>)> {
    let mut decoder = FecDecoder::default();
    let now = Instant::now();
    let mut rebuilt = Vec::new();
    for (i, p) in block.packets.iter().enumerate() {
        let seq = block.base.wrapping_add(i as u16);
        if !lost.contains(&seq) {
            rebuilt.extend(decoder.on_packet(seq, p, now));
        }
    }
    for parity in encode(block) {
        rebuilt.extend(decoder.on_parity(&parity, now));
    }
    rebuilt.sort_by_key(|(seq, _)| seq.wrapping_sub(block.base));
    rebuilt
}

#[test]
fn one_parity_per_group_of_ten() {
    assert_eq!(encode(&block(0, 1)).len(), 1);
    assert_eq!(encode(&block(0, FEC_GROUP as u16)).len(), 1);
    assert_eq!(encode(&block(0, FEC_GROUP as u16 + 1)).len(), 2);
    assert_eq!(encode(&block(0, 300)).len(), 30);
}

#[test]
fn a_lost_packet_is_rebuilt_exactly() {
    let b = block(1000, 37);
    for lost in [1000, 1013, 1036] {
        let rebuilt = deliver(&b, &[lost]);
        assert_eq!(
            rebuilt,
            vec![(lost, packet(lost, 7))],
            "lost {lost}: rebuilt bytes must be the original's, whatever its length"
        );
    }
}

#[test]
fn a_burst_up_to_the_group_count_is_rebuilt() {
    // 300 packets, 30 groups: 30 consecutive losses are one per group.
    let b = block(5000, 300);
    let lost: Vec<u16> = (5100..5130).collect();
    let rebuilt = deliver(&b, &lost);
    let want: Vec<(u16, Vec<u8>)> = lost.iter().map(|&s| (s, packet(s, 7))).collect();
    assert_eq!(rebuilt, want);
}

#[test]
fn two_losses_in_one_group_wait_for_a_resend() {
    // 20 packets, 2 groups: 0 and 2 share group 0.
    let b = block(0, 20);
    let mut decoder = FecDecoder::default();
    let now = Instant::now();
    for seq in (0..20u16).filter(|s| *s != 0 && *s != 2) {
        assert!(decoder
            .on_packet(seq, &b.packets[seq as usize], now)
            .is_empty());
    }
    let parities = encode(&b);
    assert_eq!(decoder.on_parity(&parities[0], now), None, "two missing");
    assert_eq!(decoder.on_parity(&parities[1], now), None, "none missing");
    // A resend of one of them arrives: the other is rebuilt at once.
    assert_eq!(
        decoder.on_packet(2, &b.packets[2], now + Duration::from_millis(30)),
        vec![(0, packet(0, 7))]
    );
}

#[test]
fn parity_ahead_of_its_packets_waits_for_them() {
    let b = block(70, 12);
    let mut decoder = FecDecoder::default();
    let now = Instant::now();
    // The parity overtakes the whole frame; 75 is lost.
    for parity in encode(&b) {
        assert_eq!(decoder.on_parity(&parity, now), None);
    }
    let mut rebuilt = Vec::new();
    for seq in (70..82u16).filter(|s| *s != 75) {
        rebuilt.extend(decoder.on_packet(seq, &b.packets[(seq - 70) as usize], now));
    }
    // A packet still on its way may be rebuilt first; whatever is rebuilt is exact.
    assert!(rebuilt.contains(&(75, packet(75, 7))), "{rebuilt:?}");
    for (seq, bytes) in &rebuilt {
        assert_eq!(bytes, &packet(*seq, 7));
    }
}

#[test]
fn parity_survives_the_sequence_wrap() {
    let b = block(65530, 15);
    // 15 packets, 2 groups: 65534 (offset 4) and 1 (offset 7) are in different ones.
    let rebuilt = deliver(&b, &[65534, 1]);
    assert_eq!(
        rebuilt,
        vec![(65534, packet(65534, 7)), (1, packet(1, 7))],
        "one loss on each side of the wrap, in different groups"
    );
}

#[test]
fn parity_never_rebuilds_from_another_stream() {
    // The publisher reconnected and reused the sequence numbers under a new
    // ssrc: the decoder still holds the old stream's packets for them. The
    // XOR of the wrong packets must not be handed on as video.
    let mut decoder = FecDecoder::default();
    let now = Instant::now();
    for seq in (0..10u16).filter(|s| *s != 3) {
        decoder.on_packet(seq, &packet(seq, 1111), now);
    }
    let fresh = Block {
        base: 0,
        ssrc: 2222,
        packets: (0..10).map(|s| Bytes::from(packet(s, 2222))).collect(),
    };
    for parity in encode(&fresh) {
        assert_eq!(decoder.on_parity(&parity, now), None);
    }
}

#[test]
fn stale_packets_do_not_count() {
    let b = block(0, 10);
    let mut decoder = FecDecoder::default();
    let t0 = Instant::now();
    for seq in 1..10u16 {
        decoder.on_packet(seq, &b.packets[seq as usize], t0);
    }
    // Long after: the members are too old to trust.
    let later = t0 + Duration::from_secs(5);
    assert_eq!(decoder.on_parity(&encode(&b)[0], later), None);
}

#[test]
fn malformed_parity_is_ignored() {
    let mut decoder = FecDecoder::default();
    let now = Instant::now();
    assert_eq!(decoder.on_parity(&[], now), None);
    assert_eq!(decoder.on_parity(&[0, 0, 0, 1, 1], now), None, "too short");
    assert_eq!(
        decoder.on_parity(&[0, 0, 0, 0, 1, 0, 0, 0], now),
        None,
        "an empty block"
    );
    assert_eq!(
        decoder.on_parity(&[0, 0, 0, 5, 0, 0, 0, 0], now),
        None,
        "no groups"
    );
    assert_eq!(
        decoder.on_parity(&[0, 0, 0, 5, 2, 2, 0, 0], now),
        None,
        "a group index past the group count"
    );
}

/// The header of video packet `seq`, ending a frame if `marker`.
fn hdr(seq: u16, marker: bool) -> RtpInfo {
    RtpInfo {
        marker,
        pt: 96,
        seq,
        timestamp: 90_000,
        ssrc: 7,
    }
}

#[test]
fn the_encoder_frames_a_block_per_frame() {
    let mut enc = FecEncoder::default();
    let p = |seq: u16| Bytes::from(packet(seq, 7));
    assert!(enc.push(&hdr(10, false), p(10)).is_none());
    assert!(enc.push(&hdr(11, false), p(11)).is_none());
    let b = enc
        .push(&hdr(12, true), p(12))
        .expect("the marker ends the frame");
    assert_eq!((b.base, b.ssrc), (10, 7));
    assert_eq!(b.packets.len(), 3);
    // The next frame starts a new block.
    let b = enc.push(&hdr(13, true), p(13)).expect("a one-packet frame");
    assert_eq!((b.base, b.packets.len()), (13, 1));
}

#[test]
fn the_encoder_drops_a_block_with_a_hole() {
    // The publisher's leg lost 22: parity over 20..=24 would make a viewer
    // "rebuild" 22 from the wrong members, so the broken part goes unprotected.
    let mut enc = FecEncoder::default();
    let p = |seq: u16| Bytes::from(packet(seq, 7));
    assert!(enc.push(&hdr(20, false), p(20)).is_none());
    assert!(enc.push(&hdr(21, false), p(21)).is_none());
    assert!(enc.push(&hdr(23, false), p(23)).is_none());
    let b = enc.push(&hdr(24, true), p(24)).expect("a block");
    assert_eq!((b.base, b.packets.len()), (23, 2));
}

#[test]
fn the_encoder_splits_a_huge_frame() {
    let mut enc = FecEncoder::default();
    let mut blocks = Vec::new();
    for seq in 0..(FEC_MAX_BLOCK as u16 + 5) {
        let marker = seq == FEC_MAX_BLOCK as u16 + 4;
        blocks.extend(enc.push(&hdr(seq, marker), Bytes::from_static(b"x")));
    }
    let sizes: Vec<usize> = blocks.iter().map(|b| b.packets.len()).collect();
    assert_eq!(sizes, vec![FEC_MAX_BLOCK, 5]);
    assert_eq!(blocks[1].base, FEC_MAX_BLOCK as u16);
}
