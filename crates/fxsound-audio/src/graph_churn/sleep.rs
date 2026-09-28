//! Sleep, both kinds, against a private PipeWire: the microphone's while nothing records from
//! FxSound (Input) (`docs/0.4.0-upstream.md` U19), and FxSound's while the system sleeps (U13).
//!
//! What the input lane's passive capture stream does is the server's business, not this crate's —
//! whether a recorder's link to the virtual source reaches the microphone through the input lane's
//! link-group is decided by PipeWire's scheduler — so it is watched here rather than assumed: the
//! nodes' states from `pw-dump`, and what a real `pw-record` on FxSound (Input) is handed. The
//! recorders run on a clock of their own, as an application runs on its device's, rather than on
//! the test's tone ([`PrivateGraph::clocked_recorder`] says why).
//!
//! What WirePlumber does with a Bluetooth headset cannot be run here, since there is no
//! WirePlumber. The one decision of its that matters — whether it switches the headset to its call
//! profile — depends on nothing but the graph, so it is transcribed from WirePlumber 0.5.17 and
//! asked of the graph this daemon holds ([`wireplumber_would_switch_to_headset`]).

use super::*;
use std::collections::HashSet;

/// The microphone of these tests when it stands in for a headset's: named and marked as
/// WirePlumber 0.5 makes a headset's loopback microphone
/// (`monitors/bluez/create-loopback-node.lua`), on a card id no card here has. A tone behind it, so
/// that it has something to say.
const LOOPBACK: &str = "bluez_input.00:11:22:33:44:55";

/// How much a recorder has to have written while the system sleeps before what it heard counts as
/// the sleep's silence: a tenth of a second of stereo.
///
/// No more, because what the server delivers in a given time is not fixed: a debug build's voice
/// chain on a machine busy with other test runs has been seen to deliver a quarter of a second of
/// audio in ten.
const HEARD: usize = 9_600;

/// Start an engine on `graph` with its input lane on `microphone`, and give the microphone, the
/// capture stream and the source the ports a session manager would, and link the microphone to
/// the capture stream as WirePlumber links it. Nothing records from the source yet.
fn engine_on_the_microphone(graph: &PrivateGraph, microphone: &str) -> (EngineHandle, Transcript) {
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list with the microphone",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == microphone)),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: microphone.to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some(microphone)));
    assert!(
        matches!(
            graph.nodes_until(|nodes| {
                nodes.iter().any(|(name, _)| *name == CAPTURE_NODE_NAME)
                    && nodes.iter().any(|(name, _)| *name == SOURCE_NODE_NAME)
            }),
            Some(Ok(_))
        ),
        "the input lane's pair should be in the graph"
    );
    for (node, direction, positions) in [
        (microphone, "Output", &["MONO"][..]),
        (CAPTURE_NODE_NAME, "Input", &["FL", "FR"][..]),
        (SOURCE_NODE_NAME, "Output", &["FL", "FR"][..]),
    ] {
        assert!(
            graph.configure_ports(node, direction, positions).is_some(),
            "{node} was not given ports"
        );
    }
    assert!(graph.link_nodes(microphone, CAPTURE_NODE_NAME));
    (handle, said)
}

