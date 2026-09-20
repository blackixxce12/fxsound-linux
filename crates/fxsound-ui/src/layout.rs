//! Pixel geometry, transcribed from the JUCE layout code.
//!
//! Every number here comes from a `static constexpr` or a `setBounds` call in the original, cited
//! in the comment next to it. egui works in *points*; the original works in *pixels at 100% DPI*.
//! Those are the same thing, so the constants are used unchanged and the compositor's fractional
//! scale (1.6 on the development machine) is applied by egui on top, exactly as Windows' own DPI
//! scaling did.
//!
//! Coordinates are window-local unless a name says `CONTENT_`, in which case they are relative to
//! the content area below the title bar.

use egui::{Pos2, Rect, Vec2, pos2, vec2};

/// Radius of the window's rounded corners (`FxWindow.cpp:131-139`).
pub const WINDOW_CORNER_RADIUS: f32 = 21.0;
/// Radius of the inner content panel (`FxProView.cpp:114`).
pub const PANEL_CORNER_RADIUS: f32 = 8.0;
/// Drop-shadow width the original paints around the frameless window (`FxWindow.h:44`).
pub const SHADOW_WIDTH: f32 = 5.0;
/// Height of the custom title bar (`FxWindow.cpp` title-bar bounds).
pub const TITLE_BAR_HEIGHT: f32 = 56.0;
/// The 1 px separator under the title bar.
pub const TITLE_BAR_DIVIDER_HEIGHT: f32 = 1.0;
/// Horizontal inset of the title bar from the window edge.
pub const TITLE_BAR_INSET: f32 = 21.0;

/// A title-bar button: where it sits and how big its artwork is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeButton {
    pub pos: Pos2,
    pub size: Vec2,
}

impl ChromeButton {
    const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            pos: pos2(x, y),
            size: vec2(w, h),
        }
    }

    #[must_use]
    pub fn rect(self) -> Rect {
        Rect::from_min_size(self.pos, self.size)
    }

    /// A square hit area of at least `min` points, centred on the artwork — the original's
    /// buttons are easier to hit than their icons are large.
    #[must_use]
    pub fn hit_rect(self, min: f32) -> Rect {
        let r = self.rect();
        let grow = (
            (min - r.width()).max(0.0) / 2.0,
            (min - r.height()).max(0.0) / 2.0,
        );
        r.expand2(vec2(grow.0, grow.1))
    }
}

/// The chrome shared by both views, parameterised by window width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chrome {
    /// FxSound wordmark.
    pub logo: ChromeButton,
    /// Hamburger menu.
    pub menu: ChromeButton,
    /// Power toggle.
    pub power: ChromeButton,
    /// Switch between Pro and Lite.
    pub flip: ChromeButton,
    /// Minimise to tray.
    pub minimize: ChromeButton,
    /// Close.
    pub close: ChromeButton,
}

impl Chrome {
    /// Pro-view chrome (`FxMainWindow.cpp` title-bar layout, window width 1040).
    pub const PRO: Self = Self {
        logo: ChromeButton::new(21.0, 21.0, 106.0, 15.0),
        menu: ChromeButton::new(142.0, 16.0, 24.0, 24.0),
        power: ChromeButton::new(868.0, 16.0, 24.0, 24.0),
        flip: ChromeButton::new(912.0, 15.0, 26.0, 26.0),
        minimize: ChromeButton::new(958.0, 13.0, 26.0, 30.0),
        close: ChromeButton::new(1004.0, 20.0, 15.0, 15.0),
    };

    /// Lite-view chrome (window width 550).
    pub const LITE: Self = Self {
        logo: ChromeButton::new(21.0, 21.0, 106.0, 15.0),
        menu: ChromeButton::new(142.0, 16.0, 24.0, 24.0),
        power: ChromeButton::new(380.0, 16.0, 24.0, 24.0),
        flip: ChromeButton::new(424.0, 16.0, 24.0, 24.0),
        minimize: ChromeButton::new(468.0, 13.0, 26.0, 30.0),
        close: ChromeButton::new(514.0, 20.0, 15.0, 15.0),
    };

    /// Every button in paint order, so a hit test can walk them.
    #[must_use]
    pub fn buttons(&self) -> [ChromeButton; 5] {
        [self.menu, self.power, self.flip, self.minimize, self.close]
    }
}

/// The Pro view — the full window.
pub mod pro {
    use super::{Rect, Vec2, pos2, vec2};

