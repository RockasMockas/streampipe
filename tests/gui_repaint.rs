//! Repaint cadence: an idle window must fall asleep, live things must not.

mod common;

use std::time::{Duration, Instant};

use common::{assert_awake, drive_until_settled, settled_repaint_delay, test_app, test_app_with};
use eframe::egui;
use streampipe::gui::{
    config::{Config, Contact},
    state::Mode,
    TOAST, TOAST_FRAME,
};

/// The size these tests drive the window at (matches `settled_repaint_delay`).
const SIZE: egui::Vec2 = egui::vec2(760.0, 560.0);

#[test]
fn idle_window_falls_asleep_once_the_copy_toast_expires() {
    let (mut app, _rt) = test_app();

    // Idle, nothing copied: no repaint requested at all.
    assert_eq!(settled_repaint_delay(&mut app), Duration::MAX);

    // A click arms the toast, so frames keep coming to fade it.
    app.copied = Some(("whip", Instant::now()));
    assert_awake(settled_repaint_delay(&mut app));

    // The toast window has closed: the idle app must stop asking for frames.
    // Regression — before the fix `copied` stayed set forever.
    app.copied = Some(("whip", Instant::now() - TOAST - Duration::from_millis(1)));
    assert_eq!(
        settled_repaint_delay(&mut app),
        Duration::MAX,
        "an expired toast must not keep an idle window repainting"
    );
    assert_eq!(app.copied, None, "the expired toast flag is cleared");
}

/// A live session always repaints, toast or not.
#[test]
fn an_active_session_always_repaints() {
    let (mut app, _rt) = test_app();
    app.state.lock().unwrap().broadcast = Mode::Broadcasting;
    assert_awake(settled_repaint_delay(&mut app));
}

/// A fading toast is an animation, so it must repaint at display rate — the 250 ms
/// session cadence gives a 1.5 s dissolve only six frames.
#[test]
fn a_fading_toast_repaints_at_display_rate() {
    // The action toast (raised by "Save", a relay refresh, a ticket reset).
    let (mut app, _rt) = test_app();
    app.toast = Some(("Saved".to_string(), Instant::now()));
    let delay = settled_repaint_delay(&mut app);
    assert_awake(delay);
    assert!(
        delay <= TOAST_FRAME,
        "a live toast should repaint at ~60 fps, not on the 250 ms session cadence, \
         got {delay:?}"
    );

    // The copy toast, same cadence.
    let (mut app, _rt) = test_app();
    app.copied = Some(("whip", Instant::now()));
    assert!(
        settled_repaint_delay(&mut app) <= TOAST_FRAME,
        "a live copy toast should repaint at ~60 fps too"
    );

    // Nothing live: the guard must stay disarmed, or an idle window burns frames.
    let (mut app, _rt) = test_app();
    assert_eq!(settled_repaint_delay(&mut app), Duration::MAX);
}

/// The contact-row edit tween repaints at display rate while in flight (open and
/// close) and sleeps exactly once settled — and an open panel with nothing editing
/// sleeps too. The edit state is armed directly (the fields are `pub`) with no
/// focus anywhere, so no TextEdit cursor blink can keep the window awake.
#[test]
fn the_contact_edit_tween_sleeps_when_settled() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: "alice-ticket".into(),
        }],
        ..Config::default()
    });

    // Panel open, nothing editing: the window sleeps.
    app.show_contacts = true;
    assert_eq!(
        settled_repaint_delay(&mut app),
        Duration::MAX,
        "an open panel with nothing editing must not repaint"
    );

    // Arm edit mode: the tween is in flight, so frames must keep coming.
    app.editing_contact = Some(0);
    let delay = settled_repaint_delay(&mut app);
    assert_awake(delay);

    // Poll to settle (fixed frame counts are unreliable headless): sleeps again.
    let ctx = egui::Context::default();
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    assert_eq!(
        settled_repaint_delay(&mut app),
        Duration::MAX,
        "the settled tween must not keep the window awake"
    );

    // Exit edit mode: the row animates closed (awake), then sleeps, and the
    // animating-out row (`contact_edit_prev`) is released on settle — never pinned
    // expanded.
    app.editing_contact = None;
    app.contact_edit_prev = Some(0);
    app.contact_edit_anim = 0.0;
    assert_awake(settled_repaint_delay(&mut app));
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    assert_eq!(
        settled_repaint_delay(&mut app),
        Duration::MAX,
        "the closed row must not keep repainting once settled"
    );
    assert_eq!(app.contact_edit_prev, None, "prev is released on settle");
}

/// A loaded ("active") contact row breathes its left bar, so an open panel with such a
/// row repaints on the session cadence; the same panel with nothing loaded still sleeps.
/// Regression guard for the live-pulse repaint gate — it must be gated on an active row,
/// not merely on the panel being open, or an idle open panel would burn frames.
#[test]
fn an_open_panel_wakes_only_for_a_loaded_row() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: "alice-ticket".into(),
        }],
        ..Config::default()
    });
    app.show_contacts = true;

    // Panel open, nothing loaded into the Watch box → no active row → sleeps.
    assert_eq!(
        settled_repaint_delay(&mut app),
        Duration::MAX,
        "an open panel with no loaded contact must not repaint"
    );

    // Load the contact → its card's left bar breathes → the panel repaints.
    app.watch_ticket = "alice-ticket".into();
    assert_awake(settled_repaint_delay(&mut app));
}
