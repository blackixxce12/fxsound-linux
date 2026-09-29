//! The volume of FxSound's own virtual nodes: remembered per target, and applied after the chain
//! (`docs/0.4.0-upstream.md`, U10 and U11).
//!
//! # Whose volume
//!
//! Once `fxsound_sink` is the session default it *is* the control every desktop slider, the volume
//! keys and `wpctl` move (`docs/spec/12-audio-io.md` §19.7): they write the node's `Props`. The
//! real device's volume is never touched, by this module or anything else in the crate.
//!
//! # Per target
//!
//! FxSound (Output) is one node whatever it renders to, and WirePlumber remembers one volume per
//! node (`node/state-stream.lua`, keyed by `application.id`), so a level set for headphones was the
//! level the laptop's speakers got after an unplug — upstream's #615, a hearing-safety bug. Each
//! virtual node is therefore declared `state.restore-props = false`, which takes WirePlumber out of
//! it, and the engine keeps the memory instead, one [`TargetVolume`] per real device and direction:
//! reported to the app when the node's volume changes, handed back when the engine starts
//! (`crate::StartOptions::target_volumes`), and replayed onto a new pair ([`for_new_pair`]). A
//! target never seen before gets the lower of what a new node has and what the lane was just
//! playing at — the volume never goes *up* because a device changed.
//!
//! A real device is a node *and the port it is on*. On a card that is not UCM — a plain HDA card,
//! most desktops and many laptops — the speakers and the headphones are two ports of one sink, and
//! plugging headphones in only moves the sink's active route: no node comes or goes, and the pair
//! is kept. Keyed by the node's name alone, the headphones got the speakers' level — #615 by
//! another route. So an entry is kept per node and port ([`TargetVolume::port`], the name of the
//! card's active route for the node, `crate::routes`), and when the lane's device moves to another
//! port under a running pair the engine treats it as a change of device for the volume alone:
//! remembered for the port it leaves, looked up — or never raised — for the one it moves to,
//! written to the virtual node, and faded in from silence (`follow_port` in `crate::engine`).
//!
//! # From before 0.4.0
//!
//! Taking WirePlumber out is also what would have made the first 0.4.0 run the loudest one. 0.3.0
//! left its level in WirePlumber's memory — one entry per node, under `$XDG_STATE_HOME/wireplumber/
//! stream-properties` — which WirePlumber may no longer restore, and the app's own memory starts
//! empty. With nothing else to go on, a pair would have started at unity: 24 dB up for anyone who
//! had FxSound at −24 dB. So a lane with no history at all — no pair yet this run, nothing
//! remembered for its direction — takes that entry as the level it was playing at
//! ([`inherited`]), and the never-raise rule does the rest: the lower of it and unity.
//!
//! # After the chain
//!
//! A `pw_stream` sits behind an adapter, and the adapter's channel mixer applies the node's
//! `channelVolumes` — for a sink, on the way *in*, before `process()`. Measured on a private
//! PipeWire 1.6.8 (`graph_churn::volume`), pink noise into the sink with the slider at −20 dB:
//! the chain bypassed, 19.9 dB less behind NODE 2; the volume leveller at 4, 19.0–19.7 dB less
//! for a −20 dBFS programme, whose leveller is at its +12 dB cap either way, but only 11.7–12.5 dB
//! less for a −12 dBFS one — the leveller won back eight of the twenty decibels the user asked
//! for. On Windows the DSP sees the programme at full level and the endpoint volume acts after it.
//!
//! So both virtual nodes clamp their adapter's volume range to exactly unity
//! (`channelmix.min-volume = channelmix.max-volume = 1.0`, honoured since PipeWire 0.3.72): the
//! adapter still stores and publishes whatever a desktop writes — so every slider shows what it
//! set — but multiplies by one, and the lane's DSP applies the node's volume itself, after the
//! chain ([`LaneVolume::gains`]) — up to unity. What a volume above 100 % asks for beyond that is
//! applied in front of the chain, where the adapter applied it before 0.4.0: after the chain it
//! would have nothing behind it, and the chain's limiter could not stop it clipping
//! (`crate::lane_dsp`, "Above unity, in front of the chain"). The mute is applied by both, one on
//! each side of the chain: the adapter's `mute` is not clamped, so a muted sink hands the chain
//! silence, and the lane's gains are zero while the node is muted ([`NodeVolume::gains`]), so what
//! the chain still makes of the moment before — a reverb tail, a delay line emptying — is silenced
//! after it too.
//!
//! An adapter that does not know the two keys — PipeWire before 0.3.72 — would apply the volume
//! and so would the lane: twice. Its `Props` say which it is, since the adapter lists the keys it
//! knows in them ([`PropsUpdate::clamped`]), and a lane whose node's adapter turns out not to clamp
//! leaves the volume to it ([`LaneVolume::set_post_dsp`]).
//!
//! # Threads
//!
//! [`LaneVolume`] is a lane's, shared between the main loop — which writes it from the virtual
//! node's `param_changed`, and reads it on the supervisor's tick — and NODE 1's `process()`, which
//! reads the per-channel gains once a block. Everything in it is an atomic, and the data thread
//! only ever loads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use fxsound_core::messages::TargetVolume;
use fxsound_core::{DeviceDirection, limits};
use libspa::pod::deserialize::PodDeserializer;
use libspa::pod::{Pod, Value, ValueArray};
use libspa::utils::Id;

use crate::MAX_CHANNELS;
use crate::devices;
use crate::per_direction::PerDirection;

/// The most channels a lane's pair runs, and so the most gains the DSP applies.
pub(crate) const CHANNELS: usize = MAX_CHANNELS as usize;

/// The loudest a channel's volume is applied at: the top of what the app remembers
/// ([`limits::TARGET_VOLUME`], +12 dB), so a level that is applied is also a level that can be
/// replayed. PipeWire's own ceiling is +20 dB, which only `pactl` and hand-written `Props` reach.
/// Whatever of it lies above unity reaches the chain's input rather than its output
/// (`crate::lane_dsp`), so the limiter at the chain's end has the last word on the peaks.
const LOUDEST: f32 = *limits::TARGET_VOLUME.end();

/// The adapter keys that clamp its volume range, and the value both are declared at.
pub(crate) const MIN_VOLUME_KEY: &str = "channelmix.min-volume";
pub(crate) const MAX_VOLUME_KEY: &str = "channelmix.max-volume";
pub(crate) const UNITY: &str = "1.0";

/// The volume of one virtual node as its `Props` publish it: what a desktop wrote.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeVolume {
    /// `volume`, the node's master scalar. No mixer writes it — they write `channelVolumes` — but a
    /// `pw-cli` can, and the adapter multiplies it in, so it is folded into what is applied.
    pub(crate) volume: f32,
    /// `channelVolumes`, linear, one per channel in the node's order; empty until something sets
    /// them, which reads as unity.
    pub(crate) channel_volumes: Vec<f32>,
    /// `mute`.
    pub(crate) mute: bool,
}

impl Default for NodeVolume {
    /// What PipeWire gives a node it has just made: unity, unmuted.
    fn default() -> Self {
        Self {
            volume: 1.0,
            channel_volumes: Vec::new(),
            mute: false,
        }
    }
}

impl NodeVolume {
    /// A remembered entry, as the node's volume.
    pub(crate) fn remembered(entry: &TargetVolume) -> Self {
        Self {
            volume: 1.0,
            channel_volumes: entry.channel_volumes.clone(),
            mute: entry.mute,
        }
    }

    /// Take in what one `Props` write carried. Whether anything changed.
    ///
    /// A field the write did not carry keeps its value: `wpctl set-mute` sends only `mute`, and
    /// a slider only `channelVolumes`. A value that is not a number is no volume anyone set, and
    /// is ignored rather than clamped into one.
    pub(crate) fn apply(&mut self, update: &PropsUpdate) -> bool {
        let before = self.clone();
        if let Some(volume) = update.volume.filter(|volume| volume.is_finite()) {
            self.volume = volume;
        }
        if let Some(volumes) = &update.channel_volumes
            && !volumes.is_empty()
            && volumes.iter().all(|volume| volume.is_finite())
        {
            self.channel_volumes = volumes.iter().copied().take(CHANNELS).collect();
        }
        if let Some(mute) = update.mute {
            self.mute = mute;
        }
        *self != before
    }