    /// `FxProView::WIDTH` (`FxProView.h:44`).
    pub const CONTENT_WIDTH: f32 = 1040.0;
    /// `FxProView::HEIGHT + 20`, the size once the visualizer is shown (`FxProView.cpp:70`).
    pub const CONTENT_HEIGHT: f32 = 511.0;
    /// Outer window size: content plus title bar, divider and the 20 px bottom pad.
    pub const WINDOW_SIZE: Vec2 = vec2(1040.0, 588.0);

    /// Rounded panel behind everything (`FxProView.cpp:114`: 20,16 1000×(347+140)).
    #[must_use]
    pub fn panel() -> Rect {
        Rect::from_min_size(pos2(20.0, 73.0), vec2(1000.0, 487.0))
    }

    /// Preset picker (`FxProView.h:46,48,51,52`).
    #[must_use]
    pub fn preset_combo() -> Rect {
        Rect::from_min_size(pos2(40.0, 89.0), vec2(470.0, 40.0))
    }

    /// Output-device picker (`FxProView.h:47`).
    #[must_use]
    pub fn output_combo() -> Rect {
        Rect::from_min_size(pos2(530.0, 89.0), vec2(470.0, 40.0))
    }

    /// Spectrum visualizer (`FxVisualizer.h:51-52`).
    #[must_use]
    pub fn visualizer() -> Rect {
        Rect::from_min_size(pos2(40.0, 149.0), vec2(960.0, 120.0))
    }

    /// The column of effect sliders (`FxProView.h:49-50`).
    #[must_use]
    pub fn audio_controls() -> Rect {
        Rect::from_min_size(pos2(40.0, 285.0), vec2(168.0, 257.0))
    }

    /// The graphic equalizer (`FxEqualizer.h:96-97`), 16 px right of the sliders.
    #[must_use]
    pub fn equalizer() -> Rect {
        Rect::from_min_size(pos2(224.0, 285.0), vec2(776.0, 257.0))
    }

    /// Where the error toast appears when a device fails.
    #[must_use]
    pub fn notification() -> Rect {
        Rect::from_min_size(pos2(440.0, 134.0), vec2(560.0, 120.0))
    }
}

/// The Lite view — the compact window.
pub mod lite {
    use super::{Rect, Vec2, pos2, vec2};

    /// `FxLiteView::LIST_WIDTH` is `FxView::LIST_WIDTH` (`FxView.h:39`).
    pub const LIST_SIZE: Vec2 = vec2(225.0, 50.0);
    /// `FxLiteView::WIDTH` = `LIST_WIDTH*2 + 20*3 + 40` (`FxLiteView.h:40-43`).
    pub const CONTENT_WIDTH: f32 = 550.0;
    /// `FxLiteView::HEIGHT` = `LIST_HEIGHT + 40 + 22`.
    pub const CONTENT_HEIGHT: f32 = 112.0;
    /// Outer window size.
    pub const WINDOW_SIZE: Vec2 = vec2(550.0, 189.0);

    /// Rounded panel (`BACKGROUND_WIDTH` × `BACKGROUND_HEIGHT`, centred).
    #[must_use]
    pub fn panel() -> Rect {
        Rect::from_min_size(pos2(20.0, 73.0), vec2(510.0, 90.0))
    }

    /// Preset picker (`FxLiteView.h:37,39`).
    #[must_use]
    pub fn preset_combo() -> Rect {
        Rect::from_min_size(pos2(40.0, 99.0), LIST_SIZE)
    }

    /// Output-device picker (`FxLiteView.h:38`).
    #[must_use]
    pub fn output_combo() -> Rect {
        Rect::from_min_size(pos2(285.0, 99.0), LIST_SIZE)
    }
}

/// Geometry inside the effect-slider column (`FxAudioControls.h:64-68, 93-97`).
pub mod audio_controls {
    pub const LABEL_HEIGHT: f32 = 14.0;
    pub const SLIDER_WIDTH: f32 = 160.0;
    pub const SLIDER_HEIGHT: f32 = 18.0;
    pub const X_MARGIN: f32 = 8.0;
    pub const Y_MARGIN: f32 = 21.0;
    /// Vertical gap between one slider's label and the next.
    pub const ROW_GAP: f32 = 8.0;
    /// Width of the value readout to the right of a slider.
    pub const LABEL_WIDTH: f32 = 52.0;
    pub const CONTROL_GAP: f32 = 4.0;
}

