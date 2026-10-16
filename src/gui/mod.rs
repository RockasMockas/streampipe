//! The GUI front-end, opened by `streampipe` with no arguments.
//!
//! The eframe event loop runs on the main thread; async work runs on two tokio
//! runtimes. The primary one hosts the UI's async work (the stats consumer that
//! requests repaints); a dedicated `streampipe-engine` runtime runs every media
//! session, so nothing in the streaming hot path shares a thread with rendering.
//! See [`state`] for the threading contract.

pub mod broadcast;
pub mod config;
pub mod contacts;
pub mod log;
pub mod options;
pub mod state;
pub mod watch;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::stream::{Player, StreamStats};
use eframe::egui::{self, Ui};
use tokio::{runtime::Handle, sync::mpsc};

use self::{
    config::Config,
    state::{Mode, SharedState},
};

/// Session cadence: enough for logs/stats to refresh without a repaint storm.
pub const REPAINT: Duration = Duration::from_millis(250);
/// How long a toast stays visible.
pub const TOAST: Duration = Duration::from_millis(1500);
/// Toast fades need ~60 fps; at [`REPAINT`] a 1.5 s fade only gets six visible steps.
pub const TOAST_FRAME: Duration = Duration::from_millis(16);
/// How long a copy box's click flash lasts.
pub const FLASH: Duration = Duration::from_millis(450);
/// The "live/success" accent: broadcasting status, copy toast, viewers.
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(80, 200, 120);
/// The "watching" accent: the watch status dot and the connecting spinner.
pub const WATCH_ACCENT: egui::Color32 = egui::Color32::from_rgb(90, 160, 255);
/// The "error" red: failed sessions, the stop halo, the error callout.
pub const ERROR: egui::Color32 = egui::Color32::from_rgb(220, 90, 90);
/// How long a tab's content takes to fade in after a tab switch.
pub const TAB_FADE_SECS: f32 = 0.16;
/// How long a new log line takes to fade in.
pub const LOG_FADE_SECS: f32 = 0.2;

/// The two main tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Watch,
    Broadcast,
}

