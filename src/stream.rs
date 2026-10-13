//! WHIP ingest and RTP playback over an iroh connection.
//!
//! The only WebRTC on this path is the ingest leg: OBS speaks WHIP (the
//! WebRTC-HTTP ingestion protocol) to a small HTTP server, str0m terminates that
//! one peer connection (ICE, DTLS, SRTP) on the host, and everything downstream
//! is plain RTP.
//!
//! The host is the *media authority*. It knows what the publisher offered and
//! which payload types it actually sends, so it can tell a viewer everything it
//! needs to write a playable SDP ([`crate::sdp::SessionHeader`]), and it gates
//! the media it forwards per viewer so that a viewer that joins in the middle of
//! a GOP starts at a keyframe.
//!
//! The media path is unbuffered: every RTP packet is forwarded as one QUIC
//! datagram as soon as it arrives, so there is no head-of-line blocking. A
//! packet larger than the connection's `max_datagram_size()` is fragmented into
//! several datagrams and reassembled by the viewer.
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use clap::ValueEnum;
use iroh::{
    endpoint::{Accepting, Connection, RecvStream, SendDatagramError, SendStream},
    Endpoint, EndpointAddr,
};
use n0_error::{anyerr, bail_any, ensure_any, Result, StdResultExt};
use str0m::{
    change::SdpOffer,
    media::{KeyframeRequestKind, MediaKind as Str0mMediaKind, Mid},
    net::{Protocol, Receive},
    rtp::RtpPacket,
    Candidate, Event, Input, Output, Rtc,
};
use tokio::{
    net::UdpSocket,
    process::Command,
    sync::mpsc,
    task::JoinSet,
    time::{interval, sleep, sleep_until, MissedTickBehavior},
};

use crate::{
    rtp::{
        self, reorder_timeout_for, Codec, ParameterSets, Reorder, RtpInfo, TAG_AUDIO, TAG_EPOCH,
        TAG_FRAGMENT, TAG_KEYFRAME_REQ, TAG_SESSION, TAG_VIDEO,
    },
    sdp::{self, MediaHeader, MediaKind, OfferMedia, SessionHeader},
    whip::{self, AbortOnDrop, Answer, WhipRequest},
};

/// The local address media is played out on when the user does not pick one.
pub const DEFAULT_PLAY_ADDR: &str = "127.0.0.1:5004";
/// The address OBS posts its WHIP offer to by default.
pub const DEFAULT_WHIP_ADDR: &str = "127.0.0.1:8080";

/// How often the session header is repeated to every viewer.
///
/// It is the only way a viewer learns payload type, codec and parameter sets, so
/// it is resent, not sent once: a lost datagram must not cost a viewer the stream.
const HEADER_INTERVAL: Duration = Duration::from_secs(1);
/// How long a viewer waits for its player to open the RTP socket.
const PLAYER_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a viewer waits for media before asking for a keyframe.
const STALL_INTERVAL: Duration = Duration::from_secs(2);
/// The shortest interval between two keyframe requests from one viewer.
///
/// A gap nudges the encoder to refresh, but `last_keyframe` is refreshed by every
/// keyframe that arrives, so a gap right after an IDR asks for nothing. Without
/// this, on a lossy path nearly every frame has a gap and a PLI per gap makes the
/// encoder produce IDR after IDR, blowing up bitrate and dropping more packets.
const KF_REQUEST_INTERVAL: Duration = Duration::from_secs(2);
/// A path with at least this round-trip time counts as long-distance and gets
/// an automatic jitter buffer for the player.
const WAN_RTT: Duration = Duration::from_millis(50);
/// How often a viewer reports that it is waiting for media.
const WAIT_INTERVAL: Duration = Duration::from_secs(5);
/// How often periodic counters are logged while a tunnel runs.
pub const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// How long a viewer waits before redialing a host that went away.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Capacity of the per-viewer media queue.
const VIEWER_QUEUE: usize = 8192;
/// Size of the buffer used to read from the ICE UDP socket.
pub const ICE_BUF: usize = 2048;
/// How far below the level in the GUI ffplay is started, in percentage points.
///
/// ffplay's `-volume` scale falls away much more slowly than mpv's `--volume` or
/// VLC's `--gain`, so the same number leaves ffplay noticeably louder. Starting it
/// 20 points lower lines the three up by ear (GUI 100 → ffplay 80, GUI 20 → 0).
const FFPLAY_VOLUME_OFFSET: u8 = 20;
/// Initial size of the launched player window, in pixels: 720p.
///
/// All three players open at this size, anchored top-left, so the window is as big
/// as it can be while fitting small displays (1366x768) entirely on screen, with
/// room for a taskbar and no chance of the picture falling off the edge.
const WINDOW_WIDTH: u32 = 1280;
/// Height of the launched player window, in pixels (see [`WINDOW_WIDTH`]).
const WINDOW_HEIGHT: u32 = 720;

/// The players `connect-whip` knows how to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum Player {
    /// mpv, with its `low-latency` profile.
    #[default]
    #[value(alias = "mvp")]
    Mpv,
    /// VLC, with a small network cache.
    Vlc,
    /// ffplay, from the ffmpeg distribution.
    Ffplay,
    /// Do not launch anything, just write the SDP file.
    None,
}

impl Player {
    /// Used to play the SDP file.
    pub fn command(
        &self,
        sdp: &Path,
        buffer: Option<Duration>,
        path: Option<&Path>,
        volume: u8,
        script: Option<&Path>,
    ) -> Vec<String> {
        let file = sdp.display().to_string();
        // The custom path replaces the binary name, the flags stay the player's.
        let bin = |default: &str| {
            path.map(|p| p.display().to_string())
                .unwrap_or_else(|| default.to_string())
        };
        match self {
            Player::Mpv => {
                let mut cmd = vec![
                    bin("mpv"),
                    // Ignore the user's config so the flags below are the whole
                    // story: no config-file options can quietly re-add buffering.
                    "--no-config".to_string(),
                    "--force-window=immediate".to_string(),
                    // Open as a 720p window in the top-left corner: the largest
                    // window that still fits a small laptop screen whole, anchored
                    // at 0,0 so no window manager can push it off the edge.
                    format!("--geometry={WINDOW_WIDTH}x{WINDOW_HEIGHT}+0+0"),
                    // Start at the chosen level: mpv's `--volume` is a percentage
                    // of full scale (its own default is 100).
                    format!("--volume={volume}"),
                    // A live stream must start at the live edge, never resume a
                    // watched position from a previous run.
                    "--no-resume-playback".to_string(),
                ];
                match buffer {
                    Some(ms) => {
                        cmd.push("--cache=yes".into());
                        cmd.push(format!("--demuxer-readahead-secs={}", ms.as_secs_f64()));
                        // `max_delay` is the RTP demuxer's reorder window in us,
                        // `buffer_size` the UDP receive socket it opens. They must
                        // go in ONE `--demuxer-lavf-o`: it is a key/value list
                        cmd.push(format!(
                            "--demuxer-lavf-o=max_delay={},buffer_size=4194304",
                            ms.as_micros()
                        ));
                    }
                    None => {
                        cmd.push("--no-cache".into());
                        cmd.push("--profile=low-latency".into());
                        // Enlarge the RTP/UDP receive socket buffer that lavf opens
                        // for the SDP. mpv's default is the OS default (~200 KB).
                        cmd.push("--demuxer-lavf-o=buffer_size=4194304".into());
                    }
                }
                // The control script changes `speed` to drain the buffer. The
                // low-latency profile sets `audio-buffer=0`, leaving the audio
                // pipeline no slack: every speed change resamples the audio
                // (scaletempo2) and it drifts ~10% off the video. Give the audio
                // output real slack so resampled audio tracks the speed changes.
                // Must come after `--profile=low-latency` to override its
                // `audio-buffer=0` (last occurrence of a scalar wins).
                cmd.push("--audio-buffer=0.5".into());
                // The buffer-drain control script, staged on disk by `launch()`;
                // goes before the SDP, which must stay last.
                if let Some(script) = script {
                    cmd.push(format!("--script={}", script.display()));
                }
                cmd.push(file);
                cmd
            }
            Player::Vlc => {
                let caching = buffer.map(|d| d.as_millis()).unwrap_or(100);
                vec![
                    bin("vlc"),
                    format!("--network-caching={caching}"),
                    // VLC 3 has no `--volume` any more (it is accepted as a legacy
                    // name and only warns "option --volume no longer exists"), so
                    // the startup level goes through the audio-output `--gain`, a
                    // linear multiplier where 1.0 is full scale.
                    format!("--gain={:.2}", f64::from(volume) / 100.0),
                    // The window: 720p, pinned to the top-left corner of the
                    // screen, so it fits a small display whole instead of opening
                    // at the video's own size (which can exceed the screen).
                    format!("--width={WINDOW_WIDTH}"),
                    format!("--height={WINDOW_HEIGHT}"),
                    "--video-x=0".into(),
                    "--video-y=0".into(),
                    "--no-loop".into(),
                    file,
                ]
            }
            Player::Ffplay => {
                // ffplay's `-volume` is its startup volume, 0=mute 100=max, started
                // `FFPLAY_VOLUME_OFFSET` points below the GUI level so one slider
                // setting sounds the same in all three players.
                let ffplay_volume = volume.saturating_sub(FFPLAY_VOLUME_OFFSET);
                let mut cmd = vec![
                    bin("ffplay"),
                    "-volume".into(),
                    ffplay_volume.to_string(),
                    // The window: `-x`/`-y` force the window size (720p, which every
                    // screen can show whole), `-left`/`-top` pin it to the top-left
                    // corner so it cannot be placed off-screen.
                    "-x".into(),
                    WINDOW_WIDTH.to_string(),
                    "-y".into(),
                    WINDOW_HEIGHT.to_string(),
                    "-left".into(),
                    "0".into(),
                    "-top".into(),
                    "0".into(),
                ];
                match buffer {
                    Some(ms) => {
                        // Drop corrupt packets instead of feeding them to the
                        // decoder, and let the RTP receiver wait for reordering.
                        cmd.extend(["-fflags".into(), "+discardcorrupt".into()]);
                        cmd.extend(["-max_delay".into(), ms.as_micros().to_string()]);
                    }
                    None => {
                        cmd.extend(["-fflags".into(), "nobuffer".into()]);
                    }
                }
                cmd.extend([
                    // Enlarge the UDP/RTP receive socket buffer so a brief decode
                    // hiccup at high bitrate does not overflow the kernel default
                    // (~200 KB) and silently drop inbound RTP.
                    "-buffer_size".into(),
                    "4194304".into(),
                    "-probesize".into(),
                    "1000000".into(),
                    "-analyzeduration".into(),
                    "0".into(),
                    // The SDP refers to rtp/udp, which lavfi's protocol whitelist
                    // rejects by default.
                    "-protocol_whitelist".into(),
                    "file,rtp,udp,crypto,data".into(),
                ]);
                cmd.push(file);
                cmd
            }
            Player::None => vec![file],
        }
    }

