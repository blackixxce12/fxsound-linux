//! The de-esser: a compressor that can only hear the top of the band.
//!
//! Sibilance is the one thing on a voice that a broadband compressor makes *worse*. An /s/ is
//! short, loud and narrow, so it either drives the compressor and ducks the whole word behind it,
//! or it sits under the threshold and stays sharp. Splitting it off and compressing only the split
//! is the fix every reference implementation uses.
//!
//! ```text
//!        ┌─ low  ─────────────────────────┐
//! in ──┤                                  ├──► out
//!        └─ high ─► compressor ───────────┘
//! ```
//!
//! **The crossover is Linkwitz–Riley, not one Butterworth pair.** A single second-order Butterworth
//! low-pass and high-pass at the same corner do not sum back to the signal they were split from —
//! their numerators cancel at the corner and the sum has a notch exactly where a voice's presence
//! lives. Cascading each into a fourth-order Linkwitz–Riley makes the sum `(ωc⁴ + ω⁴)/(ωc⁴ + ω⁴)`,
//! which is unity at every frequency, so a de-esser that is not reducing anything is audibly not
//! there. That is the property [`the_split_sums_back_to_what_went_in`] measures.
//!
//! **The corner is prewarped here, at the call site.** [`calc_butterworth_highpass`] is a faithful
//! port and deliberately does not prewarp, which leaves its realised corner a few percent low — a
//! property `Fidelity`'s golden coefficients depend on and nothing here may change. A de-esser
//! frequency is a number a person reads and sets, so this module asks the port for the corner that
//! lands on the one the preset wrote down. The port is untouched; only the request is.
//!
//! **Two modes.** `Classic` builds the corner the preset asks for, or nothing when the rate
//! cannot carry it — a 5500 Hz split needs about 18 kHz of sample rate, and a Bluetooth headset
//! at 16 kHz does not have it. `Adaptive` chooses the corner relative to the source's bandwidth,
//! `min(requested, bandwidth / 4)`, so that headset still gets a de-esser at 4 kHz instead of a
//! stage that says it is unavailable; below 12 kHz of bandwidth there is no sibilance band left
//! to split off and the stage stands aside. Moving the corner moves the band the threshold is
//! measured in, so the threshold is offset by the change in the band's *relative* width — a
//! band that covers more of the spectrum collects more of a broadband sibilant and would trip
//! earlier at the same number. `Classic` carries no offset, so a preset means in 0.4.0 exactly
//! what it meant in 0.3.0.
//!
//! Real-time safe: four sections and a compressor, all fixed, none of it allocating.

use crate::biquad::{
    MAX_CHANNELS, Real, Section, calc_butterworth_highpass, calc_butterworth_lowpass,
};
use crate::input::Compressor;
use crate::input::detector::Detection;
use crate::input::prewarped;
use crate::input::processor::{AudioProcessor, ProcessContext, StageMeter};
use crate::input::sane_rate;
use fxsound_core::DeEsserMode;
use fxsound_core::messages::InputDspParams;

/// How hard the band is compressed once it crosses. Not a preset field: the preset table carries a
/// frequency and a threshold, and a ratio is what makes those two numbers mean an amount.
const RATIO: Real = 4.0;
/// Fast enough to be inside a sibilant, which runs 50–150 ms.
const ATTACK_MS: Real = 5.0;
const RELEASE_MS: Real = 60.0;

/// In the adaptive mode the corner sits at this fraction of the source's bandwidth: 4 kHz for
/// a 16 kHz headset profile, 6 kHz at 24. Under [`crate::input::MAX_CORNER_FRACTION`], so the
/// crossover can always be built where it lands.
pub const ADAPTIVE_FRACTION: Real = 0.25;
/// Below this much bandwidth there is no sibilance band to split off, and the adaptive mode
/// stands aside rather than de-ess a voice's presence.
pub const MIN_ADAPTIVE_BANDWIDTH: Real = 12_000.0;
/// The rate every preset is voiced at: the reference the adaptive threshold offset is measured
/// against.
const REFERENCE_RATE: Real = 48_000.0;

