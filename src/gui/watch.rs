//! The Watch Stream tab: paste a ticket, pick a player, play.

use std::{path::PathBuf, str::FromStr};

use streampipe::EndpointTicket;
use eframe::egui::{self, Context, Ui};

use super::{
    config::{player_key, PLAYERS},
    state::{self, Mode, WatchParams},
    App, ACCENT, ERROR, WATCH_ACCENT, error_callout, pointer_over, pointer_pressed_over,
    primary_button_glow,
    section_tick, soft_glow, spinner,
};

/// Parse a buffer field (milliseconds) into an option; blank or invalid is
/// `None` (lowest latency).
pub fn parse_buffer_ms(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

/// The status label shown beside Stop while a watch session is up.
///
/// `Starting` reads "connecting…" so the tab never claims a player window exists
/// before the endpoint is online and the play address has resolved.
fn watch_action_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Starting => "connecting…",
        _ => "playing in a player window",
    }
}

/// Whether the "Watch Stream" button is actionable: idle, with a non-blank ticket.
fn watch_can_start(busy: bool, ticket: &str) -> bool {
    !busy && !ticket.trim().is_empty()
}

/// Whether Enter should start a watch session: the ticket box must own keyboard
/// focus, Enter must be pressed, and the same idle-with-non-blank-ticket gate the
/// button uses must hold. Extracted so the keyboard gate is testable without any
/// focus plumbing.
fn watch_enter_starts(focused: bool, enter: bool, busy: bool, ticket: &str) -> bool {
    focused && enter && watch_can_start(busy, ticket)
}

