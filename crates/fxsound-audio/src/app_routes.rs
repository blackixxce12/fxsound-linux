//! Per-application routes: which preset each application's stream runs through, as plain data
//! the main loop plans with (`docs/0.4.0-apps.md`, "How it works on PipeWire").
//!
//! One chain cannot run two presets at once, so an application with a preset of its own is moved
//! onto a *route*: an extra pair of FxSound nodes running that preset, attached to the same real
//! device as its lane. This module decides everything about routes that needs no server — which
//! routes a lane should have, under which numbers, which stream goes onto which, when an unused
//! route goes, and what the `default` metadata must be told for each stream — and says it as a
//! plan. The engine carries the plan out (`engine::route_pairs`): it builds and drops the pairs and
//! writes and deletes the metadata keys.
//!
//! # Rules and routes
//!
//! The app sends every rule at once ([`UiToAudio::SetAppRoutes`]): an application, a lane, a
//! preset and that preset's parameters. [`Rules`] keeps them, and [`diff`] says what changed from
//! one set to the next, per preset: which are new, which are gone, whose parameters or voice chain
//! changed. A route is per preset, not per application — every application of one lane with the
//! same preset shares one pair — and exists only while a stream needs it: a rule for a game that is
//! not running builds nothing. [`RouteTable::plan`] turns the rules and the streams in the graph
//! into the routes each lane should run:
//!
//! - A stream is matched with the rule of its own lane that names its application most
//!   specifically ([`AppKey::best_match`], the order the store itself uses), and goes onto that
//!   preset's route. A stream that may not be moved — `node.dont-move`, a target of its own that is
//!   not FxSound's, a recorder of what FxSound plays (`crate::app_streams`) — or that someone has
//!   moved by hand ([`Moves::moved_by_hand`]) stays where it is.
//! - A route keeps its number for as long as it lives; a new one takes the lowest number free in
//!   its lane. A lane runs at most [`MAX_ROUTES_PER_LANE`] routes. A preset that would need one
//!   more takes the place of a route nobody uses any more, if there is one; if not, its streams stay
//!   on the lane and the plan says so ([`Overflow`]), once per application ([`OverflowWarnings`]).
//! - A route whose streams have all gone is kept for [`ROUTE_IDLE`], so a player that closes its
//!   stream between two tracks comes back to the same pair, and then goes. One whose preset no
//!   rule names any more goes at once.
//! - Routes follow their lane ([`LaneState`]): none is built while the lane has no pair, those
//!   there are kept while it is between pairs, and every one goes when the lane is switched off.
//!
//! # The metadata
//!
//! A stream is moved the way a mixer moves it: by the `target.object` key of the `default`
//! metadata object, with the stream's node id as the subject and the route node's `object.serial`
//! as the value, of type `Spa:Id` — what `pipewire-pulse` writes when `pavucontrol` moves a stream,
//! and what WirePlumber 0.5.17 follows (`linking/find-defined-target.lua`, which reads it ahead of
//! the stream's own properties unless the stream says `node.dont-move`, and relinks the stream as
//! soon as it changes). Deleting the key moves the stream back. [`Moves`] keeps what this engine
//! wrote and what the metadata says, and plans the writes and deletes that make the one the other.
//!
//! A key someone else wrote — the user moving the stream in a mixer — is theirs: the stream is not
//! moved again, and its key is never deleted. What FxSound wrote is its own on the way to the
//! server and for as long as it stays there, and no longer: the server reports each change once,
//! in the order it applies them, and nothing for a change that changes nothing, so its report
//! settles the change it reports and every one sent before it ([`Moves::heard`]). A serial FxSound
//! has since moved the stream off, or deleted, is then the user's pick like any other, when they
//! move the stream back onto it. A key naming a route node of this process's, from an
//! earlier connection to the same server, is FxSound's and is taken over. The server forgets a
//! subject's keys when the subject goes, and refuses keys for a subject that does not exist
//! (measured on PipeWire 1.6.8 with `pw-metadata`), so a stream that has gone takes its key with it.
//!
//! # What can be tested where
//!
//! Everything here is tested without a server. The engine's part is tested against a private
//! PipeWire (`graph_churn::routes`), where the route pairs, their properties and the metadata keys
//! can all be read back — but not the move itself: the move is WirePlumber's, WirePlumber never
//! runs in a private graph, and nothing links a stream there to anything.
//!
//! Everything here runs on the main loop; nothing is anywhere near a process callback.
//!
//! [`UiToAudio::SetAppRoutes`]: fxsound_core::messages::UiToAudio::SetAppRoutes

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use fxsound_core::messages::{AppRoute, RouteParams};
use fxsound_core::{AppKey, DeviceDirection, MAX_ROUTES_PER_LANE};

use crate::per_direction::PerDirection;
use crate::{CAPTURE_STREAM_DESCRIPTION, OUTPUT_STREAM_DESCRIPTION, ROUTE_NODE_PREFIX, locale};

/// How long a route nobody plays into — or records from — is kept before it goes
/// (`docs/0.4.0-apps.md`).
///
/// A player that closes its stream at the end of a track and opens one for the next, a game
/// between a menu and a level, a call that is hung up and dialled again: each comes back within
/// seconds, and finds its route still there rather than waiting for a new pair. Long enough for
/// those, short enough that a route whose application has quit does not hold a device's worth of
/// DSP for long.
pub(crate) const ROUTE_IDLE: Duration = Duration::from_secs(10);

/// The key of the `default` metadata a stream's target is written under: the one
/// `pavucontrol`'s "move stream" writes through `pipewire-pulse`, and WirePlumber reads.
pub(crate) const TARGET_OBJECT_KEY: &str = "target.object";

/// The type the target is written with: a number, the route node's `object.serial`.
pub(crate) const TARGET_OBJECT_TYPE: &str = "Spa:Id";

/// `priority.session` of a route's virtual node: below anything a session manager could prefer,
/// the lanes' own nodes and nodes that carry no priority at all (WirePlumber reads a missing one as
/// `0`, `nutils.get_session_priority`). A route is a place applications are moved to, never a
/// device the session falls back to: were a lane's device to go with nothing else left, WirePlumber
/// would otherwise make a route the default sink, and every application would follow it onto one
/// application's preset.
pub(crate) const ROUTE_PRIORITY_SESSION: &str = "-1";

/// One route's place in its lane: the lane, and a number from `1` to [`MAX_ROUTES_PER_LANE`].
///
/// The number is what the route's nodes are named by, so it stays with the route for as long as
/// the route lives, and a number set free is taken again by the next route of the lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RouteSlot {
    pub(crate) direction: DeviceDirection,
    pub(crate) number: usize,
}

impl RouteSlot {
    pub(crate) const fn new(direction: DeviceDirection, number: usize) -> Self {
        Self { direction, number }
    }

    /// The letter a route's names carry for its lane: `o` in front of the speakers, `i` behind
    /// the microphone.
    const fn letter(self) -> char {
        match self.direction {
            DeviceDirection::Output => 'o',
            DeviceDirection::Input => 'i',
        }
    }

    /// `node.name` of the route's virtual node — `fxsound_route_o<N>`, the sink applications are
    /// moved onto, or `fxsound_route_i<N>`, the source recorders are moved onto. Its
    /// `object.serial` is what a moved stream's metadata target names.
    pub(crate) fn node_name(self) -> String {
        format!("{ROUTE_NODE_PREFIX}{}{}", self.letter(), self.number)
    }

    /// `node.name` of the route's stream on the real device: `fxsound_route_o<N>_play`, playing
    /// the route's chain into the output lane's device, or `fxsound_route_i<N>_capture`,
    /// recording the input lane's.
    pub(crate) fn stream_name(self) -> String {
        let suffix = match self.direction {
            DeviceDirection::Output => "play",
            DeviceDirection::Input => "capture",
        };
        format!("{}_{suffix}", self.node_name())
    }

    /// The `node.link-group` of the route's two nodes, `fxsound-route-o<N>` or
    /// `fxsound-route-i<N>`: a group of the route's own, for the reasons each lane has its own —
    /// WirePlumber links no member of a group to another, so the route's stream is never linked
    /// into the route's own node, and the server runs a group together, so the route's two nodes
    /// run exactly when something plays into it, or records from it.
    pub(crate) fn link_group(self) -> String {
        format!("fxsound-route-{}{}", self.letter(), self.number)
    }

    /// The route whose virtual node is called `node_name`, if it is one: not its stream on the
    /// device, and no number outside `1..=MAX_ROUTES_PER_LANE`.
    pub(crate) fn of_node(node_name: &str) -> Option<Self> {
        let rest = node_name.strip_prefix(ROUTE_NODE_PREFIX)?;
        let mut chars = rest.chars();
        let direction = match chars.next()? {
            'o' => DeviceDirection::Output,
            'i' => DeviceDirection::Input,
            _ => return None,
        };
        let digits = chars.as_str();
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let number = digits.parse::<usize>().ok()?;
        (1..=MAX_ROUTES_PER_LANE)
            .contains(&number)
            .then_some(Self::new(direction, number))
    }
}

/// `node.description` of a route's virtual node, as a mixer lists it: the lane's own node's
/// description in `language` (`FxSound (Output)`, `FxSound (Вывод)`), a middle dot, and the preset.
#[must_use]
pub(crate) fn route_description(
    direction: DeviceDirection,
    preset: &str,
    language: Option<&str>,
) -> String {
    format!(
        "{} · {preset}",
        locale::node_description(direction, language)
    )
}