/// Whether none of `nodes` runs, for twice as long as the output lane's playback stream is given
/// to fall asleep: long enough for anything that was going to wake them to have done so.
pub(super) fn none_runs_for_a_while(graph: &PrivateGraph, nodes: &[&str]) -> Result<(), String> {
    let quiet_until = Instant::now() + engine::SLEEP_AFTER * 2;
    while Instant::now() < quiet_until {
        for node in nodes {
            if graph.node_state(node).as_deref() == Some("running") {
                return Err(format!("{node} ran"));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// The tools every test here needs, and the tone: `false`, having said why, when they are not all
/// here.
fn tools_and_a_tone(graph: &PrivateGraph, tone: Option<&str>, what: &str) -> bool {
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link", "pw-record"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so {what} was not checked"
        ));
        return false;
    }
    if let Some(tone) = tone
        && graph.add_tone(tone).is_none()
    {
        skip(&format!(
            "the tone never appeared (is audiotestsrc installed?), so {what} was not checked"
        ));
        return false;
    }
    true
}

#[test]
fn a_microphone_runs_only_while_something_records_from_fxsound_input() {
    again_if_a_tone_ran_dry(|| {
        let Some(graph) = PrivateGraph::start("u19") else {
            return;
        };
        if !tools_and_a_tone(&graph, Some("t_tone"), "the microphone's idle") {
            return;
        }
        let (mut handle, _said) = engine_on_the_microphone(&graph, "t_tone");
        assert_eq!(
            graph.node_prop(CAPTURE_NODE_NAME, "node.passive"),
            Some(Some("true".to_owned())),
            "the capture stream should be passive on a server that runs a link-group together"
        );
        // An application that will record from FxSound (Input), running already — on its own clock,
        // as a real one runs on its device's — and not recording from it yet.
        let recorder = graph
            .clocked_recorder("t_recorder")
            .expect("a recorder for FxSound (Input)");

        // Linked to its microphone, and nobody recording: in 0.3.0 that ran the microphone for as long
        // as FxSound was open. Now neither it nor the pair runs, and the lane hears nothing.
        assert_eq!(
            none_runs_for_a_while(&graph, &["t_tone", CAPTURE_NODE_NAME, SOURCE_NODE_NAME]),
            Ok(()),
            "the microphone was held open with nothing recording from FxSound (Input)"
        );
        assert!(
            handle.meters(DeviceDirection::Input).input_peak < 1e-3,
            "the input lane heard the microphone with nothing recording from it"
        );

        // It records from FxSound (Input): the source runs, the input lane's group runs the capture
        // stream with it, and the capture stream's link runs the microphone — and what the application
        // is handed is the processed microphone.
        let from = recorder.written();
        assert!(graph.link_nodes(SOURCE_NODE_NAME, &recorder.name));
        for node in [SOURCE_NODE_NAME, CAPTURE_NODE_NAME, "t_tone"] {
            assert_eq!(
                graph.runs_until(node, true).map(drop),
                Ok(()),
                "{node} should run while something records from FxSound (Input)"
            );
        }
        assert!(
            meters_until(&mut handle, DeviceDirection::Input, |m| m.input_peak > 0.1).is_some(),
            "the tone never reached the input lane"
        );
        assert!(
            recorder.hears_since(from),
            "the recorder was not handed the microphone"
        );

        // It stops recording, and the microphone goes idle with the pair.
        assert!(graph.unlink_nodes(SOURCE_NODE_NAME, &recorder.name));
        for node in [SOURCE_NODE_NAME, CAPTURE_NODE_NAME, "t_tone"] {
            assert_eq!(
                graph.runs_until(node, false).map(drop),
                Ok(()),
                "{node} should stop once nothing records from FxSound (Input)"
            );
        }
        assert_eq!(
            none_runs_for_a_while(&graph, &["t_tone", CAPTURE_NODE_NAME]),
            Ok(()),
            "the microphone woke again with nothing recording"
        );

        // And the next recording wakes it again, with the pair that was built for the first.
        let before = graph.our_nodes().expect("pw-dump answered a moment ago");
        let from = recorder.written();
        assert!(graph.link_nodes(SOURCE_NODE_NAME, &recorder.name));
        for node in [SOURCE_NODE_NAME, CAPTURE_NODE_NAME, "t_tone"] {
            assert_eq!(
                graph.runs_until(node, true).map(drop),
                Ok(()),
                "{node} should wake for the next recording"
            );
        }
        // Where the tone follows, what the recording is handed is heard. Where it has to drive
        // (PipeWire before `TONE_FOLLOWS_SINCE`), starting its group again is what runs it out of
        // buffers ([`PrivateGraph::add_tone`]) — on PipeWire 1.0.5 in CI, at this second start in
        // all three runs of one test — and the recording then hears the silence of the tone, not
        // of the lane. That is let pass only with the daemon saying so, and said; a silence it does
        // not account for fails here as everywhere.
        if !recorder.hears_since(from) {
            assert!(
                !graph.a_following_tone_is_heard() && graph.tone_ran_dry(),
                "the next recording was not handed the microphone"
            );
            note(&format!(
                "on PipeWire {:?}, older than {TONE_FOLLOWS_SINCE:?}, the driving tone ran out of \
                 buffers as the group started again, so what the next recording was handed was \
                 not checked; that the nodes ran again and nothing was rebuilt was",
                graph.server_version()
            ));
        }
        assert_eq!(
            graph.our_nodes(),
            Some(before),
            "sleeping and waking is the server's business: nothing was rebuilt"
        );
        handle.shutdown();
    });
}

#[test]
fn holding_the_microphone_awake_runs_it_with_nobody_recording_and_lets_it_sleep_after() {
    again_if_a_tone_ran_dry(|| {
        let Some(graph) = PrivateGraph::start("keepawake") else {
            return;
        };
        if !tools_and_a_tone(&graph, Some("t_tone"), "holding the microphone awake") {
            return;
        }
        let (mut handle, mut said) = engine_on_the_microphone(&graph, "t_tone");
        assert_eq!(
            none_runs_for_a_while(&graph, &["t_tone", CAPTURE_NODE_NAME]),
            Ok(()),
            "nothing records yet"
        );

        // The calibration wizard opens: the engine records its own source.
        handle.send(UiToAudio::KeepInputAwake(true));
        let recorder =
            graph.nodes_until(|nodes| nodes.iter().any(|(name, _)| *name == KEEP_AWAKE_NODE_NAME));
        assert!(
            matches!(recorder, Some(Ok(_))),
            "the engine never made a stream to hold the microphone awake: {recorder:?}"
        );
        for (key, want) in [
            ("media.class", Some("Stream/Input/Audio")),
            ("target.object", Some(SOURCE_NODE_NAME)),
            ("node.passive", Some("false")),
            ("node.link-group", None),
            ("stream.monitor", None),
        ] {
            assert_eq!(
                graph.node_prop(KEEP_AWAKE_NODE_NAME, key),
                Some(want.map(str::to_owned)),
                "{KEEP_AWAKE_NODE_NAME}'s {key}"
            );
        }
        // WirePlumber links it to its target; here, by hand. And, since nothing here is a real
        // device, a clock is linked into it too, to drive what it records as a device would
        // ([`PrivateGraph::clocked_recorder`] says why the tone must not).
        assert!(
            graph
                .configure_ports(KEEP_AWAKE_NODE_NAME, "Input", &["FL", "FR"])
                .is_some()
        );
        assert!(graph.add_clock().is_some(), "the clock never appeared");
        assert!(graph.link_nodes(CLOCK, KEEP_AWAKE_NODE_NAME));
        assert!(graph.link_nodes(SOURCE_NODE_NAME, KEEP_AWAKE_NODE_NAME));
        for node in [SOURCE_NODE_NAME, CAPTURE_NODE_NAME, "t_tone"] {
            assert_eq!(
                graph.runs_until(node, true).map(drop),
                Ok(()),
                "{node} should run while the microphone is held awake"
            );
        }
        assert!(
            meters_until(&mut handle, DeviceDirection::Input, |m| m.input_peak > 0.1).is_some(),
            "the microphone held awake never reached the input lane's meters"
        );

        // Another microphone while it is held: a new pair, and a recorder of its own with it — the
        // old one's link went with the old source, and WirePlumber never relinks a stream like it.
        let first = graph
            .our_nodes()
            .and_then(|nodes| serial_of(&nodes, KEEP_AWAKE_NODE_NAME))
            .expect("the recorder is in the graph");
        handle.send(UiToAudio::SelectDevice {
            node_name: "t_mic".to_owned(),
            direction: DeviceDirection::Input,
        });
        assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));
        let again = graph.nodes_until(|nodes| {
            serial_of(nodes, KEEP_AWAKE_NODE_NAME).is_some_and(|serial| serial != first)
        });
        assert!(
            matches!(again, Some(Ok(_))),
            "the new pair was not held awake: {again:?}"
        );
        assert_eq!(
            graph
                .node_prop(KEEP_AWAKE_NODE_NAME, "target.object")
                .flatten(),
            Some(SOURCE_NODE_NAME.to_owned())
        );

        // Back on the tone, the wizard closes: the recorder goes, and nothing keeps the microphone
        // from sleeping.
        handle.send(UiToAudio::SelectDevice {
            node_name: "t_tone".to_owned(),
            direction: DeviceDirection::Input,
        });
        assert!(said.attached(&handle, DeviceDirection::Input, Some("t_tone")));
        handle.send(UiToAudio::KeepInputAwake(false));
        let gone =
            graph.nodes_until(|nodes| !nodes.iter().any(|(name, _)| *name == KEEP_AWAKE_NODE_NAME));
        assert!(
            matches!(gone, Some(Ok(_))),
            "the recorder outlived the wish: {gone:?}"
        );
        for (node, direction, positions) in [
            (CAPTURE_NODE_NAME, "Input", &["FL", "FR"][..]),
            (SOURCE_NODE_NAME, "Output", &["FL", "FR"][..]),
        ] {
            assert!(graph.configure_ports(node, direction, positions).is_some());
        }
        assert!(graph.link_nodes("t_tone", CAPTURE_NODE_NAME));
        assert_eq!(
            none_runs_for_a_while(&graph, &["t_tone", CAPTURE_NODE_NAME, SOURCE_NODE_NAME]),
            Ok(()),
            "the microphone was held open after it was let go of"
        );
        handle.shutdown();
    });
}

