//! The Contacts overlay: manage saved contacts (a name + a ticket) and load one into
//! the Watch tab's ticket box.
//!
//! A standalone panel, opened from the footer bar's "Contacts" button or the Watch tab's
//! "Select Contact". It is mutually exclusive with Settings — both are anchored over the
//! panels, so only one is ever on screen.
//!
//! Rows are read-only by default (avatar, name, truncated ticket, "Load", "Edit").
//! "Edit" — and "+  Add contact" — put exactly one row at a time into an animated edit
//! state: editable fields, a green "Save" and a red "Delete". One shared tween
//! (`App::contact_edit_anim`, stepped in `draw()`) fades the opened row in and the
//! previous row out; widgets are allocated by progress, never painted at zero alpha.
//!
//! Edits are staged in `App::contacts_dirty` and flushed to disk when the panel closes
//! (any of: footer "Close", title-bar "×", Escape, "Load") — never per keystroke, so
//! typing never touches the disk. Row "Save" persists immediately with a "Saved ✓" toast.

use std::time::Instant;

use eframe::egui::{self, Context, Ui};

use super::{
    config::{Contact, ticket_is_valid}, options::CLOSE_GREY, contact_chip, pointer_over,
    pointer_pressed_over, pulse_factor, soft_glow, top_bevel, App, Tab, ACCENT, ERROR,
    WATCH_ACCENT,
};

/// The row's name type: derived from the theme's body size, lifted a touch and set bold,
/// so a card reads as *name first, ticket second* — the same two-step hierarchy the tab
/// headings use (`body * 1.3`), kept visibly above body without hardcoding a raw size.
const NAME_LIFT: f32 = 1.5;

