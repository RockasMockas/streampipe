//! The Contacts overlay: where it opens from, how it behaves, and what it persists.
//!
//! The overlay is standalone (not a Settings section), reachable from the footer bar's
//! "Contacts" button and the Watch tab's "Select Contact". Picking a contact fills the
//! Watch ticket box and chips the name on the tab.

mod common;

use eframe::egui;
use streampipe::gui::{
    config::{Config, Contact},
    Tab, ACCENT,
};

use common::{
    click_at, drive_frames, drive_until_settled, focused_raw, frame, real_ticket, render_shapes,
    test_app, test_app_with, text_present, text_rect,
};

/// The size these tests drive the window at.
const SIZE: egui::Vec2 = egui::vec2(760.0, 560.0);

/// An Escape key press event.
fn escape_key() -> egui::Event {
    egui::Event::Key {
        key: egui::Key::Escape,
        physical_key: Some(egui::Key::Escape),
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

/// Count the painted text shapes with exactly this text (a fn, so it works on any
/// frame's shapes — a closure capturing `out` would outlive the shadowed rebinding).
fn count_text(shapes: &[egui::epaint::ClippedShape], needle: &str) -> usize {
    shapes
        .iter()
        .filter(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == needle))
        .count()
}

/// The footer bar's "Contacts" button sits immediately to the right of "Settings", both
/// inside the bottom status bar.
#[test]
fn the_footer_bar_carries_a_contacts_button_right_of_settings() {
    let (mut app, _rt) = test_app();
    let shapes = render_shapes(&mut app, SIZE);

    let settings = text_rect(&shapes, "Settings").expect("the footer paints 'Settings'");
    let contacts = text_rect(&shapes, "Contacts").expect("the footer paints 'Contacts'");

    assert!(
        contacts.min.x > settings.max.x,
        "'Contacts' sits to the right of 'Settings': {settings:?} vs {contacts:?}"
    );
    // Both live in the bottom bar (the bar is ~38 tall above the window's bottom edge).
    for rect in [settings, contacts] {
        assert!(
            rect.min.y > SIZE.y - 44.0 && rect.max.y <= SIZE.y,
            "footer label should be in the bottom bar, got {rect:?}"
        );
    }
    // On the same line, so they read as a pair of buttons.
    assert!(
        (settings.center().y - contacts.center().y).abs() < 2.0,
        "the two footer buttons share a baseline: {settings:?} vs {contacts:?}"
    );
}

/// Clicking the footer's "Contacts" button opens the Contacts overlay, and clicking it
/// again closes it.
#[test]
fn the_contacts_button_toggles_the_overlay() {
    let (mut app, _rt) = test_app();
    assert!(!app.show_contacts, "the overlay starts closed");
    let ctx = egui::Context::default();

    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let btn = text_rect(&out.shapes, "Contacts").expect("the footer paints 'Contacts'");

    click_at(&mut app, &ctx, btn.center(), SIZE);
    assert!(app.show_contacts, "the click opened the overlay");

    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    assert!(
        text_present(
            &out.shapes,
            "Pick a contact to load their ticket into the Watch tab."
        ),
        "the open overlay explains itself, got {:?}",
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );

    // The footer is still reachable (the overlay is centred, not full-width), so the
    // same button closes it again.
    let btn = text_rect(&out.shapes, "Contacts").expect("the footer is not covered");
    click_at(&mut app, &ctx, btn.center(), SIZE);
    assert!(!app.show_contacts, "the second click closed the overlay");
}

/// The two overlays occupy the same space, so opening one dismisses the other — they are
/// never both on screen.
#[test]
fn contacts_and_settings_are_mutually_exclusive() {
    let (mut app, _rt) = test_app();
    let ctx = egui::Context::default();
    let open_settings = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), SIZE)),
        focused: true,
        events: vec![egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: Some(egui::Key::Comma),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        }],
        ..Default::default()
    };

    // Cmd+, opens Settings (and there is nothing else to dismiss yet).
    frame(&ctx, &mut app, open_settings());
    assert!(app.show_options, "Cmd+, opened Settings");
    assert!(!app.show_contacts);

    // The footer's "Contacts" is below the Settings window, so it is clickable: opening
    // the picker must take Settings down.
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    let contacts = text_rect(&out.shapes, "Contacts").expect("the footer paints 'Contacts'");
    click_at(&mut app, &ctx, contacts.center(), SIZE);
    assert!(
        app.show_contacts && !app.show_options,
        "Contacts opened and Settings was dismissed"
    );

    // Same in reverse: the shortcut reopens Settings and drops the picker.
    frame(&ctx, &mut app, open_settings());
    assert!(
        app.show_options && !app.show_contacts,
        "Settings opened and Contacts was dismissed"
    );
}

