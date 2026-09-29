//! The smooth handover as the power button uses it, with WirePlumber moving the streams (roadmap
//! 0.5.0 §7, D2): what the engine leaves of an application's volume when the handover is cut
//! short — by a kill, by the way out — and what it does not touch when the desktop moves a stream
//! with FxSound out of the sound. What the moves sound like is [`clicks`](super::clicks)'.

use super::policy::PolicyGraph;
use super::*;
use crate::stream_handover::JOURNAL_FILE;

/// Set, in the child half of [`a_run_killed_in_the_middle_of_a_handover_leaves_the_next_run_the_volume_to_put_back`],
/// to `<remote>\n<journal>`: the graph's socket and the journal the engine keeps.
const ENGINE_HALF: &str = "FXSOUND_HANDOVER_ENGINE_HALF";

/// The master volume of the node called `name`, as the server holds it; `None` when `pw-dump`
/// could not say, or the node lists none.
fn volume_of(graph: &PolicyGraph, name: &str) -> Option<f64> {
    graph.node_volume(name).flatten()
}

/// Wait until the node called `name` has a master volume within a hair of `want`. Whether it did.
fn volume_settles_on(graph: &PolicyGraph, name: &str, want: f64) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if volume_of(graph, name).is_some_and(|volume| (volume - want).abs() < 1e-3) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(
        "{name} never came to a volume of {want}: {:?}",
        volume_of(graph, name)
    );
    false
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

/// Wait until `file` is gone: every handover so far has given its volumes back. Whether it went
/// within the patience.
fn gone(file: &std::path::Path) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if !file.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Wait until `file` is there — a handover has noted a volume before fading it — checking every
/// millisecond. Whether it came within the patience.
fn appears(file: &std::path::Path) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if file.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

/// The PulseAudio application of [`a_run_killed_in_the_middle_of_a_handover_leaves_the_next_run_the_volume_to_put_back`].
const PULSE_PLAYER: &str = "t_pulse_player";

/// Wait until WirePlumber's state (`node/state-stream.lua`) keeps a master volume of 0 for the
/// output stream of the application `name`: what it gives the application's next stream. Whether
/// it did within the patience.
fn wireplumber_keeps_silence_for(graph: &PolicyGraph, name: &str) -> bool {
    let file = graph.dir.join("state/wireplumber/stream-properties");
    let key = format!("Output/Audio:application.name:{name}=");
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        let kept = std::fs::read_to_string(&file).is_ok_and(|state| {
            state.lines().any(|line| {
                line.strip_prefix(&key)
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                    .and_then(|value| value["volume"].as_f64())
                    .is_some_and(|volume| volume.abs() < 1e-4)
            })
        });
        if kept {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A player that follows the default sink, and the tools every test here reads the graph with.
/// `None`, said why, when a tool is missing.
fn following_player(graph: &PolicyGraph) -> Option<apps::App> {
    if let Some(missing) = ["pw-cat", "pw-dump", "pw-metadata"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not installed, so the power button's handover was not checked"
        ));
        return None;
    }
    Some(
        graph
            .pw_cat("--playback", "t_player", "application.name = t_player", &[])
            .expect("pw-cat should play"),
    )
}

/// An engine on `graph` that has taken the default sink, with the player linked to FxSound's sink.
fn engine_with_the_default(graph: &PolicyGraph, journal: PathBuf) -> EngineHandle {
    let handle = AudioEngine::start_with_journal(Some(&graph.remote()), journal.clone())
        .expect("the engine should start");
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(())),
        "FxSound never became the default sink"
    );
    assert!(links(graph, "t_player", SINK_NODE_NAME));
    // The claim moved the player onto FxSound, faded: over once its line is out of the journal.
    assert!(
        gone(&journal),
        "the claim's handover never gave the player its volume back"
    );
    handle
}

