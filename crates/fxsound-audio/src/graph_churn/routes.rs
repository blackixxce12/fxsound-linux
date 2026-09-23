//! Per-application routes against a private PipeWire (`crate::app_routes`, `engine::route_pairs`,
//! `docs/0.4.0-apps.md`): applications started with `pw-cat`, the rules the app would send for
//! them, and what the engine makes of both — the route pairs, their properties, and the
//! `target.object` keys it writes into the `default` metadata and deletes again.
//!
//! What cannot be seen here is the move itself. It is WirePlumber's: it reads the key and relinks
//! the stream (`linking/find-defined-target.lua`). No WirePlumber runs in a private graph, nothing
//! links anything, and a `pw-cat` stays where it is, unlinked, whatever the metadata says. So these
//! tests stop at the key: written with the right subject, value and type, on the right node's
//! serial, and deleted when it should be — the whole of FxSound's side of the move.

use super::apps::App;
use super::*;
use crate::app_routes::{self, RouteSlot};
use fxsound_core::messages::{AppRoute, DspParams, InputDspParams, RouteParams};
use fxsound_core::{AppKey, AppStream, MAX_ROUTES_PER_LANE};

/// How long a route nobody uses is kept in these tests: long enough for a few ticks to find it
/// unused and for a test to see it still there, short beside the patience every wait here has.
const IDLE: Duration = Duration::from_secs(2);

/// An engine on `graph` whose routes go after [`IDLE`], once its output lane has attached itself.
/// Returns the device the output lane is on.
fn engine_on(graph: &PrivateGraph) -> (EngineHandle, Transcript, String) {
    engine_keeping_routes_for(graph, IDLE)
}

/// [`engine_on`], with an unused route kept for `idle`.
fn engine_keeping_routes_for(
    graph: &PrivateGraph,
    idle: Duration,
) -> (EngineHandle, Transcript, String) {
    let handle = AudioEngine::start_with_route_idle(Some(&graph.remote()), idle)
        .expect("the engine should start");
    let mut said = Transcript::default();
    let mut output = None;
    assert!(said.until(&handle, "the output lane attached", |message| {
        if let AudioToUi::Attached {
            direction: DeviceDirection::Output,
            node_name: Some(name),
        } = message
        {
            output = Some(name.clone());
            true
        } else {
            false
        }
    }));
    (handle, said, output.expect("attached"))
}

/// A rule for the output lane: `app` plays through `preset`.
fn output_rule(app: AppKey, preset: &str) -> AppRoute {
    AppRoute {
        direction: DeviceDirection::Output,
        app,
        preset: preset.to_owned(),
        params: RouteParams::Output(DspParams::default()),
        chain: String::new(),
    }
}

/// A rule for the input lane: `app` records through `preset`, on the voice chain `chain`.
fn input_rule(app: AppKey, preset: &str, chain: &str) -> AppRoute {
    AppRoute {
        direction: DeviceDirection::Input,
        app,
        preset: preset.to_owned(),
        params: RouteParams::Input(InputDspParams::default()),
        chain: chain.to_owned(),
    }
}

fn named(name: &str) -> AppKey {
    AppKey {
        name: name.to_owned(),
        ..AppKey::default()
    }
}

