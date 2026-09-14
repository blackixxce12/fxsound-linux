//! The master power toggle in the title bar.
//!
//! `FxPowerButton` (`fxsound/Source/GUI/FxPowerButton.cpp`) derives from `DrawableButton` but
//! overrides `paint` rather than `paintButton`, which bypasses the base class's image state
//! machine entirely: **there is no hover and no pressed artwork**. The only thing that changes the
//! glyph is the power state, and the only thing that changes its appearance otherwise is being
//! disabled, which halves the opacity (`FxPowerButton.cpp:30-49`).
//!
//! ## Geometry
//!
//! `paint` builds a *square* `image_area` of `image_width_ × image_width_` — one field used for
//! both axes — centres it in the button with `doNotResize` (so the square is translated, never
//! scaled) and then draws the drawable inside it with `stretchToFit`. The artwork is the 30 × 31
//! `power_on.svg` / `power_off.svg` glyph, so at the shipping size it is **squashed into 24 × 24
//! and its aspect ratio is not preserved** (`docs/spec/03-controls.md §7.2`). [`PowerButton`]
//! reproduces that: [`image_rect`] is passed straight to the rasteriser, which stretches.
//!
//! The owner sets both the button size and `image_width_` to `FxMainWindow::BUTTON_WIDTH` = 24
//! (`FxMainWindow.cpp:192-193`), so the image square and the button coincide in practice.
//! `image_width_` is never initialised in the constructor — it is read from uninitialised memory
//! if `paint` runs first (`FxPowerButton.h:47`, `docs/spec/03-controls.md §14.5`). Here it is
//! simply a constant, which is the bug's only sane port.
//!
//! ## Colours
//!
//! On and off differ only in fill, never in shape: dark `#E63462` / `#FFFFFF`, light `#23B6EB` /
//! `#000000` (`FxTheme.cpp:33`, `:40`). That is entirely encoded in which SVG the theme's image
//! table returns, so this widget names [`FxImage::PowerOnButton`] / [`FxImage::PowerOffButton`]
//! and lets [`AssetCache`] pick the file.

use crate::assets::{AssetCache, FxImage};
use crate::theme::Palette;
use crate::widgets::icon_button;
use egui::{CursorIcon, Id, Rect, Response, Sense, Ui, Vec2};

/// `FxMainWindow::BUTTON_WIDTH`, passed to `setImageWidth` (`FxMainWindow.cpp:193`).
pub const IMAGE_WIDTH: f32 = 24.0;

/// Opacity of the glyph while the button is disabled (`FxPowerButton.cpp:41`, `:47`).
pub const DISABLED_OPACITY: f32 = 0.5;

/// The square the glyph is stretched into: `image_width` on a side, centred in the button.
///
/// `RectanglePlacement(xMid | yMid | doNotResize)` clamps the scale factor to exactly 1.0, so the
/// square keeps its size even when the button is smaller than it.
#[must_use]
pub fn image_rect(bounds: Rect, image_width: f32) -> Rect {
    Rect::from_center_size(bounds.center(), Vec2::splat(image_width))
}

/// The master power toggle.
pub struct PowerButton<'a> {
    on: bool,
    enabled: bool,
    image_width: f32,
    tooltip: Option<&'a str>,
    hide_tooltips: bool,
}

impl<'a> PowerButton<'a> {
    /// A power button showing the on or off glyph.
    #[must_use]
    pub fn new(on: bool) -> Self {
        Self {
            on,
            enabled: true,
            image_width: IMAGE_WIDTH,
            tooltip: None,
            hide_tooltips: false,
        }
    }

    /// Whether the button can be clicked. The controller disables it when the driver is missing or
    /// the session is remote, and forces the power off at the same time
    /// (`docs/spec/03-controls.md §7.3`).
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Override `image_width_`, the side of the square the glyph is stretched into.
    #[must_use]
    pub fn image_width(mut self, width: f32) -> Self {
        self.image_width = width;
        self
    }

    /// The hover tooltip. The original only sets one while the button is disabled:
    /// `TRANS("Audio enhancements are not available over Remote Desktop")`
    /// (`FxMainWindow.cpp:413`), and clears it again when the button is re-enabled (`:417`).
    #[must_use]
    pub fn tooltip(mut self, text: &'a str) -> Self {
        self.tooltip = Some(text);
        self
    }

    /// Suppress the tooltip: pass `state.hide_tooltips`.
    #[must_use]
    pub fn hide_tooltips(mut self, hide: bool) -> Self {
        self.hide_tooltips = hide;
        self
    }

