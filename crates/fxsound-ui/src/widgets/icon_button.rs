//! The shared title-bar icon button.
//!
//! Every button in the FxSound title bar except the power toggle and the ✕ is a JUCE
//! `DrawableButton` in `ImageFitted` style with exactly two drawables — a normal one and a hover
//! one — swapped by the button's own state machine (`FxMainWindow.cpp:331-364`). The theme decides
//! *which* SVG each of those names resolves to, so this widget only has to know the two
//! [`FxImage`] tokens and let [`AssetCache`] pick the file for the active palette.
//!
//! ## Why the artwork is inset before it is fitted
//!
//! `DrawableButton::getImageBounds()` reduces the button by `jmin(edgeIndent, 30 % of the side)`
//! on each axis before fitting the drawable into what is left, and `edgeIndent` defaults to 3.
//! JUCE is not vendored in this tree, so [`EDGE_INDENT`] is the one number here that is derived
//! from the framework rather than read out of FxSound's own sources — hence
//! [`IconButton::edge_indent`], so a pixel diff against the Windows build can settle it without
//! touching this file. `docs/spec/01-window-layout.md §4.2` describes the same step in its shorter
//! form: "scales the SVG to fit the button rect preserving aspect, centred".
//!
//! ## Hit area
//!
//! `docs/spec/01-window-layout.md §4.2` records that JUCE hit-tests the whole component and
//! nothing more — "Note the ✕ glyph is only 15 px, which is a small target; the port may
//! legitimately widen the *hit* rect while keeping the *drawn* glyph at 15 px". This widget takes
//! that invitation: the artwork is drawn in the rect it was given, while the interaction rect is
//! grown to [`IconButton::min_hit_size`] (24 points, `FxMainWindow::BUTTON_WIDTH`) so the close
//! button is as easy to hit as its neighbours.

use crate::assets::{AssetCache, FxImage, NUM_IMAGES};
use crate::theme::{self, FxColor, Palette};
use egui::{Color32, CursorIcon, Id, Rect, Response, Sense, Ui, Vec2, pos2, vec2};
use fxsound_core::ThemeMode;

/// `DrawableButton::edgeIndent`, the inset applied before the artwork is fitted.
pub const EDGE_INDENT: f32 = 3.0;

/// The fraction of the button `DrawableButton::getImageBounds` will never indent past.
const MAX_INDENT_FRACTION: f32 = 0.3;

/// Default minimum interaction size, `FxMainWindow::BUTTON_WIDTH` (`FxMainWindow.h:54`).
pub const MIN_HIT_SIZE: f32 = 24.0;

/// Tooltip text height, `getNormalFont().withHeight(14.0f)` (`FxTheme.cpp:684`), converted from
/// JUCE's ascent-plus-descent to egui's em size.
const TOOLTIP_FONT_PX: f32 = 14.0;

/// `juce::Font::withHeight(h)` sets ascent + descent to `h` pixels; `egui::FontId::size` is the em
/// size, and for Gilroy ascent + descent ≈ 1.2 em (`docs/spec/01-window-layout.md §2.3`).
pub const JUCE_HEIGHT_PER_EM: f32 = 1.2;

/// The whole texture, for [`egui::Painter::image`].
const UV_FULL: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

