//! The modal message box and the transient toast.
//!
//! Two unrelated widgets that happen to be the two ways FxSound talks back to the user:
//!
//! * [`MessageBox`] is `FxConfirmationMessage` (`GUI/FxMessage.h:72-241`), a 450 × 142 modal with
//!   either Yes/No or a single OK. Three call sites use it, all listed in §4 of
//!   `docs/spec/06-dialogs.md`, and all three are in this crate's import/export flow.
//! * [`Toast`] is `FxNotification` (`GUI/FxNotification.cpp`), the rounded 216 × 80 bubble that
//!   grows to fit up to three lines of text and an optional link.
//!
//! ## `showMessage()` does not return a `bool` here
//!
//! The original blocks: `FxConfirmationMessage::showMessage(text, style) -> bool` runs a nested
//! modal loop and hands the answer straight back to its caller — which is how
//! `FxController::exportPresets()` can ask about a colliding file from inside a `for` loop
//! (`FxController.cpp:1403`). Nothing in egui can do that. [`MessageBox::show`] therefore returns
//! `Option<ConfirmChoice>`: `None` on every frame the user has not answered, `Some(..)` on the one
//! frame they do. The caller keeps the question in its own state and acts on the answer next
//! frame, which is the inversion `docs/spec/06-dialogs.md` Open question 3 asks for.
//!
//! ## Which of the toast's two modes survives on Wayland
//!
//! `FxNotification` has an autohide mode — its own borderless always-on-top window, positioned at
//! the corner of the display nearest the tray icon, fading in over 200 ms and expiring after 7 s
//! (8 s with a link) — and a persistent mode, where it is a child of the main view. **A Wayland
//! client can do neither the placement nor the always-on-top**, so §9.4 routes the autohide toast
//! to a real desktop notification over `org.freedesktop.Notifications`; that lives in the app
//! layer, which also owns the two timeouts. What is left here is the in-window error banner of
//! §6.5 — a child of the main window, where Wayland places no constraints — plus the autohide
//! *geometry*, kept because [`layout`] is the only place the original's sizing rules are written
//! down and a screenshot test of either mode has to agree with it.

use super::{ChromeResponse, DialogChrome, TextButton, draw_truncated, link, normal_font};
use crate::assets::{AssetCache, FxImage};
use crate::theme::{self, FxColor, Palette};
use crate::widgets::icon_button::{art_size, fitted_rect, paint_image};
use egui::{
    Align2, Color32, Context, CornerRadius, Id, Rect, Response, Sense, Shadow, Ui, Vec2, pos2, vec2,
};
use fxsound_core::i18n::tr;

// =============================================================================================
// FxConfirmationMessage
// =============================================================================================

/// `FxConfirmationMessage::Style` (`FxMessage.h:75`).
///
/// The C++ numbers them `YesNo = 1, OK = 2` and defaults the argument to `YesNo`, which is why
/// [`MessageBox::new`] does too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConfirmStyle {
    #[default]
    YesNo,
    Ok,
}

/// How the user answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfirmChoice {
    /// The Yes button, only reachable from [`ConfirmStyle::YesNo`].
    Yes,
    /// The No button, only reachable from [`ConfirmStyle::YesNo`].
    No,
    /// The OK button, only reachable from [`ConfirmStyle::Ok`].
    Ok,
    /// The window was closed without an answer — the ✕, the backdrop, or Escape.
    ///
    /// The original has no Escape handler at all and its ✕ returns `false`, i.e. "No"
    /// (`FxMessage.h:90-94`). `docs/spec/06-dialogs.md` §9.2 asks for that to be fixed, so
    /// dismissal is its own answer here and the caller decides whether it means "No" or "cancel
    /// the whole operation" — for the export overwrite prompt those are genuinely different.
    Dismissed,
}

