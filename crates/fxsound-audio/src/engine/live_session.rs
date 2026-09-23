//! The main loop's own bookkeeping, against a private PipeWire daemon.
//!
//! `graph_churn` drives the whole engine from outside, through its handle, and can see only what
//! the engine says and what the server shows. Some of what the 0.3.0 audit found wrong is neither:
//! which proxies the main loop still holds, what it believes the graph's clock runs at, whether a
//! device that changed under a running pair gets a new one. So the tests here *are* the main loop:
//! a [`Shared`] of their own, connected with [`connect`] to one of `graph_churn`'s daemons, pumped
//! by hand, and called into where the supervisor would call — in an order each test chooses, rather
//! than whenever a 200 ms timer happens to fire. The daemon is private for the same reason as
//! there: nothing here may touch the session's graph.

use super::*;
use crate::graph_churn::{PATIENCE, PrivateGraph, installed, skip, unless_skipped};
use crate::lane_dsp::tests::lanes_for_tests;

/// A main loop of the test's own, connected to a private daemon.
///
/// Field order is drop order: the connection's state before the context and the loop it was made
/// on, and the daemon last of all.
struct Harness {
    shared: Rc<RefCell<Shared>>,
    context: pw::context::ContextRc,
    mainloop: pw::main_loop::MainLoopRc,
    graph: PrivateGraph,
}

impl Harness {
    /// Connect to `graph`, and wait until its registry is in and every one of its three devices
    /// has said what it is made of.
    fn connect(graph: PrivateGraph) -> Self {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).expect("a main loop");
        let context = pw::context::ContextRc::new(&mainloop, None).expect("a context");
        let (notify, _) = crossbeam_channel::unbounded();
        let (dsp, handover) = lanes_for_tests();
        let shared = Rc::new(RefCell::new(Shared::new(
            notify,
            Some(graph.remote()),
            None,
            PerDirection {
                output: Some(dsp.output),
                input: Some(dsp.input),
            },
            handover,
        )));
        let harness = Self {
            shared,
            context,
            mainloop,
            graph,
        };
        connect(&harness.shared, &harness.context)
            .expect("the private daemon should take a connection");
        assert!(
            harness.until("the registry and every device's format", settled),
            "the private graph never settled"
        );
        harness
    }

    /// Run the loop until `done` holds. Whether it came to.
    fn until(&self, what: &str, done: impl Fn(&Shared) -> bool) -> bool {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if done(&self.shared.borrow()) {
                return true;
            }
            if Instant::now() >= deadline {
                println!("gave up waiting for {what}");
                return false;
            }
            self.mainloop
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
        }
    }

    /// Run the loop for a while: what was queued for the server leaves, and what the server said
    /// arrives. libpipewire writes to the socket only from the loop.
    fn pump(&self, how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            self.mainloop
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(left));
        }
    }

    /// [`Self::until`], with NODE 2 paced on every turn of the loop as the supervisor's tick paces
    /// it — for the steps that wait for time to pass rather than for an event.
    fn pacing_until(&self, what: &str, done: impl Fn(&Shared) -> bool) -> bool {
        let deadline = Instant::now() + PATIENCE;
        loop {
            {
                let mut shared = self.shared.borrow_mut();
                pace_second_node(&mut shared, DeviceDirection::Output, Instant::now());
                if done(&shared) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                println!("gave up waiting for {what}");
                return false;
            }
            self.mainloop
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
        }
    }

    /// Do something to the graph from another thread while this one keeps the loop turning. A
    /// port configured or a link made on one of our nodes is not done until this client has
    /// answered for it, and this client only answers from its loop.
    fn meanwhile<T: Send>(&self, work: impl FnOnce(&PrivateGraph) -> T + Send) -> T {
        std::thread::scope(|scope| {
            let graph = &self.graph;
            let job = scope.spawn(move || work(graph));
            while !job.is_finished() {
                self.mainloop
                    .loop_()
                    .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
            }
            job.join().expect("the graph tool thread panicked")
        })
    }

    /// The registry id of the device called `name`.
    fn id_of(&self, name: &str) -> u32 {
        self.shared
            .borrow()
            .devices
            .iter()
            .find(|device| device.name == name)
            .map(|device| device.object_id)
            .unwrap_or_else(|| panic!("{name} is one of the private graph's devices"))
    }
}

