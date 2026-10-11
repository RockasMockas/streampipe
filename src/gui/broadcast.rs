//! The Start Broadcasting tab: WHIP host/port/token, a start button, then the
//! click-to-copy WHIP URL and ticket plus live statistics.

use std::{
    net::{SocketAddrV4, SocketAddrV6},
    time::Instant,
};

use eframe::egui::{self, Context, Ui};
use iroh::SecretKey;

use super::{
    config::Config,
    flash_alpha,
    state::{self, BindAddrs, BroadcastParams, Mode},
    App, ACCENT, ERROR, FLASH, error_callout, pointer_over, pointer_pressed_over,
    primary_button_glow, pulse_factor, section_tick,
    sonar_pulse, soft_glow, spinner, stat_ease, top_bevel,
};

/// Horizontal room kept clear at the right edge of a copy box for the copy glyph,
/// so a long value is elided before it reaches the mark.
const GLYPH_GUTTER: f32 = 26.0;
/// Height of the value line inside a copy box.
const COPY_ROW_H: f32 = 22.0;
/// Vertical inner margin of a copy box's frame; doubled, it pads the value line.
const COPY_V_MARGIN: f32 = 6.0;
/// Horizontal inner margin of a copy box's frame; the frame's outer width is the
/// value line's `width` plus twice this, so callers must reserve it to avoid the
/// box (and any trailing button) overflowing past the right edge.
const COPY_H_MARGIN: f32 = 10.0;

