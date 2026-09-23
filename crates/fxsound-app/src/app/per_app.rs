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
//! - the **routes** that follow from the two ([`UiToAudio::SetAppRoutes`]): one for every running
//!   application whose rule names a preset of its own that its lane's preset store has.
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
//! The whole set goes to the engine whenever it changes, and only then: a rule changed, the
//! streams changed, a preset a rule names was saved, renamed or deleted, or something every chain
//! shares moved. A preset renamed in the window carries every rule to its new name; one deleted
//! there returns every rule that named it to following the lane, and the window says so once. A
//! rule naming a preset the store does not have — a hand edit, a file removed behind FxSound's
//! back — follows the lane without being rewritten, so the preset coming back brings the route
//! back; the window says so once a session.
//!
//! `app_routed` on the event stream is the engine's word, not the rules': it says where the engine
//! has actually moved an application ([`AppStream::route`]), so a route that could not be made —
//! more presets than a lane runs at once — is never announced as though it had been.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fxsound_core::apps::unix_now;
use fxsound_core::messages::{AppRoute, AppStream, DspParams, InputDspParams, RouteParams};
use fxsound_core::{AppKey, AppPreset, AppRules, DeviceDirection, Preset, UiToAudio};
use fxsound_preset::input::InputPreset;

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

/// The controller's per-application state.
#[derive(Debug, Default)]
pub(super) struct AppPresets {
    /// What the user chose, and every application seen.
    rules: AppRules,
    /// Where the store is written; `None` keeps it in memory — a test, or a run that must not
    /// touch the user's files, as `App::persist` keeps the settings.
    path: Option<PathBuf>,
    /// When the store is due to be written: at once for a change the user made or a new
    /// application, [`SEEN_SAVE_DELAY`] after one seen again. `None` while nothing is unwritten.
    save_due: Option<Instant>,
    /// Every application stream the engine last reported, both lanes, in its order.
    streams: Vec<AppStream>,
    /// The running applications that have a preset of their own, with that preset as loaded:
    /// rebuilt when the streams, the rules or the preset stores change, and made into routes
    /// with the levels of the moment whenever those move ([`App::refresh_app_routes`]).
    resolved: Vec<Resolved>,
    /// The routes the engine was last given, in [`route_order`].
    sent: Vec<AppRoute>,
    /// The presets, per lane, a rule named that could not be run — not there, or not readable —
    /// that the window has already said so about this session.
    said: HashSet<(DeviceDirection, String)>,
    /// Per lane and application, the route the engine last reported its streams on, in the order
    /// the engine listed them: what `app_routed` has said.
    routed: Vec<(DeviceDirection, AppKey, String)>,
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

/// A running application with a preset of its own, and that preset as its lane's store has it.
#[derive(Debug, Clone)]
struct Resolved {
    direction: DeviceDirection,
    app: AppKey,
    preset: String,
    source: Source,
}

/// A preset as its file says, before the levels of the moment are applied.
#[derive(Debug, Clone)]
enum Source {
    Music(Preset),
    Voice(InputPreset),
}

/// Why [`App::set_app_preset`] changed nothing. The text is what the command line prints and a
/// D-Bus caller is answered with: English, as every refusal of the command path is.
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

/// The order the routes are sent in, so that the same set is the same message whatever order the
/// engine listed the streams in: outputs first, then by preset, then by application.
fn route_order(a: &AppRoute, b: &AppRoute) -> std::cmp::Ordering {
    let key = |route: &AppRoute| {
        (
            lane_index(route.direction),
            route.preset.clone(),
            route.app.name.clone(),
            route.app.binary.clone(),
            route.app.flatpak.clone(),
        )
    };
    key(a).cmp(&key(b))
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
    /// one it knows is marked seen now, and written a while later ([`SEEN_SAVE_DELAY`]). The
    /// routes are worked out again for the applications running now, and sent if they changed;
    /// what the engine says it moved is said on the event stream.
    ///
    /// [`AudioToUi::AppStreams`]: fxsound_core::AudioToUi::AppStreams
    pub(super) fn adopt_app_streams(&mut self, streams: Vec<AppStream>) {
        let now = unix_now();
        let mut new_application = false;
        let mut seen_again = false;
        for stream in &streams {
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
        self.app_presets_changed();
        if new_application {
            self.save_app_rules();
        } else if seen_again {
            self.apps
                .save_due
                .get_or_insert_with(|| Instant::now() + SEEN_SAVE_DELAY);
        }
    }

    /// A preset store changed — a preset saved, imported, reset or written by the calibration —
    /// or the rules did: every running application's preset is read again and the routes that
    /// follow sent if they changed.
    pub(super) fn app_presets_changed(&mut self) {
        self.resolve_app_routes();
        self.refresh_app_routes();
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
    /// they differ from what the engine was last given. Cheap when nothing is routed, since it runs
    /// after every snapshot the window publishes ([`App::sync_params_from_state`]).
    pub(super) fn refresh_app_routes(&mut self) {
        let mut routes: Vec<AppRoute> = self
            .apps
            .resolved
            .iter()
            .map(|resolved| {
                let (params, chain) = match &resolved.source {
                    Source::Music(preset) => (
                        RouteParams::Output(self.music_route_params(preset)),
                        String::new(),
                    ),
                    Source::Voice(preset) => (
                        RouteParams::Input(self.voice_route_params(preset)),
                        preset.chain.clone(),
                    ),
                };
                AppRoute {
                    direction: resolved.direction,
                    app: resolved.app.clone(),
                    preset: resolved.preset.clone(),
                    params,
                    chain,
                }
            })
            .collect();
        routes.sort_by(route_order);
        if routes != self.apps.sent {
            self.apps.sent.clone_from(&routes);
            self.send(UiToAudio::SetAppRoutes(routes));
        }
    }

    /// When the store is next due to be written.
    pub(super) const fn app_rules_save_due(&self) -> Option<Instant> {
        self.apps.save_due
    }

    /// Write the store if it is due by `now`.
    pub(super) fn save_app_rules_if_due(&mut self, now: Instant) {
        if self.apps.save_due.is_some_and(|due| due <= now) {
            self.save_app_rules();
        }
    }

    /// On the way out: what is running now was seen now, and whatever is unwritten is written.
    pub(super) fn save_app_rules_on_exit(&mut self) {
        let now = unix_now();
        let mut unwritten = self.apps.save_due.is_some();
        for stream in &self.apps.streams {
            unwritten |= self.apps.rules.seen(&stream.app, now);
        }
        if unwritten {
            self.save_app_rules();
        }
    }

    /// Write the store now. A failure is logged: the choices are still in force for this run, and
    /// the next change tries again.
    fn save_app_rules(&mut self) {
        self.apps.save_due = None;
        if let Some(path) = &self.apps.path
            && let Err(err) = self.apps.rules.save_to(path)
        {
            log::warn!("could not save {}: {err}", path.display());
        }
    }

    /// Work out which running applications have a preset of their own, and read each such preset
    /// once. A rule naming a preset that is not there, or that does not load, leaves its
    /// application on the lane, and the window says so once a session per preset.
    fn resolve_app_routes(&mut self) {
        // Once per application and lane, however many streams it has open.
        let mut wanted: Vec<(DeviceDirection, &AppKey, AppPreset<'_>)> = Vec::new();
        for stream in &self.apps.streams {
            let lane = stream.direction;
            if wanted
                .iter()
                .any(|(direction, app, _)| *direction == lane && **app == stream.app)
            {
                continue;
            }
            let preset = self
                .apps
                .rules
                .resolve(&stream.app, lane, |name| self.lane_has_preset(lane, name));
            wanted.push((lane, &stream.app, preset));
        }

        let mut loaded: Vec<(DeviceDirection, &str, Result<Source, String>)> = Vec::new();
        let mut resolved = Vec::new();
        let mut trouble: Vec<(DeviceDirection, String, String)> = Vec::new();
        for (lane, app, preset) in wanted {
            let name = match preset {
                AppPreset::Follow => continue,
                AppPreset::Missing(name) => {
                    trouble.push((lane, name.to_owned(), followed_notice(name)));
                    continue;
                }
                AppPreset::Preset(name) => name,
            };
            let source = match loaded
                .iter()
                .find(|(direction, preset, _)| *direction == lane && *preset == name)
            {
                Some((.., source)) => source.clone(),
                None => {
                    let source = self.load_route_preset(lane, name);
                    loaded.push((lane, name, source.clone()));
                    source
                }
            };
            match source {
                Ok(source) => resolved.push(Resolved {
                    direction: lane,
                    app: app.clone(),
                    preset: name.to_owned(),
                    source,
                }),
                Err(err) => {
                    log::warn!("{name} could not be loaded for {}: {err}", app.display());
                    trouble.push((lane, name.to_owned(), tr_args("Could not load %s", &[name])));
                }
            }
        }
        self.apps.resolved = resolved;
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
