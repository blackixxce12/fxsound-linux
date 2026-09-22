//! The voice-preset store: the microphone side's [`PresetStore`](crate::PresetStore).
//!
//! 0.3.0 listed the shipped voice presets straight out of [`InputPreset::load_shipped`] and let
//! nothing be saved beside them — the modified marker, autosave and rename machinery all ran on
//! the `.fac` list and quietly did nothing for a microphone. In 0.4.0 a voice preset is a thing
//! the user edits, saves, renames and the calibration wizard writes, so it gets the same store
//! the `.fac` set has, with the same discipline: the shipped set is factory and read-only, the
//! user's copies live under `presets/Input/` and win over a factory preset of the same name,
//! unsaved edits shadow a preset from `Input/AutoSave/`, and an overwrite keeps a `.bak`.
//!
//! Everything but the directories and the file format is [`Store`]'s; this module supplies
//! those two, and the tests that the voice set obeys the same rules as the speakers'.

use crate::input::{Error, InputPreset};
use crate::store::{PresetFile, Store};
use fxsound_core::Settings;
use std::path::Path;

impl PresetFile for InputPreset {
    const EXTENSION: &'static str = "toml";
    type Error = Error;

    fn load(path: &Path) -> Result<Self, Error> {
        InputPreset::load(path)
    }

    fn save(&self, path: &Path) -> Result<(), Error> {
        InputPreset::save(self, path)
    }

