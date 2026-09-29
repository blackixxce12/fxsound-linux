//! A private graph with WirePlumber on it: for what FxSound does that only a session manager's
//! policy shows — where it links the streams that follow the default when FxSound takes it.
//!
//! Every other graph here has no session manager, and each test does by hand what WirePlumber
//! would do. That cannot show a race in WirePlumber itself, and the 0.4.0 live check found one:
//! a recorder that follows the default source, moved from a mono microphone back onto FxSound's
//! stereo source when the power comes back on, is now and then linked to nothing at all
//! (`crate::stranded`). So this graph runs the real thing, inside an environment of its own: the
//! daemon's runtime directory, a configuration, state, data and home directory of the graph's own,
//! the socket named in WirePlumber's environment and nowhere else, and no D-Bus — the profile
//! below loads nothing that would want a bus, and the bus addresses name sockets that do not
//! exist. It manages nothing but this graph's null devices: no ALSA, no Bluetooth, no cameras.
//!
//! WirePlumber 0.5 is needed, for its profiles. Ubuntu 24.04, where CI's first test leg runs, ships
//! 0.4, which is configured in Lua instead; where there is no 0.5 the tests that need it say so and
//! pass, even under [`REQUIRE_TOOLS`], which is for the tools every leg installs. CI's second leg,
//! an Arch Linux container, has 0.5 and sets [`REQUIRE_WIREPLUMBER`], under which such a skip fails
//! instead: there the tests here have to run.
//!
//! A test can also have `pipewire-pulse` on the graph ([`PolicyGraph::start_pulse`]): the
//! PulseAudio server most applications on a desktop still play and record through, whose streams
//! WirePlumber moves and keeps the volume of as it does `pw-cat`'s, and whose clients show the
//! desktop's mixer the channel volumes only. Its socket is in the graph's runtime directory, its
//! clients are told of it and of nothing else (`PULSE_SERVER`), and it has the session
//! manager's environment: no bus, no home but the graph's.

use super::*;

/// The daemon's configuration for a graph with WirePlumber on it: [`CONFIG`]'s daemon, with what
/// WirePlumber needs besides — and without the `default` metadata object, which WirePlumber makes
/// for itself. Two stereo sinks and two mono microphones, each with the session priority a real
/// device carries, so WirePlumber picks the defaults the way it would on a desktop: `t_stereo` and
/// `t_mic`, the others ranked below them for a switch between devices. The profiler is
/// loaded for `pw-top`, whose error counts tell the click measurements ([`clicks`](super::clicks))
/// which runs had xruns.
const POLICY_CONFIG: &str = r#"
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
    { name = libpipewire-module-profiler }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-device-factory }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-client-device }
    { name = libpipewire-module-access }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
    { name = libpipewire-module-session-manager }
]
context.objects = [
    { factory = adapter
        args = { factory.name = support.null-audio-sink  node.name = "t_stereo"
                 node.description = "Test Stereo Out"  media.class = Audio/Sink
                 object.linger = true  audio.channels = 2  audio.position = [ FL FR ]
                 priority.session = 1010  priority.driver = 1010  device.api = alsa } }
    { factory = adapter
        args = { factory.name = support.null-audio-sink  node.name = "t_other"
                 node.description = "Test Other Out"  media.class = Audio/Sink
                 object.linger = true  audio.channels = 2  audio.position = [ FL FR ]
                 priority.session = 1009  priority.driver = 1009  device.api = alsa } }
    { factory = adapter
        args = { factory.name = support.null-audio-sink  node.name = "t_mic"
                 node.description = "Test Microphone"  media.class = Audio/Source/Virtual
                 object.linger = true  audio.channels = 1  audio.position = [ MONO ]
                 priority.session = 2010  priority.driver = 2010  device.api = alsa } }
    { factory = adapter
        args = { factory.name = support.null-audio-sink  node.name = "t_mic2"
                 node.description = "Test Other Microphone"  media.class = Audio/Source/Virtual
                 object.linger = true  audio.channels = 1  audio.position = [ MONO ]
                 priority.session = 2009  priority.driver = 2009  device.api = alsa } }
]
"#;

/// The WirePlumber profile the graph's session manager runs: the standard policy, and nothing
/// that reaches past this graph — no hardware monitors, no D-Bus, no logind, no portal.
const PROFILE: &str = r"
wireplumber.profiles = {
  fxsound-test = {
    inherits = [ base ]
    metadata.sm-settings = required
    metadata.sm-objects = required
    policy.standard = required
    hardware.audio = disabled
    hardware.bluetooth = disabled
    hardware.video-capture = disabled
    support.dbus = disabled
    support.system-dbus = disabled
    support.logind = disabled
    support.mpris = disabled
    support.portal-permissionstore = disabled
    support.modem-manager = disabled
    support.reserve-device = disabled
    monitor.bluez.seat-monitoring = disabled
    monitor.alsa.reserve-device = disabled
  }
}
";

