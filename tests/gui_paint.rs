//! Paint probes: render the UI headlessly and assert on what was painted.
//!
//! Each probe identifies a specific visual by a property unique to it (fill colour,
//! mesh size, stroke width, galley text) and notes what it must not be confounded
//! with.

mod common;

use std::time::{Duration, Instant};

use eframe::egui;
use streampipe::{
    gui::{
        error_callout, log, log_line_color, section_tick, soft_glow, sonar_pulse, spinner,
        state::Mode, Tab, ACCENT, ERROR, WARN_AMBER, WATCH_ACCENT,
    },
    webrtc::StreamStats,
};

use common::{live_dot_present, render_shapes, test_app};

/// The central panel's ground is a gradient mesh (4 vertices, 6 indices — unique,
/// as the sparkline and spinner meshes have different counts).
#[test]
fn the_central_panel_paints_a_subtle_vertical_gradient() {
    let (mut app, _rt) = test_app();
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let gradient = shapes.iter().any(|s| match &s.shape {
        egui::epaint::Shape::Mesh(m) => m.vertices.len() == 4 && m.indices.len() == 6,
        _ => false,
    });
    assert!(
        gradient,
        "the central panel should paint its subtle vertical gradient"
    );
}

/// The central panel paints a radial vignette behind its content (32-segment fan:
/// 33 vertices, 96 indices — unique among the panel's meshes).
#[test]
fn the_central_panel_paints_a_subtle_vignette() {
    let (mut app, _rt) = test_app();
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let vignette = shapes.iter().any(|s| match &s.shape {
        egui::epaint::Shape::Mesh(m) => m.vertices.len() == 33 && m.indices.len() == 96,
        _ => false,
    });
    assert!(
        vignette,
        "the central panel should paint its subtle radial vignette"
    );
}

/// The central panel paints a "light from above" glow at the top centre: six
/// concentric low-alpha circles.
#[test]
fn the_central_panel_paints_an_ambient_light_glow() {
    let (mut app, _rt) = test_app();
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let glow_color = egui::Color32::from_rgba_unmultiplied(58, 64, 74, 3);
    let count = shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Circle(c) => c.fill == glow_color && c.stroke.width == 0.0,
            _ => false,
        })
        .count();
    assert_eq!(
        count, 6,
        "the central panel should paint its 6-circle ambient glow"
    );
}

/// Regression: the gradient mesh must be painted *before* the panel content.
/// egui 0.36 doesn't guarantee a separate background layer's order, so painting
/// it on top would cover the panel and leave a blank window.
#[test]
fn the_central_panel_gradient_paints_behind_its_content() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let mut gradient_idx = None;
    let mut content_idx = None;
    for (i, s) in shapes.iter().enumerate() {
        match &s.shape {
            egui::epaint::Shape::Mesh(m) if m.vertices.len() == 4 && m.indices.len() == 6 => {
                gradient_idx = Some(i);
            }
            // Central panel content lives in the upper area (the status bar is at
            // the bottom); the gradient must precede the first such shape.
            egui::epaint::Shape::Text(t) if t.pos.y < 500.0 && content_idx.is_none() => {
                content_idx = Some(i);
            }
            _ => {}
        }
    }
    let (g, c) = match (gradient_idx, content_idx) {
        (Some(g), Some(c)) => (g, c),
        _ => panic!(
            "expected both the central panel gradient (4-vertex mesh) and its text \
             content to be painted"
        ),
    };
    assert!(
        g < c,
        "the central panel gradient (index {g}) must be painted before its content \
         (index {c}) so it sits behind it, not on top of it"
    );
}

/// A copy box paints a faint horizontal light-catch bevel (white alpha 24) on its
/// top edge — distinct from the status bar hairline (alpha 16).
#[test]
fn the_copy_box_paints_a_top_light_catch_bevel() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let bevel = shapes.iter().any(|s| match &s.shape {
        egui::epaint::Shape::LineSegment { points, stroke } => {
            (points[0].y - points[1].y).abs() < 0.5
                && stroke.color == egui::Color32::from_white_alpha(24)
        }
        _ => false,
    });
    assert!(bevel, "the copy box should paint its top light-catch bevel");
}