impl App {
    /// Render the Start Broadcasting tab.
    pub fn broadcast_tab(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        let active = snap.broadcast.active();
        let broadcasting = snap.broadcast == Mode::Broadcasting;
        let starting = snap.broadcast == Mode::Starting;
        // Read window focus up front so the idle start button can autofocus itself
        // on the first focused frame (immutable read; the layout borrows follow).
        let window_focused = ui.input(|i| i.focused);

        // Advanced settings: host, port, bearer token and optional fixed bind
        // sockets, prefilled. Collapsed by default so the common case is just the
        // start button. Each field is a full-width box with its label stacked above
        // it, at a larger (1.3×) font, so the panel reads as one unified column.
        egui::CollapsingHeader::new("Advanced settings")
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!active, |ui| {
                    let big = ui.style().text_styles[&egui::TextStyle::Body].size * 1.3;
                    let prop = egui::FontId::proportional(big);
                    let box_h = big * 1.3;
                    // Full-width editable field with its label on the line above.
                    let field = |ui: &mut Ui,
                                     label: &str,
                                     hint: Option<&str>,
                                     text: &mut String| {
                        ui.label(egui::RichText::new(label).size(big));
                        let mut te = egui::TextEdit::singleline(text)
                            .font(prop.clone())
                            .vertical_align(egui::Align::Center);
                        if let Some(h) = hint {
                            te = te.hint_text(egui::RichText::new(h).size(big));
                        }
                        ui.add_sized([ui.available_width(), box_h], te);
                    };

                    field(ui, "WHIP host", None, &mut self.config.broadcast_host);

                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("WHIP port").size(big));
                    let mut port = self.config.broadcast_port.to_string();
                    let resp = ui.add_sized(
                        [ui.available_width(), box_h],
                        egui::TextEdit::singleline(&mut port)
                            .font(prop.clone())
                            .vertical_align(egui::Align::Center),
                    );
                    if resp.changed() {
                        if let Ok(p) = port.trim().parse::<u16>() {
                            if p >= 1 {
                                self.config.broadcast_port = p;
                            }
                        }
                    }

                    ui.add_space(8.0);
                    let mut token = self.config.bearer_token.clone().unwrap_or_default();
                    field(ui, "Bearer token", Some("none"), &mut token);
                    if token != self.config.bearer_token.clone().unwrap_or_default() {
                        self.config.bearer_token = if token.trim().is_empty() {
                            None
                        } else {
                            Some(token)
                        };
                    }

                    // Optional fixed bind sockets. Empty (the default) binds an
                    // ephemeral port and the ticket changes each session. A
                    // streamer with a static public IP can pin `ip:port` here so
                    // the ticket stays byte stable across sessions.
                    ui.add_space(8.0);
                    let mut v4 = self
                        .config
                        .broadcast_bind_ipv4
                        .clone()
                        .unwrap_or_default();
                    field(
                        ui,
                        "Bind IPv4:",
                        Some("e.g. 203.0.113.7:45678"),
                        &mut v4,
                    );
                    self.config.broadcast_bind_ipv4 = if v4.trim().is_empty() {
                        None
                    } else {
                        Some(v4)
                    };

                    ui.add_space(8.0);
                    let mut v6 = self
                        .config
                        .broadcast_bind_ipv6
                        .clone()
                        .unwrap_or_default();
                    field(
                        ui,
                        "Bind IPv6:",
                        Some("e.g. [2001:db8::1]:45678"),
                        &mut v6,
                    );
                    self.config.broadcast_bind_ipv6 = if v6.trim().is_empty() {
                        None
                    } else {
                        Some(v6)
                    };
                });
            });

        // Once live, reveal the two click-to-copy boxes and the statistics. While
        // idle, still surface the saved ticket so it can be copied and refreshed for
        // the next broadcast — but never offer a refresh live, since rebinding the
        // endpoint mid-stream would tear down the running session. These sit above
        // the primary action, which is pinned to the bottom of the tab.
        if broadcasting {
            ui.add_space(12.0);
            section_tick(ui, "Give OBS this WHIP server URL:", ACCENT);
            let w = (ui.available_width() - COPY_H_MARGIN * 2.0).max(0.0);
            self.copy_box(ui, "whip", &snap.whip_url, w);
            ui.add_space(6.0);
            section_tick(ui, "Send friends this stream ticket:", ACCENT);
            let ticket = snap.ticket.clone().unwrap_or_else(|| "…".into());
            let w = (ui.available_width() - COPY_H_MARGIN * 2.0).max(0.0);
            self.copy_box(ui, "ticket", &ticket, w);

            // Remember the ticket we are actually showing, so the most recent
            // broadcast survives restarts (for reference / history). This is *not*
            // re-advertised as the live ticket on the next broadcast — that is
            // always freshly minted from the live endpoint address so viewers get
            // current transport hints. Guarded to a real change so we never
            // rewrite config.json every frame.
            let changed = snap
                .ticket
                .as_ref()
                .map(|t| Some(t) != self.config.broadcast_ticket.as_ref())
                .unwrap_or(false);
            if changed {
                self.config.broadcast_ticket = snap.ticket.clone();
                self.config.save();
            }

            ui.add_space(12.0);
            self.stats_panel(ui, &snap);
        } else {
            ui.add_space(12.0);
            section_tick(ui, "Send friends this stream ticket:", ACCENT);
            let ticket = snap
                .ticket
                .clone()
                .or_else(|| self.config.broadcast_ticket.clone())
                .unwrap_or_else(|| "no ticket yet — press Refresh Relay".into());
            let requested =
                self.ticket_row(ui, "ticket", &ticket, !active, "Refresh Relay", "Refreshing…");
            if requested {
                self.refresh_relay(ui.ctx());
            }
        }

        if let Some(err) = &snap.broadcast_error {
            ui.add_space(6.0);
            error_callout(ui, err);
        }

        // The primary action is pinned to the bottom of the tab, full width, so it
        // reads as the one big commitment after the (optional) settings and the
        // shareable ticket/stats above it.
        ui.add_space(12.0);
        let bw = ui.available_width();
        if broadcasting || starting {
            // While OBS is actually delivering media, show nothing here: the
            // statistics panel already proves the stream is live. The "waiting"
            // hint only appears until the first media arrives from OBS, and a
            // live viewer count in the accent colour reads at a glance once
            // viewers join.
            let viewers = snap.stats.map(|s| s.viewers).unwrap_or(0);
            let receiving = snap
                .stats
                .is_some_and(|s| s.video_kbps > 0.0 || s.audio_kbps > 0.0);
            if starting {
                // A small rotating arc beside the hint so the wait for OBS to
                // deliver has visible progress instead of a static label.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    spinner(ui, self.pulse.elapsed().as_secs_f32(), ACCENT);
                    ui.label("starting…");
                });
            } else if viewers > 0 {
                ui.colored_label(
                    ACCENT,
                    format!(
                        "{viewers} viewer{} live",
                        if viewers == 1 { "" } else { "s" }
                    ),
                );
            } else if !receiving {
                // A small breathing dot (the same "live reads as an active signal"
                // language as the status and viewers dots) so the wait for OBS to
                // deliver reads as an active wait rather than a static label. The dot
                // breathes on the shared pulse clock and carries a soft sonar ring.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let secs = self.pulse.elapsed().as_secs_f32();
                    let (dot_rect, _) =
                        ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                    let center = dot_rect.center();
                    let dot = ACCENT.gamma_multiply(pulse_factor(secs));
                    ui.painter().circle_filled(center, 4.0, dot);
                    sonar_pulse(ui.painter(), center, 4.0, secs, 2.0, ACCENT);
                    ui.label("Waiting for OBS to start streaming to whip address...");
                });
            }
            // A subtle red halo behind the Stop button, so the one control that tears
            // down a live stream reads as a "stop" action at a glance (the same muted
            // red the tab uses for errors). The halo also contracts on press, so the
            // teardown reads as physically pressed in.
            let stop_rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(bw, 36.0));
            let hovered = pointer_over(ui, stop_rect);
            let pressed = pointer_pressed_over(ui, stop_rect);
            primary_button_glow(ui.painter(), stop_rect, ERROR, hovered, pressed);
            if ui
                .add_sized([bw, 36.0], egui::Button::new("■  Stop Broadcasting"))
                .clicked()
            {
                state::stop(&self.state, ui.ctx(), state::Track::Broadcast);
                self.config.save();
            }
        } else {
            // A soft accent halo behind the hero action, so the one big commitment
            // reads as lit by the live green rather than a flat button. Painted
            // before the button so the button covers the halo's centre. The fixed
            // `.fill()` below suppresses egui's own hover feedback, so the hover is
            // read from the pointer and lifts both the halo and the fill. The press
            // is read the same way and pushes the button in (fill darkens, halo
            // contracts), so the hero action feels tactile rather than flat.
            let start_rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(bw, 36.0));
            let hovered = pointer_over(ui, start_rect);
            let pressed = pointer_pressed_over(ui, start_rect);
            primary_button_glow(ui.painter(), start_rect, ACCENT, hovered, pressed);
            let button = egui::Button::new("●  Start Broadcasting")
                .fill(ACCENT.gamma_multiply(if pressed {
                    0.20
                } else if hovered {
                    0.26
                } else {
                    0.18
                }))
                .stroke(egui::Stroke::new(
                    1.0,
                    ACCENT.gamma_multiply(if pressed {
                        0.50
                    } else if hovered {
                        0.75
                    } else {
                        0.55
                    }),
                ));
            // Autofocus the start control on the first window-focused idle frame so
            // Enter/Space starts it; the Stop button (shown while active) is never
            // autofocused, so a stray Enter can never tear down a live stream. The
            // button's auto id is stable across frames, so focus requested off
            // `resp` sticks to it without needing an explicit id.
            let resp = ui.add_sized([bw, 36.0], button);
            if self.focus_primary && window_focused {
                if !active {
                    resp.request_focus();
                }
                self.focus_primary = false;
            }
            // Mouse click and keyboard (Space/Enter on the focused button) both
            // arrive via `clicked()` — a single trigger, no parallel Enter gate.
            if resp.clicked() {
                self.start_broadcast(ui.ctx());
            }
        }
    }

    /// A ticket copy box with an optional trailing button on its right edge.
    ///
    /// When `allow` is false the row is just the full-width copy box (used while
    /// broadcasting, where a live rebind would tear down the running session). The
    /// button shows `label`, or `busy_label` while a rebind is already in flight, and
    /// is disabled meanwhile. Returns `true` on the frame the button is clicked, so the
    /// caller dispatches the action (relay refresh vs. full ticket reset).
    pub(crate) fn ticket_row(
        &mut self,
        ui: &mut Ui,
        key: &'static str,
        text: &str,
        allow: bool,
        label: &str,
        busy_label: &str,
    ) -> bool {
        if !allow {
            let w = (ui.available_width() - COPY_H_MARGIN * 2.0).max(0.0);
            self.copy_box(ui, key, text, w);
            return false;
        }
        let refreshing = self.state.lock().unwrap().refreshing;
        ui.horizontal(|ui| {
            // Reserve room for the button (and the box frame's horizontal margins) so
            // the box neither overflows the row nor pushes the button offscreen.
            let reserve = 120.0;
            let w = (ui.available_width()
                - reserve
                - ui.spacing().item_spacing.x
                - COPY_H_MARGIN * 2.0)
                .max(0.0);
            self.copy_box(ui, key, text, w);
            // Match the box's rendered height so the button sits flush with it, and
            // tint it with the same accent so the pair reads as one control group.
            let h = self.copy_box_height();
            let label = if refreshing { busy_label } else { label };
            let btn = egui::Button::new(label)
                .min_size(egui::vec2(reserve, h))
                .fill(ACCENT.gamma_multiply(0.10))
                .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.5)));
            ui.add_enabled(!refreshing, btn).clicked()
        })
        .inner
    }

    /// The rendered height of a [`App::copy_box`] row (text line plus the frame's
    /// vertical inner margins), used to size a trailing button to sit flush with it.
    fn copy_box_height(&self) -> f32 {
        COPY_ROW_H + COPY_V_MARGIN * 2.0
    }

    /// Rebind the endpoint (same identity) and mint a fresh saved ticket, picking up a
    /// new relay / ephemeral port hints for the next broadcast.
    fn refresh_relay(&mut self, ctx: &Context) {
        let bind = match parse_bind(&self.config) {
            Ok(bind) => bind,
            Err(e) => {
                self.state.lock().unwrap().broadcast_error = Some(e);
                ctx.request_repaint();
                return;
            }
        };
        self.config.save();
        state::spawn_refresh_ticket(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            bind,
            "Relay refreshed ✓",
        );
    }

    /// Mint a brand-new identity: generate a fresh keypair, persist it, then rebind and
    /// mint a ticket from it. Unlike a relay refresh this changes the endpoint id, so
    /// previously shared tickets stop resolving to this stream.
    pub(crate) fn reset_ticket(&mut self, ctx: &Context) {
        let bind = match parse_bind(&self.config) {
            Ok(bind) => bind,
            Err(e) => {
                self.state.lock().unwrap().broadcast_error = Some(e);
                ctx.request_repaint();
                return;
            }
        };
        let secret = SecretKey::generate();
        self.config.secret_hex = Some(data_encoding::HEXLOWER.encode(&secret.to_bytes()));
        self.config.save();
        state::spawn_reset_ticket(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            secret,
            bind,
            "Ticket reset ✓",
        );
    }


    /// A read-only, selectable box that copies its text on click.
    ///
    /// `width` is the box's laid-out width; callers pass `ui.available_width()` for
    /// a full-row box, or a reduced width when a trailing widget (the refresh button)
    /// shares the row.
    ///
    /// The "Copied" confirmation is drawn by [`App::toast_overlay`] as a floating
    /// pill, so it never reflows the surrounding layout. This widget owns the
    /// click and the hover affordance that advertises it.
    fn copy_box(&mut self, ui: &mut Ui, key: &'static str, text: &str, width: f32) {
        // A faint accent tint reads the box as an interactive input surface. The
        // whole frame is the click target (not just the glyphs), so the copy
        // affordance is easy to hit and the tinted padding never feels dead.
        // A click briefly lifts the tint toward the accent (see `flash_alpha`).
        // `copied` is `Copy`, so reading it here leaves it intact for the click
        // handler below; the click's repaint shows the flash from the next frame.
        let flash = self
            .copied
            .filter(|(k, _)| *k == key)
            .map(|(_, at)| flash_alpha(at.elapsed(), FLASH))
            .unwrap_or(0.0);
        let fill_alpha = (14.0 + flash * 70.0) as u8;
        let frame = egui::Frame::group(ui.style())
            .fill(egui::Color32::from_rgba_unmultiplied(
                80, 200, 120, fill_alpha,
            ))
            .inner_margin(egui::Margin::symmetric(
                COPY_H_MARGIN as i8,
                COPY_V_MARGIN as i8,
            ));
        let inner = frame.show(ui, |ui| {
            // Reserve a gutter on the right for the copy glyph so a long ticket
            // neither runs under the mark nor off the box. The full string is still
            // copied on click; only the *display* is elided.
            let text_width = (width - GLYPH_GUTTER).max(0.0);
            let display = fit_monospace(ui, text, text_width);
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());
            let color = ui.style().visuals.text_color();
            let galley = ui
                .painter()
                .layout_no_wrap(display, font_id, egui::Color32::PLACEHOLDER);
            // Lay the value out ourselves at the row's left edge rather than via
            // `add_sized`, which was offsetting the galley toward the centre and
            // spilling long values past the box.
            let row = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, COPY_ROW_H));
            ui.allocate_rect(row, egui::Sense::hover());
            let y = row.center().y - galley.rect.height() * 0.5;
            ui.painter().galley(egui::pos2(row.min.x, y), galley, color);
        });
        // Sense the click over the frame's full rect so the padding is live too.
        // `on_hover_text` consumes the response, so interactions are read off it
        // before it is handed to the tooltip.
        let rect = inner.response.rect;
        // Whether the pointer is over the box this frame, for the hover affordance.
        // Read from the pointer (rather than the response, which is computed below)
        // so the copy glyph and the soft glow can be lit before the interaction is
        // sensed. For a stable rect this agrees with `resp.hovered()` below.
        let hovered = pointer_over(ui, rect);
        // A faint light catch along the top edge, so the box reads as a surface lit
        // from above rather than a flat tint. Painted first, so the copy glyph and
        // the hover stroke (drawn next) sit on top of it.
        top_bevel(ui.painter(), rect, frame.corner_radius.nw);
        // A soft accent glow around the box on hover, so the copy affordance reads as
        // lit and alive under the mouse (the same halo language as the hero buttons,
        // but dimmer: the box is a secondary surface, not the hero action). Drawn
        // before the glyph so the glyph sits on top of the glow.
        if hovered {
            soft_glow(ui.painter(), rect, frame.corner_radius, ACCENT);
        }
        // A small "copy" glyph (two overlapping sheets) at the right edge, painted
        // non-interactively so it never steals the box's click. It brightens on hover
        // (0.5 -> 0.8) so the "click to copy" affordance reads at a glance. The moment
        // the text is copied it cross-fades into a checkmark for the flash's life —
        // the confirmation right at the point of action, not only in the toast far
        // below — and fades back to the sheets as the flash fades. `flash` (0..1)
        // drives both sides: the checkmark's strength and the sheets' dimming.
        let rest = if hovered { 0.8f32 } else { 0.5f32 };
        let sheets = rest.max(flash) * (1.0 - flash);
        if flash > 0.005 {
            for seg in checkmark_segments(rect) {
                ui.painter()
                    .line_segment(seg, egui::Stroke::new(1.5, ACCENT.gamma_multiply(flash)));
            }
        }
        if sheets > 0.005 {
            let glyph = ACCENT.gamma_multiply(sheets);
            for sheet in copy_glyph_rects(rect) {
                ui.painter().rect_stroke(
                    sheet,
                    1.0,
                    egui::Stroke::new(1.0, glyph),
                    egui::StrokeKind::Inside,
                );
            }
        }
        let resp = ui.interact(rect, ui.id().with(("copy", key)), egui::Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_stroke(
                rect,
                frame.corner_radius,
                egui::Stroke::new(1.0, ui.style().visuals.selection.stroke.color),
                egui::StrokeKind::Inside,
            );
        }
        if resp.clicked() {
            ui.ctx().copy_text(text.to_string());
            self.copied = Some((key, Instant::now()));
            ui.ctx().request_repaint();
        }
        resp.on_hover_text("click to copy");
    }

    /// The live statistics panel: viewers, bitrate, fps, elapsed.
    fn stats_panel(&mut self, ui: &mut Ui, snap: &state::UiSnapshot) {
        let inner = ui.group(|ui| {
            section_tick(ui, "Statistics", ACCENT);
            let stats = snap.stats.unwrap_or_default();
            // Ease the displayed measurements toward the latest sample, so the numbers
            // settle smoothly instead of jumping on each 5-second tick. The elapsed
            // counter is shown exactly (it is a clock, not a measurement), so only the
            // measured stats are eased. `self.last_frame` was set at the top of the
            // frame in `draw`, so this is the current frame's wall-clock delta.
            let dt = (Instant::now() - self.last_frame).as_secs_f32().clamp(0.0, 0.1);
            let (viewers, video_kbps, video_fps, audio_kbps) = (
                stat_ease(self.disp_viewers, stats.viewers as f32, dt),
                stat_ease(self.disp_video_kbps, stats.video_kbps as f32, dt),
                stat_ease(self.disp_video_fps, stats.video_fps as f32, dt),
                stat_ease(self.disp_audio_kbps, stats.audio_kbps as f32, dt),
            );
            self.disp_viewers = viewers;
            self.disp_video_kbps = video_kbps;
            self.disp_video_fps = video_fps;
            self.disp_audio_kbps = audio_kbps;
            // While the numbers are still settling, ask for display-rate frames so the
            // ease is smooth; the moment they all settle the guard disarms and the panel
            // falls back to the 250 ms session cadence (the window is already awake while
            // broadcasting, so this only tightens the cadence briefly after each tick).
            let settled = (viewers - stats.viewers as f32).abs() <= 0.05
                && (video_kbps - stats.video_kbps as f32).abs() <= 0.05
                && (video_fps - stats.video_fps as f32).abs() <= 0.05
                && (audio_kbps - stats.audio_kbps as f32).abs() <= 0.05;
            if !settled {
                ui.ctx().request_repaint();
            }
            egui::Grid::new("stats_grid")
                .num_columns(2)
                .spacing([18.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Viewers");
                    viewers_cell(
                        ui,
                        stats.viewers,
                        viewers,
                        self.pulse.elapsed().as_secs_f32(),
                    );
                    ui.end_row();

                    ui.label("Video");
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.strong(
                            egui::RichText::new(format!(
                                "{:.0} kbit/s · {:.1} fps",
                                video_kbps, video_fps
                            ))
                            .monospace(),
                        );
                        // A small trend arrow showing the bitrate's recent drift, so a
                        // wobbly stream's direction (building, dropping, holding) is
                        // readable at a glance without reading the number.
                        trend_arrow(ui, bitrate_trend(&snap.video_kbps_history));
                    });
                    ui.end_row();

                    ui.label("Audio");
                    ui.strong(
                        egui::RichText::new(format!("{:.0} kbit/s", audio_kbps)).monospace(),
                    );
                    ui.end_row();

                    ui.label("Elapsed");
                    ui.strong(egui::RichText::new(format_elapsed(stats.elapsed_secs)).monospace());
                    ui.end_row();
                });
            // A sparkline of the recent video bitrate, so a wobbly stream is visible
            // at a glance without reading the number. Drawn only once two samples
            // exist (the first 5-second tick alone is not enough to form a line).
            if snap.video_kbps_history.len() >= 2 {
                ui.add_space(6.0);
                bitrate_sparkline(ui, &snap.video_kbps_history, self.pulse.elapsed().as_secs_f32());
            }
        });
        // A faint light catch along the panel's top edge, matching the copy boxes,
        // so the panel reads as a surface lit from above.
        top_bevel(
            ui.painter(),
            inner.response.rect,
            ui.style().visuals.widgets.inactive.corner_radius.nw,
        );
    }

    /// Resolve the configured host/port and start a broadcast session.
    fn start_broadcast(&mut self, ctx: &Context) {
        let host = self.config.broadcast_host.trim().to_string();
        let port = self.config.broadcast_port;
        if host.is_empty() {
            self.state.lock().unwrap().broadcast_error = Some("enter a WHIP host".into());
            ctx.request_repaint();
            return;
        }
        let url = state::whip_url(&host, port);
        let listen = format!("{host}:{port}");
        let bearer_token = self.config.bearer_token();

        // Parse the optional fixed bind addresses. Empty means ephemeral; a bad
        // value is surfaced to the UI rather than silently ignored, so a typo does
        // not quietly fall back to an ephemeral socket and a changing ticket.
        let bind = match parse_bind(&self.config) {
            Ok(bind) => bind,
            Err(e) => {
                self.state.lock().unwrap().broadcast_error = Some(e);
                ctx.request_repaint();
                return;
            }
        };

        self.config.save();

        let handle = state::spawn_broadcast(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            BroadcastParams {
                listen,
                url,
                bearer_token,
                bind,
            },
            self.stats_tx.clone(),
        );
        self.state.lock().unwrap().broadcast_session = Some(handle);
    }
}

