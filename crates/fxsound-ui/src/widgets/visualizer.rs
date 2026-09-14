//! The 960 × 120 spectrum strip above the Pro view's controls.
//!
//! Port of `FxVisualizer` (`fxsound/Source/GUI/FxVisualizer.cpp`, `.h:51-53`). The original is a
//! JUCE component that owns no DSP at all: it asks the controller for ten band levels, shifts them
//! through a small history, and fills one `Path` of a hundred rectangles with a single vertical
//! gradient. This widget does the same, and nothing else — the analysis lives in
//! `fxsound_dsp::SpectrumAnalyser` and arrives here as [`UiState::spectrum`].
//!
//! ## What one hundred bars mean
//!
//! There are ten *bands* (`FxController::NUM_SPECTRUM_BANDS`, `FxController.h:45`) and ten *bars*
//! per band (`FxVisualizer::NUM_BARS`, `FxVisualizer.h:53`). A band's ten bars are not ten
//! frequencies: they are the last five frames of that one band, mirrored about the middle bar, so
//! each band looks like a little ripple spreading outwards from its centre
//! (`FxVisualizer.cpp:107-129`, resolved in `docs/spec/04-equalizer-visualizer.md` §B6). The mirror
//! is deliberately off by one — bar 4 shows the previous frame while bar 6 shows the one before
//! that — which is what makes the animation appear to flow left to right. That asymmetry is
//! reproduced exactly in [`ripple`].
//!
//! ## Deliberate improvements over the original
//!
//! 1. **The animation is driven by elapsed time, not by timer ticks.** The original advances its
//!    history once per vblank callback that clears a fixed `1.0 / 30.0` gate
//!    (`FxVisualizer.cpp:58-70`), with a process-wide `static` holding the last timestamp, so the
//!    ripple speed follows whatever rate the callback happens to fire at. Here
//!    [`VisualizerAnimation::advance`] accumulates `Ui::input(|i| i.stable_dt)` and steps the
//!    history whenever [`FRAME_INTERVAL_SECS`] has passed, so a 60 Hz, 144 Hz or
//!    fractional-refresh Wayland output all show the same 30 Hz ripple and the same decay rate.
//! 2. **Idle fades instead of snapping.** `pause()` calls `reset()`, which zeroes all hundred
//!    entries in one frame (`FxVisualizer.cpp:82-105`) — the meter collapses to a flat line
//!    between two frames. Here the level envelope releases with [`RELEASE_TAU_SECS`], so the bars
//!    fall away over about a second and reach the same flat line. The end state is identical; only
//!    the transition differs.
//! 3. **A hold-and-release envelope in front of the history.** The original has no GUI-side
//!    smoothing or peak hold whatsoever (`docs/spec/04-equalizer-visualizer.md` §B4); all temporal
//!    shaping is the DSP's one-pole mean-square smoother. [`hold_release`] adds an envelope with an
//!    *instant* attack and a release equal to that same smoother's own decay — τ = 0.2 s on the
//!    square-rooted level. Because a one-pole cannot fall faster than its time constant, the
//!    envelope is a mathematical no-op while frames keep arriving, and only takes over when they
//!    stop: a stalled analyser, a device switch, or the power going off. That is the whole point of
//!    it — it is a graceful continuation of the DSP's decay, not a second smoother in series.
//!
//! Everything else — the bar pitch, the 0 → 0.01 floor, the `GraphHigh` → `GraphLow` → `GraphHigh`
//! gradient in component space, the 0.75 alpha while no audio flows, the desaturated palette when
//! the power is off — is reproduced as the original draws it.

use std::time::Duration;

use egui::{Color32, CornerRadius, Mesh, Pos2, Rect, Shape, Ui, pos2, vec2};

use fxsound_core::{NUM_SPECTRUM_BARS, SpectrumFrame};

use crate::layout::visualizer as geometry;
use crate::state::UiState;
use crate::theme::{FxColor, Palette};

/// Spectrum bands the DSP reports (`FxController::NUM_SPECTRUM_BANDS`, `FxController.h:45`).
///
/// `fxsound_core` calls the same ten `NUM_SPECTRUM_BARS`, because the original picks ten bars per
/// band and ten bands, and the two numbers are equal by coincidence rather than by construction.
pub const NUM_BANDS: usize = NUM_SPECTRUM_BARS;

/// History bars drawn per band (`FxVisualizer::NUM_BARS`, `FxVisualizer.h:53`).
pub const NUM_BARS: usize = geometry::NUM_BARS;

/// Rectangles in one frame (`FxVisualizer.cpp:146`).
pub const BAR_COUNT: usize = NUM_BANDS * NUM_BARS;

/// Left edge of the first bar (`FxVisualizer.cpp:143`).
pub const BAR_X0: f32 = 27.0;

/// Distance from one bar's left edge to the next (`FxVisualizer.cpp:144`).
///
/// `9.1` is written as a `double` there and narrowed to `float` on assignment, so the hundredth
/// bar lands a ten-thousandth of a point away from `27 + 99 × 9.1`. [`bar_lefts`] accumulates, as
/// the original does, rather than multiplying.
pub const BAR_PITCH: f32 = 9.1;

/// Width of one bar (`FxVisualizer.cpp:151`).
pub const BAR_WIDTH: f32 = 4.0;

/// Height of a full-scale bar (`FxVisualizer.cpp:149`: `band_value * 100.0f`).
pub const BAR_FULL_HEIGHT: f32 = 100.0;

/// The value substituted for an exactly-zero bar, so silence still draws a one-point line rather
/// than nothing at all (`FxVisualizer.cpp:148`).
pub const FLOOR_VALUE: f32 = 0.01;

/// Corner radius of the panel behind the bars (`FxVisualizer.cpp:136`).
pub const CORNER_RADIUS: f32 = crate::layout::PANEL_CORNER_RADIUS;