/// Content size (`FxMessage.h:177-178`).
pub const CONTENT_SIZE: Vec2 = vec2(450.0, 142.0);
/// Outer window size, from the shared formula.
pub const WINDOW_SIZE: Vec2 = vec2(460.0, 229.0);
/// `MESSAGE_HEIGHT = (24 + 2) * 2` — two lines' worth of box (`FxMessage.h:179`).
pub const MESSAGE_HEIGHT: f32 = 52.0;
/// `BUTTON_WIDTH` × `BUTTON_HEIGHT` (`FxMessage.h:180-181`).
pub const BUTTON_SIZE: Vec2 = vec2(120.0, 30.0);
/// The layout's working rect starts 20 points down and is inset 20 on each side
/// (`FxMessage.h:183-186`).
pub const MARGIN: f32 = 20.0;
/// Gap between the message and the buttons, and between Yes and No (`FxMessage.h:196`, `:204`).
pub const GAP: f32 = 20.0;

/// `(20, 20, 410, 52)`, centred text (`FxMessage.h:188`).
#[must_use]
pub fn message_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(content.left() + MARGIN, content.top() + MARGIN),
        vec2(content.width() - MARGIN * 2.0, MESSAGE_HEIGHT),
    )
}

/// The row both buttons sit on: `message.bottom + 20`.
#[must_use]
fn button_top(content: Rect) -> f32 {
    message_rect(content).bottom() + GAP
}

/// `((450 - (120 * 2 + 20)) / 2, 92, 120, 30)` — the pair is centred in the *content*, not in the
/// working rect (`FxMessage.h:198-202`).
#[must_use]
pub fn yes_rect(content: Rect) -> Rect {
    let pair = BUTTON_SIZE.x * 2.0 + GAP;
    Rect::from_min_size(
        pos2(
            content.left() + (content.width() - pair) / 2.0,
            button_top(content),
        ),
        BUTTON_SIZE,
    )
}

/// `yes.right + 20` (`FxMessage.h:204`).
#[must_use]
pub fn no_rect(content: Rect) -> Rect {
    yes_rect(content).translate(vec2(BUTTON_SIZE.x + GAP, 0.0))
}

/// Horizontally centred in the working rect (`FxMessage.h:208-213`).
#[must_use]
pub fn ok_rect(content: Rect) -> Rect {
    let working = Rect::from_min_max(
        pos2(content.left() + MARGIN, button_top(content)),
        pos2(
            content.right() - MARGIN,
            button_top(content) + BUTTON_SIZE.y,
        ),
    );
    Align2::CENTER_TOP.align_size_within_rect(BUTTON_SIZE, working)
}

/// The modal message box.
pub struct MessageBox<'a> {
    text: &'a str,
    style: ConfirmStyle,
}

impl<'a> MessageBox<'a> {
    /// A Yes/No box, which is the C++ default argument (`FxMessage.h:99`).
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            style: ConfirmStyle::YesNo,
        }
    }

    /// A single-button box: `"Preset files not found in the selected folder."` and
    /// `"Presets are exported successfully!"` are the app's only two.
    #[must_use]
    pub fn ok(text: &'a str) -> Self {
        Self {
            text,
            style: ConfirmStyle::Ok,
        }
    }

    #[must_use]
    pub fn style(mut self, style: ConfirmStyle) -> Self {
        self.style = style;
        self
    }

    /// Draw the whole window — chrome and buttons — into `outer`, which should be [`WINDOW_SIZE`].
    ///
    /// Returns the answer on the frame the user gives one. The window has no name, so its title
    /// bar shows the wordmark and no title text (`FxMessage.h:77`).
    pub fn show(
        self,
        ui: &mut Ui,
        outer: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> Option<ConfirmChoice> {
        let Self { text, style } = self;
        let id = Id::new("fx_message_box").with(id_salt);

        let ChromeResponse {
            content,
            close_clicked,
            ..
        } = DialogChrome::new().draggable(false).show(
            ui,
            outer,
            palette,
            assets,
            id.with("chrome"),
        );

        draw_truncated(
            ui.painter(),
            text,
            normal_font(),
            palette.color(FxColor::DefaultText),
            message_rect(content),
            Align2::CENTER_CENTER,
        );

        let mut choice = close_clicked.then_some(ConfirmChoice::Dismissed);
        match style {
            ConfirmStyle::YesNo => {
                if TextButton::new(&tr("Yes"))
                    .show(ui, yes_rect(content), palette, id.with("yes"))
                    .clicked()
                {
                    choice = Some(ConfirmChoice::Yes);
                }
                if TextButton::new(&tr("No"))
                    .show(ui, no_rect(content), palette, id.with("no"))
                    .clicked()
                {
                    choice = Some(ConfirmChoice::No);
                }
            }
            ConfirmStyle::Ok => {
                if TextButton::new(&tr("OK"))
                    .show(ui, ok_rect(content), palette, id.with("ok"))
                    .clicked()
                {
                    choice = Some(ConfirmChoice::Ok);
                }
            }
        }
        choice
    }

    /// The same box as an [`egui::Modal`] in the current viewport.
    ///
    /// This is what `docs/spec/06-dialogs.md` §9.2 recommends for the message boxes: it reproduces
    /// the modality exactly, dims the backdrop and traps focus, none of which a second Wayland
    /// toplevel can be made to do. Escape and a backdrop click both report
    /// [`ConfirmChoice::Dismissed`].
    pub fn show_modal(
        self,
        ctx: &Context,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug + Clone,
    ) -> Option<ConfirmChoice> {
        let id = Id::new("fx_message_modal").with(id_salt.clone());
        let response = egui::Modal::new(id)
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(WINDOW_SIZE, Sense::hover());
                self.show(ui, rect, palette, assets, id_salt)
            });
        if response.inner.is_some() {
            return response.inner;
        }
        response.should_close().then_some(ConfirmChoice::Dismissed)
    }
}

