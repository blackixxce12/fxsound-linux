//! Application streams: every player and recorder in the graph, as the engine sees them
//! (`docs/0.4.0-apps.md`, "Identifying an application").
//!
//! A per-application preset needs two things from the graph before any route exists: which
//! applications play or record right now, so the Applications list can show them and the app can
//! match its rules against them, and, for each one, whether it may be moved at all. This module is
//! both, as plain data the main loop feeds from the registry: [`StreamNode`] is one client stream,
//! and [`AppStreams`] is every one of them, what their clients say about themselves, and what the
//! app was last told ([`AudioToUi::AppStreams`]).
//!
//! # Where an application says who it is
//!
//! Not where the contract first looked. A stream node's registry global carries
//! `application.name`, `media.class`, `node.name`, `object.serial` and `client.id`, and nothing
//! else this module reads: `application.process.binary`, `pipewire.access.portal.app_id`,
//! `target.object`, `node.dont-move` and `stream.capture.sink` are only in the node's info, which
//! arrives once the node is bound. Measured on PipeWire 1.6.8 in a private daemon, `pw-cli ls Node`
//! against `pw-dump`, with `pw-cat --playback --properties='{application.name=Test,
//! application.process.binary=test.exe}'`: the global said `application.name = "Test"`, the info
//! said both. And a native stream that sets nothing of its own carries `application.name` alone —
//! its binary is a property of its *client*, the connection that made it, and again only in the
//! client's info. So the engine binds every application stream and every client, and a stream's
//! [`AppKey`] is:
//!
//! - its binary and its name from its own properties, else from its client's. The stream's are the
//!   more particular of the two: they describe this stream, the client's the whole connection.
//! - its Flatpak id from its client's properties first, and from its own only when the client has
//!   none. The client's is set by the access control that knows the application is a Flatpak; a
//!   stream can put anything it likes in its own properties, that key included — `pw-cat` did, in
//!   the measurement above — and the Flatpak id is meant to be the one identifier another program
//!   cannot borrow ([`AppKey`]).
//!
//! A stream is reported once its own info is in, and its client's ([`AppStreams::news`]): before
//! that it is a node with a name, and reported then it would be reported again a moment later
//! under a different key.
//!
//! # What an application stream is
//!
//! A node whose `media.class` is exactly `Stream/Output/Audio` (a player: the output lane's) or
//! `Stream/Input/Audio` (a recorder: the input lane's) — not an `…/Internal` stream of the session
//! manager's, not a device, not video — and that is not FxSound's own ([`is_fxsound_node`]):
//! FxSound's playback stream is a player like any other to the graph, and an Applications list
//! that offered it a preset would offer to run FxSound through itself. Of the rest, two kinds are
//! set apart:
//!
//! - **A recorder of what a sink plays** (a screen recorder's desktop audio, a visualiser) records
//!   no microphone. It is no input-lane application — moved onto a route behind the microphone,
//!   it would record the microphone — so it is left out, unless what it records is FxSound. Then
//!   it is listed, because it hears FxSound, and never moved ([`Pin::Monitor`]). A recorder is one
//!   of these two ways:
//!   - it says `stream.capture.sink = true`, and records a sink's monitor. It records FxSound when
//!     it names one of FxSound's nodes as its target, or names none while FxSound's sink is the
//!     default sink it follows.
//!   - it names, as its own target, a node of FxSound's output lane that WirePlumber links it to
//!     with no flag at all ([`ExplicitTarget::plays_fxsound_to`]): a sink — `fxsound_sink`, a
//!     playback route's `fxsound_route_o<N>` — by its serial or id, whose monitor it then records,
//!     or a playback stream — `fxsound_output`, `fxsound_route_o<N>_play` — whose output it
//!     records. `pw-record --target <serial of fxsound_sink>` records FxSound.
//! - **A stream that may not be moved** is listed like any other, and marked ([`Pin`]): one that
//!   says `node.dont-move = true`, whose metadata target WirePlumber ignores anyway
//!   (`linking/find-defined-target.lua`), and one whose own properties name a target that is not
//!   FxSound. That application chose its device itself, and a route would undo the choice.
//!
//! And a stream that says nothing about who it is — no binary, no name, no Flatpak id, its own or
//! its client's — is left out too: the list could not name it, and no rule could match it.
//!
//! Each stream's `object.serial` is kept beside its id. The server hands a freed id to the next
//! object but never a serial, so a stream announced under an id this module still holds is a new
//! stream, and the old one is forgotten ([`AppStreams::stream_appeared`]).
//!
//! # When the app hears of it
//!
//! Registry and info events only change what is known here. The supervisor's tick reports it
//! ([`AppStreams::news`]): the whole list, and only when it differs from what the app was last
//! told. An application that starts opens its connection and its stream, and both infos follow
//! within milliseconds, so the app hears of it once rather than once per event.
//!
//! Everything here runs on the main loop, from registry and info events; nothing is anywhere near
//! a process callback.
//!
//! [`AudioToUi::AppStreams`]: fxsound_core::messages::AudioToUi::AppStreams

use std::collections::HashMap;

use fxsound_core::messages::AppStream;
use fxsound_core::{AppKey, DeviceDirection};

use crate::{OUTPUT_NODE_NAME, ROUTE_NODE_PREFIX, SINK_NODE_NAME, is_fxsound_node};

/// `media.class` of a player's stream: an application of the output lane.
pub(crate) const PLAYBACK_MEDIA_CLASS: &str = "Stream/Output/Audio";

/// `media.class` of a recorder's stream: an application of the input lane.
pub(crate) const RECORDING_MEDIA_CLASS: &str = "Stream/Input/Audio";

/// `pipewire.sec.engine` of a connection Flatpak made inside a security context, whose
/// `pipewire.sec.app-id` is then a Flatpak application id ([`app_key`]).
const FLATPAK_ENGINE: &str = "org.flatpak";

/// The lane an application stream belongs to, from its `media.class`: `None` for anything that is
/// not a player's or a recorder's audio stream — a device, one of the session manager's internal
/// streams (`Stream/Input/Audio/Internal`), a video stream.
#[must_use]
pub(crate) fn stream_direction(media_class: &str) -> Option<DeviceDirection> {
    match media_class {
        PLAYBACK_MEDIA_CLASS => Some(DeviceDirection::Output),
        RECORDING_MEDIA_CLASS => Some(DeviceDirection::Input),
        _ => None,
    }
}

/// A boolean property read the way WirePlumber reads one (`cutils.parseBool` in
/// `lib/common-utils.lua`): `true` in any case, or `1`. Anything else, and no value at all, is
/// `false` — so FxSound and the session manager never disagree about whether a stream may move.
fn parse_bool(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1")
}

/// What one set of properties — a stream's or a client's — says about who an application is: each
/// identifier trimmed, and empty when the properties do not carry it.
///
/// The Flatpak id is `pipewire.access.portal.app_id`. A connection Flatpak made inside a PipeWire
/// security context says the same in `pipewire.sec.app-id`, with `pipewire.sec.engine =
/// org.flatpak`; that is read only when the first is missing, and only for Flatpak's engine,
/// because another sandbox's application id is not a Flatpak id and would never match a rule
/// written for one.
#[must_use]
pub(crate) fn app_key<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> AppKey {
    let text = |key: &str| get(key).map(str::trim).unwrap_or_default().to_owned();
    let mut flatpak = text("pipewire.access.portal.app_id");
    if flatpak.is_empty() && get("pipewire.sec.engine").map(str::trim) == Some(FLATPAK_ENGINE) {
        flatpak = text("pipewire.sec.app-id");
    }
    AppKey {
        binary: text("application.process.binary"),
        name: text("application.name"),
        flatpak,
    }
}