    /// Whether a player process is launched at all.
    pub fn launches(&self) -> bool {
        *self != Player::None
    }
}

/// Counters for a running media tunnel, used for logging.
#[derive(Debug, Default)]
pub struct Counters {
    /// RTP packets forwarded to viewers.
    pub forwarded: AtomicU64,
    /// Bytes of those packets.
    pub forwarded_bytes: AtomicU64,
    /// Packets dropped because they were too large for a QUIC datagram.
    pub dropped: AtomicU64,
    /// Packets dropped because they were not a negotiated media payload type.
    pub unwanted: AtomicU64,
    /// Packets that could not be delivered.
    pub undeliverable: AtomicU64,
    /// Fragment datagrams sent for packets too large for one datagram.
    pub fragmented: AtomicU64,
    /// Keyframe requests.
    pub keyframes: AtomicU64,
}

impl Counters {
    pub fn get(&self, counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    fn log_periodic(&self) {
        tracing::debug!(
            "media tunnel stats: {} packets / {} bytes forwarded, \
             {} fragmented, {} oversized dropped, {} unwanted payload types dropped, \
             {} undeliverable, {} keyframe requests",
            self.get(&self.forwarded),
            self.get(&self.forwarded_bytes),
            self.get(&self.fragmented),
            self.get(&self.dropped),
            self.get(&self.unwanted),
            self.get(&self.undeliverable),
            self.get(&self.keyframes),
        );
    }

    fn log_final(&self) {
        tracing::info!(
            "media tunnel closed: {} packets / {} bytes forwarded, \
             {} fragmented, {} oversized dropped, {} unwanted payload types dropped, \
             {} undeliverable, {} keyframe requests",
            self.get(&self.forwarded),
            self.get(&self.forwarded_bytes),
            self.get(&self.fragmented),
            self.get(&self.dropped),
            self.get(&self.unwanted),
            self.get(&self.undeliverable),
            self.get(&self.keyframes),
        );
    }

    /// Record a packet that could not even be fragmented.
    fn record_dropped(&self, size: usize, max: usize) {
        let count = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // log the first one and then every 1000th, so a misconfigured sender
        // is visible without flooding the log.
        if count == 1 || count.is_multiple_of(1000) {
            tracing::warn!(
                "dropped unfragmentable rtp datagram ({size} > {max} bytes), \
                 {count} dropped so far; lower the packet size of the publisher",
            );
        }
    }
}

/// Rolling media statistics of one media kind.
///
/// Counters are for the current [`STATS_INTERVAL`] and are reset when the
/// interval is reported, so the numbers in the line are the numbers of the
/// last five seconds rather than of the whole session.
#[derive(Debug)]
pub struct MediaTally {
    /// When the current interval started.
    pub since: Instant,
    /// RTP packets seen in this interval.
    pub packets: u64,
    /// Bytes of those packets, RTP header included.
    pub bytes: u64,
    /// Frames completed in this interval, counted on the RTP marker bit.
    pub frames: u64,
    /// Keyframes seen in this interval.
    pub keyframes: u64,
    /// Keyframes seen since the session started.
    pub keyframes_total: u64,
    /// When the last keyframe arrived, to report the keyframe interval.
    pub last_keyframe: Option<Instant>,
}

impl MediaTally {
    pub fn new(now: Instant) -> Self {
        MediaTally {
            since: now,
            packets: 0,
            bytes: 0,
            frames: 0,
            keyframes: 0,
            keyframes_total: 0,
            last_keyframe: None,
        }
    }

    /// Whether any media arrived in this interval.
    pub fn active(&self) -> bool {
        self.packets > 0
    }

    /// The average bitrate of this interval in kbit/s.
    pub fn kbit_per_second(&self, now: Instant) -> f64 {
        let secs = now.duration_since(self.since).as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        (self.bytes as f64 * 8.0) / secs / 1000.0
    }

    /// Start a new interval.
    pub fn restart(&mut self, now: Instant) {
        self.since = now;
        self.packets = 0;
        self.bytes = 0;
        self.frames = 0;
        self.keyframes = 0;
    }
}

/// What the publisher is sending, so the operator can see it working.
///
/// Reported every [`STATS_INTERVAL`] by [`Host::report_media`] and again when
/// the input closes.
#[derive(Debug)]
pub struct StreamReport {
    /// When the first media packet of this session arrived.
    pub started: Option<Instant>,
    /// The video tally.
    pub video: MediaTally,
    /// The audio tally.
    pub audio: MediaTally,
}

impl StreamReport {
    pub fn new(now: Instant) -> Self {
        StreamReport {
            started: None,
            video: MediaTally::new(now),
            audio: MediaTally::new(now),
        }
    }

    /// Account for one RTP packet.
    pub fn record(
        &mut self,
        kind: MediaKind,
        now: Instant,
        marker: bool,
        keyframe: bool,
        bytes: u64,
    ) {
        self.started.get_or_insert(now);
        let tally = match kind {
            MediaKind::Video => &mut self.video,
            MediaKind::Audio => &mut self.audio,
        };
        tally.packets += 1;
        tally.bytes += bytes;
        // The marker bit ends a video frame and marks the last audio packet of
        // a talkspurt, which is close enough to count frames and packets.
        if marker {
            tally.frames += 1;
        }
        if keyframe {
            tally.keyframes += 1;
            tally.keyframes_total += 1;
            tally.last_keyframe = Some(now);
        }
    }

    /// Print the periodic report and start a new interval.
    ///
    /// Silence is reported too: a publisher that stopped sending is the one thing
    /// a line full of zeroes says best.
    ///
    /// `gui_mode` sends the line through `tracing` (whose writer is the
    /// non-blocking tee) instead of blocking a media worker on synchronous
    /// `stderr`. The CLI keeps `eprintln!`.
    pub fn log(&mut self, now: Instant, viewers: usize, gui_mode: bool) {
        let Some(started) = self.started else {
            return;
        };
        let video = &self.video;
        let audio = &self.audio;
        if !video.active() && !audio.active() {
            // Nothing arrived: still restart the tallies, so the next interval is
            // measured from *now*. Returning early leaves `since` far behind, and
            // the next report then divides its bytes by a ballooned interval,
            // which reads as a wildly deflated bitrate after every stream gap.
            self.video.restart(now);
            self.audio.restart(now);
            return;
        }
        let secs = now.duration_since(video.since).as_secs_f64().max(0.001);
        let mut line = format!(
            "whip {elapsed}: ",
            elapsed = human_duration(now.duration_since(started))
        );
        if video.active() {
            let fps = video.frames as f64 / secs;
            let since_kf = video
                .last_keyframe
                .map(|at| format!("{:.1}s ago", now.duration_since(at).as_secs_f64()))
                .unwrap_or_else(|| "none yet".to_string());
            line.push_str(&format!(
                "video {packets} pkt {video} kbit/s {fps:.1} fps, \
                 {keyframes} keyframe(s) in this interval, last {since_kf}, \
                 {total} in total",
                video = video.kbit_per_second(now).round(),
                packets = video.packets,
                keyframes = video.keyframes,
                total = video.keyframes_total,
            ));
        } else {
            line.push_str("no video in this interval");
        }
        if audio.active() {
            line.push_str(&format!(
                ", audio {packets} pkt {audio} kbit/s",
                packets = audio.packets,
                audio = audio.kbit_per_second(now).round(),
            ));
        }
        line.push_str(&format!(", {viewers} viewer(s)"));
        if gui_mode {
            tracing::info!("{line}");
        } else {
            eprintln!("{line}");
        }
        self.video.restart(now);
        self.audio.restart(now);
    }

    /// Print the final summary of the session.
    ///
    /// `gui_mode` routes the line through `tracing` (see [`Self::log`]).
    fn log_final(&self, now: Instant, gui_mode: bool) {
        let Some(started) = self.started else {
            let line = "whip input closed without ever receiving media";
            if gui_mode {
                tracing::info!("{line}");
            } else {
                eprintln!("{line}");
            }
            return;
        };
        let secs = now.duration_since(started).as_secs_f64().max(0.001);
        let bytes = self.video.bytes + self.audio.bytes;
        let kbit = (bytes as f64 * 8.0 / secs / 1000.0).round();
        let line = format!(
            "whip input closed after {elapsed}: {kbit} kbit/s average of \
             {bytes} bytes, video {packets} packet(s) with {keyframes} keyframe(s)",
            elapsed = human_duration(now.duration_since(started)),
            keyframes = self.video.keyframes_total,
            packets = self.video.packets,
        );
        if gui_mode {
            tracing::info!("{line}");
        } else {
            eprintln!("{line}");
        }
    }
}

/// Format a duration as `42s`, `7m13s` or `1h02m03s`.
pub fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, s) => format!("{h}h{m:02}m{s:02}s"),
    }
}

/// Whether errors when sending to a local UDP socket are transient.
///
/// A player that is not running yet, or that just quit, is normal. On Linux a
/// previous `send` to a closed port shows up as `ConnectionRefused` on the next
/// operation, on Windows as `ConnectionReset`.
fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::NotConnected
    )
}

/// The media kind of a str0m media.
fn kind_of(kind: Str0mMediaKind) -> MediaKind {
    match kind {
        Str0mMediaKind::Video => MediaKind::Video,
        Str0mMediaKind::Audio => MediaKind::Audio,
    }
}

/// A message from the media session to exactly one viewer.
#[derive(Debug)]
pub enum Frame {
    /// The session header, so the viewer can (re)write its SDP.
    Header(Bytes),
    /// The next media packet is the start of a keyframe.
    Epoch,
    /// A video RTP packet.
    Video(Bytes),
    /// An audio RTP packet.
    Audio(Bytes),
}

