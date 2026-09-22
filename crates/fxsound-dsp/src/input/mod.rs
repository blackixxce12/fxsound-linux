//! The microphone chain.
//!
//! A separate signal path from [`crate::effects`], which is the original FxSound chain and exists
//! to make music sound bigger. A voice wants the opposite of most of that: no reverberation, no
//! stereo widening, no bass lift, and dynamics measured in milliseconds rather than in the
//! seconds the programme leveller works over.
//!
//! ```text
//! mic ─► denoise ─► dereverb ─► high-pass ─► gate ─► 10-band EQ ─► de-esser ─► compressor ─► makeup ─► limiter ─► out
//! ```
//!
//! That is the `voice` chain, and the default; [`ChainSpec`] names three others that reorder or
//! drop a stage for a podcast, a broadcast and a live stream, each with its reasons. Every stage
//! keeps one contract, [`AudioProcessor`], and [`InputChain`] is a list of them built from a
//! spec on the main loop. [`chain`] argues the order.
//!
//! Every stage here is allocation-free after construction, like the rest of the crate, and is
//! sized for the worst case rather than the current format. The denoiser and the de-reverb add
//! latency while they run — 960 and 480 frames — and report it; nothing else delays the signal
//! beyond the limiter's millisecond of look-ahead.

pub mod chain;
pub mod compressor;
pub mod deesser;
pub mod denoise;
pub mod dereverb;
pub mod detector;
pub mod engine;
pub mod gate;
pub mod highpass;
pub mod limiter;
pub mod makeup;
pub mod processor;

use crate::biquad::Real;

/// Every stage here takes a sample rate and none of them may be handed a nonsensical one: a rate of
/// zero or a NaN would design a filter that never recovers. Falling back to the usual rate rather
/// than clamping matches how the parameter limits behave at the engine boundary.
fn sane_rate(sample_rate: Real) -> Real {
    if sample_rate.is_finite() && sample_rate > 0.0 {
        sample_rate
    } else {
        48_000.0
    }
}

/// The corner to ask the port's Butterworth design for, so that the realised corner lands on `hz`.
///
/// [`crate::biquad::calc_butterworth_highpass`] is a faithful port and deliberately does not
/// prewarp, which puts its realised corner at `(fs/π)·arctan(π·f/fs)` — a few percent low at a
/// de-esser's frequency and immaterial at a high-pass's. `Fidelity`'s golden coefficients depend on
/// that design exactly as it is, so nothing here changes it; this changes only what is asked of it.
///
/// Returns `None` when there is no corner to ask for: the tangent runs away as `hz` approaches
/// `fs/π`, and a stage that cannot place its corner where the preset said must say so rather than
/// place it somewhere else.
fn prewarped(sample_rate: Real, hz: Real) -> Option<Real> {
    if !(sample_rate.is_finite() && hz.is_finite())
        || hz <= 0.0
        || hz >= MAX_CORNER_FRACTION * sample_rate
    {
        return None;
    }
    let request =
        sample_rate / std::f32::consts::PI * (std::f32::consts::PI * hz / sample_rate).tan();
    (request.is_finite() && 2.0 * request < sample_rate).then_some(request)
}

/// The highest corner a crossover or a high-pass can be built at, as a fraction of the sample rate.
///
/// Prewarping asks for `(fs/π)·tan(π·f/fs)`, which runs to infinity as `f` approaches `fs/π ≈
/// 0.318·fs`; past that there is no corner to ask for. Stopping short of it leaves the design sane
/// rather than merely finite. In practice this is the narrowband case: a 5500 Hz de-esser needs
/// about 18 kHz of sample rate, so at a 16 kHz capture that stage reports itself inactive and
/// passes the signal through instead of splitting it a kilohertz and a half below where the preset
/// said — unless it is in its adaptive mode, which places the corner at a quarter of the
/// bandwidth and so always under this line.
pub const MAX_CORNER_FRACTION: Real = 0.3;

pub use chain::InputChain;
pub use compressor::Compressor;
pub use deesser::DeEsser;
pub use denoise::Denoiser;
pub use dereverb::Dereverb;
pub use detector::{Detection, Follower};
pub use engine::InputEngine;
pub use gate::Gate;
pub use highpass::HighPass;
pub use limiter::LookaheadLimiter;
pub use makeup::Makeup;
pub use processor::{
    AudioProcessor, ChainSpec, MAX_STAGES, ProcessContext, Stage, StageAccess, StageKind,
    StageMeter,
};
