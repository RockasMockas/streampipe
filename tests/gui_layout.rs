//! Layout: how the tabs and the Options window arrange their controls, checked from
//! the painted shapes and the interactive-rect set.

mod common;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui;
use streampipe::{
    gui::{
        config::{Config, Contact, DEFAULT_VOLUME},
        options::CLOSE_GREY,
        state::Mode,
        Tab, ACCENT, WATCH_ACCENT,
    },
    stream::{Player, StreamStats},
};

use common::{
    click_at, drive_frames, options_interactive_rects, real_ticket, render_shapes, test_app,
    test_app_with, text_present, text_rect, watch_button_rects, watch_button_rects_with,
};

/// The Watch tab has a full-width volume slider on its own row under the
/// Player/Buffer line, with a matching `Player Initial Volume: N%` label.
#[test]
fn the_watch_tab_has_a_full_width_volume_slider() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    app.watch_volume = 40;
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);

    // The label names the control and carries the number.
    let label_rect = shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.text() == "Player Initial Volume: 40%" => {
                Some(egui::Rect::from_min_size(t.pos, t.galley.rect.size()))
            }
            _ => None,
        })
        .expect("the volume label should be painted");

    // The slider track: the widest rounded rect on the label's row, to its right.
    let track = shapes
        .iter()
        .filter_map(|s| match &s.shape {
            egui::epaint::Shape::Rect(r)
                if r.rect.left() >= label_rect.right()
                    && r.rect.max.y > label_rect.min.y
                    && r.rect.min.y < label_rect.max.y =>
            {
                Some(r.rect)
            }
            _ => None,
        })
        .max_by(|a, b| a.width().total_cmp(&b.width()))
        .expect("the slider track should be painted");

    assert!(
        track.width() > 400.0,
        "the slider should span most of the panel, got {track:?} vs label {label_rect:?}"
    );
    // It reaches the right edge (the panel is inset by its frame margins plus item
    // spacing, so "full width" lands ~40 points short of the window edge).
    assert!(
        track.right() >= size.x - 44.0,
        "the slider should reach the right edge, got {track:?}"
    );
}

/// The volume slider opens at half on a fresh install, and at whatever the user last
/// set once persisted.
#[test]
fn the_volume_slider_opens_at_half_and_reopens_where_it_was_left() {
    // Fresh install: opens at half, not full.
    let (app, _rt) = test_app();
    assert_eq!(app.watch_volume, DEFAULT_VOLUME);
    assert_eq!(app.watch_volume, 50);

    // A persisted 37 % seeds the slider at 37 %.
    let (app, _rt) = test_app_with(Config {
        volume: 37,
        ..Config::default()
    });
    assert_eq!(app.watch_volume, 37);

    // `sync_watch_from_config` pulls the slider back to the stored value.
    let (mut app, _rt) = test_app_with(Config {
        volume: 37,
        ..Config::default()
    });
    app.watch_volume = 80;
    app.sync_watch_from_config();
    assert_eq!(app.watch_volume, 37);

    // A hand-edited config above the slider's range is clamped, never wrapped.
    let (app, _rt) = test_app_with(Config {
        volume: 200,
        ..Config::default()
    });
    assert_eq!(app.watch_volume, 100);
}