pub struct DeEsser {
    /// Two cascaded second-order sections per band: a fourth-order Linkwitz–Riley crossover.
    low: [Section; 2],
    high: [Section; 2],
    band: Compressor,
    sample_rate: Real,
    /// The corner the preset asked for.
    frequency: Real,
    /// The corner actually built, when active.
    realised: Real,
    /// The threshold the preset asked for, before the adaptive offset.
    threshold_db: Real,
    offset_db: Real,
    mode: DeEsserMode,
    /// What the audio crate learned about the source: `None` means the stream rate is all
    /// there is to know.
    source_rate: Option<Real>,
    active: bool,
    enabled: bool,
}

impl std::fmt::Debug for DeEsser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeEsser")
            .field("sample_rate", &self.sample_rate)
            .field("frequency", &self.frequency)
            .field("realised", &self.realised)
            .field("mode", &self.mode)
            .field("source_rate", &self.source_rate)
            .field("offset_db", &self.offset_db)
            .field("active", &self.active)
            .field("enabled", &self.enabled)
            .field("band", &self.band)
            .finish()
    }
}

impl DeEsser {
    /// The default is where most of the preset table sits: 5500 Hz, −22 dB.
    #[must_use]
    pub fn new(sample_rate: Real) -> Self {
        let sample_rate = sane_rate(sample_rate);
        let mut band = Compressor::new(sample_rate);
        band.set_threshold_db(-22.0);
        band.set_ratio(RATIO);
        band.set_times(ATTACK_MS, RELEASE_MS);
        // RMS, like the compressor, so that a threshold means one thing everywhere in the chain —
        // the preset table's numbers are RMS thresholds and this stage is in it. The cost is the
        // window: the first few milliseconds of a sibilant go by before the stage has seen it. A
        // sibilant is 50–150 ms long, so a tenth of the shortest one is a price worth the
        // consistency; peak detection here would be faster and would make this one threshold
        // incomparable with every other threshold in the set.
        band.set_detection(Detection::Rms);

        let mut deesser = Self {
            low: [Section::new(); 2],
            high: [Section::new(); 2],
            band,
            sample_rate,
            frequency: 5500.0,
            realised: 0.0,
            threshold_db: -22.0,
            offset_db: 0.0,
            mode: DeEsserMode::Classic,
            source_rate: None,
            active: false,
            enabled: true,
        };
        deesser.design();
        deesser
    }

