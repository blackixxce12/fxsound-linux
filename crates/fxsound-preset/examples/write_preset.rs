//! Writes `.fac` files through this crate's own writer, so a hand-authored preset is byte-identical
//! in format to the ones that ship.
//!
//! Hand-editing the text is how two bands ended up one hertz apart in three shipped files, and how
//! eight presets came to store an effect amount the engine bypasses. Going through `Preset` means
//! the band count, the app-dependent integers and the bypass flags are all derived rather than
//! typed, every file round-trips before it is written, and the result is checked against the same
//! rules as everything already in the directory.
//!
//! ```text
//! cargo run -p fxsound-preset --example write_preset -- <out dir> [spec.json]
//! ```
//!
//! With no spec file it writes the three presets this port added. With one, it writes whatever the
//! file describes:
//!
//! ```json
//! [{ "name": "Jazz",
//!    "sliders": { "clarity": 2, "ambience": 0, "surround": 1, "dynamic_boost": 3, "bass": 2 },
//!    "bands": [[62.5, -1.0], [120, 0.0], ...] }]
//! ```
//!
//! `sliders` are the GUI's own 0..=10 positions, not MIDI, because that is the only scale a person
//! can reason about and the only one the interface can reproduce.

use fxsound_core::{Effect, EqBand, Preset, scale};
use serde::Deserialize;

#[derive(Deserialize)]
struct Sliders {
    clarity: f32,
    ambience: f32,
    surround: f32,
    dynamic_boost: f32,
    bass: f32,
}

#[derive(Deserialize)]
struct Spec {
    name: String,
    sliders: Sliders,
    /// Exactly ten `[hz, db]` pairs.
    bands: Vec<[f32; 2]>,
}

/// Below this stored value the Ambience stage never turns on: `(int)(midi * 0.34) > 12`.
const AMBIENCE_MIN_AUDIBLE_MIDI: u8 = 39;
/// The lowest stored value that already reaches Dynamic Boost's ceiling.
///
/// Measured: slider position 6 stores 76 and gives +11.60 dB, and positions 7, 8, 9 and 10 give
/// exactly the same. So 76 is the honest spelling of "as much as this effect has"; anything above
/// it is a number chosen to look larger.
const DYNAMIC_BOOST_CEILING_MIDI: u8 = 76;

