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
//! What the engine says is only half of each test; the other half is read from the server by
//! PipeWire's own tools, run as children with the private socket on their command line and in
//! their own environment. The engine cannot be its own witness here: its device list leaves our
//! nodes out by design, so `pw-dump` is what says which of them exist, and `pw-metadata` is what
//! says where each default points. Nodes are told apart by `object.serial`, not by id, because the
//! server reuses a freed id and a pair rebuilt in one step can come back under the ids it had.
//!
//! Nothing links anything in this graph, and not only because there is no WirePlumber to make the
//! links: there is nothing to link. An adapter has no ports until someone sets its `PortConfig`,
//! and that is the session manager's job too. So the one test that needs audio to flow does both
//! steps itself — `pw-cli set-param … PortConfig` on every node involved, then `pw-link`. Its
//! source is a tone from PipeWire's `audiotestsrc`, added with `pw-cli create-node`, rather than
//! the null microphone: a null device plays silence, and silence moves no meter, so it could show
//! that a lane is scheduled but never that it is fed — and on this server those are not the same.
//!
//! When `pipewire` is not installed these tests print why and pass, because a contributor without
//! it must still be able to run `cargo test` — but they say so loudly rather than quietly doing
//! nothing. A missing `pw-dump`, `pw-metadata`, `pw-cli` or `pw-link` skips only the checks that
//! need it, with the same kind of line.
//!
//! Loudly is not loud enough everywhere. The test harness keeps a passing test's output to itself,
//! so a `SKIPPED` line is never seen unless someone asks for it, and a skip is also how a tool that
//! ran and failed looks from here. Where the tools were installed on purpose — CI — that would let
//! a broken check pass for ever. So [`REQUIRE_TOOLS`] turns every skip into a failure, and CI is
//! the only place that sets it.

use super::*;
use fxsound_core::messages::AudioToUi;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for anything the server has to do.
const PATIENCE: Duration = Duration::from_secs(10);

