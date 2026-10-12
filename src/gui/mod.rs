//! The GUI front-end.
//!
//! `streampipe` with no arguments opens this. It owns the eframe event loop on
//! the main thread and drives the async iroh engine over channels on two
//! separate tokio runtimes: the primary one runs the UI's async work (the
//! stats consumer that requests repaints), and a dedicated `streampipe-engine`
//! runtime runs every media session, so nothing in the WebRTC hot path shares a
//! thread with rendering. See [`state`] for the threading contract.

mod broadcast;
mod config;
mod log;
mod options;
mod state;
mod watch;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use eframe::egui::{self, Ui};
use streampipe::webrtc::{Player, StreamStats};
use tokio::{runtime::Handle, sync::mpsc};

use self::{
    config::Config,
    state::{Mode, SharedState},
};

/// The repaint interval used while a session is live, so logs and stats refresh
/// without a per-event repaint storm. Too coarse for an animation: a fading toast
/// gets its own, much shorter cadence ([`TOAST_FRAME`]).
const REPAINT: Duration = Duration::from_millis(250);
/// How long a "Copied" toast stays visible.
const TOAST: Duration = Duration::from_millis(1500);
/// The repaint interval while a toast is on screen: ~60 fps, i.e. display rate.
///
/// The fade is an animation, and on [`REPAINT`] a [`TOAST`]-long one only gets six
/// frames — the pill visibly steps down through six greys instead of dissolving.
const TOAST_FRAME: Duration = Duration::from_millis(16);
/// How long a copy box's click flash brightens its fill and glyph. Shorter than
/// [`TOAST`] so the repaint guard stays armed through the whole flash.
const FLASH: Duration = Duration::from_millis(450);
/// The "live/success" accent: broadcasting status, the copy toast, and viewers.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(80, 200, 120);
/// The "watching" accent: the watch status dot and the connecting spinner.
const WATCH_ACCENT: egui::Color32 = egui::Color32::from_rgb(90, 160, 255);
/// The "error" red: failed sessions, the stop halo, and the error callout.
const ERROR: egui::Color32 = egui::Color32::from_rgb(220, 90, 90);
/// How long the tab content's fade-in takes, so a tab switch fades the new content
/// in rather than hard-cutting. Short enough to feel instant, long enough to read.
const TAB_FADE_SECS: f32 = 0.16;
/// How long a new log line fades in (seconds), so a line eases in rather than
/// popping in. Short enough to feel instant, long enough to read as a gentle reveal.
const LOG_FADE_SECS: f32 = 0.2;

/// The two main tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    /// Watch a stream from a ticket.
    Watch,
    /// Start broadcasting to OBS over WHIP.
    Broadcast,
}

/// The application.
pub struct App {
    /// Handle to the background tokio runtime that runs the engine.
    handle: Handle,
    /// Cross-thread shared state.
    state: Arc<Mutex<SharedState>>,
    /// The persisted configuration, edited live and saved on change and quit.
    config: Config,
    /// The log ring fed by the tee writer.
    log: log::LogRing,
    /// The stats channel sender, cloned to each broadcast.
    stats_tx: mpsc::UnboundedSender<StreamStats>,
    /// The selected tab.
    tab: Tab,
    /// Whether the options panel is open.
    show_options: bool,
    /// Whether the bottom log panel is expanded.
    log_open: bool,
    /// Whether the WHIP bearer token is shown in cleartext on the Broadcast tab's
    /// Advanced settings. Masked (password dots) by default so a shared secret is not
    /// left legible on screen; the eye toggle by the field flips it.
    show_bearer: bool,
    /// The ticket text on the Watch tab.
    watch_ticket: String,
    /// The player selected on the Watch tab.
    watch_player: Player,
    /// The buffer field on the Watch tab, in milliseconds (empty = auto).
    watch_buffer: String,
    /// The initial player volume on the Watch tab, in percent (0-100).
    watch_volume: u8,
    /// The last copied box and when, for the transient toast.
    copied: Option<(&'static str, Instant)>,
    /// A transient action-complete toast (e.g. "Relay refreshed ✓") and when it was
    /// raised, shown by [`App::toast_overlay`] in the same pill as a copy toast.
    toast: Option<(String, Instant)>,
    /// Clock origin for the viewers pulse, so it animates smoothly while live.
    pulse: Instant,
    /// Whether the next window-focused frame should autofocus the active tab's
    /// primary control (the ticket box on Watch, the start button on Broadcast).
    /// Set on open and on every tab switch; cleared the first time a window that
    /// that actually has keyboard focus consumes it, so it never steals focus from a
    /// widget the user has already chosen.
    focus_primary: bool,
    /// The tab underline's slide position: 0.0 = Watch, 1.0 = Broadcast.
    /// Tweened in [`App::draw`] so a tab switch slides the underline across the
    /// strip instead of jumping between the two tabs.
    tab_slide: f32,
    /// The previous frame's wall clock, for the tab underline tween's frame delta.
    last_frame: Instant,
    /// The tab content's fade-in alpha: 0.0 = hidden, 1.0 = fully shown. Reset to
    /// 0.0 by [`App::switch_tab`] and eased up over ~160 ms in [`App::draw`], so a
    /// tab switch fades the new content in instead of hard-cutting to it.
    tab_fade: f32,
    /// Whether the tab fade-in is in flight; arms the repaint guard in [`App::draw`]
    /// so frames keep coming until the fade settles, then an idle window sleeps.
    tab_fade_active: bool,
    /// The instant of the last tab switch, the fade-in's clock origin.
    tab_switched_at: Instant,
    /// The log panel's open fraction: 0.0 = fully closed, 1.0 = fully open. Tweened
    /// in [`App::log_panel`] so toggling the panel slides its height instead of
    /// hard-cutting between 0 and half the window.
    log_anim: f32,
    /// The previous frame's wall clock, for the log panel tween's frame delta.
    log_last_frame: Instant,
    /// The eased *display* of the live broadcast stats, tweened toward the latest
    /// [`StreamStats`] each frame in the stats panel so the numbers settle smoothly
    /// instead of jumping on each 5-second tick. Only the broadcast track drives the
    /// stats panel, so these are reset to zero in [`App::draw`] the moment a broadcast
    /// goes idle: a new broadcast then starts from a clean zero rather than counting
    /// down from the previous session's values.
    disp_viewers: f32,
    disp_video_kbps: f32,
    disp_video_fps: f32,
    disp_audio_kbps: f32,
}

impl App {
    /// Build the app from the engine runtime handle, config and channels.
    ///
    /// `handle` is the handle of the dedicated `streampipe-engine` runtime, so
    /// every media session the UI starts runs on threads the renderer never
    /// shares. The tests pass the handle of a runtime they keep alive.
    fn new(
        handle: Handle,
        config: Config,
        state: Arc<Mutex<SharedState>>,
        log: log::LogRing,
        stats_tx: mpsc::UnboundedSender<StreamStats>,
    ) -> Self {
        let watch_player = config.default_player();
        let watch_buffer = config
            .buffer_ms
            .map(|ms| ms.to_string())
            .unwrap_or_default();
        // The slider's range is 0..=100; the `min` only guards a value restored
        // from a hand-edited config that names a louder setting than we can show.
        let watch_volume = config.volume.min(100);
        App {
            handle,
            state,
            watch_ticket: config.last_ticket.clone(),
            config,
            log,
            stats_tx,
            tab: Tab::Watch,
            show_options: false,
            log_open: false,
            show_bearer: false,
            watch_player,
            watch_buffer,
            watch_volume,
            copied: None,
            toast: None,
            pulse: Instant::now(),
            focus_primary: true,
            tab_slide: 0.0,
            last_frame: Instant::now(),
            tab_fade: 1.0,
            tab_fade_active: false,
            tab_switched_at: Instant::now(),
            log_anim: 0.0,
            log_last_frame: Instant::now(),
            disp_viewers: 0.0,
            disp_video_kbps: 0.0,
            disp_video_fps: 0.0,
            disp_audio_kbps: 0.0,
        }
    }

    /// The bottom status bar with the Settings entry and the session status.
    fn menu_bar(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        // Computed before the closure: the closure mutably borrows `self` for the
        // Options toggle, so the pulse clock is read here.
        let secs = self.pulse.elapsed().as_secs_f32();
        egui::menu::MenuBar::new().ui(ui, |ui| {
            // Open the settings panel directly — no intermediate dropdown.
            if ui.button("Settings").clicked() {
                self.show_options = !self.show_options;
            }
            ui.separator();
            let base = if snap.broadcast == Mode::Broadcasting {
                // Broadcasting leads the colour; watching while live stays green too.
                ACCENT
            } else if snap.watch == Mode::Watching {
                WATCH_ACCENT
            } else if snap.any_active() {
                egui::Color32::YELLOW
            } else {
                egui::Color32::GRAY
            };
            // The dot breathes only while live; a `Starting` track stays calm yellow
            // and `Idle` flat gray, so a steady dot never reads as a running session.
            let pulsing = snap.broadcast == Mode::Broadcasting || snap.watch == Mode::Watching;
            let dot = if pulsing {
                base.gamma_multiply(pulse_factor(secs))
            } else {
                base
            };
            ui.spacing_mut().item_spacing.x = 4.0;
            let dot_resp = ui.colored_label(dot, "●");
            // A soft sonar pulse around the live dot, so "live" reads as an active
            // signal rather than a static glyph. Sized small enough to stay within
            // the menu bar's bounds.
            if pulsing {
                sonar_pulse(ui.painter(), dot_resp.rect.center(), 4.0, secs, 2.0, base);
            }
            ui.colored_label(base, snap.status());
            // A compact "LIVE" badge next to the status label while a session is
            // live, so the live state reads at a glance rather than only as a small
            // breathing dot. The badge breathes in the same curve as the dot, so the
            // two read as one signal. A stroked pill (not a line segment), so it
            // never disturbs the line-segment-based tests.
            if pulsing {
                ui.add_space(2.0);
                live_badge(ui, base, pulse_factor(secs));
            }
            let err = snap.broadcast_error.as_ref().or(snap.watch_error.as_ref());
            if let Some(err) = err {
                ui.colored_label(ERROR, truncate(err, 60));
            }
            // A subtle keyboard-shortcut hint after the status, so the (otherwise
            // hidden) shortcuts are discoverable without a manual. Weak and dim so it
            // reads as a gentle nudge, not a competing label. Only shown when it fits
            // in the remaining space, so it never overflows at narrow window widths.
            let hint = "Ctrl+L log · Ctrl+, settings";
            let hint_w = ui
                .painter()
                .layout_no_wrap(
                    hint.to_string(),
                    egui::FontId::proportional(11.0),
                    egui::Color32::PLACEHOLDER,
                )
                .rect
                .width();
            if ui.available_width() >= hint_w + 8.0 {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(hint).weak().size(11.0));
            }
        });
    }

    /// The app wordmark: a large, crisp, two-tone title with a soft drop shadow.
    /// "Stream" is near-white and "pipe" a soft accent green, so the wordmark
    /// carries a small brand mark and echoes the live accent used across the app.
    /// Each word is a single anti-aliased glyph (no faux-bold offset copies, which
    /// render jaggy); weight comes from size and the shadow gives it depth.
    fn brand_title(&mut self, ui: &mut Ui) {
        let font_id = egui::FontId::proportional(30.0);
        let fg = egui::Color32::from_rgb(242, 242, 246);
        // A soft, slightly lightened accent green for "pipe": clearly green, but
        // muted enough to read as a brand tint rather than a status colour.
        let pipe = egui::Color32::from_rgb(110, 205, 150);
        // A soft shadow: the same glyphs in dark, low-alpha layers at a few
        // offsets, faking a blurred drop shadow behind the crisp text.
        let shadow = egui::Color32::from_black_alpha(70);

        let stream = ui
            .painter()
            .layout_no_wrap("Stream".into(), font_id.clone(), fg);
        let pipe_galley = ui
            .painter()
            .layout_no_wrap("pipe".into(), font_id.clone(), pipe);
        let stream_shadow = ui
            .painter()
            .layout_no_wrap("Stream".into(), font_id.clone(), shadow);
        let pipe_shadow = ui.painter().layout_no_wrap("pipe".into(), font_id, shadow);

        // The two words sit side by side; the total width is the sum of the two
        // galleys. Room for the glyph plus the shadow's furthest offset so it is
        // not clipped.
        let size = egui::vec2(
            stream.size().x + pipe_galley.size().x,
            stream.size().y.max(pipe_galley.size().y),
        ) + egui::vec2(3.0, 3.0);
        let (rect, _resp) = ui.allocate_exact_size(size, egui::Sense::hover());
        let pos = rect.min;
        let pipe_x = egui::vec2(stream.size().x, 0.0);

        let painter = ui.painter();
        for off in [
            egui::vec2(1.0, 1.0),
            egui::vec2(2.0, 2.0),
            egui::vec2(3.0, 3.0),
        ] {
            painter.galley(pos + off, stream_shadow.clone(), egui::Color32::PLACEHOLDER);
            painter.galley(
                pos + pipe_x + off,
                pipe_shadow.clone(),
                egui::Color32::PLACEHOLDER,
            );
        }
        painter.galley(pos, stream, egui::Color32::PLACEHOLDER);
        painter.galley(pos + pipe_x, pipe_galley, egui::Color32::PLACEHOLDER);
    }

