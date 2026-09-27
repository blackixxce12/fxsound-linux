//! The controller's side of per-application presets, driven end to end through a stand-in engine:
//! rules in, the engine's streams in, `SetAppRoutes` out.

use super::*;
use crate::app::voice_store_for_tests;
use crate::audio_link::FakeEngine;
use fxsound_core::messages::AudioToUi;
use fxsound_core::{DenoiseLevel, Effect, EqBand, NoiseSuppressionOverride, Settings, scale};
use fxsound_preset::PresetStore;
use fxsound_ui::UiAction;
use std::path::Path;

const OUT: DeviceDirection = DeviceDirection::Output;
const IN: DeviceDirection = DeviceDirection::Input;

fn key(binary: &str, name: &str, flatpak: &str) -> AppKey {
    AppKey {
        binary: binary.to_owned(),
        name: name.to_owned(),
        flatpak: flatpak.to_owned(),
    }
}

/// The three applications of the example the feature was asked for.
fn battlefield() -> AppKey {
    key("bf6.exe", "Battlefield 6", "")
}

fn brave() -> AppKey {
    key("brave", "Brave", "")
}

fn discord() -> AppKey {
    key("Discord", "Discord", "com.discordapp.Discord")
}

fn stream(id: u32, direction: DeviceDirection, app: &AppKey) -> AppStream {
    AppStream {
        id,
        direction,
        app: app.clone(),
        route: None,
    }
}

/// `Gaming`'s curve on the ten default bands: a lift at the bottom, a cut at the top.
fn gaming_curve() -> Vec<EqBand> {
    let mut bands = fxsound_core::eq::default_bands();
    bands[0].boost_db = 5.0;
    bands[9].boost_db = -3.0;
    bands
}

/// The speakers' presets: a flat `Music`, a bass-heavy `Gaming` on ten bands, and `Volume Boost`
/// — Dynamic Boost past the slider's dead top, on a five-band curve the user's ten bands have to
/// be fitted from.
fn music_presets(dir: &Path) -> PresetStore {
    let factory = dir.join("factory");
    std::fs::create_dir_all(&factory).expect("factory directory");
    let mut gaming = Preset {
        name: "Gaming".to_owned(),
        eq_bands: gaming_curve(),
        ..Preset::default()
    };
    gaming.set_effect(Effect::Bass, 0.6);
    let mut boost = Preset {
        name: "Volume Boost".to_owned(),
        eq_bands: vec![
            EqBand::new(62.5, 4.0),
            EqBand::new(250.0, 1.0),
            EqBand::new(1000.0, 0.0),
            EqBand::new(4000.0, 2.0),
            EqBand::new(16000.0, 3.0),
        ],
        ..Preset::default()
    };
    boost.set_effect(Effect::DynamicBoost, 0.9);
    let music = Preset {
        name: "Music".to_owned(),
        ..Preset::default()
    };
    for preset in [music, gaming, boost] {
        fxsound_preset::save(&preset, &factory.join(format!("{}.fac", preset.name)))
            .expect("write");
    }
    let mut store = PresetStore::with_dirs(vec![factory], dir.join("user"));
    store.rescan();
    store
}

/// The microphone's: `Clean`, and `Headset` with 4 dB of makeup on the podcast chain.
fn voice_presets(dir: &Path) -> fxsound_preset::InputPresetStore {
    voice_store_for_tests(
        &[
            InputPreset {
                name: "Clean".to_owned(),
                ..InputPreset::default()
            },
            InputPreset {
                name: "Headset".to_owned(),
                makeup_db: 4.0,
                chain: "podcast".to_owned(),
                ..InputPreset::default()
            },
        ],
        &dir.join("voice-factory"),
        dir.join("user").join("Input"),
    )
}

/// An app started the way the real one starts, against a stand-in engine: the speakers on
/// `Music` with a 2 dB master gain, a balance and some levelling, the microphone on `Clean`, both
/// lanes on, the window on the speakers. What start-up said is taken already.
fn started() -> (App, FakeEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("scratch directory");
    let mut settings = Settings::default();
    settings.output_preset = "Music".to_owned();
    settings.input_preset = "Clean".to_owned();
    settings.master_gain = 2.0;
    settings.balance = -1.5;
    settings.volume_leveling = 1.0;
    settings.set_lane_enabled(IN, true);
    let engine = FakeEngine::new();
    let mut app = App::start_for_tests(
        settings,
        music_presets(dir.path()),
        voice_presets(dir.path()),
        &engine,
    );
    let _ = engine.take_sent();
    let _ = engine.take_events();
    let _ = app.drain_events();
    (app, engine, dir)
}

/// The engine reports these streams, and the app takes the report.
fn play(app: &mut App, engine: &FakeEngine, streams: Vec<AppStream>) {
    engine.feed(AudioToUi::AppStreams(streams));
    app.poll_audio();
}

/// Every `SetAppRoutes` the engine was sent since the last look, in order.
fn routes_sent(engine: &FakeEngine) -> Vec<Vec<AppRoute>> {
    engine
        .take_sent()
        .into_iter()
        .filter_map(|message| match message {
            UiToAudio::SetAppRoutes(routes) => Some(routes),
            _ => None,
        })
        .collect()
}

/// The one `SetAppRoutes` the engine was sent since the last look.
fn the_routes_sent(engine: &FakeEngine) -> Vec<AppRoute> {
    let mut sent = routes_sent(engine);
    assert_eq!(sent.len(), 1, "one SetAppRoutes: {sent:?}");
    sent.remove(0)
}

/// The last `SetAppRoutes` the engine was sent since the last look.
fn the_last_routes_sent(engine: &FakeEngine) -> Vec<AppRoute> {
    routes_sent(engine)
        .pop()
        .expect("at least one SetAppRoutes")
}

/// The entries of `routes` that name a preset, as `(lane, application, preset)`, in their order.
fn presets_of(routes: &[AppRoute]) -> Vec<(DeviceDirection, AppKey, &str)> {
    routes
        .iter()
        .filter(|route| !route.preset.is_empty())
        .map(|route| (route.direction, route.app.clone(), route.preset.as_str()))
        .collect()
}

/// The preset the engine runs a stream of `app` through in `direction`, given `routes`: the one
/// the most specific entry of that lane names, as `Rules::preset_for` in `fxsound-audio` picks it
/// — `None` for an entry that follows the lane, and for none at all.
fn engine_choice<'a>(
    routes: &'a [AppRoute],
    direction: DeviceDirection,
    app: &AppKey,
) -> Option<&'a str> {
    let lane: Vec<&AppRoute> = routes
        .iter()
        .filter(|route| route.direction == direction)
        .collect();
    let best = app.best_match(lane.iter().map(|route| &route.app))?;
    Some(lane[best].preset.as_str()).filter(|preset| !preset.is_empty())
}

/// What the store says `app` runs in `direction`, as the engine is to run it: `None` to follow.
fn store_choice(app: &App, direction: DeviceDirection, key: &AppKey) -> Option<String> {
    match app.app_preset(key, direction) {
        AppPreset::Preset(name) => Some(name.to_owned()),
        AppPreset::Follow | AppPreset::Missing(_) => None,
    }
}

fn route_of<'a>(routes: &'a [AppRoute], app: &AppKey, direction: DeviceDirection) -> &'a AppRoute {
    routes
        .iter()
        .find(|route| route.app == *app && route.direction == direction)
        .unwrap_or_else(|| panic!("a {direction:?} route for {app:?} in {routes:?}"))
}

fn output(route: &AppRoute) -> DspParams {
    match route.params {
        RouteParams::Output(params) => params,
        RouteParams::Input(_) => panic!("{route:?} carries a microphone's snapshot"),
    }
}

fn input(route: &AppRoute) -> InputDspParams {
    match route.params {
        RouteParams::Input(params) => params,
        RouteParams::Output(_) => panic!("{route:?} carries the speakers' snapshot"),
    }
}

/// Whether `route`'s snapshot has the power on: the effects run.
fn powered(route: &AppRoute) -> bool {
    match route.params {
        RouteParams::Output(params) => params.power,
        RouteParams::Input(params) => params.power,
    }
}

/// `route` with the power switched `power` in its snapshot.
fn with_power(mut route: AppRoute, power: bool) -> AppRoute {
    match &mut route.params {
        RouteParams::Output(params) => params.power = power,
        RouteParams::Input(params) => params.power = power,
    }
    route
}

