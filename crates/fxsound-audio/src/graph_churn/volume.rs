//! The volume of FxSound's own nodes, against a private PipeWire (`docs/0.4.0-upstream.md`,
//! U10 and U11).
//!
//! What a desktop's volume slider does to `fxsound_sink` is decided by PipeWire, not by this
//! crate: the slider writes the node's `Props`, and the adapter in front of the stream applies
//! them — somewhere. Where, relative to the process callback, is the whole of U11, so it is
//! measured rather than assumed: pink noise into the sink, the volume leveller on, the sink turned
//! down 20 dB with `pw-cli`, and the level recorded behind NODE 2 with `pw-record`.

use super::*;
use crate::StartOptions;
use fxsound_core::messages::TargetVolume;

/// The graph's rate, which every file and recording here is at.
const RATE: u32 = 48_000;

/// −20 dB as a linear amplitude: what a desktop slider at about 46 % writes into
/// `channelVolumes`, since the slider is cubic and the property is not.
const MINUS_20_DB: f32 = 0.1;

/// Write `seconds` of stereo pink noise at `rms_dbfs` as a 32-bit float WAV file.
///
/// Pink rather than a tone because the leveller's detector has a side-chain high-pass and a
/// tonality estimate (`fxsound_dsp::leveller`): a sine answers for one frequency, and music is
/// nearer to equal energy per octave. Paul Kellet's refined filter over a fixed xorshift, so the
/// file is the same on every run. Float, so a loud programme's peaks above full scale reach the
/// chain as they are and the leveller's own ceiling deals with them.
fn write_pink_noise(path: &std::path::Path, seconds: u32, rms_dbfs: f32) -> std::io::Result<()> {
    let frames = (RATE * seconds) as usize;
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut white = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        // The top 24 bits, as a float in −1..1.
        ((state >> 40) as f32 / (1u64 << 23) as f32) - 1.0
    };
    let mut b = [0.0_f32; 7];
    let mut pink = Vec::with_capacity(frames);
    for _ in 0..frames {
        let w = white();
        b[0] = 0.998_86 * b[0] + w * 0.055_517_9;
        b[1] = 0.993_32 * b[1] + w * 0.075_075_9;
        b[2] = 0.969_00 * b[2] + w * 0.153_852;
        b[3] = 0.866_50 * b[3] + w * 0.310_485_6;
        b[4] = 0.550_00 * b[4] + w * 0.532_952_2;
        b[5] = -0.761_6 * b[5] - w * 0.016_898;
        pink.push(b.iter().sum::<f32>() + w * 0.536_2);
        b[6] = w * 0.115_926;
    }
    let rms = (pink.iter().map(|x| x * x).sum::<f32>() / frames as f32).sqrt();
    let gain = 10.0_f32.powf(rms_dbfs / 20.0) / rms;

    let data_bytes = u32::try_from(frames * 2 * 4).expect("a short file");
    let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&3_u16.to_le_bytes()); // IEEE float
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&RATE.to_le_bytes());
    bytes.extend_from_slice(&(RATE * 8).to_le_bytes());
    bytes.extend_from_slice(&8_u16.to_le_bytes());
    bytes.extend_from_slice(&32_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in pink {
        let sample = (sample * gain).to_le_bytes();
        bytes.extend_from_slice(&sample);
        bytes.extend_from_slice(&sample);
    }
    std::fs::write(path, bytes)
}

/// The RMS of interleaved samples in dBFS, leaving out the first tenth: the moment a recorder is
/// linked, its first quantum can be silence.
fn rms_db(samples: &[f32]) -> f32 {
    let body = samples.get(samples.len() / 10..).unwrap_or_default();
    if body.is_empty() {
        return f32::NEG_INFINITY;
    }
    let mean = body.iter().map(|x| x * x).sum::<f32>() / body.len() as f32;
    10.0 * mean.max(1e-20).log10()
}