fn built_in() -> Vec<Spec> {
    let s = |clarity, ambience, surround, dynamic_boost, bass| Sliders {
        clarity,
        ambience,
        surround,
        dynamic_boost,
        bass,
    };
    vec![
        // Nothing on. The one thing the shipped set could not do: hear the engine with every
        // effect at rest. Not bit-transparent, and the description must not claim it is — Dynamic
        // Boost is never bypassed, so the −0.3 dBFS ceiling still applies.
        Spec {
            name: "Flat".into(),
            sliders: s(0.0, 0.0, 0.0, 0.0, 0.0),
            bands: vec![
                [62.5, 0.0],
                [115.734, 0.0],
                [214.311, 0.0],
                [396.85, 0.0],
                [734.867, 0.0],
                [1360.79, 0.0],
                [2519.84, 0.0],
                [4666.12, 0.0],
                [8640.48, 0.0],
                [16000.0, 0.0],
            ],
        },
        // A laptop's speakers have no bottom octave and a cabinet resonance where their body
        // should be. Bass Boost is deliberately 0: that effect is a fixed bell at 90 Hz, which is
        // precisely the region these speakers cannot reproduce, so asking for it spends excursion
        // on nothing. The top band sits at 11 kHz rather than 16 kHz because a band is bypassed
        // once twice its centre reaches the sample rate, and a 16 kHz band dies silently at 32 kHz.
        Spec {
            name: "Laptop & Small Speakers".into(),
            // Dynamic Boost 8 rather than 4: the design asked for about +8 dB, and since the
            // slider was respread over the range the effect responds to, that is where +8 dB now
            // lives. The number moved; the gain did not.
            sliders: s(3.0, 0.0, 0.0, 8.0, 0.0),
            bands: vec![
                [80.0, -8.0],
                [150.0, -4.0],
                [320.0, 3.0],
                [620.0, 0.0],
                [900.0, 1.0],
                [1500.0, 0.0],
                [2500.0, 2.0],
                [4000.0, 0.0],
                [6000.0, 1.0],
                [11000.0, -2.0],
            ],
        },
        // Footsteps and reloads live between 1.5 and 5 kHz; what masks them is the rumble below
        // 200 Hz a game mixes in for weight. Surround Sound is 0 and that is load-bearing: the
        // widener decorrelates the stereo image, which is the exact cue this preset sharpens.
        Spec {
            // Dynamic Boost at the top. That used to be a lie — positions 6 through 10 were one
            // setting — and is now the strongest thing the effect has, +11.60 dB, which is what a
            // preset built around hearing quiet cues wants.
            name: "Competitive FPS".into(),
            sliders: s(4.0, 0.0, 0.0, 10.0, 0.0),
            bands: vec![
                [60.0, -8.0],
                [120.0, -6.0],
                [220.0, -3.0],
                [400.0, 0.0],
                [700.0, 0.0],
                [1400.0, 2.0],
                [2500.0, 4.0],
                [4000.0, 3.0],
                [6500.0, 0.0],
                [12000.0, -2.0],
            ],
        },
    ]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .expect("usage: write_preset <out dir> [spec.json]");
    let dir = std::path::Path::new(&out);

    let specs: Vec<Spec> = match args.next() {
        Some(path) => {
            let text = std::fs::read_to_string(&path).expect("read the spec");
            serde_json::from_str(&text).expect("parse the spec")
        }
        None => built_in(),
    };

    for spec in &specs {
        assert_eq!(
            spec.bands.len(),
            10,
            "{}: {} bands, the format carries ten",
            spec.name,
            spec.bands.len()
        );

        let mut preset = Preset {
            name: spec.name.clone(),
            ..Preset::default()
        };
        // `set_effect` keeps the bypass flag in step with the value the way the original does, so
        // neither has to be written by hand.
        let sliders = [
            spec.sliders.clarity,
            spec.sliders.ambience,
            spec.sliders.surround,
            spec.sliders.dynamic_boost,
            spec.sliders.bass,
        ];
        // `Effect::ALL` is GUI order: Fidelity/Clarity, Ambience, Surround, Dynamic Boost, Bass.
        for (effect, slider) in Effect::ALL.into_iter().zip(sliders) {
            preset.set_effect(effect, scale::slider_to_value_for(effect, slider));
        }

        // Two amounts that look like settings but are not. Caught here rather than by the test
        // suite, so the person writing the numbers hears about it while they still remember why
        // they chose them.
        let ambience = preset.main_midi[Effect::Ambience.vals_index()];
        assert!(
            ambience == 0 || ambience >= AMBIENCE_MIN_AUDIBLE_MIDI,
            "{}: Ambience {ambience} is below the {AMBIENCE_MIN_AUDIBLE_MIDI} the stage needs — \
             either commit to it or set it to zero",
            spec.name
        );
        let boost = preset.main_midi[Effect::DynamicBoost.vals_index()];
        assert!(
            boost <= DYNAMIC_BOOST_CEILING_MIDI,
            "{}: Dynamic Boost {boost} is past the {DYNAMIC_BOOST_CEILING_MIDI} that already \
             reaches this effect's ceiling — the extra is a number chosen to look larger",
            spec.name
        );

        let mut sorted = spec.bands.clone();
        sorted.sort_by(|a, b| a[0].partial_cmp(&b[0]).expect("finite centres"));
        for pair in sorted.windows(2) {
            assert!(
                pair[1][0] / pair[0][0] > 1.05,
                "{}: bands at {} Hz and {} Hz are close enough to add rather than act \
                 independently — three shipped presets got this wrong and delivered nearly double",
                spec.name,
                pair[0][0],
                pair[1][0]
            );
        }

        preset.eq_bands = sorted.iter().map(|&[hz, db]| EqBand::new(hz, db)).collect();
        preset.eq_on = true;

        let path = dir.join(format!("{}.fac", spec.name));
        fxsound_preset::save(&preset, &path).expect("write the preset");
        let back = fxsound_preset::load(&path).expect("read it back");
        assert_eq!(back, preset, "{} did not round-trip", spec.name);

        println!(
            "{:<26} Fid {:>3} Amb {:>3} Sur {:>3} DynB {:>3} Bass {:>3}",
            spec.name,
            preset.main_midi[Effect::Fidelity.vals_index()],
            ambience,
            preset.main_midi[Effect::Surround.vals_index()],
            boost,
            preset.main_midi[Effect::Bass.vals_index()],
        );
    }
    println!("{} preset(s) written to {}", specs.len(), dir.display());
}