/// How many times `text` was raised as a notice among `events`.
fn notices(events: &[AppEvent], text: &str) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, AppEvent::Notice { message } if message == text))
        .count()
}

/// Select `lane`'s preset `name` in the window, leaving the window on `lane`.
fn pick(app: &mut App, lane: DeviceDirection, name: &str) {
    app.handle(&[UiAction::SetEditDirection(lane)]);
    let at = app
        .state
        .presets
        .iter()
        .position(|preset| preset.name == name)
        .unwrap_or_else(|| panic!("{name} is listed"));
    app.handle(&[UiAction::SelectPreset(at)]);
}

/// `Raid`: a user preset of the speakers, saved from `Gaming` with a stronger bass — selected.
fn with_raid(app: &mut App) {
    pick(app, OUT, "Gaming");
    app.handle(&[
        UiAction::SetEffect(Effect::Bass, 9.0),
        UiAction::SavePresetAs("Raid".to_owned()),
    ]);
    assert_eq!(app.lane_preset(OUT), Some(("Raid", false)));
}

// ---- the example ------------------------------------------------------------------------------

#[test]
fn battlefield_brave_and_discord_each_run_their_own_preset_at_the_same_time() {
    let (mut app, engine, _dir) = started();
    for (app_key, direction, preset) in [
        (battlefield(), OUT, "Gaming"),
        (brave(), OUT, "Volume Boost"),
        (discord(), IN, "Headset"),
    ] {
        assert_eq!(
            app.set_app_preset(&app_key, direction, Some(preset)),
            Ok(true)
        );
    }
    // The engine has every rule before anything plays; it builds a route once a stream needs one.
    let routes = the_last_routes_sent(&engine);
    assert_eq!(
        presets_of(&routes),
        [
            (OUT, battlefield(), "Gaming"),
            (OUT, brave(), "Volume Boost"),
            (IN, discord(), "Headset"),
        ]
    );

    play(
        &mut app,
        &engine,
        vec![
            stream(40, OUT, &battlefield()),
            stream(41, OUT, &brave()),
            stream(42, IN, &discord()),
            // Discord plays too; its rule leaves the speakers to follow.
            stream(43, OUT, &discord()),
        ],
    );
    assert!(
        routes_sent(&engine).is_empty(),
        "streams that come change no rule"
    );
    assert_eq!(engine_choice(&routes, OUT, &discord()), None);

    let game = route_of(&routes, &battlefield(), OUT);
    assert_eq!(game.preset, "Gaming");
    assert!(game.chain.is_empty());
    let params = output(game);
    assert!(
        (params.effect(Effect::Bass) - 0.6).abs() < 0.01,
        "{params:?}"
    );
    assert_eq!(params.effect(Effect::DynamicBoost), 0.0);
    let (centres, gains) = params.bands();
    let curve = gaming_curve();
    assert_eq!(
        centres,
        curve.iter().map(|b| b.center_hz).collect::<Vec<_>>()
    );
    assert_eq!(gains, curve.iter().map(|b| b.boost_db).collect::<Vec<_>>());
    // The levels are the speakers', whichever preset runs.
    assert_eq!(params.master_gain_db, 2.0);
    assert_eq!(params.balance, -1.5);
    assert_eq!(params.volume_leveling_db, 1.0);
    assert!(params.power && !params.mute);

    let browser = output(route_of(&routes, &brave(), OUT));
    assert_eq!(
        browser.bands().0.len(),
        10,
        "a five-band preset is fitted onto the user's ten bands"
    );
    assert_eq!(
        browser.effect(Effect::DynamicBoost),
        scale::slider_to_value_for(Effect::DynamicBoost, scale::SLIDER_MAX),
        "Dynamic Boost past the slider's dead top runs at the top, as on the lane"
    );
    assert_eq!(browser.master_gain_db, 2.0);

    let call = route_of(&routes, &discord(), IN);
    assert_eq!(call.preset, "Headset");
    assert_eq!(call.chain, "podcast");
    let voice = input(call);
    assert_eq!(voice.makeup_db, 4.0);
    assert!(voice.power && !voice.mute);

    assert_eq!(app.app_routes(), routes.as_slice());
}

#[test]
fn a_route_runs_exactly_what_its_lane_runs_on_the_same_preset() {
    let (mut app, engine, _dir) = started();
    app.set_noise_suppression(NoiseSuppressionOverride::Strong);
    for (app_key, direction, preset) in [
        (battlefield(), OUT, "Volume Boost"),
        (discord(), IN, "Headset"),
    ] {
        app.set_app_preset(&app_key, direction, Some(preset))
            .expect("a preset of the lane");
    }
    let routes = the_last_routes_sent(&engine);

    pick(&mut app, OUT, "Volume Boost");
    assert_eq!(
        output(route_of(&routes, &battlefield(), OUT)),
        *app.params()
    );

    pick(&mut app, IN, "Headset");
    let call = route_of(&routes, &discord(), IN);
    assert_eq!(input(call), *app.input_params());
    assert_eq!(call.chain, app.input_chain());
    assert_eq!(input(call).denoise_level, DenoiseLevel::Strong);
}

#[test]
fn two_applications_on_one_preset_share_its_name_in_the_payload() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    app.set_app_preset(&brave(), OUT, Some("Gaming"))
        .expect("set");
    let routes = the_last_routes_sent(&engine);
    assert_eq!(routes.len(), 2);
    assert!(
        routes.iter().all(|route| route.preset == "Gaming"),
        "{routes:?}"
    );
    assert_eq!(routes[0].params, routes[1].params);
    let apps: Vec<&AppKey> = routes.iter().map(|route| &route.app).collect();
    assert!(apps.contains(&&battlefield()) && apps.contains(&&brave()));
}

// ---- follow -------------------------------------------------------------------------------------

#[test]
fn an_application_that_follows_its_lane_gets_no_route() {
    let (mut app, engine, _dir) = started();
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &battlefield()), stream(2, IN, &discord())],
    );
    assert!(
        routes_sent(&engine).is_empty(),
        "nothing has a preset of its own: the engine hears nothing"
    );
    assert!(app.app_routes().is_empty());

    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    let routes = the_routes_sent(&engine);
    assert_eq!(presets_of(&routes), [(OUT, battlefield(), "Gaming")]);
    assert_eq!(
        engine_choice(&routes, OUT, &discord()),
        None,
        "Discord still follows"
    );
    assert_eq!(
        engine_choice(&routes, IN, &battlefield()),
        None,
        "and nothing records through Gaming"
    );

    // Back to following: the route goes.
    assert_eq!(app.set_app_preset(&battlefield(), OUT, None), Ok(true));
    assert_eq!(the_routes_sent(&engine), Vec::<AppRoute>::new());
}

#[test]
fn a_rule_reaches_the_engine_whether_or_not_its_application_runs_and_its_quitting_changes_nothing()
{
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    let routes = the_routes_sent(&engine);
    assert_eq!(
        presets_of(&routes),
        [(OUT, battlefield(), "Gaming")],
        "the engine builds nothing until the game plays, and then needs no word from the app"
    );

    play(&mut app, &engine, vec![stream(7, OUT, &battlefield())]);
    assert!(routes_sent(&engine).is_empty());

    // It closes its stream between two levels, or quits: the engine keeps the route for a while,
    // and the next stream finds the rule where it was.
    play(&mut app, &engine, Vec::new());
    assert!(routes_sent(&engine).is_empty());
    assert_eq!(app.app_routes(), routes.as_slice());
    play(&mut app, &engine, vec![stream(8, OUT, &battlefield())]);
    assert!(routes_sent(&engine).is_empty());
}

#[test]
fn a_store_with_rules_reaches_the_engine_at_start_up() {
    let (mut app, engine, dir) = started();
    let file = dir.path().join("apps.toml");
    std::fs::write(
        &file,
        "[[app]]\nbinary = \"bf6.exe\"\nname = \"Battlefield 6\"\noutput_preset = \"Gaming\"\n\
         last_seen = 1\n",
    )
    .expect("write the store");
    app.use_app_rules_file_for_tests(file);
    assert_eq!(
        presets_of(&the_routes_sent(&engine)),
        [(OUT, battlefield(), "Gaming")]
    );
}

