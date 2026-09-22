//! The stage contract, and the chain specification built from it.
//!
//! 0.3.0's [`super::InputChain`] was a struct with one field per stage and a `process` that
//! named each of them in order. That is the right shape for one chain and the wrong one for
//! four: a podcast chain without a gate, a broadcast chain that compresses before it voices, a
//! streaming chain that de-esses what its compressor brought up. Each of those is an *ordering*
//! over the same stages, so the stages get a common contract — [`AudioProcessor`] — and the chain
//! becomes a list built from a [`ChainSpec`].
//!
//! [`Stage`] is an enum rather than a `Box<dyn AudioProcessor>` for three reasons that add up:
//! the dispatch is a `match` and not a vtable, `Debug` keeps working without a bound on every
//! type, and typed access (`chain.stage::<Gate>()`) is a pattern match rather than a downcast.
//! The set of stages is closed — it is this crate's — so the enum costs nothing it would not
//! cost anyway.
//!
//! Real-time safe: a chain is built from a spec on the main loop, which is where the allocation
//! happens; everything else here is a `match`.

use crate::biquad::Real;
use crate::eq::GraphicEq;
use crate::input::dereverb::Dereverb;
use crate::input::highpass::HighPass;
use crate::input::limiter::LookaheadLimiter;
use crate::input::makeup::Makeup;
use crate::input::{Compressor, DeEsser, Denoiser, Gate};
use fxsound_core::messages::InputDspParams;

/// What a stage is told about the block it is handed, beyond the samples.
///
/// `voice_probability` is the denoiser's opinion of the frame just processed, `0.0` when it is
/// not running; the gate reads it as a side-chain when a preset asks. It is carried here rather
/// than read from the denoiser directly so that a stage never has to know where in the chain
/// it sits or whether a denoiser exists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcessContext {
    pub sample_rate: Real,
    /// `1..=MAX_CHANNELS`, already clamped by the engine.
    pub channels: usize,
    pub voice_probability: Real,
}

/// What a stage shows on a meter.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StageMeter {
    /// Decibels the stage is taking away, as a positive number. Zero for a stage that does not
    /// take gain away.
    pub reduction_db: Real,
    /// Whether the stage is actually processing — switched on *and* buildable at this rate.
    pub running: bool,
    /// One stage-specific number: the denoiser's voice probability, the de-esser's realised
    /// corner in hertz. Zero where a stage has nothing to add.
    pub aux: Real,
}

/// The contract every stage of the microphone chain keeps.
pub trait AudioProcessor {
    /// Redesign for a rate, then reset. Never allocates: every buffer was sized at construction
    /// for the worst case.
    fn prepare(&mut self, sample_rate: Real);
    /// Take the stage's own fields from the snapshot. Each stage knows which are its; the chain
    /// hands the whole snapshot to every stage rather than picking for them, so adding a field
    /// is a change in one place.
    fn apply(&mut self, params: &InputDspParams);
    /// Clear every piece of history, in place.
    fn reset(&mut self);
    /// Switched on *and* buildable at this rate. A stage that is not active is skipped by the
    /// chain and contributes no latency.
    fn is_active(&self) -> bool;
    /// Frames of delay the stage adds, counted only while it is active.
    fn latency_frames(&self) -> usize;
    /// One interleaved block, in place.
    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext);
    fn meter(&self) -> StageMeter;
}

/// The kinds of stage a chain can hold, one instance of each at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StageKind {
    Denoise,
    Dereverb,
    HighPass,
    Gate,
    Eq,
    DeEsser,
    Compressor,
    Makeup,
    Limiter,
}

impl StageKind {
    /// Every kind, in the order the full chain runs them.
    pub const ALL: [Self; 9] = [
        Self::Denoise,
        Self::Dereverb,
        Self::HighPass,
        Self::Gate,
        Self::Eq,
        Self::DeEsser,
        Self::Compressor,
        Self::Makeup,
        Self::Limiter,
    ];
}

/// The most stages a chain can hold: one of each kind.
pub const MAX_STAGES: usize = StageKind::ALL.len();