/// Roadmap §7, test 6: the way out in the middle of the power button's handover gives the player
/// its volume back before the connection goes, and leaves nothing in the journal.
#[test]
fn the_way_out_in_the_middle_of_a_handover_gives_the_stream_its_volume_back_first() {
    let Some(graph) = PolicyGraph::start("hoexit") else {
        return;
    };
    let Some(_player) = following_player(&graph) else {
        return;
    };
    let dir = ScratchDir::new("handover-exit");
    let journal = dir.path().join("fxsound").join(JOURNAL_FILE);
    let handle = engine_with_the_default(&graph, journal.clone());
    assert!(volume_settles_on(&graph, "t_player", 1.0));

    handle.send(UiToAudio::SetAsDefault {
        direction: DeviceDirection::Output,
        want: false,
    });
    assert!(
        appears(&journal),
        "the power off never faded the player, which follows the default"
    );
    handle.shutdown();
    assert!(
        volume_settles_on(&graph, "t_player", 1.0),
        "the way out left the player silent"
    );
    assert!(!journal.exists(), "the journal kept a volume that is back");
}

/// Roadmap §7, test 6: a run killed in the middle of the power button's handover leaves the players
/// at 0 — a `pw-cat` and a PulseAudio application — and WirePlumber then keeps the PulseAudio
/// application's 0 for its next stream, while the desktop's mixer, which shows the channel
/// volumes only, says 100 % (`d-power/evidence/x2-stuck-volume-pulse.txt` of the roadmap's
/// research). The next run finds the volumes in the journal and puts them back: on the `pw-cat`
/// that still plays, and on the PulseAudio application started again, silent from its start.
#[test]
fn a_run_killed_in_the_middle_of_a_handover_leaves_the_next_run_the_volume_to_put_back() {
    use std::io::{BufRead as _, BufReader};

    let Some(mut graph) = PolicyGraph::start("hokill") else {
        return;
    };
    if !graph.start_pulse() {
        return;
    }
    let Some(_player) = following_player(&graph) else {
        return;
    };
    let mut pulse = graph
        .pulse_player(PULSE_PLAYER, None)
        .expect("pacat should play");
    let dir = ScratchDir::new("handover-kill");
    let journal = dir.path().join("fxsound").join(JOURNAL_FILE);

    let (_, module) = module_path!()
        .split_once("::")
        .expect("a module of the crate");
    let name = format!("{module}::the_engine_half_of_a_killed_run_turns_the_power_off_when_asked");
    let mut process = support::command(std::env::current_exe().expect("this test binary"));
    process
        .args([name.as_str(), "--exact", "--test-threads=1", "--nocapture"])
        .env(
            ENGINE_HALF,
            format!("{}\n{}", graph.remote(), journal.display()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut half = support::spawn(process).expect("the engine half");
    let ready = BufReader::new(half.take_stdout().expect("piped"))
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains("engine half ready"));
    assert!(ready, "the engine half never started its engine");
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(())),
        "the engine half never became the default sink"
    );
    for player in ["t_player", PULSE_PLAYER] {
        assert!(links(&graph, player, SINK_NODE_NAME));
        assert!(volume_settles_on(&graph, player, 1.0));
    }
    assert!(
        gone(&journal),
        "the claim's handover never gave the players their volume back"
    );

    let stdin = half.stdin().expect("piped");
    writeln!(stdin).expect("the engine half reads its standard input");
    stdin.flush().expect("the line is out");
    assert!(
        appears(&journal),
        "the power off never faded the player, which follows the default"
    );
    // The fade is written with the journal, and its echo comes within a millisecond or two; the
    // move is 70 ms after it, and the volume comes back only after that.
    std::thread::sleep(Duration::from_millis(30));
    half.kill().expect("SIGKILL");
    half.wait().expect("the engine half ended");
    for player in ["t_player", PULSE_PLAYER] {
        assert!(
            volume_settles_on(&graph, player, 0.0),
            "the kill did not leave {player} where the handover had it"
        );
    }
    assert!(journal.exists(), "the killed run left no journal");

    // The PulseAudio application is closed and started again: WirePlumber has kept its 0, and
    // gives it to the new stream.
    assert!(
        wireplumber_keeps_silence_for(&graph, PULSE_PLAYER),
        "WirePlumber never kept the silence of {PULSE_PLAYER}"
    );
    drop(pulse);
    let deadline = Instant::now() + PATIENCE;
    while graph.node_id(PULSE_PLAYER).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    pulse = graph
        .pulse_player(PULSE_PLAYER, None)
        .expect("pacat should play again");
    assert!(
        volume_settles_on(&graph, PULSE_PLAYER, 0.0),
        "{PULSE_PLAYER}, started again, was not given the silence WirePlumber kept"
    );

    let next = AudioEngine::start_with_journal(Some(&graph.remote()), journal.clone())
        .expect("the engine should start");
    for player in ["t_player", PULSE_PLAYER] {
        assert!(
            volume_settles_on(&graph, player, 1.0),
            "the next run left {player} at 0"
        );
    }
    let deadline = Instant::now() + PATIENCE;
    while journal.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!journal.exists(), "the journal kept a volume that is back");
    next.shutdown();
    drop(pulse);
}