#[test]
fn a_rule_is_for_the_lane_it_names_only() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&discord(), IN, Some("Headset"))
        .expect("set");
    let routes = the_routes_sent(&engine);
    assert_eq!(presets_of(&routes), [(IN, discord(), "Headset")]);
    // Discord only plays: its microphone rule has nothing to route.
    play(&mut app, &engine, vec![stream(3, OUT, &discord())]);
    assert!(routes_sent(&engine).is_empty());
    assert_eq!(engine_choice(&routes, OUT, &discord()), None);
}

#[test]
fn an_application_with_many_streams_is_one_rule_and_the_engines_order_is_no_change() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    app.set_app_preset(&brave(), OUT, Some("Volume Boost"))
        .expect("set");
    assert_eq!(the_last_routes_sent(&engine).len(), 2);
    play(
        &mut app,
        &engine,
        vec![
            stream(1, OUT, &battlefield()),
            stream(2, OUT, &battlefield()),
            stream(3, OUT, &brave()),
        ],
    );
    assert!(routes_sent(&engine).is_empty());

    play(
        &mut app,
        &engine,
        vec![stream(3, OUT, &brave()), stream(1, OUT, &battlefield())],
    );
    assert!(
        routes_sent(&engine).is_empty(),
        "the same routes in another order are not news"
    );
}

#[test]
fn a_general_rule_reaches_the_engine_under_its_own_key_and_covers_every_application_it_names() {
    let (mut app, engine, _dir) = started();
    // Written from the command line with only a name.
    let by_name = key("", "Discord", "");
    app.set_app_preset(&by_name, IN, Some("Headset"))
        .expect("set");
    let routes = the_routes_sent(&engine);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].app, by_name, "the rule's own key");
    assert_eq!(engine_choice(&routes, IN, &discord()), Some("Headset"));
    play(&mut app, &engine, vec![stream(9, IN, &discord())]);
    assert!(routes_sent(&engine).is_empty());
    assert_eq!(
        app.app_rules().apps.len(),
        1,
        "the general rule covers the application: it is not added again"
    );
    assert!(app.app_is_running(&by_name));
}

#[test]
fn forgetting_a_running_application_takes_its_route_away_and_keeps_it_listed() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let _ = routes_sent(&engine);

    assert!(app.forget_app(&battlefield()));
    assert_eq!(the_routes_sent(&engine), Vec::<AppRoute>::new());
    let rule = app
        .app_rules()
        .rule(&battlefield())
        .expect("still remembered");
    assert!(!rule.has_preset(OUT) && !rule.has_preset(IN));
    assert!(app.app_is_running(&battlefield()));

    assert!(!app.forget_app(&brave()), "nothing to forget");
    assert!(!app.app_is_running(&brave()));
}

// ---- a rule that follows beside one that names a preset ----------------------------------------

fn native_firefox() -> AppKey {
    key("firefox", "Firefox", "")
}

fn flatpak_firefox() -> AppKey {
    key("firefox", "Firefox", "org.mozilla.firefox")
}

/// For each of `streams`, on both lanes: the engine, given what the app last sent, runs what the
/// store says.
fn assert_the_engine_runs_what_the_store_says(app: &App, streams: &[AppKey]) {
    for stream in streams {
        for direction in DeviceDirection::ALL {
            assert_eq!(
                engine_choice(app.app_routes(), direction, stream).map(str::to_owned),
                store_choice(app, direction, stream),
                "{stream:?} in {direction:?}, given {:?}",
                app.app_routes()
            );
        }
    }
}

#[test]
fn a_flatpak_that_follows_beside_the_native_rule_on_a_preset_stays_on_its_lane() {
    let (mut app, engine, _dir) = started();
    // The native Firefox plays, and gets Music of its own.
    play(&mut app, &engine, vec![stream(1, OUT, &native_firefox())]);
    app.set_app_preset(&native_firefox(), OUT, Some("Music"))
        .expect("set");
    // The Flatpak one starts: the native rule covers it by the program, so it has no row of its
    // own until the command line gives it a microphone preset, which copies Music along.
    play(
        &mut app,
        &engine,
        vec![
            stream(1, OUT, &native_firefox()),
            stream(2, OUT, &flatpak_firefox()),
            stream(3, IN, &flatpak_firefox()),
        ],
    );
    app.set_named_app_preset("org.mozilla.firefox", IN, Some("Headset"))
        .expect("set");
    assert_eq!(app.app_rules().apps.len(), 2);
    // Then "FxSound's preset" on its row's output.
    app.set_app_preset(&flatpak_firefox(), OUT, None)
        .expect("set");
    assert_eq!(app.app_preset(&flatpak_firefox(), OUT), AppPreset::Follow);

    let routes = the_last_routes_sent(&engine);
    assert_eq!(
        engine_choice(&routes, OUT, &flatpak_firefox()),
        None,
        "the native rule matches it by the program, but its own rule follows: {routes:?}"
    );
    assert_eq!(
        engine_choice(&routes, OUT, &native_firefox()),
        Some("Music")
    );
    assert_eq!(
        engine_choice(&routes, IN, &flatpak_firefox()),
        Some("Headset")
    );
    assert_eq!(
        engine_choice(&routes, IN, &native_firefox()),
        None,
        "the Flatpak's rule matches the native one by the program too"
    );
    assert_eq!(
        presets_of(&routes),
        [
            (OUT, native_firefox(), "Music"),
            (IN, flatpak_firefox(), "Headset"),
        ]
    );
    assert_eq!(
        routes.len(),
        4,
        "each beside a rule that follows: {routes:?}"
    );
}

#[test]
fn a_program_that_follows_beside_a_rule_for_its_name_on_a_preset_stays_on_its_lane() {
    let (mut app, engine, _dir) = started();
    let by_name = key("", "Game", "");
    let program = key("game.exe", "Game", "");
    app.set_app_preset(&by_name, OUT, Some("Gaming"))
        .expect("set");
    app.set_app_preset(&program, IN, Some("Headset"))
        .expect("a rule of its own, carrying Gaming along");
    app.set_app_preset(&program, OUT, None).expect("set");
    let _ = routes_sent(&engine);
    assert_the_engine_runs_what_the_store_says(
        &app,
        &[
            program.clone(),
            by_name,
            key("", "Game", ""),
            key("GAME.EXE", "", ""),
        ],
    );
    assert_eq!(engine_choice(app.app_routes(), OUT, &program), None);
}

#[test]
fn a_rule_naming_a_preset_that_is_not_there_keeps_its_application_off_the_general_rules_one() {
    let (mut app, _engine, dir) = started();
    let file = dir.path().join("apps.toml");
    std::fs::write(
        &file,
        "[[app]]\nbinary = \"firefox\"\nname = \"Firefox\"\noutput_preset = \"Music\"\n\
         last_seen = 2\n\n\
         [[app]]\nbinary = \"firefox\"\nname = \"Firefox\"\nflatpak = \"org.mozilla.firefox\"\n\
         output_preset = \"Deleted last week\"\nlast_seen = 1\n",
    )
    .expect("write the store");
    app.use_app_rules_file_for_tests(file);
    assert_eq!(
        app.app_preset(&flatpak_firefox(), OUT),
        AppPreset::Missing("Deleted last week")
    );
    assert_eq!(
        engine_choice(app.app_routes(), OUT, &flatpak_firefox()),
        None,
        "it follows the lane, as the window says"
    );
    assert_eq!(
        engine_choice(app.app_routes(), OUT, &native_firefox()),
        Some("Music")
    );
}