impl Drop for Harness {
    /// The session's listeners hold the `Shared` they report to, so the session has to be closed
    /// for that `Rc` to go at all — and closed while the loop and context it was made on are
    /// still here.
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.try_borrow_mut() {
            close_session(&mut shared);
        }
    }
}

/// The private graph's three devices are in, each with the channel count its node reported.
fn settled(shared: &Shared) -> bool {
    shared.ready()
        && shared.devices.len() == 3
        && shared.devices.iter().all(|device| device.channels != 0)
}

/// Run a lane's rules as the supervisor's tick does: the request is taken, then acted on. A test
/// that ran them without taking it would leave the flag from whatever asked before — the
/// registry's first dump asks every enabled lane — and could not tell whether anything asked since.
fn run_rules(shared: &mut Shared, direction: DeviceDirection) {
    shared.lanes.get_mut(direction).needs_rules = false;
    apply_rules(shared, direction);
}

/// What a lane's pair is attached to, and at how many channels.
fn pair_of(shared: &Shared, direction: DeviceDirection) -> Option<(String, u32)> {
    shared
        .lanes
        .get(direction)
        .nodes
        .as_ref()
        .map(|nodes| (nodes.target.clone(), nodes.format.channels))
}

/// What the output lane's NODE 2 was last told, when its pair is paced by hand.
fn pace_of(shared: &Shared) -> Option<bool> {
    shared
        .lanes
        .output
        .nodes
        .as_ref()?
        .pace
        .map(|pace| pace.active)
}

/// The rate a lane's pair was built at.
fn rate_of(shared: &Shared, direction: DeviceDirection) -> Option<u32> {
    shared
        .lanes
        .get(direction)
        .nodes
        .as_ref()
        .map(|nodes| nodes.format.rate)
}

/// A card switched to another profile keeps its name and changes its channel count, and the pair
/// attached to it has to follow. In 0.3.0 it could not, twice over: `on_node_info` ignored every
/// report after a node's first, and the rules — had they been asked — found a pair on the right
/// name and left it at the wrong width.
#[test]
fn a_pair_follows_its_device_to_a_new_channel_count() {
    let Some(graph) = PrivateGraph::start("profile") else {
        return;
    };
    let harness = Harness::connect(graph);
    let card = harness.id_of("t_71");
    let surround = ChannelMap::parse("FL,FR,FC,LFE,RL,RR,SL,SR");

    // The card is in a stereo profile when the user picks it.
    {
        let mut shared = harness.shared.borrow_mut();
        let t_71 = shared
            .devices
            .iter_mut()
            .find(|device| device.object_id == card)
            .expect("the card");
        t_71.channels = 2;
        t_71.positions = ChannelMap::default_for(2);
        shared.memory.output.user_selected = "t_71".to_owned();
        run_rules(&mut shared, DeviceDirection::Output);
        assert_eq!(
            pair_of(&shared, DeviceDirection::Output),
            Some(("t_71".to_owned(), 2))
        );
    }

    // Its profile is switched to 7.1.
    on_node_info(&harness.shared, card, 8, surround);
    {
        let shared = harness.shared.borrow();
        assert!(
            shared.lanes.output.needs_rules,
            "a pair built for two channels, on a device that now has eight, was left alone"
        );
        assert!(!shared.lanes.input.needs_rules);
    }

    // The supervisor's next tick: the rules find a pair on the right device at the wrong width.
    {
        let mut shared = harness.shared.borrow_mut();
        run_rules(&mut shared, DeviceDirection::Output);
        assert_eq!(
            pair_of(&shared, DeviceDirection::Output),
            Some(("t_71".to_owned(), 8)),
            "the rules saw the right name and kept the stereo pair"
        );
    }

    // The same report again — the node was only suspended and woken — rebuilds nothing.
    on_node_info(&harness.shared, card, 8, surround);
    assert!(!harness.shared.borrow().lanes.output.needs_rules);

    // And the server holds a 7.1 sink, not the stereo one.
    harness.pump(Duration::from_millis(300));
    if let Some(format) = unless_skipped(
        harness.graph.node_format(SINK_NODE_NAME),
        "pw-dump",
        "the rebuilt sink's format",
    ) {
        assert_eq!(format, Some((48_000, 8)));
    }
}

