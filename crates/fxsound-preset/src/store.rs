//! Enumerating, selecting and persisting presets.
//!
//! Mirrors `FxController::initPresets()` / `setPreset()` / `autoSavePreset()`
//! (`fxsound/Source/GUI/FxController.cpp:841-876, 1048-1097`) with two deliberate differences,
//! both noted in `docs/spec/11-preset-format.md`:
//!
//! * the original lists presets in filesystem-glob order, which is effectively arbitrary; this
//!   port sorts deterministically — factory presets in their numeric order, then everything else
//!   by name;
//! * paths follow the XDG base directory specification instead of `%APPDATA%`.
//!
//! The store is generic over the file it holds. 0.4.0 has two kinds of preset — the `.fac` set
//! for the speakers and the TOML voice set for the microphone — and one discipline for listing
//! them: which copy wins when a name appears twice, what order the list is in, where an autosave
//! lives, when a `.bak` is kept. A rule that lives in two places is a rule that gets fixed in
//! one place; the shadowing bug this module carried in 0.3.0 would have been copied along with
//! it. [`PresetStore`] is the `.fac` store and [`crate::InputPresetStore`] the voice one.

use crate::trash::{self, Discarded};
use crate::{PresetError, new_preset_name, sanitise_preset_name};
use fxsound_core::{Preset, Settings};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// Where a preset came from, which decides whether it can be overwritten or deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PresetSource {
    /// Shipped with the application; read-only. `PresetType::AppPreset` in the original.
    Factory,
    /// Saved or imported by the user. `PresetType::UserPreset` in the original.
    User,
}

/// One entry in the preset list, matching `FxModel::Preset` (`FxModel.h:34-40`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetEntry {
    pub name: String,
    pub path: PathBuf,
    pub source: PresetSource,
    /// `true` when an autosave exists, i.e. the user changed this preset without saving. Shown as
    /// a trailing `*` in the combo box.
    pub modified: bool,
}

/// A preset file format a [`Store`] can hold.
///
/// The store asks five things of a format: its extension, how to read and write one file, and
/// the name inside it — a preset is listed by the name it calls itself, never by its filename,
/// because the factory `.fac` set ships as `1.fac`..`12.fac` and calls itself General, Music,
/// Voice. The error constructors let the store report an unknown name, an unusable one and one
/// that would take another preset's file in the format's own error type rather than in a third
/// one every caller would have to convert.
pub trait PresetFile: Clone {
    /// The extension the store looks for and writes, without the dot.
    const EXTENSION: &'static str;
    /// What reading or writing one file can fail with.
    type Error: From<std::io::Error> + std::fmt::Display;

    fn load(path: &Path) -> Result<Self, Self::Error>;
    /// Replace `path` atomically.
    fn save(&self, path: &Path) -> Result<(), Self::Error>;
    /// As [`PresetFile::save`], keeping any previous file as `<path>.bak`.
    fn save_with_backup(&self, path: &Path) -> Result<(), Self::Error>;
    fn name(&self) -> &str;
    fn set_name(&mut self, name: &str);
    /// The error for a name the list does not hold.
    fn unknown(name: &str) -> Self::Error;
    /// The error for a name with nothing left in it once it has been made safe as a filename.
    fn empty_name() -> Self::Error;
    /// The error for a name whose file, `file`, is already the listed preset `existing`'s.
    fn shared_file(name: &str, existing: &str, file: &str) -> Self::Error;
    /// The preset as it is written for someone else to open: an export. The same preset unless
    /// the format has a reader elsewhere that expects something of it — the `.fac` a Windows
    /// FxSound reads puts a twenty-band curve back on the Windows ladder (0.4.0 audit R4).
    #[must_use]
    fn exported(&self) -> Self {
        self.clone()
    }
}

impl PresetFile for Preset {
    const EXTENSION: &'static str = "fac";
    type Error = PresetError;

    fn load(path: &Path) -> Result<Self, PresetError> {
        crate::load(path)
    }

    fn save(&self, path: &Path) -> Result<(), PresetError> {
        crate::save(self, path)
    }

    fn save_with_backup(&self, path: &Path) -> Result<(), PresetError> {
        crate::save_with_backup(self, path)
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn set_name(&mut self, name: &str) {
        self.name = name.to_owned();
    }

    fn unknown(name: &str) -> PresetError {
        PresetError::Unknown(name.to_owned())
    }

    fn empty_name() -> PresetError {
        PresetError::MissingName
    }

    fn shared_file(name: &str, existing: &str, file: &str) -> PresetError {
        PresetError::SharedFile {
            name: name.to_owned(),
            existing: existing.to_owned(),
            file: file.to_owned(),
        }
    }

    /// A twenty-band curve on the half-octave ladder is exported on the Windows one, band for
    /// band, which is what the Windows build tunes twenty bands to; this port moves it back when
    /// it reads it ([`fxsound_core::eq::move_off_the_windows_twenty_band_ladder`]).
    ///
    /// Then the first and last band go back inside the ladder's edges (0.4.0 audit R6): here they
    /// can be tuned half a band past them, where the Windows build's wheels cannot follow, so a
    /// first band at 46 Hz is exported at 62.5 Hz. Nothing else moves, so a preset Windows made
    /// goes back out as it came. Importing takes a file as it is, wider centres and all.
    fn exported(&self) -> Self {
        let mut preset = self.clone();
        fxsound_core::eq::move_onto_the_windows_twenty_band_ladder(&mut preset.eq_bands);
        fxsound_core::eq::move_end_bands_back_inside_the_ladder(&mut preset.eq_bands);
        preset
    }
}

/// The full preset list plus the directories it was built from.
#[derive(Debug, Clone)]
pub struct Store<F: PresetFile> {
    entries: Vec<PresetEntry>,
    factory_dirs: Vec<PathBuf>,
    user_dir: PathBuf,
    autosave_dir: PathBuf,
    /// Where a deleted preset goes: `None` for the desktop's home trash, found when a preset is
    /// deleted ([`crate::trash`]); otherwise a directory laid out as a trash, for a store that must
    /// not touch the user's, such as a test's.
    trash_dir: Option<PathBuf>,
    format: PhantomData<F>,
}

/// The `.fac` store: the speakers' presets.
pub type PresetStore = Store<Preset>;

impl Store<Preset> {
    /// Build a store from the standard locations.
    ///
    /// Factory presets are looked for next to the executable and in the usual install prefixes, so
    /// the binary works both from `cargo run` and from a package.
    #[must_use]
    pub fn with_default_dirs() -> Self {
        let mut factory_dirs = Vec::new();

        // Running from the source tree.
        if let Ok(exe) = std::env::current_exe()
            && let Some(target_dir) = exe
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
        {
            factory_dirs.push(target_dir.join("assets/presets/Factsoft"));
            factory_dirs.push(target_dir.join("assets/presets/BonusPresets"));
        }
        // Installed.
        for prefix in ["/usr/share/fxsound", "/usr/local/share/fxsound"] {
            factory_dirs.push(Path::new(prefix).join("presets/Factsoft"));
            factory_dirs.push(Path::new(prefix).join("presets/BonusPresets"));
        }

        Self::with_dirs(factory_dirs, Settings::user_preset_dir()).with_home_trash()
    }
}

impl<F: PresetFile> Store<F> {
    /// Build a store from explicit directories. Used by the tests and by `--preset-dir`.
    ///
    /// Autosaves live in `AutoSave` under the user directory, whichever directory that is.
    ///
    /// A store built this way keeps what it deletes in a trash of its own, `.Trash` under the user
    /// directory, so that a test's deletions never reach the desktop's; the stores built from the
    /// standard locations use the desktop's ([`Store::with_home_trash`]).
    #[must_use]
    pub fn with_dirs(factory_dirs: Vec<PathBuf>, user_dir: PathBuf) -> Self {
        let autosave_dir = user_dir.join("AutoSave");
        let trash_dir = Some(user_dir.join(".Trash"));
        Self {
            entries: Vec::new(),
            factory_dirs,
            user_dir,
            autosave_dir,
            trash_dir,
            format: PhantomData,
        }
    }

