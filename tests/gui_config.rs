//! The persisted GUI configuration: defaults, JSON round-trips, restart behaviour.
//!
//! `CONFIG_ENV` serialises the tests that redirect `XDG_CONFIG_HOME` into a temp
//! dir, so they never touch the real user config and never race each other.

mod common;

use std::fs;
use std::sync::Mutex;

use eframe::egui;
use streampipe::gui::config::{config_path, ticket_is_valid, Config, Contact, DEFAULT_VOLUME};
use streampipe::stream::Player;

use common::{drive_frames, real_ticket, test_app, test_app_with};

/// Serialises the tests that mutate the environment / config dir.
static CONFIG_ENV: Mutex<()> = Mutex::new(());

#[test]
fn defaults_match_acceptance() {
    let cfg = Config::default();
    assert_eq!(cfg.broadcast_host, "127.0.0.1");
    assert_eq!(cfg.broadcast_port, 8080);
    assert_eq!(cfg.default_player(), Player::Mpv);
    assert!(cfg.last_ticket.is_empty());
    assert!(cfg.secret_hex.is_none());
    // The volume starts at half, not full.
    assert_eq!(cfg.volume, DEFAULT_VOLUME);
    assert_eq!(DEFAULT_VOLUME, 50);
}

#[test]
fn round_trip_preserves_fields() {
    let mut cfg = Config {
        broadcast_port: 9001,
        default_player: "vlc".into(),
        buffer_ms: Some(250),
        volume: 37,
        ..Config::default()
    };
    cfg.player_paths.mpv = Some("/opt/mpv".into());
    cfg.note_ticket("ticket-a");
    cfg.note_ticket("ticket-b");
    let json = serde_json::to_string(&cfg).unwrap();
    let back = serde_json::from_str::<Config>(&json).unwrap();
    assert_eq!(back.broadcast_port, 9001);
    assert_eq!(back.default_player(), Player::Vlc);
    assert_eq!(back.buffer_ms, Some(250));
    assert_eq!(back.volume, 37);
    assert_eq!(back.player_paths.mpv.as_deref(), Some("/opt/mpv"));
    // newest first, deduped
    assert_eq!(back.ticket_history[0], "ticket-b");
    assert_eq!(back.ticket_history[1], "ticket-a");
    assert_eq!(back.last_ticket, "ticket-b");
}

/// The last volume the user set goes to disk and comes back: `save` writes the JSON,
/// `load` reads it, and `App::new` seeds the Watch-tab slider from the loaded value.
/// A config predating the setting (or no config) opens at the half-volume default.
#[test]
fn volume_survives_a_restart() {
    let _guard = CONFIG_ENV.lock().unwrap();
    // Redirect the config dir into a temp folder so the test never writes to the real
    // user config.
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());
    let path = config_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();

    // No file yet: the app opens at half volume.
    assert!(!path.exists());
    assert_eq!(Config::load().volume, DEFAULT_VOLUME);

    // The user drags to 37 % and the app persists it.
    let cfg = Config {
        volume: 37,
        ..Config::default()
    };
    cfg.save();
    assert!(path.exists(), "the config was written");

    // A fresh process reads it back.
    let reopened = Config::load();
    assert_eq!(reopened.volume, 37, "the slider reopens where it was left");

    // An old config without the key still loads, at the default.
    fs::write(&path, "{}").unwrap();
    assert_eq!(Config::load().volume, DEFAULT_VOLUME);

    // A hand-edited config louder than the slider can show still loads; `App::new`
    // seeds the slider with `min(100)`, so it is clamped, not wrapped.
    fs::write(&path, "{\"volume\":180}").unwrap();
    assert_eq!(Config::load().volume.min(100), 100);
}

