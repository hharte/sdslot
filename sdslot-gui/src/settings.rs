// SPDX-License-Identifier: MIT OR Apache-2.0
//! Persisted GUI settings: a small TOML file named `.sdslot` in the user's
//! home directory. Every change is written back immediately; a missing or
//! unparsable file silently yields the defaults.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Also list devices without media (empty card readers).
    pub show_all: bool,
    /// Allow selecting non-removable disks; enabling it is warning-gated.
    pub advanced: bool,
    /// Select the first removable disk with media at startup; enabling it
    /// is warning-gated (the first removable disk may not be the intended
    /// card).
    pub select_first_removable: bool,
    /// Re-read and compare after every write. On by default; a missing
    /// field in an older settings file also reads as on.
    pub verify: bool,
    /// Hide slots with no content, name, or image from the slot map.
    pub hide_empty_slots: bool,
    /// Hide the log pane at the bottom of the window.
    pub hide_log: bool,
    /// Show the equivalent sdslot command line for every operation.
    pub developer_mode: bool,
    /// Height of the log pane's scroll region in points; drag-adjustable,
    /// defaults to about six VT323 lines.
    pub log_height: f32,
    /// Window inner size in points, captured on resize and restored at
    /// startup.
    pub window_width: f32,
    pub window_height: f32,
}

/// The safe defaults, also used by the "Reset all settings" button.
impl Default for Settings {
    fn default() -> Settings {
        Settings {
            show_all: false,
            advanced: false,
            select_first_removable: false,
            verify: true,
            hide_empty_slots: false,
            hide_log: false,
            developer_mode: false,
            log_height: 104.0,
            window_width: 880.0,
            window_height: 680.0,
        }
    }
}

/// `~/.sdslot` (USERPROFILE on Windows, HOME elsewhere).
pub fn settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".sdslot"))
}

impl Settings {
    pub fn load() -> Settings {
        let Some(path) = settings_path() else {
            return Settings::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = settings_path().ok_or("cannot determine the home directory")?;
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let s = Settings {
            show_all: true,
            advanced: false,
            select_first_removable: true,
            verify: false,
            hide_empty_slots: true,
            hide_log: false,
            developer_mode: false,
            log_height: 104.0,
            window_width: 880.0,
            window_height: 680.0,
        };
        let text = toml::to_string(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert!(back.show_all);
        assert!(!back.advanced);
        assert!(back.select_first_removable);
        assert!(!back.verify);
    }

    #[test]
    fn defaults_are_safe_and_verify_is_on() {
        let d = Settings::default();
        assert!(!d.show_all);
        assert!(!d.advanced);
        assert!(!d.select_first_removable);
        assert!(d.verify);
        assert!(!d.hide_empty_slots);
        assert!(!d.hide_log);
        assert!(!d.developer_mode);
        assert!((d.log_height - 104.0).abs() < f32::EPSILON);
        assert!((d.window_width - 880.0).abs() < f32::EPSILON);
        assert!((d.window_height - 680.0).abs() < f32::EPSILON);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // An older settings file without the verify key must read as
        // verify = on, and unknown keys must not break parsing.
        let back: Settings =
            toml::from_str("show_all = true\nfuture_option = 3\n").unwrap_or_default();
        assert!(back.verify);
        let empty: Settings = toml::from_str("").unwrap();
        assert!(!empty.advanced);
        assert!(empty.verify);
    }
}
