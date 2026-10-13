//! Persisted GUI configuration.
//!
//! Stored as JSON in the platform config directory under `streampipe/`. A missing or
//! corrupt file yields the defaults, so the GUI always starts.
//!
//! The iroh secret is persisted (as hex) so the broadcast endpoint keeps the same
//! identity across restarts, which keeps the ticket stable for friends who saved it.
//! This is a deliberate trade-off: the secret sits in the user's config directory in
//! plaintext. `IROH_SECRET` overrides it and is never written to disk.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::{stream::Player, EndpointTicket};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

/// The application name used for the config directory.
const APP_DIR: &str = "streampipe";
/// The config file name inside [`APP_DIR`].
const CONFIG_FILE: &str = "config.json";

/// The initial player volume when nothing was saved yet: half. Only the first run
/// (or a config predating this setting) uses it; every value the user sets is
/// persisted, so the slider reopens where it was left.
pub const DEFAULT_VOLUME: u8 = 50;

/// The players the GUI offers, in dropdown order.
pub const PLAYERS: [Player; 4] = [Player::Mpv, Player::Vlc, Player::Ffplay, Player::None];

/// A stable short name for a player, used as a config key.
pub fn player_key(player: Player) -> &'static str {
    match player {
        Player::Mpv => "mpv",
        Player::Vlc => "vlc",
        Player::Ffplay => "ffplay",
        Player::None => "none",
    }
}

/// Parse a player from a config string, defaulting to mpv.
pub fn player_from_key(key: &str) -> Player {
    match key {
        "vlc" => Player::Vlc,
        "ffplay" => Player::Ffplay,
        "none" => Player::None,
        _ => Player::Mpv,
    }
}

/// Whether a ticket string parses as a well-formed [`EndpointTicket`].
///
/// A config written by an older build (or polluted by a test fixture that fakes a
/// long ticket for layout) can carry a garbage string — e.g. a run of `b`s — that
/// the current wire format rejects. Such a ticket can never be dialled, so callers
/// detect it with this and replace it with a freshly minted one.
pub fn ticket_is_valid(ticket: &str) -> bool {
    ticket.trim().parse::<EndpointTicket>().is_ok()
}

/// A saved contact: a friendly name paired with the ticket to watch.
///
/// Picked from the Options overlay ("Use") to fill the Watch tab's ticket box, so a
/// viewer who regularly watches the same friends never pastes a ticket again. The
/// Watch tab shows the matching contact's name as a chip, derived by comparing
/// [`Contact::ticket`] against the box.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Contact {
    /// The display name, shown on the Watch tab's chip.
    pub name: String,
    /// The ticket to fill into the Watch tab.
    pub ticket: String,
}

/// The persisted configuration of the GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The iroh secret key as lowercase hex, so the endpoint id is stable.
    /// `None` until the first broadcast/watch generates and persists one.
    pub secret_hex: Option<String>,
    /// The ticket most recently watched, prefilled on the Watch tab.
    pub last_ticket: String,
    /// Recently used tickets, newest first, for a quick history dropdown.
    pub ticket_history: Vec<String>,
    /// Saved contacts (name + ticket), pickable from the Options overlay to fill the
    /// Watch tab. Struct-level `#[serde(default)]` means a config predating this loads
    /// as an empty list.
    pub contacts: Vec<Contact>,
    /// The default media player, selected on the Watch tab at open.
    pub default_player: String,
    /// Optional explicit path per player, so users can prefill their installs.
    pub player_paths: PlayerPaths,
    /// The jitter buffer in milliseconds, `None` for lowest latency.
    pub buffer_ms: Option<u64>,
    /// The player's initial volume in percent (0-100), used on every launch.
    /// 100 means "leave the player at its own default" (see [`Player::command`]).
    pub volume: u8,
    /// The local address the viewer plays out on.
    pub play_addr: String,
    /// The WHIP host to bind, prefilled with loopback.
    pub broadcast_host: String,
    /// The WHIP port to bind, prefilled with 8080.
    pub broadcast_port: u16,
    /// The WHIP bearer token OBS must present, if any.
    pub bearer_token: Option<String>,
    /// The verbosity level (0-3) used for the log panel at startup.
    pub verbosity: u8,
    /// The most recently issued broadcast ticket, remembered across restarts.
    ///
    /// Stored for reference only — *not* re-advertised as the live ticket on the next
    /// broadcast, which is always freshly minted from the live endpoint address (see
    /// [`crate::gui::state::spawn_broadcast`]) so viewers get current transport hints
    /// and can hole-punch direct. Identity stays stable via [`Config::secret_hex`], so
    /// an older saved ticket still resolves to this endpoint.
    pub broadcast_ticket: Option<String>,
    /// Optional fixed IPv4 bind address (`ip:port`) for the broadcast endpoint.
    /// Empty by default (ephemeral port, ticket changes each session). A streamer
    /// with a static public IP can set this to keep the ticket stable.
    pub broadcast_bind_ipv4: Option<String>,
    /// Optional fixed IPv6 bind address (`[ip]:port`) for the broadcast endpoint.
    pub broadcast_bind_ipv6: Option<String>,
}

/// Optional per-player binary paths.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerPaths {
    /// Path to the mpv binary.
    pub mpv: Option<String>,
    /// Path to the vlc binary.
    pub vlc: Option<String>,
    /// Path to the ffplay binary.
    pub ffplay: Option<String>,
}