    /// Switch to `tab`, re-arming the content fade-in and the primary-control
    /// autofocus. A no-op (no fade) when already on `tab`, so clicking the active
    /// tab does not replay the transition. The fade is reset here — not in the
    /// tween — so the new tab's content is hidden on the very frame the switch is
    /// detected, then fades in over the following frames.
    fn switch_tab(&mut self, tab: Tab) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.focus_primary = true;
        self.tab_fade = 0.0;
        self.tab_fade_active = true;
        self.tab_switched_at = Instant::now();
    }

    /// The tab strip.
    fn tab_bar(&mut self, ui: &mut Ui) {
        self.brand_title(ui);
        ui.add_space(6.0);
        // Which tracks are live, so the active tab can carry a small breathing dot
        // (green for a broadcast, blue for a watch) — a live session is then visible
        // at a glance right on the strip, not only in the status bar below.
        let snap = state::snapshot(&self.state);
        let watch_live = snap.watch == Mode::Watching;
        let bc_live = snap.broadcast == Mode::Broadcasting;
        // The dot's breath clock; it only advances while a repaint is running, which
        // is exactly while a session is live, so the dot breathes only when it should.
        let secs = self.pulse.elapsed().as_secs_f32();
        // Two equal-width tabs that fill the whole strip; the selectable label is
        // sized to its half so the entire tab box (not just the text) is the click
        // target.
        let half = ui.available_width() / 2.0;
        let tab_h = 32.0;
        let (watch, bc) = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // Style both states explicitly (rather than relying on `.selected()`,
                // which only themes the active tab): the selected tab is a lighter
                // surface with full-strength text (plus the accent underline below),
                // while unselected tabs recede to a darker surface and dimmed text,
                // keeping focus on the primary Start Broadcasting action.
                //
                // Each tab is painted by hand (rather than an `egui::Button` with a
                // fixed fill) so a hover can lift the surface: an explicit `.fill()`
                // would otherwise suppress egui's hover feedback and the strip would
                // feel dead under the mouse. Hovering lifts either tab toward the
                // selected look, so the whole box reads as a live control.
                let make = |ui: &mut Ui,
                            label: &str,
                            selected: bool,
                            live: bool,
                            dot_color: egui::Color32| {
                    // The whole box is the click target; the hover is read from the
                    // same response before the surface is painted.
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(half, tab_h), egui::Sense::click());
                    let hovered = resp.hovered();
                    if hovered {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let corner = ui.style().visuals.widgets.inactive.corner_radius;
                    let fill = if selected {
                        egui::Color32::from_white_alpha(if hovered { 40 } else { 26 })
                    } else {
                        egui::Color32::from_black_alpha(if hovered { 40 } else { 70 })
                    };
                    let stroke = egui::Stroke::new(
                        1.0,
                        if selected {
                            egui::Color32::from_white_alpha(if hovered { 70 } else { 45 })
                        } else {
                            egui::Color32::from_white_alpha(if hovered { 30 } else { 10 })
                        },
                    );
                    let text_color = if selected {
                        egui::Color32::WHITE
                    } else if hovered {
                        egui::Color32::from_rgb(200, 200, 205)
                    } else {
                        egui::Color32::GRAY
                    };
                    // Paint the surface, the outline, then the centred label.
                    ui.painter().rect_filled(rect, corner, fill);
                    ui.painter()
                        .rect_stroke(rect, corner, stroke, egui::StrokeKind::Middle);
                    let font_id = ui.style().text_styles[&egui::TextStyle::Button].clone();
                    let galley =
                        ui.painter()
                            .layout_no_wrap(label.to_string(), font_id, text_color);
                    // The live dot (if any) sits just right of the label; the label
                    // and dot are centred together as one group, so adding the dot
                    // does not lurch the label off-centre. Read the label's width
                    // before painting it (the paint consumes the galley).
                    let label_w = galley.rect.width();
                    let dot_d = 5.0;
                    let gap = 6.0;
                    let total_w = label_w + if live { gap + dot_d } else { 0.0 };
                    let start_x = rect.center().x - total_w * 0.5;
                    let text_pos =
                        egui::pos2(start_x, rect.center().y - galley.rect.height() * 0.5);
                    ui.painter().galley(text_pos, galley, text_color);
                    if live {
                        // A shallow breath (the same curve as the status dot) so the
                        // dot reads as a live signal rather than a static mark.
                        let dot = dot_color.gamma_multiply(pulse_factor(secs));
                        let dot_c =
                            egui::pos2(start_x + label_w + gap + dot_d * 0.5, rect.center().y);
                        ui.painter().circle_filled(dot_c, dot_d * 0.5, dot);
                    }
                    resp
                };
                let w = make(
                    ui,
                    "Watch Stream",
                    self.tab == Tab::Watch,
                    watch_live,
                    WATCH_ACCENT,
                );
                let b = make(ui, "Broadcast", self.tab == Tab::Broadcast, bc_live, ACCENT);
                if w.clicked() {
                    self.switch_tab(Tab::Watch);
                }
                if b.clicked() {
                    self.switch_tab(Tab::Broadcast);
                }
                (w, b)
            })
            .inner;
        // A persistent accent underline marks the active tab, so selection is
        // legible even when the selectable_label background is subtle. A slim,
        // rounded bar inset from each tab edge reads as deliberate rather than a
        // heavy full-width block.
        // The underline's position is tweened by [`App::draw`] (`tab_slide`), so a
        // tab switch slides it across the strip instead of jumping between tabs.
        // Both tabs are equal width, so only the x range interpolates.
        let t = self.tab_slide;
        let x0 = watch.rect.min.x + (bc.rect.min.x - watch.rect.min.x) * t;
        let x1 = watch.rect.max.x + (bc.rect.max.x - watch.rect.max.x) * t;
        let underline = egui::Rect::from_min_max(
            egui::pos2(x0 + 5.0, watch.rect.max.y - 2.0),
            egui::pos2(x1 - 5.0, watch.rect.max.y),
        );
        ui.painter()
            .rect_filled(underline, egui::CornerRadius::same(1), ACCENT);
        ui.separator();
    }

    /// The log panel along the bottom.
    ///
    /// Fully collapsed (height 0) by default. Toggled by Ctrl/Cmd+L to occupy
    /// half the window height. The height is tweened (see [`log_anim_step`]) so
    /// the panel slides open and closed instead of hard-cutting between the two
    /// extremes; the tween runs on the wall clock and stops requesting frames the
    /// moment it settles, so an idle (closed) window still falls asleep.
    fn log_panel(&mut self, ui: &mut Ui) {
        // Tween the open fraction toward 1.0 (open) or 0.0 (closed). Only a toggle
        // (which repaints) ever changes the target, so the tween only moves right
        // after one and then settles.
        let target = if self.log_open { 1.0 } else { 0.0 };
        let now = Instant::now();
        let dt = (now - self.log_last_frame).as_secs_f32().clamp(0.0, 0.1);
        self.log_last_frame = now;
        let log_moving = (self.log_anim - target).abs() > 0.002;
        if log_moving {
            self.log_anim = log_anim_step(self.log_anim, target, dt);
        }
        // Fully closed: no panel, no repaint, so an idle window sleeps.
        if self.log_anim <= 0.001 {
            return;
        }
        let half = ui.ctx().content_rect().height() * 0.5;
        let h = (half * self.log_anim).max(1.0);
        // Snapshot the lines (with their arrival instants) before the panel, so the
        // fade-in repaint guard below can be armed from the same data the panel draws.
        let lines = log::snapshot(&self.log);
        // Whether any line is still fading in (younger than the fade window). While
        // true, the panel wants display-rate frames so the fade is smooth; the moment
        // every line is fully opaque the guard disarms and the panel falls back to the
        // 250 ms session cadence (so an idle, settled log still lets the window sleep).
        let any_fading = lines
            .iter()
            .any(|(_, at)| at.elapsed().as_secs_f32() < LOG_FADE_SECS);
        egui::Panel::bottom("log").exact_size(h).show(ui, |ui| {
            // A slim header: the panel's name and a close button, so the panel
            // can be dismissed without knowing the Ctrl/Cmd+L shortcut. The
            // close is the same toggle as the shortcut, so the height tween
            // slides the panel shut either way.
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Log").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if close_button(ui) {
                        self.log_open = false;
                        ui.ctx().request_repaint();
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    let default = ui.style().visuals.text_color();
                    if lines.is_empty() {
                        ui.weak("no log output yet");
                    }
                    for (line, added_at) in &lines {
                        // A new line eases in over the fade window: an ease-out
                        // cubic from its arrival instant, so the line appears
                        // quickly then settles gently rather than popping in.
                        // Older lines (and everything present when the panel
                        // opened) are already fully opaque.
                        let t = (added_at.elapsed().as_secs_f32() / LOG_FADE_SECS).clamp(0.0, 1.0);
                        let alpha = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
                        let base = log_line_color(line, default);
                        let color = egui::Color32::from_rgba_unmultiplied(
                            base.r(),
                            base.g(),
                            base.b(),
                            (base.a() as f32 * alpha) as u8,
                        );
                        // A faint hover highlight, so a line under the cursor is
                        // easy to track in a dense log. The line's rect is
                        // estimated from the current cursor and the text's natural
                        // height; the highlight is painted before the content, so
                        // the text and chip sit on top of it.
                        let line_rect = egui::Rect::from_min_size(
                            ui.cursor().min,
                            egui::vec2(ui.available_width(), 16.0),
                        );
                        if pointer_over(ui, line_rect) {
                            ui.painter().rect_filled(
                                line_rect,
                                egui::CornerRadius::same(3),
                                egui::Color32::from_white_alpha(12),
                            );
                        }
                        // A level chip (ERR/WARN) leads the line when one applies,
                        // so a problem pops out of the wall of grey log text. The
                        // chip fades in with the line (the same `alpha`).
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            log_level_chip(ui, line, alpha);
                            ui.label(
                                egui::RichText::new(line)
                                    .monospace()
                                    .size(12.0)
                                    .color(color),
                            );
                        });
                    }
                });
        });
        // Repaint while expanded so streaming logs appear without a storm.
        ui.ctx().request_repaint_after(REPAINT);
        // The height slide and the line fade-in both want display-rate frames, not
        // the 250 ms session cadence, so either in flight gets its own
        // as-soon-as-possible repaint.
        if log_moving || any_fading {
            ui.ctx().request_repaint();
        }
    }

    /// The floating status pill ("Copied ✓", "Relay refreshed ✓", …), anchored
    /// bottom-centre.
    ///
    /// Drawn in its own foreground [`egui::Area`] rather than inline, so confirming a
    /// copy or a relay/ticket action never nudges the WHIP URL box, the ticket box or
    /// the stats panel. A copy toast (`copied`) takes precedence over an action toast
    /// (`toast`) when both are momentarily live. The repaint guard in [`App::draw`]
    /// keeps frames coming while either is up and expires them, so the fade completes
    /// exactly as repaints stop and no half-faded pill is left on screen.
    fn toast_overlay(&self, ui: &Ui) {
        // `App::draw` already expired both for this frame, so if either is still set
        // it is live; the expiry threshold lives in one place there.
        let (text, at) = match (&self.copied, &self.toast) {
            (Some((_, at)), _) => ("Copied ✓", *at),
            (None, Some((msg, at))) => (msg.as_str(), *at),
            (None, None) => return,
        };
        let elapsed = at.elapsed();
        // Fade the whole pill out over its remaining lifetime.
        let alpha = (1.0 - elapsed.as_secs_f32() / TOAST.as_secs_f32()).clamp(0.0, 1.0);
        // The pill also rises 8 px as it appears: an ease-out over the first 200 ms,
        // so the confirmation feels placed rather than stamped in place.
        let enter = (elapsed.as_secs_f32() / 0.2).clamp(0.0, 1.0);
        let rise = (1.0 - enter * enter) * 8.0;
        let frame = egui::Frame::popup(ui.style())
            .fill(egui::Color32::from_rgb(30, 42, 36).gamma_multiply(alpha))
            .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(alpha)))
            .inner_margin(egui::Margin::symmetric(14, 6));
        egui::Area::new(egui::Id::new("copy_toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -24.0 + rise])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ui.ctx(), |ui| {
                frame.show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(text)
                            .strong()
                            .color(ACCENT.gamma_multiply(alpha)),
                    );
                });
            });
    }
}

impl App {
    /// Lay out the whole window.
    ///
    /// Split out of [`eframe::App::ui`], which only forwards here, so the real
    /// frame can be driven headlessly by tests without a window or GL context.
    fn draw(&mut self, ui: &mut Ui) {
        // Expire the copy toast before deciding on the next frame. A click later in
        // this same frame re-arms `copied` with a fresh `Instant`, so expiring here
        // never races a click.
        self.copied = expire_copied(self.copied, TOAST);
        self.toast = self.toast.take().filter(|(_, at)| at.elapsed() < TOAST);

        let (broadcast, watch, refreshing) = {
            let g = self.state.lock().unwrap();
            (g.broadcast, g.watch, g.refreshing)
        };
        let any_active = broadcast.active() || watch.active();
        // Reset the eased stats display the moment a broadcast goes idle, so the next
        // broadcast starts from a clean zero instead of counting down from the
        // previous session's values. Only the broadcast track feeds the stats panel.
        if !broadcast.active() {
            self.disp_viewers = 0.0;
            self.disp_video_kbps = 0.0;
            self.disp_video_fps = 0.0;
            self.disp_audio_kbps = 0.0;
        }
        // Pick up any completion notice a background task left (relay refresh or
        // ticket reset) and raise it as a toast. Taken under the same brief lock that
        // reads the modes/refreshing.
        let notice = self.state.lock().unwrap().notice.take();
        if let Some(msg) = notice {
            self.toast = Some((msg, Instant::now()));
        }
        // Slide the tab underline toward the selected tab. The tween runs on wall
        // clock so it advances at the display's frame rate, and it only ever moves
        // right after a tab switch (which itself repaints). It stops requesting
        // frames the moment it settles, so an idle window still falls asleep.
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().clamp(0.0, 0.1);
        self.last_frame = now;
        let target = f32::from(self.tab == Tab::Broadcast);
        let tab_moving = (self.tab_slide - target).abs() > 0.002;
        if tab_moving {
            self.tab_slide = tab_slide_step(self.tab_slide, target, dt);
        }
        // Fade the tab content in after a switch: an ease-out over ~160 ms from the
        // switch instant. [`App::switch_tab`] reset `tab_fade` to 0.0 and armed this;
        // the tween advances it toward 1.0 and requests frames only while it is in
        // flight, so an idle window falls asleep the moment the fade settles (and the
        // overlay is a no-op at full alpha, so no painting lingers either).
        if self.tab_fade_active {
            let (eased, settled) =
                tab_fade_step((now - self.tab_switched_at).as_secs_f32(), TAB_FADE_SECS);
            self.tab_fade = eased;
            self.tab_fade_active = !settled;
        }
        // Persist a ticket minted while idle (via Refresh Relay / Reset Ticket) so the
        // saved "current ticket" survives restarts. Guarded to a real change so we
        // never rewrite config.json every frame. The live broadcast path persists its
        // own ticket in the broadcast tab, so this only runs when not broadcasting.
        if !broadcast.active() {
            let g = self.state.lock().unwrap();
            if let Some(t) = g.ticket.as_ref() {
                if Some(t) != self.config.broadcast_ticket.as_ref() {
                    self.config.broadcast_ticket = Some(t.clone());
                    drop(g);
                    self.config.save();
                }
            }
        }
        // Repaint while either track is live, or while a refresh is in flight, so
        // controls re-enable and the new ticket appears without input.
        if any_active || refreshing {
            ui.ctx().request_repaint_after(REPAINT);
        }
        // A fading toast is an animation, not a status poll: on the 250 ms session
        // cadence a 1.5 s fade only gets six frames, so the pill visibly steps down
        // through six greys instead of dissolving. While either is up, ask for the
        // next frame at ~60 fps instead — `request_repaint_after` keeps the shortest
        // duration asked for, so this wins over the session request just above. The
        // expiry at the top of `draw` disarms the guard as soon as the fade is over,
        // so an idle window still falls asleep (see the regression test for that).
        if self.copied.is_some() || self.toast.is_some() {
            ui.ctx().request_repaint_after(TOAST_FRAME);
        }
        // The underline slide wants frames at the display rate, not the 250 ms
        // session cadence, so it gets its own as-soon-as-possible repaint.
        if tab_moving {
            ui.ctx().request_repaint();
        }
        // The tab content fade-in wants display-rate frames too, but only while it is
        // in flight; the moment it settles the guard disarms and an idle window sleeps.
        if self.tab_fade_active {
            ui.ctx().request_repaint();
        }
        // The connecting/starting spinner rotates on the wall clock, so while it is
        // on screen (the active tab's track is `Starting`) it wants frames at the
        // display rate too. Only the visible tab's spinner matters: the other tab's
        // `Starting` track is not drawn, so it must not keep the window awake.
        let spinner_visible = match self.tab {
            Tab::Broadcast => broadcast == Mode::Starting,
            Tab::Watch => watch == Mode::Starting,
        };
        if spinner_visible {
            ui.ctx().request_repaint();
        }

        // On close, abort any live session (frees the WHIP port, kills the
        // player) and persist config. The endpoint and runtime are dropped in
        // `run` after `run_native` returns.
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            state::stop_all(&self.state, ui.ctx());
            self.config.save();
        }

