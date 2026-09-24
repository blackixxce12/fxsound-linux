//! Parameter glides: how a change the user makes reaches the audio without a click.
//!
//! The original writes every parameter straight into the float array the audio thread reads
//! (`Comsftwr.c:69-94`) and designs every coefficient at once (`dfxpComm.cpp:906-920` rewrites the
//! Bass biquad's five in a row), so a gain or a filter changes between one sample and the next. A
//! 2 dB Master Gain step is a 2 dB step in the waveform, a click; dragging Bass or an equalizer
//! band steps the filter at the GUI's frame rate, a zipper on the bass (audit report #11,
//! `docs/spec/10-dsp-effects.md` open question 7). Two tools here take it out:
//!
//! - **[`Ramp`]** for a scalar gain: a straight line from wherever the gain is to the new value,
//!   [`GLIDE_SECONDS`] long, restarted from wherever it has got to when the value moves again.
//!   Master gain and balance, the effects' wet, dry, drive and widening gains, Dynamic Boost's
//!   target, Volume Leveling letting its gain go when it is switched off, and the equalizer's
//!   switch, which fades the whole GraphicEq block in or out ([`crate::Engine`]), all go through
//!   one.
//! - **[`FadingSection`]** for a biquad: the old design and the new one run side by side for
//!   [`GLIDE_SECONDS`], and the output crossfades from one to the other. The equalizer's bands and
//!   Bass are built on it. A new band count, whose sections do not line up with the old ones,
//!   crossfades the same way a whole ladder at a time: the old cascade plays on beside the new one
//!   and the output moves from one to the other ([`crate::GraphicEq`]).
//!
//! # Why a crossfade and not interpolated coefficients
//!
//! Interpolating the five coefficients is cheaper and every design on the way is stable — the
//! region of stable `(a1, a2)` is a triangle, and a straight line between two points in it stays
//! in it — but the sections here are transposed direct form II, whose state words mean different
//! things under different coefficients, and at the bottom of the spectrum that matters. A pole
//! pair at 62.5 Hz sits so close to `z = 1` that the recursive half of the section, `1/A(z)`,
//! amplifies DC some fifteen thousand times; a coefficient that moves a little every sample leaves
//! a little error in the state every sample, and that gain turns it into a transient. Measured on
//! one section, prototyped both ways, with a 25 Hz tone under a 62.5 Hz band dragged from 0 to
//! +12 dB at the GUI's rate: interpolation overshot the tone by 2 dB (0.91 for a steady 0.72)
//! and put more energy above 80 Hz than no smoothing at all (−51.7 against −55.4 dB RMS), and
//! starting it from the target's own poles made the overshoot 7 dB. A crossfade runs two filters
//! that never change, so there is no such error to amplify: the same drag came out at −67.2 dB,
//! the tone never above its steady 0.72, and on a +12 → −12 dB jump the zipper above 300 Hz fell
//! from −28.6 dB peak to −66.4. Both filters are fixed and stable, so the sum is too, whatever the
//! parameters do and however often: nothing about stability depends on how the parameters move.
//!
//! What a crossfade costs is a second section per band while it runs, and a queue: a new design
//! that arrives mid-fade waits for the fade to finish, so a band dragged continuously follows the
//! drag at most every [`GLIDE_SECONDS`]. The equalizer and Bass redesign only when the user moves
//! something, so outside a drag neither costs anything: a section that is not fading runs the one
//! loop it always ran, on the design it would have had without the fade, and what it hands back is
//! that design's output.
//!
//! # When nothing glides
//!
//! A stage that has not been heard since it was built or cleared has nothing to glide from, so
//! whoever owns it lands a parameter at once until audio has passed: [`crate::Engine`] for the
//! gain stage, [`crate::GraphicEq`] for its bands, [`crate::Chain`] for the effects (an effect
//! that is switched off is still heard, as the signal it lets through, so the chain decides and not
//! the effect). That keeps the first snapshot of a stream, and a format change, exact from the
//! first sample, which is also why every golden vector in the crate is unchanged: each one builds
//! its stage, applies its parameters and only then processes.
//!
//! A stage its owner stops running is not heard either: the equalizer and the leveller while
//! FxSound is off, and the whole GraphicEq block once the equalizer's switch has faded it out. Only
//! running a stage plays its glides, so one begun or waiting while the stage is left out would
//! wait for it to come back and then play 20 ms of a setting the listener last heard before the
//! switch — a band moved with FxSound off came back at +12 dB for 20 ms, a Volume Leveling set to
//! 0 came back lifting by 13 dB. So the owner tells such a stage it was left out
//! ([`crate::GraphicEq::sit_out`], [`crate::leveller::VolumeLeveller::sit_out`], and the engine
//! settles its own gain stage): every glide lands, and until the stage runs again a change lands
//! at once. The effect chain needs no telling: FxSound off clears it, as the original does.
//!
//! The power switch itself is the one control that still acts between two samples: it is the
//! listener's comparison against the unprocessed sound, and a bypass that faded would mix the
//! processed signal, Dynamic Boost's look-ahead behind, with the dry one for 20 ms.