/// Render the live broadcast tab at the minimum window size: egui does the layout,
/// and we check it paints, that the toast adds its own painting, and that nothing
/// escapes the window.
#[test]
fn broadcast_tab_renders_at_min_size() {
    // A live broadcast persists its shown ticket to config; redirect the config dir
    // (the mutex serialises these tests) so the fake ticket never lands in the real
    // user config — a garbage ticket there would show up on the next app start.
    let _guard = streampipe::gui::config::CONFIG_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    {
        let mut g = app.state.lock().unwrap();
        g.broadcast = Mode::Broadcasting;
        // A realistically long ticket: the copy box must elide it.
        g.ticket = Some("b".repeat(140));
        g.stats = Some(StreamStats {
            viewers: 3,
            ..Default::default()
        });
    }
    let min = egui::vec2(560.0, 420.0);
    app.copied = Some(("ticket", Instant::now()));

    let with_toast = render_shapes(&mut app, min);
    assert!(!with_toast.is_empty(), "the broadcast tab painted nothing");

    // Nothing escapes the window.
    let window = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), min);
    for shape in &with_toast {
        assert!(
            window.expand(1.0).contains_rect(shape.clip_rect),
            "painted rect {:?} escapes the window",
            shape.clip_rect
        );
    }

    // Text *extents* (not just clip rects) stay inside the window: `clip_rect` is
    // the enclosing panel clip and would pass even if the galley overflowed.
    for shape in &with_toast {
        if let egui::epaint::Shape::Text(t) = &shape.shape {
            let right = t.pos.x + t.galley.rect.width();
            assert!(
                right <= min.x + 1.0,
                "text {:?} (x={:.0}, w={:.0}) runs past the window edge",
                t.galley.text().chars().take(12).collect::<String>(),
                t.pos.x,
                t.galley.rect.width()
            );
        }
    }

    // The toast contributes its own painting on top of the tab.
    app.copied = None;
    let without_toast = render_shapes(&mut app, min);
    assert!(
        with_toast.len() > without_toast.len(),
        "the copy toast should add painting, got {} vs {}",
        with_toast.len(),
        without_toast.len()
    );

    std::env::remove_var("XDG_CONFIG_HOME");
}

/// While a refresh is in flight and no ticket is shown yet, the idle Broadcast tab's
/// ticket box reads "issuing a fresh ticket…" — the state the automatic replacement
/// of an invalid stored ticket leaves it in — instead of prompting for a button
/// press that is already happening.
#[test]
fn the_idle_ticket_box_announces_an_in_flight_refresh() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    app.config.broadcast_ticket = None;
    {
        let mut g = app.state.lock().unwrap();
        g.refreshing = true;
        g.ticket = None;
    }
    let size = egui::vec2(760.0, 560.0);
    let out = render_shapes(&mut app, size);
    assert!(
        text_present(&out, "issuing a fresh ticket…"),
        "the box announces the in-flight replacement"
    );

    // Nothing in flight, still no ticket: the original prompt returns.
    app.state.lock().unwrap().refreshing = false;
    let out = render_shapes(&mut app, size);
    assert!(
        text_present(&out, "no ticket yet — press Refresh Relay"),
        "the idle prompt shows when nothing is in flight"
    );
}

/// Render the Watch tab at the minimum window size and assert nothing escapes. It has
/// a multiline ticket box, two combo boxes and a buffer field, so it's checked
/// separately from the broadcast tab. Also checks the "Select Contact" button is
/// painted, right-aligned, and fits inside the min window.
#[test]
fn watch_tab_renders_at_min_size() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    app.watch_ticket = "a-friend-ticket-string-that-is-long-ish".into();
    let min = egui::vec2(560.0, 420.0);
    let shapes = render_shapes(&mut app, min);
    assert!(!shapes.is_empty(), "the watch tab painted nothing");
    let window = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), min);
    for shape in &shapes {
        assert!(
            window.expand(1.0).contains_rect(shape.clip_rect),
            "painted rect {:?} escapes the window",
            shape.clip_rect
        );
    }

    // The "Select Contact" button is painted, on the heading row above the ticket box,
    // right-aligned, and 22 tall (never the 30-tall footer size).
    let btn = shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Rect(r)
                if r.fill == WATCH_ACCENT.gamma_multiply(0.18)
                    && (20.0..=24.0).contains(&r.rect.height()) =>
            {
                Some(r.rect)
            }
            _ => None,
        })
        .expect("the 'Select Contact' button should be painted at min size");
    assert!(
        window.contains_rect(btn),
        "the 'Select Contact' button escapes the min window: {btn:?}"
    );
    assert!(
        btn.right() >= min.x - 24.0,
        "the button should sit against the right edge, got {btn:?}"
    );
    assert!(
        text_present(&shapes, "Select Contact"),
        "the button's label should be painted"
    );
}

