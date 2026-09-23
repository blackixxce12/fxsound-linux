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
//! The app sends every rule at once ([`UiToAudio::SetAppRoutes`]), whether its application runs or
//! not: an application, a lane, a preset and that preset's parameters — or no preset, for a rule
//! that follows the lane where it outranks a more general one that names a preset (a Flatpak
//! Firefox of its own beside the native Firefox's rule, which would match it by its binary).
//! [`Rules`] keeps them, and [`diff`] says what changed from one set to the next, per preset: which
//! are new, which are gone, whose parameters or voice chain changed. A route is per preset, not
//! per application — every application of one lane with the same preset shares one pair — and
//! exists only while a stream needs it: a rule for a game that is not running builds nothing.
//! [`RouteTable::plan`] turns the rules and the streams in the graph into the routes each lane
//! should run:
//!
//! - A stream is matched with the rule of its own lane that names its application most
//!   specifically ([`AppKey::best_match`], the order the store itself uses), and goes onto that
//!   preset's route; a rule that follows leaves it on the lane. A stream that may not be moved —
//!   `node.dont-move`, `node.dont-reconnect`, `node.dont-fallback`, a target of its own that is
//!   not FxSound's, a recorder of what FxSound plays (`crate::app_streams`) — or that someone has
//!   moved by hand ([`Moves::moved_by_hand`]) stays where it is.
//! - A route is in use while a stream is planned onto it, and also while a stream FxSound leaves
//!   where it is sits on it ([`Candidate::on_route`]): one whose own properties name the route's
//!   node, one WirePlumber will not move off it again ([`Moves::anchor`]), or a recorder whose
//!   key names a playback route's sink and records its monitor ([`Moves::monitored`]). Such a
//!   route is never idle and never makes room for another; and one whose preset no rule names any
//!   more is kept for as long as such a stream is on it ([`Plan::kept`]), rather than taken down
//!   under audio that is playing through it.
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
//! A key someone else wrote that names anything but a route of FxSound's — the user moving the
//! stream to their headphones in a mixer — is theirs: the stream is not moved again, and its key is
//! never deleted. What FxSound wrote is its own on the way to the server and for as long as it
//! stays there: the server reports each change once, in the order it applies them, and nothing for
//! a change that changes nothing, so its report settles the change it reports and every one sent
//! before it ([`Moves::heard`]).
//!
//! A key that puts a stream onto a route node of this process's is FxSound's whoever wrote it, and
//! the plan puts it right: it is kept where it names the route the stream's rule wants, rewritten
//! where it names another, and deleted where the stream has no rule. Such a key is rarely
//! anybody's choice. WirePlumber 0.5 remembers every target written for an application by the
//! target node's *name* and writes it back each time the application opens a stream
//! (`node/state-stream.lua`, `node.stream.restore-target`, on by default), and nothing clears what
//! it remembers once the application has quit while it was routed. A route's name is its number,
//! and numbers are taken again by other presets — so the key WirePlumber writes back names
//! whatever preset runs under that number now, the application's own or anybody else's, and
//! cannot be told from a mixer's. Where a stream goes is what its rule says, and the Applications
//! list is where that is chosen.
//!
//! A recorder's key naming a *playback* route's sink puts it onto no route: WirePlumber links a
//! recorder to a sink's monitor (`lutils.canLink`, `lib/linking-utils.lua`), so the recorder hears
//! what the route plays, and nothing goes through the route's chain. It is what `pipewire-pulse`
//! writes when the user points a screen recorder at `Monitor of FxSound (Output) · <preset>` —
//! a recorder of what a sink plays, which FxSound never moves (`crate::app_streams`) — and no plan
//! of FxSound's ever writes it, since a recorder is only ever planned onto its own lane's routes.
//! So it is the user's, like a key naming any other node: never deleted nor rewritten, not even on
//! the way out, and the route it names is in use while the recorder is there
//! ([`Moves::monitored`]). Which subjects record, [`Moves::stream_appeared`] is told.
//!
//! A delete someone else sent, or a key they wrote over FxSound's, takes FxSound's word back: what
//! it wrote no longer stands, and the next plan writes it again where the rule still wants it. A key
//! naming a route of an earlier connection to the same server is taken over the same way. The
//! server forgets a subject's keys when the subject goes, and refuses keys for a subject that does
//! not exist (measured on PipeWire 1.6.8 with `pw-metadata`), so a stream that has gone takes its
//! key with it. On the way out every key that puts a stream onto a route node of this process's is
//! deleted, whoever wrote it ([`Moves::everything_back`]): the node goes with the process, and a
//! key left naming it would make the stream the user's to the next run, and its name what
//! WirePlumber restores.
//!
//! A key never names a route node that is about to go. A stream whose route is rebuilt — on the
//! lane's new device, after a failure, or under the same number for another preset — has its key
//! deleted ahead of the old node, and written again once the new one is known ([`Want::Pending`]).
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

use crate::app_streams::ExplicitTarget;
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
    /// The key names no application, so no stream could match it.
    NoApplication,
}

