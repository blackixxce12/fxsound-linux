//! A music preset as the application plays it: the one reading of a `.fac` into the music chain's
//! [`DspParams`].
//!
//! The application reads a preset into its window's controls ([`music_controls`]) and maps the
//! controls onto the chain ([`write_music_params`]); an application's playback route does the
//! same in one step ([`preset_params`]). Everything else that renders a preset — the bit-exactness
//! harness of «Like FxSound for Windows» (`tests/windows_parity_bitexact.rs`), the drift
//! measurement (`tests/preset_drift.rs`) and `examples/process_wav.rs`, which the blind
//! listening comparison of `scripts/voicing` renders with — goes through [`preset_params`] too, so
//! what they measure and play is what the application plays, not the raw values of the file.
//!
//! The raw values and the played ones differ in three places, each of them the application's
//! reading of a preset rather than the engine's:
//!
//! * **The effects** are read at the slider position they show at and written back from it
//!   ([`scale::value_to_slider_in`], [`scale::slider_to_value_in`]): a Dynamic Boost past the
//!   slider's dead top plays as the top, which it sounds like anyway.
//! * **The curve** is played on the user's band count, not the preset's: fitted onto the live
//!   ladder by frequency when the counts differ ([`crate::eq::fit_preset_gains`], 0.4.0 audit #13),
//!   flat with the equalizer on for a preset with no equalizer at all (the original's "old
//!   preset").
//! * **A twenty-band curve on the Windows ladder** is moved onto the half-octave ladder that
//!   replaced it, band for band (0.4.0 audit R4,
//!   [`fxsound_core::eq::move_off_the_windows_twenty_band_ladder`]).
//!
//! The levels every music chain shares — the filter width, the master gain, the balance and the
//! volume leveller — are the settings', not the preset's ([`MusicLevels`]), and so is whose DSP
//! plays them: FxSound for Linux's, or at «Like FxSound for Windows» = Interface and sound the
//! Windows build's ([`DspCompat`]). The same `.fac` is read at either, and the Windows DSP reads
//! it as the Windows build does: Ambience's slider on its straight line again (audit #39), a curve
//! of another count by position (#13), and twenty bands on the Windows ladder, a curve on the
//! half-octave one moved onto it band for band (R4) — so a preset taken there and back is the
//! preset it was (A12).

use fxsound_core::messages::DspParams;
use fxsound_core::{DspCompat, Effect, EqBand, Preset, Settings, scale};

/// The engine's band ladder for `count` bands: the original's hard-coded table where it has
/// one, else the geometric ladder [`crate::GraphicEq`] builds. FxSound for Linux's:
/// [`ladder_in`] with [`DspCompat::Linux`].
#[must_use]
pub fn ladder(count: usize) -> Vec<f32> {
    ladder_in(count, DspCompat::Linux)
}

/// [`ladder`] in the DSP `compat` plays: at «Like FxSound for Windows» = Interface and sound
/// twenty bands are the Windows build's ladder ([`crate::eq::band_table_in`], audit report R4).
#[must_use]
pub fn ladder_in(count: usize, compat: DspCompat) -> Vec<f32> {
    crate::eq::band_table_in(count, compat).map_or_else(
        || {
            let mut eq = crate::GraphicEq::new();
            eq.set_num_bands(count);
            eq.center_frequencies().to_vec()
        },
        |(table, _, _)| table.to_vec(),
    )
}

/// An equalizer as the window holds it, from a snapshot's two parallel arrays.
#[must_use]
pub fn bands_of(centres: &[f32], boosts: &[f32]) -> Vec<EqBand> {
    centres
        .iter()
        .zip(boosts)
        .map(|(&center_hz, &boost_db)| EqBand {
            center_hz,
            boost_db,
        })
        .collect()
}

/// What a music preset puts in the window: the five effects at their slider positions, and the
/// equalizer's switch and bands.
#[derive(Debug, Clone, PartialEq)]
pub struct MusicControls {
    /// Slider positions, `0.0..=10.0`, indexed by `Effect as usize`.
    pub effects: [f32; Effect::COUNT],
    pub eq_on: bool,
    pub eq_bands: Vec<EqBand>,
}

