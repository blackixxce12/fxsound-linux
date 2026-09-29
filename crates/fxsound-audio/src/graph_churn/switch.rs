//! Another device, the same virtual node (roadmap 0.5.0 §7, D3): a lane that moves to a device at
//! the format it already runs at keeps `fxsound_sink` or `fxsound_source`, and replaces only its
//! stream on the device (`crate::engine`, "Another device, the same virtual node"). What the move
//! sounds like is [`clicks`](super::clicks)'; this is where the nodes and the applications' links
//! end up.

use super::policy::PolicyGraph;
use super::*;

/// The ids of every link from the node called `from` to the node called `to`, sorted. `None` when
/// `pw-dump` is not there to ask; empty when either node is not in the graph.
fn links_between(graph: &PrivateGraph, from: &str, to: &str) -> Option<Vec<u64>> {
    Some(links_in(&graph.dump()?, from, to))
}

/// [`links_between`], in a dump already taken.
fn links_in(objects: &[serde_json::Value], from: &str, to: &str) -> Vec<u64> {
    let id = |name| PrivateGraph::node_object(objects, name).and_then(|node| node["id"].as_u64());
    let (Some(from), Some(to)) = (id(from), id(to)) else {
        return Vec::new();
    };
    let mut links: Vec<u64> = objects
        .iter()
        .filter(|object| {
            object["type"].as_str() == Some("PipeWire:Interface:Link")
                && object["info"]["output-node-id"].as_u64() == Some(from)
                && object["info"]["input-node-id"].as_u64() == Some(to)
        })
        .filter_map(|object| object["id"].as_u64())
        .collect();
    links.sort_unstable();
    links
}

/// Wait until a link runs from the node called `from` to the node called `to`, and say which. An
/// empty list when none came within the patience.
fn links_settle(graph: &PrivateGraph, from: &str, to: &str) -> Vec<u64> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let links = links_between(graph, from, to).unwrap_or_default();
        if !links.is_empty() || Instant::now() >= deadline {
            return links;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Picking other speakers and another microphone at the same format keeps FxSound's sink and its
/// source — the same nodes, by their serials — and replaces only the playback stream and the
/// capture stream, each on the new device. 0.4.0 took both nodes of each lane down and built them
/// again, and every application on them was moved away and back.
#[test]
fn other_speakers_and_another_microphone_at_the_same_format_keep_fxsounds_sink_and_source() {
    let Some(graph) = PrivateGraph::start("keepnode") else {
        return;
    };
    for (name, direction) in [
        ("t_second", DeviceDirection::Output),
        ("t_mic_b", DeviceDirection::Input),
    ] {
        let Some(()) = unless_skipped(
            graph.add_device(name, direction),
            "pw-cli",
            "whether a move to another device keeps FxSound's own nodes",
        ) else {
            return;
        };
    }
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    for (direction, name) in [
        (DeviceDirection::Output, "t_stereo"),
        (DeviceDirection::Input, "t_mic"),
    ] {
        handle.send(UiToAudio::SelectDevice {
            node_name: name.to_owned(),
            direction,
        });
        assert!(said.attached(&handle, direction, Some(name)));
    }
    let Some(before) = unless_skipped(
        graph.settles_on(&LANE_NODE_NAMES),
        "pw-dump",
        "which of FxSound's nodes a move to another device replaces",
    ) else {
        handle.shutdown();
        return;
    };
    let before = before.expect("both pairs should be in the graph");

    for (direction, name) in [
        (DeviceDirection::Output, "t_second"),
        (DeviceDirection::Input, "t_mic_b"),
    ] {
        handle.send(UiToAudio::SelectDevice {
            node_name: name.to_owned(),
            direction,
        });
        assert!(said.attached(&handle, direction, Some(name)));
    }
    let after = graph
        .nodes_until(|nodes| {
            nodes.len() == LANE_NODE_NAMES.len()
                && [OUTPUT_NODE_NAME, CAPTURE_NODE_NAME]
                    .into_iter()
                    .all(|node| serial_of(nodes, node) != serial_of(&before, node))
        })
        .expect("pw-dump answered a moment ago")
        .expect("the streams on the devices should have been replaced");
    for node in [SINK_NODE_NAME, SOURCE_NODE_NAME] {
        assert_eq!(
            serial_of(&after, node),
            serial_of(&before, node),
            "{node} was replaced for a device at the same format"
        );
    }
    for (node, target) in [
        (OUTPUT_NODE_NAME, "t_second"),
        (CAPTURE_NODE_NAME, "t_mic_b"),
    ] {
        assert_eq!(
            graph.node_prop(node, "target.object").flatten().as_deref(),
            Some(target),
            "{node} is not on the new device"
        );
    }
    // Nothing went, so nothing of the claims was in question.
    for direction in DeviceDirection::ALL {
        if let Some(settled) = unless_skipped(
            graph.default_settles_on(direction, our_node_name(direction)),
            "pw-metadata",
            "the claims across a move to another device",
        ) {
            assert_eq!(
                settled,
                Ok(()),
                "the default {} is no longer FxSound's",
                direction.key()
            );
        }
    }
    handle.shutdown();
}

/// A player that follows the default sink and a recorder that follows the default source, both
/// on FxSound, keep their links while FxSound moves both lanes to the other devices (roadmap §7,
/// test 3): the link from the player into `fxsound_sink` and the one from `fxsound_source` into
/// the recorder are the very links they were — WirePlumber never had them to move — and neither
/// is ever linked to a device of its own. In 0.4.0 the sink went, WirePlumber moved the player
/// onto the other speakers unprocessed, and back onto the new sink a moment later.
#[test]
fn applications_on_fxsound_keep_their_links_while_it_moves_to_other_devices() {
    let Some(graph) = PolicyGraph::start("keeplink") else {
        return;
    };
    if let Some(missing) = ["pw-cat", "pw-record", "pw-dump", "pw-metadata"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not installed, so the applications' links across a move to another \
             device were not checked"
        ));
        return;
    }
    let _player = graph
        .pw_cat("--playback", "t_player", "application.name = t_player", &[])
        .expect("pw-cat should play");
    let _recorder = graph
        .follow_default_recorder("t_recorder")
        .expect("pw-record should record");
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));
    for direction in DeviceDirection::ALL {
        assert_eq!(
            graph.default_settles_on(direction, our_node_name(direction)),
            Some(Ok(())),
            "FxSound never became the default {}",
            direction.key()
        );
    }
    let player = links_settle(&graph, "t_player", SINK_NODE_NAME);
    let recorder = links_settle(&graph, SOURCE_NODE_NAME, "t_recorder");
    assert!(!player.is_empty(), "the player never reached FxSound");
    assert!(!recorder.is_empty(), "the recorder never reached FxSound");
    let before = graph.our_nodes().expect("pw-dump answered a moment ago");

    for (direction, name) in [
        (DeviceDirection::Output, "t_other"),
        (DeviceDirection::Input, "t_mic2"),
    ] {
        handle.send(UiToAudio::SelectDevice {
            node_name: name.to_owned(),
            direction,
        });
    }
    // FxSound's own streams find the new devices; the applications stay where they were.
    let deadline = Instant::now() + PATIENCE;
    let mut seen_elsewhere = Vec::new();
    loop {
        let objects = graph.dump().unwrap_or_default();
        for (from, to) in [
            ("t_player", "t_stereo"),
            ("t_player", "t_other"),
            ("t_mic", "t_recorder"),
            ("t_mic2", "t_recorder"),
        ] {
            if !links_in(&objects, from, to).is_empty() && !seen_elsewhere.contains(&(from, to)) {
                seen_elsewhere.push((from, to));
            }
        }
        let there = !links_in(&objects, OUTPUT_NODE_NAME, "t_other").is_empty()
            && !links_in(&objects, "t_mic2", CAPTURE_NODE_NAME).is_empty();
        if there || Instant::now() >= deadline {
            assert!(there, "FxSound's streams never reached the other devices");
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // And a moment more, for anything WirePlumber would do after.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        seen_elsewhere.is_empty(),
        "an application was moved off FxSound meanwhile: {seen_elsewhere:?}"
    );
    assert_eq!(
        links_between(&graph, "t_player", SINK_NODE_NAME),
        Some(player),
        "the player's link into FxSound's sink is not the one it had"
    );
    assert_eq!(
        links_between(&graph, SOURCE_NODE_NAME, "t_recorder"),
        Some(recorder),
        "the recorder's link from FxSound's source is not the one it had"
    );
    let after = graph.our_nodes().expect("pw-dump answered a moment ago");
    for node in [SINK_NODE_NAME, SOURCE_NODE_NAME] {
        assert_eq!(
            serial_of(&after, node),
            serial_of(&before, node),
            "{node} was replaced"
        );
    }
    handle.shutdown();
}