/// Start `pw-cat` as an application called `name`, under the node name `node`, with `extra`
/// properties; or say why not and give up on the test.
fn start(graph: &PrivateGraph, mode: &str, node: &str, name: &str, extra: &str) -> Option<App> {
    let app = graph.pw_cat(
        mode,
        node,
        &format!(r#"application.name = "{name}" {extra}"#),
        &[],
    );
    if app.is_none() {
        skip(&format!(
            "pw-cat could not run {name}, so its route was not checked"
        ));
    }
    app
}

/// Whether `pw-cat` is there to run applications with; says so when it is not.
fn can_run_applications() -> bool {
    let can = installed("pw-cat");
    if !can {
        skip("pw-cat is not installed, so per-application routes were not checked");
    }
    can
}

/// The application list the engine sent last, once it lists `id` with `route`.
fn reported_with(
    said: &mut Transcript,
    handle: &EngineHandle,
    id: u64,
    route: Option<&str>,
) -> Option<Vec<AppStream>> {
    let mut found = None;
    said.until(
        handle,
        &format!("stream {id} reported on route {route:?}"),
        |message| match message {
            AudioToUi::AppStreams(streams)
                if streams.iter().any(|stream| {
                    u64::from(stream.id) == id && stream.route.as_deref() == route
                }) =>
            {
                found = Some(streams.clone());
                true
            }
            _ => false,
        },
    );
    found
}

/// A route node by name, with its `object.serial`.
type RouteNode = (String, u64);

/// A `target.object` key of the `default` metadata: the subject, the value and its type.
type TargetKey = (u64, String, String);

impl PrivateGraph {
    /// Every node of a per-application route in the graph now, by name, each with its
    /// `object.serial`, sorted by name. `None` when `pw-dump` is not there to ask.
    fn route_nodes(&self) -> Option<Vec<(String, u64)>> {
        let objects = self.dump()?;
        let mut nodes: Vec<(String, u64)> = objects
            .iter()
            .filter(|object| object["type"].as_str() == Some("PipeWire:Interface:Node"))
            .filter_map(|object| {
                let props = &object["info"]["props"];
                let name = props["node.name"].as_str()?;
                name.starts_with(crate::ROUTE_NODE_PREFIX).then(|| {
                    (
                        name.to_owned(),
                        props["object.serial"].as_u64().unwrap_or(0),
                    )
                })
            })
            .collect();
        nodes.sort();
        Some(nodes)
    }

    /// Wait until the route nodes in the graph are exactly `want`, by name, and return them with
    /// their serials. `None` when `pw-dump` is not there to ask; `Some(Err(..))` with the names
    /// last seen when they never came to be.
    fn routes_settle_on(&self, want: &[&str]) -> Option<Result<Vec<RouteNode>, Vec<String>>> {
        let mut want: Vec<&str> = want.to_vec();
        want.sort_unstable();
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = self.route_nodes()?;
            if seen
                .iter()
                .map(|(name, _)| name.as_str())
                .eq(want.iter().copied())
            {
                return Some(Ok(seen));
            }
            if Instant::now() >= deadline {
                return Some(Err(seen.into_iter().map(|(name, _)| name).collect()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until the route nodes in the graph are what `done` wants. `None` when `pw-dump` is not
    /// there to ask; `Some(Err(..))` with what was last seen when they never came to be.
    fn nodes_until_routes(
        &self,
        mut done: impl FnMut(&[RouteNode]) -> bool,
    ) -> Option<Result<Vec<RouteNode>, Vec<RouteNode>>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = self.route_nodes()?;
            if done(&seen) {
                return Some(Ok(seen));
            }
            if Instant::now() >= deadline {
                return Some(Err(seen));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Every `target.object` the `default` metadata holds: `(subject, value, type)`, sorted by
    /// subject, read the way `pw-metadata -n default` prints them —
    /// `update: id:20 key:'target.object' value:'301' type:'Spa:Id'`. `None` when `pw-metadata`
    /// is not there to ask.
    fn stream_targets(&self) -> Option<Vec<(u64, String, String)>> {
        let listing = self.tool("pw-metadata", &["-n", "default"])?;
        let field = |line: &str, name: &str| -> Option<String> {
            let start = line.find(&format!("{name}:'"))? + name.len() + 2;
            let end = start + line[start..].find('\'')?;
            Some(line[start..end].to_owned())
        };
        let mut targets: Vec<(u64, String, String)> = listing
            .lines()
            .filter(|line| line.contains("key:'target.object'"))
            .filter_map(|line| {
                let subject = line
                    .split("id:")
                    .nth(1)?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()?;
                Some((subject, field(line, "value")?, field(line, "type")?))
            })
            .collect();
        targets.sort();
        Some(targets)
    }

    /// Wait until the `target.object` keys are exactly `want`. `None` when `pw-metadata` is not
    /// there to ask; `Some(Err(..))` with what it last held when they never came to be.
    fn targets_settle_on(
        &self,
        want: &[(u64, String, String)],
    ) -> Option<Result<(), Vec<TargetKey>>> {
        let mut want = want.to_vec();
        want.sort();
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = self.stream_targets()?;
            if seen == want {
                return Some(Ok(()));
            }
            if Instant::now() >= deadline {
                return Some(Err(seen));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The key a stream moved onto the route node with serial `serial` carries.
fn moved(subject: u64, serial: u64) -> (u64, String, String) {
    (
        subject,
        serial.to_string(),
        app_routes::TARGET_OBJECT_TYPE.to_owned(),
    )
}

/// The serial of the route node called `name` among `nodes`.
fn serial(nodes: &[(String, u64)], name: &str) -> u64 {
    nodes
        .iter()
        .find(|(node, _)| node == name)
        .map(|(_, serial)| *serial)
        .unwrap_or_else(|| panic!("{name} among {nodes:?}"))
}

/// The description the engine gives a route: the lane's own in the language the engine speaks —
/// the desktop's, since these tests name none — and the preset.
fn description(direction: DeviceDirection, preset: &str) -> String {
    app_routes::route_description(direction, preset, locale::system_language().as_deref())
}

#[test]
fn a_running_player_with_a_rule_gets_a_route_pair_and_is_moved_onto_it_until_it_quits() {
    let Some(graph) = PrivateGraph::start("route") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, mut said, speakers) = engine_on(&graph);
    let Some(game) = start(
        &graph,
        "--playback",
        "t_game",
        "Battlefield 6",
        "application.process.binary = bf6.exe",
    ) else {
        return;
    };
    let _player = start(&graph, "--playback", "t_player", "mpv", "");
    let Some(game_id) = unless_skipped(graph.node_id("t_game"), "pw-dump", "the game's id") else {
        return;
    };
    assert!(
        reported_with(&mut said, &handle, game_id, None).is_some(),
        "the game should be listed before any rule names it"
    );

    // The rule names the game by its binary, in another case, as a rule written on Windows would.
    handle.send(UiToAudio::SetAppRoutes(vec![output_rule(
        AppKey {
            binary: "BF6.exe".to_owned(),
            ..AppKey::default()
        },
        "Gaming",
    )]));

    let output = RouteSlot::new(DeviceDirection::Output, 1);
    let (sink, play) = (output.node_name(), output.stream_name());
    let Some(nodes) = unless_skipped(
        graph.routes_settle_on(&[&sink, &play]),
        "pw-dump",
        "the route pair",
    ) else {
        return;
    };
    let nodes = nodes.unwrap_or_else(|seen| panic!("one route pair for Gaming, not {seen:?}"));

    // The pair, as the contract names and describes it.
    let prop = |node: &str, key: &str| graph.node_prop(node, key).flatten();
    assert_eq!(prop(&sink, "media.class").as_deref(), Some("Audio/Sink"));
    assert_eq!(
        prop(&sink, "node.description"),
        Some(description(DeviceDirection::Output, "Gaming"))
    );
    assert_eq!(
        prop(&sink, "node.link-group").as_deref(),
        Some("fxsound-route-o1")
    );
    assert_eq!(
        prop(&sink, "priority.session").as_deref(),
        Some(app_routes::ROUTE_PRIORITY_SESSION),
        "never a default"
    );
    assert_eq!(
        prop(&play, "media.class").as_deref(),
        Some("Stream/Output/Audio")
    );
    assert_eq!(
        prop(&play, "target.object").as_deref(),
        Some(speakers.as_str()),
        "the route plays to the output lane's device"
    );
    assert_eq!(
        prop(&play, "node.link-group").as_deref(),
        Some("fxsound-route-o1")
    );
    assert_eq!(
        prop(&play, "node.passive").as_deref(),
        Some("true"),
        "passive like the lane's own playback stream"
    );

    // The game is moved onto the route's sink: its node id, the sink's serial, as an id. The
    // player no rule names is not.
    let key = moved(game_id, serial(&nodes, &sink));
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(std::slice::from_ref(&key)),
        "pw-metadata",
        "the game's metadata target",
    ) {
        targets.unwrap_or_else(|seen| panic!("the game's target should be {key:?}, not {seen:?}"));
    }
    let listed = reported_with(&mut said, &handle, game_id, Some("Gaming"))
        .expect("the game should be reported on its route");
    assert!(
        listed
            .iter()
            .filter(|stream| u64::from(stream.id) != game_id)
            .all(|stream| stream.route.is_none()),
        "{listed:?}"
    );
    // And the route is no device of FxSound's.
    said.settle(&handle);
    assert!(
        last_device_list(&said)
            .iter()
            .all(|(name, _)| !name.starts_with(crate::ROUTE_NODE_PREFIX)),
        "{:?}",
        last_device_list(&said)
    );

    // The game quits: its key goes with its stream at once. The route is kept for the idle period
    // — a player between two tracks comes back to it — and then goes.
    drop(game);
    let quit = Instant::now();
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(&[]),
        "pw-metadata",
        "the game's key going with it",
    ) {
        targets.unwrap_or_else(|seen| panic!("no key should be left, not {seen:?}"));
    }
    if let Some(nodes) = unless_skipped(graph.route_nodes(), "pw-dump", "the idle route") {
        assert_eq!(
            nodes.len(),
            2,
            "kept while it has been unused for less than the idle period ({:?} so far)",
            quit.elapsed()
        );
    }
    if let Some(settled) = unless_skipped(graph.routes_settle_on(&[]), "pw-dump", "the route going")
    {
        settled.unwrap_or_else(|seen| panic!("the unused route should go, not stay as {seen:?}"));
        // Not before the idle period: the supervisor's tick finds it unused a moment after the game
        // went, and it goes on the first tick the period is up.
        assert!(
            quit.elapsed() >= IDLE - crate::engine::SUPERVISOR_PERIOD,
            "gone after {:?}, before the idle period of {IDLE:?} was up",
            quit.elapsed()
        );
    }
    handle.shutdown();
}

#[test]
fn a_rule_taken_away_moves_the_stream_back_and_takes_its_route_down_at_once() {
    let Some(graph) = PrivateGraph::start("routeoff") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    // Kept long, so that a route going now can only have gone because its rule did.
    let (handle, mut said, _) = engine_keeping_routes_for(&graph, PATIENCE * 10);
    let Some(_game) = start(&graph, "--playback", "t_game", "Game", "") else {
        return;
    };
    let Some(game_id) = unless_skipped(graph.node_id("t_game"), "pw-dump", "the game's id") else {
        return;
    };
    handle.send(UiToAudio::SetAppRoutes(vec![output_rule(
        named("Game"),
        "Gaming",
    )]));
    let sink = RouteSlot::new(DeviceDirection::Output, 1).node_name();
    let Some(Ok(nodes)) = unless_skipped(
        graph.routes_settle_on(&[&sink, "fxsound_route_o1_play"]),
        "pw-dump",
        "the route pair",
    ) else {
        panic!("the route pair should be built");
    };
    let key = moved(game_id, serial(&nodes, &sink));
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(std::slice::from_ref(&key)),
        "pw-metadata",
        "the game's metadata target",
    ) {
        targets.unwrap_or_else(|seen| panic!("{key:?} should be written, not {seen:?}"));
    }

    // No rule any more: the game, still playing, goes back, and the route goes.
    handle.send(UiToAudio::SetAppRoutes(Vec::new()));
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(&[]),
        "pw-metadata",
        "the game's key deleted",
    ) {
        targets.unwrap_or_else(|seen| panic!("the key should be deleted, not {seen:?}"));
    }
    if let Some(settled) = unless_skipped(graph.routes_settle_on(&[]), "pw-dump", "the route going")
    {
        settled.unwrap_or_else(|seen| panic!("the route should go at once, not stay as {seen:?}"));
    }
    assert!(
        reported_with(&mut said, &handle, game_id, None).is_some(),
        "the game should be reported back on its lane"
    );
    handle.shutdown();
}

#[test]
fn every_stream_is_moved_back_before_the_engine_exits() {
    let Some(graph) = PrivateGraph::start("routeexit") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, _said, _) = engine_on(&graph);
    let Some(_game) = start(&graph, "--playback", "t_game", "Game", "") else {
        return;
    };
    let Some(_browser) = start(&graph, "--playback", "t_browser", "Brave", "") else {
        return;
    };
    let (Some(game_id), Some(browser_id)) = (graph.node_id("t_game"), graph.node_id("t_browser"))
    else {
        skip("pw-dump could not say the applications' ids, so the exit was not checked");
        return;
    };
    handle.send(UiToAudio::SetAppRoutes(vec![
        output_rule(named("Game"), "Gaming"),
        output_rule(named("Brave"), "Volume Boost"),
    ]));
    let Some(Ok(nodes)) = unless_skipped(
        graph.routes_settle_on(&[
            "fxsound_route_o1",
            "fxsound_route_o1_play",
            "fxsound_route_o2",
            "fxsound_route_o2_play",
        ]),
        "pw-dump",
        "two route pairs",
    ) else {
        panic!("two route pairs should be built");
    };
    // The game started first, so its preset took the first number.
    let keys = [
        moved(game_id, serial(&nodes, "fxsound_route_o1")),
        moved(browser_id, serial(&nodes, "fxsound_route_o2")),
    ];
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(&keys),
        "pw-metadata",
        "both applications' targets",
    ) {
        targets.unwrap_or_else(|seen| panic!("{keys:?} should be written, not {seen:?}"));
    }

    // `shutdown` returns once the thread is gone: the keys must be gone by then, not a moment
    // later, and the applications — still playing — left with no target of FxSound's.
    handle.shutdown();
    if let Some(targets) = unless_skipped(graph.stream_targets(), "pw-metadata", "the keys") {
        assert!(
            targets.is_empty(),
            "every key should be deleted before the engine is gone: {targets:?}"
        );
    }
    if let Some(nodes) = unless_skipped(graph.route_nodes(), "pw-dump", "the routes") {
        assert!(nodes.is_empty(), "{nodes:?}");
    }
    assert!(graph.node_id("t_game").is_some() && graph.node_id("t_browser").is_some());
}

#[test]
fn a_stream_that_may_not_move_is_left_alone_and_gets_no_route() {
    let Some(graph) = PrivateGraph::start("routepin") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, mut said, _) = engine_on(&graph);
    let Some(_kiosk) = start(
        &graph,
        "--playback",
        "t_kiosk",
        "Kiosk",
        "node.dont-move = true",
    ) else {
        return;
    };
    let Some(_pinned) = start(
        &graph,
        "--playback",
        "t_pinned",
        "Pinned",
        "target.object = t_71",
    ) else {
        return;
    };
    // A marker whose route shows the rules have been acted on.
    let Some(_marker) = start(&graph, "--playback", "t_marker", "Marker", "") else {
        return;
    };
    let Some(marker_id) = unless_skipped(graph.node_id("t_marker"), "pw-dump", "the marker's id")
    else {
        return;
    };
    handle.send(UiToAudio::SetAppRoutes(vec![
        output_rule(named("Kiosk"), "Gaming"),
        output_rule(named("Pinned"), "Gaming"),
        output_rule(named("Marker"), "Movies"),
    ]));
    assert!(
        reported_with(&mut said, &handle, marker_id, Some("Movies")).is_some(),
        "the marker should be moved onto its route"
    );
    if let Some(nodes) = unless_skipped(graph.route_nodes(), "pw-dump", "the routes") {
        let names: Vec<&str> = nodes.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec!["fxsound_route_o1", "fxsound_route_o1_play"],
            "no route for Gaming: neither of its applications may be moved"
        );
    }
    if let Some(targets) = unless_skipped(graph.stream_targets(), "pw-metadata", "the keys") {
        assert_eq!(
            targets
                .iter()
                .map(|(subject, ..)| *subject)
                .collect::<Vec<_>>(),
            vec![marker_id]
        );
    }
    handle.shutdown();
}

