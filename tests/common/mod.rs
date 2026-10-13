//! Shared harness: builds an `App` and drives it headlessly.
//!
//! Each `tests/*.rs` file is its own crate, so the helpers live here and are
//! pulled in with `mod common;`.

#![allow(dead_code)] // each test file uses a subset

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use eframe::egui;
use iroh::SecretKey;
use streampipe::gui::{
    config::Config,
    log,
    state::{self, Mode, SharedState},
    App, REPAINT,
};
use tokio::sync::mpsc;

/// An `App` wired to throwaway state, plus the runtime it borrows a handle from.
pub fn test_app() -> (App, tokio::runtime::Runtime) {
    test_app_with(Config::default())
}

/// The same, seeded from an arbitrary config.
pub fn test_app_with(config: Config) -> (App, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let whip = state::whip_url(&config.broadcast_host, config.broadcast_port);
    let shared = Arc::new(Mutex::new(SharedState::new(SecretKey::generate(), whip)));
    let (stats_tx, _stats_rx) = mpsc::unbounded_channel();
    let app = App::new(
        rt.handle().clone(),
        config,
        shared,
        log::new_ring(),
        stats_tx,
    );
    (app, rt)
}

/// Drive frames headlessly and report the repaint delay the viewport settled on
/// (`Duration::MAX` = "don't wake me up"). Read after three passes: egui reports a
/// zero delay while it primes.
pub fn settled_repaint_delay(app: &mut App) -> Duration {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(760.0, 560.0),
        )),
        ..Default::default()
    };
    let mut delay = Duration::ZERO;
    for _ in 0..3 {
        let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
        delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .expect("root viewport");
        // No backend consumes the font texture upload; clear it so it can't panic.
        out.textures_delta.clear();
    }
    delay
}

/// Assert the viewport scheduled *some* follow-up repaint. Compared as "less than
/// `Duration::MAX`" — egui shaves the time spent in the passes off the delay.
pub fn assert_awake(delay: Duration) {
    assert!(
        delay < Duration::MAX,
        "expected a scheduled repaint, got {delay:?}"
    );
    assert!(delay <= REPAINT, "repaints should not lag {REPAINT:?}");
}

/// Render one real frame headlessly and return what was painted.
pub fn render_shapes(app: &mut App, size: egui::Vec2) -> Vec<egui::epaint::ClippedShape> {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
    out.textures_delta.clear();
    out.shapes
}

/// Run one headless frame, discarding the font-texture upload no backend eats.
pub fn frame(ctx: &egui::Context, app: &mut App, raw: egui::RawInput) {
    let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
    out.textures_delta.clear();
}

/// A focused, window-keyboard-focused raw input at the default window size.
pub fn focused_raw(events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(760.0, 560.0),
        )),
        focused: true,
        events,
        ..Default::default()
    }
}

/// A single Enter key press event.
pub fn enter_key() -> egui::Event {
    egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: Some(egui::Key::Enter),
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

/// A click: a press frame then a release frame on the same point. A free fn (not a
/// closure) so the app borrow ends before the assertions that follow.
pub fn click_at(app: &mut App, ctx: &egui::Context, at: egui::Pos2, size: egui::Vec2) {
    let press = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        events: vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                button: egui::PointerButton::Primary,
                pos: at,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        ..Default::default()
    };
    let release = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        events: vec![egui::Event::PointerButton {
            button: egui::PointerButton::Primary,
            pos: at,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
        ..Default::default()
    };
    frame(ctx, app, press);
    frame(ctx, app, release);
}

/// Drive the UI headlessly for `passes` frames and return the last frame's output.
pub fn drive_frames(
    app: &mut App,
    ctx: &egui::Context,
    size: egui::Vec2,
    passes: usize,
) -> egui::FullOutput {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
    out.textures_delta.clear();
    for _ in 1..passes {
        let next = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
        out.textures_delta.clear();
        out = next;
    }
    out
}

/// A real, parseable watch ticket for a throwaway endpoint identity.
///
/// A junk string like `"abc"` makes `start_watch` bail and spawn nothing, which
/// would make every click-through assertion vacuous.
pub fn real_ticket() -> String {
    streampipe::EndpointTicket::new(iroh::EndpointAddr::new(SecretKey::generate().public()))
        .to_string()
}

/// The live dot: the only small filled circle in the tab strip. The status dot is a
/// text glyph, and sonar pulses are stroked rings.
pub fn live_dot_present(shapes: &[egui::epaint::ClippedShape]) -> bool {
    shapes.iter().any(|s| match &s.shape {
        egui::epaint::Shape::Circle(c) => {
            c.stroke.width == 0.0
                && (2.0..=3.0).contains(&c.radius)
                && c.center.y < 120.0
                && c.fill.a() > 0
        }
        _ => false,
    })
}

/// Count the interactive rects the Options window registers with `mode`, rendering
/// only that window so the tab panels' gating can't confound it.
/// `interactive_rects_last_pass` drops disabled widgets.
pub fn options_interactive_rects(mode: Mode) -> usize {
    let (mut app, _rt) = test_app();
    app.state.lock().unwrap().broadcast = mode;
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(760.0, 560.0),
        )),
        ..Default::default()
    };
    // A few passes so the window and its grid settle at their final size. Only the
    // Options window is rendered, so the tab panels' gating can't confound the count.
    for _ in 0..3 {
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            app.options_window(ui.ctx());
        });
        out.textures_delta.clear();
    }
    ctx.interactive_rects_last_pass().len()
}

/// The rect a text shape with exactly this text was painted into. Takes the shape slice
/// (a `FullOutput`'s `shapes` field derefs to one) so it works for any frame output.
/// Used to locate a button by its label before `click_at`ing it.
pub fn text_rect(shapes: &[egui::epaint::ClippedShape], text: &str) -> Option<egui::Rect> {
    shapes.iter().find_map(|s| match &s.shape {
        egui::epaint::Shape::Text(t) if t.galley.text() == text => {
            Some(egui::Rect::from_min_size(t.pos, t.galley.rect.size()))
        }
        _ => None,
    })
}

/// True if a text shape with exactly this text was painted.
pub fn text_present(shapes: &[egui::epaint::ClippedShape], text: &str) -> bool {
    text_rect(shapes, text).is_some()
}

/// Interactive rects the size of the full-width "Watch Stream" button (36 tall; the
/// tab strip buttons are 32 and the ticket box ~70, so a 34..=38 band isolates it).
/// A blank ticket disables the button, so it drops out of the set.
pub fn watch_button_rects(ticket: &str) -> usize {
    watch_button_rects_with(Mode::Idle, ticket)
}

/// As [`watch_button_rects`], but with the broadcast track forced to `broadcast`.
pub fn watch_button_rects_with(broadcast: Mode, ticket: &str) -> usize {
    let (mut app, _rt) = test_app();
    app.tab = streampipe::gui::Tab::Watch;
    app.watch_ticket = ticket.to_string();
    app.state.lock().unwrap().broadcast = broadcast;
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(760.0, 560.0),
        )),
        ..Default::default()
    };
    for _ in 0..3 {
        frame(&ctx, &mut app, raw.clone());
    }
    ctx.interactive_rects_last_pass()
        .iter()
        .filter(|r| (34.0..=38.0).contains(&r.height()))
        .count()
}
