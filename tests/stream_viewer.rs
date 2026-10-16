//! Viewer-side tests for the media tunnel.
//!
//! Everything here is the downstream half of the pipeline: how the player is
//! launched and where its SDP goes, how the jitter buffer is chosen from the
//! path RTT, how one RTP packet is forwarded to the local player port, and how
//! the periodic media tallies are computed. No WebRTC session is set up.
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, Instant};

use tokio::{net::UdpSocket, sync::mpsc};

use streampipe::{
    rtp::{self, RtpInfo},
    sdp::MediaKind,
    stream::{
        auto_buffer, forward, human_duration, sdp_path, wait_until_listening, Counters, MediaTally,
        PlayPorts, Player, StreamReport,
    },
};

fn info(pt: u8, seq: u16) -> RtpInfo {
    RtpInfo {
        marker: false,
        pt,
        seq,
        timestamp: 90_000,
        ssrc: 7,
    }
}

#[test]
fn auto_buffer_follows_the_path_rtt() {
    // An explicit buffer always wins, whatever the path says.
    let explicit = Duration::from_millis(800);
    assert_eq!(
        auto_buffer(Some(explicit), Some(Duration::from_secs(1))),
        Some(explicit)
    );
    assert_eq!(auto_buffer(Some(explicit), None), Some(explicit));
    // A local path stays at the lowest latency.
    assert_eq!(auto_buffer(None, None), None);
    assert_eq!(auto_buffer(None, Some(Duration::from_millis(2))), None);
    assert_eq!(auto_buffer(None, Some(Duration::from_millis(49))), None);
    // A long-distance path gets four round-trips, clamped to 150-500 ms.
    assert_eq!(
        auto_buffer(None, Some(Duration::from_millis(50))),
        Some(Duration::from_millis(200))
    );
    assert_eq!(
        auto_buffer(None, Some(Duration::from_millis(80))),
        Some(Duration::from_millis(320))
    );
    assert_eq!(
        auto_buffer(None, Some(Duration::from_millis(120))),
        Some(Duration::from_millis(480))
    );
    assert_eq!(
        auto_buffer(None, Some(Duration::from_millis(300))),
        Some(Duration::from_millis(500))
    );
}

#[test]
fn play_ports_are_even_pairs() {
    let ports = PlayPorts::new("127.0.0.1:5004".parse().unwrap());
    assert_eq!((ports.video, ports.audio), (5004, 5006));

    let ports = PlayPorts::new("127.0.0.1:5005".parse().unwrap());
    assert_eq!((ports.video, ports.audio), (5006, 5008));
}

#[test]
fn player_commands_are_low_latency() {
    let path = Path::new("/tmp/x.sdp");
    let mpv = Player::Mpv.command(path, None, None, 100, None);
    assert_eq!(mpv[0], "mpv");
    assert!(mpv.contains(&"--no-config".to_string()));
    assert!(mpv.contains(&"--force-window=immediate".to_string()));
    // `--untimed` plays as fast as it can decode, which drains the cache to
    // nothing and makes the `speed` control script inert: never pass it.
    assert!(!mpv.contains(&"--untimed".to_string()));
    assert!(mpv.contains(&"--profile=low-latency".to_string()));
    assert!(mpv.contains(&"--audio-buffer=0.5".to_string()), "{mpv:?}");
    let profile_i = mpv
        .iter()
        .position(|a| a == "--profile=low-latency")
        .unwrap();
    let abuf_i = mpv.iter().position(|a| a == "--audio-buffer=0.5").unwrap();
    assert!(abuf_i > profile_i, "audio-buffer must override the profile");
    // The profile's latency hacks stop mpv dropping late frames, so the picture
    // never catches up with the audio after a stall or a speed-up.
    let hacks_i = mpv
        .iter()
        .position(|a| a == "--video-latency-hacks=no")
        .unwrap_or_else(|| panic!("latency hacks left on: {mpv:?}"));
    assert!(hacks_i > profile_i, "must override the profile");
    assert!(mpv.contains(&"--no-resume-playback".to_string()));
    assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");

    // A staged control script is forwarded to mpv, and the SDP stays last so
    // mpv still gets exactly one input.
    let mpv = Player::Mpv.command(path, None, None, 100, Some(Path::new("/tmp/bs.lua")));
    assert!(mpv.contains(&"--script=/tmp/bs.lua".to_string()), "{mpv:?}");
    assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");

    assert!(Player::Vlc
        .command(path, None, None, 100, None)
        .contains(&"--network-caching=100".to_string()));
    assert!(Player::Ffplay
        .command(path, None, None, 100, None)
        .contains(&"nobuffer".to_string()));
    // Only mpv runs Lua: the others are never handed a script.
    for cmd in [
        Player::Vlc.command(path, None, None, 100, Some(Path::new("/tmp/bs.lua"))),
        Player::Ffplay.command(path, None, None, 100, Some(Path::new("/tmp/bs.lua"))),
    ] {
        assert!(!cmd.iter().any(|a| a.starts_with("--script=")), "{cmd:?}");
    }
    assert!(!Player::None.launches());
    assert!(Player::Mpv.launches());
}

