//! FxSound's hook in WirePlumber (roadmap 0.5.0 §7, D5; `crate::wireplumber_hook`) on a private
//! graph's WirePlumber, installed as Settings ▸ Experimental installs it: that WirePlumber loads it
//! and it fades a stream WirePlumber moves; that one which cannot load leaves WirePlumber running;
//! and that it and FxSound's own handover never leave a stream at 0 between them. What the moves
//! sound like with it is [`clicks`](super::clicks)'.

use super::policy::PolicyGraph;
use super::*;
use crate::stream_handover::JOURNAL_FILE;
use crate::wireplumber_hook::{HELD_KEY, Place};

/// What the hook logs for each stream it fades.
const FADING: &str = "fxsound: fading t_player out for its move";

/// What the hook logs as it gives the stream its volume back.
const GIVING: &str = "fxsound: giving t_player its volume back";

/// A player that follows the default sink. `None`, said why, when a tool is missing.
fn following_player(graph: &PolicyGraph) -> Option<apps::App> {
    if let Some(missing) = ["pw-cat", "pw-dump", "pw-metadata"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not installed, so FxSound's hook in WirePlumber was not checked"
        ));
        return None;
    }
    Some(
        graph
            .pw_cat("--playback", "t_player", "application.name = t_player", &[])
            .expect("pw-cat should play"),
    )
}

/// Wait until a link runs from the node called `from` to the node called `to`. Whether it did.
fn links(graph: &PolicyGraph, from: &str, to: &str) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if PrivateGraph::linked(graph, from, to) == Some(true) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("{from} was never linked to {to}");
    false
}

/// Wait until the node called `name` has a master volume within a hair of `want`. Whether it did.
fn volume_settles_on(graph: &PolicyGraph, name: &str, want: f64) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if graph
            .node_volume(name)
            .flatten()
            .is_some_and(|volume| (volume - want).abs() < 1e-3)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(
        "{name} never came to a volume of {want}: {:?}",
        graph.node_volume(name)
    );
    false
}

/// Wait until the graph's WirePlumber has logged `line` `times` times. Whether it did.
fn logged(graph: &PolicyGraph, line: &str, times: usize) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if graph.session_manager_log().matches(line).count() >= times {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(
        "WirePlumber never logged {line:?} {times} times:\n{}",
        graph.session_manager_log()
    );
    false
}

/// Whether the `default` metadata says the hook holds a stream ([`HELD_KEY`]). `None` when
/// `pw-metadata` could not say.
fn hook_holds_any(graph: &PolicyGraph) -> Option<bool> {
    graph
        .tool("pw-metadata", &["-n", "default"])
        .map(|listing| listing.contains(&format!("key:'{HELD_KEY}'")))
}

/// The master volume WirePlumber keeps for the application `name`'s next stream: its
/// `node/state-stream.lua`'s, in the graph's state directory, under the key it forms for a
/// player. `None` until it has kept one.
fn kept_volume(graph: &PolicyGraph, name: &str) -> Option<f64> {
    let state =
        std::fs::read_to_string(graph.dir.join("state/wireplumber/stream-properties")).ok()?;
    let line = state
        .lines()
        .find_map(|line| line.strip_prefix(&format!("Output/Audio:application.name:{name}=")))?;
    let after = &line[line.find("\"volume\":")? + "\"volume\":".len()..];
    let end = after
        .find(|c: char| c != '.' && c != '-' && !c.is_ascii_digit())
        .unwrap_or(after.len());
    after[..end].trim().parse().ok()
}

/// `pw-cli`, kept running with its commands on a pipe, to have a stream send its `Props` again at
/// a moment of the test's choosing: a millisecond or so after it is asked, where a `pw-cli` started
/// for each would take tens.
struct Prodder {
    stdin: std::process::ChildStdin,
    _cli: Guarded,
}

