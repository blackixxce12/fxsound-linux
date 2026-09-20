//! The themed combo box.
//!
//! Three drop-downs in FxSound share one control: the preset list, the playback-device list and
//! the EQ band-count list. All three are `FxComboBox`, a thin `juce::ComboBox` subclass
//! (`fxsound/Source/GUI/FxComboBox.cpp:24-83`) whose entire appearance comes from
//! `FxTheme::drawComboBox` (`fxsound/Source/GUI/FxTheme.cpp:135-164`) and
//! `FxTheme::positionComboBoxText` (`FxTheme.cpp:128-133`). egui's own
//! [`egui::ComboBox`] paints from `Visuals`, which cannot express the `height / 5` corner, the
//! 37-point right gutter or the SVG arrow, so the control is rebuilt here the same way every other
//! widget in this crate is: absolute rectangles and a [`egui::Painter`].
//!
//! ## What the original actually is
//!
//! `FxComboBox` itself only adds four things (`FxComboBox.h:26-44`): a pointing-hand cursor, a
//! [`FxComboBox::error`] outline for an unavailable playback device, a hover highlight driven from
//! the *view's* `mouseEnter` / `mouseExit` (`FxView.cpp:258-277`), and an `onShowPopup` hook the
//! device list uses to rescan devices before the menu opens. The hook is the caller's business —
//! rebuild `items` before calling [`FxComboBox::show`] and the same thing happens.
//!
//! ## The preset list's `*`
//!
//! `FxView::modelChanged` builds the preset items as `preset.modified ? name + " *" : name`
//! (`FxView.cpp:205-225`, `docs/spec/03-controls.md` §8.4), so the marker is part of the *label*,
//! not of the widget. [`preset_label`] is that one line, kept here so the marker is written once.
//! Item ids in the original are `index + 1` because JUCE reserves id 0 for "nothing selected";
//! [`FxComboBox::new`] takes an `Option<usize>` index instead, which is the same information
//! without the off-by-one.
//!
//! ## Save and undo
//!
//! There are deliberately **no save or undo affordances next to the box** (`docs/spec/03-controls.md`
//! §8.5). `Save New Preset`, `Overwrite Existing Preset` and `Undo Preset Changes` all live in the
//! hamburger menu, each with its own enable predicate (`FxMainWindow.cpp:509-551`), and the only
//! trace of them in the combo is the `*` that [`preset_label`] appends while a preset is modified.
//! A widget that grew its own save button would put the control in two places at once.
//!
//! ## Section headers **(port addition)**
//!
//! The Windows build's device list holds playback endpoints only; the PipeWire port lists capture
//! devices in the same box, so the user can put FxSound in front of a microphone as easily as in
//! front of the speakers. [`FxComboBox::headers`] draws a [`SectionHeader`] above the first item
//! of each run — `Output`, `Input`, from `DeviceDirection::label()` — as a non-interactive row:
//! smaller semibold type in `MenuText` at reduced alpha, at the same x as the item text, about
//! seven tenths of an item tall. A header never highlights, never reports a click and does not
//! close the menu, but it does count toward the menu's width like any other line. Headers are
//! pure decoration for the caller too: item `i` stays item `i`, so `UiAction::SelectDevice(i)`
//! indexes the state's list unchanged whether or not any header is drawn.
//!
//! ## Deviations from the original
//!
//! * **The popup opens below the box**, using [`egui::Popup`]'s own flip-when-it-does-not-fit
//!   logic. JUCE positions a `ComboBox` menu so that the *selected* row covers the box, which on a
//!   120-entry preset list means the menu opens under the pointer with the list scrolled to the
//!   selection. Reproducing that needs a scroll offset egui's `Popup` does not expose.
//! * **Long lists scroll with a scrollbar**, not with JUCE's top/bottom scroll arrows. The menu
//!   therefore closes on a click *outside* it or on a picked item, not on any click at all: a
//!   click on the scrollbar, a separator or a section header leaves it open, which is also what a
//!   JUCE `PopupMenu` does with its non-item rows.
//! * **No tick glyph.** `LookAndFeel_V4::drawPopupMenuItem` draws a check mark in the icon column
//!   of a ticked row. FxTheme already marks that row twice — the forced highlight background and
//!   the extra outline (`FxTheme.cpp:353-365`) — so the glyph is left out while its column is
//!   still reserved, which is what keeps the row text at the original's x.
//! * **No disabled items.** `FxComboBox::highlightText` returns early with the default text colour
//!   when the selected item is disabled (`FxComboBox.cpp:36-54`); the only disabled item in the
//!   app is an unavailable playback device, which this port reports through
//!   [`FxComboBox::error`] instead.
//!
//! JUCE is not vendored in this tree, so everything that comes from `LookAndFeel_V4` rather than
//! from FxSound's own sources is marked **[JUCE semantics]** below and, where it is a number,
//! exposed as a builder ([`FxComboBox::row_height`]) so a pixel diff against the Windows build can
//! settle it without touching this file.

use crate::assets::{AssetCache, FxImage};
use crate::theme::{self, FxColor, Palette};
use crate::widgets::icon_button::{art_size, fitted_rect};
use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Color32, CornerRadius, CursorIcon, FontId, Id, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};

