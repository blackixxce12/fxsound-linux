//! What the GUI knows and what the user did to it.
//!
//! This is the port of `FxModel` (`fxsound/Source/GUI/FxModel.h`), minus the broadcaster: instead
//! of views subscribing to change events, each view renders [`UiState`] and returns a list of
//! [`UiAction`]s describing what the user did. The application layer is the only place that turns
//! an action into a real effect — writing a preset, retuning the engine, switching a device — so
//! the whole UI crate stays free of audio and file-system dependencies and can be tested headless.

use fxsound_core::{
    AudioDevice, DeviceDirection, Effect, EqBand, SpectrumFrame, ThemeMode, ViewMode,
};

/// One preset as the combo box needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetEntry {
    pub name: String,
    /// `true` for a preset that ships with the app and cannot be overwritten or deleted.
    pub factory: bool,
    /// `true` when the user has unsaved changes; drawn as a trailing `*`.
    pub modified: bool,
}

/// Everything the views draw.
#[derive(Debug, Clone)]
pub struct UiState {
    // ---- master ----------------------------------------------------------------------------
    pub power: bool,
    pub theme: ThemeMode,
    pub view: ViewMode,

    // ---- presets ---------------------------------------------------------------------------
    pub presets: Vec<PresetEntry>,
    /// Index into `presets`, or `None` when the list is empty.
    pub selected_preset: Option<usize>,

    // ---- device ----------------------------------------------------------------------------
    pub devices: Vec<AudioDevice>,
    pub selected_device: Option<usize>,
    /// Which chain the engine is running, which follows the selected device.
    ///
    /// The original had no such thing — it only ever sat in front of a playback endpoint — so
    /// everything drawn differently because of this is drawn *only* in the input direction, where
    /// there is no layout to be faithful to.
    pub direction: DeviceDirection,

    // ---- effects ---------------------------------------------------------------------------
    /// The five knobs on the GUI's own `0..=10` scale, indexed by `Effect as usize`.
    pub effects: [f32; Effect::COUNT],

    // ---- equalizer -------------------------------------------------------------------------
    pub eq_on: bool,
    pub eq_bands: Vec<EqBand>,
    /// The filter-width knob, `1.0..=3.0` in steps of `0.5`.
    pub filter_q: f32,

    // ---- levels ----------------------------------------------------------------------------
    /// `-20..=20` dB in steps of 2.
    pub master_gain_db: f32,
    /// `-20..=20` dB in steps of 1; positive pans right.
    pub balance_db: f32,
    /// The abstract `0..=4` amount, in steps of 0.5.
    pub volume_leveling: f32,

    // ---- live data -------------------------------------------------------------------------
    pub spectrum: SpectrumFrame,
    /// `true` while audio is actually flowing; the visualizer idles otherwise.
    pub audio_active: bool,
    /// The rate the engine is running at, which is the device's, not a preference.
    ///
    /// Read for one purpose: an equalizer band centred at or above half of it cannot be built, so
    /// the band is bypassed. A bypassed band that still *looks* live is a control that silently
    /// does nothing, which is the one thing this port keeps refusing to ship.
    pub sample_rate: u32,
    /// Gain reduction of the three microphone stages, in dB, as positive numbers. Zero in the
    /// output direction, which has none of them.
    pub gate_reduction_db: f32,
    pub compressor_reduction_db: f32,
    pub deesser_reduction_db: f32,

    // ---- the microphone chain --------------------------------------------------------------
    /// Which of the voice chain's three dynamics stages are switched on.
    ///
    /// All three start off and stay off until an input preset turns them on, which is the whole
    /// of the caution in `sync_input_params_from_state`: nobody has voiced them yet, and an
    /// upgrade must not start gating someone's quiet talker. They live here rather than in the
    /// mapping so that a preset, once there are any, moves them the same way it moves everything
    /// else.
    pub gate_on: bool,
    pub compressor_on: bool,
    pub deesser_on: bool,

    // ---- chrome ----------------------------------------------------------------------------
    /// Transient message shown in the notification strip, with the frame count left to live.
    pub notification: Option<String>,
    /// Suppresses every tooltip, matching the `hide_help_tooltips` setting.
    pub hide_tooltips: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            power: true,
            theme: ThemeMode::Dark,
            view: ViewMode::Pro,
            presets: Vec::new(),
            selected_preset: None,
            devices: Vec::new(),
            selected_device: None,
            direction: DeviceDirection::Output,
            effects: [0.0; Effect::COUNT],
            eq_on: true,
            eq_bands: fxsound_core::eq::default_bands(),
            filter_q: 1.0,
            master_gain_db: 0.0,
            balance_db: 0.0,
            volume_leveling: 0.0,
            spectrum: [0.0; fxsound_core::NUM_SPECTRUM_BARS],
            audio_active: false,
            sample_rate: 48_000,
            gate_reduction_db: 0.0,
            compressor_reduction_db: 0.0,
            deesser_reduction_db: 0.0,
            gate_on: false,
            compressor_on: false,
            deesser_on: false,
            notification: None,
            hide_tooltips: false,
        }
    }
}

