//! The engine against a real PipeWire graph, rather than against its own pure pieces.
//!
//! Everything else in this crate's tests takes a function with no server behind it: the ring, the
//! quantum arithmetic, the channel map, the pod builders. None of them touches [`supervise`],
//! [`connect`], `apply_rules` or `build_nodes` — the code that actually decides what FxSound does
//! to somebody's audio graph — because those need a server, and pointing them at the developer's
//! own graph would create nodes in it and take their default device away mid-test.
//!
//! So each test here starts a **private** PipeWire: its own daemon, its own socket, its own
//! synthetic devices, nothing shared with the session. Two things make that possible without
//! touching the process environment, which matters because `std::env::set_var` is unsound with
//! threads and this crate forbids unsafe code:
//!
//! * `remote.name` accepts an absolute socket path, so [`AudioEngine::start_with_remote`] can be
//!   pointed straight at the private daemon.
//! * The daemon takes its runtime directory from its own environment, which is a *child's*
//!   environment and therefore nobody else's business.
//!
//! Two traps are encoded here rather than rediscovered. A Unix socket path may not exceed 108
//! bytes, so the runtime directory lives directly under `/tmp` and not beside the source tree —
//! the first attempt at this used a long path and the daemon refused to start with a message
//! about the file name being too long. And `libpipewire-module-access` must be loaded: without
//! it every client connects and then hangs forever, which looks exactly like a deadlock in the
//! code under test.
//!
//! When `pipewire` is not installed these tests print why and pass, because a contributor without
//! it must still be able to run `cargo test` — but they say so loudly rather than quietly doing
//! nothing.

use super::*;
use fxsound_core::messages::AudioToUi;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for anything the server has to do.
const PATIENCE: Duration = Duration::from_secs(10);

/// A PipeWire daemon of our own: one stereo sink, one 7.1 sink, one virtual microphone, and the
/// `default` metadata object a session manager would otherwise create — with no session manager
/// behind it, so nothing links anything and nothing moves a default but us.
struct PrivateGraph {
    dir: PathBuf,
    child: Child,
}

impl PrivateGraph {
    /// `None` when there is no `pipewire` to start, which is not a failure.
    fn start(tag: &str) -> Option<Self> {
        if Command::new("pipewire")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            println!("SKIPPED: pipewire is not installed, so {tag} cannot run");
            return None;
        }