#[test]
fn a_recorder_with_a_rule_gets_an_input_route_behind_the_microphone_which_goes_with_the_lane() {
    let Some(graph) = PrivateGraph::start("routein") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, mut said, _) = engine_on(&graph);
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));
    let Some(_chat) = start(&graph, "--record", "t_chat", "Discord", "") else {
        return;
    };
    let Some(chat_id) = unless_skipped(graph.node_id("t_chat"), "pw-dump", "the call's id") else {
        return;
    };
    handle.send(UiToAudio::SetAppRoutes(vec![input_rule(
        named("Discord"),
        "Headset",
        "podcast",
    )]));
    let input = RouteSlot::new(DeviceDirection::Input, 1);
    let (source, capture) = (input.node_name(), input.stream_name());
    let Some(Ok(nodes)) = unless_skipped(
        graph.routes_settle_on(&[&capture, &source]),
        "pw-dump",
        "the input route pair",
    ) else {
        panic!("an input route pair should be built");
    };
    let prop = |node: &str, key: &str| graph.node_prop(node, key).flatten();
    assert_eq!(
        prop(&source, "media.class").as_deref(),
        Some("Audio/Source")
    );
    assert_eq!(
        prop(&source, "node.description"),
        Some(description(DeviceDirection::Input, "Headset"))
    );
    assert_eq!(
        prop(&capture, "media.class").as_deref(),
        Some("Stream/Input/Audio")
    );
    assert_eq!(
        prop(&capture, "target.object").as_deref(),
        Some("t_mic"),
        "the route records the input lane's microphone"
    );
    for node in [&source, &capture] {
        assert_eq!(
            prop(node, "node.link-group").as_deref(),
            Some("fxsound-route-i1")
        );
    }
    let key = moved(chat_id, serial(&nodes, &source));
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(std::slice::from_ref(&key)),
        "pw-metadata",
        "the call's metadata target",
    ) {
        targets.unwrap_or_else(|seen| panic!("{key:?} should be written, not {seen:?}"));
    }

    // The microphone lane is switched off: the call goes back, and the route goes with the lane.
    handle.send(UiToAudio::DetachLane(DeviceDirection::Input));
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(&[]),
        "pw-metadata",
        "the call's key",
    ) {
        targets.unwrap_or_else(|seen| panic!("the key should be deleted, not {seen:?}"));
    }
    if let Some(settled) = unless_skipped(
        graph.routes_settle_on(&[]),
        "pw-dump",
        "the input route going",
    ) {
        settled.unwrap_or_else(|seen| panic!("the route should go with its lane, not {seen:?}"));
    }
    handle.shutdown();
}

