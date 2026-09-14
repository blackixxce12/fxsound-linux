//! The bundled changelog, opened from Settings ▸ Help.
//!
//! The original's "Changelog" row is a hyperlink to `https://www.fxsound.com/changelog`
//! (`FxSettingsDialog.cpp:474-475`). This fork never sends the user to the upstream site, so the
//! row opens the package's own `CHANGELOG.md` in a pane with the same chrome as Settings. The
//! text is Markdown of the Keep-a-Changelog kind and is rendered with the four things such a file
//! actually uses — headings, bullets, emphasis and links — rather than with a Markdown engine.

use egui::{Id, Key, Rect, RichText, ScrollArea, Ui, UiBuilder, Vec2, vec2};
use fxsound_core::i18n::tr;

use super::{ChromeResponse, DialogChrome, DialogResponse};
use crate::{AssetCache, FxColor, Palette, theme};

/// The same window as Settings, so opening one over the other changes nothing about the frame.
pub const WINDOW_SIZE: Vec2 = super::settings::WINDOW_SIZE;

/// Horizontal and vertical inset of the text from the content area.
pub const TEXT_INSET: Vec2 = vec2(20.0, 10.0);

/// What the pane asks of its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangelogAction {
    /// The ✕ or Escape.
    Close,
}

/// One line of the changelog, classified for drawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// `#`, `##` or `###` heading; the level is 1..=3.
    Heading(u8, String),
    /// A `- ` bullet.
    Bullet(String),
    /// A line of text, wrapped as prose.
    Text(String),
    /// Vertical breathing space.
    Blank,
}

/// The file's lines folded into blocks: a heading, a bullet or a paragraph per block, with the
/// hard-wrapped continuation lines of the source joined back onto the block they belong to, as
/// Markdown reads them.
#[must_use]
pub fn blocks(text: &str) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    for line in text.lines() {
        match classify(line) {
            Line::Text(more) => match out.last_mut() {
                Some(Line::Bullet(current) | Line::Text(current)) => {
                    current.push(' ');
                    current.push_str(&more);
                }
                _ => out.push(Line::Text(more)),
            },
            other => out.push(other),
        }
    }
    out
}

/// Classify one Markdown line and strip the inline markers the changelog uses.
#[must_use]
pub fn classify(line: &str) -> Line {
    let trimmed = line.trim_end();
    if trimmed.trim().is_empty() {
        return Line::Blank;
    }
    let level = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=3).contains(&level) && trimmed[level..].starts_with(' ') {
        return Line::Heading(level as u8, plain(trimmed[level..].trim()));
    }
    if let Some(item) = trimmed.trim_start().strip_prefix("- ") {
        return Line::Bullet(plain(item));
    }
    Line::Text(plain(trimmed.trim_start()))
}

/// Drop `*`/`**` emphasis, backticks and the `(url)` half of a `[label](url)` link.
#[must_use]
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**") {
            rest = after;
        } else if let Some(after) = rest.strip_prefix('*') {
            rest = after;
        } else if let Some(after) = rest.strip_prefix('`') {
            rest = after;
        } else if rest.starts_with('[')
            && let Some(close) = rest.find("](")
            && let Some(end) = rest[close..].find(')')
        {
            out.push_str(&rest[1..close]);
            rest = &rest[close + end + 1..];
        } else {
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                out.push(c);
            }
            rest = chars.as_str();
        }
    }
    out
}

/// The changelog pane.
pub struct ChangelogPane<'a> {
    text: &'a str,
}