/// A music preset read into the window's controls on `ladder`, the live band ladder, in the DSP
/// `compat` plays — the one reading of a `.fac` there is: the application's lane and an
/// application's route both go through it, so a preset sounds the same on either.
///
/// The effects land where the slider shows them, which is where they sound: a Dynamic Boost past
/// the slider's dead top is read as the top ([`scale::value_to_slider_in`]). The curve is the
/// preset's own when it has as many bands as the ladder, fitted onto the ladder when it has
/// another count ([`crate::eq::fit_preset_gains_in`]: by frequency, audit #13, or at «Like
/// FxSound for Windows» = Interface and sound by position, as the Windows build fits it), and
/// flat with the equalizer on when it has none, the original's "old preset".
///
/// A twenty-band curve on the other DSP's twenty-band ladder is read on this one's, band for band
/// (0.4.0 audit R4, [`fxsound_core::eq::move_to_the_twenty_band_ladder_of`]): a Windows preset,
/// or one saved before 0.4.0, on the half-octave ladder, so it plays without the paired ladder's
/// ripple and is saved on the new one the next time it is saved; and at Interface and sound a
/// preset saved on the half-octave ladder on the Windows one, as the Windows build tunes twenty
/// bands.
#[must_use]
pub fn music_controls(preset: &Preset, ladder: &[f32], compat: DspCompat) -> MusicControls {
    let effects =
        Effect::ALL.map(|effect| scale::value_to_slider_in(compat, effect, preset.effect(effect)));
    let mut preset_bands = preset.eq_bands.clone();
    fxsound_core::eq::move_to_the_twenty_band_ladder_of(&mut preset_bands, compat);
    let (eq_on, eq_bands) = if preset_bands.is_empty() {
        (true, bands_of(ladder, &vec![0.0; ladder.len()]))
    } else if preset_bands.len() == ladder.len() {
        (preset.eq_on, preset_bands)
    } else {
        let centres: Vec<f32> = preset_bands.iter().map(|b| b.center_hz).collect();
        let gains: Vec<f32> = preset_bands.iter().map(|b| b.boost_db).collect();
        let fitted = crate::eq::fit_preset_gains_in(&centres, &gains, ladder, compat);
        (preset.eq_on, bands_of(ladder, &fitted))
    };
    MusicControls {
        effects,
        eq_on,
        eq_bands,
    }
}

/// The levels every music chain shares: settings over every `.fac`, as the original's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MusicLevels {
    pub filter_q: f32,
    pub master_gain_db: f32,
    pub balance_db: f32,
    pub volume_leveling: f32,
    /// Whose DSP plays the chain: the Windows build's from «Like FxSound for Windows» =
    /// Interface and sound on ([`DspCompat::for_level`]).
    pub compat: DspCompat,
}

impl MusicLevels {
    /// The levels the settings file holds, which are the speakers' whichever lane the window
    /// edits, and the DSP the level of «Like FxSound for Windows» in force plays (Everything,
    /// which this version does not offer, runs as Interface and sound).
    #[must_use]
    pub const fn of(settings: &Settings) -> Self {
        Self {
            filter_q: settings.filter_q,
            master_gain_db: settings.master_gain,
            balance_db: settings.balance,
            volume_leveling: settings.volume_leveling,
            compat: DspCompat::for_level(settings.windows_parity.offered_or_below()),
        }
    }

    /// The band ladder of `count` bands the levels' DSP plays ([`ladder_in`]).
    ///
    /// A method rather than a call to [`ladder_in`] for the two-build comparisons, like
    /// [`MusicLevels::with_windows_dsp`]: `scripts/reference-checkout.sh` gives a build that has
    /// one ladder a method of this name that returns its [`ladder`].
    #[must_use]
    pub fn ladder(self, count: usize) -> Vec<f32> {
        ladder_in(count, self.compat)
    }

    /// The engine value a slider position of `effect` is saved as in the levels' DSP
    /// ([`scale::slider_to_value_in`]). A method for the same reason as [`MusicLevels::ladder`].
    #[must_use]
    pub fn slider_to_value(self, effect: Effect, slider: f32) -> f32 {
        scale::slider_to_value_in(self.compat, effect, slider)
    }

    /// The slider position an engine value of `effect` shows at in the levels' DSP
    /// ([`scale::value_to_slider_in`]). A method for the same reason as [`MusicLevels::ladder`].
    #[must_use]
    pub fn value_to_slider(self, effect: Effect, value: f32) -> f32 {
        scale::value_to_slider_in(self.compat, effect, value)
    }