/// What the viewer acceptor tells the media session.
#[derive(Debug)]
pub enum ViewerEvent {
    /// A viewer dialed in and announced its local RTP port.
    Joined {
        /// Identifier of this viewer.
        id: u64,
        /// The local RTP port the viewer plays out on.
        port: u16,
        /// Where to send the media of this viewer.
        tx: mpsc::Sender<Frame>,
    },
    /// A viewer asked for a keyframe, e.g. because it just started its player
    /// or because it detected a gap.
    Keyframe {
        /// Identifier of the viewer.
        id: u64,
    },
    /// A viewer is gone.
    Left {
        /// Identifier of the viewer.
        id: u64,
    },
}

/// Identifier assigned to the next viewer.
static NEXT_VIEWER: AtomicU64 = AtomicU64::new(1);

/// A snapshot of the live host statistics, sent to the GUI over a channel.
///
/// The CLI never receives these (its `stats_tx` is `None`), so its stderr
/// output is unchanged. The GUI renders this in its statistics panel. Rates
/// are the average over the last reporting interval; `viewers` is the live
/// count and is refreshed immediately when a viewer joins or leaves, so the
/// count does not flicker to a stale value between the periodic ticks.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamStats {
    /// Seconds since the broadcast session started.
    pub elapsed_secs: u64,
    /// Number of viewers currently connected.
    pub viewers: usize,
    /// Video bitrate of the last interval in kbit/s (0 when no video arrived).
    pub video_kbps: f64,
    /// Audio bitrate of the last interval in kbit/s (0 when no audio arrived).
    pub audio_kbps: f64,
    /// Video frame rate of the last interval (0 when no video arrived).
    pub video_fps: f64,
}

/// Configuration of the WHIP host.
pub struct WhipConfig {
    /// The HTTP address to serve WHIP on.
    pub listen: SocketAddr,
    /// The bearer token OBS must present, if any.
    pub bearer_token: Option<String>,
    /// The address to bind the ICE socket on, if not derived from `listen`.
    pub ice_addr: Option<SocketAddr>,
    /// Whether to log periodic counters.
    pub stats: bool,
    /// Optional channel for structured live statistics (used by the GUI).
    ///
    /// `None` for the CLI, which keeps its log-only behaviour.
    pub stats_tx: Option<mpsc::UnboundedSender<StreamStats>>,
}

/// Listen for a WHIP stream from OBS and forward it to connecting viewers.
///
/// Runs three concurrent parts until one of them fails or control-c is pressed:
/// the WHIP HTTP server, the viewer acceptor on the iroh endpoint, and the
/// media session loop that owns the [`Rtc`].
pub async fn listen_whip(endpoint: Endpoint, cfg: WhipConfig) -> Result<()> {
    let ice = Arc::new(bind_ice(cfg.ice_addr, cfg.listen.ip()).await?);
    let ice_addr = ice
        .local_addr()
        .std_context("error getting the ice socket address")?;
    tracing::info!("ice candidate is udp://{ice_addr}");

    let (req_tx, req_rx) = mpsc::channel::<WhipRequest>(16);
    let (viewer_tx, viewer_rx) = mpsc::channel::<ViewerEvent>(32);

    // Fan the three concurrent parts onto their own tasks, kept in a guarded set.
    // Polling them on one task (the old `select!` over bare futures) pinned the
    // whole ingest pipeline to one OS thread at a time; on the multi-threaded
    // runtime this lets `host_loop` run on a worker of its own, so HTTP serving
    // and viewer acceptance no longer share its scheduler slot and stall the media
    // path during a keyframe burst.
    //
    // The guard makes teardown complete: a plainly spawned task is *detached*, so
    // when the GUI aborts the session task driving `listen_whip` the three tasks
    // would keep running forever, holding the WHIP TCP listener and the ICE UDP
    // socket bound. In an `AbortOnDrop` set they die with this future and the
    // ports are handed back.
    let mut tasks = AbortOnDrop(JoinSet::new());
    tasks.spawn(whip::serve(cfg.listen, cfg.bearer_token.clone(), req_tx));
    tasks.spawn(accept_viewers(endpoint.clone(), viewer_tx));
    tasks.spawn(host_loop(
        ice,
        ice_addr,
        req_rx,
        viewer_rx,
        cfg.stats,
        cfg.stats_tx,
    ));

    let result = tokio::select! {
        // `join_next` is cancel-safe and never completes instantly here: the set
        // is never empty while this select is live, so the branch blocks until a
        // part actually finishes.
        res = tasks.join_next() => {
            match res {
                Some(Ok(res)) => res,
                Some(Err(e)) => Err(anyerr!("task panicked: {e}")),
                None => Ok(()),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("got ctrl-c, exiting");
            Ok(())
        }
    };
    tracing::info!("stopping whip input");
    // Aborting the set tears the components down exactly like dropping the old
    // inline futures: each owns an `AbortOnDrop` set (serve, accept), so
    // aborting its task also kills every connection/viewer it spawned.
    tasks.abort_all();
    endpoint.close().await;
    result
}

/// Bind the UDP socket that ICE/DTLS/SRTP runs on.
///
/// A loopback WHIP address gets a loopback socket, everything else gets the
/// address of the interface that routes to the internet, which is the address a
/// publisher on the local network can reach. Use `--ice-addr` to override that,
/// e.g. on a machine with several interfaces or a VPN.
async fn bind_ice(configured: Option<SocketAddr>, listen: IpAddr) -> Result<UdpSocket> {
    let bind = match configured {
        Some(addr) => addr,
        None if listen.is_loopback() => SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        None => SocketAddr::from((primary_ipv4().unwrap_or(Ipv4Addr::UNSPECIFIED), 0)),
    };
    let socket = UdpSocket::bind(bind)
        .await
        .std_context(format!("error binding ice udp socket to {bind}"))?;
    // Maximize the kernel buffers so an instantaneous keyframe burst from the
    // publisher (OBS emits a 6 Mbps IDR as 300+ KB at once) does not overrun
    // the default SO_RCVBUF (~212 KB on Linux, 50 KB on macOS) and lose the
    // tail of the keyframe before str0m ever sees it. `let _ =` swallows an
    // error so a lower OS hard limit just clamps to the maximum allowed.
    let sock = socket2::SockRef::from(&socket);
    let _ = sock.set_recv_buffer_size(8 * 1024 * 1024);
    let _ = sock.set_send_buffer_size(8 * 1024 * 1024);
    Ok(socket)
}

/// The IPv4 address this machine would use to reach the internet.
///
/// A UDP `connect` does not send anything, it just makes the kernel pick the
/// source address of the route, which is exactly what we want to advertise as
/// our ICE candidate.
fn primary_ipv4() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(("8.8.8.8", 53)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(addr) => Some(addr),
        IpAddr::V6(_) => None,
    }
}

/// Accept viewers on the endpoint until the endpoint is closed.
///
/// Viewer tasks live in an [`AbortOnDrop`] set, mirroring [`whip::serve`]:
/// dropping this future (the GUI aborts the session on "Stop Broadcasting")
/// stops accepting *and* aborts every running viewer handler, so no viewer
/// connection, media forwarder, or liveness stream outlives the session.
async fn accept_viewers(endpoint: Endpoint, tx: mpsc::Sender<ViewerEvent>) -> Result<()> {
    let mut viewers = AbortOnDrop::<()>(JoinSet::new());
    loop {
        let accepting = tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    break;
                };
                match incoming.accept() {
                    Ok(accepting) => accepting,
                    Err(e) => {
                        tracing::warn!("error accepting connection: {e}");
                        continue;
                    }
                }
            }
            // Reap finished viewers so the set does not grow without bound;
            // the guard keeps the branch from completing instantly (and
            // spinning the loop) while the set is empty.
            Some(joined) = viewers.join_next(), if !viewers.is_empty() => {
                if let Err(cause) = joined {
                    tracing::debug!("viewer task cancelled: {cause}");
                }
                continue;
            }
        };
        let tx = tx.clone();
        viewers.spawn(async move {
            if let Err(cause) = handle_viewer(accepting, tx).await {
                tracing::warn!("error handling viewer: {cause}");
            }
        });
    }
    Ok(())
}

/// Handle one viewer connection.
///
/// The bidi stream is only used for the handshake and the port announcement, but
/// is kept open as a liveness signal while the media rides along on the same
/// connection as datagrams.
async fn handle_viewer(accepting: Accepting, tx: mpsc::Sender<ViewerEvent>) -> Result<()> {
    let conn = accepting.await.std_context("error accepting connection")?;
    let remote = conn.remote_id();
    tracing::info!("got viewer connection from {remote}");

    let max = match conn.max_datagram_size() {
        Some(max) => max,
        None => bail_any!("the viewer does not support quic datagrams"),
    };

    let (send, mut recv) = conn
        .accept_bi()
        .await
        .std_context("error accepting viewer stream")?;
    read_handshake(&mut recv).await?;
    let port = read_announcement(&mut recv).await?;
    tracing::info!("viewer {remote} plays out on udp://127.0.0.1:{port}");

    let id = NEXT_VIEWER.fetch_add(1, Ordering::Relaxed);
    let (frame_tx, mut frame_rx) = mpsc::channel::<Frame>(VIEWER_QUEUE);
    if tx
        .send(ViewerEvent::Joined {
            id,
            port,
            tx: frame_tx,
        })
        .await
        .is_err()
    {
        bail_any!("the media session is gone");
    }

    // The bidi stream is the liveness signal, dropping it here tells the viewer
    // that the host is gone.
    let _liveness = (send, recv);

    let counters = Arc::new(Counters::default());
    let forward = {
        let conn = conn.clone();
        let counters = counters.clone();
        tokio::spawn(async move {
            // The id of the next fragmented datagram.
            let mut frag_id: u16 = 0;
            while let Some(frame) = frame_rx.recv().await {
                let (tag, payload) = match frame {
                    Frame::Header(b) => (TAG_SESSION, b),
                    Frame::Epoch => (TAG_EPOCH, Bytes::new()),
                    Frame::Video(b) => (TAG_VIDEO, b),
                    Frame::Audio(b) => (TAG_AUDIO, b),
                };
                let mut buf = Vec::with_capacity(1 + payload.len());
                buf.push(tag);
                buf.extend_from_slice(&payload);
                // The limit can move with the path MTU estimate, so ask on
                // every packet rather than once.
                let max = conn.max_datagram_size().unwrap_or(max);
                if buf.len() > max {
                    // Too large for one datagram: fragment it instead of
                    // dropping it. A lost fragment is plain packet loss, the
                    // viewer's gap detection and keyframe gate recover from it.
                    let frags = match rtp::fragment_all(&buf, frag_id, max) {
                        Some(frags) => frags,
                        None => {
                            counters.record_dropped(buf.len(), max);
                            continue;
                        }
                    };
                    frag_id = frag_id.wrapping_add(1);
                    for frag in frags {
                        match conn.send_datagram(frag.into()) {
                            Ok(()) => {
                                counters.fragmented.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                tracing::debug!("cannot send fragment to viewer: {e}");
                                break;
                            }
                        }
                    }
                    continue;
                }
                match conn.send_datagram(buf.into()) {
                    Ok(()) => {
                        counters.forwarded.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(SendDatagramError::TooLarge) => {
                        counters.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::debug!("cannot send datagram to viewer: {e}");
                        break;
                    }
                }
            }
        })
    };

    let mut stats_tick = interval(STATS_INTERVAL);
    stats_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stats_tick.tick() => counters.log_periodic(),
            datagram = conn.read_datagram() => {
                match datagram {
                    Ok(data) => {
                        if data.first() == Some(&TAG_KEYFRAME_REQ) {
                            counters.keyframes.fetch_add(1, Ordering::Relaxed);
                            if tx.send(ViewerEvent::Keyframe { id }).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("viewer {remote} datagram stream ended: {e}");
                        break;
                    }
                }
            }
        }
    }

    counters.log_final();
    forward.abort();
    let _ = tx.send(ViewerEvent::Left { id }).await;
    Ok(())
}