/// The application.
pub struct App {
    /// Handle to the `streampipe-engine` runtime that runs media sessions.
    pub handle: Handle,
    /// Cross-thread shared state.
    pub state: Arc<Mutex<SharedState>>,
    /// The persisted configuration, edited live and saved on change and quit.
    pub config: Config,
    /// Whether the loaded config carried a broadcast ticket the current wire format
    /// rejects (see [`config::ticket_is_valid`]). Set once in [`App::new`] after
    /// dropping the invalid ticket; the first idle frame mints a fresh replacement
    /// (same identity, like a manual relay refresh) and clears the flag.
    pub regen_ticket: bool,
    /// The log ring fed by the tee writer.
    pub log: log::LogRing,
    /// The stats channel sender, cloned to each broadcast.
    pub stats_tx: mpsc::UnboundedSender<StreamStats>,
    /// The selected tab.
    pub tab: Tab,
    /// Whether the options panel is open.
    pub show_options: bool,
    /// Whether the Contacts overlay is open. Mutually exclusive with `show_options` —
    /// both are anchored over the panels, so only one is ever on screen.
    pub show_contacts: bool,
    /// Whether the Contacts list has edits staged for the next flush (written when the
    /// overlay closes, never per keystroke).
    pub contacts_dirty: bool,
    /// The contact row whose name field should take keyboard focus next frame — armed by
    /// "Add contact" so typing starts in the new row, cleared once consumed.
    pub focus_contact_row: Option<usize>,
    /// The contact row whose deletion is awaiting confirmation in the modal. `Some(i)`
    /// opens the "Remove …?" dialog; confirming removes row `i`, cancelling clears it.
    pub confirm_remove_contact: Option<usize>,
    /// The contact row in edit mode — exactly one at a time. `None` means every row
    /// paints its read-only view (avatar, labels, "Load", "Edit").
    pub editing_contact: Option<usize>,
    /// The row animating out of edit mode (its progress is `1 - contact_edit_anim`),
    /// so closes play instead of snapping. Cleared once the tween settles at 1.
    pub contact_edit_prev: Option<usize>,
    /// The shared 0↔1 row-edit tween, stepped once per frame in `draw()` (single
    /// owner) so every row reads the same clock.
    pub contact_edit_anim: f32,
    /// Whether the bottom log panel is expanded.
    pub log_open: bool,
    /// Whether the WHIP bearer token is shown in cleartext. Masked by default so a
    /// shared secret is not left legible on screen.
    pub show_bearer: bool,
    /// The ticket text on the Watch tab.
    pub watch_ticket: String,
    /// The player selected on the Watch tab.
    pub watch_player: Player,
    /// The buffer field on the Watch tab, in milliseconds (empty = auto).
    pub watch_buffer: String,
    /// The "Aggressive Low Latency" checkbox on the Watch tab.
    pub watch_low_latency: bool,
    /// The initial player volume on the Watch tab, in percent (0-100).
    pub watch_volume: u8,
    /// The last copied box and when, for the transient toast.
    pub copied: Option<(&'static str, Instant)>,
    /// A transient action-complete toast (e.g. "Relay refreshed ✓") and when raised.
    pub toast: Option<(String, Instant)>,
    /// When true, the live [`Self::toast`] is drawn in the error colour (e.g. a contact
    /// whose ticket failed to parse and could not be saved).
    pub toast_error: bool,
    /// Clock origin for the pulse animations, so they advance between repaints.
    pub pulse: Instant,
    /// Whether the next window-focused frame should autofocus the active tab's primary
    /// control. Set on open and on tab switch, cleared once consumed, so it never
    /// steals focus from a widget the user already chose.
    pub focus_primary: bool,
    /// The tab underline's slide position: 0.0 = Watch, 1.0 = Broadcast.
    pub tab_slide: f32,
    /// The previous frame's clock, for the underline tween's frame delta.
    pub last_frame: Instant,
    /// The tab content's fade-in alpha (0.0 = hidden), reset on tab switch.
    pub tab_fade: f32,
    /// Whether the tab fade is in flight; arms the repaint guard while true.
    pub tab_fade_active: bool,
    /// The instant of the last tab switch, the fade-in's clock origin.
    pub tab_switched_at: Instant,
    /// The log panel's open fraction (0.0 = closed), tweened so it slides.
    pub log_anim: f32,
    /// The previous frame's clock, for the log panel tween's frame delta.
    pub log_last_frame: Instant,
    /// The eased *display* of the live broadcast stats, tweened toward the latest
    /// [`StreamStats`] each frame so the numbers settle instead of jumping on each
    /// 5-second tick. Reset to zero when the broadcast goes idle, so a new broadcast
    /// starts from a clean zero.
    pub disp_viewers: f32,
    pub disp_video_kbps: f32,
    pub disp_video_fps: f32,
    pub disp_audio_kbps: f32,
}

impl App {
    /// Build the app from the engine runtime handle, config and channels.
    ///
    /// `handle` is the `streampipe-engine` runtime, so every media session the UI
    /// starts runs on threads the renderer never shares. Tests pass a handle of a
    /// runtime they keep alive.
    pub fn new(
        handle: Handle,
        mut config: Config,
        state: Arc<Mutex<SharedState>>,
        log: log::LogRing,
        stats_tx: mpsc::UnboundedSender<StreamStats>,
    ) -> Self {
        let watch_player = config.default_player();
        let watch_buffer = config
            .buffer_ms
            .map(|ms| ms.to_string())
            .unwrap_or_default();
        // The slider's range is 0..=100; the `min` only guards a value restored from
        // a hand-edited config.
        let watch_volume = config.volume.min(100);
        let watch_low_latency = config.low_latency;
        // Self-heal a stored broadcast ticket the current wire format rejects (an
        // older build, or a test-polluted config, can leave a garbage string): drop
        // it, persist the removal, and flag the first idle frame to mint a fresh
        // replacement from the live endpoint (see `draw`).
        let regen_ticket = config
            .broadcast_ticket
            .as_deref()
            .is_some_and(|t| !config::ticket_is_valid(t));
        if regen_ticket {
            tracing::warn!("stored broadcast ticket is invalid; replacing it with a fresh one");
            config.broadcast_ticket = None;
            config.save();
        }
        App {
            handle,
            state,
            watch_ticket: config.last_ticket.clone(),
            config,
            regen_ticket,
            log,
            stats_tx,
            tab: Tab::Watch,
            show_options: false,
            show_contacts: false,
            contacts_dirty: false,
            focus_contact_row: None,
            confirm_remove_contact: None,
            editing_contact: None,
            contact_edit_prev: None,
            contact_edit_anim: 0.0,
            log_open: false,
            show_bearer: false,
            watch_player,
            watch_buffer,
            watch_low_latency,
            watch_volume,
            copied: None,
            toast: None,
            toast_error: false,
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
        // Read before the closure: it mutably borrows `self` for the pulse clock.
        let secs = self.pulse.elapsed().as_secs_f32();
        egui::menu::MenuBar::new().ui(ui, |ui| {
            // Open the panels directly — no intermediate dropdown. They occupy the same
            // space over the panels, so opening one always dismisses the other.
            if ui.button("Settings").clicked() {
                self.show_options = !self.show_options;
                if self.show_options {
                    self.show_contacts = false;
                    self.confirm_remove_contact = None;
                }
            }
            // A hairline between the two panel toggles, so the pair reads as distinct
            // entries rather than one run of buttons.
            ui.separator();
            if ui.button("Contacts").clicked() {
                self.toggle_contacts();
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
            // Only a live session breathes; `Starting` stays calm yellow and `Idle`
            // flat gray, so a steady dot never reads as a running session.
            let pulsing = snap.broadcast == Mode::Broadcasting || snap.watch == Mode::Watching;
            let dot = if pulsing {
                base.gamma_multiply(pulse_factor(secs))
            } else {
                base
            };
            ui.spacing_mut().item_spacing.x = 4.0;
            let dot_resp = ui.colored_label(dot, "●");
            // A sonar ring around the live dot, sized to stay within the menu bar.
            if pulsing {
                sonar_pulse(ui.painter(), dot_resp.rect.center(), 4.0, secs, 2.0, base);
            }
            ui.colored_label(base, snap.status());
            // A "LIVE" badge breathing in the same curve as the dot, so the live state
            // reads at a glance. Stroked pill, so it never disturbs the
            // line-segment-based tests.
            if pulsing {
                ui.add_space(2.0);
                live_badge(ui, base, pulse_factor(secs));
            }
            let err = snap.broadcast_error.as_ref().or(snap.watch_error.as_ref());
            if let Some(err) = err {
                ui.colored_label(ERROR, truncate(err, 60));
            }
            // A dim shortcut hint so the (otherwise hidden) shortcuts are discoverable.
            // Only shown when it fits, so it never overflows a narrow window.
            let hint = "Ctrl+L log · Ctrl+, settings · Ctrl+T contacts";
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

    /// The app wordmark: a large two-tone title ("Stream" near-white, "pipe" accent
    /// green) with a soft drop shadow. Each word is a single anti-aliased galley;
    /// faux-bold offset copies render jaggy, so weight comes from size alone.
    fn brand_title(&mut self, ui: &mut Ui) {
        let font_id = egui::FontId::proportional(30.0);
        let fg = egui::Color32::from_rgb(242, 242, 246);
        // Green, but muted enough to read as a brand tint rather than a status colour.
        let pipe = egui::Color32::from_rgb(110, 205, 150);
        // The shadow is the same glyphs in dark low-alpha layers at a few offsets,
        // faking a blurred drop shadow behind the crisp text.
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

        // Side by side; the total width is the sum of the galleys, plus room for the
        // shadow's furthest offset so it is not clipped.
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
    /// autofocus. A no-op when already on `tab`, so clicking the active tab does not
    /// replay the transition. The fade is reset here (not in the tween) so the new
    /// tab's content is hidden on the very frame the switch is detected.
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
        // Which tracks are live, so the active tab carries a small breathing dot
        // (green for a broadcast, blue for a watch).
        let snap = state::snapshot(&self.state);
        let watch_live = snap.watch == Mode::Watching;
        let bc_live = snap.broadcast == Mode::Broadcasting;
        // The dot's breath clock, which only advances while repaints are running —
        // exactly while a session is live.
        let secs = self.pulse.elapsed().as_secs_f32();
        // Two equal-width tabs filling the strip, so the whole tab box (not just the
        // text) is the click target.
        let half = ui.available_width() / 2.0;
        let tab_h = 32.0;
        let (watch, bc) = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // Painted by hand rather than via `egui::Button`: a fixed `.fill()`
                // suppresses egui's hover feedback, so the strip would feel dead under
                // the mouse. Both states are styled explicitly — the selected tab gets a
                // lighter surface and full-strength text, unselected tabs recede.
                let make = |ui: &mut Ui,
                            label: &str,
                            selected: bool,
                            live: bool,
                            dot_color: egui::Color32| {
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
                    ui.painter().rect_filled(rect, corner, fill);
                    ui.painter()
                        .rect_stroke(rect, corner, stroke, egui::StrokeKind::Middle);
                    let font_id = ui.style().text_styles[&egui::TextStyle::Button].clone();
                    let galley =
                        ui.painter()
                            .layout_no_wrap(label.to_string(), font_id, text_color);
                    // Label and live dot are centred together as one group, so adding the
                    // dot doesn't lurch the label off-centre. Read the width before
                    // painting (the paint consumes the galley).
                    let label_w = galley.rect.width();
                    let dot_d = 5.0;
                    let gap = 6.0;
                    let total_w = label_w + if live { gap + dot_d } else { 0.0 };
                    let start_x = rect.center().x - total_w * 0.5;
                    let text_pos =
                        egui::pos2(start_x, rect.center().y - galley.rect.height() * 0.5);
                    ui.painter().galley(text_pos, galley, text_color);
                    if live {
                        // Same shallow breath curve as the status dot.
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
        // A slim accent underline marks the active tab, inset from each edge so it
        // reads as deliberate rather than a heavy full-width block. Its position is
        // tweened via `tab_slide`, so a switch slides it across the strip.
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

    /// The log panel along the bottom, collapsed by default and toggled by
    /// Ctrl/Cmd+L to half the window height.
    ///
    /// The height is tweened (see [`log_anim_step`]) so it slides rather than
    /// hard-cutting. The tween runs on the wall clock and stops requesting frames the
    /// moment it settles, so an idle window still falls asleep.
    fn log_panel(&mut self, ui: &mut Ui) {
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
        // Snapshot the lines (with arrival instants) up front, so the fade repaint
        // guard below is armed from the same data the panel draws.
        let lines = log::snapshot(&self.log);
        // While any line is still fading the panel wants display-rate frames; once all
        // are opaque it falls back to the session cadence.
        let any_fading = lines
            .iter()
            .any(|(_, at)| at.elapsed().as_secs_f32() < LOG_FADE_SECS);
        egui::Panel::bottom("log").exact_size(h).show(ui, |ui| {
            // A slim header with a close button, so the panel can be dismissed without
            // knowing the shortcut. Same toggle, so the tween slides it shut either way.
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
                        // Ease-out cubic from the line's arrival, so it appears quickly
                        // then settles gently rather than popping in. Older lines are
                        // already fully opaque.
                        let t = (added_at.elapsed().as_secs_f32() / LOG_FADE_SECS).clamp(0.0, 1.0);
                        let alpha = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
                        let base = log_line_color(line, default);
                        let color = egui::Color32::from_rgba_unmultiplied(
                            base.r(),
                            base.g(),
                            base.b(),
                            (base.a() as f32 * alpha) as u8,
                        );
                        // A faint hover highlight so a line is easy to track in a dense
                        // log. Painted before the content, so text and chip sit on top.
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
                        // A level chip (ERR/WARN) leads the line, fading in with it.
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
        // The height slide and the line fade both want display-rate frames.
        if log_moving || any_fading {
            ui.ctx().request_repaint();
        }
    }

    /// The floating status pill ("Copied ✓", "Relay refreshed ✓", …), anchored
    /// bottom-centre in its own foreground [`egui::Area`] so confirming an action never
    /// nudges the WHIP URL box, ticket box or stats panel.
    ///
    /// A copy toast takes precedence over an action toast when both are live. The
    /// repaint guard in [`App::draw`] keeps frames coming while either is up, so the
    /// fade completes exactly as repaints stop.
    fn toast_overlay(&self, ui: &Ui) {
        // `App::draw` already expired both, so anything still set here is live.
        let (text, at) = match (&self.copied, &self.toast) {
            (Some((_, at)), _) => ("Copied ✓", *at),
            (None, Some((msg, at))) => (msg.as_str(), *at),
            (None, None) => return,
        };
        let elapsed = at.elapsed();
        // Fade out over the remaining lifetime, rising 8 px over the first 200 ms.
        let alpha = (1.0 - elapsed.as_secs_f32() / TOAST.as_secs_f32()).clamp(0.0, 1.0);
        let enter = (elapsed.as_secs_f32() / 0.2).clamp(0.0, 1.0);
        let rise = (1.0 - enter * enter) * 8.0;
        // An error toast inverts to the red palette; everything else stays green. A
        // "Copied" toast (which takes precedence over `toast`) is always green.
        let is_error = self.toast_error && self.copied.is_none();
        let (fill_rgb, tint) = if is_error {
            ((44, 30, 32), ERROR)
        } else {
            ((30, 42, 36), ACCENT)
        };
        let frame = egui::Frame::popup(ui.style())
            .fill(egui::Color32::from_rgb(fill_rgb.0, fill_rgb.1, fill_rgb.2).gamma_multiply(alpha))
            .stroke(egui::Stroke::new(1.0, tint.gamma_multiply(alpha)))
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
                            .color(tint.gamma_multiply(alpha)),
                    );
                });
            });
    }
}

impl App {
    /// Lay out the whole window.
    ///
    /// Split out of [`eframe::App::ui`] (which only forwards here) so the real frame
    /// can be driven headlessly by tests without a window or GL context.
    pub fn draw(&mut self, ui: &mut Ui) {
        // Expire toasts before deciding on the next frame. A click later in this same
        // frame re-arms them with a fresh `Instant`, so this never races a click.
        self.copied = expire_copied(self.copied, TOAST);
        self.toast = self.toast.take().filter(|(_, at)| at.elapsed() < TOAST);

        let (broadcast, watch, refreshing) = {
            let g = self.state.lock().unwrap();
            (g.broadcast, g.watch, g.refreshing)
        };
        let any_active = broadcast.active() || watch.active();
        // Reset the eased stats when a broadcast goes idle, so the next one starts from
        // zero instead of counting down from the previous session's values.
        if !broadcast.active() {
            self.disp_viewers = 0.0;
            self.disp_video_kbps = 0.0;
            self.disp_video_fps = 0.0;
            self.disp_audio_kbps = 0.0;
        }
        // Pick up any completion notice a background task left (relay refresh, ticket
        // reset) and raise it as a toast.
        let notice = self.state.lock().unwrap().notice.take();
        if let Some(msg) = notice {
            self.toast = Some((msg, Instant::now()));
        }
        // Replace an invalid stored ticket (flagged and dropped in `App::new`) with a
        // freshly minted one: once, while nothing is running and no refresh is in
        // flight. Same identity as a manual refresh, so friends' saved tickets (from
        // before the garbage got written) still resolve.
        if self.regen_ticket && !any_active && !refreshing {
            self.regen_ticket = false;
            self.refresh_relay(ui.ctx(), "Replaced invalid ticket ✓");
        }
        // Slide the tab underline toward the selected tab, on the wall clock.
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().clamp(0.0, 0.1);
        self.last_frame = now;
        let target = f32::from(self.tab == Tab::Broadcast);
        let tab_moving = (self.tab_slide - target).abs() > 0.002;
        if tab_moving {
            self.tab_slide = tab_slide_step(self.tab_slide, target, dt);
        }
        // Fade the tab content in after a switch (ease-out over ~160 ms).
        if self.tab_fade_active {
            let (eased, settled) =
                tab_fade_step((now - self.tab_switched_at).as_secs_f32(), TAB_FADE_SECS);
            self.tab_fade = eased;
            self.tab_fade_active = !settled;
        }
        // The contact-row edit tween: one shared clock, stepped here (single owner) so
        // every row reads the same `p`. It runs 0→1 only while a row is opening
        // (`editing_contact`) or closing (`contact_edit_prev`) — never at rest, so an
        // open idle panel sleeps. The opened row paints at `p = anim`, the closing row
        // at `p = 1 - anim`; when the tween settles, the closing row is released so no
        // row keeps a dead `p` (a standing `prev` would pin its row expanded).
        let contact_edit_active =
            self.editing_contact.is_some() || self.contact_edit_prev.is_some();
        let contact_edit_moving = contact_edit_active && self.contact_edit_anim < 0.998;
        if contact_edit_moving {
            self.contact_edit_anim = tab_slide_step(self.contact_edit_anim, 1.0, dt);
        }
        if contact_edit_active && !contact_edit_moving {
            self.contact_edit_prev = None;
        }
        // Persist a ticket minted while idle (Refresh Relay / Reset Ticket) so the
        // "current ticket" survives restarts. Guarded to a real change so we never
        // rewrite config.json every frame; the broadcasting path persists its own.
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
        // Repaint while a track is live or a refresh is in flight, so controls
        // re-enable and the new ticket appears without input.
        if any_active || refreshing {
            ui.ctx().request_repaint_after(REPAINT);
        }
        // A fading toast is an animation, not a status poll: the session cadence would
        // give a 1.5 s fade only six frames. `request_repaint_after` keeps the shortest
        // duration asked for, so this wins over the request above.
        if self.copied.is_some() || self.toast.is_some() {
            ui.ctx().request_repaint_after(TOAST_FRAME);
        }
        // The underline slide, tab fade and spinner all want display-rate frames while
        // in flight, and nothing at all once settled.
        if tab_moving {
            ui.ctx().request_repaint();
        }
        if self.tab_fade_active {
            ui.ctx().request_repaint();
        }
        // The contact-row edit tween wants display-rate frames while in flight —
        // only while the panel is open (closed, it must not keep the window awake).
        if self.show_contacts && contact_edit_moving {
            ui.ctx().request_repaint();
        }
        // A loaded (active) contact row breathes its left bar on the shared pulse clock,
        // so the panel repaints on the session cadence — but only while it is open *and*
        // such a row is shown, so an open panel with no loaded contact still sleeps.
        if self.show_contacts
            && self
                .config
                .contacts
                .iter()
                .any(|c| !c.ticket.is_empty() && c.ticket == self.watch_ticket)
        {
            ui.ctx().request_repaint_after(REPAINT);
        }
        // Only the visible tab's spinner matters — the other tab's `Starting` track is
        // not drawn, so it must not keep the window awake.
        let spinner_visible = match self.tab {
            Tab::Broadcast => broadcast == Mode::Starting,
            Tab::Watch => watch == Mode::Starting,
        };
        if spinner_visible {
            ui.ctx().request_repaint();
        }

        // On close, abort any live session (frees the WHIP port, kills the player) and
        // persist config. The endpoint and runtime are dropped in `run`.
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            state::stop_all(&self.state, ui.ctx());
            self.config.save();
        }

        // Global keyboard shortcuts, consumed here so egui's default focus traversal
        // doesn't also fire on the same press.
        let ctx = ui.ctx();
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.show_options = !self.show_options;
            // The two overlays share the same space over the panels — never both.
            if self.show_options {
                self.show_contacts = false;
                self.editing_contact = None;
                self.contact_edit_prev = None;
                self.confirm_remove_contact = None;
            }
            ctx.request_repaint();
        } else if ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::CTRL, egui::Key::L)
                || i.consume_key(egui::Modifiers::COMMAND, egui::Key::L)
        }) {
            self.log_open = !self.log_open;
            ctx.request_repaint();
        } else if ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::CTRL, egui::Key::T)
                || i.consume_key(egui::Modifiers::COMMAND, egui::Key::T)
        }) {
            // Ctrl/Cmd+T toggles the Contacts overlay. It shares the panels with
            // Settings, so opening it takes Settings down; closing flushes edits.
            if self.show_contacts {
                self.close_contacts();
            } else {
                self.show_contacts = true;
                self.show_options = false;
            }
            ctx.request_repaint();
        } else if !self.show_options
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
        {
            self.switch_tab(match self.tab {
                Tab::Watch => Tab::Broadcast,
                Tab::Broadcast => Tab::Watch,
            });
            // egui armed a focus traversal from this Tab press before `draw` ran; cancel
            // it so the menu-bar Settings button doesn't also grab focus.
            ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
            ctx.request_repaint();
        }

        // The status bar sits at the very bottom, registered before the `log` panel so
        // the log stacks above it. Its fill is tinted by the live session's accent,
        // mirroring the status dot, so the bar carries the session state at a glance.
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
        let bar_top_margin = bar_frame.inner_margin.topf();
        egui::Panel::bottom("menu_bar")
            .frame(bar_frame)
            .show(ui, |ui| {
                // Hairline on the bar's top edge, painted on its own full-width layer so
                // it isn't clipped to the frame's inset content rect.
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
        if self.show_contacts {
            self.contacts_window(ui.ctx());
        }
        self.log_panel(ui);
        // A subtle vertical gradient is the central panel's ground, so the surface reads
        // as lit from above rather than a flat fill.
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(ui.style())
                    .fill(egui::Color32::TRANSPARENT)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                // The ground gradient, glow and vignette are painted on the *same* layer
                // as the panel content and *before* it, so paint order puts them behind.
                // They must not go on a separate background layer: in egui 0.36 the panel
                // content already lives on the background order, so a second background
                // layer has no guaranteed order and can land on top, painting the panel
                // blank. `ui.clip_rect()` is the panel's full visible rect (the frame's
                // inner margin insets the layout, not the clip).
                central_gradient(ui.painter(), ui.clip_rect(), 1.0);
                ambient_glow(ui.painter(), ui.clip_rect());
                vignette(ui.painter(), ui.clip_rect());
                self.tab_bar(ui);
                ui.add_space(8.0);
                // Only the content below the tab strip gets the fade overlay.
                let content_top = ui.cursor().min.y;
                match self.tab {
                    Tab::Watch => self.watch_tab(ui),
                    Tab::Broadcast => self.broadcast_tab(ui),
                }
                // Fade-in overlay: the ground colour painted *after* the content, fading
                // out over ~160 ms. A no-op at full alpha.
                if self.tab_fade < 1.0 {
                    let overlay = egui::Rect::from_min_max(
                        egui::pos2(ui.max_rect().min.x, content_top),
                        ui.max_rect().max,
                    );
                    central_gradient(ui.painter(), overlay, 1.0 - self.tab_fade);
                }
            });
        // Drawn last, on its own foreground layer, so it floats over the panels.
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
/// Extracted with the window length injected so the "idle window must fall asleep"
/// regression is testable without a live window.
pub fn expire_copied(
    copied: Option<(&'static str, Instant)>,
    toast: Duration,
) -> Option<(&'static str, Instant)> {
    copied.filter(|(_, at)| at.elapsed() < toast)
}

/// Truncate a string to `max` chars, adding an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// The status-dot breath: a shallow ~2 s pulse bounded to `0.6..=1.0` of the base
/// colour, so "live" reads at a glance without blinking out.
pub fn pulse_factor(secs: f32) -> f32 {
    0.6 + 0.4 * (0.5 + 0.5 * (secs * std::f32::consts::TAU * 0.5).cos())
}

/// The copy-box click flash: a linear fade from `1.0` to `0.0` over `window`.
pub fn flash_alpha(elapsed: Duration, window: Duration) -> f32 {
    if window.is_zero() {
        return 0.0;
    }
    (1.0 - elapsed.as_secs_f32() / window.as_secs_f32()).clamp(0.0, 1.0)
}

/// A rotating 270° arc, shown beside "connecting…" / "starting…" so the wait has
/// visible progress. Drawn as a mesh of quads (crisper than a stroked arc, and it
/// avoids the layer-ordering pitfalls of a separate background layer).
pub fn spinner(ui: &mut Ui, secs: f32, color: egui::Color32) {
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
        // Four corners of the i-th ring quad (vertices at `4*i .. 4*i+4`).
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

/// A faint 1 px "light catch" along the top edge of a card surface, inset at each
/// end by `corner_radius` so it never clips the rounded corners.
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

/// A very subtle vertical gradient for the central panel's ground: lighter at the top,
/// darker at the bottom, so the window reads as a surface lit from above. The endpoints
/// average to the panel fill, so it is a depth cue, not a recolor.
///
/// Must be painted on the panel content's own painter (not a separate background layer):
/// in egui 0.36 the panel content already lives on the background order, so a second
/// background layer has no guaranteed order and can land on top, blanking the panel.
///
/// `alpha` of 1.0 is the ground itself; a lower value is the tab fade-in's reveal
/// overlay, which matches the ground so the fade is seamless.
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

/// An ambient "light from above" glow at the top centre of the central panel:
/// concentric circles at a very low alpha, whose overlap brightens the centre.
/// Painted before any content, like [`central_gradient`].
fn ambient_glow(painter: &egui::Painter, rect: egui::Rect) {
    let center = egui::pos2(rect.center().x, rect.top());
    // A lighter shade of the ground at alpha 3, so this lights rather than darkens.
    let color = egui::Color32::from_rgba_unmultiplied(58, 64, 74, 3);
    // Overlapping circles brighten the centre and fade out radially.
    for radius in [40.0, 65.0, 90.0, 115.0, 140.0, 165.0] {
        painter.circle_filled(center, radius, color);
    }
}

/// A radial vignette: transparent at the centre, fading in toward the edges, so the
/// eye is drawn to the middle. The falloff is radial, so the corners (farthest out)
/// are darkest — a proper vignette, not a flat border. Painted before any content,
/// like [`central_gradient`].
fn vignette(painter: &egui::Painter, rect: egui::Rect) {
    let center = rect.center();
    // Reaches the farthest corner, so the whole panel is covered.
    let radius = (rect.size() * 0.5).length();
    if radius <= 0.0 {
        return;
    }
    let clear = egui::Color32::from_rgba_unmultiplied(12, 14, 18, 0);
    let edge = egui::Color32::from_rgba_unmultiplied(12, 14, 18, 16);
    let n = 32;
    let mut mesh = egui::epaint::Mesh::default();
    // Centre vertex (transparent), then one faint vertex per outer segment.
    mesh.colored_vertex(center, clear);
    for i in 0..n {
        let a = std::f32::consts::TAU * i as f32 / n as f32;
        let p = center + egui::vec2(a.cos() * radius, a.sin() * radius);
        mesh.colored_vertex(p, edge);
    }
    // A fan: one triangle from the centre to each outer segment.
    for i in 0..n {
        let a = 1 + i as u32;
        let b = 1 + ((i + 1) % n) as u32;
        mesh.add_triangle(0, a, b);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// A soft halo behind a primary action button: three stacked rounded rects falling off
/// from the button's edge. Paint this *before* the button so only the rim shows.
///
/// The hero buttons use a fixed `.fill()`, which suppresses egui's own feedback, so
/// this glow carries the hover (brighter) and press (dimmer, contracted) response.
fn primary_button_glow(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    hover: bool,
    pressed: bool,
) {
    let corner = egui::CornerRadius::same(10);
    // Pressed is dimmer than the resting halo (reads as pushed in); hover is brightest.
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

/// A soft glow around a secondary interactive surface (a copy box's hover, a ticket or
/// buffer field's focus). Two stacked rounded rects, dimmer than the hero buttons'
/// [`primary_button_glow`]; takes no layout space.
pub fn soft_glow(
    painter: &egui::Painter,
    rect: egui::Rect,
    corner: egui::CornerRadius,
    color: egui::Color32,
) {
    for (expand, alpha) in [(4.0, 0.03), (2.0, 0.06)] {
        painter.rect_filled(rect.expand(expand), corner, color.gamma_multiply(alpha));
    }
}

/// The status bar's fill: the base dark ground, tinted 6% by the live session's accent
/// so the bar carries the session state at a glance. Returns `base` unchanged when
/// there is no tint, so the resting look is untouched.
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
/// response (a fixed `.fill()` suppresses egui's own hover feedback).
fn pointer_over(ui: &Ui, rect: egui::Rect) -> bool {
    ui.ctx()
        .input(|i| i.pointer.latest_pos())
        .is_some_and(|p| rect.contains(p))
}

/// Whether the primary button is pressed over `rect` this frame. A press always implies
/// a hover, so callers can treat `pressed` as a refinement of [`pointer_over`].
fn pointer_pressed_over(ui: &Ui, rect: egui::Rect) -> bool {
    ui.ctx().input(|i| {
        i.pointer.button_pressed(egui::PointerButton::Primary)
            && i.pointer.latest_pos().is_some_and(|p| rect.contains(p))
    })
}

/// A sonar pulse: a soft ring expanding from a live indicator's dot and fading out,
/// looping on `period` seconds. Decorative — takes no layout space. `secs` is the
/// animation clock, which advances between the repaints that already fire while live.
pub fn sonar_pulse(
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

/// A compact "LIVE" badge for the status bar: an outlined pill in the session's accent
/// that breathes in lockstep with the status dot (`breath` is the same
/// [`pulse_factor`] driving it).
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
    // The breathing outline carries the signal; the faint fill keeps it from shouting.
    let corner = egui::CornerRadius::same(4);
    ui.painter()
        .rect_filled(rect, corner, color.gamma_multiply(0.10));
    ui.painter().rect_stroke(
        rect,
        corner,
        egui::Stroke::new(1.0, color.gamma_multiply(breath)),
        egui::StrokeKind::Inside,
    );
    let text_pos = egui::pos2(rect.center().x - tw * 0.5, rect.center().y - th * 0.5);
    ui.painter()
        .galley(text_pos, galley, color.gamma_multiply(breath.max(0.8)));
}

/// A compact named pill (e.g. the selected contact) beside a section heading: an
/// outlined rounded box in the given accent with the label centred. Unlike
/// [`live_badge`] it is static — a name is a fact, not a live signal — and it takes an
/// arbitrary label, which is why the "LIVE" badge above cannot be reused for it.
pub fn contact_chip(ui: &mut Ui, label: &str, color: egui::Color32) {
    let font_id = egui::FontId::proportional(10.5);
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), font_id, egui::Color32::PLACEHOLDER);
    let tw = galley.rect.width();
    let th = galley.rect.height();
    let pad_x = 6.0;
    let pad_y = 2.0;
    let (rect, _resp) = ui.allocate_exact_size(
        egui::vec2(tw + pad_x * 2.0, th + pad_y * 2.0),
        egui::Sense::hover(),
    );
    let corner = egui::CornerRadius::same(5);
    ui.painter()
        .rect_filled(rect, corner, color.gamma_multiply(0.12));
    ui.painter().rect_stroke(
        rect,
        corner,
        egui::Stroke::new(1.0, color.gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    let text_pos = egui::pos2(rect.center().x - tw * 0.5, rect.center().y - th * 0.5);
    ui.painter()
        .galley(text_pos, galley, color.gamma_multiply(0.92));
}

/// A section heading: a small accent bar beside a bold label.
pub fn section_tick(ui: &mut Ui, text: &str, color: egui::Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (bar, _) = ui.allocate_exact_size(egui::vec2(3.0, 12.0), egui::Sense::hover());
        // Nudge the bar down so it centres on the label's text (the row is top-aligned).
        ui.painter().rect_filled(
            bar.translate(egui::vec2(0.0, 2.0)),
            egui::CornerRadius::same(1),
            color,
        );
        ui.label(egui::RichText::new(text).strong());
    });
}

/// A soft error callout: a muted red-tinted surface with a red left bar (mirroring
/// [`section_tick`] in the error hue) and the message in the error red.
pub fn error_callout(ui: &mut Ui, text: &str) {
    let frame = egui::Frame::group(ui.style())
        .fill(ERROR.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, ERROR.gamma_multiply(0.35)))
        .inner_margin(egui::Margin::symmetric(10, 8));
    let inner = frame.show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (bar, _) = ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
            ui.painter().rect_filled(
                bar.translate(egui::vec2(0.0, 1.0)),
                egui::CornerRadius::same(1),
                ERROR,
            );
            ui.label(egui::RichText::new(text).color(ERROR));
        });
    });
    top_bevel(ui.painter(), inner.response.rect, frame.corner_radius.nw);
}