impl PrivateGraph {
    /// Start one of `pw-cat`'s faces, made by `support::command` or its recording kind, against
    /// this daemon and nothing else: the socket on its command line and in its own environment,
    /// as [`Self::tool`] does, but left running.
    ///
    /// Its standard error goes to a file named after `name`, whose path comes back with it, for
    /// [`Guarded::account`] to quote when it does not do what it was started for.
    fn client(
        &self,
        mut client: std::process::Command,
        name: &str,
        args: &[&str],
    ) -> Option<(Guarded, PathBuf)> {
        client
            .arg("--remote")
            .arg(self.socket())
            .args(args)
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.socket())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        let log = self.stderr_log(&mut client, name);
        match support::spawn(client) {
            Ok(child) => Some((child, log)),
            Err(error) => {
                println!("{name} could not be started: {error}");
                None
            }
        }
    }

    /// Play a file into nothing yet, as a stream called `name`: linked by the caller, like
    /// everything on this graph.
    fn play(&self, name: &str, file: &std::path::Path) -> Option<Guarded> {
        let props = format!("{{ node.name = {name} }}");
        let file = file.to_str()?;
        let (mut child, log) = self.client(
            support::command("pw-play"),
            name,
            &["--target", "0", "-P", &props, file],
        )?;
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if self.node_id(name).is_some() {
                return Some(child);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!(
            "{name} never appeared in the graph: pw-play; {}",
            child.account(Some(&log))
        );
        None
    }

    /// Record `seconds` of stereo from the outputs of the node called `from` — a sink's monitor
    /// ports, once it has been given them — and hand the interleaved samples back. `None` when
    /// the recorder never appeared, could not be linked, or wrote nothing.
    fn record(&self, from: &str, seconds: f32) -> Option<Vec<f32>> {
        let name = "t_rec";
        // `.raw`: libsndfile's header-less format, which `pw-record` picks by the name, as
        // `record_from` explains.
        let file = self.dir.join("rec.raw");
        let _ = std::fs::remove_file(&file);
        let frames = ((RATE as f32) * seconds) as u32;
        let props = format!("{{ node.name = {name} }}");
        let (mut child, log) = self.client(
            support::command_writing_at_most("pw-record", RECORDING_LIMIT),
            name,
            &[
                "--target",
                "0",
                "-P",
                &props,
                "--format",
                "f32",
                "--rate",
                "48000",
                "--channels",
                "2",
                file.to_str()?,
            ],
        )?;
        let linked = (|| {
            let deadline = Instant::now() + PATIENCE;
            while self.node_id(name).is_none() {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            self.configure_ports(name, "Input", &["FL", "FR"])?;
            self.link_nodes(from, name).then_some(())
        })();
        if linked.is_none() {
            println!(
                "{name} never appeared in the graph or could not be linked from {from}: \
                 pw-record; {}",
                child.account(Some(&log))
            );
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        // Stopped by hand once it has written `frames`, rather than by `-n`, which the
        // `pw-record` of PipeWire 1.0 — Ubuntu 24.04's, where CI runs — does not have.
        let wanted = frames as u64 * 2 * 4;
        let written = || std::fs::metadata(&file).map_or(0, |meta| meta.len());
        let deadline = Instant::now() + Duration::from_secs_f32(seconds) + PATIENCE;
        while written() < wanted
            && Instant::now() < deadline
            && child.try_wait().ok().flatten().is_none()
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let bytes = std::fs::read(&file).unwrap_or_default();
        let (words, _) = bytes.as_chunks::<4>();
        let samples: Vec<f32> = words
            .iter()
            .take(frames as usize * 2)
            .map(|w| f32::from_le_bytes(*w))
            .collect();
        if samples.is_empty() {
            println!(
                "{name} recorded nothing from {from}: pw-record; {}",
                child.account(Some(&log))
            );
        }
        (!samples.is_empty()).then_some(samples)
    }

    /// Set a node's `Props` the way a desktop's slider does: `pw-cli set-param`, from outside the
    /// process that owns the node. `None` when it failed — or had not finished within the
    /// patience, which is what a client the server has stopped reading looks like from outside
    /// (`engine::Session::_volume_core`): a `pw-cli` waiting for ever on a node whose owner
    /// cannot answer, and a test hung with it rather than failed.
    fn set_props(&self, node: &str, props: &str) -> Option<()> {
        let id = self.node_id(node)?.to_string();
        let mut client = support::command("pw-cli");
        client
            .arg("-r")
            .arg(self.socket())
            .args(["set-param", &id, "Props", props])
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.socket())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = support::spawn(client).ok()?;
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return status.success().then_some(());
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                println!("pw-cli set-param {node} Props never returned");
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The node's `Props` as the server lists them: every `Props` object `pw-dump` shows for it.
    fn props_params(&self, node: &str) -> Option<Vec<serde_json::Value>> {
        let objects = self.dump()?;
        let node = Self::node_object(&objects, node)?;
        node["info"]["params"]["Props"].as_array().cloned()
    }

    /// The `channelVolumes` and `mute` the node's `Props` publish — what every desktop slider
    /// shows. `None` when `pw-dump` is not there, the node is not, or it lists no volume.
    fn published_volume(&self, node: &str) -> Option<(Vec<f32>, bool)> {
        let props = self.props_params(node)?;
        props.iter().find_map(|props| {
            let volumes = props["channelVolumes"]
                .as_array()?
                .iter()
                .map(|volume| volume.as_f64().map(|volume| volume as f32))
                .collect::<Option<Vec<f32>>>()?;
            Some((volumes, props["mute"].as_bool()?))
        })
    }

    /// Wait until the node publishes `want` for every one of its channels, unmuted. `Err` with
    /// what it last published when it never did.
    fn publishes(
        &self,
        node: &str,
        channels: usize,
        want: f32,
    ) -> Result<(), Option<(Vec<f32>, bool)>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = self.published_volume(node);
            if let Some((volumes, false)) = &seen
                && volumes.len() == channels
                && volumes.iter().all(|volume| (volume - want).abs() < 1e-4)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(seen);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// What the slider's −20 dB came to behind NODE 2, in dB, for one programme through one chain.
struct SliderDrop {
    /// Level behind NODE 2 with the sink at unity, dBFS.
    unity: f32,
    /// Level drops at 0–3 s, 7–10 s and 17–20 s after the slider moved, dB.
    after: [f32; 3],
}

impl std::fmt::Display for SliderDrop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [first, ten, twenty] = self.after;
        write!(
            f,
            "unity {:.2} dBFS; drop {first:.2} dB (0-3 s), {ten:.2} dB (7-10 s), {twenty:.2} dB \
             (17-20 s)",
            self.unity
        )
    }
}

/// The output lane on `t_stereo` with the monitor of `t_stereo` recordable, and pink noise from
/// `file` playing into the sink: the rig every measurement here runs on. Returns the player,
/// which the caller stops.
fn play_through_the_output_lane(
    graph: &PrivateGraph,
    handle: &EngineHandle,
    said: &mut Transcript,
    file: &std::path::Path,
) -> Guarded {
    said.until(
        handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == "t_stereo")),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(
        graph
            .settles_on(&[SINK_NODE_NAME, OUTPUT_NODE_NAME])
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "the output lane's pair should be in the graph"
    );
    let player = graph.play("t_pink", file).expect("pw-play should start");
    for (node, direction, positions) in [
        ("t_pink", "Output", &["FL", "FR"][..]),
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
            .is_some()
    );
    assert!(graph.link_nodes("t_pink", SINK_NODE_NAME));
    assert!(graph.link_nodes(OUTPUT_NODE_NAME, "t_stereo"));
    player
}