        // Global keyboard shortcuts, consumed here so egui's default focus
        // traversal doesn't also fire on the same press. Ctrl/Cmd+, toggles the
        // Settings overlay; Ctrl/Cmd+L toggles the log panel between closed (height
        // 0) and half the window; while Settings is closed, Tab cycles the
        // Watch/Broadcast tabs.
        let ctx = ui.ctx();
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.show_options = !self.show_options;
            ctx.request_repaint();
        } else if ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::CTRL, egui::Key::L)
                || i.consume_key(egui::Modifiers::COMMAND, egui::Key::L)
        }) {
            // Toggle the log panel between fully closed (height 0) and half the
            // window height.
            self.log_open = !self.log_open;
            ctx.request_repaint();
        } else if !self.show_options
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
        {
            self.switch_tab(match self.tab {
                Tab::Watch => Tab::Broadcast,
                Tab::Broadcast => Tab::Watch,
            });
            // egui armed a focus traversal (`focus_direction = Next`) from this Tab
            // press before `draw` ran, so cancelling it here stops the first widget
            // (the menu-bar Settings button) from stealing keyboard focus and showing
            // its focus ring. Tab now only switches tabs.
            ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
            ctx.request_repaint();
        }

        // The status/settings bar lives at the very bottom edge. Registered
        // before the `log` bottom panel, so the log stacks above it. A slightly
        // darker fill and a faint top hairline make it read as a deliberate status
        // bar rather than a strip of the same background as the content above.
        // The bar's fill is tinted by the live session's accent (green while
        // broadcasting, blue while watching, warm while a session is coming up),
        // mirroring the status dot's colour, so the whole bar carries the session's
        // state at a glance. Idle keeps the plain dark ground.
        let bar_tint = if broadcast == Mode::Broadcasting {
            Some(ACCENT)
        } else if watch == Mode::Watching {
            Some(WATCH_ACCENT)
        } else if any_active {
            Some(egui::Color32::YELLOW)
        } else {
            None
        };
        let bar_frame = egui::Frame::side_top_panel(ui.style()).fill(status_bar_fill(
            egui::Color32::from_rgb(19, 21, 25),
            bar_tint,
        ));
        // The frame's top inner margin, so the hairline can sit exactly on the
        // bar's top edge (the boundary with the content above).
        let bar_top_margin = bar_frame.inner_margin.topf();
        egui::Panel::bottom("menu_bar")
            .frame(bar_frame)
            .show(ui, |ui| {
                // A faint hairline across the top of the bar, at the boundary with
                // the content above. Painted on its own full-width layer so it is
                // not clipped to the bar's content rect (which is inset by the
                // frame's inner margin).
                let boundary = ui.max_rect().min.y - bar_top_margin;
                let window = ui.ctx().viewport_rect();
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Middle,
                    egui::Id::new("status_bar_hairline"),
                ));
                painter.hline(
                    window.min.x..=window.max.x,
                    boundary,
                    egui::Stroke::new(1.0, egui::Color32::from_white_alpha(16)),
                );
                self.menu_bar(ui);
            });
        if self.show_options {
            self.options_window(ui.ctx());
        }
        self.log_panel(ui);
        // A subtle vertical gradient is the central panel's ground: the panel's own
        // fill is cleared, and the gradient is what the eye reads as the window.
        // Slightly lighter at the top, slightly darker at the bottom, so the surface
        // reads as lit from above rather than a flat fill.
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(ui.style())
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                // The ground gradient is painted on the *same* layer as the panel
                // content (via `ui.painter()`) and *before* any content is added, so
                // it is always behind the content: shapes in one paint list draw in
                // the order they are added. It must not be painted on a separate
                // background layer — in egui 0.36 the panel content itself already
                // lives on the background order (the root UI's layer), so a second
                // background layer has no guaranteed order relative to the content
                // and can land on top of it, painting the whole panel blank.
                // `ui.clip_rect()` is the panel's full visible rect (the frame's
                // inner margin only insets the layout, not the clip), so the ground
                // covers the whole panel, margin included.
                central_gradient(ui.painter(), ui.clip_rect(), 1.0);
                // A soft ambient "light from above" glow at the top centre, behind the
                // brand title and tab strip (the same lit-from-above cue as the ground
                // gradient and the top bevels). Painted before any content, so it sits
                // behind it on the same paint list.
                ambient_glow(ui.painter(), ui.clip_rect());
                // A subtle radial vignette behind the content, so the eye is gently
                // drawn to the centre of the panel (the same depth cue as the ground
                // gradient and the ambient glow). Painted before any content, so it
                // sits behind it on the same paint list.
                vignette(ui.painter(), ui.clip_rect());
                self.tab_bar(ui);
                ui.add_space(8.0);
                // The top of the tab content, so the fade-in overlay covers only the
                // content (below the tab strip), not the brand title and tabs.
                let content_top = ui.cursor().min.y;
                match self.tab {
                    Tab::Watch => self.watch_tab(ui),
                    Tab::Broadcast => self.broadcast_tab(ui),
                }
                // Fade the new tab's content in after a switch: an overlay matching
                // the ground, fading out over ~160 ms. Painted after the content (on
                // the same layer) so it covers it while fading. A no-op at full
                // alpha, so an idle window paints nothing extra.
                if self.tab_fade < 1.0 {
                    let overlay = egui::Rect::from_min_max(
                        egui::pos2(ui.max_rect().min.x, content_top),
                        ui.max_rect().max,
                    );
                    central_gradient(ui.painter(), overlay, 1.0 - self.tab_fade);
                }
            });
        // Drawn last, on its own foreground layer, so it floats over the panels
        // without taking part in their layout.
        self.toast_overlay(ui);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

/// Drop the copy toast once its window has closed.
///
/// `copied` is what arms the repaint guard in [`App::draw`] after a session has
/// gone idle, so it has to expire: before the fix it was set on the first copy
/// and never cleared, and the app asked for a repaint every tick forever.
/// Extracted (with the window length injected) so that regression is testable
/// without a live window.
fn expire_copied(
    copied: Option<(&'static str, Instant)>,
    toast: Duration,
) -> Option<(&'static str, Instant)> {
    copied.filter(|(_, at)| at.elapsed() < toast)
}

/// Truncate a string to `max` chars, adding an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// The status-dot breath: a shallow ~2-second pulse, brightness bounded to
/// `0.6..=1.0` of the base colour so "live" reads at a glance without blinking
/// out. Mirrors the viewers cell in the broadcast tab.
fn pulse_factor(secs: f32) -> f32 {
    0.6 + 0.4 * (0.5 + 0.5 * (secs * std::f32::consts::TAU * 0.5).cos())
}

/// The copy-box click flash: a linear fade from `1.0` to `0.0` over `window`,
/// clamped outside it. Drives a brief brighten of the box fill and glyph.
fn flash_alpha(elapsed: Duration, window: Duration) -> f32 {
    if window.is_zero() {
        return 0.0;
    }
    (1.0 - elapsed.as_secs_f32() / window.as_secs_f32()).clamp(0.0, 1.0)
}

/// A small rotating arc spinner, shown beside "connecting…" / "starting…" while a
/// session is being brought up, so the waiting moment has visible progress instead
/// of a static label.
///
/// The arc is a 270° ring segment drawn as a mesh of small quads (a crisp,
/// controlled look that does not depend on line-cap styling) and rotated with
/// `secs` (one revolution every ~1.2 s). Purely decorative: it takes a hover-sense
/// slot but is never clicked. `color` is the track's accent (green for a broadcast
/// coming up, blue for a watch connecting).
fn spinner(ui: &mut Ui, secs: f32, color: egui::Color32) {
    let size = 14.0;
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let center = rect.center();
    // A 1.5 px margin keeps the ring inside its slot; a 2 px-thick ring.
    let outer = size * 0.5 - 1.5;
    let inner = outer - 2.0;
    let span = 0.75 * std::f32::consts::TAU; // a 270° arc
    let start = (secs / 1.2) * std::f32::consts::TAU; // one revolution every 1.2 s
    let n = 24;
    let mut mesh = egui::epaint::Mesh::default();
    for i in 0..n {
        let a0 = start + span * i as f32 / n as f32;
        let a1 = start + span * (i + 1) as f32 / n as f32;
        // Four corners of the i-th ring quad. `colored_vertex` pushes in order, so
        // this quad's vertices are at `4*i .. 4*i+4`.
        mesh.colored_vertex(
            center + egui::vec2(a0.cos() * outer, a0.sin() * outer),
            color,
        );
        mesh.colored_vertex(
            center + egui::vec2(a1.cos() * outer, a1.sin() * outer),
            color,
        );
        mesh.colored_vertex(
            center + egui::vec2(a1.cos() * inner, a1.sin() * inner),
            color,
        );
        mesh.colored_vertex(
            center + egui::vec2(a0.cos() * inner, a0.sin() * inner),
            color,
        );
        let v = 4 * i as u32;
        mesh.add_triangle(v, v + 1, v + 2);
        mesh.add_triangle(v, v + 2, v + 3);
    }
    ui.painter().add(egui::Shape::mesh(mesh));
}

/// A faint 1 px "light catch" along the top edge of a card surface: a bright
/// line just inside the top, so the surface reads as catching light from above
/// rather than a flat tint. Inset by `corner_radius` at each end so it never
/// clips the rounded corners. Purely decorative: it takes no layout space and is
/// painted behind whatever the caller draws next (the copy glyph, a hover stroke).
fn top_bevel(painter: &egui::Painter, rect: egui::Rect, corner_radius: u8) {
    let inset = f32::from(corner_radius);
    let left = rect.min.x + inset;
    let right = rect.max.x - inset;
    if right <= left {
        return;
    }
    painter.hline(
        left..=right,
        rect.min.y + 0.5,
        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(24)),
    );
}

