//! The Options panel: a Broadcasting section (WHIP flags and the current ticket)
//! and a Watching section
//! (default player, per-player paths, buffer, play address, verbosity).
//!
//! Opened from the menu bar. Edits the config live and writes it to disk on
//! Save (and on quit). The default player set here is auto-selected on the
//! Watch tab at open.

use std::time::Instant;

use eframe::egui::{self, Context, Ui};

use super::{
    config::{player_key, PLAYERS},
    pointer_over, pointer_pressed_over, primary_button_glow, App, ACCENT,
};

/// The neutral grey the Close button is painted in: the same halo/fill/stroke
/// language as the accent-green primary "Save", just desaturated, so the two read
/// as one set of matching actions with Save leading.
pub(crate) const CLOSE_GREY: egui::Color32 = egui::Color32::from_rgb(150, 155, 165);
/// The size of both action buttons in the panel's footer, so Save and Close are
/// the same box.
const BUTTON: egui::Vec2 = egui::vec2(90.0, 30.0);

/// The project's public repository, shown as a clickable link at the bottom of the
/// Settings panel.
pub(crate) const REPO_URL: &str = "https://github.com/RockasMockas/streampipe/releases";
/// The link's visible text: the URL without its scheme, so the footer reads as a
/// quiet signature line rather than a raw address.
pub(crate) const REPO_LABEL: &str = "github.com/RockasMockas/streampipe";