use crate::biquad::{BiquadCoeffs, Real, Section};

/// How long a glide or a crossfade takes: 20 ms, the top of the 10–20 ms the spec asks for.
///
/// The longer end, because what a glide has to hide is a change at the GUI's frame rate: a slider
/// dragged at 60 frames a second moves every 16.7 ms, and a glide shorter than that would finish
/// and sit still before each new value arrived, a staircase of short ramps.
pub const GLIDE_SECONDS: Real = 0.020;

/// A glide's length in frames at `sample_rate`: 960 at 48 kHz. Never zero, whatever the rate.
#[must_use]
pub fn glide_frames(sample_rate: Real) -> u32 {
    let frames = (sample_rate * GLIDE_SECONDS).round();
    if frames.is_finite() && frames >= 1.0 {
        // Saturating; a rate that could overflow this is refused long before it gets here.
        frames as u32
    } else {
        1
    }
}

/// A scalar that moves to a new value in a straight line instead of jumping.
///
/// [`Ramp::advance`] is called once per frame by whoever applies the value; while nothing is moving
/// it returns the value it was last set to, exactly, so a stage that holds its value computes what
/// it computed before glides existed, bit for bit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ramp {
    value: Real,
    target: Real,
    step: Real,
    left: u32,
}

impl Ramp {
    /// A ramp sitting at `value`.
    #[must_use]
    pub const fn new(value: Real) -> Self {
        Self {
            value,
            target: value,
            step: 0.0,
            left: 0,
        }
    }

    /// Head for `target`, arriving `frames` frames from now, from wherever the value is.
    ///
    /// Asking for the value it is already heading to changes nothing, so a snapshot that repeats a
    /// value neither restarts nor slows a glide. A value that is not a number is ignored: the ramp
    /// keeps heading where it was, rather than carrying a NaN into every sample after it.
    pub fn glide_to(&mut self, target: Real, frames: u32) {
        if target == self.target || !target.is_finite() {
            return;
        }
        self.target = target;
        if frames == 0 {
            self.settle();
            return;
        }
        self.step = (target - self.value) / frames as Real;
        self.left = frames;
    }

    /// Land on the target at once, as if the glide had already run.
    pub const fn settle(&mut self) {
        self.value = self.target;
        self.step = 0.0;
        self.left = 0;
    }

    /// The value for the next frame. The last frame of a glide is the target exactly.
    #[inline(always)]
    pub fn advance(&mut self) -> Real {
        if self.left > 0 {
            self.left -= 1;
            self.value = if self.left == 0 {
                self.target
            } else {
                self.value + self.step
            };
        }
        self.value
    }

    /// Whether a glide is under way.
    #[must_use]
    pub const fn is_gliding(&self) -> bool {
        self.left > 0
    }

    /// How many more calls to [`Ramp::advance`] the glide takes; the last of them returns the
    /// target. Zero when nothing is moving.
    #[must_use]
    pub const fn frames_left(&self) -> u32 {
        self.left
    }

    /// What the `frames`-th call to [`Ramp::advance`] from now will return, give or take the
    /// rounding of the steps it adds one at a time: for a caller that has to look along the glide
    /// before it plays it.
    #[must_use]
    pub fn value_after(&self, frames: u32) -> Real {
        if frames >= self.left {
            self.target
        } else {
            self.value + self.step * frames as Real
        }
    }

    /// The value the last frame played.
    #[must_use]
    pub const fn value(&self) -> Real {
        self.value
    }

    /// Where the ramp is heading, or sits.
    #[must_use]
    pub const fn target(&self) -> Real {
        self.target
    }
}

/// A peaking section that changes design by crossfading from the old one to the new one.
///
/// For the parametric designs whose `a1` equals `b1` ([`crate::biquad::calc_parametric`]): both
/// halves run [`Section::tick`]. A design that is `on == false` is the identity, so fading to it
/// fades the section out, and fading from it fades the section in — from rest, since a section that
/// was bypassed holds whatever it heard before it was, perhaps minutes ago (audit reports #9 and
/// #10). Fading out needs no special case either: the mix ends on the input, and from the next
/// frame the section is switched off and the input passes untouched.
///
/// See the module documentation for why this crossfades rather than interpolating coefficients.
#[derive(Clone, Copy, Debug)]
pub struct FadingSection {
    /// The design being faded in, or the only one when nothing is fading.
    live: Section,
    /// The design being faded out.
    previous: Section,
    /// Frames left in the crossfade; zero when none is running.
    left: u32,
    /// The crossfade's length in frames, and its reciprocal.
    length: u32,
    inverse_length: Real,
    /// A design asked for while a crossfade ran, faded to when it ends.
    pending: Option<BiquadCoeffs>,
}