/// The bottom status bar paints a distinct, slightly darker fill so it reads as
/// a deliberate bar rather than a strip of the content background.
#[test]
fn the_status_bar_uses_a_distinct_darker_fill() {
    let (mut app, _rt) = test_app();
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let bar_fill = egui::Color32::from_rgb(19, 21, 25);
    assert!(
        shapes.iter().any(|s| matches!(&s.shape,
            egui::epaint::Shape::Rect(r) if r.fill == bar_fill
        )),
        "the status bar should paint its distinct darker fill"
    );
}

/// The status bar paints a faint horizontal hairline at its top edge (the
/// boundary with the content above), so the bar is crisply separated.
#[test]
fn the_status_bar_paints_a_top_hairline() {
    let (mut app, _rt) = test_app();
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    // The hairline is a horizontal line segment near the bottom of the window (the
    // top of the status bar). The tab bar's separator is also horizontal but sits
    // near the top, so a y filter isolates the hairline.
    let hairline = shapes.iter().find_map(|s| match &s.shape {
        egui::epaint::Shape::LineSegment { points, .. } => {
            if (points[0].y - points[1].y).abs() < 0.5
                && points[0].y > size.y - 40.0
                && points[1].x - points[0].x > 100.0
            {
                Some(())
            } else {
                None
            }
        }
        _ => None,
    });
    assert!(
        hairline.is_some(),
        "the status bar should paint a top hairline"
    );
}

/// The status bar's fill tints with the live session's accent (green broadcasting,
/// blue watching); idle keeps the plain dark ground.
#[test]
fn the_status_bar_tints_with_the_live_session() {
    let size = egui::vec2(760.0, 560.0);
    // The bar's fill: the only opaque, dark, full-width rect at the bottom
    // (the Stop/Watch button's fill is low-alpha, so it can't match).
    let bar_fill = |shapes: &[egui::epaint::ClippedShape]| -> Option<egui::Color32> {
        shapes.iter().find_map(|s| match &s.shape {
            egui::epaint::Shape::Rect(r)
                if r.rect.width() > 400.0
                    && r.rect.min.y > size.y - 50.0
                    && r.fill.a() == 255
                    && r.fill.r() < 50 =>
            {
                Some(r.fill)
            }
            _ => None,
        })
    };
    let idle_fill = egui::Color32::from_rgb(19, 21, 25);

    // Idle: the plain ground, unchanged.
    let (mut app, _rt) = test_app();
    assert_eq!(
        bar_fill(&render_shapes(&mut app, size)),
        Some(idle_fill),
        "an idle status bar keeps the plain dark ground"
    );

    // Broadcasting: a green cast.
    let (mut app, _rt) = test_app();
    app.state.lock().unwrap().broadcast = Mode::Broadcasting;
    let f = bar_fill(&render_shapes(&mut app, size)).expect("the bar paints its fill");
    assert_ne!(f, idle_fill, "a live session tints the bar");
    assert!(
        f.g() > idle_fill.g(),
        "the broadcast tint is greener, got {f:?}"
    );

    // Watching: a blue cast.
    let (mut app, _rt) = test_app();
    app.state.lock().unwrap().watch = Mode::Watching;
    let f = bar_fill(&render_shapes(&mut app, size)).expect("the bar paints its fill");
    assert_ne!(f, idle_fill, "a live session tints the bar");
    assert!(f.b() > idle_fill.b(), "the watch tint is bluer, got {f:?}");
}

/// The status bar carries a "LIVE" badge while a session is live; none when idle.
#[test]
fn the_status_bar_paints_a_live_badge_when_live() {
    let size = egui::vec2(760.0, 560.0);
    let has_live_badge = |shapes: &[egui::epaint::ClippedShape]| -> bool {
        shapes
            .iter()
            .any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == "LIVE"))
    };

    // Live (broadcasting): the badge is painted.
    let (mut app, _rt) = test_app();
    app.state.lock().unwrap().broadcast = Mode::Broadcasting;
    assert!(
        has_live_badge(&render_shapes(&mut app, size)),
        "a live broadcast should paint a LIVE badge in the status bar"
    );

    // Live (watching): the badge is painted too.
    let (mut app, _rt) = test_app();
    app.state.lock().unwrap().watch = Mode::Watching;
    assert!(
        has_live_badge(&render_shapes(&mut app, size)),
        "a live watch should paint a LIVE badge in the status bar"
    );

    // Idle: no badge.
    let (mut app, _rt) = test_app();
    assert!(
        !has_live_badge(&render_shapes(&mut app, size)),
        "an idle status bar should not paint a LIVE badge"
    );
}

