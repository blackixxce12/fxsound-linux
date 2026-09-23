//! Per-application presets: who an application is, and which preset the user chose for it.
//!
//! One chain cannot run two presets at once, so an application with a preset of its own is moved
//! onto a *route* — an extra pair of FxSound nodes running that preset — attached to the same
//! real device as its lane (`docs/0.4.0-apps.md`). This module holds the two halves of that
//! feature that are plain data: [`AppKey`], how an application is recognised from its stream's
//! properties from one run of it to the next, and [`AppRules`], the local store of what the user
//! chose, `~/.config/fxsound/apps.toml`.
//!
//! The store is its own file rather than a table in `settings.toml` because it grows with every
//! application that ever played a sound, and a settings file that the window rewrites on every
//! slider release is the wrong place for a list of five hundred entries. It is written the same
//! way (`crate::atomic`), and a file that does not load — does not parse, is not UTF-8, may not
//! be read — is moved aside to `apps.toml.bad`, so a hand edit gone wrong is never silently
//! replaced.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DeviceDirection;

/// How many routes one lane runs at a time: distinct presets besides the lane's own.
///
/// Every route is a pair of nodes and a chain of its own on the audio thread, so each one costs
/// what the lane itself costs. Four covers the case the feature was asked for — a game, a
/// browser and a voice chat, each on its own preset, all at once — with one to spare. An
/// application whose route cannot be created stays on its lane, and the window says why.
pub const MAX_ROUTES_PER_LANE: usize = 4;

/// How many applications the store remembers before it forgets the one seen longest ago.
///
/// Every program that ever played a sound is remembered, so the Applications list can offer a
/// preset for a game that is not running. That list is unbounded on a machine that lives for
/// years; five hundred is far past what anyone scrolls through and still a file read in well
/// under a millisecond.
pub const MAX_REMEMBERED_APPS: usize = 500;

/// Who an application is, from its stream's PipeWire properties.
///
/// Three identifiers, each empty when the stream does not carry it:
///
/// - `binary`: `application.process.binary`. Wine and Proton set it to the `.exe` name, so a
///   game is recognised as itself and not as the Wine loader.
/// - `name`: `application.name`, what the application calls itself, and what the window shows.
/// - `flatpak`: `pipewire.access.portal.app_id`, the Flatpak application id, which only the
///   sandbox can set and so is the one identifier another program cannot borrow.
///
/// Two keys name the same application when [`AppKey::matches`] says so, which is looser than
/// `==`: a rule written from the command line with only a name still matches the running program
/// that reports a binary too. `Eq` and `Hash` compare the recorded strings exactly, for keying a
/// map of streams the engine reported; lookups in the store go through `matches`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppKey {
    /// `application.process.binary`: the executable's name.
    pub binary: String,
    /// `application.name`: the name the application gives itself.
    pub name: String,
    /// `pipewire.access.portal.app_id`: the Flatpak application id.
    pub flatpak: String,
}

/// Which identifier decided whether two keys match, weakest first, so that `Ord` ranks strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Decider {
    Name,
    Binary,
    Flatpak,
}

impl AppKey {
    /// Whether this key and `other` name the same application.
    ///
    /// The first identifier *both* keys carry decides, in the order flatpak id, binary, name:
    ///
    /// - The Flatpak id is exact. The sandbox sets it, and application ids are case-sensitive.
    /// - The binary is compared by its basename and without regard to case. Windows file names
    ///   are case-insensitive, and a game started under Proton by two launchers can be
    ///   `BF6.exe` one day and `bf6.exe` the next; a full path, which some runtimes report, is
    ///   the same program as its last component.
    /// - The name is exact. It is what the application reports about itself, spelled the same
    ///   way every run, and loosening it would join programs that only share a word.
    ///
    /// A stronger identifier present on both sides is final: two Flatpaks with different ids are
    /// different applications even when their binaries agree, and two programs with different
    /// binaries are different even when both call themselves `Chromium`. An identifier present
    /// on one side only says nothing, so a key with only a name matches every stream that
    /// reports that name. Surrounding whitespace is ignored everywhere. A key with no
    /// identifier at all matches nothing, not even itself.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.match_strength(other).is_some()
    }

    /// The name the window shows for the application: its `application.name`, else its binary's
    /// basename, else its Flatpak id. Empty only for a key with no identifier.
    #[must_use]
    pub fn display(&self) -> &str {
        let name = self.name.trim();
        if !name.is_empty() {
            return name;
        }
        let binary = self.binary_basename();
        if !binary.is_empty() {
            return binary;
        }
        self.flatpak.trim()
    }

    /// Whether the key carries no identifier, and so can name no application.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.flatpak.trim().is_empty()
            && self.binary_basename().is_empty()
            && self.name.trim().is_empty()
    }

    /// The binary's last path component, trimmed. `/usr/lib/firefox/firefox` and
    /// `C:\Games\bf6.exe` report `firefox` and `bf6.exe`.
    fn binary_basename(&self) -> &str {
        self.binary
            .trim()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .trim()
    }

    /// The strongest identifier the key carries: the most a match with it can be decided by.
    fn strongest(&self) -> Option<Decider> {
        if !self.flatpak.trim().is_empty() {
            Some(Decider::Flatpak)
        } else if !self.binary_basename().is_empty() {
            Some(Decider::Binary)
        } else if !self.name.trim().is_empty() {
            Some(Decider::Name)
        } else {
            None
        }
    }

    /// Which of `rules` — keys a rule was written for — names this application most
    /// specifically, by its index among them; `None` when none matches it.
    ///
    /// The order [`AppRules::rule`] ranks rules in: one matched by the Flatpak id wins over one
    /// matched by the binary, which wins over one matched by the name; between two matched as
    /// strongly, the one with exactly this key's identifiers wins, and then the one listed first.
    /// Public so that everything that picks a rule for a stream picks the same one — the engine
    /// choosing which route an application's stream goes onto (`docs/0.4.0-apps.md`) as much as
    /// the store answering for it.
    #[must_use]
    pub fn best_match<'a>(&self, rules: impl IntoIterator<Item = &'a Self>) -> Option<usize> {
        let mut best: Option<(usize, Decider, bool)> = None;
        for (index, rule) in rules.into_iter().enumerate() {
            let Some(strength) = rule.match_strength(self) else {
                continue;
            };
            let exact = rule.same_identity(self);
            let better = match best {
                None => true,
                Some((_, best_strength, best_exact)) => {
                    (strength, exact) > (best_strength, best_exact)
                }
            };
            if better {
                best = Some((index, strength, exact));
            }
        }
        best.map(|(index, ..)| index)
    }

    /// Whether, as a rule, this key could be the one [`AppKey::best_match`] picks for an
    /// application the rule `other` matches too: whether some application both keys match is
    /// matched by this one at least as strongly.
    ///
    /// The app sends the engine the rules that name a preset and, beside them, only the rules
    /// that follow the lane for which this holds against one of them (`docs/0.4.0-apps.md`): a
    /// rule that can never outrank a rule naming a preset decides no stream's route, and the
    /// engine then picks among what it is given exactly as the store picks among all its rules.
    /// The answer is exact, not a guess: an application can match both keys only through the
    /// identifiers they carry, and any other value in one of its fields only makes it match
    /// neither, so every application worth asking about is made of those identifiers.
    #[must_use]
    pub fn may_outrank(&self, other: &Self) -> bool {
        let (mine, theirs) = (self.parts(), other.parts());
        let choices = |field: usize| ["", mine[field], theirs[field]];
        choices(0).into_iter().any(|flatpak| {
            choices(1).into_iter().any(|binary| {
                choices(2).into_iter().any(|name| {
                    let application = [flatpak, binary, name];
                    match (strength(mine, application), strength(theirs, application)) {
                        (Some(this), Some(that)) => this >= that,
                        _ => false,
                    }
                })
            })
        })
    }

    /// The identifier that decided a match between the two keys, or `None` when they do not
    /// match. A stronger decider is a more specific rule: see [`AppRules::rule`].
    fn match_strength(&self, other: &Self) -> Option<Decider> {
        strength(self.parts(), other.parts())
    }

    /// The Flatpak id, the binary and the name, as matching reads them: trimmed, and the binary
    /// by its basename.
    fn parts(&self) -> [&str; 3] {
        [
            self.flatpak.trim(),
            self.binary_basename(),
            self.name.trim(),
        ]
    }

    /// Whether the two keys carry the same identifiers, as matching reads them: the same Flatpak
    /// id, the same binary in any case, the same name. Two such rules in the store cannot be told
    /// apart by any lookup, so the store keeps one of them.
    fn same_identity(&self, other: &Self) -> bool {
        self.flatpak.trim() == other.flatpak.trim()
            && eq_ignoring_case(self.binary_basename(), other.binary_basename())
            && self.name.trim() == other.name.trim()
    }

    /// [`AppKey::same_identity`] as a value, for finding duplicates among many rules at once.
    fn identity(&self) -> (String, String, String) {
        (
            self.flatpak.trim().to_owned(),
            self.binary_basename()
                .chars()
                .flat_map(char::to_lowercase)
                .collect(),
            self.name.trim().to_owned(),
        )
    }
}

