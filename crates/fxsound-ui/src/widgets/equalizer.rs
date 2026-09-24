//! The graphic equalizer — the 776 × 257 panel on the right of the Pro window.
//!
//! Ports `fxsound/Source/GUI/FxEqualizer.cpp` together with the vertical-slider half of
//! `FxTheme::drawLinearSlider` (`fxsound/Source/GUI/FxTheme.cpp:184-215`) and
//! `FxTheme::drawRotarySlider` (`FxTheme.cpp:257-318`), because in JUCE the panel is a parent
//! `Component` that paints the response curve and a pile of child `Slider`s that paint themselves.
//! egui has no `LookAndFeel` hook and no child components, so all of it collapses into one widget
//! that paints in the original's absolute pixel coordinates and hit-tests with `Ui::interact`.
//!
//! ## What the original is made of
//!
//! Per band (`FxEqualizer::resized`, `FxEqualizer.cpp:237-281`):
//!
//! * an `FxEqSlider` — a `LinearVertical` JUCE slider over −12…+12 dB in 1 dB steps, drawn as a
//!   dashed centre line plus the `Slider_Thumb.svg` knob, with a floating gain label above the
//!   thumb;
//! * a frequency `Label`, `centredTop`, spanning the whole column;
//! * an `FxBandCenterFreqSlider` — a 36 × 36 rotary wheel over the band's allowed frequency range,
//!   hidden entirely once the band count passes 10.
//!
//! Behind them the parent paints the response curve: a per-segment polyline through the band
//! values plus a closed polygon filled with a vertical `EqStart → EqEnd` gradient
//! (`FxEqualizer.cpp:350-393`). Here the same stroke and fill draw the equalizer's real frequency
//! response instead (see below).
//!
//! ## Why the geometry looks arbitrary
//!
//! It is JUCE's slider layout showing through. `LookAndFeel_V2::getSliderLayout` insets a vertical
//! slider by one thumb radius at each end, and `FxTheme::getSliderLayout`
//! (`FxTheme.cpp:337-341`) then takes another `2 × radius` off the top *and* off the height. For
//! the 180 px fader that leaves a 148 px travel starting 24 px down, which is why 0 dB lands on
//! y = 106 and not on the middle of the component. All of that is folded into [`EqLayout`], and
//! every number it produces is asserted against `docs/spec/04-equalizer-visualizer.md` §A7 in the
//! tests at the bottom of this file.
//!
//! ## Deliberate divergences from the original
//!
//! * **Overlapping hit areas at 31 bands.** The 32 px fader is wider than the 24 px column, so in
//!   JUCE 8 px of every boundary belongs to two sliders and the later child wins. Here the
//!   interactive width is clamped to the column (`docs/spec/04-equalizer-visualizer.md`, open
//!   question 4), so a click always lands on the band it looks like it lands on.
//! * **The curve is the response the equalizer runs** (0.4.0 audit R8). The original joins the
//!   band values with straight lines, so the filter width changed what you heard and not what you
//!   saw, and neighbouring boosts that add up drew as if they did not. Here the curve is the
//!   magnitude response of the very filter designs the audio thread installs
//!   ([`fxsound_dsp::eq::GraphicEq::response_db`]), at the device's rate and the filter width in
//!   force, sampled about [`RESPONSE_POINTS`] times across the panel. Each fader still stands at
//!   its band's centre frequency; between two faders the frequency runs geometrically. The points
//!   are worked out only when the bands, the width, the band count or the rate change
//!   ([`ResponseCache`]), never per frame.
//! * **Solo on Ctrl+Alt+drag** (`docs/spec/00-architecture.md` D-19). `FxEqualizer.cpp:123-210`
//!   walks every other band down to −10 dB while you Alt+drag one. Alt+drag moves windows on
//!   several desktops, so here the gesture takes Ctrl as well, and every band's tooltip says so. The
//!   walk is played, not written: it never touches the curve or marks the preset modified
//!   ([`crate::state::EqSolo`]). A Ctrl+Alt press leaves the band where it is until the pointer
//!   moves. The application holds the one solo there is: when it ends it — a preset load, say,
//!   from the tray with the button still down — the window lets go as well
//!   ([`UiState::eq_solo_generation`]).
//! * **The end bands turn both ways** (0.4.0 audit R6): see [`band_frequency_range`].
//! * **An EQ bypass exists.** `FxEqualizer` has no on/off control at all
//!   (`docs/spec/04-equalizer-visualizer.md` §A15); [`UiState::eq_on`] drives the desaturated
//!   painting the original reserves for the power state, while interaction still follows power
//!   alone so a bypassed curve stays editable.
//!
//! The band-count combo, the filter width, the level sliders and Restore Defaults are not drawn
//! here: in the original they are `FxEqualizerControl`, the back face of the effect column
//! (`docs/spec/03-controls.md` §5), and so they are here — [`crate::views::equalizer_controls`].
//! This widget renders only what `FxEqualizer` itself renders.

use crate::assets::{AssetCache, FxImage};
use crate::state::{EqSolo, UiAction, UiResponse, UiState};
use crate::theme::{self, FxColor, Palette};
use crate::widgets::slider;
use egui::{
    Align2, Color32, CornerRadius, Id, Mesh, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2,
};
use fxsound_core::ThemeMode;
use fxsound_core::i18n::tr;

// ---------------------------------------------------------------------------------------------
// Constants, all from FxEqualizer.h:96-105 and FxTheme.h:44-45
// ---------------------------------------------------------------------------------------------

/// `FxEqualizer::WIDTH` × `FxEqualizer::HEIGHT`.
pub const PANEL_SIZE: Vec2 = vec2(776.0, 257.0);
/// `FxEqualizer::SLIDER_HEIGHT` — the fader height while the frequency wheels are shown.
pub const SLIDER_HEIGHT: f32 = 180.0;
/// `FxEqualizer::ROTARY_SLIDER_HEIGHT` — also how much taller the fader gets once they are hidden.
pub const ROTARY_SLIDER_HEIGHT: f32 = 36.0;
/// `FxEqualizer::LABEL_HEIGHT`.
pub const LABEL_HEIGHT: f32 = 12.0;
/// `FxEqualizer::SMALL_FONT` — the frequency label size for band counts above 10.
pub const SMALL_FONT: f32 = 10.0;
/// `FxEqualizer::X_MARGIN`.
pub const X_MARGIN: f32 = 16.0;
/// `FxEqualizer::Y_MARGIN`.
pub const Y_MARGIN: f32 = 8.0;
/// `FxEqualizer::MAX_GAIN`; the range is symmetric.
pub const MAX_GAIN_DB: f32 = 12.0;
/// `setRange(-MAX_GAIN, MAX_GAIN, 1.0)` (`FxEqualizer.cpp:48`).
pub const GAIN_STEP_DB: f32 = 1.0;
/// `FxTheme::SLIDER_THUMB_RADIUS`.
pub const THUMB_RADIUS: f32 = 8.0;
/// `FxTheme::ROTARY_SLIDER_THUMB_RADIUS`.
pub const ROTARY_THUMB_RADIUS: f32 = 5.0;
/// The fader is `SLIDER_THUMB_RADIUS * 4` wide (`FxEqualizer.cpp:268`).
pub const FADER_WIDTH: f32 = THUMB_RADIUS * 4.0;
/// `FxProView.cpp:114` — the panel's rounded corner.
pub const CORNER_RADIUS: f32 = 8.0;
/// Above this many bands the wheels disappear (`FxEqualizer.cpp:255-259`).
pub const WHEEL_BAND_LIMIT: usize = 10;
/// At or above this many bands the wheel is inert even when visible (`FxEqualizer.cpp:592-593`).
pub const FIXED_FREQUENCY_BAND_LIMIT: usize = 15;
/// A JUCE `Label` paints its text at half alpha while disabled **[JUCE semantics]**: the gain
/// captions with the power off.
pub const DISABLED_LABEL_ALPHA: f32 = 0.5;
/// The band counts the combo box offers (`FxAudioControls.h:106`).
pub const BAND_COUNTS: [usize; 5] = [5, 10, 15, 20, 31];

/// `setRotaryParameters(3.66519f, 8.90118f, true)` (`FxEqualizer.cpp:53`) — 210°, clockwise from
/// twelve o'clock.
pub const ROTARY_START_ANGLE: f32 = 3.665_19;
/// The wheel's end angle: 510°, i.e. 150° after a full turn, so the sweep is 300°.
pub const ROTARY_END_ANGLE: f32 = 8.901_18;
/// JUCE's `Slider::pixelsForFullDragExtent` default: a rotary drag covers the range in 250 px.
pub const WHEEL_DRAG_PIXELS: f32 = 250.0;
/// `drawDashedLine(..., { 5, 2 }, ...)` (`FxTheme.cpp:187`).
const DASH_ON: f32 = 5.0;
/// The gap between dashes.
const DASH_OFF: f32 = 2.0;

/// The ten per-band tooltips, applied only at ten bands (`FxEqualizer.cpp:285-294`, `:334`).
pub const BAND_TOOLTIPS: [&str; 10] = [
    "Hyper-low Bass - First band for very low frequencies down to 20 Hz.",
    "Super-low Bass. Increase this for more rumble and \"thump\", decrease if there's too much boominess.",
    "Center of your Bass sound. Increase this for a fuller low end, decrease if the bass sounds overwhelming.",
    "The low end of your mid-range. Increase this to make vocals sound rich and warm, decrease it to help control instruments that sound loud and muffled.",
    "A focal point of the low-mid-range. Increase this to bring out electric guitars and vocal volume, decrease it to reduce any \"boxy\" tones.",
    "The center mid-range band. Increase this to drastically boost rhythm instruments and snare hits, reduce it to cut out \"nasal\" tones.",
    "The high-mid-range. Increase this to get more instrumental harmonics, reduce it to improve drums that have too much \"clickiness\" or orchestral instruments that are piercing.",
    "The lower end of the high-end range. Increase this for more vocal clarity and articulation, reduce it and move the frequency wheel up and down to find and cut out overly loud \"S\" and \"T\" sounds.",
    "The core high-end range. Increase this to make your audio sound more like it's in an airy, large space, reduce it to help with room noises and unwanted echoing.",
    "The highest range of average human hearing. Increase this to give your sound more of a crisp tone, with lots of overtones. Reduce it to remove hiss or painfully high sounds.",
];

/// Where a solo walks every band but the one being dragged: `-(MAX_GAIN - 2)`
/// (`FxEqualizer.cpp:187`).
pub const SOLO_FLOOR_DB: f32 = -(MAX_GAIN_DB - 2.0);
/// A solo walks the other bands a decibel per tick of a 30 Hz timer (`startTimerHz(30)`,
/// `FxEqualizer.cpp:136`): from 0 dB to the floor in a third of a second.
pub const SOLO_STEPS_PER_SECOND: f64 = 30.0;
/// What every band's tooltip adds about the solo, under the reset line.
pub const SOLO_TIP: &str = "Ctrl+Alt+drag to hear this band alone";
/// About how many points the response curve is drawn through; the exact count puts a point on
/// every band's centre (0.4.0 audit R8).
pub const RESPONSE_POINTS: usize = 200;
/// The highest panel y the curve is drawn at: a response the boosts add up to past the panel's top
/// runs along its edge.
const CURVE_TOP: f32 = 1.0;

/// The one tooltip every frequency wheel shares (`FxEqualizer.cpp:296-299`).
pub const WHEEL_TOOLTIP: &str = "This wheel allows you to adjust which frequencies this EQ band is affecting\nup or down to target different frequencies/pitches. The EQ slider above\ncontrols the volume of this EQ band. Increase or decrease to boost or cut\na portion of your audio's frequencies, without modifying the rest of your sound.";

// ---------------------------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------------------------

/// Everything `FxEqualizer::resized` derives from the band count, in panel-local coordinates.
///
/// Panel-local means "relative to the top-left of the 776 × 257 panel", which is what the original
/// works in. Callers add the panel's screen origin at paint time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqLayout {
    /// How many bands the curve has.
    pub num_bands: usize,
    /// `(776 - 16 * 2) / n`, **integer** division as in `FxEqualizer.cpp:252`.
    pub column_width: f32,
    /// 180, or 216 once the wheels are hidden and the fader absorbs their height.
    pub slider_height: f32,
    /// The fader's usable travel after JUCE and `FxTheme` have both inset it: `slider_height - 32`.
    pub region_size: f32,
    /// `jmin(36, column_width)` (`FxEqualizer.cpp:253`).
    pub rotary_size: f32,
    /// Whether the frequency wheels are drawn at all.
    pub wheels: bool,
}

impl EqLayout {
    /// Derive the layout for a band count.
    ///
    /// `num_bands == 0` yields a degenerate layout with zero-width columns; every accessor still
    /// returns a finite rectangle so a caller cannot trip over it. The original would divide by
    /// zero here (`FxEqualizer.cpp:252`) — see `docs/spec/04-equalizer-visualizer.md`, open
    /// question 3.
    #[must_use]
    pub fn new(num_bands: usize) -> Self {
        let usable = PANEL_SIZE.x - X_MARGIN * 2.0;
        let column_width = if num_bands == 0 {
            0.0
        } else {
            // C integer division: 744 / n, truncated.
            (usable as i32 / num_bands as i32) as f32
        };
        let wheels = num_bands <= WHEEL_BAND_LIMIT;
        let slider_height = if wheels {
            SLIDER_HEIGHT
        } else {
            SLIDER_HEIGHT + ROTARY_SLIDER_HEIGHT
        };
        Self {
            num_bands,
            column_width,
            slider_height,
            region_size: slider_height - THUMB_RADIUS * 4.0,
            rotary_size: ROTARY_SLIDER_HEIGHT.min(column_width),
            wheels,
        }
    }

    /// Left edge of band `i`'s column.
    #[must_use]
    pub fn column_x(&self, band: usize) -> f32 {
        X_MARGIN + self.column_width * band as f32
    }

    /// Left edge of band `i`'s 32 px fader.
    ///
    /// The offset is `(column_width - 32) / 2` in **C integer division**, so at 31 bands — where
    /// the column is only 24 px — it is −4 and the faders overlap by 8 px, exactly as in the
    /// original (`FxEqualizer.cpp:268`).
    #[must_use]
    pub fn fader_x(&self, band: usize) -> f32 {
        let offset = ((self.column_width as i32 - FADER_WIDTH as i32) / 2) as f32;
        self.column_x(band) + offset
    }

    /// The x the thumb, the curve vertex and the dashed track all share.
    #[must_use]
    pub fn center_x(&self, band: usize) -> f32 {
        self.fader_x(band) + FADER_WIDTH / 2.0
    }

    /// The fader component's rectangle — the whole strip, not just the travel.
    #[must_use]
    pub fn fader_rect(&self, band: usize) -> Rect {
        Rect::from_min_size(
            pos2(self.fader_x(band), Y_MARGIN),
            vec2(FADER_WIDTH, self.slider_height),
        )
    }

