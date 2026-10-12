//! Shared GUI state and the bridge to the iroh engine.
//!
//! All long-lived, cross-thread state lives behind a single `Arc<Mutex<SharedState>>`.
//! The rule that keeps this sound with a plain `std::sync::Mutex` is that the lock is
//! **never held across an `.await`**: engine code doesn't touch the mutex, the stats
//! consumer task locks only inside its loop body, and the UI thread locks only to read
//! a snapshot or to kick off a session before spawning.
//!
//! A single [`Endpoint`] is created once (from the persisted secret) and reused across
//! start/stop cycles, so the endpoint identity is stable. Stopping a session is a
//! synchronous [`tokio::task::JoinHandle::abort`], which drops the WHIP `serve`
//! future: the HTTP listener is freed, the accept loops stop, and — because the engine
//! keeps every connection and viewer task in an `AbortOnDrop` join set — all of them
//! are aborted too, so nothing outlives the stop. The endpoint itself is kept for
//! reuse and only dropped when the app quits.

use std::{
    collections::VecDeque,
    net::{SocketAddrV4, SocketAddrV6},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use eframe::egui;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle, time::timeout};

use crate::{
    common::{create_endpoint, webrtc_alpn, CommonArgs, ONLINE_TIMEOUT},
    udp,
    webrtc::{self, Player, StreamStats, ViewerConfig, WhipConfig},
    EndpointTicket, WEBRTC_ALPN,
};

/// How many video-bitrate samples the stats sparkline keeps (oldest first). At the
/// 5-second stats tick that is a two-minute window.
const MAX_KBPS_SAMPLES: usize = 24;

/// How long a restarted watch session waits for the session it replaces to unwind
/// before bringing the new one up.
///
/// A `Restart` click aborts the previous task; the new task drains that handle first so
/// the old player's kill has been signalled (its `PlayerHandle` drops, firing the
/// watcher's `stop_rx.recv()` arm) before a new player is launched onto the same UDP
/// ports. The ceiling exists so a wedged old task can never block the new one.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// What the GUI is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Nothing running.
    Idle,
    /// A session is being brought up (binding, dialing, waiting online).
    Starting,
    /// Broadcasting: serving WHIP and forwarding to viewers.
    Broadcasting,
    /// Watching a remote stream in a player.
    Watching,
}

impl Mode {
    /// Whether a session is running or coming up.
    pub fn active(self) -> bool {
        !matches!(self, Mode::Idle)
    }
}

/// Optional fixed bind addresses for the endpoint, mirroring the CLI's
/// `--ipv4-addr` / `--ipv6-addr`.
///
/// Empty by default, so the endpoint binds an ephemeral port and the ticket changes
/// every session (the normal, hole-punched case). When a streamer has a static public
/// IP and sets a fixed `ip:port` here, the endpoint binds that exact socket, the
/// ticket's direct addresses stop churning, and the ticket stays byte-stable across
/// sessions — useful when a friend saved the ticket and you want it to keep working.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BindAddrs {
    /// The IPv4 socket to bind, if fixed.
    pub ipv4: Option<SocketAddrV4>,
    /// The IPv6 socket to bind, if fixed.
    pub ipv6: Option<SocketAddrV6>,
}

/// The cross-thread state shared by the UI thread, the stats consumer task and the
/// engine session task.
#[derive(Debug)]
pub struct SharedState {
    /// The stable endpoint identity.
    secret: SecretKey,
    /// The shared endpoint, created lazily and reused across sessions.
    endpoint: Option<Endpoint>,
    /// The bind addresses the cached endpoint was bound with, so a change to the
    /// configured bind addresses forces a rebind rather than silently reusing an
    /// endpoint on the old (ephemeral) socket.
    bind: BindAddrs,
    /// The running broadcast task, if any.
    pub broadcast_session: Option<JoinHandle<()>>,
    /// The running watch task, if any. Broadcasting and watching are independent
    /// sessions that may run at the same time, so each track owns its own handle.
    pub watch_session: Option<JoinHandle<()>>,
    /// What the live watch session was launched with: `(player, buffer_ms)`, or
    /// `None` while no watch session is up.
    pub watch_applied: Option<(Player, Option<u64>)>,
    /// Whether the live watch session is currently receiving media from the
    /// broadcaster: `false` while idle, connecting, connected-but-waiting, or after
    /// the stream ends.
    pub watch_receiving: bool,
    /// The broadcast track's mode (`Idle` / `Starting` / `Broadcasting`).
    pub broadcast: Mode,
    /// The watch track's mode (`Idle` / `Starting` / `Watching`).
    pub watch: Mode,
    /// The live statistics of a broadcast, `None` when not broadcasting.
    pub stats: Option<StreamStats>,
    /// Recent video-bitrate samples (kbit/s), oldest first, for the stats sparkline.
    /// Fed by [`consume_stats`]; cleared when the broadcast ends.
    pub video_kbps_history: VecDeque<f64>,
    /// The current broadcast ticket, shown after the endpoint is online.
    pub ticket: Option<String>,
    /// Whether a ticket refresh is in flight (rebinding the endpoint to pick up a
    /// fresh relay / socket and mint a new ticket). Disables the refresh button so a
    /// double click cannot rebind twice.
    pub refreshing: bool,
    /// A one-shot completion notice for the UI (e.g. "Relay refreshed ✓"), set by a
    /// finished refresh/reset task and taken by the draw loop to raise a toast.
    pub notice: Option<String>,
    /// The WHIP URL to give OBS, tracking the configured host and port.
    pub whip_url: String,
    /// The last error surfaced on the Broadcast tab.
    pub broadcast_error: Option<String>,
    /// The last error surfaced on the Watch tab.
    pub watch_error: Option<String>,
}

