//! The engine against a real PipeWire graph, rather than against its own pure pieces.
//!
//! Everything else in this crate's tests takes a function with no server behind it: the ring, the
//! quantum arithmetic, the channel map, the pod builders. None of them touches [`supervise`],
//! [`connect`], `apply_rules` or `build_nodes` — the code that actually decides what FxSound does
//! to somebody's audio graph — because those need a server, and pointing them at the developer's
//! own graph would create nodes in it and take their default device away mid-test. The one
//! exception, `engine::live_session`, calls them by hand against a daemon of this module's.
//!
//! So each test here starts a **private** PipeWire: its own daemon, its own socket, its own
//! synthetic devices, nothing shared with the session. Two things make that possible without
//! touching the process environment, which matters because `std::env::set_var` is unsound with
//! threads and this crate denies unsafe code:
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
//! Nothing a test starts here may outlive it. Every daemon and tool is started through
//! `fxsound_core::test_support`, which kills it when its handle is dropped — a panicking test
//! drops it too — and has the kernel, or a watchdog where `setpriv` is missing, kill it when the
//! test process dies, however it dies: an interrupted run once left daemons and `pw-record`s
//! writing raw audio into `/tmp` for hours. Recorders are also capped at [`RECORDING_LIMIT`]. And
//! each graph gets a directory made new for it, never one an earlier run left under the same name
//! with an old daemon's socket still in it.
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
//! and that is the session manager's job too. So the tests that need audio to flow — or a node to
//! run at all, which is what the idle tests watch — do both steps themselves: `pw-cli set-param …
//! PortConfig` on every node involved, then `pw-link`, and `pw-link -d` for an application that
//! stops. Their source is a tone from PipeWire's `audiotestsrc`, added with `pw-cli create-node`,
//! rather than the null microphone: a null device plays silence, and silence moves no meter, so it
//! could show that a lane is scheduled but never that it is fed — and on this server those are not
//! the same.
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
use fxsound_core::test_support::{self as support, Guarded, ScratchDir};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

mod apps;
mod policy;
mod routes;
mod sleep;
mod volume;

/// How long to wait for anything the server has to do.
pub(crate) const PATIENCE: Duration = Duration::from_secs(10);