    /// Switch the stage in or out. A transition resets it, so it does not come back mid-release.
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled != on {
            self.enabled = on;
            self.reset();
        }
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Classic or adaptive. Redesigns, and resets only if the corner actually moved — a mode
    /// change at 48 kHz with the usual corner moves nothing and must not click.
    pub fn set_mode(&mut self, mode: DeEsserMode) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        self.redesign_and_reset_if_moved();
    }

    #[must_use]
    pub const fn mode(&self) -> DeEsserMode {
        self.mode
    }

    /// What the audio crate knows about the source's own rate — `audio.rate` when a device
    /// publishes it, 16 kHz for a Bluetooth headset profile — or `None` when the stream rate is
    /// the best information there is. Read by the adaptive mode only.
    pub fn set_source_rate(&mut self, rate: Option<Real>) {
        let rate = rate.filter(|r| r.is_finite() && *r > 0.0);
        if rate == self.source_rate {
            return;
        }
        self.source_rate = rate;
        self.redesign_and_reset_if_moved();
    }

    #[must_use]
    pub const fn source_rate(&self) -> Option<Real> {
        self.source_rate
    }

    fn redesign_and_reset_if_moved(&mut self) {
        let (was_active, was_at) = (self.active, self.realised);
        self.design();
        if self.active != was_active || self.realised != was_at {
            self.reset();
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: Real) {
        let sample_rate = sane_rate(sample_rate);
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.band.set_sample_rate(sample_rate);
        self.design();
        self.reset();
    }

    /// Where the split sits, in hertz. 5500 to 6000 is where the preset set lives; a higher voice
    /// wants the higher end of that.
    pub fn set_frequency(&mut self, hz: Real) {
        let hz = if hz.is_finite() && hz > 0.0 {
            hz
        } else {
            5500.0
        };
        if hz == self.frequency {
            return;
        }
        self.frequency = hz;
        self.design();
        self.reset();
    }

    /// The level, in dBFS, at which the band starts being compressed. This is a level **in the
    /// band**, not in the whole signal, which is why it can sit at −22 dB without touching a voice
    /// that peaks at −6. In the adaptive mode the band's own offset is added on the way in.
    pub fn set_threshold_db(&mut self, db: Real) {
        self.threshold_db = if db.is_finite() { db.min(0.0) } else { -22.0 };
        self.band
            .set_threshold_db(self.threshold_db + self.offset_db);
    }

    /// Whether the stage is running: switched on, and its crossover could be built at this rate.
    ///
    /// `false` means the stage is passing the signal through untouched, which is the honest
    /// degradation: the alternative is splitting well below where the preset asked and de-essing a
    /// voice's presence instead of its sibilance. A caller that shows the user which stages are
    /// running should read this.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.enabled && self.active
    }

    /// Whether the crossover could be built at this rate, whether or not the stage is switched on.
    #[must_use]
    pub const fn is_buildable(&self) -> bool {
        self.active
    }

    /// The corner asked for, in hertz.
    #[must_use]
    pub const fn frequency(&self) -> Real {
        self.frequency
    }

    /// The corner actually built, in hertz: the one asked for in the classic mode, possibly a
    /// lower one in the adaptive mode. Zero when the crossover could not be built.
    #[must_use]
    pub const fn realised_hz(&self) -> Real {
        if self.active { self.realised } else { 0.0 }
    }

    /// What the adaptive mode added to the threshold, in dB: `10·log10` of the ratio between the
    /// band's relative width as built and as the preset meant it at 48 kHz. Zero in the classic
    /// mode.
    #[must_use]
    pub const fn threshold_offset_db(&self) -> Real {
        self.offset_db
    }

    fn design(&mut self) {
        self.active = false;
        self.realised = 0.0;
        self.offset_db = 0.0;

        let corner = match self.mode {
            DeEsserMode::Classic => Some(self.frequency),
            DeEsserMode::Adaptive => {
                let bandwidth = self
                    .source_rate
                    .map_or(self.sample_rate, |rate| rate.min(self.sample_rate));
                (bandwidth >= MIN_ADAPTIVE_BANDWIDTH)
                    .then(|| self.frequency.min(ADAPTIVE_FRACTION * bandwidth))
            }
        };
        let Some((corner, request)) =
            corner.and_then(|corner| prewarped(self.sample_rate, corner).map(|r| (corner, r)))
        else {
            self.band.set_threshold_db(self.threshold_db);
            return;
        };
        self.active = true;
        self.realised = corner;
        if self.mode == DeEsserMode::Adaptive {
            let built = (self.sample_rate / 2.0 - corner) / (self.sample_rate / 2.0);
            let meant = (REFERENCE_RATE / 2.0 - self.frequency) / (REFERENCE_RATE / 2.0);
            if built > 0.0 && meant > 0.0 {
                let offset = 10.0 * (built / meant).log10();
                self.offset_db = if offset.is_finite() { offset } else { 0.0 };
            }
        }
        self.band
            .set_threshold_db(self.threshold_db + self.offset_db);

        let low = calc_butterworth_lowpass(self.sample_rate, request);
        let high = calc_butterworth_highpass(self.sample_rate, request);
        for section in &mut self.low {
            section.coeffs = low;
        }
        for section in &mut self.high {
            section.coeffs = high;
        }
    }

    pub fn reset(&mut self) {
        for section in &mut self.low {
            section.reset();
        }
        for section in &mut self.high {
            section.reset();
        }
        self.band.reset();
    }

    /// What a gain-reduction meter shows for the band, in dB, as a positive number.
    #[must_use]
    pub fn reduction_db(&self, channel: usize) -> Real {
        self.band.reduction_db(channel)
    }

    /// Split one sample into its two bands. Private, and the reason the tests live in this module:
    /// the crossover's own behaviour is worth measuring separately from what is done between the
    /// split and the sum.
    #[inline]
    fn split(&mut self, channel: usize, x: Real) -> (Real, Real) {
        let mut low = x;
        for section in &mut self.low {
            low = section.tick_general(channel, low);
        }
        let mut high = x;
        for section in &mut self.high {
            high = section.tick_general(channel, high);
        }
        (low, high)
    }

    /// One interleaved frame, in place.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut [Real]) {
        if !self.active {
            return;
        }
        let used = frame.len().min(MAX_CHANNELS);
        let mut low = [0.0; MAX_CHANNELS];
        let mut high = [0.0; MAX_CHANNELS];
        for (channel, &sample) in frame.iter().enumerate().take(used) {
            let (l, h) = self.split(channel, sample);
            low[channel] = l;
            high[channel] = h;
        }

        self.band.process_frame(&mut high[..used]);

        for (channel, sample) in frame.iter_mut().enumerate().take(used) {
            *sample = low[channel] + high[channel];
        }
    }

    /// A whole interleaved block, in place.
    pub fn process(&mut self, buffer: &mut [Real], channels: usize) {
        if channels == 0 || buffer.is_empty() || !self.active {
            return;
        }
        for frame in buffer.chunks_exact_mut(channels) {
            self.process_frame(frame);
        }
    }
}