#[test]
fn the_engine_runs_for_every_application_what_the_store_says_however_the_rules_overlap() {
    let (mut app, _engine, dir) = started();
    let file = dir.path().join("apps.toml");
    // Rules that overlap every way the store matches: by id, by program in two cases and by path,
    // by name alone; some on a preset, some following, one on a preset that is gone.
    std::fs::write(
        &file,
        "[[app]]\nname = \"Discord\"\noutput_preset = \"Music\"\ninput_preset = \"Headset\"\n\
         last_seen = 9\n\n\
         [[app]]\nbinary = \"Discord\"\nname = \"Discord\"\n\
         flatpak = \"com.discordapp.Discord\"\nlast_seen = 8\n\n\
         [[app]]\nbinary = \"/opt/vesktop/VESKTOP\"\nname = \"Discord\"\n\
         input_preset = \"Clean\"\nlast_seen = 7\n\n\
         [[app]]\nbinary = \"bf6.exe\"\nname = \"Battlefield 6\"\n\
         output_preset = \"Gaming\"\nlast_seen = 6\n\n\
         [[app]]\nbinary = \"BF6.EXE\"\nname = \"Battlefield 6 (beta)\"\n\
         output_preset = \"Gone\"\nlast_seen = 5\n\n\
         [[app]]\nbinary = \"mpv\"\nname = \"mpv\"\nlast_seen = 4\n\n\
         [[app]]\nflatpak = \"com.spotify.Client\"\nname = \"Spotify\"\nlast_seen = 3\n",
    )
    .expect("write the store");
    app.use_app_rules_file_for_tests(file);
    let mut streams = Vec::new();
    for flatpak in [
        "",
        "com.discordapp.Discord",
        "com.spotify.Client",
        "org.other.App",
    ] {
        for binary in [
            "", "Discord", "discord", "vesktop", "bf6.exe", "mpv", "other",
        ] {
            for name in [
                "",
                "Discord",
                "Battlefield 6",
                "Battlefield 6 (beta)",
                "mpv",
                "Other",
            ] {
                streams.push(key(binary, name, flatpak));
            }
        }
    }
    assert_the_engine_runs_what_the_store_says(&app, &streams);
}

#[test]
fn a_rule_that_follows_reaches_the_engine_only_where_it_could_outrank_one_on_a_preset() {
    let (mut app, _engine, dir) = started();
    let file = dir.path().join("apps.toml");
    std::fs::write(
        &file,
        "[[app]]\nbinary = \"bf6.exe\"\nname = \"Battlefield 6\"\noutput_preset = \"Gaming\"\n\
         last_seen = 4\n\n\
         [[app]]\nbinary = \"mpv\"\nname = \"mpv\"\nlast_seen = 3\n\n\
         [[app]]\nname = \"Solitaire\"\nlast_seen = 2\n\n\
         [[app]]\nflatpak = \"com.usebottles.bottles\"\nname = \"Bottles\"\nlast_seen = 1\n",
    )
    .expect("write the store");
    app.use_app_rules_file_for_tests(file);
    let entries: Vec<(DeviceDirection, &str, &str)> = app
        .app_routes()
        .iter()
        .map(|route| (route.direction, route.app.display(), route.preset.as_str()))
        .collect();
    assert_eq!(
        entries,
        [
            (OUT, "Battlefield 6", "Gaming"),
            // A Flatpak can run the game under its own id, as Bottles runs Windows games; the
            // store gives such a stream to Bottles' rule, so the engine must too. mpv and a rule
            // for the name Solitaire cannot match any stream Battlefield's rule matches more
            // strongly, and nothing is on a preset in the microphone's lane.
            (OUT, "Bottles", ""),
        ]
    );
}

// ---- rules refused ------------------------------------------------------------------------------

#[test]
fn a_preset_the_lane_does_not_have_or_an_application_with_no_name_is_refused() {
    let (mut app, engine, _dir) = started();
    assert_eq!(
        app.set_app_preset(&battlefield(), OUT, Some("Headset")),
        Err(AppRuleRefusal::UnknownPreset {
            direction: OUT,
            name: "Headset".to_owned(),
        }),
        "a voice preset is not the speakers'"
    );
    assert_eq!(
        app.set_app_preset(&AppKey::default(), OUT, Some("Gaming")),
        Err(AppRuleRefusal::NoApplication)
    );
    assert!(app.app_rules().apps.is_empty());
    assert!(routes_sent(&engine).is_empty());
    assert_eq!(
        AppRuleRefusal::UnknownPreset {
            direction: IN,
            name: "Loud".to_owned(),
        }
        .to_string(),
        r#"no input preset is called "Loud""#
    );
}

// ---- presets saved, renamed, deleted ------------------------------------------------------------

#[test]
fn deleting_a_preset_returns_its_applications_to_following_and_says_so_once() {
    let (mut app, engine, dir) = started();
    let file = dir.path().join("apps.toml");
    app.use_app_rules_file_for_tests(file.clone());
    with_raid(&mut app);
    app.set_app_preset(&battlefield(), OUT, Some("Raid"))
        .expect("set");
    app.set_app_preset(&brave(), OUT, Some("Raid"))
        .expect("set");
    assert_eq!(the_last_routes_sent(&engine).len(), 2);
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &battlefield()), stream(2, OUT, &brave())],
    );
    let _ = app.drain_events();

    app.handle(&[UiAction::DeletePreset]);
    assert_eq!(the_routes_sent(&engine), Vec::<AppRoute>::new());
    for app_key in [battlefield(), brave()] {
        assert_eq!(app.app_preset(&app_key, OUT), AppPreset::Follow);
    }
    let said = followed_notice("Raid");
    assert_eq!(notices(&app.drain_events(), &said), 1);
    assert_eq!(app.state.notification.as_deref(), Some(said.as_str()));
    // Written at once: the next start follows too.
    let saved = AppRules::load_from(&file);
    assert!(
        saved.apps.iter().all(|rule| !rule.has_preset(OUT)),
        "{saved:?}"
    );

    // The applications keep playing: nothing is said again.
    play(
        &mut app,
        &engine,
        vec![stream(2, OUT, &brave()), stream(1, OUT, &battlefield())],
    );
    assert_eq!(notices(&app.drain_events(), &said), 0);
}

#[test]
fn deleting_a_preset_no_rule_names_says_nothing_about_applications() {
    let (mut app, engine, _dir) = started();
    with_raid(&mut app);
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let _ = routes_sent(&engine);
    let _ = app.drain_events();

    app.handle(&[UiAction::DeletePreset]);
    assert!(routes_sent(&engine).is_empty(), "Gaming still runs");
    assert_eq!(notices(&app.drain_events(), &followed_notice("Raid")), 0);
    assert_eq!(
        app.app_preset(&battlefield(), OUT),
        AppPreset::Preset("Gaming")
    );
}

#[test]
fn deleting_a_user_copy_of_a_factory_preset_leaves_the_rule_on_the_factory_one() {
    let (mut app, engine, _dir) = started();
    // A user preset called `Gaming` shadows the factory one of that name.
    pick(&mut app, OUT, "Music");
    app.handle(&[
        UiAction::SetEffect(Effect::Bass, 10.0),
        UiAction::SavePresetAs("Gaming".to_owned()),
    ]);
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let mine = output(&the_routes_sent(&engine)[0]);
    assert_eq!(mine.effect(Effect::Bass), 1.0);
    let _ = app.drain_events();

    pick(&mut app, OUT, "Gaming");
    app.handle(&[UiAction::DeletePreset]);
    let routes = the_routes_sent(&engine);
    assert_eq!(
        routes[0].preset, "Gaming",
        "the factory Gaming takes its place"
    );
    assert!((output(&routes[0]).effect(Effect::Bass) - 0.6).abs() < 0.01);
    assert_eq!(notices(&app.drain_events(), &followed_notice("Gaming")), 0);
}

#[test]
fn a_renamed_preset_carries_its_applications_to_its_new_name() {
    let (mut app, engine, dir) = started();
    let file = dir.path().join("apps.toml");
    app.use_app_rules_file_for_tests(file.clone());
    with_raid(&mut app);
    app.set_app_preset(&battlefield(), OUT, Some("Raid"))
        .expect("set");
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let before = the_routes_sent(&engine);

    app.rename_preset("Raid Night");
    let after = the_routes_sent(&engine);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].preset, "Raid Night");
    assert_eq!(
        after[0].params, before[0].params,
        "the same preset, renamed"
    );
    assert_eq!(
        app.app_preset(&battlefield(), OUT),
        AppPreset::Preset("Raid Night")
    );
    assert_eq!(
        AppRules::load_from(&file).preset_for(&battlefield(), OUT),
        Some("Raid Night")
    );
}

#[test]
fn saving_over_a_preset_an_application_runs_sends_what_was_saved_and_never_the_unsaved_edits() {
    let (mut app, engine, _dir) = started();
    with_raid(&mut app);
    app.set_app_preset(&battlefield(), OUT, Some("Raid"))
        .expect("set");
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let saved = output(&the_routes_sent(&engine)[0]);
    assert!((saved.effect(Effect::Bass) - 0.9).abs() < 0.01, "{saved:?}");

    app.handle(&[UiAction::SetEffect(Effect::Bass, 2.0)]);
    assert_eq!(app.lane_preset(OUT), Some(("Raid", true)));
    assert!(
        routes_sent(&engine).is_empty(),
        "an edit the window has not saved is the lane's alone"
    );

    app.handle(&[UiAction::SavePreset]);
    let routes = the_routes_sent(&engine);
    assert!((output(&routes[0]).effect(Effect::Bass) - 0.2).abs() < 0.01);
}