// =============================================================================================
// FxNotification
// =============================================================================================

/// Toast geometry, all from `GUI/FxNotification.h:33-47`.
pub mod toast {
    use egui::{Pos2, Vec2, pos2, vec2};

    /// The width a toast starts at and only grows from.
    pub const WIDTH: f32 = 216.0;
    /// The height of a one-line toast.
    pub const HEIGHT: f32 = 80.0;
    /// The widest it may become.
    pub const MAX_WIDTH: f32 = 560.0;
    /// The tallest it may become, i.e. three lines.
    pub const MAX_HEIGHT: f32 = 120.0;
    /// The wordmark drawn in the autohide bubble.
    pub const ICON_SIZE: Vec2 = vec2(79.0, 12.0);
    /// …at this offset from the toast's top-left (`FxNotification.cpp:49`).
    pub const ICON_POS: Pos2 = pos2(15.0, 10.0);
    /// `fillRoundedRectangle(.., 16.0f)` (`FxNotification.cpp:211`).
    pub const CORNER_RADIUS: f32 = 16.0;
    /// The drop shadow's radius, as for every other window here.
    pub const SHADOW_RADIUS: f32 = 5.0;
    /// At most three lines are ever shown (`FxNotification.cpp:53`, `:103`, `:153`).
    pub const MAX_LINES: usize = 3;
    /// Pitch of the message lines.
    pub const LINE_HEIGHT: f32 = 20.0;
    /// y of the first line (`FxNotification.cpp:157`).
    pub const FIRST_LINE_Y: f32 = 30.0;
    /// `setSize(width, line_count * 20 + 60)` (`FxNotification.cpp:145`).
    pub const HEIGHT_PADDING: f32 = 60.0;
    /// Horizontal slack kept free of text in autohide mode (`FxNotification.cpp:125`).
    pub const AUTOHIDE_MARGIN: f32 = 80.0;
    /// …and in the in-window banner.
    pub const BANNER_MARGIN: f32 = 40.0;
    /// Left inset of the text in autohide mode (`FxNotification.cpp:153`).
    pub const AUTOHIDE_X: f32 = 40.0;
    /// …and in the in-window banner.
    pub const BANNER_X: f32 = 20.0;
    /// `getSmallFont().withHeight(17.0f)` — Gilroy **Regular** at 17, not the normal font
    /// (`FxNotification.cpp:80`).
    pub const FONT: f32 = 17.0;
    /// `BorderSize<int>(1, 0, 2, 0)` on each message label: one point off the top of its 20 point
    /// row and two off the bottom, which lifts the text half a point (`FxNotification.cpp:57`).
    pub const LINE_BORDER_TOP: f32 = 1.0;
    pub const LINE_BORDER_BOTTOM: f32 = 2.0;
}

