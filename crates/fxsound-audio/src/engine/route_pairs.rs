//! Per-application routes in the graph: the pairs of nodes `crate::app_routes` plans, and the
//! `default` metadata keys that move applications onto them (`docs/0.4.0-apps.md`).
//!
//! # A route is a lane's pair again
//!
//! Each route is built the way its lane's pair is ([`build_nodes`]) and runs the same two process
//! callbacks, [`on_sink_process`] and [`on_output_process`], so nothing on the audio thread knows
//! a route from a lane and there is no real-time code here at all. What a route has of its own is
//! what the contract gives it: its names, descriptions and link-group ([`RouteSlot`]), its own
//! DSP — the lane's chain, built here on the main loop from the preset's parameters
//! ([`lane_dsp::route`]) — its own ring and counters, and its own parameter buffer, whose writing
//! half this module keeps and writes a preset's new parameters through ([`set_rules`]).
//!
//! - **Output.** `fxsound_route_o<N>`, an `Audio/Sink` described `FxSound (Output) · <preset>`,
//!   and `fxsound_route_o<N>_play`, a playback stream to the device the output lane plays to —
//!   passive where the server runs a link-group together, like the lane's own (module docs of
//!   `engine`, "Idle") — both in `fxsound-route-o<N>`.
//! - **Input.** `fxsound_route_i<N>_capture`, a capture stream on the input lane's microphone —
//!   or on the echo canceller's source while echo cancellation runs for it, as the lane's own
//!   capture stream is — and `fxsound_route_i<N>`, an `Audio/Source` described
//!   `FxSound (Input) · <preset>`, both in `fxsound-route-i<N>`.
//!
//! A route plays at its lane's volume: its chain reads the lane's [`LaneVolume`] and the lane's
//! silence while the system sleeps, and has its history cleared with the lane's when the system
//! wakes ([`wake_up`]); and its own virtual node's adapter keeps whatever a mixer sets
//! on it at unity, as the lane's does ([`virtual_node_props`]) — so the volume keys, which move
//! FxSound's own sink, move a routed game with everything else. Nothing a mixer sets on the route's
//! node is applied. Its `priority.session` is below anything else's
//! ([`app_routes::ROUTE_PRIORITY_SESSION`]), and no metadata write of FxSound's ever names it as
//! a default, so it never becomes one; and its name is FxSound's ([`crate::is_fxsound_node`]), so
//! it is never offered as a device nor listed as an application.
//!
//! A route follows its lane. It is built beside the lane's pair, on the same device and at the same
//! format, and rebuilt whenever the lane's pair is rebuilt on another device, another format or
//! another node behind the microphone. A lane between pairs keeps its routes as they are; a lane
//! switched off, or with no device left, takes them down, its streams' keys deleted first.
//!
//! # Moving a stream
//!
//! Once a route's virtual node is in the registry, every stream the plan puts on the route gets
//! `target.object = <the node's object.serial>` in the `default` metadata ([`MetadataOp`]), which
//! WirePlumber follows. A stream that leaves the route — its rule gone, the route taken down, the
//! lane switched off, the engine exiting — has the key deleted, and the session puts it back where
//! it would have gone. A key never outlives the node it names: in one tick, on one connection, the
//! deletes and the writes onto routes that stay are queued ahead of any route's destruction —
//! including a key onto a route that is rebuilt this tick, under the same number or on the lane's
//! new device, which is deleted and written again once the new node is known ([`Want::Pending`]) —
//! and on the way out the deletes ride on the confirmed exit hand-back
//! ([`release_defaults_before_exit`]).
//!
//! A stream FxSound does not move can still be on a route: its own properties may name the route's
//! node, WirePlumber may have restored it onto one and will not move it again
//! (`node.dont-reconnect`, [`Moves::anchor`]), or it may be a recorder the user pointed at a
//! playback route's monitor, by a key FxSound leaves as it is ([`Moves::monitored`]). Such a
//! stream keeps its route in use ([`Candidate::on_route`]).
//!
//! # What a private graph can show
//!
//! In `graph_churn::routes`' private PipeWire the route pairs, their properties and the metadata
//! keys can all be read back, and are. The move itself cannot be: it is WirePlumber's to make, and
//! no WirePlumber runs there — the private graph has no session manager at all, so nothing links a
//! stream to a route, or to anything else.

use fxsound_core::MAX_ROUTES_PER_LANE;
use fxsound_core::messages::{AppRoute, RouteParams};

use super::*;
use crate::TOO_MANY_APPLICATION_PRESETS;
use crate::app_routes::{
    self, Candidate, LaneState, MetadataOp, Moves, OverflowWarnings, RoutePreset, RouteSlot,
    RouteTable, Rules, Want,
};
use crate::app_streams::{Listed, Pin};
use crate::lane_dsp::RouteParamsWriter;

/// Every route of both lanes, and everything needed to run and move onto them.
pub(super) struct Routes {
    /// The routes that exist, each with its pair while it has one. First, so that on a drop of
    /// the whole engine state the pairs go before anything they could refer to.
    live: Vec<LiveRoute>,
    /// What the app last asked for ([`UiToAudio::SetAppRoutes`]).
    rules: Rules,
    /// Which routes each lane runs, under which numbers, since when unused.
    table: RouteTable,
    /// What the `default` metadata says about each stream's target, and what of it is FxSound's.
    moves: Moves,
    /// Which applications the window has been told could not get a route.
    warnings: OverflowWarnings,
    /// How long a route nobody uses is kept ([`app_routes::ROUTE_IDLE`], shorter in the tests).
    idle: Duration,
    /// The missing `default` metadata object has been reported, and need not be again.
    no_metadata_told: bool,
    /// Routes the plan took down, kept, pair and all, while they go ([`Leaving`]). No new pair is
    /// built in a slot one of these still holds.
    leaving: Vec<Leaving>,
}

impl Routes {
    pub(super) fn new(idle: Duration) -> Self {
        Self {
            live: Vec::new(),
            rules: Rules::default(),
            table: RouteTable::default(),
            moves: Moves::default(),
            warnings: OverflowWarnings::default(),
            idle,
            no_metadata_told: false,
            leaving: Vec::new(),
        }
    }

    /// How long a route nobody uses is kept, from now on.
    pub(super) const fn set_idle(&mut self, idle: Duration) {
        self.idle = idle;
    }

    /// How many rules the engine keeps.
    #[cfg(test)]
    pub(super) const fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// How many routes exist, and how many of them have a pair right now.
    #[cfg(test)]
    pub(super) fn counts(&self) -> (usize, usize) {
        (
            self.live.len(),
            self.live
                .iter()
                .filter(|route| route.pair.is_some())
                .count(),
        )
    }