/// The display colour of a log line, keyed off its leading tracing level word
/// (`LEVEL target: message`). Info and unlevelled lines take the default colour.
pub fn log_line_color(line: &str, default: egui::Color32) -> egui::Color32 {
    match line.split_whitespace().next() {
        Some("ERROR") => ERROR,
        Some("WARN") => WARN_AMBER,
        Some("DEBUG") | Some("TRACE") => egui::Color32::from_rgb(122, 126, 132),
        _ => default,
    }
}

/// The muted amber a `WARN` line is coloured with, shared by [`log_line_color`] and
/// [`log_level_chip`] so a warning's text and chip always match.
pub const WARN_AMBER: egui::Color32 = egui::Color32::from_rgb(214, 178, 94);

/// A small level chip ("ERR" / "WARN") leading a log line, so a problem pops out of a
/// wall of grey text. INFO and unlevelled lines get no chip. `alpha` fades the chip in
/// with its line. Returns whether a chip was painted.
fn log_level_chip(ui: &mut Ui, line: &str, alpha: f32) -> bool {
    let (text, color) = match line.split_whitespace().next() {
        Some("ERROR") => ("ERR", ERROR),
        Some("WARN") => ("WARN", WARN_AMBER),
        _ => return false,
    };
    // Faint fill, steadier outline, near-full-strength text — all scaled by `alpha` so
    // the chip fades in with its line.
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

/// A hand-painted "×" for the log panel's header (two crossing strokes, brightening on
/// hover), so the panel can be dismissed without knowing Ctrl/Cmd+L. Hand-painted so it
/// doesn't depend on a glyph the bundled faces may not carry.
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
    // A 4 px inset keeps the two crossing diagonals inside the slot.
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

/// One step of the tab underline's exponential ease toward `target`, scaled by the
/// wall-clock `dt` so the slide's speed is frame-rate independent. Never overshoots,
/// and snaps on within ~a pixel so the caller can stop requesting repaints.
pub fn tab_slide_step(current: f32, target: f32, dt: f32) -> f32 {
    let next = current + (target - current) * (1.0 - (-dt * 30.0).exp());
    if (next - target).abs() <= 0.002 {
        target
    } else {
        next
    }
}

/// One step of the log panel's height tween (open fraction). Same shape as
/// [`tab_slide_step`] but slower, so the larger travel reads as a deliberate slide.
pub fn log_anim_step(current: f32, target: f32, dt: f32) -> f32 {
    let next = current + (target - current) * (1.0 - (-dt * 18.0).exp());
    if (next - target).abs() <= 0.002 {
        target
    } else {
        next
    }
}

/// One step of the live stats' eased display. Same shape as [`tab_slide_step`], slower
/// still; the 0.05 epsilon is well below one unit of every displayed stat, so the snap
/// is never visible.
pub fn stat_ease(current: f32, target: f32, dt: f32) -> f32 {
    let next = current + (target - current) * (1.0 - (-dt * 8.0).exp());
    if (next - target).abs() <= 0.05 {
        target
    } else {
        next
    }
}

/// The tab content's fade-in alpha after `elapsed` seconds, and whether it has settled.
/// An ease-out cubic, so new content appears quickly then settles gently. Pure (the
/// clock is injected) so the settle is testable headlessly.
pub fn tab_fade_step(elapsed: f32, duration: f32) -> (f32, bool) {
    let t = (elapsed / duration).clamp(0.0, 1.0);
    if t >= 1.0 {
        (1.0, true)
    } else {
        (1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t), false)
    }
}