#[test]
fn a_fifth_preset_stays_on_the_lane_and_the_window_is_told_once() {
    let Some(graph) = PrivateGraph::start("routemax") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, mut said, _) = engine_on(&graph);
    let presets = ["One", "Two", "Three", "Four", "Five"];
    let mut apps = Vec::new();
    for (index, preset) in presets.iter().enumerate() {
        let Some(app) = start(
            &graph,
            "--playback",
            &format!("t_app{index}"),
            &format!("App {preset}"),
            "",
        ) else {
            return;
        };
        apps.push(app);
    }
    let Some(ids) = (0..presets.len())
        .map(|index| graph.node_id(&format!("t_app{index}")))
        .collect::<Option<Vec<u64>>>()
    else {
        skip("pw-dump could not say the applications' ids, so the limit was not checked");
        return;
    };
    handle.send(UiToAudio::SetAppRoutes(
        presets
            .iter()
            .map(|preset| output_rule(named(&format!("App {preset}")), preset))
            .collect(),
    ));

    let want: Vec<String> = (1..=MAX_ROUTES_PER_LANE)
        .flat_map(|number| {
            let slot = RouteSlot::new(DeviceDirection::Output, number);
            [slot.node_name(), slot.stream_name()]
        })
        .collect();
    let want: Vec<&str> = want.iter().map(String::as_str).collect();
    let Some(Ok(nodes)) =
        unless_skipped(graph.routes_settle_on(&want), "pw-dump", "four route pairs")
    else {
        panic!("four route pairs should be built, and no fifth");
    };
    // The four started first are moved, in the order they started; the fifth stays.
    let keys: Vec<(u64, String, String)> = ids[..MAX_ROUTES_PER_LANE]
        .iter()
        .enumerate()
        .map(|(index, &id)| moved(id, serial(&nodes, &format!("fxsound_route_o{}", index + 1))))
        .collect();
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(&keys),
        "pw-metadata",
        "four targets",
    ) {
        targets.unwrap_or_else(|seen| panic!("{keys:?} should be written, not {seen:?}"));
    }
    let warning = |message: &AudioToUi| {
        matches!(message, AudioToUi::Warning { direction: Some(DeviceDirection::Output), message }
            if message.contains("App Five"))
    };
    assert!(
        said.heard(&handle, "the warning about App Five", warning),
        "the window should be told why App Five stays on the lane"
    );
    // Told once, however many ticks it goes on not getting a route.
    std::thread::sleep(Duration::from_millis(800));
    said.settle(&handle);
    assert_eq!(said.0.iter().filter(|message| warning(message)).count(), 1);
    handle.shutdown();
}