/// The Watch tab's "Select Contact" button opens the Contacts overlay (not Settings).
#[test]
fn select_contact_on_the_watch_tab_opens_the_contacts_overlay() {
    let (mut app, _rt) = test_app();
    app.tab = Tab::Watch;
    let ctx = egui::Context::default();

    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let btn = text_rect(&out.shapes, "Select Contact").expect("the button is painted");
    click_at(&mut app, &ctx, btn.center(), SIZE);

    assert!(app.show_contacts, "'Select Contact' opens the overlay");
    assert!(!app.show_options, "it does not open Settings");
}

/// Escape takes the overlay down.
#[test]
fn escape_closes_the_contacts_overlay() {
    let (mut app, _rt) = test_app();
    app.show_contacts = true;
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, focused_raw(vec![escape_key()]));
    assert!(!app.show_contacts, "Escape closes the overlay");
}

/// End to end, the full chore a viewer used to do by hand (find the ticket, paste it)
/// is two clicks: "Select Contact" → "Load". The box ends up holding a real, parseable
/// ticket, the overlay is gone, and the tab chips the contact's name.
#[test]
fn loading_a_contact_fills_the_ticket_and_chips_the_name() {
    use std::str::FromStr;

    let ticket = real_ticket();
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: ticket.clone(),
        }],
        ..Config::default()
    });
    app.tab = Tab::Watch;
    let ctx = egui::Context::default();

    // Nothing loaded yet: no chip, empty box.
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    assert!(
        !text_present(&out.shapes, "Alice"),
        "an empty box claims no contact"
    );

    // "Select Contact" → overlay.
    let btn = text_rect(&out.shapes, "Select Contact").expect("the button is painted");
    click_at(&mut app, &ctx, btn.center(), SIZE);
    assert!(app.show_contacts, "the overlay opened");

    // "Load" → ticket filled, overlay dismissed.
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    let load = text_rect(&out.shapes, "Load").expect("the row's 'Load' button is painted");
    click_at(&mut app, &ctx, load.center(), SIZE);
    assert_eq!(app.watch_ticket, ticket, "'Load' fills the ticket box");
    assert!(!app.show_contacts, "'Load' closes the overlay");

    // The chip appears on the heading row, just left of the "Select Contact" button,
    // and the ticket is watchable.
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    let chip = text_rect(&out.shapes, "Alice").expect("the chip shows the name");
    let btn = text_rect(&out.shapes, "Select Contact").expect("the button is painted");
    assert!(
        chip.min.y < 140.0,
        "the chip rides the 'Stream ticket' heading row, got {chip:?}"
    );
    assert!(
        chip.max.y > btn.min.y - 20.0 && chip.min.y < btn.max.y + 20.0,
        "the chip shares the button's heading row, got chip {chip:?} btn {btn:?}"
    );
    assert!(
        chip.right() < btn.left(),
        "the chip sits just left of the button, got chip {chip:?} btn {btn:?}"
    );
    assert!(
        streampipe::EndpointTicket::from_str(&app.watch_ticket).is_ok(),
        "the filled ticket parses"
    );
    assert_eq!(
        common::watch_button_rects(&app.watch_ticket),
        1,
        "a filled ticket makes the hero 'Watch Stream' button live"
    );
}