/// The Options "Streaming" inputs must leave the interactive set while a session is
/// live and return when idle. `interactive_rects_last_pass` filters out disabled
/// widgets, so this count distinguishes the two where a render check would not.
#[test]
fn options_streaming_fields_are_disabled_while_a_session_is_active() {
    let idle = options_interactive_rects(Mode::Idle);
    let broadcasting = options_interactive_rects(Mode::Broadcasting);
    assert!(
        broadcasting < idle,
        "streaming fields should be non-interactive while broadcasting \
         ({broadcasting} interactive rects vs {idle} idle)"
    );
}

/// While broadcasting, the Watch tab's primary action stays enabled, so you can watch
/// a stream in the same instance without stopping the broadcast.
#[test]
fn watch_button_stays_live_while_broadcasting() {
    assert_eq!(
        watch_button_rects_with(Mode::Broadcasting, "abc"),
        1,
        "a real ticket keeps the Watch button interactive during a broadcast"
    );
    assert_eq!(
        watch_button_rects_with(Mode::Broadcasting, "   "),
        0,
        "a blank ticket still disables it, broadcast or not"
    );
}

#[test]
fn a_blank_ticket_disables_the_watch_button() {
    assert_eq!(
        watch_button_rects("abc"),
        1,
        "a real ticket shows one interactive Watch button"
    );
    assert_eq!(
        watch_button_rects("   "),
        0,
        "a blank ticket disables the Watch button, dropping it from the set"
    );
}

