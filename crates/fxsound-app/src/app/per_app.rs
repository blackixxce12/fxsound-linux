//! Per-application presets, the controller's half (`docs/0.4.0-apps.md`, "The local store" and
//! "Core API").
//!
//! Three things meet here:
//!
//! - the **store**, `apps.toml` ([`AppRules`]): what the user chose for each application, and
//!   every application FxSound has seen play or record, so the Applications list can offer a
//!   preset for a game that is not running;
//! - the **streams** the engine reports, whole, whenever they change
//!   ([`AudioToUi::AppStreams`](fxsound_core::AudioToUi::AppStreams)):
//!   which applications play and record now, and which of them it has moved onto a route;
//! - the **routes** that follow from the store ([`UiToAudio::SetAppRoutes`]): one for every rule
//!   that names a preset of its own its lane's preset store has, whether its application runs or
//!   not, under the rule's own key; and beside them, as entries with no preset, the rules that
//!   follow the lane where one could outrank those ([`AppKey::may_outrank`]). The engine picks each
//!   stream's rule among them the way the store picks among all of its own
//!   ([`AppKey::best_match`]), so a Flatpak Firefox whose own rule follows the lane stays there
//!   although the native Firefox's rule, which names a preset, matches it by its program. The set
//!   is the engine's whatever plays: a route outlives its streams by the engine's idle time, so a
//!   player that closes its stream between two tracks comes back to the same pair, and a new stream
//!   finds its rule without waiting for the app to hear of it.
//!
//! The app resolves a rule to parameters because it owns the preset stores; the engine only runs
//! them. A route is built the way its lane's own snapshot is — the same reading of the preset into
//! controls ([`super::music_controls`]), the same mapping onto the chain
//! ([`super::write_music_params`], [`super::apply_microphone_settings`]), and the same things the
//! lane shares with every chain of its kind — so a preset sounds the same on a route as on the
//! lane:
//!
//! - a playback route runs the `.fac`'s five effects and equalizer on the user's band count, with
//!   the speakers' filter width, master gain, balance and volume leveller;
//! - a recording route runs the voice preset with the Settings pane's microphone settings written
//!   over it, and the voice chain it names;
//! - both carry the power switch and the mute of a system going to sleep.
//!
//! A route runs its preset as last **saved** ([`fxsound_preset::Store::load_saved`]): unsaved
//! edits in the window are the lane's, not what the name another application asked for says.
//!
//! The whole set goes to the engine whenever it changes, and only then: a rule changed, a preset a
//! rule names was saved, renamed or deleted, or something every chain shares moved — never because
//! a stream came or went. A preset renamed in the window carries every rule to its new name; one
//! deleted there returns every rule that named it to following the lane, and the window says so
//! once. A rule naming a preset the store does not have — a hand edit, a file removed behind
//! FxSound's back — follows the lane without being rewritten, so the preset coming back brings the
//! route back; the window says so once a session, when the application plays or records.
//!
//! `app_routed` on the event stream is the engine's word, not the rules': it says where the engine
//! has actually moved an application ([`AppStream::route`]), so a route that could not be made —
//! more presets than a lane runs at once — is never announced as though it had been.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fxsound_core::apps::{AppRule, unix_now};
use fxsound_core::messages::{AppRoute, AppStream, DspParams, InputDspParams, RouteParams};
use fxsound_core::{AppKey, AppPreset, AppRules, DeviceDirection, Preset, UiToAudio};
use fxsound_preset::input::InputPreset;
use fxsound_ui::dialogs::settings::{AppLane, AppRow};
use fxsound_ui::state::RoutedApp;

use super::{
    App, MusicControls, MusicLevels, PresetVoicing, apply_microphone_settings, ladder, lane_index,
    music_controls, write_music_params,
};
use crate::events::AppEvent;
use fxsound_core::i18n::tr_args;

/// How long after a remembered application is seen playing again the store is written.
///
/// Seeing it again changes only its `last_seen`, which decides which of five hundred remembered
/// applications is forgotten first and the order the Applications list shows them in: nothing a
/// few minutes' lag can get wrong. Written on each report, it would be a file replaced every time
/// a browser opens and closes a tab's audio. A new application, a rule chosen and a rule forgotten
/// are written at once, and whatever is still unwritten at exit is written then.
pub(super) const SEEN_SAVE_DELAY: Duration = Duration::from_secs(5 * 60);

/// How long after a write of the store failed it is tried again: a full disk, a quota, a
/// configuration directory remounted read-only for a moment. Until a write succeeds the store stays
/// due, so the timer keeps trying and the way out tries once more.
pub(super) const SAVE_RETRY_DELAY: Duration = Duration::from_secs(30);

/// How long a cold start waits to hear which applications play and record before it keeps a
/// preset named for an application FxSound does not remember ([`App::hold_app_presets`]). The
/// engine reports every stream within a supervisor tick of meeting it, but reports nothing at all
/// while nothing plays, so the wait cannot be for the report alone.
pub(super) const STREAMS_PATIENCE: Duration = Duration::from_secs(3);