/// Parse the configured fixed bind addresses, `None`/empty meaning ephemeral.
///
/// IPv4 is parsed as `ip:port`; IPv6 as `[ip]:port` (the brackets are optional, so
/// a bare `::1:45678` is rejected in favour of the explicit `[::1]:45678` form,
/// matching `SocketAddrV6`'s expected syntax).
fn parse_bind(config: &Config) -> Result<BindAddrs, String> {
    let ipv4 = match config.broadcast_bind_ipv4.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Some(s.parse::<SocketAddrV4>().map_err(|e| {
            format!("invalid Bind IPv4 {s:?}: {e} (expected ip:port)")
        })?),
        _ => None,
    };
    let ipv6 = match config.broadcast_bind_ipv6.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Some(s.parse::<SocketAddrV6>().map_err(|e| {
            format!("invalid Bind IPv6 {s:?}: {e} (expected [ip]:port)")
        })?),
        _ => None,
    };
    Ok(BindAddrs { ipv4, ipv6 })
}

/// The Viewers cell: a large count with a status dot.
///
/// With viewers it reads bright green and the dot breathes slowly so "live" is
/// obvious at a glance; at zero both sit dim, so an idle broadcast does not shout.
/// `viewers` (the true count) drives the dot and sonar; `display` is the eased count
/// shown as the number, so the value counts up smoothly as viewers join. `secs`
/// drives the breath and is expected to advance between repaints (repaints already
/// fire while broadcasting).
fn viewers_cell(ui: &mut Ui, viewers: usize, display: f32, secs: f32) {
    let green = ACCENT;
    let (dot, count) = if viewers > 0 {
        // A shallow 2-second breath: 1.0 down to 0.6, never invisible.
        let breath = 0.6 + 0.4 * (0.5 + 0.5 * (secs * std::f32::consts::TAU * 0.5).cos());
        (green.gamma_multiply(breath), green)
    } else {
        (
            egui::Color32::from_rgb(110, 110, 110),
            egui::Color32::from_rgb(150, 150, 150),
        )
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let dot_resp = ui.label(egui::RichText::new("●").size(11.0).color(dot));
        // A soft sonar pulse around the dot while viewers are live, so "live" reads
        // as an active signal rather than a static glyph.
        if viewers > 0 {
            sonar_pulse(ui.painter(), dot_resp.rect.center(), 5.0, secs, 2.0, green);
        }
        ui.label(
            egui::RichText::new(format!("{:.0}", display))
                .strong()
                .size(20.0)
                .color(count),
        );
    });
}