impl PlayerPaths {
    /// The configured path for a player, if any.
    pub fn get(&self, player: Player) -> Option<&str> {
        match player {
            Player::Mpv => self.mpv.as_deref(),
            Player::Vlc => self.vlc.as_deref(),
            Player::Ffplay => self.ffplay.as_deref(),
            Player::None => None,
        }
    }

    /// Set the path for a player.
    pub fn set(&mut self, player: Player, path: Option<String>) {
        match player {
            Player::Mpv => self.mpv = path,
            Player::Vlc => self.vlc = path,
            Player::Ffplay => self.ffplay = path,
            Player::None => {}
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            secret_hex: None,
            last_ticket: String::new(),
            ticket_history: Vec::new(),
            contacts: Vec::new(),
            default_player: player_key(Player::Mpv).to_string(),
            player_paths: PlayerPaths::default(),
            buffer_ms: None,
            volume: DEFAULT_VOLUME,
            play_addr: crate::stream::DEFAULT_PLAY_ADDR.to_string(),
            broadcast_host: "127.0.0.1".to_string(),
            broadcast_port: 8080,
            bearer_token: None,
            verbosity: 1,
            broadcast_ticket: None,
            broadcast_bind_ipv4: None,
            broadcast_bind_ipv6: None,
        }
    }
}

impl Config {
    /// The configured default player.
    pub fn default_player(&self) -> Player {
        player_from_key(&self.default_player)
    }

    /// The bearer token, `None` if blank.
    pub fn bearer_token(&self) -> Option<String> {
        self.bearer_token.clone().filter(|t| !t.trim().is_empty())
    }

    /// Load the config from disk, falling back to defaults. A missing file is normal
    /// (first run); a corrupt file is reported on stderr and treated as defaults, so
    /// the GUI still opens.
    pub fn load() -> Self {
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Config>(&text).unwrap_or_else(|e| {
                eprintln!("warning: ignoring corrupt config {}: {e}", path.display());
                Config::default()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                eprintln!("warning: cannot read config {}: {e}", path.display());
                Config::default()
            }
        }
    }

    /// Write the config to disk, creating the directory if needed. Errors are
    /// reported on stderr but never fatal: the GUI keeps running with the in-memory
    /// config.
    pub fn save(&self) {
        let path = config_path();
        if let Some(dir) = path.parent() {
            if let Err(e) = fs::create_dir_all(dir) {
                eprintln!("warning: cannot create config dir {}: {e}", dir.display());
                return;
            }
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(json) => json,
            Err(e) => {
                eprintln!("warning: cannot serialize config: {e}");
                return;
            }
        };
        // Write to a temp file then rename, so a crash cannot truncate the file.
        let tmp = tmp_path(&path);
        if let Err(e) = fs::write(&tmp, json.as_bytes()) {
            eprintln!("warning: cannot write config {}: {e}", tmp.display());
            return;
        }
        if fs::rename(&tmp, &path).is_err() {
            // Fall back to a direct write if rename fails across filesystems.
            if let Err(e) = fs::write(&path, json.as_bytes()) {
                eprintln!("warning: cannot save config {}: {e}", path.display());
            }
        }
    }

    /// Load the secret, generating and persisting one if there is none. `IROH_SECRET`
    /// wins and is not persisted; otherwise the stored secret is reused (stable
    /// endpoint id), and a fresh one is generated and saved if there is none.
    pub fn ensure_secret(&mut self) -> SecretKey {
        if let Ok(secret) = std::env::var("IROH_SECRET") {
            if let Ok(key) = secret.parse::<SecretKey>() {
                return key;
            }
            eprintln!("warning: IROH_SECRET is invalid, using the stored secret");
        }
        if let Some(hex) = &self.secret_hex {
            if let Ok(key) = hex.parse::<SecretKey>() {
                return key;
            }
            eprintln!("warning: stored secret is invalid, generating a new one");
        }
        let key = SecretKey::generate();
        self.secret_hex = Some(data_encoding::HEXLOWER.encode(&key.to_bytes()));
        self.save();
        key
    }

    /// Record a watched ticket as the last used and in history (deduped).
    pub fn note_ticket(&mut self, ticket: &str) {
        if ticket.trim().is_empty() {
            return;
        }
        self.last_ticket = ticket.to_string();
        self.ticket_history.retain(|t| t != ticket);
        self.ticket_history.insert(0, ticket.to_string());
        self.ticket_history.truncate(20);
    }
}

/// The directory the config lives in, per platform conventions. Uses standard
/// environment variables directly (no `dirs` dependency): `$XDG_CONFIG_HOME` or
/// `~/.config` on Linux, `%APPDATA%` on Windows, `~/Library/Application Support` on
/// macOS.
pub fn config_dir() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|h| PathBuf::from(h).join("AppData").join("Roaming"))
        })
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    let base = base.unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR)
}

/// The full path of the config file.
pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE)
}

/// Serializes the tests that redirect `$XDG_CONFIG_HOME` (a process-wide variable, so
/// two running at once would read each other's config dir).
pub static CONFIG_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A sibling temp path used for atomic writes.
fn tmp_path(path: &Path) -> PathBuf {
    let name = format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config")
    );
    path.with_file_name(name)
}