/// The size and line assignment [`Toast`] resolved for a message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToastLayout {
    /// What `setSize` was called with.
    pub size: Vec2,
    /// How many message lines are drawn, never more than [`toast::MAX_LINES`].
    pub lines: usize,
    /// Which line the link sits on. Equal to the last line when the link fits after the text, one
    /// past it when the text had to take the full [`toast::MAX_WIDTH`].
    pub link_line: usize,
}

/// Resolve `setMessage`'s sizing rules (`FxNotification.cpp:92-145`).
///
/// `line_widths` are the measured widths of *every* line in the message, not just the three that
/// get drawn, because the original decides whether to attach the link by comparing against the
/// full line count. `link_width` is 0 when there is no link.
///
/// Two behaviours here look like mistakes and are reproduced deliberately, because they are what
/// the shipping app does:
///
/// * the link's width joins only the **last** line's measurement, so a long first line and a short
///   last one size the toast as if the link were free;
/// * `width` is assigned inside the loop, so the **last** line that overflows sets the width — not
///   the widest one. A wide second line followed by a barely-overflowing third shrinks the toast.
#[must_use]
pub fn layout(line_widths: &[f32], link_width: f32, autohide: bool) -> ToastLayout {
    let margin = if autohide {
        toast::AUTOHIDE_MARGIN
    } else {
        toast::BANNER_MARGIN
    };
    let drawn = line_widths.len().min(toast::MAX_LINES);
    let mut width = toast::WIDTH;
    let mut link_line = 0;

    for (index, text_width) in line_widths.iter().take(drawn).enumerate() {
        let link = if link_width > 0.0 && index + 1 == line_widths.len() {
            link_line = index;
            link_width
        } else {
            0.0
        };
        let line_width = text_width + link;
        if line_width > toast::WIDTH - margin {
            if line_width > toast::MAX_WIDTH - margin {
                width = toast::MAX_WIDTH;
                if link > 0.0 {
                    link_line = index + 1;
                }
            } else {
                width = line_width + margin;
            }
        }
    }

    ToastLayout {
        size: vec2(
            width,
            drawn as f32 * toast::LINE_HEIGHT + toast::HEIGHT_PADDING,
        ),
        lines: drawn,
        link_line,
    }
}

/// One message line's rectangle: `(x, i * 20 + 30, width - 2x, 20)` (`FxNotification.cpp:157`).
#[must_use]
pub fn line_rect(bounds: Rect, index: usize, autohide: bool) -> Rect {
    let x = if autohide {
        toast::AUTOHIDE_X
    } else {
        toast::BANNER_X
    };
    Rect::from_min_size(
        pos2(
            bounds.left() + x,
            bounds.top() + index as f32 * toast::LINE_HEIGHT + toast::FIRST_LINE_Y,
        ),
        vec2(bounds.width() - x * 2.0, toast::LINE_HEIGHT),
    )
}

/// The link's rectangle: inline after the last line's text when it shares that line, otherwise at
/// the left of the line below it (`FxNotification.cpp:166-177`).
#[must_use]
pub fn link_rect(
    bounds: Rect,
    layout: &ToastLayout,
    last_line_text_width: f32,
    link_width: f32,
    autohide: bool,
) -> Rect {
    let x = if autohide {
        toast::AUTOHIDE_X
    } else {
        toast::BANNER_X
    };
    let inline = layout.lines > 0 && layout.link_line == layout.lines - 1;
    let left = bounds.left() + x + if inline { last_line_text_width } else { 0.0 };
    Rect::from_min_size(
        pos2(
            left,
            bounds.top() + layout.link_line as f32 * toast::LINE_HEIGHT + toast::FIRST_LINE_Y,
        ),
        vec2(link_width, toast::LINE_HEIGHT),
    )
}

/// The justification of the message lines: centred without a link, left-aligned with one
/// (`FxNotification.cpp:113`, `:122`).
#[must_use]
pub fn line_align(has_link: bool) -> Align2 {
    if has_link {
        Align2::LEFT_CENTER
    } else {
        Align2::CENTER_CENTER
    }
}