/// Read the `hello` handshake from a viewer.
async fn read_handshake(recv: &mut RecvStream) -> Result<()> {
    let mut buf = [0u8; crate::HANDSHAKE.len()];
    recv.read_exact(&mut buf).await.anyerr()?;
    ensure_any!(buf == crate::HANDSHAKE, "invalid handshake");
    tracing::debug!("handshake verified");
    Ok(())
}

/// Read the `rtp <port>` announcement line of a viewer.
async fn read_announcement(recv: &mut RecvStream) -> Result<u16> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = recv.read(&mut byte).await.anyerr()?;
        if read.unwrap_or(0) == 0 {
            bail_any!("viewer closed the stream before announcing its port");
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        ensure_any!(line.len() < 64, "viewer announcement is too long");
    }
    let text = String::from_utf8_lossy(&line).into_owned();
    let port = text
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse::<u16>().ok())
        .std_context(format!("invalid announcement line `{text}`"))?;
    tracing::debug!("viewer announcement: {text}");
    Ok(port)
}

/// One connected viewer.
pub struct Viewer {
    /// Identifier of the viewer.
    pub id: u64,
    /// Where to send the media of this viewer.
    pub tx: mpsc::Sender<Frame>,
    /// Whether the keyframe gate of this viewer is open.
    pub streaming: bool,
}

/// The host side of the media session.
///
/// One task owns the [`Rtc`] and everything it mutates, as required by str0m's
/// single mutation invariant: one input, then drain the output, then wait.
pub struct Host {
    /// The WebRTC session, renegotiated by every WHIP offer.
    pub rtc: Rtc,
    /// The ICE socket.
    pub ice: Arc<UdpSocket>,
    /// The address of the ICE socket, advertised as our ICE candidate.
    pub ice_addr: SocketAddr,
    /// The media the publisher offered.
    pub offer: Vec<OfferMedia>,
    /// What the viewers are told about the stream.
    pub header: SessionHeader,
    /// The last session header bytes sent to a viewer.
    pub last_header: Vec<u8>,
    /// The H.264 parameter sets seen in band.
    pub sets: ParameterSets,
    /// The connected viewers.
    pub viewers: Vec<Viewer>,
    /// Counter for the entity tags of the resources.
    pub etag_seq: u64,
    /// The mids of the live media.
    pub mids: Vec<(Mid, MediaKind)>,
    /// Counters for logging.
    pub counters: Arc<Counters>,
    /// Rolling media statistics for the periodic report.
    pub report: StreamReport,
    /// The media kinds we have seen a packet of, to log the milestone once.
    pub seen: Vec<MediaKind>,
    /// Whether to log periodic counters.
    pub stats: bool,
    /// When the host session started, for the elapsed time in the stats panel.
    pub started_at: Instant,
    /// The last non-zero video bitrate, reused on viewer join/leave so the GUI
    /// bitrate does not flicker to zero between periodic ticks.
    pub last_video_kbps: f64,
    /// The last non-zero audio bitrate, reused on viewer join/leave.
    pub last_audio_kbps: f64,
    /// The last non-zero video fps, reused on viewer join/leave.
    pub last_video_fps: f64,
    /// Optional channel for structured live statistics (used by the GUI).
    pub stats_tx: Option<mpsc::UnboundedSender<StreamStats>>,
}

impl Host {
    pub fn new(
        ice: Arc<UdpSocket>,
        ice_addr: SocketAddr,
        stats: bool,
        stats_tx: Option<mpsc::UnboundedSender<StreamStats>>,
    ) -> Self {
        Host {
            rtc: new_rtc(),
            ice,
            ice_addr,
            offer: Vec::new(),
            header: SessionHeader {
                gen: 0,
                media: Vec::new(),
            },
            last_header: Vec::new(),
            sets: ParameterSets::default(),
            viewers: Vec::new(),
            etag_seq: 0,
            mids: Vec::new(),
            counters: Arc::new(Counters::default()),
            report: StreamReport::new(Instant::now()),
            seen: Vec::new(),
            stats,
            started_at: Instant::now(),
            last_video_kbps: 0.0,
            last_audio_kbps: 0.0,
            last_video_fps: 0.0,
            stats_tx,
        }
    }

    /// Handle a request from the WHIP HTTP server.
    fn on_whip(&mut self, req: WhipRequest) {
        match req {
            WhipRequest::Offer {
                sdp,
                via_put,
                reply,
            } => {
                let answer = self.negotiate(&sdp, via_put);
                if answer.is_err() {
                    tracing::warn!(
                        "rejecting a whip offer: {}",
                        answer.as_ref().err().unwrap_or(&String::new())
                    );
                }
                if reply.send(answer).is_err() {
                    tracing::debug!("the publisher hung up before the answer");
                }
            }
            WhipRequest::Trickle { sdp } => self.add_trickle(sdp),
            WhipRequest::Delete => {
                tracing::info!("whip session deleted");
                self.close_session();
            }
        }
    }

    /// Handle one event from the viewer acceptor.
    ///
    /// Session events are logged through `tracing` when a stats channel is wired
    /// up (GUI) and through `eprintln!` in the CLI, so a media worker never
    /// blocks on terminal I/O where the GUI has a tee behind it.
    pub fn on_viewer_event(&mut self, event: ViewerEvent) {
        // The GUI has a tee writer that hands the line to a background thread;
        // the CLI writes straight to stderr.
        let gui_mode = self.stats_tx.is_some();
        match event {
            ViewerEvent::Joined { id, port, tx } => {
                self.viewers.retain(|v| v.id != id);
                self.viewers.push(Viewer {
                    id,
                    tx,
                    streaming: false,
                });
                let line = format!(
                    "viewer {id} connected on udp://127.0.0.1:{port}, \
                     {count} viewer(s) now watching, waiting for a keyframe",
                    count = self.viewers.len()
                );
                if gui_mode {
                    tracing::info!("{line}");
                } else {
                    eprintln!("{line}");
                    tracing::info!(
                        "viewer {id} joined on udp://127.0.0.1:{port}, waiting for a keyframe"
                    );
                }
                self.refresh_header(true);
                self.request_keyframe();
                self.send_cached_stats();
            }
            ViewerEvent::Keyframe { id } => {
                // A viewer asks for a keyframe when it starts its player, when media
                // stalls, or when it sees a gap. If it is already streaming it has its
                // SDP and parameter sets, so we must NOT close the gate: on a lossy
                // path nearly every frame has a gap, and closing the gate on each one
                // drops the whole stream until the next IDR — the player reports
                // thousands of missed packets, jumping timestamps and a blackout. Keep
                // forwarding and just nudge the encoder; the player's jitter buffer and
                // error concealment ride out the loss.
                let _ = id;
                self.request_keyframe();
            }
            ViewerEvent::Left { id } => {
                self.viewers.retain(|v| v.id != id);
                let line = format!(
                    "viewer {id} disconnected, {count} viewer(s) now watching",
                    count = self.viewers.len()
                );
                if gui_mode {
                    tracing::info!("{line}");
                } else {
                    eprintln!("{line}");
                    tracing::info!("viewer {id} left");
                }
                self.send_cached_stats();
            }
        }
    }

    /// Accept an SDP offer from the publisher and answer it.
    ///
    /// Every offer starts a fresh session: a `POST` is a new resource and a
    /// `PUT` replaces the existing one, so the old ICE/DTLS state is dropped
    /// and the viewers are re-gated to wait for the next keyframe.
    pub fn negotiate(&mut self, offer: &str, via_put: bool) -> std::result::Result<Answer, String> {
        let parsed = sdp::parse_offer(offer);
        if parsed.iter().all(|m| m.payloads.is_empty()) {
            return Err("the offer contains no audio or video media".to_string());
        }

        let mut rtc = new_rtc();
        let candidate = Candidate::host(self.ice_addr, Protocol::Udp).map_err(|e| {
            format!(
                "cannot make an ice candidate for {addr}: {e}",
                addr = self.ice_addr
            )
        })?;
        rtc.add_local_candidate(candidate);
        let sdp_offer =
            SdpOffer::from_sdp_string(offer).map_err(|e| format!("cannot parse the offer: {e}"))?;
        let answer = rtc
            .sdp_api()
            .accept_offer(sdp_offer)
            .map_err(|e| format!("cannot accept the offer: {e}"))?;
        let answer_sdp = answer.to_sdp_string();

        self.rtc = rtc;
        self.offer = parsed;
        self.mids.clear();
        self.sets = ParameterSets::default();
        self.header = SessionHeader {
            gen: 0,
            media: Vec::new(),
        };
        self.last_header = Vec::new();
        self.report = StreamReport::new(Instant::now());
        self.seen.clear();
        for viewer in &mut self.viewers {
            viewer.streaming = false;
        }
        self.etag_seq += 1;
        let etag = format!("streampipe-{}", self.etag_seq);
        tracing::info!(
            "accepted a whip {} with {} media line(s), etag {etag}",
            if via_put { "PUT" } else { "POST" },
            self.offer.len()
        );
        Ok(Answer {
            sdp: answer_sdp,
            etag,
        })
    }

