//! Nothing on the audio path allocates, held by a counting allocator — and, in silence, nothing on
//! it works on subnormals, held by the processor's own sticky exception flags (see
//! [`subnormal_arithmetic_in`]).
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

#[test]
fn switching_to_the_windows_dsp_and_back_allocates_nothing() {
    // «Like FxSound for Windows» = Interface and sound reaches the audio thread in the snapshot:
    // the leveller's arithmetic, Dynamic Boost's floor and level, and its limiter's linking, hold
    // and attack all change between two blocks, on 5.1 with the sides named, and back. None of it
    // may allocate.
    use fxsound_core::DspCompat;
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
    let snapshots: Vec<DspParams> = [DspCompat::Linux, DspCompat::Windows]
        .into_iter()
        .cycle()
        .take(5)
        .map(|compat| {
            let mut params = DspParams {
                compat,
                volume_leveling_db: 3.0,
                ..DspParams::default()
            };
            params.set_effect(Effect::DynamicBoost, 1.0);
            params
        })
        .collect();

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
    });
    assert_eq!(n, 0, "switching the DSP allocated {n} times");
}

/// The SSE status register's sticky flags for an arithmetic operand that was subnormal (DE, bit 1)
/// and for a result that underflowed (UE, bit 4). Each one set is an operation that took the slow
/// path: a microcode assist on many x86 parts, tens to hundreds of cycles, on the audio thread.
#[cfg(target_arch = "x86_64")]
const SUBNORMAL_FLAGS: u32 = (1 << 1) | (1 << 4);

#[cfg(target_arch = "x86_64")]
fn sse_status() -> u32 {
    let mut status = 0_u32;
    // SAFETY: `stmxcsr` stores the thread's own SSE control and status register into the `u32`
    // it is pointed at, which lives on this stack frame for the whole instruction.
    unsafe {
        core::arch::asm!(
            "stmxcsr [{}]",
            in(reg) &raw mut status,
            options(nostack, preserves_flags)
        );
    }
    status
}

/// Runs `work` and says whether any of it did arithmetic on a subnormal or underflowed into one.
///
/// Only the six sticky exception flags are cleared first — the rounding mode, the exception masks
/// and flush-to-zero are left as they are — and they belong to this thread alone. Flags are the
/// only measure of this that does not depend on how fast the machine is or how busy it is: a
/// timing comparison against flush-to-zero says the same thing on a quiet machine and nothing on
/// a loaded one.
#[cfg(target_arch = "x86_64")]
fn subnormal_arithmetic_in(work: impl FnOnce()) -> bool {
    let cleared = sse_status() & !0x3f;
    // SAFETY: `ldmxcsr` loads the thread's SSE register from the `u32` it is pointed at; the value
    // is the register's own with only its sticky exception flags cleared, so nothing about how
    // later arithmetic rounds or traps changes.
    unsafe {
        core::arch::asm!(
            "ldmxcsr [{}]",
            in(reg) &raw const cleared,
            options(nostack, readonly, preserves_flags)
        );
    }
    work();
    sse_status() & SUBNORMAL_FLAGS != 0
}

/// A ten-band curve alternating +4 and -3 dB, the report's: every band live, so every band leaves
/// its bias residue in silence.
fn boosted_equalizer() -> fxsound_dsp::GraphicEq {
    let mut eq = fxsound_dsp::GraphicEq::new();
    eq.set_sample_rate(FS);
    let centres = eq.center_frequencies().to_vec();
    let gains: Vec<f32> = (0..centres.len())
        .map(|band| if band % 2 == 0 { 4.0 } else { -3.0 })
        .collect();
    eq.set_bands(&centres, &gains);
    eq
}