/// Our nodes as the server lists them: each name with its `object.serial`.
type OurNodes = Vec<(&'static str, u64)>;

/// The most a recorder of these tests may write: a `pw-record` that nobody stopped stops here,
/// about eleven minutes of 48 kHz stereo `f32` in, rather than filling `/tmp`.
pub(crate) const RECORDING_LIMIT: u64 = 256 << 20;

/// The clock of [`PrivateGraph::clocked_recorder`]: a null sink the engine does not take for a
/// device, with a real device's driver priority.
pub(crate) const CLOCK: &str = "t_clock";

/// Set to `1` where every check here must run: `.github/workflows/ci.yml` sets it, because it
/// installs the daemon, the `pw-*` tools and the `audiotestsrc` plugin for these tests, and there a
/// skip can only mean that something which should work did not.
const REQUIRE_TOOLS: &str = "FXSOUND_REQUIRE_PIPEWIRE_TOOLS";

/// Report a check that could not run: a `SKIPPED` line and a pass on a contributor's machine, a
/// failure under [`REQUIRE_TOOLS`].
pub(crate) fn skip(reason: &str) {
    assert!(
        std::env::var_os(REQUIRE_TOOLS).is_none_or(|value| value != "1"),
        "{reason}, and {REQUIRE_TOOLS}=1 says nothing here may be skipped"
    );
    println!("SKIPPED: {reason}");
}

/// A PipeWire daemon of our own: one stereo sink, one 7.1 sink, one virtual microphone, and the
/// `default` metadata object a session manager would otherwise create — with no session manager
/// behind it, so nothing links anything and nothing moves a default but us. Like every daemon it
/// also publishes a `settings` object for its own clock, which is where a test forces its rate.
///
/// Crate-visible for `engine::live_session`, whose tests stand in for the engine's main loop
/// rather than drive its handle.
pub(crate) struct PrivateGraph {
    /// The daemon's runtime directory, configuration and whatever the tests write beside them:
    /// made for this graph, and removed after the daemons are gone.
    dir: ScratchDir,
    child: Guarded,
    /// The private system bus of a graph that can hold cards ([`Self::start_with_cards`]).
    bus: Option<Guarded>,
}

impl PrivateGraph {
    /// `None` when there is no `pipewire` to start, which is not a failure — unless
    /// [`REQUIRE_TOOLS`] says it is.
    pub(crate) fn start(tag: &str) -> Option<Self> {
        Self::spawn(tag, false)
            .inspect_err(|why| skip(&format!("{why}, so {tag} cannot run")))
            .ok()
    }

    /// [`Self::start`], for a daemon that can also hold cards — PipeWire's `Device` objects, which
    /// nodes belong to ([`Self::add_card`]).
    ///
    /// Every factory that makes a `Device` wants hardware — a sound card, a camera — except
    /// Bluetooth's enumerator, which wants a system bus and finds no BlueZ on it. So this daemon
    /// gets a system bus of its own, a `dbus-daemon` with its socket in the graph's directory, and
    /// its environment names that bus as the system bus and a socket that does not exist as the
    /// session bus. Nothing it does can reach the machine's own buses, or the BlueZ behind them.
    pub(crate) fn start_with_cards(tag: &str) -> Option<Self> {
        Self::spawn(tag, true)
            .inspect_err(|why| skip(&format!("{why}, so {tag} cannot run")))
            .ok()
    }

    /// [`Self::start`] or [`Self::start_with_cards`], saying why when there is no daemon to be
    /// had.
    fn spawn(tag: &str, cards: bool) -> Result<Self, String> {
        let config = if cards {
            card_config()
        } else {
            CONFIG.to_owned()
        };
        Self::spawn_with(tag, &config, cards)
    }

    /// [`Self::spawn`] with `config` for the daemon's configuration file.
    fn spawn_with(tag: &str, config: &str, cards: bool) -> Result<Self, String> {
        if !installed("pipewire") {
            return Err("pipewire is not installed".to_owned());
        }
        if cards {
            if !installed("dbus-daemon") {
                return Err("dbus-daemon is not installed".to_owned());
            }
            if !library_installed("spa-0.2/bluez5/libspa-bluez5.so") {
                return Err("libspa-bluez5 is not installed".to_owned());
            }
        }

        // Short, which in practice means directly under /tmp: a Unix socket path may not exceed
        // 108 bytes, and a path beside the source tree is already most of that before the socket
        // name is added. And new: a directory an earlier run left, perhaps with its daemon's
        // socket still in it, is never taken for this one.
        let dir = ScratchDir::for_sockets(&format!("t-{tag}"));
        let run = dir.join("run");
        let conf = dir.join("pipewire.conf");
        std::fs::create_dir_all(&run)
            .and_then(|()| std::fs::File::create(&conf))
            .and_then(|mut file| file.write_all(config.as_bytes()))
            .map_err(|error| format!("{} could not be prepared: {error}", dir.display()))?;

        let bus = if cards {
            Some(Self::start_bus(&dir)?)
        } else {
            None
        };

        let mut daemon = support::command("pipewire");
        daemon
            .arg("-c")
            .arg(&conf)
            .env("XDG_RUNTIME_DIR", &run)
            .env("PIPEWIRE_DEBUG", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if cards {
            daemon
                .env(
                    "DBUS_SYSTEM_BUS_ADDRESS",
                    format!("unix:path={}", dir.join("bus").display()),
                )
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", dir.join("no-session-bus").display()),
                );
        }
        let child = support::spawn(daemon)
            .map_err(|error| format!("pipewire could not be started: {error}"))?;

        let graph = Self { dir, child, bus };
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

    /// A `dbus-daemon` for [`Self::start_with_cards`], listening in `dir` and nowhere else, and
    /// waited for until its socket is there.
    fn start_bus(dir: &std::path::Path) -> Result<Guarded, String> {
        let conf = dir.join("bus.conf");
        let socket = dir.join("bus");
        let config = format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>system</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
            socket.display()
        );
        std::fs::write(&conf, config)
            .map_err(|error| format!("{} could not be written: {error}", conf.display()))?;
        let mut bus = support::command("dbus-daemon");
        bus.arg(format!("--config-file={}", conf.display()))
            .arg("--nofork")
            .arg("--nopidfile")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let bus = support::spawn(bus)
            .map_err(|error| format!("dbus-daemon could not be started: {error}"))?;
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if socket.exists() {
                return Ok(bus);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err("the private system bus never came up".to_owned())
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("run/fxsound-test-0")
    }

    /// Kill the daemon under whatever is connected to it: what a user meets as "PipeWire
    /// restarted". The directory stays until the graph is dropped.
    pub(crate) fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub(crate) fn remote(&self) -> String {
        self.socket().display().to_string()
    }

    /// Run one of PipeWire's command-line tools against this daemon and nothing else. The socket
    /// is named on the command line, and the child's runtime directory is the private one, so a
    /// tool that ignored `-r` would find no session socket to fall back on either. `None` when the
    /// tool is not installed or failed, which the callers treat as "cannot tell", not as a pass.
    ///
    /// A tool that ran and failed says how, and what it wrote to its standard error, in the
    /// test's output — which the harness shows only for a test that fails.
    pub(crate) fn tool(&self, program: &str, args: &[&str]) -> Option<String> {
        let output = support::command(program)
            .arg("-r")
            .arg(self.socket())
            .args(args)
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.socket())
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            println!(
                "{program} {} failed with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Everything in the graph, as `pw-dump` prints it. `None` when `pw-dump` is not there to ask.
    fn dump(&self) -> Option<Vec<serde_json::Value>> {
        let dump = self.tool("pw-dump", &[])?;
        Some(merged_dump(&dump))
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
    pub(crate) fn node_id(&self, name: &str) -> Option<u64> {
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
    pub(crate) fn configure_ports(
        &self,
        node: &str,
        direction: &str,
        positions: &[&str],
    ) -> Option<()> {
        self.port_config(node, direction, positions, false)
    }

    /// [`Self::configure_ports`] for a sink whose monitor is recorded as well: its input ports, and
    /// a monitor port per channel beside them, which is what the echo canceller's monitor stream
    /// is linked to. Waits for the input ports only; the monitor ports come with them.
    pub(crate) fn configure_monitored_ports(&self, node: &str, positions: &[&str]) -> Option<()> {
        self.port_config(node, "Input", positions, true)
    }

    fn port_config(
        &self,
        node: &str,
        direction: &str,
        positions: &[&str],
        monitor: bool,
    ) -> Option<()> {
        let id = self.node_id(node)?.to_string();
        let channels = positions.len();
        let listed = positions
            .iter()
            .map(|position| format!("\"{position}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let config = format!(
            "{{ \"direction\": \"{direction}\", \"mode\": \"dsp\", \"monitor\": {monitor}, \
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
    pub(crate) fn link_nodes(&self, from: &str, to: &str) -> bool {
        self.wire(from, to, &[])
    }

    /// Undo [`Self::link_nodes`]: the links an application leaves behind when it closes its
    /// stream. Whether every one of them was removed.
    pub(crate) fn unlink_nodes(&self, from: &str, to: &str) -> bool {
        self.wire(from, to, &["-d"])
    }

    /// Run `pw-link` with `flags` over every port pair [`Self::link_nodes`] would link.
    fn wire(&self, from: &str, to: &str, flags: &[&str]) -> bool {
        let (Some(outputs), Some(inputs)) = (self.ports(from, "out"), self.ports(to, "in")) else {
            return false;
        };
        let mut done = !outputs.is_empty() && !inputs.is_empty();
        for (output, channel) in &outputs {
            for (input, _) in inputs
                .iter()
                .filter(|(_, other)| outputs.len() == 1 || other == channel)
            {
                let (output, input) = (format!("{from}:{output}"), format!("{to}:{input}"));
                let args: Vec<&str> = flags.iter().copied().chain([&*output, &*input]).collect();
                let made = self.tool("pw-link", &args).is_some();
                if !made {
                    println!("pw-link {flags:?} refused {output} -> {input}");
                }
                done &= made;
            }
        }
        done
    }

    /// One property of the node called `name`, as the server holds it, written out as the text it
    /// was set from: `pw-dump` prints a property that reads as a boolean or a number as one, so
    /// `node.passive = "true"` comes back as `true` and not `"true"`. `None` when `pw-dump` is not
    /// there to ask; `Some(None)` when the node is not in the graph or does not carry the key.
    pub(crate) fn node_prop(&self, name: &str, key: &str) -> Option<Option<String>> {
        let objects = self.dump()?;
        Some(
            Self::node_object(&objects, name).and_then(|node| match &node["info"]["props"][key] {
                serde_json::Value::String(text) => Some(text.clone()),
                serde_json::Value::Bool(flag) => Some(flag.to_string()),
                serde_json::Value::Number(number) => Some(number.to_string()),
                _ => None,
            }),
        )
    }

    /// Wait until the node called `name` carries `key = want`. `None` when `pw-dump` is not there
    /// to ask; otherwise `Err` with what it last carried when it never came to.
    pub(crate) fn prop_settles_on(
        &self,
        name: &str,
        key: &str,
        want: &str,
    ) -> Option<Result<(), Option<String>>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = self.node_prop(name, key)?;
            if seen.as_deref() == Some(want) {
                return Some(Ok(()));
            }
            if Instant::now() >= deadline {
                return Some(Err(seen));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The state the server holds a node in — `"running"`, `"idle"`, `"suspended"`, … — or `None`
    /// when `pw-dump` is not there to ask or the node is not in the graph.
    pub(crate) fn node_state(&self, name: &str) -> Option<String> {
        let objects = self.dump()?;
        Self::node_object(&objects, name)?["info"]["state"]
            .as_str()
            .map(str::to_owned)
    }

    /// Wait until `name` runs (`running`) or does not. `Ok` with the state it settled in, `Err`
    /// with the last one seen when it never did.
    pub(crate) fn runs_until(&self, name: &str, running: bool) -> Result<String, Option<String>> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let state = self.node_state(name);
            if let Some(state) = &state
                && (state == "running") == running
            {
                return Ok(state.clone());
            }
            if Instant::now() >= deadline {
                return Err(state);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Start an application recording from the node called `from`: `pw-record` as a stream called
    /// `name`, given the input ports a session manager would give it and linked from `from`'s
    /// outputs, as everything on this graph is linked. It writes what it hears to a file of its own
    /// as raw interleaved stereo `f32` ([`Recorder::peak_since`]). `None` when it never appeared or
    /// could not be linked.
    ///
    /// Raw because the file's name ends in `.raw`, which is libsndfile's header-less format, and
    /// not through `--raw`, which the `pw-record` of PipeWire 1.0 — Ubuntu 24.04's, where CI runs —
    /// does not have.
    pub(crate) fn record_from(&self, from: &str, name: &str) -> Option<Recorder> {
        let file = self.dir.join(format!("{name}.raw"));
        let props = format!("{{ node.name = {name} }}");
        let mut recorder = support::command_writing_at_most("pw-record", RECORDING_LIMIT);
        recorder
            .arg("--remote")
            .arg(self.socket())
            .args(["--target", "0", "-P", &props, "--format", "f32"])
            .args(["--rate", "48000", "--channels", "2"])
            .arg(&file)
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.socket())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        let log = self.stderr_log(&mut recorder, name);
        let child = match support::spawn(recorder) {
            Ok(child) => child,
            Err(error) => {
                println!("pw-record could not be started for {name}: {error}");
                return None;
            }
        };
        let mut recorder = Recorder {
            child,
            name: name.to_owned(),
            file,
        };
        let deadline = Instant::now() + PATIENCE;
        while self.node_id(name).is_none() {
            if Instant::now() >= deadline {
                println!(
                    "{name} never appeared in the graph: pw-record; {}",
                    recorder.child.account(Some(&log))
                );
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let linked = self.configure_ports(name, "Input", &["FL", "FR"]).is_some()
            && self.link_nodes(from, name);
        if !linked {
            println!(
                "{name} could not be given ports or linked from {from}: pw-record; {}",
                recorder.child.account(Some(&log))
            );
            return None;
        }
        Some(recorder)
    }

    /// Send the standard error of `command`, a tool run against this graph, to a file of its own
    /// in the graph's directory, named after `name`, and say where: for
    /// [`Guarded::account`] to quote when the tool does not do what it was started for.
    pub(crate) fn stderr_log(&self, command: &mut std::process::Command, name: &str) -> PathBuf {
        let mut log = self.dir.join(format!("{name}.stderr"));
        let mut n = 1;
        while log.exists() {
            n += 1;
            log = self.dir.join(format!("{name}-{n}.stderr"));
        }
        command.stderr(support::log_to(&log));
        log
    }

    /// [`Self::record_from`], for a recorder that runs on a clock of its own before it records
    /// anything of ours: a null sink — which the engine does not take for a device — with a real
    /// device's driver priority, whose monitor the recorder records from the start. Whatever the
    /// test links into the recorder after that is driven by the clock, as a real device drives
    /// what an application records, and not by the test's tone.
    ///
    /// That matters wherever a group the tone is linked into stops and starts again, which is
    /// every time an application stops and starts recording from FxSound (Input). Driving such a
    /// group on a machine busy with other work, `audiotestsrc` was seen to run out of buffers
    /// (`spa.audiotestsrc: make_buffer(): out of buffers`, in the daemon's log) and stop the cycle
    /// for good — about one start in five with four test runs side by side, with the nodes all
    /// saying `running` — where a null sink driving the same group, as here, never did in thirty.
    /// A real microphone's driver is the null sink's kind, not the tone's.
    pub(crate) fn clocked_recorder(&self, name: &str) -> Option<Recorder> {
        self.add_clock()?;
        self.record_from(CLOCK, name)
    }

    /// Add the clock of [`Self::clocked_recorder`], once, with its monitor ports: linked into a
    /// stream, it drives whatever that stream records. `None` when it never appeared.
    pub(crate) fn add_clock(&self) -> Option<()> {
        if self.node_id(CLOCK).is_some() {
            return Some(());
        }
        self.add_adapter(
            CLOCK,
            &format!(
                "factory.name = support.null-audio-sink node.name = {CLOCK} \
                 node.description = \"Test Clock\" media.class = Audio/Sink/Internal \
                 priority.driver = 1010 audio.channels = 2 audio.position = [ FL FR ]"
            ),
        )?;
        self.configure_monitored_ports(CLOCK, &["FL", "FR"])
    }

    /// Add a source that plays a steady tone: PipeWire's `audiotestsrc` behind an adapter, which
    /// is what makes a lane's meters move where a null device's silence cannot. Created by a
    /// client that then leaves, so it lingers. `None` when the plugin is not installed and the node
    /// never appears.
    pub(crate) fn add_tone(&self, name: &str) -> Option<()> {
        self.add_adapter(
            name,
            &format!(
                "factory.name = audiotestsrc node.name = {name} node.description = \"Test Tone\" \
                 media.class = Audio/Source"
            ),
        )
    }

    /// Add a one-channel sink while the engine runs: a null sink laid out `MONO`, which is what a
    /// Bluetooth headset in its call profile or a mono USB headset looks like to the rules. The
    /// graph's own devices are all stereo or better, so a test that wants a mono one adds it — and
    /// adding it later is also how it arrives as a *new* device. `None` when it never appears.
    ///
    /// It gets the `priority.driver` WirePlumber gives a Bluetooth sink (`name-node.lua`), and not
    /// only for likeness. Left at 0 it ties with the tone, which is a driver too and was created
    /// first; with two drivers of equal priority joined through our pair, this daemon was seen to
    /// stop the whole graph after half a second — a runtime-made stereo null sink the same, the
    /// graph's own `t_stereo` not. No session builds that graph: every real device carries a
    /// priority of its own.
    pub(crate) fn add_mono_sink(&self, name: &str) -> Option<()> {
        self.add_adapter(
            name,
            &format!(
                "factory.name = support.null-audio-sink node.name = {name} \
                 node.description = \"Test Mono Out\" media.class = Audio/Sink \
                 priority.driver = 1010 audio.channels = 1 audio.position = [ MONO ]"
            ),
        )
    }

    /// Add a card — a PipeWire `Device` object — named `name`, with the Bluetooth address
    /// `address` in its properties, and keep it in the graph for as long as the returned holder
    /// lives. Only on a graph started with [`Self::start_with_cards`]. `None` when it never
    /// appears.
    ///
    /// It is Bluetooth's enumerator, which on this graph's empty bus enumerates nothing: a card
    /// with no nodes of its own, which a test's nodes then name in `device.id` or share an address
    /// with, as WirePlumber's nodes do with a headset's card. It is made by a `pw-cli` that stays
    /// connected, because a device belongs to the client that made it and goes when that client
    /// does — which is also how a test takes it away again.
    pub(crate) fn add_card(&self, name: &str, address: &str) -> Option<CardHolder> {
        let mut client = support::command("pw-cli");
        client
            .arg("-r")
            .arg(self.socket())
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.socket())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = support::spawn(client).ok()?;
        let command = format!(
            "create-device spa-device-factory {{ factory.name = api.bluez5.enum.dbus \
             device.name = {name} device.api = bluez5 api.bluez5.address = \"{address}\" }}\n"
        );
        // Written and left open: `pw-cli` reads its commands from it, and stays for as long as
        // there may be more.
        let written = child
            .stdin()
            .is_some_and(|stdin| stdin.write_all(command.as_bytes()).is_ok());
        let mut holder = CardHolder {
            _child: child,
            id: 0,
        };
        if !written {
            return None;
        }
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            let found = self.dump()?.iter().find_map(|object| {
                (object["type"].as_str() == Some("PipeWire:Interface:Device")
                    && object["info"]["props"]["device.name"].as_str() == Some(name))
                .then(|| object["id"].as_u64())
                .flatten()
            });
            if let Some(id) = found {
                holder.id = id;
                return Some(holder);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// One property of the card called `name`, the way [`Self::node_prop`] reads a node's.
    pub(crate) fn card_prop(&self, name: &str, key: &str) -> Option<Option<String>> {
        let objects = self.dump()?;
        Some(
            objects
                .iter()
                .find(|object| {
                    object["type"].as_str() == Some("PipeWire:Interface:Device")
                        && object["info"]["props"]["device.name"].as_str() == Some(name)
                })
                .and_then(|card| card["info"]["props"][key].as_str().map(str::to_owned)),
        )
    }

    /// Add a stereo sink that belongs to a card: `card` is the properties that say which —
    /// `device.id = <id>`, `api.bluez5.address = "<address>"`, or both. A headset's sink as
    /// WirePlumber makes one, down to the driver priority (see [`Self::add_mono_sink`] for why that
    /// matters here). `None` when it never appears.
    pub(crate) fn add_card_sink(&self, name: &str, card: &str) -> Option<()> {
        self.add_adapter(
            name,
            &format!(
                "factory.name = support.null-audio-sink node.name = {name} \
                 node.description = \"Test Headset\" media.class = Audio/Sink \
                 priority.driver = 1010 audio.channels = 2 audio.position = [ FL FR ] {card}"
            ),
        )
    }

    /// Whether any link runs from the node called `from` to the node called `to`. `None` when
    /// `pw-dump` is not there to ask; `Some(false)` when either node is not in the graph.
    pub(crate) fn linked(&self, from: &str, to: &str) -> Option<bool> {
        let objects = self.dump()?;
        let id = |name| Self::node_object(&objects, name).and_then(|node| node["id"].as_u64());
        let (Some(from), Some(to)) = (id(from), id(to)) else {
            return Some(false);
        };
        Some(objects.iter().any(|object| {
            object["type"].as_str() == Some("PipeWire:Interface:Link")
                && object["info"]["output-node-id"].as_u64() == Some(from)
                && object["info"]["input-node-id"].as_u64() == Some(to)
        }))
    }

    /// Do to the output lane's playback stream what WirePlumber 0.5.17 does to it, as far as
    /// linking goes, since there is no WirePlumber here to do it. The stream is
    /// `node.dont-reconnect`, so: one never linked before is linked to its `target.object`; one
    /// linked before is left alone, whatever became of its link — even with its target back under
    /// the same name (`linking/prepare-link.lua:71-76`). `handled` is WirePlumber's `was_handled`:
    /// the serials of the streams linked so far.
    ///
    /// Whether it linked the stream: `Some(false)` for one it had linked before. `None` when a
    /// tool is missing or refused, the stream is not there, or its target is not.
    pub(crate) fn link_like_wireplumber(&self, handled: &mut Vec<u64>) -> Option<bool> {
        let serial = serial_of(&self.our_nodes()?, OUTPUT_NODE_NAME)?;
        if handled.contains(&serial) {
            return Some(false);
        }
        let target = self.node_prop(OUTPUT_NODE_NAME, "target.object")??;
        self.configure_ports(OUTPUT_NODE_NAME, "Output", &["FL", "FR"])?;
        self.configure_ports(&target, "Input", &["FL", "FR"])?;
        if !self.link_nodes(OUTPUT_NODE_NAME, &target) {
            return None;
        }
        handled.push(serial);
        Some(true)
    }

    /// Create an adapter node with `props`, by a client that then leaves, so it lingers — and wait
    /// until the server lists it under `name`.
    fn add_adapter(&self, name: &str, props: &str) -> Option<()> {
        let props = format!("{{ {props} object.linger = true }}");
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

    /// Take the node called `name` out of the graph, the way a device goes when it is unplugged,
    /// and wait until the server no longer lists it. `None` when it was not there to take, or
    /// never went.
    pub(crate) fn remove_node(&self, name: &str) -> Option<()> {
        let id = self.node_id(name)?.to_string();
        // As for creating one: whether it goes is the answer, not what `pw-cli` exits with.
        let _ = self.tool("pw-cli", &["destroy", &id]);
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if self.node_id(name).is_none() {
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
    pub(crate) fn write_default(&self, key: &str, node_name: &str) -> Option<()> {
        let value = devices::default_node_value(node_name);
        self.tool(
            "pw-metadata",
            &["-n", "default", "0", key, &value, "Spa:String:JSON"],
        )
        .map(drop)
    }

    /// What `default.configured.audio.<sink|source>` names right now, if anything.
    pub(crate) fn configured_default(&self, direction: DeviceDirection) -> Option<String> {
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
    pub(crate) fn default_settles_on(
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
    pub(crate) fn node_format(&self, node_name: &str) -> Option<Option<(u64, u64)>> {
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

/// An application recording from the graph ([`PrivateGraph::record_from`]). Dropped, it stops, and
/// its links go with it.
pub(crate) struct Recorder {
    child: Guarded,
    /// Its `node.name`, to link and unlink it by.
    pub(crate) name: String,
    file: std::path::PathBuf,
}

impl Recorder {
    /// How many bytes it has written so far: a mark to read what it hears from on.
    pub(crate) fn written(&self) -> usize {
        std::fs::metadata(&self.file).map_or(0, |meta| meta.len() as usize)
    }

    /// The loudest sample it has written from byte `from` of its file on, and how many samples
    /// that was.
    pub(crate) fn peak_since(&self, from: usize) -> (f32, usize) {
        let bytes = std::fs::read(&self.file).unwrap_or_default();
        // From the first whole sample at or after the mark: the file may end part-way through one.
        let start = from.div_ceil(4) * 4;
        let (words, _) = bytes.get(start..).unwrap_or_default().as_chunks::<4>();
        let peak = words
            .iter()
            .map(|word| f32::from_le_bytes(*word).abs())
            .fold(0.0_f32, f32::max);
        (peak, words.len())
    }

    /// Whether it hears something from byte `from` of its file on — a sample louder than −20 dBFS
    /// — within the patience.
    pub(crate) fn hears_since(&self, from: usize) -> bool {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let (peak, written) = self.peak_since(from);
            if peak > 0.1 {
                return true;
            }
            if Instant::now() >= deadline {
                println!(
                    "{} heard nothing louder than {peak} in {written} samples",
                    self.name
                );
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until it has written at least `samples` more from byte `from` on, and say how loud they
    /// were at the loudest. `None` when it never did.
    pub(crate) fn heard_since(&self, from: usize, samples: usize) -> Option<f32> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let (peak, written) = self.peak_since(from);
            if written >= samples {
                return Some(peak);
            }
            if Instant::now() >= deadline {
                println!("{} wrote {written} samples, not {samples}", self.name);
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A card [`PrivateGraph::add_card`] made, and the `pw-cli` that holds it in the graph. Dropped,
/// the client goes and its card with it.
pub(crate) struct CardHolder {
    /// Kept for its drop, which kills the client.
    _child: Guarded,
    /// The card's registry id: what its nodes name in `device.id`.
    pub(crate) id: u64,
}

/// Whether a program can be started at all. Asked up front by a test that cannot do without a
/// tool, because [`PrivateGraph::tool`]'s `None` also means "ran and refused", which for such a
/// test is a failure and not a reason to skip.
pub(crate) fn installed(program: &str) -> bool {
    support::command(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Whether a file PipeWire loads into a client at run time is installed: `relative` under any
/// `/usr/lib*` directory, or under a multiarch directory below one (`/usr/lib/x86_64-linux-gnu`),
/// which is where Debian and Ubuntu put `spa-0.2` and `pipewire-0.3`.
pub(crate) fn library_installed(relative: &str) -> bool {
    let Ok(usr) = std::fs::read_dir("/usr") else {
        return false;
    };
    usr.flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("lib"))
        .any(|lib| {
            let lib = lib.path();
            lib.join(relative).exists()
                || std::fs::read_dir(&lib).is_ok_and(|below| {
                    below
                        .flatten()
                        .any(|entry| entry.path().join(relative).exists())
                })
        })
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
pub(crate) fn unless_skipped<T>(checked: Option<T>, tool: &str, what: &str) -> Option<T> {
    if checked.is_none() {
        skip(&format!(
            "{tool} is not available or failed, so {what} was not checked"
        ));
    }
    checked
}

impl Drop for PrivateGraph {
    /// The daemons go first, and their directory after them, with the fields.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(bus) = self.bus.as_mut() {
            let _ = bus.kill();
            let _ = bus.wait();
        }
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

    /// [`Self::heard`], counting only what the engine said from message `from` on — so a message
    /// of the same kind from before, which is still in the transcript, does not answer for it.
    fn heard_since(
        &mut self,
        handle: &EngineHandle,
        from: usize,
        what: &str,
        mut pick: impl FnMut(&AudioToUi) -> bool,
    ) -> bool {
        self.0
            .get(from..)
            .is_some_and(|since| since.iter().any(&mut pick))
            || self.until(handle, what, pick)
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

/// What `pw-dump` printed, as one list of objects. Usually that is one JSON array; but a
/// `pw-dump` of PipeWire 1.0 that sees the graph change while it gathers it prints a second array
/// after the first, with the objects that changed since — each whole, or, for one that has gone,
/// its id with `"info": null` — so each array after the first is applied to the list by id.
fn merged_dump(dump: &str) -> Vec<serde_json::Value> {
    let mut arrays = serde_json::Deserializer::from_str(dump).into_iter::<Vec<serde_json::Value>>();
    let mut objects = match arrays.next() {
        Some(Ok(objects)) => objects,
        other => panic!("pw-dump should print a JSON array ({other:?}):\n{dump}"),
    };
    for update in arrays {
        let update = update.unwrap_or_else(|error| {
            panic!("pw-dump printed something after its array that is not one ({error}):\n{dump}")
        });
        for object in update {
            let id = object["id"].as_u64();
            let at = objects.iter().position(|old| old["id"].as_u64() == id);
            match (
                at,
                object.get("info").is_some_and(serde_json::Value::is_null),
            ) {
                (Some(at), true) => {
                    objects.remove(at);
                }
                (Some(at), false) => objects[at] = object,
                (None, true) => {}
                (None, false) => objects.push(object),
            }
        }
    }
    objects
}

#[test]
fn a_dump_printed_in_two_arrays_is_read_as_the_graph_after_both() {
    let dump = r#"[
  { "id": 0, "type": "PipeWire:Interface:Core", "info": { "name": "core" } },
  { "id": 31, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "old" } } },
  { "id": 32, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "gone" } } }
]
[
  { "id": 31, "type": "PipeWire:Interface:Node", "info": { "props": { "node.name": "new" } } },
  { "id": 32, "info": null },
  { "id": 40, "type": "PipeWire:Interface:Link", "info": {} }
]
"#;
    let objects = merged_dump(dump);
    let ids: Vec<u64> = objects.iter().filter_map(|o| o["id"].as_u64()).collect();
    assert_eq!(ids, [0, 31, 40]);
    assert_eq!(objects[1]["info"]["props"]["node.name"], "new");
    // The usual case: one array, read as it is.
    assert_eq!(merged_dump("[]\n"), Vec::<serde_json::Value>::new());
}

/// Set, in a test process of this binary's that the tests below start, to what the child half
/// ([`the_child_half_starts_its_daemons_and_then_panics_or_waits`]) does once its daemons run:
/// `panic`, or wait to be killed.
const CHILD_HALF: &str = "FXSOUND_GRAPH_CHILD_HALF";

/// Not a test of its own: the half of the two tests below that runs in a test process of this
/// binary's. It starts a private PipeWire, a `pw-record` recording from it and a private
/// `dbus-daemon`, says which processes they are and where their directory is, and then does what
/// [`CHILD_HALF`] says — panics, or waits a minute to be killed. Without it, it passes at once.
///
/// Before it panics it waits for a line on its standard input, which [`graph_child_half`] writes
/// once it has read the daemons' start times. Panicking straight away raced that reading: on a
/// loaded machine the unwinding had already stopped the daemons, and the parent found nothing to
/// take a start time from. A standard input that is not a pipe ends at once and does not hold it.
#[test]
fn the_child_half_starts_its_daemons_and_then_panics_or_waits() {
    let Some(then) = std::env::var_os(CHILD_HALF) else {
        return;
    };
    let graph = PrivateGraph::start("child-half").expect("a private pipewire");
    let recorder = graph.clocked_recorder("t_rec").expect("a recorder");
    let bus = PrivateGraph::start_bus(&graph.dir).expect("a private dbus-daemon");
    println!(
        "daemons {} {} {} in {}",
        graph.child.id(),
        recorder.child.id(),
        bus.id(),
        graph.dir.display()
    );
    std::io::stdout().flush().expect("the ids are out");
    if then == "panic" {
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    assert_ne!(then, "panic", "on purpose, with the daemons still running");
    std::thread::sleep(Duration::from_secs(60));
}

/// When the process `pid` started, in clock ticks since boot (`/proc/<pid>/stat`, field 22): with
/// the id, what tells a process from a later one that was given the same id. `None` once it has
/// gone, or is a zombie waiting to be reaped.
fn started_at(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The fields after the command name, which is in parentheses; the state comes first.
    let fields: Vec<&str> = stat.rsplit_once(") ")?.1.split(' ').collect();
    if matches!(fields.first(), Some(&("Z" | "X"))) {
        return None;
    }
    fields.get(19)?.parse().ok()
}

/// Which of the processes `started` — each an id and a start time — still run once `patience`
/// has passed; none as soon as none does.
fn still_running_after(started: &[(u32, u64)], patience: Duration) -> Vec<u32> {
    let deadline = Instant::now() + patience;
    loop {
        let running: Vec<u32> = started
            .iter()
            .filter(|&&(pid, start)| started_at(pid) == Some(start))
            .map(|&(pid, _)| pid)
            .collect();
        if running.is_empty() || Instant::now() >= deadline {
            return running;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The child half as the tests below see it.
struct ChildHalf {
    /// The test process — guarded itself, so a test here that fails cannot leave it waiting.
    process: Guarded,
    /// Its daemons, each with its start time.
    started: Vec<(u32, u64)>,
    /// Their directory.
    dir: PathBuf,
}

/// Start the child half with [`CHILD_HALF`] set to `then`, and read what it says. `None`, after
/// saying so, when a tool it needs is not installed.
fn graph_child_half(then: &str) -> Option<ChildHalf> {
    use std::io::{BufRead as _, BufReader};

    let tools = [
        "pipewire",
        "pw-record",
        "pw-cli",
        "pw-link",
        "pw-dump",
        "dbus-daemon",
    ];
    if let Some(missing) = tools.into_iter().find(|tool| !installed(tool)) {
        skip(&format!(
            "{missing} is not installed, so no daemon could be left behind"
        ));
        return None;
    }
    let (_, module) = module_path!()
        .split_once("::")
        .expect("a module of the crate");
    let name = format!("{module}::the_child_half_starts_its_daemons_and_then_panics_or_waits");
    let mut process = support::command(std::env::current_exe().expect("this test binary"));
    process
        .args([name.as_str(), "--exact", "--test-threads=1", "--nocapture"])
        .env(CHILD_HALF, then)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut process = support::spawn(process).expect("the child half");
    let lines = BufReader::new(process.take_stdout().expect("piped")).lines();
    // The harness puts the test's name in front of what it prints, on the same line.
    let (pids, dir) = lines
        .map_while(Result::ok)
        .find_map(|line| {
            let (_, said) = line.rsplit_once("daemons ")?;
            let (ids, dir) = said.split_once(" in ")?;
            let ids: Option<Vec<u32>> = ids.split(' ').map(|id| id.parse().ok()).collect();
            Some((ids?, PathBuf::from(dir)))
        })
        .expect("the child half said which its daemons are");
    let started = pids
        .into_iter()
        .map(|pid| (pid, started_at(pid).expect("a daemon the child half runs")))
        .collect();
    // Noted: a child half that is to panic may now.
    let stdin = process.stdin().expect("piped");
    writeln!(stdin).expect("the child half reads its standard input");
    stdin.flush().expect("the line is out");
    Some(ChildHalf {
        process,
        started,
        dir,
    })
}

#[test]
fn a_test_that_panics_leaves_no_private_daemon_behind() {
    let Some(ChildHalf {
        mut process,
        started,
        dir,
    }) = graph_child_half("panic")
    else {
        return;
    };
    let status = process.wait().expect("the child half ended");
    assert!(!status.success(), "its test panicked, so it failed");
    assert_eq!(
        still_running_after(&started, Duration::ZERO),
        Vec::<u32>::new()
    );
    assert!(!dir.exists(), "{} is still there", dir.display());
}

#[test]
fn a_test_killed_outright_leaves_no_private_daemon_behind() {
    let Some(ChildHalf {
        mut process,
        started,
        dir,
    }) = graph_child_half("wait")
    else {
        return;
    };
    process.kill().expect("SIGKILL");
    process.wait().expect("the child half ended");
    let left = still_running_after(&started, PATIENCE);

    // No destructor ran in the killed process, so its directory is still there — which no later
    // graph takes for its own — and the recording in it has stopped growing.
    let recording = dir.join("t_rec.raw");
    let size = || std::fs::metadata(&recording).map_or(0, |meta| meta.len());
    let before = size();
    std::thread::sleep(Duration::from_millis(300));
    let after = size();
    if dir
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("fxsound-t-child-half-"))
    {
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert_eq!(
        left,
        Vec::<u32>::new(),
        "still running after the test process was killed"
    );
    assert_eq!(after, before, "the recorder is still writing");
}

/// A run killed outright leaves its graph's directory behind, with the socket in it — and for a
/// moment perhaps a daemon still behind the socket. A graph with the same tag never takes that
/// directory for its own. When the name was the tag and the process id, a run that happened to
/// get an earlier run's process id did: it found the old socket already there, took it for its
/// own daemon's, and its test talked to whatever answered on it. Here the earlier graph is still
/// running, which is the worst case, and the later one still gets a directory and a daemon of its
/// own.
#[test]
fn a_private_graph_never_starts_in_the_directory_of_one_before_it() {
    let Some(earlier) = PrivateGraph::start("reuse") else {
        return;
    };
    let Some(mut later) = PrivateGraph::start("reuse") else {
        return;
    };
    assert_ne!(earlier.dir.path(), later.dir.path());
    assert!(
        earlier.socket().exists(),
        "the earlier graph's socket stays"
    );
    assert!(later.socket().exists());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        later.child.try_wait().expect("the later daemon's status"),
        None,
        "the later daemon should be running, on a socket of its own"
    );
    if let Some(dump) = unless_skipped(later.dump(), "pw-dump", "the later graph's own daemon") {
        assert!(!dump.is_empty());
    }
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
        graph.settles_on(&LANE_NODE_NAMES),
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
    // Which pair the server rebuilt is `pw-dump`'s to say. Without it that is skipped, and only
    // that: what the engine says about both lanes is checked all the same.
    let before = unless_skipped(
        graph.settles_on(&LANE_NODE_NAMES),
        "pw-dump",
        "which pair a change of speakers rebuilds",
    )
    .map(|settled| settled.expect("both pairs should be in the graph at once"));
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

    if let Some(before) = before {
        let after = graph
            .nodes_until(|nodes| {
                nodes.len() == LANE_NODE_NAMES.len()
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
        graph.settles_on(&LANE_NODE_NAMES),
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
/// session's default sink is `t_71`. Both are real sinks, so every check below says which of the
/// two the engine listened to. The seed fills the memory's empty slots, so the output lane's choice
/// is no longer the first-run rule: were the session's default sink not read by then, the rules
/// would walk the remembered devices and land on `t_stereo`. The input seed names a microphone that
/// is not in this graph at all, as one unplugged since the last session would be.
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

/// Following the system's default (the engine's own start, with no ranking), the desktop picking
/// as its default the very sink the output lane already plays to hands FxSound the default back, as
/// picking any other sink does by moving the lane. The 0.4.0 live check found the default left on
/// the real device in that one case, and every application playing past FxSound.
#[test]
fn a_desktop_that_picks_the_device_a_following_lane_is_on_leaves_fxsound_the_default() {
    let Some(graph) = PrivateGraph::start("refollow") else {
        return;
    };
    let Some(seeded) = unless_skipped(
        graph.seed_default(DeviceDirection::Output, "t_stereo"),
        "pw-metadata",
        "whether a following lane takes the default back",
    ) else {
        return;
    };
    assert_eq!(seeded, Ok(()));
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(()))
    );

    // What WirePlumber derives from FxSound's claim, which nothing here does by itself; then what
    // a desktop's sound settings write when the user picks the speakers FxSound plays to.
    graph.write_default(
        devices::default_key(DeviceDirection::Output),
        SINK_NODE_NAME,
    );
    std::thread::sleep(Duration::from_millis(300));
    graph.seed_default(DeviceDirection::Output, "t_stereo");
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(())),
        "the default was left on the speakers, past FxSound"
    );
    said.settle(&handle);
    assert_eq!(
        said.attachments(DeviceDirection::Output).last(),
        Some(&Some("t_stereo".to_owned())),
        "the lane stays where it was"
    );

    // With a ranking the lane does not follow the desktop, and the default stays the user's.
    handle.send(UiToAudio::SetDevicePriority {
        direction: DeviceDirection::Output,
        names: vec!["t_stereo".to_owned(), "t_71".to_owned()],
        new_devices_first: false,
    });
    said.settle(&handle);
    graph.write_default(
        devices::default_key(DeviceDirection::Output),
        SINK_NODE_NAME,
    );
    std::thread::sleep(Duration::from_millis(300));
    graph.seed_default(DeviceDirection::Output, "t_stereo");
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some("t_stereo"),
        "a ranked lane took back a default the user had moved"
    );
    handle.shutdown();
}

/// Following the system's default, with a device once picked in FxSound — the saved device the
/// app announces to the engine as the lane's pick at every start — the desktop picking another
/// sink as the system's default moves the lane there and hands FxSound the default back, pick after
/// pick. The 0.4.0 live check found the lane staying on the saved device, the Windows rules'
/// explicit choice, and the default left on the device the desktop picked: every application
/// played past FxSound.
#[test]
fn a_saved_pick_does_not_keep_a_following_lane_from_the_sinks_the_desktop_picks() {
    let Some(graph) = PrivateGraph::start("pickfollow") else {
        return;
    };
    let Some(()) = unless_skipped(
        graph.add_device("t_third", DeviceDirection::Output),
        "pw-cli",
        "whether a following lane goes where the desktop picks",
    ) else {
        return;
    };
    let Some(seeded) = unless_skipped(
        graph.seed_default(DeviceDirection::Output, "t_stereo"),
        "pw-metadata",
        "whether a following lane goes where the desktop picks",
    ) else {
        return;
    };
    assert_eq!(seeded, Ok(()));
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    // The saved device, announced as the app announces it at every start.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
        Some(Ok(()))
    );

    for picked in ["t_stereo", "t_third"] {
        // What WirePlumber derives from FxSound's claim; then what a desktop's sound settings
        // write when the user picks a sink, and what WirePlumber derives from that.
        graph.write_default(
            devices::default_key(DeviceDirection::Output),
            SINK_NODE_NAME,
        );
        std::thread::sleep(Duration::from_millis(300));
        graph.seed_default(DeviceDirection::Output, picked);
        assert!(
            said.attached(&handle, DeviceDirection::Output, Some(picked)),
            "the lane stayed on {:?} when the desktop picked {picked}",
            said.attachments(DeviceDirection::Output).last()
        );
        assert_eq!(
            graph.default_settles_on(DeviceDirection::Output, SINK_NODE_NAME),
            Some(Ok(())),
            "the default was left on {picked}, past FxSound"
        );
    }
    handle.shutdown();
}

/// How long after the power comes back on a recorder that follows the default source has to be
/// recording FxSound again: time for WirePlumber's move, and for FxSound to find one that left the
/// recorder linked to nothing and move it again (`crate::stranded`), with room for a slow runner.
const RELINKED_WITHIN: Duration = Duration::from_secs(4);

/// Whether the recorder `recorder` is linked from FxSound's source and its recording `file` grows,
/// within [`RELINKED_WITHIN`].
fn records_fxsound(graph: &policy::PolicyGraph, recorder: u64, file: &std::path::Path) -> bool {
    let deadline = Instant::now() + RELINKED_WITHIN;
    while Instant::now() < deadline {
        let linked = graph
            .node_id(SOURCE_NODE_NAME)
            .and_then(|source| graph.linked(source, recorder))
            .unwrap_or(false);
        if linked {
            let before = policy::size_of(file);
            std::thread::sleep(Duration::from_millis(300));
            if policy::size_of(file) > before {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// When the power comes back on, FxSound takes the default source again and WirePlumber moves every
/// recorder that follows it from the microphone back onto FxSound (Input) — and now and then onto
/// nothing: moving from a mono microphone to a stereo source, the recorder's ports are replaced
/// while WirePlumber links them, the link names a port that goes, and WirePlumber gives up
/// (`crate::stranded`). The 0.4.0 live check found such a recorder recording nothing until the
/// default changed again, on 3 to 6 of 9 quick toggles with something playing to the default sink.
/// Here with WirePlumber itself, in an environment of the graph's own ([`policy`]): after each of
/// eight power toggles, every one of three such recorders is linked from FxSound's source and
/// receiving. Without FxSound's rescue of stranded streams this failed on the first toggle in
/// every run tried, with WirePlumber 0.5.17 and PipeWire 1.6.9.
#[test]
fn a_recorder_that_follows_the_default_source_records_fxsound_after_every_power_toggle() {
    let Some(mut graph) = policy::PolicyGraph::start("relink") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-metadata", "pw-cat", "pw-record"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not installed, so recorders after a power toggle were not checked"
        ));
        return;
    }
    // Something plays to the default sink meanwhile, and is moved at the same moment: the live
    // check never stranded the recorder without it. And three recorders rather than one: each is
    // moved on its own, and each move is another chance for WirePlumber to lose one.
    let _player = graph
        .pw_cat("--playback", "t_player", "application.name = t_player", &[])
        .expect("pw-cat should play");
    let recorders: Vec<(String, Guarded, PathBuf, u64)> = (1..=3)
        .map(|n| {
            let name = format!("t_recorder{n}");
            let (child, file) = graph
                .follow_default_recorder(&name)
                .expect("pw-record should record");
            let id = graph.node_id(&name).expect("the recorder is there");
            (name, child, file, id)
        })
        .collect();
    let stranded = |graph: &policy::PolicyGraph| -> Vec<&str> {
        recorders
            .iter()
            .filter(|(_, _, file, id)| !records_fxsound(graph, *id, file))
            .map(|(name, ..)| name.as_str())
            .collect()
    };

    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));
    for direction in DeviceDirection::ALL {
        assert_eq!(
            graph.default_settles_on(direction, our_node_name(direction)),
            Some(Ok(())),
            "FxSound never became the default {}",
            direction.key()
        );
    }
    assert_eq!(
        stranded(&graph),
        Vec::<&str>::new(),
        "these never recorded FxSound"
    );

    for toggle in 1..=8 {
        for want in [false, true] {
            for direction in DeviceDirection::ALL {
                handle.send(UiToAudio::SetAsDefault { direction, want });
            }
            if !want {
                // Long enough for WirePlumber to have moved everything onto the devices.
                std::thread::sleep(Duration::from_millis(1_200));
            }
        }
        let left = stranded(&graph);
        assert!(
            graph.session_manager_runs(),
            "the private WirePlumber went away"
        );
        assert!(
            left.is_empty(),
            "power toggle {toggle}: {left:?} followed the default source and were left linked \
             to nothing"
        );
    }
    handle.shutdown();
}

/// A run that was killed rather than quit leaves both configured defaults naming nodes that died
/// with it, and WirePlumber's state file keeps them that way. The next run finds its own names
/// there and adopts the claims without knowing what they displaced. The settings file's copy,
/// [`UiToAudio::SeedRememberedDefaults`], is what it knows instead: the output lane attaches to
/// the remembered sink rather than to whichever is listed first, and a clean exit points both keys
/// at real devices again. The input lane was on when the run was killed, and its microphone is
/// plugged in: the default source stays FxSound's from start to finish, rather than going to the
/// microphone before the app has attached the lane and coming back once it has.
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

    // The app switches the input lane back on from the device list — the saved microphone, the
    // first time it is listed — and a window coming up for the first time can take a while to read
    // it. Until it has, the claim on the default source is not one to hand back: the microphone
    // is right there, and handing it back would only have the input lane take it again a moment
    // later, moving whatever records from the default source twice. So the engine is left unread
    // while the output lane comes up and for a second after, and the key watched all the while.
    let Some(output_up) = unless_skipped(
        graph.nodes_until(|nodes| nodes.iter().any(|(name, _)| *name == SINK_NODE_NAME)),
        "pw-dump",
        "the default source before the app has read the device list",
    ) else {
        handle.shutdown();
        return;
    };
    assert!(output_up.is_ok(), "the output lane never came up");
    let unread_until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < unread_until {
        assert_eq!(
            graph.configured_default(DeviceDirection::Input).as_deref(),
            Some(SOURCE_NODE_NAME),
            "the default source was handed back before the app had seen the microphone listed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut said = Transcript::default();
    assert!(
        said.attached(&handle, DeviceDirection::Output, Some("t_71")),
        "the output lane should go to the seeded sink, not to the first one listed"
    );
    // What the app does once its loop reads the list, in the same breath.
    assert!(said.heard(&handle, "the microphone listed", |m| matches!(
        m,
        AudioToUi::Devices(devices) if devices.iter().any(|d| d.name == "t_mic")
    )));
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));

    // The input lane stands behind the claim now, and adopts it: the default source stays with
    // FxSound through the ticks that would have handed it back.
    let adopted_by = Instant::now() + Duration::from_secs(1);
    while Instant::now() < adopted_by {
        assert_eq!(
            graph.configured_default(DeviceDirection::Input).as_deref(),
            Some(SOURCE_NODE_NAME),
            "the default source was handed back from under the input lane that stands behind it"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    said.settle(&handle);
    assert!(
        !said.0.iter().any(|m| matches!(
            m,
            AudioToUi::RememberedDefault { node_name, .. }
                if OUR_NODE_NAMES.contains(&node_name.as_str())
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

/// The same killed run, and a start with the microphone it had been processing unplugged — so the
/// app never switches the input lane on, and nothing will build the node the default source still
/// names. The claim is handed back to that microphone once it is plugged in and the app has seen it
/// listed, while FxSound runs, rather than left naming nothing for the whole session; and not
/// before the microphone is there, when handing it
/// back could only have released it into nothing. The input lane stays detached throughout: the
/// hand-back is not an attachment. The output lane's claim is its own and is adopted as before.
#[test]
fn a_claim_left_behind_for_a_lane_that_stays_detached_is_handed_back_while_the_engine_runs() {
    let Some(graph) = PrivateGraph::start("orphan") else {
        return;
    };
    for (direction, ours) in [
        (DeviceDirection::Output, SINK_NODE_NAME),
        (DeviceDirection::Input, SOURCE_NODE_NAME),
    ] {
        let Some(()) = unless_skipped(
            graph.write_default(devices::configured_default_key(direction), ours),
            "pw-metadata",
            "the hand-back of a claim no lane stands behind",
        ) else {
            return;
        };
        assert_eq!(graph.default_settles_on(direction, ours), Some(Ok(())));
    }

    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    handle.send(UiToAudio::SeedRememberedDefaults {
        output: "t_71".to_owned(),
        input: "t_tone".to_owned(),
    });
    let mut said = Transcript::default();
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));

    // Unplugged: there is nothing to hand the default source back to yet, so it is left alone.
    said.settle(&handle);
    assert_eq!(
        graph.configured_default(DeviceDirection::Input).as_deref(),
        Some(SOURCE_NODE_NAME),
        "the default source was released with nothing to go to"
    );

    // Plugged in.
    if graph.add_tone("t_tone").is_none() {
        skip(concat!(
            "the tone never appeared (is audiotestsrc installed?), ",
            "so the hand-back was not checked"
        ));
        handle.shutdown();
        return;
    }
    // The app reads the list with the microphone on it, and — its lane saved as off — leaves the
    // lane detached. That is its chance gone by; the claim goes back.
    assert!(said.heard(&handle, "the microphone listed", |m| matches!(
        m,
        AudioToUi::Devices(devices) if devices.iter().any(|d| d.name == "t_tone")
    )));
    assert_eq!(
        graph.default_settles_on(DeviceDirection::Input, "t_tone"),
        Some(Ok(())),
        "the default source named a node no lane will ever build, with the microphone right there"
    );
    assert_eq!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some(SINK_NODE_NAME),
        "the output lane's claim is the output lane's"
    );
    said.settle(&handle);
    assert_eq!(
        said.attachments(DeviceDirection::Input),
        Vec::<Option<String>>::new(),
        "handing the claim back switched the input lane on"
    );
    assert!(
        !said.0.iter().any(|m| matches!(
            m,
            AudioToUi::RememberedDefault { node_name, .. }
                if OUR_NODE_NAMES.contains(&node_name.as_str())
        )),
        "a stale claim was remembered as the default from before FxSound"
    );

    handle.shutdown();
    assert_eq!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some("t_71")
    );
    assert_eq!(
        graph.configured_default(DeviceDirection::Input).as_deref(),
        Some("t_tone"),
        "the exit took the default source back from the microphone"
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

    // Each lane has to be heard going and coming back after the restart. The attachment from
    // before it is still the last one in the transcript until the lane's detach is read, so
    // asking for it alone would pass on what the engine said before the restart.
    let restarted_at = said.0.len();
    handle.send(UiToAudio::Restart);
    for (direction, device) in [
        (DeviceDirection::Input, "t_mic"),
        (DeviceDirection::Output, "t_stereo"),
    ] {
        assert!(
            said.heard_since(
                &handle,
                restarted_at,
                &format!("the {} lane's detach", direction.key()),
                |m| matches!(m, AudioToUi::Attached { direction: d, node_name: None }
                    if *d == direction)
            ),
            "the {} pair went with the socket",
            direction.key()
        );
        assert!(
            said.attached(&handle, direction, Some(device)),
            "…and the reconnect rebuilt the {} pair, because the lane was still enabled",
            direction.key()
        );
    }
    if let Some(settled) = unless_skipped(
        graph.settles_on(&LANE_NODE_NAMES),
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
/// Wired one lane at a time and checked in both after each step. The server drives the members of a
/// `node.link-group` together, and each lane's pair now has a group of its own — `fxsound`,
/// `fxsound-input` — so linking the microphone alone schedules the input pair and nothing else: the
/// output lane must not so much as report `processing` until something plays into the sink. (With
/// one group for all four nodes, as an earlier revision of `docs/spec/12-audio-io.md` §29.2 had it,
/// PipeWire 1.6.8 ran the output pair the moment the microphone was linked.) The meters are still
/// what shows each lane is *fed*, because being scheduled is not being fed: the tone reaches the
/// voice chain's with the music chain's still silent, and the music chain's only once something
/// plays into the sink.
#[test]
fn a_tone_driven_through_each_lane_reaches_that_lane_and_no_other() {
    let Some(graph) = PrivateGraph::start("flow") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link", "pw-record"]
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
            .settles_on(&LANE_NODE_NAMES)
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "both pairs should be in the graph at once"
    );
    for (node, direction, positions) in [
        ("t_tone", "Output", &["MONO"][..]),
        ("t_stereo", "Input", &["FL", "FR"][..]),
        (CAPTURE_NODE_NAME, "Input", &["MONO"][..]),
        (SOURCE_NODE_NAME, "Output", &["FL", "FR"][..]),
        (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
        (OUTPUT_NODE_NAME, "Output", &["FL", "FR"][..]),
    ] {
        assert!(
            graph.configure_ports(node, direction, positions).is_some(),
            "{node} was not given ports"
        );
    }

    // The microphone alone — and something recording from FxSound (Input), which is what runs
    // the input lane's passive capture stream (U19).
    assert!(graph.link_nodes("t_tone", CAPTURE_NODE_NAME));
    let recording = graph
        .clocked_recorder("t_recorder")
        .expect("a recorder for FxSound (Input)");
    assert!(graph.link_nodes(SOURCE_NODE_NAME, &recording.name));
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
    let woken = said.0.iter().find(|message| {
        matches!(message, AudioToUi::Status { direction: DeviceDirection::Output, status }
            if status.processing)
    });
    assert!(
        woken.is_none(),
        "the output lane ran because the microphone did: {woken:?}"
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

#[test]
fn a_mono_sink_that_appears_is_played_to_through_a_stereo_pair_its_adapter_down_mixes() {
    // A Bluetooth headset that has just switched to its call profile, as the output lane meets
    // it: a new output with one channel. Windows refused such a device (`sndDevices.h:32-39`), and
    // so did this port — the moment the sink's info said "one channel", the next run of the rules
    // tore the lane down and the music went to the speakers for the length of the call.
    let Some(graph) = PrivateGraph::start("mono") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so no mono sink was played to"
        ));
        return;
    }
    if graph.add_tone("t_tone").is_none() {
        skip(concat!(
            "the tone never appeared (is audiotestsrc installed?), ",
            "so no mono sink was played to"
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
    // The output lane attaches by itself, to one of the graph's stereo sinks. Let it settle there,
    // so that the rules have run over every device the graph started with.
    assert!(
        said.heard(&handle, "the output lane attached", |m| matches!(
            m,
            AudioToUi::Attached {
                direction: DeviceDirection::Output,
                node_name: Some(_)
            }
        )),
        "the output lane never attached to anything"
    );
    said.settle(&handle);

    // The mono sink arrives. Rule 5 takes it — it is new — whatever its channel count.
    let arrived = said.0.len();
    assert!(
        graph.add_mono_sink("t_mono").is_some(),
        "the mono sink never appeared"
    );
    assert!(
        said.attached(&handle, DeviceDirection::Output, Some("t_mono")),
        "the output lane should move to the sink that just appeared, mono as it is"
    );

    // Its info — one channel — has had time to arrive; now give the rules a reason to run again,
    // which is where the refusal used to strike: another output goes away.
    said.settle(&handle);
    assert!(
        graph.remove_node("t_71").is_some(),
        "the 7.1 sink could not be taken out of the graph"
    );
    assert!(
        said.heard_since(&handle, arrived, "a device list without t_71", |m| {
            matches!(m, AudioToUi::Devices(d) if !d.iter().any(|d| d.name == "t_71"))
        }),
        "the engine never noticed the 7.1 sink go"
    );
    said.settle(&handle);
    let since = &said.0[arrived..];
    let moves: Vec<Option<&str>> = since
        .iter()
        .filter_map(|message| match message {
            AudioToUi::Attached {
                direction: DeviceDirection::Output,
                node_name,
            } => Some(node_name.as_deref()),
            _ => None,
        })
        .collect();
    assert_eq!(
        moves,
        [Some("t_mono")],
        "the output lane should have moved to the mono sink once and stayed there"
    );
    let complaints: Vec<&AudioToUi> = since
        .iter()
        .filter(|message| {
            matches!(
                message,
                AudioToUi::Error {
                    direction: Some(DeviceDirection::Output) | None,
                    ..
                }
            )
        })
        .collect();
    assert!(
        complaints.is_empty(),
        "a mono sink is not an error: {complaints:?}"
    );
    let listed = said
        .0
        .iter()
        .rev()
        .find_map(|message| match message {
            AudioToUi::Devices(devices) => Some(devices),
            _ => None,
        })
        .expect("heard a moment ago");
    assert!(
        listed
            .iter()
            .any(|d| d.name == "t_mono" && d.direction == DeviceDirection::Output),
        "the mono sink should be offered as an output: {listed:?}"
    );

    // The pair it built: NODE 1 stays stereo, and NODE 2 declares the same stereo — the ring
    // between them is read at the stride it is written at — aimed at the mono sink, with
    // remixing left on for its adapter to down-mix.
    for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME] {
        assert_eq!(
            graph.node_format(node).flatten(),
            Some((48_000, 2)),
            "{node} should run a stereo pair in front of the mono sink"
        );
    }
    assert_eq!(
        graph.node_prop(OUTPUT_NODE_NAME, "target.object").flatten(),
        Some("t_mono".to_owned())
    );
    assert_eq!(
        graph
            .node_prop(OUTPUT_NODE_NAME, "stream.dont-remix")
            .flatten(),
        Some("false".to_owned()),
        "the playback stream must let its adapter remix, or the session manager would not \
         set its ports up at the sink's layout"
    );

    // Now play through it. The playback stream's ports are set up at the sink's own one-channel
    // layout, as WirePlumber sets up a stream that may be remixed, so its adapter turns the
    // stereo pair into one channel. What comes out of the sink is heard on the other side of it:
    // the input lane records the sink's monitor, on a microphone it was attached to only to have
    // a capture stream to link.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_mic")));
    assert_eq!(
        graph
            .settles_on(&[
                SINK_NODE_NAME,
                OUTPUT_NODE_NAME,
                CAPTURE_NODE_NAME,
                SOURCE_NODE_NAME
            ])
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "both pairs should be in the graph"
    );
    for (node, direction, positions) in [
        ("t_tone", "Output", &["MONO"][..]),
        (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
        (OUTPUT_NODE_NAME, "Output", &["MONO"][..]),
        (CAPTURE_NODE_NAME, "Input", &["MONO"][..]),
    ] {
        assert!(
            graph.configure_ports(node, direction, positions).is_some(),
            "{node} was not given ports"
        );
    }
    assert!(
        graph
            .configure_monitored_ports("t_mono", &["MONO"])
            .is_some(),
        "the mono sink was not given a port and a monitor"
    );
    assert_eq!(
        graph
            .ports(OUTPUT_NODE_NAME, "out")
            .map(|ports| ports.len()),
        Some(1),
        "the stereo playback stream should come out of its adapter as one channel"
    );
    assert!(graph.link_nodes(OUTPUT_NODE_NAME, "t_mono"));
    assert!(graph.link_nodes("t_tone", SINK_NODE_NAME));
    assert_eq!(
        graph.runs_until("t_mono", true).map(drop),
        Ok(()),
        "the mono sink should run while something plays into FxSound"
    );
    assert!(
        meters_until(&mut handle, DeviceDirection::Output, |m| {
            m.peak_left > 0.1 && m.peak_right > 0.1
        })
        .is_some(),
        "the tone never reached the output lane's meters"
    );
    // The input lane's meters read its signal before its chain, so what they show is what the
    // mono sink was handed.
    assert!(graph.link_nodes("t_mono", CAPTURE_NODE_NAME));
    assert!(
        meters_until(&mut handle, DeviceDirection::Input, |m| m.input_peak > 0.1).is_some(),
        "the tone went into the stereo pair and never came out of the mono sink"
    );

    said.settle(&handle);
    let output = said
        .0
        .iter()
        .rev()
        .find_map(|message| match message {
            AudioToUi::Status {
                direction: DeviceDirection::Output,
                status,
            } => Some(*status),
            _ => None,
        })
        .expect("a lane that played has reported its status");
    assert_eq!(
        (output.sample_rate, output.channels),
        (48_000, 2),
        "the output lane runs a stereo pair"
    );
    assert_eq!(
        output.format_mismatches, 0,
        "the two nodes of the pair disagreed about their format"
    );
    assert_eq!(
        said.attachments(DeviceDirection::Output).last(),
        Some(&Some("t_mono".to_owned())),
        "playing moved the output lane off the mono sink"
    );
    handle.shutdown();
}

#[test]
fn the_speakers_sleep_while_nothing_plays_into_the_sink_and_wake_the_moment_something_does() {
    let Some(graph) = PrivateGraph::start("asleep") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so the output lane's idle was not checked"
        ));
        return;
    }
    if graph.add_tone("t_tone").is_none() {
        skip(concat!(
            "the tone never appeared (is audiotestsrc installed?), ",
            "so the output lane's idle was not checked"
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
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(
        graph
            .settles_on(&[SINK_NODE_NAME, OUTPUT_NODE_NAME])
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "the output lane's pair should be in the graph"
    );
    for (node, direction, positions) in [
        ("t_tone", "Output", &["MONO"][..]),
        ("t_stereo", "Input", &["FL", "FR"][..]),
        (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
        (OUTPUT_NODE_NAME, "Output", &["FL", "FR"][..]),
    ] {
        assert!(
            graph.configure_ports(node, direction, positions).is_some(),
            "{node} was not given ports"
        );
    }

    // The playback stream is linked to the speakers as soon as the pair is up, as WirePlumber
    // links it, and nothing plays into the sink. In 0.3.0 that was enough to run both nodes and
    // the speakers for as long as FxSound was open.
    assert!(graph.link_nodes(OUTPUT_NODE_NAME, "t_stereo"));
    for node in [OUTPUT_NODE_NAME, SINK_NODE_NAME, "t_stereo"] {
        assert_eq!(
            graph.runs_until(node, false).map(drop),
            Ok(()),
            "{node} should not run while nothing plays into the sink"
        );
    }
    // Not merely for a moment: for longer than a playback stream paced by hand is given to fall
    // asleep, so that whichever way this server idles the pair, the pair has had time to wake.
    let quiet_until = Instant::now() + engine::SLEEP_AFTER * 2;
    while Instant::now() < quiet_until {
        for node in [OUTPUT_NODE_NAME, "t_stereo"] {
            assert_ne!(
                graph.node_state(node).as_deref(),
                Some("running"),
                "{node} ran with nothing to play"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // And a lane doing nothing has nothing to say: no status for the GUI to wake up for.
    said.settle(&handle);
    let idle_from = said.0.len();
    said.settle(&handle);
    let idle_statuses: Vec<_> = said.0[idle_from..]
        .iter()
        .filter(|m| {
            matches!(
                m,
                AudioToUi::Status {
                    direction: DeviceDirection::Output,
                    ..
                }
            )
        })
        .collect();
    assert!(
        idle_statuses.is_empty(),
        "an idle output lane kept reporting: {idle_statuses:?}"
    );

    // Something plays: the sink, the playback stream and the speakers all run, and the tone gets
    // through the chain.
    assert!(graph.link_nodes("t_tone", SINK_NODE_NAME));
    for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME, "t_stereo"] {
        assert_eq!(
            graph.runs_until(node, true).map(drop),
            Ok(()),
            "{node} should run while something plays into the sink"
        );
    }
    let played = meters_until(&mut handle, DeviceDirection::Output, |m| {
        m.peak_left > 0.1 && m.peak_right > 0.1
    });
    assert!(
        played.is_some(),
        "the tone never reached the output lane's meters"
    );

    // It stops — the application closed its stream — and so does everything it woke.
    assert!(graph.unlink_nodes("t_tone", SINK_NODE_NAME));
    for node in [OUTPUT_NODE_NAME, SINK_NODE_NAME, "t_stereo"] {
        assert_eq!(
            graph.runs_until(node, false).map(drop),
            Ok(()),
            "{node} should stop once nothing plays into the sink"
        );
    }
    handle.shutdown();
}

#[test]
fn a_microphone_being_captured_does_not_keep_the_speakers_awake() {
    let Some(graph) = PrivateGraph::start("micawake") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link", "pw-record"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so the two lanes' idle was not checked"
        ));
        return;
    }
    if graph.add_tone("t_tone").is_none() {
        skip(concat!(
            "the tone never appeared (is audiotestsrc installed?), ",
            "so the two lanes' idle was not checked"
        ));
        return;
    }
    let handle =
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
            .settles_on(&LANE_NODE_NAMES)
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "both pairs should be in the graph at once"
    );
    for (node, direction, positions) in [
        ("t_tone", "Output", &["MONO"][..]),
        ("t_stereo", "Input", &["FL", "FR"][..]),
        (CAPTURE_NODE_NAME, "Input", &["MONO"][..]),
        (SOURCE_NODE_NAME, "Output", &["FL", "FR"][..]),
        (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
        (OUTPUT_NODE_NAME, "Output", &["FL", "FR"][..]),
    ] {
        assert!(
            graph.configure_ports(node, direction, positions).is_some(),
            "{node} was not given ports"
        );
    }
    assert!(graph.link_nodes(OUTPUT_NODE_NAME, "t_stereo"));
    assert!(graph.link_nodes("t_tone", CAPTURE_NODE_NAME));
    let recording = graph
        .clocked_recorder("t_recorder")
        .expect("a recorder for FxSound (Input)");
    assert!(graph.link_nodes(SOURCE_NODE_NAME, &recording.name));

    // The microphone lane runs while something records from it — that is what it is for — and
    // the speakers' lane, with nothing playing into its sink, does not. With one link-group for
    // all four nodes the capture stream made the other pair runnable with it, and the speakers
    // never slept while the microphone lane was on.
    assert_eq!(
        graph.runs_until(CAPTURE_NODE_NAME, true).map(drop),
        Ok(()),
        "the capture stream should run while its microphone does"
    );
    let quiet_until = Instant::now() + engine::SLEEP_AFTER * 2;
    while Instant::now() < quiet_until {
        for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME, "t_stereo"] {
            assert_ne!(
                graph.node_state(node).as_deref(),
                Some("running"),
                "{node} ran because the microphone did"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    handle.shutdown();
}

/// Why the echo canceller cannot be tested here, if it cannot: `None` when PipeWire's module and
/// the null canceller — which passes the microphone through, so a test needs no WebRTC — are both
/// installed.
pub(crate) fn canceller_missing() -> Option<&'static str> {
    if !library_installed("pipewire-0.3/libpipewire-module-echo-cancel.so") {
        return Some("libpipewire-module-echo-cancel is not installed");
    }
    if !library_installed("spa-0.2/aec/libspa-aec-null.so") {
        return Some("libspa-aec-null is not installed");
    }
    None
}

/// Start an engine whose echo canceller runs `library`, and attach its lanes to the private
/// graph's stereo speakers and to `microphone`.
fn engine_with_both_lanes(
    graph: &PrivateGraph,
    library: &'static str,
    microphone: &str,
    said: &mut Transcript,
) -> EngineHandle {
    let handle = AudioEngine::start_with_canceller(Some(&graph.remote()), library)
        .expect("the engine should start");
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == microphone)),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    handle.send(UiToAudio::SelectDevice {
        node_name: microphone.to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert!(said.attached(&handle, DeviceDirection::Input, Some(microphone)));
    handle
}

/// Whether the engine has reported echo cancellation as `running`, with a detail `detail` accepts,
/// from message `from` of the transcript on.
fn heard_echo_cancel(
    said: &mut Transcript,
    handle: &EngineHandle,
    from: usize,
    running: bool,
    detail: impl Fn(&str) -> bool,
) -> bool {
    said.heard_since(
        handle,
        from,
        &format!("echo cancellation reported as running: {running}"),
        |message| {
            matches!(message, AudioToUi::EchoCancel { running: r, detail: d }
                if *r == running && detail(d))
        },
    )
}

#[test]
fn echo_cancellation_puts_the_canceller_in_front_of_the_microphone_and_takes_it_away_again() {
    if let Some(missing) = canceller_missing() {
        skip(&format!("{missing}, so echo cancellation was not checked"));
        return;
    }
    let Some(graph) = PrivateGraph::start("aec") else {
        return;
    };
    if !installed("pw-dump") {
        skip("pw-dump is not available, so echo cancellation was not checked");
        return;
    }
    let mut said = Transcript::default();
    let handle = engine_with_both_lanes(&graph, aec::NULL_LIBRARY, "t_mic", &mut said);
    let before = graph
        .settles_on(&LANE_NODE_NAMES)
        .expect("pw-dump is installed")
        .expect("both pairs should be in the graph before echo cancellation is asked for");
    assert_eq!(
        graph
            .node_prop(CAPTURE_NODE_NAME, "target.object")
            .flatten()
            .as_deref(),
        Some("t_mic"),
        "without echo cancellation the capture stream records the microphone itself"
    );
    let heard_before = said.0.len();

    let mark = said.0.len();
    handle.send(UiToAudio::SetEchoCancel(true));
    assert!(
        heard_echo_cancel(&mut said, &handle, mark, true, str::is_empty),
        "echo cancellation should be reported running once the canceller's source is in the graph"
    );

    // All seven nodes: both pairs and the canceller's three, with the input lane's capture stream
    // rebuilt to record from the canceller's source, and the speakers' pair left alone.
    let during = graph
        .nodes_until(|nodes| {
            nodes.len() == LANE_NODE_NAMES.len() + AEC_NODE_NAMES.len()
                && serial_of(nodes, CAPTURE_NODE_NAME) != serial_of(&before, CAPTURE_NODE_NAME)
        })
        .expect("pw-dump answered a moment ago")
        .expect("the canceller's nodes should join both pairs, and the capture stream be rebuilt");
    assert_eq!(
        graph.prop_settles_on(CAPTURE_NODE_NAME, "target.object", AEC_SOURCE_NODE_NAME),
        Some(Ok(())),
        "while echo cancellation runs, the capture stream records from the canceller's source"
    );
    for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME] {
        assert_eq!(
            serial_of(&during, node),
            serial_of(&before, node),
            "{node} was rebuilt for the microphone's echo cancellation"
        );
    }
    // What the canceller listens to, and the group of its own it listens in.
    for (node, key, want) in [
        (AEC_CAPTURE_NODE_NAME, "target.object", "t_mic"),
        (AEC_MONITOR_NODE_NAME, "target.object", "t_stereo"),
        (AEC_MONITOR_NODE_NAME, "stream.capture.sink", "true"),
        (AEC_CAPTURE_NODE_NAME, "node.link-group", AEC_LINK_GROUP),
        (AEC_MONITOR_NODE_NAME, "node.link-group", AEC_LINK_GROUP),
        (AEC_SOURCE_NODE_NAME, "node.link-group", AEC_LINK_GROUP),
        (
            AEC_SOURCE_NODE_NAME,
            "node.description",
            "FxSound echo-cancelled",
        ),
        (CAPTURE_NODE_NAME, "node.link-group", INPUT_LINK_GROUP),
    ] {
        assert_eq!(
            graph.node_prop(node, key).flatten().as_deref(),
            Some(want),
            "{node}'s {key}"
        );
    }

    said.settle(&handle);
    let since = &said.0[heard_before..];
    assert!(
        !since
            .iter()
            .any(|m| matches!(m, AudioToUi::Attached { .. } | AudioToUi::Error { .. })),
        "the canceller is a stage in front of the microphone, not another device: {since:?}"
    );
    assert!(
        said.0.iter().all(|m| match m {
            AudioToUi::Devices(devices) => devices.iter().all(|d| !d.name.starts_with("fxsound_")),
            _ => true,
        }),
        "the canceller's nodes were offered as devices"
    );

    // Off: the capture stream goes back to the microphone, and the canceller's nodes go.
    let mark = said.0.len();
    handle.send(UiToAudio::SetEchoCancel(false));
    assert!(
        heard_echo_cancel(&mut said, &handle, mark, false, str::is_empty),
        "switching echo cancellation off is answered"
    );
    let after = graph
        .settles_on(&LANE_NODE_NAMES)
        .expect("pw-dump answered a moment ago")
        .expect("the canceller's three nodes should be gone, and both pairs still there");
    assert_eq!(
        graph
            .node_prop(CAPTURE_NODE_NAME, "target.object")
            .flatten()
            .as_deref(),
        Some("t_mic"),
        "with echo cancellation off, the capture stream records the microphone again"
    );
    assert_ne!(
        serial_of(&after, CAPTURE_NODE_NAME),
        serial_of(&during, CAPTURE_NODE_NAME),
        "the capture stream was not rebuilt onto the microphone"
    );
    for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME] {
        assert_eq!(serial_of(&after, node), serial_of(&before, node), "{node}");
    }
    handle.shutdown();
}

#[test]
fn a_canceller_that_cannot_be_loaded_is_reported_and_the_microphone_is_recorded_directly() {
    let Some(graph) = PrivateGraph::start("aecfail") else {
        return;
    };
    if !installed("pw-dump") {
        skip("pw-dump is not available, so a failed canceller was not checked");
        return;
    }
    // A library no PipeWire ships: the load fails the way a missing `libspa-aec-webrtc` does,
    // with ENOENT, whether or not the module itself is installed.
    const MISSING: &str = "aec/libspa-aec-fxsound-no-such-canceller";
    let mut said = Transcript::default();
    let handle = engine_with_both_lanes(&graph, MISSING, "t_mic", &mut said);
    assert_eq!(
        graph
            .settles_on(&LANE_NODE_NAMES)
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "both pairs should be in the graph"
    );

    let mark = said.0.len();
    handle.send(UiToAudio::SetEchoCancel(true));
    assert!(
        heard_echo_cancel(&mut said, &handle, mark, false, |detail| detail
            .contains(MISSING)),
        "a canceller that cannot load is reported as not running, and why: {:?}",
        said.0
    );

    // And nothing else changes: no canceller nodes, the capture stream still on the microphone,
    // the lane still attached, and no pretending later on.
    said.settle(&handle);
    assert_eq!(
        graph
            .node_prop(CAPTURE_NODE_NAME, "target.object")
            .flatten()
            .as_deref(),
        Some("t_mic"),
        "the input lane keeps recording the microphone itself"
    );
    assert_eq!(
        graph.our_nodes().map(|nodes| nodes.len()),
        Some(LANE_NODE_NAMES.len()),
        "a module that failed to load left nodes behind"
    );
    assert!(
        !said
            .0
            .iter()
            .any(|m| matches!(m, AudioToUi::EchoCancel { running: true, .. })),
        "a canceller that never loaded was reported running"
    );
    assert_eq!(
        said.attachments(DeviceDirection::Input).last(),
        Some(&Some("t_mic".to_owned()))
    );
    let failures = said
        .0
        .iter()
        .filter(
            |m| matches!(m, AudioToUi::EchoCancel { running: false, detail } if !detail.is_empty()),
        )
        .count();
    assert_eq!(
        failures, 1,
        "the failure is reported once, not on every tick"
    );
    handle.shutdown();
}

/// Start an engine with both lanes on `graph` — the output lane on the stereo speakers, the input
/// lane on a tone called `t_tone` — switch echo cancellation on, and wire the result as a session
/// manager would: the playback stream to the speakers, the microphone and the speakers' monitor
/// into the canceller, and the canceller into the capture stream. Nothing records from FxSound
/// (Input) yet. `None`, having said why, when the canceller, a tool or the tone is missing; `what`
/// names the check that then goes undone.
fn engine_with_the_canceller_wired(
    graph: &PrivateGraph,
    what: &str,
) -> Option<(EngineHandle, Transcript)> {
    if let Some(missing) = canceller_missing() {
        skip(&format!("{missing}, so {what} was not checked"));
        return None;
    }
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link", "pw-record"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so {what} was not checked"
        ));
        return None;
    }
    if graph.add_tone("t_tone").is_none() {
        skip(&format!(
            "the tone never appeared (is audiotestsrc installed?), so {what} was not checked"
        ));
        return None;
    }
    let mut said = Transcript::default();
    let handle = engine_with_both_lanes(graph, aec::NULL_LIBRARY, "t_tone", &mut said);
    let mark = said.0.len();
    handle.send(UiToAudio::SetEchoCancel(true));
    assert!(heard_echo_cancel(
        &mut said,
        &handle,
        mark,
        true,
        str::is_empty
    ));
    assert_eq!(
        graph.prop_settles_on(CAPTURE_NODE_NAME, "target.object", AEC_SOURCE_NODE_NAME),
        Some(Ok(())),
        "the capture stream should have moved onto the canceller"
    );
    for (node, direction, positions) in [
        ("t_tone", "Output", &["MONO"][..]),
        (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
        (OUTPUT_NODE_NAME, "Output", &["FL", "FR"][..]),
        (AEC_CAPTURE_NODE_NAME, "Input", &["FL", "FR"][..]),
        (AEC_MONITOR_NODE_NAME, "Input", &["FL", "FR"][..]),
        (AEC_SOURCE_NODE_NAME, "Output", &["FL", "FR"][..]),
        (CAPTURE_NODE_NAME, "Input", &["FL", "FR"][..]),
        (SOURCE_NODE_NAME, "Output", &["FL", "FR"][..]),
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
    assert!(graph.link_nodes(OUTPUT_NODE_NAME, "t_stereo"));
    assert!(graph.link_nodes("t_tone", AEC_CAPTURE_NODE_NAME));
    assert!(graph.link_nodes("t_stereo", AEC_MONITOR_NODE_NAME));
    assert!(graph.link_nodes(AEC_SOURCE_NODE_NAME, CAPTURE_NODE_NAME));
    Some((handle, said))
}

/// Everything echo cancellation can keep running: the microphone, the canceller's capture stream
/// and source (its monitor stream is in the same `node.group`, and has no state of its own to
/// watch), the input lane's pair, the speakers the canceller listens to, and the output pair.
const AWAKE_WITH_THE_CANCELLER: [&str; 8] = [
    "t_tone",
    AEC_CAPTURE_NODE_NAME,
    AEC_SOURCE_NODE_NAME,
    CAPTURE_NODE_NAME,
    SOURCE_NODE_NAME,
    "t_stereo",
    OUTPUT_NODE_NAME,
    SINK_NODE_NAME,
];

#[test]
fn the_speakers_sleep_again_once_echo_cancellation_is_switched_off() {
    let Some(graph) = PrivateGraph::start("aecidle") else {
        return;
    };
    let Some((handle, mut said)) =
        engine_with_the_canceller_wired(&graph, "the idle after echo cancellation")
    else {
        return;
    };
    // Something records from FxSound (Input): the passive capture stream runs only then (U19).
    let recording = graph
        .clocked_recorder("t_recorder")
        .expect("a recorder for FxSound (Input)");
    assert!(graph.link_nodes(SOURCE_NODE_NAME, &recording.name));
    assert_eq!(
        graph.runs_until(CAPTURE_NODE_NAME, true).map(drop),
        Ok(()),
        "the capture stream should run on the canceller's source"
    );
    // What echo cancellation costs (`docs/0.4.0-design.md` §7): the canceller hears the speakers'
    // monitor on the microphone's clock, so the speakers — and the output pair, linked to them —
    // run for as long as it does, whatever plays.
    assert_eq!(
        graph.runs_until("t_stereo", true).map(drop),
        Ok(()),
        "the speakers run while the canceller listens to them"
    );

    // Off. The canceller goes, the capture stream is rebuilt on the microphone, and once it is
    // linked there — the session manager's job, done here by hand — the microphone lane runs on
    // without keeping the speakers awake.
    let mark = said.0.len();
    handle.send(UiToAudio::SetEchoCancel(false));
    assert!(heard_echo_cancel(
        &mut said,
        &handle,
        mark,
        false,
        str::is_empty
    ));
    assert_eq!(
        graph
            .settles_on(&LANE_NODE_NAMES)
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "the canceller's nodes should be gone"
    );
    assert_eq!(
        graph.prop_settles_on(CAPTURE_NODE_NAME, "target.object", "t_tone"),
        Some(Ok(())),
        "the capture stream should be back on the microphone"
    );
    assert!(
        graph
            .configure_ports(CAPTURE_NODE_NAME, "Input", &["MONO"])
            .is_some(),
        "the rebuilt capture stream was not given ports"
    );
    assert!(
        graph
            .configure_ports(SOURCE_NODE_NAME, "Output", &["FL", "FR"])
            .is_some(),
        "the rebuilt source was not given ports"
    );
    assert!(graph.link_nodes("t_tone", CAPTURE_NODE_NAME));
    // The pair was rebuilt whole, so the recorder is linked to the new source as a session manager
    // would link it.
    assert!(graph.link_nodes(SOURCE_NODE_NAME, &recording.name));
    assert_eq!(
        graph.runs_until(CAPTURE_NODE_NAME, true).map(drop),
        Ok(()),
        "the microphone lane keeps running"
    );
    for node in ["t_stereo", OUTPUT_NODE_NAME, SINK_NODE_NAME] {
        assert_eq!(
            graph.runs_until(node, false).map(drop),
            Ok(()),
            "{node} should stop once the canceller is gone"
        );
    }
    let quiet_until = Instant::now() + engine::SLEEP_AFTER * 2;
    while Instant::now() < quiet_until {
        for node in [SINK_NODE_NAME, OUTPUT_NODE_NAME, "t_stereo"] {
            assert_ne!(
                graph.node_state(node).as_deref(),
                Some("running"),
                "{node} ran with echo cancellation off and nothing playing"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    handle.shutdown();
}

/// Echo cancellation used to cost the idle outright (`docs/0.4.0-design.md` §7): the capture stream
/// was an ordinary stream, so its link to the canceller's source ran the canceller, and through the
/// canceller's `node.group` its monitor stream, the speakers it records and the output pair — for
/// as long as the microphone lane was on. Now the capture stream is passive (U19), and so are the
/// canceller's capture and monitor streams (PipeWire's module makes them so), so nothing in that
/// chain holds any of it: it all runs while something records from FxSound (Input), and sleeps
/// with the canceller still loaded when the recording stops.
#[test]
fn echo_cancellation_holds_nothing_awake_while_nothing_records_from_fxsound_input() {
    let Some(graph) = PrivateGraph::start("aecsleep") else {
        return;
    };
    let Some((handle, _said)) =
        engine_with_the_canceller_wired(&graph, "the idle under echo cancellation")
    else {
        return;
    };
    assert_eq!(
        graph.node_prop(CAPTURE_NODE_NAME, "node.passive"),
        Some(Some("true".to_owned())),
        "the capture stream should be passive on a server that runs a link-group together"
    );
    // An application that will record from FxSound (Input), running already on its own clock.
    let recorder = graph
        .clocked_recorder("t_recorder")
        .expect("a recorder for FxSound (Input)");

    // The canceller wired in front of the microphone and listening to the speakers, and nobody
    // recording: nothing runs, the speakers included.
    assert_eq!(
        sleep::none_runs_for_a_while(&graph, &AWAKE_WITH_THE_CANCELLER),
        Ok(()),
        "echo cancellation held the graph awake with nothing recording from FxSound (Input)"
    );

    // A recording runs all of it, as a call would.
    let before = graph.our_nodes().expect("pw-dump answered a moment ago");
    let from = recorder.written();
    assert!(graph.link_nodes(SOURCE_NODE_NAME, &recorder.name));
    for node in AWAKE_WITH_THE_CANCELLER {
        assert_eq!(
            graph.runs_until(node, true).map(drop),
            Ok(()),
            "{node} should run while something records from FxSound (Input) through the canceller"
        );
    }
    assert!(
        recorder.hears_since(from),
        "the recorder was not handed the microphone through the canceller"
    );

    // It stops, and all of it sleeps again with echo cancellation still on.
    assert!(graph.unlink_nodes(SOURCE_NODE_NAME, &recorder.name));
    for node in AWAKE_WITH_THE_CANCELLER {
        assert_eq!(
            graph.runs_until(node, false).map(drop),
            Ok(()),
            "{node} should stop once nothing records from FxSound (Input)"
        );
    }
    assert_eq!(
        sleep::none_runs_for_a_while(&graph, &AWAKE_WITH_THE_CANCELLER),
        Ok(()),
        "echo cancellation woke the graph again with nothing recording"
    );
    assert_eq!(
        graph.our_nodes(),
        Some(before),
        "sleeping and waking is the server's business: neither the canceller nor a pair was rebuilt"
    );
    handle.shutdown();
}

/// Wait until the canceller listens to `microphone` and `speakers`, and the capture stream records
/// from its source in a pair built since `stale` — so not a reading left over from before a reload,
/// which moves the capture stream onto the microphone and back. `None` when `pw-dump` is not there;
/// `Some(false)` when it never came to.
fn canceller_settles_on(
    graph: &PrivateGraph,
    microphone: &str,
    speakers: &str,
    stale: Option<u64>,
) -> Option<bool> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let settled = graph
            .node_prop(AEC_CAPTURE_NODE_NAME, "target.object")?
            .as_deref()
            == Some(microphone)
            && graph
                .node_prop(AEC_MONITOR_NODE_NAME, "target.object")?
                .as_deref()
                == Some(speakers)
            && graph
                .node_prop(CAPTURE_NODE_NAME, "target.object")?
                .as_deref()
                == Some(AEC_SOURCE_NODE_NAME)
            && graph
                .our_nodes()
                .is_some_and(|nodes| serial_of(&nodes, CAPTURE_NODE_NAME) != stale);
        if settled {
            return Some(true);
        }
        if Instant::now() >= deadline {
            return Some(false);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_canceller_follows_the_microphone_and_the_speakers_and_goes_with_the_microphone_lane() {
    if let Some(missing) = canceller_missing() {
        skip(&format!(
            "{missing}, so the canceller's reloads were not checked"
        ));
        return;
    }
    let Some(graph) = PrivateGraph::start("aecfollow") else {
        return;
    };
    if !installed("pw-dump") {
        skip("pw-dump is not available, so the canceller's reloads were not checked");
        return;
    }
    if graph.add_tone("t_tone").is_none() {
        skip(concat!(
            "the tone never appeared (is audiotestsrc installed?), ",
            "so the canceller's reloads were not checked"
        ));
        return;
    }
    let mut said = Transcript::default();
    let handle = engine_with_both_lanes(&graph, aec::NULL_LIBRARY, "t_mic", &mut said);
    let mark = said.0.len();
    handle.send(UiToAudio::SetEchoCancel(true));
    assert!(heard_echo_cancel(
        &mut said,
        &handle,
        mark,
        true,
        str::is_empty
    ));
    assert_eq!(
        canceller_settles_on(&graph, "t_mic", "t_stereo", None),
        Some(true),
        "the canceller should hear the microphone and the speakers the lanes are attached to"
    );

    // Other speakers: the canceller is reloaded to hear those, and the capture stream ends up on
    // the new canceller's source.
    let capture = |graph: &PrivateGraph| {
        graph
            .our_nodes()
            .and_then(|nodes| serial_of(&nodes, CAPTURE_NODE_NAME))
    };
    let before = capture(&graph);
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    // Asked for again, echo cancellation is answered as the message is handled, and messages are
    // handled in order: once the answer is in, the change of speakers has been handled in full.
    let mark = said.0.len();
    handle.send(UiToAudio::SetEchoCancel(true));
    assert!(
        said.heard_since(&handle, mark, "echo cancellation's answer", |m| {
            matches!(m, AudioToUi::EchoCancel { .. })
        })
    );
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));
    assert_ne!(
        capture(&graph),
        before,
        "the capture stream should leave the unloaded canceller's source with it, not a tick later"
    );
    assert_eq!(
        canceller_settles_on(&graph, "t_mic", "t_71", before),
        Some(true),
        "a change of speakers should reload the canceller to hear the new ones"
    );

    // Another microphone: the same, for the capture side.
    let before = capture(&graph);
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_tone".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some("t_tone")));
    assert_eq!(
        canceller_settles_on(&graph, "t_tone", "t_71", before),
        Some(true),
        "a change of microphone should reload the canceller to hear the new one"
    );

    // The microphone lane detached: the canceller goes with it, the speakers' pair stays.
    let heard_before = said.0.len();
    handle.send(UiToAudio::DetachLane(DeviceDirection::Input));
    assert!(said.attached(&handle, DeviceDirection::Input, None));
    assert_eq!(
        graph
            .settles_on(&[SINK_NODE_NAME, OUTPUT_NODE_NAME])
            .map(|settled| settled.map(drop)),
        Some(Ok(())),
        "detaching the microphone lane should unload the canceller with the lane's pair"
    );
    assert!(
        said.heard_since(
            &handle,
            heard_before,
            "echo cancellation stopping",
            |m| matches!(
                m,
                AudioToUi::EchoCancel { running: false, detail } if detail.is_empty()
            )
        ),
        "the canceller stopping with the lane is reported"
    );

    // Still on: attaching a microphone again brings it back.
    let mark = said.0.len();
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_mic".to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(heard_echo_cancel(
        &mut said,
        &handle,
        mark,
        true,
        str::is_empty
    ));
    assert_eq!(
        canceller_settles_on(&graph, "t_mic", "t_71", None),
        Some(true),
        "echo cancellation stays on across a detach, and comes back with the microphone lane"
    );
    handle.shutdown();
}

#[test]
fn a_server_that_goes_away_under_a_running_canceller_is_survived() {
    if let Some(missing) = canceller_missing() {
        skip(&format!(
            "{missing}, so losing the server under the canceller was not checked"
        ));
        return;
    }
    let Some(mut graph) = PrivateGraph::start("aecdisc") else {
        return;
    };
    let mut said = Transcript::default();
    let handle = engine_with_both_lanes(&graph, aec::NULL_LIBRARY, "t_mic", &mut said);
    let mark = said.0.len();
    handle.send(UiToAudio::SetEchoCancel(true));
    assert!(heard_echo_cancel(
        &mut said,
        &handle,
        mark,
        true,
        str::is_empty
    ));

    // The module's connection and the engine's break together. Whichever of them notices first —
    // the module destroying itself, or the engine unloading it as the session closes — the module
    // is destroyed once, and the engine says so.
    let mark = said.0.len();
    graph.kill();
    assert!(
        said.until(&handle, "a disconnection", |m| matches!(
            m,
            AudioToUi::Disconnected { .. }
        )),
        "losing the server should be reported"
    );
    assert!(
        heard_echo_cancel(&mut said, &handle, mark, false, |_| true),
        "a canceller whose server went away is no longer running"
    );
    said.settle(&handle);
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
    graph.kill();

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

/// The headset of the tests below: its card's Bluetooth address, and its sink's name.
const HEADSET_ADDRESS: &str = "00:11:22:33:44:55";
const HEADSET: &str = "t_headset";

/// A graph with a headset in it — a card, and a sink that `belongs` says how it belongs to —
/// beside the graph's own speakers, and an engine whose output lane the user put on the headset.
/// `None`, having said why, when that cannot be had here.
fn engine_on_a_headset(
    tag: &str,
    belongs: &dyn Fn(&CardHolder) -> String,
) -> Option<(PrivateGraph, CardHolder, EngineHandle, Transcript)> {
    let graph = PrivateGraph::start_with_cards(tag)?;
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!("{missing} is not available, so {tag} cannot run"));
        return None;
    }
    let Some(card) = graph.add_card("t_headset_card", HEADSET_ADDRESS) else {
        skip(&format!(
            "the headset's card never appeared, so {tag} cannot run"
        ));
        return None;
    };
    assert!(
        graph.add_card_sink(HEADSET, &belongs(&card)).is_some(),
        "the headset's sink never appeared"
    );

    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list with the headset",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == HEADSET)),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: HEADSET.to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(
        said.attached(&handle, DeviceDirection::Output, Some(HEADSET)),
        "the output lane never attached to the headset"
    );
    assert_eq!(
        graph.node_prop(OUTPUT_NODE_NAME, "target.object").flatten(),
        Some(HEADSET.to_owned())
    );
    // The node's info and the card's — where the addresses are — have had time to arrive.
    said.settle(&handle);
    Some((graph, card, handle, said))
}

/// Where the output lane said it went, from message `from` on.
fn output_moves_since(said: &Transcript, from: usize) -> Vec<Option<String>> {
    said.0[from..]
        .iter()
        .filter_map(|message| match message {
            AudioToUi::Attached {
                direction: DeviceDirection::Output,
                node_name,
            } => Some(node_name.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_headset_between_profiles_keeps_the_output_lane_and_its_sink_is_linked_again() {
    // WirePlumber's own recipe for a Bluetooth sink: `device.id` names the headset's card.
    let belongs = |card: &CardHolder| format!("device.id = {}", card.id);
    let Some((graph, card, handle, mut said)) = engine_on_a_headset("blink", &belongs) else {
        return;
    };

    // What WirePlumber does with the playback stream the engine built: links it to the headset.
    let mut handled = Vec::new();
    assert_eq!(
        graph.link_like_wireplumber(&mut handled),
        Some(true),
        "the playback stream could not be linked to the headset"
    );
    assert_eq!(graph.linked(OUTPUT_NODE_NAME, HEADSET), Some(true));
    let first = graph
        .our_nodes()
        .and_then(|nodes| serial_of(&nodes, OUTPUT_NODE_NAME))
        .expect("the playback stream is in the graph");
    let from = said.0.len();

    // The headset switches profile: its sink goes, and comes back under the same name a moment
    // later, on the same card. The gap is two supervisor ticks and more, each of which found the
    // headset gone and moved the lane to the speakers before the lane waited for it.
    assert!(
        graph.remove_node(HEADSET).is_some(),
        "the headset's sink could not be taken out of the graph"
    );
    let gone = Instant::now();
    std::thread::sleep(2 * crate::engine::SUPERVISOR_PERIOD);
    // The contract's blink: the sink re-added within 500 ms of going (`docs/0.4.0-upstream.md`,
    // U8). This is the moment the test asks the server for it, which only a sleep stands before.
    let readded = gone.elapsed();
    assert!(
        readded < Duration::from_millis(500),
        "the sink was re-added {readded:?} after it went, not within the 500 ms this test is for"
    );
    assert!(
        graph.add_card_sink(HEADSET, &belongs(&card)).is_some(),
        "the headset's sink never came back on its card"
    );
    let gap = gone.elapsed();
    println!("the headset's sink was away for {gap:?}, re-added after {readded:?}");
    // The gap as seen from here is held to the lane's wait and not to 500 ms, on purpose. On top of
    // the re-add it counts `pw-cli` starting and `pw-dump` polling until the sink is listed, which
    // a loaded runner slows by more than the 100 ms left — and a longer blink only makes the lane
    // wait longer, which is harder to pass, never easier: every way this test fails on a broken
    // engine (a move to the speakers, a pair kept on the old serial) shows at any gap. What a slow
    // runner can make is a gap the lane is right to give up on, which would fail below for a reason
    // that is no bug; this says so instead.
    assert!(
        gap >= 2 * crate::engine::SUPERVISOR_PERIOD,
        "the sink was away for {gap:?}, too short for a supervisor tick to have seen it gone"
    );
    assert!(
        gap < crate::engine::RETURN_WAIT,
        "the sink was away for {gap:?}, longer than the lane waits for it: the runner is too \
         slow for this test to say anything"
    );

    // The pair is rebuilt on the node that came back, because the stream WirePlumber linked
    // once is never linked again — and the new stream is linked.
    let rebuilt = graph.nodes_until(|nodes| {
        serial_of(nodes, OUTPUT_NODE_NAME).is_some_and(|serial| serial != first)
    });
    assert!(
        matches!(rebuilt, Some(Ok(_))),
        "the playback stream was never rebuilt for the sink that came back: {rebuilt:?}"
    );
    said.settle(&handle);
    assert_eq!(
        graph.link_like_wireplumber(&mut handled),
        Some(true),
        "the rebuilt playback stream could not be linked"
    );
    assert_eq!(
        graph.linked(OUTPUT_NODE_NAME, HEADSET),
        Some(true),
        "NODE 2 should be linked to the headset again"
    );
    assert_eq!(
        graph.node_prop(OUTPUT_NODE_NAME, "target.object").flatten(),
        Some(HEADSET.to_owned())
    );

    said.settle(&handle);
    let moves = output_moves_since(&said, from);
    assert!(
        moves.iter().all(|to| to.as_deref() == Some(HEADSET)),
        "the output lane left the headset while it switched profile: {moves:?}"
    );
    handle.shutdown();
}

#[test]
fn a_headset_back_from_a_profile_switch_takes_nothing_from_the_speakers_the_user_picked() {
    // The headset is ranked above the speakers, but the user picked the speakers: the lane is on
    // them, not on the headset, and holds no wait for it.
    let belongs = |card: &CardHolder| format!("device.id = {}", card.id);
    let Some((graph, card, handle, mut said)) = engine_on_a_headset("notnew", &belongs) else {
        return;
    };
    handle.send(rank(DeviceDirection::Output, &[HEADSET, "t_stereo"]));
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    said.settle(&handle);
    let from = said.0.len();

    // Something records from its microphone: the headset switches profile, its sink goes while
    // its card stays, and comes back under the same name after the rules have run without it.
    assert!(graph.remove_node(HEADSET).is_some(), "the sink never went");
    std::thread::sleep(3 * crate::engine::SUPERVISOR_PERIOD);
    assert!(
        graph.add_card_sink(HEADSET, &belongs(&card)).is_some(),
        "the headset's sink never came back on its card"
    );
    said.settle(&handle);
    said.settle(&handle);
    assert_eq!(
        output_moves_since(&said, from),
        [],
        "a sink back from a profile switch is no device just plugged in, ranked first or not"
    );
    drop(card);
    handle.shutdown();
}

#[test]
fn a_sink_its_card_brings_back_under_another_name_ends_the_wait_at_once() {
    let belongs = |card: &CardHolder| format!("device.id = {}", card.id);
    let Some((graph, card, handle, mut said)) = engine_on_a_headset("rename", &belongs) else {
        return;
    };
    const RENAMED: &str = "t_headset_renamed";
    // A card switched to another profile whose sink has another name: the old one never returns.
    assert!(graph.remove_node(HEADSET).is_some(), "the sink never went");
    assert!(
        graph.add_card_sink(RENAMED, &belongs(&card)).is_some(),
        "the renamed sink never appeared on the card"
    );
    let renamed_at = Instant::now();
    assert!(
        said.attached(&handle, DeviceDirection::Output, Some(RENAMED)),
        "the output lane never went to the renamed sink"
    );
    let waited = renamed_at.elapsed();
    assert!(
        waited < crate::engine::RETURN_WAIT,
        "the lane sat on an unlinked stream for {waited:?}, waiting for a name that never returns"
    );
    drop(card);
    handle.shutdown();
}

#[test]
fn a_ranking_handed_over_at_start_makes_the_first_choice_of_output() {
    let Some(graph) = PrivateGraph::start("rankstart") else {
        return;
    };
    if !installed("pw-cli") {
        skip("pw-cli is not available, so rankstart cannot run");
        return;
    }
    assert!(
        graph.add_device("t_low", DeviceDirection::Output).is_some(),
        "t_low never appeared"
    );
    let handle = AudioEngine::start_for_tests(
        Some(&graph.remote()),
        StartOptions {
            output_priority: DevicePriority {
                names: vec!["t_low".to_owned(), "t_stereo".to_owned()],
                new_devices_first: false,
            },
            ..StartOptions::default()
        },
        None,
    )
    .expect("the engine should start");
    let mut said = Transcript::default();
    assert!(
        said.attached(&handle, DeviceDirection::Output, Some("t_low")),
        "the first output is the ranking's first, not the graph's"
    );
    said.settle(&handle);
    assert_eq!(
        said.attachments(DeviceDirection::Output),
        [Some("t_low".to_owned())],
        "and it was the first choice, not a move after one made without the ranking"
    );
    handle.shutdown();
}

#[test]
fn a_power_left_off_at_start_builds_the_speakers_pair_without_claiming_the_default() {
    let Some(graph) = PrivateGraph::start("poweroff") else {
        return;
    };
    let handle = AudioEngine::start_for_tests(
        Some(&graph.remote()),
        StartOptions {
            want_default: false,
            ..StartOptions::default()
        },
        None,
    )
    .expect("the engine should start");
    let mut said = Transcript::default();
    assert!(
        said.until(&handle, "the output lane attached", |m| matches!(
            m,
            AudioToUi::Attached {
                direction: DeviceDirection::Output,
                node_name: Some(_)
            }
        ))
    );
    said.settle(&handle);
    if graph.tool("pw-metadata", &["-n", "default"]).is_none() {
        skip("pw-metadata is not available, so poweroff cannot read the default");
        handle.shutdown();
        return;
    }
    assert_ne!(
        graph.configured_default(DeviceDirection::Output).as_deref(),
        Some(SINK_NODE_NAME),
        "the first pair took the default sink the app was about to hand back"
    );
    handle.shutdown();
}

#[test]
fn a_speakers_lane_left_off_at_start_builds_no_pair_before_the_app_says_so() {
    let Some(graph) = PrivateGraph::start("outputoff") else {
        return;
    };
    let handle = AudioEngine::start_for_tests(
        Some(&graph.remote()),
        StartOptions {
            output_enabled: false,
            ..StartOptions::default()
        },
        None,
    )
    .expect("the engine should start");
    let mut said = Transcript::default();
    assert!(said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if !d.is_empty())
    ));
    said.settle(&handle);
    assert_eq!(
        said.attachments(DeviceDirection::Output),
        Vec::<Option<String>>::new(),
        "the speakers' lane was attached before the app could switch it off"
    );
    if let Some(settled) = unless_skipped(graph.settles_on(&[]), "pw-dump", "that no pair is up") {
        assert_eq!(settled.map(drop), Ok(()));
    }
    if graph.tool("pw-metadata", &["-n", "default"]).is_some() {
        assert_ne!(
            graph.configured_default(DeviceDirection::Output).as_deref(),
            Some(SINK_NODE_NAME)
        );
    }
    handle.shutdown();
}

#[test]
fn a_device_plugged_in_takes_the_lane_while_the_app_puts_new_devices_first() {
    let Some(graph) = PrivateGraph::start("newfirst") else {
        return;
    };
    if !installed("pw-cli") {
        skip("pw-cli is not available, so newfirst cannot run");
        return;
    }
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    // Every output there is, ranked, as the app's list has them: a device the list does not name
    // is one it has not seen yet, and goes first.
    handle.send(UiToAudio::SetDevicePriority {
        direction: DeviceDirection::Output,
        names: vec!["t_stereo".to_owned(), "t_71".to_owned()],
        new_devices_first: true,
    });
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    said.settle(&handle);
    // Plugged in: the rules run on it before the app has put it at the top of its list.
    assert!(
        graph.add_device("t_dac", DeviceDirection::Output).is_some(),
        "t_dac never appeared"
    );
    assert!(
        said.attached(&handle, DeviceDirection::Output, Some("t_dac")),
        "a device plugged in goes first, as the app is about to rank it"
    );
    handle.shutdown();
}

#[test]
fn a_headset_that_goes_for_good_is_given_up_once_the_lane_has_waited_for_it() {
    // Tied to its card by its Bluetooth address alone: the other way the engine asks.
    let Some((graph, card, handle, mut said)) = engine_on_a_headset("gone", &|_| {
        format!("api.bluez5.address = \"{HEADSET_ADDRESS}\"")
    }) else {
        return;
    };
    assert_eq!(
        graph.card_prop("t_headset_card", "api.bluez5.address"),
        Some(Some(HEADSET_ADDRESS.to_owned())),
        "the card should carry the address the sink shares with it"
    );
    let from = said.0.len();

    assert!(
        graph.remove_node(HEADSET).is_some(),
        "the headset's sink could not be taken out of the graph"
    );
    let gone = Instant::now();
    let moved = said.until(&handle, "the output lane leaving the headset", |m| {
        matches!(m, AudioToUi::Attached {
            direction: DeviceDirection::Output,
            node_name: Some(to),
        } if to != HEADSET)
    });
    let waited = gone.elapsed();
    assert!(moved, "the output lane never left a headset that had gone");
    println!("the output lane left the headset after {waited:?}");
    assert!(
        waited + Duration::from_millis(300) >= crate::engine::RETURN_WAIT,
        "the lane moved after {waited:?}, before its wait for the headset was up"
    );
    let moves = output_moves_since(&said, from);
    let Some(Some(to)) = moves.first() else {
        panic!("no move was heard: {moves:?}");
    };
    assert!(
        ["t_stereo", "t_71"].contains(&to.as_str()),
        "the lane should have moved to the graph's own speakers: {moves:?}"
    );
    assert_eq!(
        graph.node_prop(OUTPUT_NODE_NAME, "target.object").flatten(),
        Some(to.clone())
    );
    drop(card);
    handle.shutdown();
}

#[test]
fn a_headset_switched_off_is_left_as_soon_as_its_card_goes_after_its_sink() {
    // Switched off, a headset takes its sink and its card with it, and the registry can name the
    // sink first: for that moment the sink looks like one between profiles.
    let belongs = |card: &CardHolder| format!("device.id = {}", card.id);
    let Some((graph, card, handle, mut said)) = engine_on_a_headset("off", &belongs) else {
        return;
    };
    let from = said.0.len();

    assert!(
        graph.remove_node(HEADSET).is_some(),
        "the headset's sink could not be taken out of the graph"
    );
    let sink_gone = Instant::now();
    // Three ticks with the card still here: the lane waits, which is what makes the card's going
    // worth testing.
    said.settle(&handle);
    let moves = output_moves_since(&said, from);
    assert!(
        moves.is_empty(),
        "the output lane should wait while the headset's card is still here: {moves:?}"
    );

    drop(card);
    let card_gone = Instant::now();
    let moved = said.until(&handle, "the output lane leaving the headset", |m| {
        matches!(m, AudioToUi::Attached {
            direction: DeviceDirection::Output,
            node_name: Some(to),
        } if to != HEADSET)
    });
    let (after_card, after_sink) = (card_gone.elapsed(), sink_gone.elapsed());
    assert!(moved, "the output lane never left a headset that had gone");
    println!(
        "the output lane left the headset {after_card:?} after its card went, \
         {after_sink:?} after its sink"
    );
    assert_eq!(
        graph.card_prop("t_headset_card", "device.name"),
        Some(None),
        "the card should have gone with the client that made it"
    );
    assert!(
        after_sink + Duration::from_millis(500) < crate::engine::RETURN_WAIT,
        "the lane moved {after_sink:?} after the sink went: it waited out its whole wait for a \
         headset whose card had gone"
    );
    let moves = output_moves_since(&said, from);
    let Some(Some(to)) = moves.first() else {
        panic!("no move was heard: {moves:?}");
    };
    assert!(
        ["t_stereo", "t_71"].contains(&to.as_str()),
        "the lane should have moved to the graph's own speakers: {moves:?}"
    );
    assert_eq!(
        graph.node_prop(OUTPUT_NODE_NAME, "target.object").flatten(),
        Some(to.clone())
    );
    handle.shutdown();
}

#[test]
fn speakers_on_no_card_that_go_are_replaced_at_once() {
    // A virtual sink, or the graph's own null sinks: no card, so nothing to come back.
    let Some(graph) = PrivateGraph::start("cardless") else {
        return;
    };
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == "t_71")),
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_71".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_71")));
    said.settle(&handle);

    if unless_skipped(graph.remove_node("t_71"), "pw-cli", "the 7.1 sink going").is_none() {
        handle.shutdown();
        return;
    }
    let gone = Instant::now();
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    let waited = gone.elapsed();
    println!("the output lane left the 7.1 sink after {waited:?}");
    assert!(
        waited + Duration::from_millis(500) < crate::engine::RETURN_WAIT,
        "a sink on no card has nothing to come back with, yet the lane waited {waited:?}"
    );
    handle.shutdown();
}

/// WirePlumber 0.5's microphone for the test headset: the loopback it puts in front of the
/// headset's SCO source, named by the address with its colons (`create-loopback-node.lua:45`).
const LOOPBACK: &str = "bluez_input.00:11:22:33:44:55";
/// The SCO source behind it, which WirePlumber 0.5 marks `api.bluez5.internal`
/// (`create-node.lua:31-36`).
const SCO_SOURCE: &str = "bluez_input.00_11_22_33_44_55.0";
/// Another headset's microphone as WirePlumber 0.4 lists it: the SCO source itself, on no card
/// this graph has, so only its info can say it is a headset's.
const WP04_MICROPHONE: &str = "bluez_input.66_77_88_99_AA_BB.0";

/// The last device list the engine sent, as `(node.name, form factor)`.
fn last_device_list(said: &Transcript) -> Vec<(String, String)> {
    said.0
        .iter()
        .rev()
        .find_map(|message| match message {
            AudioToUi::Devices(devices) => Some(
                devices
                    .iter()
                    .map(|d| (d.name.clone(), d.form_factor.clone()))
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// How many warnings the engine has sent so far.
fn warnings_heard(said: &Transcript) -> usize {
    said.0
        .iter()
        .filter(|message| matches!(message, AudioToUi::Warning { .. }))
        .count()
}

#[test]
fn a_headsets_microphone_from_either_wireplumber_is_offered_as_a_headset_and_warned_about_once() {
    // The headset's sink as the bluez5 plugin names it in A2DP; its microphone and the SCO source
    // behind it follow.
    let belongs = |card: &CardHolder| {
        format!(
            "device.id = {} api.bluez5.address = \"{HEADSET_ADDRESS}\" \
             api.bluez5.profile = a2dp-sink api.bluez5.codec = sbc",
            card.id
        )
    };
    let Some((graph, card, handle, mut said)) = engine_on_a_headset("u9", &belongs) else {
        return;
    };
    let source = |name: &str, props: &str| {
        graph.add_adapter(
            name,
            &format!(
                "factory.name = support.null-audio-sink node.name = \"{name}\" \
                 node.description = \"Test Headset\" media.class = Audio/Source \
                 priority.driver = 2010 priority.session = 2010 audio.channels = 1 \
                 audio.position = [ MONO ] {props}"
            ),
        )
    };
    // As `create-loopback-node.lua:44-55` makes it: `bluez5.loopback`, the card, no address.
    assert!(
        source(
            LOOPBACK,
            &format!("bluez5.loopback = true device.id = {}", card.id)
        )
        .is_some(),
        "the loopback microphone never appeared"
    );
    assert!(
        source(
            SCO_SOURCE,
            &format!(
                "device.id = {} api.bluez5.address = \"{HEADSET_ADDRESS}\" \
                 api.bluez5.profile = headset-head-unit api.bluez5.codec = msbc \
                 api.bluez5.internal = true bluez5.loopback = false",
                card.id
            )
        )
        .is_some(),
        "the SCO source never appeared"
    );
    assert!(
        source(
            WP04_MICROPHONE,
            "api.bluez5.address = \"66:77:88:99:AA:BB\" \
             api.bluez5.profile = headset-head-unit api.bluez5.codec = cvsd"
        )
        .is_some(),
        "the other headset's microphone never appeared"
    );

    // The registry announces none of what makes these headsets' microphones; their info does.
    let headsets = |list: &[(String, String)]| {
        [LOOPBACK, WP04_MICROPHONE].iter().all(|name| {
            list.iter()
                .any(|(listed, form)| listed == name && form == "headset")
        })
    };
    assert!(
        said.until(&handle, "both microphones listed as headsets", |m| {
            matches!(m, AudioToUi::Devices(d) if headsets(
                &d.iter().map(|d| (d.name.clone(), d.form_factor.clone())).collect::<Vec<_>>()
            ))
        }),
        "the microphones were never offered as headsets: {:?}",
        last_device_list(&said)
    );
    said.settle(&handle);
    let list = last_device_list(&said);
    assert!(headsets(&list), "{list:?}");
    assert!(
        !list.iter().any(|(name, _)| name == SCO_SOURCE),
        "WirePlumber's internal SCO source was offered as a microphone: {list:?}"
    );
    assert!(
        list.iter()
            .any(|(name, form)| name == HEADSET && form == "headphone"),
        "the headset's sink in A2DP is a pair of headphones: {list:?}"
    );
    assert_eq!(warnings_heard(&said), 0, "one lane is not both");

    // The user takes the headset's microphone for the input lane, the music on the headset.
    handle.send(UiToAudio::SelectDevice {
        node_name: LOOPBACK.to_owned(),
        direction: DeviceDirection::Input,
    });
    assert!(said.attached(&handle, DeviceDirection::Input, Some(LOOPBACK)));
    assert_eq!(
        graph
            .node_prop(CAPTURE_NODE_NAME, "target.object")
            .flatten(),
        Some(LOOPBACK.to_owned()),
        "the capture stream records from the loopback, not from the SCO source behind it"
    );
    assert!(
        said.heard(&handle, "the one-headset warning", |m| matches!(
            m,
            AudioToUi::Warning { direction: None, message } if message.contains("16 kHz")
        )),
        "one headset on both lanes was never warned about"
    );
    said.settle(&handle);
    assert_eq!(warnings_heard(&said), 1, "once, not on every tick");

    // The music goes to the speakers and comes back: a new attachment, a new warning.
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    said.settle(&handle);
    assert_eq!(
        warnings_heard(&said),
        1,
        "the speakers and the headset are two devices"
    );
    handle.send(UiToAudio::SelectDevice {
        node_name: HEADSET.to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some(HEADSET)));
    said.settle(&handle);
    assert_eq!(warnings_heard(&said), 2);
    handle.shutdown();
}

// ---- The priority list (U4) -------------------------------------------------------------------

impl PrivateGraph {
    /// Add a stereo sink, or a stereo microphone, while the engine runs: how a device is plugged
    /// in here. With the driver priority a real device carries (see [`Self::add_mono_sink`] for
    /// why that matters on this daemon). `None` when it never appears.
    fn add_device(&self, name: &str, direction: DeviceDirection) -> Option<()> {
        let media_class = match direction {
            DeviceDirection::Output => "Audio/Sink",
            DeviceDirection::Input => "Audio/Source/Virtual",
        };
        self.add_adapter(
            name,
            &format!(
                "factory.name = support.null-audio-sink node.name = {name} \
                 node.description = \"Test {name}\" media.class = {media_class} \
                 priority.driver = 1010 audio.channels = 2 audio.position = [ FL FR ]"
            ),
        )
    }
}

/// Where a lane said it went, from message `from` on.
fn moves_since(said: &Transcript, direction: DeviceDirection, from: usize) -> Vec<Option<String>> {
    said.0[from..]
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

fn rank(direction: DeviceDirection, names: &[&str]) -> UiToAudio {
    UiToAudio::SetDevicePriority {
        direction,
        names: names.iter().map(|&name| name.to_owned()).collect(),
        new_devices_first: false,
    }
}

/// One lane through the priority list on a real graph. The user picks `current`; `above`, ranked
/// above it, is plugged in and takes the lane; `below`, ranked under both, is plugged in and takes
/// nothing; `above` is unplugged, and the lane goes to the first present by rank, `current` — not
/// to `below`, the newest. Under the Windows rules the second step would not happen at all, and the
/// third would move the lane back to the user's pick (rule 4).
fn a_lane_follows_its_ranking(
    tag: &str,
    direction: DeviceDirection,
    current: &str,
    below: &str,
    above: &str,
) {
    let Some(graph) = PrivateGraph::start(tag) else {
        return;
    };
    if !installed("pw-cli") {
        skip(&format!("pw-cli is not available, so {tag} cannot run"));
        return;
    }
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == current)),
    );
    handle.send(rank(direction, &[above, current, below]));
    handle.send(UiToAudio::SelectDevice {
        node_name: current.to_owned(),
        direction,
    });
    assert!(said.attached(&handle, direction, Some(current)));
    said.settle(&handle);

    assert!(
        graph.add_device(above, direction).is_some(),
        "{above} never appeared"
    );
    assert!(
        said.attached(&handle, direction, Some(above)),
        "{above} is ranked above the device the lane is on"
    );
    said.settle(&handle);

    let from = said.0.len();
    assert!(
        graph.add_device(below, direction).is_some(),
        "{below} never appeared"
    );
    // The rules run on the tick after the registry's news; give them a few.
    said.settle(&handle);
    said.settle(&handle);
    assert_eq!(
        moves_since(&said, direction, from),
        [],
        "{below} is ranked below the device the lane is on, and the user's older pick is only \
         a ranked device like any other"
    );

    assert!(graph.remove_node(above).is_some(), "{above} never went");
    assert!(
        said.attached(&handle, direction, Some(current)),
        "with {above} gone, the first present by rank is {current}, not {below}"
    );
    handle.shutdown();
}

#[test]
fn the_speakers_lane_takes_only_a_sink_ranked_above_it_and_falls_back_down_the_ranking() {
    a_lane_follows_its_ranking(
        "rankout",
        DeviceDirection::Output,
        "t_stereo",
        "t_low",
        "t_high",
    );
}

#[test]
fn the_microphone_lane_takes_only_a_microphone_ranked_above_it_and_falls_back_down_the_ranking() {
    a_lane_follows_its_ranking(
        "rankin",
        DeviceDirection::Input,
        "t_mic",
        "t_mic_low",
        "t_mic_high",
    );
}

#[test]
fn an_empty_ranking_hands_the_lane_back_to_the_old_rules() {
    let Some(graph) = PrivateGraph::start("rankoff") else {
        return;
    };
    if !installed("pw-cli") {
        skip("pw-cli is not available, so rankoff cannot run");
        return;
    }
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if d.iter().any(|d| d.name == "t_71")),
    );
    handle.send(rank(
        DeviceDirection::Output,
        &["t_high", "t_71", "t_stereo"],
    ));
    handle.send(UiToAudio::SelectDevice {
        node_name: "t_stereo".to_owned(),
        direction: DeviceDirection::Output,
    });
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    said.settle(&handle);

    // Ranked above the user's pick, a sink plugged in takes the lane…
    assert!(
        graph
            .add_device("t_high", DeviceDirection::Output)
            .is_some()
    );
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_high")));

    // …and with the ranking gone, the Windows rules are back: the user's pick that is present
    // outranks the device that arrived since (rule 4).
    let from = said.0.len();
    handle.send(rank(DeviceDirection::Output, &[]));
    assert!(said.attached(&handle, DeviceDirection::Output, Some("t_stereo")));
    assert_eq!(
        moves_since(&said, DeviceDirection::Output, from),
        [Some("t_stereo".to_owned())]
    );
    handle.shutdown();
}

/// [`CONFIG`] for a daemon that can hold cards ([`PrivateGraph::start_with_cards`]): D-Bus support
/// on, which the daemon is pointed at a private bus for, the Bluetooth plugin to make a card
/// from, and the factory that makes it.
fn card_config() -> String {
    let edits = [
        (
            "support.dbus                = false",
            "support.dbus                = true",
        ),
        (
            "    support.*       = support/libspa-support\n",
            concat!(
                "    support.*       = support/libspa-support\n",
                "    api.bluez5.*    = bluez5/libspa-bluez5\n",
            ),
        ),
        (
            "    { name = libpipewire-module-adapter }\n",
            concat!(
                "    { name = libpipewire-module-adapter }\n",
                "    { name = libpipewire-module-spa-device-factory }\n",
            ),
        ),
    ];
    edits.iter().fold(CONFIG.to_owned(), |config, (from, to)| {
        assert_eq!(
            config.matches(from).count(),
            1,
            "CONFIG no longer has {from:?}"
        );
        config.replace(from, to)
    })
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