/// What [`Toast::show`] reports.
#[derive(Debug, Clone)]
pub struct ToastResponse {
    /// The bubble itself, so the caller can hit-test it.
    pub response: Response,
    /// The link was clicked; open [`Toast::link`]'s URL.
    pub link_clicked: bool,
}

/// The rounded notification bubble.
pub struct Toast<'a> {
    message: &'a str,
    link: Option<(&'a str, &'a str)>,
    autohide: bool,
}

impl<'a> Toast<'a> {
    /// `message` is split on newlines and truncated to [`toast::MAX_LINES`]
    /// (`FxNotification.cpp:92-103`).
    #[must_use]
    pub fn new(message: &'a str) -> Self {
        Self {
            message,
            link: None,
            autohide: true,
        }
    }

    /// The trailing hyperlink. The original adds it only when **both** halves are non-empty
    /// (`FxMessage.cpp:64-70`), and its presence is also what switches the lines from centred to
    /// left-aligned.
    #[must_use]
    pub fn link(mut self, text: &'a str, url: &'a str) -> Self {
        if !text.is_empty() && !url.is_empty() {
            self.link = Some((text, url));
        }
        self
    }

    /// `false` draws the in-window error banner: no wordmark, a narrower margin and no timer
    /// (`FxNotification.cpp:196`, `FxView.cpp:58-77`).
    #[must_use]
    pub fn autohide(mut self, autohide: bool) -> Self {
        self.autohide = autohide;
        self
    }

