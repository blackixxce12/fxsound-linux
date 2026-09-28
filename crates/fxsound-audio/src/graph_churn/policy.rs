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

use super::*;

/// The daemon's configuration for a graph with WirePlumber on it: [`CONFIG`]'s daemon, with what
/// WirePlumber needs besides — and without the `default` metadata object, which WirePlumber makes
/// for itself. Two stereo sinks and a mono microphone, each with the session priority a real
/// device carries, so WirePlumber picks the defaults the way it would on a desktop.
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

/// A [`PrivateGraph`] with WirePlumber managing it. The session manager goes first when it is
/// dropped, then the daemon and its directory.
pub(crate) struct PolicyGraph {
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

/// Whether `wireplumber --version` names a library of 0.5 or later.
fn wireplumber_05() -> bool {
    let Ok(output) = support::command("wireplumber")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .skip_while(|word| *word != "libwireplumber")
        .nth(1)
        .and_then(|version| {
            let mut parts = version.split('.').map(str::parse::<u32>);
            Some((parts.next()?.ok()?, parts.next()?.ok()?))
        })
        .is_some_and(|version| version >= (0, 5))
}

impl PolicyGraph {
    /// A private graph with WirePlumber on it, once WirePlumber has picked the defaults. `None`
    /// when there is no `pipewire` (a skip, as for every graph here) or no WirePlumber 0.5 (a skip
    /// that passes everywhere); a WirePlumber that is there and never comes up is a failure.
    pub(crate) fn start(tag: &str) -> Option<Self> {
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
        let mut command = support::command("wireplumber");
        command
            .args(["-p", "fxsound-test"])
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
            .env_remove("WIREPLUMBER_DEBUG")
            .env_remove("WIREPLUMBER_CONFIG_DIR")
            .env_remove("PIPEWIRE_DEBUG")
            .env_remove("PIPEWIRE_RUNTIME_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let session_manager = support::spawn(command).expect("wireplumber should start");
        let graph = Self {
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