impl SharedState {
    /// Create the state from a loaded config's secret and WHIP settings.
    pub fn new(secret: SecretKey, whip_url: String) -> Self {
        SharedState {
            secret,
            endpoint: None,
            bind: BindAddrs::default(),
            broadcast_session: None,
            watch_session: None,
            watch_applied: None,
            watch_receiving: false,
            broadcast: Mode::Idle,
            watch: Mode::Idle,
            stats: None,
            video_kbps_history: VecDeque::new(),
            ticket: None,
            refreshing: false,
            notice: None,
            whip_url,
            broadcast_error: None,
            watch_error: None,
        }
    }
}

/// A locked snapshot of the fields the UI renders each frame, taken under one brief
/// lock so the UI never holds the mutex while building widgets.
#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub broadcast: Mode,
    pub watch: Mode,
    /// What the live watch session was launched with (see
    /// [`SharedState::watch_applied`]); `None` while the watch track is idle.
    pub watch_applied: Option<(Player, Option<u64>)>,
    /// Whether the live watch session is receiving media, so the tab can tell
    /// "playing" from "waiting for the streamer".
    pub watch_receiving: bool,
    pub stats: Option<StreamStats>,
    pub video_kbps_history: Vec<f64>,
    pub ticket: Option<String>,
    pub whip_url: String,
    pub broadcast_error: Option<String>,
    pub watch_error: Option<String>,
}

impl UiSnapshot {
    /// Whether either track is running or coming up: a broadcast, a watch, or both.
    pub fn any_active(&self) -> bool {
        self.broadcast.active() || self.watch.active()
    }

    /// A short combined status label for the header, naming whichever tracks are live
    /// ("Broadcasting", "Watching", or "Broadcasting + watching").
    pub fn status(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        match self.broadcast {
            Mode::Starting => parts.push("Starting broadcast"),
            Mode::Broadcasting => parts.push("Broadcasting"),
            _ => {}
        }
        match self.watch {
            Mode::Starting => parts.push("Connecting"),
            Mode::Watching => parts.push("Watching"),
            _ => {}
        }
        if parts.is_empty() {
            "Idle".to_string()
        } else {
            parts.join(" + ")
        }
    }
}

/// Snapshot the render-relevant state.
pub fn snapshot(state: &Arc<Mutex<SharedState>>) -> UiSnapshot {
    let g = state.lock().unwrap();
    UiSnapshot {
        broadcast: g.broadcast,
        watch: g.watch,
        // `Copy`, so the record is read under the same brief lock as the mode it's in.
        watch_applied: g.watch_applied,
        watch_receiving: g.watch_receiving,
        stats: g.stats,
        video_kbps_history: g.video_kbps_history.iter().copied().collect(),
        ticket: g.ticket.clone(),
        whip_url: g.whip_url.clone(),
        broadcast_error: g.broadcast_error.clone(),
        watch_error: g.watch_error.clone(),
    }
}

/// The WHIP URL for a host and port, e.g. `http://127.0.0.1:8080/whip`.
pub fn whip_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}/whip")
}

/// Bring up the shared endpoint, or return the existing one. Never holds the mutex
/// across the `bind`, so it is safe with `std::Mutex`.
///
/// A cached endpoint is reused only while it is still open. `listen_whip` closes the
/// endpoint it is handed when it returns — including on an engine error, or when the
/// WHIP bind fails (a port already in use) — and the abort path of `stop` deliberately
/// leaves it open for reuse. So a stored endpoint that reports `is_closed` is a dead
/// handle: handing it back would let the next broadcast bind nothing, `accept` would
/// return `None` at once, and the session would flash to `Broadcasting` and straight
/// back to `Idle` with no error. Drop it and bind a fresh one instead; the identity is
/// unchanged (same secret), so a persisted ticket still points at this endpoint.
async fn ensure_endpoint(
    state: &Arc<Mutex<SharedState>>,
    bind: BindAddrs,
) -> n0_error::Result<Endpoint> {
    let cached = {
        let g = state.lock().unwrap();
        g.endpoint.clone().filter(|_| g.bind == bind)
    };
    if let Some(endpoint) = cached {
        if !endpoint.is_closed() {
            return Ok(endpoint);
        }
        tracing::debug!("the cached endpoint is closed, binding a fresh one");
    }
    let secret = state.lock().unwrap().secret.clone();
    let common = CommonArgs {
        ipv4_addr: bind.ipv4,
        ipv6_addr: bind.ipv6,
        custom_alpn: None,
        // Verbose enough to log the endpoint lifecycle to the log panel.
        verbose: 1,
        verbose_max: 3,
    };
    let alpn = webrtc_alpn(&common)?;
    let endpoint = create_endpoint(secret, &common, vec![alpn]).await?;
    let mut g = state.lock().unwrap();
    g.endpoint = Some(endpoint.clone());
    g.bind = bind;
    Ok(endpoint)
}

/// Which session track a lifecycle action applies to. Broadcasting and watching run as
/// independent sessions, so failures, mode changes and stops are routed to the owning
/// track rather than to a single global session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Broadcast,
    Watch,
}

