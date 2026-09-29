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
    PATIENCE, PrivateGraph, again_if_a_tone_ran_dry, canceller_missing, installed, note, skip,
    unless_skipped,
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
        Self::connect_with_journal(graph, None)
    }

    /// [`Self::connect`], with the handover's journal kept at `journal` ([`fades::Fades::new`]).
    fn connect_with_journal(graph: PrivateGraph, journal: Option<std::path::PathBuf>) -> Self {
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
        shared.borrow_mut().fades = fades::Fades::new(journal);
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

    /// [`Self::until`], with the handover driven on every turn of the loop, as its clock would
    /// drive it ([`fades::drive`]).
    fn driving_until(&self, what: &str, done: impl Fn(&Shared) -> bool) -> bool {
        let deadline = Instant::now() + PATIENCE;
        loop {
            {
                let mut shared = self.shared.borrow_mut();
                fades::drive(&mut shared);
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
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(5)));
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
    again_if_a_tone_ran_dry(|| {
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
    });
}

/// The first PipeWire known to stop a passive pair with the ring's cushion still in it every time
/// its last sound goes: 1.6.9 does, on this machine and in CI's Arch leg. 1.0.5, on CI's Ubuntu
/// 24.04 leg, mostly does too, but was seen to stop it with the ring played dry (CI run
/// 36446837778) — where the test's tone has to drive the group as well
/// ([`PrivateGraph::add_tone`]), so which of the two makes the difference is not known. Nothing
/// between was tried.
const A_PASSIVE_PAIR_STOPS_AT_ONCE_SINCE: (u32, u32, u32) = (1, 6, 0);

/// How many times, on a server older than [`A_PASSIVE_PAIR_STOPS_AT_ONCE_SINCE`], a sound is
/// played and stopped to have the pair stop with a tail in the ring. Each start is a link made
/// into a group a tone drives, which runs the tone dry now and then ([`again_if_a_tone_ran_dry`]),
/// so no more than it takes.
const ROUNDS_ON_AN_OLDER_SERVER: u32 = 3;

/// On a server that runs a link-group together the output lane's NODE 2 is passive, and it stops
/// in the same cycle as NODE 1 with the ring's cushion still in it (module docs of `engine`,
/// "Idle"). The next thing to wake the pair must not be heard behind that tail: NODE 1's `Paused`
/// marks the ring stale, and NODE 2's first cycle once something plays again skips what was left
/// and re-primes, as a new pair does.
///
/// This daemon runs the pair itself, so the test watches what the server does and what the ring
/// holds, and the loop it pumps is only there to hear NODE 1's states.
///
/// Whether the pair stops with a tail in the ring is the server's doing, and a server older than
/// [`A_PASSIVE_PAIR_STOPS_AT_ONCE_SINCE`] does not always leave one: there the sound is played and
/// stopped up to [`ROUNDS_ON_AN_OLDER_SERVER`] times until a stop does, and when none does, the
/// test says so and still checks that the next sound takes the mark and primes afresh. On a newer
/// server the first stop has to leave one.
#[test]
fn a_passive_pair_that_stops_does_not_play_its_last_sound_to_the_next_one() {
    again_if_a_tone_ran_dry(|| {
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
        let ring = Arc::clone(&harness.shared.borrow().lanes.output.ring);
        let counters = Arc::clone(&harness.shared.borrow().lanes.output.counters);
        let version = harness.meanwhile(PrivateGraph::server_version);
        // A server whose version cannot be read is held to the newest one's standard.
        let stops_at_once =
            version.is_none_or(|version| version >= A_PASSIVE_PAIR_STOPS_AT_ONCE_SINCE);
        let rounds = if stops_at_once {
            1
        } else {
            ROUNDS_ON_AN_OLDER_SERVER
        };

        let mut left_a_tail = false;
        for round in 1..=rounds {
            // A sound plays through the pair. From the second round on it is also the next sound
            // after a pair that stopped: it wakes the pair as well, and takes the mark too.
            assert!(harness.meanwhile(|graph| graph.link_nodes("t_tone", SINK_NODE_NAME)));
            assert_eq!(
                harness
                    .meanwhile(|graph| graph.runs_until(OUTPUT_NODE_NAME, true))
                    .map(drop),
                Ok(()),
                "the playback stream should run while something plays into the sink"
            );
            // NODE 2's first cycle takes the mark before anything else is asked. A stop that
            // drained the ring exactly leaves `primed` set from the round before, so waiting for
            // it alone could return before NODE 2 has run since the wake.
            assert!(
                harness.until("NODE 2's first cycle since", |_| !ring.stale_pending()),
                "the mark NODE 1's last Paused left should have been taken by NODE 2's first cycle"
            );
            assert!(
                harness.until("the ring to be primed", |_| ring
                    .primed
                    .load(Ordering::Relaxed)),
                "the tone never reached the playback stream"
            );

            // It stops, and so does the pair, both nodes in one cycle, with some of the sound still
            // in the ring for NODE 2 to have played.
            let sink_cycles = counters.sink_cycles.load(Ordering::Relaxed);
            let output_cycles = counters.output_cycles.load(Ordering::Relaxed);
            let underruns = ring.underrun_frames.load(Ordering::Relaxed);
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
            if ring.fill_frames() > 0 {
                left_a_tail = true;
                break;
            }
            // What the server did instead, for the log: how many blocks NODE 1 still processed
            // after the sound was unlinked, and how many cycles NODE 2 ran and played short.
            note(&format!(
                "round {round}: the pair stopped with nothing left in the ring; after the sound \
                 was unlinked NODE 1 processed {} blocks and NODE 2 ran {} cycles, {} frames of \
                 them silence",
                counters.sink_cycles.load(Ordering::Relaxed) - sink_cycles,
                counters.output_cycles.load(Ordering::Relaxed) - output_cycles,
                ring.underrun_frames.load(Ordering::Relaxed) - underruns,
            ));
        }
        if !left_a_tail {
            assert!(
                !stops_at_once,
                "the pair stopped with nothing left in the ring, so this test shows nothing"
            );
            note(&format!(
                "PipeWire {version:?}, older than {A_PASSIVE_PAIR_STOPS_AT_ONCE_SINCE:?}, played \
                 the ring dry before it stopped the pair in all {rounds} rounds, so there was no \
                 last sound to leave out; that the next sound takes NODE 1's mark and primes \
                 afresh is still checked"
            ));
        }
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
    });
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

// ---------------------------------------------------------------------------------------------
// The handover (`fades`, `crate::stream_handover`)
// ---------------------------------------------------------------------------------------------

/// The key WirePlumber keeps a `pw-cat` player called `name` under ([`stream_handover::state_key`]).
fn player_key(name: &str) -> String {
    format!("Output/Audio:application.name:{name}")
}

/// A `pw-cat` player called `node_name`, of the application `name`, linked to `t_stereo` and
/// playing there, and the engine's registry id for it, once the engine has its volume. `None`, said
/// why, when the graph cannot show it: no `pw-cat`, or a server without the ramp.
fn playing_player(
    harness: &Harness,
    node_name: &str,
    name: &str,
) -> Option<(crate::graph_churn::apps::App, u32)> {
    if !installed("pw-cat") || !installed("pw-link") {
        skip("pw-cat or pw-link is not installed, so the handover was not checked");
        return None;
    }
    if !harness.shared.borrow().volume_ramps.get() {
        note("this PipeWire does not ramp a stream's volume, so there is no fade to check");
        return None;
    }
    let player = harness
        .graph
        .pw_cat(
            "--playback",
            node_name,
            &format!("application.name = {name}"),
            &[],
        )
        .expect("pw-cat should play");
    for (node, direction, positions) in [
        (node_name, "Output", &["FL", "FR"][..]),
        ("t_stereo", "Input", &["FL", "FR"][..]),
    ] {
        assert!(
            harness
                .graph
                .configure_ports(node, direction, positions)
                .is_some(),
            "{node} was not given ports"
        );
    }
    assert!(
        harness.graph.link_nodes(node_name, "t_stereo"),
        "the player could not be linked"
    );
    let id = u32::try_from(harness.graph.node_id(node_name).expect("the player's node"))
        .expect("a registry id");
    assert!(
        harness.until("the player playing at its volume", |shared| {
            shared.fades.watched(id).is_some_and(|watched| {
                watched.running && watched.level == Some(1.0) && watched.key.is_some()
            })
        }),
        "the engine never learned the player's volume, state and key"
    );
    Some((player, id))
}

/// Hand the stream under `id` over with a move that notes the stream's master volume, as the engine
/// knows it, at the moment it is made — `Some(level)` once made.
fn hand_over_noting(harness: &Harness, id: u32) -> Rc<Cell<Option<Option<f32>>>> {
    let seen = Rc::new(Cell::new(None));
    let noted = Rc::clone(&seen);
    let mut shared = harness.shared.borrow_mut();
    fades::hand_over(
        &mut shared,
        vec![id],
        Box::new(move |shared: &mut Shared| {
            noted.set(Some(
                shared.fades.watched(id).and_then(|watched| watched.level),
            ));
            true
        }),
    );
    assert!(
        shared.fades.busy(),
        "a playing stream is faded, not moved at once"
    );
    seen
}

/// The primitive, through the third connection, against a real stream: faded to 0 over the ramp,
/// moved only once the server says it is at 0, journalled in between, and given its volume back
/// when no new link comes within the wait.
#[test]
fn a_playing_stream_is_silent_for_its_move_and_gets_its_volume_back() {
    let Some(graph) = PrivateGraph::start("handover") else {
        return;
    };
    let dir = fxsound_core::test_support::ScratchDir::new("handover-live");
    let journal = dir
        .path()
        .join("fxsound")
        .join(stream_handover::JOURNAL_FILE);
    let harness = Harness::connect_with_journal(graph, Some(journal.clone()));
    let Some((_player, id)) = playing_player(&harness, "t_player", "Player") else {
        return;
    };

    let moved = hand_over_noting(&harness, id);
    assert_eq!(
        stream_handover::Journal::load(&journal).repair(&player_key("Player"), Some(0.0)),
        stream_handover::Repair::Restore(1.0),
        "the volume is on disk before the fade is written"
    );
    assert!(
        harness.driving_until("the move", |_| moved.get().is_some()),
        "the move was never made"
    );
    assert_eq!(
        moved.get(),
        Some(Some(0.0)),
        "the move was made before the server said the stream was at 0"
    );
    if let Some(level) = unless_skipped(
        harness.graph.node_volume("t_player"),
        "pw-dump",
        "the faded stream's volume",
    ) {
        assert_eq!(level, Some(0.0));
    }

    let moved_at = Instant::now();
    assert!(
        harness.driving_until("the handover's end", |shared| !shared.fades.busy()),
        "the stream never got its volume back"
    );
    assert!(
        moved_at.elapsed() >= stream_handover::LINK_WAIT - Duration::from_millis(50),
        "with no new link, the volume came back before the wait was up"
    );
    assert_eq!(
        harness
            .shared
            .borrow()
            .fades
            .watched(id)
            .and_then(|watched| watched.level),
        Some(1.0)
    );
    if let Some(level) = unless_skipped(
        harness.graph.node_volume("t_player"),
        "pw-dump",
        "the stream's volume after the handover",
    ) {
        assert_eq!(level, Some(1.0));
    }
    assert!(
        harness.shared.borrow().fades.journal_is_empty() && !journal.exists(),
        "the journal kept a volume that is back"
    );
}

/// A moved stream's new link ends its move: its volume comes back then, not at the end of the wait
/// — the new link WirePlumber makes, made here by hand.
#[test]
fn a_new_link_after_the_move_gives_the_volume_back_at_once() {
    let Some(graph) = PrivateGraph::start("handover-link") else {
        return;
    };
    let harness = Harness::connect(graph);
    let Some((_player, id)) = playing_player(&harness, "t_mover", "Mover") else {
        return;
    };

    assert!(
        harness
            .graph
            .configure_ports(
                "t_71",
                "Input",
                &["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"]
            )
            .is_some(),
        "t_71 was not given ports"
    );

    let moved = hand_over_noting(&harness, id);
    assert!(harness.driving_until("the move", |_| moved.get().is_some()));
    let moved_at = Instant::now();
    assert!(harness.graph.unlink_nodes("t_mover", "t_stereo"));
    assert!(harness.graph.link_nodes("t_mover", "t_71"));
    assert!(
        harness.driving_until("the handover's end", |shared| !shared.fades.busy()),
        "the stream never got its volume back"
    );
    assert!(
        moved_at.elapsed() < stream_handover::LINK_WAIT,
        "the new link did not end the move: its volume came back after {:?}",
        moved_at.elapsed()
    );
    assert_eq!(
        harness
            .shared
            .borrow()
            .fades
            .watched(id)
            .and_then(|watched| watched.level),
        Some(1.0)
    );
}

/// A volume a mixer writes while the stream is faded is the mixer's: the handover writes nothing
/// back over it.
#[test]
fn a_volume_written_during_the_handover_is_kept() {
    let Some(graph) = PrivateGraph::start("handover-mixer") else {
        return;
    };
    let harness = Harness::connect(graph);
    let Some((_player, id)) = playing_player(&harness, "t_mixed", "Mixed") else {
        return;
    };

    let moved = hand_over_noting(&harness, id);
    assert!(harness.driving_until("the move", |_| moved.get().is_some()));
    if unless_skipped(
        harness.graph.set_node_volume("t_mixed", 0.3),
        "pw-cli",
        "a mixer's volume",
    )
    .is_none()
    {
        return;
    }
    assert!(
        harness.driving_until("the handover's end", |shared| !shared.fades.busy()),
        "the handover never let the stream go"
    );
    harness.pump(stream_handover::LINK_WAIT);
    let level = harness
        .shared
        .borrow()
        .fades
        .watched(id)
        .and_then(|watched| watched.level)
        .expect("the stream's volume");
    assert!(
        (level - 0.3).abs() < 1e-4,
        "the mixer's 0.3 was overwritten with {level}"
    );
}

/// The way out mid-handover: a stream left at 0 gets its volume back before the connection goes.
#[test]
fn the_way_out_gives_a_silent_stream_its_volume_back() {
    let Some(graph) = PrivateGraph::start("handover-exit") else {
        return;
    };
    let harness = Harness::connect(graph);
    let Some((_player, id)) = playing_player(&harness, "t_leaver", "Leaver") else {
        return;
    };

    let moved = hand_over_noting(&harness, id);
    assert!(harness.driving_until("the move", |_| moved.get().is_some()));
    fades::restore_before_exit(&harness.shared, harness.mainloop.loop_());
    assert!(!harness.shared.borrow().fades.busy());
    if let Some(level) = unless_skipped(
        harness.graph.node_volume("t_leaver"),
        "pw-dump",
        "the stream's volume on the way out",
    ) {
        assert_eq!(level, Some(1.0), "the stream was left at 0");
    }
}

/// A run killed in the middle of a handover leaves a stream at 0 and its volume in the journal: the
/// next run puts it back when it meets the stream, and empties the journal.
#[test]
fn a_stream_a_killed_run_left_at_zero_gets_its_volume_back_from_the_journal() {
    if !installed("pw-cat") || !installed("pw-cli") {
        skip("pw-cat or pw-cli is not installed, so the journal was not checked");
        return;
    }
    let Some(graph) = PrivateGraph::start("handover-journal") else {
        return;
    };
    let player = graph
        .pw_cat(
            "--playback",
            "t_survivor",
            "application.name = Survivor",
            &[],
        )
        .expect("pw-cat should play");
    if unless_skipped(
        graph.set_node_volume("t_survivor", 0.0),
        "pw-cli",
        "the volume a killed run left",
    )
    .is_none()
    {
        return;
    }
    let dir = fxsound_core::test_support::ScratchDir::new("handover-repair");
    let journal = dir
        .path()
        .join("fxsound")
        .join(stream_handover::JOURNAL_FILE);
    let mut lines = stream_handover::Journal::default();
    lines.remember(&player_key("Survivor"), Some(1), 0.7);
    lines.save(&journal).expect("the journal should be written");

    let harness = Harness::connect_with_journal(graph, Some(journal.clone()));
    assert!(
        harness.until("the volume from the journal", |shared| {
            shared.fades.journal_is_empty()
                && shared
                    .fades
                    .levels()
                    .any(|level| (level - 0.7).abs() < 1e-4)
        }),
        "the stream was left at 0, or the journal kept its line"
    );
    assert!(!journal.exists(), "the emptied journal is still on disk");
    if let Some(level) = unless_skipped(
        harness.graph.node_volume("t_survivor"),
        "pw-dump",
        "the repaired stream's volume",
    ) {
        assert!(
            level.is_some_and(|level| (level - 0.7).abs() < 1e-4),
            "{level:?}"
        );
    }
    drop(player);
}

/// A killed run left a *playing* stream at 0: the repair that puts its volume back is a ramp, not a
/// step from silence to its volume in the middle of a wave. On the first connection of the run —
/// before any handover has begun, which is when a repair comes.
#[test]
fn a_playing_stream_a_killed_run_left_at_zero_gets_its_volume_back_over_the_ramp() {
    if !installed("pw-cat") || !installed("pw-cli") || !installed("pw-link") {
        skip("pw-cat, pw-cli or pw-link is not installed, so the journal's ramp was not checked");
        return;
    }
    let Some(graph) = PrivateGraph::start("handover-journal-ramp") else {
        return;
    };
    let _player = graph
        .pw_cat("--playback", "t_playing", "application.name = Playing", &[])
        .expect("pw-cat should play");
    for node in ["t_playing", "t_stereo"] {
        let direction = if node == "t_stereo" {
            "Input"
        } else {
            "Output"
        };
        assert!(
            graph
                .configure_ports(node, direction, &["FL", "FR"])
                .is_some(),
            "{node} was not given ports"
        );
    }
    assert!(
        graph.link_nodes("t_playing", "t_stereo"),
        "the player could not be linked"
    );
    assert!(
        graph.runs_until("t_playing", true).is_ok(),
        "the linked player never ran"
    );
    if unless_skipped(
        graph.set_node_volume("t_playing", 0.0),
        "pw-cli",
        "the volume a killed run left",
    )
    .is_none()
    {
        return;
    }
    let dir = fxsound_core::test_support::ScratchDir::new("handover-repair-ramp");
    let journal = dir
        .path()
        .join("fxsound")
        .join(stream_handover::JOURNAL_FILE);
    let mut lines = stream_handover::Journal::default();
    lines.remember(&player_key("Playing"), None, 0.7);
    lines.save(&journal).expect("the journal should be written");

    let harness = Harness::connect_with_journal(graph, Some(journal));
    let id = u32::try_from(
        harness
            .graph
            .node_id("t_playing")
            .expect("the player's node"),
    )
    .expect("a registry id");
    assert!(
        harness.until("the repair's write", |shared| {
            shared
                .fades
                .writes()
                .iter()
                .any(|&(written, ..)| written == id)
        }),
        "the stream a killed run left at 0 was never given its volume back"
    );
    let shared = harness.shared.borrow();
    if !shared.volume_ramps.get() {
        note("this PipeWire does not ramp a stream's volume, so there is no ramp to check");
        return;
    }
    assert!(
        shared
            .fades
            .watched(id)
            .is_some_and(|watched| watched.running),
        "the player was not playing when it was repaired, so the test shows nothing"
    );
    let writes: Vec<(f32, i32)> = shared
        .fades
        .writes()
        .iter()
        .filter(|&&(written, ..)| written == id)
        .map(|&(_, level, ramp)| (level, ramp))
        .collect();
    // The repair, and — once its ramp has been played — the same volume said once more, at once.
    let [(level, ramp), ref settled @ ..] = writes[..] else {
        panic!("a write was expected for the repair, not {writes:?}");
    };
    assert!(
        settled
            .iter()
            .all(|&(again, ramp)| (again - level).abs() < 1e-6 && ramp == 0),
        "{writes:?}"
    );
    assert!(
        (level - 0.7).abs() < 1e-4,
        "the journal said 0.7, not {level}"
    );
    assert_eq!(
        ramp,
        stream_handover::RAMP_IN_MS,
        "a playing stream was put back from silence to its volume in one step"
    );
}

/// The journal's line for an application is the faded stream's while the handover holds it at 0.
/// Another stream of the same application met meanwhile — a second tab at a volume of its own, or
/// one at 0 that WirePlumber gave the 0 it kept for the first — does not take it out: a run killed
/// before the faded stream is back must still find it.
#[test]
fn a_second_stream_of_a_faded_application_does_not_take_its_line_out_of_the_journal() {
    let Some(graph) = PrivateGraph::start("handover-sibling") else {
        return;
    };
    let dir = fxsound_core::test_support::ScratchDir::new("handover-sibling");
    let journal = dir
        .path()
        .join("fxsound")
        .join(stream_handover::JOURNAL_FILE);
    let harness = Harness::connect_with_journal(graph, Some(journal.clone()));
    let Some((_first, faded)) = playing_player(&harness, "t_first_tab", "Browser") else {
        return;
    };
    let key = player_key("Browser");
    let line_kept = |when: &str| {
        assert_eq!(
            stream_handover::Journal::load(&journal).repair(&key, Some(0.0)),
            stream_handover::Repair::Restore(1.0),
            "the faded stream's line left the journal {when}"
        );
    };

    // Not driven: the handover holds the first stream at 0 for as long as this test needs.
    let _moved = hand_over_noting(&harness, faded);
    line_kept("as the fade began");

    let _second = harness
        .graph
        .pw_cat(
            "--playback",
            "t_second_tab",
            "application.name = Browser",
            &[],
        )
        .expect("pw-cat should play");
    let second = u32::try_from(
        harness
            .graph
            .node_id("t_second_tab")
            .expect("the second tab's node"),
    )
    .expect("a registry id");
    assert!(
        harness.until("the second tab's volume and key", |shared| {
            shared.fades.watched(second).is_some_and(|watched| {
                watched.level == Some(1.0) && watched.key.as_deref() == Some(key.as_str())
            })
        }),
        "the engine never learned the second tab's volume and key"
    );
    harness.pump(Duration::from_millis(100));
    assert!(harness.shared.borrow().fades.busy());
    line_kept("when a second tab was met at a volume of its own");

    if unless_skipped(
        harness.graph.set_node_volume("t_second_tab", 0.0),
        "pw-cli",
        "the 0 WirePlumber restores for the second tab",
    )
    .is_none()
    {
        return;
    }
    assert!(
        harness.until("the second tab's volume back", |shared| {
            shared
                .fades
                .writes()
                .iter()
                .any(|&(id, level, _)| id == second && (level - 1.0).abs() < 1e-4)
                && shared
                    .fades
                    .watched(second)
                    .is_some_and(|watched| watched.level == Some(1.0))
        }),
        "a second tab left at 0 was not given the application's volume back"
    );
    harness.pump(Duration::from_millis(100));
    assert!(harness.shared.borrow().fades.busy());
    line_kept("when a second tab at 0 got its volume back");

    assert!(
        harness.driving_until("the handover's end", |shared| !shared.fades.busy()),
        "the faded stream never got its volume back"
    );
    assert!(
        harness.shared.borrow().fades.journal_is_empty() && !journal.exists(),
        "the handover's own end did not take its line out"
    );
}

/// A handover fades only the streams that play: a browser's paused tab keeps its own volume while
/// the tab beside it is faded to 0. A run killed there leaves both, and the next run meets them in
/// no set order. Meeting the paused tab first, at its own volume, does not take the line out: the
/// faded tab, met a moment later at 0, still gets its volume back from the journal.
#[test]
fn a_paused_sibling_met_before_the_faded_stream_leaves_it_its_line_in_the_journal() {
    if !installed("pw-cat") || !installed("pw-cli") {
        skip("pw-cat or pw-cli is not installed, so the journal was not checked");
        return;
    }
    let Some(graph) = PrivateGraph::start("handover-journal-sibling") else {
        return;
    };
    // Announced to a new connection in the order they were made: the paused tab first.
    let _paused = graph
        .pw_cat(
            "--playback",
            "t_paused_tab",
            "application.name = Browser",
            &[],
        )
        .expect("pw-cat should play");
    let _faded = graph
        .pw_cat(
            "--playback",
            "t_faded_tab",
            "application.name = Browser",
            &[],
        )
        .expect("pw-cat should play");
    if unless_skipped(
        graph.set_node_volume("t_faded_tab", 0.0),
        "pw-cli",
        "the volume a killed run left",
    )
    .is_none()
    {
        return;
    }
    let serial = graph
        .node_prop("t_faded_tab", "object.serial")
        .flatten()
        .and_then(|serial| serial.parse().ok());
    let dir = fxsound_core::test_support::ScratchDir::new("handover-repair-sibling");
    let journal = dir
        .path()
        .join("fxsound")
        .join(stream_handover::JOURNAL_FILE);
    let mut lines = stream_handover::Journal::default();
    lines.remember(&player_key("Browser"), serial, 0.7);
    lines.save(&journal).expect("the journal should be written");

    let harness = Harness::connect_with_journal(graph, Some(journal.clone()));
    let node = |name: &str| {
        u32::try_from(harness.graph.node_id(name).expect("the tab's node")).expect("a registry id")
    };
    let (paused, faded) = (node("t_paused_tab"), node("t_faded_tab"));
    assert!(
        harness.until("the faded tab's volume from the journal", |shared| {
            shared.fades.watched(faded).is_some_and(|watched| {
                watched
                    .level
                    .is_some_and(|level| (level - 0.7).abs() < 1e-4)
            })
        }),
        "the faded tab was left at 0: the paused tab's own volume took its line out"
    );
    assert!(
        harness.until("the line out of the journal", |shared| {
            shared.fades.journal_is_empty()
        }),
        "the journal kept its line once both tabs played at their volume"
    );
    assert!(!journal.exists(), "the emptied journal is still on disk");
    let shared = harness.shared.borrow();
    assert!(
        shared
            .fades
            .watched(paused)
            .is_some_and(|watched| watched.level == Some(1.0)),
        "the paused tab lost its own volume"
    );
    assert!(
        shared.fades.writes().iter().all(|&(id, ..)| id != paused),
        "the paused tab, at its own volume, was written to: {:?}",
        shared.fades.writes()
    );
}

/// A process stopped with `SIGSTOP` until the guard is dropped, which sends it `SIGCONT` — so a
/// test that fails half-way leaves nothing frozen behind for its drops to wait on.
struct Stopped(u32);

impl Stopped {
    /// Stop the process `pid` and wait until the kernel says it is stopped. `None`, said why, when
    /// `kill` is not there to stop it with.
    fn new(pid: u32) -> Option<Self> {
        if !signal(pid, "STOP") {
            skip(
                "kill could not stop a process, so an application that stopped answering was not checked",
            );
            return None;
        }
        let stopped = Self(pid);
        let deadline = Instant::now() + PATIENCE;
        while !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('T'))
        }) {
            assert!(Instant::now() < deadline, "process {pid} never stopped");
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(stopped)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        signal(self.0, "CONT");
    }
}