    /// Draw the button into an exact rectangle.
    ///
    /// `response.clicked()` is the toggle: the original flips `FxModel`'s power state and hands the
    /// new value to the controller (`FxMainWindow.cpp:559-567`). egui reports a click for Space on
    /// a focused widget, which covers `FxPowerButton::keyPressed` (`FxPowerButton.cpp:51-59`).
    pub fn show(
        self,
        ui: &mut Ui,
        rect: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> Response {
        let Self {
            on,
            enabled,
            image_width,
            tooltip,
            hide_tooltips,
        } = self;

        let id = Id::new("fx_power_button").with(id_salt);
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let mut response = ui.interact(
            icon_button::hit_rect(rect, icon_button::MIN_HIT_SIZE),
            id,
            sense,
        );

        let image = if on {
            FxImage::PowerOnButton
        } else {
            FxImage::PowerOffButton
        };
        let opacity = if enabled { 1.0 } else { DISABLED_OPACITY };
        icon_button::paint_image(
            ui,
            image_rect(rect, image_width),
            image,
            palette.mode(),
            assets,
            opacity,
        );

        if enabled {
            response = response.on_hover_cursor(CursorIcon::PointingHand);
        }
        if let Some(text) = tooltip
            && !hide_tooltips
        {
            response = response.on_hover_text(
                egui::RichText::new(text)
                    .font(crate::theme::semibold(
                        14.0 / icon_button::JUCE_HEIGHT_PER_EM,
                    ))
                    .color(palette.color(crate::theme::FxColor::DefaultText)),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Chrome;
    use egui::{pos2, vec2};

    #[test]
    fn the_glyph_square_covers_the_shipping_button_exactly() {
        // FxMainWindow.cpp:192-193 — a 24 x 24 button with image_width_ = 24.
        let button = Chrome::PRO.power.rect();
        assert_eq!(button.size(), vec2(24.0, 24.0));
        assert_eq!(image_rect(button, IMAGE_WIDTH), button);
    }

    #[test]
    fn the_glyph_square_is_square_even_in_an_oblong_button() {
        // image_area is (0, 0, image_width_, image_width_): one field, both axes.
        let oblong = Rect::from_min_size(pos2(10.0, 0.0), vec2(26.0, 30.0));
        let art = image_rect(oblong, IMAGE_WIDTH);
        assert_eq!(art.size(), vec2(24.0, 24.0));
        assert_eq!(art.center(), oblong.center());
    }

    #[test]
    fn a_button_smaller_than_the_glyph_does_not_shrink_it() {
        // doNotResize == onlyReduceInSize | onlyIncreaseInSize, i.e. scale == 1.0 always.
        let small = Rect::from_min_size(pos2(0.0, 0.0), vec2(16.0, 16.0));
        let art = image_rect(small, IMAGE_WIDTH);
        assert_eq!(art.size(), vec2(24.0, 24.0));
        assert_eq!(art.center(), small.center());
    }

    #[test]
    fn the_artwork_is_squashed_because_stretch_to_fit_ignores_its_aspect() {
        // power_on.svg is a 30 x 31 viewBox drawn into a 24 x 24 square, so the glyph is 3.2 %
        // shorter than it is wide. docs/spec/03-controls.md §7.2.
        let art = icon_button::art_size(FxImage::PowerOnButton);
        assert_eq!(art, vec2(30.0, 31.0));
        let dest = image_rect(Chrome::PRO.power.rect(), IMAGE_WIDTH);
        assert_ne!(dest.aspect_ratio(), art.x / art.y);
        assert_eq!(dest.width(), dest.height());
    }

    #[test]
    fn both_power_glyphs_are_the_same_size_so_the_states_only_differ_in_colour() {
        assert_eq!(
            icon_button::art_size(FxImage::PowerOnButton),
            icon_button::art_size(FxImage::PowerOffButton)
        );
    }

    #[test]
    fn the_disabled_glyph_is_drawn_at_half_opacity() {
        assert_eq!(DISABLED_OPACITY, 0.5);
    }

    #[test]
    fn the_power_button_sits_where_the_title_bar_puts_it() {
        // docs/spec/01-window-layout.md §4.2: power is the third right-aligned button, so its
        // right edge is 127 px in from the title bar's right edge (which is 21 px in from the
        // window's).
        assert_eq!(
            crate::layout::pro::WINDOW_SIZE.x - 21.0 - Chrome::PRO.power.rect().right(),
            127.0
        );
        assert_eq!(
            crate::layout::lite::WINDOW_SIZE.x - 21.0 - Chrome::LITE.power.rect().right(),
            125.0
        );
    }
}