/// Install the app's tuned dark theme once, before the first frame.
///
/// Pins a deliberate dark look (rather than egui's OS-following stock palette) with an
/// accent-tinted selection/hover. `all_styles_mut` retunes both variants so the palette
/// stays coherent if the theme is ever switched; `set_theme` makes dark the deterministic
/// default.
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

/// The window / task-bar icon: a rounded square (dark ground, lit from above) carrying a
/// green "stream" — a diagonal conduit with a soft glow and a flow arrow at its tip.
/// Generated in code, so there is no asset file to ship.
pub fn app_icon() -> Arc<egui::IconData> {
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

/// The colour of one icon pixel: the rounded-square ground (a vertical gradient, lit from
/// above), a soft glow around the stream, and the stream itself in the live accent.
/// Transparent outside the rounded square.
fn icon_pixel(x: f32, y: f32, size: f32) -> (u8, u8, u8, u8) {
    let (px, py) = (x + 0.5, y + 0.5); // centre the sample in the pixel
    let accent: (f32, f32, f32) = (80.0, 200.0, 120.0); // the live accent (ACCENT)

    let radius = size * 0.22;
    let d_bg = rounded_box_sdf(px, py, size, radius);
    if d_bg > 0.5 {
        return (0, 0, 0, 0); // outside the icon
    }
    let edge = (0.5 - d_bg).clamp(0.0, 1.0); // 1 px anti-aliasing
                                             // Ground: a vertical gradient, lighter at the top.
    let t = (py / size).clamp(0.0, 1.0);
    let (top, bot) = ((31.0, 34.0, 40.0), (19.0, 21.0, 25.0));
    let mut r = top.0 + (bot.0 - top.0) * t;
    let mut g = top.1 + (bot.1 - top.1) * t;
    let mut b = top.2 + (bot.2 - top.2) * t;

    // The stream: a thick diagonal capsule from lower-left to upper-right, with a glow.
    let a = (size * 0.30, size * 0.70);
    let tip = (size * 0.70, size * 0.30);
    let half = size * 0.085;
    let glow = size * 0.10;
    let d = dist_to_segment(px, py, a, tip);
    if d < half + glow {
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

/// A rounded box's signed distance field (negative inside) over `[0, size]²`.
fn rounded_box_sdf(px: f32, py: f32, size: f32, radius: f32) -> f32 {
    let (qx, qy) = (
        (px - size * 0.5).abs() - (size * 0.5 - radius),
        (py - size * 0.5).abs() - (size * 0.5 - radius),
    );
    (qx.max(0.0) * qx.max(0.0) + qy.max(0.0) * qy.max(0.0)).sqrt() + (qx.min(0.0).max(qy.min(0.0)))
        - radius
}

/// The distance from `(px, py)` to the segment `a..=b`.
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
/// Creates the runtimes, loads config, installs the tee logger, wires the stats channel
/// and consumer task, and runs eframe until the window closes.
///
/// Two runtimes: the primary hosts the UI's async work, the `streampipe-engine` one the
/// streaming engine. Media must not share threads with the renderer — the WHIP host polls
/// the ICE socket from a worker, and if that worker stalls the OS UDP receive buffer
/// overflows during a keyframe burst, which the viewer sees as smearing and decoding
/// errors.
pub fn run() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: unable to start runtime: {e}");
            std::process::exit(1);
        }
    };
    let handle = runtime.handle().clone();

    // The engine runtime: every media session runs here, on threads no UI repaint
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
    // Keep one sender alive for the whole session so the consumer never sees the
    // channel close between broadcast start/stops.
    let keepalive = stats_tx.clone();

    // The app gets the engine handle, so sessions land on the engine runtime while the
    // stats consumer stays on the GUI runtime with the renderer it repaints.
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

    // Close the stats channel so the consumer exits, then give any teardown (e.g. the
    // player kill task) a moment to run before the runtimes stop.
    drop(keepalive);
    engine_runtime.shutdown_timeout(Duration::from_millis(1000));
    runtime.shutdown_timeout(Duration::from_millis(1000));

    // A normal window close returns `Ok`. Report an `Err` as a failed launch so
    // launchers and scripts don't see false success.
    if let Err(e) = result {
        eprintln!("error: gui failed: {e:?}");
        std::process::exit(1);
    }
}