/// Height over which the gradient runs, in component coordinates (`FxVisualizer.cpp:185-189`).
///
/// The component is 120 points tall but the gradient is defined from y = 0 to y = 100; JUCE clamps
/// to the end colour past the last stop, so rows 100..120 stay `GraphHigh`.
pub const GRADIENT_SPAN: f32 = 100.0;

/// Where `GraphLow` sits in the ramp (`FxVisualizer.cpp:191`: `addColour(0.5f, GraphLow)`).
pub const GRADIENT_MID: f32 = 0.5;

/// Gradient alpha while audio is flowing (`FxVisualizer.cpp:182`).
pub const ALPHA_ACTIVE: f32 = 1.0;

/// Gradient alpha while it is not (`FxVisualizer.cpp:179`).
pub const ALPHA_IDLE: f32 = 0.75;

/// How often the history ripples, in seconds (`FxVisualizer.cpp:58`: `1.0 / 30.0`).
pub const FRAME_INTERVAL_SECS: f32 = 1.0 / 30.0;

/// Repaint interval once the meter has settled (`FxVisualizer.cpp:95`: `setFramesPerSecond(10)`).
pub const IDLE_INTERVAL_SECS: f32 = 0.1;

/// Release time constant of the level envelope, in seconds.
///
/// `SPECTRUM_DEFAULT_TIME_CONSTANT = 10.0` (`dsp/ptutil/include/spectrum.h:47`, used at
/// `dsp/ptutil/DspUtil/spectrum/spectrumSet.cpp:47`) is 10 nepers per second on the *mean square*,
/// i.e. τ = 0.1 s there and τ = 0.2 s on the square-rooted level the visualizer draws. Matching it
/// exactly is what makes [`hold_release`] invisible on live data — see the module docs.
pub const RELEASE_TAU_SECS: f32 = 0.2;

/// Below this a level is snapped to exactly zero.
///
/// An exponential release never reaches zero, but [`bar_height`] only substitutes the one-point
/// floor for an *exactly* zero bar, as the original does. Without a snap the idle meter would draw
/// sub-point slivers for ever instead of the flat line `reset()` produces, and would never stop
/// asking for 30 Hz repaints. [`FLOOR_VALUE`] is the natural place to snap: a level below it draws
/// a bar shorter than the line the original substitutes for silence, so there is nothing left to
/// see. Reached about 0.9 s after the signal stops (τ · ln 100).
pub const SETTLE_FLOOR: f32 = FLOOR_VALUE;

/// Largest number of history steps one [`VisualizerAnimation::advance`] will run.
///
/// The history is [`NUM_BARS`] deep, so ten steps replace every bar in it: after a stall longer
/// than a third of a second, catching up frame by frame would burn time producing a picture
/// identical to the one ten steps give.
const MAX_CATCHUP_STEPS: usize = NUM_BARS;

/// Upper bound on one frame's delta, so a resumed-from-suspend frame cannot dump a minute of
/// backlog into the accumulator.
const MAX_DELTA_SECS: f32 = 1.0;

/// Peak-hold and history state, kept across frames.
///
/// This is the whole of the widget's memory: a released level per band, the mirrored history per
/// band, and the time owed to the next 30 Hz step. It is pure data with no egui dependency, so the
/// ripple and the envelope can be — and are — tested without a rendering context.
#[derive(Debug, Clone, PartialEq)]
pub struct VisualizerAnimation {
    /// The held level of each band, after [`hold_release`].
    level: [f32; NUM_BANDS],
    /// Each band's ten history bars, newest in the middle (`FxVisualizer::band_graph_`).
    graph: [[f32; NUM_BARS]; NUM_BANDS],
    /// Seconds accumulated towards the next ripple step.
    since_step: f32,
}

impl Default for VisualizerAnimation {
    fn default() -> Self {
        Self::new()
    }
}

impl VisualizerAnimation {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            level: [0.0; NUM_BANDS],
            graph: [[0.0; NUM_BARS]; NUM_BANDS],
            since_step: 0.0,
        }
    }

    /// Clear every bar, as `FxVisualizer::reset` does (`FxVisualizer.cpp:99-105`).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The hundred bars in paint order: band 0's ten, then band 1's, and so on.
    #[must_use]
    pub fn bars(&self) -> &[f32] {
        self.graph.as_slice().as_flattened()
    }

    /// `true` once every bar and every held level is exactly zero — the state `reset()` leaves the
    /// original in, and the point at which the widget stops asking for 30 Hz repaints.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.level.iter().all(|&l| l == 0.0) && self.bars().iter().all(|&b| b == 0.0)
    }

    /// Fold one frame of elapsed time into the animation.
    ///
    /// `spectrum` is the latest band levels, `active` is [`UiState::audio_active`] (and the power
    /// state — see [`VisualizerWidget::show`]), and `dt` is the frame's duration in seconds. The
    /// envelope is updated every call from the real `dt`, which is what makes the decay
    /// frame-rate independent; the history ripples on the original's fixed 30 Hz grid.
    ///
    /// When `active` is false the bands are fed zero, which is how the original's
    /// `FxController::getSpectrumBandValues` behaves too — it substitutes a constant for every
    /// band while audio is off (`FxController.cpp:2896-2903`) and the component is reset anyway.
    pub fn advance(&mut self, spectrum: &SpectrumFrame, active: bool, dt: f32) {
        let dt = if dt.is_finite() {
            dt.clamp(0.0, MAX_DELTA_SECS)
        } else {
            0.0
        };

        let decay = decay_factor(dt, RELEASE_TAU_SECS);
        for (level, &raw) in self.level.iter_mut().zip(spectrum.iter()) {
            let incoming = if active { sanitise(raw) } else { 0.0 };
            *level = hold_release(*level, incoming, decay);
        }

        self.since_step += dt;
        let (steps, remainder) = steps_due(self.since_step, FRAME_INTERVAL_SECS, MAX_CATCHUP_STEPS);
        self.since_step = remainder;

        for _ in 0..steps {
            for (bars, &level) in self.graph.iter_mut().zip(self.level.iter()) {
                ripple(bars, level);
            }
        }
    }
}