/// Natural size of every image, in `FxImage` order, read from each file's `viewBox`.
///
/// [`AssetCache::texture`] rasterises into whatever size it is handed *without* preserving the
/// aspect ratio, so anything that wants JUCE's `RectanglePlacement::centred` behaviour has to know
/// how big the artwork really is. Both theme variants of a given `FxImage` share a size — the test
/// below proves it against the embedded bytes — so this table is indexed by the image alone.
pub const ART_SIZES: [Vec2; NUM_IMAGES] = [
    vec2(526.19, 75.15),  // DefaultLogo — logo-white.svg / logo-black.svg
    vec2(526.19, 75.15),  // HighlightedLogo — logo-red.svg / logo-blue.svg
    vec2(299.83, 219.26), // IconLogo — FxSound {White,Black} Bars.svg
    vec2(30.0, 31.0),     // PowerOnButton — power_on.svg / power_on_blue.svg
    vec2(30.0, 31.0),     // PowerOffButton — power_off.svg / power_off_black.svg
    vec2(30.0, 31.0),     // DonateButton
    vec2(30.0, 31.0),     // DonateButtonHover
    vec2(14.0, 10.0),     // MenuButton
    vec2(14.0, 10.0),     // MenuButtonHover
    vec2(18.0, 18.0),     // MinimizeButton — minimize.svg (the Pro-view flip glyph)
    vec2(18.0, 18.0),     // MinimizeButtonHover
    vec2(16.0, 16.0),     // MaximizeButton — maximize.svg (the Lite-view flip glyph)
    vec2(16.0, 16.0),     // MaximizeButtonHover
    vec2(14.0, 10.0),     // MinimizeWindowButton — min_window.svg
    vec2(14.0, 10.0),     // MinimizeWindowButtonHover
    vec2(16.0, 16.0),     // FlipButton
    vec2(16.0, 16.0),     // FlipButtonHover
    vec2(16.0, 16.0),     // RestoreDefaultsButton
    vec2(16.0, 16.0),     // RestoreDefaultsButtonHover
    vec2(16.0, 16.0),     // RemoveButton
    vec2(7.0, 11.0),      // ArrowNext
    vec2(7.0, 11.0),      // ArrowNextBW
    vec2(7.0, 11.0),      // ArrowPrev
    vec2(7.0, 11.0),      // ArrowPrevBW
    vec2(6.0, 5.0),       // ArrowUpSelected
    vec2(6.0, 5.0),       // ArrowUp
    vec2(6.0, 5.0),       // ArrowDownSelected
    vec2(6.0, 5.0),       // ArrowDown
    vec2(11.0, 7.0),      // DropDownArrow — dropdown_arrow_bw.svg
    vec2(11.0, 7.0),      // DropDownArrowHover
    vec2(64.0, 64.0),     // SliderThumb — a 16 × 16 glyph in a 64 × 64 viewBox
    vec2(16.0, 16.0),     // SliderThumbBW
];

/// The natural size of one image's artwork.
#[must_use]
pub fn art_size(image: FxImage) -> Vec2 {
    ART_SIZES[image as usize]
}

/// `DrawableButton::getImageBounds()`: the button reduced by the edge indent on both axes.
#[must_use]
pub fn image_bounds(bounds: Rect, edge_indent: f32) -> Rect {
    let x = edge_indent.min(bounds.width() * MAX_INDENT_FRACTION);
    let y = edge_indent.min(bounds.height() * MAX_INDENT_FRACTION);
    bounds.shrink2(vec2(x, y))
}

/// `RectanglePlacement::centred`: scale `art` to fit `dest`, preserving its aspect ratio, and
/// centre it. Unlike `doNotResize` this scales up as well as down.
#[must_use]
pub fn fitted_rect(dest: Rect, art: Vec2) -> Rect {
    if art.x <= 0.0 || art.y <= 0.0 {
        return dest;
    }
    let scale = (dest.width() / art.x).min(dest.height() / art.y);
    Rect::from_center_size(dest.center(), art * scale)
}

/// Grow `rect` to a minimum size about its own centre, leaving anything already big enough alone.
#[must_use]
pub fn hit_rect(rect: Rect, min_size: f32) -> Rect {
    rect.expand2(vec2(
        (min_size - rect.width()).max(0.0) / 2.0,
        (min_size - rect.height()).max(0.0) / 2.0,
    ))
}

/// Paint one themed SVG so that it exactly fills `rect`, at `opacity`.
///
/// The caller decides `rect`: pass [`fitted_rect`] for JUCE's aspect-preserving `centred`
/// placement, or the raw destination for its `stretchToFit`.
pub fn paint_image(
    ui: &Ui,
    rect: Rect,
    image: FxImage,
    theme: ThemeMode,
    assets: &mut AssetCache,
    opacity: f32,
) {
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }
    let tint = Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0).round() as u8);
    if let Some(texture) = assets.texture(ui.ctx(), image, theme, rect.size()) {
        ui.painter().image(texture.id(), rect, UV_FULL, tint);
    }
}