/// Whether WirePlumber 0.5.17 would switch the headset whose loopback microphone is `loopback` to
/// its call profile, going by the graph as `pw-dump` shows it now. `None` when `pw-dump` is not
/// there to ask.
///
/// `evaluate-bluetooth-profiles` in `device/autoswitch-bluetooth-profile.lua`, transcribed: the
/// loopback — an `Audio/Source` with `device.id` and `bluez5.loopback = true` — is `running`, and
/// some `Stream/Input/Audio` with no `node.link-group`, not a monitor and not a loopback's own,
/// reaches it ([`reaches_loopback`]).
fn wireplumber_would_switch_to_headset(graph: &PrivateGraph, loopback: &str) -> Option<bool> {
    let objects = graph.dump()?;
    let graph = DumpedGraph::new(&objects);
    let Some(microphone) = graph.nodes.iter().find(|node| {
        prop(node, "node.name").as_deref() == Some(loopback)
            && prop(node, "media.class").as_deref() == Some("Audio/Source")
            && prop(node, "device.id").is_some()
            && prop(node, "bluez5.loopback").as_deref() == Some("true")
    }) else {
        return Some(false);
    };
    if microphone["info"]["state"].as_str() != Some("running") {
        return Some(false);
    }
    let id = microphone["id"].as_u64();
    Some(
        graph
            .nodes
            .iter()
            .filter(|node| {
                graph.is_recording_stream(node) && prop(node, "node.link-group").is_none()
            })
            .any(|stream| reaches_loopback(&graph, stream, &mut HashSet::new()) == id),
    )
}