impl UiState {
    /// The selected preset, if any.
    #[must_use]
    pub fn preset(&self) -> Option<&PresetEntry> {
        self.selected_preset.and_then(|i| self.presets.get(i))
    }

    /// The name to show in the combo box, with the modified marker the original appends.
    #[must_use]
    pub fn preset_label(&self) -> String {
        match self.preset() {
            Some(preset) if preset.modified => format!("{}*", preset.name),
            Some(preset) => preset.name.clone(),
            None => String::new(),
        }
    }

    /// The selected output device, if any.
    #[must_use]
    pub fn device(&self) -> Option<&AudioDevice> {
        self.selected_device.and_then(|i| self.devices.get(i))
    }

    /// One effect on the GUI's `0..=10` scale.
    #[must_use]
    pub fn effect(&self, effect: Effect) -> f32 {
        self.effects[effect as usize]
    }

    /// Whether the controls should be drawn enabled. The original greys everything out when the
    /// power is off (`FxProView.cpp:117-123`).
    #[must_use]
    pub const fn controls_enabled(&self) -> bool {
        self.power
    }

    /// Whether the five effect sliders do anything.
    ///
    /// They are the music chain's. A microphone runs the voice chain instead, and the two share
    /// the ten-band equalizer and nothing else — reverberation, stereo widening and a bass lift
    /// are the opposite of what a voice wants. So on a microphone these five are inert, and a
    /// control that is inert has to *look* inert: the alternative is a slider that moves, reads
    /// back the value it was given, and changes nothing anyone can hear.
    #[must_use]
    pub const fn music_effects_apply(&self) -> bool {
        matches!(self.direction, DeviceDirection::Output)
    }

    /// Whether an equalizer band can be built at the current rate.
    ///
    /// A band centred at or above Nyquist has nowhere to sit: the design bypasses it, and the
    /// fader would otherwise be a control that does nothing. The case is not hypothetical — a
    /// Bluetooth headset captures at 16 kHz, where the top two bands of the standard ladder are
    /// both past it.
    #[must_use]
    pub fn band_is_live(&self, band: usize) -> bool {
        // Spelled `2·f0 < fs` rather than `f0 < fs/2` to match `GraphicEq::set_band_boost`
        // character for character. The two are the same number in exact arithmetic and the same
        // number in f32, but this rule is duplicated across a crate boundary — the equalizer
        // cannot be reached from here — and a duplicated rule that is *written* differently is one
        // that drifts. `fxsound-app` holds the test that keeps the two answering alike.
        self.eq_bands
            .get(band)
            .is_some_and(|band| band.center_hz * 2.0 < self.sample_rate as f32)
    }

    /// How many of the equalizer's bands the current rate cannot carry.
    #[must_use]
    pub fn dead_band_count(&self) -> usize {
        (0..self.eq_bands.len())
            .filter(|&band| !self.band_is_live(band))
            .count()
    }

    /// Index of the next preset, wrapping, or `None` when there are fewer than two.
    #[must_use]
    pub fn next_preset(&self) -> Option<usize> {
        let count = self.presets.len();
        if count < 2 {
            return None;
        }
        Some(self.selected_preset.map_or(0, |i| (i + 1) % count))
    }

    /// Index of the previous preset, wrapping.
    #[must_use]
    pub fn previous_preset(&self) -> Option<usize> {
        let count = self.presets.len();
        if count < 2 {
            return None;
        }
        Some(
            self.selected_preset
                .map_or(count - 1, |i| (i + count - 1) % count),
        )
    }
}

/// Something the user did. Views emit these; the application layer acts on them.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    /// Toggle the master power.
    TogglePower,
    /// Switch between the Pro and Lite windows.
    ToggleView,
    /// Switch the palette.
    ToggleTheme,

    /// Select a preset by index.
    SelectPreset(usize),
    /// Save the current settings over the selected preset.
    SavePreset,
    /// Save the current settings under a new name.
    SavePresetAs(String),
    /// Discard unsaved changes and reload the selected preset.
    UndoPresetChanges,
    /// Delete the selected preset.
    DeletePreset,

    /// Select an output device by index.
    SelectDevice(usize),

    /// Move one effect knob, on the GUI's `0..=10` scale.
    SetEffect(Effect, f32),
    /// Move one equalizer band's gain, in dB.
    SetBandGain(usize, f32),
    /// Move one equalizer band's centre frequency, in Hz.
    SetBandFrequency(usize, f32),
    /// Switch the equalizer on or off.
    SetEqEnabled(bool),
    /// Change how many bands the equalizer has.
    SetBandCount(usize),
    /// Move the filter-width knob.
    SetFilterQ(f32),
    /// Flatten the equalizer and return the level controls to their defaults.
    RestoreDefaults,

    SetMasterGain(f32),
    SetBalance(f32),
    SetVolumeLeveling(f32),

    /// Open the settings window.
    OpenSettings,
    /// Open the hamburger menu.
    OpenMenu,
    /// Hide to the tray.
    Minimise,
    /// Close the window.
    Close,
    /// Start dragging the frameless window.
    DragWindow,
}