/// A title-bar button: one SVG, a hover swap, an enlarged hit area and an optional tooltip.
pub struct IconButton<'a> {
    normal: FxImage,
    hover: Option<FxImage>,
    disabled: Option<FxImage>,
    enabled: bool,
    opacity: f32,
    edge_indent: f32,
    min_hit_size: f32,
    tooltip: Option<&'a str>,
    hide_tooltips: bool,
}

impl<'a> IconButton<'a> {
    /// A button showing `normal` in every state.
    #[must_use]
    pub fn new(normal: FxImage) -> Self {
        Self {
            normal,
            hover: None,
            disabled: None,
            enabled: true,
            opacity: 1.0,
            edge_indent: EDGE_INDENT,
            min_hit_size: MIN_HIT_SIZE,
            tooltip: None,
            hide_tooltips: false,
        }
    }

    /// The artwork shown while the pointer is over the button — the second argument of
    /// `DrawableButton::setImages` (`FxMainWindow.cpp:331`).
    #[must_use]
    pub fn hover(mut self, image: FxImage) -> Self {
        self.hover = Some(image);
        self
    }

    /// The artwork shown while the button is disabled.
    ///
    /// `DrawableButton::getDisabledImage()` falls back to the normal artwork when none was set,
    /// which is what leaving this alone reproduces. The greyed "BW" variants — `ArrowNextBW`,
    /// `DropDownArrow`, `SliderThumbBW` — are what the original passes here.
    #[must_use]
    pub fn disabled_image(mut self, image: FxImage) -> Self {
        self.disabled = Some(image);
        self
    }

    /// Whether the button reacts to the pointer at all.
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Overall opacity, the last argument of `Drawable::drawWithin`.
    #[must_use]
    pub fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    /// Override `DrawableButton::edgeIndent`; `0.0` fits the artwork to the whole button.
    #[must_use]
    pub fn edge_indent(mut self, indent: f32) -> Self {
        self.edge_indent = indent;
        self
    }

    /// The smallest interaction square, centred on the artwork. `0.0` hit-tests the drawn rect
    /// only, which is what JUCE does.
    #[must_use]
    pub fn min_hit_size(mut self, size: f32) -> Self {
        self.min_hit_size = size;
        self
    }

    /// A hover tooltip in the app's own tooltip style (`FxTheme::drawTooltip`).
    ///
    /// None of the title bar's buttons sets one — the original gives them `setHelpText`, which is
    /// accessibility text rather than a bubble (`FxMainWindow.cpp:194-221`) — so this is for an
    /// image button that does want hover text, the way `FxPowerButton` carries its Remote Desktop
    /// notice while disabled (`FxMainWindow.cpp:413`).
    #[must_use]
    pub fn tooltip(mut self, text: &'a str) -> Self {
        self.tooltip = Some(text);
        self
    }

    /// Suppress the tooltip: pass `state.hide_tooltips`, the `hide_help_tooltips` setting
    /// (`FxController.cpp:2302-2312`).
    #[must_use]
    pub fn hide_tooltips(mut self, hide: bool) -> Self {
        self.hide_tooltips = hide;
        self
    }