/// Every player can be told where to start: mpv `--volume=N`, ffplay
/// `-volume N`, VLC `--gain=N/100` (VLC 3 has no `--volume`). The flag must
/// be present whatever the buffering, and the file must stay last.
#[test]
fn player_commands_carry_the_startup_volume() {
    let path = Path::new("/tmp/x.sdp");
    for buffer in [None, Some(Duration::from_millis(250))] {
        let mpv = Player::Mpv.command(path, buffer, None, 40, None);
        assert!(mpv.contains(&"--volume=40".to_string()), "{mpv:?}");
        assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");

        let ffplay = Player::Ffplay.command(path, buffer, None, 40, None);
        let i = ffplay
            .iter()
            .position(|a| a == "-volume")
            .unwrap_or_else(|| panic!("no -volume in {ffplay:?}"));
        // ffplay starts 20 points below the GUI level, so 40 in the GUI is 20
        // in the command.
        assert_eq!(ffplay[i + 1], "20");
        // The offset is saturating: nothing goes negative, and full scale in the
        // GUI is 80 in the command.
        let ffplay = Player::Ffplay.command(path, buffer, None, 100, None);
        let i = ffplay
            .iter()
            .position(|a| a == "-volume")
            .unwrap_or_else(|| panic!("no -volume in {ffplay:?}"));
        assert_eq!(ffplay[i + 1], "80");
        for gui in [0, 20] {
            let ffplay = Player::Ffplay.command(path, buffer, None, gui, None);
            let i = ffplay
                .iter()
                .position(|a| a == "-volume")
                .unwrap_or_else(|| panic!("no -volume in {ffplay:?}"));
            assert_eq!(ffplay[i + 1], "0", "gui {gui}");
        }

        let vlc = Player::Vlc.command(path, buffer, None, 40, None);
        assert!(vlc.contains(&"--gain=0.40".to_string()), "{vlc:?}");
        // The legacy name is a warning, not a volume: never pass it.
        assert!(!vlc.iter().any(|a| a == "--volume=40"));

        // A custom binary keeps its player's volume flag.
        let mpv = Player::Mpv.command(path, buffer, Some(Path::new("/opt/mpv")), 40, None);
        assert!(mpv.contains(&"--volume=40".to_string()));
    }
}