/// The spectrum strip.
///
/// Purely a renderer: it reads [`UiState`], animates the scratch state it was handed, paints, and
/// emits no [`crate::state::UiAction`] because the original is not interactive — it takes no mouse
/// input at all (`FxVisualizer.cpp:38`: `setOpaque(false)` and no listener).
pub struct VisualizerWidget<'a> {
    state: &'a UiState,
    animation: &'a mut VisualizerAnimation,
}

impl<'a> VisualizerWidget<'a> {
    pub fn new(state: &'a UiState, animation: &'a mut VisualizerAnimation) -> Self {
        Self { state, animation }
    }

    /// Animate and draw into an exact rectangle — `layout::pro::visualizer()`, 960 × 120.
    ///
    /// Two independent flags drive the look, exactly as in the original:
    ///
    /// * `state.audio_active` (`FxController::isAudioProcessing`) sets the gradient alpha and
    ///   whether the bars move (`FxVisualizer.cpp:60-70,179-182`).
    /// * `state.controls_enabled()` (`Component::isEnabled`, set from the power state at
    ///   `FxProView.cpp:122`) desaturates the gradient and freezes the history — `update()` returns
    ///   immediately when disabled (`FxVisualizer.cpp:107-110`) and `enablementChanged()` resets it
    ///   (`FxVisualizer.cpp:158-169`).
    ///
    /// Note that the alpha follows audio activity even when the component is disabled, because
    /// `calcGradient` never looks at the enabled flag for it (`FxVisualizer.cpp:179-182`).
    pub fn show(self, ui: &mut Ui, rect: Rect, palette: Palette) {
        let enabled = self.state.controls_enabled();
        let audio_active = self.state.audio_active;
        let dt = ui.input(|i| i.stable_dt);

        self.animation
            .advance(&self.state.spectrum, audio_active && enabled, dt);

        paint(
            ui,
            rect,
            palette,
            enabled,
            audio_active,
            self.animation.bars(),
        );

        // 33.3 ms while anything is still moving, 100 ms once it has settled — the original's two
        // frame rates (`FxVisualizer.cpp:78,95`), expressed as a deadline instead of a timer.
        let interval = if audio_active || !self.animation.is_settled() {
            FRAME_INTERVAL_SECS
        } else {
            IDLE_INTERVAL_SECS
        };
        ui.ctx()
            .request_repaint_after(Duration::from_secs_f32(interval));
    }
}

/// Zero anything outside `0.0..=1.0`, as the GUI re-checks before storing a band value
/// (`FxVisualizer.cpp:116-119`).
///
/// A NaN fails the range test and is zeroed too, which the original's pair of comparisons does not
/// do — there it would poison the history for five frames.
#[must_use]
pub fn sanitise(value: f32) -> f32 {
    if (0.0..=1.0).contains(&value) { value } else { 0.0 }
}

/// One frame's worth of exponential decay: `exp(-dt / tau)`.
///
/// Multiplicative in `dt`, so N small steps decay exactly as much as one step of their sum — that
/// is the property that makes the animation frame-rate independent.
#[must_use]
pub fn decay_factor(dt: f32, tau: f32) -> f32 {
    if !dt.is_finite() || dt <= 0.0 || tau <= 0.0 {
        return 1.0;
    }
    (-dt / tau).exp()
}

/// The level envelope: follow a rising band instantly, release a falling one by `decay`.
///
/// `decay` comes from [`decay_factor`] with [`RELEASE_TAU_SECS`]. Values that fall below
/// [`SETTLE_FLOOR`] snap to exactly zero so that [`bar_height`] draws the original's one-point
/// floor line rather than an invisible sliver.
#[must_use]
pub fn hold_release(previous: f32, incoming: f32, decay: f32) -> f32 {
    let incoming = sanitise(incoming);
    let released = if previous.is_finite() {
        previous * decay
    } else {
        0.0
    };
    let held = incoming.max(released);
    if held < SETTLE_FLOOR { 0.0 } else { held }
}

/// How many ripple steps `elapsed` seconds has earned, and what is left over afterwards.
///
/// A backlog of `max` steps or more is dropped rather than replayed: `max` steps already replace
/// every bar in the history, so replaying more would cost time and change nothing.
#[must_use]
pub fn steps_due(elapsed: f32, interval: f32, max: usize) -> (usize, f32) {
    if !elapsed.is_finite() || elapsed <= 0.0 || interval <= 0.0 {
        return (0, 0.0);
    }
    let steps = (elapsed / interval).floor();
    if steps >= max as f32 {
        return (max, 0.0);
    }
    // `steps` is finite, non-negative and below `max`, so the cast cannot saturate.
    let steps = steps as usize;
    (steps, elapsed - steps as f32 * interval)
}

/// Shift one band's ten-bar history and drop `value` into the middle.
///
/// The exact unrolled form of `FxVisualizer::update`'s inner loop (`FxVisualizer.cpp:121-127`),
/// which reads `j + 1` while writing `j` and `9 - j`. Resolved, the outgoing bars are
///
/// ```text
/// bar   0     1     2     3     4     5    6     7     8     9
/// shows v-5   v-4   v-3   v-2   v-1   v    v-2   v-3   v-4   v-5
/// ```
///
/// so the left half runs one frame ahead of the right half. That off-by-one is the original's, and
/// it is the reason the ripple reads as flowing outwards rather than pulsing symmetrically.
pub fn ripple(bars: &mut [f32; NUM_BARS], value: f32) {
    let [_, o1, o2, o3, o4, o5, ..] = *bars;
    bars[0] = o1;
    bars[9] = o1;
    bars[1] = o2;
    bars[8] = o2;
    bars[2] = o3;
    bars[7] = o3;
    bars[3] = o4;
    bars[6] = o4;
    bars[4] = o5;
    bars[5] = sanitise(value);
}

