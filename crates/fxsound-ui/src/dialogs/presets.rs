//! Importing and exporting presets.
//!
//! Three windows and one prompt, ported from `GUI/FxPresetImportDialog.{h,cpp}` and
//! `GUI/FxPresetExportDialog.{h,cpp}`:
//!
//! | Window | Original | Here |
//! |---|---|---|
//! | Import | 410 × 487, with an embedded `FileBrowserComponent` | [`ImportDialog`], shrunk — see below |
//! | Import summary | 360 × 427, `FxImportCompleteMessage` | [`ImportDialog`] again, driven by [`ImportState::summary`] |
//! | Export | 410 × 492, multi-select list + animated progress bar | [`ExportDialog`] |
//! | Overwrite prompt | a Yes/No box per colliding file | [`ExportDialog`], asked **once** |
//!
//! ## The file browser is gone
//!
//! `docs/spec/06-dialogs.md` §9.2 is blunt about it: do not re-implement
//! `FileBrowserComponent`. The 360 × 310 embedded browser becomes a native folder picker behind
//! the desktop portal (§9.3), which gets the user their own file manager, their bookmarks and
//! their recent folders, and is mandatory under Flatpak. Opening it is **not this crate's job** —
//! [`PresetsAction::ChooseImportFolder`] asks the app layer to run `rfd` and hand the answer back
//! in [`ImportState::folder`]. The dialog that is left is a label, a "Choose folder…" button, the
//! chosen path and Import, so it is [`import::CONTENT_SIZE`] tall instead of the original's 400.
//!
//! ## The overwrite prompt is asked once, not once per file
//!
//! `FxController::exportPresets()` shows a Yes/No box from inside its own `for` loop, once per
//! colliding file (`FxController.cpp:1403`), each one a nested modal loop started from the
//! controller — i.e. from the layer that has no business showing UI at all. Exporting five
//! presets over five existing files opens five sequential dialogs. §9.1 and Open question 3 both
//! recommend the same fix, which is what this module does: the app layer pre-computes the
//! collisions into [`ExportState::collisions`] and the dialog asks **once**, mapping the
//! original's two buttons onto [`OverwriteChoice::OverwriteAll`] and [`OverwriteChoice::SkipAll`],
//! with dismissal cancelling the export outright.

use super::message::{ConfirmChoice, MessageBox, message_with_name};
use super::{
    DialogChrome, DialogResponse, TextButton, draw_fitted, draw_truncated, normal_font, small_font,
};
use crate::assets::AssetCache;
use crate::theme::{FxColor, Palette};
use egui::{
    Align2, CornerRadius, Id, Key, Mesh, Rect, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder,
    Vec2, pos2, vec2,
};
use fxsound_core::DeviceDirection;
use fxsound_core::i18n::tr;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Something the user did in one of these windows.
///
/// None of these carry the data the app already has: the selected rows live in
/// [`ExportState::selected`] and the chosen folder in [`ImportState::folder`], and the app owns
/// both, so repeating them in the action would only create a second copy that can disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetsAction {
    /// Run a folder picker — `rfd::AsyncFileDialog::pick_folder()`, started from the app layer and
    /// polled, never the blocking variant (`docs/spec/06-dialogs.md` §9.3).
    ChooseImportFolder,
    /// Import every preset file of the edit direction's kind — `.fac` for the speakers, a voice
    /// preset's `.toml` for the microphone — in [`ImportState::folder`]. If the glob comes back
    /// empty the app puts `"Preset files not found in the selected folder."` in
    /// [`ImportState::notice`] and leaves the window open, exactly as
    /// `FxPresetImportDialog.cpp:262-267` does.
    Import,
    /// The notice box was acknowledged.
    DismissNotice,
    /// Close the import window — the ✕, Escape, or OK on the summary.
    CloseImport,

    /// Tick or untick one row of the export list.
    ToggleExport(usize),
    /// Export every ticked preset.
    Export,
    /// Answer the overwrite prompt.
    Overwrite(OverwriteChoice),
    /// Open the export folder in the user's file manager — the portal's
    /// `OpenURI.OpenDirectory`, never a hardcoded file manager (§9.3). This is
    /// `File::revealToUser()` (`FxPresetExportDialog.cpp:196`).
    RevealExportFolder,
    /// Tick or untick "Keep the end bands where they are" ([`ExportState::end_bands_offered`]).
    ToggleEndBands,
    /// Close the export window.
    CloseExport,
}

/// How to resolve every colliding file at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverwriteChoice {
    /// Yes: overwrite each existing file.
    OverwriteAll,
    /// No: keep the existing files and export only the presets that do not collide.
    SkipAll,
    /// Dismissed: export nothing. The original has no way to express this — its ✕ answers "No"
    /// for one file and then asks about the next — so it is new, and it is what a user pressing
    /// Escape means.
    Cancel,
}

// =============================================================================================
// Import
// =============================================================================================

/// Import-window geometry.
pub mod import {
    use egui::{Vec2, vec2};

    /// What the original reserves for the embedded file browser
    /// (`FxPresetImportDialog.h:51-52`). Kept for the record: the port does not use it.
    pub const ORIGINAL_CONTENT_SIZE: Vec2 = vec2(400.0, 400.0);
    /// The content size once the browser is replaced by a portal folder picker.
    pub const CONTENT_SIZE: Vec2 = vec2(400.0, 180.0);
    /// Outer window size, from the shared formula.
    pub const WINDOW_SIZE: Vec2 = vec2(410.0, 267.0);
    /// The working rect is `reduce(20, 0)` from a top of 10 (`FxPresetImportDialog.cpp:229-233`).
    pub const MARGIN: f32 = 20.0;
    /// `TEXT_HEIGHT` (`FxPresetImportDialog.h:55`).
    pub const TEXT_HEIGHT: f32 = 20.0;
    /// `BUTTON_WIDTH` × `BUTTON_HEIGHT` (`FxPresetImportDialog.h:53-54`).
    pub const BUTTON_SIZE: Vec2 = vec2(80.0, 30.0);
    /// The folder chooser is wider than Import because its label is a sentence.
    pub const CHOOSE_BUTTON_SIZE: Vec2 = vec2(160.0, 30.0);
    /// Two lines of path, so a deep `~/Documents/…` is readable.
    pub const PATH_HEIGHT: f32 = 40.0;
}

/// Import-summary geometry (`FxImportCompleteMessage`, `FxPresetImportDialog.cpp:104-146`).
pub mod summary {
    use egui::{Vec2, vec2};

