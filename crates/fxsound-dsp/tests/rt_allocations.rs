//! Nothing on the audio path allocates, held by a counting allocator.
//!
//! The crate's contract — "nothing in this crate allocates, locks or blocks once constructed" —
//! was a comment in 0.3.0, and one path broke it unseen: `Denoiser::reset` rebuilt eight network
//! states on the PipeWire thread on every preset change. A global allocator that counts is the
//! only way to hold the contract rather than state it, and a global allocator needs `unsafe`,
//! which the library forbids; so the count lives here, in a test binary of its own, and every
//! stage is driven through the public API.
//!
//! Counted from the first block, on a thread that has never processed one. 0.4.0's first cut
//! warmed every path before it started counting, because the denoiser's FFT library planned its
//! transform through a per-thread cache the first time a thread used it — which, in production, is
//! PipeWire's data thread in the middle of an audio callback, the first time the user switches the
//! denoiser on. The warm-up made that invisible. The vendored library now plans its transforms when
//! a state is built, and each test here builds on its own thread and processes on another, as the
//! main loop and the data thread do, so a first-use cost of any kind shows up as a count.

use fxsound_core::messages::{DspEvent, DspParams, InputDspParams};
use fxsound_core::{DenoiseChannelMode, DenoiseLevel, DereverbLevel, Effect, EqBand};
use fxsound_dsp::input::dereverb::Dereverb;
use fxsound_dsp::input::{Denoiser, InputChain};
use fxsound_dsp::{ChainSpec, Engine, InputEngine};
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
fn allocations_during(work: impl FnOnce()) -> u64 {
    let before = allocations();
    work();
    allocations() - before
}

