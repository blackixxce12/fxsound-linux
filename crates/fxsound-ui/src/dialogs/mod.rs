//! The secondary windows: Settings, the preset import/export flow, message boxes and the toast.
//!
//! Everything in this module is a port of a `FxWindow` (`fxsound/Source/GUI/FxWindow.h:27`) — a
//! borderless `juce::Component` pushed straight onto the desktop, drawing its own title bar, close
//! button, rounded corners and drop shadow, and blocking its caller in `runModalLoop()`. The
//! chrome is shared, so it is written once here and the three dialog files draw only their own
//! contents.
//!
//! ## How the modality is modelled
//!
//! `runModalLoop()` is a nested event loop: `FxConfirmationMessage::showMessage()` returns a
//! `bool` from inside it, and `FxController::exportPresets()` calls it once per colliding file
//! (`docs/spec/06-dialogs.md` §9.1 and Open question 3). egui is immediate-mode and has no nested
//! loop, so every dialog here is a *state machine the caller owns*: the caller keeps the state,
//! this crate renders it and returns a [`DialogResponse`] of actions, and the caller decides what
//! happens next. Nothing in this module blocks, opens a file picker, touches the file system or
//! talks to the audio engine — exactly the discipline [`crate::state`] sets for the main window.
//!
//! ## Which egui construct each dialog maps to
//!
//! `docs/spec/06-dialogs.md` §9.1 recommends real windows for the three big dialogs and
//! [`egui::Modal`] — which does exist in 0.36 (`docs/api/egui-0.36-widgets-input.md` §8.3) — for
//! the small message boxes. This module does not *choose* for the caller: every dialog exposes a
//! `show` that draws the whole window into a [`Rect`], so the same code works inside a deferred
//! viewport of [`outer_size`] and inside a modal in the main viewport. [`message::MessageBox`]
//! additionally offers [`message::MessageBox::show_modal`], because a confirmation really does
//! want the backdrop and the focus trap.
//!
//! What Wayland takes away is listed in §9.1's table: a client cannot position its own toplevel
//! (so `centreWithSize` is gone — let the compositor place the window), cannot force itself on top
//! (so `setAlwaysOnTop` is gone — use a modal child window), and cannot move itself by tracking
//! the pointer (so a title-bar drag has to be handed to the compositor with
//! [`egui::ViewportCommand::StartDrag`], which is what [`ChromeResponse::drag_started`] is for).

pub mod changelog;
pub mod message;
pub mod presets;
pub mod settings;

pub use message::{ConfirmChoice, ConfirmStyle, MessageBox, Toast, ToastLayout, ToastResponse};
pub use presets::{
    ExportDialog, ExportState, ImportDialog, ImportState, ImportSummary, OverwriteChoice,
    PresetsAction,
};
pub use settings::{
    DevicePriority, HotkeyCommand, NavIcon, NavIcons, SettingsAction, SettingsDialog, SettingsState,
    SettingsTab,
};

use crate::assets::{AssetCache, FxImage};
use crate::theme::{self, FxColor, Palette};
use crate::widgets::icon_button::{art_size, fitted_rect, hit_rect, paint_image};
use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Painter, Rect, Sense, Shadow, Stroke,
    Ui, Vec2, pos2, vec2,
};

// ---------------------------------------------------------------------------------------------
// Fonts (`docs/spec/06-dialogs.md` §0.4, `FxTheme.cpp:466-479`)
// ---------------------------------------------------------------------------------------------

/// `FxTheme::getNormalFont()` — Gilroy Semibold, 17 px. The dialogs' default.
pub const NORMAL_FONT: f32 = 17.0;
/// `FxTheme::getSmallFont()` — Gilroy Regular, 14 px. Hotkey rows and the version string.
pub const SMALL_FONT: f32 = 14.0;
/// `FxTheme::getTitleFont()` — Gilroy Bold, 17 px. Every pane title.
pub const TITLE_FONT: f32 = 17.0;

/// `getNormalFont()`.
#[must_use]
pub fn normal_font() -> FontId {
    theme::semibold(NORMAL_FONT)
}

/// `getSmallFont()`.
#[must_use]
pub fn small_font() -> FontId {
    theme::regular(SMALL_FONT)
}