    /// The rectangle a click on band `i`'s gain must land in.
    ///
    /// Clamped to the column so that adjacent bands never share pixels; see the module docs.
    #[must_use]
    pub fn gain_hit_rect(&self, band: usize) -> Rect {
        let width = FADER_WIDTH.min(self.column_width);
        Rect::from_min_size(
            pos2(self.center_x(band) - width / 2.0, Y_MARGIN),
            vec2(width, self.slider_height),
        )
    }

    /// Panel y of the +12 dB line, i.e. the top of the fader's travel.
    #[must_use]
    pub fn track_top(&self) -> f32 {
        Y_MARGIN + THUMB_RADIUS * 3.0
    }

    /// Panel y of the −12 dB line.
    #[must_use]
    pub fn track_bottom(&self) -> f32 {
        self.track_top() + self.region_size
    }

    /// Where the curve's filled area is closed off.
    ///
    /// `slider.getBottom() - SLIDER_THUMB_RADIUS` (`FxEqualizer.cpp:377`), which is the −12 dB line
    /// — so the fill collapses to nothing when every band is cut to the floor.
    #[must_use]
    pub fn baseline(&self) -> f32 {
        Y_MARGIN + self.slider_height - THUMB_RADIUS
    }

    /// Panel y of a gain, the inverse of [`EqLayout::y_to_gain`].
    ///
    /// `y = 24 + (12 - v) / 24 * region_size` in slider-local space (JUCE's
    /// `Slider::getPositionOfValue` for a vertical slider), plus `Y_MARGIN` to reach panel space.
    #[must_use]
    pub fn gain_to_y(&self, gain_db: f32) -> f32 {
        self.track_top() + (MAX_GAIN_DB - gain_db) / (MAX_GAIN_DB * 2.0) * self.region_size
    }

    /// The gain a click at panel y selects, snapped to the 1 dB grid and clamped.
    #[must_use]
    pub fn y_to_gain(&self, y: f32) -> f32 {
        if self.region_size <= 0.0 {
            return 0.0;
        }
        let t = ((y - self.track_top()) / self.region_size).clamp(0.0, 1.0);
        snap_gain(MAX_GAIN_DB - t * (MAX_GAIN_DB * 2.0))
    }

    /// The floating gain label: the fader's full width, 12 px tall, 24 px above the thumb centre
    /// (`FxEqualizer.cpp:419-420`).
    #[must_use]
    pub fn gain_label_rect(&self, band: usize, gain_db: f32) -> Rect {
        Rect::from_min_size(
            pos2(
                self.fader_x(band),
                self.gain_to_y(gain_db) - THUMB_RADIUS * 3.0,
            ),
            vec2(FADER_WIDTH, LABEL_HEIGHT),
        )
    }

    /// The frequency caption: the full column, 6 px under the fader, one line or two
    /// (`FxEqualizer.cpp:269`, `:276`).
    #[must_use]
    pub fn freq_label_rect(&self, band: usize) -> Rect {
        let height = if self.wheels {
            LABEL_HEIGHT
        } else {
            LABEL_HEIGHT * 2.0
        };
        Rect::from_min_size(
            pos2(self.column_x(band), Y_MARGIN + self.slider_height + 6.0),
            vec2(self.column_width, height),
        )
    }

    /// The frequency wheel, 4 px under the caption, or `None` once it is hidden
    /// (`FxEqualizer.cpp:270`).
    #[must_use]
    pub fn wheel_rect(&self, band: usize) -> Option<Rect> {
        if !self.wheels {
            return None;
        }
        let size = self.rotary_size;
        let x = self.column_x(band) + (self.column_width - size) / 2.0;
        let y = self.freq_label_rect(band).bottom() + 4.0;
        Some(Rect::from_min_size(pos2(x, y), vec2(size, size)))
    }

    /// The font the frequency caption uses: 12 px with wheels, 10 px without
    /// (`FxEqualizer.cpp:267`, `:274`).
    #[must_use]
    pub fn freq_label_size(&self) -> f32 {
        if self.wheels {
            LABEL_HEIGHT
        } else {
            SMALL_FONT
        }
    }
}

/// Snap a gain to the slider's 1 dB grid the way JUCE does.
///
/// `Slider::snapLegal` is `minimum + interval * floor((v - minimum) / interval + 0.5)`, which
/// rounds halves *up* — not away from zero — so −3.5 dB becomes −3, where `f32::round` would give
/// −4.
#[must_use]
pub fn snap_gain(gain_db: f32) -> f32 {
    let snapped = (gain_db - -MAX_GAIN_DB) / GAIN_STEP_DB + 0.5;
    (-MAX_GAIN_DB + snapped.floor() * GAIN_STEP_DB).clamp(-MAX_GAIN_DB, MAX_GAIN_DB)
}

// ---------------------------------------------------------------------------------------------
// Band frequencies
// ---------------------------------------------------------------------------------------------

/// The spectrum edges a band count implies (`GraphicEqSet.cpp:430-486`).
///
/// The five counts the UI offers each overwrite `min_band_freq` / `max_band_freq` with their own
/// pair ([`fxsound_core::eq::ladder_edges_hz`]); anything else keeps whatever the engine had, which
/// is taken here as the full 20 Hz … 20 kHz span.
#[must_use]
pub fn band_span_hz(num_bands: usize) -> (f32, f32) {
    fxsound_core::eq::ladder_edges_hz(num_bands).unwrap_or((
        fxsound_core::eq::TUNING_FLOOR_HZ,
        fxsound_core::eq::TUNING_CEILING_HZ,
    ))
}

/// How far band `band`'s wheel tunes it, in Hz: [`fxsound_core::eq::band_frequency_range`].
///
/// The Windows ranges (`GraphicEqGet.cpp:105-168`) sit at the *geometric midpoints* of the generic
/// log-spaced grid, with a `+1` below a kilohertz and `+10` above it as the dead zone that stops two
/// bands ever reaching the same frequency, and pin each end band at its ladder's edge: band 1 of
/// the five- and ten-band EQ sits at 62.5 Hz, the very bottom of its own range, and could only be
/// turned up, the last band only down. Here the two end bands reach half a band past the edges
/// (0.4.0 audit R6), so both wheels start part-way round and turn either way; every other band
/// keeps the Windows range.
#[must_use]
pub fn band_frequency_range(band: usize, num_bands: usize) -> (f32, f32) {
    fxsound_core::eq::band_frequency_range(band, num_bands)
}

/// The wheel's step: a hundred positions across the band's range (`FxEqualizer.cpp:55`).
#[must_use]
pub fn frequency_step(band: usize, num_bands: usize) -> f32 {
    let (min_hz, max_hz) = band_frequency_range(band, num_bands);
    (max_hz - min_hz) / 100.0
}

/// What a right-click on the wheel restores (`FxEqualizer.cpp:589-647`).
///
/// Five and ten bands get their literal tables back. Every other count below fifteen falls into the
/// original's generic branch, which hard-codes 20 Hz … 20 kHz regardless of the band's real span
/// and truncates to an `int` — reproduced verbatim, including the truncation, because a value
/// outside the band's own range is exactly what `FxController::setEqBandFrequency` rejects
/// (`FxController.cpp:1855-1858`).
#[must_use]
pub fn default_band_frequency(band: usize, num_bands: usize) -> f32 {
    const F5: [f32; 5] = [62.5, 250.0, 1000.0, 4000.0, 16000.0];
    const F10: [f32; 10] = [
        62.5, 115.734, 214.311, 396.85, 734.867, 1360.79, 2519.84, 4666.12, 8640.48, 16000.0,
    ];
    match num_bands {
        5 => F5.get(band).copied().unwrap_or(F5[0]),
        10 => F10.get(band).copied().unwrap_or(F10[0]),
        _ => {
            if num_bands <= 1 {
                return 20.0;
            }
            let exponent = band as f32 / (num_bands as f32 - 1.0);
            (20.0 * 1000.0_f32.powf(exponent)) as i32 as f32
        }
    }
}

/// Clamp a frequency into a band's range and snap it to the wheel's hundredth.
#[must_use]
pub fn snap_frequency(freq_hz: f32, band: usize, num_bands: usize) -> f32 {
    let (min_hz, max_hz) = band_frequency_range(band, num_bands);
    let step = (max_hz - min_hz) / 100.0;
    if step <= 0.0 {
        return min_hz;
    }
    let steps = ((freq_hz - min_hz) / step).round();
    (min_hz + steps * step).clamp(min_hz, max_hz)
}

/// The caption under a band (`FxEqualizer::FxBandCenterFreqSlider::setFrequency`,
/// `FxEqualizer.cpp:505-544`).
///
/// The band count switches the whole format, and the two branches deliberately disagree about
/// 1000 Hz exactly: `>=` for fifteen bands and up, `>` below, so 1 kHz reads `1000 Hz` in the
/// ten-band view and `1.0 kHz` in the fifteen-band one. The `\n` is real — the caption box is two
/// lines tall once the wheels are gone.
///
/// A non-positive frequency yields an empty string: the original's `if (value > 0)` guard leaves
/// the previous text in place, which a stateless renderer cannot do.
#[must_use]
pub fn frequency_label(freq_hz: f32, num_bands: usize) -> String {
    if freq_hz <= 0.0 {
        return String::new();
    }
    if num_bands >= FIXED_FREQUENCY_BAND_LIMIT {
        if freq_hz >= 10_000.0 {
            format!("{}\nkHz", fixed(freq_hz / 1000.0, 0))
        } else if freq_hz >= 1000.0 {
            format!("{}\nkHz", fixed(freq_hz / 1000.0, 1))
        } else {
            format!("{} Hz", fixed(freq_hz, 0)).replace(' ', "\n")
        }
    } else if freq_hz > 1000.0 {
        format!("{} kHz", fixed(freq_hz / 1000.0, 2))
    } else {
        format!("{} Hz", fixed(freq_hz, 0))
    }
}

/// The floating gain caption: `"0"` at rest, `"+3"` / `"-7"` otherwise
/// (`FxEqualizer.cpp:416`, `:453`).
#[must_use]
pub fn gain_label(gain_db: f32) -> String {
    if gain_db == 0.0 {
        return "0".to_owned();
    }
    let rounded = f64::from(gain_db).round();
    format!("{rounded:+.0}")
}

/// `printf("%.Nf", v)` with MSVC's rounding.
///
/// The Windows build rounds halves away from zero, so 62.5 Hz prints as `63`; Rust's own `{:.0}`
/// rounds to even and would print `62`. Band 1 of the ten-band EQ is exactly 62.5 Hz, so this is
/// visible on the default layout — see `docs/spec/04-equalizer-visualizer.md`, open question 1.
fn fixed(value: f32, decimals: usize) -> String {
    let factor = 10f64.powi(decimals as i32);
    let rounded = (f64::from(value) * factor).round() / factor;
    format!("{rounded:.decimals$}")
}

// ---------------------------------------------------------------------------------------------
// Curve sampling
// ---------------------------------------------------------------------------------------------

/// One point of the drawn response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResponsePoint {
    /// Panel x.
    pub x: f32,
    /// The frequency the curve shows at `x`, in Hz.
    pub hz: f32,
    /// The equalizer's gain there, in dB.
    pub db: f32,
}

/// The frequency each fader stands for on the curve's axis: its band's centre, when the centres
/// rise from band to band as every ladder and every wheel keeps them; otherwise — a band the
/// command line moved past its neighbour — the standard ladder of the count, so the axis still
/// runs one way.
fn axis_hz(centres_hz: &[f32]) -> Vec<f32> {
    if centres_hz.windows(2).all(|pair| pair[0] < pair[1]) {
        centres_hz.to_vec()
    } else {
        fxsound_dsp::eq::standard_centres(centres_hz.len())
    }
}

/// The equalizer's frequency response as the window draws it (0.4.0 audit R8).
///
/// The curve is built from what the audio thread would be sent — the bands and the filter width
/// through `DspParams::sanitise`, the same clamps and fallbacks — and designed by the audio
/// thread's own [`fxsound_dsp::eq::GraphicEq`] at `sample_rate` (the device's; `0` leaves it at
/// 48 kHz, as a fresh engine does), so what is drawn is the magnitude of the very sections that
/// run: the Q the band count gives, the filter width, the design's own limits on Q at low
/// frequencies and small gains, a band at 0 dB or past Nyquist left out.
///
/// It runs from the first fader to the last, as the original's polyline does. Each fader stands
/// at its band's centre frequency, and between two faders the frequency runs geometrically, so
/// the curve is sampled evenly along the panel with a point on every centre: on the standard
/// ladders, which are geometric, the axis is a plain logarithmic one. Past half the rate the
/// response of the rate's top is drawn, since nothing above it can be played.
#[must_use]
pub fn response_curve(
    layout: &EqLayout,
    centres_hz: &[f32],
    gains_db: &[f32],
    filter_q: f32,
    sample_rate: u32,
) -> Vec<ResponsePoint> {
    use fxsound_core::EqBand;
    use fxsound_core::messages::DspParams;

    let count = layout.num_bands.min(centres_hz.len()).min(gains_db.len());
    if count == 0 {
        return Vec::new();
    }
    let bands: Vec<EqBand> = centres_hz
        .iter()
        .zip(gains_db)
        .take(count)
        .map(|(&hz, &db)| EqBand::new(hz, db))
        .collect();
    let mut params = DspParams::default();
    params.set_bands(&bands);
    params.filter_q = filter_q;
    params.sanitise();
    let (centres, gains) = params.bands();

    let mut eq = fxsound_dsp::eq::GraphicEq::new();
    eq.set_sample_rate(sample_rate as f32);
    eq.set_q_multiplier(params.filter_q);
    eq.set_bands(centres, gains);
    let top_hz = eq.sample_rate() * 0.5 * 0.999;
    let db_at = |hz: f32| {
        let db = eq.response_db(hz.min(top_hz));
        if db.is_finite() { db } else { 0.0 }
    };

    let axis = axis_hz(centres);
    let count = axis.len();
    if count == 1 {
        return vec![ResponsePoint {
            x: layout.center_x(0),
            hz: axis[0],
            db: db_at(axis[0]),
        }];
    }
    let per_band = RESPONSE_POINTS.div_ceil(count - 1);
    let mut points = Vec::with_capacity(per_band * (count - 1) + 1);
    for band in 0..count - 1 {
        let (x0, x1) = (layout.center_x(band), layout.center_x(band + 1));
        let (f0, f1) = (axis[band], axis[band + 1]);
        for step in 0..per_band {
            let t = step as f32 / per_band as f32;
            let hz = f0 * (f1 / f0).powf(t);
            points.push(ResponsePoint {
                x: x0 + (x1 - x0) * t,
                hz,
                db: db_at(hz),
            });
        }
    }
    let last = count - 1;
    points.push(ResponsePoint {
        x: layout.center_x(last),
        hz: axis[last],
        db: db_at(axis[last]),
    });
    points
}