/// Runs `work` on a thread of its own and returns how many allocations it made there: a thread
/// that has never processed anything, as the data thread has not when a stage first runs on it.
/// Whatever `work` processes was built on the calling thread, as the main loop builds it.
fn allocations_on_a_fresh_thread(work: impl FnOnce() + Send) -> u64 {
    std::thread::scope(|scope| {
        scope
            .spawn(|| allocations_during(work))
            .join()
            .expect("the thread standing in for the data thread panicked")
    })
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
fn the_denoiser_allocates_nothing_from_its_very_first_frame_in_any_mode() {
    let input = stereo_fixture(480 * 8);
    for first in DenoiseChannelMode::ALL {
        // A stage that has never run, switched on in one mode — a voice preset with noise
        // suppression picked for the first time in the session — and then taken through
        // everything else it can be asked to do.
        let mut d = Denoiser::new(FS);
        d.set_enabled(true);
        d.set_channels(first);
        let mut block = input.clone();
        let n = allocations_on_a_fresh_thread(|| {
            d.process(&mut block, 2);
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
        assert_eq!(
            n, 0,
            "the denoiser, first run in {first:?} mode, allocated {n} times on the audio path"
        );
    }
}

#[test]
fn the_de_reverb_allocates_nothing_after_construction() {
    let mut d = Dereverb::new(FS);
    d.set_level(DereverbLevel::Medium);
    let input = stereo_fixture(480 * 8);
    let mut block = input.clone();
    let n = allocations_on_a_fresh_thread(|| {
        d.process(&mut block, 2);
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

        let mut other = params;
        other.gate_threshold_db = -40.0;
        other.denoise_level = DenoiseLevel::Strong;
        other.denoise_control = DenoiseLevel::Strong.control();
        other.denoise_channels = DenoiseChannelMode::Linked;
        other.sanitise();

        let n = allocations_on_a_fresh_thread(|| {
            block.copy_from_slice(&input);
            chain.process(&mut block, 2);
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
    let mut other = params;
    other.makeup_db = 3.0;
    other.denoise_level = DenoiseLevel::Light;
    other.denoise_control = DenoiseLevel::Light.control();
    other.sanitise();

    let n = allocations_on_a_fresh_thread(|| {
        for chunk in block.chunks_mut(1_024 * 2) {
            engine.process(chunk, 2);
        }
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

#[test]
fn the_music_engine_allocates_nothing_whatever_the_power_and_equalizer_switches_say() {
    // Each combination takes a different path through `Engine::process` — the whole GraphicEq
    // block, the effect chain alone, the master gain alone, or nothing — and a band count change
    // arriving in a snapshot rebuilds the equalizer's ladder on the way in. All of it happens on
    // the data thread.
    let mut engine = Engine::new(FS, 1_024, 2);
    let input = stereo_fixture(1_024 * 4);
    let mut block = input.clone();

    let mut snapshots = Vec::new();
    for bands in [10_usize, 31, 5] {
        let curve: Vec<EqBand> = (0..bands)
            .map(|band| {
                let hz = 30.0 * 1.25_f32.powi(band as i32);
                EqBand::new(hz, if band % 2 == 0 { 6.0 } else { -4.0 })
            })
            .collect();
        for (power, eq_on) in [(true, true), (true, false), (false, true), (false, false)] {
            let mut params = DspParams {
                power,
                eq_on,
                master_gain_db: -4.0,
                balance: 6.0,
                volume_leveling_db: 3.0,
                ..DspParams::default()
            };
            params.set_bands(&curve);
            params.set_effect(Effect::Ambience, 0.5);
            params.set_effect(Effect::Bass, 0.7);
            params.sanitise();
            snapshots.push(params);
        }
    }

    let n = allocations_on_a_fresh_thread(|| {
        for params in &snapshots {
            engine.apply(params);
            block.copy_from_slice(&input);
            for chunk in block.chunks_mut(1_024 * 2) {
                engine.process(chunk, 2);
            }
        }
        engine.handle_event(DspEvent::ResetFilterState);
        let _ = std::hint::black_box(engine.meters());
    });
    assert_eq!(
        n, 0,
        "the music engine allocated {n} times on the audio path"
    );
}

#[test]
fn crossfading_a_new_band_count_or_the_equalizer_switch_allocates_nothing() {
    // Audit #11: once audio has passed, a new band count plays the old ladder out beside the new
    // one, a second new count in the middle of that moves the new ladder section by section, the
    // equalizer's switch fades the block through a dry copy of the buffer, and a stage left out
    // with the power off lands its crossfades. Each of those paths runs here, mid-fade, in blocks
    // larger than the dry copy holds, on the data thread.
    let mut engine = Engine::new(FS, 4_096, 6);
    let input: Vec<f32> = stereo_fixture(4_096 * 3)
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|pair| [pair[0], pair[1], pair[0], pair[1], pair[0], pair[1]])
        .collect();
    let curve = |bands: usize, boost: f32| -> Vec<EqBand> {
        (0..bands)
            .map(|band| EqBand::new(30.0 * 1.25_f32.powi(band as i32), boost))
            .collect()
    };
    let snapshot = |bands: usize, boost: f32, power: bool, eq_on: bool| {
        let mut params = DspParams {
            power,
            eq_on,
            master_gain_db: -3.0,
            volume_leveling_db: 2.0,
            ..DspParams::default()
        };
        params.set_bands(&curve(bands, boost));
        params.sanitise();
        params
    };
    // Each snapshot is followed by 64 frames, which leave its fades running; `true` adds one
    // large block, which ends them. So the second band count arrives mid-crossfade and the
    // equalizer is switched back on while it is still fading out.
    let snapshots = [
        (snapshot(10, 6.0, true, true), true),
        (snapshot(31, 6.0, true, true), false),
        (snapshot(10, -3.0, true, true), true),
        (snapshot(10, -3.0, true, false), false),
        (snapshot(10, -3.0, true, true), true),
        (snapshot(20, 4.0, true, true), false),
        (snapshot(10, 2.0, false, true), true),
        (snapshot(31, 2.0, false, false), false),
        (snapshot(31, 2.0, true, true), true),
    ];
    let mut block = input.clone();
    let n = allocations_on_a_fresh_thread(|| {
        for (params, then_a_large_block) in &snapshots {
            engine.apply(params);
            block.copy_from_slice(&input);
            let (head, tail) = block.split_at_mut(64 * 6);
            engine.process(head, 6);
            if *then_a_large_block {
                engine.process(tail, 6);
            }
        }
    });
    assert_eq!(n, 0, "the crossfades allocated {n} times on the audio path");
}

#[test]
fn switching_effects_off_and_on_and_naming_the_sides_allocates_nothing() {
    // An effect coming back from zero is cleared on the data thread — the reverb's tank on the
    // first block it runs — and a layout's sides can arrive between blocks; none of it may
    // allocate. Surround at 5.1 so the side-aware balance runs on every block.
    use fxsound_dsp::engine::ChannelSide::{Centre, Left, Right};
    let channels = 6;
    let mut engine = Engine::new(FS, 1_024, channels);
    let input: Vec<f32> = stereo_fixture(1_024 * 4)
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|frame| [frame[0], frame[1], frame[0], frame[0], frame[0], frame[1]])
        .collect();
    let mut block = input.clone();

    let mut snapshots = Vec::new();
    for amount in [0.8, 0.0, 0.8, 0.0, 0.3] {
        let mut params = DspParams {
            balance: -6.0,
            master_gain_db: -2.0,
            ..DspParams::default()
        };
        for effect in [Effect::Ambience, Effect::Bass, Effect::Fidelity] {
            params.set_effect(effect, amount);
        }
        params.band_boost_db[0] = if amount == 0.0 { 0.0 } else { 4.0 };
        params.sanitise();
        snapshots.push(params);
    }
    let mut off = snapshots[0];
    off.power = false;
    snapshots.push(off);

    let n = allocations_on_a_fresh_thread(|| {
        engine.set_lfe_channel(Some(3));
        engine.set_channel_sides(Some(&[Left, Right, Centre, Centre, Left, Right]));
        for params in &snapshots {
            engine.apply(params);
            block.copy_from_slice(&input);
            for chunk in block.chunks_mut(1_024 * channels) {
                engine.process(chunk, channels);
            }
        }
        engine.set_channel_sides(None);
        block.copy_from_slice(&input);
        engine.process(&mut block, channels);
    });
    assert_eq!(
        n, 0,
        "switching effects and naming sides allocated {n} times"
    );
}