/// With a watch session live, the tab shows two half-width buttons side by side
/// (`Restart` left, `■  Stop` right) filling the panel with a 12 px gap, the status
/// text on its own row underneath, and the Player combo and Buffer field live.
///
/// Driven at the real minimum window size (560×630): `watch_tab_renders_at_min_size`
/// only checks `clip_rect`, which is always in-bounds and so can't see an overflowing
/// row.
#[test]
fn the_live_watch_tab_shows_restart_and_stop_side_by_side() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    // A real ticket so the "no ticket yet?" hint row isn't painted (it would add a
    // row above the buttons and skew the vertical comparisons).
    app.watch_ticket = real_ticket();
    app.watch_buffer = "90".into();
    {
        let mut g = app.state.lock().unwrap();
        g.watch = Mode::Watching;
        g.watch_receiving = true;
        g.watch_applied = Some((Player::Mpv, Some(90)));
    }

    let ctx = egui::Context::default();
    let size = egui::vec2(560.0, 630.0);
    let out = drive_frames(&mut app, &ctx, size, 3);
    let rects = ctx.interactive_rects_last_pass();

    // Exactly two interactive rects the height of the buttons (36 tall). The band
    // excludes the tab strip (32), the labels, the combo and the buffer field.
    let mut band: Vec<egui::Rect> = rects
        .iter()
        .copied()
        .filter(|r| (34.0..=38.0).contains(&r.height()))
        .collect();
    assert_eq!(
        band.len(),
        2,
        "expected exactly two ~36-tall interactive rects (Restart + Stop), got \
         {band:#?} (all rects: {rects:#?})"
    );
    band.sort_by(|a, b| a.min.x.total_cmp(&b.min.x));
    let (restart, stop) = (band[0], band[1]);

    // Equal halves, 12 px apart, filling the panel.
    assert!(
        (restart.width() - stop.width()).abs() < 1.0,
        "the two buttons should be the same width, got {restart:?} vs {stop:?}"
    );
    assert!(
        (stop.min.x - restart.max.x - 12.0).abs() < 1.0,
        "the gap should be 12 px, got {restart:?} vs {stop:?}"
    );
    assert!(
        (restart.width() - (560.0 - 32.0 - 12.0) / 2.0).abs() < 1.0,
        "each button should be half the panel (258), got {restart:?}"
    );
    // Both fully inside the window at the minimum size.
    assert!(
        restart.min.x > 0.0,
        "Restart is off the left edge: {restart:?}"
    );
    assert!(stop.max.x < size.x, "Stop escapes the right edge: {stop:?}");
    assert!(
        stop.max.y < size.y,
        "the button row escapes the bottom: {stop:?}"
    );

    // The status text sits on its own row underneath the button row.
    let status = out
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t)
                if t.galley.text() == "Playing stream in player window" =>
            {
                Some(egui::Rect::from_min_size(t.pos, t.galley.rect.size()))
            }
            _ => None,
        })
        .expect("the live status label should be painted");
    assert!(
        status.min.y > restart.max.y,
        "the status text should be on its own row below the buttons, got {status:?} \
         vs {restart:?} / {stop:?}"
    );

    // The Player combo and the Buffer field are interactive while live, identified by
    // the labels they sit immediately right of.
    let label_rect = |want: &str| {
        out.shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.text() == want => {
                    Some(egui::Rect::from_min_size(t.pos, t.galley.rect.size()))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the {want:?} label should be painted"))
    };
    let player = label_rect("Player");
    let buffer = label_rect("Buffer (ms)");
    let probe = egui::pos2(player.max.x + 10.0, player.center().y);
    assert!(
        rects.iter().any(|r| r.contains(probe)),
        "the Player combo should be interactive while live (probing {probe:?} of \
         {player:?}; rects: {rects:?})"
    );
    let field = rects
        .iter()
        .copied()
        .filter(|r| (r.width() - 90.0).abs() < 1.0 && r.min.x > buffer.max.x)
        .max_by(|a, b| a.min.y.total_cmp(&b.min.y))
        .expect("the 90-wide buffer field should be interactive while live");
    assert!(
        field.min.y > buffer.min.y - 5.0 && field.min.y < player.max.y + 20.0,
        "the buffer field should be on the Player/Buffer row, got {field:?}"
    );
}

/// The prompt line appears iff a session is live *and* the shown player/buffer differ
/// from what the live session was launched with.
#[test]
fn the_watch_tab_prompts_to_restart_only_for_a_pending_edit() {
    const PROMPT: &str = "Press 'Restart' button to apply changes.";
    let shows_prompt =
        |watch: Mode, applied: Option<(Player, Option<u64>)>, player: Player, buffer: &str| {
            let (mut app, _rt) = test_app();
            app.tab = Tab::Watch;
            app.watch_ticket = real_ticket();
            app.watch_player = player;
            app.watch_buffer = buffer.to_string();
            {
                let mut g = app.state.lock().unwrap();
                g.watch = watch;
                g.watch_applied = applied;
            }
            let shapes = render_shapes(&mut app, egui::vec2(760.0, 560.0));
            shapes.iter().any(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => t.galley.text() == PROMPT,
                _ => false,
            })
        };

    // Idle with an edited buffer: nothing to restart into, so no prompt.
    assert!(
        !shows_prompt(Mode::Idle, None, Player::Mpv, "120"),
        "an idle tab must not prompt"
    );
    // Live and in sync: nothing pending, so no prompt.
    assert!(
        !shows_prompt(
            Mode::Watching,
            Some((Player::Mpv, Some(90))),
            Player::Mpv,
            "90"
        ),
        "a live tab with matching player/buffer must not prompt"
    );
    // Live with an edited buffer, or a switched player: prompt.
    assert!(
        shows_prompt(
            Mode::Watching,
            Some((Player::Mpv, Some(90))),
            Player::Mpv,
            "120"
        ),
        "an edited buffer should raise the prompt"
    );
    assert!(
        shows_prompt(
            Mode::Watching,
            Some((Player::Mpv, Some(90))),
            Player::Vlc,
            "90"
        ),
        "a switched player should raise the prompt"
    );
    // Live with nothing recorded yet (defensive: the invariant says this can't
    // happen, but the condition must still be total).
    assert!(
        shows_prompt(Mode::Starting, None, Player::Mpv, ""),
        "a live session with no record and a shown value should prompt"
    );
}