/// The brand title is two-tone: "Stream" near-white, "pipe" accent green, laid
/// out as two separate galleys.
#[test]
fn the_brand_title_is_two_tone() {
    let (mut app, _rt) = test_app();
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let pipe_green = egui::Color32::from_rgb(110, 205, 150);
    let stream_white = egui::Color32::from_rgb(242, 242, 246);
    // Whether a shape is a text galley holding `text`.
    let is_text = |s: &egui::epaint::ClippedShape, text: &str| -> bool {
        matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == text)
    };
    // A galley's laid-out colour is its first section's format colour.
    let color_of = |s: &egui::epaint::ClippedShape| -> Option<egui::Color32> {
        match &s.shape {
            egui::epaint::Shape::Text(t) => {
                t.galley.job.sections.first().map(|sec| sec.format.color)
            }
            _ => None,
        }
    };
    // The title is split into two words.
    assert!(
        shapes.iter().any(|s| is_text(s, "Stream")),
        "the title should paint a 'Stream' galley"
    );
    assert!(
        shapes.iter().any(|s| is_text(s, "pipe")),
        "the title should paint a 'pipe' galley"
    );
    // Each word is painted in its own colour (shadows are a dark low-alpha tint).
    assert!(
        shapes
            .iter()
            .any(|s| is_text(s, "pipe") && color_of(s) == Some(pipe_green)),
        "the 'pipe' word should be the accent green"
    );
    assert!(
        shapes
            .iter()
            .any(|s| is_text(s, "Stream") && color_of(s) == Some(stream_white)),
        "the 'Stream' word should be near-white"
    );
}

/// `section_tick` paints a small accent bar beside its bold label.
#[test]
fn section_tick_paints_a_bar_and_a_label() {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(300.0, 60.0),
        )),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        section_tick(ui, "Statistics", ACCENT);
    });
    out.textures_delta.clear();
    // The accent bar: a small filled rect in the full accent colour.
    let bar = out.shapes.iter().any(|s| {
        matches!(&s.shape,
            egui::epaint::Shape::Rect(r)
                if r.fill == ACCENT && r.rect.width() < 6.0 && r.rect.height() < 16.0
        )
    });
    assert!(bar, "section_tick should paint its accent bar");
    // The label: a text galley holding the section text.
    let label = out.shapes.iter().any(
        |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == "Statistics"),
    );
    assert!(label, "section_tick should paint its label");
}

/// A live track carries a small filled "live" dot on its tab; idle paints none.
#[test]
fn the_active_tab_carries_a_live_dot_when_live() {
    // A live broadcast paints a dot on the Broadcast tab.
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    app.state.lock().unwrap().broadcast = Mode::Broadcasting;
    let size = egui::vec2(760.0, 560.0);
    assert!(
        live_dot_present(&render_shapes(&mut app, size)),
        "a live broadcast should paint a live dot on its tab"
    );

    // A live watch paints a dot on the Watch tab.
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    app.state.lock().unwrap().watch = Mode::Watching;
    assert!(
        live_dot_present(&render_shapes(&mut app, size)),
        "a live watch should paint a live dot on its tab"
    );

    // Idle: no live dot anywhere on the strip.
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    assert!(
        !live_dot_present(&render_shapes(&mut app, size)),
        "an idle tab should not paint a live dot"
    );
}

/// The idle Start button paints a 3-layer accent halo (0.02 / 0.05 / 0.09),
/// distinct from the button's own 0.18 fill.
#[test]
fn the_idle_start_button_paints_a_soft_accent_halo() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let halo_alphas = [0.02, 0.05, 0.09];
    let count = shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) => halo_alphas
                .iter()
                .any(|&a| r.fill == ACCENT.gamma_multiply(a)),
            _ => false,
        })
        .count();
    assert_eq!(
        count, 3,
        "the idle start button should paint a 3-layer accent halo"
    );
}