/// `getLinkedBluetoothLoopbackSourceNodeForStream`: the loopback microphone a recording stream
/// is linked to, directly or through the other streams of whatever filter it records from — a
/// node with a `node.link-group`, whose group's recording streams are followed in turn, each group
/// once.
fn reaches_loopback(
    graph: &DumpedGraph<'_>,
    stream: &serde_json::Value,
    visited: &mut HashSet<String>,
) -> Option<u64> {
    let link = graph
        .links
        .iter()
        .find(|link| link["info"]["input-node-id"].as_u64() == stream["id"].as_u64())?;
    let peer_id = link["info"]["output-node-id"].as_u64()?;
    let peer = graph
        .nodes
        .iter()
        .find(|node| node["id"].as_u64() == Some(peer_id))?;
    if prop(peer, "media.class").as_deref() == Some("Audio/Source")
        && prop(peer, "bluez5.loopback").as_deref() == Some("true")
    {
        return Some(peer_id);
    }
    let group = prop(peer, "node.link-group")?;
    if !visited.insert(group.clone()) {
        return None;
    }
    graph
        .nodes
        .iter()
        .filter(|node| {
            graph.is_recording_stream(node)
                && prop(node, "node.link-group").as_deref() == Some(group.as_str())
        })
        .find_map(|filter_stream| reaches_loopback(graph, filter_stream, visited))
}

