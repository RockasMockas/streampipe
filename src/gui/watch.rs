//! The Watch Stream tab: paste a ticket, pick a player, play.

use std::{path::PathBuf, str::FromStr};

use crate::EndpointTicket;
use eframe::egui::{self, Context, Ui};

use super::{
    config::{player_key, PLAYERS},
    contact_chip, error_callout, pointer_over, pointer_pressed_over, primary_button_glow,
    soft_glow, spinner,
    state::{self, Mode, WatchParams},
    truncate, App, ACCENT, ERROR, WATCH_ACCENT,
};

/// Parse a buffer field (milliseconds) into an option; blank or invalid is `None`
/// (lowest latency).
pub fn parse_buffer_ms(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

/// The status label shown under the buttons while a watch session is up.
pub fn watch_action_label(mode: Mode, receiving: bool) -> &'static str {
    match mode {
        Mode::Starting => "connecting…",
        _ if receiving => "Playing stream in player window",
        _ => "Waiting for streamer to begin broadcasting...",
    }
}

/// Whether the "Watch Stream" button is actionable: idle, with a non-blank ticket.
pub fn watch_can_start(busy: bool, ticket: &str) -> bool {
    !busy && !ticket.trim().is_empty()
}

/// Whether Enter should start a watch session: the ticket box must own keyboard focus,
/// Enter must be pressed, and the button's idle/non-blank gate must hold. Extracted so
/// the keyboard gate is testable without focus plumbing.
pub fn watch_enter_starts(focused: bool, enter: bool, busy: bool, ticket: &str) -> bool {
    focused && enter && watch_can_start(busy, ticket)
}