    /// Send deleted presets to `dir`, laid out as a trash (`files/`, `info/`).
    #[must_use]
    pub fn with_trash(mut self, dir: PathBuf) -> Self {
        self.trash_dir = Some(dir);
        self
    }

    /// Send deleted presets to the desktop's home trash, where a file manager can restore them
    /// (0.4.0 audit #16).
    #[must_use]
    pub fn with_home_trash(mut self) -> Self {
        self.trash_dir = None;
        self
    }

    /// Where user presets are saved.
    #[must_use]
    pub fn user_dir(&self) -> &Path {
        &self.user_dir
    }

    /// Where factory presets are looked for, in search order — what `fxsound --self-test` reads
    /// back, so it checks the directories this store will actually use rather than a copy of the
    /// list that could drift from it.
    #[must_use]
    pub fn factory_dirs(&self) -> &[PathBuf] {
        &self.factory_dirs
    }

    /// Re-scan every directory. Unreadable directories are skipped, not fatal: a missing factory
    /// directory must not stop the application from starting.
    pub fn rescan(&mut self) {
        let mut factory = Vec::new();
        for dir in &self.factory_dirs {
            collect::<F>(dir, PresetSource::Factory, &mut factory);
        }
        let mut user = Vec::new();
        collect::<F>(&self.user_dir, PresetSource::User, &mut user);

        let mut entries = merge(factory, user);
        for entry in &mut entries {
            entry.modified = self.autosave_path(&entry.name).is_file();
        }
        self.entries = entries;
    }

    /// The presets, in display order.
    #[must_use]
    pub fn entries(&self) -> &[PresetEntry] {
        &self.entries
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn find(&self, name: &str) -> Option<&PresetEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.name == name)
    }

    /// Load a preset by name, preferring its autosave when one exists.
    ///
    /// Returns the preset and whether it came from the autosave, which is what makes the GUI show
    /// the `*` marker.
    ///
    /// # Errors
    /// The name is not in the list, or the file it names cannot be read.
    pub fn load(&self, name: &str) -> Result<(F, bool), F::Error> {
        let entry = self.find(name).ok_or_else(|| F::unknown(name))?;

        let autosave = self.autosave_path(name);
        if autosave.is_file() {
            match F::load(&autosave) {
                Ok(mut preset) => {
                    // The autosave carries the edited values but the canonical name.
                    preset.set_name(&entry.name);
                    return Ok((preset, true));
                }
                Err(err) => log::warn!(
                    "{}: {err}; falling back to the original",
                    autosave.display()
                ),
            }
        }

        let mut preset = F::load(&entry.path)?;
        preset.set_name(&entry.name);
        Ok((preset, false))
    }

    /// Load a preset as last **saved**: its own file, never its autosave.
    ///
    /// What an export hands on and a rename moves — both are about the file the name stands for,
    /// and unsaved edits are not what that name says.
    ///
    /// # Errors
    /// The name is not in the list, or its file cannot be read.
    pub fn load_saved(&self, name: &str) -> Result<F, F::Error> {
        let entry = self.find(name).ok_or_else(|| F::unknown(name))?;
        let mut preset = F::load(&entry.path)?;
        preset.set_name(&entry.name);
        Ok(preset)
    }

    /// Path of the autosave shadow copy for a preset.
    #[must_use]
    pub fn autosave_path(&self, name: &str) -> PathBuf {
        self.autosave_dir
            .join(format!("{}.{}", sanitise_preset_name(name), F::EXTENSION))
    }

    /// Stash the user's unsaved edits so they survive a preset switch or a restart.
    ///
    /// # Errors
    /// The preset's name leaves nothing to file it under, or the file cannot be written.
    pub fn autosave(&self, preset: &F) -> Result<(), F::Error> {
        let path = self.autosave_dir.join(Self::file_name(preset.name())?);
        preset.save(&path)
    }

    /// Drop a preset's autosave, clearing its modified marker.
    pub fn clear_autosave(&mut self, name: &str) {
        let path = self.autosave_path(name);
        if path.exists()
            && let Err(err) = std::fs::remove_file(&path)
        {
            log::warn!("{}: {err}", path.display());
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.name == name) {
            entry.modified = false;
        }
    }

    /// Save a preset under a name, as a user preset, and refresh the list.
    ///
    /// Returns the path it was written to. Saving under a name already in the list is the
    /// overwrite: the user's copy replaces a factory one, or their own earlier one with a `.bak`
    /// kept. Saving under a name that would take *another* listed preset's file — `Mu:sic`
    /// beside `Music` — is refused, because that overwrite is one the list could never show
    /// (`file_name` below says why one file can have two names).
    ///
    /// # Errors
    /// The name leaves nothing to file it under, its file belongs to a preset of a different
    /// name, or the file cannot be written.
    pub fn save_as(&mut self, preset: &F, name: &str) -> Result<PathBuf, F::Error> {
        let file = Self::file_name(name)?;
        if let Some(other) = self.holder_of(&file, name) {
            return Err(F::shared_file(name, &other.name, &file));
        }
        let mut to_save = preset.clone();
        to_save.set_name(name);
        let path = self.user_dir.join(file);
        // The user-facing overwrite: worth keeping the previous version, unlike the autosave.
        to_save.save_with_backup(&path)?;
        self.clear_autosave(name);
        self.rescan();
        Ok(path)
    }

