//! The GUI's cross-thread state: session lifecycle, stats plumbing, status labels.
//!
//! These are the regression tests for the "it claimed it was playing before it was"
//! family: the modes must be armed synchronously, torn down in order, and never
//! overlap.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use iroh::{EndpointAddr, SecretKey};
use streampipe::gui::config::DEFAULT_VOLUME;
use streampipe::gui::state::{
    consume_stats, ensure_endpoint, friendly_watch_error, is_self_watch, spawn_watch, stop,
    whip_url, BindAddrs, Mode, SharedState, Track, UiSnapshot, WatchParams, MAX_KBPS_SAMPLES,
    STREAM_STOPPED,
};
use streampipe::stream::{Player, StreamStats};
use tokio::sync::mpsc;

fn new_state() -> Arc<Mutex<SharedState>> {
    Arc::new(Mutex::new(SharedState::new(
        SecretKey::generate(),
        whip_url("127.0.0.1", 8080),
    )))
}

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
    let state = new_state();
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
    // `stop` spawns its teardown on the tokio runtime, so one has to be alive
    // for the whole test: keep the guard bound (as `_rt`) until the end.
    let _rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let state = new_state();
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
}

/// A cached endpoint is reused while open, but a closed one (as `listen_whip` leaves
/// it on any early return, e.g. a WHIP bind failure) must be replaced with a live
/// handle so the next broadcast actually binds. Regression: the old
/// `ensure_endpoint` handed back the closed endpoint, whose `accept` returns `None`
/// at once, so a "port in use → fix port → Start again" cycle flashed
/// `Broadcasting` and fell back to `Idle` while serving nothing.
#[tokio::test]
async fn a_closed_endpoint_is_replaced_on_the_next_session() {
    let state = new_state();

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
    let state = new_state();

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