/// Height in points of a bar holding `value` (`FxVisualizer.cpp:148-149`).
///
/// An exactly-zero bar becomes [`FLOOR_VALUE`], so silence draws a one-point line across the
/// middle of the strip instead of disappearing. The clamp is defensive: callers pass values that
/// have already been through [`sanitise`], but a public function must not hand back a negative
/// height and invert a rectangle.
#[must_use]
pub fn bar_height(value: f32) -> f32 {
    let value = if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let value = if value == 0.0 { FLOOR_VALUE } else { value };
    value * BAR_FULL_HEIGHT
}

/// The left edges of all hundred bars, accumulated exactly as the original accumulates them.
///
/// `x += 9.1f` a hundred times is not `27 + i * 9.1`: by the last bar the two differ by about
/// 1e-4 points. Irrelevant on screen, but reproducing the accumulation costs nothing and keeps a
/// screenshot diff honest.
#[must_use]
pub fn bar_lefts() -> [f32; BAR_COUNT] {
    let mut lefts = [0.0; BAR_COUNT];
    let mut x = BAR_X0;
    for left in &mut lefts {
        *left = x;
        x += BAR_PITCH;
    }
    lefts
}

/// The rectangle of one bar, given the widget's rectangle and the bar's local left edge.
///
/// Bars are centred on the strip's own mid-line (`FxVisualizer.cpp:151`:
/// `bounds.getHeight() / 2.0f - height / 2.0f`), so a full-scale bar spans y 10..110 of the 120
/// point strip and a silent one spans 59.5..60.5.
#[must_use]
pub fn bar_rect(rect: Rect, left: f32, value: f32) -> Rect {
    let height = bar_height(value);
    let centre = rect.top() + rect.height() / 2.0;
    Rect::from_min_size(
        pos2(rect.left() + left, centre - height / 2.0),
        vec2(BAR_WIDTH, height),
    )
}