/// Width of the arrow's box, `Rectangle<float>(width - margin, 0, 12, height)` (`FxTheme.cpp:161`).
pub const ARROW_WIDTH: f32 = 12.0;
/// Distance from the right edge to the arrow box on a wide combo (`FxTheme.cpp:156`).
pub const ARROW_MARGIN: f32 = 32.0;
/// The same distance on a combo no wider than [`NARROW_WIDTH`] (`FxTheme.cpp:157-158`).
pub const NARROW_ARROW_MARGIN: f32 = 24.0;
/// The width at or below which the arrow moves in (`FxTheme.cpp:157`).
pub const NARROW_WIDTH: f32 = 150.0;
/// Left edge of the text, `label.getBounds().withX(5)` (`FxTheme.cpp:132`).
pub const TEXT_LEFT: f32 = 5.0;
/// How far the text stops short of the right edge, `withRight(box.getWidth() - 37)`
/// (`FxTheme.cpp:132`) — wide enough to clear the arrow whichever margin is in force.
pub const TEXT_RIGHT_INSET: f32 = 37.0;
/// `positionComboBoxText` puts the label at y = 1 with height − 2 **[JUCE semantics]**, which is
/// also the height JUCE hands the popup as its standard item height.
pub const TEXT_Y_INSET: f32 = 1.0;
/// Left edge of the placeholder, which is *not* the text's left edge (`FxTheme.cpp:176`).
pub const PLACEHOLDER_LEFT: f32 = 10.0;
/// `withMultipliedAlpha(0.5f)` on the placeholder (`FxTheme.cpp:168`).
pub const PLACEHOLDER_ALPHA: f32 = 0.5;
/// `getComboBoxFont` below the height threshold (`FxTheme.cpp:120-126`).
pub const SMALL_FONT: f32 = 14.0;
/// `getComboBoxFont` above it.
pub const FONT: f32 = 17.0;
/// The height at or below which the combo font drops to [`SMALL_FONT`] (`FxTheme.cpp:122`).
pub const SMALL_FONT_MAX_HEIGHT: f32 = 30.0;
/// `focusedOutlineColourId` is `SliderHighlight` at this alpha (`FxTheme.cpp:76`).
pub const FOCUS_OUTLINE_ALPHA: f32 = 0.2;
/// `getPopupMenuFont()` (`FxTheme.cpp:367-370`).
pub const POPUP_FONT: f32 = 17.0;
/// `LookAndFeel_V4` divides a row's height by this to get the biggest font it will fit, and rounds
/// the same number to get the width of the icon column **[JUCE semantics]**.
pub const POPUP_FONT_RATIO: f32 = 1.3;
/// `drawPopupMenuBackground` strokes the menu's edge in the text colour at this alpha
/// **[JUCE semantics]**.
pub const POPUP_BORDER_ALPHA: f32 = 0.6;
/// A separator is a one-point line at this alpha **[JUCE semantics]**.
pub const SEPARATOR_ALPHA: f32 = 0.3;
/// …inset by this much on both ends **[JUCE semantics]**.
pub const SEPARATOR_INSET: f32 = 5.0;
/// What `" *"` marks: a preset with unsaved changes (`FxView.cpp:213`).
pub const MODIFIED_SUFFIX: &str = " *";
/// A section header is this fraction of an item row tall **(port addition)** — see the module
/// docs; the original has no headers to measure against.
pub const HEADER_HEIGHT_RATIO: f32 = 0.7;
/// A section header's title is the item font scaled by this **(port addition)**.
pub const HEADER_FONT_RATIO: f32 = 0.75;
/// A section header's title is `MenuText` at this alpha **(port addition)**: legible, but clearly
/// not one of the things that can be picked.
pub const HEADER_TEXT_ALPHA: f32 = 0.6;

/// `cornerSize = (float) height / 5` (`FxTheme.cpp:138`).
///
/// The division is by an `int` height, so the 40-point Pro combos get 8, the 50-point Lite ones 10
/// and the 20-point EQ band list 4 (`docs/spec/03-controls.md` §8.2).
#[must_use]
pub fn corner_radius(height: f32) -> f32 {
    height / 5.0
}

/// `getComboBoxFont`: 14 px at or below 30 points tall, 17 px above (`FxTheme.cpp:120-126`).
#[must_use]
pub fn font_size(height: f32) -> f32 {
    if height <= SMALL_FONT_MAX_HEIGHT {
        SMALL_FONT
    } else {
        FONT
    }
}

/// The 12 × height box the arrow SVG is fitted into (`FxTheme.cpp:156-163`).
#[must_use]
pub fn arrow_box(rect: Rect) -> Rect {
    let margin = if rect.width() <= NARROW_WIDTH {
        NARROW_ARROW_MARGIN
    } else {
        ARROW_MARGIN
    };
    Rect::from_min_size(
        pos2(rect.right() - margin, rect.top()),
        vec2(ARROW_WIDTH, rect.height()),
    )
}

/// The label's rectangle: `(5, 1, width - 42, height - 2)` (`FxTheme.cpp:128-133`).
///
/// `setMinimumHorizontalScale(1.0)` on the same line forbids JUCE's usual squeeze-to-fit, so a
/// name too long for this box is truncated with an ellipsis instead of being condensed.
#[must_use]
pub fn text_box(rect: Rect) -> Rect {
    Rect::from_min_max(
        pos2(rect.left() + TEXT_LEFT, rect.top() + TEXT_Y_INSET),
        pos2(
            rect.right() - TEXT_RIGHT_INSET,
            rect.bottom() - TEXT_Y_INSET,
        ),
    )
}

/// The height of one popup row.
///
/// `ComboBox::showPopup` passes `withStandardItemHeight(label->getHeight())` **[JUCE semantics]**,
/// and the label is the combo minus one point top and bottom, so the menu's rows are exactly as
/// tall as the closed box's text area: 38 for the Pro combos, 18 for the EQ band list.
#[must_use]
pub fn popup_row_height(combo_height: f32) -> f32 {
    (combo_height - TEXT_Y_INSET * 2.0).max(1.0)
}

/// The height of a separator row: `standardMenuItemHeight / 10`, in **integer** division
/// **[JUCE semantics]** — 3 points under a 38-point row.
#[must_use]
pub fn popup_separator_height(row_height: f32) -> f32 {
    ((row_height as i32) / 10).max(1) as f32
}

/// The font a popup row draws its text in.
///
/// `drawPopupMenuItem` shrinks `getPopupMenuFont()` to `(row height - 2) / 1.3` when 17 px would
/// not fit **[JUCE semantics]**, which is why the 18-point EQ band menu is set in ~12 px while
/// every other menu in the app is set in 17.
#[must_use]
pub fn popup_font_size(row_height: f32) -> f32 {
    POPUP_FONT.min(max_popup_font_height(row_height))
}

/// Left edge of a popup row's text, measured from the row: one point of item inset plus the icon
/// column, which is `round((row height - 2) / 1.3)` wide whether or not anything is drawn in it
/// **[JUCE semantics]**.
#[must_use]
pub fn popup_text_x(row_height: f32) -> f32 {
    1.0 + max_popup_font_height(row_height).round()
}

/// How wide a popup row wants to be: its text plus two row heights **[JUCE semantics]**, the icon
/// column and the right gutter `getIdealPopupMenuItemSizeWithOptions` reserves.
#[must_use]
pub fn ideal_item_width(text_width: f32, row_height: f32) -> f32 {
    text_width + row_height * 2.0
}

/// The height of a section header row **(port addition)**: [`HEADER_HEIGHT_RATIO`] of an item,
/// rounded to whole points so the item rows under it stay on the pixel grid — 27 under a 38-point
/// row, 34 under the Lite view's 48.
#[must_use]
pub fn popup_header_height(row_height: f32) -> f32 {
    (row_height * HEADER_HEIGHT_RATIO).round().max(1.0)
}

/// The font a section header's title is set in **(port addition)**: the item font at
/// [`HEADER_FONT_RATIO`], so it shrinks with the items in a short-rowed menu.
#[must_use]
pub fn popup_header_font_size(row_height: f32) -> f32 {
    popup_font_size(row_height) * HEADER_FONT_RATIO
}

/// `(row height - 2) / 1.3`, the one quantity both the popup font and the icon column derive from.
fn max_popup_font_height(row_height: f32) -> f32 {
    (row_height - 2.0).max(1.0) / POPUP_FONT_RATIO
}

/// A preset's combo label: its name, plus `" *"` while it has unsaved changes
/// (`FxView.cpp:213`, `FxSystemTrayView.cpp:231`).
#[must_use]
pub fn preset_label(name: &str, modified: bool) -> String {
    if modified {
        format!("{name}{MODIFIED_SUFFIX}")
    } else {
        name.to_owned()
    }
}

