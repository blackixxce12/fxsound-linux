//! The 0.3.0 voice chain, frozen.
//!
//! 0.4.0 rebuilt [`fxsound_dsp::InputChain`] as a list of stages built from a spec. The
//! `voice` spec is *defined* as the 0.3.0 chain, and the only proof worth having that a refactor
//! of a signal path changed nothing is the signal: these numbers were captured from the 0.3.0
//! implementation before the refactor began, and the rebuilt chain has to reproduce them.
//!
//! Every window's RMS and peak, on a fixture with a voice, a room floor and a sibilant in it, at
//! two rates and two parameter sets, mono and stereo. RMS and peak rather than a sample dump
//! because a dump is unreadable when it fails; the tolerance is tight enough that a change of one
//! coefficient anywhere in the chain fails it.
//!
//! The denoiser is off in every fixture here, deliberately: its 0.4.0 control surface changes its
//! output by design, and that change is measured in its own module.

use fxsound_core::Detection;
use fxsound_core::messages::InputDspParams;
use fxsound_dsp::InputEngine;

/// One frame of the fixture, deterministic. A talker (harmonics under a syllabic envelope), the
/// room (hum under hiss) and a sibilant burst every half second.
fn fixture(rate: f32, channels: usize, frames: usize) -> Vec<f32> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut out = Vec::with_capacity(frames * channels);
    for n in 0..frames {
        let t = n as f32 / rate;
        let f = 130.0;
        let body = (t * std::f32::consts::TAU * f).sin() * 0.6
            + (t * std::f32::consts::TAU * f * 2.0).sin() * 0.3
            + (t * std::f32::consts::TAU * f * 5.0).sin() * 0.2
            + (t * std::f32::consts::TAU * f * 11.0).sin() * 0.1;
        let syllable = (t * 4.0).fract();
        let envelope = if syllable < 0.55 {
            (syllable / 0.55 * std::f32::consts::PI).sin().powf(0.6)
        } else {
            0.02
        };
        let voice = body * envelope * 0.25;
        let sibilant = if (t * 2.0).fract() < 0.12 {
            ((t * std::f32::consts::TAU * 6_500.0).sin()
                + (t * std::f32::consts::TAU * 7_900.0).sin())
                * 0.08
        } else {
            0.0
        };
        let hum = (t * std::f32::consts::TAU * 50.0).sin() * 0.004;
        for channel in 0..channels {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let hiss = ((state >> 40) as f32 / 8_388_608.0 - 1.0) * 0.002;
            // The right channel is the same talker a little further from the microphone.
            let weight = if channel == 0 { 1.0 } else { 0.7 };
            out.push(voice * weight + sibilant * weight + hum + hiss);
        }
    }
    out
}

/// Clean Voice — the defaults — and a second set that exercises the other order of high-pass,
/// the other detector, and a moved de-esser.
fn parameter_sets() -> [InputDspParams; 2] {
    let mut second = InputDspParams {
        highpass_hz: 120.0,
        highpass_order: 4,
        gate_threshold_db: -40.0,
        gate_ratio: 3.0,
        gate_range_db: -20.0,
        gate_hold_ms: 50.0,
        gate_detection: Detection::Peak,
        deesser_hz: 6_000.0,
        deesser_threshold_db: -25.0,
        compressor_threshold_db: -24.0,
        compressor_ratio: 4.0,
        compressor_attack_ms: 10.0,
        compressor_detection: Detection::Peak,
        makeup_db: 3.0,
        ceiling_db: -1.0,
        filter_q: 1.2,
        ..InputDspParams::default()
    };
    second.band_boost_db[2] = -2.0;
    second.band_boost_db[6] = 2.5;
    second.band_boost_db[8] = 1.5;
    second.sanitise();
    [InputDspParams::default(), second]
}

/// RMS and peak of every 100 ms window of channel `channel`.
fn measure(rate: f32, channels: usize, params: &InputDspParams, channel: usize) -> Vec<(f32, f32)> {
    const BLOCK: usize = 1_024;
    let frames = rate as usize;
    let mut signal = fixture(rate, channels, frames);
    let mut engine = InputEngine::new(rate, BLOCK, channels);
    engine.apply(params);
    for block in signal.chunks_mut(BLOCK * channels) {
        engine.process(block, channels);
    }
    let window = frames / 10;
    (0..10)
        .map(|w| {
            let samples: Vec<f32> = signal
                .iter()
                .skip(channel)
                .step_by(channels)
                .skip(w * window)
                .take(window)
                .copied()
                .collect();
            let rms = (samples.iter().map(|x| x * x).sum::<f32>() / window as f32).sqrt();
            let peak = samples.iter().fold(0.0_f32, |a, b| a.max(b.abs()));
            (rms, peak)
        })
        .collect()
}

fn check(name: &str, got: &[(f32, f32)], want: &[(f32, f32)]) {
    assert_eq!(got.len(), want.len(), "{name}: window count");
    for (w, ((rms, peak), (want_rms, want_peak))) in got.iter().zip(want).enumerate() {
        let rms_ok = (rms - want_rms).abs() <= want_rms.abs() * 2.0e-5 + 1.0e-7;
        let peak_ok = (peak - want_peak).abs() <= want_peak.abs() * 2.0e-5 + 1.0e-7;
        assert!(
            rms_ok && peak_ok,
            "{name} window {w}: rms {rms:.7} peak {peak:.7}, the 0.3.0 chain gave rms \
             {want_rms:.7} peak {want_peak:.7}\n  whole run: {got:?}"
        );
    }
}