impl FadingSection {
    /// A bypassed section at rest.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            live: Section::new(),
            previous: Section::new(),
            left: 0,
            length: 1,
            inverse_length: 1.0,
            pending: None,
        }
    }

    /// The design this section is heading for: the last one asked for.
    #[must_use]
    pub fn design(&self) -> BiquadCoeffs {
        self.pending.unwrap_or(self.live.coeffs)
    }

    /// Whether the section does anything to the signal: a design switched on, or a crossfade in
    /// either direction.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.live.coeffs.on || self.left > 0
    }

    /// Whether a crossfade is running.
    #[must_use]
    pub const fn is_fading(&self) -> bool {
        self.left > 0
    }

    /// Move to `design`, crossfading over `frames` frames.
    ///
    /// Mid-fade, the design waits for the running fade to finish; a later one replaces it, so only
    /// the newest waits. A design whose coefficients are not all numbers is refused: it could only
    /// fade a NaN into the output.
    pub fn set_design(&mut self, design: BiquadCoeffs, frames: u32) {
        if design == self.design() || !is_finite(&design) {
            return;
        }
        if self.left > 0 {
            // The running fade keeps its length; the waiting design takes it too.
            self.pending = Some(design);
        } else {
            self.length = frames.max(1);
            self.inverse_length = 1.0 / self.length as Real;
            self.begin(design);
        }
    }

    fn begin(&mut self, design: BiquadCoeffs) {
        self.pending = None;
        if design == self.live.coeffs {
            return;
        }
        if !design.on && !self.live.coeffs.on {
            // Bypassed either way: nothing to fade.
            self.live.coeffs = design;
            return;
        }
        self.previous = self.live;
        self.live = if self.previous.coeffs.on && design.on {
            self.previous.continued_as(design)
        } else {
            // In from the identity, from rest; or out to it, where the state is never read.
            let mut fresh = Section::new();
            fresh.coeffs = design;
            fresh
        };
        self.left = self.length;
    }

    /// Land on the last design asked for at once, as if every crossfade had already run.
    ///
    /// A section that lands on a design from the identity starts from rest, as it would have at
    /// the end of the fade; one that was running keeps its state, carried over as [`FadingSection`]
    /// carries it into a crossfade.
    pub fn settle(&mut self) {
        let design = self.design();
        self.pending = None;
        self.left = 0;
        self.live = if self.live.coeffs.on && design.on {
            self.live.continued_as(design)
        } else if design.on {
            let mut fresh = Section::new();
            fresh.coeffs = design;
            fresh
        } else {
            let mut off = self.live;
            off.coeffs = design;
            off
        };
    }

    /// Clear the history, landing on the last design asked for.
    pub fn reset(&mut self) {
        self.settle();
        self.live.reset();
        self.previous.reset();
    }

    /// The section a steady, un-faded block runs: the one design there is. Only meaningful while
    /// [`FadingSection::is_fading`] is false.
    #[inline(always)]
    pub const fn steady(&mut self) -> &mut Section {
        &mut self.live
    }

    /// One interleaved frame through the section, crossfading if a fade runs.
    ///
    /// The mix is linear — equal gain, not equal power — because the two designs hear the same
    /// input and answer it almost alike, so their outputs are correlated and an equal-power fade
    /// would bulge by up to 3 dB halfway.
    #[inline(always)]
    pub fn process_frame(&mut self, frame: &mut [Real]) {
        if self.left == 0 {
            if self.live.coeffs.on {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample = self.live.tick(channel, *sample);
                }
            }
            return;
        }
        self.left -= 1;
        let t = (self.length - self.left) as Real * self.inverse_length;
        let (new_on, old_on) = (self.live.coeffs.on, self.previous.coeffs.on);
        for (channel, sample) in frame.iter_mut().enumerate() {
            let x = *sample;
            let new = if new_on {
                self.live.tick(channel, x)
            } else {
                x
            };
            let old = if old_on {
                self.previous.tick(channel, x)
            } else {
                x
            };
            *sample = old + t * (new - old);
        }
        if self.left == 0
            && let Some(design) = self.pending
        {
            self.begin(design);
        }
    }
}

impl Default for FadingSection {
    fn default() -> Self {
        Self::new()
    }
}