/// One stage of the chain, whichever kind it is.
///
/// Every variant is boxed: a denoiser is four kilobytes of inline frame buffers and a makeup
/// gain is two floats, and the chain holds nine of these in an array. The boxes are made when
/// the chain is built, on the main loop, and never touched again; the audio path only follows
/// them.
#[derive(Debug)]
pub enum Stage {
    Denoise(Box<Denoiser>),
    Dereverb(Box<Dereverb>),
    HighPass(Box<HighPass>),
    Gate(Box<Gate>),
    Eq(Box<GraphicEq>),
    DeEsser(Box<DeEsser>),
    Compressor(Box<Compressor>),
    Makeup(Box<Makeup>),
    Limiter(Box<LookaheadLimiter>),
}

impl Stage {
    /// Build one stage of a kind, sized for the worst case. This is the allocation; do it on
    /// the main loop.
    #[must_use]
    pub fn build(kind: StageKind, sample_rate: Real) -> Self {
        match kind {
            StageKind::Denoise => Self::Denoise(Box::new(Denoiser::new(sample_rate))),
            StageKind::Dereverb => Self::Dereverb(Box::new(Dereverb::new(sample_rate))),
            StageKind::HighPass => Self::HighPass(Box::new(HighPass::new(sample_rate))),
            StageKind::Gate => Self::Gate(Box::new(Gate::new(sample_rate))),
            StageKind::Eq => {
                let mut eq = GraphicEq::new();
                eq.set_sample_rate(sample_rate);
                Self::Eq(Box::new(eq))
            }
            StageKind::DeEsser => Self::DeEsser(Box::new(DeEsser::new(sample_rate))),
            StageKind::Compressor => Self::Compressor(Box::new(Compressor::new(sample_rate))),
            StageKind::Makeup => Self::Makeup(Box::default()),
            StageKind::Limiter => Self::Limiter(Box::new(LookaheadLimiter::new(
                sample_rate,
                1.0,
                LOOKAHEAD_MS,
                LIMITER_RELEASE_MS,
            ))),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> StageKind {
        match self {
            Self::Denoise(_) => StageKind::Denoise,
            Self::Dereverb(_) => StageKind::Dereverb,
            Self::HighPass(_) => StageKind::HighPass,
            Self::Gate(_) => StageKind::Gate,
            Self::Eq(_) => StageKind::Eq,
            Self::DeEsser(_) => StageKind::DeEsser,
            Self::Compressor(_) => StageKind::Compressor,
            Self::Makeup(_) => StageKind::Makeup,
            Self::Limiter(_) => StageKind::Limiter,
        }
    }
}

/// A millisecond of look-ahead for the chain's limiter. Enough for it to arrive before a
/// transient does, short enough that nobody is talking over themselves.
const LOOKAHEAD_MS: Real = 1.0;
const LIMITER_RELEASE_MS: Real = 80.0;

/// Every method forwards to the variant. A macro rather than nine hand-written matches, so a
/// new stage is one line here and cannot be forgotten in one of the forwards.
macro_rules! forward {
    ($self:ident, $stage:ident => $body:expr) => {
        match $self {
            Stage::Denoise($stage) => $body,
            Stage::Dereverb($stage) => $body,
            Stage::HighPass($stage) => $body,
            Stage::Gate($stage) => $body,
            Stage::Eq($stage) => $body,
            Stage::DeEsser($stage) => $body,
            Stage::Compressor($stage) => $body,
            Stage::Makeup($stage) => $body,
            Stage::Limiter($stage) => $body,
        }
    };
}

// Fully qualified throughout: most stages also have an inherent `process(buffer, channels)` or
// `reset()`, and method syntax would pick the inherent one. The trait is implemented for the
// stage types and not for their boxes, so each call goes through the box explicitly
// (`as_mut` / `as_ref`) rather than relying on auto-deref, which does not apply to a path call.
impl AudioProcessor for Stage {
    fn prepare(&mut self, sample_rate: Real) {
        forward!(self, stage => AudioProcessor::prepare(stage.as_mut(), sample_rate));
    }

    fn apply(&mut self, params: &InputDspParams) {
        forward!(self, stage => AudioProcessor::apply(stage.as_mut(), params));
    }

    fn reset(&mut self) {
        forward!(self, stage => AudioProcessor::reset(stage.as_mut()));
    }

    fn is_active(&self) -> bool {
        forward!(self, stage => AudioProcessor::is_active(stage.as_ref()))
    }

    fn latency_frames(&self) -> usize {
        forward!(self, stage => AudioProcessor::latency_frames(stage.as_ref()))
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        forward!(self, stage => AudioProcessor::process(stage.as_mut(), buffer, ctx));
    }

    fn meter(&self) -> StageMeter {
        forward!(self, stage => AudioProcessor::meter(stage.as_ref()))
    }
}

/// Typed access to a stage: `chain.stage::<Gate>()`. Implemented for every stage type; the
/// chain searches its list for the variant.
pub trait StageAccess: Sized {
    const KIND: StageKind;
    fn from_stage(stage: &Stage) -> Option<&Self>;
    fn from_stage_mut(stage: &mut Stage) -> Option<&mut Self>;
}

macro_rules! access {
    ($type:ty, $variant:ident) => {
        impl StageAccess for $type {
            const KIND: StageKind = StageKind::$variant;

            fn from_stage(stage: &Stage) -> Option<&Self> {
                match stage {
                    Stage::$variant(inner) => Some(inner.as_ref()),
                    _ => None,
                }
            }

            fn from_stage_mut(stage: &mut Stage) -> Option<&mut Self> {
                match stage {
                    Stage::$variant(inner) => Some(inner.as_mut()),
                    _ => None,
                }
            }
        }
    };
}

access!(Denoiser, Denoise);
access!(Dereverb, Dereverb);
access!(HighPass, HighPass);
access!(Gate, Gate);
access!(GraphicEq, Eq);
access!(DeEsser, DeEsser);
access!(Compressor, Compressor);
access!(Makeup, Makeup);
access!(LookaheadLimiter, Limiter);

/// The equalizer as a stage. It is the same [`GraphicEq`] the output side runs, with its own
/// state; what is particular to the microphone chain is which snapshot fields drive it.
impl AudioProcessor for GraphicEq {
    fn prepare(&mut self, sample_rate: Real) {
        self.set_sample_rate(sample_rate);
        self.reset();
    }

    fn apply(&mut self, params: &InputDspParams) {
        self.set_enabled(params.eq_on);
        self.set_q_multiplier(params.filter_q);
        // A snapshot arrives whenever *anything* moves; rebuilding the bands clears their
        // history, which is a click, so a ladder that did not change is not rebuilt.
        let (centers, boosts) = params.bands();
        if centers != self.center_frequencies() || boosts != self.boosts_db() {
            self.set_bands(centers, boosts);
        }
    }

    fn reset(&mut self) {
        GraphicEq::reset(self);
    }

    fn is_active(&self) -> bool {
        self.is_enabled()
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        if self.is_enabled() {
            GraphicEq::process(self, buffer, ctx.channels);
        }
    }

    fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: 0.0,
            running: self.is_enabled(),
            aux: 0.0,
        }
    }
}