/// A stream's key from its own identifiers and its client's (module docs, "Where an application
/// says who it is"): the stream's binary and name, the client's filling in what they leave empty,
/// and the client's Flatpak id before the stream's.
#[must_use]
fn merged(own: &AppKey, client: Option<&AppKey>) -> AppKey {
    let Some(client) = client else {
        return own.clone();
    };
    let first = |preferred: &str, fallback: &str| {
        if preferred.is_empty() {
            fallback.to_owned()
        } else {
            preferred.to_owned()
        }
    };
    AppKey {
        binary: first(&own.binary, &client.binary),
        name: first(&own.name, &client.name),
        flatpak: first(&client.flatpak, &own.flatpak),
    }
}

/// How one of FxSound's nodes carries what FxSound plays, to a recorder linked to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plays {
    /// A sink applications play into — `fxsound_sink`, or a playback route's `fxsound_route_o<N>`
    /// — whose monitor a recorder records.
    Sink,
    /// A playback stream FxSound plays a chain's output with — `fxsound_output`, or a playback
    /// route's `fxsound_route_o<N>_play` — whose output a recorder records.
    Player,
}

/// Whether the node called `node_name` carries what FxSound plays, and how ([`Plays`]): the output
/// lane's pair and every playback route's pair, and nothing else. The input lane's nodes and the
/// recording routes' carry a microphone; the echo canceller's are its own business; anybody
/// else's node is not FxSound's.
///
/// A route's number is not checked. A node that starts the way a playback route's does is taken
/// for one: were it not, a recorder that hears it would be offered to the microphone's routes.
fn plays(node_name: &str) -> Option<Plays> {
    if node_name == SINK_NODE_NAME {
        return Some(Plays::Sink);
    }
    if node_name == OUTPUT_NODE_NAME {
        return Some(Plays::Player);
    }
    let route = node_name
        .strip_prefix(ROUTE_NODE_PREFIX)?
        .strip_prefix('o')?;
    Some(if route.ends_with("_play") {
        Plays::Player
    } else {
        Plays::Sink
    })
}

/// A target a stream names in its own properties: where the application asked to be linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExplicitTarget {
    /// `target.object`: a node's `node.name` or `object.path`, or its `object.serial`.
    Object(String),
    /// `node.target`, the key `target.object` replaced and WirePlumber still reads: a node's
    /// `node.name`, or its registry id.
    Node(String),
}

impl ExplicitTarget {
    /// The target a stream's properties name, in WirePlumber's order: `target.object` over
    /// `node.target` when both are there (`linking/find-defined-target.lua`). An empty value names
    /// nothing.
    fn from_props<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> Option<Self> {
        let value = |key: &str| {
            get(key)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        value("target.object")
            .map(Self::Object)
            .or_else(|| value("node.target").map(Self::Node))
    }

    /// The node of FxSound's own it names by number, if the registry lists one: `target.object`
    /// by its `object.serial`, `node.target` by its registry id, the keys WirePlumber looks a
    /// number up by (`linking/find-defined-target.lua`). `-1` — "link me to nothing" — is no
    /// node's, and neither is a number no node of ours carries.
    fn own_node<'a>(&self, own: &'a [OwnNode]) -> Option<&'a OwnNode> {
        match self {
            Self::Object(value) => {
                let serial = value.parse::<u64>().ok()?;
                own.iter().find(|node| node.serial == Some(serial))
            }
            Self::Node(value) => {
                let id = value.parse::<u32>().ok()?;
                own.iter().find(|node| node.id == id)
            }
        }
    }

    /// Whether it names one of FxSound's nodes: by name, or by number ([`Self::own_node`]).
    fn is_fxsound(&self, own: &[OwnNode]) -> bool {
        is_fxsound_node(self.value()) || self.own_node(own).is_some()
    }

    /// Whether a recorder that names this target records what FxSound plays, as WirePlumber links
    /// it (`linking/find-defined-target.lua`, `lutils.canLink` in `lib/linking-utils.lua`), with
    /// `capture_sink` its `stream.capture.sink`:
    ///
    /// - by number, whenever the number is one of the output lane's nodes ([`plays`]), flag or no
    ///   flag. A number is looked up with no regard to direction, and `canLink` lets a recorder
    ///   link to a sink — to its monitor — as readily as to a player's output: `pw-record --target
    ///   <serial of fxsound_sink>` records FxSound, and says nothing of a monitor.
    /// - by name, only when the node is of the direction the recorder looks in
    ///   (`cutils.getTargetDirection`): a sink with the flag, a player without it. A name in the
    ///   other direction is not found, and the recorder follows its default instead — the default
    ///   source, a microphone, when it has no flag.
    fn plays_fxsound_to(&self, own: &[OwnNode], capture_sink: bool) -> bool {
        if let Some(node) = self.own_node(own) {
            return node.plays.is_some();
        }
        match plays(self.value()) {
            Some(Plays::Sink) => capture_sink,
            Some(Plays::Player) => !capture_sink,
            None => false,
        }
    }

    /// The value as the application wrote it, for the log.
    fn value(&self) -> &str {
        match self {
            Self::Object(value) | Self::Node(value) => value,
        }
    }
}

/// One application's stream in the graph: a player's or a recorder's node, with what its own
/// properties say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamNode {
    /// The registry id. Runtime only: the subject a route writes the stream's metadata target for.
    pub(crate) id: u32,
    /// `object.serial`, which the server never hands to another object: what tells this stream
    /// from a later one under the same id. Runtime only.
    pub(crate) serial: Option<u64>,
    /// A player (`Output`) or a recorder (`Input`): the lane whose device it uses.
    pub(crate) direction: DeviceDirection,
    /// `client.id`: the connection that made it, whose properties fill in what its own leave out.
    pub(crate) client: Option<u32>,
    /// What its own properties say about who it is — before its client's are added
    /// ([`AppStreams::key`]).
    pub(crate) key: AppKey,
    /// `node.dont-move = true`: it may never be moved ([`Pin::DontMove`]).
    pub(crate) dont_move: bool,
    /// `stream.capture.sink = true` on a recorder: it records a sink's monitor, not a microphone.
    /// WirePlumber reads the flag on recorders only (`cutils.getTargetDirection`), and so does this.
    /// A recorder without it can record what FxSound plays all the same, by naming one of the
    /// output lane's nodes as its target ([`ExplicitTarget::plays_fxsound_to`]);
    /// [`AppStreams::records_a_sink`] asks both.
    pub(crate) monitor: bool,
    /// The target its own properties name, if they name one.
    pub(crate) target: Option<ExplicitTarget>,
    /// Its info has been read — every property, not only the registry global's handful — or
    /// cannot be, and the global is all there will ever be.
    pub(crate) complete: bool,
}

