//! Codec classification: which RTP payload starts a decodable frame, and the
//! H.264 parameter sets a player needs before it can decode one.
//!
//! The host gates every viewer on these answers (keyframe gate + `sprop` in the
//! session header), so a wrong answer is a grey or corrupt picture downstream.

use streampipe::rtp::{
    h264_starts_keyframe, nal, scan_h264_parameter_sets, starts_keyframe, vp8_starts_keyframe,
    Codec, ParameterSets,
};

/// An H.264 nal unit of type `ty` with the given body.
fn nal_unit(ty: u8, body: &[u8]) -> Vec<u8> {
    let mut buf = vec![0x60 | ty];
    buf.extend_from_slice(body);
    buf
}

#[test]
fn h264_single_idr_is_keyframe() {
    assert!(h264_starts_keyframe(&nal_unit(nal::IDR, b"x")));
    assert!(!h264_starts_keyframe(&nal_unit(1, b"x")));
    assert!(h264_starts_keyframe(&nal_unit(nal::SPS, b"x")));
    assert!(!h264_starts_keyframe(&nal_unit(nal::PPS, b"x")));
}

#[test]
fn h264_stap_a_is_classified() {
    let sps = nal_unit(nal::SPS, b"abc");
    let pps = nal_unit(nal::PPS, b"de");
    let idr = nal_unit(nal::IDR, b"frame");
    let mut buf = vec![nal::STAP_A];
    for nal in [&sps, &pps, &idr] {
        buf.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        buf.extend_from_slice(nal);
    }
    assert!(h264_starts_keyframe(&buf));

    let mut buf = vec![nal::STAP_A];
    let slice = nal_unit(1, b"inter");
    buf.extend_from_slice(&(slice.len() as u16).to_be_bytes());
    buf.extend_from_slice(&slice);
    assert!(!h264_starts_keyframe(&buf));
}

#[test]
fn h264_fu_a_start_only() {
    // start of an IDR fragment
    let start = vec![0x7c /* fu-a */, 0x85 /* S=1, type 5 */, 1, 2, 3];
    assert!(h264_starts_keyframe(&start));
    // continuation of an IDR fragment
    let cont = vec![0x7c, 0x45 /* S=0, E=1, type 5 */, 1, 2, 3];
    assert!(!h264_starts_keyframe(&cont));
    // start of a non-IDR fragment
    let inter = vec![0x7c, 0x81, 1, 2, 3];
    assert!(!h264_starts_keyframe(&inter));
}

#[test]
fn h264_parameter_sets_are_collected() {
    let mut sets = ParameterSets::default();
    assert!(!sets.complete());
    scan_h264_parameter_sets(&nal_unit(nal::SPS, b"sps"), &mut sets);
    assert!(!sets.complete());
    scan_h264_parameter_sets(&nal_unit(nal::PPS, b"pps"), &mut sets);
    assert!(sets.complete());
    assert_eq!(sets.sps.as_deref(), Some(&nal_unit(nal::SPS, b"sps")[..]));

    let sps = nal_unit(nal::SPS, b"1");
    let pps = nal_unit(nal::PPS, b"2");
    let mut buf = vec![nal::STAP_A];
    for nal in [&sps, &pps] {
        buf.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        buf.extend_from_slice(nal);
    }
    let mut sets = ParameterSets::default();
    scan_h264_parameter_sets(&buf, &mut sets);
    assert!(sets.complete());
    assert_eq!(sets.sps.as_deref(), Some(&sps[..]));
    assert_eq!(sets.pps.as_deref(), Some(&pps[..]));
}

#[test]
fn sprop_hex_roundtrip() {
    let sets = ParameterSets {
        sps: Some(vec![0x67, 0x42, 0xc0, 0x1f]),
        pps: Some(vec![0x68, 0xce, 0x3c, 0x80]),
    };
    let hex = sets.to_sprop_hex();
    let (sps, pps) = hex.split_once(':').unwrap();
    let parsed = ParameterSets::from_sprop_hex(sps, pps);
    assert_eq!(parsed, sets);
    assert_eq!(
        sets.to_sprop().unwrap(),
        "Z0LAHw==,aM48gA==",
        "sprop must be standard base64"
    );
    assert!(ParameterSets::default().to_sprop().is_none());
}

#[test]
fn vp8_keyframe_flag() {
    // S=1, PID=0, keyframe (inverse keyframe flag clear)
    assert!(vp8_starts_keyframe(&[0x10, 0x00, 0x00]));
    // inter frame
    assert!(!vp8_starts_keyframe(&[0x11, 0x00, 0x00]));
    // extended descriptor, X=1, still a keyframe
    assert!(vp8_starts_keyframe(&[0x90, 0x7f, 0x01, 0x02]));
    assert!(!vp8_starts_keyframe(&[0x91, 0x7f, 0x01, 0x02]));
}

#[test]
fn opus_is_always_decodable() {
    assert!(starts_keyframe(b"", Codec::Opus));
    assert!(starts_keyframe(b"", Codec::Other));
    assert!(!starts_keyframe(&nal_unit(1, b"x"), Codec::H264));
}

#[test]
fn codec_names() {
    assert_eq!(Codec::from_name("H264"), Codec::H264);
    assert_eq!(Codec::from_name("h264"), Codec::H264);
    assert_eq!(Codec::from_name("VP8"), Codec::Vp8);
    assert_eq!(Codec::from_name("opus"), Codec::Opus);
    assert_eq!(Codec::from_name("L16"), Codec::Other);
}