/// The level behind NODE 2 over `seconds`, dBFS.
fn level(graph: &PrivateGraph, seconds: f32) -> f32 {
    rms_db(
        &graph
            .record("t_stereo", seconds)
            .expect("pw-record should record the speakers' monitor"),
    )
}

/// `channelVolumes` for both channels, as `pw-cli` takes it.
fn channel_volumes(volume: f32) -> String {
    format!("{{ \"channelVolumes\": [ {volume}, {volume} ] }}")
}

/// Settle at unity for ten seconds, move the slider 20 dB down, and follow the level for twenty.
fn slider_drop(graph: &PrivateGraph) -> SliderDrop {
    graph
        .set_props(SINK_NODE_NAME, &channel_volumes(1.0))
        .expect("pw-cli set-param");
    std::thread::sleep(Duration::from_secs(10));
    let unity = level(graph, 3.0);
    graph
        .set_props(SINK_NODE_NAME, &channel_volumes(MINUS_20_DB))
        .expect("pw-cli set-param");
    let first = level(graph, 3.0);
    std::thread::sleep(Duration::from_secs(4));
    let ten = level(graph, 3.0);
    std::thread::sleep(Duration::from_secs(7));
    let twenty = level(graph, 3.0);
    SliderDrop {
        unity,
        after: [unity - first, unity - ten, unity - twenty],
    }
}