impl<'a> ChangelogPane<'a> {
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        Self { text }
    }

    /// Draw the whole window into `outer`, which should be [`WINDOW_SIZE`].
    pub fn show(
        self,
        ui: &mut Ui,
        outer: Rect,
        palette: Palette,
        assets: &mut AssetCache,
    ) -> DialogResponse<ChangelogAction> {
        let id = Id::new("fx_changelog_pane");
        let mut response = DialogResponse::default();
        let ChromeResponse {
            content,
            close_clicked,
            ..
        } = DialogChrome::titled(&tr("Changelog")).show(ui, outer, palette, assets, id.with("chrome"));
        response.push_if(close_clicked, ChangelogAction::Close);
        response.push_if(
            ui.input(|i| i.key_pressed(Key::Escape)),
            ChangelogAction::Close,
        );

        let inner = Rect::from_min_max(content.min + TEXT_INSET, content.max - TEXT_INSET);
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner).id_salt(id.with("text")));
        child.set_clip_rect(inner);
        let body = palette.color(FxColor::DefaultText);
        let heading = palette.color(FxColor::HighlightedText);
        ScrollArea::vertical()
            .id_salt(id.with("scroll"))
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                ui.set_width(inner.width() - 16.0);
                ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
                for block in blocks(self.text) {
                    match block {
                        Line::Heading(level, text) => {
                            let font = match level {
                                1 => theme::bold(20.0),
                                2 => theme::semibold(17.0),
                                _ => theme::semibold(15.0),
                            };
                            ui.add_space(if level == 1 { 2.0 } else { 8.0 });
                            ui.label(RichText::new(text).font(font).color(heading));
                        }
                        Line::Bullet(text) => {
                            ui.horizontal_top(|ui| {
                                ui.add_space(8.0);
                                ui.label(RichText::new("•").font(theme::regular(14.0)).color(body));
                                ui.add_space(4.0);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(text).font(theme::regular(14.0)).color(body),
                                    )
                                    .wrap(),
                                );
                            });
                        }
                        Line::Text(text) => {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text).font(theme::regular(14.0)).color(body),
                                )
                                .wrap(),
                            );
                        }
                        Line::Blank => ui.add_space(6.0),
                    }
                }
            });
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::ThemeMode;

    #[test]
    fn markdown_lines_are_classified_and_stripped() {
        assert_eq!(classify("# Changelog"), Line::Heading(1, "Changelog".into()));
        assert_eq!(
            classify("## [0.2.0] — 2026-09-14"),
            Line::Heading(2, "[0.2.0] — 2026-09-14".into())
        );
        assert_eq!(classify("### Added"), Line::Heading(3, "Added".into()));
        assert_eq!(
            classify("- **Input mode.** Pick a `microphone`."),
            Line::Bullet("Input mode. Pick a microphone.".into())
        );
        assert_eq!(
            classify("  continuation of a bullet"),
            Line::Text("continuation of a bullet".into())
        );
        assert_eq!(classify(""), Line::Blank);
        assert_eq!(classify("   "), Line::Blank);
        assert_eq!(classify("#hashtag"), Line::Text("#hashtag".into()));
    }

    #[test]
    fn hard_wrapped_lines_join_their_bullet_or_paragraph() {
        let text = "## Added\n- One thing that is long\n  and continues here.\n- Two.\n\nA paragraph\nwrapped twice\nover.\n";
        assert_eq!(
            blocks(text),
            vec![
                Line::Heading(2, "Added".into()),
                Line::Bullet("One thing that is long and continues here.".into()),
                Line::Bullet("Two.".into()),
                Line::Blank,
                Line::Text("A paragraph wrapped twice over.".into()),
            ]
        );
    }

    #[test]
    fn links_keep_their_label_and_lose_their_url() {
        assert_eq!(
            plain("follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); done"),
            "follows Keep a Changelog; done"
        );
        assert_eq!(plain("a [broken link"), "a [broken link");
        assert_eq!(plain("`code` and **bold** and *italic*"), "code and bold and italic");
    }

    #[test]
    fn the_pane_draws_the_bundled_changelog_and_closes_on_escape() {
        let text = "# Changelog\n\n## [0.2.0]\n\n### Added\n- One thing.\n- Another.\n";
        let palette = Palette::new(ThemeMode::Dark);
        let mut assets = AssetCache::new();
        let mut closed = false;
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, palette);
        let quiet = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, WINDOW_SIZE)),
            ..Default::default()
        };
        let mut output = ctx.run_ui(quiet, |ui| {
            let outer = Rect::from_min_size(egui::pos2(0.0, 0.0), WINDOW_SIZE);
            let response = ChangelogPane::new(text).show(ui, outer, palette, &mut assets);
            closed |= response.contains(&ChangelogAction::Close);
        });
        output.textures_delta.clear();
        assert!(!closed, "nothing asked to close");

        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, WINDOW_SIZE)),
            events: vec![egui::Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let outer = Rect::from_min_size(egui::pos2(0.0, 0.0), WINDOW_SIZE);
            let response = ChangelogPane::new(text).show(ui, outer, palette, &mut assets);
            closed |= response.contains(&ChangelogAction::Close);
        });
        output.textures_delta.clear();
        assert!(closed, "Escape closes the pane");
    }
}