/// The limiter as a stage: the one that has no switch.
impl AudioProcessor for LookaheadLimiter {
    fn prepare(&mut self, sample_rate: Real) {
        self.set_sample_rate(sample_rate);
    }

    fn apply(&mut self, params: &InputDspParams) {
        self.set_ceiling_db(params.ceiling_db);
    }

    fn reset(&mut self) {
        LookaheadLimiter::reset(self);
    }

    fn is_active(&self) -> bool {
        true
    }

    fn latency_frames(&self) -> usize {
        LookaheadLimiter::latency_frames(self)
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        LookaheadLimiter::process(self, buffer, ctx.channels);
    }

    fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: self.reduction_db(0),
            running: true,
            aux: 0.0,
        }
    }
}

/// An ordering of stages: what a chain is built from.
///
/// Always ends in the limiter, whatever it was asked for. Makeup gain is the one control in the
/// chain that can manufacture a sample above full scale, and something has to be standing behind
/// it; a spec that leaves the limiter out gets it appended, and one that puts it elsewhere gets
/// it moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChainSpec {
    stages: [Option<StageKind>; MAX_STAGES],
}

impl ChainSpec {
    /// The names [`ChainSpec::by_name`] answers to, in the order the interface lists them.
    pub const NAMES: [&'static str; 4] = ["voice", "podcast", "broadcast", "streaming"];

    /// The chain 0.3.0 shipped, and the default: a frozen fingerprint of that release's output
    /// holds this ordering to it sample for sample.
    ///
    /// ```text
    /// denoise ─► dereverb ─► high-pass ─► gate ─► EQ ─► de-esser ─► compressor ─► makeup ─► limiter
    /// ```
    ///
    /// The de-reverb slot is new in 0.4.0 and inert unless a preset or the settings ask for it,
    /// which is why it can sit in the reference chain: a stage at `Off` is not in the signal
    /// path at all, and it has to be *somewhere* for the global setting to reach a voice preset
    /// that never mentions it. Its position — after the denoiser, before the high-pass — is
    /// argued in [`super::chain`].
    #[must_use]
    pub const fn voice() -> Self {
        Self::of(&StageKind::ALL)
    }

    /// The voice chain without its gate.
    ///
    /// For material on its way to an editor. A gate that opens and closes on a talker's pauses
    /// leaves a room tone that comes and goes, and a floor that comes and goes defeats a
    /// crossfade over a splice — the Podcast preset's own note says so and sets the gentlest
    /// gate in the set; a chain without one is that argument taken to its end. The denoiser
    /// does the floor's work without the switching, and the editor does the rest.
    #[must_use]
    pub const fn podcast() -> Self {
        Self::of(&[
            StageKind::Denoise,
            StageKind::Dereverb,
            StageKind::HighPass,
            StageKind::Eq,
            StageKind::DeEsser,
            StageKind::Compressor,
            StageKind::Makeup,
            StageKind::Limiter,
        ])
    }

    /// The voice chain with the compressor ahead of the equalizer and the de-esser.
    ///
    /// The radio order: compress the microphone, then voice the result. The argument is the one
    /// that puts the gate before the EQ, applied to the compressor — a preset that lifts
    /// presence by two decibels must not thereby move its own compressor threshold by two, and
    /// a broadcast preset's equalizer is the one in the set allowed past ±2 dB. The de-esser
    /// follows the compressor because a compressor riding phrases brings up the sibilance
    /// between them, and a de-esser in front never saw it.
    #[must_use]
    pub const fn broadcast() -> Self {
        Self::of(&[
            StageKind::Denoise,
            StageKind::Dereverb,
            StageKind::HighPass,
            StageKind::Gate,
            StageKind::Compressor,
            StageKind::Eq,
            StageKind::DeEsser,
            StageKind::Makeup,
            StageKind::Limiter,
        ])
    }

    /// The voice chain with the de-esser after the compressor.
    ///
    /// A live send compresses hard — 4:1 at −20 dB in the Streaming preset — and a hard
    /// compressor brings up whatever sits between the words, sibilance first. A de-esser before
    /// it measures the sibilant at the level it *arrived*, which is under its threshold; after
    /// it, the de-esser measures what the listener will hear. The equalizer stays in front of
    /// the compressor as in the voice chain, because a streaming preset's curve is a tone and
    /// not a broadcast voicing.
    #[must_use]
    pub const fn streaming() -> Self {
        Self::of(&[
            StageKind::Denoise,
            StageKind::Dereverb,
            StageKind::HighPass,
            StageKind::Gate,
            StageKind::Eq,
            StageKind::Compressor,
            StageKind::DeEsser,
            StageKind::Makeup,
            StageKind::Limiter,
        ])
    }

    /// Any ordering. Duplicates are dropped (the first occurrence wins), and the limiter is put
    /// last whether or not it was named.
    #[must_use]
    pub const fn custom(kinds: &[StageKind]) -> Self {
        Self::of(kinds)
    }

    const fn of(kinds: &[StageKind]) -> Self {
        let mut stages = [None; MAX_STAGES];
        let mut len = 0;
        let mut i = 0;
        while i < kinds.len() {
            let kind = kinds[i];
            i += 1;
            if matches!(kind, StageKind::Limiter) || Self::holds(&stages, kind) {
                continue;
            }
            if len < MAX_STAGES - 1 {
                stages[len] = Some(kind);
                len += 1;
            }
        }
        stages[len] = Some(StageKind::Limiter);
        Self { stages }
    }

    const fn holds(stages: &[Option<StageKind>; MAX_STAGES], kind: StageKind) -> bool {
        let mut i = 0;
        while i < MAX_STAGES {
            if let Some(held) = stages[i]
                && held as u8 == kind as u8
            {
                return true;
            }
            i += 1;
        }
        false
    }

    /// The spec a preset names: one of [`ChainSpec::NAMES`], case-insensitively.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "voice" => Some(Self::voice()),
            "podcast" => Some(Self::podcast()),
            "broadcast" => Some(Self::broadcast()),
            "streaming" => Some(Self::streaming()),
            _ => None,
        }
    }

    /// The name this spec answers to, if it is one of the built-in four.
    #[must_use]
    pub fn name(&self) -> Option<&'static str> {
        [
            Self::voice(),
            Self::podcast(),
            Self::broadcast(),
            Self::streaming(),
        ]
        .iter()
        .zip(Self::NAMES)
        .find(|(spec, _)| *spec == self)
        .map(|(_, name)| name)
    }

    /// The kinds, in order.
    pub fn kinds(&self) -> impl Iterator<Item = StageKind> + '_ {
        self.stages.iter().flatten().copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.kinds().count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn contains(&self, kind: StageKind) -> bool {
        self.kinds().any(|k| k == kind)
    }
}