impl Prodder {
    /// `None`, said why, when it could not be started.
    fn start(graph: &PolicyGraph) -> Option<Self> {
        let mut command = support::command("pw-cli");
        command
            .arg("-r")
            .arg(graph.socket())
            .env("XDG_RUNTIME_DIR", graph.dir.join("run"))
            .env("PIPEWIRE_REMOTE", graph.socket())
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        graph.stderr_log(&mut command, "pw-cli-prodder");
        let mut cli = support::spawn(command)
            .inspect_err(|error| println!("pw-cli could not be started: {error}"))
            .ok()?;
        let stdin = cli.take_stdin()?;
        let mut prodder = Self { stdin, _cli: cli };
        // Its first command may come before it knows the graph's objects: one to spare, and time
        // to learn them.
        prodder.prod(0, 1);
        std::thread::sleep(Duration::from_millis(500));
        Some(prodder)
    }

    /// Have the node `id` send its `Props` again: its monitor's mute, which a playback stream's
    /// sound does not pass through, turned on for an odd `n`, off for an even one. The volume it
    /// reports is where its converter's ramp has come to.
    fn prod(&mut self, id: u64, n: usize) {
        let _ = writeln!(
            self.stdin,
            "set-param {id} Props {{ monitorMute: {} }}",
            n % 2 == 1
        );
        let _ = self.stdin.flush();
    }
}

/// Wait, a millisecond at a time, until the graph's WirePlumber has logged `line`. Whether it did.
fn logs_now(graph: &PolicyGraph, line: &str) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if graph.session_manager_log().contains(line) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    println!(
        "WirePlumber never logged {line:?}:\n{}",
        graph.session_manager_log()
    );
    false
}

/// Pick `sink` as the desktop does.
fn desktop_picks(graph: &PolicyGraph, sink: &str) {
    assert!(
        graph
            .write_default(
                devices::configured_default_key(DeviceDirection::Output),
                sink
            )
            .is_some(),
        "pw-metadata could not pick {sink}"
    );
}

/// Installed where the graph's WirePlumber reads it, the hook is loaded, fades a playing stream
/// WirePlumber moves to another sink and back, and gives it its volume back each time.
#[test]
fn wireplumber_loads_fxsounds_hook_and_it_gives_a_stream_it_faded_its_volume_back() {
    let Some(mut graph) = PolicyGraph::start_with_the_hook("wphook") else {
        return;
    };
    let Some(_player) = following_player(&graph) else {
        return;
    };
    assert!(links(&graph, "t_player", "t_stereo"));
    assert!(
        !graph.session_manager_log().contains(FADING),
        "the first link is not a move"
    );

    desktop_picks(&graph, "t_other");
    assert!(links(&graph, "t_player", "t_other"));
    assert!(logged(&graph, FADING, 1), "the hook did not fade the move");
    assert!(volume_settles_on(&graph, "t_player", 1.0));

    desktop_picks(&graph, "t_stereo");
    assert!(links(&graph, "t_player", "t_stereo"));
    assert!(
        logged(&graph, FADING, 2),
        "the hook did not fade the move back"
    );
    assert!(volume_settles_on(&graph, "t_player", 1.0));
    assert!(graph.session_manager_runs());
}

/// A stream whose volume the user lowered comes back to that volume, not to full.
#[test]
fn a_stream_the_hook_faded_comes_back_to_its_own_volume() {
    let Some(graph) = PolicyGraph::start_with_the_hook("wpvol") else {
        return;
    };
    let Some(_player) = following_player(&graph) else {
        return;
    };
    assert!(links(&graph, "t_player", "t_stereo"));
    assert!(graph.set_node_volume("t_player", 0.5).is_some());
    assert!(volume_settles_on(&graph, "t_player", 0.5));

    desktop_picks(&graph, "t_other");
    assert!(links(&graph, "t_player", "t_other"));
    assert!(logged(&graph, FADING, 1));
    assert!(volume_settles_on(&graph, "t_player", 0.5));
}

