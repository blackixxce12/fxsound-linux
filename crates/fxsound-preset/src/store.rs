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

use crate::{PresetError, sanitise_preset_name};
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
}

/// The full preset list plus the directories it was built from.
#[derive(Debug, Clone)]
pub struct Store<F: PresetFile> {
    entries: Vec<PresetEntry>,
    factory_dirs: Vec<PathBuf>,
    user_dir: PathBuf,
    autosave_dir: PathBuf,
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

        Self::with_dirs(factory_dirs, Settings::user_preset_dir())
    }
}

impl<F: PresetFile> Store<F> {
    /// Build a store from explicit directories. Used by the tests and by `--preset-dir`.
    ///
    /// Autosaves live in `AutoSave` under the user directory, whichever directory that is.
    #[must_use]
    pub fn with_dirs(factory_dirs: Vec<PathBuf>, user_dir: PathBuf) -> Self {
        let autosave_dir = user_dir.join("AutoSave");
        Self {
            entries: Vec::new(),
            factory_dirs,
            user_dir,
            autosave_dir,
            format: PhantomData,
        }
    }

    /// Where user presets are saved.
    #[must_use]
    pub fn user_dir(&self) -> &Path {
        &self.user_dir
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

    /// Delete a user preset. Factory presets are refused, as in the original.
    ///
    /// # Errors
    /// The preset is a factory one, or its file cannot be removed.
    pub fn delete(&mut self, name: &str) -> Result<(), F::Error> {
        let Some(entry) = self.find(name) else {
            return Ok(());
        };
        if entry.source == PresetSource::Factory {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{name} is a factory preset"),
            )
            .into());
        }
        let path = entry.path.clone();
        std::fs::remove_file(path)?;
        self.clear_autosave(name);
        self.rescan();
        Ok(())
    }

    /// Import a preset file into the user directory, rejecting anything that does not parse.
    ///
    /// Returns the imported preset's name, which is the file's stem: a file someone chose to
    /// import is a file they know by its name on disk.
    ///
    /// # Errors
    /// The file does not parse, or cannot be saved into the user directory.
    pub fn import(&mut self, source: &Path) -> Result<String, F::Error> {
        let preset = F::load(source)?;
        let name = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| preset.name())
            .to_owned();
        self.save_as(&preset, &name)?;
        Ok(name)
    }

    /// Export a preset to a directory chosen by the user.
    ///
    /// # Errors
    /// The name is unknown, leaves nothing to file it under, or the file cannot be written.
    pub fn export(&self, name: &str, dir: &Path) -> Result<PathBuf, F::Error> {
        let (preset, _) = self.load(name)?;
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
    fn file_name(name: &str) -> Result<String, F::Error> {
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

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fxsound-preset-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
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
}