/// The graph's rate is in the server's `settings` object, whose properties come as events on the
/// proxy bound to it — and 0.3.0 dropped that proxy as soon as it had bound it. A graph forced to
/// 44.1 kHz got a 48 kHz sink, and one forced back again was never heard of.
#[test]
fn an_output_pair_runs_at_the_rate_the_graph_is_forced_to_and_follows_its_release() {
    let Some(graph) = PrivateGraph::start("clock") else {
        return;
    };
    let force = |graph: &PrivateGraph, rate: &str| {
        graph
            .tool(
                "pw-metadata",
                &["-n", "settings", "0", "clock.force-rate", rate],
            )
            .map(drop)
    };
    // Forced before FxSound connects, as a session running a pro-audio application would be.
    if unless_skipped(force(&graph, "44100"), "pw-metadata", "a forced graph rate").is_none() {
        return;
    }
    let harness = Harness::connect(graph);
    assert!(
        harness.until("the forced rate", |shared| shared.clock.rate() == 44_100),
        "the settings object's forced rate never reached the engine"
    );

    // `t_stereo` publishes no rate of its own, like most ALSA sinks: its pair runs at the graph's.
    {
        let mut shared = harness.shared.borrow_mut();
        shared.memory.output.user_selected = "t_stereo".to_owned();
        run_rules(&mut shared, DeviceDirection::Output);
        assert_eq!(rate_of(&shared, DeviceDirection::Output), Some(44_100));
    }

    // Released: the plain rate is back, and the pair follows it.
    assert!(
        force(&harness.graph, "0").is_some(),
        "pw-metadata forced the rate and then would not release it"
    );
    assert!(
        harness.until("the forced rate's release", |shared| {
            shared.clock.rate() == 48_000 && shared.lanes.output.needs_rules
        }),
        "the release never reached the engine, or never asked the output lane"
    );
    let mut shared = harness.shared.borrow_mut();
    run_rules(&mut shared, DeviceDirection::Output);
    assert_eq!(rate_of(&shared, DeviceDirection::Output), Some(48_000));
    assert_eq!(
        pair_of(&shared, DeviceDirection::Output),
        Some(("t_stereo".to_owned(), 2))
    );
}

/// A node probe is a proxy on the connection it was bound on: one per device of that connection,
/// gone with it, and never kept beside the next connection's probe for the same device.
#[test]
fn every_device_has_one_node_probe_and_a_reconnect_leaves_none_behind() {
    let Some(graph) = PrivateGraph::start("probes") else {
        return;
    };
    let harness = Harness::connect(graph);
    let ids = |shared: &Shared| {
        let mut probed: Vec<u32> = shared.node_probes.keys().copied().collect();
        let mut devices: Vec<u32> = shared.devices.iter().map(|d| d.object_id).collect();
        probed.sort_unstable();
        devices.sort_unstable();
        (probed, devices)
    };
    {
        let (probed, devices) = ids(&harness.shared.borrow());
        assert_eq!(probed.len(), 3);
        assert_eq!(probed, devices);
    }

    disconnect(&mut harness.shared.borrow_mut(), "the test pulled the plug");
    assert!(
        harness.shared.borrow().node_probes.is_empty(),
        "probes bound on a dead core were kept"
    );

    connect(&harness.shared, &harness.context).expect("the daemon takes a second connection");
    assert!(harness.until("the registry, again", settled));
    let (probed, devices) = ids(&harness.shared.borrow());
    assert_eq!(
        probed, devices,
        "one probe per device of the new connection"
    );
    assert_eq!(probed.len(), 3);
}