/// The row whose ticket is in the box is marked "In the box" and leaves the interactive
/// set (there is nothing left to load); the other row stays live.
#[test]
fn the_loaded_row_reads_in_the_box_and_the_others_stay_loadable() {
    let alice = real_ticket();
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![
            Contact {
                name: "Alice".into(),
                ticket: alice.clone(),
            },
            Contact {
                name: "Bob".into(),
                ticket: "bob-ticket".into(),
            },
        ],
        ..Config::default()
    });
    app.tab = Tab::Watch;
    app.show_contacts = true;
    let ctx = egui::Context::default();

    // Nothing loaded: both rows offer "Load", and nothing reads "In the box".
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let loads: Vec<_> = out
        .shapes
        .iter()
        .filter(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == "Load"))
        .collect();
    assert_eq!(loads.len(), 2, "both rows offer 'Load'");
    assert!(!text_present(&out.shapes, "In the box"));

    // Load the top row (Alice).
    let first = text_rect(&out.shapes, "Load").expect("a 'Load' button");
    click_at(&mut app, &ctx, first.center(), SIZE);
    assert_eq!(app.watch_ticket, alice, "the top row was loaded");
    assert!(!app.show_contacts, "'Load' closes the overlay");

    // Reopen: the loaded row now reads "In the box", the other is still loadable.
    app.show_contacts = true;
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    assert!(
        text_present(&out.shapes, "In the box"),
        "the loaded row is marked"
    );
    assert_eq!(
        out.shapes
            .iter()
            .filter(|s| {
                matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == "Load")
            })
            .count(),
        1,
        "only the unloaded row still offers 'Load'"
    );
}