/// [`AppKey::match_strength`] over two keys' [`AppKey::parts`]: the first identifier both carry
/// decides, in the order Flatpak id, binary, name.
fn strength(a: [&str; 3], b: [&str; 3]) -> Option<Decider> {
    let [flatpak_a, binary_a, name_a] = a;
    let [flatpak_b, binary_b, name_b] = b;
    if !flatpak_a.is_empty() && !flatpak_b.is_empty() {
        return (flatpak_a == flatpak_b).then_some(Decider::Flatpak);
    }
    if !binary_a.is_empty() && !binary_b.is_empty() {
        return eq_ignoring_case(binary_a, binary_b).then_some(Decider::Binary);
    }
    if !name_a.is_empty() && !name_b.is_empty() {
        return (name_a == name_b).then_some(Decider::Name);
    }
    None
}

/// Case-insensitive comparison without allocating: `to_lowercase` on both sides, one character
/// at a time, which also gets the non-ASCII executables of localised Windows games right.
fn eq_ignoring_case(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// What the user chose for one application: a preset per direction, or "follow FxSound's".
///
/// Written as one `[[app]]` table of `apps.toml`, the key's three fields beside the rest:
///
/// ```toml
/// [[app]]
/// binary = "bf6.exe"
/// name = "Battlefield 6"
/// flatpak = ""
/// output_preset = "Gaming"
/// input_preset = ""
/// last_seen = 1790000000
/// ```
///
/// Every field defaults, so an entry a hand edit left short costs that field, not the file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppRule {
    /// Which application the rule is for.
    #[serde(flatten)]
    pub key: AppKey,
    /// The preset the application's playback runs through; empty to follow the output lane's.
    pub output_preset: String,
    /// The preset the application's recording runs through; empty to follow the input lane's.
    pub input_preset: String,
    /// When a stream of the application was last seen, or the rule last changed, in seconds
    /// since the Unix epoch. Which remembered application is forgotten first.
    pub last_seen: u64,
}

impl AppRule {
    /// The preset chosen for `direction`, empty for "follow the lane's preset".
    #[must_use]
    pub fn preset(&self, direction: DeviceDirection) -> &str {
        match direction {
            DeviceDirection::Output => &self.output_preset,
            DeviceDirection::Input => &self.input_preset,
        }
    }

    /// Choose `preset` for `direction`; empty to follow the lane's preset.
    pub fn set_preset(&mut self, direction: DeviceDirection, preset: &str) {
        let slot = match direction {
            DeviceDirection::Output => &mut self.output_preset,
            DeviceDirection::Input => &mut self.input_preset,
        };
        preset.clone_into(slot);
    }

    /// Whether the rule gives `direction` a preset of its own rather than following the lane.
    #[must_use]
    pub fn has_preset(&self, direction: DeviceDirection) -> bool {
        !self.preset(direction).trim().is_empty()
    }
}

/// What a rule says for one application and direction, measured against the presets that exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPreset<'a> {
    /// No rule, or a rule that follows the lane: the application stays on its lane's chain.
    Follow,
    /// A route running this preset.
    Preset(&'a str),
    /// The rule names a preset that no longer exists. The application follows the lane, and
    /// the window says so once, with the name, rather than letting a rule fail quietly.
    Missing(&'a str),
}

/// The local store of per-application choices, `apps.toml`.
///
/// Every lookup and change goes to the rule that matches the key most specifically
/// ([`AppRules::rule`]). A change goes there only when that rule matched by the strongest
/// identifier the key carries, though: a rule that matched by something weaker is a more general
/// rule — one for every program called `Discord`, written from the command line — and choosing a
/// preset for one of those programs adds a rule of its own rather than narrowing the general one
/// to it ([`AppRules::upsert`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppRules {
    /// One entry per remembered application, in the order they were first remembered.
    #[serde(rename = "app")]
    pub apps: Vec<AppRule>,
}

impl AppRules {
    /// `~/.config/fxsound/apps.toml` (or `$XDG_CONFIG_HOME/fxsound/apps.toml`), beside
    /// `settings.toml`.
    #[must_use]
    pub fn config_path() -> PathBuf {
        crate::Settings::config_dir().join("apps.toml")
    }