/// `getTitleFont()`.
#[must_use]
pub fn title_font() -> FontId {
    theme::bold(TITLE_FONT)
}

// ---------------------------------------------------------------------------------------------
// `FxWindow` geometry (`docs/spec/06-dialogs.md` §0.1, §0.2)
// ---------------------------------------------------------------------------------------------

/// `FxWindow::SHADOW_WIDTH` (`FxWindow.h:44`) — the transparent gutter the shadow is painted in.
pub const SHADOW_WIDTH: f32 = 5.0;
/// `FxWindow::CLOSE_BUTTON_WIDTH` (`FxWindow.h:45`); the button is square.
pub const CLOSE_BUTTON_WIDTH: f32 = 15.0;
/// `FxTheme::WINDOW_CORNER_RADIUS` (`FxTheme.h:42`).
pub const WINDOW_CORNER_RADIUS: f32 = 21.0;
/// `FxTheme::TITLE_BAR_HEIGHT` (`FxTheme.h:43`) — the band the outer-size formula reserves.
pub const TITLE_BAR_BAND: f32 = 57.0;
/// The title-bar component itself is one point shorter (`FxWindow.cpp:29`).
pub const TITLE_BAR_HEIGHT: f32 = TITLE_BAR_BAND - 1.0;
/// `TitleBar::ICON_WIDTH` (`FxWindow.h:71`) — the wordmark shown by a window with no name.
pub const ICON_WIDTH: f32 = 106.0;
/// `TitleBar::ICON_HEIGHT` (`FxWindow.h:72`).
pub const ICON_HEIGHT: f32 = 15.0;
/// A *named* window draws the narrow bars logo one point shorter (`FxWindow.cpp:280`).
pub const TITLE_ICON_HEIGHT: f32 = ICON_HEIGHT - 1.0;
/// Gap between that logo and the title text (`FxWindow.cpp:283`, `.withX(width + 2)`).
pub const TITLE_TEXT_GAP: f32 = 2.0;
/// Thickness of the close button's "X", as a fraction of the square it is fitted into
/// (`FxWindow.cpp:163-164`, `Path::addLineSegment(…, 0.08f)`).
pub const CLOSE_GLYPH_THICKNESS: f32 = 0.08;

/// The drop shadow's colour.
///
/// `DropShadow`'s default — black at `0x90` — with only `radius` overridden (`FxWindow.cpp:125`).
/// JUCE is not vendored in this tree, so this is the one number here read out of the framework
/// rather than out of FxSound's own sources **[JUCE semantics]**.
pub const SHADOW_COLOUR: Color32 = Color32::from_black_alpha(0x90);

/// The outer window size for a given content size (`docs/spec/06-dialogs.md` §0.2).
///
/// ```text
/// outer_width  = content_width  + SHADOW_WIDTH * 2                                 = w + 10
/// outer_height = content_height + title bar + WINDOW_CORNER_RADIUS + SHADOW * 2    = h + 87
/// ```
///
/// This is `FxWindow::setContent()` (`FxWindow.cpp:74-82`) and it is load-bearing: it is the only
/// reason the Settings window is 610 × 597 rather than 600 × 510.
#[must_use]
pub fn outer_size(content: Vec2) -> Vec2 {
    vec2(
        content.x + SHADOW_WIDTH * 2.0,
        content.y + TITLE_BAR_HEIGHT + WINDOW_CORNER_RADIUS + SHADOW_WIDTH * 2.0,
    )
}

/// The content size [`outer_size`] was given, recovered from the window.
#[must_use]
pub fn content_size(outer: Vec2) -> Vec2 {
    vec2(
        outer.x - SHADOW_WIDTH * 2.0,
        outer.y - TITLE_BAR_HEIGHT - WINDOW_CORNER_RADIUS - SHADOW_WIDTH * 2.0,
    )
}

