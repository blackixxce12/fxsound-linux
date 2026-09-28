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
//!   ([`scale::value_to_slider_for`], [`scale::slider_to_value_for`]): a Dynamic Boost past the
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
//! Windows build's ([`DspCompat`]). The same `.fac` is read the same way at every level.

use fxsound_core::messages::DspParams;
use fxsound_core::{DspCompat, Effect, EqBand, Preset, Settings, scale};

/// The engine's band ladder for `count` bands: the original's hard-coded table where it has
/// one, else the geometric ladder [`crate::GraphicEq`] builds.
#[must_use]
pub fn ladder(count: usize) -> Vec<f32> {
    crate::eq::band_table(count).map_or_else(
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

/// A music preset read into the window's controls on `ladder`, the live band ladder — the one
/// reading of a `.fac` there is: the application's lane and an application's route both go
/// through it, so a preset sounds the same on either.
///
/// The effects land where the slider shows them, which is where they sound: a Dynamic Boost past
/// the slider's dead top is read as the top ([`scale::value_to_slider_for`]). The curve is the
/// preset's own when it has as many bands as the ladder, fitted onto the ladder by frequency when
/// it has another count ([`crate::eq::fit_preset_gains`], audit #13), and flat with the
/// equalizer on when it has none, the original's "old preset".
///
/// A twenty-band curve on the Windows ladder — a Windows preset, or one saved before 0.4.0 — is
/// read on the half-octave ladder that replaced it, band for band (0.4.0 audit R4,
/// [`fxsound_core::eq::move_off_the_windows_twenty_band_ladder`]), so it plays without the
/// paired ladder's ripple and is saved on the new one the next time it is saved.
#[must_use]
pub fn music_controls(preset: &Preset, ladder: &[f32]) -> MusicControls {
    let effects =
        Effect::ALL.map(|effect| scale::value_to_slider_for(effect, preset.effect(effect)));
    let mut preset_bands = preset.eq_bands.clone();
    fxsound_core::eq::move_off_the_windows_twenty_band_ladder(&mut preset_bands);
    let (eq_on, eq_bands) = if preset_bands.is_empty() {
        (true, bands_of(ladder, &vec![0.0; ladder.len()]))
    } else if preset_bands.len() == ladder.len() {
        (preset.eq_on, preset_bands)
    } else {
        let centres: Vec<f32> = preset_bands.iter().map(|b| b.center_hz).collect();
        let gains: Vec<f32> = preset_bands.iter().map(|b| b.boost_db).collect();
        let fitted = crate::eq::fit_preset_gains(&centres, &gains, ladder);
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

/// Map a music chain's controls onto its snapshot: the effects from their slider positions, the
/// equalizer, and the shared levels. Everything else in `params` is left as it is — the power and
/// the mute are the whole application's.
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
            scale::slider_to_value_for(effect, effects[effect as usize]),
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
/// application's tests hold it to that. `ladder` is the user's band count's: [`ladder`] of
/// `settings.num_bands`.
#[must_use]
pub fn preset_params(preset: &Preset, ladder: &[f32], levels: MusicLevels) -> DspParams {
    let MusicControls {
        effects,
        eq_on,
        eq_bands,
    } = music_controls(preset, ladder);
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
    fn the_windows_dsp_changes_nothing_else_a_preset_is_read_into() {
        let bands: Vec<(f32, f32)> = ladder(20).iter().map(|&hz| (hz, 4.0)).collect();
        let preset = preset([30, 60, 90, 120, 10], &bands);
        for count in [10, 20, 31] {
            let linux = preset_params(&preset, &ladder(count), MusicLevels::default());
            let windows = preset_params(
                &preset,
                &ladder(count),
                MusicLevels::default().with_windows_dsp(true),
            );
            assert_eq!(windows.compat, DspCompat::Windows);
            assert_eq!(
                DspParams {
                    compat: DspCompat::Linux,
                    ..windows
                },
                linux,
                "{count} bands"
            );
            assert_eq!(
                MusicLevels::default()
                    .with_windows_dsp(true)
                    .with_windows_dsp(false),
                MusicLevels::default()
            );
        }
    }
}
