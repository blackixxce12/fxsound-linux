//! The effect column's second face: `FxEqualizerControl` (`FxAudioControls.cpp:268-551`).
//!
//! The column in the Pro window is a two-sided card (`docs/spec/03-controls.md` §1). Face A is the
//! five effect sliders; the flip button in the card's top-right corner turns it over to this face,
//! which holds what the equalizer is run *with* rather than the curve itself: the band count, the
//! master gain, the volume leveling, the filter width, the balance and Restore Defaults. Upstream
//! moved these out of the Settings dialog onto the main window in 1.2.12 (f58a423); before this
//! module the port could reach them only from the command line.
//!
//! ```text
//! column   (0,   0, 168, 257)   ControlBackground, radius 8 — FxAudioControls::paint
//! flip     (145, 5,  18,  18)   both faces
//! bands    (8,  28, 152,  20)   "10 Bands"
//! row 0    caption (16,  56, 160, 14)   slider (8,  71, 160, 18)   Master Gain
//! row 1    caption (16,  97, 160, 14)   slider (8, 112, 160, 18)   Volume Leveling
//! row 2    caption (16, 138, 160, 14)   slider (8, 153, 160, 18)   Filter Q
//! row 3    caption (16, 179, 160, 14)   slider (8, 194, 160, 18)   Balance
//! Left     (8,  212, 80, 14)            Right (88, 212, 48, 14)
//! restore  (8,  234, 18, 18)
//! ```
//!
//! (`FxAudioControls.cpp:430-460`, resolved in `docs/spec/03-controls.md` §5.1.)
//!
//! ## Semantics
//!
//! Every control writes through the same [`UiAction`] the command line uses, so what a control does
//! is the controller's business: the band count carries the curve over rather than flattening it
//! (upstream 182a329), and Restore Defaults puts back no leveling, a centred balance, the
//! narrowest filter and no gain, and keeps the band count and the curve, which are the user's and
//! the preset's and not defaults (`FxEqualizerControl::restoreDefaults`,
//! `FxAudioControls.cpp:529-544`, which also put back ten bands; 0.4.0 audit R5). The ranges and the
//! right-click resets are the original's (§5.2): the four level sliders are `FxAudioSlider` and
//! `FxBalanceSlider`, which both reset to their default on a right-click. The steps are too, but
//! for the master gain and the balance, which step by one decibel where the original's step by two
//! ([`Level::range`], 0.4.0 audit #22).
//!
//! ## On a microphone
//!
//! The original has no input direction. Here the column addresses the edit direction, and on a
//! microphone this face shows only what the voice chain has:
//!
//! * **the band count and the filter width**, which the voice chain's equalizer takes like the
//!   music chain's does;
//! * **Makeup Gain in the place of Master Gain**. It is the same control and the same action, and
//!   on a voice it *is* the preset's makeup gain — the stage after the compressor — so it is named
//!   for what it does there. It steps in whole decibels, as the master gain does: voice presets
//!   are voiced at 3, 5, 7 and 9 dB;
//! * **Restore Defaults**, with the controller's meaning for a voice (the gain goes back to 0 and
//!   the width to x1, as an edit to the preset; the band count and the curve stay).
//!
//! Volume Leveling and Balance belong to the music chain's equalizer block and have no stage in
//! the voice chain, so on a microphone they are not drawn at all, and Filter Q moves up into the
//! row under the gain. Face A greys its five sliders instead, because there nothing at all would be
//! left; here the controls that do apply are the face, and two dead rows between them would only
//! be clutter.
//!
//! ## Greyed while the equalizer is off
//!
//! Upstream aad64c1 made the equalizer's switch the whole block's: with the EQ off, the master
//! gain, the balance and the leveling are bypassed with it. A control that silently does nothing
//! is the one thing this port keeps refusing to ship, so while the EQ is off the sliders whose
//! stage is bypassed are drawn grey — and stay live, like the equalizer's own faders, so a setting
//! can be made before the EQ is switched back on. The power switch still greys and disables
//! everything, as `FxProView::paint` does in the original (`docs/spec/03-controls.md` §6).

use crate::assets::{AssetCache, FxImage};
use crate::state::{UiAction, UiResponse, UiState};
use crate::theme::{FxColor, Palette};
use crate::views::pro::{CAPTION_FONT_PX, VALUE_FONT_PX, caption_font};
use crate::widgets::equalizer::BAND_COUNTS;
use crate::widgets::slider::{self, FxSlider, Track};
use crate::widgets::{FxComboBox, IconButton};
use egui::{Align2, Rect, Ui, Vec2, pos2, vec2};
use fxsound_core::i18n::tr;
use fxsound_core::{DeviceDirection, WindowsLook};

/// `FxEqualizerControl::X_MARGIN`.
pub const X_MARGIN: f32 = 8.0;
/// `FxEqualizerControl::Y_MARGIN`: where the band-count combo starts.
pub const Y_MARGIN: f32 = 28.0;
/// `FxEqualizerControl::ROW_GAP`: between one slider's bottom and the next caption.
pub const ROW_GAP: f32 = 8.0;
/// `FxEqualizerControl::LABEL_HEIGHT`.
pub const CAPTION_HEIGHT: f32 = 14.0;
/// `caption.bottom + 1` is where the slider starts (`FxAudioControls.cpp:440`).
pub const CAPTION_GAP: f32 = 1.0;
/// `SLIDER_WIDTH × SLIDER_HEIGHT`.
pub const SLIDER_SIZE: Vec2 = vec2(160.0, 18.0);
/// `COMBOBOX_HEIGHT`; the width is the column's less a margin either side.
pub const COMBO_HEIGHT: f32 = 20.0;
/// `BUTTON_WIDTH × BUTTON_HEIGHT` — Restore Defaults here, and the flip button on the card.
pub const BUTTON_SIZE: Vec2 = vec2(18.0, 18.0);
/// The flip button sits this far in from the card's top and right edges
/// (`FxAudioControls.cpp:83`: `getWidth() - BUTTON_WIDTH - 5, 5`).
pub const FLIP_INSET: f32 = 5.0;
/// One row: caption, gap, slider, gap.
pub const ROW_PITCH: f32 = CAPTION_HEIGHT + CAPTION_GAP + SLIDER_SIZE.y + ROW_GAP;
/// `FxAudioSlider::LABEL_WIDTH × LABEL_HEIGHT` — the floating value readout.
pub const VALUE_LABEL_SIZE: Vec2 = vec2(40.0, 14.0);
/// `pos + SLIDER_THUMB_RADIUS / 2 + 1` (`FxAudioSlider.cpp:99`).
pub const VALUE_LABEL_OFFSET: f32 = slider::THUMB_RADIUS / 2.0 + 1.0;
/// `juce::Label`'s default left border: the readout's glyphs start five points inside its box.
pub const LABEL_BORDER_LEFT: f32 = 5.0;
/// The Left and Right captions under the balance: `getNormalFont().withHeight(12.0f)`
/// (`FxAudioControls.cpp:365-377`).
pub const SIDE_FONT_PX: f32 = 12.0;
/// A JUCE `Label` paints its text at half alpha while disabled **[JUCE semantics]**
/// (`LookAndFeel_V2::drawLabel`).
pub const DISABLED_TEXT_ALPHA: f32 = 0.5;
/// A `DrawableButton` without a disabled image draws its normal one at this opacity while
/// disabled **[JUCE semantics]** (`DrawableButton::buttonStateChanged`).
pub const DISABLED_BUTTON_OPACITY: f32 = 0.4;

/// The band-count combo: `(X_MARGIN, Y_MARGIN, width − 2·X_MARGIN, COMBOBOX_HEIGHT)`.
#[must_use]
pub fn band_combo(column: Rect) -> Rect {
    Rect::from_min_size(
        column.min + vec2(X_MARGIN, Y_MARGIN),
        vec2(column.width() - X_MARGIN * 2.0, COMBO_HEIGHT),
    )
}

/// Top of row `row`'s caption: `ROW_GAP` under the combo, then one pitch per row.
#[must_use]
pub fn row_top(column: Rect, row: usize) -> f32 {
    band_combo(column).bottom() + ROW_GAP + ROW_PITCH * row as f32
}

/// Row `row`'s caption, lined up with the thumb's centre at the minimum as face A's are.
#[must_use]
pub fn caption_rect(column: Rect, row: usize) -> Rect {
    Rect::from_min_size(
        pos2(
            column.left() + X_MARGIN + slider::THUMB_RADIUS,
            row_top(column, row),
        ),
        vec2(SLIDER_SIZE.x, CAPTION_HEIGHT),
    )
}

/// Row `row`'s slider.
#[must_use]
pub fn slider_rect(column: Rect, row: usize) -> Rect {
    Rect::from_min_size(
        pos2(
            column.left() + X_MARGIN,
            caption_rect(column, row).bottom() + CAPTION_GAP,
        ),
        SLIDER_SIZE,
    )
}

