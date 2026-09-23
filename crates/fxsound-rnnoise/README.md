# fxsound-rnnoise

This is Joe Neeman's [`nnnoiseless`](https://github.com/jneem/nnnoiseless) 0.5.2, a pure-Rust
port of Xiph's [RNNoise](https://github.com/xiph/rnnoise), vendored into the FxSound tree. It is
BSD-3-Clause, not the workspace's AGPL; its `COPYING` and `AUTHORS` travel with it, byte for byte,
and so does the model, `src/weights.rnn`.

It is vendored rather than taken from the registry because the published crate's API is one
function, `process_frame`, and the denoiser in `crates/fxsound-dsp/src/input/denoise.rs` needs
three things that are not reachable through it: the twenty-two band gains (a linked-stereo mask
is built from them, and a control surface edits them before they are applied), an in-place
`reset()` (the registry crate could only be rebuilt, which is several heap allocations on the
PipeWire thread on every preset change), and a synthesis path without the network (the
per-channel half of linked stereo). A fourth came later: an FFT planned when a state is built,
because the registry crate's was planned on the first frame, on whatever thread ran it — the
PipeWire data thread, in the middle of an audio callback. `docs/0.4.0-design.md` §2 & 3 is the
contract.

The rule for this copy: the source stays as close to upstream as it can, so that a future upstream
diff still applies, and **every change is marked `// fxsound:` in the source**. Nothing the
original computed was changed; the tests at the end of this file hold it to that.

## Changes from nnnoiseless 0.5.2

`diff -u ~/.cargo/registry/src/*/nnnoiseless-0.5.2/src/<file>.rs src/<file>.rs` shows every hunk
below and nothing else, apart from rustfmt collapsing three one-line `if … else` bodies
(`rnn.rs` `unsigned`, `pitch.rs` `t1b`, `features.rs` `max`) and reordering two `use` lists to the
2024 edition's style.

### Replaced

- **The FFT** (`fft.rs`, a new file). Upstream transformed through `easyfft`'s `real_fft_using`
  and `real_ifft_using`, which keep the planner, the plans and their scratch and input-copy buffers
  in a cache private to the calling thread (`easyfft` 0.4.2, `src/dyn_size/realfft.rs` and
  `src/lib.rs`). The first frame a thread analysed therefore planned the 960-point transforms and
  allocated those buffers on that thread — in FxSound, PipeWire's real-time data thread, the first
  time noise suppression was switched on, and again on every other data thread the capture stream
  later ran on. Now `DenoiseFeatures` and `Stft` each hold an `Fft`: a forward and an inverse
  `realfft` plan made with `RealFftPlanner` in the constructor, and owned input-copy and scratch
  buffers, run with `process_with_scratch` — the pattern `fxsound-dsp`'s de-reverb already used.
  The transforms are stored in **`Spectrum`**, a fixed-size stand-in for `easyfft::DynRealDft`
  with the methods upstream called on it under the same names (`get_offset`,
  `get_frequency_bins` and their `_mut` twins, `*=` by a scalar and by a slice of gains, and
  `Deref` to the bins), so the arithmetic around them reads exactly as upstream's. That includes
  `get_frequency_bins` returning bins 1 to 478, which is what `DynRealDft` returned for this size
  and what the pitch filter was written against. The public fields `DenoiseFeatures::x` and `p`
  are `Spectrum`s now, which is the one change to upstream's API. `easyfft` is a
  dev-dependency only, for the tests that hold both directions of the new transform to its output
  bit for bit (`fft.rs`); `realfft` 3.5, which `easyfft` wraps, is the dependency.
- **The tables are built by the constructors.** `DenoiseFeatures::new` and `Stft::new` call
  `common()`, so the window and DCT tables exist before real time starts rather than being built by
  the first frame's `OnceCell::get_or_init`. That initialiser never allocated, but it was unbounded
  first-use work in an audio callback all the same.

### Added

