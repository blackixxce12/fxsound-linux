//! FxSound's hook in WirePlumber (roadmap 0.5.0 §7, D5; `crate::wireplumber_hook`) on a private
//! graph's WirePlumber, installed as Settings ▸ Experimental installs it: that WirePlumber loads it
//! and it fades a stream WirePlumber moves; that one which cannot load leaves WirePlumber running;
//! and that it and FxSound's own handover never leave a stream at 0 between them. What the moves
//! sound like with it is [`clicks`](super::clicks)'.

use super::policy::PolicyGraph;
use super::*;
use crate::stream_handover::JOURNAL_FILE;
use crate::wireplumber_hook::Place;

/// What the hook logs for each stream it fades.
const FADING: &str = "fxsound: fading t_player out for its move";

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
    // After the move, 70 ms after the fade, and well before the volume comes back, a quarter of a
    // second after the move.
    std::thread::sleep(Duration::from_millis(170));
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
