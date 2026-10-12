//! The animation tweens: monotonic, settling exactly on their target, so the
//! repaint guard can always disarm and an idle window falls asleep.

mod common;

use std::time::{Duration, Instant};

use streampipe::gui::{
    expire_copied, flash_alpha, log_anim_step, pulse_factor, stat_ease, tab_fade_step,
    tab_slide_step, truncate, FLASH, LOG_FADE_SECS, TAB_FADE_SECS,
};

#[test]
fn expire_copied_leaves_a_live_toast_alone() {
    let at = Instant::now();
    assert!(expire_copied(Some(("ticket", at)), Duration::from_secs(10)).is_some());
    assert!(expire_copied(Some(("ticket", at)), Duration::ZERO).is_none());
    assert!(expire_copied(None, Duration::from_secs(10)).is_none());
}

#[test]
fn truncate_stays_within_budget() {
    assert_eq!(truncate("hello", 10), "hello");
    assert_eq!(truncate("hello world", 5), "hell…");
    assert_eq!(truncate("hello world", 5).chars().count(), 5);
}

#[test]
fn pulse_factor_peaks_and_troughs_within_bounds() {
    assert!((pulse_factor(0.0) - 1.0).abs() < 1e-6, "peak at t=0");
    assert!((pulse_factor(1.0) - 0.6).abs() < 1e-6, "trough at t=1");
    let mut secs = 0.0;
    while secs <= 4.0 {
        let f = pulse_factor(secs);
        assert!(
            (0.6..=1.0).contains(&f),
            "pulse_factor({secs}) = {f} outside 0.6..=1.0"
        );
        secs += 0.1;
    }
}

#[test]
fn flash_alpha_fades_linearly() {
    assert_eq!(flash_alpha(Duration::ZERO, FLASH), 1.0);
    assert_eq!(flash_alpha(FLASH, FLASH), 0.0);
    assert_eq!(
        flash_alpha(FLASH * 2, FLASH),
        0.0,
        "clamped past the window"
    );
    let half = flash_alpha(FLASH / 2, FLASH);
    assert!(half > 0.0 && half < 1.0, "mid-window is between 0 and 1");
    assert!(
        flash_alpha(Duration::from_millis(100), FLASH)
            > flash_alpha(Duration::from_millis(200), FLASH),
        "monotonically decreasing"
    );
}

#[test]
fn tab_slide_settles_on_the_target_without_overshooting() {
    // One frame moves partway, never past.
    let step = tab_slide_step(0.0, 1.0, 1.0 / 60.0);
    assert!(step > 0.0 && step < 1.0, "one frame is partway, got {step}");

    let mut pos = 0.0;
    let mut frames = 0;
    while pos != 1.0 {
        pos = tab_slide_step(pos, 1.0, 1.0 / 60.0);
        frames += 1;
        assert!(pos <= 1.0, "no overshoot at frame {frames}, got {pos}");
        assert!(frames < 60, "must settle within 1 s, took {frames} frames");
    }
    assert!(
        frames < 45,
        "settling took {frames} frames (~{} ms at 60 fps)",
        frames * 1000 / 60
    );

    // Settled stays settled; reverse settles the same way.
    assert_eq!(tab_slide_step(1.0, 1.0, 1.0 / 60.0), 1.0);
    let mut pos = 1.0;
    while pos != 0.0 {
        pos = tab_slide_step(pos, 0.0, 1.0 / 60.0);
    }
    assert_eq!(pos, 0.0);
}

#[test]
fn log_anim_settles_on_the_target_without_overshooting() {
    // One frame moves partway, never past.
    let step = log_anim_step(0.0, 1.0, 1.0 / 60.0);
    assert!(step > 0.0 && step < 1.0, "one frame is partway, got {step}");

    let mut pos = 0.0;
    let mut frames = 0;
    while pos != 1.0 {
        pos = log_anim_step(pos, 1.0, 1.0 / 60.0);
        frames += 1;
        assert!(pos <= 1.0, "no overshoot at frame {frames}, got {pos}");
        assert!(frames < 60, "must settle within 1 s, took {frames} frames");
    }
    assert!(
        frames < 45,
        "settling took {frames} frames (~{} ms at 60 fps)",
        frames * 1000 / 60
    );

    // Settled stays settled; reverse settles the same way.
    assert_eq!(log_anim_step(1.0, 1.0, 1.0 / 60.0), 1.0);
    let mut pos = 1.0;
    while pos != 0.0 {
        pos = log_anim_step(pos, 0.0, 1.0 / 60.0);
    }
    assert_eq!(pos, 0.0);
}

#[test]
fn stat_ease_settles_on_the_target_without_overshooting() {
    // One frame moves partway, never past.
    let step = stat_ease(0.0, 5000.0, 1.0 / 60.0);
    assert!(
        step > 0.0 && step < 5000.0,
        "one frame is partway, got {step}"
    );

    let mut pos = 0.0;
    let mut frames = 0;
    while pos != 5000.0 {
        pos = stat_ease(pos, 5000.0, 1.0 / 60.0);
        frames += 1;
        assert!(pos <= 5000.0, "no overshoot at frame {frames}, got {pos}");
        assert!(frames < 120, "must settle within 2 s, took {frames}");
    }

    // Settled stays settled; reverse settles without undershooting.
    assert_eq!(stat_ease(5000.0, 5000.0, 1.0 / 60.0), 5000.0);
    let mut pos = 5000.0;
    while pos != 0.0 {
        pos = stat_ease(pos, 0.0, 1.0 / 60.0);
        assert!(pos >= 0.0, "no undershoot, got {pos}");
    }
    assert_eq!(pos, 0.0);
}

/// The tab content's fade is monotonic and settles exactly on visible at the
/// duration, so the repaint guard can disarm.
#[test]
fn tab_fade_settles_on_fully_visible_without_overshooting() {
    assert_eq!(tab_fade_step(0.0, TAB_FADE_SECS), (0.0, false));

    let (mid, settled) = tab_fade_step(TAB_FADE_SECS * 0.5, TAB_FADE_SECS);
    assert!((0.0..1.0).contains(&mid), "mid-fade is partway, got {mid}");
    assert!(!settled, "mid-fade is not settled");

    assert_eq!(tab_fade_step(TAB_FADE_SECS, TAB_FADE_SECS), (1.0, true));
    assert_eq!(
        tab_fade_step(TAB_FADE_SECS * 5.0, TAB_FADE_SECS),
        (1.0, true)
    );

    let mut prev = 0.0;
    let mut t = 0.0;
    while t <= TAB_FADE_SECS {
        let (alpha, _) = tab_fade_step(t, TAB_FADE_SECS);
        assert!(alpha >= prev, "alpha must be monotonic at t={t}");
        prev = alpha;
        t += 0.01;
    }
}

/// The log fade behaves like the tab fade.
#[test]
fn log_fade_settles_on_fully_visible() {
    assert_eq!(tab_fade_step(0.0, LOG_FADE_SECS), (0.0, false));
    assert_eq!(tab_fade_step(LOG_FADE_SECS, LOG_FADE_SECS), (1.0, true));
    assert_eq!(
        tab_fade_step(LOG_FADE_SECS * 5.0, LOG_FADE_SECS),
        (1.0, true)
    );
}