    /// The same levels played by the Windows build's DSP, or by FxSound for Linux's.
    ///
    /// A `bool` rather than a [`DspCompat`] for the two-build comparisons
    /// (`tests/windows_parity_bitexact.rs`), whose copy in an older build must compile where there
    /// is no `DspCompat`: `scripts/reference-checkout.sh` gives such a build a method of this name
    /// that changes nothing, since it has only the one DSP.
    #[must_use]
    pub const fn with_windows_dsp(self, windows: bool) -> Self {
        Self {
            compat: if windows {
                DspCompat::Windows
            } else {
                DspCompat::Linux
            },
            ..self
        }
    }
}

impl Default for MusicLevels {
    /// The levels of a settings file that has never been touched.
    fn default() -> Self {
        Self::of(&Settings::default())
    }
}

/// Map a music chain's controls onto its snapshot: the effects from their slider positions in the
/// levels' DSP ([`scale::slider_to_value_in`]), the equalizer, and the shared levels. Everything
/// else in `params` is left as it is — the power and the mute are the whole application's.
pub fn write_music_params(
    params: &mut DspParams,
    effects: &[f32; Effect::COUNT],
    eq_on: bool,
    eq_bands: &[EqBand],
    levels: MusicLevels,
) {
    for effect in Effect::ALL {
        params.set_effect(
            effect,
            scale::slider_to_value_in(levels.compat, effect, effects[effect as usize]),
        );
    }
    params.eq_on = eq_on;
    params.set_bands(eq_bands);
    params.filter_q = levels.filter_q;
    params.master_gain_db = levels.master_gain_db;
    params.balance = levels.balance_db;
    params.volume_leveling_db = levels.volume_leveling;
    params.compat = levels.compat;
}