/// Add a row (it opens in edit mode), save it (persist + toast, back to view mode),
/// edit it again, delete it through the modal. Rows are read-only by default — the
/// fields, "Save" and "Delete" only exist while a row edits.
#[test]
fn contacts_can_be_added_edited_and_removed() {
    // These clicks write config; redirect the config dir (the mutex serialises them).
    let _guard = streampipe::gui::config::CONFIG_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let (mut app, _rt) = test_app();
    app.show_contacts = true;
    let ctx = egui::Context::default();

    // Empty list: the hint shows, and "Add contact" appends a row.
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let hint = "No contacts yet — add one to fill the Watch tab in one click.";
    assert!(
        text_present(&out.shapes, hint),
        "the empty-list hint is painted"
    );
    let add = text_rect(&out.shapes, "+  Add contact").expect("the add button is painted");
    click_at(&mut app, &ctx, add.center(), SIZE);
    assert_eq!(app.config.contacts.len(), 1, "'Add contact' appends a row");
    assert!(
        app.focus_contact_row.is_some(),
        "the new row's name field is armed for typing"
    );

    // The new row opens (animated) in edit mode: the hint recedes, the row carries
    // the two fields, a green "Save" and a red "Delete"; "Load" fades out.
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 1);
    assert!(
        !text_present(&out.shapes, hint),
        "the empty-list hint recedes once there is a row"
    );
    assert!(text_present(&out.shapes, "Name"));
    assert!(text_present(&out.shapes, "Paste their ticket here"));
    assert!(text_present(&out.shapes, "Delete"));
    assert!(
        !text_present(&out.shapes, "Load"),
        "'Load' is gone while the row edits"
    );

    // The row's "Save" (the first match — the footer's is below it) writes the list,
    // raises the confirmation pill, and returns the row to view mode. Fill a parseable
    // ticket first: an empty/invalid ticket is now refused with a red error toast.
    app.config.contacts[0].ticket = real_ticket();
    let save = text_rect(&out.shapes, "Save").expect("the row's 'Save' button is painted");
    click_at(&mut app, &ctx, save.center(), SIZE);
    assert_eq!(
        app.toast.as_ref().map(|(m, _)| m.as_str()),
        Some("Saved ✓"),
        "Save raises the confirmation toast"
    );
    assert!(
        !app.toast_error,
        "a valid ticket saves without the error toast"
    );
    let written = std::fs::read_to_string(dir.path().join("streampipe").join("config.json"))
        .expect("Save wrote the redirected config");
    assert!(
        written.contains("\"contacts\""),
        "the saved config carries the contacts key: {written}"
    );

    // The row animates back to view mode: "Edit" is back, "Delete" is gone.
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 1);
    assert!(text_present(&out.shapes, "Edit"));
    assert!(
        !text_present(&out.shapes, "Delete"),
        "no delete affordance outside edit mode"
    );
    assert!(text_present(&out.shapes, "Load"));

    // "Edit" reopens the row; its red "Delete" arms the confirmation modal; nothing
    // is removed until it is confirmed.
    let edit = text_rect(&out.shapes, "Edit").expect("the row's 'Edit' button is painted");
    click_at(&mut app, &ctx, edit.center(), SIZE);
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 1);
    let delete = text_rect(&out.shapes, "Delete").expect("the row's 'Delete' button");
    click_at(&mut app, &ctx, delete.center(), SIZE);
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    assert_eq!(
        app.config.contacts.len(),
        1,
        "the modal is open; nothing removed yet"
    );
    assert!(
        text_present(&out.shapes, "Remove this contact?"),
        "the modal asks to remove the (nameless) contact"
    );

    // Escape dismisses the modal; the row keeps editing behind it.
    frame(&ctx, &mut app, focused_raw(vec![escape_key()]));
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    assert!(
        !text_present(&out.shapes, "Remove this contact?"),
        "Escape dismissed the modal"
    );
    assert_eq!(
        count_text(&out.shapes, "Delete"),
        1,
        "the row is still editing"
    );

    // Re-arm the modal and confirm: the row drops, nothing stays expanded.
    let delete = text_rect(&out.shapes, "Delete").expect("the row's 'Delete' button");
    click_at(&mut app, &ctx, delete.center(), SIZE);
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    let remove = text_rect(&out.shapes, "Remove").expect("the modal's 'Remove' button");
    click_at(&mut app, &ctx, remove.center(), SIZE);
    assert!(app.config.contacts.is_empty(), "confirming removes the row");
    assert_eq!(app.editing_contact, None, "the row left edit mode");
    assert_eq!(app.contact_edit_prev, None, "nothing animates out");

    std::env::remove_var("XDG_CONFIG_HOME");
}

/// An invalid (here, empty) ticket is refused on Save: a red error toast rises, the row
/// stays in edit mode, and nothing is written to disk.
#[test]
fn saving_an_invalid_ticket_is_refused_with_an_error_toast() {
    let _guard = streampipe::gui::config::CONFIG_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let (mut app, _rt) = test_app();
    app.show_contacts = true;
    let ctx = egui::Context::default();

    // Add a row (opens in edit mode, empty ticket), settle, then hit "Save".
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let add = text_rect(&out.shapes, "+  Add contact").expect("the add button is painted");
    click_at(&mut app, &ctx, add.center(), SIZE);
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 1);

    let save = text_rect(&out.shapes, "Save").expect("the row's 'Save' button is painted");
    click_at(&mut app, &ctx, save.center(), SIZE);

    assert!(
        app.toast_error,
        "the refusal raises the toast in the error colour"
    );
    assert!(
        app.toast.as_ref().map(|(m, _)| m.as_str()) == Some("Invalid ticket — check and try again"),
        "the error toast explains the refusal: {:?}",
        app.toast
    );
    assert!(
        app.editing_contact.is_some(),
        "the row stays open for correction after a refused save"
    );
    assert!(
        !dir.path().join("streampipe").join("config.json").exists(),
        "a refused save writes nothing to disk"
    );

    std::env::remove_var("XDG_CONFIG_HOME");
}