#[test]
fn the_voice_spec_is_the_chain_0_3_0_shipped() {
    let sets = parameter_sets();
    for fixture in FROZEN {
        let got = measure(
            fixture.rate,
            fixture.channels,
            &sets[fixture.set],
            fixture.channel,
        );
        check(fixture.name, &got, fixture.want);
    }
}

/// One row of the fixture: which signal, through which parameter set, and what 0.3.0 measured
/// on it.
struct Fixture {
    name: &'static str,
    rate: f32,
    channels: usize,
    /// Index into [`parameter_sets`].
    set: usize,
    /// The channel measured.
    channel: usize,
    /// `(rms, peak)` of each 100 ms window. Written as the shortest digits that read back to the
    /// f32 measured, so the literal *is* the number and not one rounded on the way in.
    want: &'static [(f32, f32)],
}

/// Captured from the 0.3.0 implementation. Do not regenerate these from the current code without
/// saying why in the commit: the point of the test is that they *cannot* be regenerated.
const FROZEN: &[Fixture] = &[
    Fixture {
        name: "clean voice, 48 kHz, mono",
        rate: 48_000.0,
        channels: 1,
        set: 0,
        channel: 0,
        want: &[
            (0.22250324, 0.70666546),
            (0.08682354, 0.40037364),
            (0.10651136, 0.43284184),
            (0.18100421, 0.48907238),
            (0.0055342237, 0.017606888),
            (0.22146547, 0.7067135),
            (0.08675135, 0.39879978),
            (0.10648105, 0.43276837),
            (0.18094262, 0.49259058),
            (0.005536448, 0.016323047),
        ],
    },
    Fixture {
        name: "second set, 48 kHz, mono",
        rate: 48_000.0,
        channels: 1,
        set: 1,
        channel: 0,
        want: &[
            (0.06175819, 0.23854743),
            (0.018596249, 0.085943386),
            (0.036395278, 0.15241495),
            (0.048723403, 0.15221816),
            (0.0019411583, 0.00662185),
            (0.057016417, 0.19658619),
            (0.018548826, 0.08617496),
            (0.036490053, 0.15187079),
            (0.04872253, 0.15112144),
            (0.0019411171, 0.0062553864),
        ],
    },
    Fixture {
        name: "clean voice, 48 kHz, stereo, right",
        rate: 48_000.0,
        channels: 2,
        set: 0,
        channel: 1,
        want: &[
            (0.16179076, 0.5382504),
            (0.06466748, 0.30144593),
            (0.07601237, 0.30959177),
            (0.13073455, 0.35177583),
            (0.0045247185, 0.015093633),
            (0.16174579, 0.5423881),
            (0.064677045, 0.30052653),
            (0.07605441, 0.30904055),
            (0.13074411, 0.35276315),
            (0.0045562307, 0.015741553),
        ],
    },
    Fixture {
        name: "second set, 44.1 kHz, stereo, left",
        rate: 44_100.0,
        channels: 2,
        set: 1,
        channel: 0,
        want: &[
            (0.061701275, 0.23824038),
            (0.01861638, 0.086339794),
            (0.036419712, 0.15258662),
            (0.048704796, 0.1519484),
            (0.0019489499, 0.0063756644),
            (0.056987368, 0.1982838),
            (0.018618004, 0.08618469),
            (0.036402613, 0.15216196),
            (0.04870999, 0.15139058),
            (0.001945186, 0.006386901),
        ],
    },
    Fixture {
        name: "clean voice, 16 kHz, mono",
        rate: 16_000.0,
        channels: 1,
        set: 0,
        channel: 0,
        want: &[
            (0.22314739, 0.69966376),
            (0.08697265, 0.39720392),
            (0.10646302, 0.43307436),
            (0.1808926, 0.48611856),
            (0.0054854816, 0.01493543),
            (0.2218455, 0.6995022),
            (0.08676658, 0.3984363),
            (0.10647797, 0.4345128),
            (0.1809831, 0.48753074),
            (0.005513083, 0.015589758),
        ],
    },
];

#[test]
#[ignore = "prints the fixture's numbers so FROZEN can be written down once"]
fn print_the_fixture() {
    let sets = parameter_sets();
    for fixture in FROZEN {
        let got = measure(
            fixture.rate,
            fixture.channels,
            &sets[fixture.set],
            fixture.channel,
        );
        // `{:?}` prints the shortest digits that read back to the same f32: the literal written
        // down is the number measured, and not one that clippy would call rounded.
        let rows: Vec<String> = got
            .iter()
            .map(|(rms, peak)| format!("({rms:?}, {peak:?})"))
            .collect();
        println!(
            "Fixture {{ name: \"{}\", rate: {:?}, channels: {}, set: {}, channel: {}, want: &[{}] }},",
            fixture.name,
            fixture.rate,
            fixture.channels,
            fixture.set,
            fixture.channel,
            rows.join(", ")
        );
    }
}