    /// `WIDTH` × `HEIGHT` (`FxPresetImportDialog.cpp:104-105`).
    pub const CONTENT_SIZE: Vec2 = vec2(350.0, 340.0);
    /// Outer window size, from the shared formula.
    pub const WINDOW_SIZE: Vec2 = vec2(360.0, 427.0);
    pub const MARGIN: f32 = 20.0;
    /// `TEXT_HEIGHT` (`FxPresetImportDialog.cpp:107`).
    pub const TEXT_HEIGHT: f32 = 20.0;
    /// `LIST_HEIGHT` — the height of each read-only name box (`FxPresetImportDialog.cpp:108`).
    pub const LIST_HEIGHT: f32 = 100.0;
    /// `BUTTON_WIDTH` × `BUTTON_HEIGHT` (`FxPresetImportDialog.cpp:104-106`); note this OK button
    /// is 50 wide, not the 80 of the Import and Export buttons.
    pub const BUTTON_SIZE: Vec2 = vec2(50.0, 30.0);
    /// Rows of names are set in the normal font; the box scrolls when they overflow.
    pub const ROW_HEIGHT: f32 = 20.0;
}

/// `"Select the folder which contains the presets..."` (`FxPresetImportDialog.cpp:172`).
pub const SELECT_FOLDER_LABEL: &str = "Select the folder which contains the presets...";
/// `"Preset files not found in the selected folder."` (`FxPresetImportDialog.cpp:264`).
pub const NO_PRESETS_FOUND: &str = "Preset files not found in the selected folder.";
/// `"Presets successfully imported"` (`FxPresetImportDialog.cpp:117`).
pub const IMPORTED_LABEL: &str = "Presets successfully imported";
/// `"Duplicate presets not imported"` (`FxPresetImportDialog.cpp:126`).
pub const SKIPPED_LABEL: &str = "Duplicate presets not imported";

/// What `FxController::importPresets()` reported (`FxController.cpp:1419-1458`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    /// Names copied into the user preset directory.
    pub imported: Vec<String>,
    /// Names refused because a preset of that name already exists — the comparison is
    /// case-insensitive (`FxModel.cpp:142-153`).
    pub skipped: Vec<String>,
}

impl ImportSummary {
    /// One name per line.
    ///
    /// The original appends `"\n"` after *every* entry including the last, because its guard is
    /// `if (i != size)` inside an `i < size` loop (`FxPresetImportDialog.cpp:61-69`). That is a
    /// stray blank line, not a feature: this joins.
    #[must_use]
    pub fn imported_text(&self) -> String {
        self.imported.join("\n")
    }

    #[must_use]
    pub fn skipped_text(&self) -> String {
        self.skipped.join("\n")
    }
}

/// Everything the import flow remembers between frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportState {
    /// The lane the window was opened for: its store is the one imported into, `.fac` files for
    /// the speakers and voice presets for the microphone, whichever lane the window has been
    /// moved to since — by the tray, the command line or D-Bus — when Import is pressed.
    pub lane: DeviceDirection,
    /// The folder the app's picker came back with, or `None` while nothing is chosen.
    pub folder: Option<PathBuf>,
    /// Set once the import has run; while it is `Some` the summary window is shown instead of the
    /// chooser. This is the state machine `docs/spec/06-dialogs.md` §2.1 asks for in place of the
    /// original's "tear the modal down from inside its own button handler and start another one".
    pub summary: Option<ImportSummary>,
    /// A message box in front of the window.
    pub notice: Option<String>,
}

impl ImportState {
    /// `true` while the summary is showing.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.summary.is_some()
    }

    /// Import is only offered once a folder has been chosen.
    #[must_use]
    pub const fn can_import(&self) -> bool {
        self.folder.is_some()
    }
}

/// The import window and its summary.
pub struct ImportDialog<'a> {
    state: &'a ImportState,
}

impl<'a> ImportDialog<'a> {
    #[must_use]
    pub fn new(state: &'a ImportState) -> Self {
        Self { state }
    }

    /// The outer size this window wants right now — the chooser and the summary are different
    /// shapes, so a caller hosting them in one viewport has to resize between them.
    #[must_use]
    pub fn window_size(&self) -> Vec2 {
        if self.state.is_complete() {
            summary::WINDOW_SIZE
        } else {
            import::WINDOW_SIZE
        }
    }

    /// Draw the window into `outer`, which should be [`ImportDialog::window_size`].
    pub fn show(
        self,
        ui: &mut Ui,
        outer: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> DialogResponse<PresetsAction> {
        let id = Id::new("fx_import_dialog").with(id_salt);
        let mut response = DialogResponse::default();

        if let Some(summary) = &self.state.summary {
            // `FxImportCompleteMessage` is constructed with an empty name, so its title bar shows
            // the wordmark and no title (`FxPresetImportDialog.cpp:24`).
            let chrome = DialogChrome::new().show(ui, outer, palette, assets, id.with("summary"));
            response.push_if(chrome.close_clicked, PresetsAction::CloseImport);
            self.summary_content(ui, chrome.content, summary, palette, id, &mut response);
        } else {
            let chrome = DialogChrome::titled(&tr("Import Presets")).show(
                ui,
                outer,
                palette,
                assets,
                id.with("chrome"),
            );
            response.push_if(chrome.close_clicked, PresetsAction::CloseImport);
            self.chooser_content(ui, chrome.content, palette, id, &mut response);
        }

        match &self.state.notice {
            Some(notice) => {
                // The app files the notice under its English key.
                if MessageBox::ok(&tr(notice))
                    .show_modal(ui.ctx(), palette, assets, id.with("notice"))
                    .is_some()
                {
                    response.push(PresetsAction::DismissNotice);
                }
            }
            // Escape closes the import window (`FxPresetImportDialog.cpp:181-191`) — but only
            // when it is the thing in front; a notice box answers Escape itself.
            None => response.push_if(
                ui.input(|i| i.key_pressed(Key::Escape)),
                PresetsAction::CloseImport,
            ),
        }
        response
    }

    fn chooser_content(
        &self,
        ui: &mut Ui,
        content: Rect,
        palette: Palette,
        id: Id,
        response: &mut DialogResponse<PresetsAction>,
    ) {
        draw_fitted(
            ui.painter(),
            &tr(SELECT_FOLDER_LABEL),
            normal_font(),
            palette.color(FxColor::HighlightedText),
            label_rect(content),
            Align2::LEFT_CENTER,
        );

        if TextButton::new(&tr("Choose folder…"))
            .show(ui, choose_rect(content), palette, id.with("choose"))
            .clicked()
        {
            response.push(PresetsAction::ChooseImportFolder);
        }

        // JUCE's browser puts the chosen directory in a "Folder:" box; there is no box to put it
        // in any more, so the path is shown as text — dimmed while nothing is chosen.
        let (text, colour) = match &self.state.folder {
            Some(folder) => (
                folder.display().to_string(),
                palette.color(FxColor::DefaultText),
            ),
            None => (
                tr("Folder: (none chosen)"),
                palette.color(FxColor::HintText),
            ),
        };
        super::draw_wrapped(
            ui.painter(),
            &text,
            small_font(),
            colour,
            path_rect(content),
        );

        if TextButton::new(&tr("Import"))
            .enabled(self.state.can_import())
            .show(ui, import_rect(content), palette, id.with("import"))
            .clicked()
        {
            response.push(PresetsAction::Import);
        }
    }

    fn summary_content(
        &self,
        ui: &mut Ui,
        content: Rect,
        summary: &ImportSummary,
        palette: Palette,
        id: Id,
        response: &mut DialogResponse<PresetsAction>,
    ) {
        for (index, (label, names)) in [
            (tr(IMPORTED_LABEL), &summary.imported),
            (tr(SKIPPED_LABEL), &summary.skipped),
        ]
        .into_iter()
        .enumerate()
        {
            draw_fitted(
                ui.painter(),
                &label,
                normal_font(),
                palette.color(FxColor::HighlightedText),
                summary_label_rect(content, index),
                Align2::LEFT_CENTER,
            );
            name_list(
                ui,
                summary_list_rect(content, index),
                names,
                palette,
                id.with(("names", index)),
            );
        }

        if TextButton::new(&tr("OK"))
            .show(ui, summary_ok_rect(content), palette, id.with("ok"))
            .clicked()
        {
            response.push(PresetsAction::CloseImport);
        }
    }
}

/// `(20, 10, 360, 20)` (`FxPresetImportDialog.cpp:236`).
#[must_use]
pub fn label_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            content.left() + import::MARGIN,
            content.top() + import::MARGIN / 2.0,
        ),
        vec2(content.width() - import::MARGIN * 2.0, import::TEXT_HEIGHT),
    )
}