/// The desktop picks the other speakers with FxSound on and following the system's default device
/// (roadmap §7, test 4): WirePlumber moves the player onto them, FxSound follows with its sink kept
/// and takes the default back, and the player is on FxSound's same sink again, not on a new one.
#[test]
fn a_desktop_pick_with_fxsound_on_is_followed_with_its_sink_kept_and_the_default_taken_back() {
    let Some(graph) = PolicyGraph::start("keepdesk") else {
        return;
    };
    if let Some(missing) = ["pw-cat", "pw-dump", "pw-metadata"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not installed, so a desktop's pick with FxSound on was not checked"
        ));
        return;
    }
    let _player = graph
        .pw_cat("--playback", "t_player", "application.name = t_player", &[])
        .expect("pw-cat should play");
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(()))
    );
    assert!(!links_settle(&graph, "t_player", SINK_NODE_NAME).is_empty());
    let before = graph.our_nodes().expect("pw-dump answered a moment ago");

    assert!(
        graph
            .write_default(
                devices::configured_default_key(DeviceDirection::Output),
                "t_other"
            )
            .is_some()
    );
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_other")));
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(())),
        "FxSound did not take the default back"
    );
    assert!(
        !links_settle(&graph, "t_player", SINK_NODE_NAME).is_empty(),
        "the player is not back on FxSound"
    );
    assert!(
        !links_settle(&graph, OUTPUT_NODE_NAME, "t_other").is_empty(),
        "FxSound does not play to the speakers the desktop picked"
    );
    let after = graph.our_nodes().expect("pw-dump answered a moment ago");
    assert_eq!(
        serial_of(&after, SINK_NODE_NAME),
        serial_of(&before, SINK_NODE_NAME),
        "FxSound's sink was replaced for the desktop's pick"
    );
    assert_ne!(
        serial_of(&after, OUTPUT_NODE_NAME),
        serial_of(&before, OUTPUT_NODE_NAME),
        "FxSound's playback stream was not replaced on the new speakers"
    );
    handle.shutdown();
}