    /// Load the store from [`AppRules::config_path`]; see [`AppRules::load_from`].
    #[must_use]
    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    /// Load the store from `path`, empty when there is none.
    ///
    /// A file that is there but does not load — it does not parse, it is not UTF-8 (a hand edit
    /// saved in Latin-1), or this user may not read it — is **moved aside**, to `apps.toml.bad`,
    /// and the store starts empty. That is what the settings file does with one that does not
    /// parse, and here it covers every file that fails to load, because the store is saved far
    /// more often than the settings: the first application that plays a sound after this load
    /// is remembered, and that save (`crate::atomic`) renames a fresh file over this path. A file
    /// left in place would be replaced by an empty list — every choice the user made gone, with
    /// only a log line to say so. Moved aside, the next save writes beside it and the file is
    /// there to be read back.
    ///
    /// Two cases are left where they are. A missing file is simply an empty store. A directory
    /// in the file's place holds nothing a save can destroy: a rename cannot replace a directory,
    /// so every save fails, loudly, until someone removes it — and moving someone's directory
    /// out of the way is not this loader's business. What parses is sanitised
    /// ([`AppRules::sanitise`]).
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) if err.kind() == std::io::ErrorKind::IsADirectory => {
                log::warn!("{}: {err}; no per-application presets", path.display());
                return Self::default();
            }
            Err(err) => {
                Self::move_aside(path, &err);
                return Self::default();
            }
        };
        match toml::from_str::<Self>(&text) {
            Ok(mut rules) => {
                rules.sanitise();
                rules
            }
            Err(err) => {
                Self::move_aside(path, &err);
                Self::default()
            }
        }
    }

    /// Rename a store that did not load to [`AppRules::bad_path`], saying why in the log.
    ///
    /// When the rename itself fails the file stays where it is, and the log says so. What stops
    /// this rename — a directory this user may not write, a read-only filesystem — stops the next
    /// save's rename as well, so the file is still not replaced.
    fn move_aside(path: &Path, why: &dyn std::fmt::Display) {
        let aside = Self::bad_path(path);
        match std::fs::rename(path, &aside) {
            Ok(()) => log::warn!(
                "{}: {why}; moved aside as {} and starting with no per-application presets",
                path.display(),
                aside.display()
            ),
            Err(rename_err) => log::warn!(
                "{}: {why}; could not move it aside ({rename_err}); no per-application presets",
                path.display()
            ),
        }
    }

    /// Where [`AppRules::load_from`] moves a file it could not load: `apps.toml.bad` beside
    /// `apps.toml`.
    #[must_use]
    pub fn bad_path(path: &Path) -> PathBuf {
        let mut name = path
            .file_name()
            .map_or_else(|| std::ffi::OsString::from("apps.toml"), ToOwned::to_owned);
        name.push(".bad");
        path.with_file_name(name)
    }

    /// Write the store to [`AppRules::config_path`]; see [`AppRules::save_to`].
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::config_path())
    }

    /// Write the store to `path` by durable replace (`crate::atomic`), creating the directory if
    /// needed: an interrupted save leaves the previous file whole. What is written is sanitised
    /// first, so the file never holds an entry the loader would have to drop.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let mut checked = self.clone();
        checked.sanitise();
        let text = toml::to_string_pretty(&checked)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        crate::atomic::write(path, text.as_bytes())
    }

    /// Drop what no lookup could ever answer with, and keep to [`MAX_REMEMBERED_APPS`].
    ///
    /// - An entry with no identifier matches no application, so it goes.
    /// - A second entry with the same identifiers as an earlier one can only be a hand edit, and
    ///   every lookup answers with the first; the first is kept, and the shadow cannot outlive
    ///   the next save.
    /// - A `last_seen` past what a TOML integer holds is pulled back to the largest one, so the
    ///   save that follows cannot fail on it.
    /// - Past the cap, the applications seen longest ago are forgotten.
    ///
    /// The order of what remains is the file's order.
    pub fn sanitise(&mut self) {
        let before = self.apps.len();
        let mut seen = HashSet::with_capacity(before);
        self.apps
            .retain(|rule| !rule.key.is_empty() && seen.insert(rule.key.identity()));
        let dropped = before - self.apps.len();
        if dropped > 0 {
            log::info!(
                "apps.toml: dropped {dropped} entries that named no application or repeated one"
            );
        }
        for rule in &mut self.apps {
            rule.last_seen = rule.last_seen.min(TOML_INTEGER_MAX);
        }
        self.enforce_cap(None);
    }

    /// The rule for the application `key` names, if any: the one matching it most specifically.
    ///
    /// A rule matched by the Flatpak id wins over one matched by the binary, which wins over one
    /// matched by the name. Between rules matched as strongly, the one with exactly the key's
    /// identifiers wins, and then the one listed first.
    #[must_use]
    pub fn rule(&self, key: &AppKey) -> Option<&AppRule> {
        self.best_match(key).map(|index| &self.apps[index])
    }

    /// The preset the application `key` names runs through in `direction`, or `None` when it
    /// follows the lane's: no rule matches it, or the rule leaves that direction empty.
    #[must_use]
    pub fn preset_for(&self, key: &AppKey, direction: DeviceDirection) -> Option<&str> {
        // Only a blank preset is read as "follow"; a name is handed on as written, since it is
        // the preset store's name and not this file's to tidy.
        let preset = self.rule(key)?.preset(direction);
        (!preset.trim().is_empty()).then_some(preset)
    }

    /// [`AppRules::preset_for`], measured against the presets that exist: `exists` says whether
    /// a preset of that name does, in the store for `direction`.
    #[must_use]
    pub fn resolve(
        &self,
        key: &AppKey,
        direction: DeviceDirection,
        exists: impl Fn(&str) -> bool,
    ) -> AppPreset<'_> {
        match self.preset_for(key, direction) {
            None => AppPreset::Follow,
            Some(preset) if exists(preset) => AppPreset::Preset(preset),
            Some(preset) => AppPreset::Missing(preset),
        }
    }

    /// Choose `preset` for the application `key` names in `direction` — empty to follow the
    /// lane's — and mark it seen at `now`. Returns whether the store changed.
    ///
    /// The rule changed is the one [`AppRules::rule`] answers with, when it matched by the
    /// strongest identifier the key carries — the Flatpak id when the key has one, else the
    /// binary, else the name. That is the application's own rule, even when it was written under
    /// another name or another case of its binary, so a program that renamed itself keeps one
    /// row. A rule that matched only by something weaker is a general one, and is left alone: a
    /// new rule is added for the key, carrying what the general rule said until now, so choosing
    /// a microphone preset for one Discord neither narrows a rule for every `Discord` to it nor
    /// undoes the output preset that rule gave it. A key with no identifier is refused: it would
    /// add a rule no application could match.
    pub fn upsert(
        &mut self,
        key: &AppKey,
        direction: DeviceDirection,
        preset: &str,
        now: u64,
    ) -> bool {
        if key.is_empty() {
            return false;
        }
        let now = now.min(TOML_INTEGER_MAX);
        let best = self.best_match(key);
        if let Some(index) = best
            && self.apps[index].key.match_strength(key) == key.strongest()
        {
            let rule = &mut self.apps[index];
            let changed = rule.preset(direction) != preset || rule.last_seen < now;
            rule.set_preset(direction, preset);
            rule.last_seen = rule.last_seen.max(now);
            return changed;
        }
        let mut rule = best
            .map(|index| self.apps[index].clone())
            .unwrap_or_default();
        key.clone_into(&mut rule.key);
        rule.set_preset(direction, preset);
        rule.last_seen = now;
        self.apps.push(rule);
        self.enforce_cap(Some(self.apps.len() - 1));
        true
    }

    /// Remember that a stream of the application `key` names was seen at `now`. Returns whether
    /// the store changed.
    ///
    /// An application a rule already covers refreshes that rule's `last_seen`, and nothing else:
    /// a new entry of its own would match it more specifically than a general rule does, and
    /// would silently take the general rule's presets away from it. Anything else is added,
    /// following both lanes, so the Applications list can offer it a preset after it has quit.
    pub fn seen(&mut self, key: &AppKey, now: u64) -> bool {
        if key.is_empty() {
            return false;
        }
        let now = now.min(TOML_INTEGER_MAX);
        if let Some(index) = self.best_match(key) {
            let rule = &mut self.apps[index];
            if rule.last_seen >= now {
                return false;
            }
            rule.last_seen = now;
            return true;
        }
        self.apps.push(AppRule {
            key: key.clone(),
            last_seen: now,
            ..AppRule::default()
        });
        self.enforce_cap(Some(self.apps.len() - 1));
        true
    }

    /// Forget the rule [`AppRules::rule`] answers with for `key`. Returns whether anything was
    /// forgotten.
    ///
    /// One rule at a time: the ✕ on a row forgets that row, since a rule is always its own key's
    /// best match. Forgetting a specific rule can leave a general one that still matches the
    /// application, which is then what it follows.
    pub fn forget(&mut self, key: &AppKey) -> bool {
        match self.best_match(key) {
            Some(index) => {
                self.apps.remove(index);
                true
            }
            None => false,
        }
    }

    /// A preset of `direction` was renamed from `from` to `to`: every rule that chose it follows
    /// the new name. Returns how many rules changed.
    pub fn rename_preset(&mut self, direction: DeviceDirection, from: &str, to: &str) -> usize {
        let mut changed = 0;
        for rule in &mut self.apps {
            if rule.preset(direction) == from && from != to {
                rule.set_preset(direction, to);
                changed += 1;
            }
        }
        changed
    }

    /// The index of the rule [`AppRules::rule`] answers with.
    fn best_match(&self, key: &AppKey) -> Option<usize> {
        key.best_match(self.apps.iter().map(|rule| &rule.key))
    }

    /// Keep at most [`MAX_REMEMBERED_APPS`] rules, forgetting the ones seen longest ago; between
    /// two seen at the same second, the one listed later goes. `protect` is a rule that stays
    /// whatever its `last_seen` says — the one the user has just set, which a clock that jumped
    /// back must not make the first to go.
    fn enforce_cap(&mut self, protect: Option<usize>) {
        let count = self.apps.len();
        if count <= MAX_REMEMBERED_APPS {
            return;
        }
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by(|&a, &b| {
            (Some(b) == protect)
                .cmp(&(Some(a) == protect))
                .then(self.apps[b].last_seen.cmp(&self.apps[a].last_seen))
                .then(a.cmp(&b))
        });
        let mut keep = vec![false; count];
        for &index in &order[..MAX_REMEMBERED_APPS] {
            keep[index] = true;
        }
        let mut index = 0;
        self.apps.retain(|_| {
            let kept = keep[index];
            index += 1;
            kept
        });
        log::info!(
            "apps.toml: forgot the {} applications seen longest ago",
            count - MAX_REMEMBERED_APPS
        );
    }
}

