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

/// A horizontal slider drawn like FxSound's.
pub struct FxSlider<'a> {
    value: &'a mut f32,
    min: f32,
    max: f32,
    step: f32,
    default: f32,
    enabled: bool,
    reset_on_secondary_click: bool,
    fidelity: Fidelity,
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
            reset_on_secondary_click: false,
            fidelity: Fidelity::default(),
        }
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

    #[must_use]
    pub fn fidelity(mut self, fidelity: Fidelity) -> Self {
        self.fidelity = fidelity;
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
            reset_on_secondary_click,
            fidelity,
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

            // Clicking the track jumps to that position and starts the drag from there, which is
            // JUCE's `snapsToMousePos` default.
            if response.is_pointer_button_down_on()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let t = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
                new_value = min + t * span;
                interacted = true;
            }

            if reset_on_secondary_click && response.secondary_clicked() {
                new_value = default;
                interacted = true;
            }

            if response.hovered() {
                // 0.36 exposes only the smoothed delta; one notch is still one step, which is
                // what JUCE's default wheel handling does.
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    new_value += scroll.signum() * step;
                    interacted = true;
                }
            }

            if response.has_focus() {
                let stepped = ui.input(|i| {
                    let mut delta = 0.0;
                    if i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::ArrowRight) {
                        delta += step;
                    }
                    if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::ArrowLeft) {
                        delta -= step;
                    }
                    delta
                });
                if stepped != 0.0 {
                    new_value += stepped;
                    interacted = true;
                }
            }

            if interacted {
                let new_value = quantise(new_value, min, max, step);
                if new_value != *value {
                    *value = new_value;
                    response.mark_changed();
                }
            }
        }

        paint(
            ui, rect, track, *value, min, max, palette, assets, enabled, fidelity, &response,
        );
        response
    }
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

#[allow(clippy::too_many_arguments)]
fn paint(
    ui: &Ui,
    rect: Rect,
    track: Rect,
    value: f32,
    min: f32,
    max: f32,
    palette: Palette,
    assets: &mut AssetCache,
    enabled: bool,
    fidelity: Fidelity,
    response: &Response,
) {
    let painter = ui.painter();
    let span = (max - min).max(f32::EPSILON);
    let t = ((value - min) / span).clamp(0.0, 1.0);
    // The original's `sliderPos` is an absolute component coordinate, not an offset.
    let thumb_x = track.left() + track.width() * t;

    let corner = CornerRadius::same(TRACK_CORNER_RADIUS as u8);
    let track_colour = palette.color(FxColor::SliderTrack);
    let track_colour = if enabled {
        track_colour
    } else {
        desaturate(track_colour)
    };

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

    // 3. The thumb.
    let thumb_image = if enabled {
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
    fn alpha_helper_matches_the_two_track_layers() {
        let colour = egui::Color32::from_rgb(0xe3, 0x32, 0x50);
        assert_eq!(with_alpha(colour, 0.2).a(), 51);
        assert_eq!(with_alpha(colour, 1.0).a(), 255);
        assert_eq!(with_alpha(colour, 0.1).a(), 26);
    }
}