    /// A route of `preset` in `slot`, as a plan would have it made: its DSP built and on the main
    /// loop, and no pair — for tests with no server to build one on.
    #[cfg(test)]
    pub(super) fn add_for_tests(&mut self, slot: RouteSlot, preset: RoutePreset) {
        self.live.push(LiveRoute::new(slot, preset, Instant::now()));
    }

    /// Take the route's DSP off the main loop, as its pair does when it is built.
    #[cfg(test)]
    pub(super) fn take_dsp_for_tests(&mut self, slot: RouteSlot) -> Option<LaneDsp> {
        self.live
            .iter_mut()
            .find(|route| route.slot == slot)?
            .dsp
            .take()
    }

    /// Send the route's DSP home through its recycle channel, as its pair does when it is dropped.
    #[cfg(test)]
    pub(super) fn send_dsp_home_for_tests(&self, slot: RouteSlot, dsp: LaneDsp) {
        let route = self
            .live
            .iter()
            .find(|route| route.slot == slot)
            .expect("a route in that slot");
        route.recycle.0.send(dsp).expect("the route keeps its end");
    }

    /// Plan the routes for `streams` with the rules the engine keeps, both lanes attached, as a
    /// tick would: the table only, with no pair built and no key written — for tests with no
    /// server.
    #[cfg(test)]
    pub(super) fn plan_for_tests(
        &mut self,
        streams: &[Candidate],
        now: Instant,
    ) -> app_routes::Plan {
        let lanes = PerDirection::from_fn(|_| LaneState::Attached);
        self.table
            .plan(&self.rules, streams, &lanes, now, self.idle)
    }

    /// The parameters the route in `slot` runs, as the main loop last wrote them into its buffer.
    #[cfg(test)]
    pub(super) fn params_for_tests(
        &self,
        slot: RouteSlot,
    ) -> Option<fxsound_core::messages::RouteParams> {
        self.live
            .iter()
            .find(|route| route.slot == slot)
            .map(|route| route.preset.params)
    }

    /// How many events wait in each route's queue, by slot, in the order the routes were made.
    #[cfg(test)]
    pub(super) fn events_waiting(&self) -> Vec<(RouteSlot, usize)> {
        self.live
            .iter()
            .map(|route| (route.slot, route.events.len()))
            .collect()
    }
}

/// One route: the preset it runs, its DSP and its paths, and its pair while it has one.
///
/// Field order is drop order: the pair first, because dropping its NODE 1 sends the DSP home
/// through [`Self::recycle`], which has to be there to take it.
struct LiveRoute {
    pair: Option<RoutePair>,
    slot: RouteSlot,
    preset: RoutePreset,
    /// The writing half of the route's parameter buffer ([`lane_dsp::route`]).
    writer: RouteParamsWriter,
    /// The main loop's ends of an input route's chain hand-over; `None` for an output route.
    handover: Option<ChainHandover>,
    /// The sending end of the route's event queue ([`lane_dsp::route`]): what the wake clears the
    /// route's chain history through, as it clears its lane's ([`wake_up`]).
    events: Sender<DspEvent>,
    /// The voice chain an input route runs, or is about to.
    spec: ChainSpec,
    /// `spec` changed while the DSP was on the audio thread, and the replacement has not gone
    /// over yet ([`hand_over_chain`]).
    spec_pending: bool,
    /// The DSP, while it is on the main loop: between pairs.
    dsp: Option<LaneDsp>,
    recycle: (Sender<LaneDsp>, Receiver<LaneDsp>),
    ring: Arc<SampleRing>,
    counters: Arc<Counters>,
    status: Arc<StreamStatus>,
    /// The registry id and `object.serial` of the pair's virtual node, once the registry has
    /// announced it: what a moved stream's metadata target names.
    node: Option<(u32, Option<u64>)>,
    /// Failed pairs in a row, for the route's own backoff, and when the next may be tried.
    attempts: u32,
    next_attempt: Instant,
}

impl LiveRoute {
    /// A route of `preset` in `slot`, with its DSP built and no pair yet.
    fn new(slot: RouteSlot, preset: RoutePreset, now: Instant) -> Self {
        let spec = chain_spec(&preset.chain);
        let (dsp, writer, handover, events) = lane_dsp::route(preset.params, spec);
        Self {
            pair: None,
            slot,
            preset,
            writer,
            handover,
            events,
            spec,
            spec_pending: false,
            dsp: Some(dsp),
            recycle: crossbeam_channel::unbounded(),
            ring: Arc::new(SampleRing::new()),
            counters: Arc::new(Counters::new()),
            status: Arc::new(StreamStatus::default()),
            node: None,
            attempts: 0,
            next_attempt: now,
        }
    }

    /// The serial moved streams are written with: the pair's virtual node's, while the pair is on
    /// `on` and its node has been announced.
    fn serial_on(&self, on: Option<&Attachment>) -> Option<u64> {
        let pair = self.pair.as_ref()?;
        if on != Some(&pair.on) {
            return None;
        }
        self.node.and_then(|(_, serial)| serial)
    }

    /// Drop the pair, and take its DSP back.
    fn drop_pair(&mut self) {
        self.pair = None;
        self.node = None;
        self.status.clear();
        self.counters.format_mismatches.swap(0, Ordering::Relaxed);
        while let Ok(dsp) = self.recycle.1.try_recv() {
            self.dsp = Some(dsp);
        }
    }
}

/// A route the plan took down, on its way out. First its streams are moved off it — silenced, and
/// their keys deleted ([`hand_over_moves`], `fades`): a route's node that went first would have
/// WirePlumber move them itself, unfaded. Then its chain is switched off, which fades what it
/// still plays — a reverb's tail — out over its 20 ms glide, and the pair goes once that has been
/// played ([`ROUTE_TAIL`]): taken down at once, the tail stopped in the middle of its wave, which
/// clicked at −36 dBFS on the speakers.
struct Leaving {
    route: LiveRoute,
    /// When its pair may go: `None` while its streams are still being moved off it.
    goes_at: Option<Instant>,
}

/// How long a route's switched-off chain plays before its pair goes ([`Leaving`]): its 20 ms glide,
/// and what the stream to the device has buffered.
const ROUTE_TAIL: Duration = Duration::from_millis(80);

/// Where a lane's pair is: what its routes are built on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attachment {
    /// The real device's `node.name`, and its serial when the lane's pair was built.
    target: String,
    target_serial: Option<u64>,
    format: PairFormat,
    /// The node an input pair records from instead of the microphone: the echo canceller's source.
    via: Option<&'static str>,
}

impl Attachment {
    /// Where the lane's pair is, if it has one.
    fn of(lane: &Lane) -> Option<Self> {
        let nodes = lane.nodes.as_ref()?;
        Some(Self {
            target: nodes.target.clone(),
            target_serial: nodes.target_serial,
            format: nodes.format,
            via: nodes.via,
        })
    }