/// Mark one track's session failed and return it to idle, surfacing `msg` to the UI on
/// that track's tab.
fn fail(state: &Arc<Mutex<SharedState>>, ctx: &egui::Context, track: Track, msg: String) {
    tracing::error!("{msg}");
    {
        let mut g = state.lock().unwrap();
        match track {
            Track::Broadcast => {
                g.broadcast = Mode::Idle;
                g.stats = None;
                g.video_kbps_history.clear();
                g.broadcast_error = Some(msg);
            }
            Track::Watch => {
                g.watch = Mode::Idle;
                g.watch_applied = None;
                g.watch_receiving = false;
                g.watch_error = Some(msg);
            }
        }
    }
    ctx.request_repaint();
}

/// The parameters needed to bring up a broadcast.
pub struct BroadcastParams {
    /// The `host:port` string to resolve and serve WHIP on.
    pub listen: String,
    /// The WHIP URL to display (uses the host/port as typed).
    pub url: String,
    /// The bearer token OBS must present, if any.
    pub bearer_token: Option<String>,
    /// Optional fixed bind addresses, so a streamer with a static IP can keep the
    /// ticket stable. Empty by default.
    pub bind: BindAddrs,
}

/// Start a broadcast session, returning its task handle to store in the state.
///
/// Sets the mode to `Starting` synchronously, then brings the endpoint online, mints a
/// fresh ticket from the live endpoint address (exactly like the CLI `listen-whip`) and
/// runs [`webrtc::listen_whip`] until it returns or is aborted.
pub fn spawn_broadcast(
    state: Arc<Mutex<SharedState>>,
    handle: Handle,
    ctx: egui::Context,
    params: BroadcastParams,
    stats_tx: mpsc::UnboundedSender<StreamStats>,
) -> JoinHandle<()> {
    {
        let mut g = state.lock().unwrap();
        g.broadcast = Mode::Starting;
        g.broadcast_error = None;
        // Show a zeroed stats panel immediately, before any viewer joins.
        g.stats = Some(StreamStats::default());
        g.ticket = None;
    }
    ctx.request_repaint();
    handle.spawn(async move {
        let endpoint = match ensure_endpoint(&state, params.bind).await {
            Ok(endpoint) => endpoint,
            Err(e) => {
                return fail(
                    &state,
                    &ctx,
                    Track::Broadcast,
                    format!("could not bind endpoint: {e:#}"),
                )
            }
        };
        if timeout(ONLINE_TIMEOUT, endpoint.online()).await.is_err() {
            tracing::warn!("failed to connect to the home relay");
        }
        let addr = endpoint.addr();
        // Always mint the ticket from the *live* endpoint address, like the CLI. The
        // ticket embeds the endpoint's current relay and public socket hints, so a
        // viewer dialing it can hole-punch direct instead of falling back to a far
        // relay — which the player reports as a lossy "long-distance path" with missed
        // RTP packets and H.264 decode errors. Re-displaying a persisted ticket was the
        // bug: its transport addresses go stale the moment the NAT mapping or relay
        // changes. Identity stays stable via the persisted secret, so an *older* ticket
        // a friend saved still resolves to this endpoint.
        let shown = EndpointTicket::new(addr).to_string();
        tracing::debug!("fresh ticket for this session: {shown}");
        let listen = match udp::resolve(&params.listen) {
            Ok(listen) => listen,
            Err(e) => {
                return fail(
                    &state,
                    &ctx,
                    Track::Broadcast,
                    format!("invalid listen address: {e:#}"),
                );
            }
        };
        {
            let mut g = state.lock().unwrap();
            g.ticket = Some(shown);
            g.whip_url = params.url.clone();
            g.broadcast = Mode::Broadcasting;
        }
        tracing::info!("serving WHIP on {} and waiting for OBS", params.url);
        ctx.request_repaint();

        let cfg = WhipConfig {
            listen,
            bearer_token: params.bearer_token,
            ice_addr: None,
            stats: true,
            stats_tx: Some(stats_tx),
        };
        let result = webrtc::listen_whip(endpoint, cfg).await;

        let mut g = state.lock().unwrap();
        g.broadcast = Mode::Idle;
        g.stats = None;
        g.video_kbps_history.clear();
        if let Err(e) = result {
            g.broadcast_error = Some(format!("{e:#}"));
        }
        ctx.request_repaint();
    })
}

/// Bring an online endpoint's live address into a ticket, store it as the current
/// ticket, clear `refreshing`, and raise `done_msg` as a completion notice. Shared by
/// the relay-refresh and ticket-reset tasks: both rebind and then need the identical
/// "wait online, mint, publish" tail.
async fn mint_and_publish(
    state: &Arc<Mutex<SharedState>>,
    endpoint: &Endpoint,
    ctx: &egui::Context,
    done_msg: &'static str,
) {
    if timeout(ONLINE_TIMEOUT, endpoint.online()).await.is_err() {
        tracing::warn!("failed to connect to the home relay");
    }
    let shown = EndpointTicket::new(endpoint.addr()).to_string();
    tracing::debug!("fresh ticket: {shown}");
    let mut g = state.lock().unwrap();
    g.refreshing = false;
    g.ticket = Some(shown);
    g.notice = Some(done_msg.to_string());
    drop(g);
    ctx.request_repaint();
}