/// Every player is told where and how big to open: a 1280x720 window in the
/// top-left corner, the largest window that still fits a small laptop screen
/// (1366x768) whole. mpv takes a single `--geometry=WxH+X+Y`; VLC
/// `--width`/`--height` plus `--video-x`/`--video-y`; ffplay `-x`/`-y` plus
/// `-left`/`-top`. The flags must be present whatever the buffering.
#[test]
fn player_windows_open_at_720p_in_the_corner() {
    let path = Path::new("/tmp/x.sdp");
    for buffer in [None, Some(Duration::from_millis(250))] {
        let mpv = Player::Mpv.command(path, buffer, None, 60, None);
        assert!(
            mpv.contains(&"--geometry=1280x720+0+0".to_string()),
            "{mpv:?}"
        );
        assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");

        let vlc = Player::Vlc.command(path, buffer, None, 60, None);
        assert!(vlc.contains(&"--width=1280".to_string()), "{vlc:?}");
        assert!(vlc.contains(&"--height=720".to_string()), "{vlc:?}");
        assert!(vlc.contains(&"--video-x=0".to_string()), "{vlc:?}");
        assert!(vlc.contains(&"--video-y=0".to_string()), "{vlc:?}");
        assert_eq!(vlc.last().unwrap(), "/tmp/x.sdp");

        let ffplay = Player::Ffplay.command(path, buffer, None, 60, None);
        // ffplay's window flags are space-separated pairs, so check the pair.
        let pair = |name: &str, value: &str| {
            let i = ffplay
                .iter()
                .position(|a| a == name)
                .unwrap_or_else(|| panic!("no {name} in {ffplay:?}"));
            assert_eq!(ffplay[i + 1], value, "{name}");
        };
        pair("-x", "1280");
        pair("-y", "720");
        pair("-left", "0");
        pair("-top", "0");
        assert_eq!(ffplay.last().unwrap(), "/tmp/x.sdp");
    }
}

/// `--no-config` keeps mpv.conf out, but the user's input.conf is passed back
/// by absolute path; with no input.conf there is no `--input-conf` at all.
#[test]
fn mpv_loads_the_users_input_conf_but_not_their_config() {
    let path = Path::new("/tmp/x.sdp");
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("MPV_HOME", dir.path());

    let mpv = Player::Mpv.command(path, None, None, 100, None);
    assert!(
        !mpv.iter().any(|a| a.starts_with("--input-conf")),
        "{mpv:?}"
    );

    let conf = dir.path().join("input.conf");
    std::fs::write(&conf, "x show-text hi\n").unwrap();
    let mpv = Player::Mpv.command(path, None, None, 100, None);
    std::env::remove_var("MPV_HOME");
    assert!(mpv.contains(&"--no-config".to_string()));
    assert!(
        mpv.contains(&format!("--input-conf={}", conf.display())),
        "{mpv:?}"
    );
    assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");
}

#[test]
fn a_player_path_replaces_the_binary_but_keeps_the_flags() {
    let path = Path::new("/tmp/x.sdp");
    let bin = Path::new("/opt/mpv");
    let mpv = Player::Mpv.command(path, None, Some(bin), 100, None);
    assert_eq!(mpv[0], "/opt/mpv");
    assert!(mpv.contains(&"--profile=low-latency".to_string()));
    assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");
    // The path is used whatever the player, with that player's flags.
    let ffplay =
        Player::Ffplay.command(path, Some(Duration::from_millis(300)), Some(bin), 100, None);
    assert_eq!(ffplay[0], "/opt/mpv");
    assert!(ffplay.contains(&"+discardcorrupt".to_string()));
}

#[test]
fn a_buffer_switches_the_player_out_of_zero_buffering() {
    let path = Path::new("/tmp/x.sdp");
    let ms = Duration::from_millis(300);
    let mpv = Player::Mpv.command(path, Some(ms), None, 100, None);
    assert!(!mpv.contains(&"--profile=low-latency".to_string()));
    assert!(!mpv.contains(&"--no-cache".to_string()));
    assert!(mpv.contains(&"--cache=yes".to_string()));
    assert!(mpv.contains(&"--no-resume-playback".to_string()));
    assert!(mpv.iter().any(|a| a.contains("max_delay=300000")));
    assert!(!mpv.iter().any(|a| a.starts_with("--video-latency-hacks")));

    let ffplay = Player::Ffplay.command(path, Some(ms), None, 100, None);
    assert!(!ffplay.iter().any(|a| a == "nobuffer"));
    assert!(ffplay.contains(&"+discardcorrupt".to_string()));
    assert!(ffplay.iter().any(|a| a == "300000"));

    let vlc = Player::Vlc.command(path, Some(ms), None, 100, None);
    assert!(vlc.contains(&"--network-caching=300".to_string()));
}