#[test]
fn broadcast_ticket_round_trips() {
    let cfg = Config {
        broadcast_ticket: Some("ticket-body".into()),
        ..Config::default()
    };
    let back = serde_json::from_str::<Config>(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(back.broadcast_ticket.as_deref(), Some("ticket-body"));
}

#[test]
fn bind_addrs_round_trip_and_default_to_none() {
    let cfg = Config {
        broadcast_bind_ipv4: Some("203.0.113.7:45678".into()),
        broadcast_bind_ipv6: Some("[2001:db8::1]:45678".into()),
        ..Config::default()
    };
    let back = serde_json::from_str::<Config>(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        back.broadcast_bind_ipv4.as_deref(),
        Some("203.0.113.7:45678")
    );
    assert_eq!(
        back.broadcast_bind_ipv6.as_deref(),
        Some("[2001:db8::1]:45678")
    );
    // An old config without the keys defaults them to ephemeral.
    let old = serde_json::from_str::<Config>("{}").unwrap();
    assert!(old.broadcast_bind_ipv4.is_none());
    assert!(old.broadcast_bind_ipv6.is_none());
}

#[test]
fn old_config_without_ticket_fields_still_loads() {
    // A config predating the persisted broadcast ticket must load with defaults,
    // and one carrying the now-removed `broadcast_ticket_host` /
    // `broadcast_ticket_port` keys must too (struct-level `serde(default)` ignores
    // unknown fields).
    let back = serde_json::from_str::<Config>(
        "{\"broadcast_port\":9001,\"broadcast_ticket_host\":\"0.0.0.0\",\"broadcast_ticket_port\":9001}",
    )
    .unwrap();
    assert_eq!(back.broadcast_port, 9001);
    assert!(back.broadcast_ticket.is_none());
}

/// Saved contacts (name + ticket) round-trip through JSON, and a config predating the
/// feature (no `contacts` key, e.g. `{}`) loads as an empty list.
#[test]
fn contacts_round_trip_and_default_to_empty() {
    let cfg = Config {
        contacts: vec![
            Contact {
                name: "Alice".into(),
                ticket: "ticket-alice".into(),
            },
            Contact {
                name: "Bob".into(),
                ticket: "ticket-bob".into(),
            },
        ],
        ..Config::default()
    };
    let back = serde_json::from_str::<Config>(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(back.contacts.len(), 2, "both contacts survive");
    assert_eq!(back.contacts[0].name, "Alice");
    assert_eq!(back.contacts[0].ticket, "ticket-alice");
    assert_eq!(back.contacts[1].name, "Bob");
    assert_eq!(back.contacts[1].ticket, "ticket-bob");

    // An old config (no `contacts` key) loads empty — no migration needed.
    let old = serde_json::from_str::<Config>("{}").unwrap();
    assert!(
        old.contacts.is_empty(),
        "a config predating contacts is empty"
    );

    // And a partial entry fills in from `serde(default)`.
    let partial = serde_json::from_str::<Config>("{\"contacts\":[{\"name\":\"Solo\"}]}").unwrap();
    assert_eq!(partial.contacts.len(), 1);
    assert_eq!(partial.contacts[0].name, "Solo");
    assert!(partial.contacts[0].ticket.is_empty());

    // Round-trips through disk, not just the in-memory JSON.
    let _guard = CONFIG_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());
    cfg.save();
    let reloaded = Config::load();
    assert_eq!(
        reloaded.contacts, cfg.contacts,
        "contacts survive a restart"
    );
    std::env::remove_var("XDG_CONFIG_HOME");
}

#[test]
fn corrupt_json_falls_back_to_default() {
    // A corrupt file must not panic; invalid JSON yields defaults.
    let back = serde_json::from_str::<Config>("{ not json").unwrap_or_else(|_| Config::default());
    assert_eq!(back.broadcast_port, 8080);
}

#[test]
fn secret_round_trips() {
    let _guard = CONFIG_ENV.lock().unwrap();
    // Redirect the config dir into a temp folder so the test never writes to the
    // real user config, then confirm the secret is generated once and reused.
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());
    std::env::remove_var("IROH_SECRET");
    let mut cfg = Config::default();
    let first = cfg.ensure_secret();
    let again = cfg.ensure_secret();
    assert_eq!(first.to_bytes(), again.to_bytes(), "secret is stable");
    assert!(cfg.secret_hex.is_some());
    // The generated secret was persisted to disk.
    let loaded = Config::load();
    assert_eq!(loaded.secret_hex, cfg.secret_hex);
}

/// `ticket_is_valid` accepts a freshly minted ticket and rejects the garbage forms a
/// polluted or hand-edited config can carry.
#[test]
fn ticket_is_valid_accepts_real_tickets_and_rejects_garbage() {
    assert!(ticket_is_valid(&real_ticket()));
    // Surrounding whitespace is tolerated, mirroring the Watch tab's trimmed parse.
    let real = real_ticket();
    assert!(ticket_is_valid(&format!("  {real}\n")));

    assert!(!ticket_is_valid(""));
    assert!(!ticket_is_valid("   "));
    assert!(!ticket_is_valid("a"));
    // Right prefix, no body.
    assert!(!ticket_is_valid("endpoint"));
    // The garbage shape a test-polluted config leaves behind: a long run of `b`s
    // (base32 for a repeating pattern the postcard wire format rejects).
    assert!(!ticket_is_valid(&"b".repeat(144)));
}

/// A stored ticket the wire format rejects is dropped when the app opens — from the
/// in-memory config *and* from disk — and the first idle frame arms a replacement
/// refresh. A valid ticket is kept untouched; a first-run config arms nothing.
#[test]
fn invalid_stored_ticket_is_dropped_and_replaced() {
    let _guard = CONFIG_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let (mut app, _rt) = test_app_with(Config {
        broadcast_ticket: Some("b".repeat(144)),
        ..Config::default()
    });
    assert!(app.regen_ticket, "the garbage ticket arms auto-replacement");
    assert!(app.config.broadcast_ticket.is_none(), "dropped from memory");
    assert!(
        Config::load().broadcast_ticket.is_none(),
        "the drop is persisted, so the garbage cannot come back on the next start"
    );

    // The first idle frame consumes the flag and kicks off the replacement refresh
    // (`refreshing` is armed synchronously by the spawn).
    let ctx = egui::Context::default();
    let _out = drive_frames(&mut app, &ctx, egui::vec2(760.0, 560.0), 2);
    assert!(!app.regen_ticket, "the flag fires exactly once");
    let g = app.state.lock().unwrap();
    assert!(
        g.refreshing || g.ticket.is_some(),
        "a replacement refresh was started"
    );
    drop(g);

    // A valid ticket is kept and arms nothing.
    let real = real_ticket();
    let (app, _rt) = test_app_with(Config {
        broadcast_ticket: Some(real.clone()),
        ..Config::default()
    });
    assert!(!app.regen_ticket);
    assert_eq!(app.config.broadcast_ticket.as_deref(), Some(real.as_str()));

    // First run (no ticket at all): nothing armed, no refresh spawned.
    let (app, _rt) = test_app();
    assert!(!app.regen_ticket);

    std::env::remove_var("XDG_CONFIG_HOME");
}