/// Pink noise through the output lane, with the sink at unity and then 20 dB down, and the level
/// behind NODE 2 at each: once with the chain bypassed, which says where the volume is applied —
/// on this code after the chain, by the lane, since the adapter is clamped to unity (the 0.3.0
/// figures in `docs/spec/12-audio-io.md` §19.7.1 were taken before the clamp, when the adapter
/// applied it in front of `process()`) — and then with the volume leveller at 4 for a
/// quiet programme (−20 dBFS RMS) and a loud one (−12 dBFS RMS), which say how much of the drop
/// the leveller wins back.
///
/// Slow — well over a minute of audio — and a measurement first, so it is ignored by default:
/// `cargo test -p fxsound-audio sink_volume -- --ignored --nocapture` prints the numbers. Before
/// U11 the loud programme's drop was 11.7–12.5 dB; with the volume after the chain it is the
/// slider's 20 (`docs/spec/12-audio-io.md` §19.7.1 has the table). The quick check that holds it
/// there is [`a_slider_twenty_decibels_down_is_twenty_decibels_down_whatever_the_leveller_does`].
#[test]
#[ignore = "a long measurement; run with --ignored --nocapture to see the levels"]
fn the_sink_volume_is_measured_behind_node_two_with_the_leveller_bypassed_and_at_four() {
    let Some(graph) = PrivateGraph::start("volume") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link", "pw-play", "pw-record"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so the sink volume was not measured"
        ));
        return;
    }
    let quiet = graph.dir.join("quiet.wav");
    let loud = graph.dir.join("loud.wav");
    write_pink_noise(&quiet, 60, -20.0).expect("the quiet pink noise file");
    write_pink_noise(&loud, 60, -12.0).expect("the loud pink noise file");

    let mut handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let bypass = DspParams {
        power: false,
        ..DspParams::default()
    };
    let levelled = DspParams {
        volume_leveling_db: 4.0,
        ..DspParams::default()
    };
    handle.set_params(bypass);
    let mut said = Transcript::default();
    let mut player = play_through_the_output_lane(&graph, &handle, &mut said, &quiet);

    // Bypassed: nothing in the chain can move the level, so the drop behind NODE 2 is the volume
    // alone — applied by the lane after the chain on this code, by the adapter before the clamp.
    std::thread::sleep(Duration::from_secs(1));
    let bypass_unity = level(&graph, 1.5);
    graph
        .set_props(SINK_NODE_NAME, &channel_volumes(MINUS_20_DB))
        .expect("pw-cli set-param");
    std::thread::sleep(Duration::from_millis(500));
    let bypass_down = level(&graph, 1.5);
    println!(
        "U11 bypass, -20 dBFS pink: unity {bypass_unity:.2} dBFS, slider at -20 dB \
         {bypass_down:.2} dBFS, drop {:.2} dB",
        bypass_unity - bypass_down
    );
    println!(
        "U11 Props of {SINK_NODE_NAME} after the slider: {}",
        serde_json::to_string(&graph.props_params(SINK_NODE_NAME)).unwrap_or_default()
    );

    handle.set_params(levelled);
    let quiet_drop = slider_drop(&graph);
    println!("U11 VL=4, -20 dBFS pink: {quiet_drop}");

    // The loud programme, from a leveller that has forgotten the quiet one.
    let _ = player.kill();
    let _ = player.wait();
    handle.set_params(bypass);
    std::thread::sleep(Duration::from_millis(300));
    handle.set_params(levelled);
    let mut player = graph.play("t_pink", &loud).expect("pw-play should start");
    assert!(
        graph
            .configure_ports("t_pink", "Output", &["FL", "FR"])
            .is_some()
    );
    assert!(graph.link_nodes("t_pink", SINK_NODE_NAME));
    let loud_drop = slider_drop(&graph);
    println!("U11 VL=4, -12 dBFS pink: {loud_drop}");

    let _ = player.kill();
    let _ = player.wait();
    handle.shutdown();
}

// ---- U10: the volume is the target's, and never raised by a change of device