/// What a view returns after one frame.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UiResponse {
    pub actions: Vec<UiAction>,
}

impl UiResponse {
    pub fn push(&mut self, action: UiAction) {
        self.actions.push(action);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    #[must_use]
    pub fn contains(&self, action: &UiAction) -> bool {
        self.actions.contains(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_band_above_the_devices_nyquist_limit_is_not_live() {
        // The case is a Bluetooth headset, which captures at 16 kHz: the standard ladder's top two
        // bands, 8640 and 16000 Hz, are both at or past 8 kHz and the design bypasses them. A
        // fader that still looks live there is a control that does nothing.
        let mut state = UiState {
            sample_rate: 16_000,
            ..UiState::default()
        };
        assert_eq!(state.eq_bands.len(), 10);
        assert!(state.band_is_live(0), "62.5 Hz must survive anything");
        assert!(!state.band_is_live(8), "8640 Hz is past 8 kHz");
        assert!(!state.band_is_live(9), "16000 Hz is far past it");
        assert_eq!(state.dead_band_count(), 2);

        // And at a rate that can carry them, every band is live.
        state.sample_rate = 48_000;
        assert_eq!(state.dead_band_count(), 0);
    }

    #[test]
    fn the_music_effects_apply_only_to_a_playback_device() {
        let mut state = UiState::default();
        assert!(state.music_effects_apply());
        state.direction = DeviceDirection::Input;
        assert!(
            !state.music_effects_apply(),
            "reverberation and a bass lift are not what a voice wants"
        );
        // The power switch is a different question and is not answered by this one.
        assert!(state.controls_enabled());
    }

    fn state_with_presets(count: usize) -> UiState {
        UiState {
            presets: (0..count)
                .map(|i| PresetEntry {
                    name: format!("Preset {i}"),
                    factory: true,
                    modified: false,
                })
                .collect(),
            selected_preset: if count > 0 { Some(0) } else { None },
            ..UiState::default()
        }
    }

    #[test]
    fn the_preset_label_marks_unsaved_changes() {
        let mut state = state_with_presets(2);
        assert_eq!(state.preset_label(), "Preset 0");
        state.presets[0].modified = true;
        assert_eq!(state.preset_label(), "Preset 0*");
    }

    #[test]
    fn an_empty_preset_list_has_no_label_and_no_navigation() {
        let state = state_with_presets(0);
        assert_eq!(state.preset_label(), "");
        assert!(state.preset().is_none());
        assert!(state.next_preset().is_none());
        assert!(state.previous_preset().is_none());
    }

    #[test]
    fn a_single_preset_cannot_be_cycled() {
        let state = state_with_presets(1);
        assert!(state.next_preset().is_none());
        assert!(state.previous_preset().is_none());
    }

    #[test]
    fn preset_navigation_wraps_in_both_directions() {
        let mut state = state_with_presets(3);
        assert_eq!(state.next_preset(), Some(1));
        state.selected_preset = Some(2);
        assert_eq!(state.next_preset(), Some(0));
        assert_eq!(state.previous_preset(), Some(1));
        state.selected_preset = Some(0);
        assert_eq!(state.previous_preset(), Some(2));
    }

    #[test]
    fn the_power_state_gates_the_controls() {
        let mut state = UiState::default();
        assert!(state.controls_enabled());
        state.power = false;
        assert!(!state.controls_enabled());
    }

    #[test]
    fn effects_are_indexed_by_the_gui_enum_order() {
        let mut state = UiState::default();
        state.effects[Effect::Bass as usize] = 7.0;
        assert_eq!(state.effect(Effect::Bass), 7.0);
        assert_eq!(state.effect(Effect::Fidelity), 0.0);
    }

    #[test]
    fn responses_collect_actions_in_order() {
        let mut response = UiResponse::default();
        assert!(response.is_empty());
        response.push(UiAction::TogglePower);
        response.push(UiAction::SetEffect(Effect::Ambience, 5.0));
        assert!(response.contains(&UiAction::TogglePower));
        assert_eq!(response.actions.len(), 2);
        assert_eq!(
            response.actions[1],
            UiAction::SetEffect(Effect::Ambience, 5.0)
        );
    }
}