/// Refresh the saved broadcast ticket without starting a session.
///
/// Drops the cached endpoint so the refresh rebinds and picks up a fresh relay / socket,
/// then mints a new ticket from the live endpoint address (exactly like the start path)
/// and stores it as the current ticket. Identity is unchanged (same secret), so
/// previously shared tickets still resolve. The UI persists it to
/// `config.broadcast_ticket`. Only offered while idle, so a refresh can never rebind the
/// endpoint out from under a running stream.
pub fn spawn_refresh_ticket(
    state: Arc<Mutex<SharedState>>,
    handle: Handle,
    ctx: egui::Context,
    bind: BindAddrs,
    done_msg: &'static str,
) {
    {
        let mut g = state.lock().unwrap();
        g.refreshing = true;
        g.broadcast_error = None;
        g.notice = None;
        // Drop the cached endpoint so `ensure_endpoint` rebinds rather than handing back
        // the old socket — that rebind is what yields a new relay and ticket.
        g.endpoint = None;
    }
    ctx.request_repaint();
    handle.spawn(async move {
        let endpoint = match ensure_endpoint(&state, bind).await {
            Ok(endpoint) => endpoint,
            Err(e) => {
                let mut g = state.lock().unwrap();
                g.refreshing = false;
                g.broadcast_error = Some(format!("could not bind endpoint: {e:#}"));
                drop(g);
                ctx.request_repaint();
                return;
            }
        };
        mint_and_publish(&state, &endpoint, &ctx, done_msg).await;
    });
}

/// Reset the broadcast identity to a brand-new keypair and mint a fresh ticket.
///
/// Like [`spawn_refresh_ticket`] but installs `secret` as the endpoint's identity first,
/// so the rebind produces a new endpoint id and a ticket that shares nothing with any
/// previously issued one. The caller persists the new secret to `config.secret_hex`.
/// Only offered while idle.
pub fn spawn_reset_ticket(
    state: Arc<Mutex<SharedState>>,
    handle: Handle,
    ctx: egui::Context,
    secret: SecretKey,
    bind: BindAddrs,
    done_msg: &'static str,
) {
    {
        let mut g = state.lock().unwrap();
        g.refreshing = true;
        g.broadcast_error = None;
        g.notice = None;
        g.secret = secret;
        g.endpoint = None;
    }
    ctx.request_repaint();
    handle.spawn(async move {
        let endpoint = match ensure_endpoint(&state, bind).await {
            Ok(endpoint) => endpoint,
            Err(e) => {
                let mut g = state.lock().unwrap();
                g.refreshing = false;
                g.broadcast_error = Some(format!("could not bind endpoint: {e:#}"));
                drop(g);
                ctx.request_repaint();
                return;
            }
        };
        mint_and_publish(&state, &endpoint, &ctx, done_msg).await;
    });
}

/// The parameters needed to start watching a stream.
pub struct WatchParams {
    /// The parsed remote endpoint address.
    pub addr: EndpointAddr,
    /// The player to launch.
    pub player: Player,
    /// An explicit player binary path, if configured.
    pub player_path: Option<PathBuf>,
    /// The local play-out address.
    pub play_addr: String,
    /// The jitter buffer in milliseconds, `None` for lowest latency.
    pub buffer_ms: Option<u64>,
    /// The player's startup volume in percent (0-100), 100 for its own default.
    pub volume: u8,
}

/// Whether a watch ticket points at our own endpoint identity.
///
/// iroh refuses to `connect` to an `EndpointId` equal to the local one ("Connecting to
/// ourself is not supported"), so a ticket carrying our own id cannot be dialled from
/// the endpoint that holds that identity. This happens when you watch your own broadcast
/// — in a second GUI sharing one config, or in this same instance while broadcasting. The
/// comparison uses the live [`SharedState::secret`] — the identity `ensure_endpoint`
/// binds and which `spawn_reset_ticket` mutates on an identity reset — so it never
/// misclassifies mid-reset. [`spawn_watch`] dials under a throwaway id (so self-watch
/// always works) and uses this only to log the reason clearly.
fn is_self_watch(addr: &EndpointAddr, secret: &SecretKey) -> bool {
    addr.id == secret.public()
}

/// The plain sentence the Watch tab shows when a stream ends.
const STREAM_STOPPED: &str = "Broadcaster has stopped streaming.";

/// The engine's wording for "the viewer's datagram read failed", as built by
/// `webrtc::connect_whip` when `Connection::read_datagram` returns an error.
const DATAGRAM_READ_FAILED: &str = "error reading datagrams from the host:";

/// Map a raw watch-session failure to the text the Watch tab shows.
///
/// When the broadcaster stops, the viewer's datagram read is the first thing to fail,
/// and the engine reports it as `error reading datagrams from the host: timed out` — the
/// QUIC connection idles out with nothing left to read. That is transport noise to a
/// viewer, who only wants to know the stream is over, so that one case is rewritten to
/// [`STREAM_STOPPED`]. Every other failure is shown verbatim: there the transport detail
/// is the part that actually explains what went wrong.
fn friendly_watch_error(raw: &str) -> &str {
    let Some(detail) = raw.strip_prefix(DATAGRAM_READ_FAILED) else {
        return raw;
    };
    // Only the timeout, exactly: a reset or an application close means the broadcaster is
    // already gone, and the wording says so.
    if detail.trim() == "timed out" {
        STREAM_STOPPED
    } else {
        raw
    }
}

