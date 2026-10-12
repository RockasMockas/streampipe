//! The Watch tab's pure gates: the status label, the start button, the Enter key.
//!
//! These are extracted from the tab so the label/keyboard logic is testable without
//! egui focus plumbing.

use streampipe::gui::state::Mode;
use streampipe::gui::watch::{watch_action_label, watch_can_start, watch_enter_starts};

#[test]
fn watch_action_label_distinguishes_connecting_waiting_from_playing() {
    assert_eq!(watch_action_label(Mode::Starting, false), "connecting…");
    // Connecting, with a gate signal in flight: still "connecting…" — the session
    // is not up yet, so the live labels must not show.
    assert_eq!(watch_action_label(Mode::Starting, true), "connecting…");
    assert_eq!(
        watch_action_label(Mode::Watching, true),
        "Playing stream in player window"
    );
    // Regression: `Watching` used to claim it was playing even while the broadcaster
    // had sent nothing, so the tab lied about a blank window.
    assert_eq!(
        watch_action_label(Mode::Watching, false),
        "Waiting for streamer to begin broadcasting..."
    );
}

#[test]
fn watch_can_start_needs_idle_and_a_non_blank_ticket() {
    assert!(!watch_can_start(false, " "));
    assert!(!watch_can_start(false, ""));
    assert!(watch_can_start(false, "abc"));
    assert!(!watch_can_start(true, "abc"));
}

#[test]
fn watch_enter_starts_needs_focus_enter_and_a_startable_ticket() {
    // Enter starts only when the box owns focus, Enter is pressed, and the button's
    // own idle/non-blank gate holds.
    assert!(watch_enter_starts(true, true, false, "abc"));
    assert!(!watch_enter_starts(false, true, false, "abc")); // not focused
    assert!(!watch_enter_starts(true, false, false, "abc")); // no Enter
    assert!(!watch_enter_starts(true, true, false, "  ")); // blank ticket
    assert!(!watch_enter_starts(true, true, false, ""));
    assert!(!watch_enter_starts(true, true, true, "abc")); // busy
}
