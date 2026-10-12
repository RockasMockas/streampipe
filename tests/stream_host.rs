//! Host-side tests for the WHIP ingest session.
//!
//! These drive the host through its seams: an SDP offer in, an answer out,
//! [`Host::handle_media`] for the RTP the publisher sends,
//! [`Host::on_viewer_event`] for the viewers behind it. The last test is the
//! only one that runs a real peer connection (ICE, DTLS and SRTP on loopback)
//! end to end; everything else avoids the network stack.
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::{
    change::SdpAnswer,
    media::{Direction, MediaKind as Str0mMediaKind},
    net::{Protocol, Receive},
    rtp::RtpWrite,
    Candidate, Event, Input, Output, Rtc,
};
use tokio::{net::UdpSocket, sync::mpsc, time::sleep_until};

use streampipe::{
    rtp::{self, RtpInfo},
    sdp::{self, render_player_sdp, MediaKind},
    stream::{parse_candidate, Frame, Host, ViewerEvent, ICE_BUF},
};

fn offer() -> String {
    let fingerprint = "00:01:02:03:04:05:06:07:08:09:0A:0B:0C:0D:0E:0F:10:11:12:13:14:15:16:17:18:19:1A:1B:1C:1D:1E:1F";
    format!(
        "v=0\r\n\
         o=- 4611731400498476682 2 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=group:BUNDLE 0 1\r\n\
         m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=rtcp-mux\r\n\
         a=ice-ufrag:aBcD\r\n\
         a=ice-pwd:012345678901234567890123456789\r\n\
         a=fingerprint:sha-256 {fingerprint}\r\n\
         a=setup:actpass\r\n\
         a=mid:0\r\n\
         a=sendonly\r\n\
         a=rtpmap:96 H264/90000\r\n\
         a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
         a=rtpmap:97 rtx/90000\r\n\
         a=fmtp:97 apt=96\r\n\
         m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=rtcp-mux\r\n\
         a=ice-ufrag:aBcD\r\n\
         a=ice-pwd:012345678901234567890123456789\r\n\
         a=fingerprint:sha-256 {fingerprint}\r\n\
         a=setup:actpass\r\n\
         a=mid:1\r\n\
         a=sendonly\r\n\
         a=rtpmap:111 opus/48000/2\r\n\
         a=fmtp:111 minptime=10;useinbandfec=1\r\n"
    )
}

fn nal(ty: u8, body: &[u8]) -> Vec<u8> {
    let mut buf = vec![0x60 | ty];
    buf.extend_from_slice(body);
    buf
}

fn stap(nals: &[&[u8]]) -> Vec<u8> {
    let mut buf = vec![24];
    for nal in nals {
        buf.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        buf.extend_from_slice(nal);
    }
    buf
}

fn info(pt: u8, seq: u16) -> RtpInfo {
    RtpInfo {
        marker: false,
        pt,
        seq,
        timestamp: 90_000,
        ssrc: 7,
    }
}

fn keyframe() -> Vec<u8> {
    let sps = nal(7, b"sps");
    let pps = nal(8, b"pps");
    stap(&[&sps, &pps, &nal(5, b"idr")])
}

async fn host() -> Host {
    let ice = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let ice_addr = ice.local_addr().unwrap();
    Host::new(ice, ice_addr, false, None)
}

fn viewer(host: &mut Host) -> mpsc::Receiver<Frame> {
    let (tx, rx) = mpsc::channel(16);
    host.on_viewer_event(ViewerEvent::Joined {
        id: 1,
        port: 5004,
        tx,
    });
    rx
}

fn text(frame: Frame) -> String {
    match frame {
        Frame::Header(b) => String::from_utf8(b.to_vec()).expect("utf8 header"),
        other => panic!("expected a header, got {other:?}"),
    }
}

#[tokio::test]
async fn whip_offer_is_answered() {
    let mut host = host().await;
    let answer = host.negotiate(&offer(), false).expect("an answer");
    assert!(answer.sdp.contains("m=video"), "{}", answer.sdp);
    assert!(answer.sdp.contains("a=recvonly"), "{}", answer.sdp);
    assert!(!answer.etag.is_empty());
    assert_eq!(host.offer.len(), 2);
    assert_eq!(host.payload_of(96).unwrap().0, MediaKind::Video);
    assert_eq!(host.payload_of(96).unwrap().1.codec, "H264");
    assert!(host.payload_of(97).is_none(), "rtx is never forwarded");
}