/// A player closed while the hook holds it silent leaves WirePlumber its own volume to keep, not
/// the hook's 0. WirePlumber keeps a stream's master volume for the application's next stream
/// (`node/state-stream.lua`), and a 0 kept there would play every later stream of the application
/// silent while the desktop's mixer, which shows the channel volumes only, says 100 %. The
/// longest hold, and so the one a player is likeliest to be closed in — stopped, or a browser's
/// tab closed: a move off one of FxSound's nodes, after which the hook waits a quarter of a second
/// for FxSound to take the default back. A sink named as FxSound's stands in for it here, and
/// nothing takes the default back. A PulseAudio application, as most of a desktop's are: `pw-cat`
/// sets its own volume once it plays, over what WirePlumber gave it, and would hide what
/// WirePlumber kept.
#[test]
fn a_player_closed_while_the_hook_holds_it_silent_starts_again_at_its_own_volume() {
    const PLAYER: &str = "t_pulse_player";
    const STAND_IN: &str = "fxsound_t_stand_in";
    let Some(mut graph) = PolicyGraph::start_with_the_hook("wpgone") else {
        return;
    };
    if !graph.start_pulse() {
        return;
    }
    if let Some(missing) = ["pw-cli", "pw-dump", "pw-metadata"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not installed, so a player closed while the hook held it was not checked"
        ));
        return;
    }
    assert!(
        graph
            .add_adapter(
                STAND_IN,
                &format!(
                    "factory.name = support.null-audio-sink node.name = {STAND_IN} \
                     media.class = Audio/Sink audio.channels = 2 audio.position = [ FL FR ] \
                     priority.session = 1000 priority.driver = 1000 device.api = alsa"
                ),
            )
            .is_some(),
        "the stand-in for FxSound's sink was not made"
    );
    desktop_picks(&graph, STAND_IN);
    let player = graph.pulse_player(PLAYER, None).expect("pacat should play");
    assert!(links(&graph, PLAYER, STAND_IN));
    assert!(graph.set_node_volume(PLAYER, 0.5).is_some());
    assert!(volume_settles_on(&graph, PLAYER, 0.5));

    desktop_picks(&graph, "t_other");
    let fading = format!("fxsound: fading {PLAYER} out for its move");
    let deadline = Instant::now() + PATIENCE;
    while !graph.session_manager_log().contains(&fading) {
        assert!(
            Instant::now() < deadline,
            "the hook did not fade the move:\n{}",
            graph.session_manager_log()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let faded = Instant::now();
    // The hook says it holds the player: FxSound's handover leaves it alone meanwhile.
    assert_eq!(
        hook_holds_any(&graph),
        Some(true),
        "the hook did not say in the default metadata that it held the player"
    );
    // After the move, 70 ms after the fade, and well before the volume comes back, a quarter of a
    // second after the move.
    if let Some(left) = Duration::from_millis(170).checked_sub(faded.elapsed()) {
        std::thread::sleep(left);
    }
    drop(player);
    assert!(
        logged(
            &graph,
            &format!("fxsound: {PLAYER} went away while silent for its move"),
            1
        ),
        "the player was not closed while the hook held it silent"
    );
    let deadline = Instant::now() + PATIENCE;
    while graph.node_id(PLAYER).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    let _again = graph
        .pulse_player(PLAYER, None)
        .expect("pacat should play again");
    assert!(links(&graph, PLAYER, "t_other"));
    assert!(
        volume_settles_on(&graph, PLAYER, 0.5),
        "{PLAYER}, started again, was not given its own volume"
    );
    // Given the time to have been put at anything else.
    std::thread::sleep(Duration::from_secs(1));
    assert!(volume_settles_on(&graph, PLAYER, 0.5));
    // And the hook's word that it held the player went with the player.
    assert_eq!(hook_holds_any(&graph), Some(false));
    assert!(graph.session_manager_runs());
}

/// PipeWire's converter reports a point of a ramp when a stream's `Props` are sent again in the
/// middle of one, and may never report its end. Sent again in the middle of the hook's fade, the
/// stream's last report may be a point on the way down while it plays at 0: taken for someone
/// else's volume, the hook would give nothing back, and the stream would stay silent while every
/// mixer says 100 %. Sent again in the middle of its ramp back, the last report is a point on the
/// way up — 0.37 of 0.5, every time, before the fix: kept by WirePlumber for the application's
/// next stream, and read by the next move as the stream's volume. The hook takes a point of its
/// own ramps for its own, keeps it from WirePlumber's state, and says the volume once more at the
/// end of the ramp back, until the server reports it. At a quantum of 64 frames, each ramp spans a
/// dozen cycles and more, and a report in the middle of one is a point on the way.
#[test]
fn a_stream_whose_ramps_are_reported_part_way_gets_its_own_volume_back_and_wireplumber_keeps_that()
{
    const PLAYER: &str = "t_player";
    let Some(mut graph) = PolicyGraph::start_with_the_hook("wpmid") else {
        return;
    };
    if let Some(missing) = ["pw-cat", "pw-cli", "pw-dump", "pw-metadata"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not installed, so a ramp reported part way was not checked"
        ));
        return;
    }
    let _player = graph
        .pw_cat(
            "--playback",
            PLAYER,
            "application.name = t_player",
            &["--latency", "64"],
        )
        .expect("pw-cat should play");
    assert!(links(&graph, PLAYER, "t_stereo"));
    assert!(graph.set_node_volume(PLAYER, 0.5).is_some());
    assert!(volume_settles_on(&graph, PLAYER, 0.5));
    let id = graph.node_id(PLAYER).expect("the player's id");
    let mut prodder = Prodder::start(&graph).expect("pw-cli should run");

    desktop_picks(&graph, "t_other");
    assert!(logs_now(&graph, FADING), "the hook did not fade the move");
    // In the middle of the fade out, 20 ms long.
    for n in 1..=4 {
        std::thread::sleep(Duration::from_millis(3));
        prodder.prod(id, n);
    }
    assert!(
        logs_now(&graph, GIVING),
        "the hook did not give the player its volume back"
    );
    // In the middle of the ramp back, 50 ms long.
    for n in 1..=4 {
        std::thread::sleep(Duration::from_millis(9));
        prodder.prod(id, n);
    }
    assert!(links(&graph, PLAYER, "t_other"));
    assert!(
        volume_settles_on(&graph, PLAYER, 0.5),
        "the player was not given its own volume back"
    );
    // Given the time to have been put at anything else, and WirePlumber to write what it keeps.
    std::thread::sleep(Duration::from_secs(2));
    assert!(volume_settles_on(&graph, PLAYER, 0.5));
    let deadline = Instant::now() + PATIENCE;
    while kept_volume(&graph, PLAYER).is_none_or(|kept| (kept - 0.5).abs() > 1e-3)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        kept_volume(&graph, PLAYER).map(|kept| (kept * 1e3).round() / 1e3),
        Some(0.5),
        "WirePlumber keeps a point of the hook's ramps for the player's next stream"
    );
    let log = graph.session_manager_log();
    for wrong in [
        "it is theirs",
        "moved meanwhile",
        "never reported its volume back",
    ] {
        assert!(!log.contains(wrong), "{wrong}:\n{log}");
    }
    assert_eq!(hook_holds_any(&graph), Some(false));
    assert!(graph.session_manager_runs());
}