/// Map bitrate samples (kbit/s, oldest first) to pixel points spanning `rect`:
/// the oldest sample at the left edge, newest at the right, auto-scaled to the
/// window's peak with a 60 kbit/s floor so a quiet but steady stream still
/// shows its shape instead of hugging the bottom edge.
fn sparkline_points(samples: &[f64], rect: egui::Rect) -> Vec<egui::Pos2> {
    let max = samples.iter().copied().fold(0.0, f64::max).max(60.0);
    samples
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = rect.min.x + rect.width() * i as f32 / (samples.len() - 1) as f32;
            let y = rect.max.y - 2.0 - (v / max) as f32 * (rect.height() - 6.0);
            egui::pos2(x, y)
        })
        .collect()
}

/// The direction of the video bitrate over the recent samples: a small visual cue
/// beside the Video stat so a wobbly stream's drift (building up, dropping, or
/// holding steady) is readable at a glance without reading the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trend {
    /// The bitrate is building up over the recent samples.
    Rising,
    /// The bitrate is dropping over the recent samples.
    Falling,
    /// The bitrate is holding steady (within the noise band).
    Flat,
}

/// Classify the video bitrate's recent direction from its samples (kbit/s, oldest
/// first). The newest sample is compared against the sample up to three ticks back
/// (about fifteen seconds at the 5-second stats tick); a change larger than a small
/// relative band (3% of the older sample, floored at 5 kbit/s) reads as rising or
/// falling, and anything within the band — or too few samples to judge — reads as
/// flat. Pure (the samples are injected) so the classification is testable without a
/// live window.
pub(crate) fn bitrate_trend(samples: &[f64]) -> Trend {
    if samples.len() < 2 {
        return Trend::Flat;
    }
    let back = 3usize.min(samples.len() - 1);
    let prev = samples[samples.len() - 1 - back];
    let last = *samples.last().unwrap();
    let delta = last - prev;
    // A small relative band, so a steady stream does not flicker between arrows on
    // normal jitter, but a real drift (a viewer joining, a codec switching) shows.
    let band = prev.abs() * 0.03 + 5.0;
    if delta > band {
        Trend::Rising
    } else if delta < -band {
        Trend::Falling
    } else {
        Trend::Flat
    }
}