#[tokio::test]
async fn bad_offer_is_rejected() {
    let mut host = host().await;
    let err = host.negotiate("v=0\r\n", false).unwrap_err();
    assert!(!err.is_empty());
    assert!(host.offer.is_empty());
}

#[tokio::test]
async fn mid_gop_join_starts_at_a_keyframe() {
    let mut host = host().await;
    host.negotiate(&offer(), false).expect("an answer");
    let mut rx = viewer(&mut host);

    let header = text(rx.try_recv().expect("a header"));
    assert!(header.contains("gen="), "{header}");

    host.handle_media(info(96, 10), &nal(1, b"inter"));
    host.handle_media(info(96, 11), &nal(1, b"inter"));
    assert!(rx.try_recv().is_err(), "media must wait for a keyframe");

    let kf = keyframe();
    host.handle_media(info(96, 12), &kf);
    let header = text(rx.recv().await.expect("a header"));
    assert!(header.contains("pt=96"), "{header}");
    assert!(header.contains("media video"), "{header}");
    assert!(matches!(rx.recv().await, Some(Frame::Epoch)));
    assert!(matches!(rx.recv().await, Some(Frame::Video(_))));

    let sdp = render_player_sdp(&host.header, 5004, 5006);
    assert!(
        sdp.contains("sprop-parameter-sets="),
        "a mid-gop join needs the parameter sets: {sdp}"
    );

    host.handle_media(info(96, 13), &nal(1, b"inter"));
    assert!(matches!(rx.try_recv(), Ok(Frame::Video(_))));
}

#[tokio::test]
async fn keyframe_request_keeps_the_stream_flowing() {
    // Once a viewer is streaming it has its SDP and parameter sets, so a
    // keyframe request (a gap on a lossy path) must NOT blackout the stream:
    // the gate stays open, inter frames keep flowing, and every keyframe is
    // epoch-tagged so the viewer can pace its requests.
    let mut host = host().await;
    host.negotiate(&offer(), false).expect("an answer");
    let mut rx = viewer(&mut host);
    rx.try_recv().expect("a header");

    host.handle_media(info(96, 1), &keyframe());
    assert!(matches!(rx.try_recv(), Ok(Frame::Header(_))));
    assert!(matches!(rx.try_recv(), Ok(Frame::Epoch)));
    assert!(matches!(rx.try_recv(), Ok(Frame::Video(_))));

    // A gap nudges the encoder, but the gate stays open: the next inter
    // frame is still forwarded, no blackout until the next IDR.
    host.on_viewer_event(ViewerEvent::Keyframe { id: 1 });
    host.handle_media(info(96, 2), &nal(1, b"inter"));
    assert!(
        matches!(rx.try_recv(), Ok(Frame::Video(_))),
        "the gate stays open after a keyframe request"
    );

    // The next keyframe is epoch-tagged (no header, the viewer has it) so
    // the viewer resets its sequence baseline there.
    host.handle_media(info(96, 3), &keyframe());
    assert!(matches!(rx.try_recv(), Ok(Frame::Epoch)));
    assert!(matches!(rx.try_recv(), Ok(Frame::Video(_))));
}

#[tokio::test]
async fn audio_and_video_are_tagged_apart() {
    let mut host = host().await;
    host.negotiate(&offer(), false).expect("an answer");
    let mut rx = viewer(&mut host);
    rx.try_recv().expect("a header");

    host.handle_media(info(96, 1), &keyframe());
    assert!(matches!(rx.try_recv(), Ok(Frame::Header(_))));
    assert!(matches!(rx.try_recv(), Ok(Frame::Epoch)));
    assert!(matches!(rx.try_recv(), Ok(Frame::Video(_))));

    host.handle_media(info(111, 1), b"opus-frame");
    assert!(matches!(rx.try_recv(), Ok(Frame::Audio(_))));

    host.handle_media(info(97, 2), b"rtx");
    assert!(rx.try_recv().is_err());
    assert_eq!(host.counters.get(&host.counters.unwanted), 1);
}