    /// The node the route's stream on the device is linked to: the device, or what records it in
    /// its place.
    fn device(&self) -> &str {
        self.via.unwrap_or(&self.target)
    }
}

/// A route's two nodes and everything that must die with them. Field order is drop order: every
/// listener before the stream it hooks, as in the lanes' [`Nodes`].
struct RoutePair {
    _first_listener: pw::stream::StreamListener<SinkData>,
    _second_listener: pw::stream::StreamListener<OutData>,
    /// Kept reachable to republish the chain's delay when it changes ([`republish_latency`]).
    first: pw::stream::StreamRc,
    _second: pw::stream::StreamRc,
    /// Where it was built.
    on: Attachment,
    /// When it was built: what the route's backoff is forgiven against.
    built_at: Instant,
    /// The delay last declared on NODE 1, in frames.
    published_latency: u32,
}

/// The voice chain a route's preset names; the `voice` chain for a name this build does not know,
/// as the input lane's own ([`set_input_chain`]).
fn chain_spec(name: &str) -> ChainSpec {
    if name.trim().is_empty() {
        return ChainSpec::voice();
    }
    ChainSpec::by_name(name).unwrap_or_else(|| {
        log::warn!(
            "an application's voice preset names a chain this build does not know ({name:?}); \
             its route runs the voice chain"
        );
        ChainSpec::voice()
    })
}

// ---------------------------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------------------------

/// Take the app's whole set of rules ([`UiToAudio::SetAppRoutes`]): diff it against the set the
/// engine runs, write the new parameters of every preset a route runs into its buffer, hand a new
/// voice chain to every input route whose preset names another, and keep the set. The routes and
/// the streams follow on the [`reconcile`] that ends every control message.
pub(super) fn set_rules(shared: &mut Shared, asked: Vec<AppRoute>) {
    let (rules, refused) = Rules::new(asked);
    for (rule, why) in &refused {
        log::warn!(
            "an application rule is refused ({why:?}): {:?} in the {} lane, preset {:?}",
            rule.app.display(),
            rule.direction.key(),
            rule.preset
        );
    }
    let routes = &mut shared.routes;
    let changes = app_routes::diff(&routes.rules, &rules);
    if changes.is_empty() {
        log::debug!("{} application rules, unchanged", rules.len());
        routes.rules = rules;
        return;
    }
    log::info!(
        "{} application rules: {} presets new, {} gone, {} with new parameters, {} with a new \
         chain{}",
        rules.len(),
        changes.added.len(),
        changes.removed.len(),
        changes.params.len(),
        changes.chains.len(),
        if changes.applications {
            "; applications moved between presets"
        } else {
            ""
        }
    );
    // A preset named again while its route was kept for a stream FxSound could not move off it
    // (`Plan::kept`): the route runs the preset as it is now, parameters and chain.
    let renamed: Vec<RoutePreset> = changes
        .added
        .iter()
        .filter(|(direction, name)| routes.table.slot_of(*direction, name).is_some())
        .filter_map(|(direction, name)| rules.preset(*direction, name))
        .collect();
    for preset in changes.params.iter().chain(&renamed) {
        if let Some(route) = live_route(routes, preset.direction, &preset.name)
            && route.preset.params != preset.params
            && route.writer.write(preset.params)
        {
            route.preset.params = preset.params;
        }
    }
    for preset in changes.chains.iter().chain(&renamed) {
        if let Some(route) = live_route(routes, preset.direction, &preset.name) {
            route.preset.chain.clone_from(&preset.chain);
            let spec = chain_spec(&preset.chain);
            if spec != route.spec {
                log::info!(
                    "route {}: voice chain {}",
                    route.slot.node_name(),
                    preset.chain
                );
                route.spec = spec;
                route.spec_pending = true;
                hand_over_chain(route);
            }
        }
    }
    routes.rules = rules;
}

/// The route running `preset` in `direction`, if there is one.
fn live_route<'r>(
    routes: &'r mut Routes,
    direction: DeviceDirection,
    preset: &str,
) -> Option<&'r mut LiveRoute> {
    let slot = routes.table.slot_of(direction, preset)?;
    routes
        .live
        .iter_mut()
        .find(|route| route.slot == slot && route.preset.name == preset)
}

/// Bring an input route's voice chain up to its spec, wherever its DSP is: rebuilt in place on the
/// main loop, or built here and sent over to the audio thread, which swaps it in and sends the old
/// one back ([`ChainHandover`]) — the input lane's own way ([`reconcile_input_chain`]). A
/// replacement that finds the slot full goes on the next tick.
fn hand_over_chain(route: &mut LiveRoute) {
    if let Some(handover) = route.handover.as_ref() {
        while handover.retired.try_recv().is_ok() {}
    }
    if !route.spec_pending {
        return;
    }
    if let Some(dsp) = route.dsp.as_mut().and_then(LaneDsp::as_input_mut) {
        dsp.set_spec(route.spec);
        route.spec_pending = false;
        return;
    }
    let (Some(handover), Some(_)) = (route.handover.as_ref(), route.pair.as_ref()) else {
        return;
    };
    let engine = Box::new(InputEngine::new_with_spec(
        route.counters.sample_rate.load(Ordering::Relaxed) as f32,
        MAX_QUANTUM_FRAMES,
        route.counters.channels.load(Ordering::Relaxed) as usize,
        route.spec,
    ));
    match handover.replacement.try_send(engine) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => route.spec_pending = false,
        // The last replacement has not been taken yet; this one is dropped here, on the main
        // loop, and built again on the next tick.
        Err(TrySendError::Full(_)) => {}
    }
}

// ---------------------------------------------------------------------------------------------
// The tick
// ---------------------------------------------------------------------------------------------

/// Where a lane stands, for its routes.
fn lane_state(lane: &Lane) -> LaneState {
    if !lane.enabled {
        LaneState::Off
    } else if lane.nodes.is_some() {
        LaneState::Attached
    } else if lane.last_target.is_some() {
        LaneState::Between
    } else {
        LaneState::Off
    }
}

