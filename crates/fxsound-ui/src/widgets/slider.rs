//! The shared horizontal slider.
//!
//! Every linear slider in FxSound — the five effect knobs, master gain, volume levelling, filter
//! width and balance — is the same 160 × 18 JUCE `LinearHorizontal` slider drawn by
//! `FxTheme::drawLinearSlider` (`fxsound/Source/GUI/FxTheme.cpp:217-250`). Reproducing that one
//! painter gets the whole control surface right.
//!
//! ## Geometry, derived once
//!
//! JUCE insets the track by the thumb radius, then `FxTheme::getSliderLayout` takes another
//! `4 × radius` off the width (`FxTheme.cpp:344-348`). For the 160 × 18 slider the app actually
//! uses that gives a 112 px track starting 8 px in:
//!
//! ```text
//! component  (0, 0, 160, 18)
//!   inset 8  (8, 0, 144, 18)   JUCE thumbIndent = getSliderThumbRadius()
//!   w -= 32  (8, 0, 112, 18)   FxTheme::getSliderLayout
//! ```
//!
//! ## Interaction
//!
//! None of the sliders customise JUCE's defaults, so: clicking the track jumps to that position
//! and starts a drag, the wheel steps by the interval, the arrow keys step by the interval, and
//! **double-click does nothing** — FxSound has no double-click-to-reset. Right-click resets to the
//! default on the audio sliders and on balance, and does nothing on the five effect sliders.
//!
//! Two departures (0.4.0 audit #14). On **every** slider a press on the thumb moves nothing until
//! the pointer does, where JUCE jumps to the pointer there too: a value can sit between a
//! slider's positions — a preset's Surround of 1.6 among eleven positions over 128 stored
//! values, a `--master_gain=3` on the speakers' 2 dB interval — and a touch snapped it to one,
//! and even on a position a press half a thumb off centre moved the master gain or the balance
//! by a whole 2 dB step. And on a slider whose values are finer than its interval, the five
//! effects, **Shift** makes the arrows, the wheel and a drag move by one stored value
//! ([`FineSteps`]).
//!
//! ## Balance
//!
//! `FxBalanceSlider` is the one slider that paints itself (`FxBalanceSlider.cpp:65-105`): no
//! filled/unfilled split, but one bar whose two ends fade — the left end at `1 − t` alpha, the
//! right at `t` — so the thumb's side of centre reads as the louder one. [`Track::Balance`] draws
//! that bar; everything else about the slider (thumb, focus halo, gestures) is shared.

use crate::assets::{AssetCache, FxImage};
use crate::theme::{FxColor, Palette};
use egui::{CornerRadius, Id, Rect, Response, Sense, Ui, Vec2, pos2, vec2};
use fxsound_core::ThemeMode;

/// `FxTheme::SLIDER_THUMB_RADIUS`.
pub const THUMB_RADIUS: f32 = 8.0;
/// Height of the track bar itself.
const TRACK_THICKNESS: f32 = 3.0;
/// `FxTheme.cpp:228`.
const TRACK_CORNER_RADIUS: f32 = 5.6;

/// How faithfully to reproduce the original's painting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fidelity {
    /// Reproduce `drawLinearSlider` exactly, including the filled-track overshoot: the original
    /// passes the thumb's absolute x as the fill's *width*, so the fill is 8 px wide at the
    /// minimum and runs 8 px past the track at the maximum (`FxTheme.cpp:230-237`).
    ///
    /// This is the default, because matching the Windows build pixel for pixel is the point of
    /// this port.
    #[default]
    Faithful,
    /// Fill the track from its start to the thumb, which is what the original clearly meant.
    Corrected,
}

/// Which of the original's two track painters a slider uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Track {
    /// `FxTheme::drawLinearSlider`: a 20 % track, filled at full alpha up to the thumb.
    #[default]
    Filled,
    /// `FxBalanceSlider::paint`: one bar in a horizontal gradient from `SliderTrack` at `1 − t`
    /// alpha on the left to `t` on the right, with no fill (`FxBalanceSlider.cpp:79-92`).
    ///
    /// [`Fidelity::Faithful`] keeps the original's slip: the gradient's end is given as the
    /// track's *width*, 112, where an x was meant, so it stops eight points short of the track's
    /// end at 120 and the last eight points are the end colour (`docs/spec/03-controls.md` §3.4).
    Balance,
}