/// A small hand-painted trend arrow for the Video stat: an up triangle in the live
/// accent (rising), a down triangle in the error red (falling), or a short dash in a
/// dim grey (flat). Hand-painted (a mesh / line segment) rather than a text glyph, so
/// it does not depend on a symbol the bundled faces may not carry — the same reason
/// the copy glyph and the close button are drawn by hand. Purely decorative: it takes
/// a hover-sense slot but is never clicked.
fn trend_arrow(ui: &mut Ui, trend: Trend) {
    let size = 9.0;
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let c = rect.center();
    let color = match trend {
        Trend::Rising => ACCENT,
        Trend::Falling => ERROR,
        Trend::Flat => egui::Color32::from_rgb(130, 133, 140),
    };
    let p = ui.painter();
    match trend {
        Trend::Rising => {
            let (h, w) = (size * 0.4, size * 0.45);
            let mut m = egui::epaint::Mesh::default();
            m.colored_vertex(egui::pos2(c.x, c.y - h), color);
            m.colored_vertex(egui::pos2(c.x - w, c.y + h), color);
            m.colored_vertex(egui::pos2(c.x + w, c.y + h), color);
            m.add_triangle(0, 1, 2);
            p.add(egui::Shape::mesh(m));
        }
        Trend::Falling => {
            let (h, w) = (size * 0.4, size * 0.45);
            let mut m = egui::epaint::Mesh::default();
            m.colored_vertex(egui::pos2(c.x, c.y + h), color);
            m.colored_vertex(egui::pos2(c.x - w, c.y - h), color);
            m.colored_vertex(egui::pos2(c.x + w, c.y - h), color);
            m.add_triangle(0, 1, 2);
            p.add(egui::Shape::mesh(m));
        }
        Trend::Flat => {
            p.line_segment(
                [
                    egui::pos2(c.x - size * 0.3, c.y),
                    egui::pos2(c.x + size * 0.3, c.y),
                ],
                egui::Stroke::new(1.5, color),
            );
        }
    }
}

