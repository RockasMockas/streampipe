//! The Contacts overlay: manage saved contacts (a name + a ticket) and load one into
//! the Watch tab's ticket box.
//!
//! A standalone panel, opened from the footer bar's "Contacts" button or the Watch tab's
//! "Select Contact". It is mutually exclusive with Settings — both are anchored over the
//! panels, so only one is ever on screen.
//!
//! Edits are staged in `App::contacts_dirty` and flushed to disk when the panel closes
//! (any of: footer "Close", title-bar "×", Escape, "Load") — never per keystroke, so
//! typing never touches the disk.

use std::time::Instant;

use eframe::egui::{self, Context, Ui};

use super::{
    config::Contact, options::CLOSE_GREY, pointer_over, pointer_pressed_over,
    primary_button_glow, App, ACCENT, ERROR, WATCH_ACCENT,
};

/// The footer pair's box — the same size and painted language as Settings' Save/Close.
const BUTTON: egui::Vec2 = egui::vec2(90.0, 30.0);
/// The per-row action widths, so the fields can split what is left exactly.
const LOAD_W: f32 = 74.0;
/// A small square "✕" that opens the delete-confirmation modal.
const REMOVE_W: f32 = 28.0;
/// The list stops growing here and scrolls, so the footer always stays on screen.
const LIST_MAX_H: f32 = 248.0;
/// The per-row card's height (avatar, fields and actions all sit inside it).
const ROW_H: f32 = 34.0;
/// The initials avatar's diameter.
const AVATAR_D: f32 = 24.0;