/// Closing the panel flushes the staged edits, so nothing typed is lost — and an
/// untouched panel writes nothing.
#[test]
fn closing_the_panel_flushes_staged_edits() {
    let _guard = streampipe::gui::config::CONFIG_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let (mut app, _rt) = test_app();
    app.show_contacts = true;
    let ctx = egui::Context::default();

    // Add a row (it opens in edit mode), let the row settle, then close via the
    // title-bar "×" path (`close_contacts`): the panel is gone and the config on disk
    // carries it.
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let add = text_rect(&out.shapes, "+  Add contact").expect("the add button is painted");
    click_at(&mut app, &ctx, add.center(), SIZE);
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    app.close_contacts();

    assert!(!app.show_contacts, "closing dismisses the panel");
    assert!(!app.contacts_dirty, "the staged edit was flushed");
    assert_eq!(app.editing_contact, None, "edit mode was exited on close");
    let written = std::fs::read_to_string(dir.path().join("streampipe").join("config.json"))
        .expect("closing wrote the redirected config");
    let reloaded: Config = serde_json::from_str(&written).unwrap();
    assert_eq!(
        reloaded.contacts.len(),
        1,
        "the added contact survived a restart"
    );

    std::env::remove_var("XDG_CONFIG_HOME");
}

/// Rows in view mode are read-only: they paint avatar, labels, "Load" and "Edit" —
/// no editable fields, no delete affordance, no per-row save. Those widgets exist
/// only while a row is in edit mode (the distinguishing check against the old
/// always-editable layout).
#[test]
fn view_mode_rows_are_read_only() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: "alice-ticket".into(),
        }],
        ..Config::default()
    });
    app.show_contacts = true;
    let ctx = egui::Context::default();

    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    assert!(text_present(&out.shapes, "Load"), "the row offers 'Load'");
    assert!(text_present(&out.shapes, "Edit"), "the row offers 'Edit'");
    assert!(
        !text_present(&out.shapes, "Paste their ticket here"),
        "no ticket field is painted in view mode"
    );
    assert!(
        !text_present(&out.shapes, "Name"),
        "no name-field hint is painted in view mode"
    );
    assert!(
        !text_present(&out.shapes, "Delete"),
        "no delete affordance is painted in view mode"
    );
}

/// Clicking "Edit" animates a row into the edit state; exactly one row edits at a
/// time. Opening the second row animates the first closed and the second open.
/// Probes: "Delete" is only painted by an editing row, "Load" only by a view row
/// (the fields here are non-empty, so the TextEdit hints never paint).
#[test]
fn only_one_row_edits_at_a_time() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![
            Contact {
                name: "Alice".into(),
                ticket: "alice-ticket".into(),
            },
            Contact {
                name: "Bob".into(),
                ticket: "bob-ticket".into(),
            },
        ],
        ..Config::default()
    });
    app.show_contacts = true;
    let ctx = egui::Context::default();

    // View mode: both rows offer "Load"/"Edit"; no delete affordance, and no "Save"
    // at all (saving is per-row, so the footer carries only "Close").
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    assert_eq!(count_text(&out.shapes, "Edit"), 2, "both rows offer 'Edit'");
    assert_eq!(count_text(&out.shapes, "Load"), 2, "both rows offer 'Load'");
    assert_eq!(count_text(&out.shapes, "Delete"), 0);
    assert_eq!(
        count_text(&out.shapes, "Save"),
        0,
        "no whole-list 'Save' in view mode"
    );

    // Open the top row (the first "Edit" match).
    let edit = text_rect(&out.shapes, "Edit").expect("an 'Edit' button");
    click_at(&mut app, &ctx, edit.center(), SIZE);
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 1);
    assert_eq!(
        count_text(&out.shapes, "Delete"),
        1,
        "exactly one row edits"
    );
    assert_eq!(
        count_text(&out.shapes, "Edit"),
        1,
        "the editing row's 'Edit' is gone"
    );
    assert_eq!(
        count_text(&out.shapes, "Load"),
        1,
        "only the idle row offers 'Load'"
    );
    assert_eq!(
        count_text(&out.shapes, "Save"),
        1,
        "only the editing row's green Save paints"
    );

    // Open the idle row via the only remaining "Edit": the first row animates
    // closed (it is now the top row in view mode), the second animates open.
    let edit2 = text_rect(&out.shapes, "Edit").expect("the idle row's 'Edit'");
    click_at(&mut app, &ctx, edit2.center(), SIZE);
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 1);
    assert_eq!(
        count_text(&out.shapes, "Delete"),
        1,
        "still exactly one row edits"
    );
    assert_eq!(
        count_text(&out.shapes, "Edit"),
        1,
        "only the idle row offers 'Edit'"
    );
    // The editing row (with "Delete") is the lower one; the idle row (with "Edit")
    // sits on top.
    let delete_rect = text_rect(&out.shapes, "Delete").expect("'Delete' on the editing row");
    let edit_rect = text_rect(&out.shapes, "Edit").expect("'Edit' on the idle row");
    assert!(
        delete_rect.min.y > edit_rect.min.y,
        "row 2 edits, row 1 is back in view: {delete_rect:?} vs {edit_rect:?}"
    );
}