/// `node.description` of a route's stream on the device, as the lane's own stream's is — not
/// localised, and listed under the route's node by its link-group — with the preset after it.
#[must_use]
pub(crate) fn route_stream_description(direction: DeviceDirection, preset: &str) -> String {
    let lane = match direction {
        DeviceDirection::Output => OUTPUT_STREAM_DESCRIPTION,
        DeviceDirection::Input => CAPTURE_STREAM_DESCRIPTION,
    };
    format!("{lane} · {preset}")
}

/// A preset a route runs: what its chain is built from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RoutePreset {
    pub(crate) direction: DeviceDirection,
    /// The preset's name, as the rules give it.
    pub(crate) name: String,
    /// Its parameters, sanitised.
    pub(crate) params: RouteParams,
    /// The voice chain an input route runs, by name ([`fxsound_dsp::ChainSpec::by_name`]); empty
    /// for an output route, which has no chain to choose.
    pub(crate) chain: String,
}

/// Why a rule of [`UiToAudio::SetAppRoutes`] was not taken.
///
/// [`UiToAudio::SetAppRoutes`]: fxsound_core::messages::UiToAudio::SetAppRoutes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The parameters are for the other lane's chain ([`AppRoute::is_consistent`]).
    Inconsistent,
    /// No preset is named: there would be nothing to call the route, and "follow the lane" is
    /// said by sending no rule at all.
    NoPreset,
    /// The key names no application, so no stream could match it.
    NoApplication,
}

/// The rules the app last sent ([`UiToAudio::SetAppRoutes`]), as the engine keeps them: every
/// rule it can act on, in the order they came, each preset's parameters sanitised.
///
/// [`UiToAudio::SetAppRoutes`]: fxsound_core::messages::UiToAudio::SetAppRoutes
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Rules {
    rules: Vec<AppRoute>,
}

impl Rules {
    /// The rules of `asked` the engine can act on, and each refused one with why.
    ///
    /// A preset's name is trimmed, since it names a node to the user and groups rules into
    /// routes, and two spellings that differ only by a space are one preset. Every snapshot is
    /// sanitised, as [`crate::EngineHandle::set_params`] sanitises the lanes', because this is the
    /// one gate between the message and a route's chain.
    pub(crate) fn new(asked: Vec<AppRoute>) -> (Self, Vec<(AppRoute, Refusal)>) {
        let mut rules = Vec::with_capacity(asked.len());
        let mut refused = Vec::new();
        for mut rule in asked {
            let refusal = if !rule.is_consistent() {
                Some(Refusal::Inconsistent)
            } else if rule.preset.trim().is_empty() {
                Some(Refusal::NoPreset)
            } else if rule.app.is_empty() {
                Some(Refusal::NoApplication)
            } else {
                None
            };
            if let Some(refusal) = refusal {
                refused.push((rule, refusal));
                continue;
            }
            rule.preset = rule.preset.trim().to_owned();
            rule.params.sanitise();
            rule.chain = rule.chain.trim().to_owned();
            rules.push(rule);
        }
        (Self { rules }, refused)
    }

    /// How many rules there are.
    pub(crate) const fn len(&self) -> usize {
        self.rules.len()
    }

    /// The preset the application `app` runs through in `direction`: the one its most specific
    /// rule of that lane names, or `None` when no rule of that lane names it.
    pub(crate) fn preset_for(&self, direction: DeviceDirection, app: &AppKey) -> Option<&str> {
        let lane: Vec<&AppRoute> = self
            .rules
            .iter()
            .filter(|rule| rule.direction == direction)
            .collect();
        let best = app.best_match(lane.iter().map(|rule| &rule.app))?;
        lane.get(best).map(|rule| rule.preset.as_str())
    }

    /// The preset `name` of `direction`, as its first rule gives it. The app resolves one preset
    /// to one set of parameters; were two rules to disagree, the first is the one a route runs.
    pub(crate) fn preset(&self, direction: DeviceDirection, name: &str) -> Option<RoutePreset> {
        self.rules
            .iter()
            .find(|rule| rule.direction == direction && rule.preset == name)
            .map(|rule| RoutePreset {
                direction,
                name: rule.preset.clone(),
                params: rule.params,
                chain: match direction {
                    DeviceDirection::Output => String::new(),
                    DeviceDirection::Input => rule.chain.clone(),
                },
            })
    }

    /// Whether any rule of `direction` names the preset `name`.
    pub(crate) fn names(&self, direction: DeviceDirection, name: &str) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.direction == direction && rule.preset == name)
    }

    /// Every preset the rules name, once, in the order they are first named.
    pub(crate) fn presets(&self) -> Vec<RoutePreset> {
        let mut presets: Vec<RoutePreset> = Vec::new();
        for rule in &self.rules {
            if presets.iter().all(|known| {
                (known.direction, known.name.as_str()) != (rule.direction, &rule.preset)
            }) && let Some(preset) = self.preset(rule.direction, &rule.preset)
            {
                presets.push(preset);
            }
        }
        presets
    }

    /// Which application runs which preset, in one order whatever order the rules came in: what
    /// tells a set that moves an application from one that only changes a preset's parameters.
    fn assignments(&self) -> Vec<(DeviceDirection, AppKey, String)> {
        let mut assignments: Vec<(DeviceDirection, AppKey, String)> = self
            .rules
            .iter()
            .map(|rule| (rule.direction, rule.app.clone(), rule.preset.clone()))
            .collect();
        assignments.sort_by(|a, b| {
            (a.0.key(), &a.1.flatpak, &a.1.binary, &a.1.name, &a.2).cmp(&(
                b.0.key(),
                &b.1.flatpak,
                &b.1.binary,
                &b.1.name,
                &b.2,
            ))
        });
        assignments.dedup();
        assignments
    }
}

/// What changed from one set of rules to the next ([`diff`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RulesDiff {
    /// Presets the new set names and the old did not: routes that can now be built, once a
    /// stream needs one.
    pub(crate) added: Vec<(DeviceDirection, String)>,
    /// Presets the old set named and the new does not: their routes go at once, their streams
    /// back to their lanes first.
    pub(crate) removed: Vec<(DeviceDirection, String)>,
    /// Presets both name, whose parameters changed: written into a running route's buffer, on
    /// the main loop, with no new pair.
    pub(crate) params: Vec<RoutePreset>,
    /// Input presets both name, whose voice chain changed: handed over to a running route's
    /// audio thread as a new engine, as the input lane's chain is.
    pub(crate) chains: Vec<RoutePreset>,
    /// Which application runs which preset changed.
    pub(crate) applications: bool,
}

impl RulesDiff {
    /// Whether the two sets were the same in everything a route depends on.
    pub(crate) fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.params.is_empty()
            && self.chains.is_empty()
            && !self.applications
    }
}

/// What changed from `old` to `new`, preset by preset (`docs/0.4.0-apps.md`: the full set arrives
/// each time, and the engine diffs it). The order of the rules does not matter; a voice chain's
/// name is compared without regard to case, as [`fxsound_dsp::ChainSpec::by_name`] reads it.
pub(crate) fn diff(old: &Rules, new: &Rules) -> RulesDiff {
    let before = old.presets();
    let after = new.presets();
    let find = |presets: &[RoutePreset], preset: &RoutePreset| {
        presets
            .iter()
            .find(|known| known.direction == preset.direction && known.name == preset.name)
            .cloned()
    };
    let mut changes = RulesDiff {
        applications: old.assignments() != new.assignments(),
        ..RulesDiff::default()
    };
    for preset in &after {
        match find(&before, preset) {
            None => changes.added.push((preset.direction, preset.name.clone())),
            Some(was) => {
                if was.params != preset.params {
                    changes.params.push(preset.clone());
                }
                if !was.chain.eq_ignore_ascii_case(&preset.chain) {
                    changes.chains.push(preset.clone());
                }
            }
        }
    }
    for preset in &before {
        if find(&after, preset).is_none() {
            changes
                .removed
                .push((preset.direction, preset.name.clone()));
        }
    }
    changes
}

/// A lane, as its routes see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaneState {
    /// The lane has a pair on a device: routes are built beside it, on the same device.
    Attached,
    /// The lane is on and has had a device, but has no pair right now: it is being rebuilt, is
    /// backing off after a failure, or waits for its device to come back. Its routes are kept as
    /// they are, streams and all, and no new one is built until it has a pair again.
    Between,
    /// The lane is off, or has no device to be on: none of its routes is kept.
    Off,
}

/// One application stream, as the routes see it: its node id, its lane, who it is, and whether it
/// may be moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) id: u32,
    pub(crate) direction: DeviceDirection,
    pub(crate) app: AppKey,
    /// Neither pinned where it is (`crate::app_streams::Pin`) nor moved by hand
    /// ([`Moves::moved_by_hand`]).
    pub(crate) movable: bool,
}

/// Why a route goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Teardown {
    /// No rule of its lane names its preset any more.
    RuleGone,
    /// Nothing has played into it — or recorded from it — for [`ROUTE_IDLE`].
    Idle,
    /// Its lane was switched off, or has no device left.
    LaneOff,
    /// A preset whose streams are here now needed its place, and it had none.
    Evicted,
}