#[tokio::test]
async fn viewers_leave_and_rejoin() {
    let mut host = host().await;
    host.negotiate(&offer(), false).expect("an answer");
    let rx = viewer(&mut host);
    assert_eq!(host.viewers.len(), 1);

    host.handle_media(info(96, 1), &keyframe());
    host.on_viewer_event(ViewerEvent::Left { id: 1 });
    assert!(host.viewers.is_empty());

    host.handle_media(info(96, 2), &nal(1, b"inter"));
    assert_eq!(host.counters.get(&host.counters.undeliverable), 1);
    drop(rx);

    let mut rx = viewer(&mut host);
    rx.try_recv().expect("a header");
    host.handle_media(info(96, 3), &nal(1, b"inter"));
    assert!(rx.try_recv().is_err());
    host.handle_media(info(96, 4), &keyframe());
    assert!(matches!(rx.try_recv(), Ok(Frame::Header(_))));
}

#[tokio::test]
async fn streaming_is_announced_once_per_kind() {
    let mut h = host().await;
    h.negotiate(&offer(), false).expect("an answer");
    let video = sdp::Payload {
        pt: 96,
        codec: "H264".into(),
        clock: 90000,
        channels: 1,
        fmtp: String::new(),
    };
    h.log_streaming(MediaKind::Video, &video, false);
    h.log_streaming(MediaKind::Video, &video, true);
    assert_eq!(h.seen, vec![MediaKind::Video]);
    h.log_streaming(MediaKind::Audio, &video, false);
    assert_eq!(h.seen, vec![MediaKind::Video, MediaKind::Audio]);
}

#[test]
fn trickle_candidates_are_parsed() {
    let c = parse_candidate("1 1 udp 2130706431 192.168.1.10 50000 typ host").expect("candidate");
    assert_eq!(c.addr().ip().to_string(), "192.168.1.10");
    assert!(parse_candidate("nonsense").is_none());
}