/// Aggressive Low Latency: no player buffer even when one was asked for, no RTP
/// reorder queue in the player (the viewer already orders the packets), a small
/// mpv audio buffer, late frames still dropped, and the speed script told to
/// chase the live edge.
#[test]
fn low_latency_player_commands_drop_every_buffer() {
    let path = Path::new("/tmp/x.sdp");
    let script = Some(Path::new("/tmp/bs.lua"));
    let pair = |cmd: &[String], name: &str| {
        let i = cmd
            .iter()
            .position(|a| a == name)
            .unwrap_or_else(|| panic!("no {name} in {cmd:?}"));
        cmd[i + 1].clone()
    };
    for buffer in [None, Some(Duration::from_millis(300))] {
        let mpv = Player::Mpv.command_for(path, buffer, None, 100, script, true);
        assert!(mpv.contains(&"--no-cache".to_string()), "{mpv:?}");
        assert!(!mpv.contains(&"--cache=yes".to_string()), "{mpv:?}");
        let profile = mpv
            .iter()
            .position(|a| a == "--profile=low-latency")
            .expect("the low-latency profile");
        // One lavf list (a second would replace the first), restating the
        // profile's own fflags.
        let lavf: Vec<&String> = mpv
            .iter()
            .filter(|a| a.starts_with("--demuxer-lavf-o"))
            .collect();
        assert_eq!(lavf.len(), 1, "{mpv:?}");
        for opt in [
            "fflags=+nobuffer",
            "max_delay=0",
            "reorder_queue_size=0",
            "buffer_size=4194304",
        ] {
            assert!(lavf[0].contains(opt), "{opt} missing from {}", lavf[0]);
        }
        let audio = mpv
            .iter()
            .position(|a| a == "--audio-buffer=0.1")
            .unwrap_or_else(|| panic!("no small audio buffer in {mpv:?}"));
        assert!(
            audio > profile,
            "the audio buffer must override the profile"
        );
        assert!(!mpv.contains(&"--audio-buffer=0.5".to_string()));
        let hacks = mpv
            .iter()
            .position(|a| a == "--video-latency-hacks=no")
            .unwrap_or_else(|| panic!("latency hacks left on: {mpv:?}"));
        assert!(hacks > profile, "must override the profile");
        assert!(mpv.contains(&"--script-opts=streampipe-aggressive=yes".to_string()));
        assert!(mpv.contains(&"--script=/tmp/bs.lua".to_string()));
        assert_eq!(mpv.last().unwrap(), "/tmp/x.sdp");

        let ffplay = Player::Ffplay.command_for(path, buffer, None, 100, None, true);
        assert_eq!(pair(&ffplay, "-max_delay"), "0");
        assert_eq!(pair(&ffplay, "-reorder_queue_size"), "0");
        assert_eq!(pair(&ffplay, "-flags"), "low_delay");
        assert_eq!(pair(&ffplay, "-fflags"), "nobuffer");
        assert!(!ffplay.contains(&"+discardcorrupt".to_string()));
        assert_eq!(ffplay.last().unwrap(), "/tmp/x.sdp");

        let vlc = Player::Vlc.command_for(path, buffer, None, 100, None, true);
        assert!(vlc.contains(&"--network-caching=30".to_string()), "{vlc:?}");
    }
    // The standard tuning is `command_for(.., false)`, and never carries the
    // aggressive script option.
    assert_eq!(
        Player::Ffplay.command(path, None, None, 100, None),
        Player::Ffplay.command_for(path, None, None, 100, None, false)
    );
    let mpv = Player::Mpv.command(path, None, None, 100, script);
    assert!(
        !mpv.iter().any(|a| a.starts_with("--script-opts")),
        "{mpv:?}"
    );
}