/// The controller's per-application state.
#[derive(Debug, Default)]
pub(super) struct AppPresets {
    /// What the user chose, and every application seen.
    rules: AppRules,
    /// Where the store is written; `None` keeps it in memory — a test, or a run that must not
    /// touch the user's files, as `App::persist` keeps the settings.
    path: Option<PathBuf>,
    /// When the store is due to be written: at once for a change the user made or a new
    /// application, [`SEEN_SAVE_DELAY`] after one seen again, [`SAVE_RETRY_DELAY`] after a write
    /// that failed. `None` while nothing is unwritten.
    save_due: Option<Instant>,
    /// Whether the last write failed, so that the next failure is not logged as loudly and a
    /// success is.
    save_failing: bool,
    /// Every application stream the engine last reported, both lanes, in its order.
    streams: Vec<AppStream>,
    /// What the engine is to know of the rules, lane by lane, each lane in the store's order: every
    /// rule that names a preset its lane's store has and that loads, with that preset as loaded,
    /// and every rule that follows the lane where it could outrank one of those. Rebuilt when the
    /// rules or the preset stores change, and made into routes with the levels of the moment
    /// whenever those move ([`App::refresh_app_routes`]).
    resolved: Vec<Resolved>,
    /// The presets, per lane, a rule named that are there but did not load.
    unloadable: Vec<(DeviceDirection, String)>,
    /// The routes the engine was last given.
    sent: Vec<AppRoute>,
    /// The presets, per lane, a rule named that could not be run — not there, or not readable —
    /// that the window has already said so about this session.
    said: HashSet<(DeviceDirection, String)>,
    /// While a cold start waits to hear which applications play and record: until when
    /// ([`STREAMS_PATIENCE`]).
    holding: Option<Instant>,
    /// The presets named in the meantime for applications FxSound does not remember, in the order
    /// they were named: `(text, lane, preset)`.
    held: Vec<(String, DeviceDirection, String)>,
    /// Per lane and application, the route the engine last reported its streams on, in the order
    /// the engine listed them: what `app_routed` has said.
    routed: Vec<(DeviceDirection, AppKey, String)>,
    /// Every application and lane the engine has reported a stream for since start-up: which
    /// combos the Applications pane gives a row. The store does not say which lanes an
    /// application uses, so one not heard this session gets both.
    used: HashSet<(AppKey, DeviceDirection)>,
}

impl AppPresets {
    /// The store at `~/.config/fxsound/apps.toml`, written back there.
    pub(super) fn load() -> Self {
        Self::load_from(AppRules::config_path())
    }

    /// The store at `path`, written back there. A file that does not load is moved aside and the
    /// store starts empty ([`AppRules::load_from`]).
    fn load_from(path: PathBuf) -> Self {
        Self {
            rules: AppRules::load_from(&path),
            path: Some(path),
            ..Self::default()
        }
    }
}

/// One rule as the engine is to know it: its lane, its key, and the preset it runs there as its
/// lane's store has it — or `None`, with an empty name, for a rule that follows the lane.
#[derive(Debug, Clone)]
struct Resolved {
    direction: DeviceDirection,
    app: AppKey,
    preset: String,
    source: Option<Source>,
}

/// A preset as its file says, before the levels of the moment are applied.
#[derive(Debug, Clone)]
enum Source {
    Music(Preset),
    Voice(InputPreset),
}

/// Why [`App::set_app_preset`] or [`App::set_named_app_preset`] changed nothing. The text is what
/// the command line prints and a D-Bus caller is answered with: English, as every refusal of the
/// command path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRuleRefusal {
    /// The application was named by nothing: no binary, no name, no Flatpak id. No stream could
    /// ever match such a rule.
    NoApplication,
    /// No preset of `direction`'s store is called `name`.
    UnknownPreset {
        direction: DeviceDirection,
        name: String,
    },
}

impl std::fmt::Display for AppRuleRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoApplication => f.write_str("no application was named"),
            Self::UnknownPreset { direction, name } => {
                write!(f, "no {} preset is called {name:?}", direction.key())
            }
        }
    }
}

impl std::error::Error for AppRuleRefusal {}

/// The notice that the applications whose rule named `preset` follow the lane's preset now: the
/// preset was deleted, or is not there to run.
pub(super) fn followed_notice(preset: &str) -> String {
    tr_args(
        "Applications that used %s now follow FxSound's preset",
        &[preset],
    )
}

/// What [`App::set_named_app_preset`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedAppRule {
    /// The applications the name reached ([`apps_named`]); the one [`unseen_key`] makes of it when
    /// it reached none. Empty when it reached none and there was nothing to choose, or the choice
    /// is held.
    pub apps: Vec<AppKey>,
    /// Whether the name reached no application FxSound has seen, so that the preset was kept
    /// under [`unseen_key`], or is held until FxSound has heard which applications run.
    pub unseen: bool,
    /// Whether the choice is held until FxSound has heard which applications play and record
    /// ([`App::hold_app_presets`]): the name reached no application FxSound remembers, and one
    /// that runs now may still answer to it.
    pub held: bool,
    /// Whether the store changed.
    pub changed: bool,
}