/// Where the content sits inside the window: `(SHADOW_WIDTH, title_bar.bottom + 1)` = `(5, 62)`
/// (`FxWindow.cpp:74`, `:150`).
///
/// Note that this does **not** reach the bottom of the frame: the window reserves
/// `WINDOW_CORNER_RADIUS` below the content, and the one-point rule under the title bar eats a
/// point of it, so a 597-point window's 510 points of content stop 25 points short of the bottom.
#[must_use]
pub fn content_rect(outer: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            outer.left() + SHADOW_WIDTH,
            title_bar_rect(outer).bottom() + 1.0,
        ),
        content_size(outer.size()),
    )
}

/// The title bar: inset by `WINDOW_CORNER_RADIUS + SHADOW_WIDTH` = 26 on both sides, `SHADOW_WIDTH`
/// from the top, 56 tall (`FxWindow.cpp:147`).
#[must_use]
pub fn title_bar_rect(outer: Rect) -> Rect {
    let inset = WINDOW_CORNER_RADIUS + SHADOW_WIDTH;
    Rect::from_min_size(
        pos2(outer.left() + inset, outer.top() + SHADOW_WIDTH),
        vec2(outer.width() - inset * 2.0, TITLE_BAR_HEIGHT),
    )
}

/// The rounded rectangle the window actually paints, i.e. everything but the shadow gutter.
#[must_use]
pub fn frame_rect(outer: Rect) -> Rect {
    outer.shrink(SHADOW_WIDTH)
}

/// The close button: 15 × 15, right-aligned, vertically centred in the title bar
/// (`FxWindow.cpp:188`, `:263-264`).
#[must_use]
pub fn close_button_rect(title_bar: Rect) -> Rect {
    let size = Vec2::splat(CLOSE_BUTTON_WIDTH);
    Align2::RIGHT_CENTER.align_size_within_rect(size, title_bar)
}

/// The wordmark a window constructed with an **empty** name shows (`FxWindow.cpp:272-278`).
///
/// The original anchors it `xLeft | yTop`, which puts a 15-point logo at the very top of a
/// 56-point bar. `docs/spec/06-dialogs.md` §0.3 describes it as vertically centred and that is
/// what it should be, so the port centres it — one of the four quirks §7 of the spec's Open
/// questions asks to be decided deliberately rather than by accident.
#[must_use]
pub fn logo_rect(title_bar: Rect) -> Rect {
    let box_ = Align2::LEFT_CENTER.align_size_within_rect(vec2(ICON_WIDTH, ICON_HEIGHT), title_bar);
    // The wordmark is a hair wider than 7:1 while its box is 106 × 15, so fitting it leaves a
    // point of slack; `xLeft` puts that slack on the right, not half of it on each side.
    let art = fitted_rect(box_, art_size(FxImage::DefaultLogo));
    Align2::LEFT_CENTER.align_size_within_rect(art.size(), box_)
}

/// The narrow bars logo a *named* window shows, scaled to `ICON_HEIGHT - 1` = 14
/// (`FxWindow.cpp:280-282`). Vertically centred, for the same reason as [`logo_rect`].
#[must_use]
pub fn title_icon_rect(title_bar: Rect) -> Rect {
    let art = art_size(FxImage::IconLogo);
    let width = TITLE_ICON_HEIGHT * art.x / art.y;
    Align2::LEFT_CENTER.align_size_within_rect(vec2(width, TITLE_ICON_HEIGHT), title_bar)
}

/// Where a named window's title text goes: `icon.right + 2`, filling the rest of the bar up to the
/// close button (`FxWindow.cpp:283`).
#[must_use]
pub fn title_text_rect(title_bar: Rect) -> Rect {
    let left = title_icon_rect(title_bar).right() + TITLE_TEXT_GAP;
    Rect::from_min_max(
        pos2(left, title_bar.top()),
        pos2(close_button_rect(title_bar).left(), title_bar.bottom()),
    )
}

// ---------------------------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------------------------

/// What a dialog returns after one frame.
///
/// The same shape as [`crate::state::UiResponse`], parameterised by the action type, because each
/// dialog has its own vocabulary and none of them should be able to emit another's actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogResponse<A> {
    pub actions: Vec<A>,
}

impl<A> Default for DialogResponse<A> {
    fn default() -> Self {
        Self {
            actions: Vec::new(),
        }
    }
}

impl<A> DialogResponse<A> {
    pub fn push(&mut self, action: A) {
        self.actions.push(action);
    }