    fn save_with_backup(&self, path: &Path) -> Result<(), Error> {
        InputPreset::save_with_backup(self, path)
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn set_name(&mut self, name: &str) {
        self.name = name.to_owned();
    }

    fn unknown(name: &str) -> Error {
        Error::Unknown(name.to_owned())
    }

    fn empty_name() -> Error {
        Error::MissingName
    }

    fn shared_file(name: &str, existing: &str, file: &str) -> Error {
        Error::SharedFile {
            name: name.to_owned(),
            existing: existing.to_owned(),
            file: file.to_owned(),
        }
    }
}

/// The TOML store: the microphone's presets.
pub type InputPresetStore = Store<InputPreset>;

impl Store<InputPreset> {
    /// Build a store from the standard locations.
    ///
    /// Factory presets come from wherever [`InputPreset::default_dirs`] finds them — the source
    /// tree first, then the install prefixes — and user presets live in an `Input` directory
    /// under the `.fac` user directory, so the two sets never share a namespace: a `.toml` and a
    /// `.fac` both called "Voice" would otherwise be one name with two meanings. Autosaves go
    /// under `Input/AutoSave/`.
    #[must_use]
    pub fn with_default_dirs() -> Self {
        Self::with_dirs(
            InputPreset::default_dirs(),
            Settings::user_preset_dir().join("Input"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Denoise, Gate};
    use crate::{PresetSource, sanitise_preset_name};
    use fxsound_core::DenoiseLevel;
    use std::path::PathBuf;

    fn assets() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/presets/Input")
            .canonicalize()
            .expect("assets/presets/Input exists")
    }

    fn store_in(tmp: &Path) -> InputPresetStore {
        let mut store = InputPresetStore::with_dirs(vec![assets()], tmp.to_path_buf());
        store.rescan();
        store
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fxsound-input-store-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn named(name: &str) -> InputPreset {
        InputPreset {
            name: name.to_owned(),
            ..InputPreset::default()
        }
    }

    #[test]
    fn finds_every_shipped_voice_preset() {
        let tmp = tempdir("enumerate");
        let store = store_in(&tmp);
        let files = std::fs::read_dir(assets())
            .expect("read")
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
            .count();
        assert_eq!(store.entries().len(), files, "one entry per shipped file");
        assert!(
            store.entries().len() >= 13,
            "found {}",
            store.entries().len()
        );
        for name in [
            "Clean Voice",
            "Flat",
            "Gaming Headset",
            "Noisy Room",
            "Mechanical Keyboard",
        ] {
            assert_eq!(
                store.find(name).map(|e| e.source),
                Some(PresetSource::Factory),
                "{name}"
            );
        }
    }

    #[test]
    fn every_shipped_voice_preset_is_filed_under_its_own_name() {
        // The shipped files are named for the preset inside them — `Clean Voice.toml` — and a
        // name that the sanitiser would change is a name whose user copy lands in a different
        // file from the factory one. Both are invariants of the set, so both are checked here.
        let tmp = tempdir("filenames");
        let store = store_in(&tmp);
        for entry in store.entries() {
            assert_eq!(
                sanitise_preset_name(&entry.name),
                entry.name,
                "{}: the name is not usable as a filename as written",
                entry.name
            );
            assert_eq!(
                entry.path.file_stem().and_then(|s| s.to_str()),
                Some(entry.name.as_str()),
                "{}: the file is not named for the preset inside it",
                entry.path.display()
            );
        }
    }

    #[test]
    fn a_presets_name_comes_from_the_file_not_the_filename() {
        let tmp = tempdir("names");
        let mut store = store_in(&tmp);
        named("Named Inside")
            .save(&tmp.join("whatever.toml"))
            .expect("write");
        store.rescan();

        let entry = store.find("Named Inside").expect("listed by its own name");
        assert_eq!(entry.source, PresetSource::User);
        assert!(entry.path.ends_with("whatever.toml"));
        assert!(store.find("whatever").is_none());
    }

    #[test]
    fn voice_presets_are_listed_by_name_ignoring_case() {
        // No numbered files on this side, so the whole list is the alphabetical rest.
        let tmp = tempdir("ordering");
        let mut store = store_in(&tmp);
        named("aardvark")
            .save(&tmp.join("aardvark.toml"))
            .expect("write");
        named("Zebra").save(&tmp.join("Zebra.toml")).expect("write");
        store.rescan();

        let names: Vec<String> = store
            .entries()
            .iter()
            .map(|e| e.name.to_lowercase())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert_eq!(names.first().map(String::as_str), Some("aardvark"));
        assert_eq!(names.last().map(String::as_str), Some("zebra"));
    }

    #[test]
    fn a_user_preset_shadows_a_factory_one_of_the_same_name() {
        let tmp = tempdir("shadow");
        let mut store = store_in(&tmp);
        let (mut clean, _) = store.load("Clean Voice").expect("load");
        assert_eq!(clean.makeup_db, 6.0, "the factory numbers");
        clean.makeup_db = 2.0;
        store.save_as(&clean, "Clean Voice").expect("save");

        let entry = store.find("Clean Voice").expect("still listed");
        assert_eq!(entry.source, PresetSource::User);
        assert_eq!(
            store
                .entries()
                .iter()
                .filter(|e| e.name == "Clean Voice")
                .count(),
            1
        );
        let (reloaded, _) = store.load("Clean Voice").expect("reload");
        assert_eq!(reloaded.makeup_db, 2.0);

        // Deleting the user's copy uncovers the factory one again.
        store.delete("Clean Voice").expect("delete");
        let entry = store.find("Clean Voice").expect("the factory copy is back");
        assert_eq!(entry.source, PresetSource::Factory);
        let (restored, _) = store.load("Clean Voice").expect("load factory");
        assert_eq!(restored.makeup_db, 6.0);
    }

    #[test]
    fn autosave_marks_a_preset_modified_and_is_preferred_on_load() {
        let tmp = tempdir("autosave");
        let mut store = store_in(&tmp);

        let (mut flat, was_autosaved) = store.load("Flat").expect("load");
        assert!(!was_autosaved);
        assert!(!store.find("Flat").unwrap().modified);

        flat.gate = Some(Gate {
            threshold_db: -50.0,
            ..InputPreset::default().gate.expect("Clean Voice has a gate")
        });
        store.autosave(&flat).expect("autosave");
        assert!(
            store
                .autosave_path("Flat")
                .starts_with(tmp.join("AutoSave")),
            "{}",
            store.autosave_path("Flat").display()
        );
        store.rescan();
        assert!(store.find("Flat").unwrap().modified);

        let (loaded, from_autosave) = store.load("Flat").expect("load autosaved");
        assert!(from_autosave);
        assert_eq!(
            loaded.name, "Flat",
            "the canonical name, not the autosave's"
        );
        assert_eq!(loaded.gate.map(|g| g.threshold_db), Some(-50.0));

        store.clear_autosave("Flat");
        assert!(!store.find("Flat").unwrap().modified);
        let (loaded, from_autosave) = store.load("Flat").expect("load after clear");
        assert!(!from_autosave);
        assert!(loaded.gate.is_none(), "Flat has no gate");
    }

    #[test]
    fn factory_presets_cannot_be_deleted() {
        let tmp = tempdir("delete");
        let mut store = store_in(&tmp);
        let err = store.delete("Flat").unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err}");
        assert!(store.find("Flat").is_some());
        assert!(
            assets().join("Flat.toml").is_file(),
            "and the file is untouched"
        );
    }

    #[test]
    fn an_unknown_name_is_reported_as_unknown() {
        let tmp = tempdir("unknown");
        let store = store_in(&tmp);
        let err = store.load("No Such Voice").unwrap_err();
        assert!(
            matches!(&err, Error::Unknown(name) if name == "No Such Voice"),
            "{err}"
        );
        let err = store.export("No Such Voice", &tmp).unwrap_err();
        assert!(matches!(err, Error::Unknown(_)), "{err}");
    }

    #[test]
    fn import_round_trips_through_the_user_directory() {
        let tmp = tempdir("import");
        let mut store = store_in(&tmp);
        let source_dir = tmp.join("elsewhere");
        let source = source_dir.join("My Import.toml");
        let mut preset = named("Called Something Else Inside");
        preset.denoise = Some(Denoise {
            level: DenoiseLevel::Strong,
            ..Denoise::default()
        });
        preset.save(&source).expect("write source");

        let name = store.import(&source).expect("import");
        assert_eq!(
            name, "My Import",
            "a file someone chose is known by its stem"
        );
        let entry = store.find("My Import").expect("listed");
        assert_eq!(entry.source, PresetSource::User);
        assert!(entry.path.starts_with(&tmp));
        assert!(!entry.path.starts_with(&source_dir));

        let exported_dir = tmp.join("export");
        std::fs::create_dir_all(&exported_dir).expect("mkdir");
        let path = store.export("My Import", &exported_dir).expect("export");
        assert!(path.is_file());
        let (original, _) = store.load("My Import").expect("load");
        assert_eq!(InputPreset::load(&path).expect("parse export"), original);
        assert_eq!(original.denoise_level(), DenoiseLevel::Strong);
    }

    #[test]
    fn overwriting_a_user_preset_keeps_the_previous_version_without_listing_it() {
        let tmp = tempdir("overwrite-backup");
        let mut store = InputPresetStore::with_dirs(Vec::new(), tmp.join("user"));

        let mut preset = named("Mine");
        preset.makeup_db = 1.0;
        store.save_as(&preset, "Mine").expect("first save");
        preset.makeup_db = 2.0;
        let path = store.save_as(&preset, "Mine").expect("overwrite");

        let mut backup = path.as_os_str().to_owned();
        backup.push(".bak");
        let kept = InputPreset::load(Path::new(&backup)).expect("the backup is valid TOML");
        assert_eq!(kept.makeup_db, 1.0, "the backup holds the old value");
        assert_eq!(InputPreset::load(&path).expect("load").makeup_db, 2.0);

        let names: Vec<_> = store.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["Mine".to_owned()], "listed: {names:?}");
    }