/// The response curve, worked out only when what it is drawn from changes.
///
/// A frame compares the band count, the rate, the width and every centre and gain with what the
/// points were worked out from — a few dozen integers, no allocation — and only a difference
/// designs the sections and samples them again. A window left alone costs nothing for the curve,
/// and a drag costs one computation per step the band actually moves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponseCache {
    /// What `points` were worked out from, bit for bit: the band count, the rate, the width, then
    /// every centre and every gain.
    key: Vec<u32>,
    points: Vec<ResponsePoint>,
    computations: u64,
}

impl ResponseCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The curve for these bands, from the cache when nothing changed since the last call.
    pub fn curve(
        &mut self,
        layout: &EqLayout,
        centres_hz: &[f32],
        gains_db: &[f32],
        filter_q: f32,
        sample_rate: u32,
    ) -> &[ResponsePoint] {
        let key = || {
            [layout.num_bands as u32, sample_rate, filter_q.to_bits()]
                .into_iter()
                .chain(centres_hz.iter().map(|hz| hz.to_bits()))
                .chain(gains_db.iter().map(|db| db.to_bits()))
        };
        if self.computations == 0 || !self.key.iter().copied().eq(key()) {
            self.points = response_curve(layout, centres_hz, gains_db, filter_q, sample_rate);
            self.key = key().collect();
            self.computations += 1;
        }
        &self.points
    }

    /// How many times the curve has been worked out.
    #[must_use]
    pub const fn computations(&self) -> u64 {
        self.computations
    }
}

/// Where a response point is drawn: its gain's y, kept inside the panel above and at the fill's
/// baseline below.
#[must_use]
pub fn response_y(layout: &EqLayout, db: f32) -> f32 {
    layout.gain_to_y(db).clamp(CURVE_TOP, layout.baseline())
}

/// The response curve's vertices in panel space.
#[must_use]
pub fn curve_points(layout: &EqLayout, response: &[ResponsePoint]) -> Vec<Pos2> {
    response
        .iter()
        .map(|point| pos2(point.x, response_y(layout, point.db)))
        .collect()
}

/// The closed polygon the gradient fills: the curve, dropped to the baseline at both ends
/// (`FxEqualizer.cpp:372-389`).
#[must_use]
pub fn fill_polygon(layout: &EqLayout, curve: &[Pos2]) -> Vec<Pos2> {
    if curve.is_empty() {
        return Vec::new();
    }
    let baseline = layout.baseline();
    let mut polygon = Vec::with_capacity(curve.len() + 2);
    polygon.push(pos2(curve[0].x, baseline));
    polygon.extend_from_slice(curve);
    polygon.push(pos2(curve[curve.len() - 1].x, baseline));
    polygon
}

/// Where a band at `gain_db` has got to `steps` ticks into a solo: a decibel a tick towards
/// [`SOLO_FLOOR_DB`], and there it stays (`FxEqualizer.cpp:172-210`).
///
/// The original steps from wherever the band is by a whole decibel and stops only on the floor
/// exactly, so a band at +2.5 dB — a `.fac` can hold one — overshoots to −10.5 dB and then swings
/// between it and −9.5 dB until the drag ends; here it settles on −10 dB.
#[must_use]
pub fn solo_walk(gain_db: f32, steps: u32) -> f32 {
    let steps = steps as f32;
    if gain_db > SOLO_FLOOR_DB {
        (gain_db - steps).max(SOLO_FLOOR_DB)
    } else {
        (gain_db + steps).min(SOLO_FLOOR_DB)
    }
}

/// How many ticks of the solo's 30 Hz walk `elapsed` seconds hold. The first comes a tick after
/// the press, as the original's timer's does.
#[must_use]
pub fn solo_steps(elapsed: f64) -> u32 {
    // Past 64 ticks every band from −12 to +12 dB is on the floor.
    (elapsed.max(0.0) * SOLO_STEPS_PER_SECOND).floor().min(64.0) as u32
}

/// Where the dashes of a `{5, 2}` dashed line start and end along `top..bottom`.
///
/// JUCE's `Graphics::drawDashedLine` walks the line consuming the pattern and clips the last dash
/// to the end of the line, which is what the trailing partial span here reproduces.
#[must_use]
pub fn dash_spans(top: f32, bottom: f32) -> Vec<(f32, f32)> {
    let mut spans = Vec::new();
    if bottom <= top {
        return spans;
    }
    let mut y = top;
    while y < bottom {
        let end = (y + DASH_ON).min(bottom);
        spans.push((y, end));
        y += DASH_ON + DASH_OFF;
    }
    spans
}

/// The angle of the wheel at proportion `t`, in radians clockwise from twelve o'clock.
#[must_use]
pub fn rotary_angle(t: f32) -> f32 {
    ROTARY_START_ANGLE + t.clamp(0.0, 1.0) * (ROTARY_END_ANGLE - ROTARY_START_ANGLE)
}

/// A point on the wheel's arc.
#[must_use]
pub fn rotary_point(center: Pos2, radius: f32, angle: f32) -> Pos2 {
    let a = angle - std::f32::consts::FRAC_PI_2;
    pos2(center.x + radius * a.cos(), center.y + radius * a.sin())
}

/// Where a rotary drag lands.
///
/// JUCE turns the drag into `(x - x0) + (y0 - y)` pixels and divides by
/// `pixelsForFullDragExtent` (250 by default), so dragging right or up raises the value
/// (`juce::Slider::Pimpl::mouseDrag`).
#[must_use]
pub fn wheel_drag_proportion(start_proportion: f32, drag: Vec2) -> f32 {
    (start_proportion + (drag.x - drag.y) / WHEEL_DRAG_PIXELS).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------------------------
// Interaction state
// ---------------------------------------------------------------------------------------------

/// A frequency wheel drag in progress.
#[derive(Debug, Clone, Copy, PartialEq)]
struct WheelDrag {
    band: usize,
    /// The wheel's normalised value when the button went down; the drag is measured from it.
    start_proportion: f32,
}

/// A solo in progress: the band held with Ctrl+Alt, since when, and what the application was last
/// told to play.
#[derive(Debug, Clone, PartialEq)]
struct Solo {
    band: usize,
    /// `egui::InputState::time` at the press.
    started: f64,
    /// [`UiState::eq_solo_generation`] at the press: once it has moved, the application has ended
    /// the solo, and the window lets go of it too.
    generation: u64,
    /// The gains last sent with [`UiAction::SoloBand`], so each step of the walk is sent once.
    sent: Vec<f32>,
}

/// What the equalizer remembers between frames.
///
/// Only genuinely transient things live here — which band is under the pointer, which control is
/// mid-drag, a solo — and the response curve last worked out, which is a cache and not state.
/// Everything else is read from [`UiState`], so the widget stays a pure function of the model plus
/// this scratch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EqInteraction {
    gain_drag: Option<usize>,
    wheel_drag: Option<WheelDrag>,
    hovered: Option<usize>,
    solo: Option<Solo>,
    /// The band a Ctrl+Alt press landed on, until the pointer moves. Pressing to solo is not an
    /// edit, so the band stays where it is — also once the solo is over, when the application
    /// ended it under a button still held.
    still: Option<usize>,
    /// What the last frame drew every band at: the curve, with a solo's walk laid over it.
    drawn: Vec<f32>,
    response: ResponseCache,
}

impl EqInteraction {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The band whose gain is being dragged, if any.
    #[must_use]
    pub const fn dragged_band(&self) -> Option<usize> {
        self.gain_drag
    }

    /// The band whose frequency wheel is being dragged, if any.
    #[must_use]
    pub const fn dragged_wheel(&self) -> Option<usize> {
        match self.wheel_drag {
            Some(drag) => Some(drag.band),
            None => None,
        }
    }

    /// The band under the pointer, if any.
    #[must_use]
    pub const fn hovered_band(&self) -> Option<usize> {
        self.hovered
    }

    /// The band being soloed, if any.
    #[must_use]
    pub fn soloed_band(&self) -> Option<usize> {
        self.solo.as_ref().map(|solo| solo.band)
    }

    /// What the last frame drew every band at, in dB: the curve, with a solo's walk laid over it
    /// while one is on — what the window says the equalizer plays.
    #[must_use]
    pub fn drawn_gains(&self) -> &[f32] {
        &self.drawn
    }

    /// The response curve's cache.
    #[must_use]
    pub const fn response(&self) -> &ResponseCache {
        &self.response
    }

    /// Forget every in-progress gesture, e.g. after the band count changed underneath us. The
    /// response curve is kept: it is worked out again only if it no longer fits.
    pub fn clear(&mut self) {
        self.gain_drag = None;
        self.wheel_drag = None;
        self.hovered = None;
        self.solo = None;
        self.still = None;
    }
}

// ---------------------------------------------------------------------------------------------
// The widget
// ---------------------------------------------------------------------------------------------

/// The equalizer panel.
///
/// Construct it per frame around the current [`UiState`] and a long-lived [`EqInteraction`], then
/// [`show`](EqualizerWidget::show) it into the rectangle `layout::pro::equalizer()` gives.
pub struct EqualizerWidget<'a> {
    state: &'a UiState,
    interaction: &'a mut EqInteraction,
    db_scale: bool,
}

impl<'a> EqualizerWidget<'a> {
    #[must_use]
    pub fn new(state: &'a UiState, interaction: &'a mut EqInteraction) -> Self {
        Self {
            state,
            interaction,
            db_scale: false,
        }
    }

    /// Draw horizontal guides and captions at −12, −6, 0, +6 and +12 dB.
    ///
    /// An addition: `FxEqualizer` draws no scale at all, so this is off by default.
    #[must_use]
    pub fn with_db_scale(mut self, show: bool) -> Self {
        self.db_scale = show;
        self
    }

    /// Render one frame and collect what the user did.
    pub fn show(
        self,
        ui: &mut Ui,
        rect: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        response: &mut UiResponse,
    ) {
        let Self {
            state,
            interaction,
            db_scale,
        } = self;

        let layout = EqLayout::new(state.eq_bands.len());
        // Power gates interaction, exactly as `FxProView::paint` gates `setEnabled`.
        let powered = state.controls_enabled();
        let (now, primary_down, primary_pressed, solo_keys) = ui.input(|input| {
            (
                input.time,
                input.pointer.primary_down(),
                input.pointer.primary_pressed(),
                input.modifiers.ctrl && input.modifiers.alt,
            )
        });

        // A solo lasts while the button that started it is held, on a band that is still there,
        // with the power on, and until the application ends it — a preset, a band count or a lane
        // that came from the tray, the command line or D-Bus with the button still down. When it
        // ends the application is told, so the curve plays again; when the application ended it,
        // that is news only to a step of the walk that reached it afterwards.
        if interaction.solo.as_ref().is_some_and(|solo| {
            !primary_down
                || !powered
                || solo.band >= layout.num_bands
                || solo.generation != state.eq_solo_generation
        }) {
            interaction.solo = None;
            response.push(UiAction::SoloBand(None));
        }
        if !primary_down {
            interaction.still = None;
        }
        // A band count that changed underneath a drag would carry the gesture onto the wrong band.
        if interaction
            .gain_drag
            .is_some_and(|band| band >= layout.num_bands)
            || interaction
                .wheel_drag
                .is_some_and(|drag| drag.band >= layout.num_bands)
        {
            interaction.clear();
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(
            rect,
            CornerRadius::same(CORNER_RADIUS as u8),
            palette.color(FxColor::ControlBackground),
        );

        // Hit-test every band first: the curve behind the faders is drawn from values this frame's
        // drags may already have changed.
        interaction.hovered = None;
        let mut gains: Vec<f32> = state.eq_bands.iter().map(|band| band.boost_db).collect();
        for (band, gain) in gains.iter_mut().enumerate().take(layout.num_bands) {
            let hit = translate(layout.gain_hit_rect(band), rect.min.to_vec2());
            let id = Id::new("fx_eq_band").with(band);
            let sense = if powered {
                Sense::click_and_drag()
            } else {
                Sense::hover()
            };
            let mut band_response = ui.interact(hit, id, sense);

            if band_response.hovered() {
                interaction.hovered = Some(band);
                if powered {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            }

            if powered {
                let mut new_gain = *gain;

                // Ctrl+Alt on the press solos the band (`FxEqualizer::sliderDragStarted`,
                // `FxEqualizer.cpp:123-149`, which reads Alt alone).
                if interaction.solo.is_none()
                    && solo_keys
                    && primary_pressed
                    && band_response.is_pointer_button_down_on()
                {
                    interaction.solo = Some(Solo {
                        band,
                        started: now,
                        generation: state.eq_solo_generation,
                        sent: Vec::new(),
                    });
                    interaction.still = Some(band);
                }
                // A solo is for listening, and pressing to start one is not an edit: the band
                // stays where it is until the pointer moves it, and then follows it as any drag.
                if interaction.still == Some(band)
                    && band_response
                        .total_drag_delta()
                        .is_some_and(|moved| moved != Vec2::ZERO)
                {
                    interaction.still = None;
                }

                // Right-click resets the band to flat (`FxEqualizer.cpp:481-494`).
                if band_response.secondary_clicked() {
                    new_gain = 0.0;
                } else if band_response.is_pointer_button_down_on()
                    && let Some(pointer) = band_response.interact_pointer_pos()
                    && interaction.still != Some(band)
                {
                    // JUCE's `setSliderSnapsToMousePosition` default: the value jumps to the
                    // pointer on press and then tracks it.
                    new_gain = layout.y_to_gain(pointer.y - rect.min.y);
                }

                if band_response.drag_started() {
                    interaction.gain_drag = Some(band);
                }
                if band_response.drag_stopped() && interaction.gain_drag == Some(band) {
                    interaction.gain_drag = None;
                }

                if band_response.has_focus() {
                    ui.input(|input| {
                        if input.key_pressed(egui::Key::ArrowUp) {
                            new_gain = snap_gain(new_gain + GAIN_STEP_DB);
                        }
                        if input.key_pressed(egui::Key::ArrowDown) {
                            new_gain = snap_gain(new_gain - GAIN_STEP_DB);
                        }
                    });
                }

                if new_gain != *gain {
                    *gain = new_gain;
                    band_response.mark_changed();
                    response.push(UiAction::SetBandGain(band, new_gain));
                }
            }

            // `FxEqualizer::paint` re-applies the per-band tooltips every frame, and only ever at
            // ten bands (`FxEqualizer.cpp:326-343`). Under them, and alone at the other counts,
            // the right-click reset nothing else mentions (0.4.0 audit R9) and the solo.
            if !state.hide_tooltips {
                let described = (layout.num_bands == BAND_TOOLTIPS.len())
                    .then(|| BAND_TOOLTIPS.get(band))
                    .flatten()
                    .map(|tip| tr(tip));
                let tip = format!(
                    "{}\n{}",
                    slider::with_reset_tip(described.as_deref()),
                    tr(SOLO_TIP)
                );
                let _ = band_response.on_hover_text(tip);
            }
        }

        let solo = walk_the_solo(ui, interaction, &mut gains, now, response);
        interaction.drawn.clone_from(&gains);

        let ctx = PaintCtx {
            origin: rect.min.to_vec2(),
            palette,
            powered,
            // A bypassed equalizer is drawn dead but stays editable; see the module docs.
            lit: powered && state.eq_on,
            solo,
        };

        let centres: Vec<f32> = state.eq_bands.iter().map(|band| band.center_hz).collect();
        let curve = curve_points(
            &layout,
            interaction.response.curve(
                &layout,
                &centres,
                &gains,
                state.filter_q,
                state.sample_rate,
            ),
        );

        if db_scale {
            paint_db_scale(&painter, &ctx, &layout);
        }
        paint_curve_fill(&painter, &ctx, &layout, &curve);
        paint_curve_line(&painter, &ctx, &layout, &curve);

        // `gains` is built from `state.eq_bands`, which is what `layout.num_bands` counts, so the
        // two always agree in length.
        for (band, &gain) in gains.iter().enumerate().take(layout.num_bands) {
            paint_fader(&painter, &ctx, &layout, band, interaction);
            paint_thumb(&painter, ui, assets, &ctx, &layout, band, gain);
            // `showValue(false)` on every band a solo walks (`FxEqualizer.cpp:140-141`).
            if ctx.solo.is_none_or(|soloed| soloed == band) {
                paint_gain_label(&painter, &ctx, &layout, band, gain);
            }
            paint_freq_label(&painter, &ctx, &layout, band, state);
        }

        for band in 0..layout.num_bands {
            wheel(
                ui,
                assets,
                &painter,
                &ctx,
                &layout,
                band,
                state,
                interaction,
                response,
            );
        }
    }
}

/// Take a solo one frame further: every band but the soloed one walks towards
/// [`SOLO_FLOOR_DB`] from its gain on the curve, `gains` is left holding what is played and drawn,
/// and the application hears of each new step once. Returns the soloed band.
///
/// The walk is measured from the press, so a slow frame catches up rather than slowing it down,
/// and the window asks for a frame at the next step until every band is on the floor.
fn walk_the_solo(
    ui: &Ui,
    interaction: &mut EqInteraction,
    gains: &mut [f32],
    now: f64,
    response: &mut UiResponse,
) -> Option<usize> {
    let solo = interaction.solo.as_mut()?;
    let band = solo.band;
    let steps = solo_steps(now - solo.started);
    for (index, gain) in gains.iter_mut().enumerate() {
        if index != band {
            *gain = solo_walk(*gain, steps);
        }
    }
    let stepped = solo.sent.len() != gains.len()
        || gains
            .iter()
            .zip(&solo.sent)
            .enumerate()
            .any(|(index, (now, sent))| index != band && now != sent);
    if stepped {
        solo.sent = gains.to_vec();
        response.push(UiAction::SoloBand(Some(EqSolo {
            band,
            gains_db: gains.to_vec(),
        })));
    }
    let walking = gains
        .iter()
        .enumerate()
        .any(|(index, &gain)| index != band && gain != SOLO_FLOOR_DB);
    if walking {
        let next = solo.started + f64::from(steps + 1) / SOLO_STEPS_PER_SECOND;
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64((next - now).max(0.0)));
    }
    Some(band)
}

/// Everything the painting helpers need that does not change between bands.
struct PaintCtx {
    origin: Vec2,
    palette: Palette,
    /// Whether the user can touch the controls at all — the original's `isEnabled()`.
    powered: bool,
    /// Whether the colours keep their hue.
    lit: bool,
    /// The band being soloed: every other fader is drawn disabled meanwhile, as the original
    /// disables them (`FxEqualizer.cpp:138-139`).
    solo: Option<usize>,
}

impl PaintCtx {
    /// A palette colour, greyed when the equalizer is not contributing: the original's
    /// `Colour::withSaturation(0.0f)` in the dark palette and, in the light one, a grey that can
    /// still be seen ([`Palette::greyed`], 0.4.0 audit #24).
    fn colour(&self, id: FxColor, alpha: f32) -> Color32 {
        self.colour_if(self.lit, id, alpha)
    }