/// "Left", under the balance slider's left half (`FxAudioControls.cpp:455`).
#[must_use]
pub fn left_label(column: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            column.left() + X_MARGIN,
            slider_rect(column, Level::BALANCE_ROW).bottom(),
        ),
        vec2(SLIDER_SIZE.x / 2.0, CAPTION_HEIGHT),
    )
}

/// "Right", `SLIDER_WIDTH / 2 − 4·SLIDER_THUMB_RADIUS` wide after "Left", so its right-justified
/// text ends 24 points in from the column's edge (`FxAudioControls.cpp:456`).
#[must_use]
pub fn right_label(column: Rect) -> Rect {
    let left = left_label(column);
    Rect::from_min_size(
        pos2(left.right(), left.top()),
        vec2(
            SLIDER_SIZE.x / 2.0 - slider::THUMB_RADIUS * 4.0,
            CAPTION_HEIGHT,
        ),
    )
}

/// Restore Defaults, `ROW_GAP` under the Left and Right captions (`FxAudioControls.cpp:459`).
///
/// It stays here on a microphone too, where the rows above it are fewer: a button that moved with
/// the edit direction would be one to hunt for.
#[must_use]
pub fn restore_defaults(column: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            column.left() + X_MARGIN,
            left_label(column).bottom() + ROW_GAP,
        ),
        BUTTON_SIZE,
    )
}

/// The flip button, on the card rather than on either face (`FxAudioControls.cpp:83`).
#[must_use]
pub fn flip_button(column: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            column.right() - BUTTON_SIZE.x - FLIP_INSET,
            column.top() + FLIP_INSET,
        ),
        BUTTON_SIZE,
    )
}

/// The value readout's box for a value at proportion `t` of the slider's range: 40 × 14, five
/// points right of the thumb's centre, vertically centred (`FxAudioSlider.cpp:92-100`).
#[must_use]
pub fn value_label_rect(slider_rect: Rect, t: f32) -> Rect {
    let track = slider::track_rect(slider_rect);
    let thumb_x = track.left() + track.width() * t.clamp(0.0, 1.0);
    Rect::from_min_size(
        pos2(
            thumb_x + VALUE_LABEL_OFFSET,
            slider_rect.top() + ((slider_rect.height() - VALUE_LABEL_SIZE.y) / 2.0).floor(),
        ),
        VALUE_LABEL_SIZE,
    )
}

/// Where a readout `text_width` wide starts: at the label's own inset, or — near the maximum, where
/// the original's 40-point box hangs past the slider — pulled back so the text ends at the
/// slider's edge instead of being clipped by the card (`docs/spec/03-controls.md` §5.3).
#[must_use]
pub fn value_text_x(slider_rect: Rect, t: f32, text_width: f32) -> f32 {
    let label = value_label_rect(slider_rect, t);
    (label.left() + LABEL_BORDER_LEFT).min(slider_rect.right() - text_width)
}

/// What a band-count row says: `String(bands) + TRANS(" Bands")` (`FxAudioControls.cpp:301`).
#[must_use]
pub fn band_count_label(count: usize) -> String {
    format!("{count}{}", tr(" Bands"))
}

/// The row of the combo that shows `count`, when it is one of the five.
///
/// `selectEqualizerBands` shows 10 for any other count (`FxAudioControls.cpp:509-527`); the port
/// shows the count itself instead (see [`show`]), since a box that names a band count the curve
/// does not have is a box that misreports it.
#[must_use]
pub fn band_count_row(count: usize) -> Option<usize> {
    BAND_COUNTS.iter().position(|&offered| offered == count)
}

/// The four level sliders of `FxEqualizerControl`, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    MasterGain,
    VolumeLeveling,
    FilterQ,
    Balance,
}

/// A level slider's range, step and right-click value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelRange {
    pub min: f32,
    pub max: f32,
    pub step: f32,
    /// What a right-click and Restore Defaults put back.
    pub default: f32,
}

impl Level {
    /// The rows on the speakers, in the original's order.
    pub const OUTPUT: [Self; 4] = [
        Self::MasterGain,
        Self::VolumeLeveling,
        Self::FilterQ,
        Self::Balance,
    ];
    /// The rows on a microphone: the two the voice chain has (see the module docs).
    pub const INPUT: [Self; 2] = [Self::MasterGain, Self::FilterQ];
    /// The balance's row on the speakers, which the Left and Right captions hang under.
    pub const BALANCE_ROW: usize = 3;

    /// The rows this face shows for an edit direction.
    #[must_use]
    pub fn rows(direction: DeviceDirection) -> &'static [Self] {
        match direction {
            DeviceDirection::Output => &Self::OUTPUT,
            DeviceDirection::Input => &Self::INPUT,
        }
    }

    /// The original's range and step (`docs/spec/03-controls.md` §5.2), except that the master
    /// gain and the balance step by one decibel rather than two, in either direction.
    ///
    /// The original's sliders step by 2 dB while its controller and command line round to whole
    /// decibels (`FxAudioControls.cpp:312`, `FxBalanceSlider.cpp:36`, `FxController.cpp:1801-1818`),
    /// so `--master_gain=3` could not be set from the window and one arrow press took it to 4
    /// (0.4.0 audit #22). One decibel is what the rest of the app already means by a step, and a
    /// voice's makeup gain needs it anyway: voice presets are voiced at 3, 5, 7 and 9 dB.
    #[must_use]
    pub const fn range(self, direction: DeviceDirection) -> LevelRange {
        let _ = direction;
        let (min, max, step, default) = match self {
            Self::MasterGain | Self::Balance => (-20.0, 20.0, 1.0, 0.0),
            Self::VolumeLeveling => (0.0, 4.0, 0.5, 0.0),
            Self::FilterQ => (1.0, 3.0, 0.5, 1.0),
        };
        LevelRange {
            min,
            max,
            step,
            default,
        }
    }

    /// The range the window's slider has for the state in force: [`Level::range`], except that at
    /// «Как в Windows» = Interface and above the speakers' Master Gain and Balance step by two
    /// decibels, as the Windows sliders do ([`WindowsLook::LevelSteps`], 0.4.0 audit #22). The
    /// microphone's Makeup Gain, which Windows does not have, keeps its one.
    #[must_use]
    pub const fn range_in(self, state: &UiState) -> LevelRange {
        let mut range = self.range(state.direction);
        if matches!(self, Self::MasterGain | Self::Balance)
            && matches!(state.direction, DeviceDirection::Output)
            && state.windows_look(WindowsLook::LevelSteps)
        {
            range.step = 2.0;
        }
        range
    }

    /// The caption over the slider, translated.
    #[must_use]
    pub fn caption(self, direction: DeviceDirection) -> String {
        tr(match (self, direction) {
            (Self::MasterGain, DeviceDirection::Output) => "Master Gain",
            (Self::MasterGain, DeviceDirection::Input) => "Makeup Gain",
            (Self::VolumeLeveling, _) => "Volume Leveling",
            (Self::FilterQ, _) => "Filter Q",
            (Self::Balance, _) => "Balance",
        })
    }

    /// The edit direction's value.
    #[must_use]
    pub const fn value(self, state: &UiState) -> f32 {
        match self {
            Self::MasterGain => state.master_gain_db,
            Self::VolumeLeveling => state.volume_leveling,
            Self::FilterQ => state.filter_q,
            Self::Balance => state.balance_db,
        }
    }

    /// What moving the slider to `value` asks the controller for.
    #[must_use]
    pub const fn action(self, value: f32) -> UiAction {
        match self {
            Self::MasterGain => UiAction::SetMasterGain(value),
            Self::VolumeLeveling => UiAction::SetVolumeLeveling(value),
            Self::FilterQ => UiAction::SetFilterQ(value),
            Self::Balance => UiAction::SetBalance(value),
        }
    }

    /// The floating readout: the original's `printf` formats, `%0.0f dB` and `%.1fx`, and the
    /// balance's magnitude alone — which side it leans to is the thumb's and the Left and Right
    /// captions' to say (`FxAudioControls.cpp:268-271`, `FxBalanceSlider.cpp:146`).
    ///
    /// Two liberties. The volume leveling reads a bare `%.1f`: the original says `%.1f dB`, but
    /// the amount is a 0 to 4 setting of the leveller, whose 2.0 is a target level and no two
    /// decibels of anything (0.4.0 audit #23). And a value that rounds to nothing reads `0`, never
    /// `-0` — a voice preset's −0.4 dB of makeup is no gain, not a negative one.
    #[must_use]
    pub fn readout(self, value: f32) -> String {
        let (number, unit) = match self {
            Self::MasterGain => (format!("{value:.0}"), " dB"),
            Self::VolumeLeveling => (format!("{value:.1}"), ""),
            Self::FilterQ => (format!("{value:.1}"), "x"),
            Self::Balance => (format!("{:.0}", value.abs()), " dB"),
        };
        let number = match number.strip_prefix('-') {
            Some(magnitude) if magnitude.chars().all(|c| c == '0' || c == '.') => magnitude,
            _ => number.as_str(),
        };
        format!("{number}{unit}")
    }

    /// The readout the window shows for the state in force: [`Level::readout`], except that at
    /// «Как в Windows» = Interface and above Volume Leveling says `dB` after its amount again, as
    /// the original's `%.1f dB` does ([`WindowsLook::LevelingUnit`], 0.4.0 audit #23).
    #[must_use]
    pub fn readout_in(self, value: f32, state: &UiState) -> String {
        let text = self.readout(value);
        if self == Self::VolumeLeveling && state.windows_look(WindowsLook::LevelingUnit) {
            format!("{text} dB")
        } else {
            text
        }
    }

    /// Whether the stage this slider sets is in the signal path right now.
    ///
    /// The filter width is the equalizer's and the volume leveller is in its block, so both go
    /// with its switch (upstream aad64c1). The master gain and the balance do not: the engine
    /// plays them whatever the equalizer's switch says (0.4.0 audit R3), where upstream took
    /// them out with the block. On a microphone the makeup gain is a stage of its own.
    #[must_use]
    pub const fn in_path(self, state: &UiState) -> bool {
        match self {
            Self::MasterGain | Self::Balance => true,
            Self::VolumeLeveling | Self::FilterQ => state.eq_on,
        }
    }
}