    /// Push `action` when `condition` holds — the shape almost every button site wants.
    pub fn push_if(&mut self, condition: bool, action: A) {
        if condition {
            self.actions.push(action);
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.actions.len()
    }
}

impl<A: PartialEq> DialogResponse<A> {
    #[must_use]
    pub fn contains(&self, action: &A) -> bool {
        self.actions.contains(action)
    }
}

/// What the shared chrome reports back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeResponse {
    /// The content area, already inset by the shadow gutter and the title bar.
    pub content: Rect,
    /// The ✕ was clicked. `FxSettingsDialog::closeButtonPressed` and friends all do the same
    /// thing: leave the modal state and take the window off the desktop.
    pub close_clicked: bool,
    /// A drag began on the title bar. Answer it with
    /// [`egui::ViewportCommand::StartDrag`] — Wayland has no client-side window moving
    /// (`docs/spec/06-dialogs.md` §9.1).
    pub drag_started: bool,
}

// ---------------------------------------------------------------------------------------------
// The chrome itself
// ---------------------------------------------------------------------------------------------

/// The borderless window frame every dialog in this module sits in.
///
/// Constructed with a title for the three big dialogs (`"Settings"`, `"Import Presets"`,
/// `"Export Presets"`) and without one for the message boxes, which is what selects between the
/// narrow bars logo plus title text and the full wordmark (`FxWindow.cpp:270-284`).
pub struct DialogChrome<'a> {
    title: Option<&'a str>,
    draggable: bool,
    shadow: bool,
}

impl Default for DialogChrome<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> DialogChrome<'a> {
    /// A window with no name: the wordmark fills the left of the title bar and there is no title
    /// text (`FxMessage`, `FxConfirmationMessage`, `FxImportCompleteMessage`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            title: None,
            draggable: true,
            shadow: true,
        }
    }

    /// A named window: narrow logo, then the title in `getNormalFont()`.
    #[must_use]
    pub fn titled(title: &'a str) -> Self {
        Self {
            title: Some(title),
            ..Self::new()
        }
    }

    /// Whether a press on the title bar reports [`ChromeResponse::drag_started`]. Switch it off
    /// when the chrome is drawn inside a modal, where there is no toplevel to move.
    #[must_use]
    pub fn draggable(mut self, draggable: bool) -> Self {
        self.draggable = draggable;
        self
    }

    /// Whether to paint the drop shadow (`FxWindow::setShadowEnabled`, `FxWindow.cpp:105-111`).
    #[must_use]
    pub fn shadow(mut self, shadow: bool) -> Self {
        self.shadow = shadow;
        self
    }

    /// Paint the frame into `outer` and report the content area.
    ///
    /// `outer` is the whole window including the 5-point shadow gutter, i.e. [`outer_size`] of the
    /// dialog's content size.
    pub fn show(
        self,
        ui: &mut Ui,
        outer: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> ChromeResponse {
        let Self {
            title,
            draggable,
            shadow,
        } = self;

        let id = Id::new("fx_dialog_chrome").with(id_salt);
        let frame = frame_rect(outer);
        let title_bar = title_bar_rect(outer);
        let corner = CornerRadius::same(WINDOW_CORNER_RADIUS as u8);
        let background = palette.window_background();

        if shadow {
            let shape = Shadow {
                offset: [0, 0],
                blur: SHADOW_WIDTH as u8,
                spread: 0,
                color: SHADOW_COLOUR,
            }
            .as_shape(frame, corner);
            ui.painter().add(shape);
        }
        ui.painter().rect_filled(frame, corner, background);

        // `FxWindow::paint` runs this rule from x = shadow_width to `getWidth() - shadow_width*2`,
        // passing a width where an x is wanted, so the original stops five points short of the
        // right edge (`FxWindow.cpp:141-142`). Drawn flat across the frame here.
        ui.painter().hline(
            frame.left()..=frame.right(),
            title_bar.bottom(),
            Stroke::new(1.0, palette.color(FxColor::ControlBackground)),
        );

        match title {
            None => paint_image(
                ui,
                logo_rect(title_bar),
                FxImage::DefaultLogo,
                palette.mode(),
                assets,
                1.0,
            ),
            Some(text) => {
                paint_image(
                    ui,
                    title_icon_rect(title_bar),
                    FxImage::IconLogo,
                    palette.mode(),
                    assets,
                    1.0,
                );
                draw_truncated(
                    ui.painter(),
                    text,
                    normal_font(),
                    palette.color(FxColor::HighlightedText),
                    title_text_rect(title_bar),
                    Align2::LEFT_CENTER,
                );
            }
        }

        let close = close_button_rect(title_bar);
        let close_response = ui.interact(
            hit_rect(close, crate::widgets::icon_button::MIN_HIT_SIZE),
            id.with("close"),
            Sense::click(),
        );
        if close_response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        paint_close_glyph(ui.painter(), close, palette);

        let drag_started = if draggable {
            // Everything but the close button drags the window (`FxWindow.cpp:339-358`).
            let mut bar = title_bar;
            bar.set_right(close.left());
            ui.interact(bar, id.with("drag"), Sense::drag()).drag_started()
        } else {
            false
        };

        ChromeResponse {
            content: content_rect(outer),
            close_clicked: close_response.clicked(),
            drag_started,
        }
    }
}

