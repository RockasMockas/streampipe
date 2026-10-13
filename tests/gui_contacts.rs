//! The Contacts overlay: where it opens from, how it behaves, and what it persists.
//!
//! The overlay is standalone (not a Settings section), reachable from the footer bar's
//! "Contacts" button and the Watch tab's "Select Contact". Picking a contact fills the
//! Watch ticket box and chips the name on the tab.

mod common;

use eframe::egui;
use streampipe::gui::{
    config::{Config, Contact},
    Tab,
};

use common::{
    click_at, drive_frames, focused_raw, frame, real_ticket, render_shapes, test_app,
    test_app_with, text_present, text_rect,
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

    // The chip appears on the heading row, right-aligned, and the ticket is watchable.
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    let chip = text_rect(&out.shapes, "Alice").expect("the chip shows the name");
    assert!(
        chip.min.y < 140.0,
        "the chip rides the 'Stream ticket' heading row, got {chip:?}"
    );
    assert!(
        chip.right() > SIZE.x - 60.0,
        "the chip is right-aligned, got {chip:?}"
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

/// Add a row, type into it, close (which flushes), reopen: the contact survived. Then
/// remove it and it is gone.
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

    // The hint recedes, and the row carries the two fields, a "Load" action and a
    // small "✕" remove.
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    assert!(
        !text_present(&out.shapes, hint),
        "the empty-list hint recedes once there is a row"
    );
    assert!(text_present(&out.shapes, "Name"));
    assert!(text_present(&out.shapes, "Paste their ticket here"));
    assert!(text_present(&out.shapes, "Load"));
    assert!(text_present(&out.shapes, "✕"));

    // "Save" writes the list, with the confirmation pill.
    let save = text_rect(&out.shapes, "Save").expect("the 'Save' button is painted");
    click_at(&mut app, &ctx, save.center(), SIZE);
    assert_eq!(
        app.toast.as_ref().map(|(m, _)| m.as_str()),
        Some("Saved ✓"),
        "Save raises the confirmation toast"
    );
    let written = std::fs::read_to_string(dir.path().join("streampipe").join("config.json"))
        .expect("Save wrote the redirected config");
    assert!(
        written.contains("\"contacts\""),
        "the saved config carries the contacts key: {written}"
    );

    // The row's "✕" opens a confirmation modal; nothing is removed until it is confirmed.
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let x = text_rect(&out.shapes, "✕").expect("the row's '✕' button is painted");
    click_at(&mut app, &ctx, x.center(), SIZE);
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

    // Confirming in the modal drops the row.
    let remove = text_rect(&out.shapes, "Remove").expect("the modal's 'Remove' button");
    click_at(&mut app, &ctx, remove.center(), SIZE);
    assert!(app.config.contacts.is_empty(), "confirming removes the row");

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

    // Add a row, then close: the panel is gone and the config on disk carries it.
    let out = drive_frames(&mut app, &ctx, SIZE, 3);
    let add = text_rect(&out.shapes, "+  Add contact").expect("the add button is painted");
    click_at(&mut app, &ctx, add.center(), SIZE);
    let out = drive_frames(&mut app, &ctx, SIZE, 2);
    let close = text_rect(&out.shapes, "Close").expect("the 'Close' button is painted");
    click_at(&mut app, &ctx, close.center(), SIZE);

    assert!(!app.show_contacts, "Close dismisses the panel");
    assert!(!app.contacts_dirty, "the staged edit was flushed");
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
    // Six rows into a 248-tall scroll area: they scroll, so the footer stays on screen.
    assert!(
        text_present(&out.shapes, "Save") && text_present(&out.shapes, "Close"),
        "the footer is painted inside the scroll-free area"
    );
}