/// The values between a slider's whole steps that it can also stand at: what Shift reaches.
pub trait FineSteps {
    /// The value one fine step from `value`, up or down; `value` itself at either end.
    fn step(&self, value: f32, up: bool) -> f32;
    /// The fine value nearest `value`.
    fn nearest(&self, value: f32) -> f32;
}

/// A horizontal slider drawn like FxSound's.
pub struct FxSlider<'a> {
    value: &'a mut f32,
    min: f32,
    max: f32,
    step: f32,
    default: f32,
    enabled: bool,
    lit: bool,
    reset_on_secondary_click: bool,
    fidelity: Fidelity,
    track: Track,
    fine: Option<&'a dyn FineSteps>,
}

impl<'a> FxSlider<'a> {
    /// A slider over `min..=max` stepping by `step`.
    pub fn new(value: &'a mut f32, min: f32, max: f32, step: f32) -> Self {
        Self {
            value,
            min,
            max,
            step,
            default: min,
            enabled: true,
            lit: true,
            reset_on_secondary_click: false,
            fidelity: Fidelity::default(),
            track: Track::default(),
            fine: None,
        }
    }

    /// The values between the whole steps this slider can stand at, which the arrows, the wheel
    /// and a drag reach with Shift held (0.4.0 audit #14). Without them Shift does nothing.
    #[must_use]
    pub fn fine_steps(mut self, fine: &'a dyn FineSteps) -> Self {
        self.fine = Some(fine);
        self
    }

    /// The value a right-click resets to. Only meaningful with
    /// [`FxSlider::reset_on_secondary_click`].
    #[must_use]
    pub fn default_value(mut self, default: f32) -> Self {
        self.default = default;
        self
    }

    /// Enable right-click-to-reset, which `FxAudioSlider` and `FxBalanceSlider` have and the five
    /// effect sliders do not.
    #[must_use]
    pub fn reset_on_secondary_click(mut self, reset: bool) -> Self {
        self.reset_on_secondary_click = reset;
        self
    }

    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Draw the slider grey while leaving it live: the control is there to be set, but what it
    /// sets is not in the signal path right now — the level controls while the equalizer, whose
    /// block they belong to, is switched off. The equalizer panel draws its own faders the same
    /// way for the same reason. A disabled slider is always drawn grey.
    #[must_use]
    pub fn lit(mut self, lit: bool) -> Self {
        self.lit = lit;
        self
    }

    #[must_use]
    pub fn fidelity(mut self, fidelity: Fidelity) -> Self {
        self.fidelity = fidelity;
        self
    }

    /// Which track painter to use; [`Track::Filled`] unless this is the balance slider.
    #[must_use]
    pub fn track(mut self, track: Track) -> Self {
        self.track = track;
        self
    }