/// Bring the routes and the metadata in line with the rules, the streams and the lanes: once a
/// supervisor tick, and at the end of every control message.
///
/// In this order: the streams that leave a route are moved back, and those that go onto a route
/// whose node is in the registry and stays are moved onto it; then the routes that go are taken
/// down, the new ones built, and those that follow a lane to another device rebuilt there. Every
/// key is where it is to be before any node it could name goes. A route built or rebuilt in this
/// call has no node in the registry yet; its streams are moved on a later tick, once it has — and
/// until then have no key naming the node it had, nor another route's ([`Want::Pending`]).
pub(super) fn reconcile(shared: &mut Shared, now: Instant) {
    for route in &mut shared.routes.live {
        while let Ok(dsp) = route.recycle.1.try_recv() {
            route.dsp = Some(dsp);
        }
        hand_over_chain(route);
    }
    drop_gone_routes(shared, now);
    if !shared.ready() {
        return;
    }
    catch_failures(shared, now);

    let lanes = PerDirection::from_fn(|direction| lane_state(shared.lanes.get(direction)));
    let attachments =
        PerDirection::from_fn(|direction| Attachment::of(shared.lanes.get(direction)));
    // Every route node the registry has announced: its route, its registry id, its serial.
    let nodes: Vec<(RouteSlot, u32, u64)> = shared
        .routes
        .live
        .iter()
        .filter_map(|route| {
            let (id, serial) = route.node?;
            Some((route.slot, id, serial?))
        })
        .collect();
    let listed = shared.apps.listed();
    let moves = &mut shared.routes.moves;
    let streams: Vec<Candidate> = listed
        .into_iter()
        .map(|stream| Candidate {
            movable: stream.pin.is_none() && !moves.moved_by_hand(stream.id),
            on_route: resting_on(moves, &stream, &nodes),
            id: stream.id,
            direction: stream.direction,
            app: stream.app,
        })
        .collect();
    let Routes {
        rules, table, idle, ..
    } = &mut shared.routes;
    let plan = table.plan(rules, &streams, &lanes, now, *idle);

    // The node each route keeps through this tick: not one that goes, nor one whose pair is
    // rebuilt on the lane's new device. A lane between pairs keeps its routes' pairs as they are.
    let staying: std::collections::HashMap<RouteSlot, u64> = shared
        .routes
        .live
        .iter()
        .filter(|route| {
            !plan
                .teardown
                .iter()
                .any(|(gone, _)| gone.slot == route.slot && gone.preset == route.preset.name)
        })
        .filter(|route| {
            route.pair.as_ref().is_some_and(|pair| {
                attachments
                    .get(route.slot.direction)
                    .as_ref()
                    .is_none_or(|on| *on == pair.on)
            })
        })
        .filter_map(|route| Some((route.slot, route.node?.1?)))
        .collect();

    // Where each stream is to be: its route's serial, or — for a route this call builds or moves
    // to another device, whose node the registry has not announced yet, or one kept while its lane
    // is between pairs — pending, on the node the route keeps if it keeps one.
    let wanted: Vec<(u32, Want)> = plan
        .assigned
        .iter()
        .map(|&(id, slot)| {
            let serial = if plan.build.iter().any(|(built, _)| *built == slot) {
                None
            } else {
                shared
                    .routes
                    .live
                    .iter()
                    .find(|route| route.slot == slot)
                    .and_then(|route| route.serial_on(attachments.get(slot.direction).as_ref()))
            };
            let want = serial.map_or(
                Want::Pending {
                    still: staying.get(&slot).copied(),
                },
                Want::Onto,
            );
            (id, want)
        })
        .collect();
    let ops = shared.routes.moves.plan(&wanted);
    let (deletes, writes): (Vec<MetadataOp>, Vec<MetadataOp>) = ops
        .into_iter()
        .partition(|op| matches!(op, MetadataOp::Delete { .. }));

    // 1. The streams leaving a route go back, and the streams going onto a route whose node is
    //    known and stays are moved onto it — both ahead of any node a key could name going. Each
    //    stream that plays is faded to silence first, and the keys are written once it is (`fades`,
    //    [`hand_over_moves`]): noted as sent now, so that the next plan does not ask again.
    let deletes = note(shared, deletes);
    let writes = note(shared, writes);

    // 2. The routes that go; and those kept only for a stream FxSound does not move, said once.
    //    Each one goes once its streams have been moved off it.
    for (slot, preset) in &plan.kept {
        log::info!(
            "route {} ({preset}) is kept though no rule names its preset any more: an application \
             FxSound does not move plays or records through it; it goes once that one has left",
            slot.node_name()
        );
    }
    let mut leaving = Vec::new();
    for (gone, why) in &plan.teardown {
        if let Some(index) = shared
            .routes
            .live
            .iter()
            .position(|route| route.slot == gone.slot && route.preset.name == gone.preset)
        {
            log::info!(
                "route {} ({}) goes: {}",
                gone.slot.node_name(),
                gone.preset,
                why.reason()
            );
            let route = shared.routes.live.remove(index);
            leaving.push((route.slot, route.preset.name.clone()));
            shared.routes.leaving.push(Leaving {
                route,
                goes_at: None,
            });
        }
    }
    hand_over_moves(shared, deletes, writes, leaving);

    // 3. The new ones, as routes with no pair yet.
    for (slot, preset) in &plan.build {
        let Some(preset) = shared.routes.rules.preset(slot.direction, preset) else {
            continue;
        };
        log::info!(
            "route {} runs {:?} for the {} lane's applications",
            slot.node_name(),
            preset.name,
            slot.direction.key()
        );
        shared.routes.live.push(LiveRoute::new(*slot, preset, now));
    }

    // 4. Every route on its lane's device: a pair built where there is none, and rebuilt where
    //    the lane has moved.
    build_pairs(shared, &attachments, now);

    // 5. What the app is told of each stream: the preset of the route it was moved onto.
    let mut routed = std::collections::HashMap::new();
    for &(id, slot) in &plan.assigned {
        let Some(route) = shared.routes.live.iter().find(|route| route.slot == slot) else {
            continue;
        };
        let on = route.serial_on(attachments.get(slot.direction).as_ref());
        if on.is_some() && shared.routes.moves.written(id) == on {
            routed.insert(id, route.preset.name.clone());
        }
    }
    shared.apps.set_routes(routed);

    // 6. An application whose preset could not get a route stays on its lane, and the window is
    //    told why — once.
    for overflow in shared.routes.warnings.news(&plan.overflow) {
        log::info!(
            "{} stays on the {} lane's preset: the lane already runs {MAX_ROUTES_PER_LANE} \
             routes, every one in use, and {:?} would need another",
            overflow.app,
            overflow.direction.key(),
            overflow.preset
        );
        shared.notify(AudioToUi::Warning {
            direction: Some(overflow.direction),
            message: fxsound_core::i18n::tr_args(
                TOO_MANY_APPLICATION_PRESETS,
                &[&overflow.app, &MAX_ROUTES_PER_LANE.to_string()],
            ),
        });
    }

    for route in &mut shared.routes.live {
        republish_latency_of(route);
    }
}