/// A very subtle vertical gradient for the central panel's background: slightly
/// lighter at the top, slightly darker at the bottom, so the window reads as a
/// surface lit from above rather than a flat fill. The two endpoints average to
/// the panel fill, so it is a depth cue, not a recolor.
///
/// The ground gradient is painted with the panel content's own painter, *before*
/// any content is added, so it sits behind the content on the same paint list. It
/// must not be painted on a separate background layer: in egui 0.36 the panel
/// content already lives on the background order (the root UI's layer), so a
/// second background layer has no guaranteed order relative to the content and
/// can land on top of it, painting the whole panel blank.
///
/// `alpha` scales the whole gradient's opacity: at 1.0 it is the ground itself; at
/// a lower value it is the tab fade-in's reveal overlay (painted after the content,
/// so it covers it while fading), which matches the ground so the fade is seamless.
fn central_gradient(painter: &egui::Painter, rect: egui::Rect, alpha: f32) {
    let top = egui::Color32::from_rgb(27, 29, 34).gamma_multiply(alpha);
    let bottom = egui::Color32::from_rgb(21, 23, 27).gamma_multiply(alpha);
    let mut mesh = egui::epaint::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// A subtle ambient "light from above" glow at the top centre of the central panel:
/// a few concentric, semi-transparent circles whose overlap brightens the centre,
/// so the brand title and tab strip read as lit from above (the same cue as the
/// ground gradient and the top bevels). Painted on the panel's own painter, *before*
/// any content is added, so it sits behind the content (the same rule as
/// [`central_gradient`]). The fill is a slightly lighter shade of the ground at a
/// very low alpha (not a `gamma_multiply` of a dark colour, which would darken the
/// top rather than light it): a depth cue, not a recolor.
fn ambient_glow(painter: &egui::Painter, rect: egui::Rect) {
    let center = egui::pos2(rect.center().x, rect.top());
    // A slightly lighter shade of the ground (the ground's top is ~27,29,34), so the
    // glow reads as light from above rather than a darkening.
    let color = egui::Color32::from_rgba_unmultiplied(58, 64, 74, 3);
    // Six concentric circles, each larger than the last, at the same low alpha. The
    // overlap brightens the centre (where all six coincide) and fades out radially
    // (where only the outermost reach), so the result reads as a soft radial glow
    // rather than a set of rings.
    for radius in [40.0, 65.0, 90.0, 115.0, 140.0, 165.0] {
        painter.circle_filled(center, radius, color);
    }
}

/// A subtle radial vignette: a darkening that is transparent at the centre and
/// fades in toward the edges, so the eye is gently drawn to the centre of the panel
/// (the same depth / "lit from above" language as the ground gradient and the
/// ambient glow). The falloff is radial from the centre, so the corners (the farthest
/// points) are the darkest and the mid-edges are lighter — a proper vignette, not a
/// flat border. Painted on the panel's own painter, *before* any content is added, so
/// it sits behind the content (the same rule as [`central_gradient`]). A depth cue,
/// not a recolor: the edge alpha is very low.
fn vignette(painter: &egui::Painter, rect: egui::Rect) {
    let center = rect.center();
    // The radius reaches the farthest corner, so the whole panel is covered (the
    // mid-edges sit inside the circle and read lighter than the corners).
    let radius = (rect.size() * 0.5).length();
    if radius <= 0.0 {
        return;
    }
    // A dark neutral, transparent at the centre and faint at the edges.
    let clear = egui::Color32::from_rgba_unmultiplied(12, 14, 18, 0);
    let edge = egui::Color32::from_rgba_unmultiplied(12, 14, 18, 16);
    let n = 32;
    let mut mesh = egui::epaint::Mesh::default();
    // The centre vertex (transparent).
    mesh.colored_vertex(center, clear);
    // The outer ring vertices (the faint edge), one per segment.
    for i in 0..n {
        let a = std::f32::consts::TAU * i as f32 / n as f32;
        let p = center + egui::vec2(a.cos() * radius, a.sin() * radius);
        mesh.colored_vertex(p, edge);
    }
    // A fan: one triangle from the centre to each outer segment, wrapping the last
    // segment back to the first outer vertex.
    for i in 0..n {
        let a = 1 + i as u32;
        let b = 1 + ((i + 1) % n) as u32;
        mesh.add_triangle(0, a, b);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// A soft halo behind the primary action button: three stacked rounded rects that
/// fall off from the button's edge (nearest strongest, farthest faintest), so the
/// hero action reads as lit by `color` rather than a flat button.
///
/// The caller paints this *before* adding the button, so the button (added next,
/// on top) covers the halo's centre and only the rim around the button's edge
/// shows. `color` is the action's accent: the live green for a Start/Watch, a
/// muted red for a Stop, so the halo's hue matches the button's intent.
///
/// `hover` lifts the halo: the alphas roughly double, so the hero action reads as
/// lit brighter under the mouse. `pressed` contracts it: the alphas drop below the
/// resting level and the halo tightens, so the hero action reads as physically
/// pressed in rather than flat. The hero buttons use a fixed `.fill()`, which
/// suppresses egui's own hover feedback, so the glow carries the hover and press
/// response.
fn primary_button_glow(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    hover: bool,
    pressed: bool,
) {
    let corner = egui::CornerRadius::same(10);
    // A pressed halo is dimmer than even the resting halo, so the button reads as
    // pushed in. A hover is the brightest. The resting state is the baseline.
    let (a1, a2, a3) = if pressed {
        (0.01, 0.03, 0.06)
    } else if hover {
        (0.04, 0.09, 0.16)
    } else {
        (0.02, 0.05, 0.09)
    };
    for (expand, alpha) in [(6.0, a1), (4.0, a2), (2.0, a3)] {
        painter.rect_filled(rect.expand(expand), corner, color.gamma_multiply(alpha));
    }
}

/// A soft accent glow around a secondary interactive surface: two stacked rounded
/// rects that fall off from the edge (nearest strongest, farthest faintest). The
/// shared halo language for the copy box's hover and the ticket/buffer boxes' focus,
/// so both read as lit by the accent under the same cue. Dimmer than the hero
/// buttons' halo: the surfaces it lights are secondary, not the hero action.
/// Purely decorative: it takes no layout space. `corner` matches the surface's own
/// corner radius so the glow hugs its rounded outline, and `color` is the accent the
/// surface is lit by (the live green for a broadcast surface, the watch blue for a
/// watch surface), so the glow's hue matches the surface's own accent.
pub(crate) fn soft_glow(
    painter: &egui::Painter,
    rect: egui::Rect,
    corner: egui::CornerRadius,
    color: egui::Color32,
) {
    for (expand, alpha) in [(4.0, 0.03), (2.0, 0.06)] {
        painter.rect_filled(rect.expand(expand), corner, color.gamma_multiply(alpha));
    }
}

/// The status bar's fill, tinted by the live session's accent so the bar carries
/// the session's state at a glance: a green cast while broadcasting, a blue cast
/// while watching, a warm cast while a session is coming up, and the plain dark
/// ground when idle. The tint is blended in at a low alpha (6%), so it reads as a
/// subtle state cue rather than a recolor; when there is no tint the base fill is
/// returned unchanged, so the bar's resting look is byte-identical to before.
fn status_bar_fill(base: egui::Color32, tint: Option<egui::Color32>) -> egui::Color32 {
    let Some(t) = tint else {
        return base;
    };
    let a = 0.06;
    let mix = |b: u8, t: u8| -> u8 { (b as f32 * (1.0 - a) + t as f32 * a).round() as u8 };
    egui::Color32::from_rgb(
        mix(base.r(), t.r()),
        mix(base.g(), t.g()),
        mix(base.b(), t.b()),
    )
}

/// Whether the pointer is over `rect` this frame, for a hand-painted widget's hover
/// response. The hero buttons use a fixed `.fill()`, which suppresses egui's own
/// hover feedback, so the hover is read from the pointer position instead.
fn pointer_over(ui: &Ui, rect: egui::Rect) -> bool {
    ui.ctx()
        .input(|i| i.pointer.latest_pos())
        .is_some_and(|p| rect.contains(p))
}

/// Whether the primary pointer button is pressed over `rect` this frame, for a
/// hand-painted widget's press response. The hero buttons use a fixed `.fill()`,
/// which suppresses egui's own hover *and* active feedback, so the press is read
/// from the pointer position and button state instead. A press always implies a
/// hover (the pointer is over the rect), so callers can treat `pressed` as a
/// refinement of `hovered`.
fn pointer_pressed_over(ui: &Ui, rect: egui::Rect) -> bool {
    ui.ctx().input(|i| {
        i.pointer.button_pressed(egui::PointerButton::Primary)
            && i.pointer.latest_pos().is_some_and(|p| rect.contains(p))
    })
}

/// A sonar pulse: a soft ring that expands from a live indicator's dot and fades
/// out, looping on `period` seconds, so "live" reads as an active signal rather than
/// a static glyph. `center` is the dot's centre, `base` its resting radius, `color`
/// the accent, `secs` the animation clock (expected to advance between repaints,
/// which already fire while live). Purely decorative: it paints on the caller's
/// painter and takes no layout space.
fn sonar_pulse(
    painter: &egui::Painter,
    center: egui::Pos2,
    base: f32,
    secs: f32,
    period: f32,
    color: egui::Color32,
) {
    let phase = (secs % period) / period; // 0..1, loops
    let radius = base + phase * base; // expand to 2× the resting radius
    let alpha = (1.0 - phase) * 0.4; // fade out as it expands
    if alpha <= 0.005 {
        return;
    }
    painter.circle_stroke(
        center,
        radius,
        egui::Stroke::new(1.0, color.gamma_multiply(alpha)),
    );
}

/// A compact "LIVE" badge for the status bar: a small outlined pill in the session's
/// accent, so the live state reads at a glance rather than only as a small breathing
/// dot. `color` is the session's accent (green while broadcasting, blue while
/// watching) and `breath` (0.6..=1.0) is the same pulse factor that drives the dot,
/// so the badge and the dot breathe in lockstep. The pill is a stroked rect (not a
/// line segment) and the text is a laid-out galley, so neither disturbs the
/// line-segment-based tests. Purely decorative: it takes a hover-sense slot but is
/// never clicked.
fn live_badge(ui: &mut Ui, color: egui::Color32, breath: f32) {
    let font_id = egui::FontId::proportional(10.0);
    let galley =
        ui.painter()
            .layout_no_wrap("LIVE".to_string(), font_id, egui::Color32::PLACEHOLDER);
    let tw = galley.rect.width();
    let th = galley.rect.height();
    let pad_x = 5.0;
    let pad_y = 2.0;
    let (rect, _resp) = ui.allocate_exact_size(
        egui::vec2(tw + pad_x * 2.0, th + pad_y * 2.0),
        egui::Sense::hover(),
    );
    // A faint fill and a breathing outline in the session's accent: the outline
    // carries the signal, the fill keeps it from shouting.
    let corner = egui::CornerRadius::same(4);
    ui.painter()
        .rect_filled(rect, corner, color.gamma_multiply(0.10));
    ui.painter().rect_stroke(
        rect,
        corner,
        egui::Stroke::new(1.0, color.gamma_multiply(breath)),
        egui::StrokeKind::Inside,
    );
    // Centre the text in the pill.
    let text_pos = egui::pos2(rect.center().x - tw * 0.5, rect.center().y - th * 0.5);
    ui.painter()
        .galley(text_pos, galley, color.gamma_multiply(breath.max(0.8)));
}

/// A section heading: a small accent-coloured rounded bar to the left of a bold
/// label, so the label reads as a deliberate section marker rather than a plain
/// strong label. The bar and label sit in a horizontal row, so the bar sits neatly
/// beside the text. `color` is the section's accent (the live green for a broadcast
/// section, the watch blue for a watch section), echoing the two-accent design
/// language used across the app.
fn section_tick(ui: &mut Ui, text: &str, color: egui::Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (bar, _) = ui.allocate_exact_size(egui::vec2(3.0, 12.0), egui::Sense::hover());
        // Nudge the bar down a couple of pixels so it centres on the label's text: the
        // row is top-aligned, so an un-nudged bar would sit a touch high relative to the
        // label. The nudge stays within the row's height, so it never overflows.
        ui.painter().rect_filled(
            bar.translate(egui::vec2(0.0, 2.0)),
            egui::CornerRadius::same(1),
            color,
        );
        ui.label(egui::RichText::new(text).strong());
    });
}

/// A soft error callout: a muted red-tinted rounded surface with a red left bar
/// (the section-tick's language, in the error hue) and the message in the error
/// red, so a failure reads as a deliberate surface rather than a stray line of
/// text. The tint is dark enough to recede against the ground; the bar and the
/// text carry the signal. Long messages wrap within the surface.
fn error_callout(ui: &mut Ui, text: &str) {
    let frame = egui::Frame::group(ui.style())
        .fill(ERROR.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, ERROR.gamma_multiply(0.35)))
        .inner_margin(egui::Margin::symmetric(10, 8));
    let inner = frame.show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            // The left bar, centred on the first line's text (the same nudge as
            // [`section_tick`], which it mirrors in the error hue).
            let (bar, _) = ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
            ui.painter().rect_filled(
                bar.translate(egui::vec2(0.0, 1.0)),
                egui::CornerRadius::same(1),
                ERROR,
            );
            ui.label(egui::RichText::new(text).color(ERROR));
        });
    });
    // The same light catch the copy boxes and stats panel carry, so the callout
    // reads as a surface lit from above like the rest of the app.
    top_bevel(ui.painter(), inner.response.rect, frame.corner_radius.nw);
}

/// The display colour of a log line, by its level: tracing's default format is
/// `LEVEL target: message`, so the level is the line's first word. Errors read in
/// the app's error red, warnings in a muted amber, debug and trace recede to a dim
/// grey, and everything else (info, and lines without a level prefix) takes the
/// normal text colour.
fn log_line_color(line: &str, default: egui::Color32) -> egui::Color32 {
    match line.split_whitespace().next() {
        Some("ERROR") => ERROR,
        Some("WARN") => WARN_AMBER,
        Some("DEBUG") | Some("TRACE") => egui::Color32::from_rgb(122, 126, 132),
        _ => default,
    }
}

/// The muted amber a `WARN` line is coloured with (shared by [`log_line_color`] and
/// [`log_level_chip`], so a warning's text and its chip always match).
const WARN_AMBER: egui::Color32 = egui::Color32::from_rgb(214, 178, 94);

/// A small level chip for a log line: a compact rounded tag in the line's level
/// colour ("ERR" in the error red, "WARN" in the muted amber), so an error or warning
/// pops out of a wall of grey log text instead of reading as just another line. INFO
/// and level-less lines get no chip. `alpha` fades the chip in with the line it tags
/// (the same fade window the line uses), so the chip and the line appear together.
/// Returns `true` if a chip was painted. The chip is a stroked rect plus a laid-out
/// galley (no line segments), so it never disturbs the line-segment-based tests.
fn log_level_chip(ui: &mut Ui, line: &str, alpha: f32) -> bool {
    let (text, color) = match line.split_whitespace().next() {
        Some("ERROR") => ("ERR", ERROR),
        Some("WARN") => ("WARN", WARN_AMBER),
        _ => return false,
    };
    // A faint fill and a steady outline in the level colour; the text is near-full
    // strength so the tag is legible. All three scale with `alpha` so the chip fades
    // in with its line. The text is laid out with its own (non-placeholder) colour,
    // so the galley carries the chip's hue.
    let corner = egui::CornerRadius::same(3);
    let fill = egui::Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (26.0 * alpha) as u8,
    );
    let stroke = egui::Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (120.0 * alpha) as u8,
    );
    let text_color = egui::Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (230.0 * alpha) as u8,
    );
    let font_id = egui::FontId::monospace(10.0);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font_id, text_color);
    let pad_x = 4.0;
    let pad_y = 1.5;
    let (rect, _resp) = ui.allocate_exact_size(
        egui::vec2(
            galley.rect.width() + pad_x * 2.0,
            galley.rect.height() + pad_y * 2.0,
        ),
        egui::Sense::hover(),
    );
    ui.painter().rect_filled(rect, corner, fill);
    ui.painter().rect_stroke(
        rect,
        corner,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    let text_pos = egui::pos2(
        rect.center().x - galley.rect.width() * 0.5,
        rect.center().y - galley.rect.height() * 0.5,
    );
    ui.painter().galley(text_pos, galley, text_color);
    true
}

/// A small hand-painted "×" for the log panel's header: two crossing strokes in a
/// dim grey that brightens on hover, so the panel can be dismissed without knowing
/// the Ctrl/Cmd+L shortcut. Hand-painted (rather than a text button) so it does not
/// depend on a glyph the bundled faces may not carry — the same reason the copy
/// box's glyph is drawn by hand.
fn close_button(ui: &mut Ui) -> bool {
    let size = 16.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if hovered {
        egui::Color32::from_rgb(200, 200, 205)
    } else {
        egui::Color32::from_rgb(130, 133, 140)
    };
    // A 4 px inset keeps the strokes inside the slot; two diagonals cross at the
    // slot's centre.
    let m = 4.0;
    ui.painter().line_segment(
        [
            egui::pos2(rect.min.x + m, rect.min.y + m),
            egui::pos2(rect.max.x - m, rect.max.y - m),
        ],
        egui::Stroke::new(1.5, color),
    );
    ui.painter().line_segment(
        [
            egui::pos2(rect.min.x + m, rect.max.y - m),
            egui::pos2(rect.max.x - m, rect.min.y + m),
        ],
        egui::Stroke::new(1.5, color),
    );
    resp.clicked()
}

/// One step of the tab underline's exponential ease: `current` moves toward
/// `target` by a fraction of the remaining distance that grows with `dt`
/// (wall-clock seconds), so the slide's speed is independent of frame rate.
///
/// A step never overshoots, and once within `0.002` of the target (about a
/// pixel at the widest window) it snaps on, so the caller can stop requesting
/// repaints and an idle window falls asleep again.
fn tab_slide_step(current: f32, target: f32, dt: f32) -> f32 {
    let next = current + (target - current) * (1.0 - (-dt * 30.0).exp());
    if (next - target).abs() <= 0.002 {
        target
    } else {
        next
    }
}

/// One step of the log panel's height tween: `current` moves toward `target` (the
/// open fraction, 0.0 closed / 1.0 open) by a fraction of the remaining distance
/// that grows with `dt` (wall-clock seconds), so the slide's speed is independent
/// of frame rate. A step never overshoots, and once within `0.002` of the target it
/// snaps on, so the caller can stop requesting repaints and an idle window falls
/// asleep again. A touch slower than the tab underline (`30.0` vs `18.0`), so the
/// larger travel (up to half the window) reads as a deliberate slide rather than a
/// snap.
fn log_anim_step(current: f32, target: f32, dt: f32) -> f32 {
    let next = current + (target - current) * (1.0 - (-dt * 18.0).exp());
    if (next - target).abs() <= 0.002 {
        target
    } else {
        next
    }
}