/// The ✕: two diagonals of the largest square that fits, stroked at 8 % of its side
/// (`FxWindow.cpp:154-169`).
pub fn paint_close_glyph(painter: &Painter, rect: Rect, palette: Palette) {
    let side = rect.width().min(rect.height());
    let square = Rect::from_center_size(rect.center(), Vec2::splat(side));
    let stroke = Stroke::new(
        side * CLOSE_GLYPH_THICKNESS,
        palette.color(FxColor::ImageButton),
    );
    painter.line_segment([square.left_top(), square.right_bottom()], stroke);
    painter.line_segment([square.right_top(), square.left_bottom()], stroke);
}

// ---------------------------------------------------------------------------------------------
// The shared text button (`juce::TextButton`)
// ---------------------------------------------------------------------------------------------

/// `LookAndFeel_V4::drawButtonBackground`'s corner **[JUCE semantics]**.
///
/// `FxTheme` overrides neither `drawButtonBackground` nor `drawButtonText`, and the JUCE modules
/// are not vendored in this tree (`docs/spec/06-dialogs.md` Open question 1), so the four numbers
/// below are the framework's, not FxSound's.
pub const BUTTON_CORNER_RADIUS: f32 = 6.0;
/// How far a pressed button's fill is moved towards its contrasting colour **[JUCE semantics]**.
pub const BUTTON_DOWN_CONTRAST: f32 = 0.2;
/// The same for a hovered button **[JUCE semantics]**.
pub const BUTTON_HOVER_CONTRAST: f32 = 0.05;
/// `withMultipliedAlpha` on a disabled button **[JUCE semantics]**.
pub const BUTTON_DISABLED_ALPHA: f32 = 0.5;

/// `juce::Colour::getPerceivedBrightness()` — the weighted RMS JUCE uses to pick between black and
/// white, not a luminance.
#[must_use]
pub fn perceived_brightness(colour: Color32) -> f32 {
    let r = f32::from(colour.r()) / 255.0;
    let g = f32::from(colour.g()) / 255.0;
    let b = f32::from(colour.b()) / 255.0;
    (r * r * 0.241 + g * g * 0.691 + b * b * 0.068).sqrt()
}

/// `juce::Colour::contrasting(amount)`: overlay black or white at `amount`, whichever contrasts.
///
/// This is what makes a hovered or pressed `TextButton` change colour at all, so it is worth
/// having exactly. The `withMultipliedSaturation(0.9f)` that `drawButtonBackground` applies first
/// is deliberately **not** reproduced: it desaturates FxSound's brand red by a tenth, and every
/// other surface in this port draws `TextButtonBackground` unmodified (`FxTheme.cpp:83`).
#[must_use]
pub fn contrasting(colour: Color32, amount: f32) -> Color32 {
    let t = amount.clamp(0.0, 1.0);
    let target = if perceived_brightness(colour) >= 0.5 {
        0.0_f32
    } else {
        255.0
    };
    let mix = |c: u8| (f32::from(c) + (target - f32::from(c)) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        mix(colour.r()),
        mix(colour.g()),
        mix(colour.b()),
        colour.a(),
    )
}