/// The key a preset named for `text` is kept under when no application FxSound has seen answers
/// to it: `text` in the identifier it looks like. Each identifier of a rule is compared with the
/// same one of a stream only ([`AppKey::matches`]), so a Flatpak id kept as a program would never
/// reach the Flatpak.
///
/// - A path or a Windows program (`/usr/bin/mpv`, `bf6.exe`) is a program.
/// - A Flatpak id (`com.discordapp.Discord`: three or more parts between dots, each of letters,
///   digits, `_` and `-`, and starting with a letter or `_`) is a Flatpak id.
/// - Text with a space in it (`Battlefield 6`) is the name the application gives itself.
/// - One word (`firefox`, `Discord`) is a program, which is compared without regard to case, as
///   a name is not.
#[must_use]
pub fn unseen_key(text: &str) -> AppKey {
    let text = text.trim();
    let program = text.contains(['/', '\\'])
        || text
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("exe"));
    let parts: Vec<&str> = text.split('.').collect();
    let flatpak = parts.len() >= 3
        && parts.iter().all(|part| {
            part.chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    let mut key = AppKey::default();
    let field = if program {
        &mut key.binary
    } else if flatpak {
        &mut key.flatpak
    } else if text.contains(char::is_whitespace) {
        &mut key.name
    } else {
        &mut key.binary
    };
    text.clone_into(field);
    key
}

/// What a key [`unseen_key`] made names, in words for the command line's note and the log: `the
/// program "bf6.exe"`, `the Flatpak "com.discordapp.Discord"`, `the application called
/// "Battlefield 6"`.
#[must_use]
pub fn unseen_description(key: &AppKey) -> String {
    if !key.flatpak.trim().is_empty() {
        format!("the Flatpak {:?}", key.flatpak.trim())
    } else if !key.binary.trim().is_empty() {
        format!("the program {:?}", key.binary.trim())
    } else {
        format!("the application called {:?}", key.name.trim())
    }
}

/// One remembered application as the lists show it: its rule, and what its streams are doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedApp<'a> {
    /// The rule's place in the store.
    pub index: usize,
    pub rule: &'a AppRule,
    /// Per lane, output first: whether a stream the rule answers for plays or records now.
    pub running: [bool; 2],
    /// Per lane: the preset of the route the engine has moved those streams onto, `None` while
    /// they are on the lane's own chain.
    pub routed: [Option<&'a str>; 2],
}

impl ListedApp<'_> {
    /// Whether it plays or records now.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.contains(&true)
    }
}

/// Every rule of `rules`, in the order Settings ▸ Applications and `--list-apps` show them: the
/// ones some stream of `streams` answers to ([`AppRules::rule`]) first, by name, then the rest,
/// the most recently seen first, then by name.
#[must_use]
pub fn list_apps<'a>(rules: &'a AppRules, streams: &'a [AppStream]) -> Vec<ListedApp<'a>> {
    let mut listed: Vec<ListedApp<'a>> = rules
        .apps
        .iter()
        .enumerate()
        .map(|(index, rule)| ListedApp {
            index,
            rule,
            running: [false; 2],
            routed: [None; 2],
        })
        .collect();
    for stream in streams {
        if let Some(index) = stream
            .app
            .best_match(rules.apps.iter().map(|rule| &rule.key))
        {
            let lane = lane_index(stream.direction);
            let entry = &mut listed[index];
            entry.running[lane] = true;
            if entry.routed[lane].is_none() {
                entry.routed[lane] = stream.route.as_deref();
            }
        }
    }
    listed.sort_by(|a, b| {
        let by_name = || {
            let name = |listed: &ListedApp<'_>| listed.rule.key.display().to_lowercase();
            name(a).cmp(&name(b))
        };
        b.is_running().cmp(&a.is_running()).then_with(|| {
            if a.is_running() {
                by_name()
            } else {
                b.rule.last_seen.cmp(&a.rule.last_seen).then_with(by_name)
            }
        })
    });
    listed
}

/// The applications `text` names, the way `--app-preset` and D-Bus's `SetAppPreset` take one: by
/// its Flatpak id, else by its program, else by its name, each without regard to case or
/// surrounding whitespace, and the first of the three that answers decides. A program is compared
/// by its last path component on both sides, as the store compares it.
///
/// The store is asked first, and every rule that answers is named, in the store's order, not only
/// the first: `firefox` names both a native and a Flatpak Firefox when both have a rule, and the
/// Flatpak id tells them apart. Only when no rule answers are the streams playing and recording
/// now asked, in the engine's order: an application a rule covers under other identifiers — a
/// Flatpak Firefox the rule written for the native one covers — is still reached by its own.
/// Empty when nothing answers.
#[must_use]
pub fn apps_named(rules: &AppRules, streams: &[AppStream], text: &str) -> Vec<AppKey> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let fields: [fn(&AppKey) -> &str; 3] = [
        |key| key.flatpak.trim(),
        |key| basename(&key.binary),
        |key| key.name.trim(),
    ];
    let wanted = [text, basename(text), text];
    let answering = |keys: &[&AppKey]| {
        for (field, wanted) in fields.into_iter().zip(wanted) {
            let mut named: Vec<AppKey> = Vec::new();
            for &key in keys {
                let value = field(key);
                if !value.is_empty() && same_ignoring_case(value, wanted) && !named.contains(key) {
                    named.push(key.clone());
                }
            }
            if !named.is_empty() {
                return named;
            }
        }
        Vec::new()
    };
    let from_rules = answering(&rules.apps.iter().map(|rule| &rule.key).collect::<Vec<_>>());
    if !from_rules.is_empty() {
        return from_rules;
    }
    answering(&streams.iter().map(|stream| &stream.app).collect::<Vec<_>>())
}