fn is_finite(design: &BiquadCoeffs) -> bool {
    [design.b0, design.b1, design.b2, design.a1, design.a2]
        .iter()
        .all(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biquad::calc_parametric;

    #[test]
    fn a_ramp_reaches_its_target_exactly_in_the_frames_it_was_given() {
        let mut ramp = Ramp::new(1.0);
        ramp.glide_to(0.1, 960);
        let mut last = 1.0;
        for frame in 0..960 {
            let value = ramp.advance();
            assert!(
                value < last,
                "frame {frame} did not move towards the target"
            );
            last = value;
        }
        assert_eq!(last.to_bits(), 0.1_f32.to_bits());
        assert!(!ramp.is_gliding());
        assert_eq!(ramp.advance().to_bits(), 0.1_f32.to_bits());
    }

    #[test]
    fn a_ramp_redirected_mid_glide_starts_from_where_it_had_got_to() {
        let mut ramp = Ramp::new(0.0);
        ramp.glide_to(1.0, 100);
        for _ in 0..50 {
            ramp.advance();
        }
        let halfway = ramp.value();
        ramp.glide_to(0.0, 100);
        let first = ramp.advance();
        assert!(
            (first - halfway).abs() < 0.01,
            "{halfway} jumped to {first}"
        );
    }

    #[test]
    fn a_ramp_ignores_a_value_that_is_not_a_number() {
        let mut ramp = Ramp::new(0.5);
        for bad in [Real::NAN, Real::INFINITY, Real::NEG_INFINITY] {
            ramp.glide_to(bad, 10);
            assert_eq!(ramp.advance(), 0.5);
            assert!(!ramp.is_gliding());
        }
    }

    #[test]
    fn a_repeated_target_does_not_restart_the_glide() {
        let mut ramp = Ramp::new(0.0);
        ramp.glide_to(1.0, 10);
        for _ in 0..5 {
            ramp.advance();
        }
        ramp.glide_to(1.0, 10);
        for _ in 0..5 {
            ramp.advance();
        }
        assert!(!ramp.is_gliding());
        assert_eq!(ramp.value(), 1.0);
    }

    #[test]
    fn glides_are_twenty_milliseconds_at_every_rate() {
        assert_eq!(glide_frames(48_000.0), 960);
        assert_eq!(glide_frames(44_100.0), 882);
        assert_eq!(glide_frames(192_000.0), 3_840);
        assert_eq!(glide_frames(0.0), 1);
        assert_eq!(glide_frames(Real::NAN), 1);
    }

    fn tone(frames: usize) -> Vec<Real> {
        (0..frames)
            .map(|n| (n as Real * 62.5 * std::f32::consts::TAU / 48_000.0).sin() * 0.3)
            .collect()
    }

    #[test]
    fn a_section_faded_out_ends_as_the_identity_and_switches_off() {
        let mut section = FadingSection::new();
        section.set_design(calc_parametric(48_000.0, 62.5, 9.0, 1.6), 960);
        section.settle();
        let mut buffer = tone(4_800);
        for frame in buffer.as_chunks_mut::<1>().0 {
            section.process_frame(frame);
        }
        section.set_design(BiquadCoeffs::UNITY, 960);
        let input = tone(2_000);
        let mut buffer = input.clone();
        for frame in buffer.as_chunks_mut::<1>().0 {
            section.process_frame(frame);
        }
        assert!(!section.is_active());
        assert_eq!(&buffer[960..], &input[960..], "the tail is not the input");
    }

    #[test]
    fn a_design_asked_for_mid_fade_waits_and_only_the_newest_waits() {
        let mut section = FadingSection::new();
        let a = calc_parametric(48_000.0, 62.5, 3.0, 1.6);
        let b = calc_parametric(48_000.0, 62.5, 6.0, 1.6);
        let c = calc_parametric(48_000.0, 62.5, 9.0, 1.6);
        section.set_design(a, 100);
        section.set_design(b, 100);
        section.set_design(c, 100);
        assert_eq!(section.design(), c);
        let mut buffer = vec![0.1; 100];
        for frame in buffer.as_chunks_mut::<1>().0 {
            section.process_frame(frame);
        }
        assert!(section.is_fading(), "the waiting design did not start");
        let mut buffer = vec![0.1; 100];
        for frame in buffer.as_chunks_mut::<1>().0 {
            section.process_frame(frame);
        }
        assert!(!section.is_fading());
        assert_eq!(section.steady().coeffs, c);
    }

    #[test]
    fn a_design_that_is_not_a_number_is_never_faded_in() {
        let mut section = FadingSection::new();
        let good = calc_parametric(48_000.0, 1_000.0, 6.0, 1.6);
        section.set_design(good, 10);
        section.set_design(
            BiquadCoeffs {
                b0: Real::NAN,
                ..good
            },
            10,
        );
        assert_eq!(section.design(), good);
    }
}