#[test]
fn a_rule_naming_a_preset_that_is_not_there_follows_says_so_once_and_comes_back_with_it() {
    let (mut app, engine, dir) = started();
    let file = dir.path().join("apps.toml");
    std::fs::write(
        &file,
        "[[app]]\nbinary = \"bf6.exe\"\nname = \"Battlefield 6\"\noutput_preset = \"Nowhere\"\n\
         last_seen = 1\n",
    )
    .expect("write the store");
    app.use_app_rules_file_for_tests(file);
    assert_eq!(
        app.app_preset(&battlefield(), OUT),
        AppPreset::Missing("Nowhere")
    );

    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    assert!(routes_sent(&engine).is_empty(), "it follows the lane");
    let said = followed_notice("Nowhere");
    assert_eq!(notices(&app.drain_events(), &said), 1);
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &battlefield()), stream(2, OUT, &brave())],
    );
    assert_eq!(notices(&app.drain_events(), &said), 0, "once a session");
    assert_eq!(
        app.app_rules().preset_for(&battlefield(), OUT),
        Some("Nowhere"),
        "the rule is not rewritten"
    );

    // A preset of that name appears: the application runs it.
    pick(&mut app, OUT, "Gaming");
    app.handle(&[UiAction::SavePresetAs("Nowhere".to_owned())]);
    let routes = the_routes_sent(&engine);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].preset, "Nowhere");
}

// ---- what every chain shares --------------------------------------------------------------------

/// Battlefield on `Gaming` and Discord on `Headset`, both running, what that sent taken.
fn routed() -> (App, FakeEngine, tempfile::TempDir) {
    let (mut app, engine, dir) = started();
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    app.set_app_preset(&discord(), IN, Some("Headset"))
        .expect("set");
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &battlefield()), stream(2, IN, &discord())],
    );
    let _ = routes_sent(&engine);
    (app, engine, dir)
}

#[test]
fn moving_a_level_the_speakers_share_resends_the_playback_routes_with_it() {
    let (mut app, engine, _dir) = routed();
    let game = |routes: &[AppRoute]| output(route_of(routes, &battlefield(), OUT));

    app.handle(&[UiAction::SetMasterGain(6.0)]);
    let routes = the_routes_sent(&engine);
    assert_eq!(game(&routes).master_gain_db, 6.0);
    assert_eq!(
        input(route_of(&routes, &discord(), IN)).makeup_db,
        4.0,
        "a recording route keeps its voice's own makeup"
    );

    app.handle(&[UiAction::SetBalance(3.0)]);
    assert_eq!(game(&the_routes_sent(&engine)).balance, 3.0);
    app.handle(&[UiAction::SetVolumeLeveling(2.5)]);
    assert_eq!(game(&the_routes_sent(&engine)).volume_leveling_db, 2.5);
    app.handle(&[UiAction::SetFilterQ(2.0)]);
    assert_eq!(game(&the_routes_sent(&engine)).filter_q, 2.0);

    app.handle(&[UiAction::SetBandCount(15)]);
    let bands = game(&the_routes_sent(&engine));
    assert_eq!(bands.bands().0.len(), 15, "the user's band count");

    // Nothing moved: nothing is sent.
    app.handle(&[UiAction::SetMasterGain(6.0)]);
    assert!(routes_sent(&engine).is_empty());
}

#[test]
fn a_voices_own_makeup_and_the_edit_direction_move_no_route() {
    let (mut app, engine, _dir) = routed();
    app.handle(&[UiAction::SetEditDirection(IN)]);
    app.handle(&[UiAction::SetMasterGain(9.0)]);
    app.handle(&[UiAction::SetEditDirection(OUT)]);
    assert!(routes_sent(&engine).is_empty());
}

#[test]
fn a_microphone_setting_reaches_the_recording_routes() {
    let (mut app, engine, _dir) = routed();
    app.set_noise_suppression(NoiseSuppressionOverride::Strong);
    let routes = the_routes_sent(&engine);
    let voice = input(route_of(&routes, &discord(), IN));
    assert_eq!(voice.denoise_level, DenoiseLevel::Strong);
    assert!(voice.rnnoise, "a pinned level runs the denoiser");
}

#[test]
fn turning_the_power_off_takes_every_route_away_and_turning_it_on_brings_them_back() {
    // Power off hands both defaults back so that nothing passes through FxSound (U12): a game
    // left on `FxSound (Output) · Gaming` would stay on the lane's device, at FxSound's volume,
    // when the user moves the system's default somewhere else.
    let (mut app, engine, _dir) = routed();
    let before = app.app_routes().to_vec();
    assert!(!before.is_empty());

    app.handle(&[UiAction::TogglePower]);
    assert_eq!(
        the_last_routes_sent(&engine),
        [],
        "no route while the power is off"
    );
    assert!(app.app_routes().is_empty());
    assert_eq!(
        store_choice(&app, OUT, &battlefield()).as_deref(),
        Some("Gaming"),
        "the choice itself is kept"
    );

    // Nothing that changes meanwhile puts a route back: a level, a preset saved, a new rule.
    app.handle(&[UiAction::SetMasterGain(6.0)]);
    app.set_app_preset(&brave(), OUT, Some("Volume Boost"))
        .expect("set");
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &battlefield()), stream(3, OUT, &brave())],
    );
    assert!(routes_sent(&engine).iter().all(Vec::is_empty));
    assert!(app.app_routes().is_empty());

    app.handle(&[UiAction::TogglePower]);
    let routes = the_routes_sent(&engine);
    assert_eq!(
        presets_of(&routes),
        [
            (OUT, battlefield(), "Gaming"),
            (OUT, brave(), "Volume Boost"),
            (IN, discord(), "Headset")
        ],
        "every rule resolved meanwhile, back at once"
    );
    let game = output(route_of(&routes, &battlefield(), OUT));
    assert!(game.power);
    assert_eq!(game.master_gain_db, 6.0, "with the levels of the moment");
    assert!(input(route_of(&routes, &discord(), IN)).power);
}

#[test]
fn turning_the_power_off_switches_every_route_off_before_it_takes_them_away() {
    // The engine keeps a route no rule names any more for as long as a stream it cannot move is
    // on it — one pinned to the route's node, or anchored there by WirePlumber — running what it
    // was last sent. Taken away at once, such a route went on with the preset's effects while the
    // switch said `Off`.
    let (mut app, engine, _dir) = routed();
    let before = app.app_routes().to_vec();
    assert!(
        before
            .iter()
            .filter(|route| !route.preset.is_empty())
            .all(powered),
        "{before:?}"
    );

    app.handle(&[UiAction::TogglePower]);
    let sent = routes_sent(&engine);
    let [bypassed, none] = sent.as_slice() else {
        panic!("the routes switched off, then none: {sent:?}");
    };
    assert!(
        !bypassed.iter().any(powered),
        "no route is left running its effects: {bypassed:?}"
    );
    assert_eq!(
        bypassed
            .iter()
            .cloned()
            .map(|route| with_power(route, true))
            .collect::<Vec<_>>(),
        before,
        "the routes as they were, with nothing but the power changed"
    );
    assert!(none.is_empty());

    // Off stays off: nothing more is sent, and nothing that is sent runs.
    app.handle(&[UiAction::SetMasterGain(6.0)]);
    assert!(routes_sent(&engine).is_empty());
}

#[test]
fn a_start_with_the_power_off_sends_no_route_until_it_is_turned_on() {
    let dir = tempfile::tempdir().expect("scratch directory");
    let file = dir.path().join("apps.toml");
    std::fs::write(
        &file,
        "[[app]]\nbinary = \"bf6.exe\"\nname = \"Battlefield 6\"\noutput_preset = \"Gaming\"\n\
         last_seen = 1\n",
    )
    .expect("write the store");
    let mut settings = Settings::default();
    settings.output_preset = "Music".to_owned();
    settings.power = false;
    let engine = FakeEngine::new();
    let mut app = App::start_for_tests(
        settings,
        music_presets(dir.path()),
        voice_presets(dir.path()),
        &engine,
    );
    app.use_app_rules_file_for_tests(file);
    assert_eq!(
        store_choice(&app, OUT, &battlefield()).as_deref(),
        Some("Gaming")
    );
    assert!(
        routes_sent(&engine).iter().all(Vec::is_empty),
        "a game started while FxSound is off is moved nowhere"
    );

    app.handle(&[UiAction::TogglePower]);
    assert_eq!(
        presets_of(&the_last_routes_sent(&engine)),
        [(OUT, battlefield(), "Gaming")]
    );
}