/// Whether `stage` does subnormal arithmetic on the silence `filter` hands it.
///
/// Two seconds of music through both, as a listener's session would have, then two of digital
/// silence to let every tail die and every detector settle, and then one more second of `filter`'s
/// silence — its bias residue, around 1e-30, not zeros — to `stage` alone, with the flags watched.
/// 480-frame blocks.
#[cfg(target_arch = "x86_64")]
fn subnormal_arithmetic_behind(
    filter: &mut dyn FnMut(&mut [f32]),
    stage: &mut dyn FnMut(&mut [f32]),
) -> bool {
    let music = stereo_fixture(96_000);
    let mut block = vec![0.0_f32; 960];
    for chunk in music.as_chunks::<960>().0 {
        block.copy_from_slice(chunk);
        filter(&mut block);
        stage(&mut block);
    }
    for _ in 0..200 {
        block.fill(0.0);
        filter(&mut block);
        stage(&mut block);
    }
    let mut residue: Vec<Vec<f32>> = (0..100)
        .map(|_| {
            block.fill(0.0);
            filter(&mut block);
            block.clone()
        })
        .collect();
    let largest = residue
        .iter()
        .flatten()
        .fold(0.0_f32, |most, sample| most.max(sample.abs()));
    assert!(
        largest > 0.0 && largest < 1e-20,
        "the filter should hand on bias residue in silence, not {largest:e}"
    );
    let slow = subnormal_arithmetic_in(|| {
        for block in &mut residue {
            stage(block);
        }
    });
    std::hint::black_box(&residue);
    slow
}

#[cfg(target_arch = "x86_64")]
#[test]
fn the_leveller_behind_a_boosted_equalizer_does_no_subnormal_arithmetic_in_silence() {
    // Audit #5, the half its first fix missed. The detector's filter state was flushed, but the
    // "silence" the stage is handed behind any live equalizer band is the bands' bias residue,
    // around 1e-30, and the detector and the post-gain statistics squared it on every sample: an
    // underflow each time. Behind this curve at Volume Leveling 2, with everything else off, the
    // engine on stereo cost 96.5 ns a frame in silence against 60.6 on music, and 53.4 with the
    // processor flushing subnormals itself (48 kHz, 480-frame blocks, best of seven release runs
    // on a Ryzen 7 6800H). The stage now reads a sample that small as zero before it squares
    // anything, and silence costs 45.6.
    let mut eq = boosted_equalizer();
    let mut leveller = fxsound_dsp::VolumeLeveller::new(FS);
    leveller.set_amount(2.0);
    assert!(
        !subnormal_arithmetic_behind(&mut |block| eq.process(block, 2), &mut |block| leveller
            .process(block, 2),),
        "the leveller did subnormal arithmetic on a second of the equalizer's silence"
    );
}

#[cfg(target_arch = "x86_64")]
#[test]
fn dynamic_boost_behind_bass_does_no_subnormal_arithmetic_in_silence() {
    // Audit #7's estimator squares the front pair in f32, and behind any stage that leaves bias
    // residue in silence — Bass, the equalizer, Fidelity, Ambience — both squares underflowed on
    // every frame. The stage is never bypassed, so that cost every preset with anything switched
    // on: with Bass at 0.6 and everything else off, the engine on stereo cost 40.7 ns a frame in
    // silence against 30.6 on music, and 24.1 with the processor flushing subnormals itself
    // (measured as above). A pair that small now reads as silence without being squared, and
    // silence costs 21.9.
    use fxsound_dsp::effects::{Bass, DynamicBoost, Effect as _};
    let mut bass = Bass::new(FS);
    bass.set_amount(0.6);
    bass.settle();
    let mut eq = boosted_equalizer();
    for amount in [0.0, 0.6, 1.0] {
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(amount);
        boost.settle();
        assert!(
            !subnormal_arithmetic_behind(&mut |block| bass.process(block, 2), &mut |block| boost
                .process(block, 2),),
            "Dynamic Boost at {amount} did subnormal arithmetic on a second of Bass's silence"
        );
        let mut boost = DynamicBoost::new(FS);
        boost.set_amount(amount);
        boost.settle();
        assert!(
            !subnormal_arithmetic_behind(&mut |block| eq.process(block, 2), &mut |block| boost
                .process(block, 2),),
            "Dynamic Boost at {amount} did subnormal arithmetic on the equalizer's silence"
        );
    }
}

/// Whether `stage` does subnormal arithmetic on a muted microphone.
///
/// Two seconds of a talker in a room, then `muted` seconds of digital zeros — a USB microphone
/// muted in hardware, or a capture switch turned off — for every recursion to decay as far as it
/// is going to, and then one more second of zeros with the flags watched. 480-frame blocks,
/// stereo. The zeros are exact: in front of the microphone chain there is no filter to leave a
/// bias residue, which is why its own recursions have to stop somewhere of their own accord.
#[cfg(target_arch = "x86_64")]
fn subnormal_arithmetic_on_a_muted_microphone(
    stage: &mut dyn FnMut(&mut [f32]),
    muted: usize,
) -> bool {
    let talker = stereo_fixture(96_000);
    let mut block = vec![0.0_f32; 960];
    for chunk in talker.as_chunks::<960>().0 {
        block.copy_from_slice(chunk);
        stage(&mut block);
    }
    for _ in 0..muted * 100 {
        block.fill(0.0);
        stage(&mut block);
    }
    let slow = subnormal_arithmetic_in(|| {
        for _ in 0..100 {
            block.fill(0.0);
            stage(&mut block);
        }
    });
    std::hint::black_box(&block);
    slow
}