    /// Add trickle ICE candidates from a `PATCH` body.
    fn add_trickle(&mut self, sdp: String) {
        let mut added = 0;
        for line in sdp.lines() {
            let Some(rest) = line.strip_prefix("a=candidate:") else {
                continue;
            };
            let Some(c) = parse_candidate(rest) else {
                tracing::debug!("cannot parse trickle candidate: {rest}");
                continue;
            };
            self.rtc.add_remote_candidate(c);
            added += 1;
        }
        tracing::debug!("added {added} trickle ice candidates");
    }

    /// Tear down the WebRTC session, keeping the viewers connected.
    ///
    /// The viewers stay, with their gate closed, so that a publisher that
    /// reconnects within seconds does not need them to redial.
    fn close_session(&mut self) {
        self.rtc.disconnect();
        self.offer.clear();
        self.mids.clear();
        self.sets = ParameterSets::default();
        for viewer in &mut self.viewers {
            viewer.streaming = false;
        }
    }

    /// The kind and codec of a payload type the publisher offered.
    pub fn payload_of(&self, pt: u8) -> Option<(MediaKind, &sdp::Payload)> {
        for media in &self.offer {
            for payload in &media.payloads {
                if payload.pt == pt {
                    return Some((media.kind, payload));
                }
            }
        }
        None
    }

    /// Rebuild the session header and send it to every viewer.
    ///
    /// With `force` the header goes out even when unchanged, which is what the
    /// periodic tick does: the header is the viewers' only description of the
    /// stream, and datagrams are unreliable.
    fn refresh_header(&mut self, force: bool) {
        let bytes = self.header.encode();
        if !force && bytes == self.last_header {
            return;
        }
        self.last_header = bytes.clone();
        self.send_all(Frame::Header(Bytes::from(bytes)));
    }

    fn send_all(&self, frame: Frame) {
        for viewer in &self.viewers {
            try_send(&viewer.tx, &frame, &self.counters);
        }
    }

    /// Forward one incoming str0m RTP packet.
    fn on_rtp(&mut self, packet: RtpPacket) {
        let info = RtpInfo {
            marker: packet.header.marker,
            pt: *packet.header.payload_type,
            seq: packet.header.sequence_number,
            timestamp: packet.header.timestamp,
            ssrc: *packet.header.ssrc,
        };
        self.handle_media(info, &packet.payload);
    }

    /// Forward one RTP packet to the viewers whose gate is open.
    pub fn handle_media(&mut self, info: RtpInfo, payload: &[u8]) {
        let pt = info.pt;
        let Some((kind, announced)) = self.payload_of(pt) else {
            // rtx, red, or a payload type that was never negotiated.
            self.counters.unwanted.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let codec = Codec::from_name(&announced.codec);
        // Own the announcement: the rest of this function mutates the session.
        let params = announced.clone();
        let data = Bytes::from(rtp::serialize_rtp(&info, payload));

        if codec == Codec::H264 {
            let before = self.sets.clone();
            rtp::scan_h264_parameter_sets(payload, &mut self.sets);
            if before != self.sets {
                // The parameter sets travel with the header, no generation bump.
                self.set_header_media(kind, pt, &params);
            }
        }

        // Correct the announced payload type if the publisher picked another one
        // than the first we guessed.
        let matches = self
            .header
            .media(kind)
            .is_some_and(|m| m.pt == pt && m.codec == params.codec);
        if !matches {
            self.header.gen += 1;
            self.set_header_media(kind, pt, &params);
        }

        let size = data.len() as u64;
        let keyframe = kind == MediaKind::Video && rtp::starts_keyframe(payload, codec);
        self.log_streaming(kind, &params, keyframe);
        self.report
            .record(kind, Instant::now(), info.marker, keyframe, size);

        if self.viewers.is_empty() {
            self.counters.undeliverable.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let frame = if kind == MediaKind::Video {
            Frame::Video(data)
        } else {
            Frame::Audio(data)
        };
        let mut opened = 0;
        for viewer in &mut self.viewers {
            if !viewer.streaming {
                if !keyframe {
                    continue;
                }
                viewer.streaming = true;
                opened += 1;
                // The viewer needs its SDP before it can start the player, so
                // send the header right before the epoch marker.
                try_send(
                    &viewer.tx,
                    &Frame::Header(Bytes::from(self.header.encode())),
                    &self.counters,
                );
            }
            // Tag every keyframe with an epoch, not just the one that opens the
            // gate: the viewer uses it to reset its sequence baseline (so an IDR
            // does not look like a gap) and to pace its keyframe requests, which
            // keeps a lossy path from poking the encoder with a PLI per frame.
            if keyframe {
                try_send(&viewer.tx, &Frame::Epoch, &self.counters);
            }
            try_send(&viewer.tx, &frame, &self.counters);
        }
        if opened > 0 {
            tracing::info!("opened the gate for {opened} viewer(s) at a keyframe");
        }
        let count = self.viewers.len() as u64;
        self.counters.forwarded.fetch_add(count, Ordering::Relaxed);
        self.counters
            .forwarded_bytes
            .fetch_add(frame_len(&frame) as u64 * count, Ordering::Relaxed);
    }

    /// Put or replace the session header entry of a media kind.
    fn set_header_media(&mut self, kind: MediaKind, pt: u8, payload: &sdp::Payload) {
        let media = MediaHeader {
            kind,
            pt,
            codec: payload.codec.clone(),
            clock: payload.clock,
            channels: if kind == MediaKind::Audio {
                payload.channels
            } else {
                1
            },
            fmtp: payload.fmtp.clone(),
            sets: self.sets.clone(),
        };
        match self.header.media.iter_mut().find(|m| m.kind == kind) {
            Some(existing) => *existing = media,
            None => self.header.media.push(media),
        }
    }

    /// Announce, once per media kind, that the publisher is sending it.
    ///
    /// The WHIP handshake answers with an SDP before a single packet flows, so
    /// this is the first moment we can honestly say the stream works. The line
    /// names the codec the publisher actually chose — the thing an operator
    /// staring at a black player wants confirmed.
    ///
    /// Routed through `tracing` in GUI mode, `eprintln!` in the CLI.
    pub fn log_streaming(&mut self, kind: MediaKind, params: &sdp::Payload, keyframe: bool) {
        if self.seen.contains(&kind) {
            return;
        }
        self.seen.push(kind);
        let gui_mode = self.stats_tx.is_some();
        let viewers = self.viewers.len();
        if kind == MediaKind::Video {
            let gate = if keyframe {
                "streaming now".to_string()
            } else {
                format!("waiting for a keyframe to open the gate for {viewers} viewer(s)")
            };
            let line = format!(
                "whip streaming video: {codec}/{clock}, {gate}",
                codec = params.codec,
                clock = params.clock
            );
            if gui_mode {
                tracing::info!("{line}");
            } else {
                eprintln!("{line}");
            }
        } else {
            let line = format!(
                "whip streaming audio: {codec}/{clock}/{channels}",
                codec = params.codec,
                clock = params.clock,
                channels = params.channels
            );
            if gui_mode {
                tracing::info!("{line}");
            } else {
                eprintln!("{line}");
            }
        }
    }

    /// Print the periodic media report of the last interval.
    pub fn report_media(&mut self) {
        let now = Instant::now();
        // Compute the rates of the interval that `log` is about to report and
        // reset, before the tallies are cleared. Silence reports as zero.
        let v_secs = now
            .duration_since(self.report.video.since)
            .as_secs_f64()
            .max(0.001);
        let (video_kbps, video_fps) = if self.report.video.active() {
            (
                self.report.video.kbit_per_second(now),
                self.report.video.frames as f64 / v_secs,
            )
        } else {
            (0.0, 0.0)
        };
        let audio_kbps = if self.report.audio.active() {
            self.report.audio.kbit_per_second(now)
        } else {
            0.0
        };
        // Unchanged CLI log: prints the periodic line and restarts the tallies.
        // In GUI mode the line goes through the non-blocking tee writer.
        self.report
            .log(now, self.viewers.len(), self.stats_tx.is_some());
        // Cache the last non-zero rates so a viewer join/leave can refresh the
        // viewer count without zeroing the bitrate between ticks.
        if video_kbps > 0.0 {
            self.last_video_kbps = video_kbps;
        }
        if video_fps > 0.0 {
            self.last_video_fps = video_fps;
        }
        if audio_kbps > 0.0 {
            self.last_audio_kbps = audio_kbps;
        }
        self.send_stats(now, video_kbps, video_fps, audio_kbps);
    }

    /// Send a structured stats snapshot to the GUI, if a channel is wired up.
    ///
    /// The CLI passes no channel, so this is a no-op there.
    fn send_stats(&self, now: Instant, video_kbps: f64, video_fps: f64, audio_kbps: f64) {
        if let Some(tx) = &self.stats_tx {
            let _ = tx.send(StreamStats {
                elapsed_secs: now.duration_since(self.started_at).as_secs(),
                viewers: self.viewers.len(),
                video_kbps,
                video_fps,
                audio_kbps,
            });
        }
    }

    /// Refresh the viewer count for the GUI using the last known rates, so a
    /// join or leave updates the count immediately without flicking the bitrate
    /// to zero.
    fn send_cached_stats(&self) {
        let now = Instant::now();
        self.send_stats(
            now,
            self.last_video_kbps,
            self.last_video_fps,
            self.last_audio_kbps,
        );
    }

    /// Ask the publisher for a keyframe.
    fn request_keyframe(&mut self) {
        let mut requested = false;
        for (mid, kind) in self.mids.clone() {
            if kind != MediaKind::Video {
                continue;
            }
            if let Some(rx) = self.rtc.direct_api().stream_rx_by_mid(mid, None) {
                rx.request_keyframe(KeyframeRequestKind::Pli);
                requested = true;
            }
        }
        if !requested {
            tracing::debug!("no video receive stream yet, cannot request a keyframe");
        }
    }

    /// Feed one datagram from the publisher into the WebRTC session.
    pub fn on_ice(&mut self, buf: &[u8], source: SocketAddr) {
        let Ok(receive) = Receive::new(Protocol::Udp, source, self.ice_addr, buf) else {
            tracing::trace!("ignoring {source} data that is not webrtc");
            return;
        };
        if let Err(e) = self
            .rtc
            .handle_input(Input::Receive(Instant::now(), receive))
        {
            tracing::debug!("webrtc input error: {e}");
        }
    }

    /// Drive the WebRTC session clock.
    pub fn on_timeout(&mut self) {
        if let Err(e) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
            tracing::debug!("webrtc timeout error: {e}");
        }
    }

    /// Handle one str0m event.
    ///
    /// The connected line goes through `tracing` in GUI mode (see
    /// [`Self::log_streaming`]) and straight to stderr in the CLI.
    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Connected => {
                let line = "whip publisher connected, ICE and DTLS up, waiting for media";
                if self.stats_tx.is_some() {
                    tracing::info!("{line}");
                } else {
                    eprintln!("{line}");
                    tracing::info!("webrtc publisher connected");
                }
            }
            Event::IceConnectionStateChange(state) => {
                tracing::debug!("ice connection state: {state:?}");
            }
            Event::MediaAdded(added) => {
                let kind = kind_of(added.kind);
                tracing::info!(
                    "publisher added {} media on mid {}",
                    kind.as_str(),
                    added.mid
                );
                self.mids.retain(|(mid, _)| *mid != added.mid);
                self.mids.push((added.mid, kind));
                self.header.gen += 1;
                self.refresh_header(true);
            }
            Event::MediaChanged(changed) => {
                tracing::info!("publisher changed media on mid {}", changed.mid);
                self.header.gen += 1;
                self.refresh_header(true);
            }
            Event::RtpPacket(packet) => self.on_rtp(packet),
            Event::StreamPaused(paused) => {
                tracing::info!(
                    "publisher stream on mid {} paused: {}",
                    paused.mid,
                    paused.paused
                );
            }
            Event::KeyframeRequest(_) => {
                // The publisher sends media, it does not ask us for keyframes.
            }
            Event::Closed => {
                tracing::info!("webrtc publisher closed the session");
                self.close_session();
            }
            _ => {}
        }
    }
}