/// While broadcasting, the Stop button paints a 3-layer red halo, so the one
/// control that tears down a live stream reads as "stop" at a glance.
#[test]
fn the_stop_button_paints_a_subtle_red_halo() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    app.state.lock().unwrap().broadcast = Mode::Broadcasting;
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    let red = egui::Color32::from_rgb(220, 90, 90);
    let halo_alphas = [0.02, 0.05, 0.09];
    let count = shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) => {
                halo_alphas.iter().any(|&a| r.fill == red.gamma_multiply(a))
            }
            _ => false,
        })
        .count();
    assert_eq!(count, 3, "the stop button should paint a 3-layer red halo");
}

/// The Settings "Save" button paints the same 3-layer accent halo as the hero buttons.
#[test]
fn the_settings_save_button_paints_a_soft_accent_halo() {
    let (mut app, _rt) = test_app();
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(760.0, 560.0),
        )),
        ..Default::default()
    };
    // Settle the window to its final size.
    for _ in 0..3 {
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            app.options_window(ui.ctx());
        });
        out.textures_delta.clear();
    }
    let mut out = ctx.run_ui(raw, |ui| {
        app.options_window(ui.ctx());
    });
    out.textures_delta.clear();
    let halo_alphas = [0.02, 0.05, 0.09];
    let count = out
        .shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) => halo_alphas
                .iter()
                .any(|&a| r.fill == ACCENT.gamma_multiply(a)),
            _ => false,
        })
        .count();
    assert_eq!(
        count, 3,
        "the Save button should paint a 3-layer accent halo"
    );
}

/// The error callout paints a red-tinted surface, a red left bar, and its
/// message in the error red.
#[test]
fn the_error_callout_paints_a_tinted_surface_with_a_red_bar() {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(300.0, 60.0),
        )),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        error_callout(ui, "invalid ticket: bad base64");
    });
    out.textures_delta.clear();
    // The tinted surface: a filled rect in the muted red tint.
    let tint = ERROR.gamma_multiply(0.12);
    assert!(
        out.shapes
            .iter()
            .any(|s| matches!(&s.shape, egui::epaint::Shape::Rect(r) if r.fill == tint)),
        "the callout should paint its muted red-tinted surface"
    );
    // The left bar: a small filled rect in the full error red.
    let bar = out.shapes.iter().any(|s| {
        matches!(&s.shape,
            egui::epaint::Shape::Rect(r)
                if r.fill == ERROR && r.rect.width() < 6.0 && r.rect.height() < 16.0
        )
    });
    assert!(bar, "the callout should paint its red left bar");
    // The message, laid out in the error red.
    let msg = out
        .shapes
        .iter()
        .find(|s| {
            matches!(&s.shape,
                egui::epaint::Shape::Text(t) if t.galley.text() == "invalid ticket: bad base64"
            )
        })
        .expect("the callout should paint its message");
    let color = match &msg.shape {
        egui::epaint::Shape::Text(t) => t.galley.job.sections.first().map(|sec| sec.format.color),
        _ => None,
    };
    assert_eq!(color, Some(ERROR), "the message should be the error red");
}

/// Log lines are coloured by their tracing level prefix; unprefixed lines and
/// info keep the default text colour.
#[test]
fn log_line_color_maps_levels_to_their_colours() {
    let default = egui::Color32::from_rgb(200, 200, 200);
    let amber = egui::Color32::from_rgb(214, 178, 94);
    let dim = egui::Color32::from_rgb(122, 126, 132);
    assert_eq!(
        log_line_color("ERROR streampipe::whip: bind failed", default),
        ERROR
    );
    assert_eq!(
        log_line_color("WARN streampipe::udp: dropping datagram", default),
        amber
    );
    assert_eq!(
        log_line_color("DEBUG streampipe::rtp: reordering", default),
        dim
    );
    assert_eq!(
        log_line_color("TRACE streampipe::rtp: packet", default),
        dim
    );
    assert_eq!(
        log_line_color("INFO streampipe::state: endpoint online", default),
        default
    );
    assert_eq!(
        log_line_color("a manual line without a level", default),
        default
    );
    assert_eq!(log_line_color("", default), default);
}