    /// The volume of each of `channels` channels, with the master scalar folded in and clamped to
    /// what is applied — not counting the mute. A count that does not match the node's is spread
    /// as its average over every channel, as the adapter does (`fix_volumes` in `audioconvert.c`).
    pub(crate) fn effective(&self, channels: usize) -> Vec<f32> {
        let scalar = clamp_volume(self.volume);
        fit(&self.channel_volumes, channels)
            .into_iter()
            .map(|volume| clamp_volume(volume * scalar))
            .collect()
    }

    /// Whether `other` is the same volume as this one over `channels` channels: the same level on
    /// every channel, as [`Self::effective`] has it, and the same mute.
    ///
    /// The level is compared as it is kept, not as it is heard. Two mutes at different levels
    /// sound alike — [`Self::gains`] is silence for both — until the mute is lifted, and then they
    /// do not: a node that was left muted at 0.9 and should be muted at 0.1 plays at 0.9 the
    /// moment the mute key is pressed.
    pub(crate) fn same_as(&self, other: &Self, channels: usize) -> bool {
        self.mute == other.mute && self.effective(channels) == other.effective(channels)
    }

    /// What the lane multiplies each channel by: [`Self::effective`], or silence while muted.
    /// Channels past `channels` are left at unity; the DSP never reaches them.
    pub(crate) fn gains(&self, channels: usize) -> [f32; CHANNELS] {
        let mut gains = [1.0; CHANNELS];
        for (gain, volume) in gains.iter_mut().zip(self.effective(channels)) {
            *gain = if self.mute { 0.0 } else { volume };
        }
        gains
    }

    /// The entry the app keeps for this volume on `target`'s port `port` (`None`: a device whose
    /// card names no port).
    pub(crate) fn to_target(
        &self,
        direction: DeviceDirection,
        target: &str,
        port: Option<&str>,
        channels: usize,
    ) -> Option<TargetVolume> {
        TargetVolume {
            direction,
            target: target.to_owned(),
            port: port.unwrap_or_default().to_owned(),
            channel_volumes: self.effective(channels),
            mute: self.mute,
        }
        .sanitised()
    }
}

/// `volumes` spread over `channels` channels: as they are when the counts match, unity when there
/// are none, and their average on every channel otherwise.
fn fit(volumes: &[f32], channels: usize) -> Vec<f32> {
    let channels = channels.clamp(1, CHANNELS);
    if volumes.len() == channels {
        return volumes.to_vec();
    }
    let average = if volumes.is_empty() {
        1.0
    } else {
        volumes.iter().sum::<f32>() / volumes.len() as f32
    };
    vec![average; channels]
}

/// A volume as the lane applies it: never below silence, never above [`LOUDEST`], and a value that
/// is not a number is silence rather than a guess.
fn clamp_volume(volume: f32) -> f32 {
    if volume.is_finite() {
        volume.clamp(0.0, LOUDEST)
    } else {
        0.0
    }
}

/// The volume a new pair starts at, on a target with the lane's history behind it (U10).
///
/// * A target the user has a remembered volume for gets it — upward too: that is the level they
///   chose for that device, and upstream's 1.2.16 does the same (PR #620).
/// * A target never seen gets, channel by channel, the lower of what a node PipeWire has just made
///   carries — unity — and `last`: what the lane was playing at before, or failing that the
///   quietest level remembered for any device of this direction, or failing that the level
///   WirePlumber kept for the node before 0.4.0 ([`inherited`]). A mute carries over. The volume
///   never goes up because a device changed (upstream #606/#607, 4a74ba3).
/// * A target remembered with no level at all — an entry with no `channel_volumes`, which only a
///   hand edit writes and [`TargetVolume::sanitised`] keeps as a remembered mute — is a target
///   never seen that keeps its mute. Replayed as a level it would be the unity of an empty list.
/// * With no history at all the node keeps what it was made with, which is what WirePlumber would
///   have given a node it had never seen, too.
pub(crate) fn for_new_pair(
    remembered: Option<&TargetVolume>,
    last: Option<&NodeVolume>,
    channels: usize,
) -> NodeVolume {
    if let Some(entry) = remembered.filter(|entry| !entry.channel_volumes.is_empty()) {
        let mut volume = NodeVolume::remembered(entry);
        volume.channel_volumes = fit(&volume.channel_volumes, channels);
        return volume;
    }
    let remembered_mute = remembered.is_some_and(|entry| entry.mute);
    let current = NodeVolume::default();
    let Some(last) = last else {
        return NodeVolume {
            mute: current.mute || remembered_mute,
            ..current
        };
    };
    NodeVolume {
        volume: 1.0,
        channel_volumes: current
            .effective(channels)
            .into_iter()
            .zip(last.effective(channels))
            .map(|(now, before)| now.min(before))
            .collect(),
        mute: current.mute || last.mute || remembered_mute,
    }
}

/// The quietest level remembered for any device of `direction`, as a volume: the entry whose
/// loudest channel is lowest. Muted entries are left out — a mute is a choice about that device,
/// not a level — and so are entries of the other lane, and entries with no level at all, which
/// would otherwise rank as silence and start every device never seen silent.
pub(crate) fn quietest(entries: &[TargetVolume], direction: DeviceDirection) -> Option<NodeVolume> {
    let loudest = |entry: &TargetVolume| {
        entry
            .channel_volumes
            .iter()
            .copied()
            .fold(0.0_f32, f32::max)
    };
    entries
        .iter()
        .filter(|entry| {
            entry.direction == direction && !entry.mute && !entry.channel_volumes.is_empty()
        })
        .min_by(|a, b| loudest(a).total_cmp(&loudest(b)))
        .map(|entry| NodeVolume {
            volume: 1.0,
            channel_volumes: vec![loudest(entry)],
            mute: false,
        })
}

/// The name of WirePlumber's state file for stream volumes, and of the one group in it
/// (`State ("stream-properties")` in `node/state-stream.lua`).
const WIREPLUMBER_STATE: &str = "stream-properties";

/// The key WirePlumber saved the volume of FxSound's node of `direction` under: `formKey` in
/// `node/state-stream.lua` — the node's `media.class`, then the first of `application.id`,
/// `application.name`, `media.name` and `node.name` that it has. Both virtual nodes carry an
/// `application.id`, and did in 0.3.0.
pub(crate) fn wireplumber_key(direction: DeviceDirection) -> String {
    let media_class = match direction {
        DeviceDirection::Output => devices::SINK_MEDIA_CLASS,
        DeviceDirection::Input => devices::SOURCE_MEDIA_CLASS,
    };
    format!("{media_class}:application.id:{}", crate::APP_ID)
}

/// Where WirePlumber 0.5 keeps [`WIREPLUMBER_STATE`] for this process's user
/// ([`wireplumber_state_file_in`]).
///
/// Only 0.5's file is read. WirePlumber 0.4 kept another file in another format, and its
/// `restore-stream.lua` — as far as is known without a 0.4 here to check against — restored
/// `Stream/*` nodes only, so a virtual sink started at unity under 0.3.0 there too, and there is
/// nothing to inherit.
pub(crate) fn wireplumber_state_file() -> Option<PathBuf> {
    wireplumber_state_file_in(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Where WirePlumber 0.5 keeps [`WIREPLUMBER_STATE`], given `$XDG_STATE_HOME` and `$HOME`:
/// `wireplumber/` under the first, or under `.local/state` in the second when the first is not
/// set to a path (`lib/wp/state.c`; the XDG spec ignores a relative one). `None` with neither.
fn wireplumber_state_file_in(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    Some(
        state_dir_in(xdg_state_home, home)?
            .join("wireplumber")
            .join(WIREPLUMBER_STATE),
    )
}

/// The user's state directory, given `$XDG_STATE_HOME` and `$HOME`: the first, or `.local/state`
/// in the second when the first is not set to a path (the XDG spec ignores a relative one). `None`
/// with neither. WirePlumber's state is kept under it, and FxSound's handover journal
/// (`crate::stream_handover`).
pub(crate) fn state_dir_in(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    xdg_state_home
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| {
            home.filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("state"))
        })
}