/// Create a str0m session in RTP mode.
///
/// RTP mode is what makes the tunnel possible: str0m hands over the RTP packets
/// as they arrive instead of depacketizing them into frames, which is exactly
/// what a plain RTP player downstream wants.
fn new_rtc() -> Rtc {
    Rtc::builder().set_rtp_mode(true).build(Instant::now())
}

/// The byte size of a frame, for the counters.
fn frame_len(frame: &Frame) -> usize {
    match frame {
        Frame::Header(b) | Frame::Video(b) | Frame::Audio(b) => b.len(),
        Frame::Epoch => 0,
    }
}

/// Send a frame to a viewer, dropping it if the viewer is too slow.
///
/// A full queue is egress loss: count it as undeliverable so a slow or
/// overloaded viewer is visible in the stats instead of only in `trace!`.
fn try_send(tx: &mpsc::Sender<Frame>, frame: &Frame, counters: &Counters) {
    if tx.try_send(clone_frame(frame)).is_err() {
        counters.undeliverable.fetch_add(1, Ordering::Relaxed);
        tracing::trace!("viewer queue full, dropped a frame");
    }
}

/// Clone a frame cheaply; the payload is a `Bytes`.
fn clone_frame(frame: &Frame) -> Frame {
    match frame {
        Frame::Header(b) => Frame::Header(b.clone()),
        Frame::Epoch => Frame::Epoch,
        Frame::Video(b) => Frame::Video(b.clone()),
        Frame::Audio(b) => Frame::Audio(b.clone()),
    }
}

/// The media session loop: one task owns the `Rtc` and everything that mutates
/// it.
async fn host_loop(
    ice: Arc<UdpSocket>,
    ice_addr: SocketAddr,
    mut req_rx: mpsc::Receiver<WhipRequest>,
    mut viewer_rx: mpsc::Receiver<ViewerEvent>,
    stats: bool,
    stats_tx: Option<mpsc::UnboundedSender<StreamStats>>,
) -> Result<()> {
    let mut host = Host::new(ice.clone(), ice_addr, stats, stats_tx);
    let mut header_tick = interval(HEADER_INTERVAL);
    header_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stats_tick = interval(STATS_INTERVAL);
    stats_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut report_tick = interval(STATS_INTERVAL);
    report_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut buf = vec![0u8; ICE_BUF];

    loop {
        // Drain everything str0m wants to do before the next mutation.
        let deadline = drain(&mut host).await?;
        let sleep = sleep_until(tokio::time::Instant::from_std(deadline));

        tokio::select! {
            req = req_rx.recv() => {
                let Some(req) = req else { break };
                host.on_whip(req);
            }
            event = viewer_rx.recv() => {
                let Some(event) = event else { break };
                host.on_viewer_event(event);
            }
            res = ice.recv_from(&mut buf) => {
                let (len, source) = res.std_context("error receiving from the ice socket")?;
                host.on_ice(&buf[..len], source);

                // Keep pulling packets off a hot socket so a keyframe burst (hundreds
                // of packets at once) does not overflow the OS receive buffer while we
                // sit in `select!`. str0m's single-mutation invariant forbids two
                // back-to-back mutations, so we alternate: drain `poll_output` (forward-
                // ing decrypted RTP + transmits), then feed the next packet.
                // `try_recv_from` returns `WouldBlock` once the socket is empty.
                loop {
                    drain(&mut host).await?;
                    match ice.try_recv_from(&mut buf) {
                        Ok((len, source)) => host.on_ice(&buf[..len], source),
                        Err(_) => break,
                    }
                }
            }
            _ = sleep => host.on_timeout(),
            _ = header_tick.tick() => host.refresh_header(true),
            _ = stats_tick.tick(), if host.stats => host.counters.log_periodic(),
            _ = report_tick.tick() => host.report_media(),
        }
    }
    host.counters.log_final();
    host.report
        .log_final(Instant::now(), host.stats_tx.is_some());
    Ok(())
}

/// Drain `poll_output` until str0m asks for the next timeout.
async fn drain(host: &mut Host) -> Result<Instant> {
    loop {
        match host.rtc.poll_output() {
            Ok(Output::Timeout(at)) => return Ok(at),
            Ok(Output::Transmit(t)) => {
                host.ice
                    .send_to(&t.contents, t.destination)
                    .await
                    .std_context("error sending on the ice socket")?;
            }
            Ok(Output::Event(event)) => host.on_event(event),
            Err(e) => bail_any!("webrtc session error: {e}"),
        }
    }
}

/// Parse one `a=candidate:` line of a trickle ICE fragment.
pub fn parse_candidate(rest: &str) -> Option<Candidate> {
    // `<foundation> <component> <transport> <priority> <ip> <port> typ <type> ...`
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 7 {
        return None;
    }
    let addr: SocketAddr = format!("{}:{}", fields[4], fields[5]).parse().ok()?;
    Candidate::host(addr, fields[2]).ok()
}

/// Configuration of the viewer.
pub struct ViewerConfig {
    /// The ticket of the host.
    pub addr: EndpointAddr,
    /// The ALPN to dial with.
    pub alpn: Vec<u8>,
    /// The local address to play out on.
    pub play: SocketAddr,
    /// Where to write the SDP file.
    pub sdp: Option<PathBuf>,
    /// Whether to write the default SDP file into the current folder rather than
    /// the system temp dir. Set for `--player none` and a bare `--sdp`.
    pub sdp_here: bool,
    /// The player to launch.
    pub player: Player,
    /// An explicit path to the player binary, for a player not on `PATH`.
    pub player_path: Option<PathBuf>,
    /// Whether to launch the player at all.
    pub no_launch: bool,
    /// How much jitter buffer to give the player, `None` for lowest latency.
    pub buffer: Option<Duration>,
    /// The player's startup volume in percent (0-100), 100 for its own default.
    pub volume: u8,
    /// Whether to log periodic counters.
    pub stats: bool,
    /// Optional mirror of the media gate: `false` while nothing is being forwarded to
    /// the player (connected, waiting for the broadcaster to start sending), `true`
    /// once media flows.
    pub media_state: Option<mpsc::UnboundedSender<bool>>,
}

/// The local RTP ports the viewer plays out on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayPorts {
    /// The video RTP port, always even.
    pub video: u16,
    /// The audio RTP port.
    pub audio: u16,
}

impl PlayPorts {
    /// Derive the ports from the requested address.
    ///
    /// RTP over UDP uses an even port with RTCP on the port above it, and a
    /// media player binds both for every media, so the two media need two pairs.
    pub fn new(requested: SocketAddr) -> Self {
        let video = if requested.port().is_multiple_of(2) {
            requested.port()
        } else {
            requested.port() + 1
        };
        PlayPorts {
            video,
            audio: video + 2,
        }
    }
}

/// A running player, so it can be stopped when the stream changes.
struct PlayerHandle {
    /// Send to make the player quit.
    stop: mpsc::Sender<()>,
}

/// A message from the viewer's QUIC read loop to its forwarding task.
///
/// The read loop never blocks on the localhost UDP send: it pushes to a bounded
/// channel and a dedicated task drains it. A full channel drops the packet (reads
/// as ordinary RTP loss, triggering a throttled keyframe request) instead of the
/// read loop parking on `send_to` and dumping the QUIC backlog in one burst.
enum ForwardMsg {
    /// A keyframe epoch: reset the gap-detection baseline in the forward task.
    Epoch,
    /// An RTP packet to forward: its player port, the bytes, and whether it is
    /// video (video drives gap detection and keyframe requests).
    Packet(u16, Vec<u8>, bool),
}