impl App {
    /// Render the Watch Stream tab.
    pub fn watch_tab(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        let busy = snap.watch.active();
        let watching = snap.watch == Mode::Watching;
        let starting = snap.watch == Mode::Starting;

        section_tick(ui, "Stream ticket", WATCH_ACCENT);

        // Read input BEFORE rendering the ticket box, so a key consumed by the
        // widget later in this frame cannot defeat Enter-to-start.
        let ticket_id = egui::Id::new("watch_ticket_input");
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let window_focused = ui.input(|i| i.focused);
        // Whether the ticket box owns keyboard focus (read from the previous
        // frame), so the frame's stroke and a soft glow can mark it as the active
        // input — a focus ring the box otherwise lacks (only a blinking caret would
        // show it). A disabled (busy) box can never be focused, so the ring also
        // recedes on its own while a session is up.
        let focused = ui.ctx().memory(|m| m.has_focus(ticket_id));

        // A framed surface via the TextEdit's OWN frame (not an outer group, which
        // would double the border and eat width). `return_key(None)` suppresses the
        // stray newline Enter would otherwise insert into a multiline box. The
        // stroke brightens while focused, so the active input reads as lit.
        let textedit = egui::TextEdit::multiline(&mut self.watch_ticket)
            .id(ticket_id)
            .hint_text("Paste stream ticket here to start watching broadcasted stream...")
            .return_key(None::<egui::KeyboardShortcut>)
            .frame(
                egui::Frame::group(ui.style())
                    .fill(egui::Color32::from_rgba_unmultiplied(80, 200, 120, 14))
                    .stroke(egui::Stroke::new(
                        1.0,
                        ACCENT.gamma_multiply(if focused { 0.75 } else { 0.4 }),
                    ))
                    .inner_margin(egui::Margin::symmetric(6, 4)),
            );
        let resp = ui
            .add_enabled_ui(!busy, |ui| {
                ui.add_sized([ui.available_width(), 54.0], textedit)
            })
            .inner;
        // A soft accent glow around the focused box (the same halo language as the
        // copy box's hover), so the active input reads as lit by the accent.
        if focused {
            soft_glow(
                ui.painter(),
                resp.rect,
                ui.style().visuals.widgets.inactive.corner_radius,
                ACCENT,
            );
        }

        // Autofocus the ticket only when it already holds a prefilled ticket, on a
        // window that has keyboard focus, and once per switch. Autofocusing an empty
        // box would only blink a caret that can never submit (Enter on a blank ticket
        // is a no-op), and a focused multiline TextEdit schedules ~2Hz caret-blink
        // repaints that would keep an idle Watch tab from ever falling asleep. A
        // prefilled ticket is exactly the "press Enter to replay" case worth grabbing.
        // (`RawInput::default().focused` is `true`, so a bare focus flag is not a
        // sufficient guard against the repaint-sleep regression.)
        if self.focus_primary && window_focused {
            if !busy && !self.watch_ticket.trim().is_empty() {
                resp.request_focus();
            }
            self.focus_primary = false;
        }

        // Enter-to-start, gated on the box actually owning focus and the button's
        // own idle/non-blank conditions. Focus is read off the same `resp`.
        let focused = ui.ctx().memory(|m| m.has_focus(resp.id));
        if watch_enter_starts(focused, enter, busy, &self.watch_ticket) {
            self.start_watch(ui.ctx());
        }

        // The ticket box itself remembers the last-used ticket (seeded from
        // `config.last_ticket` at startup and updated on each watch), so no
        // separate history picker is needed.

        // A friendly "where do I get a ticket?" hint below the box while it is
        // empty, so a first-time viewer knows what to do. The box's own hint text
        // only says "paste a ticket here", not where one comes from; this fills
        // that gap. Weak and dim so it reads as a gentle nudge, not a competing
        // instruction. It recedes on its own once a ticket is pasted (or a
        // session is up, which always has a non-blank ticket).
        if self.watch_ticket.trim().is_empty() {
            ui.add_space(4.0);
            ui.weak("No ticket yet? Start a broadcast, or ask a friend to share theirs.");
        }

        ui.add_space(8.0);
        // Match the Broadcast tab's "Advanced settings" sizing: a larger (1.3×)
        // proportional font and matching input-box height, so the controls read as
        // one consistent column across tabs.
        let big = ui.style().text_styles[&egui::TextStyle::Body].size * 1.3;
        let prop = egui::FontId::proportional(big);
        let box_h = big * 1.3;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Player").size(big));
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("watch_player")
                    .selected_text(egui::RichText::new(player_key(self.watch_player)).size(big))
                    .show_ui(ui, |ui| {
                        for player in PLAYERS {
                            ui.selectable_value(&mut self.watch_player, player, player_key(player));
                        }
                    });
            });
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Buffer (ms)").size(big));
            // A stable id, so the buffer field's focus can be read (for the focus
            // glow below) the same way the ticket box's is.
            let buffer_id = egui::Id::new("watch_buffer_input");
            let buffer_resp = ui.add_enabled_ui(!busy, |ui| {
                ui.add_sized(
                    [90.0, box_h],
                    egui::TextEdit::singleline(&mut self.watch_buffer)
                        .id(buffer_id)
                        .hint_text(egui::RichText::new("auto").size(big))
                        .font(prop.clone())
                        .vertical_align(egui::Align::Center),
                )
            });
            // A soft accent glow around the focused buffer field (the same halo
            // language as the ticket box's focus ring), so the active input reads as
            // lit by the accent. A disabled (busy) field can never be focused, so the
            // glow recedes on its own while a session is up.
            if ui.ctx().memory(|m| m.has_focus(buffer_id)) {
                soft_glow(
                    ui.painter(),
                    buffer_resp.inner.rect,
                    ui.style().visuals.widgets.inactive.corner_radius,
                    ACCENT,
                );
            }
        });

        // The initial player volume, on its own full-width row under the
        // Player/Buffer line: the label leads on the left and carries the number,
        // the slider track fills the rest of the panel so the drag target is as
        // wide as the tab. The label is formatted from the same `watch_volume` the
        // slider writes, so the two can never disagree.
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // Format first: the label must not borrow what the slider takes mutably.
            let label = format!("Player Initial Volume: {}%", self.watch_volume);
            ui.label(egui::RichText::new(label).size(big));
            // `slider_width` is the track width, and a child `Ui`'s spacing is scoped
            // to that child, so widening it here stretches this slider (and only this
            // one) out to the panel edge. The 24 points left over are the widget's
            // own margins, so the track ends flush with the content area.
            let track = (ui.available_width() - 24.0).max(120.0);
            ui.spacing_mut().slider_width = track;
            // `show_value(false)` drops the slider's own readout: the leading label
            // already shows the same number, so the row reads as label, then bar.
            let slider = egui::Slider::new(&mut self.watch_volume, 0..=100)
                .step_by(1.0)
                .show_value(false);
            let resp = ui.add_enabled_ui(!busy, |ui| ui.add(slider));
            resp.inner.on_hover_text(
                "where the player window starts: mpv gets --volume=N, ffplay -volume N, \
                 VLC --gain=N/100 (VLC 3 has no --volume). 100 is full volume.",
            );
        });

        if let Some(path) = self.config.player_paths.get(self.watch_player) {
            ui.weak(format!("player path: {path}"));
        }

        ui.add_space(12.0);
        if watching || starting {
            ui.horizontal(|ui| {
                // A subtle red halo behind the Stop button, matching the Broadcast
                // tab, so the one control that stops a watch reads as a "stop" action.
                // The halo also contracts on press, so the stop reads as pressed in.
                let stop_rect =
                    egui::Rect::from_min_size(ui.cursor().min, egui::vec2(140.0, 30.0));
                let hovered = pointer_over(ui, stop_rect);
                let pressed = pointer_pressed_over(ui, stop_rect);
                primary_button_glow(ui.painter(), stop_rect, ERROR, hovered, pressed);
                if ui
                    .add(egui::Button::new("■  Stop").min_size(egui::vec2(140.0, 30.0)))
                    .clicked()
                {
                    state::stop(&self.state, ui.ctx(), state::Track::Watch);
                }
                // While connecting, a small rotating arc beside the hint gives the
                // wait visible progress; once playing it is dropped.
                if starting {
                    spinner(ui, self.pulse.elapsed().as_secs_f32(), WATCH_ACCENT);
                }
                ui.label(watch_action_label(snap.watch));
            });
        } else {
            // Full-width primary action, matching the Broadcast tab's Start button.
            let can_start = watch_can_start(busy, &self.watch_ticket);
            let resp = ui
                .add_enabled_ui(can_start, |ui| {
                    // A soft accent halo behind the hero action, lit by the live green.
                    // Only when the button is live: a disabled button should not glow.
                    // The fixed `.fill()` suppresses egui's own hover feedback, so the
                    // hover is read from the pointer and lifts both the halo and the fill.
                    let start_rect = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), 36.0),
                    );
                    let hovered = can_start && pointer_over(ui, start_rect);
                    let pressed = can_start && pointer_pressed_over(ui, start_rect);
                    if can_start {
                        primary_button_glow(ui.painter(), start_rect, ACCENT, hovered, pressed);
                    }
                    let button = egui::Button::new("▶  Watch Stream")
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
                    ui.add_sized([ui.available_width(), 36.0], button)
                })
                .inner;
            if resp.clicked() {
                self.start_watch(ui.ctx());
            }
        }

        if let Some(err) = &snap.watch_error {
            ui.add_space(6.0);
            error_callout(ui, err);
        }
    }

    /// Validate the ticket and start a watch session.
    fn start_watch(&mut self, ctx: &Context) {
        let ticket_text = self.watch_ticket.trim().to_string();
        let ticket = match EndpointTicket::from_str(&ticket_text) {
            Ok(ticket) => ticket,
            Err(e) => {
                // Inline error, no spawn.
                self.state.lock().unwrap().watch_error = Some(format!("invalid ticket: {e}"));
                ctx.request_repaint();
                return;
            }
        };
        let addr = ticket.endpoint_addr().clone();
        let player_path = self
            .config
            .player_paths
            .get(self.watch_player)
            .map(PathBuf::from);
        let play_addr = self.config.play_addr.clone();
        let buffer_ms = parse_buffer_ms(&self.watch_buffer);
        // The slider's range is 0..=100, so no clamping is needed here; the
        // `min` only guards a value restored from a hand-edited config.
        let volume = self.watch_volume.min(100);

        // Persist the last-used buffer, volume and ticket. The default player is
        // owned by Options (it is the value auto-selected on the Watch tab at
        // open), so a quick-switch here only affects this session and must not
        // overwrite it.
        self.config.buffer_ms = buffer_ms;
        self.config.volume = volume;
        self.config.note_ticket(&ticket_text);
        self.config.save();

        let handle = state::spawn_watch(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            WatchParams {
                addr,
                player: self.watch_player,
                player_path,
                play_addr,
                buffer_ms,
                volume,
            },
        );
        self.state.lock().unwrap().watch_session = Some(handle);
    }

    /// Keep the Watch tab's live fields in sync with the config after Options
    /// edits them.
    pub fn sync_watch_from_config(&mut self) {
        self.watch_player = self.config.default_player();
        self.watch_buffer = self
            .config
            .buffer_ms
            .map(|ms| ms.to_string())
            .unwrap_or_default();
        self.watch_volume = self.config.volume.min(100);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_action_label_distinguishes_connecting_from_playing() {
        assert_eq!(watch_action_label(Mode::Starting), "connecting…");
        assert_eq!(
            watch_action_label(Mode::Watching),
            "playing in a player window"
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
        // Enter starts only when the box owns focus, Enter is pressed, and the
        // button's own idle/non-blank gate holds.
        assert!(watch_enter_starts(true, true, false, "abc"));
        // Not focused → never starts (Enter elsewhere must not fire a watch).
        assert!(!watch_enter_starts(false, true, false, "abc"));
        // No Enter → nothing to trigger.
        assert!(!watch_enter_starts(true, false, false, "abc"));
        // Blank ticket → mirrors the disabled button.
        assert!(!watch_enter_starts(true, true, false, "  "));
        assert!(!watch_enter_starts(true, true, false, ""));
        // Busy (incl. Starting) → mirrors the disabled button.
        assert!(!watch_enter_starts(true, true, true, "abc"));
    }
}