/// The folder chooser, on the row the file browser used to start on.
#[must_use]
pub fn choose_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            content.left() + import::MARGIN,
            label_rect(content).bottom() + 10.0,
        ),
        import::CHOOSE_BUTTON_SIZE,
    )
}

/// The chosen path, under the chooser.
#[must_use]
pub fn path_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            content.left() + import::MARGIN,
            choose_rect(content).bottom() + 10.0,
        ),
        vec2(content.width() - import::MARGIN * 2.0, import::PATH_HEIGHT),
    )
}

/// Import: right-aligned on the last row, as in the original (`FxPresetImportDialog.cpp:250-252`).
#[must_use]
pub fn import_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            content.right() - import::MARGIN - import::BUTTON_SIZE.x,
            content.bottom() - 10.0 - import::BUTTON_SIZE.y,
        ),
        import::BUTTON_SIZE,
    )
}

/// `(20, 10, 310, 20)` and `(20, 150, 310, 20)` (`FxPresetImportDialog.cpp:113-131`).
#[must_use]
pub fn summary_label_rect(content: Rect, index: usize) -> Rect {
    let block = index as f32 * (summary::TEXT_HEIGHT + summary::LIST_HEIGHT + 20.0);
    Rect::from_min_size(
        pos2(
            content.left() + summary::MARGIN,
            content.top() + 10.0 + block,
        ),
        vec2(
            content.width() - summary::MARGIN * 2.0,
            summary::TEXT_HEIGHT,
        ),
    )
}

/// `(20, 40, 310, 100)` and `(20, 180, 310, 100)`.
#[must_use]
pub fn summary_list_rect(content: Rect, index: usize) -> Rect {
    let label = summary_label_rect(content, index);
    Rect::from_min_size(
        pos2(label.left(), label.bottom() + 10.0),
        vec2(label.width(), summary::LIST_HEIGHT),
    )
}

/// `(150, 300, 50, 30)` — horizontally centred (`FxPresetImportDialog.cpp:143`).
#[must_use]
pub fn summary_ok_rect(content: Rect) -> Rect {
    let row = Rect::from_min_size(
        pos2(
            content.left(),
            summary_list_rect(content, 1).bottom() + 20.0,
        ),
        vec2(content.width(), summary::BUTTON_SIZE.y),
    );
    Align2::CENTER_TOP.align_size_within_rect(summary::BUTTON_SIZE, row)
}

/// One of the summary's two read-only, scrolling name boxes
/// (`FxPresetImportDialog.cpp:55-59`, `:77-81`).
fn name_list(ui: &mut Ui, rect: Rect, names: &[String], palette: Palette, id: Id) {
    ui.painter().rect_filled(
        rect,
        CornerRadius::ZERO,
        palette.color(FxColor::DefaultFill),
    );
    let font = normal_font();
    let colour = palette.color(FxColor::DefaultText);
    ui.scope_builder(UiBuilder::new().max_rect(rect).id_salt(id), |ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        egui::ScrollArea::vertical()
            .id_salt(id)
            .max_height(rect.height())
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for name in names {
                    let (row, _) = ui.allocate_exact_size(
                        vec2(ui.available_width(), summary::ROW_HEIGHT),
                        Sense::hover(),
                    );
                    draw_truncated(
                        ui.painter(),
                        name,
                        font.clone(),
                        colour,
                        row.shrink2(vec2(4.0, 0.0)),
                        Align2::LEFT_CENTER,
                    );
                }
            });
    });
}

// =============================================================================================
// Export
// =============================================================================================

/// Export-window geometry (`FxPresetExportDialog.h:67-72`, `.cpp:108-136`).
pub mod export {
    use egui::{Vec2, vec2};

    /// `WIDTH` × `HEIGHT` (`FxPresetExportDialog.h:67-68`).
    pub const CONTENT_SIZE: Vec2 = vec2(400.0, 405.0);
    /// Outer window size, from the shared formula.
    pub const WINDOW_SIZE: Vec2 = vec2(410.0, 492.0);
    pub const MARGIN: f32 = 20.0;
    /// `TEXT_HEIGHT` (`FxPresetExportDialog.h:71`).
    pub const TEXT_HEIGHT: f32 = 20.0;
    /// `LIST_HEIGHT` (`FxPresetExportDialog.h:72`).
    pub const LIST_HEIGHT: f32 = 310.0;
    /// `BUTTON_WIDTH` × `BUTTON_HEIGHT` (`FxPresetExportDialog.h:69-70`).
    pub const BUTTON_SIZE: Vec2 = vec2(80.0, 30.0);
    /// `setRowHeight(TEXT_HEIGHT + 6)` (`FxPresetExportDialog.cpp:86`).
    pub const ROW_HEIGHT: f32 = TEXT_HEIGHT + 6.0;
    /// Rows inset their text by ten points on each side (`FxPresetExportDialog.cpp:151`).
    pub const ROW_TEXT_INSET: f32 = 10.0;
    /// The progress bar spans the whole content width, ignoring the 20 point margins
    /// (`FxPresetExportDialog.cpp:131`).
    pub const PROGRESS_HEIGHT: f32 = 2.0;
    /// `AnimatedAppComponent` frame rate (`FxPresetExportDialog.cpp:52`).
    pub const PROGRESS_FPS: f32 = 30.0;
    /// How far the gradient's start moves each frame (`FxPresetExportDialog.cpp:60`).
    pub const PROGRESS_STEP: f32 = 0.01;
}