/// What WirePlumber last saved for each of FxSound's two nodes, read from the state file at
/// `path` ([`inherited`]). Neither, when the file is not there or cannot be read.
///
/// Read once, when the engine starts and before it connects — never on the data thread.
pub(crate) fn inherited_from(path: &Path) -> PerDirection<Option<NodeVolume>> {
    match std::fs::read_to_string(path) {
        Ok(text) => PerDirection::from_fn(|direction| inherited(&text, direction)),
        Err(error) => {
            log::debug!("no WirePlumber volumes to inherit from {path:?}: {error}");
            PerDirection::default()
        }
    }
}

/// The volume WirePlumber saved for FxSound's node of `direction` in `text`, the contents of its
/// `stream-properties` file: the last level WirePlumber stored for the node while it still kept
/// one, which after an upgrade is where 0.3.0 was left (module docs, "From before 0.4.0").
///
/// `None` when the file has no entry for the node, or one that is not a level anybody set: a
/// value that is not a number, or is below zero. That is the same as an entry that is not there,
/// and a lane with nothing else to go on starts at unity, as it did before 0.4.0 on a node
/// WirePlumber had never seen.
///
/// The level is the node's as the adapter applied it: the master `volume` times each of the
/// `channelVolumes`, both linear. A `mute` comes with it, as WirePlumber would have restored it.
pub(crate) fn inherited(text: &str, direction: DeviceDirection) -> Option<NodeVolume> {
    let value = keyfile_value(text, WIREPLUMBER_STATE, &wireplumber_key(direction))?;
    let saved: serde_json::Value = serde_json::from_str(&value).ok()?;
    let saved = saved.as_object()?;
    let level = |value: &serde_json::Value| {
        value
            .as_f64()
            .map(|level| level as f32)
            .filter(|level| level.is_finite() && *level >= 0.0)
    };
    let volume = match saved.get("volume") {
        Some(volume) => level(volume)?,
        None => 1.0,
    };
    let channel_volumes = match saved.get("channelVolumes") {
        Some(volumes) => volumes
            .as_array()?
            .iter()
            .take(CHANNELS)
            .map(level)
            .collect::<Option<Vec<f32>>>()?,
        None => Vec::new(),
    };
    let mute = match saved.get("mute") {
        Some(mute) => mute.as_bool()?,
        None => false,
    };
    if saved.get("volume").is_none() && channel_volumes.is_empty() && !mute {
        // An entry that holds only a target or a channel map says nothing about the level.
        return None;
    }
    Some(NodeVolume {
        volume,
        channel_volumes,
        mute,
    })
}

/// The value of `key` in `group` of a GLib key file, as `GKeyFile` reads one: the text after the
/// first `=`, leading blanks gone, its escapes (`\s`, `\n`, `\t`, `\r`, `\\`) undone; of two
/// lines with the same key, the later. Keys are compared as written, which is how WirePlumber
/// writes FxSound's — it escapes only blanks, `=`, `[`, `]` and `\`, and ours have none.
fn keyfile_value(text: &str, group: &str, key: &str) -> Option<String> {
    let mut in_group = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            in_group = name.trim_end().strip_suffix(']') == Some(group);
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim_end() == key {
            found = Some(value.trim_start());
        }
    }
    found.map(unescape_keyfile_value)
}

/// A key-file value with `GKeyFile`'s escapes undone. An escape it does not know is kept as it
/// is.
fn unescape_keyfile_value(value: &str) -> String {
    let mut plain = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            plain.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => plain.push(' '),
            Some('n') => plain.push('\n'),
            Some('t') => plain.push('\t'),
            Some('r') => plain.push('\r'),
            Some('\\') => plain.push('\\'),
            Some(other) => {
                plain.push('\\');
                plain.push(other);
            }
            None => plain.push('\\'),
        }
    }
    plain
}

/// What one `Props` object said about the node's volume. Every field is `None` when the object
/// did not carry it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PropsUpdate {
    pub(crate) volume: Option<f32>,
    pub(crate) channel_volumes: Option<Vec<f32>>,
    pub(crate) mute: Option<bool>,
    /// Whether the adapter clamps its volume range to unity — both [`MIN_VOLUME_KEY`] and
    /// [`MAX_VOLUME_KEY`] at 1.0 — as the adapter's own `Props` list its settings in `params`.
    /// `None` for a `Props` without `params`, which is what a desktop's write looks like.
    ///
    /// Meaningful only for the node's whole `Props` (`on_own_props` in `crate::engine`). A write
    /// that carries `params` names only the settings it changes, and one that changes some other
    /// setting reads here as `Some(false)`, an adapter that does not clamp.
    pub(crate) clamped: Option<bool>,
    /// Whether the adapter ignores every volume written to it (`channelmix.lock-volumes`), as its
    /// own `Props` list its settings in `params`; `None` for a `Props` without `params`. What tells
    /// the handover a stream it cannot fade (`crate::stream_handover`).
    pub(crate) locked: Option<bool>,
    /// Whether the object carries `softVolumes`: the volumes the adapter derives and applies
    /// itself, which only its own whole `Props` list. A desktop's write carries what it sets —
    /// `channelVolumes`, `mute`, now and then a setting in `params` — and never them.
    ///
    /// PipeWire 1.0's adapter also hands its whole `Props` to the stream it wraps when it sets
    /// itself up, as if they were written to it: at unity, before the engine has written the
    /// volume the pair starts at (`take_props` in `crate::engine`, which leaves them alone).
    pub(crate) whole: bool,
}

impl PropsUpdate {
    /// Read a `Props` object: from `param_changed`, where it is what somebody wrote, or from the
    /// node's own `Props`, where it is everything. `None` for a pod that is not a `Props` object.
    ///
    /// Looked up by key rather than deserialised whole, like a route (`crate::routes`): the object
    /// carries a dozen other things, and a key a later PipeWire adds must not make it unreadable.
    pub(crate) fn from_pod(pod: &Pod) -> Option<Self> {
        let object = pod.as_object().ok()?;
        if object.type_().as_raw() != libspa::sys::SPA_TYPE_OBJECT_Props {
            return None;
        }
        let prop = |key: u32| object.find_prop(Id(key)).map(|prop| prop.value());
        Some(Self {
            volume: prop(libspa::sys::SPA_PROP_volume).and_then(|value| value.get_float().ok()),
            channel_volumes: prop(libspa::sys::SPA_PROP_channelVolumes).and_then(float_array),
            mute: prop(libspa::sys::SPA_PROP_mute).and_then(|value| value.get_bool().ok()),
            clamped: prop(libspa::sys::SPA_PROP_params).and_then(clamped),
            locked: prop(libspa::sys::SPA_PROP_params).and_then(locked),
            whole: prop(libspa::sys::SPA_PROP_softVolumes).is_some(),
        })
    }
}

/// An array of floats, as `channelVolumes` is.
fn float_array(pod: &Pod) -> Option<Vec<f32>> {
    // A pod's bytes stop at its size, and libspa's deserializer wants the padding to eight that
    // follows it in any buffer: a stereo `channelVolumes` is a 24-byte array (`crate::routes`).
    let mut padded = pod.as_bytes().to_vec();
    padded.resize(padded.len().next_multiple_of(8), 0);
    match PodDeserializer::deserialize_any_from(&padded).ok()?.1 {
        Value::ValueArray(ValueArray::Float(volumes)) => Some(volumes),
        _ => None,
    }
}