/// The outline `drawComboBox` strokes around the box (`FxTheme.cpp:145-153`).
///
/// Keyboard focus wins, as it does in the original. Otherwise the colour is whatever
/// `outlineColourId` currently holds, and that is a three-state, not a two-state:
/// `FxTheme::init` leaves it at `ComboBoxBackground` — invisible against the fill — while
/// `FxComboBox::setError` overwrites it with `SliderTrack` on error and `DefaultFill` off it
/// (`FxComboBox.cpp:61-73`). `DefaultFill` is white in the light palette, so a combo that has ever
/// reported an error keeps a visible white hairline afterwards; only a combo that never calls
/// `setError` — the preset list and the band list — stays outline-less.
#[must_use]
pub fn outline_colour(palette: Palette, error: Option<bool>, focused: bool) -> Color32 {
    if focused {
        return palette.color_alpha(FxColor::SliderHighlight, FOCUS_OUTLINE_ALPHA);
    }
    match error {
        Some(true) => palette.color(FxColor::SliderTrack),
        Some(false) => palette.color(FxColor::DefaultFill),
        None => palette.color(FxColor::ComboBoxBackground),
    }
}

/// The closed box's text colour (`FxComboBox::highlightText`, `FxComboBox.cpp:36-54`).
///
/// The original flips to `highlightedText` from the *view's* mouse handlers
/// (`FxView.cpp:258-277`), which in practice means "while the pointer is over the combo" — so the
/// hover test lives here instead. A disabled box never highlights.
#[must_use]
pub fn text_colour(palette: Palette, enabled: bool, hovered: bool) -> Color32 {
    if enabled && hovered {
        palette.color(FxColor::HighlightedText)
    } else {
        palette.color(FxColor::DefaultText)
    }
}

/// A section title drawn above one item **(port addition)** — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionHeader<'a> {
    /// The index of the first item under the title.
    pub before: usize,
    /// The title, e.g. `DeviceDirection::label()`.
    pub label: &'a str,
}

impl<'a> SectionHeader<'a> {
    #[must_use]
    pub const fn new(before: usize, label: &'a str) -> Self {
        Self { before, label }
    }
}

/// A drop-down drawn like FxSound's.
pub struct FxComboBox<'a> {
    items: &'a [String],
    selected: Option<usize>,
    enabled: bool,
    placeholder: &'a str,
    error: Option<bool>,
    separator_before: Option<usize>,
    headers: &'a [SectionHeader<'a>],
    row_height: Option<f32>,
}

impl<'a> FxComboBox<'a> {
    /// `items` are already-formatted labels; `selected` indexes them.
    ///
    /// Formatted, because the original formats them too: the preset list appends [`preset_label`]'s
    /// `*`, and the band list spells its entries `"<n> Bands"` (`FxAudioControls.cpp:301`).
    #[must_use]
    pub fn new(items: &'a [String], selected: Option<usize>) -> Self {
        Self {
            items,
            selected,
            enabled: true,
            placeholder: "",
            error: None,
            separator_before: None,
            headers: &[],
            row_height: None,
        }
    }

    /// A disabled box shows the grey arrow and cannot be opened (`FxTheme.cpp:159-163`).
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// What to draw when nothing is selected (`FxTheme.cpp:166-179`).
    ///
    /// Both of the app's own lists set this to the empty string (`FxView.cpp:86`, `:95`), so an
    /// empty preset list really does render as an empty box.
    #[must_use]
    pub fn placeholder(mut self, text: &'a str) -> Self {
        self.placeholder = text;
        self
    }

    /// Report the playback device as unavailable, or as available again
    /// (`FxComboBox::setError`, `FxComboBox.cpp:61-73`).
    ///
    /// Leaving this unset is *not* the same as `error(false)` — see [`outline_colour`].
    #[must_use]
    pub fn error(mut self, error: bool) -> Self {
        self.error = Some(error);
        self
    }

    /// Draw a separator above item `index`.
    ///
    /// The preset list emits exactly one, at the factory→user boundary
    /// (`FxView.cpp:205-225`, `docs/spec/03-controls.md` §8.4 rule 3).
    #[must_use]
    pub fn separator_before(mut self, index: Option<usize>) -> Self {
        self.separator_before = index;
        self
    }

    /// Draw a section title above each `header.before` item **(port addition)**.
    ///
    /// The device list passes one per direction it holds, so a list of playback devices alone
    /// is titled `Output` and nothing else. A header whose index is past the end of `items` is
    /// never drawn; one that shares its index with [`FxComboBox::separator_before`] is drawn
    /// under the rule.
    #[must_use]
    pub fn headers(mut self, headers: &'a [SectionHeader<'a>]) -> Self {
        self.headers = headers;
        self
    }

    /// Override the popup's row height, which otherwise follows [`popup_row_height`].
    ///
    /// The default is derived from JUCE rather than from FxSound's own sources; this is the escape
    /// hatch for correcting it against a screenshot.
    #[must_use]
    pub fn row_height(mut self, height: f32) -> Self {
        self.row_height = Some(height);
        self
    }

    /// Draws the closed box and, when open, the popup. Returns the index the user picked.
    pub fn show(
        self,
        ui: &mut Ui,
        rect: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> (Response, Option<usize>) {
        let Self {
            items,
            selected,
            enabled,
            placeholder,
            error,
            separator_before,
            headers,
            row_height,
        } = self;

        let id = Id::new("fx_combo_box").with(id_salt);
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let response = ui.interact(rect, id, sense);
        if enabled && response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }

        paint_box(
            ui,
            rect,
            palette,
            assets,
            items,
            selected,
            enabled,
            placeholder,
            error,
            &response,
        );

        if !enabled {
            return (response, None);
        }

        let row_h = row_height.unwrap_or_else(|| popup_row_height(rect.height()));
        let picked = popup(
            ui,
            rect,
            palette,
            items,
            selected,
            separator_before,
            headers,
            row_h,
            &response,
        );
        (response, picked)
    }
}

