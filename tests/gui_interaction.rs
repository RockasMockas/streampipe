//! Interaction: focus, keyboard, hover, and click round-trips driven headlessly.

mod common;

use std::time::Instant;

use eframe::egui;
use streampipe::gui::{log, state::Mode, Tab, ACCENT};

use common::{click_at, enter_key, focused_raw, frame, render_shapes, test_app};

/// Autofocus grabs a prefilled ticket (the "press Enter to replay" case) but leaves an
/// empty box unfocused, so an idle Watch tab still falls asleep.
#[test]
fn autofocus_grabs_a_prefilled_ticket_but_not_an_empty_one() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    app.watch_ticket = "a-persisted-ticket".into();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, focused_raw(vec![]));
    frame(&ctx, &mut app, focused_raw(vec![]));
    assert!(
        ctx.memory(|m| m.focused()).is_some(),
        "a prefilled ticket autofocuses so Enter can start it"
    );

    // Empty: nothing focuses — this is what keeps the repaint-sleep regression green.
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, focused_raw(vec![]));
    frame(&ctx, &mut app, focused_raw(vec![]));
    assert_eq!(
        app.watch_ticket, "",
        "the headless app opens with an empty ticket"
    );
    assert!(
        ctx.memory(|m| m.focused()).is_none(),
        "an empty ticket must not autofocus (no blink repaints while idle)"
    );
}

/// Regression: Enter used to drop a stray newline into the multiline ticket field.
/// Now it never inserts one, and an invalid ticket never starts (matching the button).
#[test]
fn enter_on_the_focused_ticket_never_inserts_a_newline() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    app.watch_ticket = "not-a-real-ticket".into();
    let ctx = egui::Context::default();
    // Two frames for autofocus, then press Enter with focus held.
    frame(&ctx, &mut app, focused_raw(vec![]));
    frame(&ctx, &mut app, focused_raw(vec![]));
    assert!(ctx.memory(|m| m.focused()).is_some(), "ticket autofocused");
    frame(&ctx, &mut app, focused_raw(vec![enter_key()]));

    assert!(
        !app.watch_ticket.contains('\n'),
        "Enter must not drop a newline into the ticket box"
    );
    assert_eq!(
        app.watch_ticket, "not-a-real-ticket",
        "ticket text untouched"
    );
    assert_eq!(
        app.state.lock().unwrap().watch,
        Mode::Idle,
        "an invalid ticket surfaces an inline error but never starts"
    );
}

/// A blank ticket never starts a session, focused Enter or not.
#[test]
fn enter_on_a_blank_ticket_does_not_start() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    // Force focus on the blank box to isolate the blank gate from autofocus.
    app.focus_primary = false;
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, focused_raw(vec![enter_key()]));
    assert_eq!(
        app.state.lock().unwrap().watch,
        Mode::Idle,
        "Enter on a blank ticket does not start a session"
    );
}

/// On the Broadcast tab, when idle and focused, the start button owns keyboard focus
/// so Enter/Space can start it.
#[test]
fn broadcast_start_button_autofocuses_when_idle() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, focused_raw(vec![]));
    frame(&ctx, &mut app, focused_raw(vec![]));
    assert!(
        ctx.memory(|m| m.focused()).is_some(),
        "the idle start button autofocuses so Enter/Space can start it"
    );
}

/// Hovering a tab raises the pointing-hand cursor.
#[test]
fn hovering_a_tab_raises_the_pointing_hand_cursor() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    let size = egui::vec2(760.0, 560.0);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    // Render once (no pointer) to locate the Broadcast tab's fill rect.
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
    out.textures_delta.clear();
    let tab_rects: Vec<egui::Rect> = out
        .shapes
        .iter()
        .filter_map(|s| match &s.shape {
            // The two tab surfaces are the only ~32-tall filled rects up top.
            egui::epaint::Shape::Rect(r)
                if (30.0..34.0).contains(&r.rect.height()) && r.rect.min.y < 120.0 =>
            {
                Some(r.rect)
            }
            _ => None,
        })
        .collect();
    assert!(
        tab_rects.len() >= 2,
        "expected the two tab fill rects, got {tab_rects:?}"
    );
    // The Broadcast tab is the right-hand one.
    let bc = tab_rects
        .iter()
        .fold(&tab_rects[0], |a, b| if b.min.x > a.min.x { b } else { a });
    let center = bc.center();

    // egui computes hover from the *previous* pass's rect, so the first pass
    // establishes the rect and the second is the one whose hover is live.
    let ctx2 = egui::Context::default();
    let raw_hover = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        events: vec![egui::Event::PointerMoved(center)],
        ..Default::default()
    };
    frame(&ctx2, &mut app, raw_hover.clone());
    let out = ctx2.run_ui(raw_hover, |ui| app.draw(ui));
    assert_eq!(
        out.platform_output.cursor_icon,
        egui::CursorIcon::PointingHand,
        "hovering a tab should raise the pointing-hand cursor"
    );
}