/// The per-row action widths, so the fields can split what is left exactly.
const LOAD_W: f32 = 74.0;
/// The view-mode "Edit" pill.
const EDIT_W: f32 = 64.0;
/// The edit-mode "Save" pill.
const SAVE_W: f32 = 64.0;
/// The edit-mode "Delete" pill — opens the confirmation modal.
const DELETE_W: f32 = 74.0;
/// The per-row action pills' height — one size for "Load"/"Edit"/"Save"/"Delete", so the
/// actions line up whether a row is read or open.
const PILL_H: f32 = 24.0;
/// The action pills' corner radius — half their height, so they read as true pills (the
/// app's rounded language, matching the cards' 7 and the window's 8).
const PILL_R: egui::CornerRadius = egui::CornerRadius::same(12);
/// The pill label's fallback size: the compact size used when the theme's `Button` size
/// would overflow the fixed pill widths. Pills are interactive controls, so they aim for
/// `Button` (the app's button type) and fall back here rather than widening and breaking
/// the pair-sum layout invariant.
const PILL_TEXT: f32 = 11.0;
/// Quiet margin between the action pills and the card's right edge, in every state
/// (view and edit), so the pills ride the edge without touching the rounded corner.
const RIGHT_PAD: f32 = 10.0;
/// The list stops growing here and scrolls, so the footer always stays on screen.
const LIST_MAX_H: f32 = 248.0;
/// The per-row card's height (avatar, fields and actions all sit inside it).
const ROW_H: f32 = 34.0;
/// Extra height a row grows by at full edit progress (`p`), for the expand animation.
const ROW_GROW: f32 = 12.0;
/// The initials avatar's diameter.
const AVATAR_D: f32 = 24.0;
/// The placeholder card the empty list paints. Matches a fully-opened row, so the panel
/// keeps one rhythm whether it holds a ghost or a contact.
const EMPTY_H: f32 = 46.0;

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

        // Escape while a row edits exits edit mode and keeps the panel up. The modal
        // consumes Escape first while it is up (`consume_key` removes the event), so
        // this only fires when no dialog is standing — and the close check below never
        // sees a key either of the two consumed.
        if self.editing_contact.is_some()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.set_editing(None);
            ctx.request_repaint();
        }

        // The title-bar "×" and Escape both funnel through `close_contacts`, so the
        // staged edits are flushed whichever way the panel goes away. The modal consumes
        // Escape while it is up, so closing the dialog leaves the panel standing.
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_contacts();
        }
    }

    /// The delete-confirmation modal: a dim backdrop that swallows clicks, plus a
    /// centred dialog asking to remove the contact. Confirming deletes the row; the
    /// "Cancel" button, a click on the backdrop, or Escape all dismiss it.
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
            // The removed row was (or was animating out of) edit mode; clear both so no
            // row keeps an expanded state and the slot it leaves behind — now a
            // different contact — never animates open.
            self.editing_contact = None;
            self.contact_edit_prev = None;
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
        self.editing_contact = None;
        self.contact_edit_prev = None;
        // A pending confirmation must not survive the panel — reopening would
        // otherwise raise a stale "Remove …?" dialog over a fresh list.
        self.confirm_remove_contact = None;
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
        self.editing_contact = None;
        self.contact_edit_prev = None;
        self.confirm_remove_contact = None;
        if self.show_contacts {
            self.show_options = false;
            self.contacts_dirty = false;
        }
    }

    /// Move the single-row edit state to `next`, arming the shared tween: the old row
    /// becomes `contact_edit_prev` (so it animates closed) and the clock resets to 0.
    /// A no-op when nothing changes, so re-clicking "Edit" never replays the tween.
    fn set_editing(&mut self, next: Option<usize>) {
        if self.editing_contact == next {
            return;
        }
        self.contact_edit_prev = self.editing_contact;
        self.contact_edit_anim = 0.0;
        self.editing_contact = next;
    }

    /// The window's contents: an intro line, the scrollable list of contact cards, then
    /// the Save/Close footer.
    fn contacts_content(&mut self, ui: &mut Ui) {
        // Type is derived from the theme so the list reads as one family with Watch /
        // Settings: the name is body lifted + bold (name-first hierarchy), the ticket is
        // the theme's monospace (matching the Watch ticket box), and the pills aim for
        // the `Button` size — falling back to the compact `PILL_TEXT` if the longest
        // labels would overflow their fixed widths (which must never widen, or the
        // crossfade's flush-right pills jump). Measured with a bold safety margin, since
        // `layout_no_wrap` measures the regular face while the labels are bold.
        let body = ui.style().text_styles[&egui::TextStyle::Body].size;
        let mono = ui.style().text_styles[&egui::TextStyle::Monospace].size;
        let btn = ui.style().text_styles[&egui::TextStyle::Button].size;
        let pill_pad = ui.spacing().button_padding.x;
        let pill_fits = |label: &str, w: f32, size: f32| {
            let gw = ui
                .painter()
                .layout_no_wrap(label.to_string(), egui::FontId::proportional(size), egui::Color32::WHITE)
                .rect
                .width();
            gw * 1.15 + pill_pad * 2.0 + 2.0 <= w
        };
        let pill_size = if pill_fits("In the box", LOAD_W, btn) && pill_fits("Delete", DELETE_W, btn) {
            btn
        } else {
            PILL_TEXT
        };
        let name_font = egui::FontId::proportional(body + NAME_LIFT);
        let ticket_font = egui::FontId::monospace(mono);

        // The intro line leads; the saved count rides the right edge as a quiet pill —
        // the same chip language the Watch tab uses for the loaded contact, so the two
        // panels read as one set. It is a fact, not a live signal, so it stays static.
        let saved = self.config.contacts.len();
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Pick a contact to load their ticket into the Watch tab.")
                    .weak()
                    .size(body),
            );
            if saved > 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    contact_chip(ui, &format!("{saved} saved"), CLOSE_GREY);
                });
            }
        });
        ui.add_space(8.0);

        // Every mutation is collected as an index during layout and applied once the list
        // has been built — the `Vec` is never mutated while it is being iterated, and the
        // buttons keep their final (settled) rects between frames.
        let mut add = false;
        let mut load_idx: Option<usize> = None;
        let mut edit_idx: Option<usize> = None;
        let mut save_idx: Option<usize> = None;
        let mut remove_idx: Option<usize> = None;
        let mut dirty = self.contacts_dirty;
        // Read Enter once, up front: a key consumed by a widget later in this frame must
        // not defeat Enter-to-save.
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));

        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(LIST_MAX_H)
            .show(ui, |ui| {
                if self.config.contacts.is_empty() {
                    // A calm, centred empty state: a placeholder card — the ghost of the
                    // row that will appear — so the panel reads as "waiting" rather than
                    // "nothing here". A stroked ghost avatar and two muted bars stand in
                    // for the name and ticket a real row carries.
                    ui.vertical_centered(|ui| {
                        ui.add_space(14.0);
                        let card_w = ui.available_width().min(340.0);
                        let (card, _) =
                            ui.allocate_exact_size(egui::vec2(card_w, EMPTY_H), egui::Sense::hover());
                        let corner = egui::CornerRadius::same(7);
                        ui.painter()
                            .rect_filled(card, corner, egui::Color32::from_white_alpha(4));
                        // The ghost carries the same additive top-light as a live card, so
                        // the empty state keeps one lit-from-above rhythm with the list.
                        top_light(ui.painter(), card, 7);
                        ui.painter().rect_stroke(
                            card,
                            corner,
                            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(12)),
                            egui::StrokeKind::Inside,
                        );
                        top_bevel(ui.painter(), card, 7);
                        let ghost = egui::pos2(card.min.x + 8.0 + AVATAR_D * 0.5, card.center().y);
                        ui.painter().circle_stroke(
                            ghost,
                            AVATAR_D * 0.5,
                            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(22)),
                        );
                        // The two bars: name (shorter, brighter) above ticket (longer,
                        // dimmer) — the same visual weight order as a filled row.
                        let bar_x = ghost.x + AVATAR_D * 0.5 + 12.0;
                        let bar_room = (card.max.x - 14.0 - bar_x).max(0.0);
                        for (dy, frac, alpha) in [(-6.0, 0.42f32, 22u8), (6.0, 0.66, 13)] {
                            let bar = egui::Rect::from_min_size(
                                egui::pos2(bar_x, ghost.y + dy - 2.0),
                                egui::vec2(bar_room * frac, 4.0),
                            );
                            ui.painter().rect_filled(
                                bar,
                                egui::CornerRadius::same(2),
                                egui::Color32::from_white_alpha(alpha),
                            );
                        }
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new(
                                "No contacts yet — add one to fill the Watch tab in one click.",
                            )
                            .weak()
                            .size(body),
                        );
                        ui.add_space(14.0);
                    });
                }

                for (i, contact) in self.config.contacts.iter_mut().enumerate() {
                    // The row whose ticket is sitting in the Watch box: tinted, with its
                    // action reading "In the box" — there is nothing left to load.
                    let active = !contact.ticket.is_empty() && contact.ticket == self.watch_ticket;

                    // Per-row edit progress, read off the panel's single shared tween:
                    // the editing row fades in (`p` rises), the row animating out fades
                    // back (`1 - p`), everything else rests at 0.
                    let editing = self.editing_contact == Some(i);
                    let p = if editing {
                        self.contact_edit_anim
                    } else if self.contact_edit_prev == Some(i) {
                        1.0 - self.contact_edit_anim
                    } else {
                        0.0
                    };
                    // Allocation rule: widgets are shown/hidden with the tween, never
                    // painted at zero alpha (a zero-alpha shape survives in the frame
                    // output). The two sets overlap only mid-tween, so the row
                    // crossfades.
                    let show_view = p < 0.98;
                    let show_edit = p > 0.02;

                    ui.horizontal(|ui| {
                        // The card: a rounded surface spanning the row — tinted
                        // watch-accent when its ticket is loaded, lifting toward the
                        // accent (with a soft glow) as the edit state opens. Painted
                        // first (from the row's full width) so widgets sit on top of
                        // it. No hover brightening: hovering the row lights up the
                        // whole card and washes out the pills' own hover feedback.
                        let avail = ui.available_width();
                        // The open motion is eased (`ease(p)`) so the card grows quickly
                        // then settles gently; settled geometry (p = 1) is unchanged.
                        let grow = ROW_GROW * ease(p);
                        let row_rect =
                            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(avail, ROW_H + grow));
                        // A quiet hover cue on a non-active row: a brighter stroke and a
                        // thin left accent tick. It deliberately leaves the pills' own
                        // hover fills untouched, so the row's feedback never washes out
                        // theirs (the old full-card brightening did exactly that).
                        let row_hovered = !active && pointer_over(ui, row_rect);
                        let fill = if active {
                            WATCH_ACCENT.gamma_multiply(0.12)
                        } else if p > 0.02 {
                            egui::Color32::from_white_alpha(5)
                                .lerp_to_gamma(ACCENT.gamma_multiply(0.10), p)
                        } else {
                            egui::Color32::from_white_alpha(5)
                        };
                        let stroke = if active {
                            egui::Stroke::new(1.0, WATCH_ACCENT.gamma_multiply(0.55))
                        } else {
                            egui::Stroke::new(
                                1.0,
                                egui::Color32::from_white_alpha(if row_hovered { 30 } else { 16 })
                                    .lerp_to_gamma(ACCENT.gamma_multiply(0.55), p),
                            )
                        };
                        let corner = egui::CornerRadius::same(7);
                        // The accent halo goes down *before* the card, so only the rim
                        // shows — it marks the edit state opening, not pointer hover.
                        if p > 0.02 {
                            soft_glow(ui.painter(), row_rect, corner, ACCENT.gamma_multiply(p));
                        }
                        ui.painter().rect_filled(row_rect, corner, fill);
                        // A faint additive top-light over the (untouched) fill, so the
                        // card reads as lit from above without altering its tint.
                        top_light(ui.painter(), row_rect, 7);
                        ui.painter().rect_stroke(
                            row_rect,
                            corner,
                            stroke,
                            egui::StrokeKind::Inside,
                        );
                        // A faint "light catch" along the top edge, so the card reads as
                        // lit from above — the same depth cue the central panel's ground
                        // and the error callout use.
                        top_bevel(ui.painter(), row_rect, 7);
                        // A loaded contact gets a solid bar down its left edge — the
                        // app's left-bar language (`section_tick`, the error callout) —
                        // breathing gently on the shared pulse clock so "in the box" reads
                        // as live while the panel is open. A hovered (not loaded) row gets
                        // the same left-edge tick in the action accent instead.
                        if active {
                            let pulse = pulse_factor(self.pulse.elapsed().as_secs_f32());
                            let bar = egui::Rect::from_min_size(
                                egui::pos2(row_rect.min.x + 3.0, row_rect.min.y + 6.0),
                                egui::vec2(3.0, row_rect.height() - 12.0),
                            );
                            ui.painter().rect_filled(
                                bar,
                                egui::CornerRadius::same(1),
                                WATCH_ACCENT.gamma_multiply(0.9 * pulse),
                            );
                        } else if row_hovered {
                            let tick = egui::Rect::from_min_size(
                                egui::pos2(row_rect.min.x + 3.0, row_rect.min.y + 6.0),
                                egui::vec2(3.0, row_rect.height() - 12.0),
                            );
                            ui.painter().rect_filled(
                                tick,
                                egui::CornerRadius::same(1),
                                ACCENT.gamma_multiply(0.55),
                            );
                        }

                        // A zero-width, full-card-height spacer: a horizontal row is
                        // only as tall as its tallest child (the 24px pills), so
                        // without it every widget rides the card's top edge. Claiming
                        // the full height puts the row's centre line in the middle of
                        // the card, and everything centers inside it with a clean
                        // margin above and below.
                        ui.allocate_exact_size(
                            egui::vec2(0.0, ROW_H + grow),
                            egui::Sense::hover(),
                        );
                        // Inset the initials avatar from the card's left edge.
                        ui.add_space(8.0);
                        let (avatar_rect, _resp) = ui.allocate_exact_size(
                            egui::vec2(AVATAR_D, AVATAR_D),
                            egui::Sense::hover(),
                        );
                        let (initials, hue) = avatar_for(&contact.name);
                        // The avatar eases from 90% to full size as the row opens, so the
                        // contact's face "wakes up" when its row goes into edit mode —
                        // softened by `ease`, settling exactly at full size.
                        let av_scale = 0.9 + 0.1 * ease(p);
                        let av_r = (AVATAR_D * 0.5 - 1.0) * av_scale;
                        let [ar, ag, ab, _aa] =
                            egui::epaint::Hsva::new(hue, 0.42, 0.5, 1.0).to_srgba_unmultiplied();
                        let av_bg = egui::Color32::from_rgba_unmultiplied(ar, ag, ab, 255);
                        ui.painter().circle_filled(avatar_rect.center(), av_r, av_bg);
                        // A faint white sheen in the avatar's upper arc — the same lit-from-
                        // above cue the cards carry, kept inside the disc so it reads as a
                        // highlight rather than a blob.
                        ui.painter().circle_filled(
                            egui::pos2(avatar_rect.center().x, avatar_rect.center().y - av_r * 0.32),
                            av_r * 0.55,
                            egui::Color32::from_white_alpha(12),
                        );
                        // A 1px ring in a brighter shade of the same hue gives each
                        // contact a distinct, polished face — the app's stroked-pill
                        // language, applied to a circle.
                        let [rr, rg, rb, _ra] =
                            egui::epaint::Hsva::new(hue, 0.42, 0.74, 1.0).to_srgba_unmultiplied();
                        // The ring rides the tween with the avatar's scale, so the face
                        // brightens as its row opens instead of only growing.
                        let ring = egui::Color32::from_rgba_unmultiplied(
                            rr,
                            rg,
                            rb,
                            (140.0 + 95.0 * p).min(235.0) as u8,
                        );
                        ui.painter().circle_stroke(
                            avatar_rect.center(),
                            av_r,
                            egui::Stroke::new(1.0, ring),
                        );
                        ui.painter().text(
                            avatar_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            &initials,
                            egui::FontId::proportional(11.0 * av_scale),
                            egui::Color32::from_rgb(245, 245, 250),
                        );
                        ui.add_space(4.0);

                        // Split the row explicitly: fields flush left, action pills
                        // flush right. Both the view and the edit set live in the SAME
                        // band — two overlapping child UIs — so laying one out never
                        // advances the other's cursor. Reserving the band once in the
                        // parent keeps the row (and the pinned panel) from ever growing
                        // wider for the few frames the crossfade runs both states at
                        // once. Left to flow in sequence, the second block started at the
                        // first block's right margin and spilled past the card.
                        let band_min = ui.cursor().min;
                        let band_w = (row_rect.max.x - RIGHT_PAD - band_min.x).max(0.0);
                        let band_h = row_rect.height();
                        ui.allocate_exact_size(egui::vec2(band_w, band_h), egui::Sense::hover());
                        let band = egui::Rect::from_min_size(band_min, egui::vec2(band_w, band_h));
                        let gap = ui.spacing().item_spacing.x;
                        // Both states carry two pills of the same combined width, so the
                        // fields split the leftover identically in either child.
                        let fields = (band_w - (LOAD_W + EDIT_W) - gap * 2.0).max(64.0);
                        // A name caps at 42% of the budget: the ticket is the string that
                        // hurts to retype, so it keeps the bulk of the room.
                        let name_w = (fields * 0.42).clamp(28.0, 190.0).min(fields);
                        let ticket_w = (fields - name_w - gap).max(24.0);
                        let band_layout = egui::Layout::left_to_right(egui::Align::Center);

                        if show_view {
                            // View mode: read-only labels and the two actions, fading
                            // out as the row opens.
                            let mut vu = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(band)
                                    .layout(band_layout)
                                    .id_salt(("contact_view_row", i)),
                            );
                            let a = 1.0 - p;
                            let named = !contact.name.trim().is_empty();
                            let (name_disp, name_cut) = fit_text(
                                &vu,
                                if named { &contact.name } else { "Unnamed" },
                                &name_font,
                                name_w,
                            );
                            let name_col = if named {
                                egui::Color32::WHITE
                            } else {
                                egui::Color32::GRAY
                            };
                            let name_resp = vu.label(
                                egui::RichText::new(name_disp)
                                    .font(name_font.clone())
                                    .strong()
                                    .color(name_col.gamma_multiply(a)),
                            );
                            if name_cut {
                                name_resp.on_hover_text(&contact.name);
                            }
                            let (ticket_disp, ticket_cut) =
                                fit_text(&vu, &contact.ticket, &ticket_font, ticket_w);
                            let ticket_resp = vu.label(
                                egui::RichText::new(ticket_disp)
                                    .font(ticket_font.clone())
                                    .color(egui::Color32::PLACEHOLDER.gamma_multiply(a)),
                            );
                            // A cut ticket is the one thing worth revealing in full: the
                            // whole string comes back on hover.
                            if ticket_cut {
                                ticket_resp.on_hover_text(&contact.ticket);
                            }

                            // The spacer between the labels and the pills: exactly the
                            // slack the labels did not use, so "Edit" lands flush with
                            // the card's right margin.
                            let pad =
                                (band.max.x - (LOAD_W + EDIT_W + gap) - vu.cursor().min.x).max(0.0);
                            vu.add_space(pad);

                            // "Load" is the row's primary action (accent-filled); the
                            // loaded row reads "In the box" and leaves the interactive
                            // set. Flat paint, no halo — the fill and stroke just
                            // brighten a touch on hover, dim on press, riding the row's
                            // alpha `a` so the pill fades as one object.
                            let load_rect = egui::Rect::from_min_size(
                                egui::pos2(vu.cursor().min.x, row_rect.center().y - PILL_H * 0.5),
                                egui::vec2(LOAD_W, PILL_H),
                            );
                            let load_hovered = pointer_over(&vu, load_rect);
                            let load_pressed = pointer_pressed_over(&vu, load_rect);
                            // A soft accent halo on hover — the app's interactive-surface
                            // language (the copy box uses the same). Painted before the
                            // pill, so only the rim shows; it rides the row's alpha `a`.
                            if load_hovered && !active {
                                soft_glow(vu.painter(), load_rect, PILL_R, ACCENT.gamma_multiply(a));
                            }
                            let load = egui::Button::new(pill_label(
                                if active { "In the box" } else { "Load" },
                                if active {
                                    WATCH_ACCENT.gamma_multiply(a)
                                } else {
                                    egui::Color32::WHITE.gamma_multiply(a)
                                },
                                pill_size,
                            ))
                            .min_size(egui::vec2(LOAD_W, PILL_H))
                            .corner_radius(PILL_R)
                            .fill(if active {
                                WATCH_ACCENT.gamma_multiply(0.16 * a)
                            } else if load_pressed {
                                ACCENT.gamma_multiply(0.40 * a)
                            } else if load_hovered {
                                ACCENT.gamma_multiply(0.34 * a)
                            } else {
                                ACCENT.gamma_multiply(0.24 * a)
                            })
                            .stroke(egui::Stroke::new(
                                1.0,
                                if active {
                                    WATCH_ACCENT.gamma_multiply(0.5 * a)
                                } else if load_pressed {
                                    ACCENT.gamma_multiply(0.55 * a)
                                } else if load_hovered {
                                    ACCENT.gamma_multiply(0.95 * a)
                                } else {
                                    ACCENT.gamma_multiply(0.7 * a)
                                },
                            ));
                            if vu.add_enabled(!active, load).clicked() {
                                load_idx = Some(i);
                            }

                            // "Edit" opens this row's edit state (one row at a time);
                            // the grey pill sits right of "Load". Flat paint, no halo;
                            // its whole treatment fades with the row, matching "Load".
                            let edit_rect = egui::Rect::from_min_size(
                                egui::pos2(vu.cursor().min.x, row_rect.center().y - PILL_H * 0.5),
                                egui::vec2(EDIT_W, PILL_H),
                            );
                            let edit_hovered = pointer_over(&vu, edit_rect);
                            let edit_pressed = pointer_pressed_over(&vu, edit_rect);
                            let edit = egui::Button::new(pill_label(
                                "Edit",
                                CLOSE_GREY.gamma_multiply(a),
                                pill_size,
                            ))
                            .min_size(egui::vec2(EDIT_W, PILL_H))
                            .corner_radius(PILL_R)
                            .fill(CLOSE_GREY.gamma_multiply(
                                (if edit_pressed {
                                    0.10
                                } else if edit_hovered {
                                    0.18
                                } else {
                                    0.12
                                }) * a,
                            ))
                            .stroke(egui::Stroke::new(
                                1.0,
                                CLOSE_GREY.gamma_multiply(
                                    (if edit_pressed {
                                        0.30
                                    } else if edit_hovered {
                                        0.60
                                    } else {
                                        0.40
                                    }) * a,
                                ),
                            ));
                            if vu.add(edit).clicked() {
                                edit_idx = Some(i);
                            }
                        }

                        if show_edit {
                            // Edit mode: editable fields, green "Save", red "Delete",
                            // fading in as the row opens. Its own child UI, laid out in
                            // the same band as the view set so the two crossfade in place.
                            let mut eu = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(band)
                                    .layout(band_layout)
                                    .id_salt(("contact_edit_row", i)),
                            );
                            let name = eu.add(
                                egui::TextEdit::singleline(&mut contact.name)
                                    .hint_text("Name")
                                    .desired_width(name_w),
                            );
                            // Stable ids per row, so Enter-to-save can be read off the
                            // focused field.
                            let ticket_id = egui::Id::new(("contact_ticket", i));
                            let ticket = eu.add(
                                egui::TextEdit::singleline(&mut contact.ticket)
                                    .id(ticket_id)
                                    .hint_text("Paste their ticket here")
                                    .desired_width(ticket_w)
                                    .font(egui::TextStyle::Monospace),
                            );
                            // Enter in a focused field saves the row — the same gesture
                            // as the Watch tab's Enter-to-play, one step earlier in the
                            // flow.
                            if enter
                                && (eu.ctx().memory(|m| m.has_focus(ticket_id))
                                    || eu.ctx().memory(|m| m.has_focus(name.id)))
                            {
                                save_idx = Some(i);
                            }
                            // Staged, not written: the flush happens when the panel closes.
                            if name.changed() || ticket.changed() {
                                dirty = true;
                            }
                            // "Add contact" / "Edit" arms this, so the caret lands in
                            // the row's name field. Cleared straight away: a standing
                            // request would yank focus back every frame and defeat
                            // anything the user clicks next.
                            if self.focus_contact_row == Some(i) {
                                name.request_focus();
                                self.focus_contact_row = None;
                            }

                            // Same spacer as the view pair: push "Save"/"Delete" onto
                            // the card's right margin, off the rounded corner.
                            let pad =
                                (band.max.x - (SAVE_W + DELETE_W + gap) - eu.cursor().min.x).max(0.0);
                            eu.add_space(pad);

                            // Green "Save": persists immediately, raises the toast, and
                            // drops the row back to view mode.
                            let save_rect = egui::Rect::from_min_size(
                                egui::pos2(eu.cursor().min.x, row_rect.center().y - 12.0),
                                egui::vec2(SAVE_W, 24.0),
                            );
                            let save_hovered = pointer_over(&eu, save_rect);
                            let save_pressed = pointer_pressed_over(&eu, save_rect);
                            // The accent halo blooms on hover, riding the row's open alpha.
                            if save_hovered {
                                soft_glow(eu.painter(), save_rect, PILL_R, ACCENT.gamma_multiply(p));
                            }
                            let save = egui::Button::new(pill_label(
                                "Save",
                                ACCENT.gamma_multiply(p),
                                pill_size,
                            ))
                            .min_size(egui::vec2(SAVE_W, 24.0))
                            .corner_radius(PILL_R)
                            .fill(ACCENT.gamma_multiply(if save_pressed {
                                0.40
                            } else if save_hovered {
                                0.34
                            } else {
                                0.24
                            }))
                            .stroke(egui::Stroke::new(
                                1.0,
                                ACCENT.gamma_multiply(if save_pressed {
                                    0.55
                                } else if save_hovered {
                                    0.95
                                } else {
                                    0.70
                                }),
                            ));
                            if eu.add(save).clicked() {
                                save_idx = Some(i);
                            }

                            // Red "Delete": opens the confirmation modal — a stray click
                            // can never delete a contact. The row keeps editing behind
                            // the dialog.
                            let del_rect = egui::Rect::from_min_size(
                                egui::pos2(eu.cursor().min.x, row_rect.center().y - 12.0),
                                egui::vec2(DELETE_W, 24.0),
                            );
                            let del_hovered = pointer_over(&eu, del_rect);
                            let del_pressed = pointer_pressed_over(&eu, del_rect);
                            // A red halo on hover — the destructive affordance, in the
                            // error hue the callouts already use, riding the open alpha.
                            if del_hovered {
                                soft_glow(eu.painter(), del_rect, PILL_R, ERROR.gamma_multiply(p));
                            }
                            let delete =
                                egui::Button::new(pill_label(
                                "Delete",
                                ERROR.gamma_multiply(p),
                                pill_size,
                            ))
                                    .min_size(egui::vec2(DELETE_W, 24.0))
                                    .corner_radius(PILL_R)
                            .fill(ERROR.gamma_multiply(if del_pressed {
                                0.40
                            } else if del_hovered {
                                0.34
                            } else {
                                0.16
                            }))
                            .stroke(egui::Stroke::new(
                                1.0,
                                ERROR.gamma_multiply(if del_pressed {
                                    0.55
                                } else if del_hovered {
                                    0.95
                                } else {
                                    0.50
                                }),
                            ));
                            if eu.add(delete).clicked() {
                                remove_idx = Some(i);
                            }
                        }
                    });
                    // The gap below the row grows with the card, keeping the rhythm.
                    ui.add_space(4.0 + ROW_GROW * ease(p));
                }
            });

        ui.add_space(2.0);
        let add_btn = egui::Button::new(pill_label("+  Add contact", ACCENT, pill_size))
            .min_size(egui::vec2(0.0, PILL_H))
            .corner_radius(PILL_R)
            .fill(ACCENT.gamma_multiply(0.14))
            .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.45)));
        if ui.add(add_btn).clicked() {
            add = true;
        }

        // Apply what the list collected.
        if add {
            self.config.contacts.push(Contact::default());
            let idx = self.config.contacts.len() - 1;
            // The new row opens straight into edit mode, caret in the name field.
            self.set_editing(Some(idx));
            self.focus_contact_row = Some(idx);
            dirty = true;
        }
        if let Some(i) = edit_idx {
            // One row at a time: the clicked row animates open, the row that was
            // editing (if any) animates closed; the caret lands in the name field.
            self.set_editing(Some(i));
            self.focus_contact_row = Some(i);
            ui.ctx().request_repaint();
        }
        if let Some(i) = save_idx {
            // Row "Save" writes the config now, raises the toast, and returns the row
            // to view mode — but only if the ticket parses. A malformed ticket would
            // never dial, so refuse the save, flag the field with a red toast, and
            // leave the row open for correction.
            if ticket_is_valid(&self.config.contacts[i].ticket) {
                self.config.save();
                dirty = false;
                self.set_editing(None);
                self.toast_error = false;
                self.toast = Some(("Saved ✓".to_string(), Instant::now()));
            } else {
                self.toast_error = true;
                self.toast = Some(("Invalid ticket — check and try again".to_string(), Instant::now()));
            }
            ui.ctx().request_repaint();
        }
        // A "Delete" click only *requests* removal — the row is deleted from the
        // confirmation modal (rendered by `contacts_window`), never straight from the
        // list. The row keeps editing behind the dialog.
        if let Some(i) = remove_idx {
            self.confirm_remove_contact = Some(i);
            ui.ctx().request_repaint();
        }
        if let Some(i) = load_idx {
            // Fill the Watch tab's ticket box and drop the panel, so the filled box and
            // its new name chip are revealed on the very next frame. Loading is a
            // watch-side gesture, so a contact loaded from the Broadcast tab jumps back
            // to Watch.
            self.watch_ticket = self.config.contacts[i].ticket.clone();
            self.show_contacts = false;
            self.focus_contact_row = None;
            self.editing_contact = None;
            self.contact_edit_prev = None;
            self.confirm_remove_contact = None;
            self.switch_tab(Tab::Watch);
            if dirty {
                self.config.save();
            }
            self.contacts_dirty = false;
            ui.ctx().request_repaint();
            return;
        }
        self.contacts_dirty = dirty;
    }
}

