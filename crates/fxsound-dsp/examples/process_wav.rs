//! Run a WAV file through the FxSound engine offline.
//!
//! This exists so the DSP can be listened to and measured without a sound server in the loop — it
//! is the quickest way to tell whether a change to a filter actually did what you meant. It also
//! doubles as a worked example of driving [`fxsound_dsp::Engine`].
//!
//! ```text
//! cargo run -p fxsound-dsp --example process_wav -- in.wav out.wav \
//!     --preset assets/presets/BonusPresets/Jazz.fac
//! cargo run -p fxsound-dsp --example process_wav -- in.wav out.wav \
//!     --bass 10 --fidelity 6 --master-gain -3
//! ```
//!
//! Only 16-bit PCM WAV is handled, which is what `.wav` almost always means and what the original
//! engine's `processAudio` took.

use fxsound_dsp::Engine;
use fxsound_core::{Effect, messages::DspParams};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!(
            "usage: process_wav <in.wav> <out.wav> [--preset FILE] \
             [--fidelity N] [--ambience N] [--surround N] [--dynamic-boost N] [--bass N] \
             [--master-gain DB] [--balance DB] [--no-eq]"
        );
        std::process::exit(2);
    };

    let mut params = DspParams::default();
    let rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        let flag = rest[i].as_str();
        let mut value = || -> Result<f32, Box<dyn std::error::Error>> {
            i += 1;
            Ok(rest
                .get(i)
                .ok_or_else(|| format!("{flag} needs a value"))?
                .parse()?)
        };
        match flag {
            // The command line takes the GUI's 0..10 scale, like the original's --fidelity.
            "--fidelity" => params.set_effect(Effect::Fidelity, value()? / 10.0),
            "--ambience" => params.set_effect(Effect::Ambience, value()? / 10.0),
            "--surround" => params.set_effect(Effect::Surround, value()? / 10.0),
            "--dynamic-boost" => params.set_effect(Effect::DynamicBoost, value()? / 10.0),
            "--bass" => params.set_effect(Effect::Bass, value()? / 10.0),
            "--master-gain" => params.master_gain_db = value()?,
            "--balance" => params.balance = value()?,
            "--no-eq" => params.eq_on = false,
            "--preset" => {
                i += 1;
                let path = rest.get(i).ok_or("--preset needs a path")?;
                apply_preset(&mut params, Path::new(path))?;
            }
            other => return Err(format!("unknown option {other}").into()),
        }
        i += 1;
    }

    let wav = Wav::read(Path::new(&input))?;
    println!(
        "in:  {} Hz, {} ch, {} frames ({:.2} s)",
        wav.sample_rate,
        wav.channels,
        wav.samples.len() / wav.channels as usize,
        wav.samples.len() as f32 / wav.channels as f32 / wav.sample_rate as f32
    );

    const BLOCK: usize = 1024;
    let mut engine = Engine::new(wav.sample_rate as f32, BLOCK, wav.channels as usize);
    engine.apply(&params);
    println!("latency: {} frames", engine.latency_frames());

    // Convert to f32 in the ±1.0 range the engine works in, process block by block the way a real
    // audio callback would, then convert back.
    let mut floats: Vec<f32> = wav.samples.iter().map(|s| f32::from(*s) / 32768.0).collect();
    let block_samples = BLOCK * wav.channels as usize;
    for block in floats.chunks_mut(block_samples) {
        engine.process(block, wav.channels as usize);
    }

    let meters = engine.meters();
    println!(
        "out: peak L {:.3} R {:.3}, {} frames processed",
        meters.peak_left, meters.peak_right, meters.processed_samples
    );

    let clipped = floats.iter().filter(|s| s.abs() > 1.0).count();
    if clipped > 0 {
        println!("warning: {clipped} samples exceeded full scale and were clamped");
    }

    let samples: Vec<i16> = floats
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect();
    Wav {
        sample_rate: wav.sample_rate,
        channels: wav.channels,
        samples,
    }
    .write(Path::new(&output))?;
    println!("wrote {output}");
    Ok(())
}

fn apply_preset(params: &mut DspParams, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let preset = fxsound_preset::parse(&bytes)?;
    println!("preset: {}", preset.name);
    for effect in Effect::ALL {
        params.set_effect(effect, preset.effect(effect));
    }
    params.set_bands(&preset.eq_bands);
    params.eq_on = preset.eq_on;
    Ok(())
}

/// The smallest WAV reader/writer that covers 16-bit PCM.
struct Wav {
    sample_rate: u32,
    channels: u16,
    /// Interleaved.
    samples: Vec<i16>,
}

impl Wav {
    fn read(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let bytes = std::fs::read(path)?;
        if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err("not a RIFF/WAVE file".into());
        }

        let mut channels = 0_u16;
        let mut sample_rate = 0_u32;
        let mut bits = 0_u16;
        let mut data: Option<&[u8]> = None;

        // Walk the chunk list rather than assuming the canonical 44-byte header: real files often
        // carry LIST/fact chunks before the data.
        let mut pos = 12;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
            let body_start = pos + 8;
            let body_end = (body_start + size).min(bytes.len());

            match id {
                b"fmt " if size >= 16 => {
                    let body = &bytes[body_start..body_end];
                    channels = u16::from_le_bytes(body[2..4].try_into()?);
                    sample_rate = u32::from_le_bytes(body[4..8].try_into()?);
                    bits = u16::from_le_bytes(body[14..16].try_into()?);
                }
                b"data" => data = Some(&bytes[body_start..body_end]),
                _ => {}
            }
            // Chunks are word-aligned.
            pos = body_start + size + (size & 1);
        }

        let data = data.ok_or("no data chunk")?;
        if bits != 16 {
            return Err(format!("only 16-bit PCM is supported, this file is {bits}-bit").into());
        }
        if channels == 0 {
            return Err("no fmt chunk".into());
        }

        let samples = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        Ok(Self {
            sample_rate,
            channels,
            samples,
        })
    }

    fn write(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let data_len = (self.samples.len() * 2) as u32;
        let byte_rate = self.sample_rate * u32::from(self.channels) * 2;
        let block_align = self.channels * 2;

        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16_u32.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        out.extend_from_slice(&self.channels.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16_u16.to_le_bytes()); // bits per sample
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for sample in &self.samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, out)?;
        Ok(())
    }
}