    fn colour_if(&self, lit: bool, id: FxColor, alpha: f32) -> Color32 {
        let base = self.palette.color_alpha(id, alpha);
        if lit { base } else { self.palette.greyed(base) }
    }

    /// Whether band `band`'s own fader keeps its colours: not while another band is soloed.
    fn band_lit(&self, band: usize) -> bool {
        self.lit && self.solo.is_none_or(|soloed| soloed == band)
    }

    fn theme_mode(&self) -> ThemeMode {
        self.palette.mode()
    }
}

fn translate(rect: Rect, origin: Vec2) -> Rect {
    rect.translate(origin)
}

/// The `EqStart@0.34 → EqEnd@0.00` ramp, anchored to band 1's fader rather than to the panel
/// (`FxEqualizer.cpp:391`). A solo desaturates its bottom stop (`FxEqualizer.cpp:345-348`).
fn fill_colour_at(ctx: &PaintCtx, layout: &EqLayout, panel_y: f32) -> Color32 {
    let top = Y_MARGIN;
    let bottom = Y_MARGIN + layout.slider_height;
    let t = ((panel_y - top) / (bottom - top)).clamp(0.0, 1.0);
    let start = ctx.colour(FxColor::EqStart, 0.34);
    let end = ctx.colour_if(ctx.lit && ctx.solo.is_none(), FxColor::EqEnd, 0.0);
    start.lerp_to_gamma(end, t)
}

/// The filled area under the curve, as one gradient-shaded quad per pair of points.
///
/// egui has no gradient brush, and the polygon is not convex, so it goes out as a `Mesh` with
/// per-vertex colours. The polygon is x-monotone with a flat bottom, which makes the fan trivially
/// correct.
fn paint_curve_fill(painter: &egui::Painter, ctx: &PaintCtx, layout: &EqLayout, curve: &[Pos2]) {
    if curve.len() < 2 {
        return;
    }
    let baseline = layout.baseline();
    let baseline_colour = fill_colour_at(ctx, layout, baseline);

    let mut mesh = Mesh::default();
    mesh.reserve_vertices(curve.len() * 2);
    mesh.reserve_triangles((curve.len() - 1) * 2);
    for point in curve {
        let top = *point + ctx.origin;
        let bottom = pos2(point.x, baseline) + ctx.origin;
        mesh.colored_vertex(top, fill_colour_at(ctx, layout, point.y));
        mesh.colored_vertex(bottom, baseline_colour);
    }
    for i in 0..curve.len() - 1 {
        let base = i as u32 * 2;
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base + 2, base + 1, base + 3);
    }
    painter.add(Shape::mesh(mesh));
}

/// The curve itself, in the original's colour and weight.
///
/// The original's `addLineSegment(line, 1.0)` + `strokePath(PathStrokeType(1.0))` lays down roughly
/// two pixels of ink around a hollow core; 1.5 px is the closest single stroke. It went out as one
/// segment per band pair; a response sampled a couple of hundred times is one path.
///
/// During a solo only the stretch from the soloed band's left neighbour to its right one keeps its
/// colour — the two segments the original leaves coloured because one of their ends is enabled
/// (`FxEqualizer.cpp:360-367`) — and the rest is grey.
fn paint_curve_line(painter: &egui::Painter, ctx: &PaintCtx, layout: &EqLayout, curve: &[Pos2]) {
    if curve.len() < 2 {
        return;
    }
    let colour = ctx.colour(FxColor::SliderTrack, 1.0);
    let stroke = |lit: bool| {
        Stroke::new(
            1.5,
            if lit {
                colour
            } else {
                ctx.palette.greyed(colour)
            },
        )
    };
    let line = |points: &[Pos2], lit: bool| {
        let points = points.iter().map(|p| *p + ctx.origin).collect();
        painter.add(Shape::line(points, stroke(lit)));
    };
    let Some(band) = ctx.solo else {
        line(curve, true);
        return;
    };
    let last = layout.num_bands.saturating_sub(1);
    let from = layout.center_x(band.saturating_sub(1));
    let to = layout.center_x((band + 1).min(last));
    let lit_segment = |pair: &[Pos2]| {
        let middle = (pair[0].x + pair[1].x) / 2.0;
        (from..=to).contains(&middle)
    };
    let mut start = 0;
    let mut lit = lit_segment(&curve[0..2]);
    for i in 1..curve.len() - 1 {
        let next = lit_segment(&curve[i..i + 2]);
        if next != lit {
            line(&curve[start..=i], lit);
            start = i;
            lit = next;
        }
    }
    line(&curve[start..], lit);
}

/// One fader's dashed track and, while it is being dragged or focused, its highlight.
///
/// The dash gradient runs `SliderTrack@0.4` at the component's own top down to
/// `VerticalSliderLow@0.4` at `region_size` below it — *not* at the line's own end
/// (`FxTheme.cpp:201-202`), so the bottom quarter of every track is flat colour. That mismatch is
/// reproduced rather than corrected. A disabled fader's track is grey (`FxTheme.cpp:195-199`).
fn paint_fader(
    painter: &egui::Painter,
    ctx: &PaintCtx,
    layout: &EqLayout,
    band: usize,
    interaction: &EqInteraction,
) {
    let x = layout.center_x(band) + ctx.origin.x;
    let top = layout.track_top();
    let bottom = layout.track_bottom();
    let lit = ctx.band_lit(band);
    let colour_top = ctx.colour_if(lit, FxColor::SliderTrack, 0.4);
    let colour_bottom = ctx.colour_if(lit, FxColor::VerticalSliderLow, 0.4);

    for (dash_top, dash_bottom) in dash_spans(top, bottom) {
        let middle = (dash_top + dash_bottom) / 2.0;
        let t = ((middle - Y_MARGIN) / layout.region_size).clamp(0.0, 1.0);
        painter.line_segment(
            [
                pos2(x, dash_top + ctx.origin.y),
                pos2(x, dash_bottom + ctx.origin.y),
            ],
            Stroke::new(1.0, colour_top.lerp_to_gamma(colour_bottom, t)),
        );
    }

    if interaction.gain_drag == Some(band) {
        // `(x, y, 32, height)` expanded by `(0, 8)`, corner radius 20 (`FxTheme.cpp:212`).
        let halo = Rect::from_min_size(
            pos2(layout.fader_x(band), top),
            vec2(FADER_WIDTH, layout.region_size),
        )
        .expand2(vec2(0.0, THUMB_RADIUS));
        painter.rect_filled(
            translate(halo, ctx.origin),
            CornerRadius::same(20),
            ctx.palette.color_alpha(FxColor::SliderHighlight, 0.1),
        );
    }
}

/// The thumb: `Slider_Thumb.svg` in a 16 × 16 box centred on the value, or the grey variant when
/// the equalizer is not contributing or another band is soloed (`FxTheme.cpp:204-207`).
#[allow(clippy::too_many_arguments)]
fn paint_thumb(
    painter: &egui::Painter,
    ui: &Ui,
    assets: &mut AssetCache,
    ctx: &PaintCtx,
    layout: &EqLayout,
    band: usize,
    gain_db: f32,
) {
    let center = pos2(layout.center_x(band), layout.gain_to_y(gain_db)) + ctx.origin;
    let rect = Rect::from_center_size(center, Vec2::splat(THUMB_RADIUS * 2.0));
    let image = if ctx.band_lit(band) {
        FxImage::SliderThumb
    } else {
        FxImage::SliderThumbBW
    };
    draw_image(painter, ui, assets, image, ctx.theme_mode(), rect);
}

/// The floating gain caption, which `FxProView` keeps visible at all times since v2.0
/// (`FxProView.cpp:70`).
///
/// With the power off too, at a disabled label's half alpha. The original hides it there because
/// `showValue(show)` is `show && isEnabled()` (`FxEqualizer.cpp:423-426`) — an accident of a
/// stale state against "values always visible since version 2.0" (`FxProView.cpp:56-73`), which
/// left the curve on screen with nothing saying what its bands are set to (0.4.0 audit #42).
fn paint_gain_label(
    painter: &egui::Painter,
    ctx: &PaintCtx,
    layout: &EqLayout,
    band: usize,
    gain_db: f32,
) {
    let rect = translate(layout.gain_label_rect(band, gain_db), ctx.origin);
    painter.text(
        rect.center_top(),
        Align2::CENTER_TOP,
        gain_label(gain_db),
        theme::semibold(LABEL_HEIGHT),
        gain_label_colour(ctx.palette, ctx.powered),
    );
}

/// A band's gain caption colour: `DefaultText`, at [`DISABLED_LABEL_ALPHA`] with the power off.
#[must_use]
pub fn gain_label_colour(palette: Palette, powered: bool) -> Color32 {
    if powered {
        palette.color(FxColor::DefaultText)
    } else {
        palette.color_alpha(FxColor::DefaultText, DISABLED_LABEL_ALPHA)
    }
}

/// The frequency caption, `centredTop` across the whole column (`FxEqualizer.cpp:44`).
fn paint_freq_label(
    painter: &egui::Painter,
    ctx: &PaintCtx,
    layout: &EqLayout,
    band: usize,
    state: &UiState,
) {
    let Some(eq_band) = state.eq_bands.get(band) else {
        return;
    };
    let rect = translate(layout.freq_label_rect(band), ctx.origin);
    // A band centred at or above Nyquist cannot be built, so the design bypasses it and the fader
    // above this label does nothing at all. Struck through rather than merely greyed: grey already
    // means "the equalizer is switched off" here, and this is a different thing — the control is
    // on, and the *device* cannot carry it. A 16 kHz Bluetooth capture kills the top two bands of
    // the standard ladder, which is not a hypothetical.
    let live = state.band_is_live(band);
    let colour = if live {
        ctx.palette.color(FxColor::DefaultText)
    } else {
        ctx.palette.color_alpha(FxColor::DefaultText, 0.4)
    };
    painter.text(
        rect.center_top(),
        Align2::CENTER_TOP,
        frequency_label(eq_band.center_hz, layout.num_bands),
        theme::semibold(layout.freq_label_size()),
        colour,
    );
    if !live {
        let y = rect.top() + layout.freq_label_size() * 0.62;
        let half = layout.freq_label_size() * 1.1;
        painter.line_segment(
            [
                pos2(rect.center().x - half, y),
                pos2(rect.center().x + half, y),
            ],
            Stroke::new(1.0, colour),
        );
    }
}

