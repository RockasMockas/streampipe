//! SDP handling for the WHIP ingest / RTP playback path.
//!
//! Two halves are pinned here: parsing a publisher's offer down to the one payload
//! type per media we forward, and rendering the plain RTP SDP a local player opens.

use streampipe::rtp::ParameterSets;
use streampipe::sdp::{
    parse_offer, render_player_sdp, MediaHeader, MediaKind, OfferMedia, SessionHeader,
};

const OFFER: &str = "v=0\r\n\
    o=- 4611731400498476682 2 IN IP4 127.0.0.1\r\n\
    s=-\r\n\
    t=0 0\r\n\
    a=group:BUNDLE 0 1\r\n\
    m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
    c=IN IP4 0.0.0.0\r\n\
    a=rtcp:9 IN IP4 0.0.0.0\r\n\
    a=ice-ufrag:aBcD\r\n\
    a=rtcp-mux\r\n\
    a=rtpmap:96 H264/90000\r\n\
    a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
    a=rtpmap:97 rtx/90000\r\n\
    a=fmtp:97 apt=96\r\n\
    a=sendonly\r\n\
    m=audio 9 UDP/TLS/RTP/SAVPF 111 63\r\n\
    c=IN IP4 0.0.0.0\r\n\
    a=rtpmap:111 opus/48000/2\r\n\
    a=fmtp:111 minptime=10;useinbandfec=1\r\n\
    a=rtpmap:63 red/48000/2\r\n\
    a=fmtp:63 111/111\r\n\
    a=sendonly\r\n";

#[test]
fn offer_is_parsed() {
    let media: Vec<OfferMedia> = parse_offer(OFFER);
    assert_eq!(media.len(), 2);
    assert_eq!(media[0].kind, MediaKind::Video);
    assert_eq!(media[0].payloads.len(), 1, "rtx is not forwarded");
    let video = &media[0].payloads[0];
    assert_eq!(video.pt, 96);
    assert_eq!(video.codec, "H264");
    assert_eq!(video.clock, 90_000);
    assert_eq!(
        video.fmtp,
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
    );

    assert_eq!(media[1].kind, MediaKind::Audio);
    assert_eq!(media[1].payloads.len(), 1, "red is not forwarded");
    let audio = &media[1].payloads[0];
    assert_eq!(audio.pt, 111);
    assert_eq!(audio.codec, "opus");
    assert_eq!(audio.channels, 2);
    assert_eq!(audio.fmtp, "minptime=10;useinbandfec=1");
}

#[test]
fn offer_sprop_is_stripped() {
    let sdp = "m=video 9 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
        a=fmtp:96 sprop-parameter-sets=Z0LAHw==,aM48gA==;packetization-mode=1\r\n";
    let media = parse_offer(sdp);
    assert_eq!(media[0].payloads[0].fmtp, "packetization-mode=1");
}

#[test]
fn offer_without_media_is_empty() {
    assert!(parse_offer("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n").is_empty());
    // application media (data channels) is ignored
    assert!(parse_offer("m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n").is_empty());
}

fn header() -> SessionHeader {
    SessionHeader {
        gen: 7,
        media: vec![
            MediaHeader {
                kind: MediaKind::Video,
                pt: 96,
                codec: "H264".into(),
                clock: 90_000,
                channels: 1,
                fmtp: "packetization-mode=1;profile-level-id=42e01f".into(),
                sets: ParameterSets {
                    sps: Some(vec![0x67, 0x42, 0xc0, 0x1f]),
                    pps: Some(vec![0x68, 0xce, 0x3c, 0x80]),
                },
            },
            MediaHeader {
                kind: MediaKind::Audio,
                pt: 111,
                codec: "opus".into(),
                clock: 48_000,
                channels: 2,
                fmtp: "minptime=10".into(),
                sets: ParameterSets::default(),
            },
        ],
    }
}

#[test]
fn session_header_roundtrips() {
    let header = header();
    let bytes = header.encode();
    let decoded = SessionHeader::decode(&bytes).expect("decodes");
    assert_eq!(decoded, header);
}

#[test]
fn session_header_rejects_rubbish() {
    assert!(SessionHeader::decode(b"hello").is_none());
    assert!(SessionHeader::decode(b"gen=1\nmedia video pt=x\n").is_none());
    assert_eq!(SessionHeader::decode(b"gen=1\n").unwrap().media.len(), 0);
}

#[test]
fn player_sdp_is_playable_shape() {
    let sdp = render_player_sdp(&header(), 5004, 5006);
    assert!(sdp.contains("c=IN IP4 127.0.0.1"));
    assert!(sdp.contains("m=video 5004 RTP/AVP 96"));
    assert!(sdp.contains("a=rtpmap:96 H264/90000"));
    assert!(sdp.contains(
        "a=fmtp:96 packetization-mode=1;profile-level-id=42e01f;sprop-parameter-sets=Z0LAHw==,aM48gA=="
    ));
    assert!(sdp.contains("m=audio 5006 RTP/AVP 111"));
    assert!(sdp.contains("a=rtpmap:111 opus/48000/2"));
    assert_eq!(sdp.matches("a=recvonly").count(), 2);
}

#[test]
fn player_sdp_without_parameter_sets() {
    let mut header = header();
    header.media[0].sets = ParameterSets::default();
    let sdp = render_player_sdp(&header, 5004, 5006);
    assert!(sdp.contains("a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\n"));
    assert!(!sdp.contains("sprop"));
}