/// A build that fails is retried on the lane's backoff, and nothing the registry says in the
/// meantime brings the retry forward: an enabled lane is asked for its rules by every device that
/// comes or goes and every default that moves, and each of those asking again during the wait
/// would be a rebuild on every tick.
#[test]
fn a_failed_build_waits_out_its_backoff_whatever_the_registry_says_meanwhile() {
    let Some(graph) = PrivateGraph::start("backoff") else {
        return;
    };
    let harness = Harness::connect(graph);
    let mut shared = harness.shared.borrow_mut();
    shared.lanes.input.enabled = true;
    shared.lanes.input.needs_rules = true;
    shared.memory.input.user_selected = "t_mic".to_owned();
    // Without its DSP the lane cannot build: `build_nodes` refuses once the rules have chosen.
    let dsp = shared.lanes.input.dsp.take();

    let start = Instant::now();
    supervise_lane(&mut shared, DeviceDirection::Input, start);
    assert_eq!(
        shared.lanes.input.attempts, 1,
        "the build was tried, and failed"
    );
    assert!(shared.lanes.input.nodes.is_none());
    let retry = shared.lanes.input.next_attempt;
    assert!(retry >= start + Duration::from_millis(200));

    // The registry has news for the lane on every tick of the wait.
    let mut tick = start;
    while tick + Duration::from_millis(50) < retry {
        tick += Duration::from_millis(50);
        shared.mark_lane_for_rules(DeviceDirection::Input);
        supervise_lane(&mut shared, DeviceDirection::Input, tick);
        assert_eq!(
            shared.lanes.input.attempts,
            1,
            "rebuilt {:?} into a {:?} wait",
            tick - start,
            retry - start
        );
    }

    // The wait over, the lane tries again — and, still without its DSP, waits twice as long.
    let failed = Instant::now();
    supervise_lane(&mut shared, DeviceDirection::Input, retry);
    assert_eq!(shared.lanes.input.attempts, 2);
    let retry = shared.lanes.input.next_attempt;
    assert!(retry >= failed + Duration::from_millis(400));

    // With its DSP back, the next try builds the pair.
    shared.lanes.input.dsp = dsp;
    supervise_lane(&mut shared, DeviceDirection::Input, retry);
    assert_eq!(
        pair_of(&shared, DeviceDirection::Input),
        Some(("t_mic".to_owned(), 2))
    );
}