/// The largest value a TOML integer holds; a `last_seen` past it could not be saved.
const TOML_INTEGER_MAX: u64 = i64::MAX as u64;

/// Now, in seconds since the Unix epoch: what [`AppRule::last_seen`] counts in. A clock set
/// before 1970 reads as zero.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(binary: &str, name: &str, flatpak: &str) -> AppKey {
        AppKey {
            binary: binary.to_owned(),
            name: name.to_owned(),
            flatpak: flatpak.to_owned(),
        }
    }

    fn rule(key: AppKey, output: &str, input: &str, last_seen: u64) -> AppRule {
        AppRule {
            key,
            output_preset: output.to_owned(),
            input_preset: input.to_owned(),
            last_seen,
        }
    }

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;

    // ---- matching ------------------------------------------------------------------------

    #[test]
    fn the_flatpak_id_decides_when_both_keys_carry_one() {
        let a = key("discord", "Discord", "com.discordapp.Discord");
        let b = key("Discord", "Discord", "com.discordapp.DiscordCanary");
        assert!(
            !a.matches(&b),
            "two sandboxes with different ids are two applications, whatever their binaries say"
        );
        let c = key("something-else", "Other name", "com.discordapp.Discord");
        assert!(
            a.matches(&c),
            "the same id is the same application, whatever it calls itself"
        );
    }

    #[test]
    fn the_flatpak_id_is_compared_exactly() {
        let a = key("", "", "org.mozilla.firefox");
        assert!(!a.matches(&key("", "", "org.mozilla.Firefox")));
        assert!(a.matches(&key("", "", " org.mozilla.firefox ")));
    }

    #[test]
    fn the_binary_decides_when_only_one_side_is_a_flatpak() {
        let native = key("firefox", "Firefox", "");
        let sandboxed = key("firefox", "Firefox", "org.mozilla.firefox");
        assert!(native.matches(&sandboxed));
        assert!(sandboxed.matches(&native));
        assert!(!native.matches(&key("chromium", "Firefox", "org.mozilla.firefox")));
    }

    #[test]
    fn the_binary_is_compared_without_regard_to_case() {
        let rule = key("bf6.exe", "Battlefield 6", "");
        assert!(rule.matches(&key("BF6.EXE", "Battlefield 6", "")));
        assert!(rule.matches(&key("Bf6.Exe", "", "")));
        // Beyond ASCII too: a localised Windows game's executable.
        assert!(key("ИГРА.exe", "", "").matches(&key("игра.EXE", "", "")));
    }

    #[test]
    fn the_binary_is_compared_by_its_basename() {
        let rule = key("firefox", "", "");
        assert!(rule.matches(&key("/usr/lib/firefox/firefox", "", "")));
        assert!(key("bf6.exe", "", "").matches(&key("C:\\Games\\BF6\\bf6.exe", "", "")));
        assert!(!rule.matches(&key("/usr/lib/firefox/firefox-bin", "", "")));
    }

    #[test]
    fn different_binaries_are_different_applications_even_with_the_same_name() {
        let chrome = key("chrome", "Chromium", "");
        let brave = key("brave", "Chromium", "");
        assert!(!chrome.matches(&brave));
    }

    #[test]
    fn the_name_decides_when_neither_stronger_identifier_is_on_both_sides() {
        let from_the_command_line = key("", "Discord", "");
        assert!(from_the_command_line.matches(&key(
            "discord",
            "Discord",
            "com.discordapp.Discord"
        )));
        assert!(from_the_command_line.matches(&key("Discord", "Discord", "")));
        assert!(!from_the_command_line.matches(&key("discord", "Discord Canary", "")));
    }

    #[test]
    fn the_name_is_compared_exactly_apart_from_surrounding_whitespace() {
        let rule = key("", "Discord", "");
        assert!(
            !rule.matches(&key("", "discord", "")),
            "a name keeps its case"
        );
        assert!(rule.matches(&key("", "  Discord ", "")));
    }

    #[test]
    fn keys_with_nothing_in_common_do_not_match() {
        assert!(!key("mpv", "", "").matches(&key("", "mpv", "")));
        assert!(!key("", "", "io.mpv.Mpv").matches(&key("mpv", "mpv", "")));
    }

    #[test]
    fn a_key_with_no_identifier_matches_nothing_not_even_itself() {
        let empty = AppKey::default();
        assert!(empty.is_empty());
        assert!(!empty.matches(&empty));
        assert!(!empty.matches(&key("mpv", "mpv", "io.mpv.Mpv")));
        assert!(
            key("  ", " ", "\t").is_empty(),
            "whitespace is not an identifier"
        );
        assert!(
            key("/usr/bin/", "", "").is_empty(),
            "nor is a path with no file name"
        );
    }

    #[test]
    fn matching_is_symmetric() {
        let keys = [
            key("bf6.exe", "Battlefield 6", ""),
            key("BF6.exe", "", ""),
            key("", "Battlefield 6", ""),
            key("firefox", "Firefox", "org.mozilla.firefox"),
            key("firefox", "", ""),
            key("", "", "org.mozilla.firefox"),
            key("chrome", "Chromium", ""),
        ];
        for a in &keys {
            for b in &keys {
                assert_eq!(a.matches(b), b.matches(a), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn the_display_name_is_the_application_name_then_the_binary_then_the_flatpak_id() {
        assert_eq!(
            key("bf6.exe", "Battlefield 6", "").display(),
            "Battlefield 6"
        );
        assert_eq!(key("C:\\Games\\bf6.exe", "", "").display(), "bf6.exe");
        assert_eq!(
            key("", " ", "org.mozilla.firefox").display(),
            "org.mozilla.firefox"
        );
        assert_eq!(AppKey::default().display(), "");
    }

    #[test]
    fn equality_and_hashing_compare_the_recorded_strings_exactly() {
        use std::collections::HashSet;
        let a = key("bf6.exe", "Battlefield 6", "");
        let b = key("BF6.exe", "Battlefield 6", "");
        assert!(a.matches(&b));
        assert_ne!(a, b, "matching is looser than equality");
        let set: HashSet<AppKey> = [a.clone(), b, a].into_iter().collect();
        assert_eq!(set.len(), 2);
    }

    // ---- serde -----------------------------------------------------------------------------

    #[test]
    fn a_key_round_trips_through_toml_under_the_property_names_it_came_from() {
        let original = key("bf6.exe", "Battlefield 6", "");
        let text = toml::to_string(&original).expect("serialise");
        for line in [
            "binary = \"bf6.exe\"",
            "name = \"Battlefield 6\"",
            "flatpak = \"\"",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
        let back: AppKey = toml::from_str(&text).expect("parse");
        assert_eq!(back, original);
    }

    #[test]
    fn a_short_key_fills_in_its_missing_fields() {
        let parsed: AppKey = toml::from_str("name = \"Discord\"\n").expect("parse");
        assert_eq!(parsed, key("", "Discord", ""));
    }

    #[test]
    fn a_store_is_written_as_the_contract_spells_it() {
        let rules = AppRules {
            apps: vec![rule(
                key("bf6.exe", "Battlefield 6", ""),
                "Gaming",
                "",
                1_790_000_000,
            )],
        };
        let text = toml::to_string_pretty(&rules).expect("serialise");
        let expected = [
            "[[app]]",
            "binary = \"bf6.exe\"",
            "name = \"Battlefield 6\"",
            "flatpak = \"\"",
            "output_preset = \"Gaming\"",
            "input_preset = \"\"",
            "last_seen = 1790000000",
        ];
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(lines, expected, "in this order, in:\n{text}");
        let back: AppRules = toml::from_str(&text).expect("parse");
        assert_eq!(back, rules);
    }

    #[test]
    fn the_contracts_example_file_reads_back() {
        let text = "\
[[app]]
binary = \"bf6.exe\"
name = \"Battlefield 6\"
flatpak = \"\"
output_preset = \"Gaming\"      # absent = follow FxSound's output preset
input_preset = \"\"             # absent/empty = follow
last_seen = 1790000000
";
        let parsed: AppRules = toml::from_str(text).expect("parse");
        assert_eq!(
            parsed.apps,
            vec![rule(
                key("bf6.exe", "Battlefield 6", ""),
                "Gaming",
                "",
                1_790_000_000
            )]
        );
    }

    #[test]
    fn an_entry_short_of_fields_and_carrying_unknown_ones_still_reads() {
        let text = "\
[[app]]
name = \"Discord\"
input_preset = \"Headset\"
colour = \"blue\"

[[app]]
flatpak = \"com.brave.Browser\"
output_preset = \"Volume Boost\"
";
        let parsed: AppRules = toml::from_str(text).expect("parse");
        assert_eq!(
            parsed.apps,
            vec![
                rule(key("", "Discord", ""), "", "Headset", 0),
                rule(key("", "", "com.brave.Browser"), "Volume Boost", "", 0),
            ]
        );
    }

    #[test]
    fn an_empty_store_round_trips() {
        let text = toml::to_string_pretty(&AppRules::default()).expect("serialise");
        let back: AppRules = toml::from_str(&text).expect("parse");
        assert_eq!(back, AppRules::default());
        let from_nothing: AppRules = toml::from_str("").expect("parse an empty file");
        assert_eq!(from_nothing, AppRules::default());
    }

    // ---- the file ------------------------------------------------------------------------

    #[test]
    fn the_store_lives_beside_the_settings_file() {
        assert_eq!(
            AppRules::config_path(),
            crate::Settings::config_dir().join("apps.toml")
        );
        assert_eq!(
            AppRules::config_path().parent(),
            crate::Settings::config_path().parent()
        );
    }

    #[test]
    fn a_saved_store_loads_back_the_same() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("apps.toml");
        let rules = AppRules {
            apps: vec![
                rule(
                    key("bf6.exe", "Battlefield 6", ""),
                    "Gaming",
                    "",
                    1_790_000_000,
                ),
                rule(key("brave", "Brave", ""), "Volume Boost", "", 1_790_000_100),
                rule(
                    key("discord", "Discord", "com.discordapp.Discord"),
                    "",
                    "Headset",
                    1_790_000_200,
                ),
            ],
        };
        rules.save_to(&path).expect("save");
        assert_eq!(AppRules::load_from(&path), rules);
        let left: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("list")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left,
            vec!["apps.toml".to_owned()],
            "no temporary file left behind"
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_store_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        assert_eq!(AppRules::load_from(&path), AppRules::default());
        assert!(!path.exists());
        assert!(!AppRules::bad_path(&path).exists());
    }

    #[test]
    fn a_file_that_does_not_parse_is_moved_aside_rather_than_silently_lost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        let broken = "[[app]]\nname = \"Discord\"\nlast_seen = \"yesterday\"\n";
        std::fs::write(&path, broken).expect("write");

        assert_eq!(AppRules::load_from(&path), AppRules::default());
        let aside = dir.path().join("apps.toml.bad");
        assert_eq!(AppRules::bad_path(&path), aside);
        assert_eq!(
            std::fs::read_to_string(&aside).expect("the broken file was moved aside"),
            broken,
            "byte for byte, so the hand edit can be read back"
        );
        assert!(!path.exists(), "nothing is left to be overwritten in place");

        // The next save writes a fresh file beside it and leaves the evidence alone.
        let mut rules = AppRules::default();
        rules.upsert(&key("", "Discord", ""), IN, "Headset", 1);
        rules.save_to(&path).expect("save");
        assert_eq!(AppRules::load_from(&path), rules);
        assert_eq!(
            std::fs::read_to_string(&aside).expect("still there"),
            broken
        );
    }

    #[test]
    fn a_file_that_is_not_a_list_of_apps_is_moved_aside_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        std::fs::write(&path, "app = \"Discord\"\n").expect("write");
        assert_eq!(AppRules::load_from(&path), AppRules::default());
        assert!(AppRules::bad_path(&path).exists());
    }

    #[test]
    fn a_directory_in_the_files_place_is_left_where_it_is() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        std::fs::create_dir(&path).expect("create");
        std::fs::write(path.join("notes"), "mine").expect("write");
        assert_eq!(AppRules::load_from(&path), AppRules::default());
        assert!(path.is_dir());
        assert!(!AppRules::bad_path(&path).exists());

        // And no save can destroy it: a rename cannot replace a directory.
        let mut rules = AppRules::default();
        rules.upsert(&key("", "Discord", ""), IN, "Headset", 1);
        assert!(rules.save_to(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("notes")).expect("still there"),
            "mine"
        );
    }

    #[test]
    fn a_file_that_is_not_utf8_is_moved_aside_rather_than_silently_lost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        // A hand edit saved in Latin-1: `ÿ` is one byte there and never valid UTF-8.
        let latin1: &[u8] = b"[[app]]\nname = \"\xff\"\noutput_preset = \"Rock\"\n";
        std::fs::write(&path, latin1).expect("write");

        assert_eq!(AppRules::load_from(&path), AppRules::default());
        let aside = AppRules::bad_path(&path);
        assert_eq!(
            std::fs::read(&aside).expect("the file was moved aside"),
            latin1,
            "byte for byte, so the hand edit can be read back"
        );
        assert!(!path.exists(), "nothing is left to be overwritten in place");

        // The save that remembering the next application makes cannot touch it now.
        let mut rules = AppRules::default();
        rules.seen(&key("firefox", "Firefox", ""), 2);
        rules.save_to(&path).expect("save");
        assert_eq!(AppRules::load_from(&path), rules);
        assert_eq!(std::fs::read(&aside).expect("still there"), latin1);
    }

    #[test]
    fn a_file_this_user_may_not_read_is_moved_aside_rather_than_replaced() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        let text = "[[app]]\nname = \"Discord\"\ninput_preset = \"Headset\"\n";
        std::fs::write(&path, text).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        if std::fs::read(&path).is_ok() {
            // Root reads a mode-000 file anyway, so there is no unreadable file to test with.
            return;
        }

        assert_eq!(AppRules::load_from(&path), AppRules::default());
        let aside = AppRules::bad_path(&path);
        assert!(
            !path.exists(),
            "nothing is left for the next save to replace"
        );

        let mut rules = AppRules::default();
        rules.seen(&key("firefox", "Firefox", ""), 2);
        rules.save_to(&path).expect("save");
        std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert_eq!(
            std::fs::read_to_string(&aside).expect("readable once allowed"),
            text,
            "the user's choices are still there once the permissions are fixed"
        );
    }

    #[test]
    fn loading_drops_entries_no_application_could_match_and_keeps_the_first_of_two_alike() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        let text = "\
[[app]]
binary = \"bf6.exe\"
name = \"Battlefield 6\"
output_preset = \"Gaming\"
last_seen = 10