/// The route a stream sits on by something no plan of FxSound's changes ([`Candidate::on_route`]),
/// among `nodes` — each route's slot, its virtual node's registry id and its serial:
///
/// - a stream WirePlumber will not move again once it is linked (`node.dont-reconnect`), on the
///   route its key put it on ([`Moves::anchor`]) for as long as that route's node is there;
/// - a recorder whose key names a playback route's sink, on that route, whose monitor it records
///   ([`Moves::monitored`]) — a key of the user's, which WirePlumber reads ahead of the stream's
///   own properties; or
/// - a stream whose own properties name a route's node ([`app_routes::route_of_target`]), unless
///   the metadata holds a key for it, which WirePlumber reads first — or the stream says
///   `node.dont-move`, and WirePlumber reads no key for it at all.
fn resting_on(
    moves: &mut Moves,
    stream: &Listed,
    nodes: &[(RouteSlot, u32, u64)],
) -> Option<RouteSlot> {
    let slot_of = |serial: u64| {
        nodes
            .iter()
            .find(|&&(.., node)| node == serial)
            .map(|&(slot, ..)| slot)
    };
    if stream.pin == Some(Pin::DontReconnect)
        && let Some(slot) = moves.anchor(stream.id).and_then(slot_of)
    {
        return Some(slot);
    }
    if stream.pin != Some(Pin::DontMove) && moves.has_key(stream.id) {
        return moves.monitored(stream.id).and_then(slot_of);
    }
    stream
        .target
        .as_ref()
        .and_then(|target| app_routes::route_of_target(target, nodes))
}

/// A route whose pair reported an error, or whose two nodes negotiated different formats, loses
/// its pair, and gets a new one on its own backoff — the lane's rule, for the lane's reasons
/// ([`supervise_lane`]). One that has stayed up as long as its last wait is forgiven.
fn catch_failures(shared: &mut Shared, now: Instant) {
    for route in &mut shared.routes.live {
        let Some(pair) = route.pair.as_ref() else {
            continue;
        };
        let failed = route.status.sink_error.swap(false, Ordering::Relaxed)
            | route.status.output_error.swap(false, Ordering::Relaxed);
        let mismatched = route.counters.format_mismatches.swap(0, Ordering::Relaxed) > 0;
        if failed || mismatched {
            log::warn!(
                "route {} {}; rebuilding it after {:?}",
                route.slot.node_name(),
                if failed {
                    "reported an error"
                } else {
                    "negotiated two formats"
                },
                backoff(route.attempts)
            );
            route.drop_pair();
            route.next_attempt = now + backoff(route.attempts);
            route.attempts = route.attempts.saturating_add(1);
        } else if now >= pair.built_at + backoff(route.attempts.saturating_sub(1)) {
            route.attempts = 0;
        }
    }
}

/// Give every route a pair on its lane's device, rebuilding the ones whose lane has moved. Not
/// while the lane is between pairs — the route keeps what it has — and not before the route's
/// backoff allows.
fn build_pairs(shared: &mut Shared, attachments: &PerDirection<Option<Attachment>>, now: Instant) {
    let Some(core) = shared.session.as_ref().map(|session| session.core.clone()) else {
        return;
    };
    let scheduled = shared.link_groups_scheduled.get();
    let Shared {
        routes,
        lanes,
        language,
        ..
    } = shared;
    for route in &mut routes.live {
        let Some(on) = attachments.get(route.slot.direction) else {
            continue;
        };
        if route.pair.as_ref().is_some_and(|pair| pair.on == *on) || now < route.next_attempt {
            continue;
        }
        // Not under a name a route that is going still holds: the server never sees two nodes of
        // one name. Built on the next pass, once that one has gone.
        if routes
            .leaving
            .iter()
            .any(|gone| gone.route.slot == route.slot)
        {
            continue;
        }
        if route.pair.is_some() {
            log::info!(
                "route {} follows its lane to {}",
                route.slot.node_name(),
                on.target
            );
            route.drop_pair();
        }
        let lane = lanes.get(route.slot.direction);
        match build_pair(&core, route, lane, on, language.as_deref(), scheduled) {
            Ok(pair) => {
                log::info!(
                    "route {} is up on {} ({} ch @ {} Hz){}",
                    route.slot.node_name(),
                    on.target,
                    on.format.channels,
                    on.format.rate,
                    if on.via.is_some() {
                        ", through the echo canceller"
                    } else {
                        ""
                    }
                );
                route.pair = Some(pair);
            }
            Err(error) => {
                log::warn!(
                    "route {} could not be built ({error}); trying again after {:?}",
                    route.slot.node_name(),
                    backoff(route.attempts)
                );
                route.drop_pair();
                route.next_attempt = now + backoff(route.attempts);
                route.attempts = route.attempts.saturating_add(1);
            }
        }
    }
}

/// Write `deletes` and then `writes` to the `default` metadata object once every stream they move
/// that plays has faded to silence (`fades`), and then switch off the routes of `leaving` — each
/// slot and preset, held in [`Routes::leaving`] meanwhile — for [`drop_gone_routes`] to take down
/// once their tails have been played. The streams get their volume back once they are linked where
/// the keys put them. With nothing moved and nothing leaving, nothing is asked of the handover.
fn hand_over_moves(
    shared: &mut Shared,
    deletes: Vec<MetadataOp>,
    writes: Vec<MetadataOp>,
    leaving: Vec<(RouteSlot, String)>,
) {
    if deletes.is_empty() && writes.is_empty() && leaving.is_empty() {
        return;
    }
    let mut streams: Vec<u32> = deletes
        .iter()
        .chain(&writes)
        .map(|op| match *op {
            MetadataOp::Write { subject, .. } | MetadataOp::Delete { subject } => subject,
        })
        .collect();
    streams.sort_unstable();
    streams.dedup();
    fades::hand_over(
        shared,
        streams,
        Box::new(move |shared: &mut Shared| {
            write_ops(shared, &deletes);
            write_ops(shared, &writes);
            let goes_at = Instant::now() + ROUTE_TAIL;
            for gone in &mut shared.routes.leaving {
                let route = &mut gone.route;
                if gone.goes_at.is_none()
                    && leaving
                        .iter()
                        .any(|(slot, preset)| route.slot == *slot && route.preset.name == *preset)
                {
                    let mut params = route.preset.params;
                    match &mut params {
                        RouteParams::Output(params) => params.power = false,
                        RouteParams::Input(params) => params.power = false,
                    }
                    route.writer.write(params);
                    gone.goes_at = Some(goes_at);
                }
            }
            !deletes.is_empty() || !writes.is_empty()
        }),
    );
}

/// Take down every route on its way out whose tail has been played ([`Leaving`]).
fn drop_gone_routes(shared: &mut Shared, now: Instant) {
    shared.routes.leaving.retain_mut(|gone| {
        let goes = gone.goes_at.is_some_and(|at| now >= at);
        if goes {
            gone.route.drop_pair();
        }
        !goes
    });
}

/// Send `ops` to the `default` metadata object at once, and note each as sent.
fn send(shared: &mut Shared, ops: &[MetadataOp]) {
    let ops = note(shared, ops.to_vec());
    write_ops(shared, &ops);
}