/// The engine on the private graph, with its first device list in and the output lane attached to
/// `t_stereo` — and the `pw-*` tools there to watch it, or `None` after saying why not.
fn engine_on_the_stereo_sink(
    graph: &PrivateGraph,
    what: &str,
) -> Option<(EngineHandle, Transcript)> {
    if let Some(missing) = ["pw-dump", "pw-cli"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so {what} was not checked"
        ));
        return None;
    }
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == "t_71")),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(
        graph
            .settles_on(&[SINK_NODE_NAME, OUTPUT_NODE_NAME])
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "the output lane's pair should be in the graph"
    );
    Some((handle, said))
}

/// Whether the engine reported `volumes` (every channel at it), unmuted, for `target`.
fn reported(
    said: &mut Transcript,
    handle: &EngineHandle,
    direction: DeviceDirection,
    target: &str,
    channels: usize,
    volume: f32,
) -> bool {
    said.heard(
        handle,
        &format!("{target}'s volume reported at {volume}"),
        |message| {
            matches!(message, AudioToUi::TargetVolume(reported)
            if reported.direction == direction
                && reported.target == target
                && !reported.mute
                && reported.channel_volumes.len() == channels
                && reported.channel_volumes.iter().all(|v| (v - volume).abs() < 1e-4))
        },
    )
}

#[test]
fn a_volume_set_on_fxsounds_sink_is_reported_for_the_device_it_is_attached_to() {
    let Some(graph) = PrivateGraph::start("volreport") else {
        return;
    };
    let Some((handle, mut said)) = engine_on_the_stereo_sink(&graph, "the volume report") else {
        return;
    };
    assert_eq!(
        graph.node_prop(SINK_NODE_NAME, "state.restore-props"),
        Some(Some("false".to_owned())),
        "WirePlumber must not restore one volume for every device"
    );
    assert_eq!(
        graph.node_prop(crate::OUTPUT_NODE_NAME, "state.restore-props"),
        Some(Some("false".to_owned())),
        "nor its one volume for FxSound's streams onto the stream that plays to the device"
    );

    graph
        .set_props(SINK_NODE_NAME, &channel_volumes(0.25))
        .expect("pw-cli set-param");
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Output,
        "t_stereo",
        2,
        0.25
    ));
    assert_eq!(
        graph.published_volume(SINK_NODE_NAME),
        Some((vec![0.25, 0.25], false)),
        "the slider shows what it set: the engine does not write over a desktop's volume"
    );

    // A mute key sends `mute` alone; the level stays, and the report carries both.
    graph
        .set_props(SINK_NODE_NAME, "{ \"mute\": true }")
        .expect("pw-cli set-param");
    assert!(said.heard(&handle, "the mute reported", |message| {
        matches!(message, AudioToUi::TargetVolume(reported)
            if reported.target == "t_stereo" && reported.mute
                && reported.channel_volumes == [0.25, 0.25])
    }));
    handle.shutdown();
}

#[test]
fn a_device_never_seen_starts_no_louder_and_a_device_come_back_to_starts_where_it_was_left() {
    let Some(graph) = PrivateGraph::start("volroam") else {
        return;
    };
    let Some((handle, mut said)) = engine_on_the_stereo_sink(&graph, "the volume per device")
    else {
        return;
    };
    graph
        .set_props(SINK_NODE_NAME, &channel_volumes(0.25))
        .expect("pw-cli set-param");
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Output,
        "t_stereo",
        2,
        0.25
    ));

    // The 7.1 sink has never been played to: it gets the level the stereo one was at, not the
    // unity a new node is made with.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));
    assert_eq!(graph.publishes(SINK_NODE_NAME, 8, 0.25), Ok(()));
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Output,
        "t_71",
        8,
        0.25
    ));

    // Turned up there, and then back to the stereo sink: which starts where it was left, not
    // where the 7.1 sink was.
    let eight = format!("{{ \"channelVolumes\": [ {} ] }}", ["0.5"; 8].join(", "));
    graph
        .set_props(SINK_NODE_NAME, &eight)
        .expect("pw-cli set-param");
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Output,
        "t_71",
        8,
        0.5
    ));
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(graph.publishes(SINK_NODE_NAME, 2, 0.25), Ok(()));

    // And the 7.1 sink, come back to, starts at the level it was turned up to — upward too.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));
    assert_eq!(graph.publishes(SINK_NODE_NAME, 8, 0.5), Ok(()));
    handle.shutdown();
}