impl StreamNode {
    /// A stream from a node's registry global, or `None` when the node is not an application's
    /// audio stream: not a player's or a recorder's media class ([`stream_direction`]), or one of
    /// FxSound's own nodes ([`is_fxsound_node`]).
    #[must_use]
    pub(crate) fn from_props<'a>(id: u32, get: &impl Fn(&str) -> Option<&'a str>) -> Option<Self> {
        let direction = stream_direction(get("media.class")?)?;
        if is_fxsound_node(get("node.name").unwrap_or_default()) {
            return None;
        }
        let mut stream = Self {
            id,
            serial: None,
            direction,
            client: None,
            key: AppKey::default(),
            dont_move: false,
            monitor: false,
            target: None,
            complete: false,
        };
        stream.read(get);
        Some(stream)
    }

    /// Take what the node's info says — every property — and mark the stream complete. Returns
    /// whether anything about it changed.
    pub(crate) fn learn<'a>(&mut self, get: &impl Fn(&str) -> Option<&'a str>) -> bool {
        let before = self.clone();
        self.read(get);
        self.complete = true;
        *self != before
    }

    fn read<'a>(&mut self, get: &impl Fn(&str) -> Option<&'a str>) {
        self.serial = get("object.serial").and_then(|serial| serial.parse().ok());
        self.client = get("client.id").and_then(|id| id.parse().ok());
        self.key = app_key(get);
        self.dont_move = parse_bool(get("node.dont-move"));
        self.monitor =
            self.direction == DeviceDirection::Input && parse_bool(get("stream.capture.sink"));
        self.target = ExplicitTarget::from_props(get);
    }
}

/// Why a listed stream may not be moved onto a route (module docs, "What an application stream
/// is").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pin {
    /// `node.dont-move = true`: the application asked never to be moved, and WirePlumber would
    /// not follow a metadata target for it anyway.
    DontMove,
    /// It records what a sink plays, not a microphone ([`AppStreams::records_a_sink`]) — what
    /// FxSound plays, or it would not be listed. Behind the microphone it would record the
    /// microphone.
    Monitor,
    /// Its own properties name a target that is not one of FxSound's nodes — `-1`, "link me to
    /// nothing", included. The application chose where it goes.
    Target,
}

impl Pin {
    /// What the log says.
    #[must_use]
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::DontMove => "it says node.dont-move",
            Self::Monitor => "it records what a sink plays",
            Self::Target => "it names a target of its own",
        }
    }
}

/// What [`AppStreams::remove`] found under an id that left the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tracked {
    /// An application's stream, whose node probe goes with it.
    Stream,
    /// A client, whose probe goes with it.
    Client,
    /// One of FxSound's own nodes, kept only to resolve targets by number; whatever else the
    /// registry knows it as — the echo canceller's source — is still to be told.
    Own,
}

/// A client — a connection to the server — as far as it says who it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Client {
    key: AppKey,
    /// Its info has been read, or cannot be.
    complete: bool,
}

/// One of FxSound's own nodes, as the registry lists it: what a stream naming its target by
/// number is compared with, and what it then turns out to have named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnNode {
    id: u32,
    serial: Option<u64>,
    /// Whether it carries what FxSound plays, from its `node.name` ([`plays`]): a recorder that
    /// names it by number records FxSound rather than a microphone.
    plays: Option<Plays>,
}

/// Every application stream in the graph, both directions, with what their clients say, and what
/// the app was last told about them.
///
/// Belongs to the session: emptied when it closes, like the probes that feed it, so a stream on a
/// dead server is never reported as running.
#[derive(Debug, Default)]
pub(crate) struct AppStreams {
    streams: Vec<StreamNode>,
    clients: HashMap<u32, Client>,
    own: Vec<OwnNode>,
    /// FxSound's sink is the session's default sink (`default.audio.sink`), so a recorder of a
    /// sink's monitor that names no target of its own records FxSound.
    fxsound_default: bool,
    /// Something changed that the next report may differ by.
    changed: bool,
    /// What the app was last told: nothing, until something was.
    reported: Vec<AppStream>,
}

impl AppStreams {
    /// An application's stream appeared in the registry. One under an id this already holds
    /// replaces what was there: the server has handed the id on.
    pub(crate) fn stream_appeared(&mut self, stream: StreamNode) {
        self.remove(stream.id);
        self.streams.push(stream);
        self.changed = true;
    }