/// The nodes and links of one `pw-dump`.
struct DumpedGraph<'a> {
    nodes: Vec<&'a serde_json::Value>,
    links: Vec<&'a serde_json::Value>,
}

impl<'a> DumpedGraph<'a> {
    fn new(objects: &'a [serde_json::Value]) -> Self {
        let of = |kind: &str| {
            objects
                .iter()
                .filter(|object| object["type"].as_str() == Some(kind))
                .collect()
        };
        Self {
            nodes: of("PipeWire:Interface:Node"),
            links: of("PipeWire:Interface:Link"),
        }
    }

    /// A `Stream/Input/Audio` that is neither a monitor nor a loopback's own: what the script asks
    /// of every stream it follows, before it asks about groups.
    fn is_recording_stream(&self, node: &serde_json::Value) -> bool {
        prop(node, "media.class").as_deref() == Some("Stream/Input/Audio")
            && prop(node, "stream.monitor").as_deref() != Some("true")
            && prop(node, "bluez5.loopback").as_deref() != Some("true")
    }
}

/// One property of a dumped node, written out as the text it was set from (see
/// [`PrivateGraph::node_prop`]).
fn prop(node: &serde_json::Value, key: &str) -> Option<String> {
    match &node["info"]["props"][key] {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Wait until [`wireplumber_would_switch_to_headset`] says `want`. `None` when `pw-dump` is not
/// there to ask; otherwise whether it came to.
fn switch_settles_on(graph: &PrivateGraph, want: bool) -> Option<bool> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if wireplumber_would_switch_to_headset(graph, LOOPBACK)? == want {
            return Some(true);
        }
        if Instant::now() >= deadline {
            return Some(false);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_headsets_microphone_held_awake_is_switched_to_its_call_profile_as_a_call_would_switch_it() {
    again_if_a_tone_ran_dry(|| {
        let Some(graph) = PrivateGraph::start("hfp") else {
            return;
        };
        if !tools_and_a_tone(&graph, None, "the headset's call profile") {
            return;
        }
        // WirePlumber 0.5's microphone for a headset, as `create-loopback-node.lua` marks it — with a
        // tone behind it, where the real one has the headset's SCO source.
        if graph
            .add_adapter(
                LOOPBACK,
                &format!(
                    "factory.name = audiotestsrc node.name = \"{LOOPBACK}\" \
                 node.description = \"Test Headset\" media.class = Audio/Source \
                 bluez5.loopback = true device.id = 4242 priority.session = 2010"
                ),
            )
            .is_none()
        {
            skip(concat!(
                "the headset's microphone never appeared (is audiotestsrc installed?), ",
                "so the headset's call profile was not checked"
            ));
            return;
        }
        let (handle, _said) = engine_on_the_microphone(&graph, LOOPBACK);

        // With nothing recording from FxSound (Input), the capture stream is linked to the headset and
        // does not run it. Before it was passive it did, and the headset stayed in A2DP all the same:
        // the script passes over a stream in a link-group, and the input lane's microphone recorded
        // the silence of a loopback with no call profile behind it.
        assert_eq!(
            none_runs_for_a_while(&graph, &[LOOPBACK]),
            Ok(()),
            "the headset's microphone was held open with nothing recording from FxSound (Input)"
        );
        assert_eq!(
            wireplumber_would_switch_to_headset(&graph, LOOPBACK),
            Some(false)
        );

        // The microphone meters open.
        handle.send(UiToAudio::KeepInputAwake(true));
        assert!(
            matches!(
                graph.nodes_until(|nodes| nodes
                    .iter()
                    .any(|(name, _)| *name == KEEP_AWAKE_NODE_NAME)),
                Some(Ok(_))
            ),
            "the engine never made a stream to hold the microphone awake"
        );
        assert!(
            graph
                .configure_ports(KEEP_AWAKE_NODE_NAME, "Input", &["FL", "FR"])
                .is_some()
        );
        assert!(graph.link_nodes(SOURCE_NODE_NAME, KEEP_AWAKE_NODE_NAME));
        assert_eq!(
            switch_settles_on(&graph, true),
            Some(true),
            "WirePlumber would leave the headset in A2DP, and its microphone silent"
        );

        // They close: back to A2DP, as WirePlumber restores it two seconds after the last recorder.
        handle.send(UiToAudio::KeepInputAwake(false));
        assert_eq!(
            switch_settles_on(&graph, false),
            Some(true),
            "the headset would be kept in its call profile after the meters closed"
        );

        // An application recording from FxSound (Input) is what switched it before the meters did, and
        // what switches it still: the same walk, through the source's group, finds the headset.
        // Recording from nothing else, as a call does: the script follows a stream's first link only,
        // so a recorder that also took a clock's monitor, as the recorders elsewhere here do, would
        // not be what a call looks like to it.
        let call = graph
            .record_from(SOURCE_NODE_NAME, "t_call")
            .expect("a recorder on FxSound (Input)");
        assert_eq!(
            switch_settles_on(&graph, true),
            Some(true),
            "a call recording from FxSound (Input) would not switch the headset"
        );
        drop(call);
        handle.shutdown();
    });
}

#[test]
fn both_lanes_fall_silent_while_the_system_sleeps_and_are_heard_the_moment_they_are_attached_again()
{
    again_if_a_tone_ran_dry(|| {
        let Some(graph) = PrivateGraph::start("u13") else {
            return;
        };
        if !tools_and_a_tone(&graph, Some("t_tone"), "the sleep's silence") {
            return;
        }
        let (mut handle, mut said) = engine_on_the_microphone(&graph, "t_tone");
        handle.send(UiToAudio::SelectDevice {
            node_name: "t_stereo".to_owned(),
            direction: DeviceDirection::Output,
        });
        assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
        for (node, direction, positions) in [
            (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
            (OUTPUT_NODE_NAME, "Output", &["FL", "FR"][..]),
        ] {
            assert!(
                graph.configure_ports(node, direction, positions).is_some(),
                "{node} was not given ports"
            );
        }
        assert!(
            graph
                .configure_monitored_ports("t_stereo", &["FL", "FR"])
                .is_some(),
            "the speakers were not given ports and a monitor"
        );
        // The tone plays through the music lane into the speakers, whose monitor is recorded, and is
        // recorded from FxSound (Input) through the voice lane.
        assert!(graph.link_nodes("t_tone", SINK_NODE_NAME));
        assert!(graph.link_nodes(OUTPUT_NODE_NAME, "t_stereo"));
        let speakers = graph
            .record_from("t_stereo", "t_speakers")
            .expect("a recorder on the speakers' monitor");
        let recording = graph
            .clocked_recorder("t_recorder")
            .expect("a recorder for FxSound (Input)");
        assert!(graph.link_nodes(SOURCE_NODE_NAME, &recording.name));
        let both = [&speakers, &recording];
        for recorder in both {
            assert!(
                recorder.hears_since(0),
                "{} heard nothing before the sleep",
                recorder.name
            );
        }
        let before = graph.our_nodes().expect("pw-dump answered a moment ago");

        // The system is about to sleep. Within a block or two, both lanes hand on silence — the
        // chains still running under it, the graph as it was.
        handle.send(UiToAudio::SystemSleeping(true));
        std::thread::sleep(Duration::from_millis(300));
        for recorder in both {
            let from = recorder.written();
            let peak = recorder.heard_since(from, HEARD);
            assert_eq!(
                peak,
                Some(0.0),
                "{} heard something while the system slept",
                recorder.name
            );
        }
        let asleep = handle.meters(DeviceDirection::Input);
        assert!(
            asleep.input_peak > 0.1,
            "the voice chain stopped following the microphone: {asleep:?}"
        );

        // It wakes. Both lanes' devices are there, their rules find them attached already, and both
        // are heard again on the tick after — long before the two seconds a lane waiting for its
        // device is given.
        let marks = both.map(Recorder::written);
        let woke = Instant::now();
        handle.send(UiToAudio::SystemSleeping(false));
        for (recorder, from) in both.into_iter().zip(marks) {
            let deadline = woke + engine::WAKE_MUTE;
            let heard = loop {
                let (peak, _) = recorder.peak_since(from);
                if peak > 0.1 {
                    break Some(woke.elapsed());
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            println!("{} heard again {heard:?} after the wake", recorder.name);
            assert!(
                heard.is_some(),
                "{} was not heard again before the wake's mute ran out",
                recorder.name
            );
        }
        assert_eq!(
            graph.our_nodes(),
            Some(before),
            "a wake with every device where it was rebuilds nothing"
        );
        handle.shutdown();
    });
}

#[test]
fn a_headset_that_reconnects_after_the_wake_gets_its_lane_back_rather_than_the_speakers() {
    // WirePlumber's own recipe for a Bluetooth sink: `device.id` names the headset's card.
    let belongs = |card: &CardHolder| format!("device.id = {}", card.id);
    let Some((graph, card, handle, mut said)) = engine_on_a_headset("wakebt", &belongs) else {
        return;
    };
    let first = graph
        .our_nodes()
        .and_then(|nodes| serial_of(&nodes, OUTPUT_NODE_NAME))
        .expect("the playback stream is in the graph");
    let from = said.0.len();

    // The system sleeps, and the headset goes with it: its sink, then its card. Awake, that is a
    // headset switched off, and the lane leaves it on the next tick
    // (`a_headset_switched_off_is_left_as_soon_as_its_card_goes_after_its_sink`).
    handle.send(UiToAudio::SystemSleeping(true));
    said.settle(&handle);
    assert!(graph.remove_node(HEADSET).is_some());
    drop(card);
    said.settle(&handle);
    assert_eq!(
        output_moves_since(&said, from),
        Vec::<Option<String>>::new(),
        "the lane moved while the system slept"
    );

    // It wakes, and the headset reconnects a moment later: a new card, and its sink under its old
    // name.
    handle.send(UiToAudio::SystemSleeping(false));
    let woke = Instant::now();
    std::thread::sleep(2 * crate::engine::SUPERVISOR_PERIOD);
    let card = graph
        .add_card("t_headset_card", HEADSET_ADDRESS)
        .expect("the headset's card came back");
    assert!(
        graph.add_card_sink(HEADSET, &belongs(&card)).is_some(),
        "the headset's sink never came back"
    );
    let back = woke.elapsed();
    println!("the headset was back {back:?} after the wake");
    assert!(
        back < crate::engine::RETURN_WAIT,
        "the headset took {back:?} to come back, longer than the lane waits for it after a wake: \
         the runner is too slow for this test to say anything"
    );

    // The lane is rebuilt on the headset that came back, and never went anywhere else.
    let rebuilt = graph.nodes_until(|nodes| {
        serial_of(nodes, OUTPUT_NODE_NAME).is_some_and(|serial| serial != first)
    });
    assert!(
        matches!(rebuilt, Some(Ok(_))),
        "the playback stream was never rebuilt for the headset: {rebuilt:?}"
    );
    assert_eq!(
        graph.node_prop(OUTPUT_NODE_NAME, "target.object").flatten(),
        Some(HEADSET.to_owned())
    );
    said.settle(&handle);
    let moves = output_moves_since(&said, from);
    assert!(
        moves.iter().all(|to| to.as_deref() == Some(HEADSET)),
        "the output lane left the headset across the sleep: {moves:?}"
    );
    drop(card);
    handle.shutdown();
}
