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

use crate::{PresetError, load, save};
use fxsound_core::{Preset, Settings};
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

/// The full preset list plus the directories it was built from.
#[derive(Debug, Clone)]
pub struct PresetStore {
    entries: Vec<PresetEntry>,
    factory_dirs: Vec<PathBuf>,
    user_dir: PathBuf,
    autosave_dir: PathBuf,
}

impl PresetStore {
    /// Build a store from the standard locations.
    ///
    /// Factory presets are looked for next to the executable and in the usual install prefixes, so
    /// the binary works both from `cargo run` and from a package.
    #[must_use]
    pub fn with_default_dirs() -> Self {
        let mut factory_dirs = Vec::new();

        // Running from the source tree.
        if let Ok(exe) = std::env::current_exe()
            && let Some(target_dir) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
        {
            factory_dirs.push(target_dir.join("assets/presets/Factsoft"));
            factory_dirs.push(target_dir.join("assets/presets/BonusPresets"));
        }
        // Installed.
        for prefix in ["/usr/share/fxsound", "/usr/local/share/fxsound"] {
            factory_dirs.push(Path::new(prefix).join("presets/Factsoft"));
            factory_dirs.push(Path::new(prefix).join("presets/BonusPresets"));
        }

        let user_dir = Settings::user_preset_dir();
        let autosave_dir = user_dir.join("AutoSave");
        Self {
            entries: Vec::new(),
            factory_dirs,
            user_dir,
            autosave_dir,
        }
    }

    /// Build a store from explicit directories. Used by the tests and by `--preset-dir`.
    #[must_use]
    pub fn with_dirs(factory_dirs: Vec<PathBuf>, user_dir: PathBuf) -> Self {
        let autosave_dir = user_dir.join("AutoSave");
        Self {
            entries: Vec::new(),
            factory_dirs,
            user_dir,
            autosave_dir,
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
        let mut entries = Vec::new();

        for dir in &self.factory_dirs {
            collect(dir, PresetSource::Factory, &mut entries);
        }
        collect(&self.user_dir, PresetSource::User, &mut entries);

        // A user preset with the same name as a factory one wins, matching the original's
        // "load the user's copy" behaviour.
        entries.sort_by(|a, b| {
            sort_key(a)
                .cmp(&sort_key(b))
                .then(a.source.cmp(&b.source).reverse())
        });
        entries.dedup_by(|a, b| a.name == b.name);

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
    pub fn load(&self, name: &str) -> Result<(Preset, bool), PresetError> {
        let entry = self
            .find(name)
            .ok_or_else(|| PresetError::Malformed {
                line: 0,
                expected: "a known preset name",
                found: name.to_owned(),
            })?;

        let autosave = self.autosave_path(name);
        if autosave.is_file() {
            match load(&autosave) {
                Ok(mut preset) => {
                    // The autosave carries the edited values but the canonical name.
                    preset.name = entry.name.clone();
                    return Ok((preset, true));
                }
                Err(err) => log::warn!("{}: {err}; falling back to the original", autosave.display()),
            }
        }

        let mut preset = load(&entry.path)?;
        preset.name = entry.name.clone();
        Ok((preset, false))
    }

    /// Path of the autosave shadow copy for a preset.
    #[must_use]
    pub fn autosave_path(&self, name: &str) -> PathBuf {
        self.autosave_dir.join(format!("{name}.fac"))
    }

    /// Stash the user's unsaved edits so they survive a preset switch or a restart.
    pub fn autosave(&self, preset: &Preset) -> Result<(), PresetError> {
        save(preset, &self.autosave_path(&preset.name))
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
    /// Returns the path it was written to.
    pub fn save_as(&mut self, preset: &Preset, name: &str) -> Result<PathBuf, PresetError> {
        let mut to_save = preset.clone();
        to_save.name = name.to_owned();
        let path = self.user_dir.join(format!("{}.fac", sanitise(name)));
        save(&to_save, &path)?;
        self.clear_autosave(name);
        self.rescan();
        Ok(path)
    }

    /// Delete a user preset. Factory presets are refused, as in the original.
    pub fn delete(&mut self, name: &str) -> Result<(), PresetError> {
        let Some(entry) = self.find(name) else {
            return Ok(());
        };
        if entry.source == PresetSource::Factory {
            return Err(PresetError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{name} is a factory preset"),
            )));
        }
        let path = entry.path.clone();
        std::fs::remove_file(path)?;
        self.clear_autosave(name);
        self.rescan();
        Ok(())
    }

    /// Import a `.fac` file into the user directory, rejecting anything that does not parse.
    ///
    /// Returns the imported preset's name.
    pub fn import(&mut self, source: &Path) -> Result<String, PresetError> {
        let preset = load(source)?;
        let name = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&preset.name)
            .to_owned();
        self.save_as(&preset, &name)?;
        Ok(name)
    }

    /// Export a preset to a directory chosen by the user.
    pub fn export(&self, name: &str, dir: &Path) -> Result<PathBuf, PresetError> {
        let (preset, _) = self.load(name)?;
        let path = dir.join(format!("{}.fac", sanitise(name)));
        save(&preset, &path)?;
        Ok(path)
    }
}

/// Factory presets first in the order the vendor numbered their files, then everything else by
/// name.
///
/// The numbering is the only record of the intended order — `1.fac` is General, `2.fac` is Music —
/// and it is more useful than sorting those twelve alphabetically. The original lists presets in
/// filesystem-glob order, which is arbitrary; this is the deterministic version of the same intent.
fn sort_key(entry: &PresetEntry) -> (u8, u32, String) {
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

fn collect(dir: &Path, source: PresetSource, out: &mut Vec<PresetEntry>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("fac") {
            continue;
        }
        // The original calls getPresetInfo() and skips any file whose name decodes empty; parsing
        // is the equivalent check and also rejects files that are not presets at all.
        match load(&path) {
            Ok(preset) => {
                // The name lives INSIDE the file, not in its filename. The factory presets are
                // shipped as `1.fac`..`12.fac` but call themselves General, Music, Voice and so on
                // — `getPresetInfo()` reads the file (`DfxDspPreset.cpp:363-397`) and the combo box
                // shows what it finds. Only fall back to the stem for a file with no name in it,
                // which the original skips outright.
                let name = if preset.name.trim().is_empty() {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    preset.name.clone()
                };
                if name.is_empty() {
                    log::warn!("{}: no preset name; skipping", path.display());
                    continue;
                }
                out.push(PresetEntry {
                    name,
                    path,
                    source,
                    modified: false,
                });
            }
            Err(err) => log::warn!("{}: {err}; skipping", path.display()),
        }
    }
}

/// Keep a user-chosen preset name usable as a filename.
fn sanitise(name: &str) -> String {
    name.chars()
        .map(|c| if matches!(c, '/' | '\\' | '\0') { '_' } else { c })
        .collect()
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
        assert!(store.entries().len() >= 30, "found {}", store.entries().len());
        assert!(store.find("Jazz").is_some());
        assert!(store.entries().iter().all(|e| e.source == PresetSource::Factory));
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
                entry.path.file_stem().unwrap().to_str().unwrap().parse::<u32>().is_ok(),
                "{name} should come from a numbered file"
            );
        }
        for digit in ["1", "2", "12"] {
            assert!(store.find(digit).is_none(), "{digit} must not be shown as a name");
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
        assert_eq!(store.entries().iter().filter(|e| e.name == "Jazz").count(), 1);

        let (reloaded, _) = store.load("Jazz").expect("reload");
        assert_eq!(reloaded.effect(fxsound_core::Effect::Bass), 1.0);
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
}
