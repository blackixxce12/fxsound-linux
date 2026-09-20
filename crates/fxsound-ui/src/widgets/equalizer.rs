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
//! Behind them the parent paints the response curve: a per-segment polyline plus a closed polygon
//! filled with a vertical `EqStart → EqEnd` gradient (`FxEqualizer.cpp:350-393`).
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
//! * **No Alt+drag solo mode.** `FxEqualizer.cpp:123-210` walks every other band down to −10 dB
//!   while you drag one. Alt+drag is the window-move gesture on GNOME and KDE, so the binding is
//!   unusable on Wayland and the feature is left out rather than rebound silently.
//! * **An EQ bypass exists.** `FxEqualizer` has no on/off control at all
//!   (`docs/spec/04-equalizer-visualizer.md` §A15); [`UiState::eq_on`] drives the desaturated
//!   painting the original reserves for the power state, while interaction still follows power
//!   alone so a bypassed curve stays editable.
//! * **The side controls are opt-in.** The band-count combo, the filter-width slider and the
//!   restore-defaults button live in `FxAudioControls`, not here
//!   (`docs/spec/03-controls.md` §5.1). [`EqualizerWidget::with_controls`] draws them at their
//!   original offsets when the caller hands over that column; without it this widget renders only
//!   what `FxEqualizer` itself renders.

use crate::assets::{AssetCache, FxImage};
use crate::state::{UiAction, UiResponse, UiState};
use crate::theme::{self, FxColor, Palette};
use crate::widgets::slider::FxSlider;
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
/// pair; anything else keeps whatever the engine had, which for a freshly created equalizer is the
/// full 20 Hz … 20 kHz span.
#[must_use]
pub fn band_span_hz(num_bands: usize) -> (f32, f32) {
    match num_bands {
        5 | 10 => (62.5, 16_000.0),
        15 => (25.0, 16_000.0),
        20 => (20.0, 16_000.0),
        31 => (20.0, 20_000.0),
        _ => (20.0, 20_000.0),
    }
}