/// Escape while a row edits exits edit mode and keeps the panel up; Escape with
/// nothing editing closes the panel (the existing behaviour is preserved).
#[test]
fn escape_exits_edit_mode_then_closes_the_panel() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: "alice-ticket".into(),
        }],
        ..Config::default()
    });
    app.show_contacts = true;
    let ctx = egui::Context::default();

    // Open a row's edit mode ("Delete" is the edit-mode probe: the field is
    // non-empty, so its hint never paints).
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let edit = text_rect(&out.shapes, "Edit").expect("the row paints 'Edit'");
    click_at(&mut app, &ctx, edit.center(), SIZE);
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 1);
    assert_eq!(count_text(&out.shapes, "Delete"), 1, "the row is editing");

    // Escape exits edit mode; the panel stays up.
    frame(&ctx, &mut app, focused_raw(vec![escape_key()]));
    assert!(app.show_contacts, "Escape leaves the panel standing");
    assert_eq!(app.editing_contact, None, "Escape exited edit mode");
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    assert!(app.show_contacts, "the panel is still open");
    assert_eq!(
        count_text(&out.shapes, "Delete"),
        0,
        "the row is back in view mode"
    );

    // Escape with nothing editing closes the panel (existing behaviour).
    frame(&ctx, &mut app, focused_raw(vec![escape_key()]));
    assert!(!app.show_contacts, "the second Escape closed the panel");
}

/// The delete confirmation never survives the panel: closing the panel by any route
/// (here Cmd+, opening Settings over it) drops the pending confirmation, so
/// reopening Contacts shows a clean list — not a stale "Remove …?" dialog.
#[test]
fn the_delete_confirmation_does_not_survive_the_panel() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: "alice-ticket".into(),
        }],
        ..Config::default()
    });
    app.show_contacts = true;
    let ctx = egui::Context::default();

    // Edit the row, arm the delete confirmation, and let the modal paint.
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let edit = text_rect(&out.shapes, "Edit").expect("the row paints 'Edit'");
    click_at(&mut app, &ctx, edit.center(), SIZE);
    drive_until_settled(&mut app, &ctx, SIZE, 1.0);
    let out = drive_frames(&mut app, &ctx, SIZE, 1);
    let delete = text_rect(&out.shapes, "Delete").expect("the row paints 'Delete'");
    click_at(&mut app, &ctx, delete.center(), SIZE);
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    assert!(
        text_present(&out.shapes, "Remove Alice?"),
        "the modal is up"
    );

    // Cmd+, opens Settings over the panel; the confirmation goes down with it.
    frame(
        &ctx,
        &mut app,
        focused_raw(vec![egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: Some(egui::Key::Comma),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        }]),
    );
    assert!(
        app.show_options && !app.show_contacts,
        "Cmd+, swapped the panel"
    );
    assert_eq!(
        app.confirm_remove_contact, None,
        "the pending confirmation was dropped with the panel"
    );

    // Reopen Contacts from the footer: a clean list, no stale dialog.
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    let contacts = text_rect(&out.shapes, "Contacts").expect("the footer paints 'Contacts'");
    click_at(&mut app, &ctx, contacts.center(), SIZE);
    assert!(
        app.show_contacts && !app.show_options,
        "the footer reopened the picker"
    );
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    assert!(
        !text_present(&out.shapes, "Remove Alice?"),
        "no stale confirmation dialog on reopen"
    );
}