impl Teardown {
    /// What the log says.
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::RuleGone => "no rule names its preset any more",
            Self::Idle => "nothing has used it for a while",
            Self::LaneOff => "its lane has no device",
            Self::Evicted => "another preset needed its place",
        }
    }
}

/// A stream whose preset could not get a route: its lane runs [`MAX_ROUTES_PER_LANE`] already,
/// every one of them in use. It stays on its lane's chain.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Overflow {
    pub(crate) direction: DeviceDirection,
    pub(crate) preset: String,
    /// The application, as the window names it ([`AppKey::display`]).
    pub(crate) app: String,
}

/// One route the table keeps: its place, its preset, and since when nothing has used it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Slotted {
    pub(crate) slot: RouteSlot,
    pub(crate) preset: String,
    /// When its last stream left; `None` while it has one.
    pub(crate) idle_since: Option<Instant>,
}

/// What one run of [`RouteTable::plan`] asks of the engine, in the order it is to be done:
/// the streams that leave a route are moved back, the routes that go are taken down, the new
/// routes are built, and the streams that go onto one are moved onto it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Plan {
    /// Routes to take down, with why. Their streams' keys are deleted first.
    pub(crate) teardown: Vec<(Slotted, Teardown)>,
    /// Routes to build, each for its preset.
    pub(crate) build: Vec<(RouteSlot, String)>,
    /// Every stream that is to be on a route, by node id, and the route. Ordered by id.
    pub(crate) assigned: Vec<(u32, RouteSlot)>,
    /// Every stream whose preset could not get a route.
    pub(crate) overflow: Vec<Overflow>,
}

/// Every route of both lanes: which preset each runs, under which number, and since when nothing
/// has used it. The pairs themselves are the engine's (`engine::route_pairs`); this is what
/// decides them.
#[derive(Debug, Clone, Default)]
pub(crate) struct RouteTable {
    routes: Vec<Slotted>,
}

impl RouteTable {
    /// Every route kept, in the order they were made.
    #[cfg(test)]
    pub(crate) fn routes(&self) -> &[Slotted] {
        &self.routes
    }

    /// The route running `preset` in `direction`, if there is one.
    pub(crate) fn slot_of(&self, direction: DeviceDirection, preset: &str) -> Option<RouteSlot> {
        self.routes
            .iter()
            .find(|route| route.slot.direction == direction && route.preset == preset)
            .map(|route| route.slot)
    }

    /// The preset the route in `slot` runs, if there is one there.
    #[cfg(test)]
    pub(crate) fn preset_of(&self, slot: RouteSlot) -> Option<&str> {
        self.routes
            .iter()
            .find(|route| route.slot == slot)
            .map(|route| route.preset.as_str())
    }

    /// Forget every route: the connection they were made on has gone, and they with it.
    pub(crate) fn clear(&mut self) {
        self.routes.clear();
    }

    /// Decide the routes both lanes should run now, and which stream goes onto which, from the
    /// rules, every application stream in the graph, and where each lane stands (module docs,
    /// "Rules and routes"). The table is brought up to date with the plan as it is made: what it
    /// holds afterwards is what the engine is to have once it has carried the plan out.
    pub(crate) fn plan(
        &mut self,
        rules: &Rules,
        streams: &[Candidate],
        lanes: &PerDirection<LaneState>,
        now: Instant,
        idle: Duration,
    ) -> Plan {
        let mut plan = Plan::default();
        for direction in DeviceDirection::ALL {
            self.plan_lane(
                direction,
                rules,
                streams,
                *lanes.get(direction),
                now,
                idle,
                &mut plan,
            );
        }
        plan.assigned.sort_by_key(|&(id, _)| id);
        plan
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_lane(
        &mut self,
        direction: DeviceDirection,
        rules: &Rules,
        streams: &[Candidate],
        lane: LaneState,
        now: Instant,
        idle: Duration,
        plan: &mut Plan,
    ) {
        // A lane that is off keeps nothing, and moves nothing.
        if lane == LaneState::Off {
            self.take_down(
                |route| route.slot.direction == direction,
                Teardown::LaneOff,
                plan,
            );
            return;
        }

        // Which preset each stream that may move wants, grouped by preset in the order of each
        // preset's first stream, streams by id — so the same graph always plans the same way.
        let mut movable: Vec<&Candidate> = streams
            .iter()
            .filter(|stream| stream.direction == direction && stream.movable)
            .collect();
        movable.sort_by_key(|stream| stream.id);
        let mut wanted: Vec<(String, Vec<&Candidate>)> = Vec::new();
        for stream in movable {
            let Some(preset) = rules.preset_for(direction, &stream.app) else {
                continue;
            };
            match wanted.iter_mut().find(|(name, _)| name == preset) {
                Some((_, members)) => members.push(stream),
                None => wanted.push((preset.to_owned(), vec![stream])),
            }
        }

        // A route whose preset no rule of the lane names any more goes at once.
        self.take_down(
            |route| route.slot.direction == direction && !rules.names(direction, &route.preset),
            Teardown::RuleGone,
            plan,
        );

        // A route with streams is in use; one without has been idle since its last one left, and
        // goes once that is long enough ago.
        for route in self
            .routes
            .iter_mut()
            .filter(|route| route.slot.direction == direction)
        {
            let used = wanted.iter().any(|(preset, _)| *preset == route.preset);
            route.idle_since = if used {
                None
            } else {
                Some(route.idle_since.unwrap_or(now))
            };
        }
        self.take_down(
            |route| {
                route.slot.direction == direction
                    && route
                        .idle_since
                        .is_some_and(|since| now.saturating_duration_since(since) >= idle)
            },
            Teardown::Idle,
            plan,
        );

        for (preset, members) in wanted {
            let slot = match self.slot_of(direction, &preset) {
                Some(slot) => Some(slot),
                // No new route while the lane has no pair to build it beside; its streams stay
                // where they are until it has one, and that is no overflow.
                None if lane == LaneState::Between => continue,
                None => self.make_room(direction, &preset, plan),
            };
            match slot {
                Some(slot) => plan
                    .assigned
                    .extend(members.iter().map(|stream| (stream.id, slot))),
                None => {
                    for stream in members {
                        let overflow = Overflow {
                            direction,
                            preset: preset.clone(),
                            app: stream.app.display().to_owned(),
                        };
                        if !plan.overflow.contains(&overflow) {
                            plan.overflow.push(overflow);
                        }
                    }
                }
            }
        }
    }

    /// A place for a new route of `preset` in `direction`: the lowest number free, or the place of
    /// the route that has been unused longest, which goes. `None` when every place is taken by a
    /// route in use.
    fn make_room(
        &mut self,
        direction: DeviceDirection,
        preset: &str,
        plan: &mut Plan,
    ) -> Option<RouteSlot> {
        let free = (1..=MAX_ROUTES_PER_LANE)
            .map(|number| RouteSlot::new(direction, number))
            .find(|slot| self.routes.iter().all(|route| route.slot != *slot));
        let slot = match free {
            Some(slot) => slot,
            None => {
                let longest_idle = self
                    .routes
                    .iter()
                    .filter(|route| route.slot.direction == direction)
                    .filter_map(|route| route.idle_since.map(|since| (since, route.slot)))
                    .min_by_key(|&(since, slot)| (since, slot.number))
                    .map(|(_, slot)| slot)?;
                self.take_down(|route| route.slot == longest_idle, Teardown::Evicted, plan);
                longest_idle
            }
        };
        self.routes.push(Slotted {
            slot,
            preset: preset.to_owned(),
            idle_since: None,
        });
        plan.build.push((slot, preset.to_owned()));
        Some(slot)
    }

    /// Take every route `which` picks out of the table, into the plan's teardowns.
    fn take_down(&mut self, which: impl Fn(&Slotted) -> bool, why: Teardown, plan: &mut Plan) {
        let mut index = 0;
        while index < self.routes.len() {
            if which(&self.routes[index]) {
                plan.teardown.push((self.routes.remove(index), why));
            } else {
                index += 1;
            }
        }
    }
}

/// One change to the `default` metadata ([`Moves::plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataOp {
    /// Write the stream's `target.object`: the route node's serial.
    Write { subject: u32, serial: u64 },
    /// Delete the stream's `target.object`: it goes back to wherever the session sends it.
    Delete { subject: u32 },
}

impl MetadataOp {
    /// The stream the change is for.
    #[cfg(test)]
    pub(crate) const fn subject(self) -> u32 {
        match self {
            Self::Write { subject, .. } | Self::Delete { subject } => subject,
        }
    }
}

/// What the `default` metadata says about where each application stream goes, and which of it is
/// FxSound's (module docs, "The metadata").
#[derive(Debug, Clone, Default)]
pub(crate) struct Moves {
    /// The route node's serial this engine last wrote for each stream, and stands behind.
    written: HashMap<u32, u64>,
    /// Every change this engine has sent for each stream that the server has not reported back
    /// yet, oldest first: `Some(serial)` a write, `None` a delete. What makes the report of a write
    /// still on its way FxSound's after a later write was sent — and only until the server has
    /// reported a later change of FxSound's ([`Self::settle`]): from then on a serial this engine
    /// moved the stream off, or deleted, is no more FxSound's than any other.
    unconfirmed: HashMap<u32, VecDeque<Option<u64>>>,
    /// The `target.object` the metadata holds for each subject, as its events report it.
    held: HashMap<u32, Held>,
    /// The serial of every route node this process has made, on any connection. A serial is never
    /// handed to another object.
    ours: HashSet<u64>,
    /// Those of [`Self::ours`] whose node is in the graph now.
    live: HashSet<u64>,
}