/// The gradient colour at `y` points below the top of the strip.
///
/// `GraphHigh` at y = 0, `GraphLow` at y = 50, `GraphHigh` again at y = 100, and clamped to
/// `GraphHigh` below that (`FxVisualizer.cpp:185-196`, plus JUCE's clamping past the last stop).
/// The ramp is in *component* space and shared by all hundred bars, not per bar: a short bar
/// therefore samples only the `GraphLow` waist, and a tall one runs high → low → high from tip to
/// base.
#[must_use]
pub fn gradient_colour(high: Color32, low: Color32, y: f32) -> Color32 {
    let t = if y.is_finite() {
        (y / GRADIENT_SPAN).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if t <= GRADIENT_MID {
        high.lerp_to_gamma(low, t / GRADIENT_MID)
    } else {
        low.lerp_to_gamma(high, (t - GRADIENT_MID) / (1.0 - GRADIENT_MID))
    }
}

/// The two gradient stop colours for the current state.
///
/// `calcGradient` picks them from the palette when enabled and desaturates them when not, then
/// applies the audio-activity alpha to both (`FxVisualizer.cpp:177-197`).
#[must_use]
pub fn graph_colours(palette: Palette, enabled: bool, audio_active: bool) -> (Color32, Color32) {
    let alpha = if audio_active {
        ALPHA_ACTIVE
    } else {
        ALPHA_IDLE
    };
    if enabled {
        (
            palette.color_alpha(FxColor::GraphHigh, alpha),
            palette.color_alpha(FxColor::GraphLow, alpha),
        )
    } else {
        (
            with_alpha(desaturate(palette.color(FxColor::GraphHigh)), alpha),
            with_alpha(desaturate(palette.color(FxColor::GraphLow)), alpha),
        )
    }
}

fn paint(
    ui: &Ui,
    rect: Rect,
    palette: Palette,
    enabled: bool,
    audio_active: bool,
    bars: &[f32],
) {
    if !rect.is_positive() {
        return;
    }

    // Clipped to the strip so a bar can never bleed over the panel behind it.
    let painter = ui.painter_at(rect);

    // `ControlBackground` at alpha 1.0, radius 8 (`FxVisualizer.cpp:135-136`).
    painter.rect_filled(
        rect,
        CornerRadius::same(CORNER_RADIUS as u8),
        palette.color(FxColor::ControlBackground),
    );

    let (high, low) = graph_colours(palette, enabled, audio_active);

    // The original fills one `Path` of a hundred rectangles with one gradient brush
    // (`FxVisualizer.cpp:141-156`). egui has no gradient brush, so the equivalent is one untextured
    // mesh whose vertex colours are sampled from the same component-space ramp.
    let mut mesh = Mesh::default();
    mesh.reserve_vertices(BAR_COUNT * 8);
    mesh.reserve_triangles(BAR_COUNT * 6);
    for (&left, &value) in bar_lefts().iter().zip(bars.iter()) {
        push_bar(&mut mesh, rect, left, value, high, low);
    }
    painter.add(Shape::mesh(mesh));
}

/// Append one bar to `mesh` as a vertical gradient strip.
///
/// The strip is split at every breakpoint of the ramp that falls inside the bar — the `GraphLow`
/// waist at y = 50 and the clamp at y = 100. Without those splits a tall bar would interpolate
/// straight from its `GraphHigh` tip to its `GraphHigh` base and lose the waist entirely, which is
/// the one place where the naive four-vertex quad from the spec's sketch goes visibly wrong.
fn push_bar(mesh: &mut Mesh, rect: Rect, left: f32, value: f32, high: Color32, low: Color32) {
    let bar = bar_rect(rect, left, value);

    let mut rows = [bar.top(); 4];
    let mut count = 1;
    for breakpoint in [GRADIENT_SPAN * GRADIENT_MID, GRADIENT_SPAN] {
        let y = rect.top() + breakpoint;
        if y > bar.top() && y < bar.bottom() {
            rows[count] = y;
            count += 1;
        }
    }
    rows[count] = bar.bottom();
    count += 1;

    let base = u32::try_from(mesh.vertices.len()).unwrap_or(u32::MAX);
    for (row, &y) in rows[..count].iter().enumerate() {
        let colour = gradient_colour(high, low, y - rect.top());
        mesh.colored_vertex(pos2(bar.left(), y), colour);
        mesh.colored_vertex(pos2(bar.right(), y), colour);
        if row > 0 {
            let quad = base + (row as u32 - 1) * 2;
            mesh.add_triangle(quad, quad + 1, quad + 2);
            mesh.add_triangle(quad + 2, quad + 1, quad + 3);
        }
    }
}

/// `juce::Colour::withAlpha`.
fn with_alpha(colour: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        colour.r(),
        colour.g(),
        colour.b(),
        (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// `juce::Colour::withSaturation(0.0f)`, which round-trips through HSB and so yields
/// `max(r, g, b)` on all three channels.
///
/// `docs/spec/04-equalizer-visualizer.md` §A8 tabulates the results this must produce: dark
/// `GraphHigh` `#d51535` → `#d5d5d5` and `GraphLow` `#fe566a` → `#fefefe`; both light-theme graph
/// colours → `#ffffff`, which is the original's real legibility bug in light mode with the power
/// off, faithfully reproduced.
fn desaturate(colour: Color32) -> Color32 {
    let brightness = colour.r().max(colour.g()).max(colour.b());
    Color32::from_rgba_unmultiplied(brightness, brightness, brightness, colour.a())
}

/// The position of the spectrum strip's centre line, exposed for tests and callers that want to
/// line something up with it.
#[must_use]
pub fn centre_line(rect: Rect) -> Pos2 {
    pos2(rect.center().x, rect.top() + rect.height() / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::ThemeMode;

    fn strip() -> Rect {
        Rect::from_min_size(
            pos2(40.0, 149.0),
            vec2(geometry::WIDTH, geometry::HEIGHT),
        )
    }

    fn frame(value: f32) -> SpectrumFrame {
        [value; NUM_BANDS]
    }

    #[test]
    fn the_strip_holds_one_hundred_bars() {
        assert_eq!(BAR_COUNT, 100);
        assert_eq!(VisualizerAnimation::new().bars().len(), BAR_COUNT);
        assert_eq!(strip().size(), vec2(960.0, 120.0));
    }

    #[test]
    fn the_bar_pitch_reproduces_the_originals_accumulated_x() {
        let lefts = bar_lefts();
        assert_eq!(lefts[0], BAR_X0);
        assert!((lefts[1] - 36.1).abs() < 1e-4, "{}", lefts[1]);
        // 27 + 99 * 9.1, reached by accumulation.
        assert!((lefts[99] - 927.9).abs() < 1e-3, "{}", lefts[99]);
        // Every gap is one pitch, and the visible gap between two bars is 5.1.
        for pair in lefts.windows(2) {
            assert!((pair[1] - pair[0] - BAR_PITCH).abs() < 1e-3);
        }
        assert!((BAR_PITCH - BAR_WIDTH - 5.1).abs() < 1e-6);
    }

    #[test]
    fn the_last_bar_leaves_the_documented_right_margin() {
        let last = bar_lefts()[BAR_COUNT - 1];
        let right_margin = geometry::WIDTH - (last + BAR_WIDTH);
        assert!((right_margin - 28.1).abs() < 1e-3, "{right_margin}");
        // Slightly wider than the 27 point left margin — an asymmetry of the original's.
        assert!(right_margin > BAR_X0);
    }

    #[test]
    fn a_silent_bar_draws_the_one_point_floor_line() {
        assert_eq!(bar_height(0.0), 1.0);
        let rect = bar_rect(strip(), BAR_X0, 0.0);
        assert_eq!(rect.height(), 1.0);
        assert_eq!(rect.top() - strip().top(), 59.5);
        assert_eq!(rect.bottom() - strip().top(), 60.5);
        assert_eq!(rect.width(), BAR_WIDTH);
    }

    #[test]
    fn a_full_scale_bar_spans_one_hundred_points_about_the_centre() {
        assert_eq!(bar_height(1.0), 100.0);
        let rect = bar_rect(strip(), BAR_X0, 1.0);
        assert_eq!(rect.top() - strip().top(), 10.0);
        assert_eq!(rect.bottom() - strip().top(), 110.0);
        assert_eq!(rect.left(), strip().left() + BAR_X0);
        assert_eq!(centre_line(strip()).y - strip().top(), 60.0);
    }

    #[test]
    fn bar_height_is_linear_between_the_floor_and_full_scale() {
        assert_eq!(bar_height(0.5), 50.0);
        assert_eq!(bar_height(0.25), 25.0);
        // Defensive clamping: neither an out-of-range nor a NaN value inverts the rectangle.
        assert_eq!(bar_height(-1.0), 1.0);
        assert_eq!(bar_height(2.0), 100.0);
        assert_eq!(bar_height(f32::NAN), 1.0);
    }

    #[test]
    fn out_of_range_band_values_are_zeroed_like_the_original() {
        assert_eq!(sanitise(0.0), 0.0);
        assert_eq!(sanitise(0.5), 0.5);
        assert_eq!(sanitise(1.0), 1.0);
        assert_eq!(sanitise(-0.001), 0.0);
        assert_eq!(sanitise(1.001), 0.0);
        assert_eq!(sanitise(f32::NAN), 0.0);
        assert_eq!(sanitise(f32::INFINITY), 0.0);
    }

    #[test]
    fn the_ripple_mirrors_the_history_with_the_originals_off_by_one() {
        let mut bars = [0.0_f32; NUM_BARS];
        for value in [0.1, 0.2, 0.3, 0.4, 0.5, 0.6] {
            ripple(&mut bars, value);
        }
        // docs/spec/04-equalizer-visualizer.md §B6: v-5 v-4 v-3 v-2 v-1 v v-2 v-3 v-4 v-5.
        assert_eq!(
            bars,
            [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.4, 0.3, 0.2, 0.1],
            "the left half must run one frame ahead of the right half"
        );
    }

    #[test]
    fn the_ripple_zeroes_an_out_of_range_value_before_storing_it() {
        let mut bars = [0.0_f32; NUM_BARS];
        ripple(&mut bars, 7.0);
        assert_eq!(bars[5], 0.0);
        ripple(&mut bars, 0.75);
        assert_eq!(bars[5], 0.75);
    }

    #[test]
    fn ten_ripple_steps_flush_the_whole_history() {
        let mut bars = [0.0_f32; NUM_BARS];
        for _ in 0..6 {
            ripple(&mut bars, 1.0);
        }
        assert!(bars.iter().all(|&b| b == 1.0));
        for _ in 0..NUM_BARS {
            ripple(&mut bars, 0.0);
        }
        assert_eq!(bars, [0.0; NUM_BARS]);
    }

    #[test]
    fn the_decay_factor_is_multiplicative_so_the_rate_does_not_follow_the_frame_rate() {
        let one_long = decay_factor(0.1, RELEASE_TAU_SECS);
        let three_short = decay_factor(0.1 / 3.0, RELEASE_TAU_SECS).powi(3);
        assert!((one_long - three_short).abs() < 1e-6, "{one_long} {three_short}");
        // exp(-0.1 / 0.2) = exp(-0.5).
        assert!((one_long - (-0.5_f32).exp()).abs() < 1e-6);
        // Degenerate arguments hold the value rather than destroying it.
        assert_eq!(decay_factor(0.0, RELEASE_TAU_SECS), 1.0);
        assert_eq!(decay_factor(-1.0, RELEASE_TAU_SECS), 1.0);
        assert_eq!(decay_factor(0.1, 0.0), 1.0);
        assert_eq!(decay_factor(f32::NAN, RELEASE_TAU_SECS), 1.0);
    }

    #[test]
    fn the_envelope_follows_a_rising_level_instantly() {
        let decay = decay_factor(FRAME_INTERVAL_SECS, RELEASE_TAU_SECS);
        assert_eq!(hold_release(0.0, 0.8, decay), 0.8);
        assert_eq!(hold_release(0.2, 1.0, decay), 1.0);
    }

    #[test]
    fn the_envelope_is_a_no_op_while_the_dsp_smoother_is_still_decaying() {
        // A one-pole with the same time constant cannot fall faster than the envelope releases, so
        // on live data the envelope always picks the incoming value.
        let dt = FRAME_INTERVAL_SECS;
        let decay = decay_factor(dt, RELEASE_TAU_SECS);
        let mut previous = 1.0_f32;
        let mut dsp = 1.0_f32;
        let mut steps = 0;
        while dsp >= SETTLE_FLOOR {
            dsp *= decay_factor(dt, RELEASE_TAU_SECS);
            let held = hold_release(previous, dsp, decay);
            if dsp < SETTLE_FLOOR {
                // Below the floor both the held value and the incoming one would draw the same
                // one-point line, and the envelope snaps so the meter can stop repainting.
                assert_eq!(held, 0.0);
                break;
            }
            assert!((held - dsp).abs() < 1e-6, "held {held} vs dsp {dsp}");
            previous = held;
            steps += 1;
        }
        assert!(steps > 20, "the no-op stretch was too short to mean anything");
    }

    #[test]
    fn the_envelope_releases_a_lost_signal_by_one_time_constant_per_tau() {
        let decay = decay_factor(RELEASE_TAU_SECS, RELEASE_TAU_SECS);
        let held = hold_release(1.0, 0.0, decay);
        assert!((held - (-1.0_f32).exp()).abs() < 1e-6, "{held}");
    }

    #[test]
    fn the_envelope_snaps_to_zero_once_a_bar_would_be_invisible() {
        let decay = decay_factor(FRAME_INTERVAL_SECS, RELEASE_TAU_SECS);
        let mut level = 1.0_f32;
        let mut steps = 0;
        while level > 0.0 {
            level = hold_release(level, 0.0, decay);
            steps += 1;
            assert!(steps < 1000, "the release never reached the settle floor");
        }
        // exp(-t / 0.2) < 0.01 at t ~ 0.92 s, which is 28 steps of the 30 Hz grid.
        assert!((26..=30).contains(&steps), "{steps} steps to settle");
        assert_eq!(level, 0.0);
        assert_eq!(bar_height(level), 1.0, "a settled bar draws the floor line");
    }

    #[test]
    fn the_history_steps_on_the_originals_thirty_hertz_grid() {
        assert_eq!(steps_due(0.0, FRAME_INTERVAL_SECS, MAX_CATCHUP_STEPS).0, 0);
        assert_eq!(steps_due(0.02, FRAME_INTERVAL_SECS, MAX_CATCHUP_STEPS).0, 0);
        assert_eq!(steps_due(0.05, FRAME_INTERVAL_SECS, MAX_CATCHUP_STEPS).0, 1);
        assert_eq!(steps_due(0.09, FRAME_INTERVAL_SECS, MAX_CATCHUP_STEPS).0, 2);
        assert_eq!(steps_due(0.21, FRAME_INTERVAL_SECS, MAX_CATCHUP_STEPS).0, 6);

        // The leftover is carried, so sixty 60 Hz frames still produce thirty steps.
        let mut carried = 0.0;
        let mut total = 0;
        for _ in 0..60 {
            let (steps, remainder) =
                steps_due(carried + 1.0 / 60.0, FRAME_INTERVAL_SECS, MAX_CATCHUP_STEPS);
            total += steps;
            carried = remainder;
        }
        assert_eq!(total, 30);
    }

    #[test]
    fn a_long_stall_drops_the_backlog_instead_of_replaying_it() {
        let (steps, remainder) = steps_due(5.0, FRAME_INTERVAL_SECS, MAX_CATCHUP_STEPS);
        assert_eq!(steps, MAX_CATCHUP_STEPS);
        assert_eq!(remainder, 0.0);
        // Ten steps is exactly the depth of the history, so nothing older survives anyway.
        assert_eq!(MAX_CATCHUP_STEPS, NUM_BARS);
    }

    #[test]
    fn steps_due_ignores_nonsense_input() {
        assert_eq!(steps_due(f32::NAN, FRAME_INTERVAL_SECS, 10), (0, 0.0));
        assert_eq!(steps_due(-1.0, FRAME_INTERVAL_SECS, 10), (0, 0.0));
        assert_eq!(steps_due(1.0, 0.0, 10), (0, 0.0));
    }

    #[test]
    fn a_live_frame_reaches_the_middle_bar_of_every_band() {
        let mut animation = VisualizerAnimation::new();
        animation.advance(&frame(0.5), true, FRAME_INTERVAL_SECS);
        for band in 0..NUM_BANDS {
            let bars = &animation.bars()[band * NUM_BARS..(band + 1) * NUM_BARS];
            assert_eq!(bars[5], 0.5, "band {band}");
            assert_eq!(bars[4], 0.0, "band {band} history is still empty");
        }
        assert!(!animation.is_settled());
    }

    #[test]
    fn the_animation_only_steps_once_a_full_thirtieth_of_a_second_has_passed() {
        let mut animation = VisualizerAnimation::new();
        animation.advance(&frame(1.0), true, 0.008);
        assert!(animation.bars().iter().all(|&b| b == 0.0), "too early to step");
        // Four 8 ms frames are 32 ms; the fifth crosses 33.3 ms.
        for _ in 0..4 {
            animation.advance(&frame(1.0), true, 0.008);
        }
        assert_eq!(animation.bars()[5], 1.0);
    }

    #[test]
    fn silence_settles_the_meter_to_the_flat_line_the_original_resets_to() {
        let mut animation = VisualizerAnimation::new();
        for _ in 0..20 {
            animation.advance(&frame(1.0), true, FRAME_INTERVAL_SECS);
        }
        assert!(animation.bars().iter().all(|&b| b == 1.0));

        // Two seconds of silence: the envelope releases and ten ripple steps flush the history.
        for _ in 0..60 {
            animation.advance(&frame(0.0), false, FRAME_INTERVAL_SECS);
        }
        assert!(animation.is_settled());
        assert!(animation.bars().iter().all(|&b| b == 0.0));
        assert!(animation.bars().iter().all(|&b| bar_height(b) == 1.0));
    }

    #[test]
    fn an_inactive_frame_is_fed_zero_whatever_the_dsp_reports() {
        let mut animation = VisualizerAnimation::new();
        for _ in 0..20 {
            animation.advance(&frame(1.0), false, FRAME_INTERVAL_SECS);
        }
        assert!(animation.is_settled());
    }

    #[test]
    fn reset_clears_every_bar() {
        let mut animation = VisualizerAnimation::new();
        for _ in 0..20 {
            animation.advance(&frame(0.9), true, FRAME_INTERVAL_SECS);
        }
        assert!(!animation.is_settled());
        animation.reset();
        assert!(animation.is_settled());
        assert_eq!(animation, VisualizerAnimation::new());
    }

    #[test]
    fn the_release_tracks_the_same_curve_at_every_frame_rate() {
        // The newest bar holds the envelope as it stood at the last 30 Hz step, so at any frame
        // rate it must sit between the true continuous envelope now and one step ago. Anything
        // that made the decay follow the frame rate — a per-frame multiplier, say — would leave
        // that bracket immediately.
        let release_secs = 0.4;
        for rate in [30.0_f32, 60.0, 144.0, 240.0] {
            let dt = 1.0 / rate;
            let mut animation = VisualizerAnimation::new();
            for _ in 0..(rate as usize / 2) {
                animation.advance(&frame(1.0), true, dt);
            }
            assert_eq!(animation.bars()[5], 1.0, "{rate} Hz never reached full scale");

            let frames = (release_secs / dt).round() as usize;
            for _ in 0..frames {
                animation.advance(&frame(0.0), true, dt);
            }

            let elapsed = frames as f32 * dt;
            let newest = animation.bars()[5];
            let now = (-elapsed / RELEASE_TAU_SECS).exp();
            let one_step_ago = (-(elapsed - FRAME_INTERVAL_SECS) / RELEASE_TAU_SECS).exp();
            assert!(
                newest >= now - 1e-3 && newest <= one_step_ago + 1e-3,
                "{rate} Hz gave {newest}, outside {now}..={one_step_ago}"
            );
        }
    }

    #[test]
    fn the_gradient_runs_high_then_low_then_high_over_the_first_hundred_points() {
        let palette = Palette::new(ThemeMode::Dark);
        let (high, low) = graph_colours(palette, true, true);
        assert_eq!(gradient_colour(high, low, 0.0), high);
        assert_eq!(gradient_colour(high, low, 50.0), low);
        assert_eq!(gradient_colour(high, low, 100.0), high);
        // Quarter way down is halfway between the two stops.
        assert_eq!(gradient_colour(high, low, 25.0), high.lerp_to_gamma(low, 0.5));
        assert_eq!(gradient_colour(high, low, 75.0), low.lerp_to_gamma(high, 0.5));
    }

    #[test]
    fn the_gradient_clamps_below_the_hundred_point_stop() {
        let palette = Palette::new(ThemeMode::Dark);
        let (high, low) = graph_colours(palette, true, true);
        // The strip is 120 points tall but the ramp ends at 100, and JUCE holds the end colour.
        assert_eq!(gradient_colour(high, low, 110.0), high);
        assert_eq!(gradient_colour(high, low, geometry::HEIGHT), high);
        assert_eq!(gradient_colour(high, low, -5.0), high);
    }

    #[test]
    fn the_enabled_gradient_uses_the_two_graph_colours_from_fxtheme() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let palette = Palette::new(mode);
            let (high, low) = graph_colours(palette, true, true);
            assert_eq!(high, palette.color(FxColor::GraphHigh));
            assert_eq!(low, palette.color(FxColor::GraphLow));
        }
    }

    #[test]
    fn the_disabled_gradient_matches_the_precomputed_grey_table() {
        // docs/spec/04-equalizer-visualizer.md §A8/§B8.
        let (high, low) = graph_colours(Palette::new(ThemeMode::Dark), false, true);
        assert_eq!(high, Color32::from_rgb(0xd5, 0xd5, 0xd5));
        assert_eq!(low, Color32::from_rgb(0xfe, 0xfe, 0xfe));

        let (high, low) = graph_colours(Palette::new(ThemeMode::Light), false, true);
        assert_eq!(high, Color32::WHITE);
        assert_eq!(
            low,
            Color32::WHITE,
            "both light graph colours desaturate to white — the original's legibility bug"
        );
    }

    #[test]
    fn the_gradient_dims_to_three_quarter_alpha_while_no_audio_flows() {
        let palette = Palette::new(ThemeMode::Dark);
        let (active_high, active_low) = graph_colours(palette, true, true);
        assert_eq!(active_high.a(), 255);
        assert_eq!(active_low.a(), 255);

        let (idle_high, idle_low) = graph_colours(palette, true, false);
        assert_eq!(idle_high.a(), 191);
        assert_eq!(idle_low.a(), 191);
        assert_eq!(idle_high, palette.color_alpha(FxColor::GraphHigh, 0.75));

        // The dimming follows audio activity even when the power is off, because `calcGradient`
        // never consults the enabled flag for the alpha (`FxVisualizer.cpp:179-182`).
        let (disabled_high, _) = graph_colours(palette, false, false);
        assert_eq!(disabled_high.a(), 191);
    }

    #[test]
    fn desaturation_keeps_the_juce_hsb_brightness() {
        assert_eq!(
            desaturate(Color32::from_rgb(0xd5, 0x15, 0x35)),
            Color32::from_rgb(0xd5, 0xd5, 0xd5)
        );
        assert_eq!(
            desaturate(Color32::from_rgb(0x1a, 0xc1, 0xff)),
            Color32::WHITE
        );
        // Alpha survives the round trip.
        let faded = desaturate(Color32::from_rgba_unmultiplied(0xfe, 0x56, 0x6a, 191));
        assert_eq!(faded.a(), 191);
    }

    #[test]
    fn a_tall_bar_is_split_at_the_waist_so_the_gradient_survives() {
        let rect = strip();
        let mut mesh = Mesh::default();
        push_bar(&mut mesh, rect, BAR_X0, 1.0, Color32::RED, Color32::BLUE);
        // Tip, waist at y = 50, clamp at y = 100, base: four rows of two vertices.
        assert_eq!(mesh.vertices.len(), 8);
        assert_eq!(mesh.indices.len(), 3 * 6);
        assert!(mesh.is_valid());

        let ys: Vec<f32> = mesh.vertices.iter().map(|v| v.pos.y - rect.top()).collect();
        assert_eq!(ys, vec![10.0, 10.0, 50.0, 50.0, 100.0, 100.0, 110.0, 110.0]);
        // The waist really is the low colour, which is the whole reason for splitting.
        assert_eq!(mesh.vertices[2].color, Color32::BLUE);
        // The tip is *not* the full high colour: the ramp is in component space, so a bar that
        // starts 10 points down is already a fifth of the way towards the waist.
        assert_eq!(
            mesh.vertices[0].color,
            Color32::RED.lerp_to_gamma(Color32::BLUE, 0.2)
        );
        // y = 100 is the ramp's end stop, and the two points below it are clamped to it.
        assert_eq!(mesh.vertices[4].color, Color32::RED);
        assert_eq!(mesh.vertices[6].color, Color32::RED);
    }

    #[test]
    fn a_short_bar_is_one_quad_sampling_only_the_waist() {
        let rect = strip();
        let mut mesh = Mesh::default();
        // A bar of value 0.1 spans y 55..65, entirely below the waist and above the clamp.
        push_bar(&mut mesh, rect, BAR_X0, 0.1, Color32::RED, Color32::BLUE);
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.indices.len(), 6);
        for vertex in &mesh.vertices {
            assert_ne!(vertex.color, Color32::RED, "a short bar never reaches a tip");
        }
    }

    #[test]
    fn every_bar_of_a_full_frame_stays_inside_the_strip() {
        let rect = strip();
        for (&left, value) in bar_lefts().iter().zip([0.0, 0.5, 1.0].into_iter().cycle()) {
            let bar = bar_rect(rect, left, value);
            assert!(rect.contains_rect(bar), "{bar:?} escapes {rect:?}");
        }
    }

    #[test]
    fn the_mesh_covers_every_bar_of_the_hundred() {
        let rect = strip();
        let mut mesh = Mesh::default();
        for &left in bar_lefts().iter() {
            push_bar(&mut mesh, rect, left, 0.0, Color32::RED, Color32::BLUE);
        }
        // A floor bar is a single quad, so a hundred of them are a hundred quads.
        assert_eq!(mesh.vertices.len(), BAR_COUNT * 4);
        assert!(mesh.is_valid());
        let bounds = mesh.calc_bounds();
        assert!((bounds.left() - (rect.left() + BAR_X0)).abs() < 1e-3);
        assert!((bounds.right() - (rect.left() + 931.9)).abs() < 1e-2);
    }
}