/// Start a watch session, returning its task handle to store in the state.
///
/// Mirrors [`spawn_broadcast`] on its own track: sets `watch` to `Starting`
/// synchronously, then brings the endpoint online and resolves the play-out address,
/// flipping to `Watching` only once the play address resolves and immediately before
/// dialing (the closest honest "ready" signal available — [`ViewerConfig`] carries no
/// player-launch callback).
///
/// The watch always dials from its own throwaway connect-only endpoint built from a
/// freshly generated [`SecretKey`], never the shared broadcast endpoint. That keeps
/// broadcasting and watching fully independent within one instance:
/// - it sidesteps iroh's self-connect rejection when the ticket is our own identity
///   ([`is_self_watch`]) — the throwaway id always differs from the broadcast id;
/// - it avoids binding two endpoints with the *same* secret (a fresh id, so the relay
///   never sees a duplicate) while we are also broadcasting on the shared one;
/// - stopping either session can never tear down the other, because only `listen_whip`
///   closes the shared endpoint and the watch never touches it.
///
/// The throwaway endpoint is never written to `config.secret_hex` nor stored in
/// `SharedState::endpoint`; it drops when the task ends (normally, on error, or when
/// aborted by [`stop`]).
///
/// `previous` is the handle of the watch session this one replaces, if any (a
/// `Restart` click). It is aborted in the same lock scope that arms `Starting` and
/// records `watch_applied`, and the new task then waits (bounded by
/// [`TEARDOWN_TIMEOUT`]) for it to unwind before doing any endpoint work — so the old
/// player's kill has been signalled before a new player is launched onto the same UDP
/// ports, and at most one watch session is ever live.
pub fn spawn_watch(
    state: Arc<Mutex<SharedState>>,
    handle: Handle,
    ctx: egui::Context,
    previous: Option<JoinHandle<()>>,
    params: WatchParams,
) -> JoinHandle<()> {
    {
        // One lock: mode flip, the record the prompt compares against, and the abort of
        // the session being replaced all happen atomically, so the UI can never show a
        // live track with no record (or the reverse).
        let mut g = state.lock().unwrap();
        g.watch = Mode::Starting;
        g.watch_error = None;
        g.watch_applied = Some((params.player, params.buffer_ms));
        if let Some(previous) = &previous {
            previous.abort();
        }
    }
    ctx.request_repaint();
    handle.spawn(async move {
        // Let the session we replace unwind first, outside every lock scope (the module's
        // rule is that the lock is never held across an `.await`). Aborting it drops its
        // `PlayerHandle`, whose sole `stop_tx` sender closing fires the watcher's
        // `stop_rx.recv()` arm, i.e. the player's kill is signalled; the bounded wait
        // gives that watcher time to finish. A finished handle returns immediately, and a
        // wedged one is cut off at the ceiling.
        if let Some(previous) = previous {
            let _ = timeout(TEARDOWN_TIMEOUT, previous).await;
        }
        // Read the live identity once, releasing the lock before any `.await`, purely to
        // log a self-watch clearly; the endpoint is throwaway either way.
        let is_self = is_self_watch(&params.addr, &state.lock().unwrap().secret);
        if is_self {
            tracing::info!(
                "ticket points at our own identity ({}) — watching under a throwaway id",
                params.addr.id
            );
        }
        // Ephemeral bind (no fixed addrs), no registered alpns (the dial ALPN rides in
        // `ViewerConfig`), and the same verbosity as `ensure_endpoint` so the log panel
        // shows the endpoint lifecycle.
        let common = CommonArgs {
            ipv4_addr: None,
            ipv6_addr: None,
            custom_alpn: None,
            verbose: 1,
            verbose_max: 3,
        };
        let secret = SecretKey::generate();
        let endpoint = match create_endpoint(secret, &common, vec![]).await {
            Ok(endpoint) => endpoint,
            Err(e) => {
                return fail(
                    &state,
                    &ctx,
                    Track::Watch,
                    format!("could not bind endpoint: {e:#}"),
                )
            }
        };
        // Online before dialing — the throwaway endpoint must reach a relay to dial a
        // relay-only ticket (including our own broadcast's).
        if timeout(ONLINE_TIMEOUT, endpoint.online()).await.is_err() {
            tracing::warn!("failed to connect to the home relay");
        }
        let play = match udp::resolve(&params.play_addr) {
            Ok(play) => play,
            Err(e) => {
                return fail(
                    &state,
                    &ctx,
                    Track::Watch,
                    format!("invalid play address: {e:#}"),
                )
            }
        };
        // The endpoint is online and the play address resolves: we are ready to dial.
        // Flip to `Watching` now (just before `connect_whip`, the sole player-launch
        // point) so the UI never claims "playing" during bring-up.
        {
            let mut g = state.lock().unwrap();
            g.watch = Mode::Watching;
        }
        ctx.request_repaint();
        // Mirror the engine's media gate into the shared state, so the tab can read
        // "waiting for the streamer to begin broadcasting…" while the player has nothing
        // to show, and "playing in a player window" once media actually flows. The sender
        // lives in `cfg`, which `connect_whip` owns, so the channel closes when the
        // session ends and the mirror drains with it.
        let (media_tx, mut media_rx) = mpsc::unbounded_channel::<bool>();
        let cfg = ViewerConfig {
            addr: params.addr,
            alpn: WEBRTC_ALPN.to_vec(),
            play,
            sdp: None,
            sdp_here: false,
            player: params.player,
            player_path: params.player_path,
            no_launch: false,
            buffer: params.buffer_ms.map(Duration::from_millis),
            volume: params.volume,
            stats: true,
            media_state: Some(media_tx),
        };
        let mirror_state = state.clone();
        let mirror_ctx = ctx.clone();
        let mirror = tokio::spawn(async move {
            while let Some(receiving) = media_rx.recv().await {
                {
                    let mut g = mirror_state.lock().unwrap();
                    g.watch_receiving = receiving;
                }
                // The label changed, so repaint now rather than on the next tick.
                mirror_ctx.request_repaint();
            }
        });
        let result = webrtc::connect_whip(endpoint, cfg).await;
        // The sender went away with `cfg`, so the mirror has already seen the channel
        // close; the abort is belt-and-braces so nothing outlives the session.
        mirror.abort();

        let mut g = state.lock().unwrap();
        g.watch = Mode::Idle;
        // Same lock scope as the mode flip, so the record never outlives the session
        // it belongs to.
        g.watch_applied = None;
        g.watch_receiving = false;
        if let Err(e) = result {
            g.watch_error = Some(friendly_watch_error(&format!("{e:#}")).to_string());
        }
        ctx.request_repaint();
    })
}