/// A program's last path component, trimmed: `/usr/lib/firefox/firefox` and `C:\Games\bf6.exe`
/// are `firefox` and `bf6.exe`, as the store reads them.
fn basename(program: &str) -> &str {
    program
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
}

/// Whether the two are the same text in any case, character by character — which also gets a
/// localised Windows game's non-ASCII program right.
fn same_ignoring_case(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

// =============================================================================================
// What the window, the command line and D-Bus read and change
// =============================================================================================

impl App {
    /// The store: every remembered application and what was chosen for it.
    #[must_use]
    pub fn app_rules(&self) -> &AppRules {
        &self.apps.rules
    }

    /// Every application stream the engine last reported, both lanes, each with the route it is
    /// on.
    #[must_use]
    pub fn app_streams(&self) -> &[AppStream] {
        &self.apps.streams
    }

    /// The applications playing (`Output`) or recording (`Input`) now, each once, in the order
    /// the engine listed them.
    #[must_use]
    pub fn running_apps(&self, direction: DeviceDirection) -> Vec<&AppKey> {
        let mut running: Vec<&AppKey> = Vec::new();
        for stream in &self.apps.streams {
            if stream.direction == direction && !running.contains(&&stream.app) {
                running.push(&stream.app);
            }
        }
        running
    }

    /// Whether the application `app` names plays or records now: a stream whose rule is the one
    /// `app`'s is, or — for an application no rule covers — a stream `app` matches.
    #[must_use]
    pub fn app_is_running(&self, app: &AppKey) -> bool {
        let rules = &self.apps.rules;
        match rules.rule(app) {
            Some(rule) => self.apps.streams.iter().any(|stream| {
                rules
                    .rule(&stream.app)
                    .is_some_and(|own| std::ptr::eq(own, rule))
            }),
            None => self
                .apps
                .streams
                .iter()
                .any(|stream| stream.app.matches(app)),
        }
    }

    /// What the rule for `app` says for `direction`, measured against the presets that lane's
    /// store has now.
    #[must_use]
    pub fn app_preset(&self, app: &AppKey, direction: DeviceDirection) -> AppPreset<'_> {
        self.apps
            .rules
            .resolve(app, direction, |name| self.lane_has_preset(direction, name))
    }

    /// The routes the engine was last given ([`UiToAudio::SetAppRoutes`]).
    #[must_use]
    pub fn app_routes(&self) -> &[AppRoute] {
        &self.apps.sent
    }

    /// Settings ▸ Applications: a row per remembered application, in [`list_apps`]' order — the
    /// ones playing or recording now first, by name, then the rest, the most recently seen first.
    ///
    /// A row is a rule of the store, and a running one is a rule some stream answers to
    /// ([`AppRules::rule`]), so a general rule — one for every program called `Discord` — is one
    /// row however many programs it covers, as it is one choice. A row whose application has
    /// played or recorded this session has a combo for every lane it was heard on and every lane
    /// it has a preset of its own on. One not heard this session — remembered from an earlier one,
    /// or named from the command line — has both: the store does not say which lanes it uses, and
    /// a preset chosen for one lane says nothing about the other, so choosing one neither takes
    /// the other combo away nor moves the rows below under the pointer.
    #[must_use]
    pub fn app_rows(&self) -> Vec<AppRow> {
        let rules = &self.apps.rules;
        // Which lanes each rule's applications were heard on this session.
        let mut used = vec![[false; 2]; rules.apps.len()];
        for (app, direction) in &self.apps.used {
            if let Some(index) = app.best_match(rules.apps.iter().map(|rule| &rule.key)) {
                used[index][lane_index(*direction)] = true;
            }
        }

        list_apps(rules, &self.apps.streams)
            .into_iter()
            .map(|listed| {
                let rule = listed.rule;
                let heard = |direction: DeviceDirection| {
                    listed.running[lane_index(direction)]
                        || used[listed.index][lane_index(direction)]
                };
                let known = DeviceDirection::ALL.into_iter().any(heard);
                let lanes: Vec<DeviceDirection> = DeviceDirection::ALL
                    .into_iter()
                    .filter(|&direction| !known || heard(direction) || rule.has_preset(direction))
                    .collect();
                AppRow {
                    app: rule.key.clone(),
                    name: rule.key.display().to_owned(),
                    running: listed.is_running(),
                    lanes: lanes
                        .into_iter()
                        .map(|direction| AppLane {
                            direction,
                            // As written: a name the lane's store does not carry exactly is one
                            // the application does not run, and the pane shows it dimmed.
                            preset: rule
                                .has_preset(direction)
                                .then(|| rule.preset(direction).to_owned()),
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// The applications `text` names, the way the command line and D-Bus name one:
    /// [`apps_named`] over the store and the streams playing and recording now.
    #[must_use]
    pub fn apps_named(&self, text: &str) -> Vec<AppKey> {
        apps_named(&self.apps.rules, &self.apps.streams, text)
    }

    /// `--app-preset TEXT=PRESET` and D-Bus's `SetAppPreset`: choose the preset the applications
    /// `text` names run through in `direction` — `None` or a blank name to follow the lane's
    /// preset — and say so to the engine at once for the ones running.
    ///
    /// Each application [`App::apps_named`] finds for `text` gets it as the Settings pane gives it
    /// ([`AppRules::upsert`]): a rule's own key changes that rule, and a running application's
    /// key found only among the streams gets a rule of its own beside the one that covered it.
    /// When `text` names nothing, a rule is added for `text` as the identifier it looks like
    /// ([`unseen_key`]) — except to follow the lane, which an application with no rule does
    /// already. While a cold start waits to hear which applications play and record
    /// ([`App::hold_app_presets`]), such a choice is held instead, and made once it has. The
    /// store is written at once when it changed.
    ///
    /// # Errors
    ///
    /// A blank `text` (or one that is only a path's separators), and a preset `direction`'s store
    /// does not have, are refused and change nothing — not even a rule added for an application
    /// FxSound has not seen.
    pub fn set_named_app_preset(
        &mut self,
        text: &str,
        direction: DeviceDirection,
        preset: Option<&str>,
    ) -> Result<NamedAppRule, AppRuleRefusal> {
        let text = text.trim();
        if text.is_empty() {
            return Err(AppRuleRefusal::NoApplication);
        }
        let preset = preset.map_or("", str::trim);
        if !preset.is_empty() && !self.lane_has_preset(direction, preset) {
            return Err(AppRuleRefusal::UnknownPreset {
                direction,
                name: preset.to_owned(),
            });
        }
        let mut apps = self.apps_named(text);
        let unseen = apps.is_empty();
        if unseen {
            let kept = unseen_key(text);
            // `/` names no program: its last component is empty, and no stream could match it.
            if kept.is_empty() {
                return Err(AppRuleRefusal::NoApplication);
            }
            let held = !preset.is_empty() && self.apps.holding.is_some();
            if held {
                self.apps
                    .held
                    .push((text.to_owned(), direction, preset.to_owned()));
            }
            if preset.is_empty() || held {
                return Ok(NamedAppRule {
                    apps,
                    unseen,
                    held,
                    changed: false,
                });
            }
            apps.push(kept);
        }
        // A rule's key is that rule's own best match, so its upsert changes it in place; the
        // others each add the one rule of their own.
        let now = unix_now();
        let mut changed = false;
        for app in &apps {
            changed |= self.apps.rules.upsert(app, direction, preset, now);
        }
        if changed {
            self.save_app_rules();
            self.app_presets_changed();
        }
        Ok(NamedAppRule {
            apps,
            unseen,
            held: false,
            changed,
        })
    }

    /// Hold every preset [`App::set_named_app_preset`] is given for an application FxSound does
    /// not remember until the engine has said which applications play and record: what a cold
    /// start does before it runs the command line's `--app-preset` and `--app-input-preset`,
    /// since those run before any stream is known. An application that plays now under a name the
    /// store does not know — a Firefox started before FxSound, whose program is `firefox-bin`,
    /// named `Firefox` — is then reached through its streams, as a running FxSound reaches it,
    /// rather than given a rule that guesses at its identifiers ([`unseen_key`]).
    ///
    /// The wait ends with the engine's first report of the streams, or [`STREAMS_PATIENCE`] after
    /// this call, since nothing is reported while nothing plays, or on the way out; what was held
    /// is chosen then, and the log says what became of it. Nothing is held without an engine,
    /// which reports nothing.
    pub fn hold_app_presets(&mut self) {
        if self.engine.is_some() {
            self.apps.holding = Some(Instant::now() + STREAMS_PATIENCE);
        }
    }

    /// The wait [`App::hold_app_presets`] began is over: choose what was held, in the order it was
    /// named.
    fn release_held_app_presets(&mut self) {
        if self.apps.holding.take().is_none() {
            return;
        }
        for (text, direction, preset) in std::mem::take(&mut self.apps.held) {
            match self.set_named_app_preset(&text, direction, Some(&preset)) {
                Ok(done) if done.unseen => log::info!(
                    "no application FxSound knows or hears is called {text:?}: {preset} is kept \
                     for {}",
                    done.apps
                        .first()
                        .map_or_else(|| format!("{text:?}"), unseen_description)
                ),
                Ok(done) => log::info!(
                    "{preset} chosen for {}",
                    done.apps
                        .iter()
                        .map(|app| app.display().to_owned())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Err(refusal) => log::warn!("{text}={preset} is not chosen: {refusal}"),
            }
        }
    }

    /// Choose the preset the application `app` names runs through in `direction` — `None` or a
    /// blank name to follow the lane's preset — and say so to the engine at once when the
    /// application is running. Returns whether the store changed; it is written at once.
    ///
    /// The rule changed is the application's own ([`AppRules::upsert`]): a rule written for every
    /// program of that name stays as it is, and one of the application's own is added beside it.
    ///
    /// # Errors
    ///
    /// A key with no identifier, and a preset `direction`'s store does not have, are refused and
    /// change nothing.
    pub fn set_app_preset(
        &mut self,
        app: &AppKey,
        direction: DeviceDirection,
        preset: Option<&str>,
    ) -> Result<bool, AppRuleRefusal> {
        if app.is_empty() {
            return Err(AppRuleRefusal::NoApplication);
        }
        let preset = preset.map_or("", str::trim);
        if !preset.is_empty() && !self.lane_has_preset(direction, preset) {
            return Err(AppRuleRefusal::UnknownPreset {
                direction,
                name: preset.to_owned(),
            });
        }
        let changed = self.apps.rules.upsert(app, direction, preset, unix_now());
        if changed {
            self.save_app_rules();
            self.app_presets_changed();
        }
        Ok(changed)
    }

    /// Forget the rule for the application `app` names — the ✕ on its row. Returns whether one
    /// was forgotten; the store is written at once.
    ///
    /// An application that is playing or recording now is still one FxSound has seen, so it is
    /// remembered again at once, following both lanes: forgetting a running application takes its
    /// presets away, and it stays in the list as it came in.
    pub fn forget_app(&mut self, app: &AppKey) -> bool {
        if !self.apps.rules.forget(app) {
            return false;
        }
        let now = unix_now();
        for stream in &self.apps.streams {
            self.apps.rules.seen(&stream.app, now);
        }
        self.save_app_rules();
        self.app_presets_changed();
        true
    }

    /// Read the store from `path` and write it back there, instead of keeping it in memory — the
    /// tests' way to a store on disk that is not the user's.
    #[doc(hidden)]
    pub fn use_app_rules_file_for_tests(&mut self, path: PathBuf) {
        self.apps = AppPresets::load_from(path);
        self.app_presets_changed();
    }
}

// =============================================================================================
// The controller's side: streams in, routes out
// =============================================================================================

impl App {
    /// The engine's list of every application stream, in full ([`AudioToUi::AppStreams`]).
    ///
    /// Every application in it that the store does not know is remembered, following both lanes;
    /// one it knows is marked seen now, and written a while later ([`SEEN_SAVE_DELAY`]). What the
    /// engine says it moved is said on the event stream, and a rule of a running application that
    /// names a preset that cannot run is said once a session. The routes are worked out again only
    /// when the rules changed — a new application's rule may outrank one that names a preset — and
    /// never because a stream came or went: the engine has every rule already. The first report
    /// ends a cold start's wait for the streams ([`App::hold_app_presets`]).
    ///
    /// [`AudioToUi::AppStreams`]: fxsound_core::AudioToUi::AppStreams
    pub(super) fn adopt_app_streams(&mut self, streams: Vec<AppStream>) {
        let now = unix_now();
        let mut new_application = false;
        let mut seen_again = false;
        for stream in &streams {
            self.apps
                .used
                .insert((stream.app.clone(), stream.direction));
            let known = self.apps.rules.rule(&stream.app).is_some();
            if self.apps.rules.seen(&stream.app, now) {
                if known {
                    seen_again = true;
                } else {
                    new_application = true;
                }
            }
        }
        self.apps.streams = streams;
        self.note_app_routes();
        if new_application {
            self.save_app_rules();
            self.resolve_app_routes();
            self.refresh_app_routes();
        } else if seen_again {
            self.apps
                .save_due
                .get_or_insert_with(|| Instant::now() + SEEN_SAVE_DELAY);
        }
        self.release_held_app_presets();
        self.say_app_route_trouble();
    }

    /// A preset store changed — a preset saved, imported, reset or written by the calibration —
    /// or the rules did: every rule's preset is read again and the routes that follow sent if they
    /// changed.
    pub(super) fn app_presets_changed(&mut self) {
        self.resolve_app_routes();
        self.refresh_app_routes();
        self.say_app_route_trouble();
    }

    /// `lane`'s preset `old` is called `new` now: every rule that named it follows it there.
    pub(super) fn app_preset_renamed(&mut self, lane: DeviceDirection, old: &str, new: &str) {
        if self.apps.rules.rename_preset(lane, old, new) > 0 {
            self.save_app_rules();
        }
        self.app_presets_changed();
    }

    /// `lane`'s preset `name` was deleted: every rule that named it follows the lane's preset from
    /// now on. Returns whether any did, for the caller's one notice ([`followed_notice`]).
    ///
    /// A user preset that shadowed a factory one of the same name leaves that one in its place:
    /// the rules still name a preset, and run it.
    pub(super) fn app_preset_deleted(&mut self, lane: DeviceDirection, name: &str) -> bool {
        let followed =
            !self.lane_has_preset(lane, name) && self.apps.rules.rename_preset(lane, name, "") > 0;
        if followed {
            self.save_app_rules();
        }
        self.app_presets_changed();
        followed
    }

    /// Make the routes from what is resolved and the levels of the moment, and send them when
    /// they differ from what the engine was last given. Cheap when nothing changed, since it runs
    /// after every snapshot the window publishes ([`App::sync_params_from_state`]): what was sent
    /// is compared entry by entry, and a new set is built only when one differs.
    ///
    /// They go in the order they were resolved in — lane by lane, each lane in the store's order —
    /// since that order breaks a tie between two rules that match a stream as strongly, for the
    /// engine as for the store. A rule that follows the lane carries an empty preset and a snapshot
    /// of its lane's kind that nothing runs.
    pub(super) fn refresh_app_routes(&mut self) {
        let unchanged = self.apps.resolved.len() == self.apps.sent.len()
            && self
                .apps
                .resolved
                .iter()
                .zip(&self.apps.sent)
                .all(|(resolved, sent)| {
                    let (params, chain) = self.route_payload(resolved);
                    sent.direction == resolved.direction
                        && sent.app == resolved.app
                        && sent.preset == resolved.preset
                        && sent.params == params
                        && sent.chain == chain
                });
        if unchanged {
            return;
        }
        let routes: Vec<AppRoute> = self
            .apps
            .resolved
            .iter()
            .map(|resolved| {
                let (params, chain) = self.route_payload(resolved);
                AppRoute {
                    direction: resolved.direction,
                    app: resolved.app.clone(),
                    preset: resolved.preset.clone(),
                    params,
                    chain: chain.to_owned(),
                }
            })
            .collect();
        self.apps.sent.clone_from(&routes);
        self.send(UiToAudio::SetAppRoutes(routes));
    }

    /// The parameters and the voice chain of one resolved rule, with the levels of the moment.
    fn route_payload<'a>(&self, resolved: &'a Resolved) -> (RouteParams, &'a str) {
        match &resolved.source {
            Some(Source::Music(preset)) => {
                (RouteParams::Output(self.music_route_params(preset)), "")
            }
            Some(Source::Voice(preset)) => (
                RouteParams::Input(self.voice_route_params(preset)),
                preset.chain.as_str(),
            ),
            None => (
                match resolved.direction {
                    DeviceDirection::Output => RouteParams::Output(DspParams::default()),
                    DeviceDirection::Input => RouteParams::Input(InputDspParams::default()),
                },
                "",
            ),
        }
    }

    /// When the store next has something to do: be written, or stop waiting for the streams
    /// ([`App::hold_app_presets`]).
    pub(super) fn app_rules_due(&self) -> Option<Instant> {
        self.apps
            .save_due
            .into_iter()
            .chain(self.apps.holding)
            .min()
    }

    /// Do what is due by `now`: stop waiting for the streams, and write the store.
    pub(super) fn app_rules_tick(&mut self, now: Instant) {
        if self.apps.holding.is_some_and(|until| until <= now) {
            self.release_held_app_presets();
        }
        if self.apps.save_due.is_some_and(|due| due <= now) {
            self.save_app_rules();
        }
    }

    /// On the way out: what was held is chosen, what is running now was seen now, and whatever is
    /// unwritten is written — a write that failed earlier included.
    pub(super) fn save_app_rules_on_exit(&mut self) {
        self.release_held_app_presets();
        let now = unix_now();
        let mut unwritten = self.apps.save_due.is_some();
        for stream in &self.apps.streams {
            unwritten |= self.apps.rules.seen(&stream.app, now);
        }
        if unwritten {
            self.save_app_rules();
        }
    }

    /// Write the store now. The choices are in force for this run whether or not it is written; a
    /// write that fails leaves the store due [`SAVE_RETRY_DELAY`] later, so the timer tries again
    /// and the way out does too, and a choice made while the disk was full is not lost to a
    /// restart that nothing else wrote before. The first failure is a warning, the ones after it
    /// are not, and the write that succeeds again says so.
    fn save_app_rules(&mut self) {
        self.apps.save_due = None;
        let Some(path) = &self.apps.path else {
            return;
        };
        match self.apps.rules.save_to(path) {
            Ok(()) => {
                if std::mem::take(&mut self.apps.save_failing) {
                    log::info!("saved {} after all", path.display());
                }
            }
            Err(err) => {
                if std::mem::replace(&mut self.apps.save_failing, true) {
                    log::debug!("still could not save {}: {err}", path.display());
                } else {
                    log::warn!(
                        "could not save {}: {err}; trying again every {} s",
                        path.display(),
                        SAVE_RETRY_DELAY.as_secs()
                    );
                }
                self.apps.save_due = Some(Instant::now() + SAVE_RETRY_DELAY);
            }
        }
    }

    /// Work out what the engine is to know of the rules ([`AppPresets::resolved`]), reading each
    /// preset a rule names once.
    ///
    /// Every rule that names a preset its lane's store has and that loads goes, whether its
    /// application runs or not. A rule that follows the lane — by choice, or because the preset it
    /// names is not there or does not load — goes too, with no preset, where it could outrank one
    /// of those ([`AppKey::may_outrank`]): left out, a stream it answers for would be matched by
    /// the more general rule instead. The rest are left out, so the set stays as small as the
    /// presets chosen, however many applications the store remembers.
    fn resolve_app_routes(&mut self) {
        let mut loaded: Vec<(DeviceDirection, &str, Option<Source>)> = Vec::new();
        let mut unloadable: Vec<(DeviceDirection, String)> = Vec::new();
        let mut resolved = Vec::new();
        for lane in DeviceDirection::ALL {
            // Per rule, in the store's order: the preset it runs on this lane, `None` to follow.
            let mut runs: Vec<(&AppRule, Option<(&str, Source)>)> = Vec::new();
            for rule in &self.apps.rules.apps {
                let name = rule.preset(lane);
                let source = if !rule.has_preset(lane) || !self.lane_has_preset(lane, name) {
                    None
                } else if let Some((.., source)) = loaded
                    .iter()
                    .find(|(direction, preset, _)| *direction == lane && *preset == name)
                {
                    source.clone()
                } else {
                    let source = match self.load_route_preset(lane, name) {
                        Ok(source) => Some(source),
                        Err(err) => {
                            log::warn!("{name} could not be loaded for an application: {err}");
                            unloadable.push((lane, name.to_owned()));
                            None
                        }
                    };
                    loaded.push((lane, name, source.clone()));
                    source
                };
                runs.push((rule, source.map(|source| (name, source))));
            }
            let needed: Vec<bool> = runs
                .iter()
                .map(|(rule, preset)| {
                    preset.is_some()
                        || runs.iter().any(|(other, preset)| {
                            preset.is_some() && rule.key.may_outrank(&other.key)
                        })
                })
                .collect();
            for ((rule, preset), needed) in runs.into_iter().zip(needed) {
                if !needed {
                    continue;
                }
                let (preset, source) = match preset {
                    Some((name, source)) => (name.to_owned(), Some(source)),
                    None => (String::new(), None),
                };
                resolved.push(Resolved {
                    direction: lane,
                    app: rule.key.clone(),
                    preset,
                    source,
                });
            }
        }
        self.apps.resolved = resolved;
        self.apps.unloadable = unloadable;
    }

    /// Say, once a session per lane and preset, that an application playing or recording now has
    /// a rule naming a preset it cannot run — one that is not there, or that does not load — and
    /// follows the lane instead.
    fn say_app_route_trouble(&mut self) {
        let mut trouble: Vec<(DeviceDirection, String, String)> = Vec::new();
        for stream in &self.apps.streams {
            let lane = stream.direction;
            let notice = match self.app_preset(&stream.app, lane) {
                AppPreset::Missing(name) => (name, followed_notice(name)),
                AppPreset::Preset(name)
                    if self
                        .apps
                        .unloadable
                        .iter()
                        .any(|(direction, preset)| *direction == lane && preset == name) =>
                {
                    (name, tr_args("Could not load %s", &[name]))
                }
                AppPreset::Preset(_) | AppPreset::Follow => continue,
            };
            trouble.push((lane, notice.0.to_owned(), notice.1));
        }
        for (lane, preset, notice) in trouble {
            if self.apps.said.insert((lane, preset)) {
                self.raise_notice(notice);
            }
        }
    }

    /// `lane`'s preset `name` as last saved.
    fn load_route_preset(&self, lane: DeviceDirection, name: &str) -> Result<Source, String> {
        match lane {
            DeviceDirection::Output => self
                .presets
                .load_saved(name)
                .map(Source::Music)
                .map_err(|err| err.to_string()),
            DeviceDirection::Input => self
                .voice_presets
                .load_saved(name)
                .map(Source::Voice)
                .map_err(|err| err.to_string()),
        }
    }

    /// A music preset as a playback route runs it: read into controls on the ladder of the user's
    /// band count, with the speakers' levels, as the output lane's own snapshot is built.
    ///
    /// The ladder is the engine's own for the band count. The lane fits a preset of another count
    /// onto the centres its window holds at that moment, which are the same ones unless the
    /// preset showing moved a band's frequency; a route does not hang on what the window happens
    /// to show.
    fn music_route_params(&self, preset: &Preset) -> DspParams {
        let count = (self.settings.num_bands as usize).clamp(1, fxsound_core::eq::MAX_BANDS);
        let MusicControls {
            effects,
            eq_on,
            eq_bands,
        } = music_controls(preset, &ladder(count));
        let mut params = DspParams::default();
        write_music_params(
            &mut params,
            &effects,
            eq_on,
            &eq_bands,
            MusicLevels::of(&self.settings),
        );
        params.power = self.state.power;
        params.mute = self.sleeping;
        params
    }

    /// A voice preset as a recording route runs it: its own parameters with the Settings pane's
    /// microphone settings written over them, as the input lane's own snapshot is built.
    fn voice_route_params(&self, preset: &InputPreset) -> InputDspParams {
        let mut params = preset.to_params();
        let voicing = PresetVoicing::of(&params);
        apply_microphone_settings(&mut params, &voicing, &self.settings);
        params.power = self.state.power;
        params.mute = self.sleeping;
        params
    }

    /// Say, for every application whose streams the engine has moved onto a route or off one
    /// since its last report, where they are now ([`AppEvent::AppRouted`]). An application that
    /// has gone while routed is back on nothing, and says so the same way.
    fn note_app_routes(&mut self) {
        let mut now: Vec<(DeviceDirection, AppKey, String)> = Vec::new();
        for stream in &self.apps.streams {
            if let Some(preset) = &stream.route
                && !now
                    .iter()
                    .any(|(direction, app, _)| *direction == stream.direction && *app == stream.app)
            {
                now.push((stream.direction, stream.app.clone(), preset.clone()));
            }
        }
        self.state.routed_apps = now
            .iter()
            .map(|(direction, app, preset)| RoutedApp {
                direction: *direction,
                name: app.display().to_owned(),
                preset: preset.clone(),
            })
            .collect();
        let before = std::mem::replace(&mut self.apps.routed, now);
        for (direction, app, preset) in &self.apps.routed {
            let unchanged = before.iter().any(|(was_direction, was_app, was)| {
                was_direction == direction && was_app == app && was == preset
            });
            if !unchanged {
                self.events.push(AppEvent::AppRouted {
                    app: app.clone(),
                    direction: *direction,
                    preset: Some(preset.clone()),
                });
            }
        }
        for (direction, app, _) in before {
            let still =
                self.apps.routed.iter().any(|(now_direction, now_app, _)| {
                    *now_direction == direction && *now_app == app
                });
            if !still {
                self.events.push(AppEvent::AppRouted {
                    app,
                    direction,
                    preset: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