    /// Delete a user preset: its file goes to the desktop's trash, where a file manager can
    /// restore it, or when the trash cannot take it, is set aside beside itself as
    /// `<file>.1.bak` or the next free number ([`trash::set_aside`]) (0.4.0 audit #16; the
    /// original deletes it for good). Factory presets are refused, as in the original.
    ///
    /// Its unsaved edits, the autosave, go the same way after it rather than being dropped: a
    /// preset with unsaved changes can be deleted, and those edits exist nowhere else. The trash
    /// records each file's own place, so restoring both puts the preset back in the list with its
    /// edits and its `*`. Should the autosave move nowhere at all, it is left where it is.
    ///
    /// Returns where the preset's file went, or `None` for a name the list does not hold.
    ///
    /// # Errors
    /// The preset is a factory one, or its file can be neither trashed nor renamed.
    pub fn delete(&mut self, name: &str) -> Result<Option<Discarded>, F::Error> {
        let Some(entry) = self.find(name) else {
            return Ok(None);
        };
        if entry.source == PresetSource::Factory {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{name} is a factory preset"),
            )
            .into());
        }
        let path = entry.path.clone();
        let discarded = self.discard(&path)?;
        let autosave = self.autosave_path(name);
        if autosave.is_file() {
            match self.discard(&autosave) {
                Ok(Discarded::Trash(to) | Discarded::Backup(to)) => {
                    log::info!("{name}: its unsaved changes went to {}", to.display());
                }
                Err(err) => log::warn!("{}: left where it is: {err}", autosave.display()),
            }
        }
        self.rescan();
        Ok(Some(discarded))
    }

    /// [`trash::discard`] into this store's trash.
    fn discard(&self, path: &Path) -> std::io::Result<Discarded> {
        match &self.trash_dir {
            Some(dir) => trash::discard_into(path, dir),
            None => trash::discard(path),
        }
    }

    /// Give the user preset `old` the name `new` (0.4.0 audit #19).
    ///
    /// A move, not a copy: the file is rewritten in place with the new name inside it and then
    /// renamed with [`std::fs::rename`], and its autosave, if it has one, is renamed the same way.
    /// The original saves a copy under the new name and deletes the old file
    /// (`FxController.cpp:1244-1276`), and on a filesystem that folds case — a FAT or exFAT stick,
    /// a case-insensitive ext4 directory — `rock.fac` and `Rock.fac` are one file, so renaming
    /// `rock` to `Rock` that way wrote the new file over the old one and then deleted it. Renaming
    /// the one file cannot. The name inside is written first, so that a rename that then fails
    /// still leaves a preset the list shows under its new name.
    ///
    /// A name that would take another listed preset's file is refused, as [`Store::save_as`]
    /// refuses it; the preset's own file is not another's, so a change of case alone is allowed.
    /// An unlisted file already under the new name is set aside as `<file>.1.bak` or the next free
    /// number ([`trash::set_aside`]), never over a `.bak` already there. An autosave already under
    /// the new name, when the preset brings none of its own, goes to the trash the way a deleted
    /// preset's does: it holds another preset's edits, never this one's.
    ///
    /// # Errors
    /// `old` is not a user preset, `new` leaves nothing to file it under or is another preset's
    /// file, or the files cannot be written or moved.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), F::Error> {
        let entry = self.find(old).ok_or_else(|| F::unknown(old))?;
        if entry.source == PresetSource::Factory {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{old} is a factory preset"),
            )
            .into());
        }
        let from = entry.path.clone();
        let file = Self::file_name(new)?;
        // Another preset of exactly the new name, or one whose file the new name would take.
        if let Some(other) = self
            .find(new)
            .filter(|other| other.name != old)
            .or_else(|| self.holder_of(&file, new).filter(|other| other.name != old))
        {
            return Err(F::shared_file(new, &other.name, &file));
        }
        let to = self.user_dir.join(&file);

        let mut preset = F::load(&from)?;
        preset.set_name(new);
        // Another file already there that is not this one: kept, beside every other `.bak`.
        if to != from && to.exists() && !same_file(&from, &to) {
            trash::set_aside(&to)?;
        }
        preset.save(&from)?;
        std::fs::rename(&from, &to)?;

        let old_autosave = self.autosave_path(old);
        let new_autosave = self.autosave_dir.join(&file);
        if old_autosave.is_file() {
            let moved = F::load(&old_autosave).and_then(|mut stash| {
                stash.set_name(new);
                stash.save(&old_autosave)?;
                std::fs::rename(&old_autosave, &new_autosave)?;
                Ok(())
            });
            if let Err(err) = moved {
                log::warn!("{}: {err}", old_autosave.display());
            }
        } else if new_autosave.is_file() {
            // An autosave already under the new name is not this preset's: a 0.3.0 microphone
            // curve stashed under a voice preset's name, the edits of a preset deleted by hand or
            // one whose autosave the trash would not take. Left there, it would be read as the
            // renamed preset's unsaved changes — the `*` on a preset that had none, and its curve
            // loaded in place of the file's on the next pick or start (0.4.0 review FA). It goes
            // where a deleted preset's edits go, not over them.
            if let Err(err) = self.discard(&new_autosave) {
                log::warn!("{}: {err}; removing it", new_autosave.display());
                if let Err(err) = std::fs::remove_file(&new_autosave) {
                    log::warn!("{}: {err}", new_autosave.display());
                }
            }
        }
        self.rescan();
        Ok(())
    }

    /// Import a preset file into the user directory, rejecting anything that does not parse.
    ///
    /// Returns the imported preset's name, which is the file's stem: a file someone chose to
    /// import is a file they know by its name on disk. The stem is a new name, so it is made one
    /// every FxSound can read ([`new_preset_name`]), and that is the name written inside the
    /// copy too (0.4.0 audit #15): a file called `a:b.fac` used to be imported as `ab.fac` with
    /// `a:b` inside it, a name the Windows build could never file.
    ///
    /// An import adds a preset and never replaces one of the user's: a name a user preset already
    /// has, or a file already in the user directory — which on a filesystem that folds case is
    /// also `rock.fac` when `Rock.fac` is there — is refused rather than handed to
    /// [`Store::save_as`], whose overwrite would leave the earlier preset only in a `.bak` the
    /// list does not show. A factory preset's name can still be taken, as a save can take it: the
    /// user's copy then stands in for the factory one. The controller refuses that too, and any
    /// name already listed in another case, before it asks.
    ///
    /// # Errors
    /// The file does not parse, its name leaves nothing to file it under, is a user preset's or
    /// another listed preset's file, or the copy cannot be saved into the user directory.
    pub fn import(&mut self, source: &Path) -> Result<String, F::Error> {
        let preset = F::load(source)?;
        let name = new_preset_name(
            source
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_else(|| preset.name()),
        );
        let to = self.user_dir.join(Self::file_name(&name)?);
        if self
            .find(&name)
            .is_some_and(|entry| entry.source == PresetSource::User)
            || to.exists()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{name}: {} is already there", to.display()),
            )
            .into());
        }
        self.save_as(&preset, &name)?;
        Ok(name)
    }

    /// Export a preset to a directory chosen by the user.
    ///
    /// What is exported is the preset as last **saved**, never its autosave: an export is a file
    /// someone hands on under the preset's name, and unsaved edits are not what that name says
    /// (upstream PR #155, `FxController.cpp:1411`, which exports `preset.path`). Save first to
    /// export the edits.
    ///
    /// # Errors
    /// The name is unknown, leaves nothing to file it under, or the file cannot be written.
    pub fn export(&self, name: &str, dir: &Path) -> Result<PathBuf, F::Error> {
        let preset = self.load_saved(name)?.exported();
        let path = dir.join(Self::file_name(name)?);
        preset.save(&path)?;
        Ok(path)
    }

    /// The file a name is stored under: the sanitised stem and the format's extension.
    ///
    /// Every route from a name to a filename goes through here and so through
    /// [`sanitise_preset_name`] — the same function the command line applies — which is what
    /// keeps a preset saved from the window and one saved from a script in the same file.
    ///
    /// The sanitiser is not one-to-one, and that is the cost of keeping the typed name inside
    /// the file: `Mu:sic` and `Music` are two names and one stem, so two listed presets can lay
    /// claim to one file. Between two user presets it is the same `.fac`; beside a factory
    /// preset, which lives under a numbered file, it is the same autosave and the same export,
    /// so an edit to one would surface as the other's unsaved change. [`Self::save_as`] refuses
    /// to create either state, through `holder_of`. The state can still arrive from files put in
    /// the directory by hand, which `rescan` lists as the two names they are; the shared autosave
    /// slot is then the one thing the list cannot tell apart. Stems are compared the way the
    /// filesystem compares them, exactly; the case-insensitive rule between *names* is the
    /// controller's, as the sanitiser's own note says.
    ///
    /// Public so a caller that has to know where a preset will land — the export window, asking
    /// about a file already there before it writes — asks the store rather than keeping a copy of
    /// the rule that could drift from it.
    ///
    /// # Errors
    /// The name leaves nothing to file it under.
    pub fn file_name(name: &str) -> Result<String, F::Error> {
        let stem = sanitise_preset_name(name);
        if stem.is_empty() {
            return Err(F::empty_name());
        }
        Ok(format!("{stem}.{}", F::EXTENSION))
    }

    /// The listed preset, if any, other than `name` itself whose file is `file`.
    ///
    /// An entry whose own name sanitises to nothing has no file to hold, so it can never be the
    /// holder; the `Ok` filter is that, not an error swallowed.
    fn holder_of(&self, file: &str, name: &str) -> Option<&PresetEntry> {
        self.entries.iter().find(|entry| {
            entry.name != name && Self::file_name(&entry.name).is_ok_and(|held| held == file)
        })
    }
}

/// Whether two paths are one file: the same inode, which on a filesystem that folds case is what
/// `rock.fac` and `Rock.fac` are.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The order the list is in: numbered factory files first, by number; everything else by name.
type SortKey = (u8, u32, String);

/// Factory presets first in the order the vendor numbered their files, then everything else by
/// name.
///
/// The numbering is the only record of the intended order — `1.fac` is General, `2.fac` is Music —
/// and it is more useful than sorting those twelve alphabetically. The original lists presets in
/// filesystem-glob order, which is arbitrary; this is the deterministic version of the same intent.
fn sort_key(entry: &PresetEntry) -> SortKey {
    let numbered_file = entry
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u32>().ok());
    match numbered_file {
        Some(n) if entry.source == PresetSource::Factory => (0, n, String::new()),
        _ => (1, 0, entry.name.to_lowercase()),
    }
}