#[test]
fn sdp_path_honours_explicit_here_and_default() {
    // A named file wins as-is.
    let named = sdp_path(Some("/tmp/mine.sdp".into()), false, 5004);
    assert_eq!(named, std::path::Path::new("/tmp/mine.sdp"));
    // A bare `--sdp`, or no player, drops the default name in the cwd.
    let here = sdp_path(None, true, 5004);
    assert!(here.starts_with(std::env::current_dir().unwrap()));
    assert_eq!(
        here.file_name().unwrap().to_str().unwrap(),
        "streampipe-5004.sdp"
    );
    // A launched player with no path keeps it in the temp dir.
    let tmp = sdp_path(None, false, 5004);
    assert!(tmp.starts_with(std::env::temp_dir()));
    assert!(!tmp.starts_with(std::env::current_dir().unwrap()));
}

#[tokio::test]
async fn forward_writes_rtp_and_asks_for_a_keyframe_on_a_gap() {
    let player = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = player.local_addr().unwrap().port();
    let sender = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
    let counters = Arc::new(Counters::default());
    let (kf_tx, mut kf_rx) = mpsc::channel(8);
    let mut last = None;
    let mut last_kf = None;

    let packet = |seq: u16| rtp::serialize_rtp(&info(96, seq), b"frame");

    forward(
        &sender,
        &packet(10),
        port,
        &counters,
        &mut last,
        &mut last_kf,
        true,
        &kf_tx,
    )
    .await;
    forward(
        &sender,
        &packet(11),
        port,
        &counters,
        &mut last,
        &mut last_kf,
        true,
        &kf_tx,
    )
    .await;
    assert_eq!(counters.forwarded.load(Ordering::Relaxed), 2);
    assert_eq!(counters.undeliverable.load(Ordering::Relaxed), 0);
    assert!(kf_rx.try_recv().is_err(), "no gap, no request");

    forward(
        &sender,
        &packet(20),
        port,
        &counters,
        &mut last,
        &mut last_kf,
        true,
        &kf_tx,
    )
    .await;
    assert_eq!(kf_rx.try_recv(), Ok(()), "a gap asks for a keyframe");

    // a second gap within the throttle interval does not ask again: on a
    // lossy path that would be a keyframe storm
    forward(
        &sender,
        &packet(30),
        port,
        &counters,
        &mut last,
        &mut last_kf,
        true,
        &kf_tx,
    )
    .await;
    assert!(kf_rx.try_recv().is_err(), "the second gap is throttled");

    // audio is not a video sequence, its gaps mean nothing here
    forward(
        &sender,
        &packet(40),
        port,
        &counters,
        &mut last,
        &mut last_kf,
        false,
        &kf_tx,
    )
    .await;
    assert!(kf_rx.try_recv().is_err(), "audio gaps are not video gaps");

    // the player must see exactly the bytes we were fed
    let mut buf = [0u8; 1024];
    for seq in [10u16, 11, 20, 30, 40] {
        let (n, _) = player.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &packet(seq)[..], "packet {seq}");
    }
}

#[tokio::test]
async fn forward_survives_a_player_that_is_not_listening() {
    // Nothing is bound on port 1. Whether the kernel reports the packet as
    // undeliverable depends on it having seen the ICMP error before, so
    // this asserts the invariant, not the split: the packet is accounted
    // for and the tunnel keeps going.
    let sender = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
    let counters = Arc::new(Counters::default());
    let (kf_tx, mut kf_rx) = mpsc::channel(8);
    let mut last = None;
    let mut last_kf = None;

    forward(
        &sender,
        &rtp::serialize_rtp(&info(96, 1), b"frame"),
        1,
        &counters,
        &mut last,
        &mut last_kf,
        true,
        &kf_tx,
    )
    .await;

    let forwarded = counters.forwarded.load(Ordering::Relaxed);
    let undeliverable = counters.undeliverable.load(Ordering::Relaxed);
    assert_eq!(forwarded + undeliverable, 1, "accounted for exactly once");
    assert!(kf_rx.try_recv().is_err(), "the first packet is no gap");

    forward(
        &sender,
        &rtp::serialize_rtp(&info(96, 9), b"frame"),
        1,
        &counters,
        &mut last,
        &mut last_kf,
        true,
        &kf_tx,
    )
    .await;
    assert_eq!(
        kf_rx.try_recv(),
        Ok(()),
        "a gap is a gap whether or not it was delivered"
    );
}