/// The log panel leads error lines with a red "ERR" chip and warnings with an
/// amber "WARN" chip; info gets none. Lines are backdated so chip colours are
/// deterministic.
#[test]
fn the_log_panel_marks_errors_and_warnings_with_chips() {
    let (mut app, _rt) = test_app();
    app.log_open = true;
    app.log_anim = 1.0;
    log::push(&app.log, "ERROR streampipe::whip: bind failed");
    log::push(&app.log, "WARN streampipe::udp: dropping datagram");
    log::push(&app.log, "INFO streampipe::state: endpoint online");
    // Backdate every line past the fade window so chip colours are exact.
    {
        let mut ring = app.log.lock().unwrap();
        for entry in ring.iter_mut() {
            entry.1 = Instant::now() - Duration::from_secs(5);
        }
    }
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    // A chip's laid-out colour is its galley's first section's format colour.
    let chip_color = |text: &str| -> Option<egui::Color32> {
        shapes.iter().find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.text() == text => {
                t.galley.job.sections.first().map(|sec| sec.format.color)
            }
            _ => None,
        })
    };
    let err_expected = egui::Color32::from_rgba_unmultiplied(ERROR.r(), ERROR.g(), ERROR.b(), 230);
    let warn_expected =
        egui::Color32::from_rgba_unmultiplied(WARN_AMBER.r(), WARN_AMBER.g(), WARN_AMBER.b(), 230);
    assert_eq!(
        chip_color("ERR"),
        Some(err_expected),
        "an error line should carry a red ERR chip"
    );
    assert_eq!(
        chip_color("WARN"),
        Some(warn_expected),
        "a warning line should carry an amber WARN chip"
    );
    assert_eq!(
        chip_color("INFO"),
        None,
        "an info line should carry no chip"
    );
}

/// New log lines fade in; lines older than the fade window are fully opaque.
#[test]
fn a_new_log_line_fades_in_and_an_old_one_is_opaque() {
    let (mut app, _rt) = test_app();
    app.log_open = true;
    app.log_anim = 1.0;
    // First line backdated (opaque), second fresh (still fading in).
    log::push(&app.log, "old line");
    log::push(&app.log, "fresh line");
    {
        let mut ring = app.log.lock().unwrap();
        if let Some(entry) = ring.front_mut() {
            entry.1 = Instant::now() - Duration::from_secs(5);
        }
    }
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    // The laid-out text alpha of the galley holding `text`.
    let alpha_of = |text: &str| -> Option<u8> {
        shapes.iter().find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.text() == text => t
                .galley
                .job
                .sections
                .first()
                .map(|sec| sec.format.color.a()),
            _ => None,
        })
    };
    let old_alpha = alpha_of("old line").expect("the old line is painted");
    let fresh_alpha = alpha_of("fresh line").expect("the fresh line is painted");
    assert_eq!(old_alpha, 255, "a backdated line is fully opaque");
    assert!(
        fresh_alpha < 255,
        "a fresh line is faded in, got alpha {fresh_alpha}"
    );
}

/// The bitrate sparkline carries a "now … kbit/s" caption above the chart.
#[test]
fn the_sparkline_carries_a_now_value_caption() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    {
        let mut g = app.state.lock().unwrap();
        g.broadcast = Mode::Broadcasting;
        // Two+ samples so the sparkline (and caption) is drawn; newest is "now".
        g.video_kbps_history = std::collections::VecDeque::from(vec![100.0, 200.0, 150.0]);
        g.stats = Some(StreamStats {
            video_kbps: 150.0,
            ..Default::default()
        });
    }
    let size = egui::vec2(760.0, 560.0);
    let shapes = render_shapes(&mut app, size);
    assert!(
        shapes.iter().any(|s| matches!(&s.shape,
            egui::epaint::Shape::Text(t) if t.galley.text() == "now 150 kbit/s"
        )),
        "the sparkline should paint its 'now' value caption"
    );
}