/// A small video-bitrate sparkline for the statistics panel: a vertical
/// gradient area fill under a 1.5 px line, with a small "now" dot on the newest
/// sample. Purely decorative of data the panel already shows as a number, so it
/// is non-interactive apart from a hover tooltip. `secs` drives the "now" dot's
/// sonar pulse and is expected to advance between repaints (repaints already fire
/// while broadcasting), so the live edge reads as an active signal rather than a
/// static mark.
fn bitrate_sparkline(ui: &mut Ui, samples: &[f64], secs: f32) {
    let width = ui.available_width();
    let height = 26.0;
    if samples.len() < 2 || width <= 0.0 {
        return;
    }
    let now = samples.last().copied().unwrap_or(0.0);
    // A small "now" value, right-aligned above the chart, so the current bitrate is
    // readable at a glance without hovering (the tooltip keeps the fuller two-minute
    // context). Weak and monospaced so it reads as a caption, not a competing number.
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(
            egui::RichText::new(format!("now {now:.0} kbit/s"))
                .monospace()
                .size(10.0)
                .weak(),
        );
    });
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    if rect.width() <= 0.0 {
        return;
    }
    let points = sparkline_points(samples, rect);
    // A faint baseline along the bottom edge, so the area fill and the line read as
    // sitting on a ground rather than floating. Dimmer than the line, so it never
    // competes with the data.
    ui.painter().hline(
        rect.left()..=rect.right(),
        rect.bottom(),
        egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.25)),
    );
    // Area fill under the line: a triangle fan from the bottom edge whose alpha
    // is a function of height — strongest just below the line, fading to nothing
    // at the bottom edge. A colour that is linear in y interpolates exactly across
    // each triangle, so the gradient is smooth. The same "lit from above" cue as
    // the panel ground and the top bevels.
    let fill = |y: f32| {
        let t = ((rect.max.y - y) / rect.height()).clamp(0.0, 1.0);
        ACCENT.gamma_multiply(0.22 * t)
    };
    let mut mesh = egui::epaint::Mesh::default();
    mesh.colored_vertex(egui::pos2(rect.left(), rect.bottom()), fill(rect.bottom()));
    mesh.colored_vertex(egui::pos2(rect.right(), rect.bottom()), fill(rect.bottom()));
    for (i, &p) in points.iter().enumerate() {
        let v = 2 + i as u32;
        mesh.colored_vertex(p, fill(p.y));
        if i + 1 < points.len() {
            mesh.add_triangle(0, v, v + 1);
        }
    }
    ui.painter().add(egui::Shape::mesh(mesh));
    // A faint dashed horizontal line at the recent average, so the current value and
    // the peak can be read against the stream's "normal" level (the peak marker shows
    // the high, the "now" dot the current, and this the level they drift around).
    // Dashed (not solid) and a muted green-grey (not the data's accent), so it reads
    // as a reference, not a fourth data series.
    let avg = samples.iter().sum::<f64>() / samples.len() as f64;
    let max = samples.iter().copied().fold(0.0, f64::max).max(60.0);
    let avg_y = rect.max.y - 2.0 - (avg / max) as f32 * (rect.height() - 6.0);
    let avg_color = egui::Color32::from_rgb(120, 150, 135);
    let (dash, gap) = (4.0, 3.0);
    let mut x = rect.left();
    while x < rect.right() {
        let x2 = (x + dash).min(rect.right());
        ui.painter()
            .line_segment([egui::pos2(x, avg_y), egui::pos2(x2, avg_y)], egui::Stroke::new(1.0, avg_color));
        x += dash + gap;
    }
    // The line itself, one segment at a time.
    for w in points.windows(2) {
        ui.painter().line_segment([w[0], w[1]], egui::Stroke::new(1.5, ACCENT));
    }
    // A subtle peak marker: a faint vertical line from the recent peak down to the
    // baseline, with a small dot at the peak, so a wobbly stream's headroom is
    // visible at a glance (how far the current bitrate is from the recent peak).
    // It is drawn before the "now" dot so the live edge stays on top, and is
    // skipped when the peak *is* the newest sample (the "now" dot already marks
    // that point, so a second mark there would only clutter the live edge).
    if let Some((peak_idx, _)) = samples
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
    {
        if peak_idx + 1 < samples.len() {
            let peak = points[peak_idx];
            ui.painter().vline(
                peak.x,
                peak.y..=rect.bottom(),
                egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.18)),
            );
            ui.painter().circle_filled(peak, 1.8, ACCENT.gamma_multiply(0.55));
        }
    }
    // A small dot on the newest sample, so "where the stream is right now" has a
    // mark of its own rather than only the line's right edge. A soft sonar pulse
    // around the dot (the same language as the status and viewers dots) makes the
    // live edge read as an active signal rather than a static mark.
    if let Some(last) = points.last() {
        ui.painter().circle_filled(*last, 2.5, ACCENT);
        sonar_pulse(ui.painter(), *last, 3.0, secs, 2.0, ACCENT);
    }
    // The tooltip carries the current value, so a hover reads the number without
    // leaving the chart. The recent peak is included too (the peak marker's value),
    // and the recent average (the dashed reference line's value), so a hover reads
    // where the stream is now, its recent high, and its normal level.
    let peak = samples.iter().copied().fold(0.0, f64::max);
    resp.on_hover_text(format!(
        "video bitrate, last two minutes — now {now:.0} kbit/s, peak {peak:.0} kbit/s, avg {avg:.0} kbit/s"
    ));
}