impl App {
    /// Show the Contacts window and take it down on "×" / Escape, persisting edits.
    pub fn contacts_window(&mut self, ctx: &Context) {
        // A local `open` avoids borrowing a field of `self` inside the content closure.
        let mut open = true;
        // Centred over the panels: a picker is a compact panel, not the full-width
        // Settings slab. The width tracks the viewport so it never overflows a small
        // window, and `min == max` pins it — an auto-sized window jitters as rows
        // come and go.
        let vp = ctx.viewport_rect();
        let width = (vp.width() * 0.62)
            .clamp(430.0, 620.0)
            .min((vp.width() - 16.0).max(320.0));
        egui::Window::new("Contacts")
            .anchor(egui::Align2::CENTER_TOP, [0.0, 44.0])
            .default_width(width)
            .min_width(width)
            .max_width(width)
            .fade_in(false)
            .resizable([false, true])
            .open(&mut open)
            .show(ctx, |ui| {
                self.contacts_content(ui);
            });

        // The delete-confirmation modal, drawn above the panel. It is rendered here (not
        // inside `contacts_content`) so it sits on top and can swallow clicks behind it.
        if self.show_contacts {
            self.remove_confirm_modal(ctx);
        }

        // "×", Escape and the footer "Close" all funnel through `close_contacts`, so the
        // staged edits are flushed whichever way the panel goes away. The modal consumes
        // Escape while it is up, so closing the dialog leaves the panel standing.
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_contacts();
        }
    }

    /// The delete-confirmation modal: a dim backdrop that swallows clicks, plus a
    /// centred dialog asking to remove the contact. Confirming deletes the row; the
    /// "✕", "Cancel", a click on the backdrop, or Escape all dismiss it.
    fn remove_confirm_modal(&mut self, ctx: &Context) {
        let Some(i) = self.confirm_remove_contact else {
            return;
        };

        let screen = ctx.viewport_rect();

        // Full-screen dim that reports a click anywhere outside the dialog (→ dismiss).
        let bg_clicked = egui::Area::new(egui::Id::new("remove_confirm_bg"))
            .order(egui::Order::Foreground)
            .interactable(true)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(120));
                resp
            })
            .inner
            .clicked();

        let name = self
            .config
            .contacts
            .get(i)
            .map(|c| c.name.trim().to_string())
            .unwrap_or_default();
        let subject = if name.is_empty() {
            "this contact".to_string()
        } else {
            name.clone()
        };

        let mut confirm = false;
        let mut dismiss = false;

        egui::Window::new("Remove contact?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.label(format!("Remove {subject}?"));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        dismiss = true;
                    }
                    if ui.button("Remove").clicked() {
                        confirm = true;
                    }
                });
            });

        // Escape dismisses only the modal (consumed, so the panel stays up).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            dismiss = true;
        }

        if confirm {
            self.config.contacts.remove(i);
            self.confirm_remove_contact = None;
            self.contacts_dirty = true;
            ctx.request_repaint();
        } else if dismiss || bg_clicked {
            self.confirm_remove_contact = None;
            ctx.request_repaint();
        }
    }

    /// Take the Contacts panel down, flushing any staged edit to disk.
    pub fn close_contacts(&mut self) {
        self.show_contacts = false;
        self.focus_contact_row = None;
        if self.contacts_dirty {
            self.config.save();
        }
        self.contacts_dirty = false;
    }

    /// Open the Contacts panel from the footer bar, closing Settings if it is up — the
    /// two overlays occupy the same space, so they are never both shown.
    pub fn toggle_contacts(&mut self) {
        self.show_contacts = !self.show_contacts;
        self.focus_contact_row = None;
        if self.show_contacts {
            self.show_options = false;
            self.contacts_dirty = false;
        }
    }

    /// The window's contents: an intro line, the scrollable list of contact cards, then
    /// the Save/Close footer.
    fn contacts_content(&mut self, ui: &mut Ui) {
        ui.label(
            egui::RichText::new("Pick a contact to load their ticket into the Watch tab.")
                .weak()
                .size(12.0),
        );
        ui.add_space(8.0);

        // Every mutation is collected as an index during layout and applied once the list
        // has been built — the `Vec` is never mutated while it is being iterated, and the
        // buttons keep their final (settled) rects between frames.
        let mut add = false;
        let mut load_idx: Option<usize> = None;
        let mut remove_idx: Option<usize> = None;
        let mut dirty = self.contacts_dirty;
        // Read Enter once, up front: a key consumed by a widget later in this frame must
        // not defeat Enter-to-load.
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));

        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(LIST_MAX_H)
            .show(ui, |ui| {
                if self.config.contacts.is_empty() {
                    // A calm, centred empty state with room to breathe.
                    ui.vertical_centered(|ui| {
                        ui.add_space(22.0);
                        ui.label(
                            egui::RichText::new(
                                "No contacts yet — add one to fill the Watch tab in one click.",
                            )
                            .weak()
                            .size(12.0),
                        );
                        ui.add_space(22.0);
                    });
                }

                for (i, contact) in self.config.contacts.iter_mut().enumerate() {
                    // The row whose ticket is sitting in the Watch box: tinted, with its
                    // action reading "In the box" — there is nothing left to load.
                    let active =
                        !contact.ticket.is_empty() && contact.ticket == self.watch_ticket;

                    ui.horizontal(|ui| {
                        // The card: a rounded surface spanning the row — brighter on hover,
                        // tinted in the watch accent when its ticket is loaded. Painted
                        // first (from the row's full width) so widgets sit on top of it.
                        let avail = ui.available_width();
                        let row_rect =
                            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(avail, ROW_H));
                        let hovered = pointer_over(ui, row_rect);
                        let fill = if active {
                            WATCH_ACCENT.gamma_multiply(0.12)
                        } else if hovered {
                            egui::Color32::from_white_alpha(14)
                        } else {
                            egui::Color32::from_white_alpha(5)
                        };
                        let stroke = if active {
                            egui::Stroke::new(1.0, WATCH_ACCENT.gamma_multiply(0.55))
                        } else {
                            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(16))
                        };
                        let corner = egui::CornerRadius::same(7);
                        ui.painter().rect_filled(row_rect, corner, fill);
                        ui.painter()
                            .rect_stroke(row_rect, corner, stroke, egui::StrokeKind::Inside);

                        // Inset the initials avatar from the card's left edge.
                        ui.add_space(8.0);
                        let (avatar_rect, _resp) =
                            ui.allocate_exact_size(egui::vec2(AVATAR_D, AVATAR_D), egui::Sense::hover());
                        let (initials, hue) = avatar_for(&contact.name);
                        let [ar, ag, ab, _aa] =
                            egui::epaint::Hsva::new(hue, 0.42, 0.5, 1.0).to_srgba_unmultiplied();
                        let av_bg = egui::Color32::from_rgba_unmultiplied(ar, ag, ab, 255);
                        ui.painter()
                            .circle_filled(avatar_rect.center(), AVATAR_D * 0.5 - 1.0, av_bg);
                        ui.painter().text(
                            avatar_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            &initials,
                            egui::FontId::proportional(11.0),
                            egui::Color32::from_rgb(245, 245, 250),
                        );
                        ui.add_space(4.0);

                        // Split the row explicitly: two fields, then the two actions. The
                        // widths come from `available_width()` so the row fills the panel at
                        // any window size (a `Grid` clamps each column to the previous
                        // frame's width).
                        let gap = ui.spacing().item_spacing.x;
                        // 8 (avatar inset) + avatar + 4 (avatar/name gap) + the four
                        // inter-widget gaps flanking the two action buttons.
                        let used = 8.0 + AVATAR_D + 4.0 + LOAD_W + REMOVE_W + gap * 4.0;
                        let fields = (avail - used).max(150.0);
                        let name_w = (fields * 0.4).clamp(70.0, 190.0);
                        let ticket_w = (fields - name_w).max(110.0);

                        let name = ui.add(
                            egui::TextEdit::singleline(&mut contact.name)
                                .hint_text("Name")
                                .desired_width(name_w),
                        );
                        // A stable id per row, so Enter-to-load can be read off the
                        // ticket field's focus.
                        let ticket_id = egui::Id::new(("contact_ticket", i));
                        let ticket = ui.add(
                            egui::TextEdit::singleline(&mut contact.ticket)
                                .id(ticket_id)
                                .hint_text("Paste their ticket here")
                                .desired_width(ticket_w)
                                .font(egui::TextStyle::Monospace),
                        );
                        // Enter in a ticket field loads that contact — the same gesture as
                        // the Watch tab's Enter-to-play, one step earlier in the flow.
                        if enter
                            && !active
                            && ui.ctx().memory(|m| m.has_focus(ticket_id))
                            && !contact.ticket.trim().is_empty()
                        {
                            load_idx = Some(i);
                        }
                        // Staged, not written: the flush happens when the panel closes.
                        if name.changed() || ticket.changed() {
                            dirty = true;
                        }
                        // "Add contact" arms this, so the caret lands in the new row's name
                        // field. Cleared straight away: a standing request would yank focus
                        // back every frame and defeat anything the user clicks next.
                        if self.focus_contact_row == Some(i) {
                            name.request_focus();
                            self.focus_contact_row = None;
                        }

                        // "Load" is the row's primary action (accent-filled); the loaded row
                        // reads "In the box" and leaves the interactive set.
                        let load = egui::Button::new(
                            egui::RichText::new(if active { "In the box" } else { "Load" })
                                .color(if active { WATCH_ACCENT } else { egui::Color32::WHITE }),
                        )
                        .min_size(egui::vec2(LOAD_W, 24.0))
                        .fill(if active {
                            WATCH_ACCENT.gamma_multiply(0.16)
                        } else {
                            ACCENT.gamma_multiply(0.24)
                        })
                        .stroke(egui::Stroke::new(
                            1.0,
                            if active {
                                WATCH_ACCENT.gamma_multiply(0.5)
                            } else {
                                ACCENT.gamma_multiply(0.7)
                            },
                        ));
                        if ui.add_enabled(!active, load).clicked() {
                            load_idx = Some(i);
                        }

                        // A small "✕" that opens the delete-confirmation modal, so a stray
                        // click can never delete a contact.
                        let remove = egui::Button::new(
                            egui::RichText::new("✕").color(ERROR.gamma_multiply(0.95)),
                        )
                        .min_size(egui::vec2(REMOVE_W, 24.0))
                        .fill(ERROR.gamma_multiply(0.12))
                        .stroke(egui::Stroke::new(1.0, ERROR.gamma_multiply(0.4)));
                        if ui.add(remove).clicked() {
                            remove_idx = Some(i);
                        }
                    });
                    ui.add_space(4.0);
                }
            });

        ui.add_space(2.0);
        let add_btn = egui::Button::new(egui::RichText::new("+  Add contact").color(ACCENT))
            .fill(ACCENT.gamma_multiply(0.14))
            .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.45)));
        if ui.add(add_btn).clicked() {
            add = true;
        }

        // Apply what the list collected.
        if add {
            self.config.contacts.push(Contact::default());
            self.focus_contact_row = Some(self.config.contacts.len() - 1);
            dirty = true;
        }
        // A "✕" click only *requests* removal — the row is deleted from the confirmation
        // modal (rendered by `contacts_window`), never straight from the list.
        if let Some(i) = remove_idx {
            self.confirm_remove_contact = Some(i);
        }
        if let Some(i) = load_idx {
            // Fill the Watch tab's ticket box and drop the panel, so the filled box and
            // its new name chip are revealed on the very next frame.
            self.watch_ticket = self.config.contacts[i].ticket.clone();
            self.show_contacts = false;
            self.focus_contact_row = None;
            if dirty {
                self.config.save();
            }
            self.contacts_dirty = false;
            ui.ctx().request_repaint();
            return;
        }
        self.contacts_dirty = dirty;

        // Footer: the same matched Save/Close pair as the Settings overlay, so the two
        // panels read as one set.
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
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
                self.config.save();
                self.contacts_dirty = false;
                self.toast = Some(("Saved ✓".to_string(), Instant::now()));
                ui.ctx().request_repaint();
            }

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
                } else if hovered {
                    0.20
                } else {
                    0.12
                }))
                .stroke(egui::Stroke::new(
                    1.0,
                    CLOSE_GREY.gamma_multiply(if close_pressed {
                        0.35
                    } else if hovered {
                        0.55
                    } else {
                        0.40
                    }),
                ));
            if ui.add_sized(BUTTON, close).clicked() {
                self.close_contacts();
                ui.ctx().request_repaint();
            }
        });
    }
}

/// The initials (up to two letters) shown on a contact's avatar, plus a deterministic
/// hue in `[0, 1)` derived from the name so each contact reads as a distinct face.
fn avatar_for(name: &str) -> (String, f32) {
    let trimmed = name.trim();
    let initials = if trimmed.is_empty() {
        "?".to_string()
    } else {
        let mut words = trimmed.split_whitespace();
        let first = words
            .next()
            .and_then(|w| w.chars().next())
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default();
        let second = words
            .next()
            .and_then(|w| w.chars().next())
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default();
        format!("{first}{second}")
    };
    // A cheap deterministic hash → hue, so the same name always gets the same colour.
    let mut h: u32 = 0;
    for c in trimmed.chars() {
        h = h.wrapping_mul(31).wrapping_add(c as u32);
    }
    (initials, (h % 360) as f32 / 360.0)
}