impl App {
    /// Render the Watch Stream tab.
    pub fn watch_tab(&mut self, ui: &mut Ui) {
        let snap = state::snapshot(&self.state);
        let busy = snap.watch.active();
        let watching = snap.watch == Mode::Watching;
        let starting = snap.watch == Mode::Starting;

        // The chosen contact's name, *derived* by matching the ticket in the box. Not
        // stored: editing the ticket makes the lookup miss, so the chip can never claim a
        // contact whose ticket is no longer there. Cloned to an owned `Option<String>` so
        // the row closure borrows nothing of `self`.
        let active_name: Option<String> = self
            .config
            .contacts
            .iter()
            .find(|c| !c.ticket.is_empty() && c.ticket == self.watch_ticket)
            .map(|c| truncate(&c.name, 28))
            .filter(|n| !n.trim().is_empty());

        // Heading row: the "Stream ticket" label (enlarged to the tab's "big" scale so
        // its line height matches the button beside it) on the left, with the contact
        // chip and the "Select Contact" button flush right — the entry point to the
        // picker now sits right where you read the heading, not under the box.
        let head = ui.style().text_styles[&egui::TextStyle::Body].size * 1.3;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (bar, _) = ui.allocate_exact_size(egui::vec2(3.0, 16.0), egui::Sense::hover());
            ui.painter().rect_filled(
                bar.translate(egui::vec2(0.0, 1.0)),
                egui::CornerRadius::same(1),
                WATCH_ACCENT,
            );
            ui.label(egui::RichText::new("Stream ticket").strong().size(head));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // "Select Contact" opens the Contacts overlay; 22 tall, flush right and
                // vertically centred within the heading row.
                let row_h = ui.available_height().max(22.0);
                let rect = egui::Rect::from_min_size(
                    egui::pos2(
                        ui.cursor().max.x - 140.0,
                        ui.cursor().min.y + (row_h - 22.0) * 0.5,
                    ),
                    egui::vec2(140.0, 22.0),
                );
                let hovered = pointer_over(ui, rect);
                let pressed = pointer_pressed_over(ui, rect);
                primary_button_glow(ui.painter(), rect, WATCH_ACCENT, hovered, pressed);
                let button = egui::Button::new("Select Contact")
                    .fill(WATCH_ACCENT.gamma_multiply(if pressed {
                        0.20
                    } else if hovered {
                        0.26
                    } else {
                        0.18
                    }))
                    .stroke(egui::Stroke::new(
                        1.0,
                        WATCH_ACCENT.gamma_multiply(if pressed {
                            0.50
                        } else if hovered {
                            0.75
                        } else {
                            0.55
                        }),
                    ));
                if ui.add_sized([140.0, 22.0], button).clicked() {
                    self.show_contacts = true;
                    // A click schedules its own repaint; this lands the overlay on the very
                    // next frame even when the window has fallen asleep.
                    ui.ctx().request_repaint();
                }
                // The chosen contact's name rides just left of the button, well clear of
                // both the heading and the ticket box below it.
                if let Some(name) = &active_name {
                    contact_chip(ui, name, WATCH_ACCENT);
                }
            });
        });

        // Read input BEFORE rendering the ticket box, so a key consumed by the widget
        // later in this frame cannot defeat Enter-to-start.
        let ticket_id = egui::Id::new("watch_ticket_input");
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let window_focused = ui.input(|i| i.focused);
        // Whether the ticket box owns keyboard focus (read from the previous frame), so
        // the frame's stroke and a soft glow can mark it as the active input. A
        // disabled (busy) box can never be focused, so the ring recedes on its own.
        let focused = ui.ctx().memory(|m| m.has_focus(ticket_id));

        // A framed surface via the TextEdit's OWN frame (not an outer group, which
        // would double the border and eat width). `return_key(None)` suppresses the
        // stray newline Enter would otherwise insert into a multiline box.
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
        // A soft accent glow around the focused box (the same halo language as the copy
        // box's hover), so the active input reads as lit by the accent.
        if focused {
            soft_glow(
                ui.painter(),
                resp.rect,
                ui.style().visuals.widgets.inactive.corner_radius,
                ACCENT,
            );
        }

        // Autofocus the ticket only when it already holds a prefilled ticket, on a
        // window that has keyboard focus, and once per switch. Autofocusing an empty box
        // would only blink a caret that can never submit (Enter on a blank ticket is a
        // no-op), and a focused multiline TextEdit schedules ~2Hz caret-blink repaints
        // that would keep an idle Watch tab from ever falling asleep. A prefilled ticket
        // is exactly the "press Enter to replay" case worth grabbing.
        // (`RawInput::default().focused` is `true`, so a bare focus flag is not a
        // sufficient guard against the repaint-sleep regression.)
        if self.focus_primary && window_focused {
            if !busy && !self.watch_ticket.trim().is_empty() {
                resp.request_focus();
            }
            self.focus_primary = false;
        }

        // Enter-to-start, gated on the box owning focus and the button's own
        // idle/non-blank conditions.
        let focused = ui.ctx().memory(|m| m.has_focus(resp.id));
        if watch_enter_starts(focused, enter, busy, &self.watch_ticket) {
            self.start_watch(ui.ctx());
        }

        // The ticket box remembers the last-used ticket (seeded from
        // `config.last_ticket` at startup, updated on each watch), so no separate
        // history picker is needed.

        ui.add_space(8.0);
        // Match the Broadcast tab's "Advanced settings" sizing: larger (1.3×)
        // proportional font and matching input-box height, so the controls read as one
        // consistent column across tabs.
        let big = ui.style().text_styles[&egui::TextStyle::Body].size * 1.3;
        let prop = egui::FontId::proportional(big);
        let box_h = big * 1.3;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Player").size(big));
            // Deliberately NOT gated on `!busy`: the viewer picks a player mid-session
            // and presses `Restart` to apply it, so retuning playback is one click
            // instead of stop-edit-restart. A stable id keeps the focus ring working.
            egui::ComboBox::from_id_salt("watch_player")
                .selected_text(egui::RichText::new(player_key(self.watch_player)).size(big))
                .show_ui(ui, |ui| {
                    for player in PLAYERS {
                        ui.selectable_value(&mut self.watch_player, player, player_key(player));
                    }
                });
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Buffer (ms)").size(big));
            // A stable id, so the buffer field's focus can be read (for the focus glow).
            let buffer_id = egui::Id::new("watch_buffer_input");
            // Also live while a session is up: edit the buffer, press `Restart`, and the
            // new session launches with it.
            let buffer_resp = ui.add_sized(
                [90.0, box_h],
                egui::TextEdit::singleline(&mut self.watch_buffer)
                    .id(buffer_id)
                    .hint_text(egui::RichText::new("auto").size(big))
                    .font(prop.clone())
                    .vertical_align(egui::Align::Center),
            );
            // A soft accent glow around the focused buffer field (same halo language as
            // the ticket box's focus ring).
            if ui.ctx().memory(|m| m.has_focus(buffer_id)) {
                soft_glow(
                    ui.painter(),
                    buffer_resp.rect,
                    ui.style().visuals.widgets.inactive.corner_radius,
                    ACCENT,
                );
            }
        });

        // The initial player volume, on its own full-width row under the Player/Buffer
        // line: the label leads on the left and carries the number, the slider track
        // fills the rest of the panel so the drag target is as wide as the tab. The
        // label is formatted from the same `watch_volume` the slider writes, so the two
        // can never disagree.
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // Format first: the label must not borrow what the slider takes mutably.
            let label = format!("Player Initial Volume: {}%", self.watch_volume);
            ui.label(egui::RichText::new(label).size(big));
            // `slider_width` is the track width, and a child `Ui`'s spacing is scoped to
            // that child, so widening it here stretches this slider (and only this one)
            // out to the panel edge. The 24 points left over are the widget's own
            // margins, so the track ends flush with the content area.
            let track = (ui.available_width() - 24.0).max(120.0);
            ui.spacing_mut().slider_width = track;
            // `show_value(false)` drops the slider's own readout: the label already
            // shows the same number.
            let slider = egui::Slider::new(&mut self.watch_volume, 0..=100)
                .step_by(1.0)
                .show_value(false);
            let resp = ui.add_enabled_ui(!busy, |ui| ui.add(slider));
            let slider = resp.inner.on_hover_text(
                "where the player window starts: mpv gets --volume=N, VLC --gain=N/100 \
                 (VLC 3 has no --volume), ffplay -volume N-20: its scale is louder per \
                 point than the other two, so it starts 20 points lower and all three \
                 sound about the same for one setting.",
            );
            // Persist as soon as the gesture lands: `drag_stopped` is the release at the
            // end of a drag, `changed && !dragged` is a click-jump on the track or an
            // arrow key. So the app reopens at the last volume the user set, not merely
            // the last one they pressed Watch with. Guarded to a real change: dragging
            // back to the same number writes nothing, and the guard keeps the disk out of
            // the per-frame path.
            if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                let volume = self.watch_volume.min(100);
                if self.config.volume != volume {
                    self.config.volume = volume;
                    self.config.save();
                }
            }
        });

        if let Some(path) = self.config.player_paths.get(self.watch_player) {
            ui.weak(format!("player path: {path}"));
        }

        ui.add_space(12.0);
        if watching || starting {
            // Two half-width actions side by side — `Restart` (left) and `■  Stop`
            // (right) — with the status text on its own row underneath.
            //
            // `half` is computed BEFORE the child `Ui`: the panel's content width is the
            // window width minus its 2×16 side margins, and `available_width()` here is
            // exactly that. Each button is `add_sized` to `half` and the row's
            // `item_spacing.x` is 12, so `2 * half + 12` fills the panel exactly.
            // (`add_space` is *in addition to* `item_spacing`, so it must not be used
            // between the two buttons, and the spacing is mutated on the child, which
            // scopes it to this row.)
            let half = ((ui.available_width() - 12.0) / 2.0).max(1.0);
            let resp = ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                // `Restart` brings a new session up with the player/buffer currently
                // shown, tearing the previous one down first (`start_watch` hands the old
                // handle to `spawn_watch`, which aborts and drains it).
                let restart_rect =
                    egui::Rect::from_min_size(ui.cursor().min, egui::vec2(half, 36.0));
                let hovered = pointer_over(ui, restart_rect);
                let pressed = pointer_pressed_over(ui, restart_rect);
                primary_button_glow(ui.painter(), restart_rect, ACCENT, hovered, pressed);
                let restart = ui.add_sized(
                    [half, 36.0],
                    egui::Button::new("Restart")
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
                        )),
                );

                // A subtle red halo behind the Stop button, matching the Broadcast tab, so
                // the one control that stops a watch reads as a "stop" action. The halo
                // also contracts on press. The halo rect is re-read from the cursor, which
                // has advanced past the Restart button and its spacing.
                let stop_rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(half, 36.0));
                let hovered = pointer_over(ui, stop_rect);
                let pressed = pointer_pressed_over(ui, stop_rect);
                primary_button_glow(ui.painter(), stop_rect, ERROR, hovered, pressed);
                let stop = ui.add_sized(
                    [half, 36.0],
                    egui::Button::new("■  Stop")
                        .fill(ERROR.gamma_multiply(if pressed {
                            0.20
                        } else if hovered {
                            0.26
                        } else {
                            0.18
                        }))
                        .stroke(egui::Stroke::new(
                            1.0,
                            ERROR.gamma_multiply(if pressed {
                                0.50
                            } else if hovered {
                                0.75
                            } else {
                                0.55
                            }),
                        )),
                );
                (restart, stop)
            });
            if resp.inner.0.clicked() {
                self.start_watch(ui.ctx());
            }
            if resp.inner.1.clicked() {
                state::stop(&self.state, ui.ctx(), state::Track::Watch);
            }

            ui.add_space(6.0);
            // The status text on its own row under the buttons, with the connecting
            // spinner paired to it (dropped once playing).
            ui.horizontal(|ui| {
                if starting {
                    spinner(ui, self.pulse.elapsed().as_secs_f32(), WATCH_ACCENT);
                }
                ui.label(watch_action_label(snap.watch, snap.watch_receiving));
            });

            // Tell the viewer an edit is pending, so they know one click (`Restart`)
            // applies it. Compares the *parsed* buffer, so `" 90 "`, `"90"` and
            // unparseable input each collapse to one value and cannot flicker the line.
            if busy
                && snap.watch_applied
                    != Some((self.watch_player, parse_buffer_ms(&self.watch_buffer)))
            {
                ui.add_space(4.0);
                ui.weak("Press 'Restart' button to apply changes.");
            }
        } else {
            // Full-width primary action, matching the Broadcast tab's Start button.
            let can_start = watch_can_start(busy, &self.watch_ticket);
            let resp = ui
                .add_enabled_ui(can_start, |ui| {
                    // A soft accent halo behind the hero action, lit by the live green.
                    // Only when the button is live: a disabled button should not glow. The
                    // fixed `.fill()` suppresses egui's own hover feedback, so the hover
                    // is read from the pointer and lifts both the halo and the fill.
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
        // The slider's range is 0..=100; the `min` only guards a value restored from a
        // hand-edited config.
        let volume = self.watch_volume.min(100);

        // Persist the last-used buffer, volume and ticket. The default player is owned by
        // Options (it is the value auto-selected on the Watch tab at open), so a
        // quick-switch here only affects this session and must not overwrite it.
        self.config.buffer_ms = buffer_ms;
        self.config.volume = volume;
        self.config.note_ticket(&ticket_text);
        self.config.save();

        // A `Restart` while a session is live hands the previous handle to
        // `spawn_watch`, which aborts and drains it (bounded) before bringing the new
        // session up — so at most one watch session is ever live. Three sequential lock
        // acquisitions here (take, spawn's own, store); no `.await` between them.
        let previous = self.state.lock().unwrap().watch_session.take();
        let handle = state::spawn_watch(
            self.state.clone(),
            self.handle.clone(),
            ctx.clone(),
            previous,
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

    /// Keep the Watch tab's live fields in sync with the config after Options edits.
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
