//! The Broadcast tab's pure helpers and painted widgets.
//!
//! Bind parsing is tested directly; the painted widgets are tested by running the
//! egui context headlessly (`Context::run_ui`) and inspecting the shape list.

use eframe::egui;
use streampipe::gui::broadcast::{
    bitrate_sparkline, bitrate_trend, copy_glyph_rects, eye_toggle, fit_monospace, parse_bind,
    sparkline_points, trend_arrow, Trend,
};
use streampipe::gui::config::Config;
use streampipe::gui::state::BindAddrs;

/// Empty bind fields mean ephemeral (no fixed socket, ticket changes each run).
#[test]
fn parse_bind_empty_is_ephemeral() {
    let bind = parse_bind(&Config::default()).unwrap();
    assert_eq!(bind, BindAddrs::default());
}

/// A fixed IPv4/IPv6 bind parses through and would pin the ticket's socket.
#[test]
fn parse_bind_accepts_fixed_addresses() {
    let cfg = Config {
        broadcast_bind_ipv4: Some(" 203.0.113.7:45678 ".into()),
        broadcast_bind_ipv6: Some("[2001:db8::1]:45678".into()),
        ..Config::default()
    };
    let bind = parse_bind(&cfg).unwrap();
    assert_eq!(
        bind,
        BindAddrs {
            ipv4: Some("203.0.113.7:45678".parse().unwrap()),
            ipv6: Some("[2001:db8::1]:45678".parse().unwrap()),
        }
    );
}

/// A malformed bind is surfaced as an error, not silently dropped to ephemeral.
#[test]
fn parse_bind_rejects_malformed() {
    let cfg = Config {
        broadcast_bind_ipv4: Some("not-an-address".into()),
        ..Config::default()
    };
    assert!(parse_bind(&cfg).is_err());
}

/// The copy glyph's two sheets sit inside the box, non-degenerate, front up-right
/// of the back — a misplaced glyph passes a shape-count check but not this.
#[test]
fn copy_glyph_sheets_are_in_bounds_and_offset() {
    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 34.0));
    let [back, front] = copy_glyph_rects(rect);
    for sheet in [back, front] {
        assert!(
            sheet.width() > 0.0 && sheet.height() > 0.0,
            "non-degenerate"
        );
        assert!(
            rect.contains(sheet.min) && rect.contains(sheet.max),
            "in bounds"
        );
    }
    let front_c = front.center();
    let back_c = back.center();
    assert!(front_c.x > back_c.x, "front is right of back");
    assert!(front_c.y < back_c.y, "front is above back");
}

/// `fit_monospace` leaves text that fits untouched, and middle-elides long values
/// to a width that fits — keeping head and tail so a ticket's changing transport
/// hints stay visible between refreshes. The full string is what gets copied.
#[test]
fn fit_monospace_elides_only_when_too_wide() {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(760.0, 560.0),
        )),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        let short = "http://127.0.0.1:8080/whip";
        assert_eq!(
            fit_monospace(ui, short, 400.0),
            short,
            "a WHIP URL that fits is unchanged"
        );

        let long = "abcdefghij".repeat(20);
        let elided = fit_monospace(ui, &long, 200.0);
        assert!(
            elided.contains('…') && elided.chars().count() < long.chars().count(),
            "a long ticket is middle-elided, got {elided:?}"
        );
        assert!(
            elided.starts_with('a') && elided.ends_with('j'),
            "both ends are kept so the changing tail stays visible, got {elided:?}"
        );
        let w = ui
            .painter()
            .layout_no_wrap(
                elided.clone(),
                egui::TextStyle::Monospace.resolve(ui.style()),
                egui::Color32::PLACEHOLDER,
            )
            .rect
            .width();
        assert!(w <= 200.0, "elided display width {w} must fit 200");
    });
    out.textures_delta.clear();
}