/// One step of the live stats' eased display: `current` moves toward `target` by a
/// fraction of the remaining distance that grows with `dt` (wall-clock seconds), so
/// the settle's speed is independent of frame rate. A step never overshoots, and once
/// within a small epsilon of the target it snaps on, so the caller can stop requesting
/// repaints the moment the numbers settle. A touch slower than the tab underline, so a
/// bitrate change reads as a gentle settle rather than a snap. The epsilon (0.05) is
/// well below one unit of every displayed stat (kbit/s, fps, viewers), so the snap is
/// never visible.
fn stat_ease(current: f32, target: f32, dt: f32) -> f32 {
    let next = current + (target - current) * (1.0 - (-dt * 8.0).exp());
    if (next - target).abs() <= 0.05 {
        target
    } else {
        next
    }
}

/// One step of the tab content's fade-in: the eased alpha after `elapsed` seconds
/// from the switch, and whether the fade has settled. An ease-out cubic over
/// `duration`, so the new content appears quickly then settles gently. Pure (the
/// clock is injected) so the settle is testable without a live window.
fn tab_fade_step(elapsed: f32, duration: f32) -> (f32, bool) {
    let t = (elapsed / duration).clamp(0.0, 1.0);
    if t >= 1.0 {
        (1.0, true)
    } else {
        (1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t), false)
    }
}

/// Install the app's tuned dark theme once, before the first frame.
///
/// egui ships an OS-following stock palette; this pins a deliberate dark look
/// with an accent-tinted selection/hover so the app does not depend on the host
/// OS theme. `all_styles_mut` retunes both the dark and light variants, so the
/// palette stays coherent even if the theme preference is ever switched, while
/// `set_theme` makes dark the deterministic default across platforms. Deltas are
/// kept small: a slightly deeper neutral ground, an accent selection, and gentle
/// widget and spacing bumps.
fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        // A touch deeper than egui's stock dark so panels read as deliberate.
        v.panel_fill = egui::Color32::from_rgb(24, 26, 30);
        v.window_fill = egui::Color32::from_rgb(30, 33, 38);
        v.faint_bg_color = egui::Color32::from_rgb(38, 42, 48);
        // Accent-tinted selection and hover, reused by the copy-box stroke.
        v.selection.bg_fill = ACCENT.gamma_multiply(0.28);
        v.selection.stroke.color = ACCENT;
        // Nudge the interactive surfaces toward the accent without shouting.
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(44, 48, 54);
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(44, 48, 54);
        v.widgets.hovered.weak_bg_fill = ACCENT.gamma_multiply(0.16);
        v.widgets.hovered.bg_fill = ACCENT.gamma_multiply(0.16);
        v.widgets.active.weak_bg_fill = ACCENT.gamma_multiply(0.30);
        v.widgets.active.bg_fill = ACCENT.gamma_multiply(0.30);
        // A little breathing room and softer corners.
        style.spacing.item_spacing = egui::vec2(10.0, 7.0);
        style.spacing.window_margin = egui::Margin::same(14);
        v.window_corner_radius = egui::CornerRadius::same(8);
        v.menu_corner_radius = egui::CornerRadius::same(8);
    });
}

/// The application's window / task-bar icon: a rounded square (the app's dark
/// ground, lit from above) carrying a green "stream" — a thick diagonal conduit
/// with a soft glow and a flow arrow at its tip — so the app has a small brand
/// mark in the title bar and task bar instead of egui's default "e". Generated in
/// code (no asset file), square, with a transparent margin outside the rounded
/// square. The stream runs through the icon's centre and reads as "streaming" at
/// a glance even at task-bar sizes.
fn app_icon() -> Arc<egui::IconData> {
    let size = 64usize;
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let (r, g, b, a) = icon_pixel(x as f32, y as f32, size as f32);
            let i = (y * size + x) * 4;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = a;
        }
    }
    Arc::new(egui::IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    })
}

/// The colour of one icon pixel: the rounded-square ground (a subtle vertical
/// gradient, lit from above), a soft glow around the stream, and the stream itself
/// (a thick diagonal capsule plus a flow arrow) in the live accent. Transparent
/// outside the rounded square.
fn icon_pixel(x: f32, y: f32, size: f32) -> (u8, u8, u8, u8) {
    let (px, py) = (x + 0.5, y + 0.5); // centre the sample in the pixel
    let accent: (f32, f32, f32) = (80.0, 200.0, 120.0); // the live accent (ACCENT)
                                                        // The rounded-square ground.
    let radius = size * 0.22;
    let d_bg = rounded_box_sdf(px, py, size, radius);
    if d_bg > 0.5 {
        return (0, 0, 0, 0); // outside the icon
    }
    let edge = (0.5 - d_bg).clamp(0.0, 1.0); // 1 px anti-aliasing
                                             // A subtle vertical gradient: lighter at the top (lit from above).
    let t = (py / size).clamp(0.0, 1.0);
    let (top, bot) = ((31.0, 34.0, 40.0), (19.0, 21.0, 25.0));
    let mut r = top.0 + (bot.0 - top.0) * t;
    let mut g = top.1 + (bot.1 - top.1) * t;
    let mut b = top.2 + (bot.2 - top.2) * t;
    // The stream: a thick diagonal capsule from lower-left to upper-right.
    let a = (size * 0.30, size * 0.70);
    let tip = (size * 0.70, size * 0.30);
    let half = size * 0.085;
    let glow = size * 0.10;
    let d = dist_to_segment(px, py, a, tip);
    if d < half + glow {
        // A soft glow: strongest at the capsule's edge, fading outward.
        let gt = ((half + glow - d) / glow).clamp(0.0, 1.0);
        let ga = gt * gt * 0.5;
        r = r * (1.0 - ga) + accent.0 * ga;
        g = g * (1.0 - ga) + accent.1 * ga;
        b = b * (1.0 - ga) + accent.2 * ga;
    }
    // The stream's core and its flow arrow: full accent.
    let (dx, dy) = normalize(tip.0 - a.0, tip.1 - a.1);
    let in_core = d <= half;
    let in_arrow = point_in_triangle(
        px,
        py,
        (tip.0 + dx * size * 0.15, tip.1 + dy * size * 0.15), // apex, ahead of the tip
        (tip.0 - dy * size * 0.11, tip.1 + dx * size * 0.11), // base, one side
        (tip.0 + dy * size * 0.11, tip.1 - dx * size * 0.11), // base, other side
    );
    if in_core || in_arrow {
        (r, g, b) = accent;
    }
    (r as u8, g as u8, b as u8, (edge * 255.0) as u8)
}

/// A rounded box's signed distance field (negative inside): the box fills `[0,
/// size]²` with corner radius `radius`.
fn rounded_box_sdf(px: f32, py: f32, size: f32, radius: f32) -> f32 {
    let (qx, qy) = (
        (px - size * 0.5).abs() - (size * 0.5 - radius),
        (py - size * 0.5).abs() - (size * 0.5 - radius),
    );
    (qx.max(0.0) * qx.max(0.0) + qy.max(0.0) * qy.max(0.0)).sqrt() + (qx.min(0.0).max(qy.min(0.0)))
        - radius
}

/// The distance from `(px, py)` to the segment `a..=b` (a capsule's centre line).
fn dist_to_segment(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let t = (((px - a.0) * abx + (py - a.1) * aby) / (abx * abx + aby * aby)).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + abx * t, a.1 + aby * t);
    ((px - cx) * (px - cx) + (py - cy) * (py - cy)).sqrt()
}

/// Whether `(px, py)` is inside the triangle `p1 p2 p3`.
fn point_in_triangle(px: f32, py: f32, p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)) -> bool {
    let sign = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
        (a.0 - c.0) * (b.1 - c.1) - (b.0 - c.0) * (a.1 - c.1)
    };
    let (d1, d2, d3) = (
        sign((px, py), p1, p2),
        sign((px, py), p2, p3),
        sign((px, py), p3, p1),
    );
    let has_neg = (d1 < 0.0) || (d2 < 0.0) || (d3 < 0.0);
    let has_pos = (d1 > 0.0) || (d2 > 0.0) || (d3 > 0.0);
    !(has_neg && has_pos)
}

/// A unit vector (or `(0, 0)` for a zero input).
fn normalize(x: f32, y: f32) -> (f32, f32) {
    let len = (x * x + y * y).sqrt();
    if len <= 0.0 {
        (0.0, 0.0)
    } else {
        (x / len, y / len)
    }
}