/// Note `ops` as this engine's, sent — what the plan and the metadata's reports are weighed
/// against — and hand them back for [`write_ops`] to send. None when there is no `default` metadata
/// object to send them to, said once: they are asked for again by the next plan.
fn note(shared: &mut Shared, ops: Vec<MetadataOp>) -> Vec<MetadataOp> {
    if ops.is_empty() {
        return ops;
    }
    if shared
        .session
        .as_ref()
        .and_then(|session| session.metadata.as_ref())
        .is_none()
    {
        if !shared.routes.no_metadata_told {
            log::warn!(
                "no `default` metadata object (a bare pipewire with no session manager?): \
                 applications cannot be moved onto their routes"
            );
            shared.routes.no_metadata_told = true;
        }
        return Vec::new();
    }
    for &op in &ops {
        shared.routes.moves.sent(op);
    }
    ops
}

/// Write `ops`, noted already ([`note`]), to the `default` metadata object.
fn write_ops(shared: &Shared, ops: &[MetadataOp]) {
    let Some(metadata) = shared
        .session
        .as_ref()
        .and_then(|session| session.metadata.as_ref())
    else {
        return;
    };
    for &op in ops {
        match op {
            MetadataOp::Write { subject, serial } => {
                log::debug!("application stream {subject} → route node serial {serial}");
                metadata.set_property(
                    subject,
                    app_routes::TARGET_OBJECT_KEY,
                    Some(app_routes::TARGET_OBJECT_TYPE),
                    Some(&serial.to_string()),
                );
            }
            MetadataOp::Delete { subject } => {
                log::debug!("application stream {subject} goes back to its lane");
                metadata.set_property(subject, app_routes::TARGET_OBJECT_KEY, None, None);
            }
        }
    }
}

/// Keep a route's published delay honest, as [`republish_latency`] does a lane's: an input route's
/// denoiser adds ten milliseconds when its chain changes.
fn republish_latency_of(route: &mut LiveRoute) {
    let current = route.counters.dsp_latency_frames.load(Ordering::Relaxed);
    let Some(pair) = route.pair.as_mut() else {
        return;
    };
    if current == pair.published_latency || current == 0 {
        return;
    }
    let values = process_latency_pod(current as usize);
    let Some(pod) = Pod::from_bytes(&values) else {
        return;
    };
    if let Err(error) = pair.first.update_params(&mut [pod]) {
        log::warn!(
            "could not republish route {}'s latency: {error}",
            route.slot.node_name()
        );
        return;
    }
    pair.published_latency = current;
}

// ---------------------------------------------------------------------------------------------
// Sleep
// ---------------------------------------------------------------------------------------------