/// Connect to a WHIP host and play the stream out locally.
///
/// The viewer is a plain RTP forwarder: it announces the local video port, then
/// writes every forwarded packet to it, and writes the SDP the host describes the
/// stream with to a file a player can open.
pub async fn connect_whip(endpoint: Endpoint, cfg: ViewerConfig) -> Result<()> {
    let ports = PlayPorts::new(cfg.play);
    if ports.video != cfg.play.port() {
        tracing::info!(
            "rtp ports must be even, using udp://{}:{} instead of udp://{}",
            cfg.play.ip(),
            ports.video,
            cfg.play
        );
    }
    // An explicit `--sdp=path` wins; a bare `--sdp` or `--player none` drops
    // the default-named file in the current folder for the user to open; a
    // launched player keeps it in the temp dir.
    let sdp_path = sdp_path(cfg.sdp.clone(), cfg.sdp_here, ports.video);
    tracing::info!("writing the player description to {}", sdp_path.display());

    let video = Arc::new(
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .std_context("error binding the local forwarding socket")?,
    );
    // A big send buffer absorbs the bursts the forward task cannot pace away,
    // so a dense reorder flush does not spill onto the wire into ffplay's
    // default-size receive buffer. `let _ =` clamps to the OS hard limit.
    let _ = socket2::SockRef::from(&*video).set_send_buffer_size(8 * 1024 * 1024);
    let counters = Arc::new(Counters::default());

    // Nothing is flowing yet: tell the caller the gate is closed, so the Watch tab can
    // read "waiting for the streamer" from the moment the session is up.
    if let Some(media_state) = &cfg.media_state {
        let _ = media_state.send(false);
    }

    let mut player: Option<PlayerHandle> = None;
    let mut written: Option<String> = None;
    let mut launched = String::new();

    loop {
        tracing::info!("connecting to {}", cfg.addr.id);
        let conn = endpoint
            .connect(cfg.addr.clone(), &cfg.alpn)
            .await
            .std_context("error connecting to the host")?;
        tracing::info!("connected to {}", cfg.addr.id);

        // Pick the player's jitter buffer for this path. A local network stays
        // at the lowest latency; a long-distance path gets a buffer sized by
        // its round-trip time, otherwise the player's zero-buffer RTP receiver
        // drops every reordered packet and the log fills with `max delay
        // reached` and corrupt pictures.
        let rtt = conn
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.rtt());
        let buffer = auto_buffer(cfg.buffer, rtt);
        if cfg.buffer.is_none() && buffer.is_some() {
            eprintln!(
                "long-distance path (rtt {} ms), buffering {} ms",
                rtt.unwrap_or_default().as_millis(),
                buffer.unwrap_or_default().as_millis(),
            );
        }

        let (mut send, recv) = conn
            .open_bi()
            .await
            .std_context("error opening stream to the host")?;
        write_handshake(&mut send).await?;
        send.write_all(format!("rtp {}\n", ports.video).as_bytes())
            .await
            .anyerr()?;
        // keep the stream open as a liveness signal
        let _liveness = (send, recv);

        let (kf_tx, mut kf_rx) = mpsc::channel::<()>(8);
        let (exit_tx, mut exit_rx) = mpsc::channel::<()>(4);

        let mut header: Option<SessionHeader> = None;
        let mut forwarding = false;
        // Datagrams the host had to fragment, reassembled before they are
        // forwarded to the player.
        let mut reassembler = rtp::Reassembler::default();
        // QUIC datagrams are unordered, so put each media back into RTP
        // sequence order before the player sees it; otherwise ffplay reads the
        // reordering as loss and corrupts the picture. Video and audio have
        // separate sequence spaces, so each gets its own buffer. The reorder
        // window is sized from `--buffer` so the two reorder stages stay in
        // tandem: a bigger player buffer lets us wait a little longer for a
        // reordered packet, but never long enough to stall on a lost one.
        let reorder_timeout = reorder_timeout_for(cfg.buffer);
        let mut video_order = Reorder::with_timeout(reorder_timeout);
        let mut audio_order = Reorder::with_timeout(reorder_timeout);

        // Decouple the QUIC read loop from the localhost UDP send. The read
        // loop pushes packets onto this bounded channel and never waits for the
        // player; a dedicated task drains it to ffplay. If the player hiccups
        // the channel fills and packets drop (counted as undeliverable), which
        // reads as ordinary RTP loss and triggers a throttled keyframe request,
        // instead of the read loop parking on send_to and releasing the QUIC
        // backlog at the player in one burst.
        let (forward_tx, mut forward_rx) = mpsc::channel::<ForwardMsg>(VIEWER_QUEUE);
        let forward_video = video.clone();
        let forward_counters = counters.clone();
        let forward_kf_tx = kf_tx.clone();
        let forward_task = tokio::spawn(async move {
            // Gap detection and keyframe pacing live here, driven by the order
            // packets are actually written to the player.
            let mut last_seq: Option<u16> = None;
            let mut last_kf: Option<Instant> = None;
            // Pace local UDP writes: ffplay opens its RTP socket via an SDP file
            // where FFmpeg ignores -buffer_size, so it is bounded by the OS default
            // SO_RCVBUF (~50-212 KB). A dense reorder flush can dump >100 KB at
            // once and overrun that socket. A token bucket caps a burst to 64 KB and
            // refills at 16 KB/ms: zero latency in steady state (bucket stays full
            // far above a 6 Mbps stream), a dense flush spread over a few ms.
            const BURST_CAP: usize = 64 * 1024;
            const TOKEN_RATE: usize = 16 * 1024; // bytes per ms
            let mut tokens = BURST_CAP;
            let mut last_fill = tokio::time::Instant::now();
            while let Some(msg) = forward_rx.recv().await {
                match msg {
                    ForwardMsg::Epoch => {
                        // A keyframe just arrived: reset the baseline so the gap
                        // right after it is not mistaken for a loss.
                        last_seq = None;
                        last_kf = Some(Instant::now());
                    }
                    ForwardMsg::Packet(port, packet, is_video) => {
                        let required = packet.len().min(BURST_CAP);
                        loop {
                            let now = tokio::time::Instant::now();
                            let elapsed_micros = now.duration_since(last_fill).as_micros() as u64;
                            if elapsed_micros > 0 {
                                let added =
                                    ((elapsed_micros * (TOKEN_RATE as u64)) / 1000) as usize;
                                tokens = (tokens + added).min(BURST_CAP);
                                last_fill = now;
                            }
                            if tokens >= required {
                                tokens -= required;
                                break;
                            }
                            // Sleep until the bucket can pay for the packet. Never
                            // spin: `yield_now` re-readies this task immediately, so
                            // it pins a worker thread at 100% and starves every other
                            // task on the runtime. Host and viewer share one runtime
                            // in the GUI, and a starved ICE loop lets the OS UDP
                            // receive buffer overflow, which is exactly the packet
                            // loss this pacing exists to prevent.
                            let missing = required - tokens;
                            // Tokens refill at TOKEN_RATE per millisecond.
                            let wait_micros = (missing as u64 * 1000).div_ceil(TOKEN_RATE as u64);
                            // At least 1 ms: a sub-millisecond timer would wake on
                            // the next tick anyway, so this is the cheapest real yield.
                            let wait_micros = wait_micros.max(1000);
                            sleep(Duration::from_micros(wait_micros)).await;
                        }
                        forward(
                            &forward_video,
                            &packet,
                            port,
                            &forward_counters,
                            &mut last_seq,
                            &mut last_kf,
                            is_video,
                            &forward_kf_tx,
                        )
                        .await;
                    }
                }
            }
        });

        let mut last_packet = tokio::time::Instant::now();
        let mut wait_tick = interval(WAIT_INTERVAL);
        wait_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut stats_tick = interval(STATS_INTERVAL);
        stats_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let result: Result<()> = loop {
            let silence = sleep_until(last_packet + STALL_INTERVAL);
            tokio::select! {
                datagram = conn.read_datagram() => {
                    let data = match datagram {
                        Ok(data) => data,
                        Err(e) => {
                            break Err(anyerr!("error reading datagrams from the host: {e}"));
                        }
                    };
                    // Reassemble fragmented datagrams and let the complete
                    // datagram fall through to the dispatch below.
                    let data = match data.first().copied() {
                        Some(TAG_FRAGMENT) => match reassembler.push(&data, Instant::now()) {
                            Some(full) => Bytes::from(full),
                            None => continue,
                        },
                        _ => data,
                    };
                    match data.first().copied() {
                        Some(TAG_SESSION) => {
                            let Some(decoded) = SessionHeader::decode(&data[1..]) else {
                                tracing::warn!("cannot decode the session header");
                                continue;
                            };
                            if header.as_ref() == Some(&decoded) {
                                continue;
                            }
                            let signature = media_signature(&decoded);
                            let renegotiated = signature != launched;
                            header = Some(decoded);
                            if let Some(text) = render(header.as_ref(), ports, &mut written) {
                                write_sdp(&sdp_path, &text).await?;
                            }
                            if renegotiated {
                                if let Some(player) = player.take() {
                                    let _ = player.stop.send(()).await;
                                }
                                launched = signature;
                                player =
                                    launch(&cfg, buffer, &sdp_path, ports.video, &kf_tx, &exit_tx)
                                        .await?;
                            }
                        }
                        Some(TAG_EPOCH) => {
                            if header.as_ref().is_some_and(|h| h.has_media()) {
                                if !forwarding {
                                    forwarding = true;
                                    // Only wipe the buffers when opening the gate.
                                    // Mid-stream epochs would delete valid fragments
                                    // and reordered packets of the new keyframe,
                                    // since QUIC datagrams are unordered.
                                    reassembler.clear();
                                    video_order.clear();
                                    audio_order.clear();
                                    tracing::info!("streaming from a keyframe");
                                    // The gate is open: the player now has a stream to
                                    // show, so the Watch tab can drop the "waiting" label.
                                    if let Some(media_state) = &cfg.media_state {
                                        let _ = media_state.send(true);
                                    }
                                }
                                // Tell the forward task to reset its gap baseline:
                                // a gap right after this keyframe is not a loss and
                                // must not poke the encoder.
                                if forward_tx.try_send(ForwardMsg::Epoch).is_err() {
                                    counters.undeliverable.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Some(tag @ (TAG_VIDEO | TAG_AUDIO)) if forwarding => {
                            let is_video = tag == TAG_VIDEO;
                            let port = if is_video { ports.video } else { ports.audio };
                            let packet = &data[1..];
                            // Reorder by RTP sequence number before handing the
                            // packet to the player, so the unordered datagram path
                            // does not look like packet loss to ffplay.
                            let order = if is_video { &mut video_order } else { &mut audio_order };
                            let emit = match rtp::parse_header(packet) {
                                Some((info, _)) => {
                                    order.push(info.seq, packet.to_vec(), Instant::now())
                                }
                                None => vec![packet.to_vec()],
                            };
                            for pkt in emit {
                                if forward_tx
                                    .try_send(ForwardMsg::Packet(port, pkt, is_video))
                                    .is_err()
                                {
                                    counters.undeliverable.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            last_packet = tokio::time::Instant::now();
                        }
                        _ => {}
                    }
                }
                Some(()) = kf_rx.recv() => {
                    send_datagram(&conn, TAG_KEYFRAME_REQ, &counters);
                }
                Some(()) = exit_rx.recv() => {
                    tracing::info!("the player exited, waiting for the next keyframe");
                    player = None;
                    launched = String::new();
                    forwarding = false;
                    // Gate closed again: back to "waiting for the streamer".
                    if let Some(media_state) = &cfg.media_state {
                        let _ = media_state.send(false);
                    }
                }
                _ = silence, if forwarding => {
                    tracing::debug!("no media for a while, asking for a keyframe");
                    send_datagram(&conn, TAG_KEYFRAME_REQ, &counters);
                    last_packet = tokio::time::Instant::now();
                }
                _ = wait_tick.tick() => {
                    if !forwarding {
                        tracing::info!("waiting for media from {}", cfg.addr.id);
                    }
                }
                _ = stats_tick.tick(), if cfg.stats => counters.log_periodic(),
            }
        };

        // Stop the forward task before closing the connection so it cannot leak
        // across a reconnect.
        forward_task.abort();
        counters.log_final();
        if let Some(player) = player.take() {
            let _ = player.stop.send(()).await;
        }
        conn.close(0u32.into(), b"viewer is leaving");
        result?;
        // The loop only exits the inner select on an error, so this is
        // unreachable, but the compiler wants the reconnect.
        sleep(RECONNECT_DELAY).await;
    }
}

/// Render the player SDP if the header describes a stream and changed.
fn render(
    header: Option<&SessionHeader>,
    ports: PlayPorts,
    written: &mut Option<String>,
) -> Option<String> {
    let header = header?;
    if !header.has_media() {
        return None;
    }
    let text = sdp::render_player_sdp(header, ports.video, ports.audio);
    if written.as_ref() == Some(&text) {
        return None;
    }
    *written = Some(text.clone());
    Some(text)
}

/// A signature of the media of a header, to detect a codec change.
fn media_signature(header: &SessionHeader) -> String {
    header
        .media
        .iter()
        .map(|m| format!("{}:{}:{}", m.kind.as_str(), m.pt, m.codec))
        .collect::<Vec<_>>()
        .join(",")
}

/// The jitter buffer to give the player.
///
/// An explicit `--buffer` always wins. Otherwise the path decides: a local
/// network stays at the lowest latency, a long-distance path gets four
/// round-trip times, clamped to 150-500 ms. That is the reorder window the
/// player waits for late packets; without it a path that jitters more than a
/// few milliseconds drops intact packets and shows corrupt pictures.
pub fn auto_buffer(configured: Option<Duration>, rtt: Option<Duration>) -> Option<Duration> {
    configured.or_else(|| {
        rtt.filter(|r| *r >= WAN_RTT)
            .map(|r| (r * 4).clamp(Duration::from_millis(150), Duration::from_millis(500)))
    })
}

/// Where the SDP file goes.
///
/// An explicit path wins; a directory gets the default name inside it. With no
/// usable path, `here` drops the file in the current folder so the user can
/// open it by hand (`--player none`, or a bare `--sdp`); otherwise it goes to
/// the system temp dir to keep the folder clean.
pub fn sdp_path(explicit: Option<PathBuf>, here: bool, video_port: u16) -> PathBuf {
    let name = format!("streampipe-{video_port}.sdp");
    if let Some(p) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return if p.is_dir() { p.join(name) } else { p };
    }
    let dir = if here {
        std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir())
    } else {
        std::env::temp_dir()
    };
    dir.join(name)
}

/// Write the SDP file.
async fn write_sdp(path: &Path, sdp: &str) -> Result<()> {
    tokio::fs::write(path, sdp)
        .await
        .with_std_context(|_| format!("error writing sdp file to {path:?}"))?;
    tracing::info!("wrote {}", path.display());
    Ok(())
}

/// Stage the mpv buffer-drain control script on disk for `--script=`.
async fn stage_mpv_script(player: Player) -> Option<PathBuf> {
    if player != Player::Mpv {
        return None;
    }
    let path = std::env::temp_dir().join("streampipe-buffer-speed.lua");
    match tokio::fs::write(&path, include_str!("../scripts/buffer_speed.lua")).await {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("could not stage the mpv control script: {e}");
            None
        }
    }
}

/// Launch the player and ask the host for a keyframe once it is listening.
///
/// `buffer` is the jitter buffer for this path, see [`auto_buffer`].
/// Returns `None` when no player is to be launched.
async fn launch(
    cfg: &ViewerConfig,
    buffer: Option<Duration>,
    sdp_path: &Path,
    video_port: u16,
    kf_tx: &mpsc::Sender<()>,
    exit_tx: &mpsc::Sender<()>,
) -> Result<Option<PlayerHandle>> {
    if !cfg.player.launches() {
        // No player at all: tell the operator exactly where the file is and how
        // to open it, since we are leaving the watching to them.
        let path = sdp_path.display();
        eprintln!("saved the stream description to {path}");
        eprintln!("open it in a player to watch, e.g. 'mpv {path}' or 'vlc {path}'");
        return Ok(None);
    }
    if cfg.no_launch {
        // Stage the mpv control script before printing, so the printed command
        // refers to a file that already exists.
        let script = stage_mpv_script(cfg.player).await;
        tracing::info!(
            "not launching a player, play with: {}",
            cfg.player
                .command(
                    sdp_path,
                    buffer,
                    cfg.player_path.as_deref(),
                    cfg.volume,
                    script.as_deref(),
                )
                .join(" ")
        );
        return Ok(None);
    }
    let script = stage_mpv_script(cfg.player).await;
    let command = cfg.player.command(
        sdp_path,
        buffer,
        cfg.player_path.as_deref(),
        cfg.volume,
        script.as_deref(),
    );
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .spawn()
        .with_std_context(|_| format!("error launching {}", command[0]))?;
    tracing::info!("launched {}", command.join(" "));

    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let exited = exit_tx.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = stop_rx.recv() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            status = child.wait() => {
                tracing::debug!("the player exited: {status:?}");
                let _ = exited.send(()).await;
            }
        }
    });

    wait_until_listening(video_port).await;
    let _ = kf_tx.try_send(());
    Ok(Some(PlayerHandle { stop: stop_tx }))
}