- **`DenoiseState::reset`** (`denoise.rs`), an in-place reset that allocates nothing. It zeroes the
  gain floor `lastg` and the `silent` flag, and calls three resets that did not exist upstream
  because the only way to start afresh was `DenoiseState::new()`:
  `RnnState::reset` (`rnn.rs`, the three GRU state vectors), `DenoiseFeatures::reset`
  (`features.rs`, the input history, cepstral memory, high-pass state, both transforms, the band
  energies and the overlap-add tail) and `PitchFinder::reset` (`pitch.rs`, the last period and
  gain, the downsampled history and — so that a reset state is bit-identical to a new one — the
  scratch buffers too).
- **`DenoiseState::analyse` and `DenoiseState::synthesise`** (`denoise.rs`), the two halves of
  `process_frame`, with the band gains exposed between them as **`Analysis { vad, silence, gains:
  [f32; 22] }`**. `analyse` shifts the frame in through the high-pass filter, computes the
  features, runs the network and the pitch filter, applies the release floor `max(g, 0.6·last)`
  and returns what it found; `synthesise` interpolates whatever gains it is handed to bins, applies
  them and overlap-adds. A frame `analyse` found silent (`e < 0.04`, the network skipped) is
  remembered in a new private field, `silent`, and `synthesise` passes it through untouched
  whatever gains it is given, which is what the original did with silence. `process_frame` is now
  literally `analyse` followed by `synthesise` with the gains it returned.
- **`Stft`** (`stft.rs`, a new file): the library's own transform pair — high-pass, Vorbis window,
  960-point real FFT, overlap-add — with nothing between analysis and synthesis but a set of band
  gains handed in from outside. `push` a frame, `synthesise` it with gains, `reset` in place; the
  output is delayed one frame exactly as `DenoiseState`'s is. It does not run the pitch filter,
  which needs a pitch search of its own; that is the documented linked-stereo trade-off. To make
  it run the *identical* arithmetic, two things in `features.rs` were opened up: the body of
  `frame_synthesis` moved into a free function `overlap_add` (which `frame_synthesis` now calls),
  and `transform_input` became `pub(crate)`. Both take the caller's `Fft`.
- `impl Default for DenoiseFeatures` (`features.rs`), because `new` is public and clippy asks for
  it under `-D warnings`.

### Changed inside

- `lib.rs` `common()`: `OnceCell::get_or_init` in place of a `get().is_none()` probe, a `set` and
  a `get().unwrap()`. The same tables, built once, without an `unwrap` on the audio path.
- `pitch.rs` `PitchFinder::new`: the run-time `assert!` on four constants is a `const _: () =
  assert!(…)`.
- `util.rs` `Biquad::filter_in_place`: `#[cfg_attr(not(feature = "train"), allow(dead_code))]`
  became a plain `#[allow(dead_code)]`, since the `train` feature is gone.
- `lib.rs` carries a crate-level `#![allow(clippy::…)]` for seven *style* lints
  (`needless_range_loop` and the like) so that the workspace's `clippy -D warnings` passes without
  rewriting two thousand lines that compute correctly. No correctness lint is allowed. The
  `#![deny(missing_docs)]` is upstream's and stays.
- The `DenoiseState` doc example says `use fxsound_rnnoise::DenoiseState`, and its sentence
  pointing at `DenoiseSignal` is gone with the `dasp` feature.

### Dropped

- The Cargo features `bin`, `train`, `capi` and `dasp` (and so `default`), with the dependencies
  only they pulled in: `anyhow`, `clap`, `dasp`, `dasp_interpolate`, `dasp_ring_buffer`, `glob`,
  `hdf5`, `hound`, `libc`, `ndarray`, `rand`, and the dev-dependencies `assert_cmd`, `assert_fs`,
  `criterion`, `predicates`, `static_assertions`. What remains is what the library needs:
  `once_cell` at upstream's version, and `realfft` in `easyfft`'s place (see *Replaced*).
- The files those features compiled: `src/nnnoiseless.rs` (the command-line tool),
  `src/training.rs` (the `train` binary), `src/capi.rs` (the RNNoise-compatible C API and its
  `cbindgen.toml`), `src/signal.rs` (the `dasp` adaptor), the `train/` directory (Python scripts
  and their README), `examples/corr.rs`, `benches/sin.rs`, `tests/cli.rs`, `release.toml`,
  upstream's `CHANGELOG.md` and `.github/`. `util` is always a private module (upstream made it
  `pub` under `train`).