/// A pill's label: one compact, bold type at `size`, so "Load"/"Edit"/"Save"/
/// "Delete"/"Add contact" all read as the same family of control. `size` is the theme's
/// `Button` size when it fits the fixed pill widths, else [`PILL_TEXT`] (see
/// `contacts_content`).
fn pill_label(text: &str, color: egui::Color32, size: f32) -> egui::RichText {
    egui::RichText::new(text)
        .size(size)
        .strong()
        .color(color)
}

/// A faint *additive* top-light over a card: white at the top edge fading to transparent
/// ~40% down, so the surface reads as lit from above. Mirrors the central panel's
/// [`super::central_gradient`] mesh, but painted *over* the card's tuned fill (which is
/// left untouched) — so it brightens without re-deriving (and so altering) the fill's
/// tint. Inset by `corner` so it never spills past the rounded corners.
fn top_light(painter: &egui::Painter, rect: egui::Rect, corner: u8) {
    let inset = f32::from(corner);
    let x0 = rect.min.x + inset;
    let x1 = rect.max.x - inset;
    let y0 = rect.min.y + 1.0;
    let y1 = (rect.min.y + rect.height() * 0.4).min(rect.max.y);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let mut mesh = egui::epaint::Mesh::default();
    mesh.colored_vertex(egui::pos2(x0, y0), egui::Color32::from_white_alpha(9));
    mesh.colored_vertex(egui::pos2(x1, y0), egui::Color32::from_white_alpha(9));
    mesh.colored_vertex(egui::pos2(x1, y1), egui::Color32::TRANSPARENT);
    mesh.colored_vertex(egui::pos2(x0, y1), egui::Color32::TRANSPARENT);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// Ease-out cubic on a 0↔1 tween: arrives quickly, settles gently, and lands exactly on
/// `1.0` at `p = 1` (so settled geometry is byte-identical to the linear case). Softens
/// the row's open motion without introducing a new clock — it is a pure function of the
/// existing shared `p`.
fn ease(p: f32) -> f32 {
    let q = 1.0 - p;
    1.0 - q * q * q
}

/// Fit `text` into `max_w` pixels at `font_id`, eliding with "…" if it does not fit.
///
/// Char-count truncation is useless here: a proportional face makes "Wi-Fi" and "iiii"
/// wildly different widths, so the cut has to be measured, not counted. Returns the
/// string to draw and whether it was cut, so the caller can offer the whole string on
/// hover.
fn fit_text(ui: &Ui, text: &str, font_id: &egui::FontId, max_w: f32) -> (String, bool) {
    let width_of = |s: &str| {
        ui.painter()
            .layout_no_wrap(s.to_string(), font_id.clone(), egui::Color32::WHITE)
            .rect
            .width()
    };
    if max_w <= 0.0 || width_of(text) <= max_w {
        return (text.to_string(), false);
    }
    let chars: Vec<char> = text.chars().collect();
    // Binary-search the longest prefix that still fits once the ellipsis joins it.
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if width_of(&candidate) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    // Drop a trailing space before the ellipsis — "Alice …" reads as a gap, not a cut.
    let mut out: String = chars[..lo].iter().collect();
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    out.push('…');
    (out, true)
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