/// A music preset as the application plays it on `ladder` with `levels`: [`music_controls`]
/// mapped onto a default snapshot by [`write_music_params`], the power on.
///
/// This is what an application's playback route runs, and what the music lane runs after the
/// preset is picked while the window holds the engine's own ladder for the band count — the
/// application's tests hold it to that. `ladder` is the user's band count's: `levels`'
/// [`MusicLevels::ladder`] of `settings.num_bands`.
#[must_use]
pub fn preset_params(preset: &Preset, ladder: &[f32], levels: MusicLevels) -> DspParams {
    let MusicControls {
        effects,
        eq_on,
        eq_bands,
    } = music_controls(preset, ladder, levels.compat);
    let mut params = DspParams::default();
    write_music_params(&mut params, &effects, eq_on, &eq_bands, levels);
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(effects: [u8; 5], bands: &[(f32, f32)]) -> Preset {
        let mut preset = Preset::default();
        for (effect, midi) in Effect::ALL.into_iter().zip(effects) {
            preset.set_effect(effect, scale::midi_to_value(midi));
        }
        preset.eq_bands = bands
            .iter()
            .map(|&(center_hz, boost_db)| EqBand {
                center_hz,
                boost_db,
            })
            .collect();
        preset.eq_on = true;
        preset
    }

    #[test]
    fn a_preset_of_the_live_count_plays_its_own_curve() {
        let ten = ladder(10);
        let bands: Vec<(f32, f32)> = ten.iter().map(|&hz| (hz, 3.0)).collect();
        let params = preset_params(&preset([0; 5], &bands), &ten, MusicLevels::default());
        let (centres, gains) = params.bands();
        assert_eq!(centres, ten.as_slice());
        assert!(gains.iter().all(|&g| g == 3.0));
    }

    #[test]
    fn a_preset_of_another_count_is_fitted_onto_the_live_ladder() {
        let ten = ladder(10);
        let bands: Vec<(f32, f32)> = ten.iter().map(|&hz| (hz, 6.0)).collect();
        let thirty_one = ladder(31);
        let params = preset_params(&preset([0; 5], &bands), &thirty_one, MusicLevels::default());
        let (centres, gains) = params.bands();
        assert_eq!(centres, thirty_one.as_slice());
        let centres_ten: Vec<f32> = ten.clone();
        let gains_ten = vec![6.0; 10];
        assert_eq!(
            gains,
            crate::eq::fit_preset_gains(&centres_ten, &gains_ten, &thirty_one).as_slice()
        );
    }

    #[test]
    fn a_preset_with_no_equalizer_plays_flat_with_the_equalizer_on() {
        let mut empty = preset([0; 5], &[]);
        empty.eq_on = false;
        let twenty = ladder(20);
        let params = preset_params(&empty, &twenty, MusicLevels::default());
        assert!(params.eq_on);
        assert_eq!(params.bands().0, twenty.as_slice());
        assert!(params.bands().1.iter().all(|&g| g == 0.0));
    }

    #[test]
    fn a_windows_twenty_band_curve_is_played_on_the_half_octave_ladder() {
        let bands: Vec<(f32, f32)> = fxsound_core::eq::WINDOWS_TWENTY_BAND_CENTRES_HZ
            .iter()
            .map(|&hz| (hz, 2.0))
            .collect();
        let twenty = ladder(20);
        let params = preset_params(&preset([0; 5], &bands), &twenty, MusicLevels::default());
        assert_eq!(
            params.bands().0,
            fxsound_core::eq::TWENTY_BAND_CENTRES_HZ.as_slice()
        );
    }

    #[test]
    fn a_dynamic_boost_past_the_dead_top_plays_as_the_top() {
        let top = preset([0, 0, 0, scale::DYNAMIC_BOOST_MAX_MIDI, 0], &[]);
        let past = preset([0, 0, 0, 127, 0], &[]);
        let levels = MusicLevels::default();
        let ten = ladder(10);
        assert_eq!(
            preset_params(&past, &ten, levels).effect(Effect::DynamicBoost),
            preset_params(&top, &ten, levels).effect(Effect::DynamicBoost)
        );
    }

    #[test]
    fn the_levels_are_the_settings_and_the_power_is_on() {
        let mut settings = Settings::default();
        settings.filter_q = 2.0;
        settings.master_gain = -6.0;
        settings.balance = 4.0;
        settings.volume_leveling = 1.5;
        let params = preset_params(
            &preset([64; 5], &[]),
            &ladder(10),
            MusicLevels::of(&settings),
        );
        assert!(params.power);
        assert_eq!(params.filter_q, 2.0);
        assert_eq!(params.master_gain_db, -6.0);
        assert_eq!(params.balance, 4.0);
        assert_eq!(params.volume_leveling_db, 1.5);
        assert_eq!(params.compat, DspCompat::Linux);
    }

    #[test]
    fn interface_and_sound_plays_the_windows_dsp_and_the_levels_below_it_do_not() {
        use fxsound_core::WindowsParity;
        for level in WindowsParity::ALL {
            let mut settings = Settings::default();
            settings.windows_parity = level;
            let params = preset_params(
                &preset([64; 5], &[]),
                &ladder(10),
                MusicLevels::of(&settings),
            );
            let windows = matches!(level, WindowsParity::Sound | WindowsParity::Full);
            assert_eq!(params.compat.windows(), windows, "{level:?}");
        }
    }

    #[test]
    fn the_windows_dsp_changes_only_what_the_windows_build_reads_differently() {
        // Ten bands, no Ambience below 39: the same snapshot but for `compat`.
        let bands: Vec<(f32, f32)> = ladder(10).iter().map(|&hz| (hz, 4.0)).collect();
        let preset = preset([30, 60, 0, 120, 10], &bands);
        let linux = preset_params(&preset, &ladder(10), MusicLevels::default());
        let windows_levels = MusicLevels::default().with_windows_dsp(true);
        let windows = preset_params(&preset, &windows_levels.ladder(10), windows_levels);
        assert_eq!(windows.compat, DspCompat::Windows);
        assert_eq!(
            DspParams {
                compat: DspCompat::Linux,
                ..windows
            },
            linux
        );
        assert_eq!(
            MusicLevels::default()
                .with_windows_dsp(true)
                .with_windows_dsp(false),
            MusicLevels::default()
        );
    }

    #[test]
    fn at_interface_and_sound_twenty_bands_are_the_windows_ladder_and_a_half_octave_curve_moves_onto_it()
     {
        // Audit report R4 taken back: the ladder the window gets, and a curve saved on the
        // half-octave ladder — by 0.4.0, or at Off — read onto the Windows one band for band.
        let windows = MusicLevels::default().with_windows_dsp(true);
        assert_eq!(
            windows.ladder(20),
            fxsound_core::eq::WINDOWS_TWENTY_BAND_CENTRES_HZ
        );
        assert_eq!(MusicLevels::default().ladder(20), ladder(20));
        let half_octave: Vec<(f32, f32)> = fxsound_core::eq::TWENTY_BAND_CENTRES_HZ
            .iter()
            .enumerate()
            .map(|(i, &hz)| (hz, i as f32 - 9.5))
            .collect();
        let played = preset_params(&preset([0; 5], &half_octave), &windows.ladder(20), windows);
        let (centres, gains) = played.bands();
        assert_eq!(centres, fxsound_core::eq::WINDOWS_TWENTY_BAND_CENTRES_HZ);
        let want: Vec<f32> = half_octave.iter().map(|&(_, gain)| gain).collect();
        assert_eq!(gains, want.as_slice(), "every gain on its band");
    }

    #[test]
    fn a_twenty_band_curve_read_at_interface_and_sound_and_back_is_the_curve_it_was() {
        // A12, at the reading: the two ladders are band for band, so either curve comes back to
        // the bit from the other DSP.
        for ladder_hz in [
            fxsound_core::eq::TWENTY_BAND_CENTRES_HZ,
            fxsound_core::eq::WINDOWS_TWENTY_BAND_CENTRES_HZ,
        ] {
            let bands: Vec<(f32, f32)> = ladder_hz
                .iter()
                .enumerate()
                .map(|(i, &hz)| (hz, (i as f32 * 0.37).sin() * 9.0))
                .collect();
            let preset = preset([0; 5], &bands);
            let linux = music_controls(&preset, &ladder(20), DspCompat::Linux);
            let windows = music_controls(
                &preset,
                &ladder_in(20, DspCompat::Windows),
                DspCompat::Windows,
            );
            let mut there = linux.eq_bands.clone();
            assert!(fxsound_core::eq::move_to_the_twenty_band_ladder_of(
                &mut there,
                DspCompat::Windows
            ));
            assert_eq!(there, windows.eq_bands);
            let mut back = there;
            fxsound_core::eq::move_to_the_twenty_band_ladder_of(&mut back, DspCompat::Linux);
            assert_eq!(back, linux.eq_bands);
        }
    }

    #[test]
    fn at_interface_and_sound_a_curve_of_another_count_is_fitted_by_position() {
        // Audit report #13 taken back.
        let ten = ladder(10);
        let mut bands: Vec<(f32, f32)> = ten.iter().map(|&hz| (hz, 0.0)).collect();
        bands[0].1 = 6.0;
        let windows = MusicLevels::default().with_windows_dsp(true);
        let played = preset_params(&preset([0; 5], &bands), &windows.ladder(31), windows);
        let gains: Vec<f32> = bands.iter().map(|&(_, gain)| gain).collect();
        assert_eq!(
            played.bands().1,
            crate::eq::remap_band_gains_by_position(&gains, 31).as_slice()
        );
        assert_eq!(
            played.bands().1[0],
            6.0,
            "the 20 Hz band takes the 62.5 Hz boost"
        );
    }

    #[test]
    fn at_interface_and_sound_ambiences_slider_shows_a_stored_value_on_the_windows_line() {
        // Audit report #39 taken back: stored 13 is position 1 on Windows, a third of one here;
        // either way it plays as stored.
        let stored = preset([0, 13, 0, 0, 0], &[]);
        let linux = music_controls(&stored, &ladder(10), DspCompat::Linux);
        let windows = music_controls(&stored, &ladder(10), DspCompat::Windows);
        let ambience = Effect::Ambience as usize;
        assert!((windows.effects[ambience] - 1.024).abs() < 0.001);
        assert!((linux.effects[ambience] - 0.333).abs() < 0.001);
        let windows_levels = MusicLevels::default().with_windows_dsp(true);
        for (controls, levels) in [(&linux, MusicLevels::default()), (&windows, windows_levels)] {
            let mut params = DspParams::default();
            write_music_params(
                &mut params,
                &controls.effects,
                controls.eq_on,
                &controls.eq_bands,
                levels,
            );
            assert_eq!(
                scale::value_to_midi(params.effect(Effect::Ambience)),
                13,
                "{:?}",
                levels.compat
            );
        }
        // Moving the slider to position 1 saves 13 there, 39 here.
        assert_eq!(
            scale::value_to_midi(windows_levels.slider_to_value(Effect::Ambience, 1.0)),
            13
        );
        assert_eq!(
            scale::value_to_midi(MusicLevels::default().slider_to_value(Effect::Ambience, 1.0)),
            39
        );
        assert_eq!(
            windows_levels.value_to_slider(Effect::Ambience, scale::midi_to_value(13)),
            windows.effects[ambience]
        );
    }
}
