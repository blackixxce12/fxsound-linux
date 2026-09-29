//! Application streams against a private PipeWire (`crate::app_streams`, `docs/0.4.0-apps.md`):
//! players and recorders started with `pw-cat`, carrying the application properties a real
//! program carries, and what the engine reports of them — who each one is, which lane it belongs
//! to, and that it is gone once its process is.
//!
//! Nothing here is linked or moved. There is no WirePlumber, and reporting a stream needs neither:
//! an unlinked `pw-cat` sits in the graph with its node announced and its properties readable
//! until it is killed, which is what a paused player looks like to the registry.

use super::*;
use fxsound_core::{AppKey, AppStream};

/// An application `pw-cat` runs on a private graph ([`PrivateGraph::pw_cat`]). Dropped, its
/// process is killed, and its stream goes with its connection.
pub(crate) struct App {
    /// Kept for its drop, which kills the process.
    child: Guarded,
}

impl App {
    /// The process id of its `pw-cat`: `setpriv`, when it is there, execs it in its own place.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl PrivateGraph {
    /// Start `pw-cat` in `mode` — `--playback` of endless silence, or `--record` into
    /// `/dev/null` — with `properties` for its stream and `extra` arguments after them, and wait
    /// until its node, named `node_name` in `properties`, is in the graph. The socket is on its
    /// command line and in its own environment, and nowhere else. `None` when it could not be
    /// started or its node never appeared.
    ///
    /// No `--raw`: the `pw-cat` of PipeWire 1.0, which Ubuntu 24.04 ships and CI runs, has no such
    /// option and exits at once when given it. So what it plays is a file libsndfile
    /// reads without being told its format — a Sun AU stream of unknown length on its standard
    /// input ([`feed_silence`]), which never ends as `/dev/zero` never did — and what it records
    /// goes to `/dev/null` as whatever format that name gives, which nobody reads.
    pub(crate) fn pw_cat(
        &self,
        mode: &str,
        node_name: &str,
        properties: &str,
        extra: &[&str],
    ) -> Option<App> {
        let playback = mode == "--playback";
        let mut player = support::command("pw-cat");
        player
            .arg("--remote")
            .arg(self.socket())
            .arg(mode)
            .arg(format!(
                "--properties={{ node.name = {node_name} {properties} }}"
            ))
            .args(extra)
            .arg(if playback { "-" } else { "/dev/null" })
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.socket())
            .stdin(if playback {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null());
        let log = self.stderr_log(&mut player, node_name);
        let mut child = match support::spawn(player) {
            Ok(child) => child,
            Err(error) => {
                println!("pw-cat could not be started for {node_name}: {error}");
                return None;
            }
        };
        if let Some(stdin) = child.take_stdin() {
            feed_silence(stdin);
        }
        let deadline = Instant::now() + PATIENCE;
        while self.node_id(node_name).is_none() {
            if Instant::now() >= deadline {
                println!(
                    "{node_name} never appeared in the graph: pw-cat {mode}; {}",
                    child.account(Some(&log))
                );
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(App { child })
    }
}

/// Write silence into `stdin` from a thread of its own until the pipe breaks — when the player
/// reading it has been killed — as a Sun AU stream: a header saying 16-bit stereo at 48 kHz, of
/// unknown length, which libsndfile reads from a pipe, and then zeros. The player's own pace, not
/// this thread's, sets how fast it goes: the pipe fills and the writes wait.
fn feed_silence(mut stdin: std::process::ChildStdin) {
    const AU_UNKNOWN_SIZE: u32 = u32::MAX;
    const AU_PCM_16: u32 = 3;
    let mut header = b".snd".to_vec();
    for word in [24, AU_UNKNOWN_SIZE, AU_PCM_16, 48_000, 2] {
        header.extend_from_slice(&u32::to_be_bytes(word));
    }
    std::thread::spawn(move || {
        let silence = [0_u8; 16 * 1024];
        if stdin.write_all(&header).is_ok() {
            while stdin.write_all(&silence).is_ok() {}
        }
    });
}

/// Start an engine on `graph` and wait until it has listed the graph's devices: connected, with
/// the first look at the graph behind it. Its output lane attaches by itself, so FxSound's own
/// playback stream — a player like any other to the graph — is there too, and must never be
/// reported as an application.
fn engine_on(graph: &PrivateGraph) -> (EngineHandle, Transcript) {
    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    assert!(said.until(
        &handle,
        "a device list",
        |m| matches!(m, AudioToUi::Devices(d) if !d.is_empty())
    ));
    (handle, said)
}

/// Read what the engine says until it reports an application list `done` accepts, and return
/// that list. `None` when none came within the patience.
fn apps_until(
    said: &mut Transcript,
    handle: &EngineHandle,
    what: &str,
    mut done: impl FnMut(&[AppStream]) -> bool,
) -> Option<Vec<AppStream>> {
    let mut found = None;
    said.until(handle, what, |message| match message {
        AudioToUi::AppStreams(streams) if done(streams) => {
            found = Some(streams.clone());
            true
        }
        _ => false,
    });
    found
}

/// One application as a test compares it: `(direction, binary, name, flatpak)`.
type Keyed = (DeviceDirection, String, String, String);

/// A list as [`Keyed`] entries, in one order whatever order it came in.
fn keyed(streams: &[AppStream]) -> Vec<Keyed> {
    sorted(
        streams
            .iter()
            .map(|stream| {
                let AppKey {
                    binary,
                    name,
                    flatpak,
                } = stream.app.clone();
                (stream.direction, binary, name, flatpak)
            })
            .collect(),
    )
}

/// `entries` by direction, then by name.
fn sorted(mut entries: Vec<Keyed>) -> Vec<Keyed> {
    entries.sort_by(|a, b| (a.0.key(), &a.2).cmp(&(b.0.key(), &b.2)));
    entries
}

/// One [`Keyed`] entry, from string slices.
fn entry(direction: DeviceDirection, binary: &str, name: &str, flatpak: &str) -> Keyed {
    (
        direction,
        binary.to_owned(),
        name.to_owned(),
        flatpak.to_owned(),
    )
}

#[test]
fn every_player_and_recorder_is_reported_with_its_key_and_lane_and_gone_once_it_exits() {
    let Some(graph) = PrivateGraph::start("apps") else {
        return;
    };
    if !installed("pw-cat") {
        skip("pw-cat is not installed, so application streams were not checked");
        return;
    }
    let (handle, mut said) = engine_on(&graph);

    // A game that names itself and its binary on its stream, as Wine and Proton do; a voice chat
    // recording, with a Flatpak id on its stream; and a player that says nothing but its name, so
    // that its binary can only come from its client — `pw-cat`, as the connection says.
    let game = graph
        .pw_cat(
            "--playback",
            "t_game",
            r#"application.name = "Battlefield 6" application.process.binary = bf6.exe"#,
            &[],
        )
        .expect("pw-cat should play");
    let chat = graph
        .pw_cat(
            "--record",
            "t_chat",
            "application.name = Discord application.process.binary = discord \
             pipewire.access.portal.app_id = com.discordapp.Discord",
            &[],
        )
        .expect("pw-cat should record");
    let bare = graph
        .pw_cat("--playback", "t_bare", "application.name = Bare", &[])
        .expect("pw-cat should play");

    let want = sorted(vec![
        entry(
            DeviceDirection::Input,
            "discord",
            "Discord",
            "com.discordapp.Discord",
        ),
        entry(DeviceDirection::Output, "pw-cat", "Bare", ""),
        entry(DeviceDirection::Output, "bf6.exe", "Battlefield 6", ""),
    ]);
    let listed = apps_until(&mut said, &handle, "the three applications", |streams| {
        keyed(streams) == want
    })
    .unwrap_or_else(|| {
        panic!(
            "the engine never listed exactly the three applications; it said {:?}",
            said.0
                .iter()
                .filter(|m| matches!(m, AudioToUi::AppStreams(_)))
                .collect::<Vec<_>>()
        )
    });

    // Each under its node's id — the subject a route will write its metadata target for — and
    // on no route yet.
    for (node, name) in [
        ("t_game", "Battlefield 6"),
        ("t_chat", "Discord"),
        ("t_bare", "Bare"),
    ] {
        let stream = listed
            .iter()
            .find(|stream| stream.app.name == name)
            .expect("listed");
        if let Some(id) = unless_skipped(graph.node_id(node), "pw-dump", "the stream's node id") {
            assert_eq!(u64::from(stream.id), id, "{name} under its node's id");
        }
        assert_eq!(stream.route, None);
    }
    // FxSound's own playback stream is in the graph and in no list.
    if let Some(nodes) = unless_skipped(graph.our_nodes(), "pw-dump", "FxSound's own stream") {
        assert!(
            nodes.iter().any(|(name, _)| *name == OUTPUT_NODE_NAME),
            "the output lane's playback stream should be there to be left out: {nodes:?}"
        );
    }

    // The game quits.
    drop(game);
    let listed = apps_until(&mut said, &handle, "the game gone", |streams| {
        streams.len() == 2
    })
    .expect("the game's stream should leave the list");
    assert!(
        listed
            .iter()
            .all(|stream| stream.app.name != "Battlefield 6")
    );

    // And the rest.
    drop(chat);
    drop(bare);
    assert!(
        apps_until(&mut said, &handle, "an empty list", <[AppStream]>::is_empty).is_some(),
        "the list should be empty once every application has exited"
    );
    handle.shutdown();
}

#[test]
fn a_monitor_recorder_is_listed_only_when_it_records_fxsound_and_a_dont_move_stream_is_listed() {
    let Some(graph) = PrivateGraph::start("appmon") else {
        return;
    };
    if !installed("pw-cat") {
        skip("pw-cat is not installed, so application streams were not checked");
        return;
    }
    let (handle, mut said) = engine_on(&graph);

    // A visualiser on the speakers' monitor records no microphone and no FxSound: left out. A
    // screen recorder on FxSound's sink's monitor hears FxSound: listed, as a recorder. A kiosk
    // player that may never be moved: listed all the same.
    let _visualiser = graph
        .pw_cat(
            "--record",
            "t_visualiser",
            "application.name = Visualiser stream.capture.sink = true",
            &["--target", "t_stereo"],
        )
        .expect("pw-cat should record");
    let _screen = graph
        .pw_cat(
            "--record",
            "t_screen",
            r#"application.name = "Screen Recorder" stream.capture.sink = true"#,
            &["--target", SINK_NODE_NAME],
        )
        .expect("pw-cat should record");
    let _kiosk = graph
        .pw_cat(
            "--playback",
            "t_kiosk",
            "application.name = Kiosk node.dont-move = true",
            &[],
        )
        .expect("pw-cat should play");
    // Started last, so that once it is listed, everything started before it has been looked at:
    // the registry announces nodes in order, and their infos arrive in the order they were bound.
    let _marker = graph
        .pw_cat("--playback", "t_marker", "application.name = Marker", &[])
        .expect("pw-cat should play");

    let listed = apps_until(&mut said, &handle, "the marker", |streams| {
        streams.iter().any(|stream| stream.app.name == "Marker")
    })
    .expect("the marker should be listed");
    let mut names: Vec<(&str, DeviceDirection)> = listed
        .iter()
        .map(|stream| (stream.app.name.as_str(), stream.direction))
        .collect();
    names.sort_by_key(|(name, _)| *name);
    assert_eq!(
        names,
        vec![
            ("Kiosk", DeviceDirection::Output),
            ("Marker", DeviceDirection::Output),
            ("Screen Recorder", DeviceDirection::Input),
        ],
        "the visualiser records another sink's monitor and is no application of either lane"
    );
    assert!(
        graph.node_id("t_visualiser").is_some(),
        "the visualiser should still be in the graph, so its absence from the list means something"
    );
    handle.shutdown();
}

/// Applications that were playing and recording before FxSound started are in its first list, all
/// of them, each with its whole key — its client's binary included where its stream names none. A
/// list in instalments would show the Applications page filling in, and a rule would first be
/// matched against a key with no binary in it. Two things see to it: a stream is reported only
/// once its info and its client's are in (`app_streams`), and nothing is reported before the
/// engine's first look at the graph is complete (`engine::publish_app_streams`).
#[test]
fn applications_already_running_are_reported_whole_in_the_first_list() {
    let Some(graph) = PrivateGraph::start("appfirst") else {
        return;
    };
    if !installed("pw-cat") {
        skip("pw-cat is not installed, so application streams were not checked");
        return;
    }
    let _game = graph
        .pw_cat(
            "--playback",
            "t_game",
            r#"application.name = "Battlefield 6" application.process.binary = bf6.exe"#,
            &[],
        )
        .expect("pw-cat should play");
    let _bare = graph
        .pw_cat("--playback", "t_bare", "application.name = Bare", &[])
        .expect("pw-cat should play");
    let _chat = graph
        .pw_cat("--record", "t_chat", "application.name = Discord", &[])
        .expect("pw-cat should record");

    let handle =
        AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
    let mut said = Transcript::default();
    let first = apps_until(&mut said, &handle, "the first application list", |_| true)
        .expect("the engine should list the applications that were already running");
    assert_eq!(
        keyed(&first),
        sorted(vec![
            entry(DeviceDirection::Input, "pw-cat", "Discord", ""),
            entry(DeviceDirection::Output, "pw-cat", "Bare", ""),
            entry(DeviceDirection::Output, "bf6.exe", "Battlefield 6", ""),
        ]),
        "the first list should hold every application, each with its client's binary where its \
         stream names none"
    );
    handle.shutdown();
}