#[test]
fn media_tally_counts_bitrate_frames_and_keyframes() {
    let start = Instant::now();
    let mut tally = MediaTally::new(start);
    // 1250 bytes over exactly one second is 10 kbit/s.
    tally.bytes = 1250;
    tally.packets = 5;
    let one_sec = (tally.kbit_per_second(start + Duration::from_secs(1)) - 10.0).abs();
    assert!(one_sec < 0.001, "bitrate was {one_sec} off");
    // A zero length interval must not divide by zero.
    assert_eq!(tally.kbit_per_second(start), 0.0);
    assert!(tally.active());
    tally.restart(start + Duration::from_secs(1));
    assert_eq!(tally.packets, 0);
    assert_eq!(tally.bytes, 0);
    assert!(!tally.active());
}

#[test]
fn stream_report_log_resets_the_interval_but_keeps_totals() {
    let start = Instant::now();
    let mut report = StreamReport::new(start);
    // Nothing recorded yet: reporting is a no-op and must not panic.
    report.log(start + Duration::from_secs(5), 0, false);
    report.record(MediaKind::Video, start, true, true, 1000);
    report.record(MediaKind::Audio, start, true, false, 500);
    assert_eq!(report.video.packets, 1);
    assert_eq!(report.video.frames, 1);
    assert_eq!(report.video.keyframes, 1);
    assert_eq!(report.audio.packets, 1);
    report.log(start + Duration::from_secs(5), 3, false);
    // The per interval tallies reset, the running keyframe total does not.
    assert_eq!(report.video.packets, 0);
    assert_eq!(report.video.keyframes, 0);
    assert_eq!(report.video.keyframes_total, 1);
    assert_eq!(report.audio.packets, 0);
}

#[test]
fn human_duration_reads_like_a_stopwatch() {
    assert_eq!(human_duration(Duration::from_secs(42)), "42s");
    assert_eq!(human_duration(Duration::from_secs(7 * 60 + 13)), "7m13s");
    assert_eq!(
        human_duration(Duration::from_secs(3600 + 2 * 60 + 3)),
        "1h02m03s"
    );
}