/// The hook's component is optional: a script missing, or failing to load on a WirePlumber that
/// changed, is logged and skipped, and WirePlumber runs and links as if it were not there. A
/// component WirePlumber requires would stop it — and the sound of the whole session with it.
#[test]
fn a_hook_that_cannot_load_leaves_wireplumber_running_and_linking() {
    /// What is done to the hook's files before WirePlumber starts.
    type Prepare = fn(&Place);
    let broken: [(&str, Prepare); 2] = [
        ("wpmiss", |hook| {
            hook.install().expect("the hook installs");
            std::fs::remove_file(hook.script()).expect("the script goes");
        }),
        ("wpbad", |hook| {
            hook.install().expect("the hook installs");
            std::fs::write(hook.script(), "Nothing.here ()\n").expect("the script is broken");
        }),
    ];
    for (tag, prepare) in broken {
        let Some(mut graph) = PolicyGraph::start_prepared(tag, prepare) else {
            return;
        };
        let Some(_player) = following_player(&graph) else {
            return;
        };
        assert!(graph.session_manager_runs(), "{tag}: WirePlumber stopped");
        assert!(links(&graph, "t_player", "t_stereo"), "{tag}");
        desktop_picks(&graph, "t_other");
        assert!(links(&graph, "t_player", "t_other"), "{tag}");
        assert!(volume_settles_on(&graph, "t_player", 1.0), "{tag}");
        let log = graph.session_manager_log();
        assert!(
            log.contains("optional component 'custom.fxsound.fade-on-move"),
            "{tag}: WirePlumber did not say it left the hook out:\n{log}"
        );
        assert!(!log.contains(FADING), "{tag}");
        assert!(graph.session_manager_runs(), "{tag}: WirePlumber stopped");
    }
}

