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

/// A PipeWire daemon of our own: one stereo sink, one 7.1 sink, one virtual microphone.
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
}

impl Drop for PrivateGraph {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
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
            AudioToUi::Status(status) if status.channels == 8 => Some(status),
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
fn switching_direction_rebuilds_the_nodes_the_other_way_round() {
    let Some(graph) = PrivateGraph::start("direction") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    wait_for(&handle, "a device list", |m| {
        matches!(m, AudioToUi::Devices(ref d) if !d.is_empty()).then_some(())
    });

    // The 7.1 sink rather than the stereo one, and that is not arbitrary: a status is published
    // only when it *differs* from the last, and the engine's starting status is already two
    // channels at 48 kHz. Waiting for "stereo" is waiting for a message the engine is right not
    // to send.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    wait_for(&handle, "the output format", |message| match message {
        AudioToUi::Status(status) if status.channels == 8 => Some(status),
        _ => None,
    })
    .expect("the 7.1 sink should be negotiated");

    // Now face the other way. The evidence is that the eight-channel layout is gone: a capture
    // stream on a *mono* source does not negotiate one channel — PipeWire converts, and this one
    // comes up stereo — so the test asks the question it can actually answer, which is whether
    // the nodes were rebuilt rather than relabelled.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    let status = wait_for(&handle, "the input format", |message| match message {
        AudioToUi::Status(status) if status.channels != 8 => Some(status),
        _ => None,
    })
    .expect("the microphone's format should be negotiated");
    assert!(
        status.channels >= 1 && status.channels <= 2,
        "a mono source should not come up with {} channels",
        status.channels
    );
    assert_eq!(status.sample_rate, 48_000);
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