#[tokio::test]
async fn wait_until_listening_does_not_hog_the_port() {
    // Regression: the probe used to hold the RTP port across its sleep, so
    // the player it was waiting for could never bind it. The port must stay
    // bindable by the player while we wait, and the wait must end once the
    // player has it.
    let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let wait = tokio::spawn(wait_until_listening(port));

    let mut player = None;
    for _ in 0..40 {
        match UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(socket) => {
                player = Some(socket);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    let _player = player.expect("the player never got the port it was waiting for");

    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .expect("wait_until_listening did not return after the player bound")
        .unwrap();
}

/// A player that is slow to open its port starts at the live edge.
///
/// mpv takes a second or so to open its RTP port. The viewer used to wait for
/// it inside its datagram loop, so QUIC queued that second of media and the
/// player got it in one burst once it listened: it started a second behind
/// live, and the speed-up that drained it pulled the audio away from the
/// picture. The media in the meantime is dropped now; once the port is open
/// the viewer asks for a keyframe, and the player starts there.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_player_starts_at_the_live_edge() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use bytes::Bytes;
    use iroh::{endpoint::presets, Endpoint, EndpointAddr};
    use streampipe::{
        rtp::{ParameterSets, TAG_EPOCH, TAG_KEYFRAME_REQ, TAG_SESSION, TAG_VIDEO},
        sdp::{MediaHeader, SessionHeader},
        stream::{connect_whip, ViewerConfig},
    };

    let loopback = || {
        Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
    };
    let host = loopback()
        .alpns(vec![streampipe::STREAM_ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let viewer = loopback().bind().await.unwrap();
    let host_addr = EndpointAddr::new(host.id()).with_ip_addr(host.bound_sockets()[0]);

    // The "player" only records that it was started; the test opens its port
    // for it, a second later.
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("player.pid");
    let script = dir.path().join("player.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let play = probe.local_addr().unwrap().port() & !1;
    drop(probe);

    let cfg = ViewerConfig {
        addr: host_addr,
        alpn: streampipe::STREAM_ALPN.to_vec(),
        play: (Ipv4Addr::LOCALHOST, play).into(),
        sdp: Some(dir.path().join("x.sdp")),
        sdp_here: false,
        player: Player::Mpv,
        player_path: Some(script),
        no_launch: false,
        buffer: None,
        low_latency: false,
        volume: 100,
        stats: false,
        media_state: None,
    };
    let viewing = tokio::spawn(connect_whip(viewer, cfg));

    // The host: a session header, then one video packet every 5 ms, with an
    // epoch (a keyframe) at the start and whenever the viewer asks for one.
    let conn = host
        .accept()
        .await
        .expect("an incoming viewer")
        .accept()
        .expect("accepting")
        .await
        .expect("a connection");
    let (_send, mut recv) = conn.accept_bi().await.unwrap();
    let mut hello = [0u8; 5];
    recv.read_exact(&mut hello).await.unwrap();
    assert_eq!(hello, streampipe::HANDSHAKE);
    let header = SessionHeader {
        gen: 1,
        media: vec![MediaHeader {
            kind: MediaKind::Video,
            pt: 96,
            codec: "H264".into(),
            clock: 90_000,
            channels: 1,
            fmtp: String::new(),
            sets: ParameterSets::default(),
        }],
    }
    .encode();
    let tagged = |tag: u8, body: &[u8]| Bytes::from([&[tag][..], body].concat());
    let sent: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let hosting = {
        let (conn, sent) = (conn.clone(), sent.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(5));
            let mut keyframe = true;
            let mut seq = 0u16;
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    datagram = conn.read_datagram() => {
                        match datagram {
                            Ok(d) if d.first() == Some(&TAG_KEYFRAME_REQ) => keyframe = true,
                            Ok(_) => {}
                            Err(_) => return,
                        }
                        continue;
                    }
                }
                if seq.is_multiple_of(200) {
                    let _ = conn.send_datagram(tagged(TAG_SESSION, &header));
                }
                if std::mem::take(&mut keyframe) {
                    let _ = conn.send_datagram(tagged(TAG_EPOCH, &[]));
                }
                let packet = rtp::serialize_rtp(&info(96, seq), &[0x65, 0x88, 0x84]);
                sent.lock().unwrap().push(Instant::now());
                let _ = conn.send_datagram(tagged(TAG_VIDEO, &packet));
                seq += 1;
            }
        })
    };

    // The player starts, then takes a second to open its port.
    let started = Instant::now();
    while !pid_file.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no player launched"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let player = loop {
        // The viewer's own probe holds the port for an instant now and then.
        match UdpSocket::bind((Ipv4Addr::LOCALHOST, play)).await {
            Ok(socket) => break socket,
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    };
    let listening = Instant::now();

    let mut buf = [0u8; 2048];
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), player.recv_from(&mut buf))
        .await
        .expect("no media reached the player")
        .unwrap();
    let (first, _) = rtp::parse_header(&buf[..len]).expect("an rtp packet");
    let sent_at = sent.lock().unwrap()[first.seq as usize];
    assert!(
        sent_at >= listening,
        "the player's first packet is {:?} older than its port",
        listening - sent_at
    );

    hosting.abort();
    viewing.abort();
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        // Usually gone already: the viewer stops its player when dropped.
        let _ = std::process::Command::new("kill")
            .arg(pid.trim())
            .stderr(std::process::Stdio::null())
            .status();
    }
}