/// Guides at the five round decibel values. Not part of the original.
fn paint_db_scale(painter: &egui::Painter, ctx: &PaintCtx, layout: &EqLayout) {
    let colour = ctx.palette.color_alpha(FxColor::DefaultText, 0.15);
    for gain in [-MAX_GAIN_DB, -6.0, 0.0, 6.0, MAX_GAIN_DB] {
        let y = layout.gain_to_y(gain) + ctx.origin.y;
        painter.line_segment(
            [
                pos2(X_MARGIN + ctx.origin.x, y),
                pos2(PANEL_SIZE.x - X_MARGIN + ctx.origin.x, y),
            ],
            Stroke::new(1.0, colour),
        );
        painter.text(
            pos2(2.0 + ctx.origin.x, y),
            Align2::LEFT_CENTER,
            gain_label(gain),
            theme::regular(SMALL_FONT - 1.0),
            ctx.palette.color_alpha(FxColor::DefaultText, 0.6),
        );
    }
}

/// One frequency wheel: hit testing, the two arcs, the thumb and the right-click reset.
#[allow(clippy::too_many_arguments)]
fn wheel(
    ui: &Ui,
    assets: &mut AssetCache,
    painter: &egui::Painter,
    ctx: &PaintCtx,
    layout: &EqLayout,
    band: usize,
    state: &UiState,
    interaction: &mut EqInteraction,
    response: &mut UiResponse,
) {
    let Some(local_rect) = layout.wheel_rect(band) else {
        return;
    };
    let Some(eq_band) = state.eq_bands.get(band) else {
        return;
    };
    let rect = translate(local_rect, ctx.origin);
    let (min_hz, max_hz) = band_frequency_range(band, layout.num_bands);
    let span = (max_hz - min_hz).max(f32::EPSILON);
    let mut proportion = ((eq_band.center_hz - min_hz) / span).clamp(0.0, 1.0);

    // The wheel is inert from fifteen bands up (`FxEqualizer.cpp:592-593`); it is also hidden
    // there, so this only matters if a caller ever shows more wheels than the original would.
    let interactive = ctx.powered && layout.num_bands < FIXED_FREQUENCY_BAND_LIMIT;
    let sense = if interactive {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let mut wheel_response = ui.interact(rect, Id::new("fx_eq_wheel").with(band), sense);

    if interactive {
        if wheel_response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let mut new_hz = eq_band.center_hz;

        if wheel_response.secondary_clicked() {
            // Right-click restores the band's default centre (`FxEqualizer.cpp:589-647`).
            let default = default_band_frequency(band, layout.num_bands);
            // The controller rejects anything outside the range rather than clamping
            // (`FxController.cpp:1855-1858`), so do the same and drop it.
            if (min_hz..=max_hz).contains(&default) {
                new_hz = default;
            }
        } else {
            if wheel_response.drag_started() {
                interaction.wheel_drag = Some(WheelDrag {
                    band,
                    start_proportion: proportion,
                });
            }
            if let Some(drag) = interaction.wheel_drag.filter(|drag| drag.band == band)
                && let Some(total) = wheel_response.total_drag_delta()
            {
                proportion = wheel_drag_proportion(drag.start_proportion, total);
                new_hz = snap_frequency(min_hz + proportion * span, band, layout.num_bands);
            }
            if wheel_response.drag_stopped() && interaction.dragged_wheel() == Some(band) {
                interaction.wheel_drag = None;
            }
        }

        if wheel_response.has_focus() {
            let step = frequency_step(band, layout.num_bands);
            ui.input(|input| {
                if input.key_pressed(egui::Key::ArrowUp) {
                    new_hz = snap_frequency(new_hz + step, band, layout.num_bands);
                }
                if input.key_pressed(egui::Key::ArrowDown) {
                    new_hz = snap_frequency(new_hz - step, band, layout.num_bands);
                }
            });
        }

        if new_hz != eq_band.center_hz {
            proportion = ((new_hz - min_hz) / span).clamp(0.0, 1.0);
            wheel_response.mark_changed();
            response.push(UiAction::SetBandFrequency(band, new_hz));
        }
    }

    if !state.hide_tooltips {
        let tip = tr(WHEEL_TOOLTIP);
        let _ = wheel_response.on_hover_text(slider::with_reset_tip(Some(&tip)));
    }

    // `reduced(2)` then `radius - lineW * 0.5` (`FxTheme.cpp:348-352`).
    let bounds = rect.shrink(2.0);
    let radius = bounds.width().min(bounds.height()) / 2.0;
    let line_width = 5.0;
    let arc_radius = radius - line_width * 0.5;
    let center = bounds.center();

    painter.add(Shape::line(
        arc_points(center, arc_radius, ROTARY_START_ANGLE, ROTARY_END_ANGLE),
        Stroke::new(line_width, ctx.colour(FxColor::SliderTrack, 0.2)),
    ));
    let value_angle = rotary_angle(proportion);
    painter.add(Shape::line(
        arc_points(center, arc_radius, ROTARY_START_ANGLE, value_angle),
        Stroke::new(line_width, ctx.colour(FxColor::SliderTrack, 1.0)),
    ));

    let thumb_center = rotary_point(center, arc_radius, value_angle);
    let thumb_rect = Rect::from_center_size(thumb_center, Vec2::splat(ROTARY_THUMB_RADIUS * 2.0));
    let image = if ctx.lit {
        FxImage::SliderThumb
    } else {
        FxImage::SliderThumbBW
    };
    draw_image(painter, ui, assets, image, ctx.theme_mode(), thumb_rect);
}

/// A polyline approximating a circular arc; epaint has no arc primitive.
fn arc_points(center: Pos2, radius: f32, from: f32, to: f32) -> Vec<Pos2> {
    // One segment per ~6° keeps a 14 px arc visually smooth.
    let steps = (((to - from).abs() / 0.105).ceil() as usize).clamp(2, 64);
    (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            rotary_point(center, radius, from + (to - from) * t)
        })
        .collect()
}