/// One entry per name, with the user's copy winning, in display order.
///
/// The user's copy replaces the factory one *in place*: it takes the factory entry's sort key,
/// so a user "General" still leads the list where the factory General did, rather than sinking
/// into the alphabetical rest. 0.3.0 sorted first and then removed *consecutive* duplicates,
/// which is why a user preset named like a numbered factory one survived beside it — the two
/// sorted apart, both were listed, and `find` returned the factory copy. A name found in two
/// factory directories (the source tree and an installed package, say) is listed from the first;
/// two user files carrying the same name are listed from the first in path order, with a warning.
fn merge(factory: Vec<PresetEntry>, user: Vec<PresetEntry>) -> Vec<PresetEntry> {
    let mut keyed: Vec<(SortKey, PresetEntry)> = Vec::with_capacity(factory.len() + user.len());
    for entry in factory {
        if keyed.iter().any(|(_, kept)| kept.name == entry.name) {
            continue;
        }
        keyed.push((sort_key(&entry), entry));
    }
    for entry in user {
        match keyed.iter_mut().find(|(_, kept)| kept.name == entry.name) {
            Some((_, kept)) if kept.source == PresetSource::Factory => *kept = entry,
            Some((_, kept)) => log::warn!(
                "{}: another user preset is already named {:?} ({}); skipping",
                entry.path.display(),
                entry.name,
                kept.path.display()
            ),
            None => keyed.push((sort_key(&entry), entry)),
        }
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, entry)| entry).collect()
}