/// `"Select the presets to export..."` (`FxPresetExportDialog.cpp:78`).
pub const SELECT_PRESETS_LABEL: &str = "Select the presets to export...";
/// The export window's choice from «Like FxSound for Windows» = Interface and sound on (roadmap
/// 0.5.0 §14 #56): ticked, a `.fac` keeps its first and last band where they are; unticked, as
/// in 0.4.0, they go back into the range the Windows build tunes them in (0.4.0 audit R6).
pub const END_BANDS_AS_THEY_ARE: &str = "Keep the end bands where they are";
/// `"Presets are exported successfully!"` (`FxPresetExportDialog.cpp:195`).
pub const EXPORT_SUCCEEDED: &str = "Presets are exported successfully!";
/// The original's per-file prompt (`FxController.cpp:1403`). `%s` is the preset name.
pub const OVERWRITE_MESSAGE: &str =
    "Preset file %s already exists in the export path, do you want to overwrite the preset file?";
/// The batch form, for the "ask once" behaviour `docs/spec/06-dialogs.md` Open question 3 asks
/// for. `%s` is the number of colliding files.
pub const OVERWRITE_MESSAGE_PLURAL: &str =
    "%s preset files already exist in the export path, do you want to overwrite them?";

/// `FxController::FormatString` (`FxController.cpp:2907-2914`) is `swprintf_s` with exactly one
/// string argument. Rust has no `%s`, so this substitutes the first one — and leaves a template
/// without a placeholder alone, which is what makes it safe for a translator to have moved or
/// dropped it (`docs/spec/06-dialogs.md` Open question 11).
#[must_use]
pub fn format_string(template: &str, argument: &str) -> String {
    template.replacen("%s", argument, 1)
}

/// The question to ask about `names`, which are the presets whose file already exists.
#[must_use]
pub fn overwrite_message(names: &[String]) -> String {
    match names {
        [one] => format_string(&tr(OVERWRITE_MESSAGE), one),
        many => format_string(&tr(OVERWRITE_MESSAGE_PLURAL), &many.len().to_string()),
    }
}

/// Everything the export window remembers between frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportState {
    /// The lane the window was opened for, whose names [`ExportState::presets`] lists: its store
    /// is the one exported from, whichever lane the window has been moved to since.
    pub lane: DeviceDirection,
    /// Every preset, factory and user alike — the list comes straight from the model
    /// (`FxPresetExportDialog.cpp:138-141`).
    pub presets: Vec<String>,
    /// Which rows are ticked. Multiple selection is on and a click toggles a row
    /// (`FxPresetExportDialog.cpp:88-89`).
    pub selected: BTreeSet<usize>,
    /// `true` from the moment Export is pressed: the button goes dead and the progress bar appears
    /// (`FxPresetExportDialog.cpp:174-180`).
    pub exporting: bool,
    /// Presets whose file already exists in the export folder. Non-empty raises the prompt.
    pub collisions: Vec<String>,
    /// Set once the export has run: `true` if at least one file was written.
    ///
    /// That is exactly what `FxController::exportPresets()` returns, and it gates both the success
    /// box and the reveal in the original (`FxPresetExportDialog.cpp:190-197`). The window then
    /// closes either way (`:199-200`).
    pub finished: Option<bool>,
    /// Whether the window offers [`END_BANDS_AS_THEY_ARE`]: for the speakers' presets from «Like
    /// FxSound for Windows» = Interface and sound on. Below it, and for a voice preset, which has
    /// no Windows reader, the window is the original's.
    pub end_bands_offered: bool,
    /// Whether it is ticked: the `export_unshifted` setting.
    pub end_bands_as_they_are: bool,
}

impl ExportState {
    /// The Export button is enabled iff at least one row is ticked
    /// (`FxPresetExportDialog.cpp:158-168`) and no export is already running.
    #[must_use]
    pub fn can_export(&self) -> bool {
        !self.selected.is_empty() && !self.exporting
    }

    /// The ticked names, in list order.
    #[must_use]
    pub fn selected_names(&self) -> Vec<&str> {
        self.selected
            .iter()
            .filter_map(|&index| self.presets.get(index).map(String::as_str))
            .collect()
    }
}

/// The export window.
pub struct ExportDialog<'a> {
    state: &'a ExportState,
}

impl<'a> ExportDialog<'a> {
    #[must_use]
    pub fn new(state: &'a ExportState) -> Self {
        Self { state }
    }

    /// Draw the window into `outer`, which should be [`export::WINDOW_SIZE`].
    pub fn show(
        self,
        ui: &mut Ui,
        outer: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> DialogResponse<PresetsAction> {
        let id = Id::new("fx_export_dialog").with(id_salt);
        let mut response = DialogResponse::default();

        let chrome = DialogChrome::titled(&tr("Export Presets")).show(
            ui,
            outer,
            palette,
            assets,
            id.with("chrome"),
        );
        response.push_if(chrome.close_clicked, PresetsAction::CloseExport);
        let content = chrome.content;

        draw_fitted(
            ui.painter(),
            &tr(SELECT_PRESETS_LABEL),
            normal_font(),
            palette.color(FxColor::HighlightedText),
            export_label_rect(content),
            Align2::LEFT_CENTER,
        );

        let list = if self.state.end_bands_offered {
            export_list_rect_with_end_bands(content)
        } else {
            export_list_rect(content)
        };
        if let Some(index) = preset_list(ui, list, self.state, palette, id.with("list")) {
            response.push(PresetsAction::ToggleExport(index));
        }
        if self.state.end_bands_offered
            && tick_box(
                ui,
                end_bands_rect(content),
                &tr(END_BANDS_AS_THEY_ARE),
                self.state.end_bands_as_they_are,
                !self.state.exporting,
                palette,
                id.with("end_bands"),
            )
        {
            response.push(PresetsAction::ToggleEndBands);
        }

        if self.state.exporting {
            let time = ui.ctx().input(|i| i.time);
            paint_progress(ui, progress_rect(content), palette, progress_phase(time));
            // `AnimatedAppComponent` drives its own 30 fps timer; egui only redraws when asked.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f32(
                    1.0 / export::PROGRESS_FPS,
                ));
        }