/// Our nodes as the server lists them: each name with its `object.serial`.
type OurNodes = Vec<(&'static str, u64)>;

/// Set to `1` where every check here must run: `.github/workflows/ci.yml` sets it, because it
/// installs the daemon, the `pw-*` tools and the `audiotestsrc` plugin for these tests, and there a
/// skip can only mean that something which should work did not.
const REQUIRE_TOOLS: &str = "FXSOUND_REQUIRE_PIPEWIRE_TOOLS";

/// Report a check that could not run: a `SKIPPED` line and a pass on a contributor's machine, a
/// failure under [`REQUIRE_TOOLS`].
fn skip(reason: &str) {
    assert!(
        std::env::var_os(REQUIRE_TOOLS).is_none_or(|value| value != "1"),
        "{reason}, and {REQUIRE_TOOLS}=1 says nothing here may be skipped"
    );
    println!("SKIPPED: {reason}");
}

/// A PipeWire daemon of our own: one stereo sink, one 7.1 sink, one virtual microphone, and the
/// `default` metadata object a session manager would otherwise create — with no session manager
/// behind it, so nothing links anything and nothing moves a default but us.
struct PrivateGraph {
    dir: PathBuf,
    child: Child,
}

impl PrivateGraph {
    /// `None` when there is no `pipewire` to start, which is not a failure — unless
    /// [`REQUIRE_TOOLS`] says it is.
    fn start(tag: &str) -> Option<Self> {
        Self::spawn(tag)
            .inspect_err(|why| skip(&format!("{why}, so {tag} cannot run")))
            .ok()
    }

    /// [`Self::start`], saying why when there is no daemon to be had.
    fn spawn(tag: &str) -> Result<Self, String> {
        if !installed("pipewire") {
            return Err("pipewire is not installed".to_owned());
        }

        // Directly under /tmp: a Unix socket path may not exceed 108 bytes, and a path beside the
        // source tree is already most of that before the socket name is added.
        let dir = PathBuf::from(format!("/tmp/fxsound-t-{}-{tag}", std::process::id()));
        let run = dir.join("run");
        let conf = dir.join("pipewire.conf");
        std::fs::create_dir_all(&run)
            .and_then(|()| std::fs::File::create(&conf))
            .and_then(|mut file| file.write_all(CONFIG.as_bytes()))
            .map_err(|error| format!("{} could not be prepared: {error}", dir.display()))?;

        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&conf)
            .env("XDG_RUNTIME_DIR", &run)
            .env("PIPEWIRE_DEBUG", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("pipewire could not be started: {error}"))?;

        let graph = Self { dir, child };
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if graph.socket().exists() {
                // The socket exists a moment before the daemon answers on it.
                std::thread::sleep(Duration::from_millis(300));
                return Ok(graph);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("the private pipewire never came up".to_owned())
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

    /// Everything in the graph, as `pw-dump` prints it. `None` when `pw-dump` is not there to ask.
    fn dump(&self) -> Option<Vec<serde_json::Value>> {
        let dump = self.tool("pw-dump", &[])?;
        Some(serde_json::from_str(&dump).expect("pw-dump should print a JSON array"))
    }

    /// The node called `name` in a dump, if it is there.
    fn node_object<'a>(
        objects: &'a [serde_json::Value],
        name: &str,
    ) -> Option<&'a serde_json::Value> {
        objects.iter().find(|object| {
            object["type"].as_str() == Some("PipeWire:Interface:Node")
                && object["info"]["props"]["node.name"].as_str() == Some(name)
        })
    }

    /// Which of FxSound's own nodes are in the graph right now, in [`OUR_NODE_NAMES`] order, each
    /// with its `object.serial` — which, unlike its id, is never handed to another object, so two
    /// readings with the same serial are the same node and not a rebuilt one.
    fn our_nodes(&self) -> Option<OurNodes> {
        let objects = self.dump()?;
        Some(
            OUR_NODE_NAMES
                .into_iter()
                .filter_map(|name| {
                    let serial =
                        Self::node_object(&objects, name)?["info"]["props"]["object.serial"]
                            .as_u64();
                    Some((name, serial.expect("every node carries an object.serial")))
                })
                .collect(),
        )
    }

    /// Wait until our nodes are what `done` wants. `None` when `pw-dump` is not there to ask;
    /// `Some(Err(..))` with what was last seen when they never got there.
    fn nodes_until(
        &self,
        mut done: impl FnMut(&[(&'static str, u64)]) -> bool,
    ) -> Option<Result<OurNodes, OurNodes>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = self.our_nodes()?;
            if done(&seen) {
                return Some(Ok(seen));
            }
            if Instant::now() >= deadline {
                return Some(Err(seen));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until exactly `want` of our nodes are in the graph, and say which serials they settled
    /// with. `None` when `pw-dump` is not there to ask; `Some(Err(..))` with the names last seen
    /// when they never settled there.
    fn settles_on(&self, want: &[&str]) -> Option<Result<OurNodes, Vec<&'static str>>> {
        let names = |nodes: &[(&'static str, u64)]| -> Vec<&'static str> {
            nodes.iter().map(|(name, _)| *name).collect()
        };
        self.nodes_until(|nodes| names(nodes) == want)
            .map(|settled| settled.map_err(|seen| names(&seen)))
    }

    /// The id of the node called `name`, for `pw-cli`, which addresses objects by number.
    fn node_id(&self, name: &str) -> Option<u64> {
        let objects = self.dump()?;
        Self::node_object(&objects, name)?["id"].as_u64()
    }

    /// A node's ports in one direction, `"in"` or `"out"`, as `(port.name, audio.channel)`. Found
    /// rather than spelt out in the tests, because what a port is called is the adapter's choice
    /// and not ours. `None` when `pw-dump` is not there or the node is not in the graph.
    fn ports(&self, node: &str, direction: &str) -> Option<Vec<(String, String)>> {
        let objects = self.dump()?;
        let id = Self::node_object(&objects, node)?["id"].as_u64();
        Some(
            objects
                .iter()
                .filter(|object| object["type"].as_str() == Some("PipeWire:Interface:Port"))
                .map(|object| &object["info"]["props"])
                .filter(|props| {
                    props["node.id"].as_u64() == id
                        && props["port.direction"].as_str() == Some(direction)
                })
                .filter_map(|props| {
                    let name = props["port.name"].as_str()?.to_owned();
                    let channel = props["audio.channel"].as_str().unwrap_or_default();
                    Some((name, channel.to_owned()))
                })
                .collect(),
        )
    }

    /// Give a node the ports a session manager would: DSP mode, one float port per entry of
    /// `positions`, at the graph's rate. WirePlumber does this to every node it links, and until
    /// something does an adapter has no ports at all. Waits until the ports are there, since they
    /// appear a moment after the parameter is set. `None` when the node is not in the graph, a
    /// tool is missing or refused, or the ports never came.
    fn configure_ports(&self, node: &str, direction: &str, positions: &[&str]) -> Option<()> {
        let id = self.node_id(node)?.to_string();
        let channels = positions.len();
        let listed = positions
            .iter()
            .map(|position| format!("\"{position}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let config = format!(
            "{{ \"direction\": \"{direction}\", \"mode\": \"dsp\", \
             \"format\": {{ \"mediaType\": \"audio\", \"mediaSubtype\": \"raw\", \
             \"format\": \"F32P\", \"rate\": 48000, \"channels\": {channels}, \
             \"position\": [ {listed} ] }} }}"
        );
        self.tool("pw-cli", &["set-param", &id, "PortConfig", &config])?;

        let side = if direction == "Input" { "in" } else { "out" };
        let deadline = Instant::now() + PATIENCE;
        loop {
            if self.ports(node, side)?.len() == channels {
                return Some(());
            }
            if Instant::now() >= deadline {
                println!("gave up waiting for the ports of {node}");
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Link `from`'s outputs to `to`'s inputs channel by channel, as a session manager would — or,
    /// from a node with a single output, that one to every input. Whether every link was made;
    /// `false` as well when either side has no ports to link.
    fn link_nodes(&self, from: &str, to: &str) -> bool {
        let (Some(outputs), Some(inputs)) = (self.ports(from, "out"), self.ports(to, "in")) else {
            return false;
        };
        let mut linked = !outputs.is_empty() && !inputs.is_empty();
        for (output, channel) in &outputs {
            for (input, _) in inputs
                .iter()
                .filter(|(_, other)| outputs.len() == 1 || other == channel)
            {
                let (output, input) = (format!("{from}:{output}"), format!("{to}:{input}"));
                let made = self.tool("pw-link", &[&output, &input]).is_some();
                if !made {
                    println!("pw-link refused {output} -> {input}");
                }
                linked &= made;
            }
        }
        linked
    }

    /// Add a source that plays a steady tone: PipeWire's `audiotestsrc` behind an adapter, which
    /// is what makes a lane's meters move where a null device's silence cannot. Created by a
    /// client that then leaves, so it lingers. `None` when the plugin is not installed and the node
    /// never appears.
    fn add_tone(&self, name: &str) -> Option<()> {
        let props = format!(
            "{{ factory.name = audiotestsrc node.name = {name} node.description = \"Test Tone\" \
             media.class = Audio/Source object.linger = true }}"
        );
        // What `pw-cli` exits with says less than whether the node turns up, so only that is asked.
        let _ = self.tool("pw-cli", &["create-node", "adapter", &props]);
        // The same patience as every other wait here: a slow runner that is merely late to show
        // the node must not read as a missing plugin, which CI turns into a failure.
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if self.node_id(name).is_some() {
                return Some(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// Point a direction's default at `node_name` before the engine starts, the way a session
    /// would have it: the configured key the user's choice lives in, and the current key a session
    /// manager derives from it — which, with no session manager here, nobody else will write.
    /// `None` when `pw-metadata` is not there; otherwise whether the server came to show it.
    fn seed_default(
        &self,
        direction: DeviceDirection,
        node_name: &str,
    ) -> Option<Result<(), Option<String>>> {
        self.write_default(devices::configured_default_key(direction), node_name)?;
        self.write_default(devices::default_key(direction), node_name)?;
        self.default_settles_on(direction, node_name)
    }

    /// Write one key of the `default` metadata object. `None` when `pw-metadata` is not there or
    /// refused.
    fn write_default(&self, key: &str, node_name: &str) -> Option<()> {
        let value = devices::default_node_value(node_name);
        self.tool(
            "pw-metadata",
            &["-n", "default", "0", key, &value, "Spa:String:JSON"],
        )
        .map(drop)
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
    /// The node's `Format` param wins once something has negotiated one. Unless a test links our
    /// streams itself nothing does — there is no session manager to — so `Format` stays empty and
    /// the server holds only the `EnumFormat` the engine declared. That is still the answer. The
    /// engine offers exactly one fixed format, and a stream offering one value runs at that
    /// value or not at all. A declaration with a range or more than one entry is reported as
    /// no format, because that would mean the engine stopped pinning it.
    fn node_format(&self, node_name: &str) -> Option<Option<(u64, u64)>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let objects = self.dump()?;
            let params = Self::node_object(&objects, node_name).map(|node| &node["info"]["params"]);
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

/// Whether a program can be started at all. Asked up front by a test that cannot do without a
/// tool, because [`PrivateGraph::tool`]'s `None` also means "ran and refused", which for such a
/// test is a failure and not a reason to skip.
fn installed(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// The serial `nodes` lists for `name`, if it lists `name` at all.
fn serial_of(nodes: &[(&str, u64)], name: &str) -> Option<u64> {
    nodes
        .iter()
        .find(|(node, _)| *node == name)
        .map(|(_, serial)| *serial)
}

/// Say so, loudly, when a check had to be skipped because a PipeWire tool gave no answer — which
/// is a missing tool on a contributor's machine, and under [`REQUIRE_TOOLS`] a failure whatever
/// the reason.
fn unless_skipped<T>(checked: Option<T>, tool: &str, what: &str) -> Option<T> {
    if checked.is_none() {
        skip(&format!(
            "{tool} is not available or failed, so {what} was not checked"
        ));
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

    /// Whether the engine has said something the predicate accepts — already, or within the
    /// patience. For messages that may have gone by while a test was waiting for another one.
    fn heard(
        &mut self,
        handle: &EngineHandle,
        what: &str,
        mut pick: impl FnMut(&AudioToUi) -> bool,
    ) -> bool {
        self.0.iter().any(&mut pick) || self.until(handle, what, pick)
    }

    /// Give the supervisor three ticks to say whatever it was going to say, and keep all of it.
    /// What a test asserts the engine did *not* say is only worth anything after this.
    fn settle(&mut self, handle: &EngineHandle) {
        std::thread::sleep(Duration::from_millis(600));
        while let Some(message) = handle.try_recv() {
            self.0.push(message);
        }
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
            settled.map(drop),
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
    said.settle(&handle);
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
fn choosing_other_speakers_rebuilds_the_output_pair_and_leaves_the_microphone_pair_alone() {
    let Some(graph) = PrivateGraph::start("respeak") else {
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
    let Some(before) = unless_skipped(
        graph.settles_on(&OUR_NODE_NAMES),
        "pw-dump",
        "which pair a change of speakers rebuilds",
    ) else {
        handle.shutdown();
        return;
    };
    let before = before.expect("both pairs should be in the graph at once");
    let heard_before = said.0.len();

    // Stereo to 7.1: a new channel count, so the output pair cannot be kept and must be rebuilt.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));
    assert!(
        said.heard(&handle, "the output lane's 7.1 status", |m| matches!(
            m,
            AudioToUi::Status { direction: DeviceDirection::Output, status } if status.channels == 8
        )),
        "the new layout is reported, and as the output lane's"
    );

    let after = graph
        .nodes_until(|nodes| {
            nodes.len() == OUR_NODE_NAMES.len()
                && [SINK_NODE_NAME, OUTPUT_NODE_NAME]
                    .into_iter()
                    .all(|node| serial_of(nodes, node) != serial_of(&before, node))
        })
        .expect("pw-dump answered a moment ago")
        .expect("the output pair should have been rebuilt beside the microphone's");
    for node in [CAPTURE_NODE_NAME, SOURCE_NODE_NAME] {
        assert_eq!(
            serial_of(&after, node),
            serial_of(&before, node),
            "{node} was rebuilt for a change of speakers"
        );
    }
    for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME] {
        assert_eq!(
            graph.node_format(node).flatten(),
            Some((48_000, 8)),
            "{node} should hold the 7.1 layout"
        );
    }
    // The sink came back under its old name, so the claim on it stands; the source's was never
    // in question.
    if let Some(sink) = unless_skipped(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        "pw-metadata",
        "the claims across a rebuild",
    ) {
        assert_eq!(sink, Ok(()), "the default sink should still be ours");
        assert_eq!(
            graph.configured_default(DeviceDirection::Input).as_deref(),
            Some(SOURCE_NODE_NAME),
            "a change of speakers moved the default source"
        );
    }

    said.settle(&handle);
    let since = &said.0[heard_before..];
    assert!(
        !since.iter().any(|m| matches!(
            m,
            AudioToUi::Attached {
                direction: DeviceDirection::Input,
                ..
            }
        )),
        "a change of speakers re-announced the microphone's lane: {since:?}"
    );
    assert!(
        !since.iter().any(|m| matches!(
            m,
            AudioToUi::Status { direction: DeviceDirection::Input, status } if status.channels == 8
        )),
        "the speakers' layout was reported as the microphone's"
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

    // Both pairs first, or the detach below would prove nothing: a capture pair that never
    // existed also leaves only the output pair behind.
    let both = unless_skipped(
        graph.settles_on(&OUR_NODE_NAMES),
        "pw-dump",
        "that both pairs exist before the detach",
    )
    .map(|settled| settled.expect("both pairs should be in the graph at once"));
    if let Some(claimed) = unless_skipped(
        graph.default_settles_on(DeviceDirection::Input, SOURCE_NODE_NAME),
        "pw-metadata",
        "the default source claim",
    ) {
        assert_eq!(claimed, Ok(()), "the input lane takes the default source");
        assert_eq!(
            graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
            Some(Ok(())),
            "the output lane takes the default sink"
        );
    }

    handle.send(UiToAudio::DetachLane(DeviceDirection::Input));
    assert!(
        said.attached(&handle, DeviceDirection::Input, None),
        "a detach is answered with Attached(None)"
    );

    if let Some(both) = both {
        let left = graph
            .settles_on(&[SINK_NODE_NAME, OUTPUT_NODE_NAME])
            .expect("pw-dump answered a moment ago")
            .expect("only the output lane's pair should be left");
        // The same two nodes, not a pair rebuilt under the same names.
        for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME] {
            assert_eq!(
                serial_of(&left, node),
                serial_of(&both, node),
                "{node} was rebuilt when the input lane was detached"
            );
        }
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

/// The defaults the session had before FxSound started are what each lane follows and what it
/// remembers when it claims its own — even when the settings file remembers something else.
///
/// The settings file's copy, [`UiToAudio::SeedRememberedDefaults`], names `t_stereo` here, and the
/// session's default sink is `t_71`. Both are real sinks, so every check below says which of the two
/// the engine listened to. The seed fills the memory's empty slots, so the output lane's choice is
/// no longer the first-run rule: were the session's default sink not read by then, the rules would
/// walk the remembered devices and land on `t_stereo`. The input seed names a microphone that is
/// not in this graph at all, as one unplugged since the last session would be.
///
/// The exit check proves less than the rest: only that both keys go back to real devices rather
/// than to our nodes. What it hands back to is the device each lane last used — `t_71` because the
/// output lane played to it, `t_mic` because it was picked — and those are also the session's
/// defaults here, so the exit alone cannot tell the session's memory from the lanes' own.
#[test]
fn the_session_defaults_outrank_the_settings_file_and_are_remembered_per_lane() {
    let Some(graph) = PrivateGraph::start("seeded") else {
        return;
    };
    for (direction, device) in [
        (DeviceDirection::Output, "t_71"),
        (DeviceDirection::Input, "t_mic"),
    ] {
        let Some(seeded) = unless_skipped(
            graph.seed_default(direction, device),
            "pw-metadata",
            "what each lane remembers of the defaults the session had",
        ) else {
            return;
        };
        assert_eq!(
            seeded,
            Ok(()),
            "the default {} was never seeded",
            direction.key()
        );
    }

    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    // What the app sends straight after the start, from its settings file — written by an earlier
    // session whose defaults were not this one's.
    handle.send(UiToAudio::SeedRememberedDefaults {
        output: "t_stereo".to_owned(),
        input: "t_old_mic".to_owned(),
    });
    let mut said = Transcript::default();
    assert!(
        said.attached(&handle, DeviceDirection::Output, Some("t_71")),
        "the output lane should follow the session's default sink, not the settings file's"
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));

    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(())),
        "the output lane takes the default sink"
    );
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Input, SOURCE_NODE_NAME),
        Some(Ok(())),
        "the input lane takes the default source"
    );
    // Each claim says what it displaced, so it reaches the settings file, and under its own lane.
    // Only the session's metadata can have told it `t_71` and `t_mic`: the seed says otherwise,
    // and the engine never repeats a seed back as something it displaced.
    for (direction, device) in [
        (DeviceDirection::Output, "t_71"),
        (DeviceDirection::Input, "t_mic"),
    ] {
        assert!(
            said.heard(
                &handle,
                &format!("the {} lane's remembered default", direction.key()),
                |m| matches!(m, AudioToUi::RememberedDefault { direction: d, node_name }
                    if *d == direction && node_name == device),
            ),
            "the {} lane never said it displaced {device}",
            direction.key()
        );
    }
    said.settle(&handle);
    for message in &said.0 {
        if let AudioToUi::RememberedDefault {
            direction,
            node_name,
        } = message
        {
            let expected = match direction {
                DeviceDirection::Output => "t_71",
                DeviceDirection::Input => "t_mic",
            };
            assert_eq!(
                node_name,
                expected,
                "the {} lane remembered the wrong default",
                direction.key()
            );
        }
    }
    // Not even for a moment: a lane that went to the seeded sink first and moved on once the
    // metadata arrived would have claimed the default without knowing what it displaced.
    assert!(
        !said
            .attachments(DeviceDirection::Output)
            .contains(&Some("t_stereo".to_owned())),
        "the output lane went to the settings file's sink: {:?}",
        said.attachments(DeviceDirection::Output)
    );

    handle.shutdown();
    assert_eq!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some("t_71"),
        "the default sink should go back to the sink the output lane played to, not stay ours"
    );
    assert_eq!(
        graph.configured_default(DeviceDirection::Input).as_deref(),
        Some("t_mic"),
        "the default source should go back to the microphone that was picked, not stay ours"
    );
}

/// A run that was killed rather than quit leaves both configured defaults naming nodes that died
/// with it, and WirePlumber's state file keeps them that way. The next run finds its own names
/// there and adopts the claims without knowing what they displaced. The settings file's copy,
/// [`UiToAudio::SeedRememberedDefaults`], is what it knows instead: the output lane attaches to
/// the remembered sink rather than to whichever is listed first, and a clean exit points both keys
/// at real devices again.
///
/// Sent the way the app sends it, straight after the start. The first rules run on the first
/// supervisor tick, 200 ms later, so the seed is in place by then.
#[test]
fn a_claim_left_behind_by_a_killed_run_is_handed_back_to_the_seeded_devices() {
    let Some(graph) = PrivateGraph::start("stale") else {
        return;
    };
    for (direction, ours) in [
        (DeviceDirection::Output, SINK_NODE_NAME),
        (DeviceDirection::Input, SOURCE_NODE_NAME),
    ] {
        let Some(()) = unless_skipped(
            graph.write_default(devices::configured_default_key(direction), ours),
            "pw-metadata",
            "the repair of a stale claim",
        ) else {
            return;
        };
        assert_eq!(graph.default_settles_on(direction, ours), Some(Ok(())));
    }

    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    handle.send(UiToAudio::SeedRememberedDefaults {
        output: "t_71".to_owned(),
        input: "t_mic".to_owned(),
    });
    let mut said = Transcript::default();
    assert!(
        said.attached(&handle, DeviceDirection::Output, Some("t_71")),
        "the output lane should go to the seeded sink, not to the first one listed"
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));

    said.settle(&handle);
    assert!(
        !said.0.iter().any(|m| matches!(
            m,
            AudioToUi::RememberedDefault { node_name, .. } if OUR_NODE_NAMES.contains(&node_name.as_str())
        )),
        "a stale claim was remembered as the default from before FxSound"
    );

    handle.shutdown();
    assert_eq!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some("t_71"),
        "the stale default sink should go to the seeded sink"
    );
    assert_eq!(
        graph.configured_default(DeviceDirection::Input).as_deref(),
        Some("t_mic"),
        "the stale default source should go to the microphone"
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
    said.settle(&handle);
    assert_eq!(
        said.attachments(DeviceDirection::Input),
        Vec::<Option<String>>::new()
    );
    if let Some(settled) = unless_skipped(
        graph.settles_on(&[SINK_NODE_NAME, OUTPUT_NODE_NAME]),
        "pw-dump",
        "that the input lane has no nodes",
    ) {
        assert_eq!(settled.map(drop), Ok(()));
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
        assert_eq!(settled.map(drop), Ok(()));
    }
    handle.shutdown();
}

/// A tone through both lanes, each wired end to end as WirePlumber would wire it: a tone source,
/// picked as the microphone, into the capture stream; the same tone into the sink, standing in for
/// an application playing; the output stream into the stereo sink.
///
/// Wired one lane at a time and checked in both after each step, because being scheduled is not
/// being fed. On PipeWire 1.6.8, linking the input lane alone already has the output lane report
/// `processing`: the server drives the members of a `node.link-group` together, and all four of
/// our nodes share `fxsound`. So `processing` cannot tell the lanes apart, and the meters can: the
/// tone reaches the voice chain's with the music chain's still silent, and the music chain's only
/// once something plays into the sink.
#[test]
fn a_tone_driven_through_each_lane_reaches_that_lane_and_no_other() {
    let Some(graph) = PrivateGraph::start("flow") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so no audio was driven through the lanes"
        ));
        return;
    }
    if graph.add_tone("t_tone").is_none() {
        skip(concat!(
            "the tone never appeared (is audiotestsrc installed?), ",
            "so no audio was driven through the lanes"
        ));
        return;
    }
    let mut handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == "t_tone")),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_tone".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_tone")));
    assert_eq!(
        graph
            .settles_on(&OUR_NODE_NAMES)
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "both pairs should be in the graph at once"
    );
    for (node, direction, positions) in [
        ("t_tone", "Output", &["MONO"][..]),
        ("t_stereo", "Input", &["FL", "FR"][..]),
        (CAPTURE_NODE_NAME, "Input", &["MONO"][..]),
        (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
        (OUTPUT_NODE_NAME, "Output", &["FL", "FR"][..]),
    ] {
        assert!(
            graph.configure_ports(node, direction, positions).is_some(),
            "{node} was not given ports"
        );
    }

    // The microphone alone.
    assert!(graph.link_nodes("t_tone", CAPTURE_NODE_NAME));
    let heard = meters_until(&mut handle, DeviceDirection::Input, |m| m.input_peak > 0.1);
    assert!(
        heard.is_some(),
        "the tone never reached the input lane's meters"
    );
    said.settle(&handle);
    let music = handle.meters(DeviceDirection::Output);
    assert!(
        music.peak_left < 1e-3 && music.peak_right < 1e-3,
        "the microphone was heard in the output lane: {music:?}"
    );

    // Now something plays, and the speakers are connected.
    assert!(graph.link_nodes("t_tone", SINK_NODE_NAME));
    assert!(graph.link_nodes(OUTPUT_NODE_NAME, "t_stereo"));
    let played = meters_until(&mut handle, DeviceDirection::Output, |m| {
        m.peak_left > 0.1 && m.peak_right > 0.1
    });
    assert!(
        played.is_some(),
        "the tone never reached the output lane's meters"
    );

    // And each lane says so under its own direction, describing its own pair.
    for direction in DeviceDirection::ALL {
        assert!(
            said.heard(
                &handle,
                &format!("the {} lane processing", direction.key()),
                |m| matches!(m, AudioToUi::Status { direction: d, status }
                    if *d == direction && status.processing),
            ),
            "the {} lane never reported processing",
            direction.key()
        );
    }
    let last_status = |direction: DeviceDirection| {
        said.0
            .iter()
            .rev()
            .find_map(|message| match message {
                AudioToUi::Status {
                    direction: d,
                    status,
                } if *d == direction => Some(*status),
                _ => None,
            })
            .expect("heard a moment ago")
    };
    let (output, input) = (
        last_status(DeviceDirection::Output),
        last_status(DeviceDirection::Input),
    );
    assert_eq!(
        (output.sample_rate, output.channels),
        (48_000, 2),
        "the output lane's status should describe the stereo sink"
    );
    assert_eq!(
        input.sample_rate, 48_000,
        "the input lane's status should describe the 48 kHz capture"
    );
    for (direction, status) in [
        (DeviceDirection::Output, output),
        (DeviceDirection::Input, input),
    ] {
        assert_eq!(
            status.format_mismatches,
            0,
            "wiring the {} lane disturbed the format its pair was built with",
            direction.key()
        );
    }
    handle.shutdown();
}

/// Poll a lane's meters until the predicate accepts them, or the patience runs out.
fn meters_until(
    handle: &mut EngineHandle,
    direction: DeviceDirection,
    pick: impl Fn(&fxsound_core::messages::Meters) -> bool,
) -> Option<fxsound_core::messages::Meters> {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        let meters = handle.meters(direction);
        if pick(&meters) {
            return Some(meters);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("gave up waiting on the {} lane's meters", direction.key());
    None
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
    # For the tone one test adds with `pw-cli create-node`; no object here uses it.
    audiotestsrc    = audiotestsrc/libspa-audiotestsrc
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