    /// Draw the slider into an exact rectangle and report what the user did.
    ///
    /// `rect` is the whole 160 × 18 control, not the track.
    pub fn show(
        self,
        ui: &mut Ui,
        rect: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> Response {
        let Self {
            value,
            min,
            max,
            step,
            default,
            enabled,
            lit,
            reset_on_secondary_click,
            fidelity,
            track: style,
            fine,
        } = self;

        let id = Id::new("fx_slider").with(id_salt);
        let sense = if enabled {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        };
        let mut response = ui.interact(rect, id, sense);

        let track = track_rect(rect);
        let span = (max - min).max(f32::EPSILON);

        if enabled {
            let mut new_value = *value;
            // Only a gesture may change the value. Without this guard the slider would quantise
            // whatever it was handed and report that as a change: a preset stores its knobs as
            // MIDI, so `51/127 * 10 = 4.016` arrives here, snaps to 4, and the first frame after
            // loading a preset would mark it modified before the user has touched anything.
            let mut interacted = false;

            // `FxAudioSlider` and `FxBalanceSlider` take the right button for themselves and never
            // hand it to `Slider::mouseDown` (`FxAudioSlider.cpp:74-87`), so a right-click on them
            // resets without first dragging the thumb to the pointer.
            let resetting = reset_on_secondary_click
                && ui.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
            // Shift asks for the fine values, where the slider has them.
            let fine = fine.filter(|_| ui.input(|i| i.modifiers.shift));

            // Clicking the track jumps to that position and starts the drag from there, which is
            // JUCE's `snapsToMousePos` default — except a press on the thumb, which holds the
            // value until the pointer moves (see the module documentation).
            if !resetting
                && response.is_pointer_button_down_on()
                && let Some(pointer) = response.interact_pointer_pos()
                && !held_on_thumb(ui, id, rect, track, (*value - min) / span, pointer)
            {
                let t = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
                new_value = min + t * span;
                if let Some(fine) = fine {
                    new_value = fine.nearest(new_value);
                }
                interacted = true;
            }

            if reset_on_secondary_click && response.secondary_clicked() {
                new_value = default;
                interacted = true;
            }

            if response.hovered() {
                // 0.36 exposes only the smoothed delta; one notch is still one step, which is
                // what JUCE's default wheel handling does. With Shift held egui turns the wheel
                // sideways, so the fine step reads the other axis.
                let scroll = ui.input(|i| {
                    if fine.is_some() {
                        i.smooth_scroll_delta.x + i.smooth_scroll_delta.y
                    } else {
                        i.smooth_scroll_delta.y
                    }
                });
                if scroll != 0.0 {
                    new_value = match fine {
                        Some(fine) => fine.step(new_value, scroll > 0.0),
                        None => step_towards(new_value, min, step, scroll > 0.0),
                    };
                    interacted = true;
                }
            }

            if response.has_focus() {
                let stepped = ui.input(|i| {
                    let mut delta = 0;
                    if i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::ArrowRight) {
                        delta += 1;
                    }
                    if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::ArrowLeft) {
                        delta -= 1;
                    }
                    delta
                });
                if stepped != 0 {
                    new_value = match fine {
                        Some(fine) => fine.step(new_value, stepped > 0),
                        None => step_towards(new_value, min, step, stepped > 0),
                    };
                    interacted = true;
                }
            }

            if interacted {
                // A fine value is already one the slider can stand at; snapping it to the
                // interval would take it straight back to a whole step.
                let new_value = if fine.is_some() {
                    new_value.clamp(min.min(max), max.max(min))
                } else {
                    quantise(new_value, min, max, step)
                };
                if new_value != *value {
                    *value = new_value;
                    response.mark_changed();
                }
            }
        }

        paint(
            ui,
            rect,
            track,
            *value,
            (min, max),
            palette,
            assets,
            enabled && lit,
            fidelity,
            style,
            &response,
        );
        response
    }
}

/// Whether the press under way began on the thumb and the pointer has not left its starting
/// point since, in which case the slider holds its value: a touch is not a move (0.4.0 audit
/// #14), on every slider, the levels as much as the effects. Once the pointer has moved two
/// points the press is an ordinary drag for the rest of its life, back over its starting point
/// included.
fn held_on_thumb(ui: &Ui, id: Id, rect: Rect, track: Rect, t: f32, pointer: egui::Pos2) -> bool {
    let Some(origin) = ui.input(|i| i.pointer.press_origin()) else {
        return false;
    };
    let key = id.with("held_on_thumb");
    let (pressed_at, mut holding) = ui
        .data(|d| d.get_temp::<(egui::Pos2, bool)>(key))
        .filter(|(pressed_at, _)| *pressed_at == origin)
        .unwrap_or_else(|| {
            let thumb = pos2(
                track.left() + track.width() * t.clamp(0.0, 1.0),
                rect.center().y,
            );
            (origin, origin.distance(thumb) <= THUMB_RADIUS)
        });
    if holding && (pointer.x - pressed_at.x).abs() >= 2.0 {
        holding = false;
    }
    ui.data_mut(|d| d.insert_temp(key, (pressed_at, holding)));
    holding
}