        if TextButton::new(&tr("Export"))
            .enabled(self.state.can_export())
            .show(ui, export_button_rect(content), palette, id.with("export"))
            .clicked()
        {
            response.push(PresetsAction::Export);
        }

        if !self.state.collisions.is_empty() {
            // One name goes into the question, shortened in the middle if the box needs it to
            // show the question whole (0.4.0 audit #49).
            let message = match self.state.collisions.as_slice() {
                [one] => message_with_name(ui.ctx(), &tr(OVERWRITE_MESSAGE), one),
                many => overwrite_message(many),
            };
            if let Some(choice) = MessageBox::new(&message).show_modal(
                ui.ctx(),
                palette,
                assets,
                id.with("overwrite"),
            ) {
                response.push(PresetsAction::Overwrite(match choice {
                    ConfirmChoice::Yes => OverwriteChoice::OverwriteAll,
                    ConfirmChoice::No | ConfirmChoice::Ok => OverwriteChoice::SkipAll,
                    ConfirmChoice::Dismissed => OverwriteChoice::Cancel,
                }));
            }
        } else if let Some(written) = self.state.finished {
            if written {
                // Announce it, then open the export folder, then close — the original's order
                // (`FxPresetExportDialog.cpp:193-200`).
                if MessageBox::ok(&tr(EXPORT_SUCCEEDED))
                    .show_modal(ui.ctx(), palette, assets, id.with("done"))
                    .is_some()
                {
                    response.push(PresetsAction::RevealExportFolder);
                    response.push(PresetsAction::CloseExport);
                }
            } else {
                // Nothing was written, so there is nothing to announce and nothing to reveal. The
                // original closes the window all the same.
                response.push(PresetsAction::CloseExport);
            }
        } else {
            // Escape closes the export window (`FxPresetExportDialog.cpp:31-41`), unless a box is
            // in front of it answering Escape itself.
            response.push_if(
                ui.input(|i| i.key_pressed(Key::Escape)),
                PresetsAction::CloseExport,
            );
        }

        response
    }
}

/// `(20, 10, 360, 20)` (`FxPresetExportDialog.cpp:111-115`).
#[must_use]
pub fn export_label_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(content.left() + export::MARGIN, content.top() + 10.0),
        vec2(content.width() - export::MARGIN * 2.0, export::TEXT_HEIGHT),
    )
}

/// `(20, 40, 360, 310)` (`FxPresetExportDialog.cpp:117-120`).
#[must_use]
pub fn export_list_rect(content: Rect) -> Rect {
    let label = export_label_rect(content);
    Rect::from_min_size(
        pos2(label.left(), label.bottom() + 10.0),
        vec2(label.width(), export::LIST_HEIGHT),
    )
}

/// The list when the window offers [`END_BANDS_AS_THEY_ARE`]: [`export_list_rect`] a row
/// shorter, so the choice sits between the list and the progress bar, and everything below keeps
/// the original's place.
#[must_use]
pub fn export_list_rect_with_end_bands(content: Rect) -> Rect {
    let list = export_list_rect(content);
    Rect::from_min_size(
        list.min,
        vec2(list.width(), list.height() - export::ROW_HEIGHT - 4.0),
    )
}

/// The row of [`END_BANDS_AS_THEY_ARE`]: the list's width, [`export::ROW_HEIGHT`] tall, under
/// the shortened list.
#[must_use]
pub fn end_bands_rect(content: Rect) -> Rect {
    let list = export_list_rect_with_end_bands(content);
    Rect::from_min_size(
        pos2(list.left(), list.bottom() + 4.0),
        vec2(list.width(), export::ROW_HEIGHT),
    )
}

/// A tick box and its label in `rect`, drawn as the Settings pane's (`settings::TICK_BOX_SIDE`
/// and its neighbours), with the label set smaller before it is cut ([`draw_fitted`]). Returns
/// whether it was clicked.
fn tick_box(
    ui: &mut Ui,
    rect: Rect,
    label: &str,
    checked: bool,
    enabled: bool,
    palette: Palette,
    id: Id,
) -> bool {
    use super::settings::{TICK_BOX_CORNER, TICK_BOX_SIDE};
    let response = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let colour = palette.color(if enabled {
        FxColor::HighlightedText
    } else {
        FxColor::HintText
    });
    let tick = Align2::LEFT_CENTER.align_size_within_rect(Vec2::splat(TICK_BOX_SIDE), rect);
    ui.painter().rect_stroke(
        tick,
        CornerRadius::same(TICK_BOX_CORNER as u8),
        Stroke::new(1.5, colour),
        StrokeKind::Inside,
    );
    if checked {
        let mark = tick.shrink(4.0);
        let stroke = Stroke::new(2.0, colour);
        let elbow = pos2(
            mark.left() + mark.width() * 0.36,
            mark.top() + mark.height() * 0.82,
        );
        ui.painter()
            .line_segment([pos2(mark.left(), mark.center().y), elbow], stroke);
        ui.painter()
            .line_segment([elbow, pos2(mark.right(), mark.top())], stroke);
    }
    draw_fitted(
        ui.painter(),
        label,
        normal_font(),
        colour,
        end_bands_label_rect(rect),
        Align2::LEFT_CENTER,
    );
    if enabled && response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

/// Where a tick box's label goes in its row: past the box and its gap.
fn end_bands_label_rect(row: Rect) -> Rect {
    use super::settings::{TICK_BOX_GAP, TICK_BOX_SIDE};
    Rect::from_min_max(
        pos2(row.left() + TICK_BOX_SIDE + TICK_BOX_GAP, row.top()),
        row.max,
    )
}

/// `(0, 360, 400, 2)` — the full content width, margins and all
/// (`FxPresetExportDialog.cpp:127-131`).
#[must_use]
pub fn progress_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(content.left(), export_list_rect(content).bottom() + 10.0),
        vec2(content.width(), export::PROGRESS_HEIGHT),
    )
}

/// `(300, 372, 80, 30)` (`FxPresetExportDialog.cpp:133-135`).
#[must_use]
pub fn export_button_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            content.right() - export::MARGIN - export::BUTTON_SIZE.x,
            progress_rect(content).bottom() + 10.0,
        ),
        export::BUTTON_SIZE,
    )
}

/// Where the gradient starts, as a fraction of the bar's width.
///
/// The original is an `AnimatedAppComponent` at 30 fps that adds `0.01` per frame and wraps at
/// `1.0`, so one cycle takes 100 frames ≈ 3.33 s (`FxPresetExportDialog.cpp:49-72`). Deriving it
/// from the clock instead of counting frames keeps the same speed whatever egui's repaint rate
/// turns out to be.
#[must_use]
pub fn progress_phase(time_secs: f64) -> f32 {
    let per_second = f64::from(export::PROGRESS_FPS * export::PROGRESS_STEP);
    (time_secs * per_second).rem_euclid(1.0) as f32
}