/// `FxTheme::getTextButtonFont`: Gilroy Semibold at `min(17, button height)` (`FxTheme.cpp:461-464`).
#[must_use]
pub fn text_button_font(button_height: f32) -> FontId {
    theme::semibold(NORMAL_FONT.min(button_height))
}

/// A `juce::TextButton`: rounded fill in `TextButtonBackground`, label in `HighlightedText`
/// (`FxTheme.cpp:83-86`).
pub struct TextButton<'a> {
    label: &'a str,
    enabled: bool,
}

impl<'a> TextButton<'a> {
    #[must_use]
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            enabled: true,
        }
    }

    /// A disabled button keeps its colour at half alpha and does not react to the pointer.
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Draw into an exact rectangle; `response.clicked()` is the press.
    pub fn show(
        self,
        ui: &mut Ui,
        rect: Rect,
        palette: Palette,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> egui::Response {
        let Self { label, enabled } = self;
        let id = Id::new("fx_text_button").with(id_salt);
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let response = ui.interact(rect, id, sense);

        let base = palette.color(FxColor::TextButtonBackground);
        let fill = if !enabled {
            Color32::from_rgba_unmultiplied(
                base.r(),
                base.g(),
                base.b(),
                (BUTTON_DISABLED_ALPHA * 255.0).round() as u8,
            )
        } else if response.is_pointer_button_down_on() {
            contrasting(base, BUTTON_DOWN_CONTRAST)
        } else if response.hovered() {
            contrasting(base, BUTTON_HOVER_CONTRAST)
        } else {
            base
        };

        // `bounds.reduced(0.5f)` before the fill, so the rounded edge lands on the button's own
        // outermost point **[JUCE semantics]**.
        ui.painter().rect_filled(
            rect.shrink(0.5),
            CornerRadius::same(BUTTON_CORNER_RADIUS as u8),
            fill,
        );

        let mut colour = palette.color(FxColor::HighlightedText);
        if !enabled {
            colour = colour.gamma_multiply(BUTTON_DISABLED_ALPHA);
        }
        // `drawButtonText` lays the label out in the button reduced by two points on each side and
        // one at the top **[JUCE semantics]**; wrapping matters because the reset-presets button is
        // allowed up to three lines (`FxSettingsDialog.cpp:289-315`).
        let inner = rect.shrink2(vec2(2.0, 1.0));
        let galley = ui
            .painter()
            .layout(label.to_owned(), text_button_font(rect.height()), colour, inner.width());
        let placed = Align2::CENTER_CENTER.align_size_within_rect(galley.size(), inner);
        ui.painter().galley(placed.min, galley, colour);

        if enabled && response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        response
    }
}

/// A `FxHyperlink`: the normal font, underlined, in `defaultText` — alpha × 0.4 when disabled
/// (`FxHyperlink.cpp:27-42`).
pub fn link(
    ui: &mut Ui,
    rect: Rect,
    text: &str,
    palette: Palette,
    align: Align2,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
) -> egui::Response {
    let id = Id::new("fx_dialog_link").with(id_salt);
    let colour = palette.color(FxColor::DefaultText);
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: normal_font(),
            color: colour,
            underline: Stroke::new(1.0, colour),
            ..Default::default()
        },
    );
    job.wrap = TextWrapping::truncate_at_width(rect.width().max(0.0));
    let galley = ui.painter().layout_job(job);
    let placed = align.align_size_within_rect(galley.size(), rect);
    ui.painter().galley(placed.min, galley, colour);

    let response = ui.interact(placed, id, Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

// ---------------------------------------------------------------------------------------------
// Shared text helpers
// ---------------------------------------------------------------------------------------------

/// Draw one line, elided with `…` rather than condensed, aligned inside `rect`.
///
/// Every label in these dialogs sets `setMinimumHorizontalScale(1.0f)` or is drawn with
/// `drawFittedText`, both of which forbid JUCE's squeeze-to-fit and drop the overflow instead.
pub(crate) fn draw_truncated(
    painter: &Painter,
    text: &str,
    font: FontId,
    colour: Color32,
    rect: Rect,
    align: Align2,
) -> Rect {
    let mut job = LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, colour));
    job.wrap = TextWrapping::truncate_at_width(rect.width().max(0.0));
    let galley = painter.layout_job(job);
    let placed = align.align_size_within_rect(galley.size(), rect);
    painter.galley(placed.min, galley, colour);
    placed
}

