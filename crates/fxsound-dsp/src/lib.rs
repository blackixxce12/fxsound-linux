//! The FxSound DSP engine, ported to Rust.
//!
//! Nothing in this crate allocates, locks or blocks once constructed: every buffer it needs is
//! sized at construction time from the maximum format it will be asked to handle. That is the
//! contract the PipeWire process callback depends on.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod biquad;
pub mod effects;
pub mod engine;
pub mod eq;
pub mod input;
pub mod leveller;
pub mod spectrum;

pub use biquad::{BiquadCoeffs, Real, Section, cascade_db, magnitude};
pub use effects::{Chain, Effect};
pub use engine::Engine;
pub use eq::GraphicEq;
pub use input::{InputChain, InputEngine};
pub use leveller::{Normaliser, VolumeLeveller};
pub use spectrum::SpectrumAnalyser;