/// Middle-elide a monospace string so its rendered width fits `max_width`: the
/// first and last runs of characters are kept and the middle is replaced with an
/// ellipsis. Only the *display* is truncated — callers still copy the full string.
///
/// The tail is kept on purpose: a ticket's transport hints (relay, ephemeral ports)
/// change at the end, so a head-only truncation would look identical between refreshes
/// even when the ticket had actually changed. Measured with the same monospace face
/// the box renders with, so the estimate is exact; the constant advance of a monospace
/// font lets us compute the cut in one step rather than re-laying out per character.
fn fit_monospace(ui: &Ui, text: &str, max_width: f32) -> String {
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let full = ui
        .painter()
        .layout_no_wrap(text.to_string(), font_id, egui::Color32::PLACEHOLDER);
    if text.is_empty() || full.rect.width() <= max_width {
        return text.to_string();
    }
    let count = text.chars().count();
    let advance = full.rect.width() / count as f32;
    if advance <= 0.0 {
        return "…".to_string();
    }
    // Whole characters that fit, minus one slot for the ellipsis. Split the budget so
    // both ends stay visible; the head gets the leftover so the tail (the part that
    // actually changes) is never squeezed to nothing.
    let budget = ((max_width / advance) as usize).saturating_sub(1);
    if budget < 2 {
        return "…".to_string();
    }
    let tail = (budget / 2).max(1);
    let head = budget - tail;
    let head_s: String = text.chars().take(head).collect();
    let tail_s: String = text.chars().skip(count - tail).collect();
    format!("{head_s}…{tail_s}")
}

