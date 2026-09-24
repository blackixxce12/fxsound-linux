//! The modal message box.
//!
//! [`MessageBox`] is `FxConfirmationMessage` (`GUI/FxMessage.h:72-241`), a 450 × 142 modal with
//! either Yes/No or a single OK. The original's three call sites, all listed in §4 of
//! `docs/spec/06-dialogs.md`, are in this crate's import/export flow; the app adds two, the
//! questions before Delete Preset and Reset presets (0.4.0 audit #16, #21).
//!
//! The original's other way of talking back, `FxNotification` (`GUI/FxNotification.cpp`), the
//! rounded bubble at the corner of the display, is a desktop notification here
//! (`org.freedesktop.Notifications`, in the app's `notify` module): a Wayland client can neither
//! place a window of its own at a corner nor keep it on top. Its in-window twin, a port of the
//! bubble with its sizing rules — two of them the original's layout mistakes, reproduced — was
//! never shown anywhere, and went in 0.4.0 (audit #38).
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
//! ## The message wraps
//!
//! The original's message is a JUCE `Label` two lines tall (`MESSAGE_HEIGHT`), which wraps the
//! text over both lines and squeezes each one to as little as 0.7 of its width before it elides
//! (`LookAndFeel_V2::drawLabel`, `GlyphArrangement::addFittedText`). 0.3.0 drew it on one line,
//! elided, so the overwrite question lost the question (0.4.0 audit #49). [`message_galley`] wraps
//! it again, centred line by line, and where two lines of the normal font do not hold it — a
//! longer translation, a long preset name — takes the font down, to no less than 0.7 of it,
//! because egui cannot squeeze a glyph sideways. Only a message too long even for that is elided.

use super::{ChromeResponse, DialogChrome, NORMAL_FONT, TextButton};
use crate::assets::AssetCache;
use crate::theme::{self, FxColor, Palette};
use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align, Align2, Color32, Context, Galley, Id, Pos2, Rect, Sense, TextFormat, Ui, Vec2, pos2,
    vec2,
};
use fxsound_core::i18n::tr;
use std::sync::Arc;

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

/// The smallest size the message is taken down to: 0.7 of the normal font's 17 px, the least a
/// JUCE label squeezes a line to (`Font::getDefaultMinimumHorizontalScaleFactor`).
pub const MIN_MESSAGE_FONT: f32 = 12.0;

/// How far the message's font is taken down at a time, looking for a size that fits.
const MESSAGE_FONT_STEP: f32 = 0.5;

/// Lay `text` out as the box shows it in its message rect ([`message_rect`], 410 × 52): wrapped at
/// the rect's width and centred line by line, in the normal font when that fits the rect's height —
/// two lines — and otherwise in the largest size down to [`MIN_MESSAGE_FONT`] that does. A message
/// that not even the smallest size holds fills the lines that fit and is elided on the last, which
/// [`Galley::elided`] reports (see the module's "The message wraps").
///
/// The galley is centred on x = 0 ([`Align::Center`]): paint it at the rect's centre, as
/// [`MessageBox::show`] does. `ctx` must be in a pass: fonts exist from the first one on.
#[must_use]
pub fn message_galley(ctx: &Context, text: &str, colour: Color32) -> Arc<Galley> {
    let rect = message_rect(Rect::from_min_size(Pos2::ZERO, CONTENT_SIZE));
    let layout = |job: LayoutJob| ctx.fonts_mut(|fonts| fonts.layout_job(job));
    let job = |size: f32, max_rows: usize| {
        let mut job = LayoutJob::single_section(
            text.to_owned(),
            TextFormat::simple(theme::semibold(size), colour),
        );
        job.wrap = TextWrapping {
            max_width: rect.width().max(0.0),
            max_rows,
            break_anywhere: false,
            overflow_character: Some('…'),
        };
        job.halign = Align::Center;
        job
    };
    let fits = |galley: &Galley| galley.size().y <= rect.height() + 0.01;

    let mut size = NORMAL_FONT;
    loop {
        let galley = layout(job(size, usize::MAX));
        if fits(&galley) {
            return galley;
        }
        if size <= MIN_MESSAGE_FONT {
            break;
        }
        size = (size - MESSAGE_FONT_STEP).max(MIN_MESSAGE_FONT);
    }
    let row = ctx.fonts_mut(|fonts| fonts.row_height(&theme::semibold(MIN_MESSAGE_FONT)));
    let rows = ((rect.height() / row.max(1.0)).floor() as usize).max(1);
    layout(job(MIN_MESSAGE_FONT, rows))
}