/// Whether the `params` struct of an adapter's `Props` — key, value, key, value — has both volume
/// bounds at unity. `Some(false)` for one that does not list them: an adapter too old to know them.
fn clamped(params: &Pod) -> Option<bool> {
    let fields = params.as_struct().ok()?;
    let mut fields = fields.fields();
    let (mut min, mut max) = (None, None);
    while let (Some(key), Some(value)) = (fields.next(), fields.next()) {
        let Ok(Some(key)) = key.get_string_raw() else {
            continue;
        };
        let number = value
            .get_float()
            .ok()
            .or_else(|| value.get_double().ok().map(|value| value as f32));
        match key.to_str() {
            Ok(MIN_VOLUME_KEY) => min = number,
            Ok(MAX_VOLUME_KEY) => max = number,
            _ => {}
        }
    }
    Some(min == Some(1.0) && max == Some(1.0))
}

/// Whether the `params` struct of an adapter's `Props` says `channelmix.lock-volumes = true`.
/// `Some(false)` for one that does not list it.
fn locked(params: &Pod) -> Option<bool> {
    let fields = params.as_struct().ok()?;
    let mut fields = fields.fields();
    while let (Some(key), Some(value)) = (fields.next(), fields.next()) {
        if key
            .get_string_raw()
            .ok()
            .flatten()
            .is_some_and(|key| key.to_bytes() == LOCK_VOLUMES_KEY.as_bytes())
        {
            return Some(value.get_bool().unwrap_or(false));
        }
    }
    Some(false)
}

/// The adapter setting that has it ignore every volume written to it.
const LOCK_VOLUMES_KEY: &str = "channelmix.lock-volumes";

/// `SPA_PROP_volumeRampStepSamples` and `SPA_PROP_volumeRampTime` (`spa/param/props.h`), by
/// value: `libspa-sys` generates its constants from the headers it is built against, and the
/// release is built against PipeWire 0.3.65's, which predate them ([`crate::stream_handover`]'s
/// "A stream without the ramp").
const PROP_VOLUME_RAMP_STEP_SAMPLES: u32 = 0x10013;
const PROP_VOLUME_RAMP_TIME: u32 = 0x10014;

/// A `Props` object that takes an application stream's master volume to `level` over
/// `ramp_ms` milliseconds, [`crate::stream_handover::RAMP_STEP_SAMPLES`] samples a step — or at
/// once, with `ramp_ms` 0 — and changes
/// nothing else: not `channelVolumes`, the level a desktop's slider shows (`crate::stream_handover`).
pub(crate) fn master_volume_pod(level: f32, ramp_ms: i32) -> Vec<u8> {
    use libspa::pod::{Object, Property, PropertyFlags};

    let property = |key: u32, value: Value| Property {
        key,
        flags: PropertyFlags::empty(),
        value,
    };
    let mut properties = Vec::with_capacity(3);
    if ramp_ms > 0 {
        properties.push(property(PROP_VOLUME_RAMP_TIME, Value::Int(ramp_ms)));
        properties.push(property(
            PROP_VOLUME_RAMP_STEP_SAMPLES,
            Value::Int(crate::stream_handover::RAMP_STEP_SAMPLES),
        ));
    }
    properties.push(property(libspa::sys::SPA_PROP_volume, Value::Float(level)));
    libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: libspa::sys::SPA_TYPE_OBJECT_Props,
            id: libspa::param::ParamType::Props.as_raw(),
            properties,
        }),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}

/// A `Props` object that sets a node's volume to `volume` over `channels` channels: the master
/// scalar at unity, since it is folded into the channel volumes, `channelVolumes` and `mute`.
pub(crate) fn props_pod(volume: &NodeVolume, channels: usize) -> Vec<u8> {
    use libspa::pod::{Object, Property, PropertyFlags};

    let property = |key: u32, value: Value| Property {
        key,
        flags: PropertyFlags::empty(),
        value,
    };
    libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: libspa::sys::SPA_TYPE_OBJECT_Props,
            id: libspa::param::ParamType::Props.as_raw(),
            properties: vec![
                property(libspa::sys::SPA_PROP_volume, Value::Float(1.0)),
                property(
                    libspa::sys::SPA_PROP_channelVolumes,
                    Value::ValueArray(ValueArray::Float(volume.effective(channels))),
                ),
                property(libspa::sys::SPA_PROP_mute, Value::Bool(volume.mute)),
            ],
        }),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}

/// A lane's volume, where the main loop and the lane's DSP both reach it.
///
/// The main loop writes: the virtual node's `param_changed` on every `Props` write
/// ([`Self::update`]), and the rules when a pair is built ([`Self::begin_pair`]). NODE 1's
/// `process()` reads the gains once a block ([`Self::gains`]) — loads only, so the data thread
/// never waits and never allocates. The gains of one block can straddle a write, the left channel
/// at the new volume and the right at the old: for one block, and ramped (`crate::lane_dsp`).
pub(crate) struct LaneVolume {
    /// What the DSP multiplies each channel by, as `f32` bits.
    gains: [AtomicU32; CHANNELS],
    /// Whether the DSP applies [`Self::gains`] at all: `false` once the node's adapter turns out
    /// to apply the volume itself.
    post_dsp: AtomicBool,
    /// The node's `volume`, as `f32` bits.
    volume: AtomicU32,
    /// The node's `channelVolumes`, as `f32` bits: the first `channel_count` are live.
    channel_volumes: [AtomicU32; CHANNELS],
    channel_count: AtomicUsize,
    mute: AtomicBool,
    /// The pair's channel count, which the gains are spread over.
    channels: AtomicUsize,
    /// How many writes from outside have changed the volume: what the supervisor waits to hold
    /// still before it reports ([`Debounce`]).
    changes: AtomicU64,
    /// How many times the main loop has asked the DSP to fade the running pair in again: the
    /// target's port changed under it, and the volume with it (`follow_port` in `crate::engine`).
    fades: AtomicU64,
}

impl LaneVolume {
    pub(crate) fn new() -> Self {
        let volume = Self {
            gains: std::array::from_fn(|_| AtomicU32::new(1.0_f32.to_bits())),
            post_dsp: AtomicBool::new(true),
            volume: AtomicU32::new(1.0_f32.to_bits()),
            channel_volumes: std::array::from_fn(|_| AtomicU32::new(1.0_f32.to_bits())),
            channel_count: AtomicUsize::new(0),
            mute: AtomicBool::new(false),
            channels: AtomicUsize::new(crate::MIN_CHANNELS as usize),
            changes: AtomicU64::new(0),
            fades: AtomicU64::new(0),
        };
        volume.store(&NodeVolume::default());
        volume
    }

    /// The node's volume as the main loop last recorded it.
    pub(crate) fn snapshot(&self) -> NodeVolume {
        let count = self.channel_count.load(Ordering::Relaxed).min(CHANNELS);
        NodeVolume {
            volume: f32::from_bits(self.volume.load(Ordering::Relaxed)),
            channel_volumes: self.channel_volumes[..count]
                .iter()
                .map(|bits| f32::from_bits(bits.load(Ordering::Relaxed)))
                .collect(),
            mute: self.mute.load(Ordering::Relaxed),
        }
    }