/// Everything `FxTheme::drawComboBox` puts on screen, in its order (`FxTheme.cpp:135-164`).
#[allow(clippy::too_many_arguments)]
fn paint_box(
    ui: &Ui,
    rect: Rect,
    palette: Palette,
    assets: &mut AssetCache,
    items: &[String],
    selected: Option<usize>,
    enabled: bool,
    placeholder: &str,
    error: Option<bool>,
    response: &Response,
) {
    let painter = ui.painter().clone();
    let corner = CornerRadius::same(corner_radius(rect.height()) as u8);

    painter.rect_filled(rect, corner, palette.color(FxColor::ComboBoxBackground));
    // `drawRoundedRectangle(bounds.reduced(0.5f), cornerSize, 1.0f)` centres a one-point stroke on
    // a path half a point inside the box, i.e. it paints the box's own outermost point.
    painter.rect_stroke(
        rect,
        corner,
        Stroke::new(1.0, outline_colour(palette, error, response.has_focus())),
        StrokeKind::Inside,
    );

    let font = theme::semibold(font_size(rect.height()));
    let text = selected.and_then(|index| items.get(index));
    let (label, colour, left) = match text {
        Some(label) => (
            label.as_str(),
            text_colour(palette, enabled, response.hovered()),
            rect.left() + TEXT_LEFT,
        ),
        // `drawComboBoxTextWhenNothingSelected` starts five points further in and halves the alpha.
        None => (
            placeholder,
            palette.color_alpha(FxColor::DefaultText, PLACEHOLDER_ALPHA),
            rect.left() + PLACEHOLDER_LEFT,
        ),
    };
    if !label.is_empty() {
        let box_ = text_box(rect);
        draw_truncated(
            &painter,
            label,
            font,
            colour,
            left,
            box_.right(),
            box_.center().y,
        );
    }

    // The arrow is a `Drawable`, and `drawWithin` ignores the colour set on the graphics context,
    // so `arrowColourId`'s alpha never reaches the artwork: the only thing `isEnabled()` changes
    // is which of the two SVGs is used (`FxTheme.cpp:159-163`).
    let image = if enabled {
        FxImage::DropDownArrowHover
    } else {
        FxImage::DropDownArrow
    };
    let art = fitted_rect(arrow_box(rect), art_size(image));
    if let Some(texture) = assets.texture(ui.ctx(), image, palette.mode(), art.size()) {
        painter.image(
            texture.id(),
            art,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

/// The drop-down menu (`FxTheme::drawPopupMenuItem`, `FxTheme.cpp:353-365`, and
/// `docs/spec/03-controls.md` §8.3).
#[allow(clippy::too_many_arguments)]
fn popup(
    ui: &mut Ui,
    rect: Rect,
    palette: Palette,
    items: &[String],
    selected: Option<usize>,
    separator_before: Option<usize>,
    headers: &[SectionHeader<'_>],
    row_height: f32,
    response: &Response,
) -> Option<usize> {
    let popup_id = egui::Popup::default_response_id(response);
    let opening = egui::Popup::is_id_open(ui.ctx(), popup_id) || response.clicked();

    let font = theme::semibold(popup_font_size(row_height));
    let header_font = theme::semibold(popup_header_font_size(row_height));
    // `withMinimumWidth(getWidth())`: the menu is at least as wide as the box, and wider if an
    // item needs it **[JUCE semantics]**. Measuring every line costs a galley each, so it only
    // happens on the frames where the menu is actually on screen. A section title is a line like
    // any other here: a long one widens the menu just as a long item would.
    let width = if opening {
        let painter = ui.painter();
        let measure = |text: &str, font: &FontId| {
            let galley =
                painter.layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER);
            ideal_item_width(galley.size().x, row_height)
        };
        items
            .iter()
            .map(|item| measure(item, &font))
            .chain(
                headers
                    .iter()
                    .filter(|header| header.before < items.len())
                    .map(|header| measure(header.label, &header_font)),
            )
            .fold(rect.width(), f32::max)
    } else {
        rect.width()
    };
    // JUCE keeps the menu on screen by scrolling it; this keeps it on screen by scrolling it too,
    // just with a scrollbar instead of arrows.
    let max_height = (ui.ctx().content_rect().height() - rect.height()).max(row_height);

    let frame = egui::Frame::NONE
        .fill(palette.color(FxColor::DefaultFill))
        .stroke(Stroke::new(
            1.0,
            palette.color_alpha(FxColor::MenuText, POPUP_BORDER_ALPHA),
        ))
        .inner_margin(egui::Margin::ZERO);

    let mut picked = None;
    egui::Popup::from_toggle_button_response(response)
        // A JUCE menu dismisses itself when an *item* is chosen or the click lands outside it;
        // a press on a separator, a section header or the scroll gutter is swallowed and the
        // menu stays. `CloseOnClick` would close on every one of those, so the picked row closes
        // the menu itself (`Ui::close`) and only an outside click is left to egui.
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .gap(0.0)
        .width(width)
        .frame(frame)
        .show(|ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (index, item) in items.iter().enumerate() {
                        if separator_before == Some(index) {
                            separator(ui, palette, width, row_height);
                        }
                        for section in headers.iter().filter(|header| header.before == index) {
                            header(ui, palette, section.label, &header_font, width, row_height);
                        }
                        let ticked = selected == Some(index);
                        if row(ui, palette, item, &font, width, row_height, ticked) {
                            picked = Some(index);
                            ui.close();
                        }
                    }
                });
        });
    picked
}

/// One menu item. Returns whether it was clicked.
fn row(
    ui: &mut Ui,
    palette: Palette,
    text: &str,
    font: &FontId,
    width: f32,
    height: f32,
    ticked: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    // `drawPopupMenuItem` passes `isHighlighted || isTicked` down as the highlight flag, so the
    // selected row is painted hovered even when the pointer is elsewhere (`FxTheme.cpp:358-359`).
    let highlighted = ticked || response.hovered();
    let painter = ui.painter();

    if highlighted {
        painter.rect_filled(
            rect,
            CornerRadius::ZERO,
            palette.color(FxColor::ImageButton),
        );
    }
    if ticked {
        // FxTheme's one addition to the stock item: a rectangle around the ticked row.
        painter.rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, palette.color(FxColor::MenuText)),
            StrokeKind::Inside,
        );
    }

    let colour = if highlighted {
        palette.color(FxColor::HighlightedText)
    } else {
        palette.color(FxColor::MenuText)
    };
    draw_truncated(
        painter,
        text,
        font.clone(),
        colour,
        rect.left() + popup_text_x(height),
        // `area.reduced(1)` and then `r.removeFromRight(3)` before the text is drawn
        // **[JUCE semantics]**.
        rect.right() - 1.0 - 3.0,
        rect.center().y,
    );

    if response.hovered() {
        // `preparePopupMenuWindow` gives every child of the menu the pointing hand
        // (`FxTheme.cpp:372-380`).
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response.clicked()
}

/// The one-point rule between the factory and user presets **[JUCE semantics]**.
fn separator(ui: &mut Ui, palette: Palette, width: f32, row_height: f32) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(width, popup_separator_height(row_height)),
        Sense::hover(),
    );
    let y = rect.center().y.round();
    ui.painter().hline(
        (rect.left() + SEPARATOR_INSET)..=(rect.right() - SEPARATOR_INSET),
        y,
        Stroke::new(1.0, palette.color_alpha(FxColor::MenuText, SEPARATOR_ALPHA)),
    );
}