        // Directly under /tmp: a Unix socket path may not exceed 108 bytes, and a path beside the
        // source tree is already most of that before the socket name is added.
        let dir = PathBuf::from(format!("/tmp/fxsound-t-{}-{tag}", std::process::id()));
        let run = dir.join("run");
        std::fs::create_dir_all(&run).ok()?;
        let conf = dir.join("pipewire.conf");
        let mut file = std::fs::File::create(&conf).ok()?;
        file.write_all(CONFIG.as_bytes()).ok()?;
        drop(file);

        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&conf)
            .env("XDG_RUNTIME_DIR", &run)
            .env("PIPEWIRE_DEBUG", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let graph = Self { dir, child };
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if graph.socket().exists() {
                // The socket exists a moment before the daemon answers on it.
                std::thread::sleep(Duration::from_millis(300));
                return Some(graph);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("SKIPPED: the private pipewire never came up for {tag}");
        None
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("run/fxsound-test-0")
    }

    fn remote(&self) -> String {
        self.socket().display().to_string()
    }

    /// Run one of PipeWire's command-line tools against this daemon and nothing else. The socket
    /// is named on the command line, and the child's runtime directory is the private one, so a
    /// tool that ignored `-r` would find no session socket to fall back on either. `None` when the
    /// tool is not installed or failed, which the callers treat as "cannot tell", not as a pass.
    fn tool(&self, program: &str, args: &[&str]) -> Option<String> {
        let output = Command::new(program)
            .arg("-r")
            .arg(self.socket())
            .args(args)
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.socket())
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Which of FxSound's own nodes are in the graph right now, in [`OUR_NODE_NAMES`] order.
    fn our_nodes(&self) -> Option<Vec<&'static str>> {
        let dump = self.tool("pw-dump", &[])?;
        Some(
            OUR_NODE_NAMES
                .into_iter()
                .filter(|name| dump.contains(&format!("\"node.name\": \"{name}\"")))
                .collect(),
        )
    }

    /// Wait until exactly `want` of our nodes are in the graph. `None` when `pw-dump` is not there
    /// to ask; `Some(Err(..))` with what was last seen when they never settled there.
    fn settles_on(&self, want: &[&str]) -> Option<Result<(), Vec<&'static str>>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = self.our_nodes()?;
            if seen == want {
                return Some(Ok(()));
            }
            if Instant::now() >= deadline {
                return Some(Err(seen));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// What `default.configured.audio.<sink|source>` names right now, if anything.
    fn configured_default(&self, direction: DeviceDirection) -> Option<String> {
        let listing = self.tool("pw-metadata", &["-n", "default"])?;
        let key = format!("key:'{}'", devices::configured_default_key(direction));
        listing
            .lines()
            .find(|line| line.contains(&key))
            .and_then(|line| line.split("value:'").nth(1))
            .and_then(devices::parse_default_node_name)
    }

    /// Wait until the configured default of `direction` names `want`. `None` when `pw-metadata` is
    /// not there to ask; otherwise what it last named.
    fn default_settles_on(
        &self,
        direction: DeviceDirection,
        want: &str,
    ) -> Option<Result<(), Option<String>>> {
        self.tool("pw-metadata", &["-n", "default"])?;
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = self.configured_default(direction);
            if seen.as_deref() == Some(want) {
                return Some(Ok(()));
            }
            if Instant::now() >= deadline {
                return Some(Err(seen));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The format one of our nodes holds, as the server has it: `(rate, channels)`. `None` when
    /// `pw-dump` is not there to ask; `Some(None)` when the node never showed one fixed format.
    ///
    /// Read from the graph rather than from a [`AudioToUi::Status`], because a status is only
    /// sent when it differs from the last one, and 48 kHz stereo is the status the engine starts
    /// with. A lane that came up at exactly that format says nothing, and silence is not evidence.
    ///
    /// The node's `Format` param wins once something has negotiated one. In this graph nothing
    /// does: there is no session manager to link our streams, so `Format` stays empty and the
    /// server holds only the `EnumFormat` the engine declared. That is still the answer. The
    /// engine offers exactly one fixed format, and a stream offering one value runs at that
    /// value or not at all. A declaration with a range or more than one entry is reported as
    /// no format, because that would mean the engine stopped pinning it.
    fn node_format(&self, node_name: &str) -> Option<Option<(u64, u64)>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let dump = self.tool("pw-dump", &[])?;
            let objects: serde_json::Value =
                serde_json::from_str(&dump).expect("pw-dump should print JSON");
            let params = objects.as_array().and_then(|objects| {
                objects
                    .iter()
                    .map(|object| &object["info"])
                    .find(|info| info["props"]["node.name"].as_str() == Some(node_name))
                    .map(|info| &info["params"])
            });
            let format = params.and_then(|params| {
                let fixed = |id: &str| match params[id].as_array().map(Vec::as_slice) {
                    Some([only]) => Some((only["rate"].as_u64()?, only["channels"].as_u64()?)),
                    _ => None,
                };
                fixed("Format").or_else(|| fixed("EnumFormat"))
            });
            // A node's params reach the dump a moment after the node itself does.
            if format.is_some() || Instant::now() >= deadline {
                return Some(format);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Say so, loudly, when a check had to be skipped because a PipeWire tool is missing.
fn unless_skipped<T>(checked: Option<T>, tool: &str, what: &str) -> Option<T> {
    if checked.is_none() {
        println!("SKIPPED: {tool} is not available, so {what} was not checked");
    }
    checked
}

impl Drop for PrivateGraph {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Everything the engine said, kept, so a test can ask afterwards what it did *not* say.
#[derive(Default)]
struct Transcript(Vec<AudioToUi>);

impl Transcript {
    /// Read messages until one the predicate accepts arrives. Returns whether it did.
    fn until(
        &mut self,
        handle: &EngineHandle,
        what: &str,
        mut pick: impl FnMut(&AudioToUi) -> bool,
    ) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            while let Some(message) = handle.try_recv() {
                let found = pick(&message);
                self.0.push(message);
                if found {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("gave up waiting for {what}");
        false
    }

    /// Wait until the last attachment a lane reported is `node_name` (`None`: nothing). Already
    /// true when the engine said so before anyone asked — the output lane attaches on its own,
    /// in the same tick that publishes the first device list.
    fn attached(
        &mut self,
        handle: &EngineHandle,
        direction: DeviceDirection,
        node_name: Option<&str>,
    ) -> bool {
        if self.attachments(direction).last().map(Option::as_deref) == Some(node_name) {
            return true;
        }
        self.until(
            handle,
            &format!("the {} lane attached to {node_name:?}", direction.key()),
            |message| {
                matches!(message, AudioToUi::Attached { direction: d, node_name: n }
                    if *d == direction && n.as_deref() == node_name)
            },
        )
    }

    /// Every attachment the engine reported for a lane, in order.
    fn attachments(&self, direction: DeviceDirection) -> Vec<Option<String>> {
        self.0
            .iter()
            .filter_map(|message| match message {
                AudioToUi::Attached {
                    direction: d,
                    node_name,
                } if *d == direction => Some(node_name.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Wait for a message the predicate accepts, draining everything before it.
fn wait_for<T>(
    handle: &EngineHandle,
    what: &str,
    mut pick: impl FnMut(AudioToUi) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        while let Some(message) = handle.try_recv() {
            if let Some(found) = pick(message) {
                return Some(found);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("gave up waiting for {what}");
    None
}

#[test]
fn the_engine_connects_to_a_graph_and_reports_what_is_in_it() {
    let Some(graph) = PrivateGraph::start("connect") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");

    let devices = wait_for(&handle, "a device list", |message| match message {
        AudioToUi::Devices(devices) if !devices.is_empty() => Some(devices),
        _ => None,
    })
    .expect("the graph's devices should be reported");

    let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"t_stereo"), "{names:?}");
    assert!(names.contains(&"t_71"), "{names:?}");
    assert!(names.contains(&"t_mic"), "{names:?}");

    // FxSound's own nodes must never be offered as somewhere to send audio.
    assert!(
        !names.iter().any(|n| n.starts_with("fxsound_")),
        "the engine offered its own nodes as devices: {names:?}"
    );
    handle.shutdown();
}

#[test]
fn selecting_a_device_builds_the_nodes_and_negotiates_its_format() {
    let Some(graph) = PrivateGraph::start("select") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    wait_for(&handle, "a device list", |m| {
        matches!(m, AudioToUi::Devices(ref d) if !d.is_empty()).then_some(())
    });

    // The eight-channel sink, because a format that is merely "stereo" would pass whether or not
    // the engine read the device's real layout.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });

    let status = wait_for(
        &handle,
        "an eight-channel format",
        |message| match message {
            AudioToUi::Status { status, .. } if status.channels == 8 => Some(status),
            _ => None,
        },
    )
    .expect("the 7.1 sink's format should be negotiated");
    assert_eq!(status.sample_rate, 48_000);
    assert_eq!(
        status.dropped_frames, 0,
        "a freshly built graph should not be dropping frames"
    );
    handle.shutdown();
}

#[test]
fn a_microphone_runs_beside_the_speakers_rather_than_instead_of_them() {
    let Some(graph) = PrivateGraph::start("twolanes") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();

    // The 7.1 sink rather than the stereo one, so that the output lane's format is one nothing
    // else could be mistaken for.
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if !d.is_empty()),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));

    // Now the microphone. In 0.3.0 this tore the speakers' pair down; now it builds a second.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));

    if let Some(settled) = unless_skipped(
        graph.settles_on(&OUR_NODE_NAMES),
        "pw-dump",
        "that all four nodes exist at once",
    ) {
        assert_eq!(
            settled,
            Ok(()),
            "both pairs of nodes should be in the graph"
        );
    }

    // The microphone's pair has a format of its own. The capture rate is pinned at 48 kHz, where
    // RNNoise lives. A mono source does not come up with one channel: the lane runs at least a
    // stereo pair and PipeWire converts. Both nodes of the pair must hold the same format, or the
    // ring between them would be read at the wrong stride. And the speakers' pair still holds
    // the 7.1 layout, so the second lane was added beside the first rather than built over it.
    if let Some(capture) = unless_skipped(
        graph.node_format(CAPTURE_NODE_NAME),
        "pw-dump",
        "the input lane's format",
    ) {
        let (rate, channels) = capture.expect("fxsound_capture never showed a fixed format");
        assert_eq!(rate, 48_000, "the microphone should be captured at 48 kHz");
        assert!(
            (1..=2).contains(&channels),
            "a mono source should not come up with {channels} channels"
        );
        assert_eq!(
            graph.node_format(SOURCE_NODE_NAME).flatten(),
            Some((rate, channels)),
            "the input pair disagrees about its format"
        );
        for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME] {
            assert_eq!(
                graph.node_format(node).flatten(),
                Some((48_000, 8)),
                "{node} should still be 7.1 beside the microphone's pair"
            );
        }
    }

    // Give the supervisor a few ticks to say anything it was going to say about the output lane.
    std::thread::sleep(Duration::from_millis(600));
    while let Some(message) = handle.try_recv() {
        said.0.push(message);
    }
    assert_eq!(
        said.attachments(DeviceDirection::Output).last(),
        Some(&Some("t_71".to_owned())),
        "picking a microphone moved the speakers' lane: {:?}",
        said.attachments(DeviceDirection::Output)
    );
    assert!(
        !said.attachments(DeviceDirection::Output).contains(&None),
        "the output lane was detached along the way"
    );
    assert!(
        said.0.iter().all(|message| !matches!(
            message,
            AudioToUi::Status { direction: DeviceDirection::Output, status } if status.channels != 8
        )),
        "the output lane's format changed under it"
    );
    handle.shutdown();
}

#[test]
fn detaching_the_input_lane_takes_only_its_pair_and_its_default_away() {
    let Some(graph) = PrivateGraph::start("detach") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if !d.is_empty()),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));
    if let Some(claimed) = unless_skipped(
        graph.default_settles_on(DeviceDirection::Input, SOURCE_NODE_NAME),
        "pw-metadata",
        "the default source claim",
    ) {
        assert_eq!(claimed, Ok(()), "the input lane takes the default source");
    }

    handle.send(UiToAudio::DetachLane(DeviceDirection::Input));
    assert!(
        said.attached(&handle, DeviceDirection::Input, None),
        "a detach is answered with Attached(None)"
    );

    if let Some(settled) = unless_skipped(
        graph.settles_on(&[SINK_NODE_NAME, OUTPUT_NODE_NAME]),
        "pw-dump",
        "which nodes a detach leaves",
    ) {
        assert_eq!(
            settled,
            Ok(()),
            "only the output lane's pair should be left"
        );
    }
    if let Some(source) = unless_skipped(
        graph.default_settles_on(DeviceDirection::Input, "t_mic"),
        "pw-metadata",
        "the hand-back of the default source",
    ) {
        assert_eq!(
            source,
            Ok(()),
            "the default source goes back to the microphone"
        );
        assert_eq!(
            graph.configured_default(DeviceDirection::Output).as_deref(),
            Some(SINK_NODE_NAME),
            "the default sink is still the output lane's"
        );
    }
    assert!(
        !said.attachments(DeviceDirection::Output).contains(&None),
        "the output lane was detached along with the input lane"
    );
    handle.shutdown();
}

#[test]
fn both_defaults_are_handed_back_when_the_engine_stops() {
    let Some(graph) = PrivateGraph::start("exit") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if !d.is_empty()),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));

    let Some(sink) = unless_skipped(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        "pw-metadata",
        "the exit hand-back",
    ) else {
        handle.shutdown();
        return;
    };
    assert_eq!(sink, Ok(()), "the output lane takes the default sink");
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Input, SOURCE_NODE_NAME),
        Some(Ok(())),
        "the input lane takes the default source"
    );

    // `shutdown` returns only once the server has confirmed the hand-back, so there is nothing
    // to wait for afterwards: the keys either name the real devices now or never will.
    handle.shutdown();
    assert_eq!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some("t_71"),
        "the default sink was left naming a node that is gone"
    );
    assert_eq!(
        graph.configured_default(DeviceDirection::Input).as_deref(),
        Some("t_mic"),
        "the default source was left naming a node that is gone"
    );
}