/// Abort one track's session, if any, returning that track to idle.
///
/// Synchronous by design: `abort` drops the engine future, which frees the WHIP port,
/// stops accepting, kills the player, and — since the engine keeps its connection and
/// viewer tasks in `AbortOnDrop` join sets — aborts every spawned connection handler
/// too, so a stopped session leaves nothing bound, connected, or processing. The shared
/// endpoint is kept for reuse. Stopping one track never disturbs the other — a live
/// broadcast keeps serving while you stop a watch and vice versa — because the watch
/// owns its own throwaway endpoint and the broadcast owns the shared one.
pub fn stop(state: &Arc<Mutex<SharedState>>, ctx: &egui::Context, track: Track) {
    let session = {
        let mut g = state.lock().unwrap();
        match track {
            Track::Broadcast => {
                g.broadcast = Mode::Idle;
                g.stats = None;
                g.video_kbps_history.clear();
                g.broadcast_session.take()
            }
            Track::Watch => {
                g.watch = Mode::Idle;
                g.watch_applied = None;
                g.watch_receiving = false;
                g.watch_session.take()
            }
        }
    };
    if let Some(session) = session {
        session.abort();
    }
    ctx.request_repaint();
}

/// Abort every running session (used when the window closes), leaving both tracks idle.
pub fn stop_all(state: &Arc<Mutex<SharedState>>, ctx: &egui::Context) {
    let (broadcast, watch) = {
        let mut g = state.lock().unwrap();
        g.broadcast = Mode::Idle;
        g.watch = Mode::Idle;
        g.watch_applied = None;
        g.watch_receiving = false;
        g.stats = None;
        g.video_kbps_history.clear();
        (g.broadcast_session.take(), g.watch_session.take())
    };
    if let Some(session) = broadcast {
        session.abort();
    }
    if let Some(session) = watch {
        session.abort();
    }
    ctx.request_repaint();
}