#[test]
fn a_system_going_to_sleep_leaves_every_route_to_the_engine_s_own_mute() {
    // The engine silences each route with its lane while the system sleeps, and gives a sleep
    // that never ends up after a minute awake. A route that carried the app's own mute would stay
    // silent for as long as logind's `false` stayed lost.
    let (mut app, engine, _dir) = routed();
    app.system_sleeping(true);
    assert!(
        routes_sent(&engine).is_empty(),
        "nothing to change on a route"
    );
    assert!(app.app_routes().iter().all(|route| match route.params {
        RouteParams::Output(params) => !params.mute,
        RouteParams::Input(params) => !params.mute,
    }));
}

// ---- the store ----------------------------------------------------------------------------------

#[test]
fn an_application_seen_for_the_first_time_is_remembered_following_both_lanes_at_once() {
    let (mut app, engine, dir) = started();
    let file = dir.path().join("apps.toml");
    app.use_app_rules_file_for_tests(file.clone());
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);

    let rule = app.app_rules().rule(&battlefield()).expect("remembered");
    assert_eq!(rule.key, battlefield());
    assert!(!rule.has_preset(OUT) && !rule.has_preset(IN));
    assert!(rule.last_seen > 0);
    assert_eq!(
        AppRules::load_from(&file).apps,
        app.app_rules().apps,
        "written at once"
    );
    assert_eq!(app.running_apps(OUT), vec![&battlefield()]);
    assert!(app.running_apps(IN).is_empty());
}

#[test]
fn an_application_seen_again_is_written_a_while_later_or_on_the_way_out() {
    let (mut app, engine, dir) = started();
    let file = dir.path().join("apps.toml");
    let remembered = "[[app]]\nbinary = \"bf6.exe\"\nname = \"Battlefield 6\"\nlast_seen = 1\n";
    std::fs::write(&file, remembered).expect("write the store");
    app.use_app_rules_file_for_tests(file.clone());

    let before = Instant::now();
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let seen = app.app_rules().apps[0].last_seen;
    assert!(seen > 1, "seen now, in memory");
    assert_eq!(
        AppRules::load_from(&file).apps[0].last_seen,
        1,
        "not written on every report"
    );
    let due = app.next_deadline().expect("the store is due to be written");
    assert!(due >= before + SEEN_SAVE_DELAY, "{due:?}");

    app.poll_audio_at(due - Duration::from_secs(1));
    assert_eq!(AppRules::load_from(&file).apps[0].last_seen, 1);
    app.poll_audio_at(due);
    assert_eq!(AppRules::load_from(&file).apps[0].last_seen, seen);
    assert_eq!(app.next_deadline(), None);

    // Seen again, and the app quits before the delay is up: written on the way out.
    std::fs::write(&file, remembered).expect("write the store");
    app.use_app_rules_file_for_tests(file.clone());
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    app.shutdown();
    assert!(AppRules::load_from(&file).apps[0].last_seen > 1);
}

#[test]
fn a_rule_chosen_is_written_at_once() {
    let (mut app, _engine, dir) = started();
    let file = dir.path().join("apps.toml");
    app.use_app_rules_file_for_tests(file.clone());
    app.set_app_preset(&discord(), IN, Some("Headset"))
        .expect("set");
    assert_eq!(
        AppRules::load_from(&file).preset_for(&discord(), IN),
        Some("Headset")
    );
    assert_eq!(app.next_deadline(), None, "nothing left to write");
}

#[test]
fn a_test_app_keeps_its_store_in_memory() {
    let (mut app, engine, _dir) = started();
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    assert!(app.apps.path.is_none());
    assert_eq!(app.app_rules().apps.len(), 1);
}

#[test]
fn a_choice_the_disk_would_not_take_is_written_on_the_way_out_with_nothing_playing() {
    let (mut app, _engine, dir) = started();
    // The configuration directory cannot be made for a moment: a file is where it would go.
    let config = dir.path().join("config");
    std::fs::write(&config, "in the way").expect("a file");
    let file = config.join("apps.toml");
    app.use_app_rules_file_for_tests(file.clone());

    let before = Instant::now();
    assert_eq!(
        app.set_app_preset(&battlefield(), OUT, Some("Gaming")),
        Ok(true),
        "in force for this run whether or not it is written"
    );
    assert!(!file.exists());
    let due = app.next_deadline().expect("the store is still due");
    assert!(due >= before + SAVE_RETRY_DELAY, "{due:?}");

    // The trouble clears; the game has quit and nothing plays; FxSound exits.
    std::fs::remove_file(&config).expect("out of the way");
    app.shutdown();
    assert_eq!(
        AppRules::load_from(&file).preset_for(&battlefield(), OUT),
        Some("Gaming")
    );
}

#[test]
fn a_write_that_failed_is_tried_again_by_the_timer_until_it_succeeds() {
    let (mut app, _engine, dir) = started();
    let config = dir.path().join("config");
    std::fs::write(&config, "in the way").expect("a file");
    let file = config.join("apps.toml");
    app.use_app_rules_file_for_tests(file.clone());
    app.set_app_preset(&discord(), IN, Some("Headset"))
        .expect("set");

    let due = app.next_deadline().expect("due again");
    app.poll_audio_at(due);
    assert!(!file.exists());
    let again = app.next_deadline().expect("still due while it fails");

    std::fs::remove_file(&config).expect("out of the way");
    app.poll_audio_at(again);
    assert_eq!(
        AppRules::load_from(&file).preset_for(&discord(), IN),
        Some("Headset")
    );
    assert_eq!(app.next_deadline(), None, "nothing left to write");
}

// ---- a name nothing answers to ------------------------------------------------------------------

#[test]
fn a_name_nothing_answers_to_is_kept_as_the_identifier_it_looks_like() {
    let program = |text: &str| key(text, "", "");
    let flatpak = |text: &str| key("", "", text);
    let name = |text: &str| key("", text, "");
    for (text, kept) in [
        ("bf6.exe", program("bf6.exe")),
        ("My Game.EXE", program("My Game.EXE")),
        ("C:\\Games\\bf6.exe", program("C:\\Games\\bf6.exe")),
        ("/usr/bin/mpv", program("/usr/bin/mpv")),
        ("firefox", program("firefox")),
        ("Discord", program("Discord")),
        ("libfoo.so", program("libfoo.so")),
        ("1.2.3", program("1.2.3")),
        ("org..firefox", program("org..firefox")),
        ("com.discordapp.Discord", flatpak("com.discordapp.Discord")),
        (" org.mozilla.firefox ", flatpak("org.mozilla.firefox")),
        (
            "io.github.some_app.Tool-2",
            flatpak("io.github.some_app.Tool-2"),
        ),
        ("Battlefield 6", name("Battlefield 6")),
        ("OBS Studio", name("OBS Studio")),
    ] {
        assert_eq!(unseen_key(text), kept, "{text:?}");
    }
    assert_eq!(
        unseen_description(&flatpak("com.discordapp.Discord")),
        "the Flatpak \"com.discordapp.Discord\""
    );
    assert_eq!(
        unseen_description(&program("bf6.exe")),
        "the program \"bf6.exe\""
    );
    assert_eq!(
        unseen_description(&name("Battlefield 6")),
        "the application called \"Battlefield 6\""
    );
}

#[test]
fn a_flatpak_id_or_a_name_given_before_the_application_ever_ran_reaches_it_when_it_does() {
    let (mut app, engine, _dir) = started();
    for (text, direction, preset) in [
        ("com.discordapp.Discord", IN, "Headset"),
        ("Battlefield 6", OUT, "Gaming"),
    ] {
        let done = app
            .set_named_app_preset(text, direction, Some(preset))
            .expect("kept");
        assert!(done.unseen && !done.held && done.changed, "{done:?}");
    }
    play(
        &mut app,
        &engine,
        vec![stream(1, IN, &discord()), stream(2, OUT, &battlefield())],
    );
    assert_eq!(
        app.app_rules().apps.len(),
        2,
        "each is the application of its rule: {:?}",
        app.app_rules()
    );
    let routes = app.app_routes();
    assert_eq!(engine_choice(routes, IN, &discord()), Some("Headset"));
    assert_eq!(engine_choice(routes, OUT, &battlefield()), Some("Gaming"));
}