- Upstream's `lib.rs` test module (`compare_to_reference`, `compare_signal_to_reference`). Both
  read `test_data/*.raw`, which upstream's `Cargo.toml` excludes from the published crate, so they
  never compiled from the registry copy either. The `static_assertions` check that
  `DenoiseState` is `Send + Sync` is kept as a plain generic-function test.
- Upstream's README, which this file replaces: its `cargo install`, C API and training sections
  describe the parts not carried. They are still true of upstream.

### Kept

- `src/weights.rnn`, `COPYING` and `AUTHORS`, byte-identical.
- The whole of upstream's library API: `DenoiseState::{new, from_model, with_model,
  process_frame, FRAME_SIZE}`, `DenoiseFeatures` and its public methods, `RnnModel::{from_bytes,
  from_static_bytes}` and the `#[doc(hidden)]` constants. A custom model still loads through
  `RnnModel::from_bytes` and `DenoiseState::from_model`, and `process_frame` still behaves exactly
  as it did.

## Safety

One `unsafe` block remains, and it is upstream's, unchanged: `to_i8` in `src/rnn.rs` (line 11)
turns the `&[u8]` that `include_bytes!` gives into the `&[i8]` the weights are, with
`slice::from_raw_parts` at the same pointer and length. `i8` and `u8` have the same size and
alignment and every bit pattern is valid for both, so it is sound. It is why this crate is the one
in the workspace without `#![forbid(unsafe_code)]`. Upstream's README counted two unsafe places;
the other, an `f32`-to-`Complex` cast for the FFT, was already gone by 0.5.2 (the port had moved
to `easyfft`), and the `unsafe extern "C"` functions went with `capi.rs`. The block could be
replaced by a one-time signed copy when the model is built — that happens before real time — at
the cost of one more hunk against upstream; it has not been, because it does not need to be.

## Held by tests

`cargo test -p fxsound-rnnoise` runs, in `src/denoise.rs`:

- `process_frame_is_analyse_followed_by_synthesise_sample_for_sample` — the registry crate's
  `process_frame` reproduced verbatim on the private fields (`reference_process_frame`), and the
  split path held to it bit for bit, output and voice probability, over forty frames of hiss, hum
  and a harmonic voice.
- `a_reset_state_computes_exactly_what_a_fresh_one_does`.
- `a_silent_frame_is_reported_and_passed_through`.
- `the_stft_is_the_librarys_own_synthesis_path_without_the_pitch_filter` and
  `the_pitch_filter_is_the_only_difference_between_the_stft_and_the_state`.
- `the_state_is_send_and_sync`.

in `src/stft.rs`: `with_unity_gains_it_is_an_identity_delayed_by_one_frame`,
`a_zero_gain_takes_the_band_out`, `a_reset_transform_starts_from_silence`;

and in `src/fft.rs`: `the_forward_transform_is_easyffts_bit_for_bit`,
`the_inverse_transform_is_easyffts_bit_for_bit` and
`the_frequency_bins_are_the_range_upstreams_pitch_filter_was_written_against`.

Over in `fxsound-dsp`, `tests/rt_allocations.rs` builds the denoiser on one thread and runs it
on another that has never run it, in every channel mode, under a counting allocator, and holds
it — and so `reset`, `analyse`, `synthesise` and `Stft` — to zero allocations from the very first
frame. With `easyfft`'s thread-local transforms, that first frame allocated 48 times.

## Taking a newer upstream

Diff the new upstream against 0.5.2 in the registry, apply that onto `src/`, and keep every
`// fxsound:` hunk. If upstream's `process_frame` changes, `reference_process_frame` in the
`denoise.rs` tests must be updated to its new body — it exists to pin the split to upstream's
arithmetic, so it has to *be* upstream's arithmetic. Then update the version in this file, in
`Cargo.toml`'s comment and `description`, and in the `lib.rs` module doc.