/// A remembered volume for the output lane, every channel at `volume`.
fn seeded(target: &str, channels: usize, volume: f32) -> TargetVolume {
    TargetVolume {
        direction: DeviceDirection::Output,
        target: target.to_owned(),
        port: String::new(),
        channel_volumes: vec![volume; channels],
        mute: false,
        extra: toml::Table::new(),
    }
}

#[test]
fn a_volume_remembered_at_start_is_where_its_device_starts_and_nothing_contradicts_it() {
    let Some(graph) = PrivateGraph::start("volseed") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so the remembered volumes were not checked"
        ));
        return;
    }
    // As the app does it: the memory handed over with the engine, so that the output lane — which
    // attaches on its own as soon as the registry is read — builds its first pair with it.
    let handle = AudioEngine::start_for_tests(
        Some(&graph.remote()),
        StartOptions {
            target_volumes: vec![seeded("t_stereo", 2, 0.3), seeded("t_71", 8, 0.6)],
            ..StartOptions::default()
        },
        None,
    )
    .expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == "t_71")),
    );
    for (target, channels, volume) in [("t_stereo", 2, 0.3), ("t_71", 8, 0.6)] {
        handle.send(UiToAudio::SelectDevice {
            node_name: target.to_owned(),
            direction: DeviceDirection::Output,
        });
        assert!(said.attached(&handle, DeviceDirection::Output, Some(target)));
        assert_eq!(
            graph.publishes(SINK_NODE_NAME, channels, volume),
            Ok(()),
            "{target}"
        );
    }
    // Replaying what the app remembers tells it nothing new: every pair, the first one included,
    // started at the level remembered for its device. A report of any level at all would be a
    // pair that had started at another — unity, before the memory arrived — and said so.
    said.settle(&handle);
    let reports: Vec<&TargetVolume> = said
        .0
        .iter()
        .filter_map(|message| match message {
            AudioToUi::TargetVolume(reported) => Some(reported),
            _ => None,
        })
        .collect();
    assert!(reports.is_empty(), "{reports:?}");
    handle.shutdown();
}

#[test]
fn the_first_pair_after_an_upgrade_starts_at_the_level_wireplumber_kept_for_0_3_0() {
    let Some(graph) = PrivateGraph::start("volinherit") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so the inherited volume was not checked"
        ));
        return;
    }
    // WirePlumber's memory as 0.3.0 left it, about −24 dB, and nothing in the app's: the first
    // 0.4.0 start after an upgrade.
    let state = graph.dir.join("stream-properties");
    std::fs::write(
        &state,
        "[stream-properties]\n\
         Audio/Sink:application.id:com.fxsound.FxSound={\"channelVolumes\":[0.064000, 0.064000], \
         \"mute\":false, \"channelMap\":[\"FL\", \"FR\"], \"volume\":1.000000}\n",
    )
    .expect("WirePlumber's state file");
    let handle =
        AudioEngine::start_for_tests(Some(&graph.remote()), StartOptions::default(), Some(state))
            .expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == "t_71")),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(
        graph.publishes(SINK_NODE_NAME, 2, 0.064),
        Ok(()),
        "not the unity a node WirePlumber may no longer restore is made with"
    );
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Output,
        "t_stereo",
        2,
        0.064
    ));
    handle.shutdown();
}

#[test]
fn a_seed_that_arrives_after_the_pair_is_up_still_sets_where_its_device_is() {
    let Some(graph) = PrivateGraph::start("volseedlate") else {
        return;
    };
    let Some((handle, mut said)) = engine_on_the_stereo_sink(&graph, "a late seed") else {
        return;
    };
    // The pair on the stereo sink is up already, at the unity nothing remembered gave it.
    handle.send(UiToAudio::SeedTargetVolumes(vec![seeded(
        "t_stereo", 2, 0.3,
    )]));
    assert_eq!(graph.publishes(SINK_NODE_NAME, 2, 0.3), Ok(()));
    // Said back to the app: a pair up long enough may have reported the unity it started at, and
    // the app's last word for the device has to be the level the device now plays.
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Output,
        "t_stereo",
        2,
        0.3
    ));

    // And a desktop that moves it afterwards is heard as usual.
    graph
        .set_props(SINK_NODE_NAME, &channel_volumes(0.4))
        .expect("pw-cli set-param");
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Output,
        "t_stereo",
        2,
        0.4
    ));
    handle.shutdown();
}