/// The graph's WirePlumber's log, in the graph's directory.
const SESSION_MANAGER_LOG: &str = "wireplumber.log";

/// A [`PrivateGraph`] with WirePlumber managing it. The PulseAudio server goes first when it is
/// dropped, then the session manager, then the daemon and its directory.
pub(crate) struct PolicyGraph {
    /// `pipewire-pulse`, once a test has asked for it ([`Self::start_pulse`]).
    pulse: Option<Guarded>,
    session_manager: Guarded,
    graph: PrivateGraph,
}

impl std::ops::Deref for PolicyGraph {
    type Target = PrivateGraph;

    fn deref(&self) -> &PrivateGraph {
        &self.graph
    }
}

/// Set to `1` where WirePlumber 0.5 is installed for these tests: `.github/workflows/ci.yml` sets it
/// on its Arch Linux leg, where a skip here can only mean that something which should work did not.
const REQUIRE_WIREPLUMBER: &str = "FXSOUND_REQUIRE_WIREPLUMBER";

/// Report that a check needing WirePlumber 0.5 could not run: a `SKIPPED` line and a pass, under
/// [`REQUIRE_TOOLS`] as well — CI's Ubuntu ships 0.4 (module docs) — and a failure under
/// [`REQUIRE_WIREPLUMBER`].
fn skip_without_session_manager(reason: &str) {
    assert!(
        std::env::var_os(REQUIRE_WIREPLUMBER).is_none_or(|value| value != "1"),
        "{reason}, and {REQUIRE_WIREPLUMBER}=1 says WirePlumber 0.5 is here"
    );
    println!("SKIPPED: {reason}");
}

/// Whether `wireplumber --version` names a library of 0.5 or later
/// ([`crate::wireplumber_hook::supported`]).
fn wireplumber_05() -> bool {
    let Ok(output) = support::command("wireplumber")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    crate::wireplumber_hook::parse_version(&String::from_utf8_lossy(&output.stdout))
        .is_some_and(crate::wireplumber_hook::supported)
}

/// The environment every process of `graph`'s own runs in beside the daemon: its runtime
/// directory and socket, a configuration, state, data and home directory of the graph's, bus
/// addresses naming sockets that do not exist, and the graph's own PulseAudio server
/// ([`pulse_socket`]) for a PulseAudio client.
fn private_environment(graph: &PrivateGraph, command: &mut std::process::Command) {
    command
        .env("XDG_RUNTIME_DIR", graph.dir.join("run"))
        .env("PIPEWIRE_REMOTE", graph.socket())
        .env("XDG_CONFIG_HOME", graph.dir.join("config"))
        .env("XDG_STATE_HOME", graph.dir.join("state"))
        .env("XDG_DATA_HOME", graph.dir.join("data"))
        .env("HOME", graph.dir.join("home"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", graph.dir.join("no-session-bus").display()),
        )
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            format!("unix:path={}", graph.dir.join("no-system-bus").display()),
        )
        .env(
            "PULSE_SERVER",
            format!("unix:{}", pulse_socket(graph).display()),
        )
        .env_remove("PULSE_RUNTIME_PATH")
        .env_remove("PULSE_COOKIE")
        .env_remove("PIPEWIRE_DEBUG")
        .env_remove("PIPEWIRE_RUNTIME_DIR");
}

/// Where `pipewire-pulse` listens on `graph`: `pulse/native` in the graph's runtime directory, as
/// its own configuration has it (`unix:native`).
fn pulse_socket(graph: &PrivateGraph) -> PathBuf {
    graph.dir.join("run/pulse/native")
}

impl PolicyGraph {
    /// A private graph with WirePlumber on it, once WirePlumber has picked the defaults. `None`
    /// when there is no `pipewire` (a skip, as for every graph here) or no WirePlumber 0.5 (a skip
    /// that passes everywhere); a WirePlumber that is there and never comes up is a failure.
    pub(crate) fn start(tag: &str) -> Option<Self> {
        Self::start_prepared(tag, |_| {})
    }

    /// [`Self::start`], with FxSound's hook in WirePlumber installed where the graph's WirePlumber
    /// reads it ([`crate::wireplumber_hook`]), as Settings ▸ Experimental installs it for a user.
    pub(crate) fn start_with_the_hook(tag: &str) -> Option<Self> {
        Self::start_prepared(tag, |hook| hook.install().expect("the hook installs"))
    }