/// On a server older than 0.3.68 the output lane's NODE 2 cannot be passive, and the main loop
/// paces it by hand (module docs of `engine`, "Idle"): asleep once NODE 1 has been paused for
/// [`SLEEP_AFTER`], awake the moment NODE 1 streams — on NODE 1's own word, through the wake
/// channel, without waiting for a tick.
///
/// This daemon is newer, so the engine is told it is not. And NODE 2 is left unlinked: on a server
/// this new, a running NODE 2 linked to a sink keeps its whole link-group running, NODE 1 included,
/// and NODE 1 would never be seen to pause — which is exactly why such a server gets a passive NODE
/// 2 instead.
#[test]
fn a_playback_stream_paced_by_hand_sleeps_while_its_sink_is_paused_and_wakes_the_moment_it_streams()
{
    let Some(graph) = PrivateGraph::start("pace") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so pacing by hand was not checked"
        ));
        return;
    }
    let harness = Harness::connect(graph);
    let _wake = attach_wake(harness.mainloop.loop_(), &harness.shared);
    if harness
        .meanwhile(|graph| graph.add_tone("t_tone"))
        .is_none()
    {
        skip(concat!(
            "the tone never appeared (is audiotestsrc installed?), ",
            "so pacing by hand was not checked"
        ));
        return;
    }
    {
        let mut shared = harness.shared.borrow_mut();
        shared.link_groups_scheduled.set(false);
        shared.memory.output.user_selected = "t_stereo".to_owned();
        run_rules(&mut shared, DeviceDirection::Output);
        assert_eq!(
            pace_of(&shared),
            Some(true),
            "an older server's output pair is paced by hand, and its NODE 2 starts awake"
        );
    }
    for (node, direction, positions) in [
        ("t_tone", "Output", &["MONO"][..]),
        ("t_stereo", "Input", &["FL", "FR"][..]),
        (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
        (OUTPUT_NODE_NAME, "Output", &["FL", "FR"][..]),
    ] {
        assert!(
            harness
                .meanwhile(|graph| graph.configure_ports(node, direction, positions))
                .is_some(),
            "{node} was not given ports"
        );
    }

    // Nothing plays into the sink, which has been paused since the server bound it: once it has
    // been for the whole wait, NODE 2 is put to sleep.
    assert!(
        harness.pacing_until("NODE 2 put to sleep", |shared| pace_of(shared)
            == Some(false)),
        "NODE 2 was left running under a paused sink"
    );

    // Asleep on the server's side too, not only in the bookkeeping: linked to the speakers now,
    // an active NODE 2 would run them — on this server it would run NODE 1 as well — and an
    // inactive one runs nothing.
    assert!(harness.meanwhile(|graph| graph.link_nodes(OUTPUT_NODE_NAME, "t_stereo")));
    let quiet_until = Instant::now() + SLEEP_AFTER;
    while Instant::now() < quiet_until {
        let state = harness.meanwhile(|graph| graph.node_state(OUTPUT_NODE_NAME));
        assert_ne!(
            state.as_deref(),
            Some("running"),
            "NODE 2 was told to sleep and ran as soon as it had somewhere to play"
        );
    }
    assert!(harness.meanwhile(|graph| graph.unlink_nodes(OUTPUT_NODE_NAME, "t_stereo")));

    // Something plays. From here on nothing paces NODE 2 but the wake channel: no tick runs.
    assert!(harness.meanwhile(|graph| graph.link_nodes("t_tone", SINK_NODE_NAME)));
    assert!(
        harness.until("NODE 2 woken", |shared| pace_of(shared) == Some(true)),
        "NODE 1 streamed and nothing woke NODE 2"
    );
    // And the server runs it, which it was just shown not to do while NODE 2 slept: awake again,
    // NODE 2 is part of the group the tone is running.
    assert_eq!(
        harness
            .meanwhile(|graph| graph.runs_until(OUTPUT_NODE_NAME, true))
            .map(drop),
        Ok(()),
        "NODE 2 was told to wake and the server never ran it"
    );

    // It stops, and NODE 2 goes back to sleep.
    assert!(harness.meanwhile(|graph| graph.unlink_nodes("t_tone", SINK_NODE_NAME)));
    assert!(
        harness.pacing_until("NODE 2 put back to sleep", |shared| pace_of(shared)
            == Some(false)),
        "NODE 2 was left running once the sink had nothing to play"
    );
    assert_eq!(
        harness
            .meanwhile(|graph| graph.runs_until(OUTPUT_NODE_NAME, false))
            .map(drop),
        Ok(()),
        "NODE 2 was told to sleep and the server kept running it"
    );
}