    /// The lines this toast will draw, at most [`toast::MAX_LINES`].
    #[must_use]
    pub fn lines(&self) -> Vec<&'a str> {
        self.message.lines().take(toast::MAX_LINES).collect()
    }

    /// Measure the message and resolve [`layout`]; the caller needs this to size the rect it then
    /// passes to [`Toast::show`].
    #[must_use]
    pub fn measure(&self, ui: &Ui) -> ToastLayout {
        let font = theme::regular(toast::FONT);
        let widths: Vec<f32> = self
            .message
            .lines()
            .map(|line| {
                ui.painter()
                    .layout_no_wrap(line.to_owned(), font.clone(), Color32::PLACEHOLDER)
                    .size()
                    .x
            })
            .collect();
        layout(&widths, self.link_width(ui), self.autohide)
    }

    fn link_width(&self, ui: &Ui) -> f32 {
        self.link.map_or(0.0, |(text, _)| {
            ui.painter()
                .layout_no_wrap(text.to_owned(), normal_font(), Color32::PLACEHOLDER)
                .size()
                .x
        })
    }

    /// Draw the bubble into an exact rectangle.
    ///
    /// Pass [`ToastLayout::size`] from [`Toast::measure`] for the autohide bubble; the in-window
    /// banner is forced to `MAX_WIDTH × MAX_HEIGHT` instead (`FxView.cpp:66-69`).
    pub fn show(
        self,
        ui: &mut Ui,
        rect: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> ToastResponse {
        let id = Id::new("fx_toast").with(id_salt);
        let corner = CornerRadius::same(toast::CORNER_RADIUS as u8);

        let shadow = Shadow {
            offset: [0, 0],
            blur: toast::SHADOW_RADIUS as u8,
            spread: 0,
            color: super::SHADOW_COLOUR,
        }
        .as_shape(rect, corner);
        ui.painter().add(shadow);
        ui.painter()
            .rect_filled(rect, corner, palette.color(FxColor::DefaultFill));

        if self.autohide {
            let box_ = Rect::from_min_size(rect.min + toast::ICON_POS.to_vec2(), toast::ICON_SIZE);
            paint_image(
                ui,
                fitted_rect(box_, art_size(FxImage::DefaultLogo)),
                FxImage::DefaultLogo,
                palette.mode(),
                assets,
                1.0,
            );
        }

        let resolved = self.measure(ui);
        let lines = self.lines();
        let font = theme::regular(toast::FONT);
        let colour = palette.color(FxColor::DefaultText);
        let align = line_align(self.link.is_some());
        let mut last_line_width = 0.0;
        for (index, line) in lines.iter().enumerate() {
            let row = line_rect(rect, index, self.autohide);
            let text_area = Rect::from_min_max(
                row.min + vec2(0.0, toast::LINE_BORDER_TOP),
                row.max - vec2(0.0, toast::LINE_BORDER_BOTTOM),
            );
            let placed = draw_truncated(ui.painter(), line, font.clone(), colour, text_area, align);
            last_line_width = placed.width();
        }

        let mut link_clicked = false;
        if let Some((text, _)) = self.link {
            let width = self.link_width(ui);
            let area = link_rect(rect, &resolved, last_line_width, width, self.autohide);
            link_clicked = link(
                ui,
                area,
                text,
                palette,
                Align2::LEFT_CENTER,
                id.with("link"),
            )
            .clicked();
        }

        // `Sense::hover()` because the in-window banner stays up while the pointer is over it
        // (`FxView.cpp:207-214`); it is never a click target itself.
        ToastResponse {
            response: ui.interact(rect, id, Sense::hover()),
            link_clicked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{frame, test_context};
    use super::*;
    use fxsound_core::ThemeMode;

    fn content() -> Rect {
        Rect::from_min_size(pos2(5.0, 62.0), CONTENT_SIZE)
    }

    #[test]
    fn the_confirmation_box_is_four_hundred_and_sixty_by_two_hundred_and_twenty_nine() {
        assert!((super::super::outer_size(CONTENT_SIZE) - WINDOW_SIZE).length() < 1e-4);
        // MESSAGE_HEIGHT = (24 + 2) * 2.
        assert!((MESSAGE_HEIGHT - (24.0 + 2.0) * 2.0).abs() < 1e-6);
    }

    #[test]
    fn every_widget_lands_where_the_specs_table_says() {
        // docs/spec/06-dialogs.md §4, in content-local coordinates.
        let content = content();
        let local = |r: Rect| r.translate(-content.min.to_vec2());

        let message = local(message_rect(content));
        assert!(
            (message.min - pos2(20.0, 20.0)).length() < 1e-4,
            "{message:?}"
        );
        assert!((message.size() - vec2(410.0, 52.0)).length() < 1e-4);

        let yes = local(yes_rect(content));
        assert!((yes.min - pos2(95.0, 92.0)).length() < 1e-4, "{yes:?}");
        assert!((yes.size() - BUTTON_SIZE).length() < 1e-4);

        let no = local(no_rect(content));
        assert!((no.min - pos2(235.0, 92.0)).length() < 1e-4, "{no:?}");

        let ok = local(ok_rect(content));
        assert!((ok.min - pos2(165.0, 92.0)).length() < 1e-4, "{ok:?}");
    }

    #[test]
    fn the_ok_button_sits_between_the_yes_and_no_buttons() {
        let content = content();
        let ok = ok_rect(content);
        assert!(ok.left() > yes_rect(content).left());
        assert!(ok.right() < no_rect(content).right());
        assert!((ok.center().x - content.center().x).abs() < 1e-4);
    }

    #[test]
    fn a_yes_no_box_draws_both_buttons_and_answers_nothing_on_its_own() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), WINDOW_SIZE);
        frame(&ctx, |ui| {
            let answer = MessageBox::new("Preset file Rock already exists in the export path, do you want to overwrite the preset file?")
                .show(ui, outer, Palette::new(ThemeMode::Dark), &mut assets, "overwrite");
            assert_eq!(answer, None, "nothing was clicked, so nothing was answered");
        });
    }

    #[test]
    fn an_ok_box_draws_in_both_palettes() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), WINDOW_SIZE);
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            frame(&ctx, |ui| {
                let answer = MessageBox::ok("Preset files not found in the selected folder.").show(
                    ui,
                    outer,
                    Palette::new(mode),
                    &mut assets,
                    ("not-found", mode as u8),
                );
                assert_eq!(answer, None);
            });
        }
    }

    #[test]
    fn the_modal_variant_runs_and_reports_nothing_until_it_is_answered() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        frame(&ctx, |ui| {
            let answer = MessageBox::ok("Presets are exported successfully!").show_modal(
                ui.ctx(),
                Palette::new(ThemeMode::Dark),
                &mut assets,
                "exported",
            );
            assert_eq!(answer, None);
        });
    }

    // ---- toast --------------------------------------------------------------------------------

    #[test]
    fn a_short_single_line_toast_keeps_the_default_size() {
        // 216 - 80 = 136 points of room in autohide mode.
        let resolved = layout(&[100.0], 0.0, true);
        assert!((resolved.size - vec2(toast::WIDTH, toast::HEIGHT)).length() < 1e-4);
        assert_eq!(resolved.lines, 1);
    }

    #[test]
    fn each_extra_line_adds_twenty_points_up_to_the_maximum() {
        for (count, height) in [(1, 80.0), (2, 100.0), (3, 120.0)] {
            let widths = vec![10.0; count];
            let resolved = layout(&widths, 0.0, true);
            assert!(
                (resolved.size.y - height).abs() < 1e-4,
                "{count} lines came out {} tall",
                resolved.size.y
            );
            assert!(resolved.size.y <= toast::MAX_HEIGHT);
        }
        // A fourth line is dropped, not drawn.
        let resolved = layout(&[10.0; 5], 0.0, true);
        assert_eq!(resolved.lines, toast::MAX_LINES);
        assert!((resolved.size.y - toast::MAX_HEIGHT).abs() < 1e-4);
    }

    #[test]
    fn a_line_wider_than_the_bubble_grows_it_by_exactly_the_margin() {
        // 200 > 216 - 80, and 200 < 560 - 80, so width = 200 + 80.
        let resolved = layout(&[200.0], 0.0, true);
        assert!((resolved.size.x - 280.0).abs() < 1e-4, "{resolved:?}");
        // The in-window banner uses the smaller margin: 200 > 216 - 40 gives 200 + 40.
        let resolved = layout(&[200.0], 0.0, false);
        assert!((resolved.size.x - 240.0).abs() < 1e-4, "{resolved:?}");
    }

    #[test]
    fn a_line_wider_than_the_maximum_clamps_and_pushes_the_link_down_a_row() {
        // 500 > 560 - 80, so the toast takes MAX_WIDTH and the link cannot share the line.
        let resolved = layout(&[480.0], 30.0, true);
        assert!((resolved.size.x - toast::MAX_WIDTH).abs() < 1e-4);
        assert_eq!(
            resolved.link_line, 1,
            "the link should drop to the next row"
        );
        // Narrow enough and the link stays inline on the last line.
        let resolved = layout(&[100.0, 120.0], 30.0, true);
        assert_eq!(resolved.link_line, 1);
        assert_eq!(resolved.lines, 2);
    }

    #[test]
    fn the_link_only_widens_the_last_line_and_the_last_overflow_wins() {
        // Faithful to FxNotification.cpp:125-140: `width` is assigned inside the loop, so the
        // *last* overflowing line decides it even when an earlier one was wider.
        let resolved = layout(&[400.0, 200.0], 0.0, true);
        assert!(
            (resolved.size.x - 280.0).abs() < 1e-4,
            "the second line should have overwritten the first's 480, got {resolved:?}"
        );
        // The 300 point link is not added to the first line's 100 points, so line 0 never overflows.
        let resolved = layout(&[100.0, 100.0], 300.0, true);
        assert!((resolved.size.x - 480.0).abs() < 1e-4, "{resolved:?}");
    }

    #[test]
    fn a_link_on_a_message_with_more_lines_than_fit_is_never_attached() {
        // The loop breaks at three lines, so `i == lines.size() - 1` is never reached and the
        // link stays on row 0 — a real quirk of the original, reproduced here.
        let resolved = layout(&[10.0; 5], 50.0, true);
        assert_eq!(resolved.link_line, 0);
        assert!((resolved.size.x - toast::WIDTH).abs() < 1e-4);
    }

    #[test]
    fn line_rectangles_step_by_twenty_points_from_thirty() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(toast::WIDTH, toast::MAX_HEIGHT));
        for index in 0..toast::MAX_LINES {
            let row = line_rect(bounds, index, true);
            assert!((row.top() - (index as f32 * 20.0 + 30.0)).abs() < 1e-4);
            assert!((row.height() - 20.0).abs() < 1e-4);
            assert!((row.left() - toast::AUTOHIDE_X).abs() < 1e-4);
            assert!((row.width() - (toast::WIDTH - 80.0)).abs() < 1e-4);
        }
        // The banner insets by 20 instead of 40.
        assert!((line_rect(bounds, 0, false).left() - toast::BANNER_X).abs() < 1e-4);
    }

    #[test]
    fn an_inline_link_starts_after_the_last_lines_text_and_a_dropped_one_does_not() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(toast::MAX_WIDTH, 120.0));
        let inline = ToastLayout {
            size: bounds.size(),
            lines: 2,
            link_line: 1,
        };
        let rect = link_rect(bounds, &inline, 150.0, 40.0, true);
        assert!(
            (rect.left() - (toast::AUTOHIDE_X + 150.0)).abs() < 1e-4,
            "{rect:?}"
        );
        assert!((rect.top() - (20.0 + 30.0)).abs() < 1e-4);

        let dropped = ToastLayout {
            size: bounds.size(),
            lines: 1,
            link_line: 1,
        };
        let rect = link_rect(bounds, &dropped, 150.0, 40.0, true);
        assert!((rect.left() - toast::AUTOHIDE_X).abs() < 1e-4, "{rect:?}");
    }

    #[test]
    fn the_lines_are_centred_only_when_there_is_no_link() {
        assert_eq!(line_align(false), Align2::CENTER_CENTER);
        assert_eq!(line_align(true), Align2::LEFT_CENTER);
    }

    #[test]
    fn an_empty_link_half_is_the_same_as_no_link_at_all() {
        let toast = Toast::new("Output Disconnected").link("", "https://example.invalid");
        assert!(toast.link.is_none());
        let toast = Toast::new("Output Disconnected").link("steps.", "");
        assert!(toast.link.is_none());
    }

    #[test]
    fn a_toast_draws_its_three_lines_and_its_link() {
        // The in-window error banner of §6.5, forced to 560 x 120.
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(toast::MAX_WIDTH, toast::MAX_HEIGHT));
        frame(&ctx, |ui| {
            let toast = Toast::new(
                "FxSound is unable to play processed audio through the selected output device.\n\
                 Another application could be using it in exclusive mode or the device could be\n\
                 disconnected. To disable exclusive mode follow these ",
            )
            .link(
                "steps.",
                "https://www.fxsound.com/learning-center/no-sound-with-fxsound-realtek",
            )
            .autohide(false);
            assert_eq!(toast.lines().len(), 3);
            let response = toast.show(
                ui,
                rect,
                Palette::new(ThemeMode::Dark),
                &mut assets,
                "error-banner",
            );
            assert!(!response.link_clicked);
            assert!((response.response.rect.size() - rect.size()).length() < 1e-4);
        });
    }

    #[test]
    fn a_measured_toast_sizes_itself_between_the_two_limits() {
        let ctx = test_context();
        frame(&ctx, |ui| {
            let short = Toast::new("Preset: Rock").measure(ui);
            assert!((short.size - vec2(toast::WIDTH, toast::HEIGHT)).length() < 1e-4);

            let long = Toast::new(
                "FxSound is unable to play processed audio through the selected output device.",
            )
            .measure(ui);
            assert!(long.size.x > toast::WIDTH, "{long:?}");
            assert!(long.size.x <= toast::MAX_WIDTH);
            assert_eq!(long.lines, 1);
        });
    }

    #[test]
    fn a_message_line_gives_up_three_of_its_twenty_points_to_its_border() {
        // BorderSize<int>(1, 0, 2, 0) — FxNotification.cpp:57.
        assert!(
            (toast::LINE_HEIGHT - toast::LINE_BORDER_TOP - toast::LINE_BORDER_BOTTOM - 17.0).abs()
                < 1e-6
        );
    }
}