/// `sparkline_points` spans the rect edge to edge, peak above troughs, all points
/// inside the rect, and floors the scale at 60 kbit/s so quiet streams don't hug
/// the bottom edge.
#[test]
fn sparkline_points_span_the_rect_and_scale_to_the_peak() {
    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 26.0));
    let samples = [60.0, 120.0, 60.0];
    let pts = sparkline_points(&samples, rect);
    assert_eq!(pts.len(), samples.len());
    assert_eq!(pts[0].x, rect.left(), "oldest sample at the left edge");
    assert_eq!(pts[2].x, rect.right(), "newest sample at the right edge");
    assert!(
        pts[1].y < pts[0].y,
        "the peak sample sits above the troughs"
    );
    for p in &pts {
        assert!(rect.contains(*p), "point {p:?} escapes the rect");
    }

    // A quiet stream (below the 60 kbit/s floor) stays low: its peak maps
    // well clear of the top of the rect.
    let quiet = sparkline_points(&[10.0, 20.0], rect);
    assert!(
        quiet[1].y > rect.height() * 0.5,
        "a 20 kbit/s stream must not scale to the full height, got y={}",
        quiet[1].y
    );
}

/// The sparkline's area fill is a vertical gradient (bottom-edge vertices
/// transparent, line vertices carry the accent's alpha), and the newest sample
/// carries a small filled "now" dot.
#[test]
fn the_sparkline_fills_with_a_vertical_gradient_and_marks_the_now_point() {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(300.0, 60.0),
        )),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        bitrate_sparkline(ui, &[60.0, 120.0, 60.0], 0.0);
    });
    out.textures_delta.clear();

    // The area fill: one mesh, bottom-edge vertices transparent, line vertices
    // more opaque the higher they sit.
    let mesh = out
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Mesh(m) => Some(m.as_ref()),
            _ => None,
        })
        .expect("the sparkline paints its area mesh");
    let bottom = &mesh.vertices[0];
    assert_eq!(
        bottom.color.a(),
        0,
        "the fill fades to nothing at the bottom edge"
    );
    let line_vertex = &mesh.vertices[2];
    assert!(
        line_vertex.color.a() > bottom.color.a(),
        "the fill must be strongest near the line, got a={}",
        line_vertex.color.a()
    );
    assert!(
        line_vertex.color.a() < 255,
        "the fill stays a soft tint, not an opaque block"
    );

    // The "now" dot: a small filled circle (a stroked ring would have a width).
    let dot = out.shapes.iter().any(|s| {
        matches!(&s.shape,
            egui::epaint::Shape::Circle(c)
                if c.stroke.width == 0.0 && (2.0..3.0).contains(&c.radius) && c.fill.a() > 0
        )
    });
    assert!(dot, "the newest sample should carry a filled 'now' dot");
}

/// The "now" dot carries a soft sonar pulse (a stroked ring in the accent, same
/// language as the status/viewers dots), distinct from the filled dot (width 0);
/// it expands and fades over the pulse.
#[test]
fn the_sparkline_now_dot_carries_a_sonar_pulse() {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(300.0, 60.0),
        )),
        ..Default::default()
    };
    // The sonar ring at a given time: a stroked circle (width > 0).
    let ring = |secs: f32| -> Option<f32> {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            bitrate_sparkline(ui, &[60.0, 120.0, 60.0], secs);
        });
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Circle(c) if c.stroke.width > 0.0 => Some(c.radius),
                _ => None,
            })
            .next()
    };
    // Mid-pulse: a ring is painted, and it expands over the pulse.
    let early = ring(0.2).expect("an early-pulse frame paints a ring");
    let late = ring(1.0).expect("a late-pulse frame paints a ring");
    assert!(
        late > early,
        "the ring should expand over the pulse ({} -> {})",
        early,
        late
    );
    // Near the end of the pulse the ring has faded out: nothing is painted.
    assert!(ring(1.99).is_none(), "a nearly-faded ring paints nothing");
}

/// `bitrate_trend` classifies direction: clear rise → Rising, clear drop →
/// Falling, within the noise band (or too few samples) → Flat; a two-sample rise
/// compares against the first.
#[test]
fn bitrate_trend_classifies_rising_falling_and_flat() {
    assert_eq!(bitrate_trend(&[100.0, 100.0, 100.0, 200.0]), Trend::Rising);
    assert_eq!(bitrate_trend(&[200.0, 200.0, 200.0, 100.0]), Trend::Falling);
    // A 2 kbit/s wobble on a 100 kbit/s stream is within the band: flat.
    assert_eq!(bitrate_trend(&[100.0, 100.0, 100.0, 102.0]), Trend::Flat);
    // Too few samples to judge: flat.
    assert_eq!(bitrate_trend(&[]), Trend::Flat);
    assert_eq!(bitrate_trend(&[100.0]), Trend::Flat);
    // A two-sample rise still compares against the first sample.
    assert_eq!(bitrate_trend(&[100.0, 200.0]), Trend::Rising);
}

