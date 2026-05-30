use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Channels exposed by the GoXLR daemon. The cycle hotkey iterates the
/// `visible_channels` subset in the order the user kept in their config.
pub const ALL_CHANNELS: &[&str] = &[
    "Mic",
    "LineIn",
    "Console",
    "System",
    "Game",
    "Chat",
    "Sample",
    "Music",
    "Headphones",
    "MicMonitor",
    "LineOut",
];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// Channel selected at startup. Updated whenever the user picks a
    /// new channel (tray click or cycle hotkey) so the app reopens
    /// where it was left.
    pub default_channel: String,
    /// Subset of [`ALL_CHANNELS`] the cycle hotkey iterates over. Hidden
    /// channels stay selectable from the tray menu — visibility only
    /// scopes the cycle.
    pub visible_channels: Vec<String>,
    /// Global hotkey that cycles through `visible_channels`. Format:
    /// `"ctrl+shift+alt+v"` (case-insensitive, parts split by `+`).
    /// `None` disables the hotkey. Edited via `config.json`; restart to
    /// apply.
    pub cycle_hotkey: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_channel: "Game".into(),
            visible_channels: ALL_CHANNELS.iter().map(|s| (*s).to_string()).collect(),
            cycle_hotkey: Some("ctrl+shift+alt+v".into()),
        }
    }
}

/// Position of `name` inside [`ALL_CHANNELS`]. Falls back to `0` for
/// unknown names so a malformed config can't panic the app on startup.
pub fn channel_index(name: &str) -> usize {
    ALL_CHANNELS.iter().position(|c| *c == name).unwrap_or(0)
}

/// `ALL_CHANNELS[idx]`, bounds-clamped to `ALL_CHANNELS[0]` if the index
/// has drifted out of range.
pub fn channel_name(idx: usize) -> &'static str {
    ALL_CHANNELS.get(idx).copied().unwrap_or(ALL_CHANNELS[0])
}

/// `%LOCALAPPDATA%\GoXLR Volume Wheel\config.json`.
pub fn path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("GoXLR Volume Wheel").join("config.json"))
}

/// Loads the config from disk, restoring defaults if the file is
/// missing or malformed. Writes the default file back so the user has
/// something to edit by hand.
pub fn load() -> Config {
    let Some(path) = path() else {
        return Config::default();
    };
    if let Ok(data) = fs::read_to_string(&path) {
        match serde_json::from_str::<Config>(&data) {
            Ok(cfg) => return cfg,
            Err(err) => crate::log::error(&format!(
                "config parse failed ({}), restoring defaults",
                err
            )),
        }
    }
    let cfg = Config::default();
    save(&cfg);
    cfg
}

/// Atomic save: write to `config.json.tmp` then rename. A crash
/// mid-write can never leave a half-truncated config behind.
pub fn save(cfg: &Config) {
    let Some(path) = path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(data) = serde_json::to_string_pretty(cfg) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, data).is_err() {
        return;
    }
    let _ = fs::rename(&tmp, &path);
}
