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
use crate::graph_churn::{
    PATIENCE, PrivateGraph, canceller_missing, installed, skip, unless_skipped,
};
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

    /// [`Self::until`], with the supervisor's whole tick run on every turn of the loop — for what
    /// the supervisor does over more than one tick. The hand-back of a claim no lane stands behind
    /// is one: it waits a tick for the device list to be sent, and another after it has been taken
    /// ([`Shared::gui_has_had_its_chance`]). Nobody listens to this main loop's notifications, so
    /// the list counts as taken the moment it is sent.
    fn supervising_until(&self, what: &str, done: impl Fn(&Shared) -> bool) -> bool {
        let deadline = Instant::now() + PATIENCE;
        loop {
            supervise(&self.shared, &self.context);
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

    /// Run the supervisor's tick `ticks` times, turning the loop for a while after each, so that
    /// whatever it was going to write reaches the server and the server's answer comes back.
    fn supervise_for(&self, ticks: u32) {
        for _ in 0..ticks {
            supervise(&self.shared, &self.context);
            self.pump(Duration::from_millis(50));
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

// ---- The session default: who claims it, and when it goes back ---------------------------------

/// What the input lane's capture stream records from instead of the microphone, if anything.
fn via_of(shared: &Shared) -> Option<&'static str> {
    shared
        .lanes
        .input
        .nodes
        .as_ref()
        .and_then(|nodes| nodes.via)
}

/// The configured default of `direction`, as this main loop last heard it from the server.
fn configured_of(shared: &Shared, direction: DeviceDirection) -> Option<&str> {
    shared.defaults.get(direction).configured.as_deref()
}

impl Harness {
    /// Attach the input lane to `t_mic`, as picking it does, and wait until the server says the
    /// default source is FxSound's.
    fn attach_the_microphone(&self) {
        {
            let mut shared = self.shared.borrow_mut();
            shared.lanes.input.enabled = true;
            shared.memory.input.user_selected = "t_mic".to_owned();
            run_rules(&mut shared, DeviceDirection::Input);
            assert_eq!(
                pair_of(&shared, DeviceDirection::Input),
                Some(("t_mic".to_owned(), 2))
            );
        }
        assert!(
            self.until("the input lane's claim on the default source", |shared| {
                shared.defaults.input.holding
                    && configured_of(shared, DeviceDirection::Input) == Some(SOURCE_NODE_NAME)
            }),
            "the input lane never took the default source"
        );
    }

    /// What the user does in their sound settings: make `t_mic` itself the default source.
    fn user_picks_the_microphone_as_the_default_source(&self) {
        let written = self.meanwhile(|graph| {
            graph.write_default(
                devices::configured_default_key(DeviceDirection::Input),
                "t_mic",
            )
        });
        assert!(written.is_some(), "pw-metadata would not write the default");
        assert!(
            self.until("the user's choice to arrive", |shared| {
                !shared.defaults.input.holding
                    && configured_of(shared, DeviceDirection::Input) == Some("t_mic")
            }),
            "the user's choice of default source never reached the engine"
        );
    }

    /// Whether the server's configured default source settles on `t_mic` and stays there for a
    /// few turns of the loop — long enough for a claim the engine had issued to arrive.
    fn default_source_stays_with_the_microphone(&self) -> bool {
        self.pump(Duration::from_millis(300));
        self.graph
            .configured_default(DeviceDirection::Input)
            .as_deref()
            == Some("t_mic")
            && configured_of(&self.shared.borrow(), DeviceDirection::Input) == Some("t_mic")
    }
}

/// With echo cancellation on, choosing other speakers reloads the canceller — it listens to the
/// speakers' monitor — and the input lane's capture stream is moved off its source onto the
/// microphone, then back onto the new canceller's source a tick or two later. Both moves rebuild
/// the input lane's pair on the microphone it was already on. Neither is the input lane attaching
/// to anything, and neither may take back a default source the user had moved away from FxSound —
/// which is what they did in 0.4.0's first cut, from a click on the *speakers*.
#[test]
fn a_capture_stream_the_echo_canceller_moves_leaves_the_users_default_source_alone() {
    if let Some(missing) = canceller_missing() {
        skip(&format!(
            "{missing}, so the echo canceller's rebuilds of the capture stream were not checked"
        ));
        return;
    }
    if !installed("pw-metadata") {
        skip("pw-metadata is not available, so the user's default source could not be chosen");
        return;
    }
    let Some(graph) = PrivateGraph::start("aecclaim") else {
        return;
    };
    let harness = Harness::connect(graph);
    {
        let mut shared = harness.shared.borrow_mut();
        shared.aec = EchoCancel::new(aec::NULL_LIBRARY);
        shared.aec.set_on(true);
        shared.memory.output.user_selected = "t_stereo".to_owned();
        run_rules(&mut shared, DeviceDirection::Output);
    }
    harness.attach_the_microphone();

    // The supervisor's echo-cancellation step, as its tick runs it.
    let tick = || {
        let mut shared = harness.shared.borrow_mut();
        reconcile_echo_cancel(&mut shared, Some(&harness.context));
        if shared.aec.route_moved() {
            run_rules(&mut shared, DeviceDirection::Input);
        }
    };
    let onto_the_canceller = |what: &str| {
        tick();
        assert!(
            harness.until(what, |shared| shared.aec.running()),
            "the canceller never ran"
        );
        tick();
        assert_eq!(
            via_of(&harness.shared.borrow()),
            Some(AEC_SOURCE_NODE_NAME),
            "the capture stream did not move onto the canceller's source"
        );
    };
    onto_the_canceller("the canceller's source");

    harness.user_picks_the_microphone_as_the_default_source();

    // Other speakers. The canceller that listened to the old ones goes in the same call, and the
    // capture stream goes back to the microphone with it.
    control(
        &mut harness.shared.borrow_mut(),
        UiToAudio::SelectDevice {
            node_name: "t_71".to_owned(),
            direction: DeviceDirection::Output,
        },
    );
    {
        let shared = harness.shared.borrow();
        assert_eq!(
            pair_of(&shared, DeviceDirection::Output).map(|p| p.0),
            Some("t_71".to_owned())
        );
        assert_eq!(
            pair_of(&shared, DeviceDirection::Input).map(|p| p.0),
            Some("t_mic".to_owned())
        );
        assert_eq!(
            via_of(&shared),
            None,
            "the stream was left on a canceller that has gone"
        );
        assert!(
            !shared.defaults.input.holding,
            "choosing other speakers took the default source back from the user"
        );
    }
    // A canceller for the new speakers, and the stream back onto its source.
    onto_the_canceller("the new canceller's source");
    assert!(!harness.shared.borrow().defaults.input.holding);
    assert!(
        harness.default_source_stays_with_the_microphone(),
        "the server's default source is {:?}, not the microphone the user chose",
        harness.graph.configured_default(DeviceDirection::Input)
    );
}

/// A pair that fails is rebuilt on the same device after its backoff, and that rebuild is a repair
/// of the pair, not a claim on the default: a default the user moved away from FxSound while the
/// pair was up stays where they put it.
#[test]
fn a_pair_rebuilt_on_its_own_device_after_a_failure_leaves_the_users_default_alone() {
    if !installed("pw-metadata") {
        skip("pw-metadata is not available, so the user's default source could not be chosen");
        return;
    }
    let Some(graph) = PrivateGraph::start("repair") else {
        return;
    };
    let harness = Harness::connect(graph);
    harness.attach_the_microphone();
    harness.user_picks_the_microphone_as_the_default_source();

    {
        let mut shared = harness.shared.borrow_mut();
        shared
            .lanes
            .input
            .status
            .sink_error
            .store(true, Ordering::Relaxed);
        supervise_lane(&mut shared, DeviceDirection::Input, Instant::now());
        assert_eq!(
            pair_of(&shared, DeviceDirection::Input),
            None,
            "the failed pair went"
        );
        let retry = shared.lanes.input.next_attempt;
        supervise_lane(&mut shared, DeviceDirection::Input, retry);
        assert_eq!(
            pair_of(&shared, DeviceDirection::Input),
            Some(("t_mic".to_owned(), 2)),
            "the pair came back on its microphone"
        );
        assert!(
            !shared.defaults.input.holding,
            "rebuilding the pair took the default source back from the user"
        );
    }
    assert!(harness.default_source_stays_with_the_microphone());
}

/// A lane detached while the connection is down cannot hand its default back then — the
/// connection is gone, and with it what was known about the defaults — and the key still names
/// FxSound's source when the connection returns. The reconnect's supervisor hands it back to the
/// microphone, once the device list has gone to the GUI, rather than adopting a claim no lane
/// stands behind for the rest of the session.
#[test]
fn a_lane_detached_while_the_connection_was_down_hands_back_the_claim_the_reconnect_finds() {
    let Some(graph) = PrivateGraph::start("detachdown") else {
        return;
    };
    let harness = Harness::connect(graph);
    harness.attach_the_microphone();

    disconnect(&mut harness.shared.borrow_mut(), "the test pulled the plug");
    control(
        &mut harness.shared.borrow_mut(),
        UiToAudio::DetachLane(DeviceDirection::Input),
    );
    connect(&harness.shared, &harness.context).expect("the daemon takes a second connection");
    assert!(
        harness.until("the registry and the default source, again", |shared| {
            settled(shared) && shared.defaults.input.holding
        }),
        "the reconnect never read the default source back"
    );

    assert!(
        harness.supervising_until(
            "the default source to go back to the microphone",
            |shared| { configured_of(shared, DeviceDirection::Input) == Some("t_mic") }
        ),
        "the default source still names a node no lane will build: {:?}",
        configured_of(&harness.shared.borrow(), DeviceDirection::Input)
    );
    let shared = harness.shared.borrow();
    assert!(!shared.lanes.input.enabled);
    assert_eq!(pair_of(&shared, DeviceDirection::Input), None);
}

/// [`UiToAudio::SetAsDefault`] with `want: false`, sent while the connection is down: the same
/// hand-back on the reconnect, and the lane itself comes back without the default.
#[test]
fn an_opt_out_sent_while_the_connection_was_down_is_honoured_when_it_returns() {
    let Some(graph) = PrivateGraph::start("optoutdown") else {
        return;
    };
    let harness = Harness::connect(graph);
    harness.attach_the_microphone();

    disconnect(&mut harness.shared.borrow_mut(), "the test pulled the plug");
    control(
        &mut harness.shared.borrow_mut(),
        UiToAudio::SetAsDefault {
            direction: DeviceDirection::Input,
            want: false,
        },
    );
    connect(&harness.shared, &harness.context).expect("the daemon takes a second connection");
    assert!(
        harness.until("the registry and the default source, again", |shared| {
            settled(shared) && shared.defaults.input.holding
        }),
        "the reconnect never read the default source back"
    );

    assert!(
        harness.supervising_until(
            "the default source to go back to the microphone",
            |shared| { configured_of(shared, DeviceDirection::Input) == Some("t_mic") }
        ),
        "the opt-out was ignored for the session: the default source is {:?}",
        configured_of(&harness.shared.borrow(), DeviceDirection::Input)
    );
    let shared = harness.shared.borrow();
    assert_eq!(
        pair_of(&shared, DeviceDirection::Input),
        Some(("t_mic".to_owned(), 2)),
        "the lane is still attached; it only stopped being the default"
    );
    assert!(!shared.defaults.input.holding);
}

/// The registry's first dump says there is a `default` metadata object; what it holds is sent a
/// round trip later, in answer to the bind the dump called for. Nothing may choose a device in
/// between ([`Barrier`]): by the first moment the rules may run after a connect, the default
/// source the server holds has been read.
#[test]
fn the_rules_wait_for_the_session_defaults_after_a_connect() {
    let Some(graph) = PrivateGraph::start("barrier") else {
        return;
    };
    let harness = Harness::connect(graph);
    harness.attach_the_microphone();

    disconnect(&mut harness.shared.borrow_mut(), "the test pulled the plug");
    connect(&harness.shared, &harness.context).expect("the daemon takes a second connection");
    assert!(
        harness.until("the rules to be allowed to run", Shared::ready),
        "the reconnect never let the rules run"
    );
    let shared = harness.shared.borrow();
    assert_eq!(
        configured_of(&shared, DeviceDirection::Input),
        Some(SOURCE_NODE_NAME),
        "the rules were allowed to run before the default source was read"
    );
    assert!(shared.defaults.input.holding);
}

/// [`an_opt_out_sent_while_the_connection_was_down_is_honoured_when_it_returns`], with the
/// supervisor ticking from the moment the connection returns rather than once the default source
/// has been read back. A tick between the registry's `done` and the `default` object's keys built
/// the input lane's pair first; the key then read as the user's own pick of a node that was there,
/// and the opt-out was ignored for the session.
#[test]
fn an_opt_out_sent_while_the_connection_was_down_survives_a_tick_before_the_defaults_are_read() {
    let Some(graph) = PrivateGraph::start("optouttick") else {
        return;
    };
    let harness = Harness::connect(graph);
    harness.attach_the_microphone();

    disconnect(&mut harness.shared.borrow_mut(), "the test pulled the plug");
    control(
        &mut harness.shared.borrow_mut(),
        UiToAudio::SetAsDefault {
            direction: DeviceDirection::Input,
            want: false,
        },
    );
    connect(&harness.shared, &harness.context).expect("the daemon takes a second connection");

    assert!(
        harness.supervising_until(
            "the default source to go back to the microphone",
            |shared| { configured_of(shared, DeviceDirection::Input) == Some("t_mic") }
        ),
        "the opt-out was ignored for the session: the default source is {:?}",
        configured_of(&harness.shared.borrow(), DeviceDirection::Input)
    );
    assert!(
        harness.supervising_until("the input lane's pair to come back", |shared| {
            pair_of(shared, DeviceDirection::Input).is_some()
        }),
        "the reconnect never rebuilt the input lane"
    );
    let shared = harness.shared.borrow();
    assert_eq!(
        pair_of(&shared, DeviceDirection::Input),
        Some(("t_mic".to_owned(), 2)),
        "the lane is still attached; it only stopped being the default"
    );
    assert!(!shared.defaults.input.holding);
}

/// An attached lane that has opted out of the default, and a user who made FxSound's source the
/// default in their sound settings all the same. The connection goes and comes back. The reconnect
/// reads the key before the pair is rebuilt, when there is no node of ours for anyone to have
/// picked — and it is still the user's pick, and stays theirs however many ticks go by.
#[test]
fn a_default_the_user_gave_an_opted_out_lane_by_hand_survives_a_reconnect() {
    if !installed("pw-metadata") {
        skip("pw-metadata is not available, so the user's default source could not be chosen");
        return;
    }
    let Some(graph) = PrivateGraph::start("byhand") else {
        return;
    };
    let harness = Harness::connect(graph);
    harness.attach_the_microphone();
    control(
        &mut harness.shared.borrow_mut(),
        UiToAudio::SetAsDefault {
            direction: DeviceDirection::Input,
            want: false,
        },
    );
    assert!(
        harness.until("the opt-out's hand-back", |shared| {
            configured_of(shared, DeviceDirection::Input) == Some("t_mic")
        }),
        "opting out never handed the default source back"
    );

    // In their sound settings, the user picks FxSound's source anyway.
    let written = harness.meanwhile(|graph| {
        graph.write_default(
            devices::configured_default_key(DeviceDirection::Input),
            SOURCE_NODE_NAME,
        )
    });
    assert!(written.is_some(), "pw-metadata would not write the default");
    assert!(
        harness.until("the user's choice to arrive", |shared| {
            shared.defaults.input.holding && !shared.defaults.input.disowned
        }),
        "the user's choice of default source never reached the engine"
    );

    disconnect(&mut harness.shared.borrow_mut(), "the test pulled the plug");
    connect(&harness.shared, &harness.context).expect("the daemon takes a second connection");
    assert!(
        harness.until("the registry and the default source, again", |shared| {
            settled(shared) && shared.defaults.input.holding
        }),
        "the reconnect never read the default source back"
    );
    assert!(
        harness.supervising_until("the input lane's pair to come back", |shared| {
            pair_of(shared, DeviceDirection::Input).is_some()
        }),
        "the reconnect never rebuilt the input lane"
    );
    // Well past the ticks a hand-back waits for.
    harness.supervise_for(5);
    assert_eq!(
        harness
            .graph
            .configured_default(DeviceDirection::Input)
            .as_deref(),
        Some(SOURCE_NODE_NAME),
        "the reconnect undid the default source the user picked"
    );
    let shared = harness.shared.borrow();
    assert!(shared.defaults.input.holding);
    assert!(
        !shared.lanes.input.want_default,
        "and the lane is still opted out"
    );
}

/// Wait, without turning this main loop, until the server no longer lists the node called `name`.
/// Whether it came to.
fn gone_from_the_server(graph: &PrivateGraph, name: &str) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while graph.node_id(name).is_some() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// A running application is known by its stream's properties and its client's, each through a
/// probe of its own, and both probes go with it: dropped at once when the server removed them
/// before the registry said the objects had gone, which is the order it keeps for a bind that
/// succeeded.
#[test]
fn a_running_stream_is_known_through_its_own_probe_and_its_clients_and_both_go_with_it() {
    let Some(graph) = PrivateGraph::start("appprobe") else {
        return;
    };
    if !installed("pw-cat") {
        skip("pw-cat is not installed, so the probes of application streams were not checked");
        return;
    }
    let harness = Harness::connect(graph);
    let app = harness
        .meanwhile(|graph| graph.pw_cat("--playback", "t_player", "application.name = Player", &[]))
        .expect("pw-cat should play");
    assert!(
        harness.until("the player, complete", |shared| {
            shared
                .apps
                .report()
                .iter()
                .any(|stream| stream.app.name == "Player")
        }),
        "the player was never known well enough to report"
    );
    let (stream_id, client_id) = {
        let shared = harness.shared.borrow();
        let listed = shared.apps.report();
        let player = listed
            .iter()
            .find(|stream| stream.app.name == "Player")
            .expect("listed");
        assert_eq!(
            player.app.binary, "pw-cat",
            "a stream that names no binary is known by its client's"
        );
        let client = shared
            .apps
            .stream(player.id)
            .and_then(|stream| stream.client)
            .expect("a stream names its client");
        assert!(shared.app_probes.contains_key(&player.id));
        assert!(shared.app_probes.contains_key(&client));
        (player.id, client)
    };

    drop(app);
    assert!(
        harness.until("the player gone", |shared| {
            shared.apps.stream(stream_id).is_none() && !shared.app_probes.contains_key(&client_id)
        }),
        "the player's stream or client was kept after it exited"
    );
    let shared = harness.shared.borrow();
    assert!(!shared.app_probes.contains_key(&stream_id));
    assert!(
        shared.retired_probes.is_empty(),
        "a probe the server had removed was kept"
    );
    assert!(!shared.restart_requested);
}

/// A client or a stream that has gone before the main loop binds it — a `pw-dump`, a sound played
/// for a moment — must not cost the connection. The bind fails, and the probe's proxy names an
/// object the server never made: dropped as the registry says the object went, it would send the
/// server a `destroy` it answers with a core error, and a core error restarts everything. So such a
/// probe is retired until the server has removed it, and dropped on the next tick.
#[test]
fn a_stream_and_a_client_gone_before_the_main_loop_bound_them_leave_the_connection_alone() {
    let Some(graph) = PrivateGraph::start("appsrace") else {
        return;
    };
    if !installed("pw-cat") {
        skip("pw-cat is not installed, so the probes of application streams were not checked");
        return;
    }
    let harness = Harness::connect(graph);

    // Out of this loop's sight: a player comes and goes, and so does every `pw-dump` that watched
    // it. The loop hears of all of it at once — each object announced, then gone — with its binds
    // still to reach the server.
    let blink = harness
        .graph
        .pw_cat("--playback", "t_blink", "application.name = Blink", &[])
        .expect("pw-cat should play");
    drop(blink);
    assert!(
        gone_from_the_server(&harness.graph, "t_blink"),
        "the player never went"
    );
    harness.pump(Duration::from_millis(500));
    {
        let shared = harness.shared.borrow();
        assert!(
            !shared.restart_requested,
            "a probe of an object already gone cost the connection"
        );
        assert!(
            !shared.retired_probes.is_empty(),
            "nothing was bound after it had gone, so this test tested nothing"
        );
        assert!(
            shared
                .retired_probes
                .iter()
                .all(|probe| probe.removed.get()),
            "the server removed every failed bind within half a second"
        );
        assert!(shared.apps.report().is_empty());
    }

    harness.supervise_for(1);
    let shared = harness.shared.borrow();
    assert!(
        shared.retired_probes.is_empty(),
        "the tick keeps retired probes the server has removed"
    );
    assert!(shared.session.is_some(), "the connection was restarted");
    assert!(!shared.restart_requested);
}