/// The engine half of the test above, run as a process of its own so that it can be killed: an
/// engine on the graph [`ENGINE_HALF`] names, keeping its journal where it says, that turns the
/// power off when a line comes on its standard input, and then waits to be killed. Passes at once
/// when run on its own.
#[test]
fn the_engine_half_of_a_killed_run_turns_the_power_off_when_asked() {
    let Some(said) = std::env::var_os(ENGINE_HALF) else {
        return;
    };
    let said = said.to_string_lossy().into_owned();
    let (remote, journal) = said.split_once('\n').expect("a remote and a journal");
    let handle = AudioEngine::start_with_journal(Some(remote), PathBuf::from(journal))
        .expect("the engine should start");
    println!("engine half ready");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    handle.send(UiToAudio::SetAsDefault {
        direction: DeviceDirection::Output,
        want: false,
    });
    // Until killed; or until the test that started it has gone, and its standard input with it.
    let _ = std::io::stdin().read_line(&mut line);
    std::thread::sleep(PATIENCE);
    handle.shutdown();
}

/// Roadmap §7, test 5: with the power off, the desktop picking another sink is WirePlumber's move
/// alone. The player goes to the sink picked, FxSound writes nothing to its volume and does not
/// take the default back; the lane, which follows the system's default device, goes there too,
/// and the app is told, for the next start (0.4.0 deferred).
#[test]
fn a_desktop_pick_with_the_power_off_moves_the_stream_without_fxsound() {
    let Some(graph) = PolicyGraph::start("hooff") else {
        return;
    };
    let Some(_player) = following_player(&graph) else {
        return;
    };
    let dir = ScratchDir::new("handover-off");
    let journal = dir.path().join("fxsound").join(JOURNAL_FILE);
    let handle = engine_with_the_default(&graph, journal.clone());
    let mut said = Transcript::default();
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));

    handle.send(UiToAudio::SetAsDefault {
        direction: DeviceDirection::Output,
        want: false,
    });
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, "t_stereo"),
        Some(Ok(())),
        "the power off never handed the default back"
    );
    assert!(links(&graph, "t_player", "t_stereo"));
    assert!(volume_settles_on(&graph, "t_player", 1.0));
    let from = said.0.len();

    assert!(
        graph
            .write_default(
                devices::configured_default_key(DeviceDirection::Output),
                "t_other"
            )
            .is_some(),
        "pw-metadata could not pick t_other"
    );
    assert!(
        links(&graph, "t_player", "t_other"),
        "the player did not follow the desktop's pick"
    );
    assert!(
        said.heard_since(&handle, from, "the desktop's pick told to the app", |message| {
            matches!(message, AudioToUi::DesktopPick { direction: DeviceDirection::Output, node_name }
                if node_name == "t_other")
        }),
        "the app was not told of the desktop's pick"
    );
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_other")));
    said.settle(&handle);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some("t_other"),
        "FxSound took the default back with its power off"
    );
    assert_eq!(
        volume_of(&graph, "t_player"),
        Some(1.0),
        "FxSound touched the volume of a stream it did not move"
    );
    assert!(!journal.exists(), "FxSound faded a stream it did not move");
    handle.shutdown();
}