/// Build the route for `Game` on `graph`'s engine and wait until the game is on it. Returns the
/// route's virtual node's serial and the game's node id, or `None` when a tool could not say.
fn game_on_its_route(graph: &PrivateGraph, handle: &EngineHandle) -> Option<(u64, u64)> {
    let game_id = unless_skipped(graph.node_id("t_game"), "pw-dump", "the game's id")?;
    handle.send(UiToAudio::SetAppRoutes(vec![output_rule(
        named("Game"),
        "Gaming",
    )]));
    let nodes = unless_skipped(
        graph.routes_settle_on(&["fxsound_route_o1", "fxsound_route_o1_play"]),
        "pw-dump",
        "the route pair",
    )?
    .unwrap_or_else(|seen| panic!("one route pair, not {seen:?}"));
    let serial = serial(&nodes, "fxsound_route_o1");
    let key = moved(game_id, serial);
    unless_skipped(
        graph.targets_settle_on(std::slice::from_ref(&key)),
        "pw-metadata",
        "the game's target",
    )?
    .unwrap_or_else(|seen| panic!("{key:?} should be written, not {seen:?}"));
    Some((serial, game_id))
}

#[test]
fn a_route_follows_its_lane_to_other_speakers_and_the_stream_follows_the_route() {
    let Some(graph) = PrivateGraph::start("routemove") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, mut said, first) = engine_on(&graph);
    let Some(_game) = start(&graph, "--playback", "t_game", "Game", "") else {
        return;
    };
    let Some((before, game_id)) = game_on_its_route(&graph, &handle) else {
        return;
    };
    let other = if first == "t_71" { "t_stereo" } else { "t_71" };
    handle.send(UiToAudio::SelectDevice {
        node_name: other.to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some(other)));

    // The route is rebuilt on the new speakers — a new node, at their format — and the game is
    // moved onto the new node.
    let Some(Ok(())) = unless_skipped(
        graph.prop_settles_on("fxsound_route_o1_play", "target.object", other),
        "pw-dump",
        "the route's new device",
    ) else {
        panic!("the route should follow its lane to {other}");
    };
    let Some(Ok(nodes)) = unless_skipped(
        graph.routes_settle_on(&["fxsound_route_o1", "fxsound_route_o1_play"]),
        "pw-dump",
        "the rebuilt route",
    ) else {
        panic!("one route pair");
    };
    let after = serial(&nodes, "fxsound_route_o1");
    assert_ne!(after, before, "a new node");
    let channels = if other == "t_71" { "8" } else { "2" };
    assert_eq!(
        graph
            .node_prop("fxsound_route_o1", "audio.channels")
            .flatten()
            .as_deref(),
        Some(channels),
        "at the new speakers' format, as the lane's own pair"
    );
    let key = moved(game_id, after);
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(std::slice::from_ref(&key)),
        "pw-metadata",
        "the game's new target",
    ) {
        targets.unwrap_or_else(|seen| panic!("{key:?} should be written, not {seen:?}"));
    }
    handle.shutdown();
}