    #[test]
    fn a_name_becomes_a_filename_through_the_one_sanitiser() {
        let tmp = tempdir("sanitise");
        let mut store = InputPresetStore::with_dirs(Vec::new(), tmp.join("user"));
        let path = store
            .save_as(&named("Calibrated: USB Mic?"), "Calibrated: USB Mic?")
            .expect("save");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("Calibrated USB Mic.toml")
        );
        assert_eq!(store.entries()[0].name, "Calibrated: USB Mic?");

        let err = store.save_as(&named("?"), "?").unwrap_err();
        assert!(matches!(err, Error::MissingName), "{err}");
    }

    #[test]
    fn a_name_that_would_take_another_presets_file_is_refused() {
        // The `.fac` rule on this side: `Clean: Voice` sanitises to the shipped Clean Voice's
        // stem, so the two would share `AutoSave/Clean Voice.toml` and every export.
        let tmp = tempdir("shared-file");
        let mut store = store_in(&tmp);
        let err = store
            .save_as(&named("Clean: Voice"), "Clean: Voice")
            .unwrap_err();
        let Error::SharedFile {
            name,
            existing,
            file,
        } = &err
        else {
            panic!("expected SharedFile, got {err}");
        };
        assert_eq!(name, "Clean: Voice");
        assert_eq!(existing, "Clean Voice");
        assert_eq!(file, "Clean Voice.toml");
        assert!(store.find("Clean: Voice").is_none());
        assert!(
            !tmp.join("Clean Voice.toml").exists(),
            "nothing was written"
        );

        // Two user presets, one file: the second is refused and the first is untouched.
        let mut mine = named("Mine");
        mine.makeup_db = 1.0;
        let path = store.save_as(&mine, "Mine").expect("save");
        let err = store.save_as(&mine, "Mi|ne").unwrap_err();
        assert!(
            matches!(&err, Error::SharedFile { existing, .. } if existing == "Mine"),
            "{err}"
        );
        assert!(store.find("Mi|ne").is_none());
        assert_eq!(InputPreset::load(&path).expect("intact").makeup_db, 1.0);
        let mut backup = path.as_os_str().to_owned();
        backup.push(".bak");
        assert!(!Path::new(&backup).exists());

        // Under its own name it is the overwrite, `.bak` and all.
        mine.makeup_db = 2.0;
        store.save_as(&mine, "Mine").expect("overwrite");
        assert!(Path::new(&backup).is_file());
        assert_eq!(InputPreset::load(&path).expect("reload").makeup_db, 2.0);
    }

    #[test]
    fn a_user_file_that_will_not_parse_does_not_hide_the_rest() {
        // The 0.3.0 loader gave up on the whole directory at the first bad file. With a directory
        // the user writes into, one typo would have emptied the microphone's preset list.
        let tmp = tempdir("tolerant");
        let mut store = store_in(&tmp);
        named("Good One")
            .save(&tmp.join("good.toml"))
            .expect("write");
        std::fs::write(
            tmp.join("bad.toml"),
            "name = \"Bad\"\nhighpass_hz = \"no\"\n",
        )
        .expect("write");
        store.rescan();

        assert!(store.find("Good One").is_some());
        assert!(store.find("Bad").is_none());
        assert!(
            store.find("Clean Voice").is_some(),
            "the factory set is still there"
        );
    }

    #[test]
    fn the_default_directories_keep_voice_presets_apart_from_the_fac_set() {
        let store = InputPresetStore::with_default_dirs();
        assert!(
            store.user_dir().ends_with("fxsound/presets/Input"),
            "{}",
            store.user_dir().display()
        );
        assert!(
            store.autosave_path("X").ends_with("Input/AutoSave/X.toml"),
            "{}",
            store.autosave_path("X").display()
        );
    }
}