/// Clicking `Restart` tears the previous session down before the new one is brought
/// up: the old handle finishes, a different handle takes its place, and the record
/// moves to what's shown on screen.
#[test]
fn clicking_restart_replaces_the_live_watch_session() {
    // The click writes config; redirect the config dir (mutex serializes these tests).
    let _guard = streampipe::gui::config::CONFIG_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let (mut app, rt) = test_app();
    app.tab = Tab::Watch;
    app.watch_ticket = real_ticket();
    app.watch_buffer = "120".into();
    // `Player::None` never launches a process, so nothing spawns in CI, and the
    // shown pair is visibly stale (live = Mpv, shown = None).
    app.watch_player = Player::None;

    // A stand-in for the live session: one long sleep plus a drop-flag the abort
    // trips. `JoinHandle` isn't `Clone` and is moved into `spawn_watch`, so the
    // flag — not the handle — is what the test observes.
    let torn_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = torn_down.clone();
    let previous = rt.spawn(async move {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _flag = Flag(flag);
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    {
        let mut g = app.state.lock().unwrap();
        g.watch = Mode::Watching;
        g.watch_session = Some(previous);
        g.watch_applied = Some((Player::Mpv, None));
    }

    let ctx = egui::Context::default();
    let size = egui::vec2(560.0, 630.0);
    let out = drive_frames(&mut app, &ctx, size, 3);
    // The leftmost of the two ~36-tall rects is `Restart` (clicking Stop would abort
    // and go idle, which the assertions below would catch).
    let mut band: Vec<egui::Rect> = ctx
        .interactive_rects_last_pass()
        .into_iter()
        .filter(|r| (34.0..=38.0).contains(&r.height()))
        .collect();
    assert_eq!(band.len(), 2, "expected Restart + Stop, got {band:?}");
    band.sort_by(|a, b| a.min.x.total_cmp(&b.min.x));
    let restart = band[0];

    // The previous session must still be live right up to the click.
    assert!(
        !torn_down.load(std::sync::atomic::Ordering::SeqCst),
        "the stand-in session must be live before the click"
    );

    click_at(&mut app, &ctx, restart.center(), size);

    // Asserted on the release frame: `start_watch` runs synchronously inside it.
    let g = app.state.lock().unwrap();
    assert_eq!(
        g.watch_applied,
        Some((Player::None, Some(120))),
        "the record moves to the player/buffer shown on screen"
    );
    let session = g
        .watch_session
        .as_ref()
        .expect("a session handle replaces the old one");
    assert!(
        !session.is_finished(),
        "the new session has just been spawned and must still be running"
    );
    drop(g);

    // The previous session must be torn down — at most one watch session is live at
    // any moment. Bounded polling only.
    let mut polls = 0;
    while !torn_down.load(std::sync::atomic::Ordering::SeqCst) && polls < 200 {
        std::thread::sleep(Duration::from_millis(10));
        polls += 1;
    }
    assert!(
        torn_down.load(std::sync::atomic::Ordering::SeqCst),
        "the previous session was never torn down ({} polls)",
        polls
    );
    assert!(
        !out.shapes.is_empty(),
        "the Watch tab painted nothing before the click"
    );

    std::env::remove_var("XDG_CONFIG_HOME");
}

/// The status bar shows the Ctrl+L / Ctrl+, / Ctrl+T shortcuts so they're discoverable.
#[test]
fn the_status_bar_shows_a_keyboard_shortcut_hint() {
    let (mut app, _rt) = test_app();
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let hint = "Ctrl+L log · Ctrl+, settings · Ctrl+T contacts";
    assert!(
        shapes.iter().any(|s| matches!(&s.shape,
            egui::epaint::Shape::Text(t) if t.galley.text() == hint
        )),
        "the status bar should show a keyboard shortcut hint"
    );
}

/// Settings footer: "Save" (accent) and "Close" (grey) are matched 90x30 boxes.
/// Save writes the config and raises the "Saved" pill; Close dismisses the panel.
#[test]
fn the_settings_footer_is_a_matched_pair_and_save_raises_a_toast() {
    // The click writes config; redirect the config dir (mutex serializes these tests).
    let _guard = streampipe::gui::config::CONFIG_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let (mut app, _rt) = test_app();
    app.show_options = true;
    let ctx = egui::Context::default();
    let size = egui::vec2(760.0, 560.0);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    // Two settling passes so the footer reaches its final size.
    let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
    out.textures_delta.clear();
    let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
    out.textures_delta.clear();

    let rect_of = |fill: egui::Color32| {
        out.shapes.iter().find_map(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) if r.fill == fill && r.rect.height() == 30.0 => {
                Some(r.rect)
            }
            _ => None,
        })
    };
    let save = rect_of(ACCENT.gamma_multiply(0.18)).expect("Save paints its resting fill");
    let close =
        rect_of(CLOSE_GREY.gamma_multiply(0.12)).expect("Close paints its resting grey fill");

    // Same box, side by side.
    assert_eq!(save.size(), close.size(), "Save and Close are the same box");
    assert_eq!((save.width(), save.height()), (90.0, 30.0));
    assert!(close.min.x > save.max.x, "Close sits to the right of Save");

    // Save: config hits disk, confirmation pill raised.
    click_at(&mut app, &ctx, save.center(), size);
    assert_eq!(
        app.toast.as_ref().map(|(msg, _)| msg.as_str()),
        Some("Saved ✓"),
        "Save raises the confirmation toast"
    );
    assert!(
        dir.path().join("streampipe").join("config.json").exists(),
        "Save wrote the redirected config"
    );
    assert!(app.show_options, "Save leaves the panel open");

    // Close: the panel goes away (the toast has expired by then).
    click_at(&mut app, &ctx, close.center(), size);
    assert!(!app.show_options, "Close dismisses the panel");

    std::env::remove_var("XDG_CONFIG_HOME");
}
#[test]
fn the_watch_tab_chips_the_selected_contact_and_recedes_on_edit() {
    let ticket = real_ticket();
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: ticket.clone(),
        }],
        ..Config::default()
    });
    app.tab = Tab::Watch;
    let size = egui::vec2(760.0, 560.0);

    // Empty box: no chip.
    let out = render_shapes(&mut app, size);
    assert!(
        !text_present(&out, "Alice"),
        "an empty box claims no contact"
    );

    // Ticket filled from the contact: the chip shows the name, on the heading row
    // (above the ticket box), just left of the "Select Contact" button.
    app.watch_ticket = ticket.clone();
    let out = render_shapes(&mut app, size);
    let chip = text_rect(&out, "Alice").expect("the chip shows the contact's name");
    let btn = text_rect(&out, "Select Contact").expect("the button is painted");
    assert!(
        chip.min.y < 131.0,
        "the chip rides the 'Stream ticket' heading row, got {chip:?}"
    );
    assert!(
        chip.right() < btn.left(),
        "the chip sits just left of the button, got chip {chip:?} btn {btn:?}"
    );

    // Ticket edited so nothing matches: the chip recedes.
    app.watch_ticket = "tampered-with".into();
    let out = render_shapes(&mut app, size);
    assert!(
        !text_present(&out, "Alice"),
        "the chip recedes when the ticket no longer matches a contact"
    );
}
