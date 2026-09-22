//! What the GUI knows and what the user did to it.
//!
//! This is the port of `FxModel` (`fxsound/Source/GUI/FxModel.h`), minus the broadcaster: instead
//! of views subscribing to change events, each view renders [`UiState`] and returns a list of
//! [`UiAction`]s describing what the user did. The application layer is the only place that turns
//! an action into a real effect — writing a preset, retuning the engine, switching a device — so
//! the whole UI crate stays free of audio and file-system dependencies and can be tested headless.

use fxsound_core::{
    AudioDevice, DenoiseLevel, DeviceDirection, Effect, EqBand, SpectrumFrame, ThemeMode, ViewMode,
};
use std::time::{Duration, Instant};

/// How long a notice stays on screen before the application clears it (0.4.0 design, §11).
pub const NOTICE_LIFETIME: Duration = Duration::from_secs(4);

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

    // ---- devices: two lanes ------------------------------------------------------------------
    /// Every device the engine listed: all outputs first, then all inputs.
    pub devices: Vec<AudioDevice>,
    /// The output lane's device, as an index into `devices`; `None` while the lane is detached.
    pub selected_output: Option<usize>,
    /// The input lane's device, as an index into `devices`; `None` while the lane is detached —
    /// which is the default: the microphone lane only comes up when someone picks a microphone.
    pub selected_input: Option<usize>,
    /// The **edit direction**: which lane the preset picker, the equalizer, the level controls and
    /// the meters address. Both lanes can run at once; this only says which one the window edits.
    ///
    /// Every field in this struct that describes *a* chain — `presets`, `selected_preset`,
    /// `effects`, the equalizer, the levels, `spectrum`, `audio_active`, `sample_rate` — holds this
    /// direction's copy. The original had no such thing — it only ever sat in front of a playback
    /// endpoint — so everything drawn differently because of this is drawn *only* in the input
    /// direction, where there is no layout to be faithful to.
    pub direction: DeviceDirection,
    /// Whether each lane is actually moving audio right now.
    pub output_active: bool,
    pub input_active: bool,

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
    /// Gain reduction of the three microphone stages, in dB, as positive numbers.
    ///
    /// This field and everything down to `denoise_level` is the **input lane's** telemetry
    /// whichever lane is being edited: the output chain has none of these stages.
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
    /// Whether the preset asks for RNNoise in front of everything else.
    pub denoise_on: bool,
    /// Whether the two stages that can be asked for and still not run are running: the de-esser
    /// needs a rate that can carry its crossover, and RNNoise exists at 48 kHz and nowhere else.
    /// Asked-for-but-not-running is a third state, and the interface has to be able to say it.
    pub deesser_running: bool,
    pub denoise_running: bool,
    /// The denoiser's voice probability for the last frame, `0.0..=1.0`.
    pub voice_probability: f32,
    /// The microphone's running noise floor, measured before the chain, in dBFS.
    pub noise_floor_db: f32,
    /// What the denoiser and the de-reverb take away, as positive dB.
    pub denoise_reduction_db: f32,
    pub dereverb_reduction_db: f32,
    /// The corner the de-esser actually built, and the one the preset asked for. They differ when
    /// the adaptive mode lowered the corner for a narrow source; zero means "not known".
    pub deesser_hz: f32,
    pub deesser_requested_hz: f32,
    /// The de-reverb stage and echo cancellation, asked for or not.
    pub dereverb_on: bool,
    pub echo_cancel_on: bool,
    /// Whether the echo canceller is actually loaded: asked-for-but-not-running is a third state
    /// here too (a system without the WebRTC module).
    pub echo_cancel_running: bool,
    /// The suppression level in force, with the global override already applied.
    pub denoise_level: DenoiseLevel,

    // ---- chrome ----------------------------------------------------------------------------
    /// A transient message: drawn as a bubble in the Pro window and as a strip in the Lite one.
    ///
    /// A plain `String` rather than a struct carrying its own deadline, so that the views and the
    /// pixel tests can build a state with a notice up in one literal. The clock lives beside it in
    /// [`UiState::notice_clock`], and [`UiState::expire_notification`] — which the application
    /// calls once per poll — clears the notice [`NOTICE_LIFETIME`] after its clock started. The
    /// views only draw it; they never time it.
    ///
    /// **Raise a notice with [`UiState::notify`]**, which sets both and restarts the clock. A text
    /// written straight into this field is stamped on the first poll that sees it, which is enough
    /// for a literal, but the clock is keyed on the text: the same notice raised again while it is
    /// still up would keep the old clock and vanish early.
    pub notification: Option<String>,
    /// The notice last stamped and when. Stamped by the application, never by a view; a text that
    /// differs from `notification` is a new notice whose clock has not started yet.
    pub notice_clock: Option<(String, Instant)>,
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
            selected_output: None,
            selected_input: None,
            direction: DeviceDirection::Output,
            output_active: false,
            input_active: false,
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
            denoise_on: false,
            deesser_running: false,
            denoise_running: false,
            voice_probability: 0.0,
            noise_floor_db: -100.0,
            denoise_reduction_db: 0.0,
            dereverb_reduction_db: 0.0,
            deesser_hz: 0.0,
            deesser_requested_hz: 0.0,
            dereverb_on: false,
            echo_cancel_on: false,
            echo_cancel_running: false,
            denoise_level: DenoiseLevel::default(),
            notification: None,
            notice_clock: None,
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

    /// One lane's device, as an index into `devices`; `None` while that lane is detached.
    #[must_use]
    pub const fn selection(&self, direction: DeviceDirection) -> Option<usize> {
        match direction {
            DeviceDirection::Output => self.selected_output,
            DeviceDirection::Input => self.selected_input,
        }
    }

    /// Record one lane's device, leaving the other lane's alone.
    pub fn set_selection(&mut self, direction: DeviceDirection, index: Option<usize>) {
        match direction {
            DeviceDirection::Output => self.selected_output = index,
            DeviceDirection::Input => self.selected_input = index,
        }
    }

    /// The edit direction's device index — the one selection 0.3.0 had, now derived.
    #[must_use]
    pub const fn selected_device(&self) -> Option<usize> {
        self.selection(self.direction)
    }

    /// One lane's device, if that lane is attached and the index is still in the list.
    #[must_use]
    pub fn device_for(&self, direction: DeviceDirection) -> Option<&AudioDevice> {
        self.selection(direction)
            .and_then(|i| self.devices.get(i))
            .filter(|device| device.direction == direction)
    }

    /// The edit direction's device, if any.
    #[must_use]
    pub fn device(&self) -> Option<&AudioDevice> {
        self.device_for(self.direction)
    }

    /// Whether the output lane has a device.
    #[must_use]
    pub const fn output_enabled(&self) -> bool {
        self.selected_output.is_some()
    }

    /// Whether the input lane has a device — i.e. whether a microphone is being processed at all.
    #[must_use]
    pub const fn input_enabled(&self) -> bool {
        self.selected_input.is_some()
    }

    /// Whether one lane has a device.
    #[must_use]
    pub const fn lane_enabled(&self, direction: DeviceDirection) -> bool {
        self.selection(direction).is_some()
    }

    /// Put up a notice and start its clock now.
    ///
    /// The one way the application raises a notice. The clock restarts even when the same text is
    /// already up: a refusal clicked again three seconds after the first is a new notice, and gets
    /// its full four seconds.
    pub fn notify(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.notice_clock = Some((text.clone(), Instant::now()));
        self.notification = Some(text);
    }

    /// Take the notice down.
    pub fn dismiss_notification(&mut self) {
        self.notification = None;
        self.notice_clock = None;
    }

    /// Start the clock of a notice seen for the first time, and clear one that has been up for
    /// [`NOTICE_LIFETIME`]. Returns `true` when it cleared one.
    ///
    /// A notice written straight into `notification` is stamped here, on the first poll that sees
    /// it; one that replaces a notice still on screen gets a clock of its own rather than the
    /// remainder of the old one's.
    pub fn expire_notification(&mut self, now: Instant) -> bool {
        let Some(text) = &self.notification else {
            self.notice_clock = None;
            return false;
        };
        match &self.notice_clock {
            Some((seen, since)) if seen == text => {
                if now.saturating_duration_since(*since) >= NOTICE_LIFETIME {
                    self.dismiss_notification();
                    return true;
                }
                false
            }
            _ => {
                self.notice_clock = Some((text.clone(), now));
                false
            }
        }
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

    /// Select a device by index into `UiState::devices`, whichever direction it is.
    ///
    /// Kept for the tray and the command line, which name a device rather than a lane: the
    /// application resolves it by the device's own direction to [`UiAction::SelectOutput`] or
    /// [`UiAction::SelectInput`]. The window uses those two directly.
    SelectDevice(usize),
    /// Attach the output lane to this device (an index into `UiState::devices`).
    SelectOutput(usize),
    /// Attach the input lane to this device (an index into `UiState::devices`).
    SelectInput(usize),
    /// Detach the output lane: hand the default back and stop processing playback.
    DetachOutput,
    /// Detach the input lane: stop processing the microphone.
    DetachInput,
    /// Make this lane the one the window edits. Changes nothing the engine does.
    SetEditDirection(DeviceDirection),
    /// Take the notice down before its time is up.
    DismissNotice,

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

impl UiAction {
    /// Attach `direction`'s lane to device `index`.
    #[must_use]
    pub const fn select(direction: DeviceDirection, index: usize) -> Self {
        match direction {
            DeviceDirection::Output => Self::SelectOutput(index),
            DeviceDirection::Input => Self::SelectInput(index),
        }
    }

    /// Detach `direction`'s lane.
    #[must_use]
    pub const fn detach(direction: DeviceDirection) -> Self {
        match direction {
            DeviceDirection::Output => Self::DetachOutput,
            DeviceDirection::Input => Self::DetachInput,
        }
    }
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

    fn device(name: &str, direction: DeviceDirection) -> AudioDevice {
        AudioDevice {
            id: 0,
            name: name.to_owned(),
            description: name.to_owned(),
            is_default: false,
            direction,
            form_factor: String::new(),
        }
    }

    fn two_lanes() -> UiState {
        UiState {
            devices: vec![
                device("speakers", DeviceDirection::Output),
                device("headphones", DeviceDirection::Output),
                device("microphone", DeviceDirection::Input),
            ],
            selected_output: Some(1),
            selected_input: Some(2),
            ..UiState::default()
        }
    }

    #[test]
    fn a_fresh_state_has_both_lanes_detached_and_edits_the_output() {
        let state = UiState::default();
        assert_eq!(state.selected_output, None);
        assert_eq!(state.selected_input, None);
        assert!(!state.output_enabled());
        assert!(!state.input_enabled());
        assert_eq!(state.direction, DeviceDirection::Output);
        assert!(state.notification.is_none() && state.notice_clock.is_none());
    }

    #[test]
    fn each_lane_keeps_its_own_device() {
        let state = two_lanes();
        assert_eq!(
            state
                .device_for(DeviceDirection::Output)
                .map(|d| d.name.as_str()),
            Some("headphones")
        );
        assert_eq!(
            state
                .device_for(DeviceDirection::Input)
                .map(|d| d.name.as_str()),
            Some("microphone")
        );
        assert!(state.output_enabled() && state.input_enabled());
        assert!(state.lane_enabled(DeviceDirection::Output));
        assert!(state.lane_enabled(DeviceDirection::Input));
    }

    #[test]
    fn the_selected_device_is_the_edit_directions() {
        let mut state = two_lanes();
        assert_eq!(state.selected_device(), Some(1));
        assert_eq!(state.device().map(|d| d.name.as_str()), Some("headphones"));
        state.direction = DeviceDirection::Input;
        assert_eq!(state.selected_device(), Some(2));
        assert_eq!(state.device().map(|d| d.name.as_str()), Some("microphone"));
    }

    #[test]
    fn detaching_one_lane_leaves_the_other_alone() {
        let mut state = two_lanes();
        state.set_selection(DeviceDirection::Input, None);
        assert!(!state.input_enabled());
        assert_eq!(state.selected_output, Some(1));
        state.set_selection(DeviceDirection::Output, None);
        assert!(!state.output_enabled());
        assert_eq!(state.selection(DeviceDirection::Output), None);
    }

    #[test]
    fn an_index_of_the_wrong_direction_is_not_a_lanes_device() {
        // A stale index after the list changed under it must not make a speaker the microphone.
        let state = UiState {
            selected_input: Some(0),
            ..two_lanes()
        };
        assert!(state.device_for(DeviceDirection::Input).is_none());
        let state = UiState {
            selected_output: Some(9),
            ..two_lanes()
        };
        assert!(state.device_for(DeviceDirection::Output).is_none());
    }

    #[test]
    fn the_select_and_detach_constructors_name_the_right_lane() {
        assert_eq!(
            UiAction::select(DeviceDirection::Output, 3),
            UiAction::SelectOutput(3)
        );
        assert_eq!(
            UiAction::select(DeviceDirection::Input, 4),
            UiAction::SelectInput(4)
        );
        assert_eq!(
            UiAction::detach(DeviceDirection::Output),
            UiAction::DetachOutput
        );
        assert_eq!(
            UiAction::detach(DeviceDirection::Input),
            UiAction::DetachInput
        );
    }

    #[test]
    fn a_notice_written_directly_is_stamped_on_first_sight_and_cleared_four_seconds_later() {
        let mut state = UiState {
            notification: Some("Preset: Rock".to_owned()),
            ..UiState::default()
        };
        let start = Instant::now();
        assert!(!state.expire_notification(start));
        assert!(
            state.notice_clock.is_some(),
            "the first poll starts the clock"
        );
        assert!(!state.expire_notification(start + Duration::from_millis(3_900)));
        assert!(state.notification.is_some());
        assert!(state.expire_notification(start + NOTICE_LIFETIME));
        assert!(state.notification.is_none());
        assert!(state.notice_clock.is_none());
    }

    #[test]
    fn a_notice_that_replaces_another_gets_a_clock_of_its_own() {
        let mut state = UiState::default();
        let start = Instant::now();
        state.notification = Some("first".to_owned());
        state.expire_notification(start);
        state.notification = Some("second".to_owned());
        // Nearly four seconds after the first: the second has only just appeared.
        let later = start + Duration::from_millis(3_900);
        assert!(!state.expire_notification(later));
        assert!(!state.expire_notification(later + Duration::from_millis(3_000)));
        assert_eq!(state.notification.as_deref(), Some("second"));
        assert!(state.expire_notification(later + NOTICE_LIFETIME));
    }

    #[test]
    fn notifying_the_same_text_again_restarts_its_clock() {
        let mut state = UiState::default();
        let first = Instant::now()
            .checked_sub(Duration::from_millis(3_500))
            .expect("the clock has run for a few seconds");
        state.notification = Some("Factory presets cannot be deleted".to_owned());
        state.notice_clock = Some(("Factory presets cannot be deleted".to_owned(), first));
        state.notify("Factory presets cannot be deleted");
        // The first notice's four seconds are up; the second's are not.
        assert!(!state.expire_notification(first + NOTICE_LIFETIME));
        assert!(state.notification.is_some());
        assert!(state.expire_notification(Instant::now() + NOTICE_LIFETIME));
    }

    #[test]
    fn notify_starts_the_clock_and_dismiss_stops_it() {
        let mut state = UiState::default();
        state.notify("Saved");
        assert_eq!(state.notification.as_deref(), Some("Saved"));
        assert_eq!(
            state.notice_clock.as_ref().map(|c| c.0.as_str()),
            Some("Saved")
        );
        state.dismiss_notification();
        assert!(state.notification.is_none() && state.notice_clock.is_none());
        // No notice, no clock: a poll with nothing up leaves nothing behind.
        state.notice_clock = Some(("stale".to_owned(), Instant::now()));
        assert!(!state.expire_notification(Instant::now()));
        assert!(state.notice_clock.is_none());
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