    /// Draw the button into an exact rectangle and report what the user did.
    pub fn show(
        self,
        ui: &mut Ui,
        rect: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> Response {
        let Self {
            normal,
            hover,
            disabled,
            enabled,
            opacity,
            edge_indent,
            min_hit_size,
            tooltip,
            hide_tooltips,
        } = self;

        let id = Id::new("fx_icon_button").with(id_salt);
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let mut response = ui.interact(hit_rect(rect, min_hit_size), id, sense);

        let image = if !enabled {
            disabled.unwrap_or(normal)
        } else if response.hovered() {
            hover.unwrap_or(normal)
        } else {
            normal
        };

        let art = fitted_rect(image_bounds(rect, edge_indent), art_size(image));
        paint_image(ui, art, image, palette.mode(), assets, opacity);

        if enabled {
            response = response.on_hover_cursor(CursorIcon::PointingHand);
        }
        if let Some(text) = tooltip
            && !hide_tooltips
        {
            response = response.on_hover_text(
                egui::RichText::new(text)
                    .font(theme::semibold(TOOLTIP_FONT_PX / JUCE_HEIGHT_PER_EM))
                    .color(palette.color(FxColor::DefaultText)),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::svg_bytes;
    use crate::layout::Chrome;

    const ALL_IMAGES: [FxImage; NUM_IMAGES] = [
        FxImage::DefaultLogo,
        FxImage::HighlightedLogo,
        FxImage::IconLogo,
        FxImage::PowerOnButton,
        FxImage::PowerOffButton,
        FxImage::DonateButton,
        FxImage::DonateButtonHover,
        FxImage::MenuButton,
        FxImage::MenuButtonHover,
        FxImage::MinimizeButton,
        FxImage::MinimizeButtonHover,
        FxImage::MaximizeButton,
        FxImage::MaximizeButtonHover,
        FxImage::MinimizeWindowButton,
        FxImage::MinimizeWindowButtonHover,
        FxImage::FlipButton,
        FxImage::FlipButtonHover,
        FxImage::RestoreDefaultsButton,
        FxImage::RestoreDefaultsButtonHover,
        FxImage::RemoveButton,
        FxImage::ArrowNext,
        FxImage::ArrowNextBW,
        FxImage::ArrowPrev,
        FxImage::ArrowPrevBW,
        FxImage::ArrowUpSelected,
        FxImage::ArrowUp,
        FxImage::ArrowDownSelected,
        FxImage::ArrowDown,
        FxImage::DropDownArrow,
        FxImage::DropDownArrowHover,
        FxImage::SliderThumb,
        FxImage::SliderThumbBW,
    ];

    #[test]
    fn the_art_size_table_matches_every_embedded_svg_in_both_themes() {
        let options = usvg::Options::default();
        for image in ALL_IMAGES {
            let expected = art_size(image);
            for theme in [ThemeMode::Dark, ThemeMode::Light] {
                let tree = usvg::Tree::from_data(svg_bytes(image, theme), &options)
                    .unwrap_or_else(|e| panic!("{image:?} {theme:?} failed to parse: {e}"));
                let size = tree.size();
                assert!(
                    (size.width() - expected.x).abs() < 0.01
                        && (size.height() - expected.y).abs() < 0.01,
                    "{image:?} {theme:?} is {} x {}, the table says {expected:?}",
                    size.width(),
                    size.height()
                );
            }
        }
    }

    #[test]
    fn the_image_box_is_the_button_reduced_by_the_edge_indent() {
        // The 24 x 24 menu button (FxMainWindow.cpp:199).
        let button = Rect::from_min_size(pos2(142.0, 16.0), vec2(24.0, 24.0));
        let inner = image_bounds(button, EDGE_INDENT);
        assert_eq!(inner.min, pos2(145.0, 19.0));
        assert_eq!(inner.size(), vec2(18.0, 18.0));

        // The 26 x 30 minimise button (FxMainWindow.cpp:220, at (958, 13) in the Pro bar)
        // indents by 3 on both axes too, because 30 % of 26 is 7.8 and 30 % of 30 is 9.
        let minimise = Rect::from_min_size(pos2(958.0, 13.0), vec2(26.0, 30.0));
        assert_eq!(image_bounds(minimise, EDGE_INDENT).size(), vec2(20.0, 24.0));
    }

    #[test]
    fn a_tiny_button_is_indented_by_thirty_percent_not_by_three() {
        // jmin(edgeIndent, proportionOfWidth(0.3f)): 30 % of 8 is 2.4, so 8 - 2 * 2.4 is left.
        let tiny = Rect::from_min_size(pos2(0.0, 0.0), vec2(8.0, 8.0));
        let size = image_bounds(tiny, EDGE_INDENT).size();
        assert!(
            (size.x - 3.2).abs() < 1e-4 && (size.y - 3.2).abs() < 1e-4,
            "an 8 x 8 button left {size:?} for its artwork, not 3.2 x 3.2"
        );
    }

    #[test]
    fn the_edge_indent_can_be_switched_off() {
        let button = Rect::from_min_size(pos2(0.0, 0.0), vec2(24.0, 24.0));
        assert_eq!(image_bounds(button, 0.0), button);
    }

    #[test]
    fn fitting_preserves_the_aspect_ratio_and_centres_the_result() {
        // The 14 x 10 menu glyph inside the 24 x 24 button's 18 x 18 image box: the width binds,
        // so the glyph is scaled by 18/14 and letterboxed vertically.
        let dest = Rect::from_min_size(pos2(145.0, 19.0), vec2(18.0, 18.0));
        let art = fitted_rect(dest, art_size(FxImage::MenuButton));
        assert!((art.width() - 18.0).abs() < 1e-4);
        assert!((art.height() - 18.0 * 10.0 / 14.0).abs() < 1e-4);
        assert_eq!(art.center(), dest.center());
        assert!(art.height() < dest.height());
    }

    #[test]
    fn fitting_a_square_glyph_fills_a_square_box() {
        let dest = Rect::from_min_size(pos2(0.0, 0.0), vec2(20.0, 20.0));
        assert_eq!(fitted_rect(dest, art_size(FxImage::MaximizeButton)), dest);
    }

    #[test]
    fn fitting_scales_artwork_up_as_well_as_down() {
        // RectanglePlacement::centred has neither onlyReduceInSize nor onlyIncreaseInSize.
        let dest = Rect::from_min_size(pos2(0.0, 0.0), vec2(60.0, 60.0));
        let art = fitted_rect(dest, art_size(FxImage::DropDownArrow));
        assert!((art.width() - 60.0).abs() < 1e-4, "{art:?}");
        assert!(art.width() > art_size(FxImage::DropDownArrow).x);
    }

    #[test]
    fn degenerate_artwork_falls_back_to_the_destination_rect() {
        let dest = Rect::from_min_size(pos2(1.0, 2.0), vec2(10.0, 10.0));
        assert_eq!(fitted_rect(dest, vec2(0.0, 5.0)), dest);
        assert_eq!(fitted_rect(dest, vec2(5.0, 0.0)), dest);
    }

    #[test]
    fn the_close_button_gets_a_bigger_hit_area_than_its_glyph() {
        // FxWindow.cpp:188 sizes the glyph 15 x 15; docs/spec/01-window-layout.md §4.2 allows the
        // port to widen the hit rect.
        let close = Chrome::PRO.close.rect();
        assert_eq!(close.size(), vec2(15.0, 15.0));
        let hit = hit_rect(close, MIN_HIT_SIZE);
        assert_eq!(hit.size(), vec2(24.0, 24.0));
        assert_eq!(hit.center(), close.center());
    }

    #[test]
    fn a_button_that_is_already_big_enough_keeps_its_own_rect() {
        for button in [Chrome::PRO.menu, Chrome::PRO.power] {
            let rect = button.rect();
            assert_eq!(hit_rect(rect, MIN_HIT_SIZE), rect, "{button:?}");
        }
    }

    #[test]
    fn hit_areas_of_adjacent_chrome_buttons_never_overlap() {
        // The title bar leaves a 20 px gap between buttons (FxWindow.cpp:286-308), so growing the
        // 15 px close button to 24 must still not reach the minimize button.
        for chrome in [Chrome::PRO, Chrome::LITE] {
            let mut rects: Vec<Rect> = chrome
                .buttons()
                .iter()
                .map(|b| hit_rect(b.rect(), MIN_HIT_SIZE))
                .collect();
            rects.sort_by(|a, b| a.left().total_cmp(&b.left()));
            for pair in rects.windows(2) {
                assert!(
                    pair[0].right() <= pair[1].left(),
                    "{:?} overlaps {:?}",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    #[test]
    fn the_tooltip_font_is_gilroy_semibold_at_fourteen_juce_pixels() {
        let font = theme::semibold(TOOLTIP_FONT_PX / JUCE_HEIGHT_PER_EM);
        assert!((font.size - 11.666_667).abs() < 1e-4, "{font:?}");
        assert_eq!(font.family, theme::semibold(1.0).family);
    }
}