/// The power switched off and straight back on, before the hand-back's turn came: FxSound keeps
/// the default, and the player, faded for a move that is not made, gets its volume back at once —
/// not after a second of waiting for a new link that is not coming.
#[test]
fn a_power_switched_back_on_before_its_hand_back_keeps_fxsound_the_default_and_the_volume() {
    let Some(graph) = PolicyGraph::start("hoback") else {
        return;
    };
    let Some(_player) = following_player(&graph) else {
        return;
    };
    let dir = ScratchDir::new("handover-back");
    let journal = dir.path().join("fxsound").join(JOURNAL_FILE);
    let handle = engine_with_the_default(&graph, journal.clone());
    assert!(volume_settles_on(&graph, "t_player", 1.0));

    for want in [false, true] {
        handle.send(UiToAudio::SetAsDefault {
            direction: DeviceDirection::Output,
            want,
        });
    }
    assert!(
        appears(&journal),
        "the power off never faded the player, which follows the default"
    );
    let faded = Instant::now();
    let deadline = faded + PATIENCE;
    while journal.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!journal.exists(), "the player never got its volume back");
    assert!(
        faded.elapsed() < crate::stream_handover::LINK_WAIT,
        "the volume came back only when the wait for a new link was up: {:?}",
        faded.elapsed()
    );
    assert_eq!(volume_of(&graph, "t_player"), Some(1.0));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some(SINK_NODE_NAME),
        "the hand-back was made after the power came back on"
    );
    assert_eq!(
        PrivateGraph::linked(&graph, "t_player", SINK_NODE_NAME),
        Some(true)
    );
    handle.shutdown();
}

/// Following the system's default device, the desktop picks another sink with the power on: the
/// lane moves there and FxSound takes the default back, moving the player off that sink and onto
/// FxSound again — a move of FxSound's own, faded like the power button's. The power going off
/// then hands the default to the sink the desktop picked, where the lane plays, and not to the one
/// FxSound displaced when it first took the default (roadmap §7, D2: where the default goes back to
/// in this mode).
#[test]
fn after_a_desktop_pick_the_power_off_hands_the_default_to_the_device_picked() {
    let Some(graph) = PolicyGraph::start("hopick") else {
        return;
    };
    let Some(_player) = following_player(&graph) else {
        return;
    };
    let dir = ScratchDir::new("handover-pick");
    let journal = dir.path().join("fxsound").join(JOURNAL_FILE);
    let handle = engine_with_the_default(&graph, journal.clone());
    let mut said = Transcript::default();
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));

    assert!(
        graph
            .write_default(
                devices::configured_default_key(DeviceDirection::Output),
                "t_other"
            )
            .is_some(),
        "pw-metadata could not pick t_other"
    );
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_other")));
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(())),
        "FxSound did not take the default back after the desktop's pick"
    );
    assert!(links(&graph, "t_player", SINK_NODE_NAME));
    assert!(volume_settles_on(&graph, "t_player", 1.0));

    handle.send(UiToAudio::SetAsDefault {
        direction: DeviceDirection::Output,
        want: false,
    });
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, "t_other"),
        Some(Ok(())),
        "the power off handed the default to another sink than the one the desktop picked"
    );
    assert!(links(&graph, "t_player", "t_other"));
    assert!(volume_settles_on(&graph, "t_player", 1.0));
    handle.shutdown();
}