impl AudioProcessor for DeEsser {
    fn prepare(&mut self, sample_rate: Real) {
        self.set_sample_rate(sample_rate);
    }

    fn apply(&mut self, params: &InputDspParams) {
        self.set_enabled(params.deesser_on);
        self.set_frequency(params.deesser_hz);
        self.set_threshold_db(params.deesser_threshold_db);
        self.set_mode(params.deesser_mode);
    }

    fn reset(&mut self) {
        DeEsser::reset(self);
    }

    fn is_active(&self) -> bool {
        DeEsser::is_active(self)
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn process(&mut self, buffer: &mut [Real], ctx: &ProcessContext) {
        if self.enabled {
            DeEsser::process(self, buffer, ctx.channels);
        }
    }

    fn meter(&self) -> StageMeter {
        StageMeter {
            reduction_db: self.reduction_db(0),
            running: self.is_active(),
            aux: self.realised_hz(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::detector::{db_to_linear, linear_to_db};

    const FS: Real = 48_000.0;

    /// The amplitude of one frequency in a block, by correlating against it. Enough to tell what
    /// happened to a two-tone signal without pulling in a transform.
    fn amplitude_at(samples: &[Real], hz: Real) -> Real {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &x) in samples.iter().enumerate() {
            let phase = n as Real * std::f32::consts::TAU * hz / FS;
            re += x * phase.cos();
            im -= x * phase.sin();
        }
        let n = samples.len() as Real;
        2.0 * (re * re + im * im).sqrt() / n
    }

    fn run(deesser: &mut DeEsser, input: &[Real]) -> Vec<Real> {
        input
            .iter()
            .map(|&x| {
                let mut frame = [x];
                deesser.process_frame(&mut frame);
                frame[0]
            })
            .collect()
    }

    fn tone(hz: Real, amplitude: Real, frames: usize) -> Vec<Real> {
        (0..frames)
            .map(|n| (n as Real * std::f32::consts::TAU * hz / FS).sin() * amplitude)
            .collect()
    }

    #[test]
    fn the_split_sums_back_to_what_went_in() {
        // The property a single Butterworth pair does not have. Quiet enough that the compressor
        // never engages, so this measures the crossover alone.
        let mut deesser = DeEsser::new(FS);
        deesser.set_frequency(5500.0);
        for hz in [80.0, 300.0, 1_000.0, 3_000.0, 5_500.0, 8_000.0, 14_000.0] {
            let mut deesser = DeEsser::new(FS);
            deesser.set_frequency(5500.0);
            deesser.set_threshold_db(0.0);
            let input = tone(hz, 0.02, 12_000);
            let out = run(&mut deesser, &input);
            // Skip the filter's start-up.
            let settled = &out[4_000..];
            let gain = linear_to_db(amplitude_at(settled, hz)) - linear_to_db(0.02);
            assert!(
                gain.abs() < 0.1,
                "{hz} Hz came back {gain} dB off — the crossover is not summing flat"
            );
        }
        assert!(deesser.is_active());
    }

    #[test]
    fn the_corner_lands_on_the_frequency_the_preset_asked_for() {
        // A Linkwitz–Riley crossover is 6 dB down in each band at the corner. Without the prewarp
        // the realised corner sits about 4% low at this frequency, which moves these two numbers
        // to roughly 0.54 and 0.46 — outside the tolerance below, which is the point of measuring.
        let mut deesser = DeEsser::new(FS);
        deesser.set_frequency(5500.0);
        let input = tone(5500.0, 1.0, 12_000);
        let (mut lows, mut highs) = (Vec::new(), Vec::new());
        for (n, &x) in input.iter().enumerate() {
            let (l, h) = deesser.split(0, x);
            if n >= 4_000 {
                lows.push(l);
                highs.push(h);
            }
        }
        let low = amplitude_at(&lows, 5500.0);
        let high = amplitude_at(&highs, 5500.0);
        assert!(
            (low - 0.5).abs() < 0.02,
            "the low band should be 6 dB down at the corner, it is at {low}"
        );
        assert!(
            (high - 0.5).abs() < 0.02,
            "the high band should be 6 dB down at the corner, it is at {high}"
        );
    }

    #[test]
    fn a_loud_voice_below_the_split_is_not_de_essed() {
        // The threshold is a level in the band. A −6 dB fundamental is far above −22 dB in
        // absolute terms and must still go through untouched.
        let mut deesser = DeEsser::new(FS);
        deesser.set_frequency(5500.0);
        deesser.set_threshold_db(-22.0);
        let input = tone(300.0, db_to_linear(-6.0), 24_000);
        let out = run(&mut deesser, &input);
        let gain = linear_to_db(amplitude_at(&out[8_000..], 300.0)) - (-6.0);
        assert!(
            gain.abs() < 0.1,
            "the body of the voice was compressed by {gain} dB"
        );
        assert!(
            deesser.reduction_db(0) < 0.1,
            "a 300 Hz tone drove the de-esser: {} dB",
            deesser.reduction_db(0)
        );
    }

    #[test]
    fn the_sibilant_is_reduced_and_the_word_under_it_is_not() {
        // Both at once, which is the only way to show the bands are actually separate.
        let mut deesser = DeEsser::new(FS);
        deesser.set_frequency(5500.0);
        deesser.set_threshold_db(-30.0);
        let body = tone(300.0, db_to_linear(-10.0), 24_000);
        let ess = tone(9_000.0, db_to_linear(-10.0), 24_000);
        let input: Vec<Real> = body.iter().zip(&ess).map(|(a, b)| a + b).collect();
        let out = run(&mut deesser, &input);

        let settled = &out[8_000..];
        let body_gain = linear_to_db(amplitude_at(settled, 300.0)) - (-10.0);
        let ess_gain = linear_to_db(amplitude_at(settled, 9_000.0)) - (-10.0);
        assert!(
            body_gain.abs() < 0.2,
            "the body moved by {body_gain} dB while the sibilant was being caught"
        );
        assert!(
            ess_gain < -8.0,
            "the sibilant was only reduced by {ess_gain} dB"
        );
    }

    #[test]
    fn the_crossover_leak_caps_how_much_a_de_esser_can_take_off() {
        // A measured limit of the topology, pinned so it is known rather than discovered while
        // voicing a preset. The low band is 4th order, not a brick wall: at 9 kHz against a 5500 Hz
        // corner it still passes `1/(1+(f/fc)⁴)` — about a tenth of the signal, −18.2 dB — and that
        // tenth is never compressed. So no threshold buys unlimited de-essing. Fifty decibels of
        // threshold here buy six, and the last thirty of those buy one: the numbers converge on the
        // leak, they do not keep going. (Between them the residue partly cancels the leak, which is
        // why the middle reading is a little deeper than the leak alone.)
        //
        // Which is fine — a de-esser that takes 20 dB off is a lisp — but a preset cannot reach for
        // more by lowering the threshold, and now nobody has to find that out by ear.
        let mut floor = Vec::new();
        for threshold in [-40.0, -60.0, -90.0] {
            let mut deesser = DeEsser::new(FS);
            deesser.set_frequency(5500.0);
            deesser.set_threshold_db(threshold);
            let input = tone(9_000.0, db_to_linear(-10.0), 24_000);
            let out = run(&mut deesser, &input);
            floor.push(-(linear_to_db(amplitude_at(&out[8_000..], 9_000.0)) - (-10.0)));
        }
        assert!(
            floor.iter().all(|&reduction| reduction < 22.0),
            "the reduction is not bounded by the leak: {floor:?}"
        );
        assert!(
            floor[2] - floor[1] < 2.0,
            "thirty more decibels of threshold should buy almost nothing: {floor:?}"
        );
    }

    #[test]
    fn a_rate_that_cannot_carry_the_corner_passes_the_signal_through_instead() {
        // The Nyquist guard. At a 16 kHz capture a 5500 Hz crossover cannot be built — asking for
        // it anyway would split at about 4.2 kHz and de-ess the voice's presence. The stage says so
        // and does nothing, which is a preset degrading honestly rather than lying.
        let mut deesser = DeEsser::new(16_000.0);
        deesser.set_frequency(5500.0);
        assert!(!deesser.is_active(), "the guard did not fire at 16 kHz");

        let input: Vec<Real> = (0..2_000).map(|n| (n as Real * 0.3).sin() * 0.5).collect();
        for (n, &x) in input.iter().enumerate() {
            let mut frame = [x];
            deesser.process_frame(&mut frame);
            assert!(
                (frame[0] - x).abs() < 1.0e-12,
                "an inactive de-esser changed sample {n}"
            );
        }

        // The same corner at a rate that can carry it builds normally.
        let mut deesser = DeEsser::new(48_000.0);
        deesser.set_frequency(5500.0);
        assert!(deesser.is_active());
    }

    #[test]
    fn channels_do_not_share_a_de_esser() {
        let mut deesser = DeEsser::new(FS);
        deesser.set_frequency(5500.0);
        deesser.set_threshold_db(-40.0);
        let ess = tone(9_000.0, db_to_linear(-6.0), 24_000);
        for &x in &ess {
            let mut frame = [x, 0.0];
            deesser.process_frame(&mut frame);
        }
        assert!(
            deesser.reduction_db(0) > 6.0,
            "channel 0 was not caught: {}",
            deesser.reduction_db(0)
        );
        assert!(
            deesser.reduction_db(1).abs() < 0.01,
            "channel 1 was dragged down with it: {}",
            deesser.reduction_db(1)
        );
    }

    #[test]
    fn channels_beyond_the_supported_count_pass_through_untouched() {
        let mut deesser = DeEsser::new(FS);
        deesser.set_frequency(5500.0);
        let mut frame = [0.5; MAX_CHANNELS + 2];
        deesser.process_frame(&mut frame);
        for sample in &frame[MAX_CHANNELS..] {
            assert!(
                (sample - 0.5).abs() < 1.0e-12,
                "an unsupported channel was processed: {sample}"
            );
        }
    }

    fn adaptive(rate: Real, source: Option<Real>, requested: Real) -> DeEsser {
        let mut deesser = DeEsser::new(rate);
        deesser.set_frequency(requested);
        deesser.set_mode(DeEsserMode::Adaptive);
        deesser.set_source_rate(source);
        deesser
    }

    #[test]
    fn the_adaptive_corner_follows_the_bandwidth() {
        // The table from the design, one row each.
        let cases: [(Real, Option<Real>, Real, Real); 4] = [
            (16_000.0, None, 5_500.0, 4_000.0),
            (24_000.0, None, 5_500.0, 5_500.0),
            (48_000.0, None, 5_500.0, 5_500.0),
            (48_000.0, Some(16_000.0), 5_500.0, 4_000.0),
        ];
        for (rate, source, requested, want) in cases {
            let deesser = adaptive(rate, source, requested);
            assert!(deesser.is_active(), "{rate} Hz / {source:?}");
            assert_eq!(
                deesser.realised_hz(),
                want,
                "{rate} Hz with source {source:?}"
            );
            assert_eq!(deesser.frequency(), requested, "the request is kept");
        }
        // And a request the adaptive rule would not lower is not raised either.
        let deesser = adaptive(24_000.0, None, 6_000.0);
        assert_eq!(deesser.realised_hz(), 6_000.0);
    }

    #[test]
    fn below_twelve_kilohertz_of_bandwidth_the_adaptive_mode_stands_aside() {
        let deesser = adaptive(8_000.0, None, 5_500.0);
        assert!(!deesser.is_active());
        assert_eq!(deesser.realised_hz(), 0.0);
        let deesser = adaptive(48_000.0, Some(11_000.0), 5_500.0);
        assert!(!deesser.is_active(), "a narrow source on a wide stream");
        let deesser = adaptive(48_000.0, Some(12_000.0), 5_500.0);
        assert!(deesser.is_active(), "twelve kilohertz is the line");
        assert_eq!(deesser.realised_hz(), 3_000.0);
    }

    #[test]
    fn the_classic_mode_is_what_it_was() {
        let mut deesser = DeEsser::new(16_000.0);
        deesser.set_frequency(5500.0);
        assert!(
            !deesser.is_active(),
            "classic at 16 kHz cannot be built, as in 0.3.0"
        );
        assert_eq!(deesser.threshold_offset_db(), 0.0);
        deesser.set_source_rate(Some(16_000.0));
        assert!(
            !deesser.is_active(),
            "the source rate is the adaptive mode's business"
        );

        let mut deesser = DeEsser::new(48_000.0);
        deesser.set_frequency(5500.0);
        deesser.set_source_rate(Some(16_000.0));
        assert_eq!(deesser.realised_hz(), 5500.0);
        assert_eq!(deesser.threshold_offset_db(), 0.0);
    }

    #[test]
    fn the_threshold_offset_is_the_change_in_the_bands_relative_width() {
        // At 16 kHz the band is [4 kHz, 8 kHz], half the spectrum; the preset meant
        // [5.5 kHz, 24 kHz], 77 % of it. A broadband sibilant puts proportionally less into
        // the narrower band, so the threshold comes down by that ratio.
        let deesser = adaptive(16_000.0, None, 5_500.0);
        let want = 10.0 * ((4_000.0_f32 / 8_000.0) / (18_500.0 / 24_000.0)).log10();
        assert!(
            (deesser.threshold_offset_db() - want).abs() < 1.0e-4,
            "{} against {want}",
            deesser.threshold_offset_db()
        );
        assert!(want < 0.0 && want > -3.0, "{want}");
        // Where nothing moved, nothing is offset.
        let deesser = adaptive(48_000.0, None, 5_500.0);
        assert_eq!(deesser.threshold_offset_db(), 0.0);
    }

    #[test]
    fn the_offset_keeps_the_reduction_where_the_preset_put_it() {
        // Sibilance is broadband. A −12 dBFS noise fixture at 48 kHz through the classic stage
        // and at 16 kHz through the adaptive one has to draw the same reduction to within a
        // decibel: the band at 16 kHz holds less of the noise, and the offset is what makes up
        // the difference. Read from the band compressor's own gain, averaged over the last half
        // second, so the crossover's leak is not in the figure.
        let settled_reduction = |rate: Real, mode: DeEsserMode| {
            let mut deesser = DeEsser::new(rate);
            deesser.set_frequency(5500.0);
            deesser.set_threshold_db(-22.0);
            deesser.set_mode(mode);
            let mut state = 0x2545_f491_4f6c_dd1d_u64;
            let frames = rate as usize * 2;
            let amplitude = db_to_linear(-12.0) * 3.0_f32.sqrt();
            let mut sum = 0.0;
            let mut count = 0.0;
            for n in 0..frames {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let x = ((state >> 40) as Real / 8_388_608.0 - 1.0) * amplitude;
                let mut frame = [x];
                deesser.process_frame(&mut frame);
                if n > frames * 3 / 4 {
                    sum += deesser.reduction_db(0);
                    count += 1.0;
                }
            }
            assert!(deesser.is_active(), "{rate} Hz {mode:?}");
            sum / count
        };
        let reference = settled_reduction(48_000.0, DeEsserMode::Classic);
        let adapted = settled_reduction(16_000.0, DeEsserMode::Adaptive);
        assert!(
            reference > 3.0,
            "premise: the fixture drives the stage, {reference}"
        );
        assert!(
            (reference - adapted).abs() < 1.0,
            "48 kHz classic draws {reference:.2} dB, 16 kHz adaptive {adapted:.2}"
        );
    }

    #[test]
    fn a_mode_change_that_moves_nothing_does_not_click() {
        // Two stages fed the same continuing sibilant, one switched to adaptive at 48 kHz where
        // the corner stays where it was: sample-identical afterwards, because nothing was reset.
        let mut classic = DeEsser::new(FS);
        let mut switched = DeEsser::new(FS);
        let warm = tone(9_000.0, db_to_linear(-10.0), 12_000);
        for stage in [&mut classic, &mut switched] {
            stage.set_frequency(5500.0);
            let mut block = warm.clone();
            stage.process(&mut block, 1);
        }
        switched.set_mode(DeEsserMode::Adaptive);
        let next: Vec<Real> = (12_000..12_480)
            .map(|n| (n as Real * std::f32::consts::TAU * 9_000.0 / FS).sin() * db_to_linear(-10.0))
            .collect();
        let (mut a, mut b) = (next.clone(), next);
        classic.process(&mut a, 1);
        switched.process(&mut b, 1);
        assert_eq!(a, b);
    }

    #[test]
    fn switched_off_it_passes_the_signal_through_and_says_so() {
        let mut deesser = DeEsser::new(FS);
        deesser.set_threshold_db(-60.0);
        deesser.set_enabled(false);
        assert!(!deesser.is_active());
        assert!(deesser.is_buildable());
        let ctx = ProcessContext {
            sample_rate: FS,
            channels: 1,
            voice_probability: 0.0,
        };
        let input = tone(9_000.0, 0.3, 4_800);
        let mut block = input.clone();
        AudioProcessor::process(&mut deesser, &mut block, &ctx);
        assert_eq!(block, input);
    }

    #[test]
    fn reset_clears_a_filter_that_a_bad_sample_latched() {
        // The crossover sections carry no guard of their own, in common with every other filter in
        // the crate: the engine sanitises its input block and resets the chain if its output ever
        // stops being finite. This is that second half — a stage that has been poisoned must come
        // back clean, or the reset would be a gesture.
        let mut deesser = DeEsser::new(FS);
        deesser.set_frequency(5500.0);
        let mut frame = [Real::INFINITY];
        deesser.process_frame(&mut frame);
        let mut frame = [0.1];
        deesser.process_frame(&mut frame);
        assert!(!frame[0].is_finite(), "the premise of this test changed");

        deesser.reset();
        let input = tone(300.0, 0.1, 12_000);
        let out = run(&mut deesser, &input);
        assert!(
            out[4_000..].iter().all(|x| x.is_finite()),
            "reset did not clear the crossover"
        );
        let gain = linear_to_db(amplitude_at(&out[4_000..], 300.0)) - linear_to_db(0.1);
        assert!(gain.abs() < 0.1, "after the reset it is {gain} dB off");
    }
}