/// Draw wrapped text into `rect`, top-aligned and clipped to it, and report how tall it came out.
///
/// The height is the text's own, clipped or not, so a caller can assert that its box is big
/// enough — which the hotkey note's test does. The clip is what stops a translation longer than
/// the English text from painting over whatever sits below it.
pub(crate) fn draw_wrapped(
    painter: &Painter,
    text: &str,
    font: FontId,
    colour: Color32,
    rect: Rect,
) -> f32 {
    let galley = painter.layout(text.to_owned(), font, colour, rect.width().max(0.0));
    let height = galley.size().y;
    painter
        .with_clip_rect(rect.intersect(painter.clip_rect()))
        .galley(rect.min, galley, colour);
    height
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::ThemeMode;

    /// Drive one frame so the painting path really runs.
    pub(super) fn frame(ctx: &egui::Context, add_contents: impl FnMut(&mut Ui)) {
        ctx.run_ui(Default::default(), add_contents)
            .drop_without_applying_deltas();
    }

    pub(super) fn test_context() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions());
        ctx
    }

    #[test]
    fn the_outer_size_formula_reproduces_every_row_of_the_specs_table() {
        // docs/spec/06-dialogs.md §0.2.
        for (content, outer) in [
            (vec2(600.0, 510.0), vec2(610.0, 597.0)), // FxSettingsDialog
            (vec2(400.0, 400.0), vec2(410.0, 487.0)), // FxPresetImportDialog
            (vec2(400.0, 405.0), vec2(410.0, 492.0)), // FxPresetExportDialog
            (vec2(350.0, 340.0), vec2(360.0, 427.0)), // FxImportCompleteMessage
            (vec2(450.0, 142.0), vec2(460.0, 229.0)), // FxConfirmationMessage
            (vec2(400.0, 80.0), vec2(410.0, 167.0)),  // FxMessage
        ] {
            let got = outer_size(content);
            assert!(
                (got - outer).length() < 1e-4,
                "a {content:?} content gave a {got:?} window, not {outer:?}"
            );
        }
    }

    #[test]
    fn the_content_sits_five_points_in_and_sixty_two_points_down() {
        // FxWindow.cpp:74, :150 — content at (SHADOW_WIDTH, title_bar.bottom + 1).
        let outer = Rect::from_min_size(pos2(0.0, 0.0), outer_size(vec2(600.0, 510.0)));
        let content = content_rect(outer);
        assert!((content.left() - 5.0).abs() < 1e-4);
        assert!((content.top() - 62.0).abs() < 1e-4, "{content:?}");
        assert!((content.size() - vec2(600.0, 510.0)).length() < 1e-4);
        // The 21 points of corner allowance, less the point the title-bar rule took.
        assert!((outer.bottom() - content.bottom() - 25.0).abs() < 1e-4, "{content:?}");
        // …and the round trip is exact for every dialog in the table.
        for size in [vec2(600.0, 510.0), vec2(450.0, 142.0), vec2(350.0, 340.0)] {
            assert!((content_size(outer_size(size)) - size).length() < 1e-4);
        }
    }

    #[test]
    fn the_title_bar_is_inset_by_twenty_six_and_bottoms_out_at_sixty_one() {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), outer_size(vec2(600.0, 510.0)));
        let bar = title_bar_rect(outer);
        assert!((bar.left() - 26.0).abs() < 1e-4, "{bar:?}");
        assert!((bar.top() - 5.0).abs() < 1e-4);
        assert!((bar.height() - 56.0).abs() < 1e-4);
        assert!((bar.bottom() - 61.0).abs() < 1e-4);
        // getWidth() - 21*2 - 5*2 for the 610 point window.
        assert!((bar.width() - (610.0 - 42.0 - 10.0)).abs() < 1e-4);
    }

    #[test]
    fn the_close_button_is_a_fifteen_point_square_at_the_right_of_the_bar() {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), outer_size(vec2(450.0, 142.0)));
        let bar = title_bar_rect(outer);
        let close = close_button_rect(bar);
        assert!((close.width() - CLOSE_BUTTON_WIDTH).abs() < 1e-4);
        assert!((close.height() - CLOSE_BUTTON_WIDTH).abs() < 1e-4);
        assert!((close.right() - bar.right()).abs() < 1e-4);
        assert!((close.center().y - bar.center().y).abs() < 1e-4);
    }

    #[test]
    fn the_close_glyph_is_stroked_at_eight_percent_of_its_side() {
        // FxWindow.cpp:163-164: a 15 point button gives a 1.2 point stroke.
        assert!((CLOSE_BUTTON_WIDTH * CLOSE_GLYPH_THICKNESS - 1.2).abs() < 1e-4);
    }

    #[test]
    fn a_nameless_window_shows_the_wordmark_and_a_named_one_the_narrow_bars() {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), outer_size(vec2(600.0, 510.0)));
        let bar = title_bar_rect(outer);

        // The 106 x 15 box, aspect-fitted: the wordmark is 526.19 x 75.15, i.e. very slightly
        // wider than 7:1, so the height binds and the glyph stops a point short of 106.
        let logo = logo_rect(bar);
        assert!((logo.height() - ICON_HEIGHT).abs() < 0.01, "{logo:?}");
        assert!(logo.width() <= ICON_WIDTH + 0.01 && logo.width() > ICON_WIDTH - 2.0);
        assert!((logo.left() - bar.left()).abs() < 1e-4);
        assert!((logo.center().y - bar.center().y).abs() < 1e-4);

        // The named variant is 14 tall and much narrower, and the title starts two points after it.
        let icon = title_icon_rect(bar);
        assert!((icon.height() - 14.0).abs() < 1e-4, "{icon:?}");
        assert!(icon.width() < ICON_WIDTH);
        assert!((title_text_rect(bar).left() - (icon.right() + TITLE_TEXT_GAP)).abs() < 1e-4);
        // …and it never runs under the close button.
        assert!(title_text_rect(bar).right() <= close_button_rect(bar).left() + 1e-4);
    }

    #[test]
    fn a_dialog_response_collects_actions_in_order() {
        let mut response = DialogResponse::default();
        assert!(response.is_empty());
        response.push(1_u8);
        response.push_if(false, 2);
        response.push_if(true, 3);
        assert_eq!(response.actions, vec![1, 3]);
        assert_eq!(response.len(), 2);
        assert!(response.contains(&3));
        assert!(!response.contains(&2));
    }

    #[test]
    fn the_chrome_draws_and_reports_an_untouched_content_area() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), outer_size(vec2(600.0, 510.0)));
        frame(&ctx, |ui| {
            let response = DialogChrome::titled("Settings").show(
                ui,
                outer,
                Palette::new(ThemeMode::Dark),
                &mut assets,
                "settings",
            );
            assert!(!response.close_clicked);
            assert!(!response.drag_started);
            assert!((response.content.size() - vec2(600.0, 510.0)).length() < 1e-4);
        });
    }

    #[test]
    fn a_nameless_chrome_draws_in_both_palettes_without_a_title() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), outer_size(vec2(450.0, 142.0)));
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            frame(&ctx, |ui| {
                let response = DialogChrome::new().draggable(false).shadow(false).show(
                    ui,
                    outer,
                    Palette::new(mode),
                    &mut assets,
                    ("confirm", mode as u8),
                );
                assert!(!response.drag_started, "a non-draggable bar reported a drag");
            });
        }
    }

    #[test]
    fn the_three_dialog_fonts_are_the_three_gilroy_faces() {
        assert_eq!(normal_font().family, theme::semibold(1.0).family);
        assert_eq!(small_font().family, theme::regular(1.0).family);
        assert_eq!(title_font().family, theme::bold(1.0).family);
        assert!((normal_font().size - 17.0).abs() < 1e-6);
        assert!((small_font().size - 14.0).abs() < 1e-6);
        assert!((title_font().size - 17.0).abs() < 1e-6);
    }
}