/// The two sheets of the copy glyph, anchored at the right edge of a copy box.
///
/// Returns `[back, front]`: two small overlapping rounded rects, the front offset
/// up-right of the back, so the pair reads as the universal "copy" mark without
/// relying on a font glyph the bundled faces may not carry.
fn copy_glyph_rects(rect: egui::Rect) -> [egui::Rect; 2] {
    let size = egui::vec2(8.0, 9.0);
    let cx = rect.right() - 16.0;
    let cy = rect.center().y;
    let offset = 3.0;
    let front = egui::Rect::from_center_size(egui::pos2(cx + offset, cy - offset), size);
    let back = egui::Rect::from_center_size(egui::pos2(cx - offset, cy + offset), size);
    [back, front]
}

/// The two strokes of the checkmark the copy glyph becomes the moment its text is
/// copied: a short down-stroke into a long up-stroke, centred on the glyph's
/// position (the same anchor as [`copy_glyph_rects`]), so the confirmation reads as
/// a "done" mark right at the point of action. Kept inside the box's right gutter,
/// so it never overflows the value line or the window.
fn checkmark_segments(rect: egui::Rect) -> [[egui::Pos2; 2]; 2] {
    let cx = rect.right() - 16.0;
    let cy = rect.center().y;
    let a = egui::pos2(cx - 4.0, cy);
    let b = egui::pos2(cx - 1.0, cy + 3.0);
    let c = egui::pos2(cx + 4.5, cy - 3.5);
    [
        [a, b],
        [b, c],
    ]
}

/// Format a number of seconds as `42s`, `7m13s` or `1h02m03s`.
fn format_elapsed(secs: u64) -> String {
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, s) => format!("{h}h{m:02}m{s:02}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The copy glyph's two sheets sit inside the box, are non-degenerate, and the
    /// front reads up-right of the back. Guards against a misplaced or absent glyph
    /// that a full-render shape count would not catch.
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

    /// `fit_monospace` leaves text that already fits untouched, and middle-elides a
    /// long value to a measured width that fits — keeping both the head and the tail
    /// so a ticket whose transport hints change at the end visibly changes between
    /// refreshes. The full string is still what gets copied; this checks the display.
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

    /// `sparkline_points` spans the rect edge to edge, puts the peak sample above
    /// the troughs, keeps every point inside the rect, and floors the scale at
    /// 60 kbit/s so a quiet stream does not hug the bottom edge.
    #[test]
    fn sparkline_points_span_the_rect_and_scale_to_the_peak() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 26.0));
        let samples = [60.0, 120.0, 60.0];
        let pts = sparkline_points(&samples, rect);
        assert_eq!(pts.len(), samples.len());
        assert_eq!(pts[0].x, rect.left(), "oldest sample at the left edge");
        assert_eq!(pts[2].x, rect.right(), "newest sample at the right edge");
        assert!(pts[1].y < pts[0].y, "the peak sample sits above the troughs");
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

    /// The sparkline's area fill is a vertical gradient (the bottom-edge vertices
    /// are fully transparent, the line vertices carry the accent's alpha, so the
    /// fill reads as lit from the line down to nothing), and the newest sample
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

        // The area fill: one mesh; its two bottom-edge vertices are transparent,
        // and the line vertices are more opaque the higher they sit — the gradient.
        let mesh = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Mesh(m) => Some(m.as_ref()),
                _ => None,
            })
            .expect("the sparkline paints its area mesh");
        let bottom = &mesh.vertices[0];
        assert_eq!(bottom.color.a(), 0, "the fill fades to nothing at the bottom edge");
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
        let dot = out
            .shapes
            .iter()
            .any(|s| matches!(&s.shape,
                egui::epaint::Shape::Circle(c)
                    if c.stroke.width == 0.0 && (2.0..3.0).contains(&c.radius) && c.fill.a() > 0
            ));
        assert!(dot, "the newest sample should carry a filled 'now' dot");
    }

    /// The "now" dot carries a soft sonar pulse (a stroked ring in the accent), so
    /// the live edge of the chart reads as an active signal rather than a static
    /// mark — the same language as the status and viewers dots. The ring is a
    /// stroked circle (width > 0), distinct from the filled dot (width == 0), and it
    /// expands and fades over the pulse.
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
                    egui::epaint::Shape::Circle(c) if c.stroke.width > 0.0 => {
                        Some(c.radius)
                    }
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
        assert!(
            ring(1.99).is_none(),
            "a nearly-faded ring paints nothing"
        );
    }

    /// `bitrate_trend` classifies the recent direction: a clear rise reads Rising, a
    /// clear drop reads Falling, a change within the noise band (and too few samples
    /// to judge) reads Flat, and a two-sample rise still compares against the first.
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

    /// `trend_arrow` paints a triangle mesh for a rising or falling trend (the up /
    /// down triangle) and a plain dash (a line segment, no mesh) for a flat trend, so
    /// the three states read as distinct marks.
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
        assert!(has_mesh(Trend::Rising), "a rising trend paints a triangle mesh");
        assert!(has_mesh(Trend::Falling), "a falling trend paints a triangle mesh");
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
            !out
                .shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::epaint::Shape::Mesh(_))),
            "a flat trend paints no triangle"
        );
    }

    /// The sparkline paints a dashed horizontal average reference line (several short
    /// segments at the same y in the muted green-grey), distinct from the data line
    /// (the accent) and the baseline / peak (dimmed accent), so the stream's normal
    /// level is visible against the current value and the peak.
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
            .filter(|s| matches!(&s.shape,
                egui::epaint::Shape::LineSegment { points, stroke }
                    if (points[0].y - points[1].y).abs() < 0.01
                        && stroke.color == avg_color
            ))
            .count();
        assert!(
            avg_segs >= 2,
            "the sparkline should paint a dashed average line (got {avg_segs} segments)"
        );
    }
}