/// On a server that runs a link-group together the output lane's NODE 2 is passive, and it stops
/// in the same cycle as NODE 1 with the ring's cushion still in it (module docs of `engine`,
/// "Idle"). The next thing to wake the pair must not be heard behind that tail: NODE 1's `Paused`
/// marks the ring stale, and NODE 2's first cycle once something plays again skips what was left
/// and re-primes, as a new pair does.
///
/// This daemon runs the pair itself, so the test watches what the server does and what the ring
/// holds, and the loop it pumps is only there to hear NODE 1's states.
#[test]
fn a_passive_pair_that_stops_does_not_play_its_last_sound_to_the_next_one() {
    let Some(graph) = PrivateGraph::start("resume") else {
        return;
    };
    if let Some(missing) = ["pw-dump", "pw-cli", "pw-link"]
        .into_iter()
        .find(|tool| !installed(tool))
    {
        skip(&format!(
            "{missing} is not available, so a passive pair's resume was not checked"
        ));
        return;
    }
    let harness = Harness::connect(graph);
    if !harness.shared.borrow().link_groups_scheduled.get() {
        skip(concat!(
            "the private daemon is older than PipeWire 0.3.68 and paces NODE 2 by hand, ",
            "so a passive pair's resume was not checked"
        ));
        return;
    }
    if harness
        .meanwhile(|graph| graph.add_tone("t_tone"))
        .is_none()
    {
        skip(concat!(
            "the tone never appeared (is audiotestsrc installed?), ",
            "so a passive pair's resume was not checked"
        ));
        return;
    }
    {
        let mut shared = harness.shared.borrow_mut();
        shared.memory.output.user_selected = "t_stereo".to_owned();
        run_rules(&mut shared, DeviceDirection::Output);
        assert_eq!(
            pair_of(&shared, DeviceDirection::Output),
            Some(("t_stereo".to_owned(), 2))
        );
        assert_eq!(
            pace_of(&shared),
            None,
            "a passive NODE 2 is not paced by hand"
        );
    }
    for (node, direction, positions) in [
        ("t_tone", "Output", &["MONO"][..]),
        ("t_stereo", "Input", &["FL", "FR"][..]),
        (SINK_NODE_NAME, "Input", &["FL", "FR"][..]),
        (OUTPUT_NODE_NAME, "Output", &["FL", "FR"][..]),
    ] {
        assert!(
            harness
                .meanwhile(|graph| graph.configure_ports(node, direction, positions))
                .is_some(),
            "{node} was not given ports"
        );
    }
    assert!(harness.meanwhile(|graph| graph.link_nodes(OUTPUT_NODE_NAME, "t_stereo")));

    // A sound plays through the pair.
    assert!(harness.meanwhile(|graph| graph.link_nodes("t_tone", SINK_NODE_NAME)));
    assert_eq!(
        harness
            .meanwhile(|graph| graph.runs_until(OUTPUT_NODE_NAME, true))
            .map(drop),
        Ok(()),
        "the playback stream should run while something plays into the sink"
    );
    let ring = Arc::clone(&harness.shared.borrow().lanes.output.ring);
    assert!(
        harness.until("the ring to be primed", |_| ring
            .primed
            .load(Ordering::Relaxed)),
        "the tone never reached the playback stream"
    );
    assert!(
        !ring.stale_pending(),
        "the mark NODE 1's first Paused left should have been taken by NODE 2's first cycle"
    );

    // It stops, and so does the pair, both nodes in one cycle, with some of the sound still in
    // the ring for NODE 2 to have played.
    assert!(harness.meanwhile(|graph| graph.unlink_nodes("t_tone", SINK_NODE_NAME)));
    assert_eq!(
        harness
            .meanwhile(|graph| graph.runs_until(OUTPUT_NODE_NAME, false))
            .map(drop),
        Ok(()),
        "the playback stream should stop once nothing plays into the sink"
    );
    assert!(
        harness.until("NODE 1 to report Paused", |_| ring.stale_pending()),
        "NODE 1 paused and the ring was not marked stale"
    );
    assert!(
        ring.fill_frames() > 0,
        "the pair stopped with nothing left in the ring, so this test shows nothing"
    );
    let underruns = ring.underrun_frames.load(Ordering::Relaxed);

    // Another sound wakes the pair. Its first cycle takes the mark and plays silence while the
    // ring fills to its target; had it played the cushion, it would have been primed from the
    // start and missed nothing.
    assert!(harness.meanwhile(|graph| graph.link_nodes("t_tone", SINK_NODE_NAME)));
    assert!(
        harness.until("NODE 2's first cycle since", |_| !ring.stale_pending()),
        "the pair woke and NODE 2 never took the mark"
    );
    assert!(
        harness.until("the ring to be primed again", |_| ring
            .primed
            .load(Ordering::Relaxed)),
        "the woken pair never primed"
    );
    assert!(
        ring.underrun_frames.load(Ordering::Relaxed) > underruns,
        "the woken pair started on the last sound's cushion instead of re-priming"
    );
}
