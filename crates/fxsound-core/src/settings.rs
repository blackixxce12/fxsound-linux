//! Persisted application settings.
//!
//! The Windows build keeps these in a JUCE `PropertiesFile` XML at
//! `%APPDATA%\FxSound\FxSound.settings`. This port keeps the **same key names** in a TOML file at
//! `$XDG_CONFIG_HOME/fxsound/settings.toml` (default `~/.config/fxsound/settings.toml`) so the
//! schema stays greppable against the original C++ and a user's settings are recognisable.
//!
//! Three keys change meaning on Linux and say so in their doc comments: the hotkey commands
//! (a Wayland client cannot grab global shortcuts), `automatic_updates` (this fork is installed by
//! a package manager and never phones home) and the window position keys (Wayland gives a client
//! no way to place its own toplevel).

use serde::{Deserialize, Serialize};

/// Which of the two window layouts is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    /// The full window: visualizer, effect sliders and the graphic equalizer.
    #[default]
    Pro,
    /// The compact window: preset and output pickers only.
    Lite,
}

/// Light or dark palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

/// One remembered output device and the preset the user last used with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceConfig {
    /// On Windows an endpoint id; here the PipeWire `node.name`, which is stable across restarts.
    pub device_id: String,
    /// Human-readable name (`node.description`).
    pub device_name: String,
    /// Preset last selected while this device was active.
    pub preset: String,
    /// `speaker`, `headphone`, `hdmi`, … — used to pick a default preset for new devices.
    pub device_form_factor: String,
}

/// Keyboard shortcuts.
///
/// On Windows these are Win32 `RegisterHotKey` codes. A Wayland client cannot register a global
/// shortcut, so here they are human-readable accelerator strings that the app only *displays*;
/// the binding itself lives in the compositor and reaches the running instance through the
/// control socket (see `packaging/hyprland.conf.example`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub cmd_on_off: String,
    pub cmd_open_close: String,
    pub cmd_next_preset: String,
    pub cmd_previous_preset: String,
    pub cmd_change_output: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        // The same chords the Windows build ships, decoded from its Win32 hotkey codes.
        Self {
            cmd_on_off: "Ctrl+Shift+Q".into(),
            cmd_open_close: "Ctrl+Shift+E".into(),
            cmd_next_preset: "Ctrl+Shift+A".into(),
            cmd_previous_preset: "Ctrl+Shift+Z".into(),
            cmd_change_output: "Ctrl+Shift+W".into(),
        }
    }
}

/// The whole settings file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // ---- audio -----------------------------------------------------------------------------
    /// Master power state, restored on launch.
    pub power: bool,
    /// Name of the preset to select on launch.
    pub preset: String,
    /// `node.name` of the real output device FxSound renders to.
    pub output_device_name: String,
    /// `node.name` of the real capture device FxSound listens to, when it runs as an input.
    pub input_device_name: String,
    /// Which of the two devices above is the live one. Linux only: the Windows build has no
    /// capture mode.
    pub device_direction: crate::DeviceDirection,
    /// Remembered devices and their presets.
    pub device_configs: Vec<DeviceConfig>,
    /// Schema version of `device_configs`.
    pub device_configs_version: u32,
    /// Switch to a newly appeared output device automatically.
    pub prioritize_new_output: bool,

    // ---- dsp state that lives outside the preset --------------------------------------------
    pub master_gain: f32,
    pub balance: f32,
    pub volume_leveling: f32,
    pub filter_q: f32,
    pub num_bands: u32,

    // ---- window ----------------------------------------------------------------------------
    pub view: ViewMode,
    pub theme_mode: ThemeMode,
    /// Kept for compatibility and for the X11 fallback. Under Wayland a client cannot position
    /// its own toplevel, so the compositor decides and these are only written, never applied.
    pub window_x: i32,
    pub window_y: i32,
    /// Requested by the app through `ViewportCommand::WindowLevel`; Hyprland honours it.
    pub always_on_top: bool,
    /// Start with no window, tray only.
    pub run_minimized: bool,

    // ---- ui --------------------------------------------------------------------------------
    /// UI language code (`"en"`, `"de"`, …) — the one the user picked in Settings. Only
    /// consulted while `language_follows_system` is `false`.
    pub language: String,
    /// Show the UI in the desktop session's language rather than in [`Settings::language`].
    /// Linux only: the Windows build has no such notion, so this is what a fresh install does.
    pub language_follows_system: bool,
    pub hide_help_tooltips: bool,
    pub hide_notifications: bool,
    /// Whether the shortcut rows in Settings are shown as active.
    pub hotkeys: bool,
    #[serde(flatten)]
    pub hotkey_bindings: Hotkeys,
    /// How many presets the user may save.
    pub max_user_presets: u32,

    // ---- housekeeping ----------------------------------------------------------------------
    /// Present so the Settings pane matches the original. This fork never contacts the network;
    /// the toggle is inert and the Help pane says so.
    pub automatic_updates: bool,
    pub last_update_time: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            power: true,
            preset: "General".into(),
            output_device_name: String::new(),
            input_device_name: String::new(),
            device_direction: crate::DeviceDirection::Output,
            device_configs: Vec::new(),
            device_configs_version: 2,
            prioritize_new_output: false,

            master_gain: 0.0,
            balance: 0.0,
            volume_leveling: 0.0,
            filter_q: 1.0,
            num_bands: crate::eq::DEFAULT_BANDS as u32,

            view: ViewMode::Pro,
            theme_mode: ThemeMode::Dark,
            window_x: 0,
            window_y: 0,
            always_on_top: false,
            run_minimized: false,

            language: "en".into(),
            language_follows_system: true,
            hide_help_tooltips: false,
            hide_notifications: false,
            hotkeys: true,
            hotkey_bindings: Hotkeys::default(),
            max_user_presets: 120,

            automatic_updates: false,
            last_update_time: 0,
        }
    }
}