/// The rules the app last sent ([`UiToAudio::SetAppRoutes`]), as the engine keeps them: every
/// rule it can act on, in the order they came, each preset's parameters sanitised. A rule with no
/// preset says its application follows the lane: it takes part in matching, so that a more
/// general rule naming a preset cannot claim the application, and names no route.
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
    /// routes, and two spellings that differ only by a space are one preset; a blank one is a rule
    /// that follows the lane. Every snapshot is sanitised, as [`crate::EngineHandle::set_params`]
    /// sanitises the lanes', because this is the one gate between the message and a route's
    /// chain.
    pub(crate) fn new(asked: Vec<AppRoute>) -> (Self, Vec<(AppRoute, Refusal)>) {
        let mut rules = Vec::with_capacity(asked.len());
        let mut refused = Vec::new();
        for mut rule in asked {
            let refusal = if !rule.is_consistent() {
                Some(Refusal::Inconsistent)
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
    /// rule of that lane names, or `None` when no rule of that lane matches it or that rule
    /// follows the lane.
    pub(crate) fn preset_for(&self, direction: DeviceDirection, app: &AppKey) -> Option<&str> {
        let lane: Vec<&AppRoute> = self
            .rules
            .iter()
            .filter(|rule| rule.direction == direction)
            .collect();
        let best = app.best_match(lane.iter().map(|rule| &rule.app))?;
        lane.get(best)
            .map(|rule| rule.preset.as_str())
            .filter(|preset| !preset.is_empty())
    }

    /// The rules that name a preset: every one but those that follow the lane.
    fn with_preset(&self) -> impl Iterator<Item = &AppRoute> {
        self.rules.iter().filter(|rule| !rule.preset.is_empty())
    }

    /// The preset `name` of `direction`, as its first rule gives it. The app resolves one preset
    /// to one set of parameters; were two rules to disagree, the first is the one a route runs.
    pub(crate) fn preset(&self, direction: DeviceDirection, name: &str) -> Option<RoutePreset> {
        self.with_preset()
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
        self.with_preset()
            .any(|rule| rule.direction == direction && rule.preset == name)
    }

    /// Every preset the rules name, once, in the order they are first named.
    pub(crate) fn presets(&self) -> Vec<RoutePreset> {
        let mut presets: Vec<RoutePreset> = Vec::new();
        for rule in self.with_preset() {
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
    /// tells a set that moves an application from one that only changes a preset's parameters. A
    /// rule that follows the lane is one too, with no preset: it can take an application off a
    /// route a more general rule would put it on.
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

/// One application stream, as the routes see it: its node id, its lane, who it is, whether it
/// may be moved, and the route it sits on where FxSound does not decide that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) id: u32,
    pub(crate) direction: DeviceDirection,
    pub(crate) app: AppKey,
    /// Neither pinned where it is (`crate::app_streams::Pin`) nor moved by hand
    /// ([`Moves::moved_by_hand`]).
    pub(crate) movable: bool,
    /// The route the stream is on by something no plan of FxSound's changes: its own properties
    /// naming the route's node ([`route_of_target`]), with no key in the metadata to override
    /// them, a key WirePlumber will not move it off again ([`Moves::anchor`]), or — for a
    /// recorder, whose slot is then of the other lane — a key naming a playback route's sink,
    /// whose monitor it records ([`Moves::monitored`]). A route with such a stream on it is in use
    /// ([`Self::stays_on`]).
    pub(crate) on_route: Option<RouteSlot>,
}

impl Candidate {
    /// Whether the stream is on the route in `slot`, and stays there whatever this plan decides: a
    /// stream that may move and has a rule of its lane is where the plan puts it instead — onto
    /// its own route, or back onto its lane by a key that overrides its own properties.
    fn stays_on(&self, slot: RouteSlot, rules: &Rules) -> bool {
        self.on_route == Some(slot)
            && !(self.movable && rules.preset_for(self.direction, &self.app).is_some())
    }
}

/// The route whose virtual node a stream's own properties name as its target, if they name one:
/// by the node's name — whichever node carries that name now or will, since WirePlumber links a
/// stream to a target named by name whenever one is there — or by number, `target.object` by
/// `object.serial` and `node.target` by registry id, as WirePlumber looks each up
/// (`linking/find-defined-target.lua`), among `nodes`: each route's slot, its virtual node's
/// registry id, and its serial.
pub(crate) fn route_of_target(
    target: &ExplicitTarget,
    nodes: &[(RouteSlot, u32, u64)],
) -> Option<RouteSlot> {
    let (value, by_id) = match target {
        ExplicitTarget::Object(value) => (value.trim(), false),
        ExplicitTarget::Node(value) => (value.trim(), true),
    };
    if let Some(slot) = RouteSlot::of_node(value) {
        return Some(slot);
    }
    if by_id {
        let id = value.parse::<u32>().ok()?;
        nodes
            .iter()
            .find(|&&(_, node, _)| node == id)
            .map(|&(slot, ..)| slot)
    } else {
        let serial = value.parse::<u64>().ok()?;
        nodes
            .iter()
            .find(|&&(.., node_serial)| node_serial == serial)
            .map(|&(slot, ..)| slot)
    }
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
    /// No rule names its preset any more, and it is kept only for a stream FxSound cannot move
    /// off it ([`Plan::kept`]).
    pub(crate) orphaned: bool,
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
    /// Routes whose preset no rule names any more, kept rather than taken down because a stream
    /// FxSound does not move is on them ([`Candidate::stays_on`]) — each once, in the plan that
    /// first keeps it. Such a route goes at once when the last of those streams has left.
    pub(crate) kept: Vec<(RouteSlot, String)>,
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

        // Whether a stream FxSound leaves where it is sits on the route in `slot`: audio that
        // plays through the route whatever this plan decides.
        let held = |slot: RouteSlot| streams.iter().any(|stream| stream.stays_on(slot, rules));

        // A route whose preset no rule of the lane names any more goes at once — its streams' keys
        // deleted first — unless such a stream is on it. That one is kept, and goes once they
        // have left.
        self.take_down(
            |route| {
                route.slot.direction == direction
                    && !rules.names(direction, &route.preset)
                    && !held(route.slot)
            },
            Teardown::RuleGone,
            plan,
        );
        for route in self
            .routes
            .iter_mut()
            .filter(|route| route.slot.direction == direction)
        {
            let orphaned = !rules.names(direction, &route.preset);
            if orphaned && !route.orphaned {
                plan.kept.push((route.slot, route.preset.clone()));
            }
            route.orphaned = orphaned;
        }

        // A route with streams is in use; one without has been idle since its last one left, and
        // goes once that is long enough ago.
        for route in self
            .routes
            .iter_mut()
            .filter(|route| route.slot.direction == direction)
        {
            let used = wanted.iter().any(|(preset, _)| *preset == route.preset) || held(route.slot);
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
            orphaned: false,
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

/// Where the plan wants a stream's `target.object` ([`Moves::plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// Onto the route node with this serial.
    Onto(u64),
    /// Onto a route whose node is not known yet: built or rebuilt in this tick, or kept as it is
    /// while its lane is between pairs. `still` is the node the route has now and keeps — none
    /// for a route that is built or rebuilt — and a key naming it is left as it is until the
    /// route's node is known. A key naming any other route node of FxSound's is deleted: one that
    /// goes in this tick, which WirePlumber would otherwise have to fall back from — or destroy
    /// the stream over, when it says `node.dont-fallback` — or another preset's route.
    Pending { still: Option<u64> },
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
    /// reported a later change of FxSound's ([`Self::settle`]).
    unconfirmed: HashMap<u32, VecDeque<Option<u64>>>,
    /// The `target.object` the metadata holds for each subject, as its events report it.
    held: HashMap<u32, Held>,
    /// The serial of every route node this process has made, on any connection, with the lane of
    /// its route. A serial is never handed to another object.
    ours: HashMap<u64, DeviceDirection>,
    /// For each stream WirePlumber will not move again once it is linked, the route node its key
    /// named when FxSound first saw it ([`Self::anchor`]).
    anchored: HashMap<u32, u64>,
    /// Every application stream in the graph that records (`Stream/Input/Audio`): the subjects
    /// whose key naming a playback route's sink records that route's monitor, and is not
    /// FxSound's ([`Self::monitored`]).
    recorders: HashSet<u32>,
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
    /// A route node of ours has appeared with this serial: the virtual node of a route of the
    /// `direction` lane — a sink for a playback route, a source for a recording one.
    pub(crate) fn route_node(&mut self, serial: u64, direction: DeviceDirection) {
        self.ours.insert(serial, direction);
    }

    /// An application stream appeared under `id`: a player (`Output`) or a recorder (`Input`).
    /// Only a recorder is remembered, and one under an id the server has handed on is forgotten.
    pub(crate) fn stream_appeared(&mut self, id: u32, direction: DeviceDirection) {
        match direction {
            DeviceDirection::Input => self.recorders.insert(id),
            DeviceDirection::Output => self.recorders.remove(&id),
        };
    }

    /// The metadata says the stream `subject`'s target is now `value`, or that it has none.
    ///
    /// A report that says what a change of this engine's still unreported says is that change,
    /// and settles it ([`Self::settle`]). One that no change of FxSound's accounts for — a mixer's
    /// move, WirePlumber restoring a target, a delete someone else sent — takes back what this
    /// engine wrote before it ([`Self::overtaken`]).
    pub(crate) fn heard(&mut self, subject: u32, value: Option<&str>) {
        let Some(value) = value.map(str::trim) else {
            if !self.settle(subject, None) {
                self.overtaken(subject, None);
            }
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
        } else {
            self.overtaken(subject, serial);
        }
        self.held.insert(
            subject,
            Held {
                value: value.to_owned(),
                echo,
            },
        );
    }

    /// Someone else set the stream's key to `now` — a serial, or `None` for a delete or a value
    /// that is no number. What this engine wrote no longer stands, unless it is what they wrote,
    /// or a change of this engine's is still on its way: the server applies that one after theirs,
    /// and ends up holding what this engine last sent. The next plan writes the key again where
    /// the stream's rule still wants it — rather than go on taking the stream for moved, as
    /// neither the metadata nor WirePlumber does any more.
    fn overtaken(&mut self, subject: u32, now: Option<u64>) {
        if !self.unconfirmed.contains_key(&subject) && self.written.get(&subject).copied() != now {
            self.written.remove(&subject);
        }
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
        self.anchored.remove(&id);
        self.recorders.remove(&id);
    }

    /// The connection went, and every route node with it. What the server holds, and which
    /// streams record, is read again from the next one; which route nodes were this process's is
    /// not forgotten.
    pub(crate) fn forget_session(&mut self) {
        self.written.clear();
        self.unconfirmed.clear();
        self.held.clear();
        self.anchored.clear();
        self.recorders.clear();
    }

    /// Whether the stream's target was set by someone else — a mixer moving it, the application
    /// itself — to anything but a route of FxSound's, or to a playback route's monitor: a move
    /// FxSound leaves alone.
    pub(crate) fn moved_by_hand(&self, id: u32) -> bool {
        self.held
            .get(&id)
            .is_some_and(|held| !self.holds_ours(id, held))
    }

    /// The serial this engine last wrote for the stream `id`, while it stands behind it.
    pub(crate) fn written(&self, id: u32) -> Option<u64> {
        self.written.get(&id).copied()
    }

    /// Whether the metadata holds a target for the stream `id`, or is about to hold one of this
    /// engine's: one that WirePlumber reads ahead of the target the stream's own properties name.
    pub(crate) fn has_key(&self, id: u32) -> bool {
        self.held.contains_key(&id) || self.written.contains_key(&id)
    }

    /// The serial the stream's key names, or is about to: what this engine last wrote while it
    /// stands behind it, else what the metadata holds, when that is a number.
    pub(crate) fn target(&self, id: u32) -> Option<u64> {
        self.written.get(&id).copied().or_else(|| {
            self.held
                .get(&id)
                .and_then(|held| held.value.parse::<u64>().ok())
        })
    }

    /// Where a stream WirePlumber will not move again once it is linked (`node.dont-reconnect`)
    /// sits: the route node its key names, when that key is FxSound's to take over
    /// ([`Self::takes_over`]) — most likely WirePlumber's own restore of where the application
    /// was last — remembered from the first time it is seen for as long as the stream lives.
    /// FxSound never moves such a stream, and deletes such a key like any other of its routes'
    /// that no rule accounts for: the key moves nothing any more, and left there it is what
    /// WirePlumber would restore the application onto the next time, under whatever preset that
    /// number runs then. The stream stays linked where the key put it, all the same, and its route
    /// must not go from under it; this is how the planner knows ([`Candidate::on_route`]).
    pub(crate) fn anchor(&mut self, id: u32) -> Option<u64> {
        if let Some(serial) = self
            .target(id)
            .filter(|&serial| self.takes_over(id, serial))
        {
            self.anchored.entry(id).or_insert(serial);
        }
        self.anchored.get(&id).copied()
    }

    /// The playback route node whose monitor the recorder `id` records, by a key of the metadata
    /// naming the route's sink: where a mixer's "record from" puts a screen recorder the user
    /// points at `Monitor of FxSound (Output) · <preset>` (module docs, "The metadata"). The key is
    /// not FxSound's, and the recorder sits on that route for as long as it names it
    /// ([`Candidate::on_route`]).
    pub(crate) fn monitored(&self, id: u32) -> Option<u64> {
        if !self.recorders.contains(&id) {
            return None;
        }
        let serial = self.held.get(&id)?.value.trim().parse::<u64>().ok()?;
        (self.ours.get(&serial) == Some(&DeviceDirection::Output)).then_some(serial)
    }

    /// Whether a key naming `serial` for `subject` puts the stream onto a route of this
    /// process's, there or gone, and so is FxSound's whoever wrote it: any route node of
    /// FxSound's, except a playback route's sink named for a recorder. WirePlumber links a
    /// recorder to a sink's monitor (`lutils.canLink`, `lib/linking-utils.lua`), and recording
    /// what a route plays puts nothing through the route's chain; no plan of FxSound's ever puts
    /// a recorder there, so such a key is somebody's choice ([`Self::monitored`]).
    fn takes_over(&self, subject: u32, serial: u64) -> bool {
        self.ours.get(&serial).is_some_and(|&direction| {
            direction == DeviceDirection::Input || !self.recorders.contains(&subject)
        })
    }

    /// Whether `held`, what the metadata holds for `subject`, is FxSound's: the report of a write
    /// of this engine's, or a value [`Self::names_ours`].
    fn holds_ours(&self, subject: u32, held: &Held) -> bool {
        held.echo || self.names_ours(subject, &held.value)
    }

    /// Whether `value`, held for `subject`, is FxSound's without being the report of a write of
    /// this engine's: the serial this engine stands behind for it, or that of a route node of
    /// this process's, there or gone, whoever wrote it — unless it is a recorder's key naming a
    /// playback route's sink ([`Self::takes_over`]; module docs, "The metadata").
    fn names_ours(&self, subject: u32, value: &str) -> bool {
        let Ok(serial) = value.trim().parse::<u64>() else {
            return false;
        };
        self.written.get(&subject) == Some(&serial) || self.takes_over(subject, serial)
    }

    /// Whether the key the metadata holds for `id`, or the one this engine last wrote there, is
    /// FxSound's to delete.
    fn ours_to_delete(&self, id: u32) -> bool {
        match self.held.get(&id) {
            Some(held) => self.holds_ours(id, held),
            None => self.written.contains_key(&id),
        }
    }

    /// The writes and deletes that bring the metadata to `wanted` ([`Want`]), and every other
    /// stream FxSound has moved — or whose key puts it onto a route of FxSound's
    /// ([`Self::takes_over`]) — back. Deletes first, then writes, each in order of subject.
    ///
    /// A key that already names the route node a stream is wanted on is taken for this engine's
    /// own, with nothing sent: WirePlumber's restore of the application's last target, when it
    /// names the route its rule wants, is exactly what FxSound would have written — and a write of
    /// the value the server holds would never be reported back.
    pub(crate) fn plan(&mut self, wanted: &[(u32, Want)]) -> Vec<MetadataOp> {
        for &(id, want) in wanted {
            if let Want::Onto(serial) = want
                && self.written.get(&id) != Some(&serial)
                && !self.unconfirmed.contains_key(&id)
                && self
                    .held
                    .get(&id)
                    .is_some_and(|held| held.value.parse::<u64>() == Ok(serial))
            {
                self.written.insert(id, serial);
            }
        }
        self.ops(wanted)
    }

    fn ops(&self, wanted: &[(u32, Want)]) -> Vec<MetadataOp> {
        // Whether the key of the stream `id` is where the plan wants it, or on its way there.
        let stays = |id: u32| {
            wanted.iter().any(|&(wanted, want)| {
                wanted == id
                    && match want {
                        Want::Onto(_) => true,
                        Want::Pending { still } => {
                            let now = self.target(id);
                            now.is_none() || now == still
                        }
                    }
            })
        };
        let mut deletes: Vec<u32> = self
            .written
            .keys()
            .chain(self.held.keys())
            .copied()
            .filter(|&id| !stays(id))
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
            .filter_map(|&(id, want)| match want {
                Want::Onto(serial) => Some((id, serial)),
                Want::Pending { .. } => None,
            })
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

    /// Every key FxSound holds, to delete: the engine is going. That is every key this engine
    /// wrote, and every key that puts a stream onto a route node of this process's whoever wrote
    /// it — the node goes with the process, and no choice of anybody's survives it. A recorder's
    /// key naming a playback route's sink is its user's, and stays like a key naming any other
    /// node (module docs, "The metadata").
    pub(crate) fn everything_back(&self) -> Vec<MetadataOp> {
        self.ops(&[])
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
    use super::Want::{Onto, Pending};
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

    /// A rule that has `app` follow its lane, as the app sends one: no preset, and a snapshot of
    /// the lane's kind that nothing runs.
    fn follow_rule(direction: DeviceDirection, app: AppKey) -> AppRoute {
        AppRoute {
            direction,
            app,
            preset: String::new(),
            params: match direction {
                DeviceDirection::Output => RouteParams::Output(DspParams::default()),
                DeviceDirection::Input => RouteParams::Input(InputDspParams::default()),
            },
            chain: String::new(),
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
            on_route: None,
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
            output_rule(AppKey::default(), "Gaming", 1.0),
            output_rule(named("Game"), " Gaming ", 1.0),
        ]);
        assert_eq!(
            refused.iter().map(|(_, why)| *why).collect::<Vec<_>>(),
            vec![Refusal::Inconsistent, Refusal::NoApplication]
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules.preset_for(OUT, &named("Game")),
            Some("Gaming"),
            "a preset's name is trimmed"
        );
    }

    #[test]
    fn a_rule_with_a_blank_preset_is_taken_as_one_that_follows_the_lane() {
        let (rules, refused) = Rules::new(vec![output_rule(named("Game"), "  ", 1.0)]);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules.preset_for(OUT, &named("Game")), None);
        assert!(rules.presets().is_empty(), "it names no route");
        assert!(!rules.names(OUT, ""));
        assert_eq!(rules.preset(OUT, ""), None);
    }

    #[test]
    fn a_rule_that_follows_keeps_its_application_off_a_more_general_rules_preset() {
        // The native Firefox's rule runs Music; the Flatpak one has a rule of its own that follows
        // the lane. The native rule matches the Flatpak's stream too, by its binary, but the
        // Flatpak's own rule matches it by its id.
        let native = app("firefox", "Firefox", "");
        let sandboxed = app("firefox", "Firefox", "org.mozilla.firefox");
        let rules = rules(vec![
            output_rule(native.clone(), "Music", 1.0),
            follow_rule(OUT, sandboxed.clone()),
            follow_rule(IN, native.clone()),
            input_rule(sandboxed.clone(), "Headset", "voice"),
        ]);
        assert_eq!(rules.preset_for(OUT, &native), Some("Music"));
        assert_eq!(rules.preset_for(OUT, &sandboxed), None);
        assert_eq!(rules.preset_for(IN, &sandboxed), Some("Headset"));
        assert_eq!(
            rules.preset_for(IN, &native),
            None,
            "the Flatpak's rule matches the native stream by its binary too; the native rule is \
             its own"
        );
        // A game known by its name alone to one rule and by its program to another.
        let rules = self::rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            follow_rule(OUT, app("game.exe", "Game", "")),
        ]);
        assert_eq!(rules.preset_for(OUT, &app("game.exe", "Game", "")), None);
        assert_eq!(
            rules.preset_for(OUT, &named("Game")),
            Some("Gaming"),
            "a stream with no program is the rule for the name's"
        );
        assert_eq!(
            rules
                .presets()
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            ["Gaming"]
        );
    }

    #[test]
    fn a_native_application_on_a_preset_and_a_flatpak_one_that_follows_are_planned_apart() {
        let mut table = RouteTable::default();
        let native = app("firefox", "Firefox", "");
        let sandboxed = app("firefox", "Firefox", "org.mozilla.firefox");
        let rules = rules(vec![
            output_rule(native.clone(), "Music", 1.0),
            follow_rule(OUT, sandboxed.clone()),
        ]);
        let plan = table.plan(
            &rules,
            &[stream(20, OUT, native), stream(21, OUT, sandboxed)],
            &both(ATTACHED),
            Instant::now(),
            ROUTE_IDLE,
        );
        assert_eq!(built(&plan), vec![(slot(OUT, 1), "Music")]);
        assert_eq!(
            plan.assigned,
            vec![(20, slot(OUT, 1))],
            "the Flatpak Firefox stays on the lane"
        );
    }

    #[test]
    fn a_rule_that_starts_or_stops_following_moves_applications_and_no_preset() {
        let native = app("firefox", "Firefox", "");
        let sandboxed = app("firefox", "Firefox", "org.mozilla.firefox");
        let before = rules(vec![output_rule(native.clone(), "Music", 1.0)]);
        let after = rules(vec![
            output_rule(native, "Music", 1.0),
            follow_rule(OUT, sandboxed),
        ]);
        let changes = diff(&before, &after);
        assert!(changes.applications);
        assert!(changes.added.is_empty() && changes.removed.is_empty());
        assert!(changes.params.is_empty() && changes.chains.is_empty());
        assert!(diff(&after, &after.clone()).is_empty());
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
        let ops = moves.plan(&[(40, Onto(301)), (41, Onto(301))]);
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
        assert!(moves.plan(&[(40, Onto(301)), (41, Onto(301))]).is_empty());

        // 41 leaves the route.
        let ops = moves.plan(&[(40, Onto(301))]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 41 }]);
        moves.sent(ops[0]);
        moves.heard(41, None);
        assert_eq!(moves.written(41), None);
        assert!(moves.plan(&[(40, Onto(301))]).is_empty());
    }

    #[test]
    fn a_route_rebuilt_under_a_new_serial_is_written_again_once_its_node_is_announced() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.heard(40, Some("301"));
        // Rebuilt — on the lane's new device, or after a failure: the old node goes in this tick,
        // and the key naming it goes first, rather than be left naming a node that is not there.
        // Nothing is written while the new node is not announced.
        let ops = moves.plan(&[(40, Pending { still: None })]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 40 }]);
        moves.sent(ops[0]);
        assert!(
            moves.plan(&[(40, Pending { still: None })]).is_empty(),
            "deleted once"
        );
        moves.heard(40, None);
        assert!(moves.plan(&[(40, Pending { still: None })]).is_empty());
        // Announced.
        moves.route_node(377, OUT);
        assert_eq!(
            moves.plan(&[(40, Onto(377))]),
            vec![MetadataOp::Write {
                subject: 40,
                serial: 377
            }]
        );
    }

    #[test]
    fn a_key_on_a_route_kept_while_its_lane_is_between_pairs_is_left_as_it_is() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.heard(40, Some("301"));
        // The lane has no pair for a moment; the route keeps its node, and the stream its key.
        assert!(moves.plan(&[(40, Pending { still: Some(301) })]).is_empty());
        assert_eq!(moves.written(40), Some(301));
        // A write still on its way onto that node is left on its way, too.
        moves.sent(MetadataOp::Write {
            subject: 41,
            serial: 301,
        });
        assert!(
            moves
                .plan(&[
                    (40, Pending { still: Some(301) }),
                    (41, Pending { still: Some(301) })
                ])
                .is_empty()
        );
    }

    #[test]
    fn a_key_naming_another_route_than_the_one_a_stream_waits_for_is_deleted_meanwhile() {
        // The game's preset runs under o1 now; its own route is being built under o2. The key
        // WirePlumber restored onto o1 is not left there while o2's node is on its way: the game
        // would play through somebody else's preset until it is.
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.heard(40, Some("301"));
        let ops = moves.plan(&[(40, Pending { still: None })]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 40 }]);
        moves.sent(ops[0]);
        // And a stream waiting for a route between pairs whose own node is not o1.
        moves.route_node(302, OUT);
        moves.heard(41, Some("301"));
        let ops = moves.plan(&[(41, Pending { still: Some(302) })]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 41 }]);
        moves.sent(ops[0]);
        // Nothing to delete for a stream with no key; and a key of somebody else's is theirs.
        assert!(moves.plan(&[(42, Pending { still: None })]).is_empty());
        moves.heard(43, Some("57"));
        assert!(moves.plan(&[(43, Pending { still: None })]).is_empty());
    }

    #[test]
    fn a_stream_moved_by_hand_is_neither_moved_again_nor_moved_back() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
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

    /// A key naming a route that is there, which no change of FxSound's accounts for — a mixer's
    /// move, or WirePlumber restoring the application's last target by the route's name — is
    /// FxSound's all the same: which preset runs under that number is FxSound's business, and the
    /// application's rule's, not the name's.
    #[test]
    fn a_key_naming_a_route_that_is_there_is_fxsounds_whoever_wrote_it() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        // A player no rule names, put onto "FxSound (Output) · Gaming".
        moves.heard(44, Some("301"));
        assert!(!moves.moved_by_hand(44));
        let ops = moves.plan(&[]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 44 }]);
        moves.sent(ops[0]);
        // A player whose rule wants another route: moved onto that one.
        moves.route_node(302, OUT);
        moves.heard(45, Some("301"));
        assert!(!moves.moved_by_hand(45));
        assert_eq!(
            moves.plan(&[(45, Onto(302))]),
            vec![MetadataOp::Write {
                subject: 45,
                serial: 302
            }]
        );
    }

    #[test]
    fn the_echo_of_an_earlier_write_is_still_fxsounds_after_a_later_one() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 301,
        });
        moves.route_node(377, OUT);
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 377,
        });
        // The first write's echo arrives only now.
        moves.heard(40, Some("301"));
        assert!(!moves.moved_by_hand(40));
        assert_eq!(moves.written(40), Some(377));
        moves.heard(40, Some("377"));
        assert!(moves.plan(&[(40, Onto(377))]).is_empty());
        assert!(
            moves.unconfirmed.is_empty(),
            "both writes reported, nothing left to wait for"
        );
    }

    /// Two players on one route, the rule of one removed, and the user moving it back onto the
    /// route in a mixer: the route is FxSound's, and without a rule the player goes back.
    #[test]
    fn a_player_whose_rule_was_removed_is_moved_back_off_a_route_it_is_put_on_again() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        // Brave (40) and Chrome (41) both run "Volume Boost" on route o1.
        for op in moves.plan(&[(40, Onto(301)), (41, Onto(301))]) {
            moves.sent(op);
        }
        moves.heard(40, Some("301"));
        moves.heard(41, Some("301"));
        // Chrome's rule is removed: its key is deleted, and the server says so.
        let ops = moves.plan(&[(40, Onto(301))]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 41 }]);
        moves.sent(ops[0]);
        moves.heard(41, None);
        assert!(!moves.moved_by_hand(41));

        // The user moves Chrome onto "FxSound (Output) · Volume Boost" in pavucontrol.
        moves.heard(41, Some("301"));
        assert!(!moves.moved_by_hand(41), "a route of FxSound's");
        assert_eq!(moves.written(41), None);
        assert_eq!(
            moves.plan(&[(40, Onto(301))]),
            vec![MetadataOp::Delete { subject: 41 }],
            "no rule puts Chrome there"
        );
        assert_eq!(
            moves.everything_back(),
            vec![
                MetadataOp::Delete { subject: 40 },
                MetadataOp::Delete { subject: 41 }
            ],
        );
    }

    /// FxSound moves a stream from one route to another, and someone moves it back onto the first:
    /// not an old echo, and not the user's pick either — the stream goes back where its rule is.
    #[test]
    fn a_stream_put_back_onto_the_route_fxsound_moved_it_off_is_moved_where_its_rule_is() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(302, OUT);
        for serial in [301, 302] {
            moves.sent(MetadataOp::Write {
                subject: 40,
                serial,
            });
            moves.heard(40, Some(&serial.to_string()));
            assert!(!moves.moved_by_hand(40));
        }

        moves.heard(40, Some("301"));
        assert!(!moves.moved_by_hand(40));
        assert_eq!(
            moves.written(40),
            None,
            "FxSound no longer says the stream is on the second route's preset"
        );
        assert_eq!(
            moves.plan(&[(40, Onto(302))]),
            vec![MetadataOp::Write {
                subject: 40,
                serial: 302
            }]
        );
    }

    #[test]
    fn a_write_sent_after_a_delete_is_still_fxsounds_when_all_three_come_back() {
        // A rule added, removed and added again within one round trip: a write, a delete and a
        // write on their way at once. The two writes name routes that are both still there.
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(302, OUT);
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
            moves.plan(&[(40, Onto(302))]).is_empty(),
            "nothing to send: everything wanted is on its way"
        );
        moves.heard(40, None);
        assert!(!moves.moved_by_hand(40));
        moves.heard(40, Some("302"));
        assert!(!moves.moved_by_hand(40));
        assert_eq!(moves.written(40), Some(302));
        assert!(moves.plan(&[(40, Onto(302))]).is_empty());
        assert!(moves.unconfirmed.is_empty());
    }

    #[test]
    fn a_delete_on_its_way_is_not_sent_again_when_the_write_it_undoes_comes_back() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
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
        moves.route_node(301, OUT);
        moves.route_node(302, OUT);
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
            moves.plan(&[(40, Onto(302))]).is_empty(),
            "not written again for want of standing behind it"
        );
    }

    #[test]
    fn a_change_the_server_never_reports_is_settled_by_the_next_one_it_does() {
        // A delete of a key someone else deleted first changes nothing, and the server says
        // nothing of it. The next write's report settles it, and no stale serial of FxSound's is
        // left to be taken for its own later.
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(302, OUT);
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

        // Someone moves it onto 301: nothing FxSound sent accounts for that any more, and its
        // write onto 302 no longer stands — it is sent again.
        moves.heard(40, Some("301"));
        assert!(moves.unconfirmed.is_empty(), "not taken for an echo");
        assert_eq!(moves.written(40), None);
        assert_eq!(
            moves.plan(&[(40, Onto(302))]),
            vec![MetadataOp::Write {
                subject: 40,
                serial: 302
            }]
        );
    }

    #[test]
    fn a_key_naming_a_route_of_an_earlier_connection_is_fxsounds_and_is_taken_over() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
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
        moves.route_node(410, OUT);
        assert_eq!(
            moves.plan(&[(40, Onto(410))]),
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
        moves.route_node(301, OUT);
        moves.route_node(302, OUT);
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
        let ops = moves.plan(&[(10, Onto(302))]);
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

    // ---- keys FxSound did not write ------------------------------------------------------

    /// A stream with `on_route` set: sitting on the route in `slot` by something FxSound does not
    /// change.
    fn resting(id: u32, direction: DeviceDirection, app: AppKey, slot: RouteSlot) -> Candidate {
        Candidate {
            on_route: Some(slot),
            ..stream(id, direction, app)
        }
    }

    /// The `Want` the engine hands [`Moves::plan`] for each assigned stream, from the serial each
    /// route's node has: `Onto` where the route is not built in this plan and its node is known,
    /// `Pending` with no node kept otherwise (`engine::route_pairs::reconcile`, attached lanes).
    fn wants(plan: &Plan, serials: &[(RouteSlot, u64)]) -> Vec<(u32, Want)> {
        plan.assigned
            .iter()
            .map(|&(id, slot)| {
                let built = plan.build.iter().any(|(built, _)| *built == slot);
                let serial = serials
                    .iter()
                    .find(|(known, _)| *known == slot)
                    .map(|&(_, serial)| serial)
                    .filter(|_| !built);
                (id, serial.map_or(Pending { still: None }, Onto))
            })
            .collect()
    }

    /// Firefox plays a video through "Movies" on o1, and the video ends. It opens a new stream for
    /// the next one within the idle period, and WirePlumber — which saved `fxsound_route_o1` as
    /// Firefox's target when FxSound moved it — writes o1's serial for the new stream the moment
    /// it appears, before FxSound plans it (`node/state-stream.lua`, `node.stream.restore-target`).
    #[test]
    fn a_player_the_session_manager_restores_onto_its_own_route_keeps_it_and_is_reported_on_it() {
        let rules = rules(vec![output_rule(named("Firefox"), "Movies", 1.0)]);
        let mut table = RouteTable::default();
        let mut moves = Moves::default();
        let start = Instant::now();
        let first = [stream(40, OUT, named("Firefox"))];
        let plan = table.plan(&rules, &first, &both(ATTACHED), start, ROUTE_IDLE);
        assert_eq!(built(&plan), vec![(slot(OUT, 1), "Movies")]);
        moves.route_node(301, OUT);
        for op in moves.plan(&[(40, Onto(301))]) {
            moves.sent(op);
        }
        moves.heard(40, Some("301"));
        moves.stream_gone(40);
        let ended = start + Duration::from_secs(2);
        table.plan(&rules, &[], &both(ATTACHED), ended, ROUTE_IDLE);

        // The next video: WirePlumber's restore arrives right after the stream appears.
        moves.heard(52, Some("301"));
        assert!(!moves.moved_by_hand(52), "not the user's pick");
        let next = [Candidate {
            movable: !moves.moved_by_hand(52),
            ..stream(52, OUT, named("Firefox"))
        }];
        let back = ended + Duration::from_secs(3);
        let plan = table.plan(&rules, &next, &both(ATTACHED), back, ROUTE_IDLE);
        assert!(plan.build.is_empty() && plan.teardown.is_empty());
        assert_eq!(plan.assigned, vec![(52, slot(OUT, 1))]);
        assert!(
            moves.plan(&wants(&plan, &[(slot(OUT, 1), 301)])).is_empty(),
            "the key already says what FxSound would write"
        );
        assert_eq!(
            moves.written(52),
            Some(301),
            "FxSound stands behind it, and reports the stream on its route"
        );

        // The route is in use for as long as the stream plays, however long that is.
        for seconds in [10, 20, 60] {
            let plan = table.plan(
                &rules,
                &next,
                &both(ATTACHED),
                back + Duration::from_secs(seconds),
                ROUTE_IDLE,
            );
            assert!(plan.teardown.is_empty(), "{seconds} s on");
            assert!(moves.plan(&wants(&plan, &[(slot(OUT, 1), 301)])).is_empty());
        }
    }

    /// Game X ran through "Gaming" on o1 and quit; o1 went, and Discord's "Voice" took the number.
    /// X starts again, and WirePlumber restores it onto whatever node is called `fxsound_route_o1`
    /// now — Discord's preset. X goes onto a route of its own; and a recorder restored onto an
    /// OBS preset's input route likewise. Without a rule any more, X is moved back to its lane.
    #[test]
    fn a_stream_restored_onto_a_number_another_preset_now_runs_is_moved_onto_its_own_route() {
        let rules = rules(vec![
            output_rule(named("Discord"), "Voice", 1.0),
            output_rule(named("Game X"), "Gaming", 1.0),
            input_rule(named("OBS"), "Studio", "voice"),
            input_rule(named("Discord"), "Headset", "voice"),
        ]);
        let mut table = RouteTable::default();
        let mut moves = Moves::default();
        let now = Instant::now();
        let running = [
            stream(60, OUT, named("Discord")),
            stream(61, IN, named("OBS")),
        ];
        let plan = table.plan(&rules, &running, &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(
            built(&plan),
            vec![(slot(OUT, 1), "Voice"), (slot(IN, 1), "Studio")]
        );
        moves.route_node(301, OUT);
        moves.route_node(401, IN);
        for (id, direction) in [(60, OUT), (61, IN), (70, OUT), (71, IN)] {
            moves.stream_appeared(id, direction);
        }
        let routes = [(slot(OUT, 1), 301), (slot(IN, 1), 401)];
        for op in moves.plan(&[(60, Onto(301)), (61, Onto(401))]) {
            moves.sent(op);
        }
        moves.heard(60, Some("301"));
        moves.heard(61, Some("401"));

        // X appears, restored onto o1; Discord's capture appears, restored onto OBS's i1.
        moves.heard(70, Some("301"));
        moves.heard(71, Some("401"));
        assert!(!moves.moved_by_hand(70) && !moves.moved_by_hand(71));
        let all = [
            running[0].clone(),
            running[1].clone(),
            stream(70, OUT, named("Game X")),
            stream(71, IN, named("Discord")),
        ];
        let plan = table.plan(&rules, &all, &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(
            built(&plan),
            vec![(slot(OUT, 2), "Gaming"), (slot(IN, 2), "Headset")]
        );
        // Off the other preset's route at once, rather than through it until their own is up.
        let ops = moves.plan(&wants(&plan, &routes));
        assert_eq!(
            ops,
            vec![
                MetadataOp::Delete { subject: 70 },
                MetadataOp::Delete { subject: 71 }
            ]
        );
        for op in ops {
            moves.sent(op);
        }
        moves.heard(70, None);
        moves.heard(71, None);
        // Their routes' nodes are announced: onto them.
        moves.route_node(302, OUT);
        moves.route_node(402, IN);
        let routes = [
            (slot(OUT, 1), 301),
            (slot(IN, 1), 401),
            (slot(OUT, 2), 302),
            (slot(IN, 2), 402),
        ];
        let plan = table.plan(&rules, &all, &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(
            moves.plan(&wants(&plan, &routes)),
            vec![
                MetadataOp::Write {
                    subject: 70,
                    serial: 302
                },
                MetadataOp::Write {
                    subject: 71,
                    serial: 402
                },
            ]
        );

        // Another day: X has no rule any more, and is restored onto o1 all the same.
        let rules = rules_without_x();
        let mut table = RouteTable::default();
        let mut moves = Moves::default();
        let discord = [stream(60, OUT, named("Discord"))];
        table.plan(&rules, &discord, &both(ATTACHED), now, ROUTE_IDLE);
        moves.route_node(301, OUT);
        for op in moves.plan(&[(60, Onto(301))]) {
            moves.sent(op);
        }
        moves.heard(60, Some("301"));
        moves.heard(80, Some("301"));
        let plan = table.plan(
            &rules,
            &[discord[0].clone(), stream(80, OUT, named("Game X"))],
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        assert_eq!(plan.assigned, vec![(60, slot(OUT, 1))]);
        assert_eq!(
            moves.plan(&wants(&plan, &[(slot(OUT, 1), 301)])),
            vec![MetadataOp::Delete { subject: 80 }],
            "back to its lane, and WirePlumber forgets the route's name for it"
        );
    }

    fn rules_without_x() -> Rules {
        rules(vec![output_rule(named("Discord"), "Voice", 1.0)])
    }

    #[test]
    fn a_key_already_naming_the_route_a_stream_is_wanted_on_is_taken_as_written() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.heard(52, Some("301"));
        assert!(moves.plan(&[(52, Onto(301))]).is_empty());
        assert_eq!(moves.written(52), Some(301));
        assert!(!moves.moved_by_hand(52));
        // Not while a write of FxSound's is on its way: the server ends up on that one, and the
        // stream is written onto the route it is wanted on after it.
        moves.route_node(302, OUT);
        moves.sent(MetadataOp::Write {
            subject: 53,
            serial: 302,
        });
        moves.heard(53, Some("301"));
        assert_eq!(
            moves.plan(&[(52, Onto(301)), (53, Onto(301))]),
            vec![MetadataOp::Write {
                subject: 53,
                serial: 301
            }]
        );
    }

    #[test]
    fn a_route_a_stream_fxsound_does_not_move_sits_on_is_never_idle_and_never_given_away() {
        let presets = ["Gaming", "Two", "Three", "Four", "Five"];
        let rules = rules(
            presets
                .iter()
                .map(|preset| output_rule(named(preset), preset, 1.0))
                .collect(),
        );
        let mut table = RouteTable::default();
        let start = Instant::now();
        let game = stream(40, OUT, named("Gaming"));
        table.plan(
            &rules,
            std::slice::from_ref(&game),
            &both(ATTACHED),
            start,
            ROUTE_IDLE,
        );
        // A tester whose own properties name `fxsound_route_o1`, and no rule names: it plays
        // through o1 whatever FxSound decides.
        let tester = resting(44, OUT, named("pw-play"), slot(OUT, 1));
        // The game quits; the tester goes on.
        let later = start + ROUTE_IDLE * 3;
        let plan = table.plan(
            &rules,
            std::slice::from_ref(&tester),
            &both(ATTACHED),
            later,
            ROUTE_IDLE,
        );
        assert!(plan.teardown.is_empty(), "{:?}", plan.teardown);
        assert_eq!(table.routes()[0].idle_since, None);

        // Four more presets want a route: three get one, the fourth does not take o1's place.
        let mut all = vec![tester.clone()];
        all.extend(
            presets[1..]
                .iter()
                .enumerate()
                .map(|(index, preset)| stream(50 + index as u32, OUT, named(preset))),
        );
        let plan = table.plan(&rules, &all, &both(ATTACHED), later, ROUTE_IDLE);
        assert!(plan.teardown.is_empty(), "{:?}", plan.teardown);
        assert_eq!(plan.build.len(), 3);
        assert_eq!(plan.overflow.len(), 1);
        assert_eq!(table.preset_of(slot(OUT, 1)), Some("Gaming"));

        // Once the tester has gone, o1 is unused like any other route, and the fifth preset takes
        // its place.
        let plan = table.plan(
            &rules,
            &all[1..],
            &both(ATTACHED),
            later + Duration::from_secs(1),
            ROUTE_IDLE,
        );
        assert_eq!(gone(&plan), vec![(slot(OUT, 1), Teardown::Evicted)]);
        assert_eq!(built(&plan), vec![(slot(OUT, 1), "Five")]);
    }

    #[test]
    fn a_stream_fxsound_moves_off_a_route_does_not_keep_it_in_use() {
        // Its rule names another preset: the plan moves it — by a key WirePlumber reads ahead of
        // the stream's own properties — and the route it leaves is idle like any other.
        let rules = rules(vec![
            output_rule(named("Game"), "Gaming", 1.0),
            output_rule(named("Brave"), "Volume Boost", 1.0),
        ]);
        let mut table = RouteTable::default();
        let start = Instant::now();
        table.plan(
            &rules,
            &[stream(40, OUT, named("Game"))],
            &both(ATTACHED),
            start,
            ROUTE_IDLE,
        );
        let brave = resting(41, OUT, named("Brave"), slot(OUT, 1));
        let left = start + Duration::from_secs(1);
        let plan = table.plan(
            &rules,
            std::slice::from_ref(&brave),
            &both(ATTACHED),
            left,
            ROUTE_IDLE,
        );
        assert_eq!(plan.assigned, vec![(41, slot(OUT, 2))]);
        assert_eq!(table.routes()[0].idle_since, Some(left));
    }

    /// The game's rule is removed while a tester that names o1 in its own properties — or a
    /// player WirePlumber will not move again — plays through it: the route is not taken down
    /// under it, the plan says so once, and it goes as soon as that stream has.
    #[test]
    fn a_route_whose_rule_goes_is_kept_while_a_stream_fxsound_does_not_move_is_on_it() {
        let with = rules(vec![output_rule(named("Game"), "Gaming", 1.0)]);
        let mut table = RouteTable::default();
        let now = Instant::now();
        let game = stream(40, OUT, named("Game"));
        table.plan(
            &with,
            std::slice::from_ref(&game),
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        let pinned = Candidate {
            movable: false,
            ..resting(44, OUT, named("Old player"), slot(OUT, 1))
        };
        let streams = [game.clone(), pinned.clone()];
        let plan = table.plan(
            &Rules::default(),
            &streams,
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        assert!(plan.teardown.is_empty());
        assert!(plan.assigned.is_empty(), "the game goes back to its lane");
        assert_eq!(plan.kept, vec![(slot(OUT, 1), "Gaming".to_owned())]);
        let plan = table.plan(
            &Rules::default(),
            &streams,
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        assert!(plan.kept.is_empty(), "said once");
        assert!(plan.teardown.is_empty());

        // The rule comes back while the route is still there: the game goes back onto it, and
        // nothing is built.
        let plan = table.plan(&with, &streams, &both(ATTACHED), now, ROUTE_IDLE);
        assert!(plan.build.is_empty() && plan.teardown.is_empty());
        assert_eq!(plan.assigned, vec![(40, slot(OUT, 1))]);
        assert!(!table.routes()[0].orphaned);

        // Gone again, and then the player leaves: the route goes at once.
        table.plan(
            &Rules::default(),
            &streams,
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        let plan = table.plan(&Rules::default(), &[game], &both(ATTACHED), now, ROUTE_IDLE);
        assert_eq!(gone(&plan), vec![(slot(OUT, 1), Teardown::RuleGone)]);

        // A lane switched off takes such a route all the same: it has no device to be on.
        let mut table = RouteTable::default();
        table.plan(
            &with,
            &[stream(40, OUT, named("Game"))],
            &both(ATTACHED),
            now,
            ROUTE_IDLE,
        );
        let plan = table.plan(&with, &[pinned], &both(LaneState::Off), now, ROUTE_IDLE);
        assert_eq!(gone(&plan), vec![(slot(OUT, 1), Teardown::LaneOff)]);
    }

    #[test]
    fn a_target_a_stream_names_itself_is_found_among_the_routes_by_name_serial_or_id() {
        let nodes = [(slot(OUT, 1), 70, 700), (slot(IN, 2), 72, 720)];
        let object = |value: &str| ExplicitTarget::Object(value.to_owned());
        let node = |value: &str| ExplicitTarget::Node(value.to_owned());
        assert_eq!(
            route_of_target(&object("fxsound_route_o1"), &nodes),
            Some(slot(OUT, 1))
        );
        assert_eq!(
            route_of_target(&object("fxsound_route_o3"), &nodes),
            Some(slot(OUT, 3)),
            "by name, whatever node carries the name now or will"
        );
        assert_eq!(route_of_target(&object("700"), &nodes), Some(slot(OUT, 1)));
        assert_eq!(route_of_target(&node("72"), &nodes), Some(slot(IN, 2)));
        assert_eq!(
            route_of_target(&node(" fxsound_route_i2 "), &nodes),
            Some(slot(IN, 2))
        );
        for (target, why) in [
            (object("70"), "an id is no serial"),
            (node("700"), "a serial is no id"),
            (
                object("fxsound_route_o1_play"),
                "the route's stream is not its node",
            ),
            (object("fxsound_sink"), "the lane's node is no route"),
            (object("alsa_output.usb"), "somebody else's"),
            (object("-1"), "nothing"),
        ] {
            assert_eq!(route_of_target(&target, &nodes), None, "{why}");
        }
    }

    #[test]
    fn a_stream_that_will_not_be_moved_again_is_anchored_where_its_key_put_it() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(302, OUT);
        // A `node.dont-reconnect` player WirePlumber restored onto o1.
        moves.heard(80, Some("301"));
        assert_eq!(moves.anchor(80), Some(301));
        // Its key is FxSound's to delete — it moves nothing, and is what WirePlumber would
        // restore next time — and the stream stays anchored where it was put.
        let ops = moves.plan(&[]);
        assert_eq!(ops, vec![MetadataOp::Delete { subject: 80 }]);
        moves.sent(ops[0]);
        moves.heard(80, None);
        assert!(!moves.has_key(80));
        assert_eq!(moves.anchor(80), Some(301));
        // A later key moves it nowhere: WirePlumber will not.
        moves.heard(80, Some("302"));
        assert_eq!(moves.anchor(80), Some(301));
        // Gone with the stream, and with the connection.
        moves.stream_gone(80);
        assert_eq!(moves.anchor(80), None);
        moves.heard(81, Some("301"));
        assert_eq!(moves.anchor(81), Some(301));
        moves.forget_session();
        assert_eq!(moves.anchor(81), None);
        // Somebody else's target anchors nothing of FxSound's.
        moves.heard(82, Some("57"));
        assert_eq!(moves.anchor(82), None);
    }

    #[test]
    fn a_stream_with_no_key_has_none_and_one_with_a_key_of_any_kind_has_one() {
        let mut moves = Moves::default();
        assert!(!moves.has_key(40));
        moves.heard(40, Some("alsa_output.usb"));
        assert!(moves.has_key(40));
        assert_eq!(moves.target(40), None, "a name is no serial");
        moves.sent(MetadataOp::Write {
            subject: 41,
            serial: 301,
        });
        assert!(moves.has_key(41), "a write on its way");
        assert_eq!(moves.target(41), Some(301));
    }

    /// FxSound quits while a key it did not write names one of its routes — one it has not planned
    /// yet, WirePlumber's restore. The route goes with FxSound; the key would name a node the next
    /// run never made, and pin the stream as moved by hand for as long as it plays.
    #[test]
    fn every_key_naming_a_route_of_fxsounds_is_deleted_on_the_way_out_whoever_wrote_it() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(290, OUT);
        moves.heard(44, Some("301"));
        // One naming a route of an earlier connection's, and one somebody else's.
        moves.heard(45, Some("290"));
        moves.heard(46, Some("57"));
        assert_eq!(
            moves.everything_back(),
            vec![
                MetadataOp::Delete { subject: 44 },
                MetadataOp::Delete { subject: 45 }
            ]
        );
    }

    /// OBS records the desktop (`stream.capture.sink`, so FxSound never moves it), and the user
    /// points it at `Monitor of FxSound (Output) · Gaming` in pavucontrol, which `pipewire-pulse`
    /// writes as the route sink's serial. That records what the game plays through its preset; it
    /// puts nothing onto the route, and FxSound leaves the key where the user put it — through
    /// every plan, the route's other streams coming and going, and the way out.
    #[test]
    fn a_recorder_pointed_at_a_playback_routes_monitor_keeps_its_key_through_every_plan_and_exit() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.stream_appeared(90, IN);
        moves.heard(90, Some("301"));
        assert!(moves.moved_by_hand(90), "the user's pick");
        assert_eq!(moves.monitored(90), Some(301));
        assert_eq!(moves.written(90), None);
        assert!(moves.plan(&[]).is_empty(), "not deleted");

        // The game goes onto o1 and leaves it again; the recorder's key is neither deleted nor
        // rewritten meanwhile.
        let ops = moves.plan(&[(40, Onto(301))]);
        assert_eq!(
            ops,
            vec![MetadataOp::Write {
                subject: 40,
                serial: 301
            }]
        );
        moves.sent(ops[0]);
        moves.heard(40, Some("301"));
        assert!(moves.plan(&[(40, Onto(301))]).is_empty());
        assert_eq!(
            moves.everything_back(),
            vec![MetadataOp::Delete { subject: 40 }],
            "only the game's key goes on the way out"
        );
        assert_eq!(
            moves.plan(&[]),
            vec![MetadataOp::Delete { subject: 40 }],
            "the game's key goes with its rule, the recorder's stays"
        );
        assert_eq!(moves.anchor(90), None, "nothing of FxSound's to remember");

        // A route since rebuilt, or of an earlier connection: the recorder records a monitor that
        // is no more, as it would any other sink's that went; still the user's key.
        moves.forget_session();
        moves.stream_appeared(90, IN);
        moves.heard(90, Some("301"));
        assert!(moves.moved_by_hand(90));
        assert_eq!(moves.monitored(90), Some(301));
        assert!(moves.plan(&[]).is_empty());
        assert!(moves.everything_back().is_empty());
    }

    /// Discord's microphone capture runs through "Voice" on i1, and the user points it at the
    /// monitor of o1 instead. FxSound's write no longer stands, nor does it write it again: the
    /// capture is no longer on its route, and the user's key stays.
    #[test]
    fn a_recorder_the_user_moves_from_its_route_onto_a_playback_routes_monitor_is_left_there() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(401, IN);
        moves.stream_appeared(91, IN);
        for op in moves.plan(&[(91, Onto(401))]) {
            moves.sent(op);
        }
        moves.heard(91, Some("401"));
        assert_eq!(moves.written(91), Some(401));
        assert_eq!(moves.monitored(91), None, "on its own lane's route");

        moves.heard(91, Some("301"));
        assert!(moves.moved_by_hand(91));
        assert_eq!(
            moves.written(91),
            None,
            "not reported on its route any more"
        );
        assert_eq!(moves.monitored(91), Some(301));
        // Moved by hand, it is planned onto nothing, and nothing of its is sent.
        assert!(moves.plan(&[]).is_empty());
        assert!(moves.everything_back().is_empty());
    }

    /// The exception is a recorder's key naming a playback route's sink, and nothing wider: a
    /// recorder's key naming a recording route's source puts it onto that route, a player's key
    /// naming a playback route puts it onto that one, and both are FxSound's whoever wrote them.
    /// A subject never announced as a recorder is none.
    #[test]
    fn only_a_recorders_key_naming_a_playback_routes_sink_is_left_to_whoever_wrote_it() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(401, IN);
        moves.stream_appeared(90, IN);
        moves.stream_appeared(40, OUT);
        moves.heard(90, Some("401"));
        moves.heard(40, Some("301"));
        moves.heard(41, Some("301"));
        for id in [90, 40, 41] {
            assert!(!moves.moved_by_hand(id), "{id}");
            assert_eq!(moves.monitored(id), None, "{id}");
        }
        assert_eq!(
            moves.plan(&[]),
            vec![
                MetadataOp::Delete { subject: 40 },
                MetadataOp::Delete { subject: 41 },
                MetadataOp::Delete { subject: 90 },
            ]
        );

        // A stream is what it was last announced as: a player under an id the server has handed
        // on from a recorder is a player, and its key naming o1 is FxSound's; and a recorder
        // announced under an id that went is a recorder again.
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.stream_appeared(92, IN);
        moves.heard(92, Some("301"));
        assert_eq!(moves.monitored(92), Some(301));
        moves.stream_appeared(92, OUT);
        assert_eq!(moves.monitored(92), None);
        assert_eq!(moves.plan(&[]), vec![MetadataOp::Delete { subject: 92 }]);
        moves.stream_gone(92);
        moves.stream_appeared(92, IN);
        moves.heard(92, Some("301"));
        assert_eq!(moves.monitored(92), Some(301));
        assert!(moves.plan(&[]).is_empty());
    }

    /// OBS goes on recording the monitor of the game's route after the game has quit, and after
    /// its rule has gone: the route is in use — never idle, never given away — and kept, said
    /// once, until OBS stops recording it, when it goes at once.
    #[test]
    fn a_recorder_of_a_routes_monitor_keeps_the_playback_route_in_use() {
        let with = rules(vec![output_rule(named("Game"), "Gaming", 1.0)]);
        let mut table = RouteTable::default();
        let start = Instant::now();
        table.plan(
            &with,
            &[stream(40, OUT, named("Game"))],
            &both(ATTACHED),
            start,
            ROUTE_IDLE,
        );
        let obs = Candidate {
            movable: false,
            ..resting(90, IN, named("OBS"), slot(OUT, 1))
        };
        let later = start + ROUTE_IDLE * 3;
        let plan = table.plan(
            &with,
            std::slice::from_ref(&obs),
            &both(ATTACHED),
            later,
            ROUTE_IDLE,
        );
        assert!(plan.teardown.is_empty(), "{:?}", plan.teardown);
        assert!(plan.assigned.is_empty() && plan.build.is_empty());
        assert_eq!(table.routes()[0].idle_since, None);

        let plan = table.plan(
            &Rules::default(),
            std::slice::from_ref(&obs),
            &both(ATTACHED),
            later,
            ROUTE_IDLE,
        );
        assert!(plan.teardown.is_empty());
        assert_eq!(plan.kept, vec![(slot(OUT, 1), "Gaming".to_owned())]);

        let plan = table.plan(&Rules::default(), &[], &both(ATTACHED), later, ROUTE_IDLE);
        assert_eq!(gone(&plan), vec![(slot(OUT, 1), Teardown::RuleGone)]);
    }

    /// Someone deletes the key FxSound wrote — `pw-metadata -d`, a tool clearing the stream's keys
    /// — and WirePlumber moves the stream back to the default. FxSound neither goes on saying it
    /// is on its route nor leaves it off: the key is written again.
    #[test]
    fn a_delete_someone_else_sent_takes_back_what_fxsound_wrote_and_the_key_is_written_again() {
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        for op in moves.plan(&[(40, Onto(301))]) {
            moves.sent(op);
        }
        moves.heard(40, Some("301"));
        assert_eq!(moves.written(40), Some(301));

        moves.heard(40, None);
        assert_eq!(moves.written(40), None, "not reported as routed any more");
        assert!(!moves.moved_by_hand(40));
        let ops = moves.plan(&[(40, Onto(301))]);
        assert_eq!(
            ops,
            vec![MetadataOp::Write {
                subject: 40,
                serial: 301
            }]
        );
        moves.sent(ops[0]);
        moves.heard(40, Some("301"));
        assert_eq!(moves.written(40), Some(301));
        assert!(moves.plan(&[(40, Onto(301))]).is_empty());
    }

    #[test]
    fn a_delete_someone_else_sent_before_a_write_of_fxsounds_does_not_take_that_write_back() {
        // FxSound writes 302 while someone deletes the key; the server applies the delete first,
        // and the write after it, and ends up holding what FxSound sent.
        let mut moves = Moves::default();
        moves.route_node(301, OUT);
        moves.route_node(302, OUT);
        for op in moves.plan(&[(40, Onto(301))]) {
            moves.sent(op);
        }
        moves.heard(40, Some("301"));
        moves.sent(MetadataOp::Write {
            subject: 40,
            serial: 302,
        });
        moves.heard(40, None);
        assert_eq!(moves.written(40), Some(302));
        assert!(
            moves.plan(&[(40, Onto(302))]).is_empty(),
            "not written twice"
        );
        moves.heard(40, Some("302"));
        assert_eq!(moves.written(40), Some(302));
        assert!(moves.unconfirmed.is_empty());
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