/// At the minimum window size the overlay paints, keeps its controls inside the window,
/// and leaves the footer bar reachable.
#[test]
fn the_contacts_overlay_fits_the_min_window() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: (0..6)
            .map(|i| Contact {
                name: format!("Contact {i}"),
                ticket: format!("ticket-{i}"),
            })
            .collect(),
        ..Config::default()
    });
    app.tab = Tab::Watch;
    app.show_contacts = true;
    let min = egui::vec2(560.0, 420.0);
    let out = drive_frames(&mut app, &egui::Context::default(), min, 4);

    let window = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), min);
    for shape in &out.shapes {
        assert!(
            window.expand(1.0).contains_rect(shape.clip_rect),
            "painted rect {:?} escapes the min window",
            shape.clip_rect
        );
    }
    // Six rows into a 248-tall scroll area: they scroll, and the add button below
    // them stays on screen.
    assert!(
        text_present(&out.shapes, "+  Add contact"),
        "the add button stays painted inside the window"
    );
}

/// Each contact row's card paints a faint top light-catch bevel (white alpha 24), so
/// the card reads as lit from above — the same depth cue the copy box and error
/// callout use. Two rows paint two bevels.
#[test]
fn contact_rows_paint_a_top_light_catch_bevel() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![
            Contact {
                name: "Alice".into(),
                ticket: "alice-ticket".into(),
            },
            Contact {
                name: "Bob".into(),
                ticket: "bob-ticket".into(),
            },
        ],
        ..Config::default()
    });
    app.show_contacts = true;
    let out = drive_frames(&mut app, &egui::Context::default(), SIZE, 3);
    // A horizontal line in the bevel's exact colour. The status bar hairline is
    // alpha 16, so it cannot be mistaken for a bevel.
    let bevels = out
        .shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::LineSegment { points, stroke } => {
                (points[0].y - points[1].y).abs() < 0.5
                    && stroke.color == egui::Color32::from_white_alpha(24)
            }
            _ => false,
        })
        .count();
    assert!(
        bevels >= 2,
        "each contact row should paint its top light-catch bevel, got {bevels}"
    );
}

/// Each contact's avatar paints a 1px ring in a brighter shade of its hue, so each
/// contact reads as a distinct, polished face. A ring is a circle with a transparent
/// fill and a non-trivial stroke (the filled avatar disc has no stroke).
#[test]
fn contact_avatars_paint_a_hue_ring() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![
            Contact {
                name: "Alice".into(),
                ticket: "alice-ticket".into(),
            },
            Contact {
                name: "Bob".into(),
                ticket: "bob-ticket".into(),
            },
        ],
        ..Config::default()
    });
    app.show_contacts = true;
    let out = drive_frames(&mut app, &egui::Context::default(), SIZE, 3);
    let rings = out
        .shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Circle(c) => {
                c.fill == egui::Color32::TRANSPARENT && c.stroke.width > 0.0
            }
            _ => false,
        })
        .count();
    assert!(
        rings >= 2,
        "each contact avatar should paint a hue ring, got {rings}"
    );
}