[[app]]
output_preset = \"Rock\"
last_seen = 20

[[app]]
binary = \"C:\\\\Games\\\\BF6.EXE\"
name = \"Battlefield 6\"
output_preset = \"Movies\"
last_seen = 30

[[app]]
name = \"Discord\"
input_preset = \"Headset\"
last_seen = 40
";
        std::fs::write(&path, text).expect("write");
        let loaded = AppRules::load_from(&path);
        assert_eq!(
            loaded.apps,
            vec![
                rule(key("bf6.exe", "Battlefield 6", ""), "Gaming", "", 10),
                rule(key("", "Discord", ""), "", "Headset", 40),
            ]
        );
        assert!(
            !AppRules::bad_path(&path).exists(),
            "repaired, not rejected"
        );
    }

    #[test]
    fn a_hand_edited_last_seen_past_what_toml_holds_cannot_break_the_next_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        let rules = AppRules {
            apps: vec![rule(key("mpv", "mpv", ""), "Movies", "", u64::MAX)],
        };
        rules.save_to(&path).expect("save");
        let loaded = AppRules::load_from(&path);
        assert_eq!(loaded.apps[0].last_seen, i64::MAX as u64);
    }

    // ---- the cap ---------------------------------------------------------------------------

    fn numbered(count: usize) -> AppRules {
        AppRules {
            apps: (0..count)
                .map(|n| rule(key(&format!("app{n}"), "", ""), "", "", n as u64))
                .collect(),
        }
    }

    #[test]
    fn past_the_cap_the_applications_seen_longest_ago_are_forgotten_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("apps.toml");
        let mut rules = numbered(MAX_REMEMBERED_APPS + 25);
        // Shuffle the file order so the cap reads `last_seen`, not the position.
        rules.apps.reverse();
        let text = toml::to_string_pretty(&rules).expect("serialise");
        std::fs::write(&path, text).expect("write");

        let loaded = AppRules::load_from(&path);
        assert_eq!(loaded.apps.len(), MAX_REMEMBERED_APPS);
        assert!(loaded.apps.iter().all(|rule| rule.last_seen >= 25));
        // What remains keeps the file's order.
        assert!(
            loaded
                .apps
                .windows(2)
                .all(|pair| pair[0].last_seen > pair[1].last_seen)
        );
    }

    #[test]
    fn a_new_application_at_the_cap_forgets_the_one_seen_longest_ago() {
        let mut rules = numbered(MAX_REMEMBERED_APPS);
        assert!(rules.seen(&key("new", "", ""), 1_000));
        assert_eq!(rules.apps.len(), MAX_REMEMBERED_APPS);
        assert!(
            rules.rule(&key("app0", "", "")).is_none(),
            "the oldest went"
        );
        assert!(rules.rule(&key("app1", "", "")).is_some());
        assert!(rules.rule(&key("new", "", "")).is_some());
    }

    #[test]
    fn a_rule_just_set_survives_the_cap_whatever_the_clock_says() {
        let mut rules = numbered(MAX_REMEMBERED_APPS);
        for existing in &mut rules.apps {
            existing.last_seen += 1_000;
        }
        // A clock that jumped back: the new rule is the oldest by `last_seen`.
        assert!(rules.upsert(&key("game.exe", "", ""), OUT, "Gaming", 5));
        assert_eq!(rules.apps.len(), MAX_REMEMBERED_APPS);
        assert_eq!(
            rules.preset_for(&key("game.exe", "", ""), OUT),
            Some("Gaming")
        );
        assert!(rules.rule(&key("app0", "", "")).is_none());
    }

    #[test]
    fn between_two_seen_at_the_same_second_the_one_listed_later_goes() {
        let mut rules = AppRules {
            apps: (0..=MAX_REMEMBERED_APPS)
                .map(|n| rule(key(&format!("app{n}"), "", ""), "", "", 7))
                .collect(),
        };
        rules.sanitise();
        assert_eq!(rules.apps.len(), MAX_REMEMBERED_APPS);
        assert!(rules.rule(&key("app0", "", "")).is_some());
        assert!(
            rules
                .rule(&key(&format!("app{MAX_REMEMBERED_APPS}"), "", ""))
                .is_none()
        );
    }

    // ---- lookups and fallbacks ---------------------------------------------------------------

    #[test]
    fn an_application_with_no_rule_follows_both_lanes() {
        let rules = AppRules::default();
        let firefox = key("firefox", "Firefox", "");
        assert_eq!(rules.preset_for(&firefox, OUT), None);
        assert_eq!(rules.preset_for(&firefox, IN), None);
        assert_eq!(rules.resolve(&firefox, OUT, |_| true), AppPreset::Follow);
    }

    #[test]
    fn a_rule_gives_a_preset_only_to_the_direction_it_names() {
        let mut rules = AppRules::default();
        let discord = key("discord", "Discord", "");
        rules.upsert(&discord, IN, "Headset", 1);
        assert_eq!(rules.preset_for(&discord, IN), Some("Headset"));
        assert_eq!(rules.preset_for(&discord, OUT), None, "the output follows");
    }

    #[test]
    fn an_empty_or_blank_preset_follows_the_lane() {
        let rules = AppRules {
            apps: vec![rule(key("mpv", "", ""), "", "   ", 1)],
        };
        assert_eq!(rules.preset_for(&key("mpv", "", ""), OUT), None);
        assert_eq!(rules.preset_for(&key("mpv", "", ""), IN), None);
        assert!(!rules.apps[0].has_preset(IN));
    }

    #[test]
    fn a_rule_naming_a_preset_that_is_gone_is_reported_and_followed() {
        let mut rules = AppRules::default();
        let bf6 = key("bf6.exe", "Battlefield 6", "");
        rules.upsert(&bf6, OUT, "Gaming", 1);
        assert_eq!(
            rules.resolve(&bf6, OUT, |name| name == "Gaming"),
            AppPreset::Preset("Gaming")
        );
        assert_eq!(
            rules.resolve(&bf6, OUT, |name| name == "Rock"),
            AppPreset::Missing("Gaming"),
            "the name comes back so the window can say which preset is gone"
        );
        assert_eq!(rules.resolve(&bf6, IN, |_| false), AppPreset::Follow);
    }

    #[test]
    fn the_most_specific_rule_answers() {
        let rules = AppRules {
            apps: vec![
                rule(key("", "Discord", ""), "Rock", "", 1),
                rule(key("discord", "", ""), "Jazz", "", 2),
                rule(key("", "", "com.discordapp.Discord"), "Gaming", "", 3),
            ],
        };
        assert_eq!(
            rules.preset_for(&key("discord", "Discord", "com.discordapp.Discord"), OUT),
            Some("Gaming"),
            "the Flatpak id beats the binary and the name"
        );
        assert_eq!(
            rules.preset_for(&key("Discord", "Discord", ""), OUT),
            Some("Jazz"),
            "the binary beats the name"
        );
        assert_eq!(
            rules.preset_for(&key("", "Discord", ""), OUT),
            Some("Rock"),
            "a name alone matches the rule for the name"
        );
    }

    #[test]
    fn between_rules_matched_as_strongly_the_exact_one_answers_then_the_first() {
        let rules = AppRules {
            apps: vec![
                rule(key("firefox", "", ""), "Rock", "", 1),
                rule(key("firefox", "Firefox", ""), "Jazz", "", 2),
            ],
        };
        // Both match by binary; the second carries exactly the key's identifiers.
        assert_eq!(
            rules.preset_for(&key("firefox", "Firefox", ""), OUT),
            Some("Jazz")
        );
        // Neither is exact for this one: the first listed answers.
        assert_eq!(
            rules.preset_for(&key("FIREFOX", "Firefox Nightly", ""), OUT),
            Some("Rock")
        );
    }

    // ---- which rules can outrank which ---------------------------------------------------------

    #[test]
    fn rules_for_applications_with_nothing_in_common_never_outrank_each_other() {
        let mpv = key("mpv", "mpv", "");
        let game = key("bf6.exe", "Battlefield 6", "");
        assert!(!mpv.may_outrank(&game));
        assert!(!game.may_outrank(&mpv));
        assert!(
            !AppKey::default().may_outrank(&game),
            "an empty key matches nothing"
        );
    }

    #[test]
    fn a_flatpaks_rule_and_the_native_rule_of_its_program_may_each_outrank_the_other() {
        let native = key("firefox", "Firefox", "");
        let sandboxed = key("firefox", "Firefox", "org.mozilla.firefox");
        assert!(
            sandboxed.may_outrank(&native),
            "the Flatpak's own stream: its id beats the native rule's binary"
        );
        assert!(
            native.may_outrank(&sandboxed),
            "the native stream: both match it by the binary, and the native rule is exact"
        );
    }

    #[test]
    fn a_rule_by_name_alone_never_outranks_a_rule_by_flatpak_id() {
        let by_name = key("", "Discord", "");
        let by_id = key("", "", "com.discordapp.Discord");
        assert!(!by_name.may_outrank(&by_id));
        assert!(
            by_id.may_outrank(&by_name),
            "Discord's stream carries both, and its id decides"
        );
    }

    /// Every key whose identifiers come from a few values that share letters in both cases.
    fn universe() -> Vec<AppKey> {
        let mut keys = Vec::new();
        for flatpak in ["", "org.a.A", "org.b.B"] {
            for binary in ["", "a", "A", "b", "/opt/a"] {
                for name in ["", "A", "B", " A "] {
                    keys.push(key(binary, name, flatpak));
                }
            }
        }
        keys
    }

    #[test]
    fn a_rule_may_outrank_another_exactly_when_some_application_says_so() {
        let keys = universe();
        for mine in &keys {
            for theirs in &keys {
                let witness = keys.iter().any(|application| {
                    match (
                        mine.match_strength(application),
                        theirs.match_strength(application),
                    ) {
                        (Some(this), Some(that)) => this >= that,
                        _ => false,
                    }
                });
                assert_eq!(
                    mine.may_outrank(theirs),
                    witness,
                    "{mine:?} over {theirs:?}"
                );
            }
        }
    }

    #[test]
    fn leaving_out_the_rules_that_cannot_outrank_a_preset_changes_no_applications_preset() {
        // Every rule of the universe, each in turn naming a preset or following, as the app sends
        // them: what the engine picks among those must be what the store picks among all.
        let keys = universe();
        for (index, with_preset) in keys.iter().enumerate() {
            for (other, second) in keys.iter().enumerate().skip(index + 1).step_by(11) {
                let mut store = AppRules::default();
                for (at, rule_key) in keys.iter().enumerate() {
                    let preset = if at == index || at == other {
                        "Gaming"
                    } else {
                        ""
                    };
                    store.apps.push(rule(rule_key.clone(), preset, "", 1));
                }
                store.sanitise();
                let sent: Vec<&AppRule> = store
                    .apps
                    .iter()
                    .filter(|rule| {
                        rule.has_preset(OUT)
                            || store.apps.iter().any(|preset| {
                                preset.has_preset(OUT) && rule.key.may_outrank(&preset.key)
                            })
                    })
                    .collect();
                for application in &keys {
                    let engine = application
                        .best_match(sent.iter().map(|rule| &rule.key))
                        .map(|at| sent[at].preset(OUT))
                        .filter(|preset| !preset.is_empty());
                    assert_eq!(
                        engine,
                        store.preset_for(application, OUT),
                        "{application:?} with {with_preset:?} and {second:?} on a preset"
                    );
                }
            }
        }
    }

    // ---- changes -----------------------------------------------------------------------------

    #[test]
    fn upserting_twice_changes_one_rule() {
        let mut rules = AppRules::default();
        let brave = key("brave", "Brave", "");
        assert!(rules.upsert(&brave, OUT, "Volume Boost", 10));
        assert!(rules.upsert(&brave, OUT, "Music", 20));
        assert_eq!(rules.apps.len(), 1);
        assert_eq!(rules.preset_for(&brave, OUT), Some("Music"));
        assert_eq!(rules.apps[0].last_seen, 20);
        assert!(
            !rules.upsert(&brave, OUT, "Music", 20),
            "the same choice at the same time changes nothing"
        );
        assert!(rules.upsert(&brave, OUT, "", 20), "back to following");
        assert_eq!(rules.preset_for(&brave, OUT), None);
        assert_eq!(
            rules.apps.len(),
            1,
            "a rule that follows is still remembered"
        );
    }

    #[test]
    fn upserting_the_same_binary_in_another_case_changes_the_same_rule() {
        let mut rules = AppRules::default();
        rules.upsert(&key("bf6.exe", "Battlefield 6", ""), OUT, "Gaming", 1);
        rules.upsert(&key("BF6.EXE", "Battlefield 6", ""), IN, "Headset", 2);
        assert_eq!(rules.apps.len(), 1);
        assert_eq!(rules.apps[0].output_preset, "Gaming");
        assert_eq!(rules.apps[0].input_preset, "Headset");
    }

    #[test]
    fn a_more_specific_rule_keeps_what_the_general_one_said_for_the_other_direction() {
        let mut rules = AppRules::default();
        let everyone_called_discord = key("", "Discord", "");
        rules.upsert(&everyone_called_discord, OUT, "Music", 1);

        let running = key("discord", "Discord", "com.discordapp.Discord");
        assert!(rules.upsert(&running, IN, "Headset", 2));
        assert_eq!(rules.apps.len(), 2, "the general rule stays general");
        assert_eq!(rules.preset_for(&running, IN), Some("Headset"));
        assert_eq!(
            rules.preset_for(&running, OUT),
            Some("Music"),
            "choosing a microphone preset did not undo the output one"
        );
        // Another program with the same name still follows the general rule alone.
        let other = key("", "Discord", "");
        assert_eq!(rules.preset_for(&other, OUT), Some("Music"));
        assert_eq!(rules.preset_for(&other, IN), None);
    }

    #[test]
    fn a_key_with_no_identifier_is_refused() {
        let mut rules = AppRules::default();
        assert!(!rules.upsert(&AppKey::default(), OUT, "Gaming", 1));
        assert!(!rules.seen(&key(" ", "", ""), 1));
        assert!(rules.apps.is_empty());
        assert!(!rules.forget(&AppKey::default()));
    }

    #[test]
    fn seeing_a_new_application_remembers_it_following_both_lanes() {
        let mut rules = AppRules::default();
        let mpv = key("mpv", "mpv", "");
        assert!(rules.seen(&mpv, 100));
        assert_eq!(rules.apps, vec![rule(mpv.clone(), "", "", 100)]);
        assert!(!rules.seen(&mpv, 100), "seen again in the same second");
        assert!(
            !rules.seen(&mpv, 50),
            "a clock that went back does not age it"
        );
        assert!(rules.seen(&mpv, 200));
        assert_eq!(rules.apps[0].last_seen, 200);
    }

    #[test]
    fn seeing_an_application_a_general_rule_covers_only_refreshes_that_rule() {
        let mut rules = AppRules::default();
        rules.upsert(&key("", "Discord", ""), OUT, "Music", 1);
        let running = key("discord", "Discord", "com.discordapp.Discord");
        assert!(rules.seen(&running, 50));
        assert_eq!(
            rules.apps.len(),
            1,
            "no entry of its own to shadow the rule"
        );
        assert_eq!(
            rules.apps[0].key,
            key("", "Discord", ""),
            "and the rule is not narrowed"
        );
        assert_eq!(rules.apps[0].last_seen, 50);
        assert_eq!(rules.preset_for(&running, OUT), Some("Music"));
    }

    #[test]
    fn forgetting_removes_the_row_with_the_same_identifiers() {
        // Two rows matching each other by the binary, as a hand edit can leave them: each row's
        // own key forgets that row and no other.
        let general = key("firefox", "", "");
        let specific = key("firefox", "Firefox", "");
        let mut rules = AppRules {
            apps: vec![
                rule(general.clone(), "Rock", "", 1),
                rule(specific.clone(), "Jazz", "", 2),
            ],
        };
        assert!(rules.forget(&specific));
        assert_eq!(rules.apps, vec![rule(general.clone(), "Rock", "", 1)]);

        let mut rules = AppRules {
            apps: vec![
                rule(general.clone(), "Rock", "", 1),
                rule(specific.clone(), "Jazz", "", 2),
            ],
        };
        assert!(rules.forget(&general));
        assert_eq!(rules.apps, vec![rule(specific, "Jazz", "", 2)]);
        assert!(!rules.forget(&key("mpv", "", "")), "nothing to forget");
    }

    #[test]
    fn a_program_that_renamed_itself_keeps_one_rule() {
        let mut rules = AppRules::default();
        rules.upsert(&key("bf6.exe", "Battlefield 6", ""), OUT, "Gaming", 1);
        assert!(rules.upsert(&key("bf6.exe", "Battlefield™ 6", ""), IN, "Headset", 2));
        assert_eq!(
            rules.apps.len(),
            1,
            "the binary decided, and it is the same program"
        );
        assert_eq!(rules.apps[0].output_preset, "Gaming");
        assert_eq!(rules.apps[0].input_preset, "Headset");
        assert_eq!(rules.apps[0].last_seen, 2);
    }

    #[test]
    fn a_name_alone_changes_the_rule_of_the_program_that_carries_it() {
        // What `--app-preset "Battlefield 6=Movies"` hands the store when the game has a rule.
        let mut rules = AppRules::default();
        let game = key("bf6.exe", "Battlefield 6", "");
        rules.upsert(&game, OUT, "Gaming", 1);
        assert!(rules.upsert(&key("", "Battlefield 6", ""), OUT, "Movies", 2));
        assert_eq!(rules.apps.len(), 1);
        assert_eq!(rules.preset_for(&game, OUT), Some("Movies"));
    }

    #[test]
    fn a_sandboxed_install_gets_a_rule_of_its_own_beside_the_native_one() {
        let mut rules = AppRules::default();
        let native = key("firefox", "Firefox", "");
        let sandboxed = key("firefox", "Firefox", "org.mozilla.firefox");
        rules.upsert(&native, OUT, "Music", 1);
        assert!(rules.upsert(&sandboxed, IN, "Headset", 2));
        assert_eq!(
            rules.apps.len(),
            2,
            "the native rule matched by the binary only"
        );
        assert_eq!(
            rules.preset_for(&sandboxed, OUT),
            Some("Music"),
            "carried over"
        );
        assert_eq!(rules.preset_for(&sandboxed, IN), Some("Headset"));
        assert_eq!(
            rules.preset_for(&native, IN),
            None,
            "the native one is untouched"
        );
        // And changing the sandboxed one again changes its own rule.
        assert!(rules.upsert(&sandboxed, OUT, "Rock", 3));
        assert_eq!(rules.apps.len(), 2);
        assert_eq!(rules.preset_for(&native, OUT), Some("Music"));
    }

    #[test]
    fn forgetting_a_running_application_removes_the_rule_that_matched_it() {
        let mut rules = AppRules::default();
        rules.upsert(&key("", "Discord", ""), OUT, "Music", 1);
        assert!(rules.forget(&key("discord", "Discord", "com.discordapp.Discord")));
        assert!(rules.apps.is_empty());
    }

    #[test]
    fn forgetting_a_specific_rule_falls_back_to_the_general_one() {
        let mut rules = AppRules::default();
        let running = key("discord", "Discord", "");
        rules.upsert(&key("", "Discord", ""), OUT, "Music", 1);
        rules.upsert(&running, OUT, "Gaming", 2);
        assert_eq!(rules.preset_for(&running, OUT), Some("Gaming"));
        assert!(rules.forget(&running));
        assert_eq!(rules.preset_for(&running, OUT), Some("Music"));
    }

    #[test]
    fn a_renamed_preset_is_followed_by_every_rule_of_its_direction() {
        let mut rules = AppRules {
            apps: vec![
                rule(key("bf6.exe", "", ""), "Gaming", "", 1),
                rule(key("brave", "", ""), "Gaming", "Gaming", 2),
                rule(key("mpv", "", ""), "Movies", "", 3),
            ],
        };
        assert_eq!(rules.rename_preset(OUT, "Gaming", "Shooters"), 2);
        assert_eq!(rules.apps[0].output_preset, "Shooters");
        assert_eq!(rules.apps[1].output_preset, "Shooters");
        assert_eq!(
            rules.apps[1].input_preset, "Gaming",
            "an input preset of the same name is another preset"
        );
        assert_eq!(rules.apps[2].output_preset, "Movies");
        assert_eq!(rules.rename_preset(OUT, "Movies", "Movies"), 0);
    }

    #[test]
    fn the_most_specific_of_many_rule_keys_is_picked_as_the_store_picks_it() {
        let stream = key("discord", "Discord", "com.discordapp.Discord");
        let rules = [
            key("", "Discord", ""),
            key("Discord", "", ""),
            key("", "", "com.discordapp.Discord"),
            key("", "", "com.discordapp.Discord"),
        ];
        assert_eq!(
            stream.best_match(&rules),
            Some(2),
            "the Flatpak id beats the binary, which beats the name; the first of two equals wins"
        );
        assert_eq!(stream.best_match(&rules[..2]), Some(1));
        assert_eq!(stream.best_match(&rules[..1]), Some(0));
        assert_eq!(
            stream.best_match(&[key("", "Slack", ""), key("", "", "com.slack.Slack")]),
            None
        );
        assert_eq!(stream.best_match(std::iter::empty()), None);

        // Between two matched as strongly, the one with exactly the stream's identifiers wins.
        let general = key("discord", "", "");
        let exact = key("Discord", "Discord", "");
        let stream = key("discord", "Discord", "");
        assert_eq!(stream.best_match([&general, &exact]), Some(1));
        assert_eq!(stream.best_match([&exact, &general]), Some(0));
    }

    #[test]
    fn the_store_and_a_list_of_keys_agree_on_which_rule_names_an_application() {
        let rules = AppRules {
            apps: vec![
                rule(key("", "Chromium", ""), "Movies", "", 1),
                rule(key("brave", "", ""), "Volume Boost", "", 2),
                rule(key("", "", "com.brave.Browser"), "Gaming", "", 3),
            ],
        };
        for stream in [
            key("brave", "Chromium", ""),
            key("brave", "Chromium", "com.brave.Browser"),
            key("chromium", "Chromium", ""),
            key("", "Chromium", ""),
            key("mpv", "mpv", ""),
        ] {
            let by_store = rules.rule(&stream).map(|rule| rule.output_preset.clone());
            let by_keys = stream
                .best_match(rules.apps.iter().map(|rule| &rule.key))
                .map(|index| rules.apps[index].output_preset.clone());
            assert_eq!(by_store, by_keys, "{stream:?}");
        }
    }

    #[test]
    fn the_clock_reads_seconds_since_the_epoch() {
        // 2020-09-13, well before any build of this port; and not milliseconds, which would be
        // a thousand times larger than anything a TOML reader expects here.
        let now = unix_now();
        assert!(now > 1_600_000_000, "{now}");
        assert!(now < 100_000_000_000, "{now}");
    }
}
