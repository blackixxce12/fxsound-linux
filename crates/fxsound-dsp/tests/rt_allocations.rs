//! Nothing on the audio path allocates, held by a counting allocator.
//!
//! The crate's contract — "nothing in this crate allocates, locks or blocks once constructed" —
//! was a comment in 0.3.0, and one path broke it unseen: `Denoiser::reset` rebuilt eight network
//! states on the PipeWire thread on every preset change. A global allocator that counts is the
//! only way to hold the contract rather than state it, and a global allocator needs `unsafe`,
//! which the library forbids; so the count lives here, in a test binary of its own, and every
//! stage is driven through the public API.
//!
//! One allocation is documented and allowed for: the FFT library the denoiser is built on plans
//! its transform through a thread-local cache on first use, per thread and per size. Every path
//! is therefore warmed once before the count starts.

use fxsound_core::messages::{DspEvent, InputDspParams};
use fxsound_core::{DenoiseChannelMode, DenoiseLevel, DereverbLevel};
use fxsound_dsp::input::dereverb::Dereverb;
use fxsound_dsp::input::{Denoiser, InputChain};
use fxsound_dsp::{ChainSpec, InputEngine};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every method forwards to the system allocator unchanged; the count is a thread-local
// `Cell` with a `const` initialiser, which neither allocates nor runs a destructor.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations on this thread so far.
fn allocations() -> u64 {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

/// Runs `work` and returns how many allocations it made.
fn allocations_during(mut work: impl FnMut()) -> u64 {
    let before = allocations();
    work();
    allocations() - before
}

const FS: f32 = 48_000.0;

fn stereo_fixture(frames: usize) -> Vec<f32> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut out = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let t = n as f32 / FS;
        let voice = (t * std::f32::consts::TAU * 130.0).sin() * 0.3
            + (t * std::f32::consts::TAU * 390.0).sin() * 0.1;
        let hum = (t * std::f32::consts::TAU * 50.0).sin() * 0.03;
        for _ in 0..2 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let hiss = ((state >> 40) as f32 / 8_388_608.0 - 1.0) * 0.02;
            out.push(voice + hum + hiss);
        }
    }
    out
}

#[test]
fn the_counter_counts() {
    let n = allocations_during(|| {
        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(v);
    });
    assert!(n >= 1, "a Vec was built and nothing was counted");
}

#[test]
fn the_denoiser_allocates_nothing_after_its_first_frame_in_any_mode() {
    let mut d = Denoiser::new(FS);
    d.set_enabled(true);
    let input = stereo_fixture(480 * 8);
    let mut block = input.clone();
    // Warm every mode: the library's FFT plan is a thread-local, built on first use.
    for mode in DenoiseChannelMode::ALL {
        d.set_channels(mode);
        block.copy_from_slice(&input);
        d.process(&mut block, 2);
    }

    let n = allocations_during(|| {
        for mode in DenoiseChannelMode::ALL {
            d.set_channels(mode);
            for level in [
                DenoiseLevel::Light,
                DenoiseLevel::Strong,
                DenoiseLevel::Medium,
            ] {
                d.set_level(level);
                d.set_control(level.control());
            }
            block.copy_from_slice(&input);
            d.process(&mut block, 2);
            d.reset();
            block.copy_from_slice(&input);
            d.process(&mut block, 2);
            d.set_enabled(false);
            block.copy_from_slice(&input);
            d.process(&mut block, 2);
            d.set_enabled(true);
            block.copy_from_slice(&input);
            d.process(&mut block, 2);
        }
    });
    assert_eq!(n, 0, "the denoiser allocated {n} times on the audio path");
}

#[test]
fn the_de_reverb_allocates_nothing_after_construction() {
    let mut d = Dereverb::new(FS);
    d.set_level(DereverbLevel::Medium);
    let input = stereo_fixture(480 * 8);
    let mut block = input.clone();
    d.process(&mut block, 2);
    let n = allocations_during(|| {
        for level in [
            DereverbLevel::Light,
            DereverbLevel::Strong,
            DereverbLevel::Medium,
        ] {
            d.set_level(level);
            block.copy_from_slice(&input);
            d.process(&mut block, 2);
            d.reset();
            block.copy_from_slice(&input);
            d.process(&mut block, 2);
        }
    });
    assert_eq!(n, 0, "the de-reverb allocated {n} times on the audio path");
}

#[test]
fn every_chain_spec_processes_and_resets_without_allocating() {
    let mut params = InputDspParams {
        rnnoise: true,
        denoise_level: DenoiseLevel::Medium,
        denoise_control: DenoiseLevel::Medium.control(),
        dereverb: DereverbLevel::Light,
        vad_gate: true,
        ..InputDspParams::default()
    };
    params.sanitise();
    let input = stereo_fixture(480 * 8);
    let mut block = input.clone();

    for name in ChainSpec::NAMES {
        let spec = ChainSpec::by_name(name).expect(name);
        let mut chain = InputChain::from_spec(spec, FS);
        chain.apply(&params);
        block.copy_from_slice(&input);
        chain.process(&mut block, 2);

        let mut other = params;
        other.gate_threshold_db = -40.0;
        other.denoise_level = DenoiseLevel::Strong;
        other.denoise_control = DenoiseLevel::Strong.control();
        other.denoise_channels = DenoiseChannelMode::Linked;
        other.sanitise();

        let n = allocations_during(|| {
            chain.apply(&other);
            block.copy_from_slice(&input);
            chain.process(&mut block, 2);
            chain.reset();
            chain.apply(&params);
            block.copy_from_slice(&input);
            chain.process(&mut block, 2);
            chain.set_power(false);
            chain.set_power(true);
            block.copy_from_slice(&input);
            chain.process(&mut block, 2);
        });
        assert_eq!(
            n, 0,
            "the {name} chain allocated {n} times on the audio path"
        );
    }
}

#[test]
fn the_engine_processes_applies_and_handles_events_without_allocating() {
    let mut engine = InputEngine::new(FS, 1_024, 2);
    let mut params = InputDspParams {
        rnnoise: true,
        dereverb: DereverbLevel::Medium,
        ..InputDspParams::default()
    };
    params.sanitise();
    engine.apply(&params);
    let input = stereo_fixture(1_024 * 8);
    let mut block = input.clone();
    for chunk in block.chunks_mut(1_024 * 2) {
        engine.process(chunk, 2);
    }
    let mut other = params;
    other.makeup_db = 3.0;
    other.denoise_level = DenoiseLevel::Light;
    other.denoise_control = DenoiseLevel::Light.control();
    other.sanitise();

    let n = allocations_during(|| {
        engine.apply(&other);
        block.copy_from_slice(&input);
        for chunk in block.chunks_mut(1_024 * 2) {
            engine.process(chunk, 2);
        }
        for event in [
            DspEvent::ResetFilterState,
            DspEvent::ResetSpectrum,
            DspEvent::ResetProcessedTime,
            DspEvent::ResetCaptureStats,
        ] {
            engine.handle_event(event);
        }
        block.copy_from_slice(&input);
        for chunk in block.chunks_mut(1_024 * 2) {
            engine.process(chunk, 2);
        }
        let _ = std::hint::black_box(engine.meters());
        let _ = std::hint::black_box(engine.latency_frames());
    });
    assert_eq!(n, 0, "the engine allocated {n} times on the audio path");
}