#[test]
fn the_microphone_lanes_volume_is_reported_from_fxsounds_source() {
    let Some(graph) = PrivateGraph::start("volsource") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so the source's volume was not checked"
        ));
        return;
    }
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == "t_mic")),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));
    assert!(
        graph
            .nodes_until(|nodes| nodes.iter().any(|(name, _)| *name == SOURCE_NODE_NAME))
            .is_some_and(|settled| settled.is_ok())
    );
    assert_eq!(
        graph.node_prop(SOURCE_NODE_NAME, "state.restore-props"),
        Some(Some("false".to_owned()))
    );
    assert_eq!(
        graph.node_prop(crate::CAPTURE_NODE_NAME, "state.restore-props"),
        Some(Some("false".to_owned())),
        "a mute left on the capture stream in a mixer must not come back on every pair"
    );
    graph
        .set_props(SOURCE_NODE_NAME, &channel_volumes(0.5))
        .expect("pw-cli set-param");
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Input,
        "t_mic",
        2,
        0.5
    ));
    handle.shutdown();
}

// ---- U11: the volume is applied after the chain

/// The leveller at 4 on a loud programme, where it had most to win back: with the volume applied
/// before the chain, a slider 20 dB down came to 12 dB less behind NODE 2 (see the measurement
/// above). Applied after it, the drop is the slider's.
#[test]
fn a_slider_twenty_decibels_down_is_twenty_decibels_down_whatever_the_leveller_does() {
    let Some(graph) = PrivateGraph::start("volpost") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link", "pw-play", "pw-record"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so the volume after the chain was not checked"
        ));
        return;
    }
    let loud = graph.dir.join("loud.wav");
    write_pink_noise(&loud, 30, -12.0).expect("the pink noise file");
    let mut handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    handle.set_params(DspParams {
        volume_leveling_db: 4.0,
        ..DspParams::default()
    });
    let mut said = Transcript::default();
    let mut player = play_through_the_output_lane(&graph, &handle, &mut said, &loud);

    std::thread::sleep(Duration::from_secs(4));
    let unity = level(&graph, 1.5);
    graph
        .set_props(SINK_NODE_NAME, &channel_volumes(MINUS_20_DB))
        .expect("pw-cli set-param");
    std::thread::sleep(Duration::from_secs(1));
    let down = level(&graph, 1.5);
    println!("U11 after the fix, VL=4, -12 dBFS pink: unity {unity:.2} dBFS, down {down:.2} dBFS");
    assert!(
        unity - down > 18.5,
        "the slider's 20 dB came to {:.2} dB behind NODE 2",
        unity - down
    );
    assert_eq!(
        graph.published_volume(SINK_NODE_NAME),
        Some((vec![MINUS_20_DB, MINUS_20_DB], false)),
        "and the slider still shows what it set"
    );

    let _ = player.kill();
    let _ = player.wait();
    handle.shutdown();
}

#[test]
fn the_engine_writing_its_own_nodes_volume_leaves_the_node_answering_every_other_client() {
    // The engine writes the volume a pair starts at into its own node's `Props`, through the
    // server. Written on the connection the node belongs to, that stopped the server reading the
    // connection for good (`engine::Session::_volume_core`): the write never showed in the node's
    // `Props`, and every desktop's write after it hung.
    let Some(graph) = PrivateGraph::start("volbusy") else {
        return;
    };
    let Some((handle, mut said)) = engine_on_the_stereo_sink(&graph, "the engine's own write")
    else {
        return;
    };
    std::thread::sleep(Duration::from_millis(500));
    handle.send(UiToAudio::SeedTargetVolumes(vec![TargetVolume {
        direction: DeviceDirection::Output,
        target: "t_stereo".to_owned(),
        port: String::new(),
        channel_volumes: vec![0.3, 0.3],
        mute: false,
        extra: toml::Table::new(),
    }]));
    assert_eq!(graph.publishes(SINK_NODE_NAME, 2, 0.3), Ok(()));

    assert!(
        graph
            .set_props(SINK_NODE_NAME, &channel_volumes(0.4))
            .is_some(),
        "a desktop's write after the engine's own must still be answered"
    );
    assert_eq!(graph.publishes(SINK_NODE_NAME, 2, 0.4), Ok(()));
    assert!(reported(
        &mut said,
        &handle,
        DeviceDirection::Output,
        "t_stereo",
        2,
        0.4
    ));
    handle.shutdown();
}