/// The "Load" button is flat — no halo layers behind it — and its fill brightens a
/// touch on hover (and dims on press) so the pointer state is still legible. The
/// probe is centred on the "Load" text, isolating the row's pill from the footer's.
#[test]
fn the_load_button_is_flat_and_brightens_on_hover() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: "alice-ticket".into(),
        }],
        ..Config::default()
    });
    app.show_contacts = true;
    let ctx = egui::Context::default();
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let load = text_rect(&out.shapes, "Load").expect("the row paints 'Load'");

    // Resting: no halo — none of the 3-layer glow rects sit around the button.
    let halo_alphas = [0.02, 0.05, 0.09];
    let halos = out
        .shapes
        .iter()
        .filter(|s| match &s.shape {
            egui::epaint::Shape::Rect(r) => halo_alphas
                .iter()
                .any(|&a| r.fill == ACCENT.gamma_multiply(a))
                && (r.rect.center() - load.center()).length() < 80.0,
            _ => false,
        })
        .count();
    assert_eq!(halos, 0, "the Load button should paint no halo");

    // The resting pill itself is painted with the dim accent fill.
    let fill_of = |shapes: &[egui::epaint::ClippedShape], alpha: f32| {
        shapes.iter().any(|s| {
            match &s.shape {
                egui::epaint::Shape::Rect(r) => {
                    r.fill == ACCENT.gamma_multiply(alpha)
                        && (r.rect.center() - load.center()).length() < 20.0
                }
                _ => false,
            }
        })
    };
    assert!(
        fill_of(&out.shapes, 0.24),
        "the resting Load pill paints the 0.24 accent fill"
    );

    // Hover: move the pointer onto the pill — its fill brightens a touch.
    frame(
        &ctx,
        &mut app,
        focused_raw(vec![egui::Event::PointerMoved(load.center())]),
    );
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    assert!(
        fill_of(&out.shapes, 0.34),
        "hovering the Load pill brightens its fill to 0.34"
    );
}

/// A Ctrl+T key press event (the global "open Contacts" shortcut).
fn ctrl_t() -> egui::Event {
    egui::Event::Key {
        key: egui::Key::T,
        physical_key: Some(egui::Key::T),
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::CTRL,
    }
}

/// Ctrl+T opens the Contacts overlay, and (sharing the panels with Settings) takes
/// Settings down if it was up.
#[test]
fn ctrl_t_opens_the_contacts_overlay() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: "alice-ticket".into(),
        }],
        ..Config::default()
    });
    let ctx = egui::Context::default();
    drive_frames(&mut app, &ctx, SIZE, 2);
    assert!(!app.show_contacts, "the overlay starts closed");

    frame(&ctx, &mut app, focused_raw(vec![ctrl_t()]));
    assert!(app.show_contacts, "Ctrl+T opens the overlay");
    assert!(!app.show_options, "opening Contacts takes Settings down");
}

/// Loading a contact while the Broadcast tab is showing jumps back to the Watch tab —
/// loading is a watch-side gesture, so the filled box is revealed where it is usable.
#[test]
fn loading_a_contact_from_the_broadcast_tab_jumps_to_watch() {
    let (mut app, _rt) = test_app_with(Config {
        contacts: vec![Contact {
            name: "Alice".into(),
            ticket: "alice-ticket".into(),
        }],
        ..Config::default()
    });
    app.tab = Tab::Broadcast;
    app.show_contacts = true;
    let ctx = egui::Context::default();
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    assert_eq!(app.tab, Tab::Broadcast, "starts on the Broadcast tab");

    let load = text_rect(&out.shapes, "Load").expect("the row's 'Load' button is painted");
    click_at(&mut app, &ctx, load.center(), SIZE);
    assert_eq!(app.watch_ticket, "alice-ticket", "'Load' fills the ticket box");
    assert_eq!(app.tab, Tab::Watch, "'Load' jumps back to the Watch tab");
}