    /// A stream's info arrived, with every property ([`StreamNode::learn`]). Returns whether that
    /// completed it: the moment it becomes an application the app can be told about.
    pub(crate) fn stream_info<'a>(
        &mut self,
        id: u32,
        get: &impl Fn(&str) -> Option<&'a str>,
    ) -> bool {
        let Some(stream) = self.streams.iter_mut().find(|stream| stream.id == id) else {
            return false;
        };
        let completes = !stream.complete;
        if stream.learn(get) {
            self.changed = true;
        }
        completes
    }

    /// A stream whose node could not be bound: its registry global is all there will be, and it
    /// is reported as that rather than never.
    pub(crate) fn stream_complete_as_is(&mut self, id: u32) {
        if let Some(stream) = self.streams.iter_mut().find(|stream| stream.id == id) {
            stream.complete = true;
            self.changed = true;
        }
    }

    /// A client appeared in the registry, saying what its global says about it — a name, at most.
    pub(crate) fn client_appeared(&mut self, id: u32, key: AppKey) {
        self.remove(id);
        self.clients.insert(
            id,
            Client {
                key,
                complete: false,
            },
        );
        self.changed = true;
    }

    /// A client's info arrived, with every property.
    pub(crate) fn client_info<'a>(&mut self, id: u32, get: &impl Fn(&str) -> Option<&'a str>) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        let key = app_key(get);
        if client.key != key || !client.complete {
            client.key = key;
            client.complete = true;
            self.changed = true;
        }
    }

    /// A client that could not be bound: its streams wait for it no longer.
    pub(crate) fn client_complete_as_is(&mut self, id: u32) {
        if let Some(client) = self.clients.get_mut(&id) {
            client.complete = true;
            self.changed = true;
        }
    }

    /// One of FxSound's own nodes appeared, called `node_name`, with its `object.serial`: a player
    /// that names it by number as its target is FxSound's to move, and so is a recorder — unless
    /// it is a node of the output lane's, whose recorder records FxSound, not a microphone.
    pub(crate) fn own_node_appeared(&mut self, id: u32, serial: Option<u64>, node_name: &str) {
        self.remove(id);
        self.own.push(OwnNode {
            id,
            serial,
            plays: plays(node_name),
        });
        self.changed = true;
    }

    /// Something left the registry. Returns what it was here, if it was anything.
    pub(crate) fn remove(&mut self, id: u32) -> Option<Tracked> {
        if let Some(index) = self.streams.iter().position(|stream| stream.id == id) {
            self.streams.remove(index);
            self.changed = true;
            return Some(Tracked::Stream);
        }
        if self.clients.remove(&id).is_some() {
            self.changed = true;
            return Some(Tracked::Client);
        }
        if let Some(index) = self.own.iter().position(|node| node.id == id) {
            self.own.remove(index);
            self.changed = true;
            return Some(Tracked::Own);
        }
        None
    }

    /// The session is gone, and every stream with it. The next report says so, if the app was
    /// told of any.
    pub(crate) fn clear(&mut self) {
        self.streams.clear();
        self.clients.clear();
        self.own.clear();
        self.fxsound_default = false;
        self.changed = true;
    }

    /// The session's default sink is now `node_name` (`default.audio.sink`, the one in effect).
    /// Whether it is FxSound's decides what a recorder of the default sink's monitor records.
    pub(crate) fn default_sink_is(&mut self, node_name: Option<&str>) {
        let fxsound = node_name.is_some_and(is_fxsound_node);
        if fxsound != self.fxsound_default {
            self.fxsound_default = fxsound;
            self.changed = true;
        }
    }

    /// Who a stream belongs to: its own identifiers, with its client's filling in (module docs).
    #[must_use]
    pub(crate) fn key(&self, stream: &StreamNode) -> AppKey {
        merged(
            &stream.key,
            self.client_of(stream).map(|client| &client.key),
        )
    }

    /// Why a stream may not be moved onto a route, or `None` when it may.
    #[must_use]
    pub(crate) fn pin(&self, stream: &StreamNode) -> Option<Pin> {
        if stream.dont_move {
            Some(Pin::DontMove)
        } else if self.records_a_sink(stream) {
            Some(Pin::Monitor)
        } else if stream
            .target
            .as_ref()
            .is_some_and(|target| !target.is_fxsound(&self.own))
        {
            Some(Pin::Target)
        } else {
            None
        }
    }

    /// The stream under `id`, if it is an application's.
    #[must_use]
    pub(crate) fn stream(&self, id: u32) -> Option<&StreamNode> {
        self.streams.iter().find(|stream| stream.id == id)
    }

    /// One line for the log about the stream under `id`: who it is, which way it goes, and why it
    /// may not be moved if it may not.
    #[must_use]
    pub(crate) fn describe(&self, id: u32) -> Option<String> {
        let stream = self.stream(id)?;
        let key = self.key(stream);
        let serial = stream
            .serial
            .map_or_else(|| "?".to_owned(), |serial| serial.to_string());
        let verb = match stream.direction {
            DeviceDirection::Output => "plays",
            DeviceDirection::Input => "records",
        };
        let mut line = format!(
            "application stream {id} (serial {serial}): {:?} [binary {:?}, flatpak {:?}] {verb}",
            key.display(),
            key.binary,
            key.flatpak,
        );
        if let Some(target) = &stream.target {
            line.push_str(&format!(", target {:?}", target.value()));
        }
        if self.records_a_sink(stream) && !self.records_fxsound(stream) {
            line.push_str("; a recorder of another sink's monitor, not listed");
        } else if let Some(pin) = self.pin(stream) {
            line.push_str(&format!("; never moved: {}", pin.reason()));
        }
        Some(line)
    }

    /// Every application stream to tell the app about, outputs first, each direction by id:
    /// complete, with its client's info in, not a recorder of what a sink plays unless it records
    /// FxSound ([`Self::records_a_sink`]), and with at least one identifier — a stream that says
    /// nothing about who it is can be neither named in the list nor matched by a rule.
    #[must_use]
    pub(crate) fn report(&self) -> Vec<AppStream> {
        let mut report: Vec<AppStream> = self
            .streams
            .iter()
            .filter(|stream| self.reportable(stream))
            .filter_map(|stream| {
                let app = self.key(stream);
                (!app.is_empty()).then_some(AppStream {
                    id: stream.id,
                    direction: stream.direction,
                    app,
                    route: None,
                })
            })
            .collect();
        report.sort_by_key(|stream| (stream.direction == DeviceDirection::Input, stream.id));
        report
    }

    /// What to tell the app now: the whole list, when something changed since the last call and
    /// the list is not what it was last told. Once per supervisor tick.
    pub(crate) fn news(&mut self) -> Option<Vec<AppStream>> {
        if !std::mem::take(&mut self.changed) {
            return None;
        }
        let report = self.report();
        if report == self.reported {
            return None;
        }
        self.reported.clone_from(&report);
        Some(report)
    }

    fn client_of(&self, stream: &StreamNode) -> Option<&Client> {
        stream.client.and_then(|id| self.clients.get(&id))
    }

    /// Whether a stream is a recorder of what a sink plays rather than of a microphone (module
    /// docs, "What an application stream is"): it says `stream.capture.sink`, or it names as its
    /// target a node of FxSound's output lane that WirePlumber links a recorder to without the
    /// flag ([`ExplicitTarget::plays_fxsound_to`]). Never a player.
    fn records_a_sink(&self, stream: &StreamNode) -> bool {
        stream.monitor || self.plays_fxsound_to(stream)
    }

    /// Whether a recorder names a target of FxSound's output lane that WirePlumber links it to
    /// ([`ExplicitTarget::plays_fxsound_to`]).
    fn plays_fxsound_to(&self, stream: &StreamNode) -> bool {
        stream.direction == DeviceDirection::Input
            && stream
                .target
                .as_ref()
                .is_some_and(|target| target.plays_fxsound_to(&self.own, stream.monitor))
    }

    /// Whether a recorder of what a sink plays ([`Self::records_a_sink`]) records FxSound: it
    /// names a node of the output lane WirePlumber links it to, or — with `stream.capture.sink` —
    /// it names one of FxSound's nodes as its target, or names none and follows the default sink
    /// while that is FxSound's.
    fn records_fxsound(&self, stream: &StreamNode) -> bool {
        self.plays_fxsound_to(stream)
            || stream
                .target
                .as_ref()
                .map_or(self.fxsound_default, |target| target.is_fxsound(&self.own))
    }

    /// Whether a stream is one to report yet (see [`Self::report`]). A client the registry never
    /// announced, or one whose info cannot be read, is not waited for.
    fn reportable(&self, stream: &StreamNode) -> bool {
        stream.complete
            && self.client_of(stream).is_none_or(|client| client.complete)
            && (!self.records_a_sink(stream) || self.records_fxsound(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OUR_NODE_NAMES, OUTPUT_NODE_NAME, SINK_NODE_NAME};

    /// A property lookup over a fixed list, the shape the registry and info events hand over.
    fn props<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> {
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
        }
    }

    /// A stream from its registry global and then its info, both made of `pairs`: what the main
    /// loop builds for a stream it has bound.
    fn stream(id: u32, pairs: &[(&str, &str)]) -> StreamNode {
        let get = props(pairs);
        let mut stream = StreamNode::from_props(id, &get).expect("an application stream");
        stream.learn(&get);
        stream
    }

    fn player(id: u32, name: &str) -> StreamNode {
        stream(
            id,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", name),
            ],
        )
    }

    fn recorder(id: u32, name: &str) -> StreamNode {
        stream(
            id,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("application.name", name),
            ],
        )
    }

    fn key(binary: &str, name: &str, flatpak: &str) -> AppKey {
        AppKey {
            binary: binary.to_owned(),
            name: name.to_owned(),
            flatpak: flatpak.to_owned(),
        }
    }

    /// What `report` lists, as `(id, direction, display name)`.
    fn listed(streams: &AppStreams) -> Vec<(u32, DeviceDirection, String)> {
        streams
            .report()
            .into_iter()
            .map(|stream| (stream.id, stream.direction, stream.app.display().to_owned()))
            .collect()
    }

    #[test]
    fn a_player_is_the_output_lanes_application_and_a_recorder_the_input_lanes() {
        assert_eq!(
            stream_direction("Stream/Output/Audio"),
            Some(DeviceDirection::Output)
        );
        assert_eq!(
            stream_direction("Stream/Input/Audio"),
            Some(DeviceDirection::Input)
        );
        assert_eq!(player(80, "mpv").direction, DeviceDirection::Output);
        assert_eq!(recorder(81, "OBS").direction, DeviceDirection::Input);
    }

    #[test]
    fn devices_internal_streams_and_video_are_not_application_streams() {
        for class in [
            "Audio/Sink",
            "Audio/Source",
            "Audio/Source/Virtual",
            "Audio/Duplex",
            "Stream/Input/Audio/Internal",
            "Stream/Output/Audio/Internal",
            "Stream/Output/Video",
            "Stream/Input/Video",
            "Video/Source",
            "stream/output/audio",
            "",
        ] {
            let pairs = [("media.class", class), ("application.name", "Something")];
            assert_eq!(
                StreamNode::from_props(9, &props(&pairs)),
                None,
                "{class:?} is no application's audio stream"
            );
        }
        // And a node that does not say what it is says nothing.
        assert_eq!(
            StreamNode::from_props(9, &props(&[("application.name", "Something")])),
            None
        );
    }

    #[test]
    fn fxsounds_own_streams_are_never_application_streams() {
        let mut names: Vec<&str> = OUR_NODE_NAMES.to_vec();
        names.extend([
            "fxsound_route_o1_play",
            "fxsound_route_i2_capture",
            "fxsound_aec_reference",
        ]);
        for name in names {
            for class in [PLAYBACK_MEDIA_CLASS, RECORDING_MEDIA_CLASS] {
                let pairs = [
                    ("media.class", class),
                    ("node.name", name),
                    ("application.name", "FxSound"),
                ];
                assert_eq!(
                    StreamNode::from_props(9, &props(&pairs)),
                    None,
                    "{name} as {class} is FxSound's own"
                );
            }
        }
        // Somebody else's stream is an application's, whatever the application calls itself.
        let pairs = [
            ("media.class", PLAYBACK_MEDIA_CLASS),
            ("node.name", "pw-cat"),
            ("application.name", "FxSound"),
        ];
        assert!(StreamNode::from_props(9, &props(&pairs)).is_some());
    }

    #[test]
    fn a_streams_key_is_read_from_its_three_properties_and_trimmed() {
        let game = stream(
            87,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.process.binary", " bf6.exe "),
                ("application.name", "Battlefield 6\n"),
                ("pipewire.access.portal.app_id", ""),
                ("object.serial", "112"),
                ("client.id", "86"),
            ],
        );
        assert_eq!(game.key, key("bf6.exe", "Battlefield 6", ""));
        assert_eq!(game.serial, Some(112));
        assert_eq!(game.client, Some(86));
        assert!(game.complete);
    }

    #[test]
    fn a_stream_is_complete_only_once_its_info_has_been_read() {
        let get = props(&[
            ("media.class", PLAYBACK_MEDIA_CLASS),
            ("application.name", "Brave"),
        ]);
        let mut stream = StreamNode::from_props(5, &get).expect("a stream");
        assert!(
            !stream.complete,
            "the registry global is not the whole story"
        );
        assert!(stream.learn(&get), "completing it is a change");
        assert!(stream.complete);
        assert!(!stream.learn(&get), "the same info again changes nothing");
    }

    #[test]
    fn a_later_info_can_change_what_a_stream_says() {
        let mut stream = player(5, "Brave");
        let changed = stream.learn(&props(&[
            ("media.class", PLAYBACK_MEDIA_CLASS),
            ("application.name", "Brave"),
            ("target.object", "alsa_output.usb"),
        ]));
        assert!(changed);
        assert_eq!(
            stream.target,
            Some(ExplicitTarget::Object("alsa_output.usb".to_owned()))
        );
    }

    #[test]
    fn dont_move_and_capture_sink_are_read_the_way_wireplumber_reads_booleans() {
        for (value, meant) in [
            ("true", true),
            ("TRUE", true),
            ("True", true),
            ("1", true),
            ("false", false),
            ("0", false),
            ("yes", false),
            (" true", false),
            ("", false),
        ] {
            assert_eq!(parse_bool(Some(value)), meant, "{value:?}");
        }
        assert!(!parse_bool(None));

        let pinned = stream(
            3,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("node.dont-move", "true"),
                ("stream.capture.sink", "1"),
            ],
        );
        assert!(pinned.dont_move);
        assert!(pinned.monitor);
    }

    #[test]
    fn capture_sink_on_a_player_means_nothing() {
        let player = stream(
            3,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", "mpv"),
                ("stream.capture.sink", "true"),
            ],
        );
        assert!(!player.monitor, "only a recorder records a monitor");
        let mut streams = AppStreams::default();
        streams.stream_appeared(player);
        assert_eq!(
            listed(&streams),
            vec![(3, DeviceDirection::Output, "mpv".to_owned())]
        );
    }

    #[test]
    fn what_a_stream_does_not_say_about_itself_comes_from_its_client() {
        let mut streams = AppStreams::default();
        streams.client_appeared(40, key("", "pw-cat", ""));
        streams.client_info(
            40,
            &props(&[
                ("application.name", "pw-cat"),
                ("application.process.binary", "pw-cat"),
            ]),
        );
        let bare = stream(
            41,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", "Bare"),
                ("client.id", "40"),
            ],
        );
        assert_eq!(
            streams.key(&bare),
            key("pw-cat", "Bare", ""),
            "the stream's own name, the client's binary"
        );

        let named = stream(
            42,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.process.binary", "bf6.exe"),
                ("client.id", "40"),
            ],
        );
        assert_eq!(
            streams.key(&named),
            key("bf6.exe", "pw-cat", ""),
            "the stream's own binary, the client's name"
        );

        let orphan = stream(
            43,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", "Orphan"),
                ("client.id", "99"),
            ],
        );
        assert_eq!(
            streams.key(&orphan),
            key("", "Orphan", ""),
            "a client nobody announced adds nothing"
        );
    }

    #[test]
    fn the_flatpak_id_is_the_clients_before_the_streams() {
        let mut streams = AppStreams::default();
        streams.client_appeared(50, AppKey::default());
        streams.client_info(
            50,
            &props(&[
                ("application.name", "Discord"),
                ("pipewire.access.portal.app_id", "com.discordapp.Discord"),
            ]),
        );
        // A stream that claims to be another Flatpak does not become it.
        let claims = stream(
            51,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("application.name", "Discord"),
                ("pipewire.access.portal.app_id", "org.mozilla.firefox"),
                ("client.id", "50"),
            ],
        );
        assert_eq!(streams.key(&claims).flatpak, "com.discordapp.Discord");

        // A client with no Flatpak id leaves the stream's own, which is all there is.
        streams.client_appeared(52, AppKey::default());
        streams.client_info(52, &props(&[("application.name", "pw-cat")]));
        let own = stream(
            53,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("pipewire.access.portal.app_id", "org.test.App"),
                ("client.id", "52"),
            ],
        );
        assert_eq!(streams.key(&own).flatpak, "org.test.App");
    }

    #[test]
    fn a_flatpak_security_context_names_the_app_when_the_portal_key_is_missing() {
        let sandboxed = app_key(&props(&[
            ("pipewire.sec.engine", "org.flatpak"),
            ("pipewire.sec.app-id", "com.valvesoftware.Steam"),
        ]));
        assert_eq!(sandboxed.flatpak, "com.valvesoftware.Steam");

        let portal_first = app_key(&props(&[
            ("pipewire.access.portal.app_id", "org.mozilla.firefox"),
            ("pipewire.sec.engine", "org.flatpak"),
            ("pipewire.sec.app-id", "com.valvesoftware.Steam"),
        ]));
        assert_eq!(portal_first.flatpak, "org.mozilla.firefox");

        let other_sandbox = app_key(&props(&[
            ("pipewire.sec.engine", "org.example.Sandbox"),
            ("pipewire.sec.app-id", "some.app"),
        ]));
        assert_eq!(
            other_sandbox.flatpak, "",
            "another engine's application id is no Flatpak id"
        );
    }

    #[test]
    fn a_clients_security_context_id_beats_the_portal_id_a_stream_claims() {
        // The client is read whole, both keys, before the stream is read at all: which key the
        // id came from does not matter, whose properties it came from does.
        let mut streams = AppStreams::default();
        streams.client_appeared(54, AppKey::default());
        streams.client_info(
            54,
            &props(&[
                ("application.name", "Steam"),
                ("pipewire.sec.engine", "org.flatpak"),
                ("pipewire.sec.app-id", "com.valvesoftware.Steam"),
            ]),
        );
        let claims = stream(
            55,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", "Steam"),
                ("pipewire.access.portal.app_id", "org.mozilla.firefox"),
                ("client.id", "54"),
            ],
        );
        assert_eq!(streams.key(&claims).flatpak, "com.valvesoftware.Steam");

        // A client in another engine's sandbox has no Flatpak id, so the stream's own stands.
        streams.client_appeared(56, AppKey::default());
        streams.client_info(
            56,
            &props(&[
                ("pipewire.sec.engine", "org.example.Sandbox"),
                ("pipewire.sec.app-id", "some.app"),
            ]),
        );
        let own = stream(
            57,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("pipewire.sec.engine", "org.flatpak"),
                ("pipewire.sec.app-id", "org.test.App"),
                ("client.id", "56"),
            ],
        );
        assert_eq!(streams.key(&own).flatpak, "org.test.App");
    }

    #[test]
    fn a_stream_waits_for_its_own_info_and_its_clients_before_it_is_reported() {
        let mut streams = AppStreams::default();
        streams.client_appeared(60, key("", "pw-cat", ""));
        let global = [
            ("media.class", PLAYBACK_MEDIA_CLASS),
            ("application.name", "Test"),
            ("client.id", "60"),
        ];
        streams.stream_appeared(StreamNode::from_props(61, &props(&global)).expect("a stream"));
        assert_eq!(streams.news(), None, "nothing to say about a bare global");

        assert!(streams.stream_info(
            61,
            &props(&[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", "Test"),
                ("application.process.binary", "test.exe"),
                ("client.id", "60"),
            ]),
        ));
        assert_eq!(streams.news(), None, "its client's info is still to come");

        streams.client_info(
            60,
            &props(&[
                ("application.name", "pw-cat"),
                ("application.process.binary", "pw-cat"),
            ]),
        );
        let news = streams.news().expect("now it is an application");
        assert_eq!(news.len(), 1);
        assert_eq!(news[0].app, key("test.exe", "Test", ""));
        assert_eq!(news[0].route, None, "no route exists yet");
    }

    #[test]
    fn a_stream_or_client_that_cannot_be_bound_is_reported_as_its_global_says() {
        let mut streams = AppStreams::default();
        streams.client_appeared(70, key("", "pw-cat", ""));
        streams.client_complete_as_is(70);
        let global = [
            ("media.class", RECORDING_MEDIA_CLASS),
            ("application.name", "Rec"),
            ("client.id", "70"),
        ];
        streams.stream_appeared(StreamNode::from_props(71, &props(&global)).expect("a stream"));
        streams.stream_complete_as_is(71);
        assert_eq!(
            listed(&streams),
            vec![(71, DeviceDirection::Input, "Rec".to_owned())]
        );
    }

    #[test]
    fn the_report_lists_players_first_each_direction_by_id_and_is_sent_only_on_change() {
        let mut streams = AppStreams::default();
        streams.stream_appeared(recorder(20, "Discord"));
        streams.stream_appeared(player(31, "Brave"));
        streams.stream_appeared(player(12, "Battlefield 6"));
        let news = streams.news().expect("three applications");
        let order: Vec<(u32, DeviceDirection)> = news
            .iter()
            .map(|stream| (stream.id, stream.direction))
            .collect();
        assert_eq!(
            order,
            vec![
                (12, DeviceDirection::Output),
                (31, DeviceDirection::Output),
                (20, DeviceDirection::Input)
            ]
        );
        assert_eq!(streams.news(), None, "nothing changed since");

        // A change that leaves the list as it was says nothing either.
        streams.client_appeared(99, AppKey::default());
        assert_eq!(streams.news(), None);

        assert_eq!(streams.remove(31), Some(Tracked::Stream));
        let news = streams.news().expect("Brave has gone");
        assert_eq!(news.len(), 2);
        assert!(news.iter().all(|stream| stream.id != 31));
    }

    #[test]
    fn a_session_that_ends_takes_every_stream_with_it_and_the_app_is_told() {
        let mut streams = AppStreams::default();
        assert_eq!(streams.news(), None, "an empty graph is no news");
        streams.clear();
        assert_eq!(streams.news(), None, "and nor is an empty graph going");

        streams.stream_appeared(player(12, "mpv"));
        assert!(streams.news().is_some());
        streams.clear();
        assert_eq!(streams.news(), Some(Vec::new()), "the list is empty now");
        assert_eq!(streams.news(), None);
    }

    #[test]
    fn a_stream_under_an_id_the_server_handed_on_replaces_the_old_one() {
        let mut streams = AppStreams::default();
        streams.stream_appeared(player(12, "mpv"));
        streams.stream_appeared(recorder(12, "OBS"));
        assert_eq!(
            listed(&streams),
            vec![(12, DeviceDirection::Input, "OBS".to_owned())]
        );
        // And a client or a node of ours under that id takes it from the stream too.
        streams.client_appeared(12, AppKey::default());
        assert_eq!(streams.stream(12), None);
        assert_eq!(listed(&streams), Vec::new());
    }

    #[test]
    fn what_left_the_registry_is_named_for_the_probes_that_go_with_it() {
        let mut streams = AppStreams::default();
        streams.stream_appeared(player(1, "mpv"));
        streams.client_appeared(2, AppKey::default());
        streams.own_node_appeared(3, Some(300), OUTPUT_NODE_NAME);
        assert_eq!(streams.remove(1), Some(Tracked::Stream));
        assert_eq!(streams.remove(2), Some(Tracked::Client));
        assert_eq!(streams.remove(3), Some(Tracked::Own));
        assert_eq!(streams.remove(4), None, "a device, a card, anything else");
        assert_eq!(streams.remove(1), None, "and nothing twice");
    }

    #[test]
    fn a_stream_with_no_identifier_at_all_is_not_listed() {
        let mut streams = AppStreams::default();
        streams.stream_appeared(stream(
            8,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", "  "),
            ],
        ));
        assert_eq!(listed(&streams), Vec::new());
    }

    #[test]
    fn a_stream_that_says_dont_move_is_listed_but_pinned() {
        let mut streams = AppStreams::default();
        let kiosk = stream(
            9,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", "Kiosk"),
                ("node.dont-move", "true"),
            ],
        );
        assert_eq!(streams.pin(&kiosk), Some(Pin::DontMove));
        streams.stream_appeared(kiosk);
        assert_eq!(
            listed(&streams),
            vec![(9, DeviceDirection::Output, "Kiosk".to_owned())]
        );
        assert!(
            streams
                .describe(9)
                .is_some_and(|line| line.contains("never moved: it says node.dont-move"))
        );
    }

    #[test]
    fn a_stream_is_pinned_by_a_target_of_its_own_that_is_not_fxsound() {
        let streams = AppStreams::default();
        for (key, value) in [
            ("target.object", "alsa_output.usb-headset"),
            ("target.object", "57"),
            ("target.object", "-1"),
            ("node.target", "alsa_output.usb-headset"),
            ("node.target", "57"),
        ] {
            // A recorder too: whether 57 is a microphone, or a sink whose monitor it records, it
            // is somebody else's, and the recorder stays where it asked to be.
            for class in [PLAYBACK_MEDIA_CLASS, RECORDING_MEDIA_CLASS] {
                let stream = stream(
                    10,
                    &[
                        ("media.class", class),
                        ("application.name", "Brave"),
                        (key, value),
                    ],
                );
                assert_eq!(
                    streams.pin(&stream),
                    Some(Pin::Target),
                    "{key} = {value} is somewhere else than FxSound, for {class}"
                );
            }
        }
    }

    #[test]
    fn a_player_naming_fxsound_as_its_target_is_left_movable() {
        let mut streams = AppStreams::default();
        // FxSound's sink as the registry lists it: id 70, serial 700.
        streams.own_node_appeared(70, Some(700), SINK_NODE_NAME);
        for (key, value) in [
            ("target.object", SINK_NODE_NAME),
            ("target.object", "fxsound_route_o2"),
            ("target.object", "700"),
            ("node.target", SINK_NODE_NAME),
            ("node.target", "70"),
        ] {
            let stream = stream(
                11,
                &[
                    ("media.class", PLAYBACK_MEDIA_CLASS),
                    ("application.name", "Brave"),
                    (key, value),
                ],
            );
            assert_eq!(streams.pin(&stream), None, "{key} = {value} is FxSound");
        }
        // The serial and the id are not interchangeable: `target.object` = 70 is a serial.
        let by_id_as_serial = stream(
            11,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("target.object", "70"),
            ],
        );
        assert_eq!(streams.pin(&by_id_as_serial), Some(Pin::Target));

        // Once FxSound's node is gone, a number that named it names nothing of ours.
        streams.remove(70);
        let by_serial = stream(
            11,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("target.object", "700"),
            ],
        );
        assert_eq!(streams.pin(&by_serial), Some(Pin::Target));
    }

    #[test]
    fn target_object_is_read_before_node_target_and_an_empty_one_names_nothing() {
        let both = stream(
            12,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("target.object", SINK_NODE_NAME),
                ("node.target", "alsa_output.usb"),
            ],
        );
        assert_eq!(
            both.target,
            Some(ExplicitTarget::Object(SINK_NODE_NAME.to_owned()))
        );
        assert_eq!(AppStreams::default().pin(&both), None);

        let empty = stream(
            12,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("target.object", " "),
            ],
        );
        assert_eq!(empty.target, None);
        assert_eq!(AppStreams::default().pin(&empty), None);
    }

    #[test]
    fn dont_move_is_the_first_reason_and_a_monitor_the_second() {
        let mut streams = AppStreams::default();
        streams.default_sink_is(Some(SINK_NODE_NAME));
        let both = stream(
            13,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("node.dont-move", "true"),
                ("stream.capture.sink", "true"),
                ("target.object", "alsa_output.usb"),
            ],
        );
        assert_eq!(streams.pin(&both), Some(Pin::DontMove));
        let monitor_elsewhere = stream(
            14,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("stream.capture.sink", "true"),
                ("target.object", "alsa_output.usb"),
            ],
        );
        assert_eq!(streams.pin(&monitor_elsewhere), Some(Pin::Monitor));
    }

    #[test]
    fn a_recorder_of_another_sinks_monitor_is_not_listed() {
        let mut streams = AppStreams::default();
        streams.stream_appeared(stream(
            15,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("application.name", "Visualiser"),
                ("stream.capture.sink", "true"),
                (
                    "target.object",
                    "alsa_output.pci-0000_00_1f.3.analog-stereo",
                ),
            ],
        ));
        assert_eq!(listed(&streams), Vec::new());
        assert!(
            streams
                .describe(15)
                .is_some_and(|line| line.contains("not listed"))
        );
    }

    #[test]
    fn a_recorder_of_fxsounds_monitor_is_listed_and_pinned() {
        let mut streams = AppStreams::default();
        streams.own_node_appeared(70, Some(700), SINK_NODE_NAME);
        for (id, target) in [(16, SINK_NODE_NAME), (17, "700")] {
            streams.stream_appeared(stream(
                id,
                &[
                    ("media.class", RECORDING_MEDIA_CLASS),
                    ("application.name", "Screen Recorder"),
                    ("stream.capture.sink", "true"),
                    ("target.object", target),
                ],
            ));
            let recorder = streams.stream(id).expect("tracked");
            assert_eq!(streams.pin(recorder), Some(Pin::Monitor));
        }
        assert_eq!(
            listed(&streams),
            vec![
                (16, DeviceDirection::Input, "Screen Recorder".to_owned()),
                (17, DeviceDirection::Input, "Screen Recorder".to_owned()),
            ]
        );
    }

    /// FxSound's output lane as the registry lists it — its pair, and a playback route's — and
    /// the input lane's source and a recording route's source beside them, each `(id, serial,
    /// node.name)`.
    const OWN: [(u32, u64, &str); 6] = [
        (70, 700, SINK_NODE_NAME),
        (71, 710, OUTPUT_NODE_NAME),
        (72, 720, "fxsound_route_o2"),
        (73, 730, "fxsound_route_o2_play"),
        (74, 740, crate::SOURCE_NODE_NAME),
        (75, 750, "fxsound_route_i1"),
    ];

    fn with_own_nodes() -> AppStreams {
        let mut streams = AppStreams::default();
        for (id, serial, name) in OWN {
            streams.own_node_appeared(id, Some(serial), name);
        }
        streams
    }

    #[test]
    fn a_recorder_naming_fxsounds_sink_by_number_records_fxsound_without_the_flag() {
        // `pw-record --target <serial of fxsound_sink>`: WirePlumber looks the number up with no
        // regard to direction, and links the recorder to the sink's monitor. No
        // `stream.capture.sink` is needed, and none is set.
        let mut streams = with_own_nodes();
        for (id, key, value) in [
            (20, "target.object", "700"),
            (21, "node.target", "70"),
            (22, "target.object", "720"),
            (23, "node.target", "72"),
        ] {
            streams.stream_appeared(stream(
                id,
                &[
                    ("media.class", RECORDING_MEDIA_CLASS),
                    ("application.name", "pw-record"),
                    (key, value),
                ],
            ));
            let recorder = streams.stream(id).expect("tracked");
            assert!(!recorder.monitor, "no flag");
            assert_eq!(
                streams.pin(recorder),
                Some(Pin::Monitor),
                "{key} = {value} records what FxSound plays: never onto a microphone's route"
            );
            assert!(
                streams
                    .describe(id)
                    .is_some_and(|line| line.contains("never moved: it records what a sink plays"))
            );
        }
        assert_eq!(
            listed(&streams)
                .into_iter()
                .map(|(id, direction, _)| (id, direction))
                .collect::<Vec<_>>(),
            vec![
                (20, DeviceDirection::Input),
                (21, DeviceDirection::Input),
                (22, DeviceDirection::Input),
                (23, DeviceDirection::Input),
            ],
            "listed, because they hear FxSound"
        );
    }

    #[test]
    fn a_recorder_naming_fxsounds_playback_stream_records_fxsound_by_name_or_number() {
        // A recorder looks for a node that plays when it has no flag, and FxSound's playback
        // streams play: WirePlumber finds them by name as well as by number, and links the
        // recorder to what they play.
        let streams = with_own_nodes();
        for (key, value) in [
            ("target.object", OUTPUT_NODE_NAME),
            ("target.object", "fxsound_route_o2_play"),
            ("node.target", OUTPUT_NODE_NAME),
            ("target.object", "710"),
            ("node.target", "73"),
        ] {
            let recorder = stream(
                24,
                &[
                    ("media.class", RECORDING_MEDIA_CLASS),
                    ("application.name", "pw-record"),
                    (key, value),
                ],
            );
            assert_eq!(
                streams.pin(&recorder),
                Some(Pin::Monitor),
                "a recorder with {key} = {value} records FxSound"
            );
            // A player naming the same node hears nothing of it: two players do not link, and a
            // player that names FxSound is FxSound's to move.
            let player = stream(
                25,
                &[
                    ("media.class", PLAYBACK_MEDIA_CLASS),
                    ("application.name", "mpv"),
                    (key, value),
                ],
            );
            assert_eq!(streams.pin(&player), None, "a player with {key} = {value}");
        }
    }

    #[test]
    fn a_name_is_found_only_in_the_direction_the_recorder_looks() {
        let streams = with_own_nodes();
        // Without the flag a recorder looks for something that plays, so a sink named by name is
        // not found, and it records the default source: a microphone, FxSound's to move.
        for (key, value) in [
            ("target.object", SINK_NODE_NAME),
            ("node.target", "fxsound_route_o2"),
        ] {
            let recorder = stream(
                26,
                &[
                    ("media.class", RECORDING_MEDIA_CLASS),
                    ("application.name", "pw-record"),
                    (key, value),
                ],
            );
            assert_eq!(streams.pin(&recorder), None, "{key} = {value}");
        }
        // With the flag it looks for a sink, so the same name is its monitor.
        let flagged = stream(
            27,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("stream.capture.sink", "true"),
                ("target.object", "fxsound_route_o2"),
            ],
        );
        assert_eq!(streams.pin(&flagged), Some(Pin::Monitor));
    }

    #[test]
    fn a_recorder_naming_the_input_lanes_nodes_records_a_microphone_and_may_be_moved() {
        // FxSound's source and a recording route's carry the microphone, by name or by number:
        // a recorder of them is an input-lane application like any other.
        let mut streams = with_own_nodes();
        for (id, key, value) in [
            (30, "target.object", crate::SOURCE_NODE_NAME),
            (31, "target.object", "740"),
            (32, "node.target", "75"),
            (33, "target.object", "fxsound_route_i1"),
        ] {
            streams.stream_appeared(stream(
                id,
                &[
                    ("media.class", RECORDING_MEDIA_CLASS),
                    ("application.name", "Discord"),
                    (key, value),
                ],
            ));
            let recorder = streams.stream(id).expect("tracked");
            assert_eq!(streams.pin(recorder), None, "{key} = {value}");
        }
        assert_eq!(listed(&streams).len(), 4);
    }

    #[test]
    fn a_number_names_fxsound_only_while_fxsounds_node_is_in_the_registry() {
        let mut streams = with_own_nodes();
        streams.stream_appeared(stream(
            34,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("application.name", "pw-record"),
                ("target.object", "700"),
            ],
        ));
        let pin = |streams: &AppStreams| streams.pin(streams.stream(34).expect("tracked"));
        assert_eq!(pin(&streams), Some(Pin::Monitor));

        // FxSound's sink goes. The number names nothing now, and the recorder, which asked for a
        // place of its own, stays where WirePlumber puts it.
        assert_eq!(streams.remove(70), Some(Tracked::Own));
        assert_eq!(pin(&streams), Some(Pin::Target));
        assert_eq!(listed(&streams).len(), 1);

        // FxSound's sink comes back as a new node. The server never hands a serial on, so the old
        // number still names nothing of FxSound's.
        streams.own_node_appeared(76, Some(760), SINK_NODE_NAME);
        assert_eq!(pin(&streams), Some(Pin::Target));
    }

    #[test]
    fn the_output_lanes_nodes_and_no_others_carry_what_fxsound_plays() {
        for (name, meant) in [
            (SINK_NODE_NAME, Some(Plays::Sink)),
            (OUTPUT_NODE_NAME, Some(Plays::Player)),
            ("fxsound_route_o1", Some(Plays::Sink)),
            ("fxsound_route_o17", Some(Plays::Sink)),
            ("fxsound_route_o1_play", Some(Plays::Player)),
            ("fxsound_route_o17_play", Some(Plays::Player)),
            (crate::SOURCE_NODE_NAME, None),
            (crate::CAPTURE_NODE_NAME, None),
            (crate::KEEP_AWAKE_NODE_NAME, None),
            ("fxsound_route_i1", None),
            ("fxsound_route_i1_capture", None),
            (crate::AEC_CAPTURE_NODE_NAME, None),
            (crate::AEC_MONITOR_NODE_NAME, None),
            (crate::AEC_SOURCE_NODE_NAME, None),
            ("alsa_output.pci-0000_00_1f.3.analog-stereo", None),
            ("my_fxsound_route_o1", None),
            ("", None),
        ] {
            assert_eq!(plays(name), meant, "{name:?}");
        }
    }

    #[test]
    fn a_recorder_of_the_default_sinks_monitor_is_listed_exactly_while_fxsound_is_the_default() {
        let mut streams = AppStreams::default();
        streams.stream_appeared(stream(
            18,
            &[
                ("media.class", RECORDING_MEDIA_CLASS),
                ("application.name", "OBS"),
                ("stream.capture.sink", "true"),
            ],
        ));
        assert_eq!(streams.news(), None, "it records the speakers, not FxSound");

        streams.default_sink_is(Some(SINK_NODE_NAME));
        let news = streams.news().expect("now it records FxSound");
        assert_eq!(news.len(), 1);
        assert_eq!(news[0].direction, DeviceDirection::Input);

        streams.default_sink_is(Some(SINK_NODE_NAME));
        assert_eq!(streams.news(), None, "the same default again is no news");

        streams.default_sink_is(Some("alsa_output.usb"));
        assert_eq!(streams.news(), Some(Vec::new()));
        streams.default_sink_is(None);
        assert_eq!(streams.news(), None);
    }

    #[test]
    fn fxsounds_own_streams_are_left_out_with_the_media_class_each_really_has() {
        // Every stream FxSound makes, or has the canceller make, with its real class and the
        // application properties a process of ours carries: a player's or a recorder's stream to
        // the graph, and never an application's to the list.
        for (name, class) in [
            (OUTPUT_NODE_NAME, PLAYBACK_MEDIA_CLASS),
            (crate::CAPTURE_NODE_NAME, RECORDING_MEDIA_CLASS),
            (crate::KEEP_AWAKE_NODE_NAME, RECORDING_MEDIA_CLASS),
            (crate::AEC_CAPTURE_NODE_NAME, RECORDING_MEDIA_CLASS),
            (crate::AEC_MONITOR_NODE_NAME, RECORDING_MEDIA_CLASS),
            ("fxsound_route_o1_play", PLAYBACK_MEDIA_CLASS),
            ("fxsound_route_i1_capture", RECORDING_MEDIA_CLASS),
        ] {
            let pairs = [
                ("media.class", class),
                ("node.name", name),
                ("application.name", "fxsound"),
                ("application.process.binary", "fxsound"),
            ];
            assert_eq!(StreamNode::from_props(1, &props(&pairs)), None, "{name}");
        }
    }

    #[test]
    fn the_log_line_says_who_a_stream_is_and_where_it_asked_to_go() {
        let mut streams = AppStreams::default();
        streams.stream_appeared(stream(
            19,
            &[
                ("media.class", PLAYBACK_MEDIA_CLASS),
                ("application.name", "Brave"),
                ("application.process.binary", "brave"),
                ("object.serial", "191"),
                ("target.object", "alsa_output.usb"),
            ],
        ));
        let line = streams.describe(19).expect("tracked");
        for part in [
            "stream 19",
            "serial 191",
            "\"Brave\"",
            "\"brave\"",
            "plays",
            "target \"alsa_output.usb\"",
            "never moved: it names a target of its own",
        ] {
            assert!(line.contains(part), "{part:?} missing from {line:?}");
        }
        assert_eq!(streams.describe(20), None);
    }
}