/// The track rectangle inside a slider component.
#[must_use]
pub fn track_rect(rect: Rect) -> Rect {
    let x = rect.left() + THUMB_RADIUS;
    // JUCE insets by the thumb radius on both sides, then FxTheme removes 4 × radius more.
    let width = (rect.width() - THUMB_RADIUS * 2.0 - THUMB_RADIUS * 4.0).max(1.0);
    // `(height - 3) / 2` in the original is integer division.
    let y = rect.top() + ((rect.height() - TRACK_THICKNESS) / 2.0).floor();
    Rect::from_min_size(pos2(x, y), vec2(width, TRACK_THICKNESS))
}

/// Snap a value to the slider's interval and clamp it to the range.
#[must_use]
pub fn quantise(value: f32, min: f32, max: f32, step: f32) -> f32 {
    let clamped = value.clamp(min.min(max), max.max(min));
    if step <= 0.0 {
        return clamped;
    }
    let steps = ((clamped - min) / step).round();
    (min + steps * step).clamp(min.min(max), max.max(min))
}

/// Where one arrow key or wheel notch takes `value` on a slider whose positions sit every
/// `step` from `min`: one position on from a value that is on one, and the next position in the
/// direction of travel from a value between two.
///
/// Adding a whole step to a value between positions and then rounding skipped a position:
/// Surround at 1.57 went up to 3 and `--master_gain=3` on the 2 dB step went up to 6 (0.4.0
/// audit #14 keeps such values until the user moves the slider). The result is not clamped;
/// [`quantise`] does that.
#[must_use]
pub fn step_towards(value: f32, min: f32, step: f32, up: bool) -> f32 {
    if step <= 0.0 {
        return value;
    }
    let steps = (value - min) / step;
    let nearest = steps.round();
    // A value on a position up to float noise, such as a sum of tenths, counts as on it.
    let target = if (steps - nearest).abs() <= 1e-3 {
        nearest + if up { 1.0 } else { -1.0 }
    } else if up {
        steps.ceil()
    } else {
        steps.floor()
    };
    min + target * step
}

/// Where the balance gradient stops: at the track's end, or — reproducing
/// `ColourGradient::horizontal(left, x, right, width)` passing a width where an x belongs
/// (`FxBalanceSlider.cpp:89`) — the track's width in from the slider's own left edge.
#[must_use]
pub fn balance_gradient_end(rect: Rect, track: Rect, fidelity: Fidelity) -> f32 {
    match fidelity {
        Fidelity::Faithful => rect.left() + track.width(),
        Fidelity::Corrected => track.right(),
    }
}

/// The balance bar's two end colours for a value at proportion `t` of the range: `SliderTrack` at
/// `1 − t` alpha on the left and at `t` on the right (`FxBalanceSlider.cpp:79-82`), both grey while
/// the slider is not lit (`:83-87`).
#[must_use]
pub fn balance_colours(palette: Palette, t: f32, lit: bool) -> (egui::Color32, egui::Color32) {
    let t = t.clamp(0.0, 1.0);
    let base = palette.color(FxColor::SliderTrack);
    let base = if lit { base } else { desaturate(base) };
    (with_alpha(base, 1.0 - t), with_alpha(base, t))
}