/// Hovering the start button lifts its fill to the hover shade.
#[test]
fn hovering_the_start_button_lifts_its_fill() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    let size = egui::vec2(760.0, 560.0);
    // Located by exact colour: unique among the halo (0.02/0.05/0.09) and the
    // ticket box's flat alpha-14 tint.
    let resting_fill = ACCENT.gamma_multiply(0.18);
    let hover_fill = ACCENT.gamma_multiply(0.26);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    // Idle: locate the button by its resting fill.
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
    out.textures_delta.clear();
    let btn_rect = out
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) if r.fill == resting_fill => Some(r.rect),
            _ => None,
        })
        .expect("the idle start button paints its resting fill");

    // Two passes: egui reads the pointer from the previous pass.
    let ctx2 = egui::Context::default();
    let raw_hover = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        events: vec![egui::Event::PointerMoved(btn_rect.center())],
        ..Default::default()
    };
    frame(&ctx2, &mut app, raw_hover.clone());
    let out = ctx2.run_ui(raw_hover, |ui| app.draw(ui));
    assert!(
        out.shapes
            .iter()
            .any(|s| matches!(&s.shape, egui::epaint::Shape::Rect(r) if r.fill == hover_fill)),
        "hovering the start button should lift its fill to the hover shade"
    );
}

/// Pressing the start button pushes its fill in to the press shade.
#[test]
fn pressing_the_start_button_pushes_its_fill_in() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    let size = egui::vec2(760.0, 560.0);
    let resting_fill = ACCENT.gamma_multiply(0.18);
    let press_fill = ACCENT.gamma_multiply(0.20);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    // Idle: locate the button by its resting fill.
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
    out.textures_delta.clear();
    let btn_rect = out
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) if r.fill == resting_fill => Some(r.rect),
            _ => None,
        })
        .expect("the idle start button paints its resting fill");
    let center = btn_rect.center();

    // Two passes: egui reads the pointer from the previous pass.
    let ctx2 = egui::Context::default();
    let raw_press = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        events: vec![
            egui::Event::PointerMoved(center),
            egui::Event::PointerButton {
                button: egui::PointerButton::Primary,
                pos: center,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        ..Default::default()
    };
    frame(&ctx2, &mut app, raw_press.clone());
    let out = ctx2.run_ui(raw_press, |ui| app.draw(ui));
    assert!(
        out.shapes
            .iter()
            .any(|s| matches!(&s.shape, egui::epaint::Shape::Rect(r) if r.fill == press_fill)),
        "pressing the start button should push its fill in to the press shade"
    );
}

/// The log panel's close button (two crossing 1.5 px strokes on an idle app) closes
/// the panel when clicked.
#[test]
fn the_log_panel_close_button_closes_the_panel() {
    let (mut app, _rt) = test_app();
    app.log_open = true;
    // Fully open: the tween runs on the wall clock, which barely advances headlessly.
    app.log_anim = 1.0;
    let size = egui::vec2(760.0, 560.0);
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    // Locate the close button: the only 1.5 px line segments are its two strokes
    // (hairlines, bevels and separators are all 1.0).
    let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
    out.textures_delta.clear();
    let segs: Vec<[egui::Pos2; 2]> = out
        .shapes
        .iter()
        .filter_map(|s| match &s.shape {
            egui::epaint::Shape::LineSegment { points, stroke }
                if (stroke.width - 1.5).abs() < 0.01 =>
            {
                Some(*points)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        2,
        segs.len(),
        "the close button paints two crossing strokes"
    );
    let min_x = segs
        .iter()
        .flat_map(|p| [p[0].x, p[1].x])
        .fold(f32::INFINITY, f32::min);
    let max_x = segs
        .iter()
        .flat_map(|p| [p[0].x, p[1].x])
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = segs
        .iter()
        .flat_map(|p| [p[0].y, p[1].y])
        .fold(f32::INFINITY, f32::min);
    let max_y = segs
        .iter()
        .flat_map(|p| [p[0].y, p[1].y])
        .fold(f32::NEG_INFINITY, f32::max);
    let center = egui::pos2((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);

    click_at(&mut app, &ctx, center, size);
    assert!(
        !app.log_open,
        "clicking the close button must close the log panel"
    );
}

/// Hovering a copy box paints a 2-layer accent glow (0.03 / 0.06) and brightens its
/// copy glyph (0.5 -> 0.8).
#[test]
fn hovering_a_copy_box_paints_a_soft_glow_and_brightens_the_glyph() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    app.state.lock().unwrap().broadcast = Mode::Broadcasting;
    let size = egui::vec2(760.0, 560.0);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    // Locate the WHIP copy box by its accent-tinted fill (alpha 14 at rest).
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
    out.textures_delta.clear();
    let box_fill = egui::Color32::from_rgba_unmultiplied(80, 200, 120, 14);
    let box_rect = out
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) if r.fill == box_fill => Some(r.rect),
            _ => None,
        })
        .expect("the copy box paints its accent-tinted fill");
    let center = box_rect.center();

    // Hover the copy box: two passes make the hover live.
    let ctx2 = egui::Context::default();
    let raw_hover = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        events: vec![egui::Event::PointerMoved(center)],
        ..Default::default()
    };
    frame(&ctx2, &mut app, raw_hover.clone());
    let out = ctx2.run_ui(raw_hover, |ui| app.draw(ui));
    // Two accent rects at 0.03 / 0.06; the idle start button's halo uses
    // 0.02/0.05/0.09, so it can't match.
    let glow_count = out
        .shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) => [0.03, 0.06]
                .iter()
                .any(|&a| r.fill == ACCENT.gamma_multiply(a)),
            _ => false,
        })
        .count();
    assert_eq!(
        glow_count, 2,
        "hovering the copy box should paint a 2-layer soft glow"
    );
    // The copy glyph brightens: its two sheets are stroked brighter.
    let glyph_bright = ACCENT.gamma_multiply(0.8);
    let glyph_count = out
        .shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) => r.stroke.color == glyph_bright,
            _ => false,
        })
        .count();
    assert!(
        glyph_count >= 2,
        "hovering the copy box should brighten its copy glyph"
    );
}