impl App {
    /// Show the Options window, closing it when its close button is pressed or
    /// the Escape key is hit.
    pub fn options_window(&mut self, ctx: &Context) {
        // A local `open` avoids borrowing a field of `self` while the content
        // closure also borrows `self` mutably.
        let mut open = true;
        // Span the full width of the viewport (minus a small margin) and pin it
        // there so it snaps open edge-to-edge on the first frame — no fade and no
        // deferred auto-sizing pass that would otherwise stall the width until the
        // mouse moves. Anchored top-left; still vertically resizable.
        let margin = 8.0;
        let width = (ctx.viewport_rect().width() - margin * 2.0).max(320.0);
        egui::Window::new("Settings")
            .anchor(egui::Align2::LEFT_TOP, [margin, margin])
            .default_width(width)
            .min_width(width)
            .max_width(width)
            .fade_in(false)
            .resizable([false, true])
            .open(&mut open)
            .show(ctx, |ui| {
                self.options_content(ui);
            });
        if !open {
            self.show_options = false;
        }
        // Escape closes the overlay too, mirroring the close button.
        if self.show_options && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_options = false;
        }
    }

    /// The two sections of the Options window.
    fn options_content(&mut self, ui: &mut Ui) {
        // The ticket is reset live (rebinding the shared endpoint), so grey it out
        // while either track is live — a running broadcast or watch would be torn
        // down by a rebind of the endpoint it is using.
        let active = super::state::snapshot(&self.state).any_active();
        egui::CollapsingHeader::new("Broadcasting")
            .default_open(true)
            .show(ui, |ui| {
                ui.label("Current Ticket:");
                let snap = super::state::snapshot(&self.state);
                let ticket = snap
                    .ticket
                    .clone()
                    .or_else(|| self.config.broadcast_ticket.clone())
                    .unwrap_or_else(|| "no ticket yet — press Reset Ticket".into());
                let requested = self.ticket_row(
                    ui,
                    "opt_ticket",
                    &ticket,
                    !active,
                    "Reset Ticket",
                    "Resetting…",
                );
                if requested {
                    self.reset_ticket(ui.ctx());
                }

                ui.add_space(4.0);
                // Verbosity picker: one line, three mutually exclusive levels
                // (1 = quiet, 2 = info, 3 = debug). Defaults to level 1. Applied to
                // the log panel on the next launch, since logging is installed once
                // at startup.
                ui.horizontal(|ui| {
                    ui.label("Log Verbosity");
                    for level in 1..=3u8 {
                        if ui
                            .radio(self.config.verbosity == level, format!("Level {level}"))
                            .clicked()
                        {
                            self.config.verbosity = level;
                            self.config.save();
                        }
                    }
                });
            });

        ui.add_space(4.0);

        egui::CollapsingHeader::new("Watching")
            .default_open(true)
            .show(ui, |ui| {
                ui.label("Default player");
                ui.horizontal(|ui| {
                    for player in PLAYERS {
                        if ui
                            .radio(self.config.default_player() == player, player_key(player))
                            .clicked()
                        {
                            self.config.default_player = player_key(player).to_string();
                            self.sync_watch_from_config();
                        }
                    }
                });

                ui.add_space(4.0);
                ui.label("Player paths (optional)");
                egui::Grid::new("opt_paths")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        for player in PLAYERS {
                            if player_key(player) == "none" {
                                continue;
                            }
                            ui.label(player_key(player));
                            let mut path = self
                                .config
                                .player_paths
                                .get(player)
                                .unwrap_or_default()
                                .to_string();
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut path)
                                    .hint_text("Default")
                                    .desired_width(220.0),
                            );
                            if resp.changed() {
                                self.config.player_paths.set(
                                    player,
                                    if path.trim().is_empty() {
                                        None
                                    } else {
                                        Some(path)
                                    },
                                );
                            }
                            ui.end_row();
                        }
                    });

                ui.add_space(4.0);
                egui::Grid::new("opt_watch")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Player RTC address");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.play_addr)
                                .desired_width(160.0),
                        );
                        ui.end_row();
                    });
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // The accent primary "Save" button, matching the hero buttons on the tabs:
            // an accent fill + stroke + a soft glow, so the one action that commits the
            // edits reads as the primary commitment.
            // The fixed `.fill()` suppresses egui's own hover feedback, so the hover is
            // read from the pointer and lifts both the glow and the fill.
            let save_rect = egui::Rect::from_min_size(ui.cursor().min, BUTTON);
            let hovered = pointer_over(ui, save_rect);
            let pressed = pointer_pressed_over(ui, save_rect);
            primary_button_glow(ui.painter(), save_rect, ACCENT, hovered, pressed);
            let save = egui::Button::new("Save")
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
            if ui.add_sized(BUTTON, save).clicked() {
                self.sync_watch_from_config();
                self.config.save();
                // Confirm the commit with the floating pill, bottom-centre, fading out
                // over the toast window (1.5 s). It is drawn last and on its own
                // foreground layer, so it floats above this window instead of nudging
                // its rows.
                self.toast = Some(("Saved ✓".to_string(), Instant::now()));
                ui.ctx().request_repaint();
            }
            // "Close" is the same box and the same painted language as "Save" — fill,
            // stroke, halo, hover lift and press-in — in a neutral grey, so the footer
            // reads as two matching actions with the green one leading. Grey is dimmer
            // than the accent at rest and its halo is a shade lower, so "Close" stays
            // visibly the secondary action even at the same size.
            let close_rect = egui::Rect::from_min_size(ui.cursor().min, BUTTON);
            let close_hovered = pointer_over(ui, close_rect);
            let close_pressed = pointer_pressed_over(ui, close_rect);
            primary_button_glow(
                ui.painter(),
                close_rect,
                CLOSE_GREY,
                close_hovered,
                close_pressed,
            );
            let close = egui::Button::new("Close")
                .fill(CLOSE_GREY.gamma_multiply(if close_pressed {
                    0.14
                } else if close_hovered {
                    0.20
                } else {
                    0.12
                }))
                .stroke(egui::Stroke::new(
                    1.0,
                    CLOSE_GREY.gamma_multiply(if close_pressed {
                        0.35
                    } else if close_hovered {
                        0.55
                    } else {
                        0.40
                    }),
                ));
            if ui.add_sized(BUTTON, close).clicked() {
                self.show_options = false;
            }
        });

        // ─── Signature ───────────────────────────────────────────────────────
        // A quiet block under the actions, set off by a hairline so it reads as
        // the panel's footer rather than as one more control. Both lines take the
        // theme's faint `weak` ink at the same 11.5pt size, so they sit as one
        // matched pair and stay cleanly legible without competing with the controls
        // above them.
        //
        // The second line is the repository, shown as its own text and clickable:
        // egui's `Hyperlink` paints in the theme's link blue (which sits next to
        // the app's own blue accent, so it belongs), underlines on hover, shows the
        // URL in a tooltip, and emits `OpenUrl` on click. That command is only
        // honoured when eframe's `links` feature is on — enabled in Cargo.toml,
        // since without it `open_url_in_browser` is an empty stub and the link
        // would report clicks but go nowhere.
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Streampipe Created By: RockasMockas")
                .weak()
                .size(11.5),
        );
        // Same 11.5pt as the line above it, so the two read as one block.
        ui.hyperlink_to(egui::RichText::new(REPO_LABEL).size(11.5), REPO_URL);
    }
}