    /// The pair's channel count.
    pub(crate) fn channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed)
    }

    fn store(&self, volume: &NodeVolume) {
        self.volume
            .store(volume.volume.to_bits(), Ordering::Relaxed);
        let count = volume.channel_volumes.len().min(CHANNELS);
        for (slot, value) in self.channel_volumes.iter().zip(&volume.channel_volumes) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        self.channel_count.store(count, Ordering::Relaxed);
        self.mute.store(volume.mute, Ordering::Relaxed);
        for (slot, gain) in self.gains.iter().zip(volume.gains(self.channels())) {
            slot.store(gain.to_bits(), Ordering::Relaxed);
        }
    }

    /// A new pair of `channels` channels, starting at `volume`. Counted as a change, so the
    /// supervisor gives it a tick before it reports what the new target is at.
    pub(crate) fn begin_pair(&self, channels: usize, volume: &NodeVolume) {
        self.channels
            .store(channels.clamp(1, CHANNELS), Ordering::Relaxed);
        self.store(volume);
        self.changes.fetch_add(1, Ordering::Relaxed);
    }

    /// Replace the volume from the main loop — a remembered one that arrived after the pair was
    /// built — counted as a change like [`Self::begin_pair`].
    pub(crate) fn replace(&self, volume: &NodeVolume) {
        self.store(volume);
        self.changes.fetch_add(1, Ordering::Relaxed);
    }

    /// Take in a `Props` write on the virtual node. Whether it changed the volume; a write that
    /// only repeats it — the echo of the engine's own, above all — is not a change.
    pub(crate) fn update(&self, update: &PropsUpdate) -> bool {
        let mut volume = self.snapshot();
        if !volume.apply(update) {
            return false;
        }
        self.store(&volume);
        self.changes.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// How many changes there have been.
    pub(crate) fn changes(&self) -> u64 {
        self.changes.load(Ordering::Relaxed)
    }

    /// Ask NODE 1's DSP to fade the running pair in again from silence, at the volume as it now
    /// stands (`crate::lane_dsp`, `LaneDsp::fade_in`). Main loop.
    ///
    /// Asked after the volume it fades in to has been stored, and released, so that NODE 1, which
    /// reads the count before the gains, never starts the fade on the gains from before.
    pub(crate) fn request_fade(&self) {
        self.fades.fetch_add(1, Ordering::Release);
    }

    /// How many fades have been asked for: NODE 1 starts one when this has moved since it last
    /// looked. One atomic load, acquiring what [`Self::request_fade`] released; real-time safe.
    #[inline]
    pub(crate) fn fades(&self) -> u64 {
        self.fades.load(Ordering::Acquire)
    }

    /// Whether the lane applies the volume after its chain: as long as the node's adapter keeps
    /// its own at unity ([`PropsUpdate::clamped`]).
    pub(crate) fn set_post_dsp(&self, post_dsp: bool) {
        self.post_dsp.store(post_dsp, Ordering::Relaxed);
    }

    pub(crate) fn post_dsp(&self) -> bool {
        self.post_dsp.load(Ordering::Relaxed)
    }

    /// What NODE 1's DSP multiplies each channel by — the part up to unity after its chain, the
    /// rest in front of it (`crate::lane_dsp`): the node's volume, or unity when the adapter
    /// applies it instead. Real-time safe — nine atomic loads into an array on the stack.
    #[inline]
    pub(crate) fn gains(&self) -> [f32; CHANNELS] {
        if !self.post_dsp.load(Ordering::Relaxed) {
            return [1.0; CHANNELS];
        }
        let mut gains = [1.0; CHANNELS];
        for (gain, bits) in gains.iter_mut().zip(&self.gains) {
            *gain = f32::from_bits(bits.load(Ordering::Relaxed));
        }
        gains
    }
}

/// Waits for a lane's volume to hold still before it is reported.
///
/// A slider being dragged writes the node's `Props` dozens of times a second, and each of those is
/// a settings-file write in the app. So a change is reported once no further change has come for a
/// whole supervisor tick: 200–400 ms after the last one.
#[derive(Debug, Default)]
pub(crate) struct Debounce {
    /// The change count at the last tick.
    seen: u64,
}

impl Debounce {
    /// Called once a tick with the lane's change count: whether the volume has held still since
    /// the last tick, and so may be reported.
    pub(crate) fn settled(&mut self, changes: u64) -> bool {
        if changes == self.seen {
            return true;
        }
        self.seen = changes;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libspa::pod::serialize::PodSerializer;
    use libspa::pod::{Object, Property, PropertyFlags};

    fn pod_of(properties: Vec<Property>) -> Vec<u8> {
        pod_of_type(libspa::sys::SPA_TYPE_OBJECT_Props, properties)
    }

    fn pod_of_type(type_: u32, properties: Vec<Property>) -> Vec<u8> {
        PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(Object {
                type_,
                id: libspa::param::ParamType::Props.as_raw(),
                properties,
            }),
        )
        .expect("a pod")
        .0
        .into_inner()
    }

    fn property(key: u32, value: Value) -> Property {
        Property {
            key,
            flags: PropertyFlags::empty(),
            value,
        }
    }

    fn parse(bytes: &[u8]) -> Option<PropsUpdate> {
        PropsUpdate::from_pod(Pod::from_bytes(bytes).expect("a pod"))
    }

    fn params(pairs: Vec<Value>) -> Property {
        property(libspa::sys::SPA_PROP_params, Value::Struct(pairs))
    }

    fn entry(direction: DeviceDirection, target: &str, volumes: &[f32]) -> TargetVolume {
        TargetVolume {
            direction,
            target: target.to_owned(),
            port: String::new(),
            channel_volumes: volumes.to_vec(),
            mute: false,
        }
    }

    fn at(volumes: &[f32]) -> NodeVolume {
        NodeVolume {
            volume: 1.0,
            channel_volumes: volumes.to_vec(),
            mute: false,
        }
    }

    // ---- reading Props

    #[test]
    fn two_mutes_at_different_levels_are_not_the_same_volume() {
        let quiet = NodeVolume {
            volume: 1.0,
            channel_volumes: vec![0.1, 0.1],
            mute: true,
        };
        let loud = NodeVolume {
            channel_volumes: vec![0.9, 0.9],
            ..quiet.clone()
        };
        assert_eq!(quiet.gains(2), loud.gains(2), "both silent while muted");
        assert!(!quiet.same_as(&loud, 2), "but not once the mute is lifted");
        assert!(quiet.same_as(&quiet.clone(), 2));
        let unmuted = NodeVolume {
            mute: false,
            ..quiet.clone()
        };
        assert!(!quiet.same_as(&unmuted, 2), "the mute counts too");
        let scaled = NodeVolume {
            volume: 0.5,
            channel_volumes: vec![0.2, 0.2],
            ..quiet.clone()
        };
        assert!(
            quiet.same_as(&scaled, 2),
            "the master scalar is folded in, as the lane applies it"
        );
    }