#[test]
fn a_cold_start_holds_a_name_it_does_not_know_until_it_hears_what_plays() {
    let (mut app, engine, _dir) = started();
    app.hold_app_presets();
    // Firefox plays as `firefox-bin`, and has never been seen: its name is not its program.
    let firefox = key("firefox-bin", "Firefox", "");
    let done = app
        .set_named_app_preset("Firefox", OUT, Some("Music"))
        .expect("held");
    assert!(done.held && done.unseen && !done.changed, "{done:?}");
    assert!(app.app_rules().apps.is_empty());
    assert!(routes_sent(&engine).is_empty());
    assert_eq!(
        app.set_named_app_preset("Firefox", OUT, Some("Nope")),
        Err(AppRuleRefusal::UnknownPreset {
            direction: OUT,
            name: "Nope".to_owned(),
        }),
        "a preset the lane does not have is refused at once, not held"
    );

    // The engine's first report: the choice is made for the application that answers.
    play(&mut app, &engine, vec![stream(1, OUT, &firefox)]);
    assert_eq!(app.app_rules().apps.len(), 1);
    assert_eq!(app.app_rules().preset_for(&firefox, OUT), Some("Music"));
    assert_eq!(
        engine_choice(app.app_routes(), OUT, &firefox),
        Some("Music")
    );

    // The wait is over: the next name is kept at once.
    let done = app
        .set_named_app_preset("OBS Studio", IN, Some("Clean"))
        .expect("kept");
    assert!(!done.held && done.changed);
}

#[test]
fn a_cold_start_with_nothing_playing_stops_waiting_after_a_while() {
    let (mut app, engine, _dir) = started();
    let before = Instant::now();
    app.hold_app_presets();
    app.set_named_app_preset("com.discordapp.Discord", IN, Some("Headset"))
        .expect("held");
    let until = app.next_deadline().expect("a wait with an end");
    assert!(until >= before + STREAMS_PATIENCE, "{until:?}");

    app.poll_audio_at(until - Duration::from_millis(1));
    assert!(app.app_rules().apps.is_empty(), "still waiting");
    app.poll_audio_at(until);
    assert_eq!(
        app.app_rules()
            .preset_for(&key("", "", "com.discordapp.Discord"), IN),
        Some("Headset"),
        "kept as the Flatpak it names"
    );
    assert_eq!(app.next_deadline(), None);

    // Discord starts later: its rule was waiting for it.
    play(&mut app, &engine, vec![stream(4, IN, &discord())]);
    assert_eq!(app.app_rules().apps.len(), 1);
    assert_eq!(
        engine_choice(app.app_routes(), IN, &discord()),
        Some("Headset")
    );
}

#[test]
fn what_a_cold_start_held_is_kept_on_the_way_out() {
    let (mut app, _engine, dir) = started();
    let file = dir.path().join("apps.toml");
    app.use_app_rules_file_for_tests(file.clone());
    app.hold_app_presets();
    app.set_named_app_preset("bf6.exe", OUT, Some("Gaming"))
        .expect("held");
    app.shutdown();
    assert_eq!(
        AppRules::load_from(&file).preset_for(&battlefield(), OUT),
        Some("Gaming")
    );
}

// ---- the event stream ---------------------------------------------------------------------------

fn routed_events(events: &[AppEvent]) -> Vec<(AppKey, DeviceDirection, Option<String>)> {
    events
        .iter()
        .filter_map(|event| match event {
            AppEvent::AppRouted {
                app,
                direction,
                preset,
            } => Some((app.clone(), *direction, preset.clone())),
            _ => None,
        })
        .collect()
}

fn on_route(id: u32, direction: DeviceDirection, app: &AppKey, preset: &str) -> AppStream {
    AppStream {
        route: Some(preset.to_owned()),
        ..stream(id, direction, app)
    }
}

#[test]
fn where_the_engine_moved_an_application_is_said_once_and_its_return_too() {
    let (mut app, engine, _dir) = started();
    play(
        &mut app,
        &engine,
        vec![
            on_route(1, OUT, &battlefield(), "Gaming"),
            on_route(2, OUT, &battlefield(), "Gaming"),
            stream(3, OUT, &brave()),
        ],
    );
    assert_eq!(
        routed_events(&app.drain_events()),
        [(battlefield(), OUT, Some("Gaming".to_owned()))]
    );

    // The same again, and Discord on its lane: nothing to say.
    play(
        &mut app,
        &engine,
        vec![
            on_route(1, OUT, &battlefield(), "Gaming"),
            stream(4, IN, &discord()),
        ],
    );
    assert!(routed_events(&app.drain_events()).is_empty());

    // Discord is moved, Battlefield back onto its lane.
    play(
        &mut app,
        &engine,
        vec![
            stream(1, OUT, &battlefield()),
            on_route(4, IN, &discord(), "Headset"),
        ],
    );
    assert_eq!(
        routed_events(&app.drain_events()),
        [
            (discord(), IN, Some("Headset".to_owned())),
            (battlefield(), OUT, None),
        ]
    );

    // Discord quits while routed.
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    assert_eq!(routed_events(&app.drain_events()), [(discord(), IN, None)]);
}

#[test]
fn a_rule_the_engine_has_not_acted_on_is_not_said_to_be_routed() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    assert_eq!(the_routes_sent(&engine).len(), 1);
    assert!(
        routed_events(&app.drain_events()).is_empty(),
        "the engine's report says where it is, and it has not moved it yet"
    );
    assert!(app.unsaid_changes().is_empty());
}

// ---- Settings ▸ Applications ----------------------------------------------------------------------

use fxsound_ui::dialogs::settings::SettingsAction;
use fxsound_ui::state::RoutedApp;

/// OBS Studio: records, and never plays.
fn obs() -> AppKey {
    key("obs", "OBS Studio", "")
}

/// A pane row as `(name, running, [(lane, preset)])`.
type PaneRow = (String, bool, Vec<(DeviceDirection, Option<String>)>);

/// The pane's rows.
fn pane_rows(app: &App) -> Vec<PaneRow> {
    app.app_rows()
        .into_iter()
        .map(|row| {
            (
                row.name,
                row.running,
                row.lanes
                    .into_iter()
                    .map(|lane| (lane.direction, lane.preset))
                    .collect(),
            )
        })
        .collect()
}

fn row(name: &str, running: bool, lanes: &[(DeviceDirection, Option<&str>)]) -> PaneRow {
    (
        name.to_owned(),
        running,
        lanes
            .iter()
            .map(|(lane, preset)| (*lane, preset.map(str::to_owned)))
            .collect(),
    )
}

/// Spotify remembered from last week, Brave from yesterday with a preset of its own.
fn with_remembered(app: &mut App, dir: &Path) {
    let file = dir.join("apps.toml");
    std::fs::write(
        &file,
        "[[app]]\nbinary = \"spotify\"\nname = \"Spotify\"\nlast_seen = 100\n\n\
         [[app]]\nbinary = \"brave\"\nname = \"Brave\"\noutput_preset = \"Volume Boost\"\n\
         last_seen = 200\n",
    )
    .expect("write the store");
    app.use_app_rules_file_for_tests(file);
}

#[test]
fn the_pane_lists_the_running_applications_first_each_with_the_lanes_it_uses() {
    let (mut app, engine, dir) = started();
    with_remembered(&mut app, dir.path());
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    play(
        &mut app,
        &engine,
        vec![
            stream(1, IN, &discord()),
            stream(2, IN, &obs()),
            stream(3, OUT, &discord()),
            stream(4, OUT, &battlefield()),
        ],
    );
    assert_eq!(
        pane_rows(&app),
        [
            // Running, by name, each with a combo for the lanes it plays or records on.
            row("Battlefield 6", true, &[(OUT, Some("Gaming"))]),
            row("Discord", true, &[(OUT, None), (IN, None)]),
            row("OBS Studio", true, &[(IN, None)]),
            // Remembered, the most recently seen first. Their lanes are not known, so both, the
            // one Brave has a preset of its own on too.
            row("Brave", false, &[(OUT, Some("Volume Boost")), (IN, None)]),
            row("Spotify", false, &[(OUT, None), (IN, None)]),
        ]
    );
}