#[test]
fn a_restarted_connection_rebuilds_the_routes_and_takes_its_own_old_keys_over() {
    let Some(graph) = PrivateGraph::start("routerestart") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, mut said, _) = engine_on(&graph);
    let Some(_game) = start(&graph, "--playback", "t_game", "Game", "") else {
        return;
    };
    let Some((before, game_id)) = game_on_its_route(&graph, &handle) else {
        return;
    };

    // The connection goes, and every node of FxSound's with it. The server keeps the game's key,
    // naming a route node that no longer exists: FxSound's own, from before, and taken over.
    let from = said.0.len();
    handle.send(UiToAudio::Restart);
    assert!(
        said.heard_since(&handle, from, "the disconnect", |message| {
            matches!(message, AudioToUi::Disconnected { .. })
        })
    );
    let Some(Ok(nodes)) = unless_skipped(
        graph.nodes_until_routes(|nodes| {
            nodes.len() == 2 && nodes.iter().all(|(_, serial)| *serial > before)
        }),
        "pw-dump",
        "the rebuilt route",
    ) else {
        panic!("the route should be rebuilt on the new connection");
    };
    let key = moved(game_id, serial(&nodes, "fxsound_route_o1"));
    if let Some(targets) = unless_skipped(
        graph.targets_settle_on(std::slice::from_ref(&key)),
        "pw-metadata",
        "the game's target on the new route",
    ) {
        targets.unwrap_or_else(|seen| panic!("{key:?} should be written, not {seen:?}"));
    }
    assert!(
        reported_with(&mut said, &handle, game_id, Some("Gaming")).is_some(),
        "and reported on it"
    );
    handle.shutdown();
}

