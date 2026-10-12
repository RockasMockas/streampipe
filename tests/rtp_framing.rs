//! RTP framing: the fixed header, the tunnel tag, and what we refuse to parse.
//!
//! These are the wire format of the media tunnel (see `streampipe::rtp`), so they
//! are checked against hand-assembled bytes rather than against the helpers.

use streampipe::rtp::{parse_header, serialize_rtp, tagged, RtpInfo, RTP_HEADER_LEN, TAG_VIDEO};

#[test]
fn rtp_roundtrip() {
    let info = RtpInfo {
        marker: true,
        pt: 96,
        seq: 0x1234,
        timestamp: 0xdeadbeef,
        ssrc: 0x01020304,
    };
    let payload = vec![1, 2, 3, 4];
    let buf = serialize_rtp(&info, &payload);
    assert_eq!(buf.len(), RTP_HEADER_LEN + 4);
    assert_eq!(buf[0], 0x80);
    let (parsed, offset) = parse_header(&buf).unwrap();
    assert_eq!(parsed, info);
    assert_eq!(offset, RTP_HEADER_LEN);
    assert_eq!(&buf[offset..], &payload);
}

#[test]
fn parse_header_skips_extensions_and_csrc() {
    // V=2, X=1, CC=1: the fixed header, one csrc, a two byte profile with
    // one 32 bit word of extension data, then the payload.
    let mut buf = vec![
        0x91, 96, 0, 1, //
        0, 0, 0, 2, //
        0, 0, 0, 3, //
        0xde, 0xad, 0xbe, 0xef, // csrc
        0xbe, 0xe0, 0x00, 0x01, // extension header, one word of data
        0xca, 0xfe, 0xba, 0xbe, // extension data
    ];
    buf.extend_from_slice(b"payload");
    let (info, offset) = parse_header(&buf).unwrap();
    assert!(!info.marker);
    assert_eq!(info.pt, 96);
    assert_eq!(info.seq, 1);
    assert_eq!(offset, 24);
    assert_eq!(&buf[offset..], b"payload");

    // a truncated extension is not a packet
    let buf = &buf[..20];
    assert!(parse_header(buf).is_none());
}

#[test]
fn parse_header_rejects_rubbish() {
    assert!(parse_header(b"hi").is_none());
    let mut buf = vec![0x00, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
    assert!(parse_header(&buf).is_none());
    buf[0] = 0x80;
    assert!(parse_header(&buf).is_some());
    // csrc count beyond the buffer
    let buf = vec![0x8f, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
    assert!(parse_header(&buf).is_none());
}

#[test]
fn tagging_prefixes() {
    assert_eq!(tagged(TAG_VIDEO, b"abc"), vec![TAG_VIDEO, b'a', b'b', b'c']);
}