/// Send the signal `name` to the process `pid` with `kill`. Whether it was sent.
fn signal(pid: u32, name: &str) -> bool {
    std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// An application that has stopped answering holds whoever writes to its node until it answers
/// (`node_set_param`, `pw_impl_client_set_busy`). Its move is still made once the echo is given up
/// on; the handover's connection is taken for held and made again; and the session's connection,
/// which never wrote to it, hears the graph all along.
#[test]
fn an_application_that_stopped_answering_holds_up_neither_the_move_nor_the_session() {
    let Some(graph) = PrivateGraph::start("handover-stopped") else {
        return;
    };
    let harness = Harness::connect(graph);
    let Some((player, id)) = playing_player(&harness, "t_frozen", "Frozen") else {
        return;
    };
    let Some(_stopped) = Stopped::new(player.pid()) else {
        return;
    };
    let lines_remade = harness.shared.borrow().fades.lines_remade();

    let asked = Instant::now();
    let moved = hand_over_noting(&harness, id);
    assert!(
        harness.supervising_until("the move", |_| moved.get().is_some()),
        "an application that stopped answering held the move up"
    );
    assert!(
        asked.elapsed() >= stream_handover::ECHO_WAIT,
        "the move did not wait for the echo: {:?}",
        asked.elapsed()
    );
    assert_eq!(
        moved.get(),
        Some(Some(1.0)),
        "a stopped application echoed its volume, so nothing here was held"
    );

    assert!(
        harness.supervising_until("the handover's connection made again", |shared| {
            shared.fades.lines_remade() > lines_remade
        }),
        "the connection a stopped application holds was never made again"
    );
    assert!(
        asked.elapsed() >= fades::LINE_WATCHDOG,
        "the connection was made again before the watchdog was up: {:?}",
        asked.elapsed()
    );

    let _newcomer = harness
        .meanwhile(|graph| {
            graph.pw_cat(
                "--playback",
                "t_newcomer",
                "application.name = Newcomer",
                &[],
            )
        })
        .expect("pw-cat should play");
    assert!(
        harness.supervising_until("a stream started meanwhile", |shared| {
            shared
                .apps
                .report()
                .iter()
                .any(|stream| stream.app.name == "Newcomer")
        }),
        "the session stopped hearing the graph while an application held the handover"
    );
    let shared = harness.shared.borrow();
    assert!(shared.session.is_some() && !shared.restart_requested);
    assert!(
        shared
            .session
            .as_ref()
            .is_some_and(|session| session.fade_line.is_some()),
        "the handover's connection was not made again"
    );
}
