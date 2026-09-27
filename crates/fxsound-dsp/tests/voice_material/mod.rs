//! The voice material `voice_presets.rs` probes the microphone chain with, and the chain a voice
//! preset describes, shared with `preset_drift.rs` so that the drift harness measures every voice
//! preset on exactly the signals the voice-preset checks use.
//!
//! Not a test target: Cargo only builds `tests/*.rs` and `tests/*/main.rs` as tests, so this
//! directory is a module each of those files pulls in with `mod voice_material;`.

use fxsound_dsp::ChainSpec;
use fxsound_preset::input::InputPreset;

/// The rate the capture stream asks for, and therefore the rate every preset is voiced at.
pub const CAPTURE_RATE: f32 = 48_000.0;

/// The chain a preset describes, with every stage it asks for, in the order it names.
///
/// Through [`fxsound_dsp::InputChain::apply`], which is the one place a snapshot becomes stage
/// settings: 0.3.0 kept a second copy of that mapping here, and a field added to one and not the
/// other would have been a preset whose sound this bench could not measure.
pub fn chain_for(preset: &InputPreset) -> fxsound_dsp::InputChain {
    let spec = ChainSpec::by_name(&preset.chain)
        .unwrap_or_else(|| panic!("{}: unknown chain {:?}", preset.name, preset.chain));
    let mut chain = fxsound_dsp::InputChain::from_spec(spec, CAPTURE_RATE);
    chain.apply(&preset.to_params());
    chain
}

/// Deterministic noise, so every run measures the same signal.
pub fn noise(frames: usize, amplitude: f32) -> Vec<f32> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    (0..frames)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 40) as f32 / 8_388_608.0 - 1.0) * amplitude
        })
        .collect()
}

/// A stand-in for a voice: a fundamental with harmonics, under a syllabic envelope.
///
/// The envelope is not decoration. Without it the probe is a steady tone whose peak and RMS are a
/// few decibels apart, and a compressor detecting peak looks exactly like one detecting RMS —
/// which made this test refuse two presets that differ only in their detector, the one field the
/// research insisted on because peak and RMS against the same threshold differ by three to seven
/// decibels *on speech*. Speech has a crest factor near twelve decibels because it starts and
/// stops; a probe for a voice chain has to as well.
pub fn speech_like(amplitude: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| {
            let t = n as f32 / CAPTURE_RATE;
            let f = 130.0;
            let body = (t * std::f32::consts::TAU * f).sin() * 0.6
                + (t * std::f32::consts::TAU * f * 2.0).sin() * 0.3
                + (t * std::f32::consts::TAU * f * 5.0).sin() * 0.2
                + (t * std::f32::consts::TAU * f * 11.0).sin() * 0.1;
            // Four syllables a second, each with a quiet tail — roughly a talker's rhythm.
            let syllable = (t * 4.0).fract();
            let envelope = if syllable < 0.55 {
                (syllable / 0.55 * std::f32::consts::PI).sin().powf(0.6)
            } else {
                0.02
            };
            body * envelope * amplitude / 1.2
        })
        .collect()
}

/// What a desk microphone in a room with a computer in it actually picks up: mains hum with
/// broadband hiss over it.
pub fn room_floor(amplitude: f32, frames: usize) -> Vec<f32> {
    noise(frames, amplitude * 0.4)
        .iter()
        .enumerate()
        .map(|(n, hiss)| {
            let t = n as f32 / CAPTURE_RATE;
            hiss + (t * std::f32::consts::TAU * 50.0).sin() * amplitude
        })
        .collect()
}

/// Energy where sibilance lives, so the de-esser has something to act on.
pub fn sibilance(amplitude: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| {
            let t = n as f32 / CAPTURE_RATE;
            ((t * std::f32::consts::TAU * 6_500.0).sin()
                + (t * std::f32::consts::TAU * 7_900.0).sin()
                + (t * std::f32::consts::TAU * 9_100.0).sin())
                * amplitude
                / 3.0
        })
        .collect()
}