#[test]
fn a_stream_moved_by_hand_is_left_where_the_user_put_it() {
    let Some(graph) = PrivateGraph::start("routehand") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, mut said, speakers) = engine_on(&graph);
    let Some(_game) = start(&graph, "--playback", "t_game", "Game", "") else {
        return;
    };
    let Some((_, game_id)) = game_on_its_route(&graph, &handle) else {
        return;
    };
    let elsewhere = if speakers == "t_71" {
        "t_stereo"
    } else {
        "t_71"
    };
    let Some(Some(elsewhere_serial)) = unless_skipped(
        graph.node_prop(elsewhere, "object.serial"),
        "pw-dump",
        "a serial",
    ) else {
        return;
    };

    // The user moves the game to other speakers in a mixer, the way `pipewire-pulse` writes it.
    let Some(_) = unless_skipped(
        graph.tool(
            "pw-metadata",
            &[
                "-n",
                "default",
                &game_id.to_string(),
                app_routes::TARGET_OBJECT_KEY,
                &elsewhere_serial,
                app_routes::TARGET_OBJECT_TYPE,
            ],
        ),
        "pw-metadata",
        "a move by hand",
    ) else {
        return;
    };
    let by_hand = (
        game_id,
        elsewhere_serial.clone(),
        app_routes::TARGET_OBJECT_TYPE.to_owned(),
    );
    assert!(
        reported_with(&mut said, &handle, game_id, None).is_some(),
        "the game is on no route of FxSound's any more"
    );
    // Several ticks later FxSound has not moved it back onto its route.
    said.settle(&handle);
    if let Some(targets) = unless_skipped(graph.stream_targets(), "pw-metadata", "the key") {
        assert_eq!(targets, vec![by_hand.clone()]);
    }
    // Nor does it delete the user's key when the rule goes, or on its way out.
    handle.send(UiToAudio::SetAppRoutes(Vec::new()));
    said.settle(&handle);
    if let Some(targets) = unless_skipped(graph.stream_targets(), "pw-metadata", "the key") {
        assert_eq!(targets, vec![by_hand.clone()]);
    }
    handle.shutdown();
    if let Some(targets) = unless_skipped(graph.stream_targets(), "pw-metadata", "the key") {
        assert_eq!(targets, vec![by_hand]);
    }
}

