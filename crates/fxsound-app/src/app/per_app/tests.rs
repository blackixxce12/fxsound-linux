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
    assert!(
        routes_sent(&engine).is_empty(),
        "nothing is running yet, so there is nothing to route"
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
    let routes = the_routes_sent(&engine);
    assert_eq!(routes.len(), 3, "{routes:?}");

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
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &battlefield()), stream(2, IN, &discord())],
    );
    let routes = the_routes_sent(&engine);

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
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &battlefield()), stream(2, OUT, &brave())],
    );
    let routes = the_routes_sent(&engine);
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
    assert_eq!(routes.len(), 1, "Discord still follows: {routes:?}");
    assert_eq!(routes[0].app, battlefield());

    // Back to following: the route goes.
    assert_eq!(app.set_app_preset(&battlefield(), OUT, None), Ok(true));
    assert_eq!(the_routes_sent(&engine), Vec::<AppRoute>::new());
}

#[test]
fn a_rule_for_an_application_that_is_not_running_builds_nothing_until_it_plays() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    assert!(routes_sent(&engine).is_empty());

    play(&mut app, &engine, vec![stream(7, OUT, &battlefield())]);
    assert_eq!(the_routes_sent(&engine).len(), 1);

    // It quits: its route goes with it.
    play(&mut app, &engine, Vec::new());
    assert_eq!(the_routes_sent(&engine), Vec::<AppRoute>::new());
}

#[test]
fn a_rule_is_for_the_lane_it_names_only() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&discord(), IN, Some("Headset"))
        .expect("set");
    // Discord only plays: its microphone rule has nothing to route.
    play(&mut app, &engine, vec![stream(3, OUT, &discord())]);
    assert!(routes_sent(&engine).is_empty());
}

#[test]
fn an_application_with_many_streams_is_one_route_and_the_engines_order_is_no_change() {
    let (mut app, engine, _dir) = started();
    app.set_app_preset(&battlefield(), OUT, Some("Gaming"))
        .expect("set");
    app.set_app_preset(&brave(), OUT, Some("Volume Boost"))
        .expect("set");
    play(
        &mut app,
        &engine,
        vec![
            stream(1, OUT, &battlefield()),
            stream(2, OUT, &battlefield()),
            stream(3, OUT, &brave()),
        ],
    );
    assert_eq!(the_routes_sent(&engine).len(), 2);

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
fn a_general_rule_routes_every_application_it_covers_under_that_applications_own_key() {
    let (mut app, engine, _dir) = started();
    // Written from the command line with only a name.
    let by_name = key("", "Discord", "");
    app.set_app_preset(&by_name, IN, Some("Headset"))
        .expect("set");
    play(&mut app, &engine, vec![stream(9, IN, &discord())]);
    let routes = the_routes_sent(&engine);
    assert_eq!(routes.len(), 1);
    assert_eq!(
        routes[0].app,
        discord(),
        "the engine is given the stream's key"
    );
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
    play(
        &mut app,
        &engine,
        vec![stream(1, OUT, &battlefield()), stream(2, OUT, &brave())],
    );
    assert_eq!(the_routes_sent(&engine).len(), 2);
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
fn the_power_switch_and_the_sleep_reach_every_route() {
    let (mut app, engine, _dir) = routed();
    app.handle(&[UiAction::TogglePower]);
    let routes = the_routes_sent(&engine);
    assert!(!output(route_of(&routes, &battlefield(), OUT)).power);
    assert!(!input(route_of(&routes, &discord(), IN)).power);
    app.handle(&[UiAction::TogglePower]);
    let _ = routes_sent(&engine);

    app.system_sleeping(true);
    let routes = the_routes_sent(&engine);
    assert!(output(route_of(&routes, &battlefield(), OUT)).mute);
    assert!(input(route_of(&routes, &discord(), IN)).mute);
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
            // Remembered, the most recently seen first; Spotify's lanes are not known, so both.
            row("Brave", false, &[(OUT, Some("Volume Boost"))]),
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
    assert!(routes_sent(&engine).is_empty(), "Brave was not running");

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