/// The animated bar: flat `ImageButton` up to the gradient's start, then a ramp to
/// `VerticalSliderLow` at the right edge (`FxPresetExportDialog.cpp:62-71`).
fn paint_progress(ui: &Ui, rect: Rect, palette: Palette, phase: f32) {
    let corner = CornerRadius::same((rect.height() / 2.0) as u8);
    let start = palette.color(FxColor::ImageButton);
    let end = palette.color(FxColor::VerticalSliderLow);
    let split = rect.left() + rect.width() * phase.clamp(0.0, 1.0);

    // A `ColourGradient` is flat before its start point, so everything left of the split is the
    // start colour.
    ui.painter().rect_filled(
        Rect::from_min_max(rect.min, pos2(split, rect.bottom())),
        corner,
        start,
    );

    let mut mesh = Mesh::default();
    mesh.colored_vertex(pos2(split, rect.top()), start);
    mesh.colored_vertex(pos2(split, rect.bottom()), start);
    mesh.colored_vertex(pos2(rect.right(), rect.top()), end);
    mesh.colored_vertex(pos2(rect.right(), rect.bottom()), end);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    ui.painter().add(Shape::mesh(mesh));
}

/// The multi-select preset list. Returns the row the user just toggled.
fn preset_list(
    ui: &mut Ui,
    rect: Rect,
    state: &ExportState,
    palette: Palette,
    id: Id,
) -> Option<usize> {
    ui.painter().rect_filled(
        rect,
        CornerRadius::ZERO,
        palette.color(FxColor::DefaultFill),
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, palette.color(FxColor::DefaultFill)),
        StrokeKind::Inside,
    );

    let font = normal_font();
    // Every row draws its name in `HighlightedText`, selected or not — `paintListBoxItem` never
    // reads the list's `textColourId` (`FxPresetExportDialog.cpp:143-156`).
    let text_colour = palette.color(FxColor::HighlightedText);
    let fill = palette.color(FxColor::ImageButton);

    let mut toggled = None;
    ui.scope_builder(UiBuilder::new().max_rect(rect).id_salt(id), |ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        egui::ScrollArea::vertical()
            .id_salt(id)
            .max_height(rect.height())
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, name) in state.presets.iter().enumerate() {
                    let (row, response) = ui.allocate_exact_size(
                        vec2(ui.available_width(), export::ROW_HEIGHT),
                        Sense::click(),
                    );
                    if state.selected.contains(&index) {
                        ui.painter().rect_filled(row, CornerRadius::ZERO, fill);
                    }
                    draw_truncated(
                        ui.painter(),
                        name,
                        font.clone(),
                        text_colour,
                        row.shrink2(vec2(export::ROW_TEXT_INSET, 0.0)),
                        Align2::LEFT_CENTER,
                    );
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        toggled = Some(index);
                    }
                }
            });
    });
    toggled
}

#[cfg(test)]
mod tests {
    use super::super::tests::{frame, test_context};
    use super::*;
    use fxsound_core::ThemeMode;

    fn import_content() -> Rect {
        Rect::from_min_size(pos2(5.0, 62.0), import::CONTENT_SIZE)
    }

    fn summary_content() -> Rect {
        Rect::from_min_size(pos2(5.0, 62.0), summary::CONTENT_SIZE)
    }

    fn export_content() -> Rect {
        Rect::from_min_size(pos2(5.0, 62.0), export::CONTENT_SIZE)
    }

    fn local(content: Rect, r: Rect) -> Rect {
        r.translate(-content.min.to_vec2())
    }