#[test]
fn a_lane_heard_this_session_keeps_its_combo_after_the_application_leaves_it() {
    let (mut app, engine, _dir) = started();
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &discord()), stream(2, IN, &discord())],
    );
    play(&mut app, &engine, vec![stream(1, OUT, &discord())]);
    assert_eq!(
        pane_rows(&app),
        [row("Discord", true, &[(OUT, None), (IN, None)])]
    );
    // Quit: remembered, with the lanes it used.
    play(&mut app, &engine, Vec::new());
    assert_eq!(
        pane_rows(&app),
        [row("Discord", false, &[(OUT, None), (IN, None)])]
    );
    // A lane with a preset of its own always has its combo, heard or not.
    play(&mut app, &engine, vec![stream(5, OUT, &battlefield())]);
    app.set_app_preset(&battlefield(), IN, Some("Headset"))
        .expect("set");
    assert_eq!(
        pane_rows(&app)[0],
        row("Battlefield 6", true, &[(OUT, None), (IN, Some("Headset"))])
    );
}

#[test]
fn choosing_a_preset_on_a_remembered_row_keeps_the_other_lanes_combo() {
    let (mut app, _engine, dir) = started();
    with_remembered(&mut app, dir.path());
    let spotify = key("spotify", "Spotify", "");
    let mut pane = app.settings_state();
    app.handle_settings(
        &SettingsAction::SetAppPreset {
            app: spotify,
            direction: IN,
            preset: Some("Clean".to_owned()),
        },
        &mut pane,
    );
    // Seen just now, so first; its lanes are still not known, so both stay.
    assert_eq!(
        pane_rows(&app),
        [
            row("Spotify", false, &[(OUT, None), (IN, Some("Clean"))]),
            row("Brave", false, &[(OUT, Some("Volume Boost")), (IN, None)]),
        ]
    );
    assert_eq!(pane.apps, app.app_rows());

    // Heard playing, it has the lane it plays on and the one it has a preset of its own on.
    let (mut app, engine, _dir) = started();
    play(&mut app, &engine, vec![stream(1, OUT, &brave())]);
    app.set_app_preset(&brave(), OUT, Some("Volume Boost"))
        .expect("set");
    assert_eq!(
        pane_rows(&app),
        [row("Brave", true, &[(OUT, Some("Volume Boost"))])]
    );
}

#[test]
fn a_general_rule_is_one_row_running_for_every_program_it_covers() {
    let (mut app, engine, _dir) = started();
    let by_name = key("", "Discord", "");
    app.set_app_preset(&by_name, IN, Some("Headset"))
        .expect("set");
    play(&mut app, &engine, vec![stream(1, IN, &discord())]);
    assert_eq!(
        pane_rows(&app),
        [row("Discord", true, &[(IN, Some("Headset"))])]
    );
    assert_eq!(app.app_rows()[0].app, by_name, "the rule's own key");
}

#[test]
fn choosing_in_the_pane_routes_the_application_at_once_and_the_row_says_so() {
    let (mut app, engine, _dir) = started();
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let _ = routes_sent(&engine);
    let mut pane = app.settings_state();
    assert_eq!(pane.apps.len(), 1);

    app.handle_settings(
        &SettingsAction::SetAppPreset {
            app: battlefield(),
            direction: OUT,
            preset: Some("Gaming".to_owned()),
        },
        &mut pane,
    );
    let routes = the_routes_sent(&engine);
    assert_eq!(route_of(&routes, &battlefield(), OUT).preset, "Gaming");
    assert_eq!(
        pane.apps[0].lanes,
        [AppLane {
            direction: OUT,
            preset: Some("Gaming".to_owned()),
        }]
    );

    // "FxSound's preset": back on the lane.
    app.handle_settings(
        &SettingsAction::SetAppPreset {
            app: battlefield(),
            direction: OUT,
            preset: None,
        },
        &mut pane,
    );
    assert_eq!(the_routes_sent(&engine), Vec::<AppRoute>::new());
    assert_eq!(pane.apps[0].lanes[0].preset, None);
    assert_eq!(app.app_rules().preset_for(&battlefield(), OUT), None);
}

#[test]
fn a_preset_gone_under_the_open_pane_is_refused_and_changes_nothing() {
    let (mut app, engine, _dir) = started();
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let _ = routes_sent(&engine);
    let mut pane = app.settings_state();
    let before = pane.apps.clone();
    app.handle_settings(
        &SettingsAction::SetAppPreset {
            app: battlefield(),
            direction: OUT,
            preset: Some("Deleted a moment ago".to_owned()),
        },
        &mut pane,
    );
    assert!(routes_sent(&engine).is_empty());
    assert_eq!(pane.apps, before);
    assert_eq!(app.app_rules().preset_for(&battlefield(), OUT), None);
}

#[test]
fn the_cross_forgets_a_remembered_application_and_takes_a_running_ones_presets() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&brave(), OUT, Some("Volume Boost"))
        .expect("set");
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    let _ = routes_sent(&engine);
    let mut pane = app.settings_state();
    assert_eq!(pane.apps.len(), 2);

    app.handle_settings(&SettingsAction::ForgetApp(brave()), &mut pane);
    assert_eq!(
        pane.apps
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["Battlefield 6"]
    );
    assert_eq!(
        presets_of(&the_routes_sent(&engine)),
        [(OUT, battlefield(), "Gaming")],
        "Brave's rule is gone from the engine too, running or not"
    );

    app.handle_settings(&SettingsAction::ForgetApp(battlefield()), &mut pane);
    assert_eq!(the_routes_sent(&engine), Vec::<AppRoute>::new());
    assert_eq!(pane.apps.len(), 1, "still running, so still listed");
    assert!(pane.apps[0].lanes.iter().all(|lane| lane.preset.is_none()));
    assert!(!pane.apps[0].can_forget(), "nothing left to forget");
}

#[test]
fn the_open_pane_follows_the_applications_and_both_lanes_presets() {
    let (mut app, engine, _dir) = started();
    let mut pane = app.settings_state();
    assert!(pane.apps.is_empty());
    assert_eq!(pane.presets, ["Gaming", "Music", "Volume Boost"]);
    assert_eq!(pane.input_presets, ["Clean", "Headset"]);

    play(&mut app, &engine, vec![stream(1, OUT, &brave())]);
    app.refresh_settings_state(&mut pane);
    assert_eq!(
        pane.apps
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        ["Brave"]
    );
    // A rule chosen from the command line while the pane is open shows too.
    app.set_app_preset(&brave(), OUT, Some("Volume Boost"))
        .expect("set");
    app.refresh_settings_state(&mut pane);
    assert_eq!(
        pane.apps[0].lanes[0].preset.as_deref(),
        Some("Volume Boost")
    );
}

#[test]
fn the_window_knows_which_applications_the_engine_moved_and_the_preset_lists_tip_says_so() {
    let (mut app, engine, _dir) = started();
    play(
        &mut app,
        &engine,
        vec![
            on_route(1, OUT, &battlefield(), "Gaming"),
            on_route(2, IN, &discord(), "Headset"),
            stream(3, OUT, &brave()),
        ],
    );
    assert_eq!(
        app.state.routed_apps,
        [
            RoutedApp {
                direction: OUT,
                name: "Battlefield 6".to_owned(),
                preset: "Gaming".to_owned(),
            },
            RoutedApp {
                direction: IN,
                name: "Discord".to_owned(),
                preset: "Headset".to_owned(),
            },
        ]
    );
    app.handle(&[UiAction::SetEditDirection(OUT)]);
    assert_eq!(
        fxsound_ui::views::pro::routed_apps_tip(&app.state).as_deref(),
        Some("Battlefield 6 → Gaming")
    );
    app.handle(&[UiAction::SetEditDirection(IN)]);
    assert_eq!(
        fxsound_ui::views::pro::routed_apps_tip(&app.state).as_deref(),
        Some("Discord → Headset")
    );

    // Moved back onto its lane: the tip has nothing to say.
    play(&mut app, &engine, vec![stream(1, OUT, &battlefield())]);
    assert!(app.state.routed_apps.is_empty());
    assert_eq!(fxsound_ui::views::pro::routed_apps_tip(&app.state), None);
}