/// Open the GUI.
///
/// Creates the background runtime, loads config, installs the tee logger, wires
/// the stats channel and consumer task, and runs eframe until the window closes.
///
/// Two runtimes: the primary one hosts the eframe event loop's async work (the
/// stats consumer that requests repaints), the second one — `streampipe-engine`
/// — hosts the streaming engine. Media must not share threads with the renderer:
/// the WHIP host polls the ICE socket from a worker thread, and if that worker is
/// stalled the OS UDP receive buffer (clamped to a few hundred KB long before the
/// 8 MiB we ask for) overflows during a keyframe burst, which the viewer sees as
/// smearing, `error while decoding MB` and jumping timestamps.
pub fn run() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: unable to start runtime: {e}");
            std::process::exit(1);
        }
    };
    let handle = runtime.handle().clone();

    // The engine runtime: every media session (broadcast and watch) and every
    // task they spawn runs here, on threads no UI repaint or stats consumer
    // ever touches.
    let engine_runtime = match tokio::runtime::Builder::new_multi_thread()
        .thread_name("streampipe-engine")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: unable to start engine runtime: {e}");
            std::process::exit(1);
        }
    };
    let engine_handle = engine_runtime.handle().clone();

    let mut config = Config::load();
    let secret = config.ensure_secret();
    let log = log::new_ring();
    log::install(log.clone(), config.verbosity);

    let whip = state::whip_url(&config.broadcast_host, config.broadcast_port);
    let state = Arc::new(Mutex::new(SharedState::new(secret, whip)));

    let (stats_tx, stats_rx) = mpsc::unbounded_channel::<StreamStats>();
    // Keep one sender alive for the whole session so the consumer task does not
    // see the channel close between broadcast start/stops.
    let keepalive = stats_tx.clone();

    // The app gets the engine handle, so `spawn_broadcast`/`spawn_watch` land on
    // the engine runtime; the stats consumer stays on the GUI runtime with the
    // renderer it asks to repaint.
    let app = App::new(engine_handle, config, state.clone(), log.clone(), stats_tx);
    let consumer_state = state.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 844.0])
            .with_min_inner_size([560.0, 630.0])
            .with_title("streampipe")
            .with_icon(app_icon()),
        ..Default::default()
    };

    let result = eframe::run_native(
        "streampipe",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            apply_theme(&ctx);
            handle.spawn(state::consume_stats(stats_rx, consumer_state, ctx));
            Ok(Box::new(app))
        }),
    );

    // Close the stats channel so the consumer task exits, then give any teardown
    // (e.g. the player kill task) a moment to run before the runtime stops.
    drop(keepalive);
    engine_runtime.shutdown_timeout(Duration::from_millis(1000));
    runtime.shutdown_timeout(Duration::from_millis(1000));

    // A normal window close returns `Ok`. An `Err` means the window/GL context
    // never came up (e.g. no display); report that as a failed launch rather
    // than exiting 0, so launchers and scripts do not see false success.
    if let Err(e) = result {
        eprintln!("error: gui failed: {e:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::config::DEFAULT_VOLUME;
    use super::options::CLOSE_GREY;
    use super::*;

    /// An `App` wired to throwaway state, plus the runtime it borrows a handle
    /// from (kept alive by the caller).
    fn test_app() -> (App, tokio::runtime::Runtime) {
        test_app_with(Config::default())
    }

    /// The same, seeded from an arbitrary config (so a test can check what the app
    /// restores from disk).
    fn test_app_with(config: Config) -> (App, tokio::runtime::Runtime) {
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

    /// Drive real frames of the UI headlessly and report the repaint delay the
    /// viewport settled on. `Duration::MAX` means "do not wake me up".
    ///
    /// egui spends its first couple of passes priming (font texture, deferred
    /// repaint bookkeeping) and reports a zero delay there, so the settled value
    /// is read after three passes.
    fn settled_repaint_delay(app: &mut App) -> Duration {
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
            // There is no backend here to consume the font texture upload, so clear
            // it rather than letting it panic on drop.
            out.textures_delta.clear();
        }
        delay
    }

    /// Regression: `copied` used to be set on the first copy box click and never
    /// cleared, so the guard in [`App::draw`] stayed armed and an idle window
    /// repaints every tick forever.
    /// Assert the viewport has scheduled *some* follow-up repaint. Compared as
    /// "less than `Duration::MAX`" rather than exactly [`REPAINT`]: egui shaves
    /// the time spent inside the passes off the reported delay, and revealing the
    /// toast Area forces one settling frame.
    fn assert_awake(delay: Duration) {
        assert!(
            delay < Duration::MAX,
            "expected a scheduled repaint, got {delay:?}"
        );
        assert!(delay <= REPAINT, "repaints should not lag {REPAINT:?}");
    }

    #[test]
    fn idle_window_falls_asleep_once_the_copy_toast_expires() {
        let (mut app, _rt) = test_app();

        // Idle, nothing ever copied: no repaint is requested at all.
        assert_eq!(settled_repaint_delay(&mut app), Duration::MAX);

        // A click arms the toast, so frames keep coming (even while idle) to fade it.
        app.copied = Some(("whip", Instant::now()));
        assert_awake(settled_repaint_delay(&mut app));

        // The toast window has closed: the idle app must stop asking for frames and
        // clear the flag that armed the guard. This is the regression — before the
        // fix `copied` stayed set, so this stayed awake for ever.
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

    /// The toast fade is an animation, so it has to be repainted at display rate.
    /// It used to ride on the 250 ms session cadence, which gives a 1.5 s dissolve
    /// only six frames: the pill visibly steps down through six greys (and pops out
    /// at the end) instead of fading.
    #[test]
    fn a_fading_toast_repaints_at_display_rate() {
        // The action toast (raised by "Save", a relay refresh, a ticket reset).
        let (mut app, _rt) = test_app();
        app.toast = Some(("Saved".to_string(), Instant::now()));
        let delay = settled_repaint_delay(&mut app);
        assert_awake(delay);
        assert!(
            delay <= TOAST_FRAME,
            "a live toast should repaint at ~60 fps, not on the {REPAINT:?} session \
             cadence, got {delay:?}"
        );

        // The copy toast, same cadence.
        let (mut app, _rt) = test_app();
        app.copied = Some(("whip", Instant::now()));
        assert!(
            settled_repaint_delay(&mut app) <= TOAST_FRAME,
            "a live copy toast should repaint at ~60 fps too"
        );

        // Nothing live: the guard must stay disarmed, or an idle window burns frames
        // for ever (the regression the two tests above cover).
        let (mut app, _rt) = test_app();
        assert_eq!(settled_repaint_delay(&mut app), Duration::MAX);
    }

    /// Render one real frame of the UI headlessly and return what was painted.
    fn render_shapes(app: &mut App, size: egui::Vec2) -> Vec<egui::epaint::ClippedShape> {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
        out.textures_delta.clear();
        out.shapes
    }

    /// The Watch tab carries a full-width initial-volume slider on its own row
    /// under the Player/Buffer line, with a `Player Initial Volume: N%` label on
    /// its left whose number matches the slider.
    ///
    /// Checked from the painted shapes: the label text, the slider track (a wide
    /// rounded rect), and that the track spans nearly the whole panel.
    #[test]
    fn the_watch_tab_has_a_full_width_volume_slider() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        app.watch_volume = 40;
        let size = egui::vec2(760.0, 560.0);
        let shapes = render_shapes(&mut app, size);

        // The label names the control and carries the number. `TextShape::pos` is
        // the top-left of the text, `galley.rect` the box it lays out in.
        let label_rect = shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.text() == "Player Initial Volume: 40%" => {
                    // `pos` is where the text box is placed; the galley's size is
                    // local, so lift it to the placement.
                    Some(egui::Rect::from_min_size(t.pos, t.galley.rect.size()))
                }
                _ => None,
            })
            .expect("the volume label should be painted");

        // The slider track: the widest thin rounded rect on the label's row, to
        // the right of it (the handle is the same language, just narrower).
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
            "the slider should span most of the panel, got {track:?} vs label \
             {label_rect:?}"
        );
        // It reaches the right edge of the panel (the panel is inset by its frame
        // margins plus the item spacing, so "full width" lands ~40 points short of
        // the window edge).
        assert!(
            track.right() >= size.x - 44.0,
            "the slider should reach the right edge, got {track:?}"
        );
    }

    /// The volume slider opens at half on a fresh install, and at whatever the user
    /// last set once it has been persisted.
    ///
    /// `App::new` seeds `watch_volume` from the config that was loaded off disk, the
    /// Watch tab writes it back the moment the slider is released (and again on
    /// Watch / on close), so the number survives closing and reopening the app.
    #[test]
    fn the_volume_slider_opens_at_half_and_reopens_where_it_was_left() {
        // Fresh install: nothing on disk, so the slider opens at half, not full.
        let (app, _rt) = test_app();
        assert_eq!(app.watch_volume, DEFAULT_VOLUME);
        assert_eq!(app.watch_volume, 50);

        // A config that persisted 37 % seeds the slider at 37 %.
        let (app, _rt) = test_app_with(Config {
            volume: 37,
            ..Config::default()
        });
        assert_eq!(app.watch_volume, 37);

        // `sync_watch_from_config` (run after the Settings panel edits the config)
        // pulls the slider back to the stored value.
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

    /// Render the live broadcast tab with viewers at the minimum window size, to
    /// exercise the click-to-copy boxes, the viewers cell and the floating toast for
    /// real: egui does the layout, and we check it paints, that the toast adds its
    /// own painting, and that nothing escapes the window.
    #[test]
    fn broadcast_tab_renders_at_min_size() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Broadcast;
        {
            let mut g = app.state.lock().unwrap();
            g.broadcast = Mode::Broadcasting;
            // A realistically long ticket: the copy box must elide it so it neither
            // runs off the box nor past the window edge.
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

        // Nothing escapes the window: no overflow at the minimum size.
        let window = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), min);
        for shape in &with_toast {
            assert!(
                window.expand(1.0).contains_rect(shape.clip_rect),
                "painted rect {:?} escapes the window",
                shape.clip_rect
            );
        }

        // Text extents (not just clip rects) stay inside the window: a long ticket
        // is elided, so no glyph run spills past the right edge. `clip_rect` is the
        // enclosing panel clip and would pass even if the galley overflowed, so the
        // galley's own width is checked here — the regression the copy box had.
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
    }

    /// Render the Watch tab at the minimum window size and assert nothing escapes
    /// the window. The Watch tab has a multiline ticket box, two combo boxes and a
    /// buffer field, so it is checked separately from the broadcast tab.
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
    }

    /// Count the interactive rects the Options window registers with the given
    /// mode, rendering only that window so the tab panels' own gating cannot
    /// confound the comparison.
    fn options_interactive_rects(mode: Mode) -> usize {
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
        // A few passes so the window and its grid settle at their final size.
        for _ in 0..3 {
            let mut out = ctx.run_ui(raw.clone(), |ui| {
                app.options_window(ui.ctx());
            });
            out.textures_delta.clear();
        }
        ctx.interactive_rects_last_pass().len()
    }

    /// Regression (AC3): the Options "Streaming" inputs must leave the interactive
    /// set while a session is live and return to it when idle. `interactive_rects_last_pass`
    /// filters out disabled widgets, so this count distinguishes disabled from enabled
    /// where an in-bounds/no-crash render check would pass either way.
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

    /// The tab underline's tween approaches its target monotonically, settles
    /// exactly on it after a realistic number of 60 fps frames (so the repaint
    /// guard can disarm and an idle window falls asleep), and stays settled.
    #[test]
    fn tab_slide_settles_on_the_target_without_overshooting() {
        // A single 60 fps step moves partway but never past the target.
        let step = tab_slide_step(0.0, 1.0, 1.0 / 60.0);
        assert!(step > 0.0 && step < 1.0, "one frame is partway, got {step}");

        // Stepping at 60 fps settles exactly on the target; the slide should take
        // well under a second of frames, never overshoot, and never stall.
        let mut pos = 0.0;
        let mut frames = 0;
        while pos != 1.0 {
            pos = tab_slide_step(pos, 1.0, 1.0 / 60.0);
            frames += 1;
            assert!(pos <= 1.0, "no overshoot at frame {frames}, got {pos}");
            assert!(
                frames < 60,
                "the slide must settle within 1 s, took {frames} frames"
            );
        }
        assert!(
            frames < 45,
            "settling took {frames} frames (~{} ms at 60 fps)",
            frames * 1000 / 60
        );

        // Settled stays settled: the repaint guard can disarm.
        assert_eq!(tab_slide_step(1.0, 1.0, 1.0 / 60.0), 1.0);
        // The reverse direction (Broadcast -> Watch) settles the same way.
        let mut pos = 1.0;
        while pos != 0.0 {
            pos = tab_slide_step(pos, 0.0, 1.0 / 60.0);
        }
        assert_eq!(pos, 0.0);
    }

    /// Count the interactive rects on the Watch tab that are the size of the
    /// full-width "Watch Stream" button (it is `add_sized` to 36 tall; the tab
    /// strip buttons are 32 and the multiline ticket box ~70, so a 34..=38 band
    /// isolates the primary action). `interactive_rects_last_pass` filters out
    /// disabled widgets, so a blank ticket — which disables the button — yields zero.
    ///
    /// The faint "paste a ticket to begin" hint is *not* counted: egui registers
    /// hover-sense rects for plain labels too, so a naive total count would tie
    /// (disabled button out, hint in). Filtering by the button's height isolates
    /// the button and makes the gating observable.
    fn watch_button_rects(ticket: &str) -> usize {
        watch_button_rects_with(Mode::Idle, ticket)
    }

    /// As [`watch_button_rects`], but with the broadcast track forced to `broadcast`.
    /// Used to prove the Watch action stays live while a broadcast runs.
    fn watch_button_rects_with(broadcast: Mode, ticket: &str) -> usize {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
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
            let mut out = ctx.run_ui(raw.clone(), |ui| app.draw(ui));
            out.textures_delta.clear();
        }
        ctx.interactive_rects_last_pass()
            .iter()
            .filter(|r| (34.0..=38.0).contains(&r.height()))
            .count()
    }

    /// The core of the new feature: while broadcasting, the Watch tab's primary
    /// action stays enabled, so you can watch a stream in the same instance (your
    /// own broadcast, or a friend's) without stopping the broadcast.
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

    /// A focused, window-keyboard-focused raw input at the default window size.
    fn focused_raw(events: Vec<egui::Event>) -> egui::RawInput {
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

    /// Run one headless frame, discarding the font-texture upload no backend eats.
    fn frame(ctx: &egui::Context, app: &mut App, raw: egui::RawInput) {
        let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
        out.textures_delta.clear();
    }

    /// A single Enter key press event.
    fn enter_key() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: Some(egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Autofocus grabs a prefilled ticket (the "press Enter to replay" case) but
    /// leaves an empty box unfocused, so an idle Watch tab still falls asleep.
    #[test]
    fn autofocus_grabs_a_prefilled_ticket_but_not_an_empty_one() {
        // Prefilled: focus is taken within a couple of frames, no click issued.
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

        // Empty: nothing focuses (an empty box can never submit), which is exactly
        // what keeps the repaint-sleep regression green under `focused: true`.
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

    /// Regression for the concrete wart: Enter on the focused ticket box used to
    /// drop a stray newline into the multiline field. Now Enter never inserts a
    /// newline, and an invalid ticket never starts (matching the button path).
    #[test]
    fn enter_on_the_focused_ticket_never_inserts_a_newline() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        app.watch_ticket = "not-a-real-ticket".into();
        let ctx = egui::Context::default();
        // Two frames to let autofocus settle, then press Enter with focus held.
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
        // Force focus on the (blank) box to isolate the blank gate from autofocus.
        app.focus_primary = false;
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, focused_raw(vec![enter_key()]));
        assert_eq!(
            app.state.lock().unwrap().watch,
            Mode::Idle,
            "Enter on a blank ticket does not start a session"
        );
    }

    /// On the Broadcast tab, when idle and focused, the start button owns keyboard
    /// focus so Enter/Space can start it (AC4 precondition). Real-app start then
    /// flows through the button's native keyboard `clicked()`.
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

    /// The connecting/starting spinner paints a single mesh of 24 ring quads
    /// (96 vertices, 48 triangles) — a 270° arc — so the waiting moment has a
    /// visible, controlled shape rather than a raw stroked line.
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

    /// The spinner's arc rotates with `secs`: two different times paint two
    /// different meshes, so the animation actually moves.
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

    /// Hovering a tab raises the pointing-hand cursor, so the tab strip reads as a
    /// live control under the mouse (the hover also lifts the tab's surface).
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

        // Render with the pointer over the Broadcast tab. egui computes a widget's
        // hover from the *previous* pass's rect, so the first pass establishes the
        // tab's rect and the second is the one whose hover (and cursor) is live.
        let ctx2 = egui::Context::default();
        let raw_hover = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            events: vec![egui::Event::PointerMoved(center)],
            ..Default::default()
        };
        let mut out2 = ctx2.run_ui(raw_hover.clone(), |ui| app.draw(ui));
        out2.textures_delta.clear();
        let mut out3 = ctx2.run_ui(raw_hover, |ui| app.draw(ui));
        out3.textures_delta.clear();
        assert_eq!(
            out3.platform_output.cursor_icon,
            egui::CursorIcon::PointingHand,
            "hovering a tab should raise the pointing-hand cursor"
        );
    }

    /// The bottom status bar paints a distinct, slightly darker fill so it reads as
    /// a deliberate bar rather than a strip of the content background.
    #[test]
    fn the_status_bar_uses_a_distinct_darker_fill() {
        let (mut app, _rt) = test_app();
        let size = egui::vec2(760.0, 560.0);
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
        out.textures_delta.clear();
        let bar_fill = egui::Color32::from_rgb(19, 21, 25);
        assert!(
            out.shapes.iter().any(|s| matches!(&s.shape,
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
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
        out.textures_delta.clear();
        // The hairline is a horizontal line segment (two points at the same y) near
        // the bottom of the window (the top of the status bar). The tab bar's
        // separator is also horizontal but sits near the top, so a y filter isolates
        // the hairline.
        let hairline = out.shapes.iter().find_map(|s| match &s.shape {
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

    /// The central panel paints a subtle vertical gradient (a two-triangle quad:
    /// 4 vertices, 6 indices) as its ground, so the window reads as a surface lit
    /// from above rather than a flat fill. The sparkline and spinner are meshes too,
    /// but with more vertices / a different index count, so a 4-vertex, 6-index mesh
    /// is unique to the gradient.
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

    /// A copy box paints a faint light-catch bevel along its top edge: a horizontal
    /// line in a faint white (alpha 24), distinct from the status bar hairline
    /// (alpha 16) and the tab separator.
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

    /// The idle Start button paints a soft 3-layer accent halo (the live green at
    /// low alpha), so the hero action reads as lit by the accent rather than a flat
    /// button. The three halo fills (0.02 / 0.05 / 0.09) are distinct from the
    /// button's own fill (0.18).
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

    /// While broadcasting, the Stop button paints a subtle 3-layer red halo (the same
    /// muted red the tab uses for errors), so the one control that tears down a live
    /// stream reads as a "stop" action at a glance.
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

    /// The tab content's fade-in starts hidden, eases out monotonically, and settles
    /// exactly on fully-visible at the duration (so the repaint guard can disarm and
    /// an idle window falls asleep).
    #[test]
    fn tab_fade_settles_on_fully_visible_without_overshooting() {
        // Starts hidden, not settled.
        assert_eq!(tab_fade_step(0.0, TAB_FADE_SECS), (0.0, false));
        // Mid-fade: partway, not settled, never above fully visible.
        let (mid, settled) = tab_fade_step(TAB_FADE_SECS * 0.5, TAB_FADE_SECS);
        assert!((0.0..1.0).contains(&mid), "mid-fade is partway, got {mid}");
        assert!(!settled, "mid-fade is not settled");
        // Settles exactly on 1.0 at the duration, and stays settled past it.
        assert_eq!(tab_fade_step(TAB_FADE_SECS, TAB_FADE_SECS), (1.0, true));
        assert_eq!(
            tab_fade_step(TAB_FADE_SECS * 5.0, TAB_FADE_SECS),
            (1.0, true)
        );
        // Monotonic: the alpha never decreases as the fade advances.
        let mut prev = 0.0;
        let mut t = 0.0;
        while t <= TAB_FADE_SECS {
            let (alpha, _) = tab_fade_step(t, TAB_FADE_SECS);
            assert!(alpha >= prev, "alpha must be monotonic at t={t}");
            prev = alpha;
            t += 0.01;
        }
    }

    /// The sonar pulse paints a ring that expands over the pulse and fades out near
    /// the end (so "live" reads as an active signal), and paints nothing once faded.
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
        // Mid-pulse: a ring is painted, and it expands over the pulse.
        let early = ring(0.2).expect("an early-pulse frame paints a ring");
        let late = ring(1.0).expect("a late-pulse frame paints a ring");
        assert!(
            late.radius > early.radius,
            "the ring should expand over the pulse ({} -> {})",
            early.radius,
            late.radius
        );
        // Near the end of the pulse the ring has faded out: nothing is painted.
        assert!(ring(1.99).is_none(), "a nearly-faded ring paints nothing");
    }

    /// The button's own fill rect: full-width and 36 tall. The halo layers are
    /// expanded (taller), so a 34..=38 height band isolates the button's fill.
    ///
    /// (Locating by the button's unique resting fill below is more robust: the idle
    /// ticket copy box is also ~34 tall and full-width, so a size-only match would tie.)
    #[test]
    fn hovering_the_start_button_lifts_its_fill() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Broadcast;
        let size = egui::vec2(760.0, 560.0);
        // The Start button's fill is unique to it (the halo uses 0.02/0.05/0.09, the
        // ticket box a flat alpha-14 tint), so it can be located by exact colour.
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

        // Hover: the pointer moves over the button; egui reads the hover from the
        // pointer position, so two passes make the lifted fill live.
        let ctx2 = egui::Context::default();
        let raw_hover = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            events: vec![egui::Event::PointerMoved(btn_rect.center())],
            ..Default::default()
        };
        let mut out2 = ctx2.run_ui(raw_hover.clone(), |ui| app.draw(ui));
        out2.textures_delta.clear();
        let mut out3 = ctx2.run_ui(raw_hover, |ui| app.draw(ui));
        out3.textures_delta.clear();
        assert!(
            out3.shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::epaint::Shape::Rect(r) if r.fill == hover_fill)),
            "hovering the start button should lift its fill to the hover shade"
        );
    }

    /// The log panel's height tween approaches its target monotonically, settles
    /// exactly on it after a realistic number of 60 fps frames (so the repaint
    /// guard can disarm and an idle window falls asleep), and stays settled. The
    /// reverse direction (open -> closed) settles the same way.
    #[test]
    fn log_anim_settles_on_the_target_without_overshooting() {
        // A single 60 fps step moves partway but never past the target.
        let step = log_anim_step(0.0, 1.0, 1.0 / 60.0);
        assert!(step > 0.0 && step < 1.0, "one frame is partway, got {step}");

        // Stepping at 60 fps settles exactly on the target; the slide should take
        // well under a second of frames, never overshoot, and never stall.
        let mut pos = 0.0;
        let mut frames = 0;
        while pos != 1.0 {
            pos = log_anim_step(pos, 1.0, 1.0 / 60.0);
            frames += 1;
            assert!(pos <= 1.0, "no overshoot at frame {frames}, got {pos}");
            assert!(
                frames < 60,
                "the slide must settle within 1 s, took {frames} frames"
            );
        }
        assert!(
            frames < 45,
            "settling took {frames} frames (~{} ms at 60 fps)",
            frames * 1000 / 60
        );

        // Settled stays settled: the repaint guard can disarm.
        assert_eq!(log_anim_step(1.0, 1.0, 1.0 / 60.0), 1.0);
        // The reverse direction (open -> closed) settles the same way.
        let mut pos = 1.0;
        while pos != 0.0 {
            pos = log_anim_step(pos, 0.0, 1.0 / 60.0);
        }
        assert_eq!(pos, 0.0);
    }

    /// The stats' eased display approaches its target monotonically, settles exactly
    /// on it after a realistic number of 60 fps frames (so the repaint guard can
    /// disarm and the panel fall back to the session cadence), never overshoots, and
    /// stays settled. A single frame moves partway. The reverse direction (a bitrate
    /// drop) settles the same way, never undershooting.
    #[test]
    fn stat_ease_settles_on_the_target_without_overshooting() {
        // A single 60 fps step moves partway but never past the target.
        let step = stat_ease(0.0, 5000.0, 1.0 / 60.0);
        assert!(
            step > 0.0 && step < 5000.0,
            "one frame is partway, got {step}"
        );

        // Stepping at 60 fps settles exactly on the target; the settle should take
        // well under two seconds of frames, never overshoot, and never stall.
        let mut pos = 0.0;
        let mut frames = 0;
        while pos != 5000.0 {
            pos = stat_ease(pos, 5000.0, 1.0 / 60.0);
            frames += 1;
            assert!(pos <= 5000.0, "no overshoot at frame {frames}, got {pos}");
            assert!(
                frames < 120,
                "the settle must finish within 2 s, took {frames}"
            );
        }

        // Settled stays settled: the repaint guard can disarm.
        assert_eq!(stat_ease(5000.0, 5000.0, 1.0 / 60.0), 5000.0);
        // The reverse direction (a bitrate drop) settles the same way, never undershooting.
        let mut pos = 5000.0;
        while pos != 0.0 {
            pos = stat_ease(pos, 0.0, 1.0 / 60.0);
            assert!(pos >= 0.0, "no undershoot, got {pos}");
        }
        assert_eq!(pos, 0.0);
    }

    /// A filled small circle in the tab strip (y < 120): the live dot is the only
    /// one. The status dot is a text glyph (not a `Shape::Circle`), and the sonar
    /// pulses are *stroked* rings (`stroke.width > 0`), so a filled-circle check
    /// isolates the live dot.
    fn live_dot_present(shapes: &[egui::epaint::ClippedShape]) -> bool {
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

    /// While a track is live, its tab carries a small filled "live" dot in the tab
    /// strip (green for a broadcast, blue for a watch), so a live session is visible
    /// at a glance right on the strip. While idle, no such dot is painted.
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

        // A live watch paints a dot on the Watch tab (the broadcast is idle, so the
        // only live dot is the watch's).
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

    /// The brand title is two-tone: "Stream" in near-white and "pipe" in a soft
    /// accent green, so the wordmark carries a small brand mark. The title is
    /// painted as two separate galleys (not one "Streampipe"), and each word's
    /// foreground colour is the one it was laid out with.
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
        // The title is split into two words, each painted (plus its shadow).
        assert!(
            shapes.iter().any(|s| is_text(s, "Stream")),
            "the title should paint a 'Stream' galley"
        );
        assert!(
            shapes.iter().any(|s| is_text(s, "pipe")),
            "the title should paint a 'pipe' galley"
        );
        // The "pipe" word is the accent green and the "Stream" word near-white
        // (the foreground galleys; the shadows are a dark low-alpha tint).
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

    /// `section_tick` paints a small accent bar (a filled rect in the accent colour,
    /// a few pixels wide) beside a bold label (a text galley holding the section
    /// text), so a section heading reads as a deliberate marker rather than a plain
    /// strong label.
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

    /// The stats panel eases its displayed measurements toward the latest sample:
    /// starting from zero, a non-zero target moves the display partway (never past the
    /// target) over the first frames, so the numbers settle smoothly instead of
    /// jumping on each 5-second tick.
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

    /// When no broadcast is live, the eased stats display is reset to zero (in
    /// [`App::draw`]), so a new broadcast starts from a clean zero rather than counting
    /// down from the previous session's values.
    #[test]
    fn the_stats_display_resets_to_zero_when_idle() {
        let (mut app, _rt) = test_app();
        // Seed the display as if a broadcast had just ended mid-settle.
        app.disp_video_kbps = 4000.0;
        app.disp_viewers = 2.0;
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(760.0, 560.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.draw(ui));
        out.textures_delta.clear();
        assert_eq!(app.disp_video_kbps, 0.0, "idle resets the video display");
        assert_eq!(app.disp_viewers, 0.0, "idle resets the viewer display");
    }

    /// The Settings window's "Save" button is the accent primary action: it paints a
    /// soft 3-layer accent halo (the live green at low alpha), matching the hero
    /// buttons on the tabs, so committing the edits reads as the primary commitment.
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
        // A few passes so the window and its content settle at their final size.
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

    /// The Settings footer is a matched pair of actions: "Save" in the accent green
    /// and "Close" in a neutral grey, both the same 90x30 box with the same halo
    /// language, so they read as one set with the green one leading. Clicking Save
    /// writes the config and raises the floating "Saved" pill; clicking Close
    /// dismisses the panel.
    #[test]
    fn the_settings_footer_is_a_matched_pair_and_save_raises_a_toast() {
        // The click below writes the config, so redirect the config dir into a temp
        // folder (serialized with the other config-dir test by the same mutex).
        let _guard = config::CONFIG_ENV.lock().unwrap();
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
        // Two settling passes so the window and its footer reach their final size.
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
        // Same box, side by side: the pair matches, the grey one is the secondary.
        assert_eq!(save.size(), close.size(), "Save and Close are the same box");
        assert_eq!((save.width(), save.height()), (90.0, 30.0));
        assert!(close.min.x > save.max.x, "Close sits to the right of Save");

        // A click is a press frame then a release frame on the same point. Driven by
        // a nested `fn` (not a closure) so it only borrows the app while it runs,
        // leaving the flags below readable between the two clicks.
        fn click_at(app: &mut App, ctx: &egui::Context, at: egui::Pos2, size: egui::Vec2) {
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
            let mut out = ctx.run_ui(press, |ui| app.draw(ui));
            out.textures_delta.clear();
            let mut out = ctx.run_ui(release, |ui| app.draw(ui));
            out.textures_delta.clear();
        }

        // Save: the config goes to disk and the confirmation pill is raised.
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

        // Close: the panel goes away, and the toast has expired by then (the click
        // above was two frames ago, the pill lasts 1500 ms of wall clock, and the
        // draw loop expires it).
        click_at(&mut app, &ctx, close.center(), size);
        assert!(!app.show_options, "Close dismisses the panel");

        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// Log lines are coloured by their level prefix (tracing's `LEVEL target:
    /// message` format): errors in the app's error red, warnings in a muted amber,
    /// debug and trace receded to a dim grey, and everything else (info, and lines
    /// without a level prefix) in the normal text colour.
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

    /// The error callout paints a muted red-tinted surface with a red left bar
    /// (the section-tick's language in the error hue) and the message in the error
    /// red, so a failure reads as a deliberate surface rather than a stray line of
    /// text.
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
            egui::epaint::Shape::Text(t) => {
                t.galley.job.sections.first().map(|sec| sec.format.color)
            }
            _ => None,
        };
        assert_eq!(color, Some(ERROR), "the message should be the error red");
    }

    /// The log panel's header carries a close button (two crossing 1.5 px strokes),
    /// and clicking it closes the panel — the same toggle as Ctrl/Cmd+L, so the
    /// panel can be dismissed without knowing the shortcut.
    #[test]
    fn the_log_panel_close_button_closes_the_panel() {
        let (mut app, _rt) = test_app();
        app.log_open = true;
        // Fully open: the height tween runs on the wall clock, which barely advances
        // between headless passes, so the tween is pinned at its open target.
        app.log_anim = 1.0;
        let size = egui::vec2(760.0, 560.0);
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            ..Default::default()
        };
        // Render once to locate the close button: on an idle app the only 1.5 px
        // line segments are its two crossing strokes (the hairlines, bevels and
        // separators are all 1.0, and the sparkline and spinner are not drawn
        // while idle).
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

        // Press over the button, then release: a click.
        let press = egui::RawInput {
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
        let mut out = ctx.run_ui(press, |ui| app.draw(ui));
        out.textures_delta.clear();
        let release = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            events: vec![egui::Event::PointerButton {
                button: egui::PointerButton::Primary,
                pos: center,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut out = ctx.run_ui(release, |ui| app.draw(ui));
        out.textures_delta.clear();
        assert!(
            !app.log_open,
            "clicking the close button must close the log panel"
        );
    }

    /// A freshly-arrived log line is faded in (its text alpha is reduced), while a
    /// line that arrived long ago is fully opaque. This is the log fade-in: new lines
    /// ease in over the fade window rather than popping in.
    #[test]
    fn a_new_log_line_fades_in_and_an_old_one_is_opaque() {
        let (mut app, _rt) = test_app();
        app.log_open = true;
        app.log_anim = 1.0;
        // Two lines: the first (front) is backdated so it is fully opaque, the second
        // (back) is fresh so it is still fading in.
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
        // Read the laid-out colour's alpha of the text galley holding `text`.
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

    /// Pressing the idle Start button pushes its fill in to the press shade (a touch
    /// darker than the hover lift), so the hero action reads as physically pressed in
    /// rather than flat. The pointer is over the button and the primary button is held.
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

        // Press: the pointer moves over the button and the primary button is held.
        // Two passes make the pressed fill live (egui reads the pointer from the
        // previous pass).
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
        let mut out2 = ctx2.run_ui(raw_press.clone(), |ui| app.draw(ui));
        out2.textures_delta.clear();
        let mut out3 = ctx2.run_ui(raw_press, |ui| app.draw(ui));
        out3.textures_delta.clear();
        assert!(
            out3.shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::epaint::Shape::Rect(r) if r.fill == press_fill)),
            "pressing the start button should push its fill in to the press shade"
        );
    }

    /// Hovering a copy box paints a soft 2-layer accent glow around it and brightens
    /// its copy glyph (0.5 -> 0.8), so the "click to copy" affordance reads as lit and
    /// alive under the mouse. A live broadcast shows the copy boxes.
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
        let mut out2 = ctx2.run_ui(raw_hover.clone(), |ui| app.draw(ui));
        out2.textures_delta.clear();
        let mut out3 = ctx2.run_ui(raw_hover, |ui| app.draw(ui));
        out3.textures_delta.clear();
        // The glow: two accent rects at 0.03 and 0.06 alpha (the farthest and nearest
        // layers of the halo). The start button is idle (not pressed), so its halo
        // uses the resting alphas (0.02/0.05/0.09) and cannot match.
        let glow_count = out3
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
        // The copy glyph brightens on hover (0.5 -> 0.8): its two sheets are stroked
        // in the brighter accent.
        let glyph_bright = ACCENT.gamma_multiply(0.8);
        let glyph_count = out3
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

    /// While a session is live, the status bar's fill carries a subtle tint of the
    /// session's accent (green while broadcasting, blue while watching), mirroring
    /// the status dot, so the bar reads as part of the live session. Idle keeps the
    /// plain dark ground (byte-identical to before the tint).
    #[test]
    fn the_status_bar_tints_with_the_live_session() {
        let size = egui::vec2(760.0, 560.0);
        // The status bar's fill: the only opaque, dark, full-width rect at the very
        // bottom of the window. The Stop/Watch button's fill is a low-alpha accent
        // tint (alpha < 255), so the opaque + dark filter isolates the bar.
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

    /// The moment its text is copied, a copy box's glyph becomes a checkmark (two
    /// 1.5 px strokes in the full accent) instead of the two sheets, so the
    /// confirmation lands right at the point of action, not only in the toast far
    /// below. A box that was not just copied keeps its sheets.
    #[test]
    fn the_copy_glyph_becomes_a_checkmark_on_copy() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Broadcast;
        app.state.lock().unwrap().broadcast = Mode::Broadcasting;
        let size = egui::vec2(760.0, 560.0);
        // A checkmark stroke: a 1.5 px line segment in the accent green. The
        // checkmark is painted at `ACCENT.gamma_multiply(flash)`, and the flash
        // decays in real time over the render, so the exact shade is not stable —
        // but the hue always is (the accent's green channel leads), so the hue is
        // what is checked, not the exact colour.
        let is_check_stroke = |s: &egui::epaint::ClippedShape| -> bool {
            matches!(&s.shape, egui::epaint::Shape::LineSegment { stroke, .. }
                if (stroke.width - 1.5).abs() < 0.01
                    && stroke.color.g() > stroke.color.r()
                    && stroke.color.g() > stroke.color.b())
        };

        // Nothing copied yet: no checkmark strokes (the glyph is two sheets, which
        // are stroked rects, not line segments).
        let before = render_shapes(&mut app, size);
        assert_eq!(
            before.iter().filter(|s| is_check_stroke(s)).count(),
            0,
            "an un-copied box has no checkmark"
        );

        // Just copied: the flash is live, so the copied box's glyph cross-fades into
        // a checkmark. Only the copied box (key "whip") morphs; the ticket box keeps
        // its sheets, so exactly two checkmark strokes appear.
        app.copied = Some(("whip", Instant::now()));
        let after = render_shapes(&mut app, size);
        let check = after.iter().filter(|s| is_check_stroke(s)).count();
        assert_eq!(
            check, 2,
            "the copied box's glyph should be a checkmark (two strokes), got {check}"
        );
    }

    /// A focused ticket box paints its focus ring: the frame's stroke brightens to
    /// the focus shade (and a soft glow is lit), so the active input reads as lit
    /// rather than relying on a blinking caret alone. A prefilled ticket autofocuses
    /// on a window-focused frame, so the ring is observable headlessly.
    #[test]
    fn the_focused_ticket_box_paints_a_focus_ring() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        app.watch_ticket = "a-persisted-ticket".into();
        let size = egui::vec2(760.0, 560.0);
        let focused_raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            focused: true,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        // Two frames let the autofocus settle (a prefilled ticket grabs focus on a
        // window-focused frame).
        for _ in 0..2 {
            let mut out = ctx.run_ui(focused_raw.clone(), |ui| app.draw(ui));
            out.textures_delta.clear();
        }
        assert!(
            ctx.memory(|m| m.focused()).is_some(),
            "a prefilled ticket autofocuses"
        );
        // A focused frame paints the box's bright focus stroke.
        let mut out = ctx.run_ui(focused_raw, |ui| app.draw(ui));
        out.textures_delta.clear();
        let focus_stroke = ACCENT.gamma_multiply(0.75);
        assert!(
            out.shapes.iter().any(|s| matches!(&s.shape,
                egui::epaint::Shape::Rect(r) if r.stroke.width > 0.0 && r.stroke.color == focus_stroke
            )),
            "the focused ticket box should paint its bright focus stroke"
        );
    }

    /// While a session is live, the status bar carries a compact "LIVE" badge (a
    /// stroked pill in the session's accent), so the live state reads at a glance
    /// rather than only as a small breathing dot. While idle, no badge is painted.
    #[test]
    fn the_status_bar_paints_a_live_badge_when_live() {
        let size = egui::vec2(760.0, 560.0);
        let has_live_badge = |shapes: &[egui::epaint::ClippedShape]| -> bool {
            shapes.iter().any(
                |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == "LIVE"),
            )
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

    /// The bitrate sparkline carries a small "now … kbit/s" caption above the chart,
    /// so the current bitrate is readable at a glance without hovering.
    #[test]
    fn the_sparkline_carries_a_now_value_caption() {
        let (mut app, _rt) = test_app();
        app.tab = Tab::Broadcast;
        {
            let mut g = app.state.lock().unwrap();
            g.broadcast = Mode::Broadcasting;
            // Two or more samples so the sparkline (and its caption) is drawn; the
            // newest sample is the "now" value the caption reads out.
            g.video_kbps_history = std::collections::VecDeque::from(vec![100.0, 200.0, 150.0]);
            g.stats = Some(StreamStats {
                video_kbps: 150.0,
                ..Default::default()
            });
        }
        let size = egui::vec2(760.0, 560.0);
        let shapes = render_shapes(&mut app, size);
        assert!(
            shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == "now 150 kbit/s")),
            "the sparkline should paint its 'now' value caption"
        );
    }

    /// The log panel leads an error line with a red "ERR" chip and a warning line
    /// with an amber "WARN" chip, so problems pop out of the wall of grey log text.
    /// An info line gets no chip. The lines are backdated to fully opaque so the
    /// chip colours are deterministic.
    #[test]
    fn the_log_panel_marks_errors_and_warnings_with_chips() {
        let (mut app, _rt) = test_app();
        app.log_open = true;
        app.log_anim = 1.0;
        log::push(&app.log, "ERROR streampipe::whip: bind failed");
        log::push(&app.log, "WARN streampipe::udp: dropping datagram");
        log::push(&app.log, "INFO streampipe::state: endpoint online");
        // Backdate every line past the fade window so each is fully opaque (alpha 1),
        // making the chip colours exact.
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
        let err_expected =
            egui::Color32::from_rgba_unmultiplied(ERROR.r(), ERROR.g(), ERROR.b(), 230);
        let warn_expected = egui::Color32::from_rgba_unmultiplied(
            WARN_AMBER.r(),
            WARN_AMBER.g(),
            WARN_AMBER.b(),
            230,
        );
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

    /// Hovering a log line paints a faint background highlight, so a line under the
    /// cursor is easy to track in a dense log. The highlight is a faint white rect
    /// (alpha 12), painted before the line's content so the text sits on top. A line
    /// that is not hovered paints no such highlight.
    #[test]
    fn hovering_a_log_line_paints_a_faint_highlight() {
        let (mut app, _rt) = test_app();
        app.log_open = true;
        app.log_anim = 1.0;
        log::push(&app.log, "a log line to hover");
        let size = egui::vec2(760.0, 560.0);
        // The faint highlight: a filled rect in a faint white (alpha 12).
        let has_highlight =
            |shapes: &[egui::epaint::ClippedShape]| -> bool {
                shapes.iter().any(|s| matches!(&s.shape,
                egui::epaint::Shape::Rect(r) if r.fill == egui::Color32::from_white_alpha(12)
            ))
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
        // Hover the log line: two passes make the hover live (egui reads the pointer
        // from the previous pass).
        let ctx2 = egui::Context::default();
        let raw_hover = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            events: vec![egui::Event::PointerMoved(center)],
            ..Default::default()
        };
        let mut out2 = ctx2.run_ui(raw_hover.clone(), |ui| app.draw(ui));
        out2.textures_delta.clear();
        let mut out3 = ctx2.run_ui(raw_hover, |ui| app.draw(ui));
        out3.textures_delta.clear();
        assert!(
            has_highlight(&out3.shapes),
            "hovering a log line should paint a faint highlight"
        );
    }

    /// `soft_glow` paints a two-layer halo in the colour it is given (not a
    /// hard-coded accent), so the shared halo language can light a surface in its
    /// own accent (the live green for a broadcast surface, the watch blue for a
    /// watch surface). The two layers are 0.03 and 0.06 alpha in that colour.
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
        // The watch blue, not the default accent green, so the test proves the
        // colour is honoured rather than hard-coded.
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

    /// Regression: the central panel's ground gradient must be painted *behind* the
    /// panel content, not on top of it. The gradient is a 4-vertex, 6-index mesh
    /// covering the central panel; the panel content (e.g. the tab labels) is text in
    /// the upper area. If the gradient were painted on a separate background layer
    /// (whose order relative to the content is not guaranteed in egui 0.36, where the
    /// panel content already lives on the background order), it could land *after*
    /// the content in the paint list and cover the whole panel, leaving a blank
    /// window. Assert the gradient precedes the content so it stays behind it.
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
                // Central panel content lives in the upper area; the status bar is
                // at the very bottom (y ~ 541). Track the first such shape: the
                // gradient must precede even that one to be behind all the content.
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

    /// The bitrate sparkline carries a subtle peak marker (a small dot at the recent
    /// peak, with a faint vertical line down to the baseline), so a wobbly stream's
    /// headroom is visible at a glance (how far the current bitrate is from the
    /// recent peak). The marker is skipped when the peak is the newest sample (the
    /// "now" dot already marks that point).
    #[test]
    fn the_sparkline_marks_the_recent_peak() {
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
        let size = egui::vec2(760.0, 560.0);
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
            // The newest sample is the peak, so the peak marker is skipped.
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

    /// The central panel paints a subtle ambient "light from above" glow at the top
    /// centre: six concentric circles at a very low alpha, so the brand title and tab
    /// strip read as lit from above (the same cue as the ground gradient and the top
    /// bevels).
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

    /// The Watch tab shows a friendly "where do I get a ticket?" hint below the box
    /// while it is empty, so a first-time viewer knows what to do. The hint recedes
    /// once a ticket is pasted.
    #[test]
    fn the_watch_tab_shows_a_ticket_hint_when_empty() {
        let hint = "No ticket yet? Start a broadcast, or ask a friend to share theirs.";
        // Empty ticket: the hint is shown.
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        let size = egui::vec2(760.0, 560.0);
        let shapes = render_shapes(&mut app, size);
        assert!(
            shapes.iter().any(|s| matches!(&s.shape,
                egui::epaint::Shape::Text(t) if t.galley.text() == hint
            )),
            "an empty ticket should show the hint"
        );
        // A pasted ticket: the hint recedes.
        let (mut app, _rt) = test_app();
        app.tab = Tab::Watch;
        app.watch_ticket = "a-persisted-ticket".into();
        let shapes = render_shapes(&mut app, size);
        assert!(
            !shapes.iter().any(|s| matches!(&s.shape,
                egui::epaint::Shape::Text(t) if t.galley.text() == hint
            )),
            "a pasted ticket should hide the hint"
        );
    }

    /// The window icon is a square, non-empty RGBA image whose centre is the live
    /// accent (the stream runs through the centre) and whose corners are transparent
    /// (outside the rounded square). Guards against an empty, mis-sized, or
    /// all-opaque / all-transparent icon that the task bar would render blank or as a
    /// solid block.
    #[test]
    fn the_app_icon_is_a_square_with_a_centred_stream() {
        let icon = app_icon();
        assert_eq!(icon.width, icon.height, "the icon is square");
        assert!(!icon.is_empty(), "the icon has pixels");
        assert_eq!(
            icon.rgba.len(),
            (icon.width * icon.height * 4) as usize,
            "the rgba buffer is exactly width*height*4"
        );
        let size = icon.width as usize;
        let px = |x: usize, y: usize| -> (u8, u8, u8, u8) {
            let i = (y * size + x) * 4;
            (
                icon.rgba[i],
                icon.rgba[i + 1],
                icon.rgba[i + 2],
                icon.rgba[i + 3],
            )
        };
        // The centre sits on the stream's centre line: the full accent green.
        let (r, g, b, a) = px(size / 2, size / 2);
        assert!(a > 0, "the centre is opaque, got alpha {a}");
        assert!(
            g > r && g > b,
            "the centre is the accent green (green leads), got ({r},{g},{b})"
        );
        // A corner is outside the rounded square: fully transparent.
        assert_eq!(px(0, 0).3, 0, "the top-left corner is transparent");
        assert_eq!(
            px(size - 1, size - 1).3,
            0,
            "the bottom-right corner is transparent"
        );
        // The top edge-centre is inside the rounded square: opaque (not clipped).
        assert!(
            px(size / 2, 1).3 > 0,
            "the top edge-centre is inside the icon and opaque"
        );
    }

    /// The icon's ground is a subtle vertical gradient (lit from above): a pixel near
    /// the top edge-centre is lighter than one near the bottom edge-centre, so the
    /// icon reads as a surface lit from above (the same cue as the panel ground).
    #[test]
    fn the_app_icon_ground_is_lit_from_above() {
        let icon = app_icon();
        let size = icon.width as usize;
        let lum = |y: usize| -> u32 {
            // Sample a column just off the centre (away from the stream) so the
            // ground gradient, not the accent, is measured.
            let x = size / 2 + size / 4;
            let i = (y * size + x) * 4;
            (icon.rgba[i] as u32) + (icon.rgba[i + 1] as u32) + (icon.rgba[i + 2] as u32)
        };
        let top = lum(size / 8);
        let bottom = lum(size - size / 8);
        assert!(
            top > bottom,
            "the icon's top ({top}) should be lighter than its bottom ({bottom})"
        );
    }

    /// The central panel paints a subtle radial vignette (a fan mesh of 32 segments:
    /// 33 vertices, 96 indices) behind its content, so the eye is gently drawn to the
    /// centre of the panel (the same depth cue as the ground gradient and the ambient
    /// glow). The vertex/index count is unique to the vignette: the ground gradient is
    /// a 4-vertex quad and the spinner is a 96-vertex arc.
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

    /// The "Waiting for OBS to start streaming…" hint (a live broadcast that has not
    /// yet received media from OBS) carries a small breathing dot (a filled circle of
    /// radius 4.0 in the live accent), so the wait reads as an active signal rather
    /// than a static label. A broadcast that is already receiving media (or has
    /// viewers) shows no such dot.
    #[test]
    fn the_waiting_for_obs_hint_carries_a_breathing_dot() {
        // Live, no viewers, not receiving, no history: the "Waiting for OBS" hint
        // (and its breathing dot) is shown.
        let (mut app, _rt) = test_app();
        app.tab = Tab::Broadcast;
        app.state.lock().unwrap().broadcast = Mode::Broadcasting;
        let size = egui::vec2(760.0, 560.0);
        let shapes = render_shapes(&mut app, size);
        // The breathing dot: a filled circle (stroke.width == 0) of radius 4.0. The
        // ambient-glow circles are far larger and the sonar rings are stroked, so a
        // filled radius-4.0 circle isolates the dot.
        let dot = shapes.iter().any(|s| match &s.shape {
            egui::epaint::Shape::Circle(c) => {
                (c.radius - 4.0).abs() < 0.01 && c.stroke.width == 0.0 && c.fill.a() > 0
            }
            _ => false,
        });
        assert!(
            dot,
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
        let shapes = render_shapes(&mut app, size);
        let dot = shapes.iter().any(|s| match &s.shape {
            egui::epaint::Shape::Circle(c) => {
                (c.radius - 4.0).abs() < 0.01 && c.stroke.width == 0.0 && c.fill.a() > 0
            }
            _ => false,
        });
        assert!(
            !dot,
            "a broadcast already receiving media should drop the waiting dot"
        );
    }

    /// The status bar carries a subtle keyboard-shortcut hint on its right edge, so
    /// the (otherwise hidden) Ctrl+L / Ctrl+, shortcuts are discoverable without a
    /// manual.
    #[test]
    fn the_status_bar_shows_a_keyboard_shortcut_hint() {
        let (mut app, _rt) = test_app();
        let size = egui::vec2(760.0, 560.0);
        let shapes = render_shapes(&mut app, size);
        let hint = "Ctrl+L log · Ctrl+, settings";
        assert!(
            shapes.iter().any(|s| matches!(&s.shape,
                egui::epaint::Shape::Text(t) if t.galley.text() == hint
            )),
            "the status bar should show a keyboard shortcut hint"
        );
    }
}