#[test]
fn the_input_lane_builds_nothing_until_a_microphone_is_picked() {
    let Some(graph) = PrivateGraph::start("idle") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    // The output lane attaches on its own, by the device rules, as the Windows build did.
    assert!(
        said.until(&handle, "the output lane attached", |m| matches!(
            m,
            AudioToUi::Attached {
                direction: DeviceDirection::Output,
                node_name: Some(_)
            }
        ))
    );

    // A microphone is present all along; the input lane must not have picked it up.
    std::thread::sleep(Duration::from_millis(600));
    while let Some(message) = handle.try_recv() {
        said.0.push(message);
    }
    assert_eq!(
        said.attachments(DeviceDirection::Input),
        Vec::<Option<String>>::new()
    );
    if let Some(settled) = unless_skipped(
        graph.settles_on(&[SINK_NODE_NAME, OUTPUT_NODE_NAME]),
        "pw-dump",
        "that the input lane has no nodes",
    ) {
        assert_eq!(settled, Ok(()));
    }
    handle.shutdown();
}

#[test]
fn a_restart_rebuilds_every_lane_that_was_running() {
    let Some(graph) = PrivateGraph::start("restart") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if !d.is_empty()),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));

    handle.send(UiToAudio::Restart);
    assert!(
        said.attached(&handle, DeviceDirection::Input, None),
        "the pair went with the socket"
    );
    assert!(
        said.attached(&handle, DeviceDirection::Input, Some("t_mic")),
        "…and the reconnect rebuilt it, because the lane was still enabled"
    );
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    if let Some(settled) = unless_skipped(
        graph.settles_on(&OUR_NODE_NAMES),
        "pw-dump",
        "that both pairs came back",
    ) {
        assert_eq!(settled, Ok(()));
    }
    handle.shutdown();
}