fn collect<F: PresetFile>(dir: &Path, source: PresetSource, out: &mut Vec<PresetEntry>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(F::EXTENSION))
        {
            continue;
        }
        // The original calls getPresetInfo() and skips any file whose name decodes empty; parsing
        // is the equivalent check and also rejects files that are not presets at all.
        match F::load(&path) {
            Ok(preset) => {
                // The name lives INSIDE the file, not in its filename. The factory presets are
                // shipped as `1.fac`..`12.fac` but call themselves General, Music, Voice and so on
                // — `getPresetInfo()` reads the file (`DfxDspPreset.cpp:363-397`) and the combo box
                // shows what it finds. Only fall back to the stem for a file with no name in it,
                // which the original skips outright.
                let name = if preset.name().trim().is_empty() {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    preset.name().to_owned()
                };
                if name.is_empty() {
                    log::warn!("{}: no preset name; skipping", path.display());
                    continue;
                }
                found.push(PresetEntry {
                    name,
                    path,
                    source,
                    modified: false,
                });
            }
            Err(err) => log::warn!("{}: {err}; skipping", path.display()),
        }
    }
    // `read_dir` order is whatever the filesystem feels like; the list must not depend on it.
    found.sort_by(|a, b| a.path.cmp(&b.path));
    out.extend(found);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::test_support::ScratchDir;

    fn assets() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/presets")
            .canonicalize()
            .expect("assets/presets exists")
    }

    fn store_in(tmp: &Path) -> PresetStore {
        let assets = assets();
        let mut store = PresetStore::with_dirs(
            vec![assets.join("Factsoft"), assets.join("BonusPresets")],
            tmp.to_path_buf(),
        );
        store.rescan();
        store
    }

    /// A new directory of the test's own, removed when the test ends — also when it panics.
    fn tempdir(tag: &str) -> ScratchDir {
        ScratchDir::new(&format!("preset-test-{tag}"))
    }

    /// The twelve factory presets, in the order their filenames number them.
    const FACTORY: [&str; 12] = [
        "General",
        "Music",
        "Voice",
        "Volume Boost",
        "Gaming",
        "Classic Processing",
        "Light Processing",
        "Bass Boost",
        "Streaming Video",
        "Movies",
        "TV",
        "Transcription",
    ];

    #[test]
    fn finds_every_shipped_preset() {
        let tmp = tempdir("enumerate");
        let store = store_in(&tmp);
        assert!(
            store.entries().len() >= 30,
            "found {}",
            store.entries().len()
        );
        assert!(store.find("Jazz").is_some());
        assert!(
            store
                .entries()
                .iter()
                .all(|e| e.source == PresetSource::Factory)
        );
    }

    #[test]
    fn a_presets_name_comes_from_the_file_not_the_filename() {
        // The factory presets ship as 1.fac..12.fac and call themselves General, Music, Voice…
        // Showing "1" in the combo box was a real bug: the original reads the name out of the file.
        let tmp = tempdir("names");
        let store = store_in(&tmp);
        for name in FACTORY {
            let entry = store
                .find(name)
                .unwrap_or_else(|| panic!("{name} is missing from the list"));
            assert_eq!(entry.source, PresetSource::Factory);
            assert!(
                entry
                    .path
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .parse::<u32>()
                    .is_ok(),
                "{name} should come from a numbered file"
            );
        }
        for digit in ["1", "2", "12"] {
            assert!(
                store.find(digit).is_none(),
                "{digit} must not be shown as a name"
            );
        }
    }

    #[test]
    fn factory_presets_come_first_in_the_order_their_files_are_numbered() {
        let tmp = tempdir("ordering");
        let store = store_in(&tmp);
        let names: Vec<_> = store.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            &names[..FACTORY.len()],
            &FACTORY[..],
            "the twelve factory presets must lead the list in file order"
        );
        // Everything after them is the bonus set, sorted by name.
        let rest: Vec<String> = names[FACTORY.len()..]
            .iter()
            .map(|n| n.to_lowercase())
            .collect();
        let mut sorted = rest.clone();
        sorted.sort();
        assert_eq!(rest, sorted, "the rest of the list must be alphabetical");
    }

    #[test]
    fn a_user_preset_shadows_a_factory_one_of_the_same_name() {
        let tmp = tempdir("shadow");
        let mut store = store_in(&tmp);
        let (mut jazz, _) = store.load("Jazz").expect("load Jazz");
        jazz.set_effect(fxsound_core::Effect::Bass, 1.0);
        store.save_as(&jazz, "Jazz").expect("save");

        let entry = store.find("Jazz").expect("Jazz still listed");
        assert_eq!(entry.source, PresetSource::User);
        assert_eq!(
            store.entries().iter().filter(|e| e.name == "Jazz").count(),
            1
        );

        let (reloaded, _) = store.load("Jazz").expect("reload");
        assert_eq!(reloaded.effect(fxsound_core::Effect::Bass), 1.0);
    }

    #[test]
    fn a_user_preset_named_like_a_numbered_factory_one_wins_and_keeps_its_place() {
        // The 0.3.0 bug. `dedup_by` removes *consecutive* equals, and a user "General.fac" sorted
        // among the alphabetical rest while the factory General led the list by its file number:
        // both survived, and `find` returned the factory copy. "Jazz" above cannot see it — a
        // bonus preset and its shadow sort alike — so this one uses a numbered name.
        let tmp = tempdir("shadow-numbered");
        let mut store = store_in(&tmp);
        let (factory, _) = store.load("General").expect("load General");
        assert_ne!(
            factory.effect(fxsound_core::Effect::Bass),
            1.0,
            "the test needs a value the factory copy does not already hold"
        );
        let mut general = factory;
        general.set_effect(fxsound_core::Effect::Bass, 1.0);
        store.save_as(&general, "General").expect("save");

        assert_eq!(
            store
                .entries()
                .iter()
                .filter(|e| e.name == "General")
                .count(),
            1,
            "listed once"
        );
        let entry = store.find("General").expect("still listed");
        assert_eq!(
            entry.source,
            PresetSource::User,
            "and it is the user's copy"
        );
        assert_eq!(
            store.index_of("General"),
            Some(0),
            "which leads the list where the factory copy did"
        );
        let (reloaded, _) = store.load("General").expect("reload");
        assert_eq!(reloaded.effect(fxsound_core::Effect::Bass), 1.0);
    }

    #[test]
    fn the_same_factory_preset_in_two_directories_is_listed_once() {
        // `with_default_dirs` looks in the source tree and in the install prefixes, so a developer
        // with the package installed sees every factory preset from two places. The first wins.
        let tmp = tempdir("two-factory-dirs");
        let assets = assets();
        let copy = tmp.join("copy");
        std::fs::create_dir_all(&copy).expect("mkdir");
        std::fs::copy(assets.join("Factsoft/1.fac"), copy.join("1.fac")).expect("copy");
        let mut store =
            PresetStore::with_dirs(vec![assets.join("Factsoft"), copy], tmp.join("user"));
        store.rescan();

        assert_eq!(
            store
                .entries()
                .iter()
                .filter(|e| e.name == "General")
                .count(),
            1
        );
        assert!(store.find("General").unwrap().path.starts_with(&assets));
    }

    #[test]
    fn two_user_files_carrying_the_same_name_are_listed_once() {
        let tmp = tempdir("duplicate-user-name");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let preset = Preset {
            name: "Twice".into(),
            ..Preset::default()
        };
        crate::save(&preset, &tmp.join("user/a.fac")).expect("a");
        crate::save(&preset, &tmp.join("user/b.fac")).expect("b");
        store.rescan();

        assert_eq!(store.entries().len(), 1);
        assert!(
            store.entries()[0].path.ends_with("a.fac"),
            "the first in path order wins"
        );
    }

    #[test]
    fn an_unknown_name_is_reported_as_unknown() {
        // Not `Malformed { line: 0 }`: the string reaches the command line and D-Bus, and "line
        // 0: expected a known preset name" describes a file that was never opened.
        let tmp = tempdir("unknown");
        let store = store_in(&tmp);
        let err = store.load("No Such Preset").unwrap_err();
        assert!(
            matches!(&err, PresetError::Unknown(name) if name == "No Such Preset"),
            "{err}"
        );
        assert!(err.to_string().contains("No Such Preset"));
    }

    #[test]
    fn a_name_becomes_a_filename_through_the_one_sanitiser() {
        // 0.3.0 had two sanitisers: the command line stripped nine characters while this store
        // replaced three with underscores, so a name with a `:` typed in the window became a
        // file the Windows build could not open. Both routes now go through
        // `sanitise_preset_name`; the name inside the file stays what was typed.
        let tmp = tempdir("sanitise");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let preset = Preset {
            name: "Mu:sic?".into(),
            ..Preset::default()
        };
        let path = store.save_as(&preset, "Mu:sic?").expect("save");
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("Music.fac"));
        assert_eq!(store.entries()[0].name, "Mu:sic?");

        let exported = store.export("Mu:sic?", &tmp).expect("export");
        assert_eq!(
            exported.file_name().and_then(|n| n.to_str()),
            Some("Music.fac")
        );
        assert!(
            store
                .autosave_path("Mu:sic?")
                .ends_with("AutoSave/Music.fac"),
            "{}",
            store.autosave_path("Mu:sic?").display()
        );
    }

    #[test]
    fn a_name_with_nothing_safe_in_it_is_refused() {
        // Stripping can leave nothing, and `.fac` is not a file anyone can find again.
        let tmp = tempdir("empty-name");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let preset = Preset {
            name: " : ".into(),
            ..Preset::default()
        };
        let err = store.save_as(&preset, " : ").unwrap_err();
        assert!(matches!(err, PresetError::MissingName), "{err}");
        let err = store.autosave(&preset).unwrap_err();
        assert!(matches!(err, PresetError::MissingName), "{err}");

        let written = std::fs::read_dir(tmp.join("user")).map_or(0, Iterator::count);
        assert_eq!(written, 0, "nothing was written");
    }

    #[test]
    fn autosave_marks_a_preset_modified_and_is_preferred_on_load() {
        let tmp = tempdir("autosave");
        let mut store = store_in(&tmp);

        let (mut jazz, was_autosaved) = store.load("Jazz").expect("load");
        assert!(!was_autosaved);
        assert!(!store.find("Jazz").unwrap().modified);

        jazz.set_effect(fxsound_core::Effect::Ambience, 0.75);
        store.autosave(&jazz).expect("autosave");
        store.rescan();
        assert!(store.find("Jazz").unwrap().modified);

        let (loaded, from_autosave) = store.load("Jazz").expect("load autosaved");
        assert!(from_autosave);
        assert!((loaded.effect(fxsound_core::Effect::Ambience) - 0.75).abs() < 0.01);

        store.clear_autosave("Jazz");
        assert!(!store.find("Jazz").unwrap().modified);
        let (_, from_autosave) = store.load("Jazz").expect("load after clear");
        assert!(!from_autosave);
    }

    #[test]
    fn an_export_writes_the_saved_preset_and_never_its_unsaved_edits() {
        // Upstream PR #155: the export went through the autosave-preferring load, so a preset
        // with unsaved edits was handed on under its name carrying edits nobody had saved.
        let tmp = tempdir("export-saved");
        let mut store = store_in(&tmp);
        let (mut jazz, _) = store.load("Jazz").expect("load");
        let saved = jazz.effect(fxsound_core::Effect::Ambience);
        jazz.set_effect(fxsound_core::Effect::Ambience, 0.75);
        store.autosave(&jazz).expect("autosave");
        store.rescan();
        assert!(store.find("Jazz").unwrap().modified);

        let out = tmp.join("export");
        std::fs::create_dir_all(&out).expect("mkdir");
        let path = store.export("Jazz", &out).expect("export");
        let exported = crate::load(&path).expect("parse export");
        assert_eq!(exported.name, "Jazz");
        assert!(
            (exported.effect(fxsound_core::Effect::Ambience) - saved).abs() < 0.01,
            "the saved value, not the edit"
        );
        assert!(
            store.find("Jazz").unwrap().modified,
            "and the edits are still there to save"
        );
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            PresetStore::file_name("Jazz").ok().as_deref(),
            "where the store says it files the name"
        );
    }

    #[test]
    fn loading_as_saved_reads_the_presets_file_past_its_autosave() {
        let tmp = tempdir("load-saved");
        let store = store_in(&tmp);
        let (mut jazz, _) = store.load("Jazz").expect("load");
        let saved = jazz.effect(fxsound_core::Effect::Ambience);
        jazz.set_effect(fxsound_core::Effect::Ambience, 0.75);
        store.autosave(&jazz).expect("autosave");

        let (stashed, from_autosave) = store.load("Jazz").expect("load");
        assert!(from_autosave);
        assert!((stashed.effect(fxsound_core::Effect::Ambience) - 0.75).abs() < 0.01);
        let as_saved = store.load_saved("Jazz").expect("load as saved");
        assert_eq!(as_saved.name, "Jazz");
        assert!((as_saved.effect(fxsound_core::Effect::Ambience) - saved).abs() < 0.01);
        assert!(store.load_saved("Nope").is_err(), "an unknown name");
    }

    #[test]
    fn factory_presets_cannot_be_deleted() {
        let tmp = tempdir("delete");
        let mut store = store_in(&tmp);
        let err = store.delete("Jazz").unwrap_err();
        assert!(matches!(err, PresetError::Io(_)));
        assert!(store.find("Jazz").is_some());
    }

    #[test]
    fn import_round_trips_through_the_user_directory() {
        let tmp = tempdir("import");
        let mut store = store_in(&tmp);
        let source = assets().join("BonusPresets/Metal.fac");

        let name = store.import(&source).expect("import");
        assert_eq!(name, "Metal");
        assert_eq!(store.find("Metal").unwrap().source, PresetSource::User);

        let exported_dir = tmp.join("export");
        std::fs::create_dir_all(&exported_dir).expect("mkdir");
        let path = store.export("Metal", &exported_dir).expect("export");
        assert!(path.is_file());
        let (original, _) = store.load("Metal").expect("load");
        assert_eq!(crate::load(&path).expect("parse export"), original);
    }

    #[test]
    fn overwriting_a_user_preset_keeps_the_previous_version_without_listing_it() {
        let tmp = tempdir("overwrite-backup");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));

        let mut preset = Preset {
            name: "Mine".into(),
            ..Preset::default()
        };
        preset.main_midi[0] = 10;
        store.save_as(&preset, "Mine").expect("first save");

        preset.main_midi[0] = 99;
        let path = store.save_as(&preset, "Mine").expect("overwrite");

        let mut backup = path.as_os_str().to_owned();
        backup.push(".bak");
        let kept = crate::load(std::path::Path::new(&backup)).expect("the backup is a valid .fac");
        assert_eq!(
            kept.main_midi[0], 10,
            "the backup should hold the old value"
        );
        assert_eq!(
            crate::load(&path).expect("load").main_midi[0],
            99,
            "the live file should hold the new one"
        );

        // A `.bak` beside a preset must not become a second entry in the list.
        let names: Vec<_> = store.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["Mine".to_owned()], "listed: {names:?}");
    }

    #[test]
    fn a_name_that_would_take_another_presets_file_is_refused() {
        // `Mu:sic` and `Music` are two names and one file: the sanitiser strips the `:` and both
        // become `Music.fac`. Beside the factory Music the two would share an autosave slot;
        // beside a user Music the save would replace its file — with a `.bak`, but the list
        // would then show one name where two had been saved. Neither is a save, so neither
        // happens.
        let tmp = tempdir("shared-file");
        let mut store = store_in(&tmp);
        assert_eq!(
            store.find("Music").map(|e| e.source),
            Some(PresetSource::Factory)
        );

        let music = Preset {
            name: "Mu:sic".into(),
            ..Preset::default()
        };
        let err = store.save_as(&music, "Mu:sic").unwrap_err();
        let PresetError::SharedFile {
            name,
            existing,
            file,
        } = &err
        else {
            panic!("expected SharedFile, got {err}");
        };
        assert_eq!(name, "Mu:sic");
        assert_eq!(existing, "Music");
        assert_eq!(file, "Music.fac");
        assert!(store.find("Mu:sic").is_none());
        assert!(!tmp.join("Music.fac").exists(), "nothing was written");

        // Between two user presets the file really is the same one.
        let mut mine = Preset {
            name: "Mine".into(),
            ..Preset::default()
        };
        mine.main_midi[0] = 10;
        let path = store.save_as(&mine, "Mine").expect("save Mine");
        let err = store.save_as(&mine, "Mi?ne").unwrap_err();
        assert!(
            matches!(&err, PresetError::SharedFile { existing, .. } if existing == "Mine"),
            "{err}"
        );
        assert!(store.find("Mi?ne").is_none());
        assert_eq!(crate::load(&path).expect("Mine is intact").main_midi[0], 10);
        let mut backup = path.as_os_str().to_owned();
        backup.push(".bak");
        assert!(
            !Path::new(&backup).exists(),
            "a refused save leaves no backup either"
        );

        // The same name is the overwrite, and still goes through with its `.bak`.
        mine.main_midi[0] = 99;
        store.save_as(&mine, "Mine").expect("overwrite Mine");
        assert!(Path::new(&backup).is_file());
        assert_eq!(crate::load(&path).expect("reload").main_midi[0], 99);
    }

    fn twenty_band_preset(name: &str, ladder: &[f32; 20]) -> Preset {
        Preset {
            name: name.into(),
            eq_bands: ladder
                .iter()
                .enumerate()
                .map(|(i, &hz)| fxsound_core::EqBand::new(hz, i as f32 - 10.0))
                .collect(),
            ..Preset::default()
        }
    }

    #[test]
    fn a_deleted_preset_goes_to_the_trash_with_a_record_of_where_it_was() {
        // 0.4.0 audit #16: the original deletes a user preset for good.
        let tmp = tempdir("delete-to-trash");
        let trash = tmp.join("Trash");
        let mut store =
            PresetStore::with_dirs(Vec::new(), tmp.join("user")).with_trash(trash.clone());
        let mut mine = Preset {
            name: "Mine".into(),
            ..Preset::default()
        };
        mine.main_midi[0] = 42;
        let path = store.save_as(&mine, "Mine").expect("save");
        mine.main_midi[0] = 43;
        store.autosave(&mine).expect("autosave");

        let went = store
            .delete("Mine")
            .expect("delete")
            .expect("it was listed");
        let crate::trash::Discarded::Trash(trashed) = went else {
            panic!("expected the trash, got {went:?}");
        };
        assert_eq!(trashed, trash.join("files/Mine.fac"));
        assert!(!path.exists(), "gone from the user directory");
        assert!(store.find("Mine").is_none(), "and from the list");
        assert_eq!(
            crate::load(&trashed)
                .expect("the trashed file is the preset")
                .main_midi[0],
            42,
            "the saved preset, restorable"
        );
        let info = std::fs::read_to_string(trash.join("info/Mine.fac.trashinfo")).expect("info");
        assert!(info.contains("Mine.fac"), "{info}");
        assert!(
            !store.autosave_path("Mine").exists(),
            "its unsaved edits went with it"
        );
        // Deleting a name that is not there is nothing, not an error.
        assert_eq!(store.delete("Mine").expect("no-op"), None);
    }

    #[test]
    fn deleting_a_preset_with_unsaved_changes_puts_them_in_the_trash_and_restoring_both_brings_them_back()
     {
        // The edits exist nowhere but in the autosave, and a modified preset can be deleted:
        // dropping the autosave lost them for good however the preset itself was kept.
        let tmp = tempdir("delete-unsaved");
        let trash = tmp.join("Trash");
        let mut store =
            PresetStore::with_dirs(Vec::new(), tmp.join("user")).with_trash(trash.clone());
        let mut mine = Preset {
            name: "Mine".into(),
            ..Preset::default()
        };
        mine.main_midi[0] = 42;
        let path = store.save_as(&mine, "Mine").expect("save");
        mine.main_midi[0] = 43;
        store.autosave(&mine).expect("autosave");
        store.rescan();
        assert!(store.find("Mine").expect("listed").modified);
        let autosave = store.autosave_path("Mine");

        store
            .delete("Mine")
            .expect("delete")
            .expect("it was listed");
        assert!(!autosave.exists(), "gone from the autosaves");
        // The preset took its own name in the trash, so its edits take the next one.
        let edits = trash.join("files/Mine.2.fac");
        assert_eq!(
            crate::load(&edits)
                .expect("the trashed autosave is a preset")
                .main_midi[0],
            43,
            "the unsaved edits, recoverable"
        );
        let info =
            std::fs::read_to_string(trash.join("info/Mine.2.fac.trashinfo")).expect("its record");
        let autosave = std::path::absolute(&autosave).expect("absolute");
        assert!(
            info.lines()
                .any(|line| line == format!("Path={}", crate::trash::encode_path(&autosave))),
            "restored, it goes back among the autosaves: {info}"
        );

        // Restore both, as a file manager does: each to the place its record names.
        std::fs::rename(trash.join("files/Mine.fac"), &path).expect("restore the preset");
        std::fs::rename(&edits, &autosave).expect("restore its edits");
        store.rescan();
        assert!(
            store.find("Mine").expect("listed again").modified,
            "with its *"
        );
        let (loaded, from_autosave) = store.load("Mine").expect("load");
        assert!(from_autosave);
        assert_eq!(loaded.main_midi[0], 43, "the edits are back");
        assert_eq!(store.load_saved("Mine").expect("saved").main_midi[0], 42);
    }

    #[test]
    fn a_delete_the_trash_cannot_take_keeps_the_preset_its_edits_and_the_overwrites_backup() {
        // The trash under a path that is a file: nothing can be made there, as when it is on
        // another filesystem. The fallback used to rename the preset over the `.bak` an earlier
        // overwrite had left, and to delete the autosave.
        let tmp = tempdir("delete-no-trash");
        let blocked = tmp.join("data");
        std::fs::write(&blocked, b"not a directory").expect("block the trash");
        let mut store =
            PresetStore::with_dirs(Vec::new(), tmp.join("user")).with_trash(blocked.join("Trash"));
        let mut mine = Preset {
            name: "Mine".into(),
            ..Preset::default()
        };
        mine.main_midi[0] = 10;
        store.save_as(&mine, "Mine").expect("save");
        mine.main_midi[0] = 99;
        let path = store.save_as(&mine, "Mine").expect("overwrite");
        let overwritten = tmp.join("user/Mine.fac.bak");
        assert_eq!(
            crate::load(&overwritten)
                .expect("the overwrite's backup")
                .main_midi[0],
            10
        );
        mine.main_midi[0] = 43;
        store.autosave(&mine).expect("autosave");

        let went = store
            .delete("Mine")
            .expect("delete")
            .expect("it was listed");
        let aside = tmp.join("user/Mine.fac.1.bak");
        assert_eq!(went, crate::trash::Discarded::Backup(aside.clone()));
        assert!(!path.exists());
        assert_eq!(
            crate::load(&aside).expect("the deleted preset").main_midi[0],
            99
        );
        assert_eq!(
            crate::load(&overwritten)
                .expect("the overwrite's backup survives")
                .main_midi[0],
            10
        );
        let edits = tmp.join("user/AutoSave/Mine.fac.1.bak");
        assert_eq!(
            crate::load(&edits)
                .expect("the unsaved edits survive")
                .main_midi[0],
            43
        );
        assert!(!store.autosave_path("Mine").exists());
        assert!(store.entries().is_empty(), "{:?}", store.entries());

        // A second overwrite of a new Mine takes only its own `.bak`, never the deleted one.
        store.save_as(&mine, "Mine").expect("a new Mine");
        store.save_as(&mine, "Mine").expect("overwrite it");
        assert_eq!(
            crate::load(&aside)
                .expect("still the deleted preset")
                .main_midi[0],
            99
        );
    }

    #[test]
    fn a_rename_onto_an_unlisted_file_sets_it_aside_beside_the_bak_already_there() {
        let tmp = tempdir("rename-aside");
        let user = tmp.join("user");
        let mut store = PresetStore::with_dirs(Vec::new(), user.clone());
        let mut mine = Preset {
            name: "Mine".into(),
            ..Preset::default()
        };
        mine.main_midi[0] = 42;
        store.save_as(&mine, "Mine").expect("save");
        // Under the new name: a file the list cannot read, and an older `.bak` of it.
        std::fs::write(user.join("New.fac"), b"not a preset").expect("unlisted");
        std::fs::write(user.join("New.fac.bak"), b"older").expect("backup");
        store.rescan();
        assert!(store.find("New").is_none());

        store.rename("Mine", "New").expect("rename");
        assert_eq!(
            crate::load(&user.join("New.fac"))
                .expect("renamed")
                .main_midi[0],
            42
        );
        assert_eq!(std::fs::read(user.join("New.fac.bak")).unwrap(), b"older");
        assert_eq!(
            std::fs::read(user.join("New.fac.1.bak")).unwrap(),
            b"not a preset"
        );
    }

    #[test]
    fn a_rename_that_only_changes_case_keeps_the_one_file() {
        // 0.4.0 audit #19. The original saves a copy and deletes the old file; where the
        // filesystem folds case, `rock.fac` and `Rock.fac` are one file and that deleted it.
        let tmp = tempdir("rename-case");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let mut rock = Preset {
            name: "rock".into(),
            ..Preset::default()
        };
        rock.main_midi[0] = 77;
        let old_path = store.save_as(&rock, "rock").expect("save");

        store.rename("rock", "Rock").expect("rename");
        let names: Vec<_> = store.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["Rock"]);
        let entry = store.find("Rock").expect("listed under the new name");
        assert!(entry.path.ends_with("Rock.fac"), "{}", entry.path.display());
        assert!(!old_path.exists() || same_file(&old_path, &entry.path));
        let reloaded = crate::load(&entry.path).expect("load");
        assert_eq!(reloaded.name, "Rock", "the name inside moved too");
        assert_eq!(reloaded.main_midi[0], 77);
        let files: Vec<_> = std::fs::read_dir(tmp.join("user"))
            .expect("read")
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "fac"))
            .collect();
        assert_eq!(files.len(), 1, "one file, not a copy beside the original");
    }

    #[test]
    fn a_rename_moves_the_autosave_with_the_preset() {
        let tmp = tempdir("rename-autosave");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let mut preset = Preset {
            name: "Old".into(),
            ..Preset::default()
        };
        store.save_as(&preset, "Old").expect("save");
        preset.main_midi[0] = 99;
        store.autosave(&preset).expect("autosave");
        store.rescan();

        store.rename("Old", "New").expect("rename");
        assert!(!store.autosave_path("Old").exists());
        let (loaded, from_autosave) = store.load("New").expect("load");
        assert!(from_autosave, "the unsaved edits came along");
        assert_eq!(loaded.main_midi[0], 99);
        assert_eq!(loaded.name, "New");
        assert!(store.find("New").expect("listed").modified);
    }

    #[test]
    fn a_rename_onto_a_name_with_a_leftover_autosave_leaves_the_preset_unmodified() {
        // FA: 0.3.0 stashed microphone curves as `.fac` autosaves under voice preset names; a
        // clean preset renamed to one of them inherited it as its own unsaved changes.
        let tmp = tempdir("rename-leftover-autosave");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let preset = Preset {
            name: "My EQ".into(),
            main_midi: [10, 0, 0, 0, 0, 0],
            ..Preset::default()
        };
        store.save_as(&preset, "My EQ").expect("save");
        let leftover = Preset {
            name: "Podcast".into(),
            main_midi: [77, 0, 0, 0, 0, 0],
            ..Preset::default()
        };
        store.autosave(&leftover).expect("the leftover");
        store.rescan();
        assert!(!store.find("My EQ").expect("listed").modified);

        store.rename("My EQ", "Podcast").expect("rename");

        let entry = store.find("Podcast").expect("listed");
        assert!(!entry.modified, "no unsaved changes came from the leftover");
        assert!(!store.autosave_path("Podcast").exists());
        let (loaded, from_autosave) = store.load("Podcast").expect("load");
        assert!(!from_autosave);
        assert_eq!(loaded.main_midi, preset.main_midi, "the preset's own curve");
        let trashed = tmp.join("user/.Trash/files");
        assert!(
            std::fs::read_dir(&trashed).is_ok_and(|mut files| files.next().is_some()),
            "the leftover went to the trash, not nowhere"
        );
    }

    #[test]
    fn a_rename_onto_another_presets_file_is_refused_and_changes_nothing() {
        let tmp = tempdir("rename-shared");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let preset = Preset::default();
        store.save_as(&preset, "Music").expect("save Music");
        store.save_as(&preset, "Mine").expect("save Mine");
        let err = store.rename("Mine", "Mu:sic").unwrap_err();
        assert!(
            matches!(&err, PresetError::SharedFile { existing, .. } if existing == "Music"),
            "{err}"
        );
        let mut names: Vec<_> = store.entries().iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["Mine", "Music"]);
        // Nor onto another preset's own name: that preset's file would be put aside for it.
        assert!(store.rename("Mine", "Music").is_err());
        let music = store.find("Music").expect("still listed").path.clone();
        assert!(music.is_file());
        assert!(store.find("Mine").is_some());
        // A factory preset has no file of the user's to rename.
        let mut with_factory = store_in(&tmp.join("factory-case"));
        assert!(with_factory.rename("Jazz", "Jazz 2").is_err());
    }

    #[test]
    fn an_import_never_writes_over_a_user_preset_or_a_file_already_in_the_user_directory() {
        // FA: two files that come out as one new name went through `save_as`'s overwrite, and the
        // second replaced the first, which was left only in a `.bak` the list does not show.
        let tmp = tempdir("import-never-over");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let from = tmp.join("from");
        std::fs::create_dir_all(from.join("again")).expect("mkdir");
        let first = Preset {
            name: "first".into(),
            main_midi: [10, 0, 0, 0, 0, 0],
            ..Preset::default()
        };
        let second = Preset {
            name: "second".into(),
            main_midi: [20, 0, 0, 0, 0, 0],
            ..Preset::default()
        };
        crate::save(&first, &from.join("Night.fac")).expect("write the first");
        crate::save(&second, &from.join("again/Night.fac")).expect("write the second");

        assert_eq!(
            store.import(&from.join("Night.fac")).expect("import"),
            "Night"
        );
        let err = store.import(&from.join("again/Night.fac")).unwrap_err();
        assert!(
            matches!(&err, PresetError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists),
            "{err}"
        );
        let (kept, _) = store.load("Night").expect("load");
        assert_eq!(kept.main_midi, first.main_midi, "the first import stays");
        assert!(
            !tmp.join("user/Night.fac.bak").exists(),
            "nothing was set aside"
        );

        // A file in the user directory the list does not hold is not written over either.
        std::fs::write(tmp.join("user/Loose.fac"), "not a preset").expect("write");
        store.rescan();
        crate::save(&first, &from.join("Loose.fac")).expect("write the source");
        assert!(store.import(&from.join("Loose.fac")).is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.join("user/Loose.fac")).expect("read"),
            "not a preset"
        );
    }

    #[test]
    fn an_import_may_still_stand_in_for_a_factory_preset_of_the_same_name() {
        let tmp = tempdir("import-over-factory");
        let mut store = store_in(&tmp);
        assert_eq!(
            store.find("Jazz").expect("listed").source,
            PresetSource::Factory
        );
        let source_dir = tmp.join("from");
        std::fs::create_dir_all(&source_dir).expect("mkdir");
        crate::save(&Preset::default(), &source_dir.join("Jazz.fac")).expect("write the source");

        assert_eq!(
            store.import(&source_dir.join("Jazz.fac")).expect("import"),
            "Jazz"
        );
        assert_eq!(
            store.find("Jazz").expect("listed").source,
            PresetSource::User
        );
    }

    #[test]
    fn an_imported_file_is_saved_under_a_name_windows_can_file_and_carries_it_inside() {
        // 0.4.0 audit #15: `a:b.fac` became `ab.fac` with `a:b` inside it.
        let tmp = tempdir("import-sanitised");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let source_dir = tmp.join("from");
        std::fs::create_dir_all(&source_dir).expect("mkdir");
        let source = source_dir.join("Mu:sic?.fac");
        crate::save(
            &Preset {
                name: "whatever".into(),
                ..Preset::default()
            },
            &source,
        )
        .expect("write the source");

        let name = store.import(&source).expect("import");
        assert_eq!(name, "Music");
        let entry = store.find("Music").expect("listed");
        assert!(entry.path.ends_with("Music.fac"));
        assert_eq!(crate::load(&entry.path).expect("load").name, "Music");
    }

    #[test]
    fn a_twenty_band_curve_is_exported_on_the_windows_ladder_band_for_band() {
        // 0.4.0 audit R4: the port's twenty bands sit every half octave; a `.fac` for a Windows
        // FxSound carries the ladder it tunes twenty bands to.
        let tmp = tempdir("export-twenty");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let preset = twenty_band_preset("Twenty", &fxsound_core::eq::TWENTY_BAND_CENTRES_HZ);
        let saved = store.save_as(&preset, "Twenty").expect("save");
        assert_eq!(
            crate::load(&saved).expect("load").eq_bands,
            preset.eq_bands,
            "the user's own file keeps the port's ladder"
        );

        let out = tmp.join("out");
        let path = store.export("Twenty", &out).expect("export");
        let exported = crate::load(&path).expect("parse the export");
        let centres: Vec<f32> = exported.eq_bands.iter().map(|b| b.center_hz).collect();
        assert_eq!(centres, fxsound_core::eq::WINDOWS_TWENTY_BAND_CENTRES_HZ);
        let gains = |p: &Preset| p.eq_bands.iter().map(|b| b.boost_db).collect::<Vec<_>>();
        assert_eq!(gains(&exported), gains(&preset), "every gain on its band");

        // A ten-band preset is exported as it is.
        let ten = Preset {
            name: "Ten".into(),
            ..Preset::default()
        };
        store.save_as(&ten, "Ten").expect("save ten");
        let path = store.export("Ten", &out).expect("export ten");
        assert_eq!(crate::load(&path).expect("load").eq_bands, ten.eq_bands);
    }

    #[test]
    fn end_bands_tuned_past_the_windows_ranges_are_exported_inside_them() {
        // 0.4.0 audit R6: here band 1 of ten reaches down to 46 Hz and band 10 up to 20 kHz; the
        // Windows build's wheels stop at 62.5 Hz and 16 kHz.
        let tmp = tempdir("export-wide-ends");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let mut wide = Preset {
            name: "Wide".into(),
            ..Preset::default()
        };
        wide.eq_bands[0] = fxsound_core::EqBand::new(46.0, 6.0);
        wide.eq_bands[9] = fxsound_core::EqBand::new(20000.0, -3.0);
        let saved = store.save_as(&wide, "Wide").expect("save");
        assert_eq!(
            crate::load(&saved).expect("load").eq_bands,
            wide.eq_bands,
            "the user's own file keeps where the bands were tuned"
        );

        let path = store.export("Wide", &tmp.join("out")).expect("export");
        let exported = crate::load(&path).expect("parse the export");
        assert_eq!(exported.eq_bands[0], fxsound_core::EqBand::new(62.5, 6.0));
        assert_eq!(
            exported.eq_bands[9],
            fxsound_core::EqBand::new(16000.0, -3.0)
        );
        assert_eq!(
            exported.eq_bands[1..9],
            wide.eq_bands[1..9],
            "the rest as it is"
        );
    }

    #[test]
    fn importing_keeps_a_centre_outside_the_windows_ranges() {
        // The wider end bands are read back as they are: an import is not an export.
        let tmp = tempdir("import-wide-ends");
        let mut wide = Preset {
            name: "Wide".into(),
            ..Preset::default()
        };
        wide.eq_bands[0].center_hz = 46.0;
        wide.eq_bands[9].center_hz = 20000.0;
        let source = tmp.join("Wide.fac");
        crate::save(&wide, &source).expect("write the source");

        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        store.rescan();
        let name = store.import(&source).expect("import");
        let entry = store.find(&name).expect("listed");
        assert_eq!(
            crate::load(&entry.path).expect("load").eq_bands,
            wide.eq_bands
        );
    }

    #[test]
    fn a_shipped_preset_is_exported_as_it_came_unless_its_first_band_is_below_the_ladder() {
        // Every preset the Windows build ships starts at 62.5 Hz and ends at or below 16 kHz, where
        // its wheels stop, and goes back out exactly as it came. Two this port revoiced reach
        // lower, and a `.fac` for Windows carries their first band at 62.5 Hz (0.4.0 audit R6).
        let tmp = tempdir("export-shipped");
        let store = store_in(&tmp);
        assert!(store.entries().len() > 30, "the factory presets are there");
        let out = tmp.join("out");
        let mut moved = Vec::new();
        for entry in store.entries() {
            let original = crate::load(&entry.path).expect("load a shipped preset");
            let path = store.export(&entry.name, &out).expect("export");
            let exported = crate::load(&path).expect("parse the export");
            if exported.eq_bands == original.eq_bands {
                continue;
            }
            moved.push(entry.name.clone());
            assert!(original.eq_bands[0].center_hz < 62.5, "{}", entry.name);
            assert_eq!(exported.eq_bands[0].center_hz, 62.5, "{}", entry.name);
            assert_eq!(
                exported.eq_bands[0].boost_db, original.eq_bands[0].boost_db,
                "{}",
                entry.name
            );
            assert_eq!(
                exported.eq_bands[1..],
                original.eq_bands[1..],
                "{}",
                entry.name
            );
        }
        moved.sort();
        assert_eq!(moved, ["Competitive FPS", "Trap"]);
    }

    #[test]
    fn a_store_on_explicit_directories_keeps_what_it_deletes_to_itself() {
        // A test's deletion must never reach the desktop's trash, and the store's own trash must
        // never show up in its list.
        let tmp = tempdir("private-trash");
        let mut store = PresetStore::with_dirs(Vec::new(), tmp.join("user"));
        store.save_as(&Preset::default(), "Gone").expect("save");
        store.delete("Gone").expect("delete");
        assert!(tmp.join("user/.Trash/files/Gone.fac").is_file());
        store.rescan();
        assert!(store.entries().is_empty(), "{:?}", store.entries());
        assert!(PresetStore::with_default_dirs().trash_dir.is_none());
    }
}