/// FxSound's own handover and the hook, on the same stream: the power off and on, which FxSound
/// fades itself and the hook then finds silent and leaves alone; and the desktop's pick, which the
/// hook fades and after which FxSound takes the default back. However the two meet, the stream
/// ends at its volume and the journal empty.
#[test]
fn fxsounds_handover_and_the_hook_never_leave_a_stream_at_zero_between_them() {
    let Some(graph) = PolicyGraph::start_with_the_hook("wpboth") else {
        return;
    };
    let Some(_player) = following_player(&graph) else {
        return;
    };
    let dir = ScratchDir::new("wp-hook-both");
    let journal = dir.path().join("fxsound").join(JOURNAL_FILE);
    let handle = AudioEngine::start_with_journal(Some(&graph.remote()), journal.clone())
        .expect("the engine should start");
    let mut said = Transcript::default();
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(())),
        "FxSound never became the default sink"
    );
    assert!(links(&graph, "t_player", SINK_NODE_NAME));
    assert!(volume_settles_on(&graph, "t_player", 1.0));

    for want in [false, true, false, true] {
        handle.send(UiToAudio::SetAsDefault {
            direction: DeviceDirection::Output,
            want,
        });
        let (default, sink) = if want {
            (SINK_NODE_NAME, SINK_NODE_NAME)
        } else {
            ("t_stereo", "t_stereo")
        };
        assert_eq!(
            graph.default_settles_on(DeviceDirection::Output, default),
            Some(Ok(()))
        );
        assert!(links(&graph, "t_player", sink));
        assert!(volume_settles_on(&graph, "t_player", 1.0), "power {want}");
    }

    for sink in ["t_other", "t_stereo"] {
        desktop_picks(&graph, sink);
        assert!(said.attached(&handle, DeviceDirection::Output, Some(sink)));
        assert_eq!(
            graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
            Some(Ok(())),
            "FxSound did not take the default back after the desktop's pick of {sink}"
        );
        assert!(links(&graph, "t_player", SINK_NODE_NAME));
        assert!(volume_settles_on(&graph, "t_player", 1.0), "pick {sink}");
    }
    // Give a late timer of either the time to do something wrong.
    std::thread::sleep(Duration::from_secs(2));
    assert!(volume_settles_on(&graph, "t_player", 1.0));
    let deadline = Instant::now() + PATIENCE;
    while journal.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!journal.exists(), "a volume was left in the journal");
    handle.shutdown();
}