    /// [`Self::start`], with `prepare` given the place of FxSound's hook in the graph's
    /// WirePlumber's directories before WirePlumber starts.
    pub(crate) fn start_prepared(
        tag: &str,
        prepare: impl FnOnce(&crate::wireplumber_hook::Place),
    ) -> Option<Self> {
        if !wireplumber_05() {
            skip_without_session_manager(&format!(
                "WirePlumber 0.5 is not installed, so {tag} cannot run"
            ));
            return None;
        }
        let graph = PrivateGraph::spawn_with(tag, POLICY_CONFIG, false)
            .inspect_err(|why| skip(&format!("{why}, so {tag} cannot run")))
            .ok()?;
        for dir in [
            "config/wireplumber/wireplumber.conf.d",
            "state",
            "data",
            "home",
        ] {
            std::fs::create_dir_all(graph.dir.join(dir)).expect("the graph's directory is ours");
        }
        std::fs::write(
            graph
                .dir
                .join("config/wireplumber/wireplumber.conf.d/90-fxsound-test.conf"),
            PROFILE,
        )
        .expect("the graph's directory is ours");
        prepare(&Self::hook_place(&graph));
        let mut command = support::command("wireplumber");
        private_environment(&graph, &mut command);
        command
            .args(["-p", "fxsound-test"])
            // Warnings, and what FxSound's hook says it did ([`Self::session_manager_log`]).
            .env("WIREPLUMBER_DEBUG", "2,s-fxsound:4")
            .env_remove("WIREPLUMBER_CONFIG_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(support::log_to(&graph.dir.join(SESSION_MANAGER_LOG)));
        let session_manager = support::spawn(command).expect("wireplumber should start");
        let graph = Self {
            pulse: None,
            session_manager,
            graph,
        };
        // Up once it has made the `default` object and picked a default sink in it.
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            let picked = graph
                .tool("pw-metadata", &["-n", "default"])
                .is_some_and(|listing| listing.contains("key:'default.audio.sink'"));
            if picked {
                return Some(graph);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the private WirePlumber never picked a default sink");
    }

    /// Where FxSound's hook goes for `graph`'s WirePlumber: the configuration and data
    /// directories of its environment ([`private_environment`]).
    fn hook_place(graph: &PrivateGraph) -> crate::wireplumber_hook::Place {
        crate::wireplumber_hook::Place::under(&graph.dir.join("config"), &graph.dir.join("data"))
    }

    /// What the graph's WirePlumber has written to its log: warnings and errors, and the lines of
    /// FxSound's hook.
    pub(crate) fn session_manager_log(&self) -> String {
        std::fs::read_to_string(self.graph.dir.join(SESSION_MANAGER_LOG)).unwrap_or_default()
    }

    /// Start `pipewire-pulse` on this graph, with the configuration it is installed with, and wait
    /// until it answers `pactl`. Whether it did; `false`, said why, when `pipewire-pulse`, `pactl`
    /// or `pacat` is not installed (a skip, as for every tool here). Started once: asked again, it
    /// says whether it runs.
    pub(crate) fn start_pulse(&mut self) -> bool {
        if self.pulse.is_some() {
            return true;
        }
        if let Some(missing) = ["pipewire-pulse", "pactl", "pacat"]
            .into_iter()
            .find(|tool| !installed(tool))
        {
            skip(&format!(
                "{missing} is not installed, so no PulseAudio application could be checked"
            ));
            return false;
        }
        let mut server = support::command("pipewire-pulse");
        private_environment(&self.graph, &mut server);
        server.stdin(Stdio::null()).stdout(Stdio::null());
        let log = self.stderr_log(&mut server, "pipewire-pulse");
        let mut server = support::spawn(server).expect("pipewire-pulse should start");
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            let mut info = support::command("pactl");
            private_environment(&self.graph, &mut info);
            let answers = info
                .arg("info")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if answers {
                self.pulse = Some(server);
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "the private pipewire-pulse never answered: {}",
            server.account(Some(&log))
        );
    }

    /// `pacat` as the PulseAudio application `name` — its client and its stream so called, which
    /// `pipewire-pulse` makes the node's `node.name` and `application.name` — in `mode`,
    /// `--playback` or `--record`, raw stereo `f32` at 48 kHz on `file`, following the default
    /// device as an application that names none does. Wait until its node is in the graph. `None`,
    /// said why, when it could not start or never appeared. [`Self::start_pulse`] first.
    ///
    /// Its latency is asked for, so that its node asks the graph for no shorter a quantum than the
    /// graph's own, 1024 frames: 85 ms of buffer for a player, 25 ms of fragments for a recorder,
    /// which `pipewire-pulse` 1.6.9 makes 1080 and 1200 frames. A client that asks for none is
    /// given two seconds of fragments to record into, which it writes in bursts. And one that asks
    /// for less lowers the quantum of the graph it joins, and a PulseAudio recorder in that graph
    /// then drops samples: measured without FxSound, on this graph, a `pacat` recording `t_mic`
    /// jumped in the middle of its wave each of four times a player of 256 frames was linked into
    /// `t_mic`, and not once when it went. That is PipeWire's, whoever moves the player — and a
    /// player of 20 ms, moved by the power switch, had the click scenario's recorder jump at −17.7
    /// to −28.6 dBFS 90 ms after the switch, while it was still where it was, before its own
    /// handover began ([`clicks`](super::clicks)).
    fn pacat(&self, mode: &str, name: &str, file: &std::path::Path) -> Option<Guarded> {
        let mut client = if mode == "--record" {
            support::command_writing_at_most("pacat", RECORDING_LIMIT)
        } else {
            support::command("pacat")
        };
        client
            .arg(mode)
            .args([
                "--raw",
                "--format=float32le",
                "--rate=48000",
                "--channels=2",
            ])
            .arg(format!(
                "--latency-msec={}",
                if mode == "--record" { 25 } else { 85 }
            ))
            .arg(format!("--client-name={name}"))
            .arg(format!("--stream-name={name}"))
            .arg(file)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        private_environment(&self.graph, &mut client);
        let log = self.stderr_log(&mut client, name);
        let mut child = match support::spawn(client) {
            Ok(child) => child,
            Err(error) => {
                println!("pacat could not be started for {name}: {error}");
                return None;
            }
        };
        let deadline = Instant::now() + PATIENCE;
        while self.node_id(name).is_none() {
            if Instant::now() >= deadline {
                println!(
                    "{name} never appeared: pacat {mode}; {}",
                    child.account(Some(&log))
                );
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(child)
    }

    /// A PulseAudio application called `name` playing `file` — raw stereo `f32` at 48 kHz — or
    /// endless silence when there is none, to the default sink ([`Self::pacat`]).
    pub(crate) fn pulse_player(
        &self,
        name: &str,
        file: Option<&std::path::Path>,
    ) -> Option<Guarded> {
        self.pacat(
            "--playback",
            name,
            file.unwrap_or_else(|| std::path::Path::new("/dev/zero")),
        )
    }

    /// A PulseAudio application called `name` recording the default source into a file in the
    /// graph's directory, raw stereo `f32` at 48 kHz, capped at [`RECORDING_LIMIT`]
    /// ([`Self::pacat`]); and that file.
    pub(crate) fn pulse_recorder(&self, name: &str) -> Option<(Guarded, PathBuf)> {
        let file = self.dir.join(format!("{name}.raw"));
        let child = self.pacat("--record", name, &file)?;
        Some((child, file))
    }

    /// Whether the session manager is still running: a test that finds its own checks failing
    /// asks, so that a WirePlumber that died is not taken for FxSound's doing.
    pub(crate) fn session_manager_runs(&mut self) -> bool {
        self.session_manager
            .try_wait()
            .is_ok_and(|status| status.is_none())
    }

    /// Start `pw-record` as an application that follows the default source, recording into a file
    /// in the graph's directory, capped at [`RECORDING_LIMIT`]; and wait until its node, called
    /// `name`, is in the graph. `None` when it could not start or never appeared.
    pub(crate) fn follow_default_recorder(&self, name: &str) -> Option<(Guarded, PathBuf)> {
        let file = self.dir.join(format!("{name}.raw"));
        let mut recorder = support::command_writing_at_most("pw-record", RECORDING_LIMIT);
        recorder
            .arg("--remote")
            .arg(self.socket())
            .args(["--format", "f32", "--rate", "48000", "--channels", "2"])
            .arg(format!(
                "--properties={{ node.name = {name} application.name = {name} media.name = {name} }}"
            ))
            .arg(&file)
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.socket())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = support::spawn(recorder).ok()?;
        let deadline = Instant::now() + PATIENCE;
        while self.node_id(name).is_none() {
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some((child, file))
    }

    /// Whether a link from the node `output` into the node `input` is in the graph. `None` when
    /// `pw-dump` is not there to ask.
    pub(crate) fn linked(&self, output: u64, input: u64) -> Option<bool> {
        let objects = self.dump()?;
        Some(objects.iter().any(|object| {
            object["type"].as_str() == Some("PipeWire:Interface:Link")
                && object["info"]["output-node-id"].as_u64() == Some(output)
                && object["info"]["input-node-id"].as_u64() == Some(input)
        }))
    }
}

/// How big `file` is, or 0 while it is not there.
pub(crate) fn size_of(file: &std::path::Path) -> u64 {
    std::fs::metadata(file).map_or(0, |metadata| metadata.len())
}