fn draw_image(
    painter: &egui::Painter,
    ui: &Ui,
    assets: &mut AssetCache,
    image: FxImage,
    theme_mode: ThemeMode,
    rect: Rect,
) {
    if let Some(texture) = assets.texture(ui.ctx(), image, theme_mode, rect.size()) {
        painter.image(
            texture.id(),
            rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/spec/04-equalizer-visualizer.md` §A7: column width and the fader's x offset per band
    /// count, including the negative offset that makes the 31-band faders overlap.
    #[test]
    fn column_geometry_matches_the_spec_table_for_every_band_count() {
        let expected = [
            // (bands, column width, fader offset, centre x of band 0)
            (5_usize, 148.0_f32, 58.0_f32, 90.0_f32),
            (10, 74.0, 21.0, 53.0),
            (15, 49.0, 8.0, 40.0),
            (20, 37.0, 2.0, 34.0),
            (31, 24.0, -4.0, 28.0),
        ];
        for (bands, width, offset, first_centre) in expected {
            let layout = EqLayout::new(bands);
            assert_eq!(layout.column_width, width, "{bands} bands: column width");
            assert_eq!(
                layout.fader_x(0) - X_MARGIN,
                offset,
                "{bands} bands: fader offset"
            );
            assert_eq!(
                layout.center_x(0),
                first_centre,
                "{bands} bands: first centre"
            );
            // The pitch is the column width, so band i sits at `first + width * i`.
            assert_eq!(layout.center_x(3), first_centre + width * 3.0);
        }
    }

    #[test]
    fn the_ten_band_faders_span_53_to_719() {
        let layout = EqLayout::new(10);
        assert_eq!(layout.center_x(0), 53.0);
        assert_eq!(layout.center_x(9), 719.0);
    }

    #[test]
    fn the_31_band_faders_overlap_by_eight_points_but_the_hit_areas_do_not() {
        let layout = EqLayout::new(31);
        let first = layout.fader_rect(0);
        let second = layout.fader_rect(1);
        assert_eq!(first.right() - second.left(), 8.0);

        let first_hit = layout.gain_hit_rect(0);
        let second_hit = layout.gain_hit_rect(1);
        assert_eq!(first_hit.width(), 24.0);
        assert!(first_hit.right() <= second_hit.left());
    }

    /// §A7: the fader's travel after JUCE's inset and `FxTheme::getSliderLayout`'s second bite.
    #[test]
    fn the_usable_travel_is_148_points_with_wheels_and_184_without() {
        let ten = EqLayout::new(10);
        assert!(ten.wheels);
        assert_eq!(ten.slider_height, 180.0);
        assert_eq!(ten.region_size, 148.0);
        assert_eq!(ten.track_top(), 32.0);
        assert_eq!(ten.track_bottom(), 180.0);

        let twenty = EqLayout::new(20);
        assert!(!twenty.wheels);
        assert_eq!(twenty.slider_height, 216.0);
        assert_eq!(twenty.region_size, 184.0);
        assert_eq!(twenty.track_top(), 32.0);
        assert_eq!(twenty.track_bottom(), 216.0);
    }

    /// §A7's `y_panel(v)` table.
    #[test]
    fn gain_to_y_reproduces_the_spec_table() {
        let ten = EqLayout::new(10);
        for (gain, y) in [
            (12.0_f32, 32.0_f32),
            (6.0, 69.0),
            (0.0, 106.0),
            (-6.0, 143.0),
            (-12.0, 180.0),
        ] {
            assert_eq!(ten.gain_to_y(gain), y, "10 bands at {gain} dB");
        }

        let twenty = EqLayout::new(20);
        for (gain, y) in [
            (12.0_f32, 32.0_f32),
            (6.0, 78.0),
            (0.0, 124.0),
            (-6.0, 170.0),
            (-12.0, 216.0),
        ] {
            assert_eq!(twenty.gain_to_y(gain), y, "20 bands at {gain} dB");
        }
    }

    #[test]
    fn the_fill_baseline_is_the_minus_twelve_line() {
        assert_eq!(EqLayout::new(10).baseline(), 180.0);
        assert_eq!(EqLayout::new(10).gain_to_y(-12.0), 180.0);
        assert_eq!(EqLayout::new(20).baseline(), 216.0);
        assert_eq!(EqLayout::new(20).gain_to_y(-12.0), 216.0);
    }

    #[test]
    fn y_to_gain_inverts_gain_to_y_on_every_step() {
        let layout = EqLayout::new(10);
        let mut gain = -12.0_f32;
        while gain <= 12.0 {
            assert_eq!(layout.y_to_gain(layout.gain_to_y(gain)), gain, "{gain} dB");
            gain += 1.0;
        }
    }

    #[test]
    fn y_to_gain_clamps_outside_the_track_and_snaps_to_whole_decibels() {
        let layout = EqLayout::new(10);
        assert_eq!(layout.y_to_gain(0.0), 12.0);
        assert_eq!(layout.y_to_gain(-500.0), 12.0);
        assert_eq!(layout.y_to_gain(257.0), -12.0);
        // 106 is 0 dB; one dB is 148/24 = 6.1667 points.
        assert_eq!(layout.y_to_gain(106.0 - 6.1667), 1.0);
        assert_eq!(layout.y_to_gain(106.0 + 6.1667), -1.0);
        // Two thirds of a step still rounds to the nearer grid line.
        assert_eq!(layout.y_to_gain(106.0 - 4.0), 1.0);
        assert_eq!(layout.y_to_gain(106.0 - 2.0), 0.0);
    }

    /// JUCE's `snapLegal` is `floor(x + 0.5)`, which rounds halves toward positive infinity —
    /// unlike `f32::round`, which rounds them away from zero.
    #[test]
    fn snapping_rounds_halves_upward_like_juce() {
        assert_eq!(snap_gain(3.5), 4.0);
        assert_eq!(snap_gain(-3.5), -3.0);
        assert_eq!(snap_gain(-3.6), -4.0);
        assert_eq!(snap_gain(0.4), 0.0);
        assert_eq!(snap_gain(99.0), 12.0);
        assert_eq!(snap_gain(-99.0), -12.0);
    }

    #[test]
    fn the_label_and_wheel_rows_sit_where_resized_puts_them() {
        // §A7 branch 1: label (x, 194, width, 12), wheel (…, 210, 36, 36).
        let ten = EqLayout::new(10);
        let label = ten.freq_label_rect(0);
        assert_eq!(label.top(), 194.0);
        assert_eq!(label.height(), 12.0);
        assert_eq!(label.width(), 74.0);
        assert_eq!(label.left(), 16.0);
        let wheel = ten.wheel_rect(0).expect("ten bands show wheels");
        assert_eq!(wheel.top(), 210.0);
        assert_eq!(wheel.size(), vec2(36.0, 36.0));
        assert_eq!(wheel.bottom(), 246.0);
        assert!(wheel.bottom() <= PANEL_SIZE.y);

        // §A7 branch 2: label (x, 230, width, 24), no wheel.
        let twenty = EqLayout::new(20);
        let label = twenty.freq_label_rect(0);
        assert_eq!(label.top(), 230.0);
        assert_eq!(label.height(), 24.0);
        assert_eq!(label.bottom(), 254.0);
        assert!(twenty.wheel_rect(0).is_none());
    }

    #[test]
    fn the_wheel_shrinks_with_the_column_when_the_column_is_narrower_than_36() {
        // Five and ten bands have room; a hypothetical twelve-band layout would not.
        assert_eq!(EqLayout::new(10).rotary_size, 36.0);
        let twelve = EqLayout::new(12);
        assert_eq!(twelve.column_width, 62.0);
        assert_eq!(twelve.rotary_size, 36.0);
    }

    #[test]
    fn the_gain_label_sits_twelve_points_above_the_thumb() {
        let layout = EqLayout::new(10);
        let label = layout.gain_label_rect(0, 0.0);
        assert_eq!(
            label.bottom(),
            layout.gain_to_y(0.0) - THUMB_RADIUS * 3.0 + LABEL_HEIGHT
        );
        // Its bottom edge is 12 points above the thumb centre.
        assert_eq!(layout.gain_to_y(0.0) - label.bottom(), 12.0);
        assert_eq!(label.width(), FADER_WIDTH);
    }

    /// §A4's full range tables for the five selectable band counts, with the two end bands
    /// reaching half a band past the ladder (0.4.0 audit R6).
    #[test]
    fn band_frequency_ranges_are_the_spec_tables_with_the_end_bands_widened() {
        let five = [
            (31.0_f32, 125.0_f32),
            (126.0, 500.0),
            (501.0, 2000.0),
            (2010.0, 8000.0),
            (8010.0, 20000.0),
        ];
        for (band, expected) in five.into_iter().enumerate() {
            assert_eq!(
                band_frequency_range(band, 5),
                expected,
                "5 bands, band {band}"
            );
        }

        let ten = [
            (46.0_f32, 85.0_f32),
            (86.0, 157.0),
            (158.0, 292.0),
            (293.0, 540.0),
            (541.0, 1000.0),
            (1010.0, 1852.0),
            (1862.0, 3429.0),
            (3439.0, 6350.0),
            (6360.0, 11758.0),
            (11768.0, 20000.0),
        ];
        for (band, expected) in ten.into_iter().enumerate() {
            assert_eq!(
                band_frequency_range(band, 10),
                expected,
                "10 bands, band {band}"
            );
        }

        // Spot checks from the 15, 20 and 31 band tables, including both ends.
        assert_eq!(band_frequency_range(0, 15), (20.0, 31.0));
        assert_eq!(band_frequency_range(8, 15), (798.0, 1264.0));
        assert_eq!(band_frequency_range(14, 15), (12713.0, 20000.0));
        assert_eq!(band_frequency_range(0, 20), (20.0, 24.0));
        assert_eq!(band_frequency_range(11, 20), (805.0, 1143.0));
        assert_eq!(band_frequency_range(19, 20), (13429.0, 19077.0));
        assert_eq!(band_frequency_range(0, 31), (20.0, 22.0));
        assert_eq!(band_frequency_range(16, 31), (711.0, 893.0));
        assert_eq!(band_frequency_range(30, 31), (17835.0, 20000.0));
    }

    /// Audit report R6: on Windows the first wheel of five and ten bands starts at its minimum and
    /// the last at its maximum, so each turns one way only.
    #[test]
    fn the_end_wheels_of_five_and_ten_bands_start_part_way_round() {
        for count in [5, 10] {
            for band in [0, count - 1] {
                let (low, high) = band_frequency_range(band, count);
                let centre = default_band_frequency(band, count);
                let proportion = (centre - low) / (high - low);
                assert!(
                    (0.3..0.7).contains(&proportion),
                    "{count} bands, band {band}: {centre} Hz sits at {proportion} of {low}..{high}"
                );
            }
        }
    }

    /// The `+1` below a kilohertz and `+10` above it keep adjacent bands from ever meeting.
    #[test]
    fn adjacent_band_ranges_never_touch() {
        for bands in BAND_COUNTS {
            for band in 0..bands - 1 {
                let (_, high) = band_frequency_range(band, bands);
                let (next_low, _) = band_frequency_range(band + 1, bands);
                let gap = next_low - high;
                assert!(
                    gap == 1.0 || gap == 10.0,
                    "{bands} bands, band {band}: gap was {gap}"
                );
            }
        }
    }

    /// §A4's step column: a hundred positions across the range.
    #[test]
    fn the_wheel_steps_a_hundredth_of_the_band_range() {
        let step = frequency_step(1, 10);
        assert!(
            (step - 0.71).abs() < 1e-4,
            "band 2 of ten stepped by {step}"
        );
        // The end bands' ranges are wider, and so are their steps.
        let step = frequency_step(0, 10);
        assert!(
            (step - 0.39).abs() < 1e-4,
            "band 1 of ten stepped by {step}"
        );
        let step = frequency_step(4, 5);
        assert!(
            (step - 119.9).abs() < 1e-3,
            "band 5 of five stepped by {step}"
        );
    }

    #[test]
    fn a_bands_default_frequency_is_its_table_entry() {
        assert_eq!(default_band_frequency(0, 5), 62.5);
        assert_eq!(default_band_frequency(4, 5), 16000.0);
        assert_eq!(default_band_frequency(0, 10), 62.5);
        assert_eq!(default_band_frequency(5, 10), 1360.79);
        assert_eq!(default_band_frequency(9, 10), 16000.0);
        // The generic branch hard-codes 20 Hz … 20 kHz and truncates to an int, and it spreads the
        // bands over `band / (n - 1)`: band 7 of twelve sits at 20 * 1000^(6/11) = 865.87 Hz,
        // truncated to 865 (`FxEqualizer.cpp:637-639`).
        for (band, expected) in [(0, 20.0_f32), (6, 865.0), (11, 20000.0)] {
            let freq = default_band_frequency(band, 12);
            assert!(
                (freq - expected).abs() < 1e-3,
                "band {band} of twelve defaulted to {freq}, not {expected}"
            );
        }
    }

    #[test]
    fn frequencies_snap_into_their_band_range() {
        // Band 2 of ten spans 86 … 157 in steps of 0.71, and the range's own low edge is the
        // grid's origin, so only 86 + k * 0.71 is reachable.
        for (raw, expected) in [
            (86.0_f32, 86.0_f32),
            (0.0, 86.0),
            (1e6, 157.0),
            // 115.734 Hz is 41.87 steps up from 86, and JUCE rounds that to 42: 86 + 29.82.
            (115.734, 115.82),
        ] {
            let snapped = snap_frequency(raw, 1, 10);
            assert!(
                (snapped - expected).abs() < 1e-3,
                "{raw} Hz snapped to {snapped}, not {expected}"
            );
        }
        // Band 1 reaches below the ladder now: 46 … 85 Hz in steps of 0.39.
        assert_eq!(snap_frequency(0.0, 0, 10), 46.0);
        assert!((snap_frequency(50.0, 0, 10) - 49.9).abs() < 1e-3);
        assert_eq!(snap_frequency(1e6, 9, 10), 20000.0);
    }

    /// §A12's format table, including the deliberate `>` / `>=` asymmetry at 1000 Hz.
    #[test]
    fn frequency_labels_match_the_spec_format_table() {
        // Fifteen bands and up: two lines, kHz above 1000.
        assert_eq!(frequency_label(16000.0, 15), "16\nkHz");
        assert_eq!(frequency_label(10000.0, 31), "10\nkHz");
        assert_eq!(frequency_label(6300.0, 15), "6.3\nkHz");
        assert_eq!(frequency_label(1000.0, 15), "1.0\nkHz");
        assert_eq!(frequency_label(250.0, 20), "250\nHz");
        assert_eq!(frequency_label(31.5, 20), "32\nHz");

        // Below fifteen: one line, two decimals of kHz, and 1000 Hz stays in hertz.
        assert_eq!(frequency_label(1000.0, 10), "1000 Hz");
        assert_eq!(frequency_label(1360.79, 10), "1.36 kHz");
        assert_eq!(frequency_label(16000.0, 10), "16.00 kHz");
        assert_eq!(frequency_label(62.5, 10), "63 Hz");
        assert_eq!(frequency_label(115.734, 10), "116 Hz");
        assert_eq!(frequency_label(214.311, 10), "214 Hz");
    }

    #[test]
    fn the_half_octave_twenty_band_ladder_reads_as_whole_hertz_and_one_decimal_of_kilohertz() {
        // 0.4.0 audit R4: the twenty bands sit every half octave, so their captions are no longer
        // the round ISO numbers; the format table reads them as it reads any centre.
        let labels: Vec<String> = fxsound_core::eq::TWENTY_BAND_CENTRES_HZ
            .iter()
            .map(|&hz| frequency_label(hz, 20).replace('\n', " "))
            .collect();
        assert_eq!(
            labels,
            [
                "20 Hz", "28 Hz", "40 Hz", "57 Hz", "82 Hz", "116 Hz", "165 Hz", "235 Hz",
                "334 Hz", "474 Hz", "674 Hz", "959 Hz", "1.4 kHz", "1.9 kHz", "2.8 kHz", "3.9 kHz",
                "5.6 kHz", "7.9 kHz", "11 kHz", "16 kHz",
            ]
        );
        // Every caption stays within its column: no more characters than the old ladder's widest.
        assert!(labels.iter().all(|label| label.len() <= "1.4 kHz".len()));
    }

    #[test]
    fn a_non_positive_frequency_has_no_label() {
        assert_eq!(frequency_label(0.0, 10), "");
        assert_eq!(frequency_label(-1.0, 15), "");
    }

    #[test]
    fn gain_labels_use_a_sign_unless_the_band_is_flat() {
        assert_eq!(gain_label(0.0), "0");
        assert_eq!(gain_label(3.0), "+3");
        assert_eq!(gain_label(-7.0), "-7");
        assert_eq!(gain_label(12.0), "+12");
        assert_eq!(gain_label(-12.0), "-12");
    }

    fn centres(count: usize) -> Vec<f32> {
        fxsound_dsp::eq::standard_centres(count)
    }

    #[test]
    fn a_flat_curve_is_a_straight_line_at_zero_decibels_from_the_first_fader_to_the_last() {
        for count in BAND_COUNTS {
            let layout = EqLayout::new(count);
            let response = response_curve(&layout, &centres(count), &vec![0.0; count], 1.0, 48_000);
            assert!(response.iter().all(|p| p.db == 0.0), "{count} bands");
            let curve = curve_points(&layout, &response);
            assert!(curve.iter().all(|p| p.y == layout.gain_to_y(0.0)));
            assert_eq!(curve[0].x, layout.center_x(0));
            assert_eq!(curve[curve.len() - 1].x, layout.center_x(count - 1));
        }
        // Ten bands: the 0 dB line at y = 106, from x = 53 to 719, as the original's.
        let layout = EqLayout::new(10);
        let curve = curve_points(
            &layout,
            &response_curve(&layout, &centres(10), &[0.0; 10], 1.0, 48_000),
        );
        assert_eq!(curve[0], pos2(53.0, 106.0));
        assert_eq!(curve[curve.len() - 1], pos2(719.0, 106.0));
    }

    #[test]
    fn the_curve_has_about_two_hundred_points_and_one_on_every_bands_centre() {
        for count in BAND_COUNTS {
            let layout = EqLayout::new(count);
            let ladder = centres(count);
            let response = response_curve(&layout, &ladder, &vec![3.0; count], 1.0, 48_000);
            assert!(
                (RESPONSE_POINTS..RESPONSE_POINTS + count).contains(&response.len()),
                "{count} bands: {} points",
                response.len()
            );
            for (band, &hz) in ladder.iter().enumerate() {
                assert!(
                    response
                        .iter()
                        .any(|p| p.x == layout.center_x(band) && p.hz == hz),
                    "{count} bands: no point on band {band}"
                );
            }
            // The axis only ever rises, in x and in frequency.
            for pair in response.windows(2) {
                assert!(pair[0].x < pair[1].x && pair[0].hz < pair[1].hz);
            }
        }
    }

    #[test]
    fn a_lone_band_peaks_on_its_own_thumb() {
        let layout = EqLayout::new(10);
        let mut gains = [0.0_f32; 10];
        gains[4] = 6.0;
        let response = response_curve(&layout, &centres(10), &gains, 1.0, 48_000);
        let peak = response
            .iter()
            .max_by(|a, b| a.db.total_cmp(&b.db))
            .expect("points");
        assert_eq!(peak.x, layout.center_x(4));
        assert!((peak.db - 6.0).abs() < 0.01, "peaked at {} dB", peak.db);
        assert!((response_y(&layout, peak.db) - layout.gain_to_y(6.0)).abs() < 0.1);
        // Two bands away it is back near flat, as a peak is.
        let far = response
            .iter()
            .find(|p| p.x == layout.center_x(2))
            .expect("band 3");
        assert!(far.db.abs() < 0.5, "{} dB two bands away", far.db);
    }

    #[test]
    fn neighbouring_boosts_add_up_where_the_old_polyline_drew_them_level() {
        // 0.4.0 audit R8: every band at +6 dB was a straight line at +6 dB. What plays is higher,
        // and not flat.
        let layout = EqLayout::new(10);
        let response = response_curve(&layout, &centres(10), &[6.0; 10], 1.0, 48_000);
        let inner: Vec<f32> = response
            .iter()
            .filter(|p| p.x > layout.center_x(1) && p.x < layout.center_x(8))
            .map(|p| p.db)
            .collect();
        let (low, high) = inner.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &db| {
            (lo.min(db), hi.max(db))
        });
        assert!(low > 7.0, "the sum dips to {low} dB");
        assert!(high - low > 0.5, "ripple of {} dB", high - low);
    }

    #[test]
    fn a_narrower_filter_width_draws_a_narrower_peak() {
        // 0.4.0 audit R8: Filter Q x1 against x3 is plain to hear, and the polyline did not move.
        let layout = EqLayout::new(10);
        let mut gains = [0.0_f32; 10];
        gains[5] = 12.0;
        let wide = response_curve(&layout, &centres(10), &gains, 1.0, 48_000);
        let narrow = response_curve(&layout, &centres(10), &gains, 3.0, 48_000);
        let at = |curve: &[ResponsePoint], x: f32| {
            curve.iter().find(|p| p.x == x).expect("a point there").db
        };
        let peak = layout.center_x(5);
        assert!((at(&wide, peak) - 12.0).abs() < 0.01);
        assert!((at(&narrow, peak) - 12.0).abs() < 0.01);
        let neighbour = layout.center_x(6);
        assert!(
            at(&wide, neighbour) > at(&narrow, neighbour) + 3.0,
            "x1 {} dB, x3 {} dB at the next band",
            at(&wide, neighbour),
            at(&narrow, neighbour)
        );
    }

    /// Run a tone through the audio thread's own equalizer and measure what it does to it.
    fn measured_gain_db(eq: &mut fxsound_dsp::eq::GraphicEq, hz: f32, sample_rate: f32) -> f32 {
        eq.reset();
        let settle = (sample_rate * 0.5) as usize;
        let window = (sample_rate * 0.5) as usize;
        let omega = std::f64::consts::TAU * f64::from(hz) / f64::from(sample_rate);
        let input: Vec<f32> = (0..settle + window)
            .map(|n| (0.1 * (omega * n as f64).sin()) as f32)
            .collect();
        let mut output = input.clone();
        eq.process(&mut output, 1);
        // The same window of both, correlated at the tone's frequency: whatever the window does
        // to the one it does to the other, so their ratio is the equalizer's gain.
        let amplitude = |signal: &[f32]| {
            let (mut re, mut im) = (0.0_f64, 0.0_f64);
            for (n, &x) in signal.iter().enumerate().skip(settle) {
                let phase = omega * n as f64;
                re += f64::from(x) * phase.cos();
                im += f64::from(x) * phase.sin();
            }
            re.hypot(im)
        };
        (20.0 * (amplitude(&output) / amplitude(&input)).log10()) as f32
    }

    #[test]
    fn the_curve_is_what_the_equalizer_does_to_a_tone_within_a_tenth_of_a_decibel() {
        // 0.4.0 audit R8: the curve is the running equalizer's response. The engine's own
        // `GraphicEq`, built as the engine builds it, plays a tone at points along the curve.
        /// A band count, a filter width, a rate and each band's gain.
        type Case = (usize, f32, u32, fn(usize) -> f32);
        let cases: [Case; 4] = [
            (10, 1.0, 48_000, |band| {
                [4.0, -3.0, 8.0, 0.0, -12.0, 6.0, 2.0, -5.0, 12.0, 3.0][band]
            }),
            (
                10,
                2.5,
                44_100,
                |band| if band % 2 == 0 { 9.0 } else { -4.0 },
            ),
            (5, 3.0, 48_000, |band| [-6.0, 12.0, 0.0, 5.0, -2.0][band]),
            (31, 1.5, 96_000, |band| (band as f32 * 1.7).sin() * 11.0),
        ];
        for (count, filter_q, rate, gain) in cases {
            let layout = EqLayout::new(count);
            let ladder = centres(count);
            let gains: Vec<f32> = (0..count).map(gain).collect();
            let response = response_curve(&layout, &ladder, &gains, filter_q, rate);

            let mut eq = fxsound_dsp::eq::GraphicEq::new();
            eq.set_sample_rate(rate as f32);
            eq.set_q_multiplier(filter_q);
            eq.set_bands(&ladder, &gains);
            for point in response.iter().step_by(17).filter(|p| p.hz >= 40.0) {
                let measured = measured_gain_db(&mut eq, point.hz, rate as f32);
                assert!(
                    (measured - point.db).abs() < 0.1,
                    "{count} bands, Q x{filter_q}, {rate} Hz: at {} Hz the curve says {} dB, \
                     the equalizer does {measured} dB",
                    point.hz,
                    point.db
                );
            }
        }
    }

    #[test]
    fn the_curve_follows_a_band_moved_by_its_wheel() {
        // The fader stays in its column; the curve's peak follows the frequency the band plays at.
        let layout = EqLayout::new(10);
        let mut ladder = centres(10);
        ladder[0] = 46.0;
        let mut gains = [0.0_f32; 10];
        gains[0] = 9.0;
        let response = response_curve(&layout, &ladder, &gains, 1.0, 48_000);
        let first = response[0];
        assert_eq!((first.x, first.hz), (layout.center_x(0), 46.0));
        assert!((first.db - 9.0).abs() < 0.01, "{} dB", first.db);
    }

    #[test]
    fn a_band_past_half_the_rate_is_left_out_of_the_curve_as_it_is_out_of_the_sound() {
        // A 16 kHz headset: the top two bands of ten cannot be built, and the curve past 8 kHz is
        // what the rate's top plays.
        let layout = EqLayout::new(10);
        let mut gains = [0.0_f32; 10];
        gains[9] = 12.0;
        let response = response_curve(&layout, &centres(10), &gains, 1.0, 16_000);
        assert!(
            response.iter().all(|p| p.db.abs() < 1e-3),
            "a dead band drew a peak"
        );
    }

    #[test]
    fn a_response_past_the_panel_runs_along_its_edges() {
        let layout = EqLayout::new(31);
        let response = response_curve(&layout, &centres(31), &[12.0; 31], 1.0, 48_000);
        assert!(
            response.iter().any(|p| layout.gain_to_y(p.db) < 0.0),
            "boosts add up past it"
        );
        let curve = curve_points(&layout, &response);
        assert!(
            curve
                .iter()
                .all(|p| p.y >= CURVE_TOP && p.y <= layout.baseline())
        );
        let cut = curve_points(
            &layout,
            &response_curve(&layout, &centres(31), &[-12.0; 31], 1.0, 48_000),
        );
        assert!(
            cut.iter().all(|p| p.y == layout.baseline()),
            "cuts below −12 dB have no area"
        );
    }

    #[test]
    fn the_fill_polygon_closes_on_the_baseline_at_both_ends() {
        let layout = EqLayout::new(10);
        let curve = curve_points(
            &layout,
            &response_curve(&layout, &centres(10), &[0.0; 10], 1.0, 48_000),
        );
        let polygon = fill_polygon(&layout, &curve);
        assert_eq!(polygon.len(), curve.len() + 2);
        assert_eq!(polygon[0], pos2(53.0, 180.0));
        assert_eq!(polygon[1], pos2(53.0, 106.0));
        assert_eq!(polygon[polygon.len() - 2], pos2(719.0, 106.0));
        assert_eq!(polygon[polygon.len() - 1], pos2(719.0, 180.0));
    }

    #[test]
    fn an_empty_band_list_produces_no_curve() {
        let layout = EqLayout::new(0);
        assert!(response_curve(&layout, &[], &[], 1.0, 48_000).is_empty());
        assert!(fill_polygon(&layout, &[]).is_empty());
        // One band is one point, which draws nothing.
        let one = EqLayout::new(1);
        assert_eq!(
            response_curve(&one, &[1000.0], &[3.0], 1.0, 48_000).len(),
            1
        );
        // And the degenerate layout still answers every question finitely: the gain axis is fixed
        // by the fader height alone, so a y that reads +1 dB at ten bands reads +1 dB at none.
        assert!(layout.gain_to_y(0.0).is_finite());
        let gain = layout.y_to_gain(100.0);
        assert!((gain - 1.0).abs() < 1e-6, "y = 100 gave {gain} dB, not 1");
        assert!((gain - EqLayout::new(10).y_to_gain(100.0)).abs() < 1e-6);
    }

    #[test]
    fn a_band_the_command_line_put_past_its_neighbour_still_draws_a_rising_axis() {
        let layout = EqLayout::new(10);
        let mut ladder = centres(10);
        ladder[3] = 900.0;
        let response = response_curve(&layout, &ladder, &[2.0; 10], 1.0, 48_000);
        for pair in response.windows(2) {
            assert!(pair[0].hz < pair[1].hz);
        }
    }

    #[test]
    fn the_curve_is_worked_out_again_only_when_what_it_shows_changes() {
        let layout = EqLayout::new(10);
        let ladder = centres(10);
        let mut gains = vec![0.0_f32; 10];
        let mut cache = ResponseCache::new();
        let _ = cache.curve(&layout, &ladder, &gains, 1.0, 48_000);
        for _ in 0..100 {
            let _ = cache.curve(&layout, &ladder, &gains, 1.0, 48_000);
        }
        assert_eq!(
            cache.computations(),
            1,
            "an unchanged curve is not recomputed"
        );

        gains[2] = 4.0;
        let _ = cache.curve(&layout, &ladder, &gains, 1.0, 48_000);
        assert_eq!(cache.computations(), 2, "a gain");
        let _ = cache.curve(&layout, &ladder, &gains, 2.0, 48_000);
        assert_eq!(cache.computations(), 3, "the width");
        let _ = cache.curve(&layout, &ladder, &gains, 2.0, 44_100);
        assert_eq!(cache.computations(), 4, "the rate");
        let mut moved = ladder.clone();
        moved[0] = 50.0;
        let _ = cache.curve(&layout, &moved, &gains, 2.0, 44_100);
        assert_eq!(cache.computations(), 5, "a centre");
        let five = EqLayout::new(5);
        let curve = cache
            .curve(&five, &centres(5), &gains[..5], 2.0, 44_100)
            .to_vec();
        assert_eq!(cache.computations(), 6, "the band count");
        assert_eq!(
            curve,
            response_curve(&five, &centres(5), &gains[..5], 2.0, 44_100)
        );
        let _ = cache.curve(&five, &centres(5), &gains[..5], 2.0, 44_100);
        assert_eq!(cache.computations(), 6);
    }

    #[test]
    fn the_dashed_track_lays_five_on_two_off_and_clips_the_last_dash() {
        let spans = dash_spans(32.0, 180.0);
        assert_eq!(spans[0], (32.0, 37.0));
        assert_eq!(spans[1], (39.0, 44.0));
        // 148 points of track at a 7 point pitch: 21 full cycles plus a final clipped dash.
        assert_eq!(spans.len(), 22);
        let last = spans[spans.len() - 1];
        assert_eq!(last.0, 32.0 + 21.0 * 7.0);
        assert_eq!(last.1, 180.0);
        assert!(last.1 - last.0 < DASH_ON);
    }

    #[test]
    fn dash_spans_of_an_empty_track_are_empty() {
        assert!(dash_spans(100.0, 100.0).is_empty());
        assert!(dash_spans(100.0, 10.0).is_empty());
    }

    /// The wheel starts at seven o'clock and sweeps 300° clockwise (§A13).
    #[test]
    fn the_wheel_sweeps_three_hundred_degrees_from_seven_oclock() {
        let sweep = ROTARY_END_ANGLE - ROTARY_START_ANGLE;
        assert!(
            (sweep.to_degrees() - 300.0).abs() < 0.01,
            "swept {}°",
            sweep.to_degrees()
        );
        assert!((ROTARY_START_ANGLE.to_degrees() - 210.0).abs() < 0.01);

        let centre = pos2(0.0, 0.0);
        // At the minimum the thumb is down and to the left; at the maximum down and to the right.
        let low = rotary_point(centre, 10.0, rotary_angle(0.0));
        assert!(low.x < 0.0 && low.y > 0.0, "start thumb at {low:?}");
        let high = rotary_point(centre, 10.0, rotary_angle(1.0));
        assert!(high.x > 0.0 && high.y > 0.0, "end thumb at {high:?}");
        // Half way round is straight up.
        let middle = rotary_point(centre, 10.0, rotary_angle(0.5));
        assert!(
            middle.x.abs() < 1e-5 && middle.y < 0.0,
            "mid thumb at {middle:?}"
        );
    }

    /// JUCE covers the whole range in 250 pixels of drag, counting right and up as increases.
    #[test]
    fn a_wheel_drag_covers_the_range_in_250_points() {
        assert_eq!(wheel_drag_proportion(0.0, vec2(0.0, 0.0)), 0.0);
        assert_eq!(wheel_drag_proportion(0.0, vec2(125.0, 0.0)), 0.5);
        assert_eq!(wheel_drag_proportion(0.0, vec2(0.0, -125.0)), 0.5);
        assert_eq!(wheel_drag_proportion(0.5, vec2(0.0, 125.0)), 0.0);
        // Right and up add; the two axes are summed, not maximised.
        assert_eq!(wheel_drag_proportion(0.0, vec2(125.0, -125.0)), 1.0);
        // And it never escapes 0..=1.
        assert_eq!(wheel_drag_proportion(0.9, vec2(1000.0, 0.0)), 1.0);
        assert_eq!(wheel_drag_proportion(0.1, vec2(-1000.0, 0.0)), 0.0);
    }

    #[test]
    fn every_band_stays_inside_the_panel_for_every_offered_band_count() {
        for bands in BAND_COUNTS {
            let layout = EqLayout::new(bands);
            for band in 0..bands {
                let fader = layout.fader_rect(band);
                assert!(fader.top() >= 0.0 && fader.bottom() <= PANEL_SIZE.y);
                let label = layout.freq_label_rect(band);
                assert!(
                    label.bottom() <= PANEL_SIZE.y,
                    "{bands} bands: label overflows"
                );
                if let Some(wheel) = layout.wheel_rect(band) {
                    assert!(
                        wheel.bottom() <= PANEL_SIZE.y,
                        "{bands} bands: wheel overflows"
                    );
                }
                // Only the 31-band faders are allowed to spill past the margin.
                if bands < 31 {
                    assert!(
                        fader.left() >= X_MARGIN,
                        "{bands} bands: fader before margin"
                    );
                }
            }
        }
    }

    #[test]
    fn interaction_state_starts_empty_and_clears() {
        let mut interaction = EqInteraction::new();
        assert_eq!(interaction.dragged_band(), None);
        assert_eq!(interaction.dragged_wheel(), None);
        assert_eq!(interaction.hovered_band(), None);
        interaction.gain_drag = Some(3);
        interaction.wheel_drag = Some(WheelDrag {
            band: 3,
            start_proportion: 0.25,
        });
        assert_eq!(interaction.dragged_band(), Some(3));
        assert_eq!(interaction.dragged_wheel(), Some(3));
        interaction.clear();
        assert_eq!(interaction, EqInteraction::default());
    }

    #[test]
    fn every_band_has_a_tooltip_at_the_default_band_count() {
        assert_eq!(BAND_TOOLTIPS.len(), fxsound_core::eq::DEFAULT_BANDS);
        assert!(BAND_TOOLTIPS.iter().all(|tip| !tip.is_empty()));
    }

    #[test]
    fn a_solo_walks_a_decibel_a_tick_to_minus_ten_and_stays_there() {
        assert_eq!(solo_walk(0.0, 0), 0.0);
        assert_eq!(solo_walk(0.0, 1), -1.0);
        assert_eq!(solo_walk(0.0, 10), -10.0);
        assert_eq!(solo_walk(0.0, 64), -10.0);
        // From below the floor it walks up (`FxEqualizer.cpp:189-193`).
        assert_eq!(solo_walk(-12.0, 1), -11.0);
        assert_eq!(solo_walk(-12.0, 5), -10.0);
        assert_eq!(solo_walk(12.0, 21), -9.0);
        assert_eq!(solo_walk(12.0, 22), -10.0);
        // The original swings a band at +2.5 dB between −9.5 and −10.5 for ever; here it settles.
        assert_eq!(solo_walk(2.5, 12), -9.5);
        assert_eq!(solo_walk(2.5, 13), -10.0);
        assert_eq!(solo_walk(2.5, 14), -10.0);
    }

    #[test]
    fn the_solos_ticks_come_thirty_a_second_from_the_press() {
        assert_eq!(solo_steps(0.0), 0);
        assert_eq!(solo_steps(0.03), 0);
        assert_eq!(solo_steps(0.034), 1);
        assert_eq!(solo_steps(1.0 / 3.0 + 0.001), 10);
        assert_eq!(solo_steps(-1.0), 0);
        assert_eq!(solo_steps(1e9), 64);
    }

    mod in_the_window {
        use super::*;
        use crate::views::testing::{Harness, texts};
        use egui::{Event, Modifiers, PointerButton};
        use fxsound_core::ThemeMode;

        const CTRL_ALT: Modifiers = Modifiers {
            alt: true,
            ctrl: true,
            shift: false,
            mac_cmd: false,
            command: true,
        };

        fn panel() -> Vec2 {
            crate::layout::pro::equalizer().min.to_vec2()
        }

        fn thumb(state: &UiState, band: usize) -> Pos2 {
            let layout = EqLayout::new(state.eq_bands.len());
            pos2(
                layout.center_x(band),
                layout.gain_to_y(state.eq_bands[band].boost_db),
            ) + panel()
        }

        fn button(pos: Pos2, pressed: bool, modifiers: Modifiers) -> Event {
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers,
            }
        }

        fn solos(actions: &[UiAction]) -> Vec<Option<EqSolo>> {
            actions
                .iter()
                .filter_map(|action| match action {
                    UiAction::SoloBand(solo) => Some(solo.clone()),
                    _ => None,
                })
                .collect()
        }

        fn gain_moves(actions: &[UiAction]) -> Vec<(usize, f32)> {
            actions
                .iter()
                .filter_map(|action| match action {
                    UiAction::SetBandGain(band, db) => Some((*band, *db)),
                    _ => None,
                })
                .collect()
        }

        /// A window on `state` with the pointer pressed on band `band`'s thumb, `modifiers` held,
        /// and what the press reported.
        fn pressed_on(
            state: &UiState,
            band: usize,
            modifiers: Modifiers,
        ) -> (Harness, Vec<UiAction>) {
            let mut harness = Harness::new(ThemeMode::Dark);
            harness.settle(state);
            harness.modifiers = modifiers;
            let at = thumb(state, band);
            harness.frame(state, vec![Event::PointerMoved(at)]);
            let (actions, _) = harness.frame(
                state,
                vec![Event::PointerMoved(at), button(at, true, modifiers)],
            );
            (harness, actions)
        }

        fn curve() -> UiState {
            let mut state = UiState::default();
            state.eq_bands[0].boost_db = 3.0;
            state.eq_bands[1].boost_db = -12.0;
            state.eq_bands[5].boost_db = 6.0;
            state
        }

        #[test]
        fn ctrl_alt_on_a_band_solos_it_and_walks_every_other_band_down_to_minus_ten() {
            // 0.4.0 audit R10: `FxEqualizer.cpp:123-210`, bound to Ctrl+Alt here (D-19).
            let state = curve();
            let (mut harness, actions) = pressed_on(&state, 5, CTRL_ALT);
            assert_eq!(harness.scratch.eq.soloed_band(), Some(5));
            assert!(
                gain_moves(&actions).is_empty(),
                "the press moved {actions:?}"
            );
            let played: Vec<f32> = state.eq_bands.iter().map(|b| b.boost_db).collect();
            assert_eq!(
                solos(&actions),
                [Some(EqSolo {
                    band: 5,
                    gains_db: played
                })],
                "the curve as it is, to start with"
            );

            // Seven frames of a sixtieth: three and a half ticks, so three steps.
            let mut sent = Vec::new();
            for _ in 0..7 {
                let (actions, _) = harness.frame(&state, Vec::new());
                assert!(gain_moves(&actions).is_empty());
                sent.extend(solos(&actions));
            }
            let solo = sent.last().cloned().flatten().expect("a step was sent");
            assert_eq!(solo.band, 5);
            assert_eq!(solo.gains_db[0], 0.0, "+3 dB three steps down");
            assert_eq!(solo.gains_db[1], -10.0, "-12 dB two steps up, and there");
            assert_eq!(solo.gains_db[2], -3.0);
            assert_eq!(solo.gains_db[5], 6.0, "the soloed band plays its own gain");

            // Half a second on, everything is on the floor, and each step went out once: +3 dB
            // is thirteen steps from it.
            for _ in 0..30 {
                sent.extend(solos(&harness.frame(&state, Vec::new()).0));
            }
            assert_eq!(sent.len(), 13, "one message a step: {sent:?}");
            let solo = sent.last().cloned().flatten().expect("the last step");
            for (band, &db) in solo.gains_db.iter().enumerate() {
                if band != 5 {
                    assert_eq!(db, SOLO_FLOOR_DB, "band {band}");
                }
            }

            // Letting go ends it, and nothing was ever an edit.
            let at = thumb(&state, 5);
            let (actions, _) = harness.frame(&state, vec![button(at, false, CTRL_ALT)]);
            assert_eq!(solos(&actions), [None]);
            assert!(gain_moves(&actions).is_empty());
            assert_eq!(harness.scratch.eq.soloed_band(), None);
            let (actions, _) = harness.frame(&state, Vec::new());
            assert!(solos(&actions).is_empty(), "ended once");
        }

        #[test]
        fn a_press_without_both_ctrl_and_alt_is_no_solo() {
            let ctrl = Modifiers {
                ctrl: true,
                command: true,
                ..Modifiers::default()
            };
            let alt = Modifiers {
                alt: true,
                ..Modifiers::default()
            };
            for modifiers in [Modifiers::default(), ctrl, alt] {
                let state = curve();
                let (mut harness, mut actions) = pressed_on(&state, 5, modifiers);
                for _ in 0..10 {
                    actions.extend(harness.frame(&state, Vec::new()).0);
                }
                assert!(solos(&actions).is_empty(), "{modifiers:?}: {actions:?}");
                assert_eq!(harness.scratch.eq.soloed_band(), None);
            }
        }

        #[test]
        fn the_soloed_band_moves_only_when_the_pointer_does() {
            let state = UiState::default();
            let (mut harness, _) = pressed_on(&state, 3, CTRL_ALT);
            let at = thumb(&state, 3);
            // Six decibels up is 37 points on a 148-point travel.
            let up = at - vec2(0.0, 37.0);
            let mut actions = Vec::new();
            for pos in [at - vec2(0.0, 10.0), up] {
                actions.extend(harness.frame(&state, vec![Event::PointerMoved(pos)]).0);
            }
            let moves = gain_moves(&actions);
            assert!(!moves.is_empty(), "the drag moved nothing: {actions:?}");
            assert!(moves.iter().all(|&(band, _)| band == 3), "{moves:?}");
            assert_eq!(moves.last(), Some(&(3, 6.0)));
            assert_eq!(harness.scratch.eq.soloed_band(), Some(3), "still soloing");
        }

        #[test]
        fn switching_the_power_off_ends_a_solo() {
            let mut state = UiState::default();
            let (mut harness, _) = pressed_on(&state, 2, CTRL_ALT);
            state.power = false;
            let (actions, _) = harness.frame(&state, Vec::new());
            assert_eq!(solos(&actions), [None]);
            assert_eq!(harness.scratch.eq.soloed_band(), None);
        }

        #[test]
        fn a_band_count_that_drops_under_the_soloed_band_ends_the_solo() {
            let mut state = UiState::default();
            let (mut harness, _) = pressed_on(&state, 8, CTRL_ALT);
            state.eq_bands.truncate(5);
            let (actions, _) = harness.frame(&state, Vec::new());
            assert_eq!(solos(&actions), [None]);
        }

        /// What `state`'s curve holds, band by band.
        fn curve_of(state: &UiState) -> Vec<f32> {
            state.eq_bands.iter().map(|band| band.boost_db).collect()
        }

        #[test]
        fn a_solo_the_application_ended_ends_in_the_window_with_the_button_still_down() {
            // A preset from the tray while the button is held on a band: the application stops
            // playing the solo and moves the generation on. The walk here has long reached the
            // floor, where the new curve walked would send nothing new, so it is the generation
            // alone that tells the window.
            let mut state = curve();
            let (mut harness, _) = pressed_on(&state, 5, CTRL_ALT);
            for _ in 0..50 {
                harness.frame(&state, Vec::new());
            }
            assert!(
                harness
                    .scratch
                    .eq
                    .drawn_gains()
                    .iter()
                    .enumerate()
                    .all(|(band, &db)| band == 5 || db == SOLO_FLOOR_DB),
                "walked: {:?}",
                harness.scratch.eq.drawn_gains()
            );

            for (band, slot) in state.eq_bands.iter_mut().enumerate() {
                slot.boost_db = band as f32 - 4.0;
            }
            state.eq_solo_generation += 1;
            let (actions, _) = harness.frame(&state, Vec::new());
            assert_eq!(solos(&actions), [None], "the window lets go of it");
            assert_eq!(harness.scratch.eq.soloed_band(), None);
            assert_eq!(harness.scratch.eq.drawn_gains(), curve_of(&state));
            assert!(gain_moves(&actions).is_empty(), "holding still is no edit");

            let (actions, _) = harness.frame(&state, Vec::new());
            assert!(
                solos(&actions).is_empty() && gain_moves(&actions).is_empty(),
                "and that was all: {actions:?}"
            );
            assert_eq!(harness.scratch.eq.drawn_gains(), curve_of(&state));
        }

        #[test]
        fn a_solo_the_application_ended_mid_walk_sends_no_step_of_the_new_curve() {
            let mut state = curve();
            let (mut harness, _) = pressed_on(&state, 5, CTRL_ALT);
            for _ in 0..4 {
                harness.frame(&state, Vec::new());
            }
            state.eq_bands[0].boost_db = 9.0;
            state.eq_solo_generation += 1;
            let (actions, _) = harness.frame(&state, Vec::new());
            assert_eq!(solos(&actions), [None], "{actions:?}");
            assert_eq!(harness.scratch.eq.drawn_gains(), curve_of(&state));
        }

        #[test]
        fn a_band_pressed_to_solo_stays_put_when_the_solo_is_ended_for_it_until_the_pointer_moves()
        {
            // The press landed on the band at 0 dB; the preset loaded meanwhile puts it at
            // -4 dB. Snapping it back to the pointer would be an edit nobody made.
            let mut state = UiState::default();
            let at = thumb(&state, 3);
            let (mut harness, _) = pressed_on(&state, 3, CTRL_ALT);
            state.eq_bands[3].boost_db = -4.0;
            state.eq_solo_generation += 1;
            let mut actions = Vec::new();
            for _ in 0..5 {
                actions.extend(harness.frame(&state, Vec::new()).0);
            }
            assert!(gain_moves(&actions).is_empty(), "{actions:?}");

            // Moved, it is a drag like any other: six decibels up is 37 points.
            let (actions, _) =
                harness.frame(&state, vec![Event::PointerMoved(at - vec2(0.0, 37.0))]);
            assert_eq!(gain_moves(&actions).last(), Some(&(3, 6.0)));
            assert!(solos(&actions).is_empty(), "no solo came back: {actions:?}");
        }

        #[test]
        fn the_generation_moving_with_no_solo_on_changes_nothing_in_the_window() {
            let mut state = curve();
            let mut harness = Harness::new(ThemeMode::Dark);
            harness.settle(&state);
            state.eq_solo_generation += 1;
            let (actions, _) = harness.frame(&state, Vec::new());
            assert!(
                solos(&actions).is_empty() && gain_moves(&actions).is_empty(),
                "{actions:?}"
            );
            assert_eq!(harness.scratch.eq.drawn_gains(), curve_of(&state));
        }

        #[test]
        fn during_a_solo_only_the_soloed_band_shows_its_gain() {
            // `showValue(false)` on every other band (`FxEqualizer.cpp:140-141`).
            let state = UiState::default();
            let eq = crate::layout::pro::equalizer();
            let gain_labels = |shapes: &[egui::epaint::ClippedShape]| -> Vec<String> {
                texts(shapes)
                    .into_iter()
                    .filter(|(text, rect, _)| {
                        eq.contains(rect.center()) && text.parse::<i32>().is_ok()
                    })
                    .map(|(text, ..)| text)
                    .collect()
            };
            let mut harness = Harness::new(ThemeMode::Dark);
            assert_eq!(gain_labels(&harness.settle(&state)).len(), 10);

            let (mut harness, _) = pressed_on(&state, 4, CTRL_ALT);
            let mut shapes = Vec::new();
            for _ in 0..30 {
                shapes = harness.frame(&state, Vec::new()).1;
            }
            assert_eq!(gain_labels(&shapes), ["0"]);
        }

        #[test]
        fn every_bands_tooltip_says_how_to_solo_it() {
            for count in [5, 10, 31] {
                let state = UiState {
                    eq_bands: centres(count)
                        .into_iter()
                        .map(|hz| fxsound_core::EqBand::new(hz, 0.0))
                        .collect(),
                    ..UiState::default()
                };
                let mut harness = Harness::new(ThemeMode::Dark);
                harness.settle(&state);
                let shown = harness
                    .rest(&state, thumb(&state, 0) + vec2(0.0, 20.0))
                    .join("\n");
                assert!(shown.contains(SOLO_TIP), "{count} bands: {shown}");
                assert!(shown.contains(slider::RESET_TIP), "{count} bands: {shown}");
            }
        }

        #[test]
        fn a_window_left_alone_works_the_curve_out_once() {
            // 0.4.0 audit R8: the response is a cache, not a per-frame cost.
            let mut state = UiState::default();
            state.eq_bands[3].boost_db = 5.0;
            let mut harness = Harness::new(ThemeMode::Dark);
            for _ in 0..60 {
                harness.frame(&state, Vec::new());
            }
            assert_eq!(harness.scratch.eq.response().computations(), 1);

            state.theme = ThemeMode::Light;
            state.eq_on = false;
            state.power = false;
            for _ in 0..10 {
                harness.frame(&state, Vec::new());
            }
            assert_eq!(
                harness.scratch.eq.response().computations(),
                1,
                "colours are not the curve"
            );
            state.filter_q = 2.0;
            harness.frame(&state, Vec::new());
            state.sample_rate = 44_100;
            harness.frame(&state, Vec::new());
            state.eq_bands[3].boost_db = 6.0;
            for _ in 0..10 {
                harness.frame(&state, Vec::new());
            }
            assert_eq!(harness.scratch.eq.response().computations(), 4);
        }

        #[test]
        fn the_drawn_curve_is_the_response_in_the_originals_colour_and_weight() {
            let mut state = UiState::default();
            state.eq_bands[4].boost_db = 9.0;
            let mut harness = Harness::new(ThemeMode::Dark);
            let shapes = harness.settle(&state);
            let palette = Palette::new(ThemeMode::Dark);
            let layout = EqLayout::new(10);
            let expected = curve_points(
                &layout,
                &response_curve(
                    &layout,
                    &centres(10),
                    &[0.0, 0.0, 0.0, 0.0, 9.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                    1.0,
                    48_000,
                ),
            );
            let drawn: Vec<&egui::epaint::PathShape> = shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    Shape::Path(path) if path.points.len() == expected.len() => Some(path),
                    _ => None,
                })
                .collect();
            assert_eq!(drawn.len(), 1, "one path for the curve");
            let path = drawn[0];
            assert_eq!(path.stroke.width, 1.5);
            assert_eq!(
                path.stroke.color,
                egui::epaint::ColorMode::Solid(palette.color(FxColor::SliderTrack))
            );
            for (drawn, want) in path.points.iter().zip(&expected) {
                assert_eq!(*drawn, *want + panel());
            }
        }

        #[test]
        fn the_first_wheel_of_ten_bands_turns_below_the_ladder() {
            // 0.4.0 audit R6: on Windows band 1's wheel starts at its minimum, 62.5 Hz.
            let state = UiState::default();
            let layout = EqLayout::new(10);
            let wheel = layout
                .wheel_rect(0)
                .expect("ten bands have wheels")
                .center()
                + panel();
            let mut harness = Harness::new(ThemeMode::Dark);
            harness.settle(&state);
            let none = Modifiers::default();
            let mut actions = Vec::new();
            for events in [
                vec![Event::PointerMoved(wheel)],
                vec![Event::PointerMoved(wheel), button(wheel, true, none)],
                vec![Event::PointerMoved(wheel - vec2(10.0, 0.0))],
                vec![Event::PointerMoved(wheel - vec2(100.0, 0.0))],
                vec![button(wheel - vec2(100.0, 0.0), false, none)],
            ] {
                actions.extend(harness.frame(&state, events).0);
            }
            let tuned: Vec<f32> = actions
                .iter()
                .filter_map(|action| match action {
                    UiAction::SetBandFrequency(0, hz) => Some(*hz),
                    _ => None,
                })
                .collect();
            let lowest = tuned.iter().copied().fold(f32::MAX, f32::min);
            assert!((46.0..62.5).contains(&lowest), "tuned to {tuned:?}");
        }
    }
}