#[test]
fn new_parameters_for_a_preset_reach_its_route_without_a_new_pair() {
    let Some(graph) = PrivateGraph::start("routeparams") else {
        return;
    };
    if !can_run_applications() {
        return;
    }
    let (handle, mut said, _) = engine_on(&graph);
    let Some(_game) = start(&graph, "--playback", "t_game", "Game", "") else {
        return;
    };
    let Some((serial, game_id)) = game_on_its_route(&graph, &handle) else {
        return;
    };
    let Some(before) = unless_skipped(graph.route_nodes(), "pw-dump", "the route") else {
        return;
    };

    // The preset is saved with other settings, and the global output levels change: the app sends
    // the whole set again. The pair stays; the chain takes the new snapshot where it runs.
    let mut louder = output_rule(named("Game"), "Gaming");
    louder.params = RouteParams::Output(DspParams {
        master_gain_db: -6.0,
        mute: true,
        ..DspParams::default()
    });
    handle.send(UiToAudio::SetAppRoutes(vec![louder]));
    said.settle(&handle);
    if let Some(after) = unless_skipped(graph.route_nodes(), "pw-dump", "the route") {
        assert_eq!(after, before, "the same two nodes, serials and all");
    }
    if let Some(targets) = unless_skipped(graph.stream_targets(), "pw-metadata", "the key") {
        assert_eq!(
            targets,
            vec![moved(game_id, serial)],
            "and the game stays on it"
        );
    }
    handle.shutdown();
}