impl Settings {
    /// `~/.config/fxsound` (or `$XDG_CONFIG_HOME/fxsound`).
    #[must_use]
    pub fn config_dir() -> std::path::PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("fxsound")
    }

    /// `~/.config/fxsound/settings.toml`.
    #[must_use]
    pub fn config_path() -> std::path::PathBuf {
        Self::config_dir().join("settings.toml")
    }

    /// `~/.local/share/fxsound/presets` — where imported and user-saved presets land.
    #[must_use]
    pub fn user_preset_dir() -> std::path::PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("fxsound")
            .join("presets")
    }

    /// Load the settings file, falling back to defaults for anything missing or unreadable.
    ///
    /// A corrupt file is never fatal: the user's audio should still come up.
    #[must_use]
    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(settings) => settings,
                Err(err) => {
                    log::warn!("{}: {err}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                log::warn!("{}: {err}; using defaults", path.display());
                Self::default()
            }
        }
    }

    /// Write the settings file, creating the config directory if needed.
    ///
    /// Writes to a temporary file and renames, so an interrupted save cannot truncate the
    /// existing settings.
    pub fn save(&self) -> std::io::Result<()> {
        let dir = Self::config_dir();
        std::fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(self)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        let final_path = Self::config_path();
        let tmp_path = final_path.with_extension("toml.tmp");
        std::fs::write(&tmp_path, text)?;
        std::fs::rename(&tmp_path, &final_path)
    }

    /// The `node.name` FxSound should attach to on launch, for the saved direction.
    #[must_use]
    pub fn selected_device_name(&self) -> &str {
        match self.device_direction {
            crate::DeviceDirection::Output => &self.output_device_name,
            crate::DeviceDirection::Input => &self.input_device_name,
        }
    }

    /// Remember a device choice. The other direction's memory is kept, so switching back to an
    /// output after trying a microphone returns to the output the user had.
    pub fn set_selected_device(&mut self, node_name: &str, direction: crate::DeviceDirection) {
        match direction {
            crate::DeviceDirection::Output => node_name.clone_into(&mut self.output_device_name),
            crate::DeviceDirection::Input => node_name.clone_into(&mut self.input_device_name),
        }
        self.device_direction = direction;
    }

    /// The language code the UI should be shown in right now: the system's, or the explicit
    /// pick when there is one and a translation table exists for it.
    #[must_use]
    pub fn effective_language(&self) -> &'static str {
        crate::i18n::resolve(self.language_follows_system, &self.language)
    }

    /// Record a language pick from Settings. `None` means "follow the system".
    pub fn choose_language(&mut self, code: Option<&str>) {
        match code {
            Some(code) => {
                code.clone_into(&mut self.language);
                self.language_follows_system = false;
            }
            None => self.language_follows_system = true,
        }
    }

    /// The preset remembered for a device, if this device has been seen before.
    #[must_use]
    pub fn preset_for_device(&self, node_name: &str) -> Option<&str> {
        self.device_configs
            .iter()
            .find(|c| c.device_id == node_name)
            .map(|c| c.preset.as_str())
    }

    /// Remember which preset was in use for a device.
    pub fn remember_device_preset(&mut self, node_name: &str, description: &str, preset: &str) {
        if let Some(existing) = self
            .device_configs
            .iter_mut()
            .find(|c| c.device_id == node_name)
        {
            existing.device_name = description.to_owned();
            existing.preset = preset.to_owned();
        } else {
            self.device_configs.push(DeviceConfig {
                device_id: node_name.to_owned(),
                device_name: description.to_owned(),
                preset: preset.to_owned(),
                device_form_factor: String::new(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let original = Settings::default();
        let text = toml::to_string_pretty(&original).expect("serialise");
        let parsed: Settings = toml::from_str(&text).expect("parse");
        assert_eq!(original, parsed);
    }

    #[test]
    fn an_empty_file_yields_defaults() {
        let parsed: Settings = toml::from_str("").expect("parse empty");
        assert_eq!(parsed, Settings::default());
    }

    #[test]
    fn unknown_keys_do_not_break_loading() {
        let parsed: Settings =
            toml::from_str("preset = \"Jazz\"\nsome_future_key = 3\n").expect("parse");
        assert_eq!(parsed.preset, "Jazz");
        assert_eq!(parsed.max_user_presets, 120);
    }

    #[test]
    fn device_presets_are_remembered_and_updated() {
        let mut s = Settings::default();
        s.remember_device_preset("alsa_output.pci-0000_00_1f.3", "Speakers", "Rock");
        assert_eq!(s.preset_for_device("alsa_output.pci-0000_00_1f.3"), Some("Rock"));
        s.remember_device_preset("alsa_output.pci-0000_00_1f.3", "Speakers", "Jazz");
        assert_eq!(s.device_configs.len(), 1);
        assert_eq!(s.preset_for_device("alsa_output.pci-0000_00_1f.3"), Some("Jazz"));
    }
}