/// The sparkline marks the recent peak (dot + faint vline); skipped when the
/// peak is the newest sample, as the "now" dot already marks it.
#[test]
fn the_sparkline_marks_the_recent_peak() {
    let size = egui::vec2(760.0, 560.0);
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    {
        let mut g = app.state.lock().unwrap();
        g.broadcast = Mode::Broadcasting;
        // A peak in the middle (not the newest sample), so the peak marker is drawn.
        g.video_kbps_history = std::collections::VecDeque::from(vec![100.0, 400.0, 150.0]);
        g.stats = Some(StreamStats {
            video_kbps: 150.0,
            ..Default::default()
        });
    }
    let shapes = render_shapes(&mut app, size);
    // The peak marker's dot: a small filled circle at the peak, in a dimmed accent.
    let peak_dot = shapes.iter().any(|s| match &s.shape {
        egui::epaint::Shape::Circle(c) => {
            (c.radius - 1.8).abs() < 0.01
                && c.fill == ACCENT.gamma_multiply(0.55)
                && c.stroke.width == 0.0
        }
        _ => false,
    });
    assert!(peak_dot, "the sparkline should paint its peak marker's dot");
    // The peak marker's faint vertical line: a 1.0 px vline in a dimmed accent.
    let peak_line = shapes.iter().any(|s| match &s.shape {
        egui::epaint::Shape::LineSegment { points, stroke } => {
            (points[0].x - points[1].x).abs() < 0.5
                && (stroke.width - 1.0).abs() < 0.01
                && stroke.color == ACCENT.gamma_multiply(0.18)
        }
        _ => false,
    });
    assert!(
        peak_line,
        "the sparkline should paint its peak marker's vertical line"
    );

    // Peak at the newest sample: the peak marker is skipped (the "now" dot marks it).
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    {
        let mut g = app.state.lock().unwrap();
        g.broadcast = Mode::Broadcasting;
        g.video_kbps_history = std::collections::VecDeque::from(vec![100.0, 150.0, 400.0]);
        g.stats = Some(StreamStats {
            video_kbps: 400.0,
            ..Default::default()
        });
    }
    let shapes = render_shapes(&mut app, size);
    let peak_dot = shapes.iter().any(|s| match &s.shape {
        egui::epaint::Shape::Circle(c) => {
            (c.radius - 1.8).abs() < 0.01
                && c.fill == ACCENT.gamma_multiply(0.55)
                && c.stroke.width == 0.0
        }
        _ => false,
    });
    assert!(
        !peak_dot,
        "the peak marker is skipped when the peak is the newest sample"
    );
}

/// The stats panel eases its display toward the latest sample, never past it,
/// so the numbers settle smoothly instead of jumping on each tick.
#[test]
fn the_stats_panel_eases_its_display_toward_the_target() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    {
        let mut g = app.state.lock().unwrap();
        g.broadcast = Mode::Broadcasting;
        g.stats = Some(StreamStats {
            viewers: 3,
            video_kbps: 5000.0,
            video_fps: 30.0,
            audio_kbps: 128.0,
            ..Default::default()
        });
    }
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(760.0, 560.0),
        )),
        ..Default::default()
    };
    // A few frames: the display eases from zero toward the target.
    for _ in 0..5 {
        let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
        out.textures_delta.clear();
    }
    assert!(
        app.disp_video_kbps > 0.0,
        "the display should have started easing from zero, got {}",
        app.disp_video_kbps
    );
    assert!(
        app.disp_video_kbps < 5000.0,
        "the display must not overshoot the target, got {}",
        app.disp_video_kbps
    );
    assert!(
        app.disp_viewers > 0.0 && app.disp_viewers < 3.0,
        "the viewer count eases toward its target, got {}",
        app.disp_viewers
    );
}

/// When idle, the eased stats display resets to zero so a new session starts clean.
#[test]
fn the_stats_display_resets_to_zero_when_idle() {
    let (mut app, _rt) = test_app();
    // Seed the display as if a broadcast had just ended mid-settle.
    app.disp_video_kbps = 4000.0;
    app.disp_viewers = 2.0;
    let size = egui::vec2(760.0, 560.0);
    let _ = render_shapes(&mut app, size);
    assert_eq!(app.disp_video_kbps, 0.0, "idle resets the video display");
    assert_eq!(app.disp_viewers, 0.0, "idle resets the viewer display");
}