#[test]
fn a_server_that_goes_away_is_reported_rather_than_hung_on() {
    let Some(mut graph) = PrivateGraph::start("disconnect") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    wait_for(&handle, "a device list", |m| {
        matches!(m, AudioToUi::Devices(ref d) if !d.is_empty()).then_some(())
    });

    // The case a user meets as "PipeWire restarted": the daemon dies under a running engine.
    let _ = graph.child.kill();
    let _ = graph.child.wait();

    let reason = wait_for(&handle, "a disconnection", |message| match message {
        AudioToUi::Disconnected { reason } => Some(reason),
        _ => None,
    })
    .expect("losing the server should be reported, not swallowed");
    assert!(!reason.is_empty(), "the reason should say something");

    // And the engine is still alive and retrying, rather than having taken the thread down with
    // it: shutdown still returns.
    handle.shutdown();
}

/// One daemon, three synthetic devices, and nothing that touches the session.
///
/// `libpipewire-module-access` is not optional: without it every client connects and then hangs,
/// which is indistinguishable from a deadlock in the code under test.
const CONFIG: &str = r#"
context.properties = {
    core.daemon                 = true
    core.name                   = fxsound-test-0
    support.dbus                = false
    default.clock.rate          = 48000
    default.clock.quantum       = 1024
    default.clock.min-quantum   = 32
    default.clock.max-quantum   = 2048
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    audio.adapt     = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-access }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
]
context.objects = [
    { factory = metadata
        args = { metadata.name = default } }
    { factory = adapter
        args = { factory.name = support.null-audio-sink  node.name = "t_stereo"
                 audio.channels = 2  node.description = "Test Stereo Out"
                 media.class = Audio/Sink  object.linger = true
                 audio.position = [ FL FR ] } }
    { factory = adapter
        args = { factory.name = support.null-audio-sink  node.name = "t_71"
                 audio.channels = 8  node.description = "Test 7.1 Out"
                 media.class = Audio/Sink  object.linger = true
                 audio.position = [ FL FR FC LFE RL RR SL SR ] } }
    { factory = adapter
        args = { factory.name = support.null-audio-sink  node.name = "t_mic"
                 audio.channels = 1  node.description = "Test Microphone"
                 media.class = Audio/Source/Virtual  object.linger = true
                 audio.position = [ MONO ] } }
]
"#;