    #[test]
    fn a_sliders_write_is_read_as_channel_volumes_alone() {
        let bytes = pod_of(vec![property(
            libspa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(vec![0.25, 0.5])),
        )]);
        assert_eq!(
            parse(&bytes),
            Some(PropsUpdate {
                channel_volumes: Some(vec![0.25, 0.5]),
                ..PropsUpdate::default()
            })
        );
    }

    #[test]
    fn a_mute_key_is_read_without_touching_the_volumes() {
        let bytes = pod_of(vec![property(
            libspa::sys::SPA_PROP_mute,
            Value::Bool(true),
        )]);
        assert_eq!(
            parse(&bytes),
            Some(PropsUpdate {
                mute: Some(true),
                ..PropsUpdate::default()
            })
        );
    }

    #[test]
    fn a_full_props_object_yields_the_scalar_the_channels_and_the_mute() {
        let bytes = pod_of(vec![
            property(libspa::sys::SPA_PROP_volume, Value::Float(0.5)),
            property(libspa::sys::SPA_PROP_mute, Value::Bool(false)),
            property(
                libspa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(vec![0.1; 6])),
            ),
            // Keys this module does not read are passed over, not refused.
            property(
                libspa::sys::SPA_PROP_softVolumes,
                Value::ValueArray(ValueArray::Float(vec![1.0; 6])),
            ),
        ]);
        let update = parse(&bytes).expect("a Props object");
        assert_eq!(update.volume, Some(0.5));
        assert_eq!(update.mute, Some(false));
        assert_eq!(update.channel_volumes, Some(vec![0.1; 6]));
        assert_eq!(update.clamped, None, "no params, no opinion on the clamp");
    }

    #[test]
    fn an_adapter_that_locks_its_volumes_says_so_in_its_params() {
        let locked = pod_of(vec![params(vec![
            Value::String(MIN_VOLUME_KEY.to_owned()),
            Value::Float(0.0),
            Value::String(LOCK_VOLUMES_KEY.to_owned()),
            Value::Bool(true),
        ])]);
        assert_eq!(parse(&locked).and_then(|update| update.locked), Some(true));
        let free = pod_of(vec![params(vec![
            Value::String(LOCK_VOLUMES_KEY.to_owned()),
            Value::Bool(false),
        ])]);
        assert_eq!(parse(&free).and_then(|update| update.locked), Some(false));
        let old = pod_of(vec![params(vec![
            Value::String(MIN_VOLUME_KEY.to_owned()),
            Value::Float(0.0),
        ])]);
        assert_eq!(
            parse(&old).and_then(|update| update.locked),
            Some(false),
            "an adapter that does not know the setting locks nothing"
        );
        let write = pod_of(vec![property(
            libspa::sys::SPA_PROP_volume,
            Value::Float(0.5),
        )]);
        assert_eq!(parse(&write).and_then(|update| update.locked), None);
    }

    #[test]
    fn a_master_volume_write_ramps_the_master_volume_and_nothing_else() {
        let bytes = master_volume_pod(0.0, 20);
        let update = parse(&bytes).expect("a Props object");
        assert_eq!(update.volume, Some(0.0));
        assert_eq!(
            update.channel_volumes, None,
            "the slider's level is not touched"
        );
        assert_eq!(update.mute, None);
        let pod = Pod::from_bytes(&bytes).expect("a pod");
        let object = pod.as_object().expect("an object");
        let int = |key: u32| {
            object
                .find_prop(Id(key))
                .and_then(|prop| prop.value().get_int().ok())
        };
        assert_eq!(int(PROP_VOLUME_RAMP_TIME), Some(20));
        assert_eq!(
            int(PROP_VOLUME_RAMP_STEP_SAMPLES),
            Some(crate::stream_handover::RAMP_STEP_SAMPLES)
        );

        let at_once = master_volume_pod(0.8, 0);
        let pod = Pod::from_bytes(&at_once).expect("a pod");
        let object = pod.as_object().expect("an object");
        assert!(object.find_prop(Id(PROP_VOLUME_RAMP_TIME)).is_none());
        assert_eq!(parse(&at_once).and_then(|update| update.volume), Some(0.8));
    }

    #[test]
    fn the_ramp_properties_are_where_the_header_lists_them() {
        // `SPA_PROP_START_Audio` is 0x10000, and the audio properties follow it one by one:
        // waveType, frequency, volume, mute, patternType, ditherType, truncate, channelVolumes,
        // volumeBase, volumeStep, channelMap, monitorMute, monitorVolumes, latencyOffsetNsec,
        // softMute, softVolumes, iec958Codecs, volumeRampSamples, volumeRampStepSamples,
        // volumeRampTime. The ones every PipeWire has anchor the count.
        assert_eq!(libspa::sys::SPA_PROP_volume, 0x10003);
        assert_eq!(libspa::sys::SPA_PROP_softVolumes, 0x10010);
        assert_eq!(
            PROP_VOLUME_RAMP_STEP_SAMPLES,
            libspa::sys::SPA_PROP_softVolumes + 3
        );
        assert_eq!(PROP_VOLUME_RAMP_TIME, libspa::sys::SPA_PROP_softVolumes + 4);
    }

    #[test]
    fn only_an_object_that_lists_the_soft_volumes_is_the_adapters_whole_props() {
        let slider = pod_of(vec![property(
            libspa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(vec![0.25, 0.25])),
        )]);
        assert!(!parse(&slider).expect("a Props object").whole);
        // A slider's write that sets one of the adapter's settings as well is still a write.
        let with_params = pod_of(vec![
            property(
                libspa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(vec![0.25, 0.25])),
            ),
            params(vec![
                Value::String("monitor.channel-volumes".to_owned()),
                Value::Bool(false),
            ]),
        ]);
        assert!(!parse(&with_params).expect("a Props object").whole);
        // What PipeWire 1.0's adapter hands the stream as it sets itself up: everything, at unity.
        let adapter = pod_of(vec![
            property(libspa::sys::SPA_PROP_volume, Value::Float(1.0)),
            property(libspa::sys::SPA_PROP_mute, Value::Bool(false)),
            property(
                libspa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(vec![1.0; 8])),
            ),
            property(libspa::sys::SPA_PROP_softMute, Value::Bool(false)),
            property(
                libspa::sys::SPA_PROP_softVolumes,
                Value::ValueArray(ValueArray::Float(vec![1.0; 8])),
            ),
            params(vec![
                Value::String(MIN_VOLUME_KEY.to_owned()),
                Value::Float(1.0),
                Value::String(MAX_VOLUME_KEY.to_owned()),
                Value::Float(1.0),
            ]),
        ]);
        let update = parse(&adapter).expect("a Props object");
        assert!(update.whole);
        assert_eq!(update.channel_volumes, Some(vec![1.0; 8]));
    }

    #[test]
    fn an_object_that_is_not_props_is_not_read_as_a_volume() {
        let bytes = pod_of_type(
            libspa::sys::SPA_TYPE_OBJECT_ParamRoute,
            vec![property(libspa::sys::SPA_PROP_mute, Value::Bool(true))],
        );
        assert_eq!(parse(&bytes), None);
        let not_an_object =
            PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Int(3))
                .expect("a pod")
                .0
                .into_inner();
        assert_eq!(parse(&not_an_object), None);
    }

    #[test]
    fn channel_volumes_of_the_wrong_type_are_ignored_rather_than_misread() {
        let bytes = pod_of(vec![property(
            libspa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Int(vec![1, 1])),
        )]);
        assert_eq!(parse(&bytes), Some(PropsUpdate::default()));
    }

    #[test]
    fn an_adapter_that_lists_both_bounds_at_unity_is_known_to_clamp() {
        let bytes = pod_of(vec![params(vec![
            Value::String("monitor.channel-volumes".to_owned()),
            Value::Bool(false),
            Value::String(MIN_VOLUME_KEY.to_owned()),
            Value::Float(1.0),
            Value::String(MAX_VOLUME_KEY.to_owned()),
            Value::Float(1.0),
        ])]);
        assert_eq!(parse(&bytes).and_then(|update| update.clamped), Some(true));
    }

    #[test]
    fn an_adapter_too_old_to_list_the_bounds_or_with_them_elsewhere_does_not_clamp() {
        let old = pod_of(vec![params(vec![
            Value::String("monitor.channel-volumes".to_owned()),
            Value::Bool(false),
        ])]);
        assert_eq!(parse(&old).and_then(|update| update.clamped), Some(false));
        let default = pod_of(vec![params(vec![
            Value::String(MIN_VOLUME_KEY.to_owned()),
            Value::Float(0.0),
            Value::String(MAX_VOLUME_KEY.to_owned()),
            Value::Float(10.0),
        ])]);
        assert_eq!(
            parse(&default).and_then(|update| update.clamped),
            Some(false)
        );
    }

    #[test]
    fn the_pod_the_engine_writes_reads_back_as_the_volume_it_was_made_from() {
        let volume = NodeVolume {
            volume: 0.5,
            channel_volumes: vec![0.4, 0.8],
            mute: true,
        };
        let update = parse(&props_pod(&volume, 2)).expect("a Props object");
        assert_eq!(
            update.volume,
            Some(1.0),
            "the scalar is folded in, not written"
        );
        assert_eq!(update.channel_volumes, Some(vec![0.2, 0.4]));
        assert_eq!(update.mute, Some(true));
        let mut replayed = NodeVolume::default();
        replayed.apply(&update);
        assert_eq!(replayed.gains(2), volume.gains(2));
        assert_eq!(replayed.effective(2), volume.effective(2));
    }

    // ---- the volume a node holds

    #[test]
    fn a_write_changes_only_what_it_carries_and_says_whether_anything_changed() {
        let mut volume = at(&[0.5, 0.5]);
        assert!(!volume.apply(&PropsUpdate::default()));
        assert!(volume.apply(&PropsUpdate {
            mute: Some(true),
            ..PropsUpdate::default()
        }));
        assert_eq!(volume.channel_volumes, [0.5, 0.5], "a mute keeps the level");
        assert!(!volume.apply(&PropsUpdate {
            mute: Some(true),
            ..PropsUpdate::default()
        }));
        assert!(volume.apply(&PropsUpdate {
            channel_volumes: Some(vec![0.3, 0.3]),
            ..PropsUpdate::default()
        }));
        assert!(volume.mute, "a level keeps the mute");
    }

    #[test]
    fn a_write_that_is_not_a_number_is_ignored_not_clamped_into_a_level() {
        let mut volume = at(&[0.5, 0.5]);
        assert!(!volume.apply(&PropsUpdate {
            volume: Some(f32::NAN),
            channel_volumes: Some(vec![0.2, f32::INFINITY]),
            ..PropsUpdate::default()
        }));
        assert_eq!(volume, at(&[0.5, 0.5]));
    }

    #[test]
    fn the_gains_fold_in_the_scalar_and_the_mute_and_stop_at_plus_twelve_decibels() {
        let volume = NodeVolume {
            volume: 0.5,
            channel_volumes: vec![0.2, 100.0],
            mute: false,
        };
        let gains = volume.gains(2);
        assert_eq!(gains[..2], [0.1, LOUDEST]);
        assert!(gains[2..].iter().all(|&gain| gain == 1.0));
        let muted = NodeVolume {
            mute: true,
            ..volume
        };
        assert_eq!(muted.gains(2)[..2], [0.0, 0.0]);
        assert_eq!(
            NodeVolume::default().gains(2)[..2],
            [1.0, 1.0],
            "a node nobody set plays at unity"
        );
    }

    #[test]
    fn volumes_for_a_different_channel_count_are_spread_as_their_average() {
        assert_eq!(at(&[0.2, 0.4]).effective(6), vec![0.3_f32; 6]);
        assert_eq!(at(&[0.2, 0.4]).effective(2), vec![0.2, 0.4]);
        assert_eq!(at(&[]).effective(3), vec![1.0; 3]);
    }

    // ---- the never-raise rule

    #[test]
    fn a_remembered_target_gets_its_own_volume_back_even_when_that_is_louder() {
        let remembered = entry(DeviceDirection::Output, "headphones", &[0.6, 0.6]);
        let volume = for_new_pair(Some(&remembered), Some(&at(&[0.2, 0.2])), 2);
        assert_eq!(volume.effective(2), vec![0.6, 0.6]);
        assert!(!volume.mute);
    }

    #[test]
    fn a_remembered_mute_comes_back_with_its_target() {
        let mut remembered = entry(DeviceDirection::Output, "speakers", &[0.6, 0.6]);
        remembered.mute = true;
        assert!(for_new_pair(Some(&remembered), None, 2).mute);
    }

    #[test]
    fn a_target_never_seen_starts_no_louder_than_the_lane_was_playing() {
        let volume = for_new_pair(None, Some(&at(&[0.25, 0.25])), 2);
        assert_eq!(volume.effective(2), vec![0.25, 0.25]);
    }

    #[test]
    fn a_target_never_seen_is_not_raised_above_a_new_nodes_unity_either() {
        let volume = for_new_pair(None, Some(&at(&[3.0, 3.0])), 2);
        assert_eq!(volume.effective(2), vec![1.0, 1.0]);
    }

    #[test]
    fn a_lane_that_was_muted_stays_muted_on_a_target_never_seen() {
        let muted = NodeVolume {
            mute: true,
            ..at(&[0.5, 0.5])
        };
        assert!(for_new_pair(None, Some(&muted), 2).mute);
    }

    #[test]
    fn a_last_volume_on_other_channels_is_carried_over_as_its_average() {
        let volume = for_new_pair(None, Some(&at(&[0.2, 0.4])), 8);
        assert_eq!(volume.effective(8), vec![0.3_f32; 8]);
    }

    #[test]
    fn with_no_history_at_all_a_new_node_keeps_the_unity_it_was_made_with() {
        assert_eq!(for_new_pair(None, None, 2), NodeVolume::default());
    }

    #[test]
    fn the_quietest_level_remembered_for_the_lanes_direction_stands_in_for_a_missing_last_volume() {
        let mut muted = entry(DeviceDirection::Output, "muted", &[0.01, 0.01]);
        muted.mute = true;
        let entries = [
            entry(DeviceDirection::Output, "speakers", &[0.9, 0.9]),
            entry(DeviceDirection::Output, "headphones", &[0.2, 0.3]),
            entry(DeviceDirection::Input, "microphone", &[0.05]),
            muted,
        ];
        let quiet = quietest(&entries, DeviceDirection::Output).expect("an output entry");
        assert_eq!(
            for_new_pair(None, Some(&quiet), 2).effective(2),
            vec![0.3, 0.3],
            "the headphones' loudest channel; not the microphone's, not the muted entry's"
        );
        assert_eq!(quietest(&entries[..1], DeviceDirection::Input), None);
    }

    // ---- an entry with no level

    #[test]
    fn a_remembered_entry_with_no_level_is_not_replayed_as_unity() {
        // `fit` of no volumes is unity: replayed as a level, a hand-edited entry with no
        // `channel_volumes` would have raised the lane from where it was to full scale.
        let bare = entry(DeviceDirection::Output, "speakers", &[]);
        let volume = for_new_pair(Some(&bare), Some(&at(&[0.2, 0.2])), 2);
        assert_eq!(volume.effective(2), vec![0.2, 0.2]);
        assert!(!volume.mute);
        assert_eq!(
            for_new_pair(Some(&bare), Some(&at(&[0.2, 0.2])), 2),
            for_new_pair(None, Some(&at(&[0.2, 0.2])), 2),
            "it is a target never seen"
        );
    }

    #[test]
    fn a_remembered_entry_with_no_level_but_a_mute_starts_muted_at_no_louder_a_level() {
        let mut bare = entry(DeviceDirection::Output, "speakers", &[]);
        bare.mute = true;
        let volume = for_new_pair(Some(&bare), Some(&at(&[0.2, 0.2])), 2);
        assert!(volume.mute, "the mute is what the entry remembers");
        assert_eq!(volume.effective(2), vec![0.2, 0.2]);
        assert_eq!(volume.gains(2)[..2], [0.0, 0.0]);
        let alone = for_new_pair(Some(&bare), None, 2);
        assert!(alone.mute);
        assert_eq!(alone.effective(2), vec![1.0, 1.0]);
    }

    #[test]
    fn an_entry_with_no_level_is_not_ranked_the_quietest_and_so_starts_nothing_silent() {
        // Its loudest channel of none folds to 0.0, which would have made it the quietest entry,
        // and every device never seen would have started silent.
        let entries = [
            entry(DeviceDirection::Output, "hand-edited", &[]),
            entry(DeviceDirection::Output, "headphones", &[0.4, 0.4]),
        ];
        let quiet = quietest(&entries, DeviceDirection::Output).expect("the headphones");
        assert_eq!(quiet.effective(2), vec![0.4, 0.4]);
        assert_eq!(
            quietest(&entries[..1], DeviceDirection::Output),
            None,
            "nothing with a level is nothing to go by"
        );
    }

    // ---- what WirePlumber kept from before 0.4.0

    /// A `stream-properties` file as WirePlumber 0.5.17 writes it, with 0.3.0's sink at about
    /// −24 dB — the line is copied from a real one.
    const STREAM_PROPERTIES: &str = "\
[stream-properties]
Input/Audio:application.name:Noctalia\\sSpectrum={\"volume\":1.000000, \"channelMap\":[\"FL\", \"FR\"], \"mute\":false, \"channelVolumes\":[1.000000, 1.000000]}
Audio/Sink:application.id:com.fxsound.FxSound={\"channelVolumes\":[0.064000, 0.064000], \"mute\":false, \"channelMap\":[\"FL\", \"FR\"], \"volume\":1.000000}
Output/Audio:application.id:com.fxsound.FxSound={\"mute\":false, \"channelVolumes\":[1.000000, 1.000000], \"volume\":1.000000, \"channelMap\":[\"FL\", \"FR\"]}
Audio/Source:application.id:com.fxsound.FxSound={\"mute\":true, \"channelVolumes\":[0.500000], \"volume\":0.500000}
";

    #[test]
    fn the_level_wireplumber_kept_for_fxsounds_sink_is_read_from_its_state_file() {
        let sink = inherited(STREAM_PROPERTIES, DeviceDirection::Output).expect("an entry");
        assert_eq!(sink.effective(2), vec![0.064, 0.064]);
        assert!(!sink.mute);
        assert_ne!(
            sink.effective(2),
            vec![1.0, 1.0],
            "the playback stream's entry, under the same application.id, is not the sink's"
        );
        let source = inherited(STREAM_PROPERTIES, DeviceDirection::Input).expect("an entry");
        assert_eq!(
            source.effective(1),
            vec![0.25],
            "the master scalar folds in, as the adapter applied it"
        );
        assert!(source.mute);
    }

    #[test]
    fn the_first_pair_after_an_upgrade_starts_at_the_level_0_3_0_left_not_at_unity() {
        let sink = inherited(STREAM_PROPERTIES, DeviceDirection::Output);
        let volume = for_new_pair(None, sink.as_ref(), 2);
        assert_eq!(volume.effective(2), vec![0.064, 0.064]);
        assert_eq!(volume.gains(2)[..2], [0.064, 0.064]);
    }

    #[test]
    fn a_level_wireplumber_kept_above_unity_is_still_not_a_raise() {
        let loud = "[stream-properties]\n\
            Audio/Sink:application.id:com.fxsound.FxSound={\"channelVolumes\":[2.5, 2.5]}\n";
        let sink = inherited(loud, DeviceDirection::Output);
        assert_eq!(
            for_new_pair(None, sink.as_ref(), 2).effective(2),
            vec![1.0, 1.0]
        );
    }

    #[test]
    fn nothing_is_inherited_from_a_file_without_a_usable_entry_for_the_node() {
        for text in [
            "",
            "[stream-properties]\n",
            // The key in another group is not WirePlumber's stream memory.
            "[default-routes]\n\
             Audio/Sink:application.id:com.fxsound.FxSound={\"channelVolumes\":[0.1, 0.1]}\n",
            // Not JSON, not an object, not numbers, below zero: no level anybody set.
            "[stream-properties]\nAudio/Sink:application.id:com.fxsound.FxSound={broken\n",
            "[stream-properties]\nAudio/Sink:application.id:com.fxsound.FxSound=[0.1]\n",
            "[stream-properties]\n\
             Audio/Sink:application.id:com.fxsound.FxSound={\"channelVolumes\":[\"loud\"]}\n",
            "[stream-properties]\n\
             Audio/Sink:application.id:com.fxsound.FxSound={\"channelVolumes\":[-0.5, 0.5]}\n",
            "[stream-properties]\n\
             Audio/Sink:application.id:com.fxsound.FxSound={\"volume\":-1.0}\n",
            // A channel map alone says nothing about the level.
            "[stream-properties]\n\
             Audio/Sink:application.id:com.fxsound.FxSound={\"channelMap\":[\"FL\", \"FR\"]}\n",
            // Another application's sink.
            "[stream-properties]\n\
             Audio/Sink:application.id:org.example.Other={\"channelVolumes\":[0.1, 0.1]}\n",
        ] {
            assert_eq!(inherited(text, DeviceDirection::Output), None, "{text}");
        }
    }

    #[test]
    fn a_key_file_is_read_as_glib_reads_it() {
        let text = "\
# a comment
[other]
Audio/Sink:application.id:com.fxsound.FxSound={\"channelVolumes\":[0.9, 0.9]}

  [stream-properties]
Audio/Sink:application.id:com.fxsound.FxSound = {\"channelVolumes\":[0.3, 0.3]}
Audio/Sink:application.id:com.fxsound.FxSound=\\s{\"channelVolumes\":[0.2,\\t0.2], \"note\":\"a\\\\b\"}
";
        assert_eq!(
            keyfile_value(
                text,
                "stream-properties",
                &wireplumber_key(DeviceDirection::Output)
            ),
            Some(" {\"channelVolumes\":[0.2,\t0.2], \"note\":\"a\\b\"}".to_owned()),
            "the later line, its escapes undone"
        );
        assert_eq!(
            inherited(text, DeviceDirection::Output).map(|volume| volume.effective(2)),
            Some(vec![0.2, 0.2])
        );
        assert_eq!(unescape_keyfile_value("a\\qb\\"), "a\\qb\\");
    }

    #[test]
    fn wireplumbers_state_file_is_under_xdg_state_home_or_else_under_home() {
        use std::ffi::OsStr;

        assert_eq!(
            wireplumber_state_file_in(Some(OsStr::new("/state")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/state/wireplumber/stream-properties"))
        );
        for unusable in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
            assert_eq!(
                wireplumber_state_file_in(unusable, Some(OsStr::new("/home/u"))),
                Some(PathBuf::from(
                    "/home/u/.local/state/wireplumber/stream-properties"
                )),
                "{unusable:?}"
            );
        }
        assert_eq!(wireplumber_state_file_in(None, None), None);
        assert_eq!(wireplumber_state_file_in(None, Some(OsStr::new(""))), None);
    }

    #[test]
    fn a_state_file_that_is_not_there_leaves_both_lanes_nothing_to_inherit() {
        let missing = inherited_from(Path::new("/nonexistent/fxsound/stream-properties"));
        assert_eq!(missing.output, None);
        assert_eq!(missing.input, None);

        let dir = fxsound_core::test_support::ScratchDir::new("volume");
        let path = dir.join(WIREPLUMBER_STATE);
        std::fs::write(&path, STREAM_PROPERTIES).expect("the state file");
        let read = inherited_from(&path);
        assert_eq!(
            read.output.map(|volume| volume.effective(2)),
            Some(vec![0.064, 0.064])
        );
        assert_eq!(read.input.map(|volume| volume.mute), Some(true));
    }

    // ---- the lane's atomics

    #[test]
    fn a_lane_volume_hands_the_dsp_the_gains_of_what_it_was_told() {
        let lane = LaneVolume::new();
        assert_eq!(lane.gains(), [1.0; CHANNELS]);
        lane.begin_pair(2, &at(&[0.5, 0.25]));
        assert_eq!(lane.gains()[..2], [0.5, 0.25]);
        assert_eq!(lane.snapshot(), at(&[0.5, 0.25]));
        assert!(lane.update(&PropsUpdate {
            mute: Some(true),
            ..PropsUpdate::default()
        }));
        assert_eq!(lane.gains()[..2], [0.0, 0.0]);
    }

    #[test]
    fn a_lane_whose_adapter_applies_the_volume_hands_the_dsp_unity() {
        let lane = LaneVolume::new();
        lane.begin_pair(2, &at(&[0.1, 0.1]));
        lane.set_post_dsp(false);
        assert_eq!(
            lane.gains(),
            [1.0; CHANNELS],
            "applying it again would be twice"
        );
        lane.set_post_dsp(true);
        assert_eq!(lane.gains()[..2], [0.1, 0.1]);
    }

    #[test]
    fn only_a_write_that_changes_the_volume_counts_as_a_change() {
        let lane = LaneVolume::new();
        lane.begin_pair(2, &at(&[0.5, 0.5]));
        let built = lane.changes();
        let echo = PropsUpdate {
            volume: Some(1.0),
            channel_volumes: Some(vec![0.5, 0.5]),
            mute: Some(false),
            clamped: None,
            locked: None,
            whole: false,
        };
        assert!(!lane.update(&echo), "the engine's own write coming back");
        assert_eq!(lane.changes(), built);
        assert!(lane.update(&PropsUpdate {
            channel_volumes: Some(vec![0.4, 0.4]),
            ..PropsUpdate::default()
        }));
        assert_eq!(lane.changes(), built + 1);
    }

    // ---- the report's debounce

    #[test]
    fn a_change_is_reported_only_once_it_has_held_still_for_a_tick() {
        let mut debounce = Debounce::default();
        assert!(debounce.settled(0), "nothing has moved");
        assert!(!debounce.settled(1), "moved since the last tick");
        assert!(debounce.settled(1), "held still for a tick");
        assert!(debounce.settled(1));
    }

    #[test]
    fn a_slider_being_dragged_is_not_reported_until_it_stops() {
        let mut debounce = Debounce::default();
        for changes in 1..=10 {
            assert!(!debounce.settled(changes * 7), "tick {changes}");
        }
        assert!(debounce.settled(70));
    }
}