/// Drain the stats channel into the shared state and repaint. This task owns the only
/// async path that touches the mutex, and it locks only inside the loop body (never
/// across an `.await`).
pub async fn consume_stats(
    mut rx: mpsc::UnboundedReceiver<StreamStats>,
    state: Arc<Mutex<SharedState>>,
    ctx: egui::Context,
) {
    while let Some(stats) = rx.recv().await {
        {
            let mut g = state.lock().unwrap();
            g.stats = Some(stats);
            // Keep the sparkline's window bounded, oldest first.
            g.video_kbps_history.push_back(stats.video_kbps);
            while g.video_kbps_history.len() > MAX_KBPS_SAMPLES {
                g.video_kbps_history.pop_front();
            }
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // This module is `gui::state::tests`, so the sibling `config` module is two up.
    use crate::gui::config::DEFAULT_VOLUME;

    /// Regression (AC1): `spawn_watch` used to set `Watching` synchronously, so the
    /// Watch tab claimed "playing in a player window" the instant the button was
    /// clicked, while the endpoint was still binding/coming online. It must arm
    /// `Starting` synchronously (mirroring `spawn_broadcast`) and only flip to
    /// `Watching` later, after `udp::resolve` succeeds and just before dialing.
    ///
    /// A `current_thread` runtime is built but never run, so the async body is never
    /// polled and the mode stays where the synchronous block left it.
    #[test]
    fn spawn_watch_arms_starting_before_it_comes_online() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let secret = SecretKey::generate();
        let state = Arc::new(Mutex::new(SharedState::new(
            secret.clone(),
            whip_url("127.0.0.1", 8080),
        )));

        spawn_watch(
            state.clone(),
            rt.handle().clone(),
            egui::Context::default(),
            None,
            WatchParams {
                addr: EndpointAddr::new(secret.public()),
                player: Player::default(),
                player_path: None,
                play_addr: "127.0.0.1:0".into(),
                buffer_ms: None,
                volume: DEFAULT_VOLUME,
            },
        );

        assert_eq!(
            state.lock().unwrap().watch,
            Mode::Starting,
            "the watch track must be `Starting` immediately, not `Watching`"
        );
        assert_eq!(
            state.lock().unwrap().broadcast,
            Mode::Idle,
            "starting a watch must leave the broadcast track idle"
        );
        drop(rt);
    }

    /// Regression (AC1/AC2): a ticket carrying our own endpoint id must be detected as
    /// a self-watch (so `spawn_watch` logs the reason and dials under a throwaway id
    /// instead of the shared endpoint, which iroh would reject with "Connecting to
    /// ourself is not supported"), while a ticket for any other id is not.
    #[test]
    fn self_watch_is_detected_from_the_ticket_endpoint_id() {
        let secret = SecretKey::generate();
        let own = EndpointAddr::new(secret.public());
        let other = EndpointAddr::new(SecretKey::generate().public());

        assert!(
            is_self_watch(&own, &secret),
            "a ticket with our own endpoint id must be treated as self-watch"
        );
        assert!(
            !is_self_watch(&other, &secret),
            "a ticket with a different endpoint id must not be"
        );
    }

    /// The combined status label names whichever tracks are live, so broadcasting and
    /// watching at once is legible in the header rather than one hiding the other.
    #[test]
    fn status_names_both_live_tracks() {
        let snap = |broadcast: Mode, watch: Mode| UiSnapshot {
            broadcast,
            watch,
            watch_applied: None,
            watch_receiving: false,
            stats: None,
            video_kbps_history: Vec::new(),
            ticket: None,
            whip_url: String::new(),
            broadcast_error: None,
            watch_error: None,
        };
        assert_eq!(snap(Mode::Idle, Mode::Idle).status(), "Idle");
        assert_eq!(
            snap(Mode::Broadcasting, Mode::Idle).status(),
            "Broadcasting"
        );
        assert_eq!(snap(Mode::Idle, Mode::Watching).status(), "Watching");
        let both = snap(Mode::Broadcasting, Mode::Watching);
        assert_eq!(both.status(), "Broadcasting + Watching");
        assert!(both.any_active());
        // A single live track is still "active"; both idle is not.
        assert!(snap(Mode::Idle, Mode::Starting).any_active());
        assert!(!snap(Mode::Idle, Mode::Idle).any_active());
    }

    /// `consume_stats` appends each sample to the sparkline history, oldest first, and
    /// keeps the window bounded at [`MAX_KBPS_SAMPLES`], evicting the oldest first.
    #[tokio::test]
    async fn consume_stats_feeds_and_bounds_the_bitrate_history() {
        let state = Arc::new(Mutex::new(SharedState::new(
            SecretKey::generate(),
            whip_url("127.0.0.1", 8080),
        )));
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(consume_stats(rx, state.clone(), egui::Context::default()));
        let total = MAX_KBPS_SAMPLES + 5;
        for i in 0..total {
            tx.send(StreamStats {
                video_kbps: i as f64,
                ..Default::default()
            })
            .unwrap();
        }
        drop(tx);
        task.await.unwrap();
        let h = state
            .lock()
            .unwrap()
            .video_kbps_history
            .iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(h.len(), MAX_KBPS_SAMPLES, "the window is bounded");
        assert_eq!(
            h[0],
            (total - MAX_KBPS_SAMPLES) as f64,
            "the oldest samples were evicted, oldest first"
        );
        assert_eq!(
            h[h.len() - 1],
            (total - 1) as f64,
            "the newest sample is last"
        );
    }

    /// Stopping the broadcast track must leave a live watch track (and vice versa)
    /// running — the two sessions are independent within one instance.
    #[test]
    fn stopping_one_track_leaves_the_other_idle_state_untouched() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = Arc::new(Mutex::new(SharedState::new(
            SecretKey::generate(),
            whip_url("127.0.0.1", 8080),
        )));
        {
            let mut g = state.lock().unwrap();
            g.broadcast = Mode::Broadcasting;
            g.watch = Mode::Watching;
            g.stats = Some(StreamStats::default());
        }
        stop(&state, &egui::Context::default(), Track::Broadcast);
        {
            let g = state.lock().unwrap();
            assert_eq!(g.broadcast, Mode::Idle, "broadcast stopped");
            assert_eq!(
                g.watch,
                Mode::Watching,
                "watch untouched by a broadcast stop"
            );
            assert!(
                g.stats.is_none(),
                "broadcast stats cleared with the broadcast"
            );
        }
        stop(&state, &egui::Context::default(), Track::Watch);
        assert_eq!(
            state.lock().unwrap().watch,
            Mode::Idle,
            "the watch stops independently afterwards"
        );
        drop(rt);
    }

    /// A cached endpoint is reused while open, but a closed one (as `listen_whip` leaves
    /// it on any early return, e.g. a WHIP bind failure) must be replaced with a live
    /// handle so the next broadcast actually binds. Regression: the old
    /// `ensure_endpoint` handed back the closed endpoint, whose `accept` returns `None`
    /// at once, so a "port in use → fix port → Start again" cycle flashed
    /// `Broadcasting` and fell back to `Idle` while serving nothing.
    #[tokio::test]
    async fn a_closed_endpoint_is_replaced_on_the_next_session() {
        let state = Arc::new(Mutex::new(SharedState::new(
            SecretKey::generate(),
            whip_url("127.0.0.1", 8080),
        )));

        let first = ensure_endpoint(&state, BindAddrs::default())
            .await
            .expect("bind");
        // While open, the same endpoint is reused across calls.
        let reused = ensure_endpoint(&state, BindAddrs::default())
            .await
            .expect("reuse");
        assert!(!reused.is_closed());

        // `listen_whip` closes the endpoint it was handed when it returns.
        first.close().await;
        assert!(first.is_closed());

        // The next session must get a fresh, live endpoint, not the dead one.
        let fresh = ensure_endpoint(&state, BindAddrs::default())
            .await
            .expect("rebind");
        assert!(
            !fresh.is_closed(),
            "a closed endpoint must be replaced with a live one"
        );
        // Identity is stable across the rebind (same secret).
        assert_eq!(fresh.id(), reused.id());
    }

    /// A live endpoint is reused only while the requested bind addresses match; a
    /// changed bind must rebind so the ticket carries the new fixed socket.
    #[tokio::test]
    async fn a_changed_bind_forces_a_rebind() {
        let state = Arc::new(Mutex::new(SharedState::new(
            SecretKey::generate(),
            whip_url("127.0.0.1", 8080),
        )));

        let ephemeral = ensure_endpoint(&state, BindAddrs::default())
            .await
            .expect("bind ephemeral");
        assert!(!ephemeral.is_closed());

        // Same (empty) bind reuses the live endpoint.
        let reused = ensure_endpoint(&state, BindAddrs::default())
            .await
            .expect("reuse");
        assert_eq!(reused.id(), ephemeral.id());

        // A fixed IPv4 bind on loopback differs from empty → rebind.
        let fixed = BindAddrs {
            ipv4: Some("127.0.0.1:0".parse().unwrap()),
            ipv6: None,
        };
        let rebound = ensure_endpoint(&state, fixed).await.expect("rebind");
        assert!(!rebound.is_closed());
        // Identity unchanged (same secret); only the socket differs.
        assert_eq!(rebound.id(), ephemeral.id());
        // The requested bind is remembered, so the next identical request reuses.
        assert_eq!(state.lock().unwrap().bind, fixed);
        let reused_fixed = ensure_endpoint(&state, fixed).await.expect("reuse fixed");
        assert_eq!(reused_fixed.id(), rebound.id());
    }

    /// The stream-ended case — the viewer's datagram read timing out — reads as a plain
    /// sentence on the Watch tab, while every other failure keeps the transport detail
    /// that makes it diagnosable.
    #[test]
    fn a_timed_out_datagram_read_reads_as_a_stopped_stream() {
        assert_eq!(
            friendly_watch_error("error reading datagrams from the host: timed out"),
            STREAM_STOPPED
        );
        // Unrelated failures are shown verbatim, including a timeout on some other
        // operation and a read that failed for a different reason.
        for raw in [
            "error connecting to the host: timed out",
            "error reading datagrams from the host: connection closed",
            "could not bind endpoint: address in use",
            // Not the exact detail: do not rewrite a message that merely carries the
            // words.
            "error reading datagrams from the host: timed out after 30s",
        ] {
            assert_eq!(friendly_watch_error(raw), raw, "{raw} must pass through");
        }
    }

    /// AC3 (state level): a restart hands the previous session to `spawn_watch`, which
    /// aborts it and drains it (bounded) before bringing the new one up — so at most
    /// one watch session is live at any moment, the record moves to the new pair, and
    /// `stop` clears both the handle and the record.
    ///
    /// This proves the previous *future* unwound, i.e. the player's kill was *signalled*
    /// (dropping its `PlayerHandle` closes the sole `stop_tx` sender, which fires the
    /// watcher's `stop_rx.recv()` arm). It does not prove the child process is gone, nor
    /// that the UDP play ports are free — the manual run covers that. Complementary, not
    /// redundant.
    #[test]
    fn restarting_a_watch_tears_the_previous_session_down_first() {
        // A `current_thread` runtime, driven by `block_on` so the sleeps (and the
        // `timeout` in the drain) have a live time driver.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let secret = SecretKey::generate();
        let state = Arc::new(Mutex::new(SharedState::new(
            secret.clone(),
            whip_url("127.0.0.1", 8080),
        )));
        let ctx = egui::Context::default();

        // Session A: a stand-in that only sleeps, so nothing touches the network and the
        // abort is observable. `JoinHandle` is not `Clone` and is moved into the state,
        // so a drop-flag is what the test can watch from outside.
        let torn_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = torn_down.clone();
        let a = rt.spawn(async move {
            struct Flag(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for Flag {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _flag = Flag(flag);
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        // Drive the runtime so A is up and running before the restart. The sleep future
        // must be built *inside* `block_on`, where the time driver's context is live.
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });
        assert!(
            !torn_down.load(std::sync::atomic::Ordering::SeqCst),
            "A must still be live before the restart"
        );

        // Restart: A goes in as `previous`; the new session is armed with a different
        // player/buffer pair.
        let b = spawn_watch(
            state.clone(),
            rt.handle().clone(),
            ctx.clone(),
            Some(a),
            WatchParams {
                addr: EndpointAddr::new(secret.public()),
                player: Player::Vlc,
                player_path: None,
                play_addr: "127.0.0.1:0".into(),
                buffer_ms: Some(120),
                volume: DEFAULT_VOLUME,
            },
        );
        {
            let g = state.lock().unwrap();
            assert_eq!(g.watch, Mode::Starting, "the restart arms `Starting`");
            assert_eq!(
                g.watch_applied,
                Some((Player::Vlc, Some(120))),
                "the record moves to the pair the new session was launched with"
            );
        }

        // Drive again: A must be torn down, and B must still be live (it is draining
        // `previous`, bounded by `TEARDOWN_TIMEOUT`).
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });
        assert!(
            torn_down.load(std::sync::atomic::Ordering::SeqCst),
            "the previous session was never torn down"
        );
        assert!(
            !b.is_finished(),
            "the new session must still be running while the old one is torn down"
        );

        // `stop` returns the track to idle and clears both the handle and the record, so
        // the prompt can never be shown against a dead session.
        stop(&state, &ctx, Track::Watch);
        let g = state.lock().unwrap();
        assert_eq!(g.watch, Mode::Idle, "stop returns the track to idle");
        assert!(g.watch_session.is_none(), "stop clears the session handle");
        assert_eq!(g.watch_applied, None, "stop clears the record");
    }
}