#[cfg(target_arch = "x86_64")]
#[test]
fn a_muted_microphone_leaves_the_denoiser_no_subnormal_arithmetic_in_any_mode() {
    // RNNoise's input high-pass has its poles at a radius of 0.998 and rounds its state to f32
    // after every step, so on digital silence it decays into the subnormals and settles there in
    // a limit cycle around 5e-43, for good: every frame after that ran the window, two 960-point
    // transforms, the band correlations and the pitch search on subnormal data before the
    // silence check skipped the network — per channel, and again in each linked transform. The
    // stage's reduction meter decayed the same way.
    for mode in DenoiseChannelMode::ALL {
        let mut d = Denoiser::new(FS);
        d.set_enabled(true);
        d.set_level(DenoiseLevel::Medium);
        d.set_control(DenoiseLevel::Medium.control());
        d.set_channels(mode);
        assert!(
            !subnormal_arithmetic_on_a_muted_microphone(&mut |block| d.process(block, 2), 3),
            "the denoiser in {mode:?} mode did subnormal arithmetic on a muted microphone"
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn a_muted_microphone_leaves_the_de_reverb_no_subnormal_arithmetic() {
    // The per-bin power recursion `psd += 0.12·(power − psd)` had no floor: on digital silence it
    // decayed until `0.12·psd` rounded away a few steps above the smallest subnormal and stopped
    // there, and every hop after that did subnormal arithmetic in all 241 bins of every channel,
    // through the history ring and the tail estimate. About four seconds of zeros get it there.
    let mut d = Dereverb::new(FS);
    d.set_level(DereverbLevel::Medium);
    assert!(
        !subnormal_arithmetic_on_a_muted_microphone(&mut |block| d.process(block, 2), 8),
        "the de-reverb did subnormal arithmetic on a muted microphone"
    );
}

#[cfg(target_arch = "x86_64")]
#[test]
fn a_muted_microphone_leaves_the_whole_voice_chain_no_subnormal_arithmetic() {
    // Every stage at once, as a preset runs them, and the engine's own meters around them. Behind
    // the high-pass, whose bias residue is what "silence" is from there on, the gate's, the
    // de-esser's and the compressor's detectors squared about 1e-30 on every sample and
    // underflowed each time; the held input peak decayed into the subnormals and stuck at the
    // smallest one. With the two tests above, that made the voice chain — stereo, Medium
    // denoising and Medium de-reverb — cost 0.0182 CPU-seconds per second of a muted microphone
    // against 0.0125 with the processor flushing subnormals itself (release build, 480-frame
    // blocks, best of seven runs on a Ryzen 7 6800H; parts that take a microcode assist for each
    // subnormal operand pay far more). It costs 0.0126 now.
    let mut band_boost_db = [0.0; fxsound_core::eq::MAX_BANDS];
    for (band, boost) in band_boost_db.iter_mut().enumerate().take(10) {
        *boost = if band % 2 == 0 { 4.0 } else { -3.0 };
    }
    for mode in DenoiseChannelMode::ALL {
        let mut params = InputDspParams {
            rnnoise: true,
            denoise_level: DenoiseLevel::Medium,
            denoise_control: DenoiseLevel::Medium.control(),
            denoise_channels: mode,
            dereverb: DereverbLevel::Medium,
            vad_gate: true,
            band_boost_db,
            ..InputDspParams::default()
        };
        params.sanitise();
        let mut engine = InputEngine::new(FS, 480, 2);
        engine.apply(&params);
        assert!(
            !subnormal_arithmetic_on_a_muted_microphone(&mut |block| engine.process(block, 2), 8),
            "the voice chain with {mode:?} denoising did subnormal arithmetic on a muted \
             microphone"
        );
        std::hint::black_box(engine.meters());
    }
}