/// A subject's `target.object`, as the metadata last reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    value: String,
    /// The report was the server applying a write of this engine's: the key is FxSound's until
    /// the next report replaces it.
    echo: bool,
}

impl Moves {
    /// A route node of ours has appeared with this serial.
    pub(crate) fn route_node(&mut self, serial: u64) {
        self.ours.insert(serial);
        self.live.insert(serial);
    }

    /// The route node of ours with this serial has gone.
    pub(crate) fn route_node_gone(&mut self, serial: u64) {
        self.live.remove(&serial);
    }

    /// The metadata says the stream `subject`'s target is now `value`, or that it has none.
    ///
    /// A report that says what a change of this engine's still unreported says is that change,
    /// and settles it ([`Self::settle`]). One that no change of FxSound's accounts for is someone
    /// else's move, and what this engine wrote before it is theirs to undo, not this engine's.
    pub(crate) fn heard(&mut self, subject: u32, value: Option<&str>) {
        let Some(value) = value.map(str::trim) else {
            self.settle(subject, None);
            self.held.remove(&subject);
            return;
        };
        let serial = value.parse::<u64>().ok();
        let echo = serial.is_some_and(|serial| self.settle(subject, Some(serial)));
        if echo {
            let later = self.unconfirmed.get(&subject);
            if later.is_some_and(|later| later.contains(&None)) {
                // A delete of this engine's follows, and the key with it: the write it undoes is
                // taken for gone already — not held against the stream meanwhile, nor deleted a
                // second time.
                self.held.remove(&subject);
                return;
            }
            if later.is_none()
                && let Some(serial) = serial
            {
                // Nothing sent after it: the server holds what this engine last sent, and it
                // stands behind that — even where a move of someone else's reached the server
                // first and was overtaken by this write.
                self.written.insert(subject, serial);
            }
        } else if !self.names_ours(subject, value) {
            self.written.remove(&subject);
        }
        self.held.insert(
            subject,
            Held {
                value: value.to_owned(),
                echo,
            },
        );
    }

    /// Whether `change` — `Some(serial)` a write, `None` a delete — is the report of one this
    /// engine sent for `subject` and has not heard back yet. When it is, that one and every one
    /// sent before it are settled, and forgotten.
    ///
    /// The server applies changes in the order they reach it, reports each one it applies once and
    /// in that order, and reports nothing for a change that changes nothing — a write of the value
    /// it holds, a delete of a key it does not (measured on PipeWire 1.6.8 with `pw-metadata -m`).
    /// So the report is matched with the oldest change still unreported that says the same, and a
    /// change sent before that one will never be reported now: it either was already, or changed
    /// nothing.
    fn settle(&mut self, subject: u32, change: Option<u64>) -> bool {
        let Some(unconfirmed) = self.unconfirmed.get_mut(&subject) else {
            return false;
        };
        let Some(index) = unconfirmed.iter().position(|sent| *sent == change) else {
            return false;
        };
        unconfirmed.drain(..=index);
        if unconfirmed.is_empty() {
            self.unconfirmed.remove(&subject);
        }
        true
    }

    /// The stream went, and with it every key the server held for it.
    pub(crate) fn stream_gone(&mut self, id: u32) {
        self.written.remove(&id);
        self.unconfirmed.remove(&id);
        self.held.remove(&id);
    }

    /// The connection went, and every route node with it. What the server holds is read again
    /// from the next one; which route nodes were this process's is not forgotten.
    pub(crate) fn forget_session(&mut self) {
        self.written.clear();
        self.unconfirmed.clear();
        self.held.clear();
        self.live.clear();
    }

    /// Whether the stream's target was set by someone else — a mixer moving it, the application
    /// itself — rather than by FxSound: a move FxSound leaves alone.
    pub(crate) fn moved_by_hand(&self, id: u32) -> bool {
        self.held
            .get(&id)
            .is_some_and(|held| !self.holds_ours(id, held))
    }

    /// The serial this engine last wrote for the stream `id`, while it stands behind it.
    pub(crate) fn written(&self, id: u32) -> Option<u64> {
        self.written.get(&id).copied()
    }

    /// Whether `held`, what the metadata holds for `subject`, is FxSound's: the report of a write
    /// of this engine's, or a value [`Self::names_ours`].
    fn holds_ours(&self, subject: u32, held: &Held) -> bool {
        held.echo || self.names_ours(subject, &held.value)
    }

    /// Whether `value`, held for `subject`, is FxSound's without being the report of a write of
    /// this engine's: the serial this engine stands behind for it, or one of a route node of this
    /// process's that is gone — an earlier connection's route, or a route since rebuilt. A route
    /// node that is still there, named by a key no change of this engine's accounts for, is the
    /// user's pick: they moved the stream onto it by hand.
    fn names_ours(&self, subject: u32, value: &str) -> bool {
        let Ok(serial) = value.trim().parse::<u64>() else {
            return false;
        };
        self.written.get(&subject) == Some(&serial)
            || (self.ours.contains(&serial) && !self.live.contains(&serial))
    }

    /// Whether the key the metadata holds for `id`, or the one this engine last wrote there, is
    /// FxSound's to delete.
    fn ours_to_delete(&self, id: u32) -> bool {
        match self.held.get(&id) {
            Some(held) => self.holds_ours(id, held),
            None => self.written.contains_key(&id),
        }
    }

    /// The writes and deletes that bring the metadata to `wanted`: each stream there onto the
    /// route node with that serial — or, where the serial is `None` because the route's node has
    /// not been announced yet, left as it is — and every other stream FxSound has moved back.
    /// Deletes first, then writes, each in order of subject.
    pub(crate) fn plan(&self, wanted: &[(u32, Option<u64>)]) -> Vec<MetadataOp> {
        let mut deletes: Vec<u32> = self
            .written
            .keys()
            .chain(self.held.keys())
            .copied()
            .filter(|id| !wanted.iter().any(|(wanted, _)| wanted == id))
            .filter(|&id| self.ours_to_delete(id))
            .collect();
        deletes.sort_unstable();
        deletes.dedup();
        let mut ops: Vec<MetadataOp> = deletes
            .into_iter()
            .map(|subject| MetadataOp::Delete { subject })
            .collect();
        let mut writes: Vec<(u32, u64)> = wanted
            .iter()
            .filter_map(|&(id, serial)| serial.map(|serial| (id, serial)))
            .filter(|&(id, serial)| self.written.get(&id) != Some(&serial))
            .collect();
        writes.sort_unstable();
        ops.extend(
            writes
                .into_iter()
                .map(|(subject, serial)| MetadataOp::Write { subject, serial }),
        );
        ops
    }

    /// Every key FxSound holds, to delete: the engine is going.
    pub(crate) fn everything_back(&self) -> Vec<MetadataOp> {
        self.plan(&[])
    }

    /// `op` has been sent.
    pub(crate) fn sent(&mut self, op: MetadataOp) {
        let (subject, change) = match op {
            MetadataOp::Write { subject, serial } => {
                self.written.insert(subject, serial);
                (subject, Some(serial))
            }
            MetadataOp::Delete { subject } => {
                self.written.remove(&subject);
                // Deleted as far as this engine is concerned, and the server's report confirms it.
                // Kept until then, it would be deleted again on every plan in between.
                self.held.remove(&subject);
                (subject, None)
            }
        };
        self.unconfirmed
            .entry(subject)
            .or_default()
            .push_back(change);
    }
}

/// Which applications the window has been told could not get a route, so each is told once for as
/// long as it goes on not getting one (`docs/0.4.0-apps.md`: "the window says why").
#[derive(Debug, Clone, Default)]
pub(crate) struct OverflowWarnings {
    told: HashSet<Overflow>,
}