/// The copy glyph becomes a checkmark while the copy toast is live.
#[test]
fn the_copy_glyph_becomes_a_checkmark_on_copy() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Broadcast;
    app.state.lock().unwrap().broadcast = Mode::Broadcasting;
    let size = egui::vec2(760.0, 560.0);
    // A checkmark stroke: 1.5 px in `ACCENT.gamma_multiply(flash)`. The flash decays
    // in real time, so the exact shade is unstable — the hue (green leads) is checked.
    let is_check_stroke = |s: &egui::epaint::ClippedShape| -> bool {
        matches!(&s.shape, egui::epaint::Shape::LineSegment { stroke, .. }
            if (stroke.width - 1.5).abs() < 0.01
                && stroke.color.g() > stroke.color.r()
                && stroke.color.g() > stroke.color.b())
    };

    let before = render_shapes(&mut app, size);
    assert_eq!(
        before.iter().filter(|s| is_check_stroke(s)).count(),
        0,
        "an un-copied box has no checkmark"
    );

    // Only the copied box (key "whip") morphs, so exactly two strokes appear.
    app.copied = Some(("whip", Instant::now()));
    let after = render_shapes(&mut app, size);
    let check = after.iter().filter(|s| is_check_stroke(s)).count();
    assert_eq!(
        check, 2,
        "the copied box's glyph should be a checkmark (two strokes), got {check}"
    );
}

/// A focused ticket box paints its bright focus ring; a prefilled ticket autofocuses
/// on a window-focused frame, so this is observable headlessly.
#[test]
fn the_focused_ticket_box_paints_a_focus_ring() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    app.watch_ticket = "a-persisted-ticket".into();
    let ctx = egui::Context::default();
    // Two frames for the autofocus to settle.
    frame(&ctx, &mut app, focused_raw(vec![]));
    frame(&ctx, &mut app, focused_raw(vec![]));
    assert!(
        ctx.memory(|m| m.focused()).is_some(),
        "a prefilled ticket autofocuses"
    );
    // A focused frame paints the box's bright focus stroke.
    let out = ctx.run_ui(focused_raw(vec![]), |ui| app.draw(ui));
    let focus_stroke = ACCENT.gamma_multiply(0.75);
    assert!(
        out.shapes.iter().any(|s| matches!(&s.shape,
            egui::epaint::Shape::Rect(r)
                if r.stroke.width > 0.0 && r.stroke.color == focus_stroke
        )),
        "the focused ticket box should paint its bright focus stroke"
    );
}

/// Hovering a log line paints a faint highlight (white, alpha 12).
#[test]
fn hovering_a_log_line_paints_a_faint_highlight() {
    let (mut app, _rt) = test_app();
    app.log_open = true;
    app.log_anim = 1.0;
    log::push(&app.log, "a log line to hover");
    let size = egui::vec2(760.0, 560.0);
    let has_highlight = |shapes: &[egui::epaint::ClippedShape]| -> bool {
        shapes.iter().any(|s| {
            matches!(&s.shape,
                egui::epaint::Shape::Rect(r)
                    if r.fill == egui::Color32::from_white_alpha(12)
            )
        })
    };
    // Not hovered: no highlight.
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
    out.textures_delta.clear();
    assert!(
        !has_highlight(&out.shapes),
        "an un-hovered log line paints no highlight"
    );

    // Locate the log line's text so the pointer can be moved over it.
    let line_rect = out
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.text() == "a log line to hover" => {
                Some(egui::Rect::from_min_size(t.pos, t.galley.size()))
            }
            _ => None,
        })
        .expect("the log line is painted");
    let center = line_rect.center();

    // Hover: two passes (egui reads the pointer from the previous pass).
    let ctx2 = egui::Context::default();
    let raw_hover = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        events: vec![egui::Event::PointerMoved(center)],
        ..Default::default()
    };
    frame(&ctx2, &mut app, raw_hover.clone());
    let out = ctx2.run_ui(raw_hover, |ui| app.draw(ui));
    assert!(
        has_highlight(&out.shapes),
        "hovering a log line should paint a faint highlight"
    );
}