/// `trend_arrow` paints a triangle mesh for rising/falling and a plain dash
/// (line segment, no mesh) for flat, so the three states read as distinct marks.
#[test]
fn the_trend_arrow_paints_a_triangle_or_a_dash() {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(100.0, 100.0),
        )),
        ..Default::default()
    };
    let has_mesh = |trend: Trend| {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(raw.clone(), |ui| trend_arrow(ui, trend));
        out.textures_delta.clear();
        out.shapes
            .iter()
            .any(|s| matches!(&s.shape, egui::epaint::Shape::Mesh(_)))
    };
    assert!(
        has_mesh(Trend::Rising),
        "a rising trend paints a triangle mesh"
    );
    assert!(
        has_mesh(Trend::Falling),
        "a falling trend paints a triangle mesh"
    );
    // Flat: a dash (a line segment), and no triangle mesh.
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(raw, |ui| trend_arrow(ui, Trend::Flat));
    out.textures_delta.clear();
    assert!(
        out.shapes
            .iter()
            .any(|s| matches!(&s.shape, egui::epaint::Shape::LineSegment { .. })),
        "a flat trend paints a dash"
    );
    assert!(
        !out.shapes
            .iter()
            .any(|s| matches!(&s.shape, egui::epaint::Shape::Mesh(_))),
        "a flat trend paints no triangle"
    );
}

/// The sparkline paints a dashed horizontal average line (short same-y segments in
/// muted green-grey), distinct from the accent data line, so normal level is
/// visible against current value and peak.
#[test]
fn the_sparkline_paints_a_dashed_average_reference_line() {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(300.0, 60.0),
        )),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        bitrate_sparkline(ui, &[60.0, 120.0, 60.0], 0.0);
    });
    out.textures_delta.clear();
    let avg_color = egui::Color32::from_rgb(120, 150, 135);
    let avg_segs = out
        .shapes
        .iter()
        .filter(|s| {
            matches!(&s.shape,
                egui::epaint::Shape::LineSegment { points, stroke }
                    if (points[0].y - points[1].y).abs() < 0.01
                        && stroke.color == avg_color
            )
        })
        .count();
    assert!(
        avg_segs >= 2,
        "the sparkline should paint a dashed average line (got {avg_segs} segments)"
    );
}

/// The eye toggle paints an eyeball circle always, plus a filled pupil when open
/// and a lid line when closed, so the two states read as distinct.
#[test]
fn the_eye_toggle_paints_an_open_or_closed_eye() {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(40.0, 40.0),
        )),
        ..Default::default()
    };
    let open: Vec<_> = {
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            eye_toggle(ui, true, egui::vec2(34.0, 34.0));
        });
        out.textures_delta.clear();
        out.shapes
    };
    let closed: Vec<_> = {
        let mut out = ctx.run_ui(raw, |ui| {
            eye_toggle(ui, false, egui::vec2(34.0, 34.0));
        });
        out.textures_delta.clear();
        out.shapes
    };
    // Open: an eye circle stroke + a filled pupil (a circle with fill, no stroke).
    let open_has_pupil = open.iter().any(|s| match &s.shape {
        egui::epaint::Shape::Circle(c) => c.stroke.is_empty() && c.fill.a() > 0,
        _ => false,
    });
    assert!(open_has_pupil, "an open eye paints a filled pupil");
    // Closed: an eye circle stroke + a horizontal lid line, and no filled pupil.
    let closed_has_lid = closed.iter().any(|s| {
        matches!(&s.shape,
            egui::epaint::Shape::LineSegment { points, .. }
                if (points[0].y - points[1].y).abs() < 0.01)
    });
    let closed_has_pupil = closed.iter().any(|s| match &s.shape {
        egui::epaint::Shape::Circle(c) => c.stroke.is_empty(),
        _ => false,
    });
    assert!(closed_has_lid, "a closed eye paints a lid line");
    assert!(!closed_has_pupil, "a closed eye paints no pupil");
}