/// The whole ingest path against a real WebRTC peer.
///
/// A second str0m instance does what OBS does: it offers H.264, completes
/// ICE and DTLS on loopback, and writes one RTP keyframe. The host has to
/// answer, decrypt, and hand the packet to the viewer behind the keyframe
/// gate. This is the only test that exercises ICE, DTLS and SRTP at all;
/// everything else drives the session through its own seams.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_publisher_reaches_a_viewer() {
    let ice = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let ice_addr = ice.local_addr().unwrap();
    let mut host = Host::new(ice.clone(), ice_addr, false, None);
    let (tx, mut rx) = mpsc::channel(256);
    host.on_viewer_event(ViewerEvent::Joined {
        id: 1,
        port: 5004,
        tx,
    });

    let pub_ice = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pub_addr = pub_ice.local_addr().unwrap();
    let mut pub_rtc = Rtc::builder()
        .set_rtp_mode(true)
        .clear_codecs()
        .enable_h264(true)
        .build(Instant::now());
    pub_rtc
        .add_local_candidate(Candidate::host(pub_addr, Protocol::Udp).expect("a host candidate"));

    let mut api = pub_rtc.sdp_api();
    let mid = api.add_media(Str0mMediaKind::Video, Direction::SendOnly, None, None, None);
    let (offer, pending) = api.apply().expect("a change");

    let answer = host
        .negotiate(&offer.to_sdp_string(), false)
        .expect("an answer");
    let answer = SdpAnswer::from_sdp_string(&answer.sdp).expect("a parseable answer");
    pub_rtc
        .sdp_api()
        .accept_answer(pending, answer)
        .expect("accepting the answer");

    // the payload type the publisher offered and we accepted
    let video = host
        .offer
        .iter()
        .find(|m| m.kind == MediaKind::Video)
        .expect("video in the offer");
    let pt = video.payloads.first().expect("a video payload type");
    assert_eq!(pt.codec, "H264");
    let pt = pt.pt;

    let mut host_buf = vec![0u8; ICE_BUF];
    let mut pub_buf = vec![0u8; ICE_BUF];
    let keyframe = keyframe();
    let mut frames: Vec<Frame> = Vec::new();
    let mut connected = false;
    let mut written = false;
    let started = Instant::now();

    loop {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the session did not come up, frames so far: {frames:?}"
        );

        // Drain both engines to their timeout before the next mutation, as
        // str0m's single mutation invariant demands.
        let host_deadline = loop {
            match host.rtc.poll_output().expect("host output") {
                Output::Timeout(at) => break at,
                Output::Transmit(t) => {
                    ice.send_to(&t.contents, t.destination).await.unwrap();
                }
                Output::Event(event) => {
                    if matches!(event, Event::Connected) {
                        connected = true;
                    }
                    host.on_event(event);
                }
            }
        };
        let pub_deadline = loop {
            match pub_rtc.poll_output().expect("publisher output") {
                Output::Timeout(at) => break at,
                Output::Transmit(t) => {
                    pub_ice.send_to(&t.contents, t.destination).await.unwrap();
                }
                Output::Event(_) => {}
            }
        };

        if connected && !written {
            let mut direct = pub_rtc.direct_api();
            let stream = direct.stream_tx_by_mid(mid, None).expect("a send stream");
            stream.write_rtp(
                RtpWrite::new(
                    pt.into(),
                    1000u64.into(),
                    90_000,
                    Instant::now(),
                    keyframe.clone(),
                )
                .marker(true),
            );
            written = true;
        }

        while let Ok(frame) = rx.try_recv() {
            frames.push(frame);
        }
        if written && frames.iter().any(|f| matches!(f, Frame::Video(_))) {
            break;
        }

        tokio::select! {
            res = ice.recv_from(&mut host_buf) => {
                if let Ok((len, source)) = res {
                    host.on_ice(&host_buf[..len], source);
                }
            }
            res = pub_ice.recv_from(&mut pub_buf) => {
                if let Ok((len, source)) = res {
                    let contents = (&pub_buf[..len]).try_into().expect("a packet");
                    pub_rtc
                        .handle_input(Input::Receive(
                            Instant::now(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: pub_addr,
                                contents,
                            },
                        ))
                        .expect("publisher input");
                }
            }
            _ = sleep_until(tokio::time::Instant::from_std(host_deadline)) => host.on_timeout(),
            _ = sleep_until(tokio::time::Instant::from_std(pub_deadline)) => {
                pub_rtc
                    .handle_input(Input::Timeout(Instant::now()))
                    .expect("publisher timeout");
            }
        }
    }

    let kinds: Vec<&str> = frames
        .iter()
        .map(|f| match f {
            Frame::Header(_) => "header",
            Frame::Epoch => "epoch",
            Frame::Video(_) => "video",
            Frame::Audio(_) => "audio",
        })
        .collect();
    assert_eq!(kinds.first().copied(), Some("header"), "{kinds:?}");
    let epoch = kinds.iter().position(|k| *k == "epoch").expect("an epoch");
    let video = kinds.iter().position(|k| *k == "video").expect("video");
    assert!(epoch < video, "the epoch precedes the keyframe: {kinds:?}");

    let payload = frames
        .iter()
        .find_map(|f| match f {
            Frame::Video(b) => Some(b.clone()),
            _ => None,
        })
        .expect("video");
    let (info, start) = rtp::parse_header(&payload).expect("an rtp header");
    assert_eq!(info.pt, pt, "the payload type is untouched");
    assert_eq!(info.seq, 1000, "the sequence number is untouched");
    assert!(info.marker, "the marker bit is untouched");
    assert_eq!(
        &payload[start..],
        &keyframe[..],
        "the payload survived ice, dtls and srtp"
    );

    // the header the viewer got before the keyframe must carry the codec
    // and the parameter sets it saw in band, otherwise its player cannot
    // start; the first header a viewer gets is the empty one it joined on
    let header = frames[..epoch]
        .iter()
        .rev()
        .find_map(|f| match f {
            Frame::Header(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .expect("a header before the keyframe");
    assert!(header.contains("H264"), "{header}");
    assert!(header.contains("video"), "{header}");
}