/// A section title **(port addition)**: a row that is read, never picked.
///
/// It is allocated with no sense at all, so egui never reports a click or a hover highlight on
/// it, and — unlike the item rows — it keeps the arrow cursor: `preparePopupMenuWindow`'s hand
/// (`FxTheme.cpp:372-380`) promises a click will do something, which here it would not. The title
/// starts at [`popup_text_x`] of the *item* row height so it lines up with the item text below it.
fn header(ui: &mut Ui, palette: Palette, text: &str, font: &FontId, width: f32, row_height: f32) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(width, popup_header_height(row_height)), Sense::hover());
    draw_truncated(
        ui.painter(),
        text,
        font.clone(),
        palette.color_alpha(FxColor::MenuText, HEADER_TEXT_ALPHA),
        rect.left() + popup_text_x(row_height),
        rect.right() - 1.0 - 3.0,
        rect.center().y,
    );
}

/// Draw one line of text, elided with `…` rather than condensed.
///
/// `label.setMinimumHorizontalScale(1.0)` (`FxTheme.cpp:130`) is what rules out JUCE's default
/// squeeze-to-fit; `drawFittedText` then drops the overflow. egui's `TextWrapping::truncate_at_width`
/// is the same contract with a nicer ellipsis.
fn draw_truncated(
    painter: &egui::Painter,
    text: &str,
    font: FontId,
    colour: Color32,
    left: f32,
    right: f32,
    center_y: f32,
) {
    let mut job =
        LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, colour));
    job.wrap = TextWrapping::truncate_at_width((right - left).max(0.0));
    let galley = painter.layout_job(job);
    let y = center_y - galley.size().y / 2.0;
    painter.galley(pos2(left, y), galley, colour);
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::ClippedShape;
    use egui::{Event, Modifiers, PointerButton, Pos2, Shape};
    use fxsound_core::ThemeMode;

    /// The Pro preset / device combo (`FxProView.cpp:426-430`).
    fn pro_combo() -> Rect {
        Rect::from_min_size(pos2(40.0, 32.0), vec2(470.0, 40.0))
    }

    /// The EQ band-count combo (`FxAudioControls.cpp:436`).
    fn band_combo() -> Rect {
        Rect::from_min_size(pos2(8.0, 28.0), vec2(152.0, 20.0))
    }

    #[test]
    fn the_corner_is_a_fifth_of_the_height_for_every_combo_in_the_app() {
        // docs/spec/03-controls.md §8.2's resolved geometry table.
        for (height, expected) in [(40.0_f32, 8.0_f32), (50.0, 10.0), (20.0, 4.0)] {
            let corner = corner_radius(height);
            assert!(
                (corner - expected).abs() < 1e-6,
                "a {height} point combo rounded by {corner}, not {expected}"
            );
        }
    }

    #[test]
    fn the_font_drops_to_fourteen_points_at_thirty_and_not_at_thirty_one() {
        assert!((font_size(20.0) - SMALL_FONT).abs() < 1e-6);
        assert!((font_size(30.0) - SMALL_FONT).abs() < 1e-6);
        assert!((font_size(31.0) - FONT).abs() < 1e-6);
        assert!((font_size(40.0) - FONT).abs() < 1e-6);
        assert!((font_size(50.0) - FONT).abs() < 1e-6);
    }

    #[test]
    fn the_arrow_box_matches_the_specs_geometry_table() {
        // (438, 0, 12, 40) inside the 470 x 40 Pro combo: the wide margin of 32.
        let arrow = arrow_box(pro_combo());
        assert!((arrow.left() - (40.0 + 438.0)).abs() < 1e-4, "{arrow:?}");
        assert!((arrow.width() - 12.0).abs() < 1e-4);
        assert!((arrow.height() - 40.0).abs() < 1e-4);

        // (120, 0, 12, 20) inside the 152 x 20 band combo: 152 is *over* 150, so still 32.
        let arrow = arrow_box(band_combo());
        assert!((arrow.left() - (8.0 + 120.0)).abs() < 1e-4, "{arrow:?}");
        assert!((arrow.height() - 20.0).abs() < 1e-4);
    }

    #[test]
    fn a_combo_narrower_than_the_threshold_pulls_its_arrow_in_by_eight() {
        let narrow = Rect::from_min_size(pos2(0.0, 0.0), vec2(150.0, 20.0));
        let arrow = arrow_box(narrow);
        assert!((arrow.left() - (150.0 - NARROW_ARROW_MARGIN)).abs() < 1e-4);
        // One point wider and the margin jumps back to 32.
        let wide = Rect::from_min_size(pos2(0.0, 0.0), vec2(151.0, 20.0));
        assert!((arrow_box(wide).left() - (151.0 - ARROW_MARGIN)).abs() < 1e-4);
    }

    #[test]
    fn the_arrow_artwork_is_centred_in_its_box_without_distortion() {
        // The 11 x 7 chevron in a 12 x 40 box: the width binds, so it is scaled by 12/11.
        let box_ = arrow_box(pro_combo());
        let art = fitted_rect(box_, art_size(FxImage::DropDownArrowHover));
        assert!((art.width() - 12.0).abs() < 1e-3);
        assert!((art.height() - 12.0 * 7.0 / 11.0).abs() < 1e-3);
        assert!((art.center() - box_.center()).length() < 1e-3);
    }

    #[test]
    fn the_text_box_stops_thirty_seven_points_short_of_the_right_edge() {
        // §8.2: (5, 1, 428, 38) and (5, 1, 110, 18), relative to the combo.
        let text = text_box(pro_combo());
        assert!((text.left() - (40.0 + 5.0)).abs() < 1e-4);
        assert!((text.width() - 428.0).abs() < 1e-4, "{text:?}");
        assert!((text.top() - (32.0 + 1.0)).abs() < 1e-4);
        assert!((text.height() - 38.0).abs() < 1e-4);

        let text = text_box(band_combo());
        assert!((text.width() - 110.0).abs() < 1e-4, "{text:?}");
        assert!((text.height() - 18.0).abs() < 1e-4);
    }

    #[test]
    fn the_text_never_reaches_under_the_arrow() {
        for rect in [pro_combo(), band_combo()] {
            assert!(
                text_box(rect).right() <= arrow_box(rect).left(),
                "the text ran under the arrow of a {:?} combo",
                rect.size()
            );
        }
    }

    #[test]
    fn a_popup_row_is_as_tall_as_the_closed_boxs_text_area() {
        assert!((popup_row_height(40.0) - 38.0).abs() < 1e-4);
        assert!((popup_row_height(50.0) - 48.0).abs() < 1e-4);
        assert!((popup_row_height(20.0) - 18.0).abs() < 1e-4);
        // The same number the label uses, which is what JUCE hands the menu.
        assert!((popup_row_height(40.0) - text_box(pro_combo()).height()).abs() < 1e-4);
    }

    #[test]
    fn the_popup_font_only_shrinks_when_seventeen_points_will_not_fit() {
        // A 38 point row could take 27.7 px, so the menu font stays at its 17.
        assert!((popup_font_size(38.0) - POPUP_FONT).abs() < 1e-4);
        // An 18 point row cannot: (18 - 2) / 1.3 = 12.31.
        let small = popup_font_size(18.0);
        assert!((small - 16.0 / 1.3).abs() < 1e-4, "shrank to {small}");
        assert!(small < POPUP_FONT);
    }

    #[test]
    fn the_icon_column_reserves_the_same_width_whether_or_not_a_tick_is_drawn() {
        // round((38 - 2) / 1.3) = 28, plus the one point of item inset.
        let x = popup_text_x(38.0);
        assert!((x - 29.0).abs() < 1e-4, "text started at {x}");
        // round((18 - 2) / 1.3) = 12.
        assert!((popup_text_x(18.0) - 13.0).abs() < 1e-4);
    }

    #[test]
    fn a_separator_is_a_tenth_of_a_row_and_never_thinner_than_a_point() {
        assert!((popup_separator_height(38.0) - 3.0).abs() < 1e-6);
        assert!((popup_separator_height(48.0) - 4.0).abs() < 1e-6);
        // Integer division would floor an 18 point row to 1, not to 1.8.
        assert!((popup_separator_height(18.0) - 1.0).abs() < 1e-6);
        assert!((popup_separator_height(5.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_menu_is_never_narrower_than_the_box_it_hangs_from() {
        // An empty label still wants the icon column and the gutter.
        assert!((ideal_item_width(0.0, 38.0) - 76.0).abs() < 1e-4);
        assert!((ideal_item_width(200.0, 38.0) - 276.0).abs() < 1e-4);
    }

    #[test]
    fn a_modified_preset_carries_the_originals_trailing_star() {
        assert_eq!(preset_label("Rock", true), "Rock *");
        assert_eq!(preset_label("Rock", false), "Rock");
        // The marker is a space then an asterisk, never a bare asterisk.
        assert!(preset_label("Bass Boost", true).ends_with(" *"));
        assert_eq!(MODIFIED_SUFFIX, " *");
    }

    #[test]
    fn the_outline_is_invisible_until_something_asks_for_it() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let palette = Palette::new(mode);
            // Never touched: `FxTheme::init` leaves it equal to the fill.
            assert_eq!(
                outline_colour(palette, None, false),
                palette.color(FxColor::ComboBoxBackground),
                "{mode:?}"
            );
            // setError(true) / setError(false) are two *different* colours, and neither is the fill
            // in the light palette.
            assert_eq!(
                outline_colour(palette, Some(true), false),
                palette.color(FxColor::SliderTrack)
            );
            assert_eq!(
                outline_colour(palette, Some(false), false),
                palette.color(FxColor::DefaultFill)
            );
        }
        let light = Palette::new(ThemeMode::Light);
        assert_ne!(
            outline_colour(light, Some(false), false),
            outline_colour(light, None, false),
            "a device combo that has reported an error keeps a white hairline in the light theme"
        );
    }

    #[test]
    fn keyboard_focus_outranks_the_error_outline() {
        let palette = Palette::new(ThemeMode::Dark);
        let focused = outline_colour(palette, Some(true), true);
        assert_eq!(
            focused,
            palette.color_alpha(FxColor::SliderHighlight, FOCUS_OUTLINE_ALPHA)
        );
        // α 0.2 of 255.
        assert_eq!(focused.a(), 51);
    }

    #[test]
    fn the_text_highlights_on_hover_only_while_the_box_is_live() {
        let palette = Palette::new(ThemeMode::Dark);
        assert_eq!(
            text_colour(palette, true, true),
            palette.color(FxColor::HighlightedText)
        );
        assert_eq!(
            text_colour(palette, true, false),
            palette.color(FxColor::DefaultText)
        );
        assert_eq!(
            text_colour(palette, false, true),
            palette.color(FxColor::DefaultText)
        );
    }

    /// Drive a widget through a real frame so the painting path runs, not just the geometry.
    fn frame(ctx: &egui::Context, add_contents: impl FnMut(&mut Ui)) {
        ctx.run_ui(Default::default(), add_contents)
            .drop_without_applying_deltas();
    }

    fn test_context() -> egui::Context {
        let ctx = egui::Context::default();
        // The real faces, because the popup's width comes out of a measured galley.
        ctx.set_fonts(theme::font_definitions());
        ctx
    }

    #[test]
    fn a_disabled_combo_draws_itself_and_cannot_be_opened() {
        let items = [String::from("Speakers"), String::from("Headphones")];
        let mut assets = AssetCache::new();
        let ctx = test_context();
        frame(&ctx, |ui| {
            let (response, picked) = FxComboBox::new(&items, Some(0))
                .enabled(false)
                .error(true)
                .show(
                    ui,
                    pro_combo(),
                    Palette::new(ThemeMode::Dark),
                    &mut assets,
                    "device",
                );
            assert!(picked.is_none());
            assert!((response.rect.min - pro_combo().min).length() < 1e-4);
            assert!((response.rect.max - pro_combo().max).length() < 1e-4);
            assert!(!egui::Popup::is_id_open(
                ui.ctx(),
                egui::Popup::default_response_id(&response)
            ));
        });
    }

    #[test]
    fn an_open_popup_paints_every_item_and_the_one_separator() {
        let items: Vec<String> = ["Flat", "Rock", preset_label("My Mix", true).as_str()]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let mut assets = AssetCache::new();
        let ctx = test_context();
        let mut popup_id = None;

        let mut show = |ui: &mut Ui, id: &mut Option<egui::Id>| {
            let (response, picked) = FxComboBox::new(&items, Some(0))
                // The factory→user boundary of docs/spec/03-controls.md §8.4.
                .separator_before(Some(2))
                .show(
                    ui,
                    pro_combo(),
                    Palette::new(ThemeMode::Light),
                    &mut assets,
                    "preset",
                );
            *id = Some(egui::Popup::default_response_id(&response));
            picked
        };

        // A frame with the menu shut, to learn the popup's id...
        frame(&ctx, |ui| {
            assert!(show(ui, &mut popup_id).is_none());
        });
        let id = popup_id.expect("the closed box was drawn");

        // ...then a frame with it open, which is the one that paints the rows.
        egui::Popup::open_id(&ctx, id);
        frame(&ctx, |ui| {
            assert!(show(ui, &mut popup_id).is_none(), "nothing was clicked");
        });
        assert!(
            egui::Popup::is_id_open(&ctx, id),
            "the menu should still be open with no click to close it"
        );
    }

    #[test]
    fn a_combo_box_starts_enabled_with_nothing_placeheld_and_no_error() {
        let items = [String::from("Flat"), String::from("Rock")];
        let combo = FxComboBox::new(&items, Some(1));
        assert!(combo.enabled);
        assert_eq!(combo.selected, Some(1));
        assert_eq!(combo.placeholder, "");
        assert_eq!(combo.error, None);
        assert_eq!(combo.separator_before, None);
        assert!(combo.headers.is_empty());
        assert!(combo.row_height.is_none());

        let headers = [SectionHeader::new(0, "Output")];
        let combo = FxComboBox::new(&items, None)
            .enabled(false)
            .placeholder("No device")
            .error(true)
            .separator_before(Some(1))
            .headers(&headers)
            .row_height(26.0);
        assert!(!combo.enabled);
        assert_eq!(combo.placeholder, "No device");
        assert_eq!(combo.error, Some(true));
        assert_eq!(combo.separator_before, Some(1));
        assert_eq!(combo.headers, &headers);
        assert!(combo.row_height.is_some_and(|h| (h - 26.0).abs() < 1e-6));
    }

    // ---- section headers -------------------------------------------------------------------

    #[test]
    fn a_section_header_is_seven_tenths_of_a_row_on_the_pixel_grid() {
        // 38 * 0.7 = 26.6 and 48 * 0.7 = 33.6: both land on whole points so the item rows under
        // them do too.
        assert!((popup_header_height(38.0) - 27.0).abs() < 1e-6);
        assert!((popup_header_height(48.0) - 34.0).abs() < 1e-6);
        assert!((popup_header_height(18.0) - 13.0).abs() < 1e-6);
        assert!((popup_header_height(0.0) - 1.0).abs() < 1e-6);
        // The title is set smaller than the items, and shrinks with them.
        assert!((popup_header_font_size(38.0) - POPUP_FONT * HEADER_FONT_RATIO).abs() < 1e-4);
        assert!(popup_header_font_size(38.0) < popup_font_size(38.0));
        assert!(popup_header_font_size(18.0) < popup_font_size(18.0));
    }

    fn press(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        }
    }

    /// Every line of text in `shapes` that was painted below `combo`, i.e. inside its menu, with
    /// the rectangle it occupies. The closed box paints its own label too, which this skips.
    fn menu_texts(shapes: &[ClippedShape], combo: Rect) -> Vec<(String, Rect)> {
        shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                Shape::Text(text) => Some((
                    text.galley.text().to_owned(),
                    Rect::from_min_size(text.pos, text.galley.size()),
                )),
                _ => None,
            })
            .filter(|(_, rect)| rect.top() >= combo.bottom() - 1.0)
            .collect()
    }

    /// Where one line of the menu was painted.
    fn menu_text(shapes: &[ClippedShape], combo: Rect, text: &str) -> Rect {
        let hits: Vec<Rect> = menu_texts(shapes, combo)
            .into_iter()
            .filter(|(painted, _)| painted == text)
            .map(|(_, rect)| rect)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "{text:?} was painted {} times in the menu",
            hits.len()
        );
        hits[0]
    }

    /// How many lines of the menu read `text`.
    fn menu_text_count(shapes: &[ClippedShape], combo: Rect, text: &str) -> usize {
        menu_texts(shapes, combo)
            .iter()
            .filter(|(painted, _)| painted == text)
            .count()
    }

    /// The separators: the only free-standing line segments a combo box ever paints.
    fn separator_count(shapes: &[ClippedShape]) -> usize {
        shapes
            .iter()
            .filter(|clipped| matches!(clipped.shape, Shape::LineSegment { .. }))
            .count()
    }

    /// The menu's own frame: the widest rectangle under the box, filled in `DefaultFill`.
    fn menu_frame(shapes: &[ClippedShape], combo: Rect, palette: Palette) -> Rect {
        let frame = shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                Shape::Rect(rect) if rect.rect.top() >= combo.bottom() - 1.0 => Some(rect),
                _ => None,
            })
            .max_by(|a, b| a.rect.width().total_cmp(&b.rect.width()))
            .expect("the menu painted no frame");
        assert_eq!(
            frame.fill,
            palette.color(FxColor::DefaultFill),
            "the widest rectangle under the box is not the menu's frame"
        );
        frame.rect
    }

    /// The colour one line of the menu was painted in.
    fn menu_text_colour(shapes: &[ClippedShape], combo: Rect, text: &str) -> Color32 {
        shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                Shape::Text(shape)
                    if shape.galley.text() == text && shape.pos.y >= combo.bottom() - 1.0 =>
                {
                    Some(shape.fallback_color)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{text:?} was not painted in the menu"))
    }

    /// A device list with section titles, driven a frame at a time.
    struct DeviceMenu {
        ctx: egui::Context,
        assets: AssetCache,
        combo: Rect,
        items: Vec<String>,
        headers: Vec<SectionHeader<'static>>,
        separator: Option<usize>,
        popup_id: Option<Id>,
    }

    impl DeviceMenu {
        const PALETTE: Palette = Palette::new(ThemeMode::Light);

        fn new(
            combo: Rect,
            items: &[&str],
            headers: &[SectionHeader<'static>],
            separator: Option<usize>,
        ) -> Self {
            let ctx = test_context();
            // An `Area` fades in over `animation_time`; with it at zero the frame that paints the
            // menu paints it at full opacity, so colours can be compared exactly.
            ctx.all_styles_mut(|style| style.animation_time = 0.0);
            Self {
                ctx,
                assets: AssetCache::new(),
                combo,
                items: items.iter().map(|item| (*item).to_owned()).collect(),
                headers: headers.to_vec(),
                separator,
                popup_id: None,
            }
        }

        /// What `views::combos` builds from two outputs and one input: `Output`, the two
        /// playback devices, a rule, `Input`, the microphone.
        fn devices() -> Self {
            Self::new(
                pro_combo(),
                &["Speakers", "Headphones", "Microphone"],
                &[
                    SectionHeader::new(0, "Output"),
                    SectionHeader::new(2, "Input"),
                ],
                Some(2),
            )
        }

        fn popup_id(&self) -> Id {
            self.popup_id.expect("the box has not been drawn yet")
        }

        fn is_open(&self) -> bool {
            egui::Popup::is_id_open(&self.ctx, self.popup_id())
        }

        /// One frame: what was picked, and what was painted.
        fn frame(&mut self, events: Vec<Event>) -> (Option<usize>, Vec<ClippedShape>) {
            let Self {
                ctx,
                assets,
                combo,
                items,
                headers,
                separator,
                popup_id,
            } = self;
            let mut picked = None;
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let (response, pick) = FxComboBox::new(items, Some(0))
                    .separator_before(*separator)
                    .headers(headers)
                    .show(ui, *combo, Self::PALETTE, assets, "device");
                *popup_id = Some(egui::Popup::default_response_id(&response));
                picked = pick;
            });
            let shapes = std::mem::take(&mut output.shapes);
            output.drop_without_applying_deltas();
            (picked, shapes)
        }

        /// Open the menu and run it until it has been laid out and painted.
        ///
        /// The first frame learns the popup's id, the next is egui's invisible sizing pass, and
        /// the third is the one whose shapes are returned.
        fn open(&mut self) -> Vec<ClippedShape> {
            self.frame(Vec::new());
            egui::Popup::open_id(&self.ctx, self.popup_id());
            self.frame(Vec::new());
            let (picked, shapes) = self.frame(Vec::new());
            assert!(picked.is_none(), "opening the menu picked {picked:?}");
            assert!(self.is_open(), "the menu did not stay open");
            shapes
        }

        /// Hover, press and release at `pos`, collecting every pick reported on the way.
        ///
        /// egui hit-tests against the previous pass's widget rectangles, so the click needs the
        /// same three frames the title-bar tests use.
        fn click(&mut self, pos: Pos2) -> Vec<usize> {
            let mut picks = Vec::new();
            for events in [
                vec![Event::PointerMoved(pos)],
                vec![Event::PointerMoved(pos), press(pos, true)],
                vec![press(pos, false)],
            ] {
                picks.extend(self.frame(events).0);
            }
            picks
        }
    }

    #[test]
    fn a_section_header_is_painted_with_its_title_in_line_with_the_items() {
        let mut menu = DeviceMenu::devices();
        let shapes = menu.open();
        let combo = pro_combo();

        let output = menu_text(&shapes, combo, "Output");
        let speakers = menu_text(&shapes, combo, "Speakers");
        let headphones = menu_text(&shapes, combo, "Headphones");
        let input = menu_text(&shapes, combo, "Input");
        let microphone = menu_text(&shapes, combo, "Microphone");

        // The title starts where the item text starts…
        assert!(
            (output.left() - speakers.left()).abs() < 1e-3,
            "Output at x = {}, Speakers at x = {}",
            output.left(),
            speakers.left()
        );
        assert!((input.left() - microphone.left()).abs() < 1e-3);
        // …in a smaller face, in `MenuText` let down to the header alpha…
        assert!(output.height() < speakers.height());
        assert!(input.height() < microphone.height());
        let palette = DeviceMenu::PALETTE;
        assert_eq!(
            menu_text_colour(&shapes, combo, "Output"),
            palette.color_alpha(FxColor::MenuText, HEADER_TEXT_ALPHA)
        );
        // Headphones is neither ticked nor hovered, so it is in plain `MenuText`.
        assert_eq!(
            menu_text_colour(&shapes, combo, "Headphones"),
            palette.color(FxColor::MenuText)
        );
        // …and each sits above its own run: Output, Speakers, Headphones, Input, Microphone.
        for (above, below) in [
            (output, speakers),
            (speakers, headphones),
            (headphones, input),
            (input, microphone),
        ] {
            assert!(
                above.bottom() <= below.top() + 1e-3,
                "{above:?} is not above {below:?}"
            );
        }
    }

    #[test]
    fn outputs_and_inputs_are_titled_twice_and_ruled_once() {
        let mut menu = DeviceMenu::devices();
        let shapes = menu.open();
        assert_eq!(menu_text_count(&shapes, pro_combo(), "Output"), 1);
        assert_eq!(menu_text_count(&shapes, pro_combo(), "Input"), 1);
        assert_eq!(separator_count(&shapes), 1);
        // The rule sits between the last output and the Input title.
        let rule_y = shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                Shape::LineSegment { points, .. } => Some(points[0].y),
                _ => None,
            })
            .expect("no separator");
        let headphones = menu_text(&shapes, pro_combo(), "Headphones");
        let input = menu_text(&shapes, pro_combo(), "Input");
        assert!(
            headphones.bottom() <= rule_y && rule_y <= input.top(),
            "the rule at y = {rule_y} is not between {headphones:?} and {input:?}"
        );
    }

    #[test]
    fn a_list_of_playback_devices_alone_is_titled_once_and_never_ruled() {
        let mut menu = DeviceMenu::new(
            pro_combo(),
            &["Speakers", "Headphones"],
            &[SectionHeader::new(0, "Output")],
            None,
        );
        let shapes = menu.open();
        assert_eq!(menu_text_count(&shapes, pro_combo(), "Output"), 1);
        assert_eq!(menu_text_count(&shapes, pro_combo(), "Input"), 0);
        assert_eq!(separator_count(&shapes), 0);
    }

    #[test]
    fn clicking_a_section_header_picks_nothing_and_leaves_the_menu_open() {
        let mut menu = DeviceMenu::devices();
        let shapes = menu.open();

        for title in ["Output", "Input"] {
            let header = menu_text(&shapes, pro_combo(), title);
            let picks = menu.click(header.center());
            assert!(picks.is_empty(), "the {title} header reported {picks:?}");
            assert!(
                menu.is_open(),
                "clicking the {title} header dismissed the menu"
            );
        }

        // The same click on a row is a pick, so the header's silence is not the harness's.
        let headphones = menu_text(&shapes, pro_combo(), "Headphones");
        assert_eq!(menu.click(headphones.center()), vec![1]);
        assert!(!menu.is_open(), "picking an item should dismiss the menu");
    }

    #[test]
    fn a_click_outside_the_menu_dismisses_it_without_a_pick() {
        // The one click that still closes the menu by itself, now that a click inside it on
        // something other than an item does not.
        let mut menu = DeviceMenu::devices();
        let shapes = menu.open();
        let frame = menu_frame(&shapes, pro_combo(), DeviceMenu::PALETTE);
        let outside = pos2(frame.right() + 200.0, frame.bottom() + 200.0);
        assert!(menu.click(outside).is_empty());
        assert!(!menu.is_open(), "a click beside the menu left it open");
    }

    #[test]
    fn item_indices_ignore_the_header_and_separator_rows() {
        // Microphone is the third *item* but the sixth *row*: a title, two items, a rule and a
        // second title come before it.
        let mut menu = DeviceMenu::devices();
        let shapes = menu.open();
        let microphone = menu_text(&shapes, pro_combo(), "Microphone");
        assert_eq!(menu.click(microphone.center()), vec![2]);
    }

    #[test]
    fn a_long_section_title_widens_the_menu_like_a_long_item_would() {
        let narrow = Rect::from_min_size(pos2(40.0, 32.0), vec2(120.0, 40.0));
        let title = "A section title far wider than the box it hangs from";
        let mut menu = DeviceMenu::new(narrow, &["A", "B"], &[SectionHeader::new(0, title)], None);
        let shapes = menu.open();

        let frame = menu_frame(&shapes, narrow, DeviceMenu::PALETTE);
        let painted = menu_text(&shapes, narrow, title);
        assert!(
            frame.width() > narrow.width(),
            "a {} point menu hangs from a {} point box",
            frame.width(),
            narrow.width()
        );
        // `withMinimumWidth` plus the icon column and gutter every line reserves.
        let wanted = ideal_item_width(painted.width(), popup_row_height(narrow.height()));
        assert!(
            frame.width() + 1.0 >= wanted,
            "the menu is {} points wide but its title wants {wanted}",
            frame.width()
        );
    }

    #[test]
    fn a_header_past_the_end_of_the_list_is_not_drawn() {
        let mut menu = DeviceMenu::new(
            pro_combo(),
            &["Speakers"],
            &[
                SectionHeader::new(0, "Output"),
                SectionHeader::new(5, "Input"),
            ],
            None,
        );
        let shapes = menu.open();
        assert_eq!(menu_text_count(&shapes, pro_combo(), "Output"), 1);
        assert_eq!(menu_text_count(&shapes, pro_combo(), "Input"), 0);
    }
}