/// The "Waiting for OBS to start streaming…" hint carries a breathing dot
/// (filled, radius 4.0); a broadcast already receiving media shows none.
#[test]
fn the_waiting_for_obs_hint_carries_a_breathing_dot() {
    let size = egui::vec2(760.0, 560.0);
    // The breathing dot: a filled radius-4.0 circle (ambient glow is larger,
    // sonar rings are stroked).
    let has_dot = |shapes: &[egui::epaint::ClippedShape]| -> bool {
        shapes.iter().any(|s| match &s.shape {
            egui::epaint::Shape::Circle(c) => {
                (c.radius - 4.0).abs() < 0.01 && c.stroke.width == 0.0 && c.fill.a() > 0
            }
            _ => false,
        })
    };

    // Live, no viewers, not receiving, no history: the hint and its dot are shown.
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    app.state.lock().unwrap().broadcast = Mode::Broadcasting;
    assert!(
        has_dot(&render_shapes(&mut app, size)),
        "the 'Waiting for OBS' hint should carry a breathing dot"
    );

    // Receiving media: the hint (and its dot) is dropped.
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    {
        let mut g = app.state.lock().unwrap();
        g.broadcast = Mode::Broadcasting;
        g.stats = Some(StreamStats {
            video_kbps: 5000.0,
            ..Default::default()
        });
    }
    assert!(
        !has_dot(&render_shapes(&mut app, size)),
        "a broadcast already receiving media should drop the waiting dot"
    );
}

/// `soft_glow` paints a 2-layer halo (0.03 / 0.06) in the colour it is given,
/// not a hard-coded accent.
#[test]
fn soft_glow_paints_a_halo_in_the_given_color() {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(100.0, 100.0),
        )),
        ..Default::default()
    };
    // The watch blue, not the accent green, so the colour is proved honoured.
    let color = WATCH_ACCENT;
    let mut out = ctx.run_ui(raw, |ui| {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(80.0, 30.0));
        soft_glow(ui.painter(), rect, egui::CornerRadius::same(4), color);
    });
    out.textures_delta.clear();
    let count = out
        .shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) => [0.03, 0.06]
                .iter()
                .any(|&a| r.fill == color.gamma_multiply(a)),
            _ => false,
        })
        .count();
    assert_eq!(
        count, 2,
        "soft_glow should paint a 2-layer halo in the given colour"
    );
}

/// The spinner paints a single mesh of 24 ring quads (96 vertices, 48 triangles) —
/// a 270° arc.
#[test]
fn spinner_paints_a_270_degree_arc() {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(100.0, 100.0),
        )),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        spinner(ui, 0.0, ACCENT);
    });
    out.textures_delta.clear();
    let meshes: Vec<&egui::epaint::Mesh> = out
        .shapes
        .iter()
        .filter_map(|s| match &s.shape {
            egui::epaint::Shape::Mesh(m) => Some(m.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(meshes.len(), 1, "the spinner paints exactly one mesh");
    let mesh = &meshes[0];
    assert_eq!(mesh.vertices.len(), 96, "24 quads × 4 vertices");
    assert_eq!(
        mesh.indices.len(),
        48 * 3,
        "24 quads × 2 triangles × 3 indices"
    );
}

/// The spinner's arc rotates with `secs`: two different times paint two different
/// meshes, so the animation actually moves.
#[test]
fn spinner_rotates_with_time() {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(100.0, 100.0),
        )),
        ..Default::default()
    };
    let mesh_at = |secs: f32| {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            spinner(ui, secs, ACCENT);
        });
        out.textures_delta.clear();
        out.shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Mesh(m) => Some(m.as_ref().clone()),
                _ => None,
            })
            .expect("the spinner paints a mesh")
    };
    let a = mesh_at(0.0);
    let b = mesh_at(0.3);
    assert_ne!(a, b, "the arc should rotate between two times");
}

/// The sonar pulse paints a ring that expands as it fades, and paints nothing
/// once fully faded.
#[test]
fn sonar_pulse_paints_a_fading_ring_that_expands() {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(100.0, 100.0),
        )),
        ..Default::default()
    };
    let ring = |secs: f32| {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            sonar_pulse(ui.painter(), egui::pos2(50.0, 50.0), 5.0, secs, 2.0, ACCENT);
        });
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Circle(c) => Some(*c),
                _ => None,
            })
            .next()
    };
    // The ring expands over the pulse.
    let early = ring(0.2).expect("an early-pulse frame paints a ring");
    let late = ring(1.0).expect("a late-pulse frame paints a ring");
    assert!(
        late.radius > early.radius,
        "the ring should expand over the pulse ({} -> {})",
        early.radius,
        late.radius
    );
    // Fully faded: nothing is painted.
    assert!(ring(1.99).is_none(), "a nearly-faded ring paints nothing");
}