    #[test]
    fn every_languages_headings_fit_their_line_set_no_smaller_than_seven_tenths() {
        // E6b: German "Wählen Sie die zu exportierenden Voreinstellungen..." lost its end.
        use super::super::fitted_font;
        use super::super::tests::every_translation;
        let ctx = test_context();
        let mut problems = Vec::new();
        frame(&ctx, |ui| {
            for (key, rect) in [
                (SELECT_PRESETS_LABEL, export_label_rect(export_content())),
                (SELECT_FOLDER_LABEL, label_rect(import_content())),
                (IMPORTED_LABEL, summary_label_rect(summary_content(), 0)),
                (SKIPPED_LABEL, summary_label_rect(summary_content(), 1)),
                (
                    END_BANDS_AS_THEY_ARE,
                    end_bands_label_rect(end_bands_rect(export_content())),
                ),
            ] {
                for (code, text) in every_translation(key) {
                    // The one the original's own table makes too long even for JUCE's squeeze
                    // (57 characters): it is elided at seven tenths here as it would be there.
                    if (code, key) == ("pl", SKIPPED_LABEL) {
                        continue;
                    }
                    let font = fitted_font(ui.ctx(), &text, normal_font(), rect.width());
                    let used = ui
                        .painter()
                        .layout_no_wrap(text.clone(), font.clone(), egui::Color32::PLACEHOLDER)
                        .size()
                        .x;
                    if used > rect.width() {
                        problems.push(format!(
                            "{code}: {text:?} is {used:.0} in {:.0} at {}",
                            rect.width(),
                            font.size
                        ));
                    }
                }
            }
        });
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn the_end_bands_choice_sits_between_the_shortened_list_and_the_progress_bar() {
        // «Like FxSound for Windows» = Interface and sound (roadmap 0.5.0 §14 #56): the list
        // gives up a row, and the progress bar and the Export button keep the original's places.
        let content = export_content();
        let (list, row) = (
            export_list_rect_with_end_bands(content),
            end_bands_rect(content),
        );
        assert_eq!(list.min, export_list_rect(content).min);
        assert!(list.bottom() < row.top(), "{list:?} {row:?}");
        assert!(row.bottom() <= progress_rect(content).top(), "{row:?}");
        assert_eq!(row.width(), list.width());
        assert!(row.height() >= super::super::settings::TICK_BOX_SIDE);
        // Rows of the list still fit whole.
        assert!(list.height() >= export::ROW_HEIGHT * 10.0);
    }

    /// Move to `at`, press and release over the export window, and collect what it emitted.
    fn click_export(state: &ExportState, at: egui::Pos2) -> Vec<PresetsAction> {
        use egui::{Event, PointerButton, RawInput};
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), export::WINDOW_SIZE);
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(at)],
            vec![Event::PointerMoved(at), button(true)],
            vec![button(false)],
        ] {
            let input = RawInput {
                screen_rect: Some(outer),
                events,
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                actions.extend(
                    ExportDialog::new(state)
                        .show(ui, outer, Palette::new(ThemeMode::Dark), &mut assets, "t")
                        .actions,
                );
            })
            .drop_without_applying_deltas();
        }
        actions
    }

    #[test]
    fn the_export_window_offers_the_end_bands_only_when_told_to() {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), export::WINDOW_SIZE);
        let content = super::super::content_rect(outer);
        let row = end_bands_rect(content);
        let presets: Vec<String> = (0..12).map(|i| format!("Preset {i}")).collect();
        let offered = ExportState {
            presets: presets.clone(),
            end_bands_offered: true,
            ..ExportState::default()
        };
        assert_eq!(
            click_export(&offered, row.left_center() + vec2(9.0, 0.0)),
            [PresetsAction::ToggleEndBands]
        );
        // Below Interface and sound the place is the list's, as in the original.
        let plain = ExportState {
            presets,
            ..ExportState::default()
        };
        let actions = click_export(&plain, row.left_center() + vec2(9.0, 0.0));
        assert!(
            !actions.contains(&PresetsAction::ToggleEndBands),
            "{actions:?}"
        );
        // While the files are written the choice stands still.
        let busy = ExportState {
            exporting: true,
            ..offered
        };
        assert!(click_export(&busy, row.left_center() + vec2(9.0, 0.0)).is_empty());
    }

    #[test]
    fn every_window_size_follows_the_shared_formula() {
        use super::super::outer_size;
        assert!((outer_size(import::CONTENT_SIZE) - import::WINDOW_SIZE).length() < 1e-4);
        assert!((outer_size(summary::CONTENT_SIZE) - summary::WINDOW_SIZE).length() < 1e-4);
        assert!((outer_size(export::CONTENT_SIZE) - export::WINDOW_SIZE).length() < 1e-4);
        // The original import window is the one size this port deliberately changes: same width,
        // much shorter, because the 310 point file browser is gone.
        let (shrunk, original) = (import::CONTENT_SIZE, import::ORIGINAL_CONTENT_SIZE);
        assert!((outer_size(original) - vec2(410.0, 487.0)).length() < 1e-4);
        assert!(
            shrunk.y < original.y,
            "{shrunk:?} should be shorter than {original:?}"
        );
        assert!((shrunk.x - original.x).abs() < 1e-6);
    }

    #[test]
    fn the_import_chooser_keeps_the_originals_label_row_and_right_aligned_button() {
        let content = import_content();
        // (20, 10, 360, 20) — the label row is unchanged from FxPresetImportDialog.cpp:236.
        let label = local(content, label_rect(content));
        assert!((label.min - pos2(20.0, 10.0)).length() < 1e-4, "{label:?}");
        assert!((label.size() - vec2(360.0, 20.0)).length() < 1e-4);

        let import = local(content, import_rect(content));
        assert!((import.size() - import::BUTTON_SIZE).length() < 1e-4);
        assert!((import.right() - 380.0).abs() < 1e-4, "{import:?}");
        assert!(import.bottom() <= import::CONTENT_SIZE.y);
        // Nothing overlaps.
        assert!(choose_rect(content).bottom() <= path_rect(content).top());
        assert!(path_rect(content).bottom() <= import_rect(content).top());
    }

    #[test]
    fn the_summary_lays_out_exactly_as_the_specs_table_says() {
        // docs/spec/06-dialogs.md §2.2.
        let content = summary_content();
        let expect = |r: Rect, min: egui::Pos2, size: Vec2| {
            let r = local(content, r);
            assert!(
                (r.min - min).length() < 1e-4,
                "{r:?} should start at {min:?}"
            );
            assert!(
                (r.size() - size).length() < 1e-4,
                "{r:?} should be {size:?}"
            );
        };
        expect(
            summary_label_rect(content, 0),
            pos2(20.0, 10.0),
            vec2(310.0, 20.0),
        );
        expect(
            summary_list_rect(content, 0),
            pos2(20.0, 40.0),
            vec2(310.0, 100.0),
        );
        expect(
            summary_label_rect(content, 1),
            pos2(20.0, 150.0),
            vec2(310.0, 20.0),
        );
        expect(
            summary_list_rect(content, 1),
            pos2(20.0, 180.0),
            vec2(310.0, 100.0),
        );
        expect(
            summary_ok_rect(content),
            pos2(150.0, 300.0),
            vec2(50.0, 30.0),
        );
    }

    #[test]
    fn the_export_window_lays_out_exactly_as_the_specs_table_says() {
        // docs/spec/06-dialogs.md §3.
        let content = export_content();
        let expect = |r: Rect, min: egui::Pos2, size: Vec2| {
            let r = local(content, r);
            assert!(
                (r.min - min).length() < 1e-4,
                "{r:?} should start at {min:?}"
            );
            assert!(
                (r.size() - size).length() < 1e-4,
                "{r:?} should be {size:?}"
            );
        };
        expect(
            export_label_rect(content),
            pos2(20.0, 10.0),
            vec2(360.0, 20.0),
        );
        expect(
            export_list_rect(content),
            pos2(20.0, 40.0),
            vec2(360.0, 310.0),
        );
        expect(progress_rect(content), pos2(0.0, 360.0), vec2(400.0, 2.0));
        expect(
            export_button_rect(content),
            pos2(300.0, 372.0),
            vec2(80.0, 30.0),
        );
        // The bar really does ignore the margins the rest of the window respects.
        assert!(progress_rect(content).left() < export_list_rect(content).left());
    }

    #[test]
    fn an_export_row_is_twenty_six_points_tall() {
        assert!((export::ROW_HEIGHT - 26.0).abs() < 1e-6);
    }

    #[test]
    fn export_is_offered_only_for_a_non_empty_selection_that_is_not_already_running() {
        let mut state = ExportState {
            presets: vec!["Flat".into(), "Rock".into(), "Jazz".into()],
            ..ExportState::default()
        };
        assert!(!state.can_export());
        state.selected.insert(2);
        assert!(state.can_export());
        assert_eq!(state.selected_names(), vec!["Jazz"]);
        state.selected.insert(0);
        // A BTreeSet keeps them in list order, which is the order the files are written in.
        assert_eq!(state.selected_names(), vec!["Flat", "Jazz"]);
        state.exporting = true;
        assert!(!state.can_export());
    }

    #[test]
    fn a_selection_that_outruns_the_preset_list_is_ignored_rather_than_panicking() {
        let mut state = ExportState {
            presets: vec!["Flat".into()],
            ..ExportState::default()
        };
        state.selected.insert(7);
        assert!(state.selected_names().is_empty());
    }

    #[test]
    fn the_single_file_prompt_is_still_the_originals_sentence() {
        let one = overwrite_message(&["Rock".to_owned()]);
        assert_eq!(
            one,
            "Preset file Rock already exists in the export path, do you want to overwrite the preset file?"
        );
        // …and the batch form counts instead of naming.
        let many = overwrite_message(&["Rock".to_owned(), "Jazz".to_owned()]);
        assert!(many.starts_with("2 preset files already exist"), "{many}");
        assert!(overwrite_message(&[]).contains('0'));
    }

    #[test]
    fn format_string_substitutes_once_and_tolerates_a_missing_placeholder() {
        assert_eq!(
            format_string("Preset %s is deleted.", "Rock"),
            "Preset Rock is deleted."
        );
        // Only the first placeholder, as `swprintf_s` with one argument would.
        assert_eq!(format_string("%s and %s", "a"), "a and %s");
        // A translator who dropped the placeholder gets their string back unharmed.
        assert_eq!(format_string("no placeholder", "a"), "no placeholder");
    }

    #[test]
    fn the_progress_gradient_takes_a_hundred_frames_to_come_round() {
        // 30 fps x 0.01 per frame = 0.3 per second, so a full cycle is 10/3 seconds.
        assert!(progress_phase(0.0).abs() < 1e-6);
        assert!((progress_phase(10.0 / 6.0) - 0.5).abs() < 1e-4);
        let full = progress_phase(10.0 / 3.0);
        assert!(full < 1e-4 || full > 1.0 - 1e-4, "wrapped to {full}");
        // It never leaves 0..1, however long the app has been running.
        for seconds in [0.1, 1.0, 60.0, 3600.0, 86_400.0] {
            let phase = progress_phase(seconds);
            assert!((0.0..1.0).contains(&phase), "{seconds}s gave {phase}");
        }
    }

    #[test]
    fn the_summary_joins_names_without_the_originals_trailing_blank_line() {
        let summary = ImportSummary {
            imported: vec!["Rock".into(), "Jazz".into()],
            skipped: vec!["Flat".into()],
        };
        assert_eq!(summary.imported_text(), "Rock\nJazz");
        assert!(!summary.imported_text().ends_with('\n'));
        assert_eq!(summary.skipped_text(), "Flat");
        assert_eq!(ImportSummary::default().imported_text(), "");
    }

    #[test]
    fn the_import_window_grows_into_the_summary() {
        let mut state = ImportState::default();
        assert!(!state.is_complete());
        assert!(!state.can_import());
        assert!((ImportDialog::new(&state).window_size() - import::WINDOW_SIZE).length() < 1e-4);

        state.folder = Some(PathBuf::from("/home/u/Documents/presets"));
        assert!(state.can_import());

        state.summary = Some(ImportSummary::default());
        assert!(state.is_complete());
        assert!((ImportDialog::new(&state).window_size() - summary::WINDOW_SIZE).length() < 1e-4);
    }

    #[test]
    fn the_import_chooser_draws_and_asks_for_nothing_on_its_own() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let state = ImportState::default();
        let outer = Rect::from_min_size(pos2(0.0, 0.0), import::WINDOW_SIZE);
        frame(&ctx, |ui| {
            let response = ImportDialog::new(&state).show(
                ui,
                outer,
                Palette::new(ThemeMode::Dark),
                &mut assets,
                "import",
            );
            assert!(response.is_empty(), "{:?}", response.actions);
        });
    }

    #[test]
    fn the_import_summary_draws_both_lists_in_both_palettes() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let state = ImportState {
            lane: DeviceDirection::Output,
            folder: Some(PathBuf::from("/home/u/Documents/presets")),
            summary: Some(ImportSummary {
                imported: (0..12).map(|i| format!("Imported {i}")).collect(),
                skipped: vec!["General".into()],
            }),
            notice: None,
        };
        let outer = Rect::from_min_size(pos2(0.0, 0.0), summary::WINDOW_SIZE);
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            frame(&ctx, |ui| {
                let response = ImportDialog::new(&state).show(
                    ui,
                    outer,
                    Palette::new(mode),
                    &mut assets,
                    ("summary", mode as u8),
                );
                assert!(response.is_empty());
            });
        }
    }

    #[test]
    fn a_notice_is_drawn_over_the_chooser_without_closing_it() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let state = ImportState {
            lane: DeviceDirection::Output,
            folder: Some(PathBuf::from("/tmp/empty")),
            summary: None,
            notice: Some(NO_PRESETS_FOUND.to_owned()),
        };
        let outer = Rect::from_min_size(pos2(0.0, 0.0), import::WINDOW_SIZE);
        frame(&ctx, |ui| {
            let response = ImportDialog::new(&state).show(
                ui,
                outer,
                Palette::new(ThemeMode::Dark),
                &mut assets,
                "notice",
            );
            assert!(
                !response.contains(&PresetsAction::CloseImport),
                "the import window must stay open behind the notice"
            );
        });
    }

    #[test]
    fn an_export_that_wrote_nothing_closes_without_announcing_anything() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let state = ExportState {
            presets: vec!["Flat".into()],
            finished: Some(false),
            ..ExportState::default()
        };
        let outer = Rect::from_min_size(pos2(0.0, 0.0), export::WINDOW_SIZE);
        frame(&ctx, |ui| {
            let response = ExportDialog::new(&state).show(
                ui,
                outer,
                Palette::new(ThemeMode::Dark),
                &mut assets,
                "export-empty",
            );
            assert_eq!(response.actions, vec![PresetsAction::CloseExport]);
        });
    }

    #[test]
    fn a_successful_export_waits_for_the_message_to_be_acknowledged() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let state = ExportState {
            presets: vec!["Flat".into()],
            finished: Some(true),
            ..ExportState::default()
        };
        let outer = Rect::from_min_size(pos2(0.0, 0.0), export::WINDOW_SIZE);
        frame(&ctx, |ui| {
            let response = ExportDialog::new(&state).show(
                ui,
                outer,
                Palette::new(ThemeMode::Dark),
                &mut assets,
                "export-done",
            );
            // Nothing until OK is pressed — and then the reveal comes before the close.
            assert!(response.is_empty(), "{:?}", response.actions);
        });
    }

    #[test]
    fn the_export_window_draws_its_list_its_progress_bar_and_its_prompt() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let state = ExportState {
            lane: DeviceDirection::Output,
            presets: (0..30).map(|i| format!("Preset {i}")).collect(),
            selected: [1, 4].into_iter().collect(),
            exporting: true,
            collisions: vec!["Preset 1".into()],
            finished: None,
            end_bands_offered: true,
            end_bands_as_they_are: true,
        };
        let outer = Rect::from_min_size(pos2(0.0, 0.0), export::WINDOW_SIZE);
        frame(&ctx, |ui| {
            let response = ExportDialog::new(&state).show(
                ui,
                outer,
                Palette::new(ThemeMode::Light),
                &mut assets,
                "export",
            );
            assert!(response.is_empty(), "{:?}", response.actions);
        });
    }
}
