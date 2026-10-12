//! Repaint cadence: an idle window must fall asleep, live things must not.

mod common;

use std::time::{Duration, Instant};

use common::{assert_awake, settled_repaint_delay, test_app};
use streampipe::gui::{state::Mode, TOAST, TOAST_FRAME};

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