/// Wait until a player has bound the RTP port.
///
/// Until then there is nobody to receive the forwarded packets, and starting a
/// player takes longer than the keyframe we would send it.
pub async fn wait_until_listening(port: u16) {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let started = Instant::now();
    while started.elapsed() < PLAYER_READY_TIMEOUT {
        // Probe by binding the port, but drop the probe socket before we sleep.
        // A socket bound in a `match` scrutinee lives until the end of the whole
        // `match`, so binding and then sleeping inside the arm would hog the very
        // port the player is trying to bind and make it fail to open it.
        let free = match UdpSocket::bind(addr).await {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => false,
            Err(e) => {
                tracing::debug!("cannot probe the player port: {e}");
                return;
            }
        };
        if !free {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    tracing::warn!("the player did not open udp://{port}, forwarding anyway");
}

/// Forward one RTP packet to the local player port.
///
/// A gap in the video sequence nudges the viewer to ask the host for a keyframe,
/// but `last_kf` is refreshed by every keyframe (the host tags each with an
/// epoch), so a gap right after an IDR asks for nothing. Only a sustained gap
/// with no keyframe for `KF_REQUEST_INTERVAL` sends a PLI: on a lossy path
/// nearly every frame has a gap, and a PLI per gap is a keyframe storm that
/// amplifies the loss instead of recovering.
// Pre-existing: this forwarder threads a handful of per-viewer state pointers
// through the hot path; left as-is to avoid churn in the media forwarding code.
#[allow(clippy::too_many_arguments)]
pub async fn forward(
    socket: &Arc<UdpSocket>,
    packet: &[u8],
    port: u16,
    counters: &Arc<Counters>,
    last_seq: &mut Option<u16>,
    last_kf: &mut Option<Instant>,
    video: bool,
    kf_tx: &mpsc::Sender<()>,
) {
    let dst = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match socket.send_to(packet, dst).await {
        Ok(_) => {
            counters.forwarded.fetch_add(1, Ordering::Relaxed);
            counters
                .forwarded_bytes
                .fetch_add(packet.len() as u64, Ordering::Relaxed);
        }
        Err(e) if is_transient(&e) => {
            counters.undeliverable.fetch_add(1, Ordering::Relaxed);
            tracing::trace!("the player is not listening on {dst} yet");
        }
        Err(e) => {
            counters.undeliverable.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("error forwarding to {dst}: {e}");
        }
    }
    if !video {
        return;
    }
    let Some((info, _)) = rtp::parse_header(packet) else {
        return;
    };
    let gap = last_seq.is_some_and(|last| info.seq != last.wrapping_add(1));
    *last_seq = Some(info.seq);
    let throttled = last_kf.is_some_and(|at| at.elapsed() < KF_REQUEST_INTERVAL);
    if gap && !throttled {
        *last_kf = Some(Instant::now());
        tracing::debug!("video sequence gap at {}, asking for a keyframe", info.seq);
        let _ = kf_tx.try_send(());
    }
}

/// Send a tagged control datagram to the host.
fn send_datagram(conn: &Connection, tag: u8, counters: &Arc<Counters>) {
    match conn.send_datagram(Bytes::from(vec![tag])) {
        Ok(()) => {
            if tag == TAG_KEYFRAME_REQ {
                counters.keyframes.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(e) => tracing::debug!("cannot send control datagram: {e}"),
    }
}

/// Write the handshake to the host.
async fn write_handshake(send: &mut SendStream) -> Result<()> {
    send.write_all(&crate::HANDSHAKE).await.anyerr()?;
    tracing::debug!("handshake sent");
    Ok(())
}