/// The system has woken ([`super::wake_up`]): clear every route's chain history, as each lane's is
/// cleared. A routed application's filters, leveller and denoiser last saw the world from before
/// the suspend, as its lane's did; its route was silenced with the lane while the system slept (it
/// reads the lane's `system_mute`), and comes back with the lane and with no older a history.
///
/// Through the route's own event queue ([`lane_dsp::route`]), so the reset lands on the next
/// block whichever thread the chain is on. A chain on the main loop — a route between pairs — has
/// no block to run it, and has it applied here at once, as [`apply_idle_lane_events`] applies a
/// lane's; a DSP its last pair has sent home is taken back first, so it is not missed.
pub(super) fn wake_up(shared: &mut Shared) {
    for route in &mut shared.routes.live {
        match route.events.try_send(DspEvent::ResetFilterState) {
            // A full queue is an earlier wake's reset still waiting for the next block of a pair
            // nothing has played into since, and one reset is as good as two.
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => log::warn!(
                "could not clear route {}'s filter history after the wake",
                route.slot.node_name()
            ),
        }
        while let Ok(dsp) = route.recycle.1.try_recv() {
            route.dsp = Some(dsp);
        }
        if let Some(dsp) = route.dsp.as_mut() {
            dsp.drain_events();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The pair
// ---------------------------------------------------------------------------------------------

/// Build a route's two nodes on `on`, the lane's device, at the lane's format, on the route's own
/// ring, counters and DSP, and at the lane's volume ([`LaneVolume`]).
///
/// [`build_nodes`] for a route: the same nodes in the same order, with the route's names, the
/// route's link-group and a description that names the preset; the same process callbacks.
fn build_pair(
    core: &pw::core::CoreRc,
    route: &mut LiveRoute,
    lane: &Lane,
    on: &Attachment,
    language: Option<&str>,
    link_groups_scheduled: bool,
) -> Result<RoutePair, AudioError> {
    let slot = route.slot;
    let direction = slot.direction;
    let PairFormat {
        channels,
        rate,
        positions,
        source_rate,
    } = on.format;
    let quantum = DEFAULT_QUANTUM_FRAMES.min(MAX_QUANTUM_FRAMES as u32);
    let latency = pair_latency(rate);
    // Passive where the server runs a group together, as the lane's own; on an older server the
    // stream on the device holds it for as long as the route lives, which is at most the idle
    // period beyond its last stream.
    let (passive, _) = idle_plan(direction, link_groups_scheduled);
    let preset = route.preset.name.clone();
    let make_virtual = || {
        route_virtual_props(
            slot,
            &preset,
            language,
            (channels, rate, &positions),
            &latency,
        )
    };
    let make_stream = || route_stream_props(slot, &preset, on.device(), &latency, passive);

    // ---- NODE 1: the node the DSP runs in --------------------------------------------------
    let (first_name, first_props, first_flags) = match direction {
        DeviceDirection::Output => (
            slot.node_name(),
            make_virtual(),
            StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
        DeviceDirection::Input => (
            slot.stream_name(),
            make_stream(),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
    };
    let first = pw::stream::StreamRc::new(core.clone(), &first_name, first_props)
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    // The DSP, from here on inside `SinkData`, whose `Drop` sends it home on every failure path.
    while let Ok(dsp) = route.recycle.1.try_recv() {
        route.dsp = Some(dsp);
    }
    hand_over_chain(route);
    let Some(mut dsp) = route.dsp.take() else {
        log::error!(
            "route {}'s DSP has not come back from its previous pair",
            slot.node_name()
        );
        return Err(AudioError::PipewireDisconnected);
    };
    dsp.set_format(rate as f32, channels as usize);
    dsp.set_source_rate(source_rate.map(|rate| rate as f32));
    dsp.set_layout(&positions);
    let dsp_latency_frames = dsp.latency_frames();
    dsp.reset();
    let volume: &Arc<LaneVolume> = &lane.volume;
    dsp.set_volume(&volume.gains());
    dsp.begin_pair();

    route.ring.reconfigure(channels as usize, quantum as usize);
    route.counters.sample_rate.store(rate, Ordering::Relaxed);
    route.counters.channels.store(channels, Ordering::Relaxed);

    let first_data = SinkData {
        ring: Arc::clone(&route.ring),
        counters: Arc::clone(&route.counters),
        status: Arc::clone(&route.status),
        format: AudioInfoRaw::new(),
        channels: 0,
        recycle: route.recycle.0.clone(),
        volume: Arc::clone(volume),
        // The route's own `Props` are nobody's volume: the lane's is, and it is read, not written.
        virtual_node: false,
        system_mute: Arc::clone(&lane.system_mute),
        fades_seen: volume.fades(),
        last_sound: None,
        fade_next: AtomicBool::new(false),
        dsp: Some(dsp),
    };
    let first_label = first_name.clone();
    let first_listener = first
        .add_local_listener_with_user_data(first_data)
        .state_changed(move |_stream, data, _old, new| {
            log::debug!("{first_label}: {new:?}");
            data.status.first_node_moved(&new);
            if passive && matches!(new, StreamState::Paused) {
                super::first_node_went_idle(data);
            }
            if let StreamState::Error(message) = &new {
                log::warn!("{first_label} error: {message}");
                data.status.sink_error.store(true, Ordering::Relaxed);
            }
        })
        .param_changed(on_sink_param)
        .process(on_sink_process)
        .register()
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;
    let first_values = format_pod(rate, channels, &positions);
    let Some(first_pod) = Pod::from_bytes(&first_values) else {
        return Err(AudioError::FormatNegotiation);
    };
    first
        .connect(
            libspa::utils::Direction::Input,
            None,
            first_flags,
            &mut [first_pod],
        )
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;
    let latency_values = process_latency_pod(dsp_latency_frames);
    if let Some(pod) = Pod::from_bytes(&latency_values)
        && let Err(error) = first.update_params(&mut [pod])
    {
        log::warn!("could not declare route {first_name}'s processing latency: {error}");
    }

    // ---- NODE 2: the node that drains the ring ---------------------------------------------
    let (second_name, second_props, second_flags) = match direction {
        DeviceDirection::Output => (
            slot.stream_name(),
            make_stream(),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
        DeviceDirection::Input => (
            slot.node_name(),
            make_virtual(),
            StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        ),
    };
    let second = pw::stream::StreamRc::new(core.clone(), &second_name, second_props)
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;
    let second_data = OutData {
        ring: Arc::clone(&route.ring),
        counters: Arc::clone(&route.counters),
        status: Arc::clone(&route.status),
        format: AudioInfoRaw::new(),
        channels: 0,
        scratch: vec![0.0; MAX_QUANTUM_FRAMES * MAX_CHANNELS as usize],
        // A route's source is nobody's volume either.
        volume: None,
        stops_with_the_pair: passive,
        last_block: None,
        ends_a_switch: false,
    };
    let second_label = second_name.clone();
    let second_listener = second
        .add_local_listener_with_user_data(second_data)
        .state_changed(move |_stream, data, _old, new| {
            log::debug!("{second_label}: {new:?}");
            data.status
                .output_streaming
                .store(matches!(new, StreamState::Streaming), Ordering::Relaxed);
            if let StreamState::Error(message) = &new {
                log::warn!("{second_label} error: {message}");
                data.status.output_error.store(true, Ordering::Relaxed);
            }
        })
        .param_changed(on_output_param)
        .process(on_output_process)
        .register()
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;
    let second_values = format_pod(rate, channels, &positions);
    let Some(second_pod) = Pod::from_bytes(&second_values) else {
        return Err(AudioError::FormatNegotiation);
    };
    second
        .connect(
            libspa::utils::Direction::Output,
            None,
            second_flags,
            &mut [second_pod],
        )
        .map_err(|error| AudioError::PipewireUnavailable(error.to_string()))?;

    Ok(RoutePair {
        _first_listener: first_listener,
        _second_listener: second_listener,
        first,
        _second: second,
        on: on.clone(),
        built_at: Instant::now(),
        published_latency: u32::try_from(dsp_latency_frames).unwrap_or(u32::MAX),
    })
}

/// The properties of a route's virtual node: the lane's own ([`virtual_node_props`]) — its media
/// class, format, volume bounds and all — with the route's name, the preset in its description,
/// the route's link-group, and a session priority no device is below
/// ([`app_routes::ROUTE_PRIORITY_SESSION`]).
fn route_virtual_props(
    slot: RouteSlot,
    preset: &str,
    language: Option<&str>,
    (channels, rate, positions): (u32, u32, &ChannelMap),
    latency: &str,
) -> PropertiesBox {
    let mut props =
        virtual_node_props(slot.direction, language, channels, rate, positions, latency);
    let system = locale::system_language();
    let description =
        app_routes::route_description(slot.direction, preset, language.or(system.as_deref()));
    props.insert(*pw::keys::NODE_NAME, slot.node_name());
    props.insert(*pw::keys::NODE_DESCRIPTION, description.as_str());
    props.insert(*pw::keys::NODE_NICK, description.as_str());
    props.insert(*pw::keys::NODE_LINK_GROUP, slot.link_group());
    props.insert("priority.session", app_routes::ROUTE_PRIORITY_SESSION);
    props
}

/// The properties of a route's stream on the device: the lane's own ([`stream_props`]) — linked
/// once to `target` and never moved, passive where it may be — with the route's name, the preset
/// in its description, and the route's link-group.
fn route_stream_props(
    slot: RouteSlot,
    preset: &str,
    target: &str,
    latency: &str,
    passive: bool,
) -> PropertiesBox {
    let mut props = stream_props(slot.direction, target, latency, passive);
    props.insert(*pw::keys::NODE_NAME, slot.stream_name());
    props.insert(
        *pw::keys::NODE_DESCRIPTION,
        app_routes::route_stream_description(slot.direction, preset),
    );
    props.insert(*pw::keys::NODE_LINK_GROUP, slot.link_group());
    props
}

// ---------------------------------------------------------------------------------------------
// The registry and the metadata
// ---------------------------------------------------------------------------------------------

/// A node of FxSound's appeared in the registry. A route's virtual node gives its route the serial
/// moved streams are written with; the next [`reconcile`] writes them.
pub(super) fn node_appeared(shared: &mut Shared, id: u32, serial: Option<u64>, node_name: &str) {
    let Some(slot) = RouteSlot::of_node(node_name) else {
        return;
    };
    let routes = &mut shared.routes;
    if let Some(serial) = serial {
        routes.moves.route_node(serial, slot.direction);
    }
    if let Some(route) = routes
        .live
        .iter_mut()
        .find(|route| route.slot == slot && route.pair.is_some())
    {
        log::debug!(
            "route {} is node {id}, serial {}",
            slot.node_name(),
            serial.map_or_else(|| "?".to_owned(), |serial| serial.to_string())
        );
        route.node = Some((id, serial));
    }
}

/// A node of FxSound's left the registry. A route whose virtual node it was has none until its
/// next pair's is announced.
pub(super) fn node_removed(shared: &mut Shared, id: u32) {
    for route in &mut shared.routes.live {
        if route.node.is_some_and(|(node, _)| node == id) {
            route.node = None;
        }
    }
}

/// Whether the `default` metadata names a target for the stream `id`, or is about to name one of
/// FxSound's: then the stream does not follow its lane's default.
pub(super) fn has_target(shared: &Shared, id: u32) -> bool {
    shared.routes.moves.has_key(id)
}

/// Move the stream `subject`, which follows a default FxSound holds and was linked to nothing, onto
/// FxSound's node with `serial` and back to following the default at once: a write and a delete of
/// its key, each of which has WirePlumber rescan the graph and link it (`crate::stranded`). Sent
/// and noted as this engine's own, like a route's moves, so neither report is taken for a mixer's.
pub(super) fn nudge(shared: &mut Shared, subject: u32, serial: u64) {
    send(
        shared,
        &[
            MetadataOp::Write { subject, serial },
            MetadataOp::Delete { subject },
        ],
    );
}

/// The `default` metadata says where a stream is to go ([`app_routes::TARGET_OBJECT_KEY`]), or
/// that it has no target of its own any more.
pub(super) fn metadata_target(shared: &mut Shared, subject: u32, value: Option<&str>) {
    shared.routes.moves.heard(subject, value);
}

/// An application's stream joined the graph: a player or a recorder. What a recorder's key naming
/// a playback route's sink means depends on which it is ([`Moves::monitored`]).
pub(super) fn stream_appeared(shared: &mut Shared, id: u32, direction: DeviceDirection) {
    shared.routes.moves.stream_appeared(id, direction);
}

/// An application's stream left the registry, and its keys with it.
pub(super) fn stream_removed(shared: &mut Shared, id: u32) {
    shared.routes.moves.stream_gone(id);
}

/// The engine is going: every stream FxSound moved goes back, and every key that puts a stream
/// onto one of its routes' nodes is deleted, whoever wrote it ([`Moves::everything_back`]). Returns whether
/// anything was written, and so whether there is a write for the exit's `sync` to confirm
/// ([`release_defaults_before_exit`]). The routes' nodes go afterwards, with the session.
pub(super) fn move_everything_back(shared: &mut Shared) -> bool {
    let ops = shared.routes.moves.everything_back();
    if ops.is_empty()
        || shared
            .session
            .as_ref()
            .and_then(|s| s.metadata.as_ref())
            .is_none()
    {
        return false;
    }
    log::info!(
        "moving {} application streams back from their routes",
        ops.len()
    );
    send(shared, &ops);
    true
}

/// The session is ending: every route goes with it, pairs first. The rules stay, and the routes
/// are built again from them once the next connection knows the graph.
pub(super) fn close(shared: &mut Shared) {
    let routes = &mut shared.routes;
    for route in routes
        .live
        .iter_mut()
        .chain(routes.leaving.iter_mut().map(|gone| &mut gone.route))
    {
        route.drop_pair();
    }
    routes.live.clear();
    routes.leaving.clear();
    routes.table.clear();
    routes.moves.forget_session();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_streams::ExplicitTarget;
    use fxsound_core::AppKey;

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;

    /// Route o1's virtual node is node 70 with serial 301; route i1's, node 71 with serial 401.
    const NODES: [(RouteSlot, u32, u64); 2] = [
        (RouteSlot::new(OUT, 1), 70, 301),
        (RouteSlot::new(IN, 1), 71, 401),
    ];

    fn listed(id: u32, direction: DeviceDirection, pin: Option<Pin>) -> Listed {
        Listed {
            id,
            direction,
            app: AppKey {
                name: "OBS".to_owned(),
                ..AppKey::default()
            },
            pin,
            target: None,
        }
    }

    /// A screen recorder of FxSound's sink the user points at o1's monitor records what o1 plays,
    /// by a key FxSound leaves as it is: it sits on o1, and keeps it in use.
    #[test]
    fn a_recorder_whose_key_names_a_playback_routes_sink_rests_on_that_route() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(401, IN);
        let screen = Listed {
            target: Some(ExplicitTarget::Object(crate::SINK_NODE_NAME.to_owned())),
            ..listed(90, IN, Some(Pin::Monitor))
        };
        moves.stream_appeared(90, IN);
        assert_eq!(resting_on(&mut moves, &screen, &NODES), None, "no key yet");
        moves.heard(90, Some("301"));
        assert_eq!(
            resting_on(&mut moves, &screen, &NODES),
            Some(RouteSlot::new(OUT, 1))
        );

        // The same whatever keeps FxSound from moving it, or none: one that will not be moved
        // again, and a microphone's recorder the user has pointed there by hand.
        for pin in [Some(Pin::DontReconnect), Some(Pin::DontFallback), None] {
            let recorder = listed(91, IN, pin);
            moves.stream_appeared(91, IN);
            moves.heard(91, Some("301"));
            assert_eq!(
                resting_on(&mut moves, &recorder, &NODES),
                Some(RouteSlot::new(OUT, 1)),
                "{pin:?}"
            );
            moves.stream_gone(91);
        }

        // Not while o1's node is not in the graph: a monitor that is no more is on no route.
        assert_eq!(resting_on(&mut moves, &screen, &NODES[1..]), None);
        // Nor with a key naming somebody else's sink: the user moved it off.
        moves.heard(90, Some("57"));
        assert_eq!(resting_on(&mut moves, &screen, &NODES), None);
        // And a recorder that says `node.dont-move` has its key ignored by WirePlumber: it is
        // wherever its own properties put it.
        let kiosk = listed(92, IN, Some(Pin::DontMove));
        moves.stream_appeared(92, IN);
        moves.heard(92, Some("301"));
        assert_eq!(resting_on(&mut moves, &kiosk, &NODES), None);
    }

    /// A key naming a route that puts the stream onto it is FxSound's to delete or rewrite, and
    /// no reason by itself for the stream to sit there — but for one WirePlumber will not move
    /// again, which is anchored where it was put.
    #[test]
    fn a_key_that_puts_a_stream_onto_a_route_rests_it_there_only_when_it_will_not_be_moved_again() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(401, IN);
        moves.stream_appeared(40, OUT);
        moves.stream_appeared(90, IN);
        moves.heard(40, Some("301"));
        moves.heard(90, Some("401"));
        assert_eq!(resting_on(&mut moves, &listed(40, OUT, None), &NODES), None);
        assert_eq!(resting_on(&mut moves, &listed(90, IN, None), &NODES), None);
        assert_eq!(
            resting_on(
                &mut moves,
                &listed(90, IN, Some(Pin::DontReconnect)),
                &NODES
            ),
            Some(RouteSlot::new(IN, 1))
        );
    }
}