/// Geometry inside the equalizer (`FxEqualizer.h:96-105`).
pub mod equalizer {
    pub const WIDTH: f32 = 776.0;
    pub const HEIGHT: f32 = 257.0;
    /// Height of one band's vertical fader.
    pub const SLIDER_HEIGHT: f32 = 180.0;
    pub const LABEL_HEIGHT: f32 = 12.0;
    pub const SMALL_FONT: f32 = 10.0;
    /// The filter-width knob.
    pub const ROTARY_SLIDER_HEIGHT: f32 = 36.0;
    pub const X_MARGIN: f32 = 16.0;
    pub const Y_MARGIN: f32 = 8.0;
    /// Boost/cut range of one band, in dB (`FxEqualizer.h:105`).
    pub const MAX_GAIN_DB: f32 = 12.0;
}

/// Geometry of the spectrum visualizer (`FxVisualizer.h:51-53`).
pub mod visualizer {
    pub const WIDTH: f32 = 960.0;
    pub const HEIGHT: f32 = 120.0;
    /// `FxVisualizer::NUM_BARS`.
    pub const NUM_BARS: usize = 10;
}

/// Settings dialog geometry (`FxSettingsDialog.h`).
pub mod settings_dialog {
    use super::{Vec2, vec2};

    /// `SettingsComponent` content size.
    pub const CONTENT_SIZE: Vec2 = vec2(600.0, 510.0);
    /// Outer window size once the title bar and padding are added.
    pub const WINDOW_SIZE: Vec2 = vec2(610.0, 597.0);
    /// x of the vertical rule between the side nav and the pane.
    pub const SEPARATOR_X: f32 = 152.0;
    pub const NAV_BUTTON_SIZE: Vec2 = vec2(150.0, 40.0);
    pub const NAV_BUTTON_ORIGIN: Vec2 = vec2(20.0, 50.0);
    pub const X_MARGIN: f32 = 20.0;
    pub const Y_MARGIN: f32 = 5.0;
    pub const TITLE_HEIGHT: f32 = 24.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pro_window_height_is_the_sum_of_its_parts() {
        // title bar + divider + content + bottom pad
        let sum = TITLE_BAR_HEIGHT + TITLE_BAR_DIVIDER_HEIGHT + pro::CONTENT_HEIGHT + 20.0;
        assert_eq!(sum, pro::WINDOW_SIZE.y);
    }

    #[test]
    fn lite_window_height_is_the_sum_of_its_parts() {
        let sum = TITLE_BAR_HEIGHT + TITLE_BAR_DIVIDER_HEIGHT + lite::CONTENT_HEIGHT + 20.0;
        assert_eq!(sum, lite::WINDOW_SIZE.y);
    }

    #[test]
    fn lite_width_matches_the_constexpr_formula() {
        // FxLiteView.h:40-43 — BACKGROUND_WIDTH = LIST_WIDTH*2 + 20*3; WIDTH = BACKGROUND_WIDTH + 40
        let background_width = lite::LIST_SIZE.x * 2.0 + 20.0 * 3.0;
        assert_eq!(background_width + 40.0, lite::CONTENT_WIDTH);
    }

    #[test]
    fn the_two_combos_sit_side_by_side_inside_the_panel() {
        let panel = pro::panel();
        assert!(panel.contains_rect(pro::preset_combo()));
        assert!(panel.contains_rect(pro::output_combo()));
        // 20 px gutter between them
        assert_eq!(
            pro::output_combo().left() - pro::preset_combo().right(),
            20.0
        );
    }

    #[test]
    fn the_equalizer_sits_16px_right_of_the_sliders() {
        assert_eq!(
            pro::equalizer().left() - pro::audio_controls().right(),
            16.0
        );
        assert_eq!(
            pro::equalizer().size(),
            vec2(equalizer::WIDTH, equalizer::HEIGHT)
        );
    }

    #[test]
    fn every_pro_child_fits_inside_the_window() {
        let window =
            Rect::from_min_size(pos2(0.0, 0.0), vec2(pro::WINDOW_SIZE.x, pro::WINDOW_SIZE.y));
        for r in [
            pro::panel(),
            pro::preset_combo(),
            pro::output_combo(),
            pro::visualizer(),
            pro::audio_controls(),
            pro::equalizer(),
        ] {
            assert!(window.contains_rect(r), "{r:?} escapes the window");
        }
    }

    #[test]
    fn chrome_buttons_stay_inside_their_title_bars() {
        for (chrome, width) in [
            (Chrome::PRO, pro::WINDOW_SIZE.x),
            (Chrome::LITE, lite::WINDOW_SIZE.x),
        ] {
            for b in chrome.buttons() {
                assert!(b.rect().right() <= width, "{b:?} overflows width {width}");
                assert!(
                    b.rect().bottom() <= TITLE_BAR_HEIGHT,
                    "{b:?} overflows the bar"
                );
            }
        }
    }
}