/// How far band `band` may be tuned, in Hz (`GraphicEqGet.cpp:105-168`).
///
/// The bounds sit at the *geometric midpoints* of the generic log-spaced grid, which is why they do
/// not line up with the hard-coded ISO centres: band 1 of the five-band EQ is pinned at 62.5 Hz,
/// the very bottom of its own 62.5…125 range. The `+1` below a kilohertz and `+10` above it are the
/// dead zone that stops two bands ever reaching the same frequency.
#[must_use]
pub fn band_frequency_range(band: usize, num_bands: usize) -> (f32, f32) {
    let (min_hz, max_hz) = band_span_hz(num_bands);
    if num_bands <= 1 {
        return (min_hz, max_hz);
    }
    let ratio = f64::from(max_hz) / f64::from(min_hz);
    let denominator = (num_bands * 2 - 2) as f64;
    let one_based = band + 1;

    let low = if band == 0 {
        min_hz
    } else {
        let power = ((one_based as f64 - 1.0) * 2.0 - 1.0) / denominator;
        let edge = (f64::from(min_hz) * ratio.powf(power)).round() as f32;
        if edge < 1000.0 {
            edge + 1.0
        } else {
            edge + 10.0
        }
    };
    let high = if one_based >= num_bands {
        max_hz
    } else {
        let power = (one_based as f64 * 2.0 - 1.0) / denominator;
        (f64::from(min_hz) * ratio.powf(power)).round() as f32
    };
    (low, high)
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

/// The response curve's vertices in panel space, one per band.
///
/// The original draws straight segments between the sliders, not a computed transfer function
/// (`FxEqualizer.cpp:350-370`), so this really is the whole curve.
#[must_use]
pub fn curve_points(layout: &EqLayout, gains_db: &[f32]) -> Vec<Pos2> {
    gains_db
        .iter()
        .take(layout.num_bands)
        .enumerate()
        .map(|(i, &gain)| pos2(layout.center_x(i), layout.gain_to_y(gain)))
        .collect()
}

/// The closed polygon the gradient fills: the curve, dropped to the baseline at both ends
/// (`FxEqualizer.cpp:372-389`).
#[must_use]
pub fn fill_polygon(layout: &EqLayout, gains_db: &[f32]) -> Vec<Pos2> {
    let curve = curve_points(layout, gains_db);
    if curve.is_empty() {
        return Vec::new();
    }
    let baseline = layout.baseline();
    let mut polygon = Vec::with_capacity(curve.len() + 2);
    polygon.push(pos2(curve[0].x, baseline));
    polygon.extend_from_slice(&curve);
    polygon.push(pos2(curve[curve.len() - 1].x, baseline));
    polygon
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

/// JUCE's `Colour::withSaturation(0.0f)`.
///
/// `withSaturation` round-trips through **HSB**, whose brightness is `max(r, g, b)`, so a
/// zero-saturation colour is that maximum on all three channels — `#e33250` greys to `#e3e3e3`,
/// not to a mid grey. (`widgets::slider` desaturates through HSL instead; the two disagree, and
/// `docs/spec/04-equalizer-visualizer.md` §A8's precomputed table is the HSB one.)
#[must_use]
pub fn desaturate(colour: Color32) -> Color32 {
    let brightness = colour.r().max(colour.g()).max(colour.b());
    Color32::from_rgba_premultiplied(brightness, brightness, brightness, colour.a())
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

/// What the equalizer remembers between frames.
///
/// Only genuinely transient things live here — which band is under the pointer and which control is
/// mid-drag. Everything else is read from [`UiState`], so the widget stays a pure function of the
/// model plus this scratch.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EqInteraction {
    gain_drag: Option<usize>,
    wheel_drag: Option<WheelDrag>,
    hovered: Option<usize>,
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

    /// Forget every in-progress gesture, e.g. after the band count changed underneath us.
    pub fn clear(&mut self) {
        *self = Self::default();
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
    controls: Option<Rect>,
    db_scale: bool,
}

impl<'a> EqualizerWidget<'a> {
    #[must_use]
    pub fn new(state: &'a UiState, interaction: &'a mut EqInteraction) -> Self {
        Self {
            state,
            interaction,
            controls: None,
            db_scale: false,
        }
    }

    /// Also draw the band-count combo, the filter-width slider, the EQ bypass and the
    /// restore-defaults button into `rect`.
    ///
    /// In the original these belong to `FxEqualizerControl`, the back face of the 168 × 257 column
    /// to the left of the panel (`docs/spec/03-controls.md` §5.1), and their offsets here are that
    /// layout's. Pass [`EqualizerWidget::controls_rect`] to put them exactly where Windows does.
    /// Leave it unset when a separate audio-controls view already owns that column.
    #[must_use]
    pub fn with_controls(mut self, rect: Rect) -> Self {
        self.controls = Some(rect);
        self
    }

    /// Draw horizontal guides and captions at −12, −6, 0, +6 and +12 dB.
    ///
    /// An addition: `FxEqualizer` draws no scale at all, so this is off by default.
    #[must_use]
    pub fn with_db_scale(mut self, show: bool) -> Self {
        self.db_scale = show;
        self
    }

    /// The sibling column the side controls belong in: 168 points wide, ending 16 points left of
    /// the panel (`FxProView.cpp:97-98`).
    #[must_use]
    pub fn controls_rect(panel: Rect) -> Rect {
        Rect::from_min_size(
            pos2(panel.left() - 16.0 - 168.0, panel.top()),
            vec2(168.0, PANEL_SIZE.y),
        )
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
            controls,
            db_scale,
        } = self;

        let layout = EqLayout::new(state.eq_bands.len());
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

        let ctx = PaintCtx {
            origin: rect.min.to_vec2(),
            palette,
            // Power gates interaction, exactly as `FxProView::paint` gates `setEnabled`.
            powered: state.controls_enabled(),
            // A bypassed equalizer is drawn dead but stays editable; see the module docs.
            lit: state.controls_enabled() && state.eq_on,
        };

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
            let hit = translate(layout.gain_hit_rect(band), ctx.origin);
            let id = Id::new("fx_eq_band").with(band);
            let sense = if ctx.powered {
                Sense::click_and_drag()
            } else {
                Sense::hover()
            };
            let mut band_response = ui.interact(hit, id, sense);

            if band_response.hovered() {
                interaction.hovered = Some(band);
                if ctx.powered {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            }

            if ctx.powered {
                let mut new_gain = *gain;

                // Right-click resets the band to flat (`FxEqualizer.cpp:481-494`).
                if band_response.secondary_clicked() {
                    new_gain = 0.0;
                } else if band_response.is_pointer_button_down_on()
                    && let Some(pointer) = band_response.interact_pointer_pos()
                {
                    // JUCE's `setSliderSnapsToMousePosition` default: the value jumps to the
                    // pointer on press and then tracks it.
                    new_gain = layout.y_to_gain(pointer.y - ctx.origin.y);
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
            // ten bands (`FxEqualizer.cpp:326-343`).
            if !state.hide_tooltips
                && layout.num_bands == BAND_TOOLTIPS.len()
                && let Some(tip) = BAND_TOOLTIPS.get(band)
            {
                let _ = band_response.on_hover_text(tr(tip));
            }
        }

        if db_scale {
            paint_db_scale(&painter, &ctx, &layout);
        }
        paint_curve_fill(&painter, &ctx, &layout, &gains);
        paint_curve_line(&painter, &ctx, &layout, &gains);

        // `gains` is built from `state.eq_bands`, which is what `layout.num_bands` counts, so the
        // two always agree in length.
        for (band, &gain) in gains.iter().enumerate().take(layout.num_bands) {
            paint_fader(&painter, &ctx, &layout, band, gain, interaction);
            paint_thumb(&painter, ui, assets, &ctx, &layout, band, gain);
            paint_gain_label(&painter, &ctx, &layout, band, gain);
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

        if let Some(controls_rect) = controls {
            side_controls(ui, assets, palette, controls_rect, state, response);
        }
    }
}

/// Everything the painting helpers need that does not change between bands.
struct PaintCtx {
    origin: Vec2,
    palette: Palette,
    /// Whether the user can touch the controls at all — the original's `isEnabled()`.
    powered: bool,
    /// Whether the colours keep their hue.
    lit: bool,
}

impl PaintCtx {
    /// A palette colour, desaturated when the equalizer is not contributing.
    fn colour(&self, id: FxColor, alpha: f32) -> Color32 {
        let base = self.palette.color_alpha(id, alpha);
        if self.lit { base } else { desaturate(base) }
    }

    fn theme_mode(&self) -> ThemeMode {
        self.palette.mode()
    }
}

fn translate(rect: Rect, origin: Vec2) -> Rect {
    rect.translate(origin)
}

/// The `EqStart@0.34 → EqEnd@0.00` ramp, anchored to band 1's fader rather than to the panel
/// (`FxEqualizer.cpp:391`).
fn fill_colour_at(ctx: &PaintCtx, layout: &EqLayout, panel_y: f32) -> Color32 {
    let top = Y_MARGIN;
    let bottom = Y_MARGIN + layout.slider_height;
    let t = ((panel_y - top) / (bottom - top)).clamp(0.0, 1.0);
    let start = ctx.colour(FxColor::EqStart, 0.34);
    let end = ctx.colour(FxColor::EqEnd, 0.0);
    start.lerp_to_gamma(end, t)
}

/// The filled area under the curve, as one gradient-shaded quad per band pair.
///
/// egui has no gradient brush, and the polygon is not convex, so it goes out as a `Mesh` with
/// per-vertex colours. The polygon is x-monotone with a flat bottom, which makes the fan trivially
/// correct.
fn paint_curve_fill(painter: &egui::Painter, ctx: &PaintCtx, layout: &EqLayout, gains: &[f32]) {
    let curve = curve_points(layout, gains);
    if curve.len() < 2 {
        return;
    }
    let baseline = layout.baseline();
    let baseline_colour = fill_colour_at(ctx, layout, baseline);

    let mut mesh = Mesh::default();
    mesh.reserve_vertices(curve.len() * 2);
    mesh.reserve_triangles((curve.len() - 1) * 2);
    for point in &curve {
        let top = pos2(point.x, point.y) + ctx.origin;
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

/// The curve itself: one independent segment per band pair, with butt caps at every vertex because
/// the original clears its `Path` between segments (`FxEqualizer.cpp:350-370`).
///
/// The original's `addLineSegment(line, 1.0)` + `strokePath(PathStrokeType(1.0))` lays down roughly
/// two pixels of ink around a hollow core; 1.5 px is the closest single stroke.
fn paint_curve_line(painter: &egui::Painter, ctx: &PaintCtx, layout: &EqLayout, gains: &[f32]) {
    let curve = curve_points(layout, gains);
    let colour = ctx.colour(FxColor::SliderTrack, 1.0);
    for pair in curve.windows(2) {
        painter.line_segment(
            [pair[0] + ctx.origin, pair[1] + ctx.origin],
            Stroke::new(1.5, colour),
        );
    }
}

/// One fader's dashed track and, while it is being dragged or focused, its highlight.
///
/// The dash gradient runs `SliderTrack@0.4` at the component's own top down to
/// `VerticalSliderLow@0.4` at `region_size` below it — *not* at the line's own end
/// (`FxTheme.cpp:201-202`), so the bottom quarter of every track is flat colour. That mismatch is
/// reproduced rather than corrected.
fn paint_fader(
    painter: &egui::Painter,
    ctx: &PaintCtx,
    layout: &EqLayout,
    band: usize,
    gain_db: f32,
    interaction: &EqInteraction,
) {
    let x = layout.center_x(band) + ctx.origin.x;
    let top = layout.track_top();
    let bottom = layout.track_bottom();
    let colour_top = ctx.colour(FxColor::SliderTrack, 0.4);
    let colour_bottom = ctx.colour(FxColor::VerticalSliderLow, 0.4);

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

    let _ = gain_db;
}

/// The thumb: `Slider_Thumb.svg` in a 16 × 16 box centred on the value, or the grey variant when
/// the equalizer is not contributing (`FxTheme.cpp:204-207`).
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
    let image = if ctx.lit {
        FxImage::SliderThumb
    } else {
        FxImage::SliderThumbBW
    };
    draw_image(painter, ui, assets, image, ctx.theme_mode(), rect);
}

/// The floating gain caption, which `FxProView` keeps visible at all times since v2.0
/// (`FxProView.cpp:70`).
fn paint_gain_label(
    painter: &egui::Painter,
    ctx: &PaintCtx,
    layout: &EqLayout,
    band: usize,
    gain_db: f32,
) {
    if !ctx.powered {
        // `showValue(show)` is `show && isEnabled()` (`FxEqualizer.cpp:423-426`).
        return;
    }
    let rect = translate(layout.gain_label_rect(band, gain_db), ctx.origin);
    painter.text(
        rect.center_top(),
        Align2::CENTER_TOP,
        gain_label(gain_db),
        theme::semibold(LABEL_HEIGHT),
        ctx.palette.color(FxColor::DefaultText),
    );
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
        let _ = wheel_response.on_hover_text(tr(WHEEL_TOOLTIP));
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
// The side controls (FxEqualizerControl, docs/spec/03-controls.md §5)
// ---------------------------------------------------------------------------------------------

/// Offsets inside the 168 × 257 column (`FxAudioControls.cpp:430-460`).
pub mod controls {
    use egui::{Rect, Vec2, vec2};

    pub const X_MARGIN: f32 = 8.0;
    pub const COMBO_SIZE: Vec2 = vec2(152.0, 20.0);
    pub const SLIDER_SIZE: Vec2 = vec2(160.0, 18.0);
    pub const CAPTION_HEIGHT: f32 = 14.0;
    pub const BUTTON_SIZE: Vec2 = vec2(18.0, 18.0);

    /// The band-count combo (`FxAudioControls.cpp:436`).
    #[must_use]
    pub fn band_combo(panel: Rect) -> Rect {
        Rect::from_min_size(panel.min + vec2(X_MARGIN, 28.0), COMBO_SIZE)
    }

    /// The "Filter Q" caption (`FxAudioControls.cpp:447`).
    #[must_use]
    pub fn filter_q_caption(panel: Rect) -> Rect {
        Rect::from_min_size(panel.min + vec2(16.0, 138.0), vec2(160.0, CAPTION_HEIGHT))
    }

    /// The filter-width slider (`FxAudioControls.cpp:448`).
    #[must_use]
    pub fn filter_q_slider(panel: Rect) -> Rect {
        Rect::from_min_size(panel.min + vec2(X_MARGIN, 153.0), SLIDER_SIZE)
    }

    /// The restore-defaults button (`FxAudioControls.cpp:459`).
    #[must_use]
    pub fn restore_defaults(panel: Rect) -> Rect {
        Rect::from_min_size(panel.min + vec2(X_MARGIN, 234.0), BUTTON_SIZE)
    }

    /// The EQ bypass switch. Not in the original — it occupies the strip the flip button shares
    /// (`docs/spec/04-equalizer-visualizer.md` §A15 recommends adding one).
    #[must_use]
    pub fn bypass(panel: Rect) -> Rect {
        Rect::from_min_size(panel.min + vec2(X_MARGIN, 5.0), vec2(36.0, 18.0))
    }

    /// The caption next to the bypass switch.
    #[must_use]
    pub fn bypass_caption(panel: Rect) -> Rect {
        Rect::from_min_size(panel.min + vec2(50.0, 5.0), vec2(80.0, 18.0))
    }

    /// The label shown for a band count (`FxAudioControls.cpp:301`).
    #[must_use]
    pub fn band_count_label(count: usize) -> String {
        format!("{count} Bands")
    }
}

fn side_controls(
    ui: &mut Ui,
    assets: &mut AssetCache,
    palette: Palette,
    rect: Rect,
    state: &UiState,
    response: &mut UiResponse,
) {
    let enabled = state.controls_enabled();
    let text_colour = palette.color(FxColor::DefaultText);

    // --- EQ bypass ---------------------------------------------------------------------------
    let bypass_rect = controls::bypass(rect);
    let bypass_response = ui.interact(
        bypass_rect,
        Id::new("fx_eq_bypass"),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if bypass_response.clicked() {
        response.push(UiAction::SetEqEnabled(!state.eq_on));
    }
    {
        let painter = ui.painter();
        let track = if state.eq_on {
            palette.color(FxColor::SliderTrack)
        } else {
            palette.color_alpha(FxColor::SliderTrack, 0.2)
        };
        let track = if enabled { track } else { desaturate(track) };
        painter.rect_filled(
            bypass_rect,
            CornerRadius::same((bypass_rect.height() / 2.0) as u8),
            track,
        );
        let knob_x = if state.eq_on {
            bypass_rect.right() - bypass_rect.height() / 2.0
        } else {
            bypass_rect.left() + bypass_rect.height() / 2.0
        };
        let knob_colour = if state.eq_on {
            palette.color(FxColor::ControlBackground)
        } else {
            text_colour
        };
        painter.circle_filled(
            pos2(knob_x, bypass_rect.center().y),
            bypass_rect.height() / 2.0 - 2.0,
            knob_colour,
        );
        painter.text(
            controls::bypass_caption(rect).left_center(),
            Align2::LEFT_CENTER,
            "EQ",
            theme::semibold(12.0),
            text_colour,
        );
    }

    // --- band count --------------------------------------------------------------------------
    let combo_rect = controls::band_combo(rect);
    let combo_response = ui.interact(
        combo_rect,
        Id::new("fx_eq_band_combo"),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    {
        let painter = ui.painter();
        painter.rect_filled(
            combo_rect,
            // `height / 5` (`FxTheme.cpp:138`).
            CornerRadius::same((combo_rect.height() / 5.0) as u8),
            palette.color(FxColor::ComboBoxBackground),
        );
        painter.text(
            combo_rect.left_center() + vec2(5.0, 0.0),
            Align2::LEFT_CENTER,
            controls::band_count_label(state.eq_bands.len()),
            theme::semibold(14.0),
            text_colour,
        );
    }
    let arrow_rect = Rect::from_center_size(
        pos2(combo_rect.right() - 26.0, combo_rect.center().y),
        Vec2::splat(12.0),
    );
    let arrow = if combo_response.hovered() {
        FxImage::DropDownArrowHover
    } else {
        FxImage::DropDownArrow
    };
    {
        let painter = ui.painter().clone();
        draw_image(&painter, ui, assets, arrow, palette.mode(), arrow_rect);
    }
    if combo_response.clicked() {
        egui::Popup::toggle_id(ui.ctx(), egui::Popup::default_response_id(&combo_response));
    }
    egui::Popup::from_response(&combo_response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
        .show(|ui| {
            for count in BAND_COUNTS {
                let selected = count == state.eq_bands.len();
                if ui
                    .selectable_label(selected, controls::band_count_label(count))
                    .clicked()
                {
                    response.push(UiAction::SetBandCount(count));
                }
            }
        });

    // --- filter width ------------------------------------------------------------------------
    ui.painter().text(
        controls::filter_q_caption(rect).left_top(),
        Align2::LEFT_TOP,
        tr("Filter Q"),
        theme::semibold(controls::CAPTION_HEIGHT),
        text_colour,
    );
    let mut filter_q = state.filter_q;
    // 1 … 3 in halves, right-click back to 1 (`FxAudioControls.cpp:347-348`, `:271`).
    let slider = FxSlider::new(&mut filter_q, 1.0, 3.0, 0.5)
        .default_value(1.0)
        .reset_on_secondary_click(true)
        .enabled(enabled);
    slider.show(
        ui,
        controls::filter_q_slider(rect),
        palette,
        assets,
        "fx_eq_filter_q",
    );
    if filter_q != state.filter_q {
        response.push(UiAction::SetFilterQ(filter_q));
    }

    // --- restore defaults --------------------------------------------------------------------
    let reset_rect = controls::restore_defaults(rect);
    let reset_response = ui.interact(
        reset_rect,
        Id::new("fx_eq_restore_defaults"),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let reset_image = if reset_response.hovered() {
        FxImage::RestoreDefaultsButtonHover
    } else {
        FxImage::RestoreDefaultsButton
    };
    {
        let painter = ui.painter().clone();
        draw_image(
            &painter,
            ui,
            assets,
            reset_image,
            palette.mode(),
            reset_rect,
        );
    }
    if reset_response.hovered() && enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if reset_response.clicked() {
        response.push(UiAction::RestoreDefaults);
    }
    if !state.hide_tooltips {
        let _ = reset_response.on_hover_text(tr("Restore Defaults"));
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

    /// §A4's full range tables for the five selectable band counts.
    #[test]
    fn band_frequency_ranges_match_the_spec_tables() {
        let five = [
            (62.5_f32, 125.0_f32),
            (126.0, 500.0),
            (501.0, 2000.0),
            (2010.0, 8000.0),
            (8010.0, 16000.0),
        ];
        for (band, expected) in five.into_iter().enumerate() {
            assert_eq!(
                band_frequency_range(band, 5),
                expected,
                "5 bands, band {band}"
            );
        }

        let ten = [
            (62.5_f32, 85.0_f32),
            (86.0, 157.0),
            (158.0, 292.0),
            (293.0, 540.0),
            (541.0, 1000.0),
            (1010.0, 1852.0),
            (1862.0, 3429.0),
            (3439.0, 6350.0),
            (6360.0, 11758.0),
            (11768.0, 16000.0),
        ];
        for (band, expected) in ten.into_iter().enumerate() {
            assert_eq!(
                band_frequency_range(band, 10),
                expected,
                "10 bands, band {band}"
            );
        }

        // Spot checks from the 15, 20 and 31 band tables, including both ends.
        assert_eq!(band_frequency_range(0, 15), (25.0, 31.0));
        assert_eq!(band_frequency_range(8, 15), (798.0, 1264.0));
        assert_eq!(band_frequency_range(14, 15), (12713.0, 16000.0));
        assert_eq!(band_frequency_range(0, 20), (20.0, 24.0));
        assert_eq!(band_frequency_range(11, 20), (805.0, 1143.0));
        assert_eq!(band_frequency_range(19, 20), (13429.0, 16000.0));
        assert_eq!(band_frequency_range(0, 31), (20.0, 22.0));
        assert_eq!(band_frequency_range(16, 31), (711.0, 893.0));
        assert_eq!(band_frequency_range(30, 31), (17835.0, 20000.0));
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
        let step = frequency_step(0, 10);
        assert!(
            (step - 0.225).abs() < 1e-4,
            "band 1 of ten stepped by {step}"
        );
        let step = frequency_step(4, 5);
        assert!(
            (step - 79.9).abs() < 1e-3,
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
        // Band 1 of ten spans 62.5 … 85 in steps of 0.225, and the range's own low edge is the
        // grid's origin, so only 62.5 + k * 0.225 is reachable.
        for (raw, expected) in [
            (62.5_f32, 62.5_f32),
            (0.0, 62.5),
            (1e6, 85.0),
            // 70 Hz is 33.33 steps up from 62.5, and JUCE rounds that to 33: 62.5 + 7.425.
            (70.0, 69.925),
        ] {
            let snapped = snap_frequency(raw, 0, 10);
            assert!(
                (snapped - expected).abs() < 1e-3,
                "{raw} Hz snapped to {snapped}, not {expected}"
            );
        }
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

    #[test]
    fn the_curve_is_one_vertex_per_band_at_the_fader_centres() {
        let layout = EqLayout::new(10);
        let gains = [0.0, 6.0, -6.0, 12.0, -12.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let curve = curve_points(&layout, &gains);
        assert_eq!(curve.len(), 10);
        assert_eq!(curve[0], pos2(53.0, 106.0));
        assert_eq!(curve[1], pos2(127.0, 69.0));
        assert_eq!(curve[2], pos2(201.0, 143.0));
        assert_eq!(curve[3], pos2(275.0, 32.0));
        assert_eq!(curve[4], pos2(349.0, 180.0));
    }

    #[test]
    fn the_fill_polygon_closes_on_the_baseline_at_both_ends() {
        let layout = EqLayout::new(10);
        let gains = [0.0_f32; 10];
        let polygon = fill_polygon(&layout, &gains);
        assert_eq!(polygon.len(), 12);
        assert_eq!(polygon[0], pos2(53.0, 180.0));
        assert_eq!(polygon[1], pos2(53.0, 106.0));
        assert_eq!(polygon[10], pos2(719.0, 106.0));
        assert_eq!(polygon[11], pos2(719.0, 180.0));
    }

    #[test]
    fn a_fully_cut_curve_has_no_area() {
        let layout = EqLayout::new(10);
        let gains = [-12.0_f32; 10];
        let polygon = fill_polygon(&layout, &gains);
        assert!(
            polygon.iter().all(|p| p.y == layout.baseline()),
            "every vertex should collapse onto the baseline"
        );
    }

    #[test]
    fn an_empty_band_list_produces_no_curve() {
        let layout = EqLayout::new(0);
        assert!(curve_points(&layout, &[]).is_empty());
        assert!(fill_polygon(&layout, &[]).is_empty());
        // And the degenerate layout still answers every question finitely: the gain axis is fixed
        // by the fader height alone, so a y that reads +1 dB at ten bands reads +1 dB at none.
        assert!(layout.gain_to_y(0.0).is_finite());
        let gain = layout.y_to_gain(100.0);
        assert!((gain - 1.0).abs() < 1e-6, "y = 100 gave {gain} dB, not 1");
        assert!((gain - EqLayout::new(10).y_to_gain(100.0)).abs() < 1e-6);
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

    /// §A8's precomputed greys — `Colour::withSaturation(0)` keeps HSB brightness, i.e. the
    /// channel maximum.
    #[test]
    fn desaturation_matches_the_spec_grey_table() {
        let cases = [
            (Color32::from_rgb(0xe3, 0x32, 0x50), 0xe3), // SliderTrack, dark
            (Color32::from_rgb(0x0a, 0x4d, 0x66), 0x66), // SliderTrack, light
            (Color32::from_rgb(0xd5, 0x15, 0x35), 0xd5), // GraphHigh, dark
            (Color32::from_rgb(0xef, 0x4b, 0x65), 0xef), // EqStart, dark
            (Color32::from_rgb(0x74, 0x28, 0x34), 0x74), // EqEnd, dark
            (Color32::from_rgb(0x06, 0x32, 0x44), 0x44), // EqEnd, light
        ];
        for (colour, expected) in cases {
            let grey = desaturate(colour);
            assert_eq!(
                (grey.r(), grey.g(), grey.b()),
                (expected, expected, expected),
                "{colour:?}"
            );
        }
    }

    #[test]
    fn desaturation_keeps_the_alpha() {
        let translucent = Color32::from_rgba_premultiplied(0x74, 0x28, 0x34, 0x55);
        assert_eq!(desaturate(translucent).a(), 0x55);
    }

    #[test]
    fn the_side_controls_land_on_the_original_offsets() {
        // docs/spec/03-controls.md §5.1, resolved against a column at the origin.
        let panel = Rect::from_min_size(pos2(0.0, 0.0), vec2(168.0, 257.0));
        assert_eq!(
            controls::band_combo(panel),
            Rect::from_min_size(pos2(8.0, 28.0), vec2(152.0, 20.0))
        );
        assert_eq!(
            controls::filter_q_caption(panel),
            Rect::from_min_size(pos2(16.0, 138.0), vec2(160.0, 14.0))
        );
        assert_eq!(
            controls::filter_q_slider(panel),
            Rect::from_min_size(pos2(8.0, 153.0), vec2(160.0, 18.0))
        );
        assert_eq!(
            controls::restore_defaults(panel),
            Rect::from_min_size(pos2(8.0, 234.0), vec2(18.0, 18.0))
        );
        // The bypass switch shares the top strip with the flip button at x = 145.
        assert!(controls::bypass(panel).right() < 145.0);
    }

    #[test]
    fn the_controls_column_sits_where_fxproview_puts_it() {
        // FxProView.cpp:97-98 — audio controls at x 40, equalizer 16 points to their right.
        let panel = crate::layout::pro::equalizer();
        assert_eq!(
            EqualizerWidget::controls_rect(panel),
            crate::layout::pro::audio_controls()
        );
    }

    #[test]
    fn the_band_count_combo_spells_its_items_like_the_original() {
        assert_eq!(controls::band_count_label(5), "5 Bands");
        assert_eq!(controls::band_count_label(31), "31 Bands");
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
}