impl Default for ChainSpec {
    fn default() -> Self {
        Self::voice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_voice_spec_is_the_full_chain_in_order() {
        let kinds: Vec<StageKind> = ChainSpec::voice().kinds().collect();
        assert_eq!(kinds, StageKind::ALL.to_vec());
        assert_eq!(ChainSpec::default(), ChainSpec::voice());
    }

    #[test]
    fn every_spec_ends_in_the_limiter_and_holds_each_kind_once() {
        for name in ChainSpec::NAMES {
            let spec = ChainSpec::by_name(name).expect(name);
            let kinds: Vec<StageKind> = spec.kinds().collect();
            assert_eq!(kinds.last(), Some(&StageKind::Limiter), "{name}");
            assert_eq!(
                kinds.iter().filter(|k| **k == StageKind::Limiter).count(),
                1,
                "{name}"
            );
            for kind in StageKind::ALL {
                assert!(
                    kinds.iter().filter(|k| **k == kind).count() <= 1,
                    "{name} holds {kind:?} twice"
                );
            }
            assert_eq!(spec.name(), Some(name));
        }
    }

    #[test]
    fn a_spec_without_a_limiter_gets_one_and_one_with_it_elsewhere_has_it_moved() {
        let spec = ChainSpec::custom(&[StageKind::Gate, StageKind::Compressor]);
        let kinds: Vec<StageKind> = spec.kinds().collect();
        assert_eq!(
            kinds,
            vec![StageKind::Gate, StageKind::Compressor, StageKind::Limiter]
        );

        let spec = ChainSpec::custom(&[StageKind::Limiter, StageKind::Gate, StageKind::Gate]);
        let kinds: Vec<StageKind> = spec.kinds().collect();
        assert_eq!(kinds, vec![StageKind::Gate, StageKind::Limiter]);

        let spec = ChainSpec::custom(&[]);
        let kinds: Vec<StageKind> = spec.kinds().collect();
        assert_eq!(kinds, vec![StageKind::Limiter]);
        assert!(!spec.is_empty());
    }

    #[test]
    fn the_named_specs_differ_where_they_say_they_do() {
        assert!(!ChainSpec::podcast().contains(StageKind::Gate));
        let broadcast: Vec<StageKind> = ChainSpec::broadcast().kinds().collect();
        let compressor = broadcast
            .iter()
            .position(|k| *k == StageKind::Compressor)
            .expect("compressor");
        let eq = broadcast
            .iter()
            .position(|k| *k == StageKind::Eq)
            .expect("eq");
        assert!(compressor < eq, "broadcast compresses before it voices");

        let streaming: Vec<StageKind> = ChainSpec::streaming().kinds().collect();
        let compressor = streaming
            .iter()
            .position(|k| *k == StageKind::Compressor)
            .expect("compressor");
        let deesser = streaming
            .iter()
            .position(|k| *k == StageKind::DeEsser)
            .expect("de-esser");
        assert!(
            compressor < deesser,
            "streaming de-esses what the compressor brought up"
        );
    }

    #[test]
    fn names_are_matched_without_fuss_and_unknown_ones_are_refused() {
        assert_eq!(ChainSpec::by_name(" Podcast "), Some(ChainSpec::podcast()));
        assert_eq!(
            ChainSpec::by_name("STREAMING"),
            Some(ChainSpec::streaming())
        );
        assert_eq!(ChainSpec::by_name("radio"), None);
        assert_eq!(
            ChainSpec::custom(&[StageKind::Gate]).name(),
            None,
            "a custom ordering has no name"
        );
    }

    #[test]
    fn every_kind_builds_at_every_rate_the_port_supports() {
        for rate in [16_000.0, 44_100.0, 48_000.0, 96_000.0] {
            for kind in StageKind::ALL {
                let stage = Stage::build(kind, rate);
                assert_eq!(stage.kind(), kind);
            }
        }
    }
}