/// `template` with its first `%s` replaced by `name`, as `FxController::FormatString` does, and
/// the name cut in the middle — `Rock Ball…Night` — as far as it takes for the whole message to fit
/// the box unelided ([`message_galley`]).
///
/// A preset name may be sixty-four of the widest letters there are, and the words after it — "to
/// the trash?", "do you want to overwrite the preset file?" — are the question; an elision at the
/// end would cut those. Every name of ordinary words fits whole in every language.
#[must_use]
pub fn message_with_name(ctx: &Context, template: &str, name: &str) -> String {
    let with = |name: &str| template.replacen("%s", name, 1);
    let whole = with(name);
    if name.is_empty() || !message_galley(ctx, &whole, Color32::PLACEHOLDER).elided {
        return whole;
    }
    let chars: Vec<char> = name.chars().collect();
    let cut = |kept: usize| {
        let head = kept.div_ceil(2);
        let tail = kept / 2;
        let mut short: String = chars[..head].iter().collect();
        short.push('…');
        short.extend(&chars[chars.len() - tail..]);
        with(&short)
    };
    // The most of the name that fits: fewer letters never make the message longer.
    let (mut fits, mut too_many) = (0, chars.len());
    while too_many - fits > 1 {
        let kept = fits + (too_many - fits) / 2;
        if message_galley(ctx, &cut(kept), Color32::PLACEHOLDER).elided {
            too_many = kept;
        } else {
            fits = kept;
        }
    }
    cut(fits)
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

        let colour = palette.color(FxColor::DefaultText);
        let area = message_rect(content);
        let galley = message_galley(ui.ctx(), text, colour);
        let top = area.center().y - galley.size().y / 2.0;
        ui.painter()
            .galley(pos2(area.center().x, top), galley, colour);

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

#[cfg(test)]
mod tests {
    use super::super::tests::{every_translation, frame, test_context};
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

    // ---- the message ------------------------------------------------------------------------

    /// The font size a message galley was laid out at.
    fn font_size(galley: &Galley) -> f32 {
        galley.job.sections[0].format.font_id.size
    }

    /// Whether `galley` sits whole inside the 410 × 52 message rect.
    fn fits_the_box(galley: &Galley) -> bool {
        !galley.elided
            && galley.size().y <= MESSAGE_HEIGHT + 0.01
            && galley.size().x <= CONTENT_SIZE.x - MARGIN * 2.0 + 0.01
    }

    /// Names a preset can have: one of ordinary words, one of sixty-four characters, and the
    /// widest the name field lets through — sixty-four capital Ws, and the 126 bytes of Cyrillic
    /// and of CJK a Windows FxSound reads a name in.
    fn preset_names() -> [String; 5] {
        [
            "Rock Ballad Extended Night".to_owned(),
            "Rock Ballad Extended Night Mix For The Living Room Speakers 2026".to_owned(),
            "W".repeat(64),
            "Ш".repeat(63),
            "音".repeat(42),
        ]
    }

    #[test]
    fn the_overwrite_question_wraps_over_both_lines_instead_of_losing_the_question() {
        // 0.4.0 audit #49: "Preset file Rock already exists in the export p…" and two buttons.
        let ctx = test_context();
        frame(&ctx, |ui| {
            let text = super::super::presets::format_string(
                super::super::presets::OVERWRITE_MESSAGE,
                "Rock",
            );
            let galley = message_galley(ui.ctx(), &text, Color32::WHITE);
            assert!(fits_the_box(&galley), "{:?}", galley.size());
            assert_eq!(galley.rows.len(), 2, "two lines, as the original's label");
            assert!(
                (font_size(&galley) - NORMAL_FONT).abs() < 1e-6,
                "in the normal font"
            );
        });
    }

    #[test]
    fn a_message_two_lines_cannot_hold_is_drawn_smaller_rather_than_cut() {
        let ctx = test_context();
        frame(&ctx, |ui| {
            let text = super::super::presets::format_string(
                super::super::presets::OVERWRITE_MESSAGE,
                "Rock Ballad Extended Night Mix For The Living Room Speakers 2026",
            );
            let galley = message_galley(ui.ctx(), &text, Color32::WHITE);
            assert!(fits_the_box(&galley), "{:?}", galley.size());
            let size = font_size(&galley);
            assert!((MIN_MESSAGE_FONT..NORMAL_FONT).contains(&size), "{size}");
            // A short one stays in the normal font on one line, centred.
            let galley = message_galley(
                ui.ctx(),
                "Presets are exported successfully!",
                Color32::WHITE,
            );
            assert_eq!(galley.rows.len(), 1);
            assert!((font_size(&galley) - NORMAL_FONT).abs() < 1e-6);
            assert!(
                (galley.rect.center().x).abs() < 1.0,
                "centred on the anchor: {:?}",
                galley.rect
            );
        });
    }

    #[test]
    fn a_message_too_long_even_for_the_smallest_font_is_elided_inside_the_box() {
        let ctx = test_context();
        frame(&ctx, |ui| {
            let text = "Preset ".repeat(60);
            let galley = message_galley(ui.ctx(), &text, Color32::WHITE);
            assert!(galley.elided, "{} rows", galley.rows.len());
            assert!(
                galley.size().y <= MESSAGE_HEIGHT + 0.01,
                "{:?}",
                galley.size()
            );
            assert!((font_size(&galley) - MIN_MESSAGE_FONT).abs() < 1e-6);
            const { assert!(MIN_MESSAGE_FONT >= NORMAL_FONT * 0.7) };
        });
    }

    #[test]
    fn a_name_too_wide_for_the_box_is_cut_in_the_middle_and_the_question_stays_whole() {
        let ctx = test_context();
        frame(&ctx, |ui| {
            let template = super::super::presets::OVERWRITE_MESSAGE;
            let name = "W".repeat(64);
            let text = message_with_name(ui.ctx(), template, &name);
            assert!(fits_the_box(&message_galley(
                ui.ctx(),
                &text,
                Color32::WHITE
            )));
            assert!(text.starts_with("Preset file WWW"), "{text}");
            assert!(
                text.ends_with("WWW already exists in the export path, do you want to overwrite the preset file?"),
                "{text}"
            );
            assert!(text.contains('…'), "{text}");
            assert!(
                text.chars().count() > template.len(),
                "as much of the name as fits: {text}"
            );
            // A name that fits is left whole, and a template without a placeholder alone.
            assert_eq!(
                message_with_name(ui.ctx(), template, "Rock"),
                template.replacen("%s", "Rock", 1)
            );
            assert_eq!(
                message_with_name(ui.ctx(), "No name here.", &name),
                "No name here."
            );
        });
    }

    #[test]
    fn every_message_the_import_and_export_windows_show_fits_the_box_in_every_language() {
        use super::super::presets::{
            EXPORT_SUCCEEDED, NO_PRESETS_FOUND, OVERWRITE_MESSAGE, OVERWRITE_MESSAGE_PLURAL,
        };
        let ctx = test_context();
        frame(&ctx, |ui| {
            for key in [NO_PRESETS_FOUND, EXPORT_SUCCEEDED] {
                for (code, text) in every_translation(key) {
                    let galley = message_galley(ui.ctx(), &text, Color32::WHITE);
                    assert!(fits_the_box(&galley), "{code}: {text}");
                }
            }
            for (code, template) in every_translation(OVERWRITE_MESSAGE_PLURAL) {
                let text = template.replacen("%s", "120", 1);
                let galley = message_galley(ui.ctx(), &text, Color32::WHITE);
                assert!(fits_the_box(&galley), "{code}: {text}");
            }
            for (code, template) in every_translation(OVERWRITE_MESSAGE) {
                for (index, name) in preset_names().iter().enumerate() {
                    let text = message_with_name(ui.ctx(), &template, name);
                    let galley = message_galley(ui.ctx(), &text, Color32::WHITE);
                    assert!(fits_the_box(&galley), "{code}: {text}");
                    let (before, after) = template.split_once("%s").expect("a placeholder");
                    assert!(
                        text.starts_with(before) && text.ends_with(after),
                        "{code}: the question is whole: {text}"
                    );
                    if index < 2 {
                        assert!(
                            text.contains(name.as_str()),
                            "{code}: the name is whole: {text}"
                        );
                    }
                }
            }
        });
    }
}