impl OverflowWarnings {
    /// The overflows of this plan the window has not been told of yet. Those it was told of and
    /// that are over — the application got its route, quit, or lost its rule — are forgotten, so
    /// the next time is news again.
    pub(crate) fn news(&mut self, overflow: &[Overflow]) -> Vec<Overflow> {
        self.told.retain(|told| overflow.contains(told));
        overflow
            .iter()
            .filter(|overflow| self.told.insert((*overflow).clone()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::messages::{DspParams, InputDspParams};

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;

    fn app(binary: &str, name: &str, flatpak: &str) -> AppKey {
        AppKey {
            binary: binary.to_owned(),
            name: name.to_owned(),
            flatpak: flatpak.to_owned(),
        }
    }

    fn named(name: &str) -> AppKey {
        app("", name, "")
    }

    fn output_rule(app: AppKey, preset: &str, gain: f32) -> AppRoute {
        AppRoute {
            direction: OUT,
            app,
            preset: preset.to_owned(),
            params: RouteParams::Output(DspParams {
                master_gain_db: gain,
                ..DspParams::default()
            }),
            chain: String::new(),
        }
    }

    fn input_rule(app: AppKey, preset: &str, chain: &str) -> AppRoute {
        AppRoute {
            direction: IN,
            app,
            preset: preset.to_owned(),
            params: RouteParams::Input(InputDspParams::default()),
            chain: chain.to_owned(),
        }
    }

    fn rules(asked: Vec<AppRoute>) -> Rules {
        let (rules, refused) = Rules::new(asked);
        assert!(refused.is_empty(), "{refused:?}");
        rules
    }

    fn stream(id: u32, direction: DeviceDirection, app: AppKey) -> Candidate {
        Candidate {
            id,
            direction,
            app,
            movable: true,
        }
    }

    fn both(state: LaneState) -> PerDirection<LaneState> {
        PerDirection {
            output: state,
            input: state,
        }
    }

    const ATTACHED: LaneState = LaneState::Attached;

    fn slot(direction: DeviceDirection, number: usize) -> RouteSlot {
        RouteSlot::new(direction, number)
    }

    fn built(plan: &Plan) -> Vec<(RouteSlot, &str)> {
        plan.build
            .iter()
            .map(|(slot, preset)| (*slot, preset.as_str()))
            .collect()
    }

    fn gone(plan: &Plan) -> Vec<(RouteSlot, Teardown)> {
        plan.teardown
            .iter()
            .map(|(route, why)| (route.slot, *why))
            .collect()
    }

    // ---- names ---------------------------------------------------------------------------

    #[test]
    fn a_route_is_named_as_the_contract_names_it() {
        let output = slot(OUT, 1);
        assert_eq!(output.node_name(), "fxsound_route_o1");
        assert_eq!(output.stream_name(), "fxsound_route_o1_play");
        assert_eq!(output.link_group(), "fxsound-route-o1");
        let input = slot(IN, 4);
        assert_eq!(input.node_name(), "fxsound_route_i4");
        assert_eq!(input.stream_name(), "fxsound_route_i4_capture");
        assert_eq!(input.link_group(), "fxsound-route-i4");
    }

    #[test]
    fn every_route_node_is_one_of_fxsounds_own_and_never_a_device() {
        for direction in DeviceDirection::ALL {
            for number in 1..=MAX_ROUTES_PER_LANE {
                let slot = slot(direction, number);
                assert!(crate::is_fxsound_node(&slot.node_name()));
                assert!(crate::is_fxsound_node(&slot.stream_name()));
                let name = slot.node_name();
                let props = [("media.class", "Audio/Sink"), ("node.name", name.as_str())];
                let get = |key: &str| {
                    props
                        .iter()
                        .find(|(name, _)| *name == key)
                        .map(|(_, value)| *value)
                };
                assert!(crate::DeviceInfo::from_props(1, &get).is_none());
            }
        }
    }

    #[test]
    fn a_routes_virtual_node_is_found_by_its_name_and_nothing_else_is() {
        for direction in DeviceDirection::ALL {
            for number in 1..=MAX_ROUTES_PER_LANE {
                let slot = slot(direction, number);
                assert_eq!(RouteSlot::of_node(&slot.node_name()), Some(slot));
                assert_eq!(RouteSlot::of_node(&slot.stream_name()), None);
            }
        }
        for name in [
            "fxsound_route_o0",
            "fxsound_route_o5",
            "fxsound_route_x1",
            "fxsound_route_o",
            "fxsound_route_o+1",
            "fxsound_route_o1a",
            "fxsound_sink",
            "fxsound_route_o01x",
            "",
        ] {
            assert_eq!(RouteSlot::of_node(name), None, "{name}");
        }
    }

    #[test]
    fn a_route_is_described_as_its_lane_with_the_preset_after_it() {
        assert_eq!(
            route_description(OUT, "Gaming", Some("en")),
            "FxSound (Output) · Gaming"
        );
        assert_eq!(
            route_description(IN, "Headset", Some("en")),
            "FxSound (Input) · Headset"
        );
        assert_eq!(
            route_description(OUT, "Gaming", Some("ru")),
            "FxSound (Вывод) · Gaming"
        );
        assert_eq!(
            route_stream_description(OUT, "Gaming"),
            "FxSound output · Gaming"
        );
        assert_eq!(
            route_stream_description(IN, "Headset"),
            "FxSound capture · Headset"
        );
    }

    // ---- rules ---------------------------------------------------------------------------

    #[test]
    fn rules_the_engine_cannot_act_on_are_refused_with_why() {
        let mut crossed = output_rule(named("Game"), "Gaming", 1.0);
        crossed.params = RouteParams::Input(InputDspParams::default());
        let (rules, refused) = Rules::new(vec![
            crossed,
            output_rule(named("Game"), "  ", 1.0),
            output_rule(AppKey::default(), "Gaming", 1.0),
            output_rule(named("Game"), " Gaming ", 1.0),
        ]);
        assert_eq!(
            refused.iter().map(|(_, why)| *why).collect::<Vec<_>>(),
            vec![
                Refusal::Inconsistent,
                Refusal::NoPreset,
                Refusal::NoApplication
            ]
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules.preset_for(OUT, &named("Game")),
            Some("Gaming"),
            "a preset's name is trimmed"
        );
    }

    #[test]
    fn a_routes_parameters_are_sanitised_before_any_chain_can_see_them() {
        let rules = rules(vec![output_rule(named("Game"), "Gaming", f32::NAN)]);
        let preset = rules.preset(OUT, "Gaming").expect("named");
        let RouteParams::Output(params) = preset.params else {
            panic!("an output preset");
        };
        assert!(params.master_gain_db.is_finite());
    }

    #[test]
    fn a_stream_takes_the_preset_of_the_most_specific_rule_of_its_own_lane() {
        let rules = rules(vec![
            output_rule(named("Chromium"), "Movies", 1.0),
            output_rule(app("brave", "", ""), "Volume Boost", 1.0),
            input_rule(app("brave", "", ""), "Headset", "voice"),
        ]);
        let brave = app("brave", "Chromium", "");
        assert_eq!(rules.preset_for(OUT, &brave), Some("Volume Boost"));
        assert_eq!(rules.preset_for(IN, &brave), Some("Headset"));
        assert_eq!(
            rules.preset_for(OUT, &app("chromium", "Chromium", "")),
            Some("Movies")
        );
        assert_eq!(rules.preset_for(IN, &named("Chromium")), None);
        assert_eq!(rules.preset_for(OUT, &named("mpv")), None);
    }

    #[test]
    fn every_preset_is_listed_once_per_lane_as_its_first_rule_gives_it() {
        let rules = rules(vec![
            output_rule(named("A"), "Gaming", 1.0),
            output_rule(named("B"), "Gaming", 2.0),
            input_rule(named("C"), "Gaming", "podcast"),
            output_rule(named("D"), "Movies", 1.0),
        ]);
        let presets = rules.presets();
        assert_eq!(
            presets
                .iter()
                .map(|preset| (preset.direction, preset.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(OUT, "Gaming"), (IN, "Gaming"), (OUT, "Movies")]
        );
        assert_eq!(
            presets[0].params,
            RouteParams::Output(DspParams {
                master_gain_db: 1.0,
                ..DspParams::default()
            }),
            "the first rule's parameters"
        );
        assert_eq!(presets[1].chain, "podcast");
        assert_eq!(
            presets[2].chain, "",
            "an output route has no chain to choose"
        );
    }

    // ---- diffing -------------------------------------------------------------------------

    #[test]
    fn the_same_set_in_another_order_is_no_change() {
        let a = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            input_rule(named("Discord"), "Headset", "voice"),
        ]);
        let b = rules(vec![
            input_rule(named("Discord"), "Headset", "VOICE"),
            output_rule(named("Game"), "Gaming", 1.0),
        ]);
        assert!(diff(&a, &b).is_empty(), "{:?}", diff(&a, &b));
        assert!(diff(&Rules::default(), &Rules::default()).is_empty());
    }

    #[test]
    fn presets_that_come_and_go_are_told_apart_per_lane() {
        let old = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            output_rule(named("Brave"), "Volume Boost", 1.0),
        ]);
        let new = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            input_rule(named("Discord"), "Gaming", "voice"),
        ]);
        let changes = diff(&old, &new);
        assert_eq!(changes.added, vec![(IN, "Gaming".to_owned())]);
        assert_eq!(changes.removed, vec![(OUT, "Volume Boost".to_owned())]);
        assert!(changes.params.is_empty());
        assert!(changes.chains.is_empty());
        assert!(changes.applications);

        let back = diff(&new, &old);
        assert_eq!(back.added, vec![(OUT, "Volume Boost".to_owned())]);
        assert_eq!(back.removed, vec![(IN, "Gaming".to_owned())]);
    }

    #[test]
    fn new_parameters_for_a_preset_are_a_change_of_parameters_and_nothing_else() {
        let old = rules(vec![output_rule(named("Game"), "Gaming", 1.0)]);
        let new = rules(vec![output_rule(named("Game"), "Gaming", 3.0)]);
        let changes = diff(&old, &new);
        assert_eq!(changes.params.len(), 1);
        assert_eq!(changes.params[0].name, "Gaming");
        assert_eq!(
            changes.params[0].params,
            new.preset(OUT, "Gaming").unwrap().params
        );
        assert!(changes.added.is_empty() && changes.removed.is_empty());
        assert!(changes.chains.is_empty());
        assert!(
            !changes.applications,
            "the same application runs the same preset"
        );
    }

    #[test]
    fn a_voice_preset_that_names_another_chain_is_a_change_of_chain() {
        let old = rules(vec![input_rule(named("Discord"), "Headset", "voice")]);
        let new = rules(vec![input_rule(named("Discord"), "Headset", "podcast")]);
        let changes = diff(&old, &new);
        assert_eq!(changes.chains.len(), 1);
        assert_eq!(changes.chains[0].chain, "podcast");
        assert!(changes.params.is_empty());
        assert!(!changes.applications);
    }

    #[test]
    fn an_application_moved_to_another_preset_is_a_change_of_applications() {
        let old = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            output_rule(named("Brave"), "Movies", 1.0),
        ]);
        let new = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            output_rule(named("Brave"), "Gaming", 1.0),
            output_rule(named("mpv"), "Movies", 1.0),
        ]);
        let changes = diff(&old, &new);
        assert!(changes.applications);
        assert!(changes.added.is_empty() && changes.removed.is_empty());
    }

    // ---- planning ------------------------------------------------------------------------

    #[test]
    fn a_running_stream_with_a_rule_gets_a_route_and_one_without_does_not() {
        let mut table = RouteTable::default();
        let rules = rules(vec![output_rule(named("Game"), "Gaming", 1.0)]);
        let streams = [
            stream(40, OUT, named("Game")),
            stream(41, OUT, named("mpv")),
        ];
        let now = Instant::now();
        let plan = table.plan(&rules, &streams, &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(built(&plan), vec![(slot(OUT, 1), "Gaming")]);
        assert_eq!(plan.assigned, vec![(40, slot(OUT, 1))]);
        assert!(plan.teardown.is_empty() && plan.overflow.is_empty());

        // The next run finds the route there, and builds nothing.
        let plan = table.plan(&rules, &streams, &both(ATTACHED), now, ROUTE_IDLE);
        assert!(plan.build.is_empty());
        assert_eq!(plan.assigned, vec![(40, slot(OUT, 1))]);
    }

    #[test]
    fn a_rule_for_an_application_that_is_not_running_builds_nothing() {
        let mut table = RouteTable::default();
        let rules = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            input_rule(named("Discord"), "Headset", "voice"),
        ]);
        let plan = table.plan(&rules, &[], &both(ATTACHED), Instant::now(), ROUTE_IDLE);
        assert_eq!(plan, Plan::default());
        assert!(table.routes().is_empty());
    }

    #[test]
    fn applications_of_one_preset_share_a_route_and_other_presets_get_their_own() {
        let mut table = RouteTable::default();
        let rules = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            output_rule(named("Other game"), "Gaming", 1.0),
            output_rule(named("Brave"), "Volume Boost", 1.0),
            input_rule(named("Discord"), "Headset", "voice"),
        ]);
        let streams = [
            stream(12, OUT, named("Brave")),
            stream(10, OUT, named("Game")),
            stream(11, OUT, named("Other game")),
            stream(13, IN, named("Discord")),
            // A game with two streams: both on the one route.
            stream(14, OUT, named("Game")),
        ];
        let plan = table.plan(
            &rules,
            &streams,
            &both(ATTACHED),
            Instant::now(),
            ROUTE_IDLE,
        );
        assert_eq!(
            built(&plan),
            vec![
                (slot(OUT, 1), "Gaming"),
                (slot(OUT, 2), "Volume Boost"),
                (slot(IN, 1), "Headset"),
            ],
            "numbered per lane, in the order of each preset's first stream"
        );
        assert_eq!(
            plan.assigned,
            vec![
                (10, slot(OUT, 1)),
                (11, slot(OUT, 1)),
                (12, slot(OUT, 2)),
                (13, slot(IN, 1)),
                (14, slot(OUT, 1)),
            ]
        );
    }

    #[test]
    fn a_stream_that_may_not_move_stays_where_it_is_and_needs_no_route() {
        let mut table = RouteTable::default();
        let rules = rules(vec![output_rule(named("Kiosk"), "Gaming", 1.0)]);
        let pinned = Candidate {
            movable: false,
            ..stream(5, OUT, named("Kiosk"))
        };
        let plan = table.plan(
            &rules,
            &[pinned],
            &both(ATTACHED),
            Instant::now(),
            ROUTE_IDLE,
        );
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn a_rule_of_one_lane_moves_nothing_on_the_other() {
        let mut table = RouteTable::default();
        let rules = rules(vec![input_rule(named("Discord"), "Headset", "voice")]);
        let plan = table.plan(
            &rules,
            &[stream(7, OUT, named("Discord"))],
            &both(ATTACHED),
            Instant::now(),
            ROUTE_IDLE,
        );
        assert_eq!(
            plan,
            Plan::default(),
            "Discord's playback follows the output lane"
        );
    }

    #[test]
    fn a_lane_runs_at_most_four_routes_and_a_fifth_preset_stays_on_the_lane() {
        let mut table = RouteTable::default();
        let presets = ["One", "Two", "Three", "Four", "Five"];
        let rules = rules(
            presets
                .iter()
                .map(|preset| output_rule(named(&format!("App {preset}")), preset, 1.0))
                .collect(),
        );
        let streams: Vec<Candidate> = presets
            .iter()
            .enumerate()
            .map(|(index, preset)| stream(index as u32 + 1, OUT, named(&format!("App {preset}"))))
            .collect();
        let now = Instant::now();
        let plan = table.plan(&rules, &streams, &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(MAX_ROUTES_PER_LANE, 4);
        assert_eq!(plan.build.len(), MAX_ROUTES_PER_LANE);
        assert_eq!(
            plan.overflow,
            vec![Overflow {
                direction: OUT,
                preset: "Five".to_owned(),
                app: "App Five".to_owned(),
            }]
        );
        assert!(
            plan.assigned.iter().all(|&(id, _)| id != 5),
            "the fifth application is not moved"
        );
        assert_eq!(table.routes().len(), 4);

        // The other lane has four of its own.
        let input = rules_for_input(&presets);
        let input_streams: Vec<Candidate> = presets
            .iter()
            .enumerate()
            .map(|(index, preset)| stream(index as u32 + 20, IN, named(&format!("Rec {preset}"))))
            .collect();
        let mut table = RouteTable::default();
        let plan = table.plan(&input, &input_streams, &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(plan.build.len(), 4);
        assert_eq!(plan.overflow.len(), 1);
    }

    fn rules_for_input(presets: &[&str]) -> Rules {
        rules(
            presets
                .iter()
                .map(|preset| input_rule(named(&format!("Rec {preset}")), preset, "voice"))
                .collect(),
        )
    }

    #[test]
    fn a_fifth_preset_takes_the_place_of_the_route_unused_longest() {
        let mut table = RouteTable::default();
        let presets = ["One", "Two", "Three", "Four", "Five"];
        let rules = rules(
            presets
                .iter()
                .map(|preset| output_rule(named(preset), preset, 1.0))
                .collect(),
        );
        let start = Instant::now();
        let first_four: Vec<Candidate> = presets[..4]
            .iter()
            .enumerate()
            .map(|(index, preset)| stream(index as u32 + 1, OUT, named(preset)))
            .collect();
        table.plan(&rules, &first_four, &both(ATTACHED), start, ROUTE_IDLE);

        // "Two" quits, then "Three"; "Five" starts while both routes are still kept.
        let later = start + Duration::from_secs(1);
        table.plan(
            &rules,
            &[
                first_four[0].clone(),
                first_four[2].clone(),
                first_four[3].clone(),
            ],
            &both(ATTACHED),
            later,
            ROUTE_IDLE,
        );
        let still_later = later + Duration::from_secs(1);
        let with_five = [
            first_four[0].clone(),
            first_four[3].clone(),
            stream(5, OUT, named("Five")),
        ];
        let plan = table.plan(&rules, &with_five, &both(ATTACHED), still_later, ROUTE_IDLE);
        assert_eq!(gone(&plan), vec![(slot(OUT, 2), Teardown::Evicted)]);
        assert_eq!(built(&plan), vec![(slot(OUT, 2), "Five")]);
        assert!(plan.overflow.is_empty());
        assert_eq!(table.preset_of(slot(OUT, 3)), Some("Three"), "still kept");
    }

    #[test]
    fn a_new_route_takes_the_lowest_number_free_and_the_others_keep_theirs() {
        let mut table = RouteTable::default();
        let rules = rules(vec![
            output_rule(named("A"), "One", 1.0),
            output_rule(named("B"), "Two", 1.0),
            output_rule(named("C"), "Three", 1.0),
        ]);
        let now = Instant::now();
        table.plan(
            &rules,
            &[stream(1, OUT, named("A")), stream(2, OUT, named("B"))],
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        // "A" has quit for longer than the idle period: its route goes, and "C" takes number 1.
        let later = now + ROUTE_IDLE;
        table.plan(
            &rules,
            &[stream(2, OUT, named("B"))],
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        let plan = table.plan(
            &rules,
            &[stream(2, OUT, named("B")), stream(3, OUT, named("C"))],
            &both(ATTACHED),
            later,
            ROUTE_IDLE,
        );
        assert_eq!(gone(&plan), vec![(slot(OUT, 1), Teardown::Idle)]);
        assert_eq!(built(&plan), vec![(slot(OUT, 1), "Three")]);
        assert_eq!(table.slot_of(OUT, "Two"), Some(slot(OUT, 2)));
    }

    #[test]
    fn a_route_nobody_uses_is_kept_for_the_idle_period_and_then_goes() {
        let mut table = RouteTable::default();
        let rules = rules(vec![output_rule(named("Game"), "Gaming", 1.0)]);
        let game = [stream(40, OUT, named("Game"))];
        let start = Instant::now();
        table.plan(&rules, &game, &both(ATTACHED), start, ROUTE_IDLE);

        // The game closes its stream: the route stays, unused.
        let left = start + Duration::from_secs(1);
        let plan = table.plan(&rules, &[], &both(ATTACHED), left, ROUTE_IDLE);
        assert!(plan.teardown.is_empty());
        assert_eq!(table.routes()[0].idle_since, Some(left));

        // Just before the period is up, still there.
        let plan = table.plan(
            &rules,
            &[],
            &both(ATTACHED),
            left + ROUTE_IDLE - Duration::from_millis(1),
            ROUTE_IDLE,
        );
        assert!(plan.teardown.is_empty());

        // Once it is up, gone.
        let plan = table.plan(&rules, &[], &both(ATTACHED), left + ROUTE_IDLE, ROUTE_IDLE);
        assert_eq!(gone(&plan), vec![(slot(OUT, 1), Teardown::Idle)]);
        assert!(table.routes().is_empty());
    }

    #[test]
    fn a_stream_that_comes_back_within_the_idle_period_finds_its_route_and_the_wait_starts_over() {
        let mut table = RouteTable::default();
        let rules = rules(vec![output_rule(named("Game"), "Gaming", 1.0)]);
        let start = Instant::now();
        table.plan(
            &rules,
            &[stream(40, OUT, named("Game"))],
            &both(ATTACHED),
            start,
            ROUTE_IDLE,
        );
        table.plan(
            &rules,
            &[],
            &both(ATTACHED),
            start + Duration::from_secs(5),
            ROUTE_IDLE,
        );

        // Back under a new id, as a new stream: onto the same route, nothing built.
        let back = start + Duration::from_secs(9);
        let plan = table.plan(
            &rules,
            &[stream(52, OUT, named("Game"))],
            &both(ATTACHED),
            back,
            ROUTE_IDLE,
        );
        assert!(plan.build.is_empty() && plan.teardown.is_empty());
        assert_eq!(plan.assigned, vec![(52, slot(OUT, 1))]);
        assert_eq!(table.routes()[0].idle_since, None);

        // Gone again: the full period, counted from now.
        let plan = table.plan(
            &rules,
            &[],
            &both(ATTACHED),
            back + Duration::from_secs(6),
            ROUTE_IDLE,
        );
        assert!(plan.teardown.is_empty());
    }

    #[test]
    fn a_route_whose_rule_goes_is_taken_down_at_once() {
        let mut table = RouteTable::default();
        let with = rules(vec![output_rule(named("Game"), "Gaming", 1.0)]);
        let game = [stream(40, OUT, named("Game"))];
        let now = Instant::now();
        table.plan(&with, &game, &both(ATTACHED), now, ROUTE_IDLE);
        let plan = table.plan(&Rules::default(), &game, &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(gone(&plan), vec![(slot(OUT, 1), Teardown::RuleGone)]);
        assert!(plan.assigned.is_empty(), "the game goes back to its lane");
    }

    #[test]
    fn an_application_moved_to_another_preset_leaves_its_route_idle_and_gets_the_other() {
        let mut table = RouteTable::default();
        let now = Instant::now();
        let game = [stream(40, OUT, named("Game"))];
        let first = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            output_rule(named("Other"), "Gaming", 1.0),
        ]);
        table.plan(&first, &game, &both(ATTACHED), now, ROUTE_IDLE);
        let second = rules(vec![
            output_rule(named("Game"), "Movies", 1.0),
            output_rule(named("Other"), "Gaming", 1.0),
        ]);
        let plan = table.plan(&second, &game, &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(built(&plan), vec![(slot(OUT, 2), "Movies")]);
        assert_eq!(plan.assigned, vec![(40, slot(OUT, 2))]);
        assert!(
            plan.teardown.is_empty(),
            "Gaming is still named, and kept while idle"
        );
    }

    #[test]
    fn a_lane_switched_off_takes_every_route_of_its_own_and_none_of_the_other() {
        let mut table = RouteTable::default();
        let rules = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            input_rule(named("Discord"), "Headset", "voice"),
        ]);
        let streams = [
            stream(40, OUT, named("Game")),
            stream(41, IN, named("Discord")),
        ];
        let now = Instant::now();
        table.plan(&rules, &streams, &both(ATTACHED), now, ROUTE_IDLE);
        let plan = table.plan(
            &rules,
            &streams,
            &PerDirection {
                output: ATTACHED,
                input: LaneState::Off,
            },
            now,
            ROUTE_IDLE,
        );
        assert_eq!(gone(&plan), vec![(slot(IN, 1), Teardown::LaneOff)]);
        assert_eq!(plan.assigned, vec![(40, slot(OUT, 1))]);
        assert!(
            plan.overflow.is_empty(),
            "a lane that is off is no overflow"
        );
    }

    #[test]
    fn a_lane_between_pairs_keeps_its_routes_and_builds_none() {
        let mut table = RouteTable::default();
        let rules = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            output_rule(named("Brave"), "Volume Boost", 1.0),
        ]);
        let now = Instant::now();
        let game = stream(40, OUT, named("Game"));
        table.plan(
            &rules,
            std::slice::from_ref(&game),
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        let plan = table.plan(
            &rules,
            &[game, stream(41, OUT, named("Brave"))],
            &both(LaneState::Between),
            now,
            ROUTE_IDLE,
        );
        assert!(plan.build.is_empty() && plan.teardown.is_empty() && plan.overflow.is_empty());
        assert_eq!(plan.assigned, vec![(40, slot(OUT, 1))]);
    }

    #[test]
    fn nothing_is_built_for_a_lane_that_is_off() {
        let mut table = RouteTable::default();
        let rules = rules(vec![input_rule(named("Discord"), "Headset", "voice")]);
        let plan = table.plan(
            &rules,
            &[stream(41, IN, named("Discord"))],
            &both(LaneState::Off),
            Instant::now(),
            ROUTE_IDLE,
        );
        assert_eq!(plan, Plan::default());
    }

    // ---- the metadata --------------------------------------------------------------------

    #[test]
    fn a_stream_onto_a_route_is_written_once_and_a_stream_leaving_it_is_deleted() {
        let mut moves = Moves::default();
        let ops = moves.plan(&[(40, Some(301)), (41, Some(301))]);
        assert_eq!(
            ops,
            vec![
                MetadataOp::Write {
                    subject: 40,
                    serial: 301
                },
                MetadataOp::Write {
                    subject: 41,
                    serial: 301
                },
            ]
        );
        for op in ops {
            moves.sent(op);
        }
        // The server echoes the writes back; they are ours.
        moves.heard(40, Some("301"));
        moves.heard(41, Some("301"));
        assert!(!moves.moved_by_hand(40));
        assert!(moves.plan(&[(40, Some(301)), (41, Some(301))]).is_empty());

        // 41 leaves the route.
        let ops = moves.plan(&[(40, Some(301))]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 41 }]);
        moves.sent(ops[0]);
        moves.heard(41, None);
        assert_eq!(moves.written(41), None);
        assert!(moves.plan(&[(40, Some(301))]).is_empty());
    }

    #[test]
    fn a_route_rebuilt_under_a_new_serial_is_written_again_once_its_node_is_announced() {
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.heard(40, Some("301"));
        // Rebuilt: the new node is not announced yet, and the key is left alone meanwhile —
        // neither deleted nor written.
        assert!(moves.plan(&[(40, None)]).is_empty());
        // Announced.
        moves.route_node(377);
        assert_eq!(
            moves.plan(&[(40, Some(377))]),
            vec![MetadataOp::Write {
                subject: 40,
                serial: 377
            }]
        );
    }

    #[test]
    fn a_stream_moved_by_hand_is_neither_moved_again_nor_moved_back() {
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.heard(40, Some("301"));
        // The user moves it to their headphones in a mixer.
        moves.heard(40, Some("57"));
        assert!(moves.moved_by_hand(40));
        assert_eq!(
            moves.written(40),
            None,
            "what was written is the user's to undo"
        );
        assert!(
            moves.plan(&[]).is_empty(),
            "their key is never deleted, even with the route gone"
        );
        // A stream someone moved before FxSound ever looked at it is theirs too.
        moves.heard(41, Some("fxsound_sink_of_somebody_else"));
        assert!(moves.moved_by_hand(41));
        assert!(moves.everything_back().is_empty());
        // Once they delete their key, the stream is FxSound's to move again.
        moves.heard(40, None);
        assert!(!moves.moved_by_hand(40));
    }

    #[test]
    fn a_stream_moved_onto_a_route_by_hand_is_the_users_while_that_route_is_there() {
        let mut moves = Moves::default();
        moves.route_node(301);
        // The user moves a player no rule names onto "FxSound (Output) · Gaming" in a mixer.
        moves.heard(44, Some("301"));
        assert!(moves.moved_by_hand(44));
        assert!(moves.plan(&[]).is_empty());
        // The route goes: the key now names nothing, and is FxSound's to clear.
        moves.route_node_gone(301);
        assert!(!moves.moved_by_hand(44));
        assert_eq!(moves.plan(&[]), vec![MetadataOp::Delete { subject: 44 }]);
    }

    #[test]
    fn the_echo_of_an_earlier_write_is_still_fxsounds_after_a_later_one() {
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.route_node(377);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 377,
        });
        // The first write's echo arrives only now.
        moves.heard(40, Some("301"));
        assert!(!moves.moved_by_hand(40));
        assert_eq!(moves.written(40), Some(377));
        moves.heard(40, Some("377"));
        assert!(moves.plan(&[(40, Some(377))]).is_empty());
        assert!(
            moves.unconfirmed.is_empty(),
            "both writes reported, nothing left to wait for"
        );
    }

    /// Two players on one route, the rule of one removed, and the user moving it back onto the
    /// route in a mixer: the serial is the one FxSound wrote for it before, and is theirs now.
    #[test]
    fn a_serial_fxsound_deleted_is_the_users_when_they_move_the_stream_back_onto_it() {
        let mut moves = Moves::default();
        moves.route_node(301);
        // Brave (40) and Chrome (41) both run "Volume Boost" on route o1.
        for op in moves.plan(&[(40, Some(301)), (41, Some(301))]) {
            moves.sent(op);
        }
        moves.heard(40, Some("301"));
        moves.heard(41, Some("301"));
        // Chrome's rule is removed: its key is deleted, and the server says so.
        let ops = moves.plan(&[(40, Some(301))]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 41 }]);
        moves.sent(ops[0]);
        moves.heard(41, None);
        assert!(!moves.moved_by_hand(41));

        // The user moves Chrome onto "FxSound (Output) · Volume Boost" in pavucontrol.
        moves.heard(41, Some("301"));
        assert!(moves.moved_by_hand(41), "a move FxSound leaves alone");
        assert_eq!(moves.written(41), None);
        assert!(
            moves.plan(&[(40, Some(301))]).is_empty(),
            "their key is not deleted on the next plan"
        );
        assert_eq!(
            moves.everything_back(),
            vec![MetadataOp::Delete { subject: 40 }],
            "nor on the way out"
        );
    }

    /// FxSound moves a stream from one route to another, and the user moves it back onto the first
    /// in a mixer: the stream is on the first, by their hand, and FxSound says nothing else.
    #[test]
    fn a_stream_moved_back_by_hand_onto_the_route_fxsound_moved_it_off_is_the_users() {
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.route_node(302);
        for serial in [301, 302] {
            moves.sent(MetadataOp::Write {
                subject: 40,
                serial,
            });
            moves.heard(40, Some(&serial.to_string()));
            assert!(!moves.moved_by_hand(40));
        }

        moves.heard(40, Some("301"));
        assert!(moves.moved_by_hand(40), "not taken for an old echo");
        assert_eq!(
            moves.written(40),
            None,
            "FxSound no longer says the stream is on the second route's preset"
        );
        assert!(moves.plan(&[]).is_empty(), "and leaves the key where it is");
    }

    #[test]
    fn a_write_sent_after_a_delete_is_still_fxsounds_when_all_three_come_back() {
        // A rule added, removed and added again within one round trip: a write, a delete and a
        // write on their way at once. The two writes name routes that are both still there.
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.route_node(302);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.sent(MetadataOp::Delete { subject: 40 });
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 302,
        });

        moves.heard(40, Some("301"));
        assert!(!moves.moved_by_hand(40));
        assert!(
            moves.plan(&[(40, Some(302))]).is_empty(),
            "nothing to send: everything wanted is on its way"
        );
        moves.heard(40, None);
        assert!(!moves.moved_by_hand(40));
        moves.heard(40, Some("302"));
        assert!(!moves.moved_by_hand(40));
        assert_eq!(moves.written(40), Some(302));
        assert!(moves.plan(&[(40, Some(302))]).is_empty());
        assert!(moves.unconfirmed.is_empty());
    }

    #[test]
    fn a_delete_on_its_way_is_not_sent_again_when_the_write_it_undoes_comes_back() {
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        let ops = moves.plan(&[]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 40 }]);
        moves.sent(ops[0]);
        // The write is reported only now, the delete after it.
        moves.heard(40, Some("301"));
        assert!(!moves.moved_by_hand(40));
        assert!(
            moves.plan(&[]).is_empty(),
            "the delete is already on its way"
        );
        moves.heard(40, None);
        assert!(moves.plan(&[]).is_empty());
        assert!(moves.unconfirmed.is_empty());
    }

    #[test]
    fn a_move_by_hand_the_server_applied_before_fxsounds_own_write_is_overtaken_by_it() {
        // FxSound writes 302 while the user moves the stream to their headphones; the server takes
        // the user's first, and FxSound's after it. The key the server ends up holding is
        // FxSound's, and FxSound stands behind it.
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.route_node(302);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.heard(40, Some("301"));
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 302,
        });

        moves.heard(40, Some("57"));
        assert!(
            moves.moved_by_hand(40),
            "for as long as the server holds it"
        );
        moves.heard(40, Some("302"));
        assert!(!moves.moved_by_hand(40));
        assert_eq!(moves.written(40), Some(302));
        assert!(
            moves.plan(&[(40, Some(302))]).is_empty(),
            "not written again for want of standing behind it"
        );
    }

    #[test]
    fn a_change_the_server_never_reports_is_settled_by_the_next_one_it_does() {
        // A delete of a key someone else deleted first changes nothing, and the server says
        // nothing of it. The next write's report settles it, and no stale serial of FxSound's is
        // left to be taken for its own later.
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.route_node(302);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.heard(40, Some("301"));
        moves.sent(MetadataOp::Delete { subject: 40 });
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 302,
        });
        moves.heard(40, Some("302"));
        assert!(moves.unconfirmed.is_empty());

        // The user moves it onto 301 by hand: nothing of FxSound's accounts for that any more.
        moves.heard(40, Some("301"));
        assert!(moves.moved_by_hand(40));
    }

    #[test]
    fn a_key_naming_a_route_of_an_earlier_connection_is_fxsounds_and_is_taken_over() {
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        // The connection goes and comes back; the server still holds the key, and says so.
        moves.forget_session();
        moves.heard(40, Some("301"));
        assert!(
            !moves.moved_by_hand(40),
            "301 was a route of this process's"
        );
        // No rule for it any more: deleted.
        assert_eq!(moves.plan(&[]), vec![MetadataOp::Delete { subject: 40 }]);
        // Still routed: onto the new route.
        moves.route_node(410);
        assert_eq!(
            moves.plan(&[(40, Some(410))]),
            vec![MetadataOp::Write {
                subject: 40,
                serial: 410
            }]
        );
    }

    #[test]
    fn a_stream_that_went_takes_its_key_with_it() {
        let mut moves = Moves::default();
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.heard(40, Some("301"));
        moves.stream_gone(40);
        assert!(
            moves.plan(&[]).is_empty(),
            "the server has forgotten the subject; there is nothing to delete"
        );
    }

    #[test]
    fn every_key_of_fxsounds_is_deleted_on_the_way_out_and_nothing_else() {
        let mut moves = Moves::default();
        moves.route_node(301);
        moves.route_node(302);
        for (subject, serial) in [(40, 301), (41, 302)] {
            moves.sent(MetadataOp::Write { subject, serial });
            moves.heard(subject, Some(&serial.to_string()));
        }
        // Written, and not echoed yet.
        moves.sent(MetadataOp::Write {
            subject: 42,
            serial: 302,
        });
        // Somebody else's.
        moves.heard(43, Some("57"));
        assert_eq!(
            moves.everything_back(),
            vec![
                MetadataOp::Delete { subject: 40 },
                MetadataOp::Delete { subject: 41 },
                MetadataOp::Delete { subject: 42 },
            ]
        );
    }

    #[test]
    fn deletes_go_before_writes() {
        let mut moves = Moves::default();
        moves.sent(MetadataOp::Write {
            subject: 90,
            serial: 301,
        });
        let ops = moves.plan(&[(10, Some(302))]);
        assert_eq!(
            ops,
            vec![
                MetadataOp::Delete { subject: 90 },
                MetadataOp::Write {
                    subject: 10,
                    serial: 302
                },
            ]
        );
        assert_eq!(ops[0].subject(), 90);
    }

    // ---- warnings ------------------------------------------------------------------------

    #[test]
    fn an_application_that_cannot_get_a_route_is_warned_about_once_for_as_long_as_it_cannot() {
        let mut warnings = OverflowWarnings::default();
        let game = Overflow {
            direction: OUT,
            preset: "Five".to_owned(),
            app: "Game".to_owned(),
        };
        assert_eq!(
            warnings.news(std::slice::from_ref(&game)),
            vec![game.clone()]
        );
        assert!(warnings.news(std::slice::from_ref(&game)).is_empty());
        assert!(warnings.news(std::slice::from_ref(&game)).is_empty());
        // It got its route.
        assert!(warnings.news(&[]).is_empty());
        // And lost it again: news again.
        assert_eq!(warnings.news(std::slice::from_ref(&game)), vec![game]);
    }
}