/// Paint face B into the 168 × 257 `column` and report what the user did.
///
/// The card behind it and the flip button on top of it are the column's, drawn by the Pro view.
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    palette: Palette,
    assets: &mut AssetCache,
    column: Rect,
    response: &mut UiResponse,
) {
    let enabled = state.controls_enabled();
    let direction = state.direction;
    let caption_colour = palette.color(FxColor::DefaultText);
    // Face A's readouts are `HighlightedText`; the two faces' numbers read alike.
    let value_colour = if enabled {
        palette.color(FxColor::HighlightedText)
    } else {
        palette.color_alpha(FxColor::HighlightedText, DISABLED_TEXT_ALPHA)
    };

    // --- the band count ------------------------------------------------------------------------
    let count = state.eq_bands.len();
    let items: Vec<String> = BAND_COUNTS.into_iter().map(band_count_label).collect();
    let selected = band_count_row(count);
    // A count none of the five rows name — a hand-edited voice preset, a settings file from
    // elsewhere — is shown as it is, in the placeholder's place, rather than as a row it is not.
    let unlisted = band_count_label(count);
    let (_, picked) = FxComboBox::new(&items, selected)
        .enabled(enabled)
        .placeholder(if selected.is_none() {
            unlisted.as_str()
        } else {
            ""
        })
        .show(ui, band_combo(column), palette, assets, "fx_band_count");
    if let Some(row) = picked
        && let Some(&bands) = BAND_COUNTS.get(row)
        && bands != count
    {
        response.push(UiAction::SetBandCount(bands));
    }

    // --- the level sliders ---------------------------------------------------------------------
    for (row, &level) in Level::rows(direction).iter().enumerate() {
        ui.painter().text(
            caption_rect(column, row).left_top(),
            Align2::LEFT_TOP,
            level.caption(direction),
            caption_font(CAPTION_FONT_PX),
            caption_colour,
        );

        let range = level.range_in(state);
        let rect = slider_rect(column, row);
        let before = level.value(state);
        let mut value = before;
        let slider = FxSlider::new(&mut value, range.min, range.max, range.step)
            .default_value(range.default)
            .reset_on_secondary_click(true)
            .fidelity(super::pro::slider_fidelity(state))
            .enabled(enabled)
            .lit(level.in_path(state))
            .track(if level == Level::Balance {
                Track::Balance
            } else {
                Track::Filled
            })
            .show(ui, rect, palette, assets, ("fx_level", level as u8));
        if slider.changed() && value != before {
            response.push(level.action(value));
        }
        // The original has no tooltip here, and so nothing that tells of the right-click reset
        // (0.4.0 audit R9); "Hide help tips" hides it with the rest, and «Как в Windows» =
        // Interface and above go without it as the original does. The microphone's levels are
        // the port's own, with no Windows original to follow, and keep theirs at every level.
        if state.lane_tips_shown() {
            let _ = slider.on_hover_text(slider::with_reset_tip(None));
        }

        // A plain visible child in the original, so — unlike face A's — shown with the power off
        // too, at a disabled label's half alpha.
        let text = level.readout_in(value, state);
        let font = caption_font(VALUE_FONT_PX);
        let width = ui
            .painter()
            .layout_no_wrap(text.clone(), font.clone(), value_colour)
            .size()
            .x;
        let t = (value - range.min) / (range.max - range.min);
        let label = value_label_rect(rect, t);
        ui.painter().text(
            pos2(value_text_x(rect, t, width), label.center().y),
            Align2::LEFT_CENTER,
            text,
            font,
            value_colour,
        );
    }

    if direction == DeviceDirection::Output {
        let font = caption_font(SIDE_FONT_PX);
        ui.painter().text(
            left_label(column).left_center(),
            Align2::LEFT_CENTER,
            tr("Left"),
            font.clone(),
            caption_colour,
        );
        ui.painter().text(
            right_label(column).right_center(),
            Align2::RIGHT_CENTER,
            tr("Right"),
            font,
            caption_colour,
        );
    }

    // --- Restore Defaults ----------------------------------------------------------------------
    let tip = tr("Restore Defaults");
    let restore = IconButton::new(FxImage::RestoreDefaultsButton)
        .hover(FxImage::RestoreDefaultsButtonHover)
        .enabled(enabled)
        .opacity(if enabled {
            1.0
        } else {
            DISABLED_BUTTON_OPACITY
        })
        .tooltip(&tip)
        .hide_tooltips(state.hide_tooltips)
        .show(
            ui,
            restore_defaults(column),
            palette,
            assets,
            "fx_restore_defaults",
        );
    if restore.clicked() {
        response.push(UiAction::RestoreDefaults);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;
    use crate::views::testing::{Harness, text_below, texts};
    use crate::views::{ColumnFace, ViewScratch};
    use egui::epaint::ClippedShape;
    use egui::{Color32, Event, Modifiers, PointerButton, Pos2, Shape};
    use fxsound_core::{AudioDevice, ThemeMode, ViewMode, WindowsParity};

    /// The column at the window-local place the Pro view puts it.
    fn column() -> Rect {
        layout::pro::audio_controls()
    }

    /// The column at the origin, where the spec's column-local numbers apply as they are.
    fn local() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(168.0, 257.0))
    }

    fn at(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(x, y), vec2(w, h))
    }

    fn speakers() -> UiState {
        UiState {
            view: ViewMode::Pro,
            devices: vec![AudioDevice {
                id: 1,
                name: "alsa_output.pci-0000_00_1f.3.analog-stereo".to_owned(),
                description: "Built-in Audio".to_owned(),
                is_default: true,
                direction: DeviceDirection::Output,
                form_factor: "speaker".into(),
            }],
            selected_output: Some(0),
            master_gain_db: -4.0,
            volume_leveling: 1.5,
            filter_q: 2.0,
            balance_db: -6.0,
            ..UiState::default()
        }
    }

    fn microphone() -> UiState {
        UiState {
            devices: vec![AudioDevice {
                id: 2,
                name: "alsa_input.usb-fifine.analog-stereo".to_owned(),
                description: "fifine Microphone".to_owned(),
                is_default: true,
                direction: DeviceDirection::Input,
                form_factor: "microphone".into(),
            }],
            selected_output: None,
            selected_input: Some(0),
            direction: DeviceDirection::Input,
            master_gain_db: 5.0,
            ..speakers()
        }
    }

    /// A harness with the column already turned over.
    fn turned_over(mode: ThemeMode) -> Harness {
        let mut harness = Harness::new(mode);
        harness.scratch.column_face = ColumnFace::EqualizerControls;
        harness
    }

    /// The text painted inside `area`, top to bottom.
    fn texts_in(shapes: &[ClippedShape], area: Rect) -> Vec<String> {
        let mut found: Vec<_> = texts(shapes)
            .into_iter()
            .filter(|(_, rect, _)| area.contains(rect.center()))
            .collect();
        found.sort_by(|a, b| a.1.top().total_cmp(&b.1.top()));
        found.into_iter().map(|(text, _, _)| text).collect()
    }

    /// Where the one line reading `wanted` was painted, and in what colour.
    fn painted(shapes: &[ClippedShape], wanted: &str) -> (Rect, Color32) {
        let found: Vec<_> = texts(shapes)
            .into_iter()
            .filter(|(text, _, _)| text == wanted)
            .collect();
        assert_eq!(found.len(), 1, "{wanted:?} painted {} times", found.len());
        (found[0].1, found[0].2)
    }

    /// Press and release `button` at `pos`, every frame's actions reported.
    fn press(
        harness: &mut Harness,
        state: &UiState,
        pos: Pos2,
        button: PointerButton,
    ) -> Vec<UiAction> {
        let event = |pressed| Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::default(),
        };
        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(pos)],
            vec![Event::PointerMoved(pos), event(true)],
            vec![event(false)],
        ] {
            actions.extend(harness.frame(state, events).0);
        }
        actions
    }

    /// The fill of the rectangle painted at exactly `rect`, if one was.
    fn fill_at(shapes: &[ClippedShape], rect: Rect) -> Option<Color32> {
        shapes.iter().find_map(|clipped| match &clipped.shape {
            Shape::Rect(shape)
                if (shape.rect.min - rect.min).length() < 1e-3
                    && (shape.rect.max - rect.max).length() < 1e-3 =>
            {
                Some(shape.fill)
            }
            _ => None,
        })
    }

    fn is_grey(colour: Color32) -> bool {
        colour.r() == colour.g() && colour.g() == colour.b()
    }

    // ---- geometry ----------------------------------------------------------------------------

    #[test]
    fn every_control_lands_on_the_specs_resolved_rectangle() {
        // docs/spec/03-controls.md §5.1, column-local.
        let column = local();
        assert_eq!(band_combo(column), at(8.0, 28.0, 152.0, 20.0));
        for (row, y) in [56.0, 97.0, 138.0, 179.0].into_iter().enumerate() {
            assert_eq!(
                caption_rect(column, row),
                at(16.0, y, 160.0, 14.0),
                "row {row}"
            );
            assert_eq!(
                slider_rect(column, row),
                at(8.0, y + 15.0, 160.0, 18.0),
                "row {row}"
            );
        }
        assert_eq!(left_label(column), at(8.0, 212.0, 80.0, 14.0));
        assert_eq!(right_label(column), at(88.0, 212.0, 48.0, 14.0));
        assert_eq!(restore_defaults(column), at(8.0, 234.0, 18.0, 18.0));
    }

    #[test]
    fn the_flip_button_sits_five_points_in_from_the_cards_top_right_corner() {
        // §5.7: (getWidth() - 18 - 5, 5) = (145, 5), 18 x 18.
        assert_eq!(flip_button(local()), at(145.0, 5.0, 18.0, 18.0));
        let flip = flip_button(column());
        assert_eq!(flip.right(), column().right() - FLIP_INSET);
        // Clear of the band combo under it on this face.
        assert!(flip.bottom() < band_combo(column()).top());
    }

    #[test]
    fn the_flip_button_is_clear_of_the_first_effect_captions_text_in_every_language() {
        use crate::views::pro::effects;
        use fxsound_core::Effect;
        use fxsound_core::i18n::{Catalogue, LANGUAGES};
        let column = column();
        let flip = flip_button(column);
        // The caption's 160-point box starts at y = 21, two points above the flip's bottom edge,
        // and runs under it: the box meets the button. The text in it is short and
        // left-justified, and the text is what has to stay clear.
        let caption = effects::caption_rect(column, 0);
        assert!(caption.intersects(flip), "{caption:?} {flip:?}");

        let mut harness = Harness::new(ThemeMode::Dark);
        assert_eq!(harness.scratch.column_face, ColumnFace::Effects);
        let shapes = harness.settle(&speakers());
        let (clarity, _) = painted(&shapes, Effect::ALL[0].label());
        assert!(clarity.right() < flip.left(), "{clarity:?} {flip:?}");

        let screen = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, layout::pro::WINDOW_SIZE)),
            ..Default::default()
        };
        let mut problems = Vec::new();
        harness
            .ctx
            .run_ui(screen, |ui| {
                for language in &LANGUAGES {
                    let table = Catalogue::for_language(language);
                    let key = Effect::ALL[0].label();
                    let text = table.get(key).unwrap_or(key).to_owned();
                    let width = ui
                        .painter()
                        .layout_no_wrap(
                            text.clone(),
                            caption_font(CAPTION_FONT_PX),
                            Color32::PLACEHOLDER,
                        )
                        .size()
                        .x;
                    if caption.left() + width >= flip.left() {
                        problems.push(format!(
                            "{}: {text:?} ends at {:.0}, the flip starts at {}",
                            language.code,
                            caption.left() + width,
                            flip.left()
                        ));
                    }
                }
            })
            .drop_without_applying_deltas();
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn the_row_pitch_is_the_sum_of_the_parts_the_original_adds_up() {
        assert_eq!(ROW_PITCH, 41.0);
        assert_eq!(
            slider_rect(local(), 1).top() - slider_rect(local(), 0).top(),
            ROW_PITCH
        );
    }

    #[test]
    fn restore_defaults_leaves_the_same_five_points_at_the_bottom_as_the_flip_leaves_at_the_top() {
        let column = local();
        assert_eq!(column.bottom() - restore_defaults(column).bottom(), 5.0);
        assert_eq!(flip_button(column).top() - column.top(), 5.0);
    }

    #[test]
    fn every_slider_and_button_stays_inside_the_card() {
        let column = column();
        for row in 0..Level::OUTPUT.len() {
            assert!(column.contains_rect(slider_rect(column, row)), "row {row}");
        }
        for rect in [
            band_combo(column),
            left_label(column),
            right_label(column),
            restore_defaults(column),
            flip_button(column),
        ] {
            assert!(column.contains_rect(rect), "{rect:?}");
        }
    }

    #[test]
    fn the_readout_follows_the_thumb_five_points_to_its_right() {
        // FxAudioSlider.cpp:99-100: x = pos + 5, y = (18 - 14) / 2.
        let slider = slider_rect(local(), 0);
        for (t, x) in [(0.0, 21.0), (0.5, 77.0), (1.0, 133.0)] {
            let label = value_label_rect(slider, t);
            assert_eq!(label, at(x, slider.top() + 2.0, 40.0, 14.0), "t = {t}");
        }
    }

    #[test]
    fn a_readout_too_wide_for_the_end_of_the_slider_is_pulled_back_inside_it() {
        let slider = slider_rect(local(), 0);
        // Narrow text at the maximum starts at its label's inset…
        assert_eq!(value_text_x(slider, 1.0, 20.0), 138.0);
        // …and text that would run past the slider's edge ends on it.
        assert_eq!(value_text_x(slider, 1.0, 40.0), slider.right() - 40.0);
        assert_eq!(value_text_x(slider, 0.0, 40.0), 26.0);
    }

    // ---- the levels --------------------------------------------------------------------------

    #[test]
    fn the_four_levels_have_the_originals_ranges_and_defaults() {
        // docs/spec/03-controls.md §5.2, and the original's steps but for the gain and the
        // balance's (below).
        let out = DeviceDirection::Output;
        let range = |level: Level| {
            let r = level.range(out);
            (r.min, r.max, r.step, r.default)
        };
        assert_eq!(range(Level::MasterGain), (-20.0, 20.0, 1.0, 0.0));
        assert_eq!(range(Level::VolumeLeveling), (0.0, 4.0, 0.5, 0.0));
        assert_eq!(range(Level::FilterQ), (1.0, 3.0, 0.5, 1.0));
        assert_eq!(range(Level::Balance), (-20.0, 20.0, 1.0, 0.0));
    }

    #[test]
    fn the_master_gain_and_the_balance_step_by_the_whole_decibel_the_command_line_rounds_to() {
        // 0.4.0 audit #22: the original's 2 dB step left every odd decibel `--master_gain` and
        // `--balance` accept out of the window's reach, 21 positions of the 41.
        for level in [Level::MasterGain, Level::Balance] {
            let r = level.range(DeviceDirection::Output);
            let reachable = (-20..=20)
                .filter(|&db| {
                    let db = db as f32;
                    slider::quantise(db, r.min, r.max, r.step) == db
                })
                .count();
            assert_eq!(reachable, 41, "{level:?}");
            assert_eq!(slider::step_towards(3.0, r.min, r.step, true), 4.0);
            assert_eq!(slider::step_towards(3.0, r.min, r.step, false), 2.0);
        }
    }

    #[test]
    fn at_interface_the_master_gain_and_the_balance_step_by_two_decibels_as_on_windows() {
        // «Как в Windows» = Interface sets 0.4.0 audit #22 back (`FxAudioControls.cpp:312`,
        // `FxBalanceSlider.cpp:36`); Off keeps the whole decibel (the test above).
        for level in [WindowsParity::Interface, WindowsParity::Sound] {
            let state = UiState {
                windows_parity: level,
                ..UiState::default()
            };
            for row in [Level::MasterGain, Level::Balance] {
                let r = row.range_in(&state);
                assert_eq!((r.min, r.max, r.step, r.default), (-20.0, 20.0, 2.0, 0.0));
                // A --master_gain=3 goes to 4 or 2 at an arrow, as the Windows slider snaps it.
                assert_eq!(slider::step_towards(3.0, r.min, r.step, true), 4.0);
                assert_eq!(slider::step_towards(4.0, r.min, r.step, true), 6.0);
            }
            for row in [Level::VolumeLeveling, Level::FilterQ] {
                assert_eq!(row.range_in(&state), row.range(DeviceDirection::Output));
            }
            // The microphone's Makeup Gain has no Windows original and keeps its decibel.
            let microphone = UiState {
                direction: DeviceDirection::Input,
                ..state
            };
            assert_eq!(Level::MasterGain.range_in(&microphone).step, 1.0);
        }
        let off = UiState::default();
        assert_eq!(Level::MasterGain.range_in(&off).step, 1.0);
        assert_eq!(Level::Balance.range_in(&off).step, 1.0);
    }

    #[test]
    fn at_interface_the_volume_leveling_says_db_as_on_windows_and_off_it_does_not() {
        // 0.4.0 audit #23 set back at Interface: the original's `%.1f dB`.
        let windows = UiState {
            windows_parity: WindowsParity::Interface,
            ..UiState::default()
        };
        assert_eq!(Level::VolumeLeveling.readout_in(1.5, &windows), "1.5 dB");
        assert_eq!(
            Level::VolumeLeveling.readout_in(1.5, &UiState::default()),
            "1.5"
        );
        // The other readouts are the original's already.
        assert_eq!(Level::MasterGain.readout_in(-4.0, &windows), "-4 dB");
        assert_eq!(Level::FilterQ.readout_in(2.0, &windows), "2.0x");
    }

    #[test]
    fn the_makeup_gain_steps_in_whole_decibels_so_every_voice_presets_value_is_reachable() {
        let r = Level::MasterGain.range(DeviceDirection::Input);
        assert_eq!((r.min, r.max, r.step, r.default), (-20.0, 20.0, 1.0, 0.0));
        for voiced in [3.0, 5.0, 7.0, 9.0] {
            assert_eq!(slider::quantise(voiced, r.min, r.max, r.step), voiced);
        }
    }

    #[test]
    fn a_microphone_gets_the_gain_and_the_filter_width_and_nothing_else() {
        assert_eq!(Level::rows(DeviceDirection::Output), Level::OUTPUT);
        assert_eq!(
            Level::rows(DeviceDirection::Input),
            [Level::MasterGain, Level::FilterQ]
        );
        assert_eq!(Level::OUTPUT[Level::BALANCE_ROW], Level::Balance);
    }

    #[test]
    fn the_volume_leveling_reads_its_amount_without_a_unit_it_does_not_have() {
        // 0.4.0 audit #23: the original's "2.0 dB" is the leveller's 0 to 4 amount, whose 2.0 is
        // a target level, not two decibels.
        assert_eq!(Level::VolumeLeveling.readout(1.5), "1.5");
        assert_eq!(Level::VolumeLeveling.readout(4.0), "4.0");
        assert_eq!(Level::VolumeLeveling.readout(0.0), "0.0");
        assert!(!Level::VolumeLeveling.readout(2.0).contains("dB"));
    }

    #[test]
    fn the_readouts_use_the_originals_formats() {
        assert_eq!(Level::MasterGain.readout(-4.0), "-4 dB");
        assert_eq!(Level::MasterGain.readout(20.0), "20 dB");
        assert_eq!(Level::FilterQ.readout(1.0), "1.0x");
        assert_eq!(Level::FilterQ.readout(2.0), "2.0x");
        // The balance says how far, not which way.
        assert_eq!(Level::Balance.readout(-14.0), "14 dB");
        assert_eq!(Level::Balance.readout(14.0), "14 dB");
    }

    #[test]
    fn a_readout_never_says_minus_zero() {
        assert_eq!(Level::MasterGain.readout(-0.4), "0 dB");
        assert_eq!(Level::MasterGain.readout(-0.0), "0 dB");
        assert_eq!(Level::VolumeLeveling.readout(-0.01), "0.0");
        assert_eq!(Level::MasterGain.readout(-0.6), "-1 dB");
    }

    #[test]
    fn each_level_sends_the_action_the_command_line_sends() {
        assert_eq!(Level::MasterGain.action(2.0), UiAction::SetMasterGain(2.0));
        assert_eq!(
            Level::VolumeLeveling.action(2.0),
            UiAction::SetVolumeLeveling(2.0)
        );
        assert_eq!(Level::FilterQ.action(2.0), UiAction::SetFilterQ(2.0));
        assert_eq!(Level::Balance.action(2.0), UiAction::SetBalance(2.0));
    }

    #[test]
    fn with_the_equalizer_off_the_leveller_and_the_width_leave_the_path_and_the_gains_stay() {
        // 0.4.0 audit R3: the master gain and the balance play whatever the equalizer's switch
        // says, so they are not drawn as if it had taken them out.
        let off = UiState {
            eq_on: false,
            ..speakers()
        };
        assert!(Level::MasterGain.in_path(&off));
        assert!(Level::Balance.in_path(&off));
        assert!(!Level::VolumeLeveling.in_path(&off));
        assert!(!Level::FilterQ.in_path(&off));
        let voice_off = UiState {
            eq_on: false,
            ..microphone()
        };
        assert!(Level::MasterGain.in_path(&voice_off));
        assert!(!Level::FilterQ.in_path(&voice_off));
        assert!(Level::OUTPUT.iter().all(|level| level.in_path(&speakers())));
    }

    #[test]
    fn the_band_combo_offers_the_five_counts_and_finds_the_current_one() {
        assert_eq!(band_count_label(5), "5 Bands");
        assert_eq!(band_count_label(31), "31 Bands");
        assert_eq!(band_count_row(10), Some(1));
        assert_eq!(band_count_row(31), Some(4));
        assert_eq!(band_count_row(7), None);
    }

    // ---- the window ----------------------------------------------------------------------------

    #[test]
    fn the_column_starts_on_the_effects_and_the_flip_turns_it_over_and_back() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = speakers();
        let column = column();
        assert_eq!(harness.scratch.column_face, ColumnFace::Effects);
        let shapes = harness.settle(&state);
        assert!(texts_in(&shapes, column).contains(&"Clarity".to_owned()));

        let actions = harness.click(&state, flip_button(column).center());
        assert!(
            actions.is_empty(),
            "turning the card over is the window's: {actions:?}"
        );
        assert_eq!(harness.scratch.column_face, ColumnFace::EqualizerControls);
        let shapes = harness.settle(&state);
        let shown = texts_in(&shapes, column);
        assert_eq!(
            shown,
            [
                "10 Bands",
                "Master Gain",
                "-4 dB",
                "Volume Leveling",
                "1.5",
                "Filter Q",
                "2.0x",
                "Balance",
                "6 dB",
                "Left",
                "Right",
            ]
        );

        harness.click(&state, flip_button(column).center());
        assert_eq!(harness.scratch.column_face, ColumnFace::Effects);
        let shapes = harness.settle(&state);
        assert!(!texts_in(&shapes, column).contains(&"Master Gain".to_owned()));
    }

    #[test]
    fn a_flip_repaints_at_once_so_the_new_face_does_not_wait_for_the_next_event() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = speakers();
        harness.settle(&state);
        let pos = flip_button(column()).center();
        harness.frame(&state, vec![Event::PointerMoved(pos)]);
        harness.frame(
            &state,
            vec![Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::default(),
            }],
        );
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, layout::pro::WINDOW_SIZE)),
            events: vec![Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::default(),
            }],
            ..Default::default()
        };
        let Harness {
            ctx,
            scratch,
            assets,
            palette,
            ..
        } = &mut harness;
        let output = ctx.run_ui(input, |ui| {
            crate::views::show(ui, &state, scratch, *palette, assets);
        });
        assert_eq!(scratch.column_face, ColumnFace::EqualizerControls);
        let delay = output
            .viewport_output
            .values()
            .map(|viewport| viewport.repaint_delay)
            .min()
            .expect("a viewport");
        assert_eq!(delay, std::time::Duration::ZERO);
    }

    #[test]
    fn each_caption_and_readout_sits_on_its_own_row() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = speakers();
        let shapes = harness.settle(&state);
        let column = column();
        for (row, level) in Level::OUTPUT.into_iter().enumerate() {
            let (caption, _) = painted(&shapes, &level.caption(DeviceDirection::Output));
            let expected = caption_rect(column, row);
            assert!(
                (caption.left() - expected.left()).abs() < 1.5,
                "{level:?}: {caption:?}"
            );
            assert!(
                caption.top() >= expected.top() - 1.0
                    && caption.bottom() <= expected.bottom() + 1.0,
                "{level:?}: {caption:?} is not inside {expected:?}"
            );
            let (value, _) = painted(&shapes, &level.readout(level.value(&state)));
            let slider = slider_rect(column, row);
            assert!(
                slider.contains(value.center()),
                "{level:?}: {value:?} off {slider:?}"
            );
        }
        let (left, _) = painted(&shapes, "Left");
        let (right, _) = painted(&shapes, "Right");
        assert!(
            (left.left() - left_label(column).left()).abs() < 1.5,
            "{left:?}"
        );
        assert!(
            (right.right() - right_label(column).right()).abs() < 1.5,
            "{right:?}"
        );
    }

    #[test]
    fn the_card_is_filled_with_the_control_background_in_both_palettes() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            for face in [ColumnFace::Effects, ColumnFace::EqualizerControls] {
                let mut harness = Harness::new(mode);
                harness.scratch.column_face = face;
                let shapes = harness.settle(&speakers());
                assert_eq!(
                    fill_at(&shapes, column()),
                    Some(Palette::new(mode).color(FxColor::ControlBackground)),
                    "{mode:?}, {face:?}"
                );
            }
        }
    }

    #[test]
    fn the_captions_and_readouts_are_coloured_like_face_as_in_both_palettes() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let palette = Palette::new(mode);
            let mut harness = turned_over(mode);
            let shapes = harness.settle(&speakers());
            assert_eq!(
                painted(&shapes, "Master Gain").1,
                palette.color(FxColor::DefaultText)
            );
            assert_eq!(
                painted(&shapes, "Left").1,
                palette.color(FxColor::DefaultText)
            );
            assert_eq!(
                painted(&shapes, "-4 dB").1,
                palette.color(FxColor::HighlightedText)
            );
        }
    }

    #[test]
    fn a_microphone_face_shows_makeup_gain_and_filter_q_and_keeps_restore_where_it_was() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = microphone();
        let shapes = harness.settle(&state);
        let column = column();
        assert_eq!(
            texts_in(&shapes, column),
            ["10 Bands", "Makeup Gain", "5 dB", "Filter Q", "2.0x"]
        );
        let (makeup, _) = painted(&shapes, "Makeup Gain");
        assert!(caption_rect(column, 0).contains(makeup.center()));
        let (width, _) = painted(&shapes, "Filter Q");
        assert!(caption_rect(column, 1).contains(width.center()));

        // Restore Defaults is where it is on the speakers, and still answers.
        let actions = harness.click(&state, restore_defaults(column).center());
        assert_eq!(actions, vec![UiAction::RestoreDefaults]);
    }

    #[test]
    fn a_voices_makeup_gain_moves_in_single_decibels() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = microphone();
        harness.settle(&state);
        // 32.5 % of the way along -20..+20 is -7 dB: an odd decibel, which only the whole-decibel
        // step reaches.
        let track = slider::track_rect(slider_rect(column(), 0));
        let target = pos2(track.left() + track.width() * 0.325, track.center().y);
        let actions = press(&mut harness, &state, target, PointerButton::Primary);
        assert_eq!(actions, vec![UiAction::SetMasterGain(-7.0)]);
    }

    #[test]
    fn clicking_along_a_track_sets_that_level_on_its_own_step() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = speakers();
        harness.settle(&state);
        for (row, expected) in [
            (0, UiAction::SetMasterGain(0.0)),
            (1, UiAction::SetVolumeLeveling(2.0)),
            (2, UiAction::SetFilterQ(2.0)),
            (3, UiAction::SetBalance(0.0)),
        ] {
            let target = slider::track_rect(slider_rect(column(), row)).center();
            let actions = press(&mut harness, &state, target, PointerButton::Primary);
            if row == 2 {
                // Filter Q is already at 2.0: the middle of its track is where it is.
                assert!(actions.is_empty(), "row {row}: {actions:?}");
            } else {
                assert_eq!(actions, vec![expected], "row {row}");
            }
        }
    }

    #[test]
    fn a_right_click_puts_each_level_back_to_its_default_and_nowhere_else() {
        // FxAudioSlider.cpp:74-87 and FxBalanceSlider.cpp:126-139: the right button resets and
        // does not also move the thumb to the pointer first.
        let mut harness = turned_over(ThemeMode::Dark);
        let state = speakers();
        harness.settle(&state);
        for (row, expected) in [
            (0, UiAction::SetMasterGain(0.0)),
            (1, UiAction::SetVolumeLeveling(0.0)),
            (2, UiAction::SetFilterQ(1.0)),
            (3, UiAction::SetBalance(0.0)),
        ] {
            let track = slider::track_rect(slider_rect(column(), row));
            let target = pos2(track.right() - 2.0, track.center().y);
            let actions = press(&mut harness, &state, target, PointerButton::Secondary);
            assert_eq!(actions, vec![expected], "row {row}");
        }
    }

    #[test]
    fn every_level_slider_says_on_hover_that_a_right_click_resets_it() {
        // 0.4.0 audit R9: the reset was there and nothing said so.
        for (row, _) in Level::OUTPUT.iter().enumerate() {
            let mut harness = turned_over(ThemeMode::Dark);
            let shown = harness.rest(&speakers(), slider_rect(column(), row).center());
            assert!(
                shown.iter().any(|text| text == slider::RESET_TIP),
                "row {row}: {shown:?}"
            );
        }
        let mut harness = turned_over(ThemeMode::Dark);
        let hidden = UiState {
            hide_tooltips: true,
            ..speakers()
        };
        let shown = harness.rest(&hidden, slider_rect(column(), 0).center());
        assert!(
            !shown.iter().any(|text| text == slider::RESET_TIP),
            "{shown:?}"
        );
    }

    #[test]
    fn at_interface_no_level_slider_says_a_right_click_resets_it_and_the_right_click_still_does() {
        // The original has no tip on these (0.4.0 audit R9 set back at «Как в Windows» =
        // Interface); the reset itself is the original's and stays.
        let windows = UiState {
            windows_parity: WindowsParity::Interface,
            ..speakers()
        };
        for (row, _) in Level::OUTPUT.iter().enumerate() {
            let mut harness = turned_over(ThemeMode::Dark);
            let shown = harness.rest(&windows, slider_rect(column(), row).center());
            assert!(
                !shown.iter().any(|text| text == slider::RESET_TIP),
                "row {row}: {shown:?}"
            );
        }
        let mut harness = turned_over(ThemeMode::Dark);
        harness.settle(&windows);
        let track = slider::track_rect(slider_rect(column(), 1));
        let target = pos2(track.right() - 2.0, track.center().y);
        let actions = press(&mut harness, &windows, target, PointerButton::Secondary);
        assert_eq!(actions, vec![UiAction::SetVolumeLeveling(0.0)]);
    }

    #[test]
    fn at_interface_every_microphone_level_slider_still_says_a_right_click_resets_it() {
        // The input lane is the port's own: nothing in the Windows build to set it back to.
        let windows = UiState {
            windows_parity: WindowsParity::Interface,
            ..microphone()
        };
        let hidden = UiState {
            hide_tooltips: true,
            ..windows.clone()
        };
        for (row, _) in Level::INPUT.iter().enumerate() {
            let mut harness = turned_over(ThemeMode::Dark);
            let shown = harness.rest(&windows, slider_rect(column(), row).center());
            assert!(
                shown.iter().any(|text| text == slider::RESET_TIP),
                "row {row}: {shown:?}"
            );
            let mut harness = turned_over(ThemeMode::Dark);
            let shown = harness.rest(&hidden, slider_rect(column(), row).center());
            assert!(
                !shown.iter().any(|text| text == slider::RESET_TIP),
                "row {row} with help tips hidden: {shown:?}"
            );
        }
    }

    #[test]
    fn at_interface_a_click_on_the_master_gains_track_lands_on_an_even_decibel() {
        // 32 % of the way along is -7.2 dB: -7 on the whole-decibel step, -8 at Interface, the
        // Windows slider's nearest two-decibel position (0.4.0 audit #22 set back).
        let mut harness = turned_over(ThemeMode::Dark);
        let state = UiState {
            windows_parity: WindowsParity::Interface,
            ..speakers()
        };
        harness.settle(&state);
        let track = slider::track_rect(slider_rect(column(), 0));
        let target = pos2(track.left() + track.width() * 0.32, track.center().y);
        let actions = press(&mut harness, &state, target, PointerButton::Primary);
        assert_eq!(actions, vec![UiAction::SetMasterGain(-8.0)]);
        let mut harness = turned_over(ThemeMode::Dark);
        let off = speakers();
        harness.settle(&off);
        let actions = press(&mut harness, &off, target, PointerButton::Primary);
        assert_eq!(actions, vec![UiAction::SetMasterGain(-7.0)]);
    }

    /// The centre of `level`'s thumb on the speakers' `row`.
    fn level_thumb(state: &UiState, row: usize, level: Level) -> Pos2 {
        let rect = slider_rect(column(), row);
        let track = slider::track_rect(rect);
        let range = level.range(DeviceDirection::Output);
        let t = (level.value(state) - range.min) / (range.max - range.min);
        pos2(track.left() + track.width() * t, rect.center().y)
    }

    #[test]
    fn a_press_on_a_level_sliders_thumb_moves_nothing_until_the_pointer_does() {
        // 0.4.0 audit #14 holds for the levels too. JUCE jumps to the pointer even on the thumb,
        // and the thumb's eight-point radius is more than half a step of the master gain, the
        // leveller and the balance: a touch off its centre moved them a whole step.
        let mut harness = turned_over(ThemeMode::Dark);
        let state = speakers();
        harness.settle(&state);
        let off_centre = 7.5;
        for (row, &level) in Level::OUTPUT.iter().enumerate() {
            let at = level_thumb(&state, row, level) + vec2(off_centre, 0.0);
            let range = level.range(DeviceDirection::Output);
            let jumped = slider::quantise(
                level.value(&state)
                    + off_centre / slider::track_rect(slider_rect(column(), row)).width()
                        * (range.max - range.min),
                range.min,
                range.max,
                range.step,
            );
            if level != Level::FilterQ {
                // What the jump would have set, so the test would catch its return.
                assert_ne!(jumped, level.value(&state), "{level:?}");
            }
            let actions = press(&mut harness, &state, at, PointerButton::Primary);
            assert!(
                actions.is_empty(),
                "{level:?}: a touch moved it: {actions:?}"
            );
        }
    }

    #[test]
    fn a_master_gain_between_its_steps_survives_a_touch_and_a_drag_from_the_thumb_still_steps() {
        // A gain between two of the slider's whole-decibel positions, from a settings file
        // written by hand, used to be snapped by a touch on the thumb (0.4.0 audit #14).
        let mut harness = turned_over(ThemeMode::Dark);
        let state = UiState {
            master_gain_db: 3.5,
            ..speakers()
        };
        harness.settle(&state);
        let from = level_thumb(&state, 0, Level::MasterGain);
        let actions = press(&mut harness, &state, from, PointerButton::Primary);
        assert!(actions.is_empty(), "a touch moved it: {actions:?}");

        // Thirty points right is 10.7 dB up: 14.2 dB, which the drag puts on its 14 dB step.
        let to = from + vec2(30.0, 0.0);
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        };
        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(from)],
            vec![Event::PointerMoved(from), button(from, true)],
            vec![Event::PointerMoved(to)],
            vec![button(to, false)],
        ] {
            actions.extend(harness.frame(&state, events).0);
        }
        assert_eq!(actions, vec![UiAction::SetMasterGain(14.0)]);
    }

    #[test]
    fn an_arrow_on_a_master_gain_between_its_steps_stops_at_the_next_step_either_way() {
        // 3.5 dB sits between two positions. A whole step added and then rounded took Right to
        // 5 dB and skipped 4 dB.
        let state = UiState {
            master_gain_db: 3.5,
            ..speakers()
        };
        let arrow = |key| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        for (key, expected) in [(egui::Key::ArrowRight, 4.0), (egui::Key::ArrowLeft, 3.0)] {
            let mut harness = turned_over(ThemeMode::Dark);
            harness.settle(&state);
            harness.ctx.memory_mut(|m| {
                m.request_focus(
                    egui::Id::new("fx_slider").with(("fx_level", Level::MasterGain as u8)),
                );
            });
            harness.frame(&state, Vec::new());
            let (actions, _) = harness.frame(&state, vec![arrow(key)]);
            assert_eq!(actions, vec![UiAction::SetMasterGain(expected)], "{key:?}");
        }
    }

    #[test]
    fn an_arrow_on_a_master_gain_on_a_step_moves_a_whole_step() {
        let state = speakers();
        let arrow = |key| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        for (key, expected) in [(egui::Key::ArrowRight, -3.0), (egui::Key::ArrowLeft, -5.0)] {
            let mut harness = turned_over(ThemeMode::Dark);
            harness.settle(&state);
            harness.ctx.memory_mut(|m| {
                m.request_focus(
                    egui::Id::new("fx_slider").with(("fx_level", Level::MasterGain as u8)),
                );
            });
            harness.frame(&state, Vec::new());
            let (actions, _) = harness.frame(&state, vec![arrow(key)]);
            assert_eq!(actions, vec![UiAction::SetMasterGain(expected)], "{key:?}");
        }
    }

    #[test]
    fn picking_a_band_count_asks_for_it() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = speakers();
        harness.settle(&state);
        let combo = band_combo(column());
        harness.click(&state, combo.center());
        harness.frame(&state, Vec::new());
        let (_, shapes) = harness.frame(&state, Vec::new());
        let offered: Vec<String> = texts(&shapes)
            .into_iter()
            .filter(|(text, rect, _)| text.ends_with(" Bands") && rect.top() > combo.bottom() - 1.0)
            .map(|(text, _, _)| text)
            .collect();
        assert_eq!(
            offered,
            ["5 Bands", "10 Bands", "15 Bands", "20 Bands", "31 Bands"]
        );
        let rows = text_below(&shapes, "31 Bands", combo.bottom() - 1.0);
        let actions = harness.click(&state, rows[0].center());
        assert_eq!(actions, vec![UiAction::SetBandCount(31)]);
    }

    #[test]
    fn picking_the_band_count_already_in_use_asks_for_nothing() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = speakers();
        harness.settle(&state);
        let combo = band_combo(column());
        harness.click(&state, combo.center());
        harness.frame(&state, Vec::new());
        let (_, shapes) = harness.frame(&state, Vec::new());
        let rows = text_below(&shapes, "10 Bands", combo.bottom() - 1.0);
        assert_eq!(rows.len(), 1);
        assert!(harness.click(&state, rows[0].center()).is_empty());
    }

    #[test]
    fn a_band_count_none_of_the_rows_name_is_shown_as_it_is() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = UiState {
            eq_bands: (0..7)
                .map(|band| fxsound_core::EqBand::new(100.0 * (band + 1) as f32, 0.0))
                .collect(),
            ..speakers()
        };
        let shapes = harness.settle(&state);
        let (text, _) = painted(&shapes, "7 Bands");
        assert!(band_combo(column()).contains(text.center()));
    }

    #[test]
    fn clicking_restore_defaults_asks_the_controller_for_it() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = speakers();
        harness.settle(&state);
        let actions = harness.click(&state, restore_defaults(column()).center());
        assert_eq!(actions, vec![UiAction::RestoreDefaults]);
    }

    #[test]
    fn with_the_power_off_nothing_on_the_card_answers_and_the_readouts_fade() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let mut harness = turned_over(mode);
            let state = UiState {
                power: false,
                ..speakers()
            };
            let shapes = harness.settle(&state);
            assert_eq!(
                painted(&shapes, "-4 dB").1,
                Palette::new(mode).color_alpha(FxColor::HighlightedText, DISABLED_TEXT_ALPHA)
            );
            let column = column();
            let mut actions = harness.click(&state, restore_defaults(column).center());
            actions.extend(harness.click(&state, flip_button(column).center()));
            actions.extend(harness.click(&state, band_combo(column).center()));
            let track = slider::track_rect(slider_rect(column, 0)).center();
            actions.extend(press(&mut harness, &state, track, PointerButton::Primary));
            actions.extend(press(&mut harness, &state, track, PointerButton::Secondary));
            assert!(actions.is_empty(), "{mode:?}: {actions:?}");
            assert_eq!(harness.scratch.column_face, ColumnFace::EqualizerControls);
        }
    }

    #[test]
    fn with_the_equalizer_off_the_levels_it_takes_are_grey_but_still_answer() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = UiState {
            eq_on: false,
            ..speakers()
        };
        let shapes = harness.settle(&state);
        // The leveller (row 1) and the width (row 2) go with the equalizer; the master gain
        // (row 0) stays in the path (0.4.0 audit R3) and keeps its colour.
        for row in 1..3 {
            let track = slider::track_rect(slider_rect(column(), row));
            let fill = fill_at(&shapes, track).expect("an unfilled track");
            assert!(is_grey(fill), "row {row}: {fill:?}");
        }
        let gain = fill_at(&shapes, slider::track_rect(slider_rect(column(), 0))).unwrap();
        assert!(!is_grey(gain), "{gain:?}");
        let target = slider::track_rect(slider_rect(column(), 1)).center();
        let actions = press(&mut harness, &state, target, PointerButton::Primary);
        assert_eq!(actions, vec![UiAction::SetVolumeLeveling(2.0)]);

        // On a voice the makeup gain is a stage of its own and keeps its colour.
        let mut harness = turned_over(ThemeMode::Dark);
        let voice = UiState {
            eq_on: false,
            ..microphone()
        };
        let shapes = harness.settle(&voice);
        let makeup = fill_at(&shapes, slider::track_rect(slider_rect(column(), 0))).unwrap();
        let width = fill_at(&shapes, slider::track_rect(slider_rect(column(), 1))).unwrap();
        assert!(!is_grey(makeup), "{makeup:?}");
        assert!(is_grey(width), "{width:?}");
    }

    /// The right edge of the fill painted from `track`'s start over it, if one was: the one
    /// rectangle that starts where the track does and is not the track.
    fn fill_right(shapes: &[ClippedShape], track: Rect) -> Option<f32> {
        shapes.iter().find_map(|clipped| match &clipped.shape {
            Shape::Rect(shape)
                if (shape.rect.min - track.min).length() < 1e-3
                    && (shape.rect.height() - track.height()).abs() < 1e-3
                    && (shape.rect.width() - track.width()).abs() > 1e-3 =>
            {
                Some(shape.rect.right())
            }
            _ => None,
        })
    }

    /// Where the balance bar's gradient ends, and how many vertices the bar has.
    fn balance_end(shapes: &[ClippedShape], track: Rect) -> (f32, usize) {
        shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                Shape::Mesh(mesh)
                    if mesh.texture_id == egui::TextureId::default()
                        && mesh
                            .vertices
                            .first()
                            .is_some_and(|v| v.pos == track.left_top()) =>
                {
                    Some((mesh.vertices[1].pos.x, mesh.vertices.len()))
                }
                _ => None,
            })
            .expect("the balance bar")
    }

    #[test]
    fn at_interface_the_level_sliders_paint_as_windows_does_and_off_they_paint_corrected() {
        // 0.4.0 audit #41 set back: `drawLinearSlider` takes the thumb's x for the fill's width,
        // and `FxBalanceSlider::paint` ends its gradient a track's width in from the slider's left
        // edge and holds the end colour after it (`slider::Fidelity::Faithful`).
        let windows = UiState {
            windows_parity: WindowsParity::Interface,
            ..speakers()
        };
        let gain = slider_rect(column(), 0);
        let gain_track = slider::track_rect(gain);
        let range = Level::MasterGain.range_in(&windows);
        let t = (windows.master_gain_db - range.min) / (range.max - range.min);
        let thumb_x = gain_track.left() + gain_track.width() * t;
        let balance = slider_rect(column(), Level::BALANCE_ROW);
        let balance_track = slider::track_rect(balance);

        let mut harness = turned_over(ThemeMode::Dark);
        let shapes = harness.settle(&speakers());
        let off = fill_right(&shapes, gain_track).expect("Off: the gain's fill");
        assert!((off - thumb_x).abs() < 1e-3, "Off: {off} against {thumb_x}");
        assert_eq!(
            balance_end(&shapes, balance_track),
            (balance_track.right(), 4)
        );

        let mut harness = turned_over(ThemeMode::Dark);
        let shapes = harness.settle(&windows);
        let faithful = fill_right(&shapes, gain_track).expect("Interface: the gain's fill");
        let want = gain_track.left() + (thumb_x - gain.left());
        assert!(
            (faithful - want).abs() < 1e-3,
            "Interface: {faithful} against {want}"
        );
        assert!(
            faithful > off,
            "the thumb's x as a width runs past the thumb"
        );
        assert_eq!(
            balance_end(&shapes, balance_track),
            (balance.left() + balance_track.width(), 8),
            "a track's width in from the slider's edge, and the end colour after it"
        );
    }

    #[test]
    fn the_balance_is_one_bar_fading_toward_the_side_it_leans_away_from() {
        // FxBalanceSlider::paint: no fill split; at -6 dB (t = 0.35) the left end is the stronger.
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let mut harness = turned_over(mode);
            let shapes = harness.settle(&speakers());
            let track = slider::track_rect(slider_rect(column(), Level::BALANCE_ROW));
            let mesh = shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    Shape::Mesh(mesh)
                        if mesh.texture_id == egui::TextureId::default()
                            && mesh
                                .vertices
                                .first()
                                .is_some_and(|v| v.pos == track.left_top()) =>
                    {
                        Some(mesh.clone())
                    }
                    _ => None,
                })
                .expect("the balance bar");
            let left = mesh.vertices[0].color;
            let end = mesh.vertices[1];
            let (want_left, want_right) = slider::balance_colours(Palette::new(mode), 0.35, true);
            assert_eq!(left, want_left, "{mode:?}");
            assert_eq!(end.color, want_right, "{mode:?}");
            assert!(left.a() > end.color.a());
            // The gradient runs to the track's end, where the original's ended eight points early
            // and held its end colour after (D-3, 0.4.0 audit #41): one quad, end to end.
            assert_eq!(end.pos.x, track.right());
            assert_eq!(mesh.vertices.len(), 4, "{:?}", mesh.vertices);
            // And no 20 % track under it: the bar is the whole track.
            assert_eq!(fill_at(&shapes, track), None);
        }
    }

    #[test]
    fn at_every_levels_maximum_the_readout_stays_on_the_card() {
        let mut harness = turned_over(ThemeMode::Dark);
        let state = UiState {
            master_gain_db: 20.0,
            volume_leveling: 4.0,
            filter_q: 3.0,
            balance_db: -20.0,
            ..speakers()
        };
        let shapes = harness.settle(&state);
        let readouts: Vec<_> = texts(&shapes)
            .into_iter()
            .filter(|(text, _, _)| ["20 dB", "4.0", "3.0x"].contains(&text.as_str()))
            .collect();
        // The gain and the balance both read "20 dB".
        assert_eq!(readouts.len(), 4, "{readouts:?}");
        for (text, rect, _) in readouts {
            assert!(rect.right() <= column().right(), "{text:?}: {rect:?}");
        }
    }

    #[test]
    fn nothing_on_the_turned_over_face_reports_anything_while_left_alone() {
        let mut harness = turned_over(ThemeMode::Light);
        for state in [speakers(), microphone()] {
            for _ in 0..3 {
                let (actions, _) = harness.frame(&state, Vec::new());
                assert!(actions.is_empty(), "{actions:?}");
            }
        }
    }

    #[test]
    fn a_new_window_opens_on_the_effects() {
        assert_eq!(ViewScratch::new().column_face, ColumnFace::Effects);
        assert_eq!(ColumnFace::Effects.flipped(), ColumnFace::EqualizerControls);
        assert_eq!(ColumnFace::EqualizerControls.flipped(), ColumnFace::Effects);
    }

    // ---- every language ------------------------------------------------------------------------

    #[test]
    fn every_language_fits_the_captions_the_band_counts_and_the_two_sides() {
        use fxsound_core::i18n::{Catalogue, LANGUAGES};
        let harness = Harness::new(ThemeMode::Dark);
        let column = column();
        // The captions may use the whole of the original's 160-point box, which overhangs the
        // card by the eight points the thumb's radius moves it in; the combo's text has the box
        // `FxTheme::positionComboBoxText` leaves it; Left and Right must not meet.
        let caption_room = caption_rect(column, 0).width();
        let combo_room = crate::widgets::combo::text_box(band_combo(column)).width();
        let sides_room = right_label(column).right() - left_label(column).left();
        let mut problems = Vec::new();
        let screen = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, layout::pro::WINDOW_SIZE)),
            ..Default::default()
        };
        harness
            .ctx
            .run_ui(screen, |ui| {
                let width = |text: &str, px: f32| {
                    ui.painter()
                        .layout_no_wrap(text.to_owned(), caption_font(px), Color32::PLACEHOLDER)
                        .size()
                        .x
                };
                for language in &LANGUAGES {
                    let table = Catalogue::for_language(language);
                    let tr = |key: &str| table.get(key).unwrap_or(key).to_owned();
                    for key in [
                        "Master Gain",
                        "Makeup Gain",
                        "Volume Leveling",
                        "Filter Q",
                        "Balance",
                    ] {
                        let text = tr(key);
                        let w = width(&text, CAPTION_FONT_PX);
                        if w > caption_room {
                            problems.push(format!(
                                "{}: {text:?} is {w:.0} in {caption_room}",
                                language.code
                            ));
                        }
                    }
                    let widest = format!("31{}", tr(" Bands"));
                    let w = width(&widest, crate::widgets::combo::SMALL_FONT);
                    if w > combo_room {
                        problems.push(format!(
                            "{}: {widest:?} is {w:.0} in {combo_room}",
                            language.code
                        ));
                    }
                    let sides =
                        width(&tr("Left"), SIDE_FONT_PX) + width(&tr("Right"), SIDE_FONT_PX);
                    if sides + 4.0 > sides_room {
                        problems.push(format!(
                            "{}: Left and Right are {sides:.0} in {sides_room}",
                            language.code
                        ));
                    }
                }
            })
            .drop_without_applying_deltas();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