#[allow(clippy::too_many_arguments)]
fn paint(
    ui: &Ui,
    rect: Rect,
    track: Rect,
    value: f32,
    (min, max): (f32, f32),
    palette: Palette,
    assets: &mut AssetCache,
    lit: bool,
    fidelity: Fidelity,
    style: Track,
    response: &Response,
) {
    let painter = ui.painter();
    let span = (max - min).max(f32::EPSILON);
    let t = ((value - min) / span).clamp(0.0, 1.0);
    // The original's `sliderPos` is an absolute component coordinate, not an offset.
    let thumb_x = track.left() + track.width() * t;

    let corner = CornerRadius::same(TRACK_CORNER_RADIUS as u8);
    let track_colour = palette.color(FxColor::SliderTrack);
    let track_colour = if lit {
        track_colour
    } else {
        desaturate(track_colour)
    };

    match style {
        Track::Filled => {
            // 1. The unfilled track at 20% alpha.
            painter.rect_filled(track, corner, with_alpha(track_colour, 0.2));

            // 2. The filled portion at full alpha.
            let fill_width = match fidelity {
                Fidelity::Faithful => thumb_x - rect.left(),
                Fidelity::Corrected => track.width() * t,
            };
            if fill_width > 0.0 {
                let fill = Rect::from_min_size(track.min, vec2(fill_width, track.height()));
                painter.rect_filled(fill, corner, track_colour);
            }
        }
        Track::Balance => {
            // One bar, its two ends fading against each other. A mesh, because egui fills a
            // rectangle with one colour; the 5.6 corner on a 3 point bar is a 1.5 point rounding
            // that a gradient mesh leaves square.
            let (left, right) = balance_colours(palette, t, lit);
            let end = balance_gradient_end(rect, track, fidelity).min(track.right());
            let mut mesh = egui::Mesh::default();
            let mut quad = |from: f32, to: f32, a: egui::Color32, b: egui::Color32| {
                let base = mesh.vertices.len() as u32;
                for (x, y, colour) in [
                    (from, track.top(), a),
                    (to, track.top(), b),
                    (to, track.bottom(), b),
                    (from, track.bottom(), a),
                ] {
                    mesh.colored_vertex(pos2(x, y), colour);
                }
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base, base + 2, base + 3);
            };
            quad(track.left(), end, left, right);
            if end < track.right() {
                // Past a gradient's end point JUCE paints its end colour.
                quad(end, track.right(), right, right);
            }
            painter.add(egui::Shape::mesh(mesh));
        }
    }

    // 3. The thumb.
    let thumb_image = if lit {
        FxImage::SliderThumb
    } else {
        FxImage::SliderThumbBW
    };
    let thumb_rect = Rect::from_center_size(
        pos2(thumb_x, rect.top() + rect.height() / 2.0),
        Vec2::splat(THUMB_RADIUS * 2.0),
    );
    let theme = if palette.is_dark() {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    if let Some(texture) = assets.texture(ui.ctx(), thumb_image, theme, thumb_rect.size()) {
        painter.image(
            texture.id(),
            thumb_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }

    // 4. The keyboard-focus halo.
    if response.has_focus() {
        let halo = track.expand(THUMB_RADIUS / 2.0);
        painter.rect_filled(
            halo,
            CornerRadius::same((rect.height() + THUMB_RADIUS) as u8),
            with_alpha(palette.color(FxColor::SliderHighlight), 0.1),
        );
    }
}

fn with_alpha(colour: egui::Color32, alpha: f32) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(
        colour.r(),
        colour.g(),
        colour.b(),
        (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// JUCE's `Colour::withSaturation(0.0)`: keep the luminance, drop the hue.
fn desaturate(colour: egui::Color32) -> egui::Color32 {
    // JUCE's HSL luminance is the midpoint of the channel extremes.
    let (r, g, b) = (
        f32::from(colour.r()),
        f32::from(colour.g()),
        f32::from(colour.b()),
    );
    let luminance = ((r.max(g).max(b) + r.min(g).min(b)) / 2.0).round() as u8;
    egui::Color32::from_rgba_unmultiplied(luminance, luminance, luminance, colour.a())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn slider_rect() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(160.0, 18.0))
    }

    #[test]
    fn a_value_the_slider_only_quantised_is_not_reported_as_a_change() {
        // A preset stores its knobs as MIDI, so `51/127 * 10 = 4.0157` reaches the slider, which
        // can only show whole steps. Snapping that for display must NOT look like the user moved
        // it: doing so marked a freshly loaded preset as modified on its very first frame.
        let mut value = 51.0 / 127.0 * 10.0;
        let before = value;
        let mut assets = crate::assets::AssetCache::new();
        let mut changed = None;

        egui::__run_test_ui(|ui| {
            let response = FxSlider::new(&mut value, 0.0, 10.0, 1.0).show(
                ui,
                Rect::from_min_size(pos2(0.0, 0.0), vec2(160.0, 18.0)),
                Palette::default(),
                &mut assets,
                "quantise_guard",
            );
            changed = Some(response.changed());
        });

        assert_eq!(
            changed,
            Some(false),
            "an untouched slider must not report a change"
        );
        assert_eq!(
            value, before,
            "an untouched slider must not rewrite its value"
        );
    }

    #[test]
    fn a_disabled_slider_never_reports_a_change() {
        let mut value = 5.0;
        let mut assets = crate::assets::AssetCache::new();
        let mut changed = None;

        egui::__run_test_ui(|ui| {
            let response = FxSlider::new(&mut value, 0.0, 10.0, 1.0)
                .enabled(false)
                .show(
                    ui,
                    Rect::from_min_size(pos2(0.0, 0.0), vec2(160.0, 18.0)),
                    Palette::default(),
                    &mut assets,
                    "disabled",
                );
            changed = Some(response.changed());
        });

        assert_eq!(changed, Some(false));
        assert_eq!(value, 5.0);
    }

    #[test]
    fn the_track_geometry_matches_the_derivation() {
        // docs/spec/03-controls.md §3.1: (8, 7, 112, 3) for a 160x18 slider.
        let track = track_rect(slider_rect());
        assert_eq!(track.left(), 8.0);
        assert_eq!(track.width(), 112.0);
        assert_eq!(track.top(), 7.0);
        assert_eq!(track.height(), 3.0);
    }

    #[test]
    fn the_thumb_travels_the_full_track() {
        let track = track_rect(slider_rect());
        // pos(v) = 8 + 112 * t
        for (t, expected) in [(0.0, 8.0), (0.5, 64.0), (1.0, 120.0)] {
            let x = track.left() + track.width() * t;
            assert_eq!(x, expected, "t = {t}");
        }
    }

    #[test]
    fn quantise_snaps_to_the_interval() {
        assert_eq!(quantise(3.4, 0.0, 10.0, 1.0), 3.0);
        assert_eq!(quantise(3.6, 0.0, 10.0, 1.0), 4.0);
        assert_eq!(quantise(-5.0, 0.0, 10.0, 1.0), 0.0);
        assert_eq!(quantise(99.0, 0.0, 10.0, 1.0), 10.0);
        // The master gain steps by 2 dB over -20..+20.
        assert_eq!(quantise(3.0, -20.0, 20.0, 2.0), 4.0);
        assert_eq!(quantise(-3.0, -20.0, 20.0, 2.0), -2.0);
        // Filter width steps by 0.5 over 1..3.
        assert_eq!(quantise(1.7, 1.0, 3.0, 0.5), 1.5);
    }

    #[test]
    fn a_step_from_a_position_moves_one_whole_step_either_way() {
        assert_eq!(step_towards(2.0, 0.0, 1.0, true), 3.0);
        assert_eq!(step_towards(2.0, 0.0, 1.0, false), 1.0);
        assert_eq!(step_towards(4.0, -20.0, 2.0, true), 6.0);
        assert_eq!(step_towards(-4.0, -20.0, 2.0, false), -6.0);
        // Float noise on a position still counts as the position.
        assert_eq!(step_towards(0.1 + 0.2, 0.0, 0.1, true), 0.4);
    }

    #[test]
    fn a_step_from_between_two_positions_stops_at_the_next_one_in_that_direction() {
        // 0.4.0 audit #14: Surround at 1.57 went up to 3 and a master gain of 3 dB to 6 dB.
        assert_eq!(step_towards(1.57, 0.0, 1.0, true), 2.0);
        assert_eq!(step_towards(1.57, 0.0, 1.0, false), 1.0);
        assert_eq!(step_towards(3.0, -20.0, 2.0, true), 4.0);
        assert_eq!(step_towards(3.0, -20.0, 2.0, false), 2.0);
        assert_eq!(step_towards(-3.0, -20.0, 2.0, true), -2.0);
        assert_eq!(step_towards(-3.0, -20.0, 2.0, false), -4.0);
        // Filter width steps by 0.5 from 1.
        assert_eq!(step_towards(1.7, 1.0, 0.5, true), 2.0);
        assert_eq!(step_towards(1.7, 1.0, 0.5, false), 1.5);
    }

    #[test]
    fn a_step_past_either_end_is_clamped_back_by_quantise() {
        assert_eq!(
            quantise(step_towards(10.0, 0.0, 1.0, true), 0.0, 10.0, 1.0),
            10.0
        );
        assert_eq!(
            quantise(step_towards(0.0, 0.0, 1.0, false), 0.0, 10.0, 1.0),
            0.0
        );
        assert_eq!(
            quantise(step_towards(19.0, -20.0, 2.0, true), -20.0, 20.0, 2.0),
            20.0
        );
    }

    #[test]
    fn a_step_without_a_step_size_leaves_the_value_alone() {
        assert_eq!(step_towards(3.456, 0.0, 0.0, true), 3.456);
    }

    #[test]
    fn quantise_without_a_step_only_clamps() {
        assert_eq!(quantise(3.456, 0.0, 10.0, 0.0), 3.456);
        assert_eq!(quantise(11.0, 0.0, 10.0, 0.0), 10.0);
    }

    #[test]
    fn the_faithful_fill_overshoots_exactly_as_the_original_does() {
        let rect = slider_rect();
        let track = track_rect(rect);
        // At the minimum the fill is already one thumb radius wide.
        let at_min = track.left() + track.width() * 0.0 - rect.left();
        assert_eq!(at_min, 8.0);
        // At the maximum it reaches x = 128, eight past the track end at 120.
        let at_max = track.left() + track.width() * 1.0 - rect.left();
        assert_eq!(at_max, 120.0);
        assert_eq!(track.left() + at_max, 128.0);
        // ...and still inside the 160 px component, so it never visually clips.
        assert!(track.left() + at_max < rect.right());
    }

    #[test]
    fn the_corrected_fill_stops_at_the_thumb() {
        let track = track_rect(slider_rect());
        assert_eq!(track.width() * 0.0, 0.0);
        assert_eq!(track.width() * 1.0, 112.0);
    }

    #[test]
    fn desaturation_keeps_the_juce_luminance() {
        // JUCE's HSL lightness is (max + min) / 2 of the channels.
        let red = egui::Color32::from_rgb(0xd5, 0x15, 0x35);
        let grey = desaturate(red);
        let expected = ((0xd5 as f32 + 0x15 as f32) / 2.0).round() as u8;
        assert_eq!(
            (grey.r(), grey.g(), grey.b()),
            (expected, expected, expected)
        );
    }

    #[test]
    fn the_balance_bar_fades_its_two_ends_against_each_other() {
        // FxBalanceSlider.cpp:79-82: left at 1 - t, right at t, over -20..+20.
        let palette = Palette::new(ThemeMode::Dark);
        for (t, left, right) in [(0.0, 255, 0), (0.5, 128, 128), (1.0, 0, 255)] {
            let (l, r) = balance_colours(palette, t, true);
            assert_eq!((l.a(), r.a()), (left, right), "t = {t}");
        }
        let track = palette.color(FxColor::SliderTrack);
        let (hard_left, _) = balance_colours(palette, 0.0, true);
        assert_eq!(
            (hard_left.r(), hard_left.g(), hard_left.b()),
            (track.r(), track.g(), track.b())
        );
    }

    #[test]
    fn an_unlit_balance_bar_is_grey_at_the_same_alphas() {
        let palette = Palette::new(ThemeMode::Light);
        let (lit_left, lit_right) = balance_colours(palette, 0.25, true);
        let (left, right) = balance_colours(palette, 0.25, false);
        assert_eq!((left.a(), right.a()), (lit_left.a(), lit_right.a()));
        assert!(left.r() == left.g() && left.g() == left.b(), "{left:?}");
    }

    #[test]
    fn the_faithful_balance_gradient_stops_eight_points_short_of_the_track() {
        // docs/spec/03-controls.md §3.4: the gradient runs from x = 8 to x = 112, not 120.
        let rect = slider_rect();
        let track = track_rect(rect);
        assert_eq!(balance_gradient_end(rect, track, Fidelity::Faithful), 112.0);
        assert_eq!(
            balance_gradient_end(rect, track, Fidelity::Corrected),
            120.0
        );
        assert_eq!(track.right() - 112.0, 8.0);
    }

    #[test]
    fn alpha_helper_matches_the_two_track_layers() {
        let colour = egui::Color32::from_rgb(0xe3, 0x32, 0x50);
        assert_eq!(with_alpha(colour, 0.2).a(), 51);
        assert_eq!(with_alpha(colour, 1.0).a(), 255);
        assert_eq!(with_alpha(colour, 0.1).a(), 26);
    }
}
