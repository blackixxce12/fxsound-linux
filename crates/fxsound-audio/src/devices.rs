//! Device enumeration — playback *and* capture — and the device-selection rules.
//!
//! This is the Linux half of two Windows files: `sndDevices_GetAll()`
//! (`audiopassthru/src/sndDevices/sndDevices_GetAll.cpp:42`), which walked the WASAPI endpoint
//! enumerator, and `sndDevicesImplementDeviceRules()`
//! (`audiopassthru/src/sndDevices/sndDevicesImplementDeviceRules.cpp:44`), which decided *which*
//! endpoint FxSound should render to. The second of those is, in the original's own words, the
//! single most behaviour-defining function in the module, so it is reproduced branch for branch
//! below with only the two substitutions `docs/spec/12-audio-io.md` §19.5 calls for: a "real
//! device" is any `Audio/Sink` node that is not ours, and "the current default" is
//! `default.audio.sink` from PipeWire's `default` metadata object.
//!
//! The Linux port adds one axis the Windows code never had: **direction**. The same rules run,
//! unchanged, over `Audio/Source` nodes when FxSound sits behind a microphone
//! (`docs/spec/12-audio-io.md` §28), with "the current default" then read from
//! `default.audio.source`.
//!
//! One branch of the Windows rules is deliberately not ported: the mono guard. Windows refused
//! mono *playback* devices to work around a bug in its own driver (`sndDevices.h:32-39`,
//! `SND_DEVICES_MONO_BUG_*`), and PipeWire has no such bug to work around. So no device is refused
//! for its channel count, in either direction: a lane's pair always runs at least a stereo pair,
//! and the adapter PipeWire puts in front of the real device converts — it up-mixes a mono
//! microphone into the pair, and down-mixes the pair into a mono headset. That is what keeps the
//! music playing in a Bluetooth headset that has just switched to its call profile, which is mono
//! (`docs/spec/12-audio-io.md`, open question 6).
//!
//! Everything here is a pure function over property dictionaries. Nothing in this module talks to
//! a PipeWire server, which is what lets the rules be tested against a captured `pw-dump` rather
//! than against the machine the tests happen to run on.

use fxsound_core::{AudioDevice, DeviceDirection};

use crate::{AudioError, OUR_NODE_NAMES};

/// `SND_DEVICES_MIN_NUM_CHANS` (`audiopassthru/include/sndDevices.h:190`).
///
/// The fewest channels a lane's pair ever runs, not the fewest a device may have. A device with
/// fewer is attached like any other and its adapter converts: a mono microphone is up-mixed into
/// the pair and the pair is down-mixed into a mono headset (see [`DeviceInfo::clamped_channels`]).
/// Windows refused such a *sink* instead (`SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES`,
/// `sndDevices.h:39`), to work around a driver bug PipeWire does not have.
pub const MIN_CHANNELS: u32 = 2;

/// `SND_DEVICES_MAX_NUM_CHANS` (`sndDevices.h:191`).
pub const MAX_CHANNELS: u32 = 8;

/// `SND_DEVICES_MAX_SAMP_FREQ` (`sndDevices.h:189`).
pub const MAX_SAMPLE_RATE: u32 = 192_000;

/// The rate a Bluetooth headset (HFP/HSP) link is taken to carry when nothing names its codec:
/// mSBC's 16 kHz, the wide band nearly every headset of the last decade negotiates.
///
/// No headset figure is ever published as `audio.rate` — the node negotiates whatever the graph
/// runs at and resamples inside bluez — so the codec, and failing that the profile, is the only
/// place the link's real bandwidth shows. [`DeviceInfo::native_rate`] reports it so that the
/// adaptive de-esser has something to adapt to (`docs/0.4.0-design.md` §6). WirePlumber 0.5's
/// microphone names no codec at all ([`BluezFacts::loopback`]), so this is what it gets.
pub const BLUEZ_HEADSET_RATE: u32 = 16_000;

/// The rate a Bluetooth headset profile's codec carries, by the name PipeWire's bluez5 plugin
/// writes into `api.bluez5.codec`; `None` for a codec that is not a headset codec (an A2DP node
/// names `sbc`, `aac`, `ldac` and the like) or one this build does not know.
///
/// The four hands-free codecs PipeWire 1.6 ships
/// (`/usr/lib/spa-0.2/bluez5/libspa-codec-bluez5-hfp-*`): CVSD, the narrow band every headset
/// falls back to; mSBC, the wide band; LC3 at 24 kHz (`lc3_a127`, "LC3-24kHz"); and LC3-SWB, the
/// super-wide band of HFP 1.9 (`hfp-codec-lc3-swb.c`, `lc3_frame_samples(7500, 32000)`).
#[must_use]
pub fn bluez_codec_rate(codec: &str) -> Option<u32> {
    match codec {
        "cvsd" => Some(8_000),
        "msbc" => Some(16_000),
        "lc3_a127" => Some(24_000),
        "lc3_swb" => Some(32_000),
        _ => None,
    }
}

/// The `media.class` a node must carry to be a playback device we can render into.
pub const SINK_MEDIA_CLASS: &str = "Audio/Sink";

/// The `media.class` of a hardware capture device we can listen to.
pub const SOURCE_MEDIA_CLASS: &str = "Audio/Source";

/// The `media.class` of a virtual capture device (a null source, another app's loopback). Listed
/// as an input like any other: from our point of view it is a signal to process.
pub const VIRTUAL_SOURCE_MEDIA_CLASS: &str = "Audio/Source/Virtual";

/// Which direction a node's `media.class` puts it in, or `None` for anything that is not a device
/// FxSound can attach to — streams, devices, monitors, video.
#[must_use]
pub fn direction_of_media_class(media_class: &str) -> Option<DeviceDirection> {
    match media_class {
        SINK_MEDIA_CLASS => Some(DeviceDirection::Output),
        SOURCE_MEDIA_CLASS | VIRTUAL_SOURCE_MEDIA_CLASS => Some(DeviceDirection::Input),
        _ => None,
    }
}

/// The `default` metadata key that says what is in effect *now* for a direction — WirePlumber's to
/// write, ours only to read (`docs/spec/12-audio-io.md` §21.6).
#[must_use]
pub const fn default_key(direction: DeviceDirection) -> &'static str {
    match direction {
        DeviceDirection::Output => "default.audio.sink",
        DeviceDirection::Input => "default.audio.source",
    }
}

/// The `default` metadata key that carries the user's choice for a direction — the one FxSound
/// writes when it takes the default, and restores when it hands it back.
#[must_use]
pub const fn configured_default_key(direction: DeviceDirection) -> &'static str {
    match direction {
        DeviceDirection::Output => "default.configured.audio.sink",
        DeviceDirection::Input => "default.configured.audio.source",
    }
}

/// How the GUI should picture a device.
///
/// The eleven variants are exactly the eleven strings `sndDevices_GetAll.cpp:198-208` could write
/// into `deviceFormFactor`, kept 1:1 so the Windows build's icon table ports without a lookup
/// change. PipeWire has no single equivalent of `PKEY_AudioEndpoint_FormFactor`, so the mapping in
/// [`FormFactor::from_props`] reads several properties, in the order `docs/spec/12-audio-io.md`
/// §19.4 lays out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FormFactor {
    /// Built-in or desktop speakers.
    Speakers,
    /// Wired or wireless headphones.
    Headphones,
    /// Headphones with a microphone.
    Headset,
    /// A line-level output.
    LineLevel,
    /// An S/PDIF or IEC958 output.
    Spdif,
    /// An HDMI or DisplayPort audio output.
    Hdmi,
    /// A passthrough output whose payload is not PCM.
    DigitalPassthrough,
    /// A network sink (RAOP, roc, PulseAudio tunnel).
    NetworkDevice,
    /// A telephony handset.
    Handset,
    /// A microphone — every capture device that nothing else identifies.
    Microphone,
    /// Anything the properties do not identify.
    #[default]
    Unknown,
}

impl FormFactor {
    /// The stable key written into [`fxsound_core::settings::DeviceConfig::device_form_factor`].
    ///
    /// These are the lowercase PipeWire spellings rather than the Windows CamelCase ones, because
    /// the settings file is new on this platform and there is nothing to stay compatible with.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Speakers => "speaker",
            Self::Headphones => "headphone",
            Self::Headset => "headset",
            Self::LineLevel => "line-level",
            Self::Spdif => "spdif",
            Self::Hdmi => "hdmi",
            Self::DigitalPassthrough => "digital-passthrough",
            Self::NetworkDevice => "network",
            Self::Handset => "handset",
            Self::Microphone => "microphone",
            Self::Unknown => "unknown",
        }
    }

    /// Classify a device from its PipeWire properties.
    ///
    /// `get` is the node's property dictionary; in production it is a `DictRef`, in tests it is a
    /// fixture parsed out of `pw-dump`. The result is direction-agnostic; [`DeviceInfo::from_props`]
    /// turns "speakers or unknown" into [`FormFactor::Microphone`] for a capture device, because an
    /// ALSA source carries the same `audio-card` icon as the sink next to it.
    #[must_use]
    pub fn from_props<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> Self {
        let node_name = get("node.name").unwrap_or_default();
        let profile = get("device.profile.name").unwrap_or_default();

        // Digital outputs are recognisable from the ALSA profile/node name before anything else,
        // because their `device.form-factor` is usually absent or says "internal".
        if node_name.contains(".hdmi-") || profile.starts_with("hdmi-") {
            return Self::Hdmi;
        }
        if node_name.contains(".iec958-") || profile.contains("iec958") {
            return Self::Spdif;
        }

        if let Some(form) = get("device.form-factor") {
            match form {
                "internal" | "speaker" => return Self::Speakers,
                "headphone" => return Self::Headphones,
                "headset" => return Self::Headset,
                "hands-free" | "handset" => return Self::Handset,
                "microphone" => return Self::Microphone,
                "tv" => return Self::Hdmi,
                _ => {}
            }
        }

        // Bluetooth: the profile says whether the microphone is in play, and WirePlumber 0.5's
        // microphone, which names no profile, is a headset's by what it is. A node that is
        // Bluetooth and says neither is a pair of headphones here; [`DeviceInfo::from_props`]
        // knows its direction, and makes a microphone of that kind a headset's.
        let bluez = BluezFacts::from_props(get);
        if bluez.is_bluetooth() {
            return if bluez.headset_profile.unwrap_or(bluez.loopback) {
                Self::Headset
            } else {
                Self::Headphones
            };
        }

        // `raop`/`roc`/`pulse-tunnel` sinks all announce themselves through device.api.
        if let Some("raop" | "roc" | "pulse-tunnel") = get("device.api") {
            return Self::NetworkDevice;
        }
        if get("node.network") == Some("true") {
            return Self::NetworkDevice;
        }

        match get("device.icon-name") {
            Some(icon) if icon.contains("headphone") => Self::Headphones,
            Some(icon) if icon.contains("headset") => Self::Headset,
            Some(icon) if icon.contains("microphone") => Self::Microphone,
            Some(icon) if icon.contains("speaker") || icon.contains("audio-card") => Self::Speakers,
            _ => Self::Unknown,
        }
    }
}

/// What a node's properties say about the Bluetooth link behind it — kept whole on the
/// [`DeviceInfo`], rather than only the verdicts drawn from it, because the facts arrive in
/// pieces and the verdicts are drawn again whenever one does.
///
/// The pieces: the node's registry global carries none of them (it is `node.name`,
/// `node.description`, `media.class`, `device.id` and a few ids, nothing else); the node's own
/// info carries the rest of its properties once the engine has bound it
/// ([`DeviceInfo::learn_bluetooth`]); and whether its card is Bluetooth is a property of another
/// object, the `Device` it names in `device.id` ([`DeviceInfo::on_bluetooth_card`]).
///
/// Two generations of WirePlumber make a headset's microphone differently, and both are read
/// here. WirePlumber 0.4 lists the SCO source the bluez5 plugin emits, `bluez_input.<addr>.0`,
/// with `api.bluez5.profile = headset-head-unit` and `api.bluez5.codec`. WirePlumber 0.5 marks
/// that source `api.bluez5.internal` (`monitors/bluez/create-node.lua:31-36`) and puts a loopback
/// in front of it for applications to record from: `bluez_input.<addr>`, with
/// `bluez5.loopback = true`, `device.id` and no `api.bluez5.*` key at all
/// (`monitors/bluez/create-loopback-node.lua:44-55`) — so it names neither a profile nor a codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BluezFacts {
    /// `api.bluez5.profile`, when the node names one: whether it is a headset profile
    /// (`headset-head-unit`, `headset-audio-gateway`, or an `hfp-*` spelling) — the
    /// one kind of Bluetooth profile with a microphone in play. What a node says of its own
    /// profile outranks everything below: an A2DP sink on a headset's card is headphones.
    pub headset_profile: Option<bool>,
    /// `bluez5.loopback = true`: WirePlumber 0.5's Bluetooth microphone. It exists only for a
    /// device with a headset profile (`create-loopback-node.lua:77-86`), so it is a headset's.
    pub loopback: bool,
    /// The node says it is Bluetooth itself: `device.api = bluez5`, `device.bus = bluetooth`, or
    /// an `api.bluez5.profile`/`address`/`codec` of its own.
    pub own: bool,
    /// The card the node names in `device.id` is Bluetooth — its `Device` says
    /// `device.api = bluez5`. Never read from the node's properties: the engine learns it from the
    /// card and keeps it through the node's info ([`DeviceInfo::on_bluetooth_card`]).
    pub card: bool,
    /// The rate the node's headset codec carries ([`bluez_codec_rate`] of `api.bluez5.codec`),
    /// when it names one.
    pub codec_rate: Option<u32>,
    /// `api.bluez5.internal = true`: one of WirePlumber 0.5's own SCO nodes, behind the loopback
    /// microphone. Never a device: recording from it directly would bypass the loopback
    /// WirePlumber switches the headset's profile through.
    pub internal: bool,
}

impl BluezFacts {
    /// Read the facts a node's own properties carry. [`Self::card`] is left `false`: it is a
    /// property of another object.
    #[must_use]
    pub fn from_props<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> Self {
        let profile = get("api.bluez5.profile");
        let codec = get("api.bluez5.codec");
        Self {
            headset_profile: profile.map(|p| p.starts_with("headset") || p.starts_with("hfp")),
            loopback: get("bluez5.loopback") == Some("true"),
            own: get("device.api") == Some("bluez5")
                || get("device.bus") == Some("bluetooth")
                || profile.is_some()
                || codec.is_some()
                || get("api.bluez5.address").is_some(),
            card: false,
            codec_rate: codec.and_then(bluez_codec_rate),
            internal: get("api.bluez5.internal") == Some("true"),
        }
    }

    /// Whether anything says the node is Bluetooth: itself, its being WirePlumber's loopback, or
    /// its card.
    #[must_use]
    pub const fn is_bluetooth(&self) -> bool {
        self.own || self.loopback || self.card
    }

    /// Whether the node is a Bluetooth headset's end — a headset profile's sink or source, or a
    /// headset's microphone.
    ///
    /// The profile decides whenever the node names one. A node that names none is a headset's
    /// when it is WirePlumber 0.5's loopback, and when it is a Bluetooth *microphone*: the headset
    /// profiles are the only ones that carry a headset's microphone, and every other profile with
    /// a source in it (A2DP from a phone, LE audio) names itself. A Bluetooth sink that names no
    /// profile is not assumed to be one — the A2DP sink's registry global names none either, and
    /// would be taken for a call-quality link until its info arrived.
    #[must_use]
    pub const fn is_headset(&self, direction: DeviceDirection) -> bool {
        match self.headset_profile {
            Some(headset) => headset,
            None => {
                self.loopback
                    || (matches!(direction, DeviceDirection::Input) && (self.own || self.card))
            }
        }
    }
}

/// A form factor read from a node's properties, for a node of `direction`: an ALSA source carries
/// its card's `audio-card` icon, which reads as speakers, and for a capture device the honest
/// default is a microphone.
const fn directed(direction: DeviceDirection, form_factor: FormFactor) -> FormFactor {
    match (direction, form_factor) {
        (DeviceDirection::Input, FormFactor::Speakers | FormFactor::Unknown) => {
            FormFactor::Microphone
        }
        (_, form_factor) => form_factor,
    }
}

/// `api.bluez5.address`, the one key a Bluetooth node and its card both carry, when it says
/// anything.
pub(crate) fn bluez_address<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> Option<String> {
    get("api.bluez5.address")
        .filter(|address| !address.is_empty())
        .map(str::to_owned)
}

/// A PipeWire `Device` object: the sound card, or the Bluetooth headset, that nodes belong to —
/// what WirePlumber names `alsa_card.*` and `bluez_card.*`.
///
/// Kept for one question: when the node a lane is attached to goes, did its card go with it? A
/// card that stays is a card between profiles. WirePlumber 0.5.17 switches a headset to its call
/// profile when something records from it, and back to A2DP when the recording ends
/// (`device/autoswitch-bluetooth-profile.lua`), and every switch removes the headset's sink and
/// adds it back under the same name about half a second later (`monitors/bluez/name-node.lua:52-55`
/// names a Bluetooth node by address and node number, not by profile). An ALSA card switched to
/// another profile does the same to its nodes. The engine waits for such a node rather than
/// moving the music to the speakers for the length of the switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// The registry global id: what a node names in `device.id`. Runtime only.
    pub object_id: u32,
    /// `api.bluez5.address`, for a Bluetooth card. Not in the registry global; the engine reads it
    /// from the card's own info once it is bound.
    pub bluez_address: Option<String>,
    /// `device.api = bluez5` (or an address): a Bluetooth device. Unlike the address, the
    /// registry global carries `device.api`, so this is known the moment the card is announced —
    /// before the info of any node on it, which is what lets a node that names this card in
    /// `device.id` count as Bluetooth from its own registry global on ([`BluezFacts::card`]).
    pub bluetooth: bool,
}

impl Card {
    /// Build a card from a `Device` object's property dictionary.
    #[must_use]
    pub fn from_props<'a>(object_id: u32, get: &impl Fn(&str) -> Option<&'a str>) -> Self {
        let bluez_address = bluez_address(get);
        Self {
            object_id,
            bluetooth: get("device.api") == Some("bluez5") || bluez_address.is_some(),
            bluez_address,
        }
    }

    /// Whether a node that names `card_id` in `device.id`, or carries `bluez_address`, belongs to
    /// this card. Either is enough; neither is nothing — two unknowns are not the same card.
    ///
    /// Asked with the two keys of a node that has just gone ([`DeviceInfo::card_present`]), and
    /// again, with the same two kept, when a card goes after it: a lane waiting for a node whose
    /// card has gone too waits no more.
    #[must_use]
    pub fn owns(&self, card_id: Option<u32>, bluez_address: Option<&str>) -> bool {
        card_id == Some(self.object_id)
            || (bluez_address.is_some() && bluez_address == self.bluez_address.as_deref())
    }
}

/// The most positioned channels FxSound will ever declare, `SND_DEVICES_MAX_NUM_CHANS`.
pub const MAX_POSITIONS: usize = MAX_CHANNELS as usize;

/// A channel layout, as a fixed array of `enum spa_audio_channel` ids.
///
/// Fixed-size and `Copy` so a layout can be carried into the process callback's neighbourhood
/// without an allocation. The ids are the ones in `spa/param/audio/raw.h:153-194`, reached through
/// `libspa::sys` because 0.10.1 has no `AudioChannel` wrapper type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelMap {
    ids: [u32; MAX_POSITIONS],
    len: u8,
}

/// The channel names PipeWire prints in `audio.position`, with their SPA ids.
const CHANNEL_NAMES: [(&str, u32); 15] = [
    ("MONO", libspa::sys::SPA_AUDIO_CHANNEL_MONO),
    ("FL", libspa::sys::SPA_AUDIO_CHANNEL_FL),
    ("FR", libspa::sys::SPA_AUDIO_CHANNEL_FR),
    ("FC", libspa::sys::SPA_AUDIO_CHANNEL_FC),
    ("LFE", libspa::sys::SPA_AUDIO_CHANNEL_LFE),
    ("SL", libspa::sys::SPA_AUDIO_CHANNEL_SL),
    ("SR", libspa::sys::SPA_AUDIO_CHANNEL_SR),
    ("RL", libspa::sys::SPA_AUDIO_CHANNEL_RL),
    ("RR", libspa::sys::SPA_AUDIO_CHANNEL_RR),
    ("RC", libspa::sys::SPA_AUDIO_CHANNEL_RC),
    ("FLC", libspa::sys::SPA_AUDIO_CHANNEL_FLC),
    ("FRC", libspa::sys::SPA_AUDIO_CHANNEL_FRC),
    ("TC", libspa::sys::SPA_AUDIO_CHANNEL_TC),
    ("NA", libspa::sys::SPA_AUDIO_CHANNEL_NA),
    ("UNK", libspa::sys::SPA_AUDIO_CHANNEL_UNKNOWN),
];

impl ChannelMap {
    /// Parse an `audio.position` property value.
    ///
    /// PipeWire is inconsistent about the spelling: a node's own properties carry the JSON-ish
    /// `"[ FL, FR ]"` (seen on every ALSA node in `tests/fixtures/pw-dump-sinks.json`) while the
    /// form accepted when *setting* the property is the bare `"FL,FR"`
    /// (`/usr/share/pipewire/pipewire.conf:335`). Both are accepted here; an unrecognised name
    /// makes the whole layout unusable, which is reported as `None` so the caller falls back to
    /// [`ChannelMap::default_for`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim().trim_start_matches('[').trim_end_matches(']');
        let mut ids = [0_u32; MAX_POSITIONS];
        let mut len = 0_usize;
        for name in trimmed.split(',') {
            let name = name.trim().trim_matches('"');
            if name.is_empty() {
                continue;
            }
            if len == MAX_POSITIONS {
                // More channels than FxSound will drive; the caller clamps to 8 anyway.
                return None;
            }
            let id = CHANNEL_NAMES
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|&(_, id)| id)?;
            ids[len] = id;
            len += 1;
        }
        if len == 0 {
            return None;
        }
        Some(Self {
            ids,
            len: len as u8,
        })
    }

    /// The layout PipeWire's own audioconvert assumes for a given channel count.
    ///
    /// Used when the target does not publish `audio.position`, or publishes one this build does
    /// not understand — and for the stereo pair a mono microphone is up-mixed into.
    #[must_use]
    pub fn default_for(channels: u32) -> Self {
        use libspa::sys::{
            SPA_AUDIO_CHANNEL_FC, SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR,
            SPA_AUDIO_CHANNEL_LFE, SPA_AUDIO_CHANNEL_MONO, SPA_AUDIO_CHANNEL_RC,
            SPA_AUDIO_CHANNEL_RL, SPA_AUDIO_CHANNEL_RR, SPA_AUDIO_CHANNEL_SL, SPA_AUDIO_CHANNEL_SR,
        };
        let layout: &[u32] = match channels {
            0 | 1 => &[SPA_AUDIO_CHANNEL_MONO],
            2 => &[SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR],
            3 => &[
                SPA_AUDIO_CHANNEL_FL,
                SPA_AUDIO_CHANNEL_FR,
                SPA_AUDIO_CHANNEL_LFE,
            ],
            4 => &[
                SPA_AUDIO_CHANNEL_FL,
                SPA_AUDIO_CHANNEL_FR,
                SPA_AUDIO_CHANNEL_RL,
                SPA_AUDIO_CHANNEL_RR,
            ],
            5 => &[
                SPA_AUDIO_CHANNEL_FL,
                SPA_AUDIO_CHANNEL_FR,
                SPA_AUDIO_CHANNEL_FC,
                SPA_AUDIO_CHANNEL_RL,
                SPA_AUDIO_CHANNEL_RR,
            ],
            6 => &[
                SPA_AUDIO_CHANNEL_FL,
                SPA_AUDIO_CHANNEL_FR,
                SPA_AUDIO_CHANNEL_FC,
                SPA_AUDIO_CHANNEL_LFE,
                SPA_AUDIO_CHANNEL_RL,
                SPA_AUDIO_CHANNEL_RR,
            ],
            7 => &[
                SPA_AUDIO_CHANNEL_FL,
                SPA_AUDIO_CHANNEL_FR,
                SPA_AUDIO_CHANNEL_FC,
                SPA_AUDIO_CHANNEL_LFE,
                SPA_AUDIO_CHANNEL_RC,
                SPA_AUDIO_CHANNEL_SL,
                SPA_AUDIO_CHANNEL_SR,
            ],
            _ => &[
                SPA_AUDIO_CHANNEL_FL,
                SPA_AUDIO_CHANNEL_FR,
                SPA_AUDIO_CHANNEL_FC,
                SPA_AUDIO_CHANNEL_LFE,
                SPA_AUDIO_CHANNEL_RL,
                SPA_AUDIO_CHANNEL_RR,
                SPA_AUDIO_CHANNEL_SL,
                SPA_AUDIO_CHANNEL_SR,
            ],
        };
        let mut ids = [0_u32; MAX_POSITIONS];
        for (slot, &id) in ids.iter_mut().zip(layout.iter()) {
            *slot = id;
        }
        Self {
            ids,
            len: layout.len() as u8,
        }
    }

    /// Index of the low-frequency-effects channel, if the layout has one.
    ///
    /// The stages that must not touch the subwoofer need a position, not an index: it sits at 3 in
    /// a standard 5.1 or 7.1 layout, but a device is free to order its channels differently and
    /// several do.
    #[must_use]
    pub fn lfe_index(&self) -> Option<usize> {
        self.ids()
            .iter()
            .position(|&id| id == libspa::sys::SPA_AUDIO_CHANNEL_LFE)
    }

    /// Indices of the front left and front right channels, when the layout names both.
    ///
    /// The two stereo-by-nature effects run one instance over this pair. Finding it by position
    /// rather than assuming channels 0 and 1 is what stops a device that orders its channels
    /// differently from having its widener applied across, say, front-left and centre — which is
    /// audible immediately and impossible for the listener to attribute to FxSound.
    #[must_use]
    pub fn front_pair(&self) -> Option<(usize, usize)> {
        let left = self
            .ids()
            .iter()
            .position(|&id| id == libspa::sys::SPA_AUDIO_CHANNEL_FL)?;
        let right = self
            .ids()
            .iter()
            .position(|&id| id == libspa::sys::SPA_AUDIO_CHANNEL_FR)?;
        (left != right).then_some((left, right))
    }

    /// The layout for exactly `channels` channels: this one when it already has that many, and
    /// PipeWire's default layout for the count when it does not — which is how a mono device's
    /// `MONO` becomes the `FL,FR` its lane's pair runs, whichever way the adapter then converts.
    #[must_use]
    pub fn resized(self, channels: u32) -> Self {
        if usize::from(self.len) == channels as usize {
            return self;
        }
        Self::default_for(channels)
    }

    /// The SPA channel ids, one per channel.
    #[must_use]
    pub fn ids(&self) -> &[u32] {
        &self.ids[..usize::from(self.len)]
    }

    /// How many channels the layout describes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the layout is empty, which [`ChannelMap::parse`] never produces.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The value to write into the `audio.position` node property, e.g. `"FL,FR"`.
    ///
    /// Comma separated with no spaces — the form PipeWire's parser wants when a property is being
    /// *set*, per `docs/api/pipewire-0.10-rust.md` §14 "Spelling traps".
    #[must_use]
    pub fn to_property_value(&self) -> String {
        let mut out = String::with_capacity(self.len() * 4);
        for (i, id) in self.ids().iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let name = CHANNEL_NAMES
                .iter()
                .find(|(_, candidate)| candidate == id)
                .map_or("UNK", |&(name, _)| name);
            out.push_str(name);
        }
        out
    }

    /// The layout as the `[u32; 64]` array `libspa::param::audio::AudioInfoRaw::set_position`
    /// wants.
    #[must_use]
    pub fn to_spa_position(&self) -> [u32; libspa::param::audio::MAX_CHANNELS] {
        let mut position = [0_u32; libspa::param::audio::MAX_CHANNELS];
        for (slot, &id) in position.iter_mut().zip(self.ids()) {
            *slot = id;
        }
        position
    }
}

/// One device FxSound could attach to — the Linux `SoundDevice`
/// (`audiopassthru/include/AudioPassthru.h:32-53`), for either direction.
///
/// The Windows struct's `pwszID` becomes [`DeviceInfo::name`] (`node.name`), which is what gets
/// persisted; `object_id` is a runtime handle only and changes on every PipeWire restart, so it is
/// never written to disk. Fields the Windows struct carried but nothing ever read
/// (`pAllDevices`, `isPlaybackDevice`, and the garbage `isUserSelectedPlaybackDevice` described in
/// `docs/spec/12-audio-io.md` §15) are deliberately absent; `isCaptureDevice` (`:34`) comes back
/// as [`DeviceInfo::direction`], with the opposite meaning — on Windows it marked *our* endpoint,
/// here it marks a real microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The registry global id. Runtime only.
    pub object_id: u32,
    /// `object.serial`, monotonic within one server lifetime. Runtime only.
    ///
    /// What tells this node from another that has since taken its name: the server hands a freed
    /// id to the next object, but never a serial. A Bluetooth headset switching profile has its
    /// sink removed and added back under the same `node.name` with a new serial, and a pair built
    /// on the old one has to be rebuilt on the new one for anything to link to it again.
    pub object_serial: Option<u64>,
    /// `device.id`: the [`Card`] — PipeWire's `Device` object, the sound card or the Bluetooth
    /// headset — this node belongs to, when it belongs to one. Runtime only; a virtual sink has
    /// none. The card outlives its nodes' comings and goings, which is what
    /// [`DeviceInfo::card_present`] asks about.
    pub card_id: Option<u32>,
    /// `api.bluez5.address`, on a node PipeWire's Bluetooth plugin made: the other way to tell
    /// which [`Card`] it belongs to. The registry global does not carry it; the engine reads it
    /// from the node's own info.
    pub bluez_address: Option<String>,
    /// `node.name` — the stable identity, the analogue of the WASAPI endpoint id string.
    pub name: String,
    /// `node.description` — what the device picker shows (`deviceFriendlyName`).
    pub description: String,
    /// `node.nick`, falling back to the description (`deviceDescription`).
    pub nick: String,
    /// `audio.channels`, or 0 when the node does not say.
    pub channels: u32,
    /// `audio.rate` when the node publishes one. ALSA nodes usually do not.
    pub rate: Option<u32>,
    /// Whether the node is a Bluetooth headset's end: a headset (HFP/HSP) profile's sink or source,
    /// or WirePlumber 0.5's loopback microphone ([`BluezFacts::is_headset`]). Such a link carries
    /// its codec's rate — [`BLUEZ_HEADSET_RATE`] when it names none — and never says so in
    /// `audio.rate`; see [`DeviceInfo::native_rate`]. Drawn from [`DeviceInfo::bluez`], again
    /// whenever that changes.
    pub bluez_headset: bool,
    /// What the node's properties, and its card, say about the Bluetooth link behind it.
    pub bluez: BluezFacts,
    /// `audio.position`, or the default layout for [`DeviceInfo::channels`].
    pub positions: ChannelMap,
    /// What the GUI should draw next to it.
    pub form_factor: FormFactor,
    /// Playback device (`Audio/Sink`) or capture device (`Audio/Source`).
    pub direction: DeviceDirection,
}

impl DeviceInfo {
    /// Build a device from a node's property dictionary, or `None` if the node is neither a sink
    /// nor a source — or is one of FxSound's own four nodes, which must never be listed as a
    /// device to attach to, or one of WirePlumber's internal Bluetooth nodes
    /// ([`BluezFacts::internal`]).
    ///
    /// Mirrors pass 1 of `sndDevices_GetAll.cpp:141-267`, including its fallbacks: a node with no
    /// `node.description` is labelled with its `node.name` rather than being dropped (the Windows
    /// code wrote `L"Unknown"`, `:260-262`), and a node that does not publish a channel count gets
    /// 0 — unknown, which [`DeviceInfo::clamped_channels`] runs as stereo until the node's own
    /// info says otherwise.
    #[must_use]
    pub fn from_props<'a>(object_id: u32, get: &impl Fn(&str) -> Option<&'a str>) -> Option<Self> {
        let direction = direction_of_media_class(get("media.class")?)?;
        let name = get("node.name")?.to_owned();
        if name.is_empty() {
            // `sndDeviceHandleToSoundDevices` skips entries with an empty id
            // (`AudioPassthruPrivate.cpp:174`).
            return None;
        }
        if OUR_NODE_NAMES.contains(&name.as_str()) {
            return None;
        }
        let description = get("node.description")
            .filter(|d| !d.is_empty())
            .unwrap_or(&name)
            .to_owned();
        let nick = get("node.nick")
            .filter(|d| !d.is_empty())
            .unwrap_or(&description)
            .to_owned();
        let channels = get("audio.channels")
            .and_then(|c| c.parse::<u32>().ok())
            .unwrap_or(0);
        let rate = get("audio.rate")
            .and_then(|r| r.parse::<u32>().ok())
            .filter(|&r| r > 0 && r <= MAX_SAMPLE_RATE);
        let bluez = BluezFacts::from_props(get);
        if bluez.internal {
            return None;
        }
        let positions = get("audio.position")
            .and_then(ChannelMap::parse)
            .filter(|map| map.len() == channels as usize)
            .unwrap_or_else(|| ChannelMap::default_for(channels));
        let form_factor = directed(direction, FormFactor::from_props(get));

        let mut device = Self {
            object_id,
            object_serial: get("object.serial").and_then(|s| s.parse::<u64>().ok()),
            card_id: get("device.id").and_then(|s| s.parse::<u32>().ok()),
            bluez_address: bluez_address(get),
            name,
            description,
            nick,
            channels,
            rate,
            bluez_headset: false,
            bluez,
            positions,
            form_factor,
            direction,
        };
        device.classify_bluetooth();
        Some(device)
    }

    /// Draw [`Self::bluez_headset`] and the Bluetooth part of [`Self::form_factor`] from
    /// [`Self::bluez`]: a headset's end is a headset; any other Bluetooth node is a pair of
    /// headphones, unless its properties said something more particular. The same verdicts
    /// [`FormFactor::from_props`] draws from a node's own properties, plus what only the
    /// direction and the card can add — a Bluetooth microphone that names no profile is a
    /// headset's, and so is a node on a Bluetooth card that says nothing of itself.
    fn classify_bluetooth(&mut self) {
        self.bluez_headset = self.bluez.is_headset(self.direction);
        if self.bluez_headset {
            self.form_factor = FormFactor::Headset;
        } else if self.bluez.is_bluetooth()
            && matches!(
                self.form_factor,
                FormFactor::Unknown | FormFactor::Microphone
            )
        {
            self.form_factor = FormFactor::Headphones;
        }
    }

    /// Take what a node's own info says about its Bluetooth link — `bluez`, and `form_factor` as
    /// [`FormFactor::from_props`] reads the same dictionary — and draw the verdicts again. Returns
    /// whether any of them changed.
    ///
    /// Needed because the registry global the device was first built from carries none of it:
    /// without this, WirePlumber 0.5's microphone was a plain microphone to the engine, and even
    /// WirePlumber 0.4's SCO source, whose profile is only in its info, fed the de-esser as a
    /// 48 kHz source (`docs/0.4.0-upstream.md` U9). What the card said ([`BluezFacts::card`]) is
    /// kept: it is not a property of the node. For a node that is not Bluetooth at all the form
    /// factor is left as the registry global had it — the icon of an ALSA device is not this
    /// method's business.
    ///
    /// Only for an info that carries the node's properties: one that reports a state change
    /// carries none, and would read as a node that is Bluetooth no more.
    pub fn learn_bluetooth(&mut self, bluez: BluezFacts, form_factor: FormFactor) -> bool {
        let before = (self.bluez, self.bluez_headset, self.form_factor);
        self.bluez = BluezFacts {
            card: self.bluez.card,
            ..bluez
        };
        if self.bluez.is_bluetooth() {
            self.form_factor = directed(self.direction, form_factor);
        }
        self.classify_bluetooth();
        before != (self.bluez, self.bluez_headset, self.form_factor)
    }

    /// The card this node names in `device.id` is Bluetooth ([`Card::bluetooth`]). Returns whether
    /// that is news.
    ///
    /// What makes WirePlumber 0.5's microphone a headset's from the moment its registry global is
    /// announced, before its info says `bluez5.loopback`, and a microphone on a Bluetooth card that
    /// never says anything of itself a headset's at all.
    pub fn on_bluetooth_card(&mut self) -> bool {
        if self.bluez.card {
            return false;
        }
        let before = (self.bluez_headset, self.form_factor);
        self.bluez.card = true;
        self.classify_bluetooth();
        before != (self.bluez_headset, self.form_factor)
    }

    /// Whether this node and `other` are two ends of the same Bluetooth device — its sink and its
    /// microphone, as the two lanes see them.
    ///
    /// Both must be Bluetooth ([`BluezFacts::is_bluetooth`]): an ALSA card's speakers and
    /// microphone name the same `device.id` too, and share nothing a lane could suffer from. Then
    /// the same card by `device.id`, which WirePlumber puts on every node it makes for a headset —
    /// the loopback microphone included, which carries no address (`create-loopback-node.lua:50`)
    /// — or the same `api.bluez5.address`, a node's own or, for a node that names only its card,
    /// the card's from `cards`.
    #[must_use]
    pub fn same_bluetooth_device(&self, other: &Self, cards: &[Card]) -> bool {
        if !self.bluez.is_bluetooth() || !other.bluez.is_bluetooth() {
            return false;
        }
        if self.card_id.is_some() && self.card_id == other.card_id {
            return true;
        }
        let address = |device: &Self| {
            device.bluez_address.clone().or_else(|| {
                let id = device.card_id?;
                cards
                    .iter()
                    .find(|card| card.object_id == id)?
                    .bluez_address
                    .clone()
            })
        };
        address(self).is_some_and(|mine| address(other).as_ref() == Some(&mine))
    }

    /// The rate the device really runs at, as far as its properties say: `audio.rate` when the
    /// node publishes one; for a Bluetooth headset, the rate its codec carries
    /// ([`bluez_codec_rate`]: CVSD 8 kHz, mSBC 16 kHz, LC3 24 kHz, LC3-SWB 32 kHz), or
    /// [`BLUEZ_HEADSET_RATE`] when it names none — which WirePlumber 0.5's loopback microphone
    /// never does; and `None` when the stream rate is all there is to know.
    ///
    /// The capture stream asks for 48 kHz whatever the microphone runs at, so the negotiated
    /// format cannot tell the voice chain how much bandwidth is in the signal — a resampled
    /// 16 kHz headset arrives at 48 kHz with nothing above 8 kHz, and a de-esser built for a
    /// 5500 Hz split would be working on silence. The audio crate hands this to
    /// `InputEngine::set_source_rate` when it builds the input lane's nodes, and the adaptive
    /// de-esser places its corner from it (`docs/0.4.0-design.md` §6).
    #[must_use]
    pub fn native_rate(&self) -> Option<f32> {
        self.native_rate_hz().map(|rate| rate as f32)
    }

    /// [`Self::native_rate`], in whole hertz: what a pair keeps to tell whether it was built for
    /// the device as it is now.
    #[must_use]
    pub fn native_rate_hz(&self) -> Option<u32> {
        self.rate.or_else(|| {
            self.bluez_headset
                .then(|| self.bluez.codec_rate.unwrap_or(BLUEZ_HEADSET_RATE))
        })
    }

    /// Whether the card this node belongs to is among `cards`: the one it names in `device.id`,
    /// or a card with its Bluetooth address.
    ///
    /// Asked about a node that has just gone. Yes means it went because its card is changing
    /// profile, and a node of the same name is about to come back ([`Card`]); no — a node on no
    /// card at all, like a virtual sink, or one whose card went too — means it is gone. The
    /// address is the second way to ask because it is the one both sides of a Bluetooth pair are
    /// known to carry, whatever made the node; WirePlumber also puts `device.id` on every node it
    /// makes for a card, the loopback microphone included (`monitors/bluez/name-node.lua:34`,
    /// `create-loopback-node.lua:50`).
    #[must_use]
    pub fn card_present(&self, cards: &[Card]) -> bool {
        cards
            .iter()
            .any(|card| card.owns(self.card_id, self.bluez_address.as_deref()))
    }

    /// Project into the type the GUI consumes over [`fxsound_core::messages::AudioToUi`].
    ///
    /// `default_for_direction` is the current default *of this device's direction* —
    /// `default.audio.sink` for an output, `default.audio.source` for an input.
    #[must_use]
    pub fn to_audio_device(&self, default_for_direction: Option<&str>) -> AudioDevice {
        AudioDevice {
            id: self.object_id,
            name: self.name.clone(),
            description: self.description.clone(),
            is_default: default_for_direction == Some(self.name.as_str()),
            direction: self.direction,
            form_factor: self.form_factor.key().to_owned(),
        }
    }

    /// Whether the device *says* it has fewer than two channels.
    ///
    /// A description, never a verdict: nothing is refused for it, in either direction (module
    /// docs). Only a *known* channel count below two counts. PipeWire's registry globals do not
    /// carry `audio.channels` — it lives in the node's info, which arrives only after binding to
    /// the node — so a device discovered through the registry reports `0` here until then.
    #[must_use]
    pub const fn is_mono(&self) -> bool {
        self.channels != 0 && self.channels < MIN_CHANNELS
    }

    /// `true` when the node never told us how many channels it has.
    #[must_use]
    pub const fn channels_unknown(&self) -> bool {
        self.channels == 0
    }

    /// The channel count FxSound will actually run, clamped to `2..=8`
    /// (`sndDevices.h:190-191`; `docs/spec/12-audio-io.md` §19.3 keeps the clamp).
    ///
    /// For a mono device this is a stereo pair, in either direction: PipeWire's adapter up-mixes
    /// a mono microphone into it, and down-mixes it into a mono headset — the playback stream
    /// leaves `stream.dont-remix` off for exactly that. The same answer whether the device's
    /// channel count is still unknown or has arrived as one, so a headset whose info comes in
    /// after its pair was built is not rebuilt for it.
    #[must_use]
    pub const fn clamped_channels(&self) -> u32 {
        if self.channels < MIN_CHANNELS {
            MIN_CHANNELS
        } else if self.channels > MAX_CHANNELS {
            MAX_CHANNELS
        } else {
            self.channels
        }
    }
}

/// The five persisted device preferences the Windows rules consult.
///
/// One field per registry value under `HKCU\SOFTWARE\DFX\13\23\devices`
/// (`sndDevices.h:159-170`), with the same names. An empty string means "not set", which is what
/// `sndDevicesReg.cpp:125-128` reads back for a missing value. The engine keeps one of these per
/// direction, so trying a microphone never forgets which speakers the user had.
///
/// Unlike Windows, `user_selected` is actually written — by [`UiToAudio::SelectDevice`]. The
/// original declared the value and read it in four places but never wrote it
/// (`docs/spec/12-audio-io.md` open question 8), expressing user choice by forcing the system
/// default instead; recording the preference without mutating global state is the honest version.
///
/// [`UiToAudio::SelectDevice`]: fxsound_core::messages::UiToAudio::SelectDevice
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionMemory {
    /// The session default the very first time FxSound ran.
    pub original_default: String,
    /// The most recent default that was not us.
    pub most_recent_default: String,
    /// The default before that.
    pub prior_default: String,
    /// The last device we actually attached to.
    pub most_recent_playback: String,
    /// The device the user picked in the list.
    pub user_selected: String,
}

/// The outcome of [`choose_device`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// `node.name` of the device FxSound should attach to.
    pub target: String,
    /// Whether this selection also re-dates the remembered defaults, i.e. the
    /// `WritePreviousDefault` flag of `sndDevicesImplementDeviceRules.cpp:229`, `:248`.
    pub write_previous_default: bool,
}

fn find<'a>(devices: &'a [&DeviceInfo], name: &str) -> Option<&'a DeviceInfo> {
    if name.is_empty() {
        return None;
    }
    devices.iter().copied().find(|s| s.name == name)
}

/// Pick the real device of one direction, reproducing `sndDevicesImplementDeviceRules()`.
///
/// `devices` may carry both directions; only those matching `direction` take part, and
/// `our_node` — `fxsound_sink` or `fxsound_source` — is never a candidate. The branch order is
/// the one drawn at `docs/spec/12-audio-io.md` §19.5, which is in turn the order of
/// `sndDevicesImplementDeviceRules.cpp:97-284`:
///
/// 1. no devices at all → [`AudioError::NoOutputDevices`] / [`AudioError::NoInputDevices`]
///    (`:97-101`);
/// 2. first run after install → whatever the session default is, if it is not us (`:146-167`) —
///    unless the user has already picked a device that is present, in which case rule 4 wins
///    (see the comment in the body);
/// 3. exactly one device → that one (`:172-183`);
/// 4. an explicit user choice that still exists (`:188-193`);
/// 5. a device whose name was not there at the last enumeration (`:197-231`), whether or not the
///    number of devices grew — see the comment in the body;
/// 6. the session default, if it is not us (`:237-249`);
/// 7. otherwise the first of `most_recent_playback`, `most_recent_default`, `prior_default`,
///    `original_default` that is present, else the first device (`:257-284`).
///
/// The Windows rules then ran a mono guard (`:290-327`) that refused a mono playback device and
/// ended in `-58` or `-57`. It is not ported: it worked around a Windows driver bug, and a mono
/// device of either direction is attached like any other (module docs).
///
/// `previous_names` is the snapshot of real device names of this direction from the previous
/// enumeration, the equivalent of `pwszIDPreviousRealDevices` (`sndDevices.h:349`); pass an empty
/// slice on the first call — and on the first call after a lane is switched on again — so rule 5
/// cannot fire.
///
/// # Errors
/// [`AudioError::NoOutputDevices`] / [`AudioError::NoInputDevices`] when the direction has no real
/// device at all — the one state of the Windows rules that survives the mono guard's removal, plus
/// its input-side twin.
pub fn choose_device(
    devices: &[DeviceInfo],
    direction: DeviceDirection,
    our_node: &str,
    current_default: Option<&str>,
    previous_names: &[String],
    memory: &SelectionMemory,
) -> Result<Selection, AudioError> {
    let real: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| d.direction == direction && d.name != our_node)
        .collect();
    if real.is_empty() {
        return Err(match direction {
            DeviceDirection::Output => AudioError::NoOutputDevices,
            DeviceDirection::Input => AudioError::NoInputDevices,
        });
    }
    // "The current default, unless the current default is us."
    let foreign_default = current_default.filter(|d| *d != our_node);

    let mut write_previous_default = false;
    let mut target: Option<&DeviceInfo> = None;

    // 2 — first run after install (`:146`). If we are somehow already the default on a first run,
    // fall through to the repair path rather than picking ourselves (`:166-167`).
    //
    // One deviation: an explicit user choice that is present wins over this rule. Windows never
    // wrote `user_selected` (open question 8), so the two could not conflict there; here the first
    // run of the *input* direction happens precisely because the user picked a microphone, and
    // adopting the current default source over that pick would attach FxSound to the wrong one.
    if memory.most_recent_default.is_empty()
        && find(&real, &memory.user_selected).is_none()
        && let Some(default_name) = foreign_default
    {
        target = find(&real, default_name);
    }

    // 3 — exactly one real device (`:172-183`). Windows checked it for being mono afterwards
    // like every other branch; nothing here does.
    if target.is_none() && real.len() == 1 {
        target = Some(real[0]);
    }

    // 4 — an explicit user choice (`:188-193`).
    if target.is_none() {
        target = find(&real, &memory.user_selected);
    }

    // 5 — a device was just added (`:197-231`). Windows only looked for the new device when the
    // count had grown (`:197`), and so missed it whenever something left in the same batch of
    // registry events: a USB DAC swapped for another, a dock handing its outputs over, a device
    // that changes its node name when it changes profile — an ALSA card going from stereo to 5.1.
    // Upstream has an open PR for the same miss (#532, `reconnectedDeviceGuid`; Discussion #311).
    // Here the name sets decide, whatever the counts did, which also drops the off-by-one at
    // `:223` (see spec §15). A device that comes back under the name it left with — a Bluetooth
    // headset between profiles — is not new when it comes back within the same run of the rules:
    // the rules below find it again as what it was, the user's pick or the device we last played
    // to. If its removal had a run of its own — the engine runs the rules from its supervisor tick,
    // which a slow profile switch can straddle, and every run replaces `previous_names` — rule 5
    // takes it back as new: the same device either way, only with the remembered defaults written
    // again.
    if target.is_none() && !previous_names.is_empty() {
        target = real
            .iter()
            .copied()
            .find(|s| !previous_names.iter().any(|p| p == &s.name));
        write_previous_default = target.is_some();
    }

    // 6 — the session default is not us (`:237-249`).
    if target.is_none()
        && let Some(default_name) = foreign_default
        && let Some(device) = find(&real, default_name)
    {
        target = Some(device);
        write_previous_default = true;
    }

    // 7 — we are already the default; walk the remembered devices (`:257-284`).
    if target.is_none() {
        for remembered in [
            &memory.most_recent_playback,
            &memory.most_recent_default,
            &memory.prior_default,
            &memory.original_default,
        ] {
            target = find(&real, remembered);
            if target.is_some() {
                break;
            }
        }
        if target.is_none() {
            target = Some(real[0]);
        }
    }

    // Rule 7 always ends in a device, so this cannot fail. `:290-327` would refuse it here for
    // being mono; see the doc comment for why that is not ported.
    let chosen = target.ok_or(AudioError::DeviceNotPresent)?;

    Ok(Selection {
        target: chosen.name.clone(),
        write_previous_default,
    })
}

/// Record a committed selection, the equivalent of the registry writes at
/// `sndDevicesImplementDeviceRules.cpp:364-376`.
///
/// `most_recent_playback` always moves; the two "default" slots only move when the selection came
/// from a branch that set `WritePreviousDefault`. The first-run write of `original_default` and
/// `most_recent_default` (`:156-159`) is folded in here as "fill the slot if it is still empty",
/// which is the same thing given that only the first run leaves them empty.
pub fn commit(memory: &mut SelectionMemory, selection: &Selection) {
    let previous_playback = std::mem::take(&mut memory.most_recent_playback);
    memory.most_recent_playback.clone_from(&selection.target);
    if selection.write_previous_default {
        memory.most_recent_default.clone_from(&selection.target);
        memory.prior_default = previous_playback;
    }
    if memory.original_default.is_empty() {
        memory.original_default.clone_from(&selection.target);
    }
    if memory.most_recent_default.is_empty() {
        memory.most_recent_default.clone_from(&selection.target);
    }
}

/// Which device of `direction` the session default should be handed back to when FxSound stops
/// owning it.
///
/// The order is `sndDevicesRestoreDefaultDevice`'s (`sndDevicesSetupDevices.cpp:617-630`):
/// `user_selected` → `most_recent_playback` → `most_recent_default` → `prior_default` →
/// `original_default`, first one that is actually present *in that direction*. `None` means
/// "nothing we remember is here any more", in which case the caller must leave the default alone
/// and let WirePlumber pick.
#[must_use]
pub fn restore_default_candidate(
    memory: &SelectionMemory,
    devices: &[DeviceInfo],
    direction: DeviceDirection,
) -> Option<String> {
    [
        &memory.user_selected,
        &memory.most_recent_playback,
        &memory.most_recent_default,
        &memory.prior_default,
        &memory.original_default,
    ]
    .into_iter()
    .find(|name| {
        !name.is_empty()
            && devices
                .iter()
                .any(|d| d.direction == direction && &d.name == *name)
    })
    .cloned()
}

/// Pull the `name` out of a `Spa:String:JSON` default-metadata value.
///
/// The value WirePlumber stores under `default.audio.sink` and `default.audio.source` is
/// `{"name":"alsa_output.…"}` (`docs/spec/12-audio-io.md` §21). This is a deliberately minimal
/// scan rather than a JSON parser: the shape is fixed, the crate has no JSON dependency, and
/// anything unexpected must degrade to "no default known" rather than to an error.
#[must_use]
pub fn parse_default_node_name(value: &str) -> Option<String> {
    let after_key = value.split("\"name\"").nth(1)?;
    let after_colon = after_key.split_once(':')?.1;
    let start = after_colon.find('"')? + 1;
    let rest = after_colon.get(start..)?;
    let end = rest.find('"')?;
    let name = rest.get(..end)?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Format the value to write into `default.configured.audio.sink` / `.source`.
#[must_use]
pub fn default_node_value(node_name: &str) -> String {
    format!("{{\"name\":\"{node_name}\"}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `pw-dump` taken on a live PipeWire 1.6.8 session, filtered to the Node and Metadata
    /// objects this module cares about. Captured rather than hand-written so the property spellings
    /// are the server's, not this author's memory of them.
    const PW_DUMP: &str = include_str!("../tests/fixtures/pw-dump-sinks.json");

    /// A Bluetooth headset in its call profile, `headset-head-unit`, beside a laptop's own speakers
    /// and microphone: the graph WirePlumber 0.5.17 builds while something records from the
    /// headset's microphone.
    ///
    /// Composed rather than captured, because no headset was at hand and the session graph is not
    /// ours to probe. Every property is one something in that stack sets: WirePlumber's
    /// `monitors/bluez/name-node.lua` (the node names, description, priorities, `device.id`,
    /// `factory.name`, `spa.object.id`, `node.pause-on-idle`), `create-node.lua`
    /// (`api.bluez5.internal` and `bluez5.loopback` on the SCO source), `create-loopback-node.lua`
    /// (the loopback microphone and its internal capture stream, as written there), and the
    /// `api.bluez5.*` keys PipeWire's bluez5 plugin puts on the nodes it emits. The sink's one
    /// channel is the SCO link's; the engine reads it from the node's info, not from the registry
    /// global, and the fixture holds it where `pw-dump` would print it.
    const BLUEZ_HEADSET_HEAD_UNIT_DUMP: &str =
        include_str!("../tests/fixtures/pw-dump-bluez-headset-head-unit.json");

    /// The headset's sink in that fixture. The same name it has on A2DP: WirePlumber names a
    /// Bluetooth node by its address and its node id on the device, not by profile
    /// (`name-node.lua:52-55`).
    const HEADSET_SINK: &str = "bluez_output.00_11_22_33_44_55.1";

    /// The laptop's speakers in that fixture.
    const LAPTOP_SPEAKERS: &str = "alsa_output.pci-0000_00_1f.3.analog-stereo";

    /// The laptop's microphone in the Bluetooth fixtures — on the same ALSA card as its speakers.
    const LAPTOP_MICROPHONE: &str = "alsa_input.pci-0000_00_1f.3.analog-stereo";

    /// The same headset under WirePlumber 0.5 in A2DP, the profile it idles in: the graph a user
    /// has in front of them when they pick the headset's microphone for the input lane.
    ///
    /// Composed from [`BLUEZ_HEADSET_HEAD_UNIT_DUMP`], as that one was, with what changes between
    /// the two profiles: the sink runs `a2dp-sink` with `sbc` over two channels
    /// (`factory.name = api.bluez5.a2dp.sink`), and the SCO source is gone. The loopback microphone
    /// and its internal capture stream stay: with `bluetooth.autoswitch-to-headset-profile` on —
    /// WirePlumber's default — the loopback exists for as long as the device has a headset profile
    /// at all, whichever one it runs (`create-loopback-node.lua:86-98`).
    const BLUEZ_A2DP_WIREPLUMBER_05_DUMP: &str =
        include_str!("../tests/fixtures/pw-dump-bluez-a2dp-wireplumber-0.5.json");

    /// The same headset in its call profile under WirePlumber 0.4, which has no loopback: the SCO
    /// source `bluez_input.<addr>.0` is the microphone itself, with neither `api.bluez5.internal`
    /// nor `bluez5.loopback` (both are WirePlumber 0.5's, `create-node.lua:31-36`). Composed from
    /// [`BLUEZ_HEADSET_HEAD_UNIT_DUMP`] with those two keys taken out, the loopback nodes left out,
    /// and a headset that negotiated CVSD rather than mSBC — the narrow band some still do.
    const BLUEZ_HEADSET_HEAD_UNIT_WIREPLUMBER_04_DUMP: &str =
        include_str!("../tests/fixtures/pw-dump-bluez-headset-head-unit-wireplumber-0.4.json");

    /// WirePlumber 0.5's microphone for the headset in these fixtures: the loopback, named by the
    /// address with its colons (`create-loopback-node.lua:17`, `:45`).
    const LOOPBACK_MICROPHONE: &str = "bluez_input.00:11:22:33:44:55";

    /// The headset's SCO source: WirePlumber 0.5's internal node, and WirePlumber 0.4's microphone.
    const SCO_SOURCE: &str = "bluez_input.00_11_22_33_44_55.0";

    // ---------------------------------------------------------------------------------------
    // A minimal JSON reader, test-only.
    //
    // The crate ships no JSON dependency (the audio path must not pull one in), but the fixture
    // has to be read as the server actually wrote it. This handles the subset `pw-dump` emits.
    // ---------------------------------------------------------------------------------------
    #[derive(Debug, Clone, PartialEq)]
    enum Json {
        Null,
        Bool(bool),
        Num(f64),
        Str(String),
        Arr(Vec<Json>),
        Obj(Vec<(String, Json)>),
    }

    impl Json {
        fn get(&self, key: &str) -> Option<&Json> {
            match self {
                Self::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        /// Property dictionaries are string-keyed, but `pw-dump` prints numbers and booleans
        /// unquoted; PipeWire itself hands them to clients as strings, so stringify here.
        fn as_prop(&self) -> Option<String> {
            match self {
                Self::Str(s) => Some(s.clone()),
                Self::Bool(b) => Some(b.to_string()),
                Self::Num(n) if n.fract() == 0.0 => Some(format!("{}", *n as i64)),
                Self::Num(n) => Some(n.to_string()),
                _ => None,
            }
        }
    }

    struct Parser<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> Parser<'a> {
        fn new(text: &'a str) -> Self {
            Self {
                bytes: text.as_bytes(),
                pos: 0,
            }
        }

        fn skip_ws(&mut self) {
            while self
                .bytes
                .get(self.pos)
                .is_some_and(|b| b.is_ascii_whitespace())
            {
                self.pos += 1;
            }
        }

        fn expect(&mut self, byte: u8) {
            self.skip_ws();
            assert_eq!(self.bytes.get(self.pos).copied(), Some(byte));
            self.pos += 1;
        }

        fn peek(&mut self) -> u8 {
            self.skip_ws();
            self.bytes.get(self.pos).copied().unwrap_or(b'\0')
        }

        fn string(&mut self) -> String {
            self.expect(b'"');
            let mut out = String::new();
            while let Some(&b) = self.bytes.get(self.pos) {
                self.pos += 1;
                match b {
                    b'"' => return out,
                    b'\\' => {
                        let esc = self.bytes.get(self.pos).copied().unwrap_or(b'"');
                        self.pos += 1;
                        out.push(match esc {
                            b'n' => '\n',
                            b't' => '\t',
                            b'r' => '\r',
                            other => other as char,
                        });
                    }
                    // The fixture is UTF-8 (it contains Cyrillic device descriptions), so bytes
                    // are collected and re-decoded rather than cast one at a time.
                    _ => {
                        let start = self.pos - 1;
                        let mut end = self.pos;
                        while self
                            .bytes
                            .get(end)
                            .is_some_and(|&b| b != b'"' && b != b'\\')
                        {
                            end += 1;
                        }
                        out.push_str(std::str::from_utf8(&self.bytes[start..end]).unwrap());
                        self.pos = end;
                    }
                }
            }
            out
        }

        fn value(&mut self) -> Json {
            match self.peek() {
                b'"' => Json::Str(self.string()),
                b'{' => {
                    self.expect(b'{');
                    let mut fields = Vec::new();
                    if self.peek() == b'}' {
                        self.expect(b'}');
                        return Json::Obj(fields);
                    }
                    loop {
                        let key = self.string();
                        self.expect(b':');
                        fields.push((key, self.value()));
                        match self.peek() {
                            b',' => self.expect(b','),
                            _ => {
                                self.expect(b'}');
                                break;
                            }
                        }
                    }
                    Json::Obj(fields)
                }
                b'[' => {
                    self.expect(b'[');
                    let mut items = Vec::new();
                    if self.peek() == b']' {
                        self.expect(b']');
                        return Json::Arr(items);
                    }
                    loop {
                        items.push(self.value());
                        match self.peek() {
                            b',' => self.expect(b','),
                            _ => {
                                self.expect(b']');
                                break;
                            }
                        }
                    }
                    Json::Arr(items)
                }
                b't' => {
                    self.pos += 4;
                    Json::Bool(true)
                }
                b'f' => {
                    self.pos += 5;
                    Json::Bool(false)
                }
                b'n' => {
                    self.pos += 4;
                    Json::Null
                }
                _ => {
                    let start = self.pos;
                    while self.bytes.get(self.pos).is_some_and(|&b| {
                        b == b'-'
                            || b == b'+'
                            || b == b'.'
                            || b.is_ascii_digit()
                            || b == b'e'
                            || b == b'E'
                    }) {
                        self.pos += 1;
                    }
                    Json::Num(
                        std::str::from_utf8(&self.bytes[start..self.pos])
                            .unwrap()
                            .parse()
                            .unwrap(),
                    )
                }
            }
        }
    }

    /// One fixture object: `(id, type, props)`, where `props` is the node's own property
    /// dictionary — the same thing the registry hands a client.
    type FixtureObject = (u32, String, Vec<(String, String)>);

    /// Every object in the captured desktop fixture.
    fn fixture_objects() -> Vec<FixtureObject> {
        objects_in(PW_DUMP)
    }

    /// Every object in a `pw-dump`.
    fn objects_in(dump: &str) -> Vec<FixtureObject> {
        let Json::Arr(objects) = Parser::new(dump).value() else {
            panic!("fixture is not a JSON array");
        };
        objects
            .iter()
            .map(|o| {
                let id = match o.get("id") {
                    Some(Json::Num(n)) => *n as u32,
                    _ => panic!("object without an id"),
                };
                let type_ = match o.get("type") {
                    Some(Json::Str(s)) => s.clone(),
                    _ => String::new(),
                };
                let props = o
                    .get("info")
                    .and_then(|info| info.get("props"))
                    .or_else(|| o.get("props"));
                let props = match props {
                    Some(Json::Obj(fields)) => fields
                        .iter()
                        .filter_map(|(k, v)| v.as_prop().map(|v| (k.clone(), v)))
                        .collect(),
                    _ => Vec::new(),
                };
                (id, type_, props)
            })
            .collect()
    }

    /// The captured desktop fixture's `default` metadata entries.
    fn fixture_default_metadata() -> Vec<(String, String)> {
        default_metadata_in(PW_DUMP)
    }

    /// The `default` metadata object's entries, as `(key, value)`, the way `pw-dump` prints them.
    fn default_metadata_in(dump: &str) -> Vec<(String, String)> {
        let Json::Arr(objects) = Parser::new(dump).value() else {
            panic!("fixture is not a JSON array");
        };
        let object = objects
            .iter()
            .find(|o| {
                o.get("type") == Some(&Json::Str("PipeWire:Interface:Metadata".to_owned()))
                    && o.get("props").and_then(|p| p.get("metadata.name"))
                        == Some(&Json::Str("default".to_owned()))
            })
            .expect("the `default` metadata object is in the fixture");
        let Some(Json::Arr(entries)) = object.get("metadata") else {
            panic!("the metadata object carries no entries");
        };
        entries
            .iter()
            .filter_map(|entry| {
                let Some(Json::Str(key)) = entry.get("key") else {
                    return None;
                };
                // `pw-dump` prints the JSON value as a nested object; re-serialise the one shape
                // WirePlumber uses so the production parser sees what the server would send.
                let value = match entry.get("value") {
                    Some(Json::Obj(fields)) => {
                        let name = fields.iter().find(|(k, _)| k == "name")?;
                        let Json::Str(name) = &name.1 else {
                            return None;
                        };
                        format!("{{\"name\": \"{name}\"}}")
                    }
                    Some(other) => other.as_prop()?,
                    None => return None,
                };
                Some((key.clone(), value))
            })
            .collect()
    }

    /// Every device in the captured desktop fixture.
    fn fixture_devices() -> Vec<DeviceInfo> {
        devices_in(PW_DUMP)
    }

    /// Every device in a `pw-dump`, parsed the way the engine parses a registry global.
    fn devices_in(dump: &str) -> Vec<DeviceInfo> {
        objects_in(dump)
            .into_iter()
            .filter(|(_, type_, _)| type_ == "PipeWire:Interface:Node")
            .filter_map(|(id, _, props)| {
                DeviceInfo::from_props(id, &|key: &str| {
                    props
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.as_str())
                })
            })
            .collect()
    }

    fn device(name: &str, channels: u32, direction: DeviceDirection) -> DeviceInfo {
        DeviceInfo {
            object_id: 0,
            object_serial: None,
            card_id: None,
            bluez_address: None,
            name: name.to_owned(),
            description: name.to_owned(),
            nick: name.to_owned(),
            channels,
            rate: None,
            bluez_headset: false,
            bluez: BluezFacts::default(),
            positions: ChannelMap::default_for(channels),
            form_factor: FormFactor::Unknown,
            direction,
        }
    }

    fn sink(name: &str, channels: u32) -> DeviceInfo {
        device(name, channels, DeviceDirection::Output)
    }

    fn source(name: &str, channels: u32) -> DeviceInfo {
        device(name, channels, DeviceDirection::Input)
    }

    /// `choose_device` for outputs, with the argument order the Windows-era tests were written in.
    fn choose_output(
        devices: &[DeviceInfo],
        our_sink: &str,
        current_default: Option<&str>,
        previous_names: &[String],
        memory: &SelectionMemory,
    ) -> Result<Selection, AudioError> {
        choose_device(
            devices,
            DeviceDirection::Output,
            our_sink,
            current_default,
            previous_names,
            memory,
        )
    }

    #[test]
    fn enumeration_keeps_sinks_and_sources_from_a_real_pw_dump() {
        let devices = fixture_devices();
        let listed: Vec<(&str, DeviceDirection)> = devices
            .iter()
            .map(|d| (d.name.as_str(), d.direction))
            .collect();
        assert_eq!(
            listed,
            vec![
                (
                    "alsa_output.usb-3142_fifine_Microphone-00.analog-stereo",
                    DeviceDirection::Output
                ),
                (
                    "alsa_input.usb-3142_fifine_Microphone-00.analog-stereo",
                    DeviceDirection::Input
                ),
                (
                    "alsa_output.pci-0000_05_00.6.analog-stereo",
                    DeviceDirection::Output
                ),
                (
                    "alsa_input.pci-0000_05_00.6.analog-stereo",
                    DeviceDirection::Input
                ),
            ],
            "both Audio/Source nodes are inputs now; only the Stream/Input/Audio node is dropped"
        );
        assert_eq!(direction_of_media_class("Stream/Input/Audio"), None);
        assert_eq!(direction_of_media_class("Stream/Output/Audio"), None);
        assert_eq!(direction_of_media_class("Audio/Device"), None);
        assert_eq!(
            direction_of_media_class("Audio/Source/Virtual"),
            Some(DeviceDirection::Input),
            "a null source or another app's loopback is a signal like any other"
        );
    }

    #[test]
    fn our_own_nodes_are_never_listed_as_devices() {
        for (name, class) in [
            (crate::SINK_NODE_NAME, "Audio/Sink"),
            (crate::SOURCE_NODE_NAME, "Audio/Source"),
            // These two are streams and would be dropped by media.class anyway; the name check is
            // belt and braces against a future property change.
            (crate::OUTPUT_NODE_NAME, "Audio/Sink"),
            (crate::CAPTURE_NODE_NAME, "Audio/Source"),
            // The echo canceller's source is an `Audio/Source` like any microphone: only its name
            // keeps the input lane from being offered its own canceller to record from.
            (crate::AEC_SOURCE_NODE_NAME, "Audio/Source"),
            // Its two capture streams, belt and braces as above.
            (crate::AEC_CAPTURE_NODE_NAME, "Audio/Source"),
            (crate::AEC_MONITOR_NODE_NAME, "Audio/Source"),
        ] {
            let props = [("media.class", class), ("node.name", name)];
            let parsed = DeviceInfo::from_props(1, &|key: &str| {
                props.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
            });
            assert_eq!(parsed, None, "{name} must never be offered as a device");
        }
    }

    #[test]
    fn a_real_alsa_sink_parses_into_the_fields_the_picker_needs() {
        let devices = fixture_devices();
        let alc = devices
            .iter()
            .find(|s| s.name == "alsa_output.pci-0000_05_00.6.analog-stereo")
            .expect("the internal analogue sink is in the fixture");

        assert_eq!(alc.object_id, 57);
        assert_eq!(alc.object_serial, Some(57));
        assert_eq!(
            alc.description,
            "Ryzen HD Audio Controller Аналоговый стерео"
        );
        assert_eq!(alc.nick, "ALC294 Analog");
        assert_eq!(alc.channels, 2);
        assert_eq!(alc.positions.to_property_value(), "FL,FR");
        assert_eq!(alc.direction, DeviceDirection::Output);
        // No `device.form-factor` on this node; the icon name is what identifies it.
        assert_eq!(alc.form_factor, FormFactor::Speakers);
        assert!(!alc.is_mono());

        // A sink whose channel count never reached us is not mono — it is unknown, and runs as
        // stereo until the node's info says what it is.
        let unknown = DeviceInfo {
            channels: 0,
            ..alc.clone()
        };
        assert!(
            !unknown.is_mono(),
            "an unknown channel count is not a known one"
        );
        assert!(unknown.channels_unknown());
        assert_eq!(
            unknown.clamped_channels(),
            MIN_CHANNELS,
            "unknown falls back to stereo"
        );

        let real_mono = DeviceInfo {
            channels: 1,
            ..alc.clone()
        };
        assert!(real_mono.is_mono(), "a device that says it is mono is");
        assert_eq!(
            real_mono.clamped_channels(),
            MIN_CHANNELS,
            "and it runs a stereo pair that its adapter down-mixes"
        );
        assert_eq!(alc.clamped_channels(), 2);
    }

    #[test]
    fn a_real_alsa_source_parses_as_an_input() {
        let devices = fixture_devices();
        let mic = devices
            .iter()
            .find(|s| s.name == "alsa_input.usb-3142_fifine_Microphone-00.analog-stereo")
            .expect("the USB microphone's source is in the fixture");

        assert_eq!(mic.object_id, 56);
        assert_eq!(mic.direction, DeviceDirection::Input);
        // The same description as the sink the same USB device exposes — which is exactly why the
        // GUI needs the direction to tell them apart.
        assert_eq!(mic.description, "fifine Microphone Аналоговый стерео");
        assert_eq!(mic.channels, 2);
        assert_eq!(mic.positions.to_property_value(), "FL,FR");
        assert_eq!(
            mic.form_factor,
            FormFactor::Microphone,
            "the card's audio-card icon must not make a microphone look like speakers"
        );

        let device = mic.to_audio_device(Some(mic.name.as_str()));
        assert_eq!(device.direction, DeviceDirection::Input);
        assert!(device.is_default);
    }

    #[test]
    fn a_mono_microphone_is_an_input_that_runs_as_stereo() {
        let mono = source("mono-mic", 1);
        assert!(mono.is_mono(), "it does say it is mono");
        assert_eq!(
            mono.clamped_channels(),
            MIN_CHANNELS,
            "the capture stream declares stereo and PipeWire's adapter up-mixes"
        );
        assert_eq!(
            mono.positions
                .resized(mono.clamped_channels())
                .to_property_value(),
            "FL,FR"
        );
    }

    #[test]
    fn a_mono_sink_is_an_output_that_runs_as_stereo_like_a_mono_microphone() {
        let mono = sink("mono-headset", 1);
        assert!(mono.is_mono());
        assert_eq!(mono.positions.to_property_value(), "MONO");
        assert_eq!(
            mono.clamped_channels(),
            MIN_CHANNELS,
            "NODE 1 stays stereo; the playback stream's adapter down-mixes into the headset"
        );
        assert_eq!(
            mono.positions
                .resized(mono.clamped_channels())
                .to_property_value(),
            "FL,FR",
            "the pair gets the stereo layout, not a two-slot MONO"
        );

        // Whether the headset's info has arrived yet or not, the pair it wants is the same one, so
        // the info arriving is never a reason to rebuild a pair that is already playing.
        let unknown = sink("mono-headset", 0);
        assert_eq!(unknown.clamped_channels(), mono.clamped_channels());
        assert_eq!(
            unknown.positions.resized(unknown.clamped_channels()),
            mono.positions.resized(mono.clamped_channels())
        );
    }

    #[test]
    fn the_default_sink_is_read_out_of_the_default_metadata_object() {
        let objects = fixture_objects();
        let (_, _, props) = objects
            .iter()
            .find(|(_, type_, props)| {
                type_ == "PipeWire:Interface:Metadata"
                    && props
                        .iter()
                        .any(|(k, v)| k == "metadata.name" && v == "default")
            })
            .expect("the `default` metadata object is in the fixture");
        assert_eq!(
            props
                .iter()
                .find(|(k, _)| k == "metadata.name")
                .map(|(_, v)| v.as_str()),
            Some("default")
        );

        // The value shape, as WirePlumber writes it.
        let value = r#"{"name": "alsa_output.pci-0000_05_00.6.analog-stereo"}"#;
        assert_eq!(
            parse_default_node_name(value).as_deref(),
            Some("alsa_output.pci-0000_05_00.6.analog-stereo")
        );
        assert_eq!(
            default_node_value("fxsound_sink"),
            r#"{"name":"fxsound_sink"}"#
        );
        assert_eq!(parse_default_node_name("{}"), None);
        assert_eq!(parse_default_node_name(r#"{"name":""}"#), None);

        assert_eq!(default_key(DeviceDirection::Output), "default.audio.sink");
        assert_eq!(
            configured_default_key(DeviceDirection::Output),
            "default.configured.audio.sink"
        );
    }

    #[test]
    fn the_default_source_is_read_out_of_the_same_metadata_object() {
        let entries = fixture_default_metadata();
        let value = |key: &str| {
            entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };

        // The fixture was captured with the USB microphone as the default source and no
        // configured source at all — the normal state of a machine where nobody ever picked one.
        assert_eq!(
            value(default_key(DeviceDirection::Input))
                .and_then(parse_default_node_name)
                .as_deref(),
            Some("alsa_input.usb-3142_fifine_Microphone-00.analog-stereo")
        );
        assert_eq!(value(configured_default_key(DeviceDirection::Input)), None);
        assert_eq!(
            value(default_key(DeviceDirection::Output))
                .and_then(parse_default_node_name)
                .as_deref(),
            Some("alsa_output.pci-0000_05_00.6.analog-stereo")
        );

        assert_eq!(default_key(DeviceDirection::Input), "default.audio.source");
        assert_eq!(
            configured_default_key(DeviceDirection::Input),
            "default.configured.audio.source"
        );
        assert_eq!(
            default_node_value("fxsound_source"),
            r#"{"name":"fxsound_source"}"#
        );
    }

    #[test]
    fn a_device_the_pw_dump_marks_as_default_reports_it_to_the_gui_per_direction() {
        let devices = fixture_devices();
        let default_sink = "alsa_output.pci-0000_05_00.6.analog-stereo";
        let default_source = "alsa_input.usb-3142_fifine_Microphone-00.analog-stereo";
        let published: Vec<AudioDevice> = devices
            .iter()
            .map(|d| {
                d.to_audio_device(match d.direction {
                    DeviceDirection::Output => Some(default_sink),
                    DeviceDirection::Input => Some(default_source),
                })
            })
            .collect();
        assert_eq!(
            published.iter().filter(|d| d.is_default).count(),
            2,
            "one default per direction"
        );
        assert!(
            published
                .iter()
                .any(|d| d.id == 57 && d.is_default && d.direction == DeviceDirection::Output)
        );
        assert!(
            published
                .iter()
                .any(|d| d.id == 56 && d.is_default && d.direction == DeviceDirection::Input)
        );
        // The sink half of the USB microphone is not the default of anything.
        assert!(published.iter().any(|d| d.id == 55 && !d.is_default));
    }

    #[test]
    fn channel_positions_parse_both_spellings_pipewire_uses() {
        let bracketed = ChannelMap::parse("[ FL, FR ]").expect("node property form");
        let bare = ChannelMap::parse("FL,FR").expect("settable property form");
        assert_eq!(bracketed, bare);
        assert_eq!(bracketed.len(), 2);
        assert_eq!(bracketed.ids()[0], libspa::sys::SPA_AUDIO_CHANNEL_FL);
        assert_eq!(bracketed.ids()[1], libspa::sys::SPA_AUDIO_CHANNEL_FR);
        assert_eq!(bracketed.to_property_value(), "FL,FR");

        let surround = ChannelMap::parse("[ FL, FR, FC, LFE, RL, RR ]").expect("5.1");
        assert_eq!(surround.to_property_value(), "FL,FR,FC,LFE,RL,RR");
        assert_eq!(surround, ChannelMap::default_for(6));

        assert_eq!(ChannelMap::parse("FL,NOPE"), None);
        assert_eq!(ChannelMap::parse(""), None);
    }

    #[test]
    fn the_spa_position_array_is_padded_to_the_sixty_four_slots_libspa_wants() {
        let map = ChannelMap::default_for(2);
        let position = map.to_spa_position();
        assert_eq!(position.len(), libspa::param::audio::MAX_CHANNELS);
        assert_eq!(position[0], libspa::sys::SPA_AUDIO_CHANNEL_FL);
        assert_eq!(position[1], libspa::sys::SPA_AUDIO_CHANNEL_FR);
        assert!(position[2..].iter().all(|&id| id == 0));
        // `AudioInfoRaw::set_position` only clears UNPOSITIONED when slot 0 is non-zero
        // (libspa raw.rs:70-74), and SPA_AUDIO_CHANNEL_UNKNOWN is 0.
        assert_ne!(position[0], libspa::sys::SPA_AUDIO_CHANNEL_UNKNOWN);
    }

    #[test]
    fn form_factors_map_the_way_the_windows_icon_table_expects() {
        let probe = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            FormFactor::from_props(&|key: &str| {
                owned
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            })
        };

        assert_eq!(
            probe(&[("device.form-factor", "internal")]),
            FormFactor::Speakers
        );
        assert_eq!(
            probe(&[("device.form-factor", "headphone")]),
            FormFactor::Headphones
        );
        assert_eq!(
            probe(&[("device.form-factor", "headset")]),
            FormFactor::Headset
        );
        assert_eq!(
            probe(&[("device.form-factor", "hands-free")]),
            FormFactor::Handset
        );
        assert_eq!(probe(&[("device.form-factor", "tv")]), FormFactor::Hdmi);
        assert_eq!(
            probe(&[("node.name", "alsa_output.pci-0000_01_00.1.hdmi-stereo")]),
            FormFactor::Hdmi,
            "HDMI is recognised from the node name even with no form-factor property"
        );
        assert_eq!(
            probe(&[("device.profile.name", "iec958-stereo")]),
            FormFactor::Spdif
        );
        assert_eq!(
            probe(&[
                ("device.bus", "bluetooth"),
                ("api.bluez5.profile", "a2dp-sink")
            ]),
            FormFactor::Headphones
        );
        assert_eq!(
            probe(&[
                ("device.bus", "bluetooth"),
                ("api.bluez5.profile", "headset-head-unit")
            ]),
            FormFactor::Headset
        );
        assert_eq!(probe(&[("device.api", "raop")]), FormFactor::NetworkDevice);
        assert_eq!(
            probe(&[("device.icon-name", "audio-input-microphone")]),
            FormFactor::Microphone
        );
        assert_eq!(probe(&[]), FormFactor::Unknown);
        assert_eq!(FormFactor::Unknown.key(), "unknown");
    }

    /// The capture stream runs at 48 kHz whatever the microphone does, so the negotiated format
    /// says nothing about the bandwidth in the signal; the properties are where the truth is.
    #[test]
    fn a_bluetooth_headset_profile_has_a_sixteen_kilohertz_native_rate() {
        let parse = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            DeviceInfo::from_props(1, &|key: &str| {
                owned
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            })
            .expect("a named source or sink is a device")
        };

        let headset = parse(&[
            ("media.class", "Audio/Source"),
            ("node.name", "bluez_input.00_11_22_33_44_55.0"),
            ("device.bus", "bluetooth"),
            ("api.bluez5.profile", "headset-head-unit"),
        ]);
        assert!(headset.bluez_headset);
        assert_eq!(headset.form_factor, FormFactor::Headset);
        assert_eq!(
            headset.rate, None,
            "nothing published, as on a real bluez5 node"
        );
        assert_eq!(headset.native_rate(), Some(16_000.0));

        // The same device on its A2DP profile is a pair of headphones with no microphone, and
        // nothing about its rate is known.
        let a2dp = parse(&[
            ("media.class", "Audio/Sink"),
            ("node.name", "bluez_output.00_11_22_33_44_55.1"),
            ("device.bus", "bluetooth"),
            ("api.bluez5.profile", "a2dp-sink"),
        ]);
        assert!(!a2dp.bluez_headset);
        assert_eq!(a2dp.form_factor, FormFactor::Headphones);
        assert_eq!(a2dp.native_rate(), None);

        // A published `audio.rate` is the device's own word and wins over the profile's figure.
        let published = parse(&[
            ("media.class", "Audio/Source"),
            (
                "node.name",
                "alsa_input.usb-0d8c_USB_Audio-00.mono-fallback",
            ),
            ("audio.rate", "44100"),
        ]);
        assert_eq!(published.native_rate(), Some(44_100.0));
        let spoken_for = parse(&[
            ("media.class", "Audio/Source"),
            ("node.name", "bluez_input.00_11_22_33_44_55.0"),
            ("api.bluez5.profile", "headset-audio-gateway"),
            ("audio.rate", "8000"),
        ]);
        assert!(spoken_for.bluez_headset);
        assert_eq!(spoken_for.native_rate(), Some(8_000.0));

        // An ALSA microphone that says nothing leaves the stream rate as all there is to know.
        let mic = parse(&[
            ("media.class", "Audio/Source"),
            ("node.name", "alsa_input.pci-0000_05_00.6.analog-stereo"),
            ("device.icon-name", "audio-card"),
        ]);
        assert!(!mic.bluez_headset);
        assert_eq!(mic.native_rate(), None);
    }

    #[test]
    fn a_bluetooth_headset_in_its_call_profile_parses_as_a_one_channel_output() {
        let devices = devices_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let outputs: Vec<&str> = devices
            .iter()
            .filter(|d| d.direction == DeviceDirection::Output)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(
            outputs,
            [LAPTOP_SPEAKERS, HEADSET_SINK],
            "the headset is listed as an output, not dropped for being mono"
        );

        let headset = devices
            .iter()
            .find(|d| d.name == HEADSET_SINK)
            .expect("the headset's sink is in the fixture");
        assert_eq!(headset.object_id, 70);
        assert_eq!(headset.description, "Test Headset");
        assert_eq!(headset.channels, 1);
        assert!(headset.is_mono());
        assert_eq!(headset.positions.to_property_value(), "MONO");
        assert!(headset.bluez_headset);
        assert_eq!(headset.form_factor, FormFactor::Headset);
        assert_eq!(
            headset.native_rate(),
            Some(16_000.0),
            "mSBC, the call profile's wide band"
        );

        // What the output lane builds for it: a stereo NODE 1, which the playback stream's adapter
        // down-mixes into the one SCO channel.
        assert_eq!(headset.clamped_channels(), 2);
        assert_eq!(
            headset
                .positions
                .resized(headset.clamped_channels())
                .to_property_value(),
            "FL,FR"
        );

        // And the GUI is told it is the default, which on this graph it is.
        let entries = default_metadata_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let default_sink = entries
            .iter()
            .find(|(k, _)| k == default_key(DeviceDirection::Output))
            .and_then(|(_, v)| parse_default_node_name(v));
        assert_eq!(default_sink.as_deref(), Some(HEADSET_SINK));
        let shown = headset.to_audio_device(default_sink.as_deref());
        assert!(shown.is_default);
        assert_eq!(shown.direction, DeviceDirection::Output);
        assert_eq!(shown.form_factor, "headset");
    }

    #[test]
    fn the_rules_take_a_headset_in_its_call_profile_as_they_would_a_stereo_output() {
        let devices = devices_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let output =
            |current_default: Option<&str>, previous: &[String], memory: &SelectionMemory| {
                choose_output(&devices, "fxsound_sink", current_default, previous, memory)
                    .expect("an output target")
            };

        // 2 — the first run adopts the session default, which is the headset.
        let first = output(Some(HEADSET_SINK), &[], &SelectionMemory::default());
        assert_eq!(first.target, HEADSET_SINK);

        // 4 — the user picked it while the speakers are the session default.
        let picked = output(
            Some(LAPTOP_SPEAKERS),
            &[],
            &SelectionMemory {
                most_recent_default: LAPTOP_SPEAKERS.into(),
                user_selected: HEADSET_SINK.into(),
                ..SelectionMemory::default()
            },
        );
        assert_eq!(picked.target, HEADSET_SINK);

        // 5 — it connected, straight into its call profile, while FxSound was the default.
        let speakers_only = [LAPTOP_SPEAKERS.to_owned()];
        let at_our_default = SelectionMemory {
            most_recent_default: LAPTOP_SPEAKERS.into(),
            most_recent_playback: LAPTOP_SPEAKERS.into(),
            ..SelectionMemory::default()
        };
        let connected = output(Some("fxsound_sink"), &speakers_only, &at_our_default);
        assert_eq!(connected.target, HEADSET_SINK);
        assert!(connected.write_previous_default);

        // 6 — the session default moved to it.
        let followed = output(Some(HEADSET_SINK), &[], &at_our_default);
        assert_eq!(followed.target, HEADSET_SINK);
    }

    #[test]
    fn a_headset_that_comes_back_mono_under_the_same_name_keeps_the_output_lane() {
        // Something starts recording from the headset: WirePlumber switches it from A2DP to its
        // call profile, which removes `bluez_output.….1` and adds it back under the same name with
        // one channel. The previous enumeration is the A2DP graph — the same two names.
        let devices = devices_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let previous = vec![LAPTOP_SPEAKERS.to_owned(), HEADSET_SINK.to_owned()];
        let memory = SelectionMemory {
            original_default: LAPTOP_SPEAKERS.into(),
            most_recent_default: HEADSET_SINK.into(),
            prior_default: LAPTOP_SPEAKERS.into(),
            most_recent_playback: HEADSET_SINK.into(),
            user_selected: String::new(),
        };
        let selection = choose_output(
            &devices,
            "fxsound_sink",
            Some("fxsound_sink"),
            &previous,
            &memory,
        )
        .expect("the headset is still a target; Windows answered -58 here");
        assert_eq!(
            selection.target, HEADSET_SINK,
            "the music stays in the headset for the call instead of moving to the speakers"
        );
        assert!(
            !selection.write_previous_default,
            "a device back under its own name within the same run of the rules is not a new one \
             (rule 7, not rule 5)"
        );

        // Nor does its new shape ask for a different pair: the stereo pair built for A2DP is the
        // pair the call profile wants, so the lane can keep the nodes it has.
        let headset = devices
            .iter()
            .find(|d| d.name == HEADSET_SINK)
            .expect("the headset's sink is in the fixture");
        let on_a2dp = DeviceInfo {
            channels: 2,
            positions: ChannelMap::default_for(2),
            bluez_headset: false,
            ..headset.clone()
        };
        assert_eq!(on_a2dp.clamped_channels(), headset.clamped_channels());
        assert_eq!(
            on_a2dp.positions.resized(on_a2dp.clamped_channels()),
            headset.positions.resized(headset.clamped_channels())
        );
    }

    /// Every card in a `pw-dump`, parsed the way the engine parses a `Device` object.
    fn cards_in(dump: &str) -> Vec<Card> {
        objects_in(dump)
            .into_iter()
            .filter(|(_, type_, _)| type_ == "PipeWire:Interface:Device")
            .map(|(id, _, props)| {
                Card::from_props(id, &|key: &str| {
                    props
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.as_str())
                })
            })
            .collect()
    }

    /// The headset's sink in the Bluetooth fixture.
    fn headset_sink() -> DeviceInfo {
        devices_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP)
            .into_iter()
            .find(|d| d.name == HEADSET_SINK)
            .expect("the headset's sink is in the fixture")
    }

    #[test]
    fn a_node_says_which_card_it_belongs_to_and_a_bluetooth_node_its_address() {
        let devices = devices_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let headset = headset_sink();
        assert_eq!(headset.card_id, Some(60));
        assert_eq!(headset.bluez_address.as_deref(), Some("00:11:22:33:44:55"));
        assert_eq!(headset.object_serial, Some(70));

        let speakers = devices
            .iter()
            .find(|d| d.name == LAPTOP_SPEAKERS)
            .expect("the laptop's speakers are in the fixture");
        assert_eq!(speakers.card_id, Some(45));
        assert_eq!(speakers.bluez_address, None, "an ALSA node has no address");

        // A virtual sink belongs to no card.
        let virtual_sink = DeviceInfo::from_props(90, &|key: &str| match key {
            "media.class" => Some(SINK_MEDIA_CLASS),
            "node.name" => Some("easyeffects_sink"),
            "object.serial" => Some("900"),
            _ => None,
        })
        .expect("a virtual sink is still a sink");
        assert_eq!(virtual_sink.object_serial, Some(900));
        assert_eq!(virtual_sink.card_id, None);
        assert_eq!(virtual_sink.bluez_address, None);
    }

    #[test]
    fn a_card_reads_its_bluetooth_address_and_an_empty_address_is_no_address() {
        let cards = cards_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        assert_eq!(
            cards,
            [Card {
                object_id: 60,
                bluez_address: Some("00:11:22:33:44:55".to_owned()),
                bluetooth: true,
            }],
            "the fixture's one card is the headset's"
        );
        let blank = Card::from_props(7, &|key: &str| (key == "api.bluez5.address").then_some(""));
        assert_eq!(blank.bluez_address, None);
    }

    #[test]
    fn a_card_owns_a_node_by_its_id_or_its_address_and_never_by_two_unknowns() {
        let headset = Card {
            object_id: 60,
            bluez_address: Some("00:11:22:33:44:55".to_owned()),
            bluetooth: true,
        };
        assert!(headset.owns(Some(60), None), "by `device.id`");
        assert!(
            headset.owns(None, Some("00:11:22:33:44:55")),
            "by `api.bluez5.address`"
        );
        assert!(
            headset.owns(Some(60), Some("66:77:88:99:AA:BB")),
            "either is enough"
        );
        assert!(!headset.owns(Some(61), Some("66:77:88:99:AA:BB")));
        assert!(!headset.owns(None, None));

        let alsa = Card {
            object_id: 45,
            bluez_address: None,
            bluetooth: false,
        };
        assert!(alsa.owns(Some(45), None));
        assert!(
            !alsa.owns(None, None),
            "a card with no address and a node with none are not the same card"
        );
    }

    #[test]
    fn a_headsets_sink_that_went_while_its_card_stayed_still_has_its_card() {
        let cards = cards_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        assert!(
            headset_sink().card_present(&cards),
            "the call profile's sink went; the headset did not"
        );
        assert!(
            !headset_sink().card_present(&[]),
            "with the headset's card gone too, the headset is gone"
        );
    }

    #[test]
    fn a_bluetooth_node_without_a_device_id_is_matched_to_its_card_by_address() {
        let cards = cards_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let unnumbered = DeviceInfo {
            card_id: None,
            ..headset_sink()
        };
        assert!(unnumbered.card_present(&cards));

        let another_headset = DeviceInfo {
            card_id: None,
            bluez_address: Some("66:77:88:99:AA:BB".to_owned()),
            ..headset_sink()
        };
        assert!(
            !another_headset.card_present(&cards),
            "another headset's card is not this one's"
        );
    }

    #[test]
    fn a_node_on_no_card_or_on_a_card_that_is_not_there_has_no_card_to_wait_for() {
        let cards = cards_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let speakers = devices_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP)
            .into_iter()
            .find(|d| d.name == LAPTOP_SPEAKERS)
            .expect("the laptop's speakers are in the fixture");
        assert!(
            !speakers.card_present(&cards),
            "the fixture leaves the ALSA card out, so the speakers' card is not there"
        );
        // Neither an id nor an address: nothing matches, not even a card with no address of its
        // own — two unknowns are not the same card.
        let virtual_sink = sink("easyeffects_sink", 2);
        let cards_without_addresses = [Card {
            object_id: 0,
            bluez_address: None,
            bluetooth: false,
        }];
        assert!(!virtual_sink.card_present(&cards_without_addresses));
        assert!(!virtual_sink.card_present(&cards));
    }

    #[test]
    fn a_headset_whose_removal_had_a_run_of_its_own_comes_back_as_new_to_the_same_target() {
        // The same profile switch, slow enough to straddle two supervisor ticks: one run of the
        // rules sees the headset gone, the next sees it back. The engine replaces `previous_names`
        // after every run, so the second one no longer remembers the name.
        let devices = devices_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let without_headset: Vec<DeviceInfo> = devices
            .iter()
            .filter(|d| d.name != HEADSET_SINK)
            .cloned()
            .collect();
        let before = SelectionMemory {
            original_default: LAPTOP_SPEAKERS.into(),
            most_recent_default: HEADSET_SINK.into(),
            prior_default: LAPTOP_SPEAKERS.into(),
            most_recent_playback: HEADSET_SINK.into(),
            user_selected: String::new(),
        };
        let on_a2dp = vec![LAPTOP_SPEAKERS.to_owned(), HEADSET_SINK.to_owned()];

        // The removal's run: the speakers are all there is (rule 3), and nothing new was added.
        let mut memory = before.clone();
        let gone = choose_output(
            &without_headset,
            "fxsound_sink",
            Some("fxsound_sink"),
            &on_a2dp,
            &memory,
        )
        .expect("the speakers are still a target");
        assert_eq!(gone.target, LAPTOP_SPEAKERS);
        assert!(!gone.write_previous_default, "rule 3 writes no default");
        commit(&mut memory, &gone);
        assert_eq!(memory.most_recent_playback, LAPTOP_SPEAKERS);

        // The re-add's run: the headset is missing from the names the last run saw, so rule 5
        // takes it as new — and picks the device rule 7 would have picked in a single run.
        let speakers_only = vec![LAPTOP_SPEAKERS.to_owned()];
        let back = choose_output(
            &devices,
            "fxsound_sink",
            Some("fxsound_sink"),
            &speakers_only,
            &memory,
        )
        .expect("the headset is a target again");
        assert_eq!(
            back.target, HEADSET_SINK,
            "the music goes back to the headset, the same device either way"
        );
        assert!(
            back.write_previous_default,
            "a name the last run did not see is a new device to rule 5"
        );
        commit(&mut memory, &back);

        // The single-run path leaves the memory it had, the playback slot rewritten in place.
        let mut in_one_run = before.clone();
        let kept = choose_output(
            &devices,
            "fxsound_sink",
            Some("fxsound_sink"),
            &on_a2dp,
            &in_one_run,
        )
        .expect("the headset is still a target");
        assert_eq!(kept.target, HEADSET_SINK);
        commit(&mut in_one_run, &kept);

        // Rule 5 wrote the default slots again, and here they already held what it wrote: the
        // headset as the most recent default, the speakers played to in between as the prior one.
        assert_eq!(
            memory, in_one_run,
            "two runs or one, the headset ends up remembered the same way"
        );
        assert_eq!(in_one_run, before);
    }

    #[test]
    fn no_sinks_at_all_is_the_no_output_devices_state() {
        let memory = SelectionMemory::default();
        assert_eq!(
            choose_output(&[], "fxsound_sink", None, &[], &memory),
            Err(AudioError::NoOutputDevices)
        );
        // A graph containing only our own sink counts as empty too.
        let only_us = [sink("fxsound_sink", 2)];
        assert_eq!(
            choose_output(&only_us, "fxsound_sink", Some("fxsound_sink"), &[], &memory),
            Err(AudioError::NoOutputDevices)
        );
        // And so does a graph with microphones but no speakers.
        let only_mics = [source("mic", 2)];
        assert_eq!(
            choose_output(&only_mics, "fxsound_sink", None, &[], &memory),
            Err(AudioError::NoOutputDevices)
        );
    }

    #[test]
    fn no_sources_at_all_is_the_no_input_devices_state() {
        let memory = SelectionMemory::default();
        let only_speakers = [sink("speakers", 2), source("fxsound_source", 2)];
        assert_eq!(
            choose_device(
                &only_speakers,
                DeviceDirection::Input,
                "fxsound_source",
                Some("fxsound_source"),
                &[],
                &memory
            ),
            Err(AudioError::NoInputDevices),
            "a sink is never an input candidate, and neither is our own source"
        );
        assert_eq!(
            AudioError::NoInputDevices.to_string(),
            "no input devices present"
        );
    }

    #[test]
    fn the_rules_only_ever_see_devices_of_the_requested_direction() {
        // The USB microphone's sink and source share a description; the rules must never let a
        // sink be chosen as a microphone or a source as speakers, whatever the memory says.
        let devices = [
            sink("usb-sink", 2),
            source("usb-source", 2),
            sink("speakers", 2),
            source("internal-mic", 2),
        ];
        let memory = SelectionMemory {
            most_recent_default: "speakers".into(),
            user_selected: "usb-sink".into(),
            ..SelectionMemory::default()
        };
        let input = choose_device(
            &devices,
            DeviceDirection::Input,
            "fxsound_source",
            Some("internal-mic"),
            &[],
            &memory,
        )
        .expect("an input target");
        assert_eq!(
            input.target, "internal-mic",
            "the remembered *sink* is ignored for the input direction; rule 6 picks the default \
             source"
        );

        let output = choose_output(&devices, "fxsound_sink", Some("speakers"), &[], &memory)
            .expect("an output target");
        assert_eq!(output.target, "usb-sink", "rule 4 for outputs, as before");
    }

    #[test]
    fn a_mono_device_is_accepted_in_either_direction() {
        let memory = SelectionMemory {
            most_recent_default: "mono".into(),
            user_selected: "mono".into(),
            ..SelectionMemory::default()
        };
        let sources = [source("mono", 1), source("stereo", 2)];
        let sinks = [sink("mono", 1), sink("stereo", 2)];
        for (devices, direction, ours) in [
            (&sources, DeviceDirection::Input, "fxsound_source"),
            (&sinks, DeviceDirection::Output, "fxsound_sink"),
        ] {
            let selection = choose_device(devices, direction, ours, Some("mono"), &[], &memory)
                .expect(
                    "a mono device is a perfectly good target; on Windows the output side was -58",
                );
            assert_eq!(selection.target, "mono", "{direction:?}");

            // Rule 5, too: a newly plugged mono device is taken, whichever way it faces.
            let previous = vec!["stereo".to_owned()];
            let fresh = SelectionMemory {
                most_recent_default: "stereo".into(),
                most_recent_playback: "stereo".into(),
                ..SelectionMemory::default()
            };
            let plugged = choose_device(devices, direction, ours, Some(ours), &previous, &fresh)
                .expect("a target");
            assert_eq!(plugged.target, "mono", "{direction:?}");
            assert!(plugged.write_previous_default);
        }
    }

    #[test]
    fn the_first_run_adopts_the_session_default_and_remembers_it() {
        let sinks = [sink("speakers", 2), sink("usb-dac", 2)];
        let mut memory = SelectionMemory::default();
        let selection =
            choose_output(&sinks, "fxsound_sink", Some("usb-dac"), &[], &memory).expect("a target");
        assert_eq!(selection.target, "usb-dac");
        assert!(!selection.write_previous_default);

        commit(&mut memory, &selection);
        assert_eq!(memory.original_default, "usb-dac");
        assert_eq!(memory.most_recent_default, "usb-dac");
        assert_eq!(memory.most_recent_playback, "usb-dac");
        assert_eq!(memory.prior_default, "");
    }

    #[test]
    fn an_explicit_choice_on_the_first_run_beats_the_session_default() {
        // The first rules run of the input direction is *caused* by the user picking a microphone;
        // rule 2 must not override that pick with whatever WirePlumber had as the default source.
        let sources = [source("fifine-mic", 2), source("internal-mic", 2)];
        let memory = SelectionMemory {
            user_selected: "internal-mic".into(),
            ..SelectionMemory::default()
        };
        let selection = choose_device(
            &sources,
            DeviceDirection::Input,
            "fxsound_source",
            Some("fifine-mic"),
            &[],
            &memory,
        )
        .expect("a target");
        assert_eq!(selection.target, "internal-mic");
        assert!(!selection.write_previous_default);

        // The same holds for outputs, and a pick that is *not* present still lets rule 2 run.
        let sinks = [sink("speakers", 2), sink("usb-dac", 2)];
        let selection =
            choose_output(&sinks, "fxsound_sink", Some("usb-dac"), &[], &memory).expect("a target");
        assert_eq!(
            selection.target, "usb-dac",
            "internal-mic is not a sink; rule 2 applies"
        );
    }

    #[test]
    fn a_single_real_sink_wins_regardless_of_what_is_remembered() {
        let sinks = [sink("only-one", 2)];
        let memory = SelectionMemory {
            most_recent_default: "gone".into(),
            most_recent_playback: "gone".into(),
            ..SelectionMemory::default()
        };
        let selection = choose_output(&sinks, "fxsound_sink", Some("fxsound_sink"), &[], &memory)
            .expect("a target");
        assert_eq!(selection.target, "only-one");
    }

    #[test]
    fn an_explicitly_selected_output_beats_the_session_default() {
        let sinks = [sink("speakers", 2), sink("usb-dac", 2)];
        let memory = SelectionMemory {
            most_recent_default: "speakers".into(),
            user_selected: "usb-dac".into(),
            ..SelectionMemory::default()
        };
        let selection = choose_output(&sinks, "fxsound_sink", Some("speakers"), &[], &memory)
            .expect("a target");
        assert_eq!(selection.target, "usb-dac");
        assert!(!selection.write_previous_default);
    }

    #[test]
    fn a_newly_appeared_sink_is_taken_and_re_dates_the_remembered_defaults() {
        let sinks = [sink("speakers", 2), sink("usb-dac", 2)];
        let previous = vec!["speakers".to_owned()];
        let mut memory = SelectionMemory {
            most_recent_default: "speakers".into(),
            most_recent_playback: "speakers".into(),
            ..SelectionMemory::default()
        };
        let selection = choose_output(
            &sinks,
            "fxsound_sink",
            Some("fxsound_sink"),
            &previous,
            &memory,
        )
        .expect("a target");
        assert_eq!(selection.target, "usb-dac");
        assert!(selection.write_previous_default);

        commit(&mut memory, &selection);
        assert_eq!(memory.most_recent_playback, "usb-dac");
        assert_eq!(memory.most_recent_default, "usb-dac");
        assert_eq!(
            memory.prior_default, "speakers",
            "the device we were rendering to becomes the prior default"
        );
    }

    #[test]
    fn a_newly_appeared_mono_sink_is_taken_like_any_other() {
        // Windows skipped it here (`:202-210`); a Bluetooth headset that connects straight into
        // its call profile is exactly the device the user just asked for.
        let sinks = [sink("speakers", 2), sink("mono-bt", 1)];
        let previous = vec!["speakers".to_owned()];
        let memory = SelectionMemory {
            most_recent_default: "speakers".into(),
            most_recent_playback: "speakers".into(),
            ..SelectionMemory::default()
        };
        let selection = choose_output(
            &sinks,
            "fxsound_sink",
            Some("fxsound_sink"),
            &previous,
            &memory,
        )
        .expect("a target");
        assert_eq!(selection.target, "mono-bt");
        assert!(selection.write_previous_default);
    }

    #[test]
    fn a_device_that_arrives_as_another_leaves_is_taken_though_the_count_did_not_grow() {
        // One batch of registry events: the old DAC's node went, the new one came. Windows only
        // looked for a new device when the count had grown, so it never saw this one.
        let sinks = [sink("speakers", 2), sink("new-dac", 2)];
        let previous = vec!["speakers".to_owned(), "old-dac".to_owned()];
        let mut memory = SelectionMemory {
            most_recent_default: "old-dac".into(),
            most_recent_playback: "old-dac".into(),
            prior_default: "speakers".into(),
            ..SelectionMemory::default()
        };
        let selection = choose_output(
            &sinks,
            "fxsound_sink",
            Some("fxsound_sink"),
            &previous,
            &memory,
        )
        .expect("a target");
        assert_eq!(
            selection.target, "new-dac",
            "rule 7 would have fallen back to the speakers"
        );
        assert!(
            selection.write_previous_default,
            "a device taken by rule 5 re-dates the remembered defaults, whatever the count did"
        );

        commit(&mut memory, &selection);
        assert_eq!(memory.most_recent_playback, "new-dac");
        assert_eq!(memory.most_recent_default, "new-dac");
        assert_eq!(memory.prior_default, "old-dac");

        // The same holds for microphones: the rules are the same rules.
        let sources = [source("internal-mic", 2), source("new-headset", 1)];
        let previous = vec!["internal-mic".to_owned(), "old-headset".to_owned()];
        let memory = SelectionMemory {
            most_recent_default: "old-headset".into(),
            most_recent_playback: "old-headset".into(),
            ..SelectionMemory::default()
        };
        let selection = choose_device(
            &sources,
            DeviceDirection::Input,
            "fxsound_source",
            Some("fxsound_source"),
            &previous,
            &memory,
        )
        .expect("a target");
        assert_eq!(selection.target, "new-headset");
    }

    #[test]
    fn a_device_that_arrives_while_two_leave_is_taken_though_the_count_shrank() {
        // A dock unplugged and a headset plugged in within the same batch.
        let sinks = [sink("speakers", 2), sink("headset", 2)];
        let previous = vec![
            "speakers".to_owned(),
            "dock-hdmi".to_owned(),
            "dock-analog".to_owned(),
        ];
        let memory = SelectionMemory {
            most_recent_default: "dock-analog".into(),
            most_recent_playback: "dock-analog".into(),
            ..SelectionMemory::default()
        };
        let selection = choose_output(
            &sinks,
            "fxsound_sink",
            Some("fxsound_sink"),
            &previous,
            &memory,
        )
        .expect("a target");
        assert_eq!(selection.target, "headset");
        assert!(selection.write_previous_default);
    }

    #[test]
    fn when_several_devices_arrive_at_once_the_first_new_one_in_the_graph_is_taken() {
        // `:196`: "if more than one device was added this will set playback to the first device
        // found" — found in the graph's order, not the previous list's.
        let sinks = [
            sink("speakers", 2),
            sink("second-new", 2),
            sink("first-new", 2),
        ];
        let previous = vec!["speakers".to_owned(), "gone".to_owned()];
        let selection = choose_output(
            &sinks,
            "fxsound_sink",
            Some("fxsound_sink"),
            &previous,
            &SelectionMemory {
                most_recent_default: "speakers".into(),
                most_recent_playback: "speakers".into(),
                ..SelectionMemory::default()
            },
        )
        .expect("a target");
        assert_eq!(selection.target, "second-new");
    }

    #[test]
    fn a_device_leaving_is_not_mistaken_for_one_arriving() {
        let sinks = [sink("a", 2), sink("b", 2)];
        let previous = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let selection = choose_output(
            &sinks,
            "fxsound_sink",
            Some("fxsound_sink"),
            &previous,
            &SelectionMemory {
                most_recent_default: "b".into(),
                most_recent_playback: "c".into(),
                ..SelectionMemory::default()
            },
        )
        .expect("a target");
        assert_eq!(
            selection.target, "b",
            "nothing is new, so rule 7 walks on from the device that left"
        );
        assert!(
            !selection.write_previous_default,
            "and a device found by rule 7 re-dates nothing"
        );
    }

    #[test]
    fn nothing_counts_as_newly_arrived_without_a_previous_enumeration() {
        // The first run of a lane, and the first after it is switched on again: every device is
        // "not in the previous list", and none of them may be taken for being new.
        let sinks = [sink("a", 2), sink("b", 2)];
        let selection = choose_output(
            &sinks,
            "fxsound_sink",
            Some("fxsound_sink"),
            &[],
            &SelectionMemory {
                most_recent_default: "b".into(),
                most_recent_playback: "b".into(),
                ..SelectionMemory::default()
            },
        )
        .expect("a target");
        assert_eq!(selection.target, "b");
        assert!(!selection.write_previous_default);
    }

    #[test]
    fn when_we_are_already_the_default_the_remembered_devices_are_walked_in_order() {
        let sinks = [sink("a", 2), sink("b", 2), sink("c", 2)];
        let previous = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];

        let with = |memory: SelectionMemory| {
            choose_output(
                &sinks,
                "fxsound_sink",
                Some("fxsound_sink"),
                &previous,
                &memory,
            )
            .expect("a target")
            .target
        };

        assert_eq!(
            with(SelectionMemory {
                most_recent_default: "b".into(),
                most_recent_playback: "c".into(),
                prior_default: "a".into(),
                ..SelectionMemory::default()
            }),
            "c",
            "most_recent_playback is consulted first"
        );
        assert_eq!(
            with(SelectionMemory {
                most_recent_default: "b".into(),
                most_recent_playback: "gone".into(),
                prior_default: "a".into(),
                ..SelectionMemory::default()
            }),
            "b",
            "then most_recent_default"
        );
        assert_eq!(
            with(SelectionMemory {
                most_recent_default: "gone".into(),
                most_recent_playback: "gone".into(),
                prior_default: "a".into(),
                ..SelectionMemory::default()
            }),
            "a",
            "then prior_default"
        );
        assert_eq!(
            with(SelectionMemory {
                most_recent_default: "gone".into(),
                most_recent_playback: "gone".into(),
                prior_default: "gone".into(),
                original_default: "c".into(),
                ..SelectionMemory::default()
            }),
            "c",
            "then original_default"
        );
        assert_eq!(
            with(SelectionMemory {
                most_recent_default: "gone".into(),
                ..SelectionMemory::default()
            }),
            "a",
            "and finally the first sink in the graph"
        );
    }

    #[test]
    fn a_mono_output_the_user_picked_is_used_though_a_stereo_one_is_there() {
        // Windows answered -58 here, SND_DEVICES_ASK_USER_SELECT_PLAYBACK_DEVICE: asked the user
        // to pick an output when the user had just picked one.
        let sinks = [sink("mono-bt", 1), sink("speakers", 2)];
        let memory = SelectionMemory {
            most_recent_default: "mono-bt".into(),
            user_selected: "mono-bt".into(),
            ..SelectionMemory::default()
        };
        let selection =
            choose_output(&sinks, "fxsound_sink", Some("mono-bt"), &[], &memory).expect("a target");
        assert_eq!(selection.target, "mono-bt");
    }

    #[test]
    fn a_mono_output_is_not_swapped_for_the_last_stereo_device_we_used() {
        // Windows retried `most_recent_playback` for a mono target (`:300-302`), which moved the
        // music out of a headset in its call profile and onto the speakers mid-call.
        let sinks = [sink("mono-bt", 1), sink("speakers", 2)];
        let memory = SelectionMemory {
            most_recent_default: "mono-bt".into(),
            most_recent_playback: "speakers".into(),
            user_selected: "mono-bt".into(),
            ..SelectionMemory::default()
        };
        let selection =
            choose_output(&sinks, "fxsound_sink", Some("mono-bt"), &[], &memory).expect("a target");
        assert_eq!(selection.target, "mono-bt");
    }

    #[test]
    fn a_graph_of_nothing_but_mono_sinks_still_has_an_output() {
        // Windows answered -57 here, SND_DEVICES_NO_VALID_PLAYBACK_DEVICE, and played nothing.
        let sinks = [sink("mono-a", 1), sink("mono-b", 1)];
        let memory = SelectionMemory {
            most_recent_default: "mono-a".into(),
            ..SelectionMemory::default()
        };
        let selection =
            choose_output(&sinks, "fxsound_sink", Some("mono-a"), &[], &memory).expect("a target");
        assert_eq!(selection.target, "mono-a");
        assert!(selection.write_previous_default, "rule 6, as for any sink");

        // Rule 3 as well: the only device there is, is the device, whatever its channel count.
        let only = [sink("mono-only", 1)];
        let selection = choose_output(
            &only,
            "fxsound_sink",
            Some("fxsound_sink"),
            &[],
            &SelectionMemory::default(),
        )
        .expect("a target");
        assert_eq!(selection.target, "mono-only");
    }

    #[test]
    fn the_default_is_handed_back_to_the_first_remembered_device_still_present() {
        let sinks = [sink("speakers", 2), sink("usb-dac", 2)];
        let memory = SelectionMemory {
            original_default: "speakers".into(),
            most_recent_default: "gone".into(),
            prior_default: "also-gone".into(),
            most_recent_playback: "usb-dac".into(),
            user_selected: String::new(),
        };
        assert_eq!(
            restore_default_candidate(&memory, &sinks, DeviceDirection::Output).as_deref(),
            Some("usb-dac")
        );

        let unplugged = [sink("speakers", 2)];
        assert_eq!(
            restore_default_candidate(&memory, &unplugged, DeviceDirection::Output).as_deref(),
            Some("speakers")
        );
        assert_eq!(
            restore_default_candidate(&SelectionMemory::default(), &sinks, DeviceDirection::Output),
            None,
            "with nothing remembered the default must be left for WirePlumber to decide"
        );
    }

    #[test]
    fn the_default_source_is_handed_back_to_a_remembered_source_only() {
        let devices = [
            sink("usb-sink", 2),
            source("usb-source", 2),
            sink("speakers", 2),
            source("internal-mic", 2),
        ];
        let memory = SelectionMemory {
            original_default: "internal-mic".into(),
            most_recent_playback: "usb-source".into(),
            ..SelectionMemory::default()
        };
        assert_eq!(
            restore_default_candidate(&memory, &devices, DeviceDirection::Input).as_deref(),
            Some("usb-source")
        );
        // A memory full of source names says nothing about which *sink* to restore.
        assert_eq!(
            restore_default_candidate(&memory, &devices, DeviceDirection::Output),
            None
        );
        // And a remembered source that has been unplugged falls through to the next one.
        let unplugged = [sink("speakers", 2), source("internal-mic", 2)];
        assert_eq!(
            restore_default_candidate(&memory, &unplugged, DeviceDirection::Input).as_deref(),
            Some("internal-mic")
        );
    }

    // ---- U9: WirePlumber 0.5's Bluetooth microphone, and one headset on both lanes -----------

    /// A property dictionary out of `(key, value)` pairs, the way a test hands one to the parsers.
    fn props_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> + 'a {
        move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    /// One device out of a fixture, by name.
    fn device_in(dump: &str, name: &str) -> DeviceInfo {
        devices_in(dump)
            .into_iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} is not a device in the fixture"))
    }

    /// The devices of one direction in a fixture, by name.
    fn names_in(dump: &str, direction: DeviceDirection) -> Vec<String> {
        devices_in(dump)
            .into_iter()
            .filter(|d| d.direction == direction)
            .map(|d| d.name)
            .collect()
    }

    #[test]
    fn each_hands_free_codec_carries_its_own_rate_and_a_music_codec_none() {
        assert_eq!(bluez_codec_rate("cvsd"), Some(8_000));
        assert_eq!(bluez_codec_rate("msbc"), Some(16_000));
        assert_eq!(bluez_codec_rate("lc3_a127"), Some(24_000));
        assert_eq!(bluez_codec_rate("lc3_swb"), Some(32_000));
        for music in [
            "sbc", "sbc_xq", "aac", "ldac", "aptx_hd", "opus_05", "lc3", "",
        ] {
            assert_eq!(
                bluez_codec_rate(music),
                None,
                "{music:?} is not a call codec, and says nothing about a call's bandwidth"
            );
        }
        assert_eq!(
            BLUEZ_HEADSET_RATE,
            bluez_codec_rate("msbc").expect("mSBC is a call codec"),
            "a headset that names no codec is taken to run the wide band"
        );
    }

    #[test]
    fn wireplumbers_internal_sco_source_is_never_listed_as_a_microphone() {
        let inputs = names_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP, DeviceDirection::Input);
        assert_eq!(
            inputs,
            [LAPTOP_MICROPHONE, LOOPBACK_MICROPHONE],
            "the loopback is the headset's microphone; the SCO source behind it is WirePlumber's"
        );
        assert!(
            DeviceInfo::from_props(
                71,
                &props_of(&[
                    ("media.class", "Audio/Source"),
                    ("node.name", SCO_SOURCE),
                    ("api.bluez5.profile", "headset-head-unit"),
                    ("api.bluez5.internal", "true"),
                ])
            )
            .is_none()
        );
        let facts = BluezFacts::from_props(&props_of(&[("api.bluez5.internal", "true")]));
        assert!(facts.internal);
        assert!(
            !BluezFacts::from_props(&props_of(&[("api.bluez5.internal", "false")])).internal,
            "only `true` hides a node"
        );
    }

    #[test]
    fn wireplumber_05s_loopback_microphone_is_a_bluetooth_headsets_at_the_wide_band() {
        for dump in [BLUEZ_HEADSET_HEAD_UNIT_DUMP, BLUEZ_A2DP_WIREPLUMBER_05_DUMP] {
            let microphone = device_in(dump, LOOPBACK_MICROPHONE);
            assert_eq!(microphone.direction, DeviceDirection::Input);
            assert!(microphone.bluez.loopback);
            assert!(
                !microphone.bluez.own,
                "it carries no `api.bluez5.*` key at all"
            );
            assert_eq!(microphone.bluez.headset_profile, None);
            assert_eq!(microphone.bluez_address, None);
            assert_eq!(
                microphone.card_id,
                Some(60),
                "`device.id` names the headset's card"
            );
            assert!(microphone.bluez_headset);
            assert_eq!(microphone.form_factor, FormFactor::Headset);
            assert_eq!(
                microphone.native_rate(),
                Some(16_000.0),
                "it names no codec, so it runs the wide band nearly every headset negotiates"
            );
            assert_eq!(
                microphone.to_audio_device(None).form_factor,
                "headset",
                "the GUI draws it as a headset"
            );
        }
    }

    #[test]
    fn under_wireplumber_05_the_headset_idles_in_a2dp_as_headphones_beside_its_microphone() {
        let outputs = names_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, DeviceDirection::Output);
        assert_eq!(outputs, [LAPTOP_SPEAKERS, HEADSET_SINK]);
        let inputs = names_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, DeviceDirection::Input);
        assert_eq!(inputs, [LAPTOP_MICROPHONE, LOOPBACK_MICROPHONE]);

        let sink = device_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, HEADSET_SINK);
        assert_eq!(sink.channels, 2);
        assert!(!sink.bluez_headset, "A2DP carries no microphone");
        assert_eq!(sink.bluez.headset_profile, Some(false));
        assert_eq!(sink.bluez.codec_rate, None, "SBC is a music codec");
        assert_eq!(sink.form_factor, FormFactor::Headphones);
        assert_eq!(sink.native_rate(), None);
    }

    #[test]
    fn wireplumber_04s_microphone_is_the_sco_source_itself_at_its_codecs_rate() {
        let inputs = names_in(
            BLUEZ_HEADSET_HEAD_UNIT_WIREPLUMBER_04_DUMP,
            DeviceDirection::Input,
        );
        assert_eq!(
            inputs,
            [LAPTOP_MICROPHONE, SCO_SOURCE],
            "without `api.bluez5.internal` the SCO source is the microphone"
        );
        let microphone = device_in(BLUEZ_HEADSET_HEAD_UNIT_WIREPLUMBER_04_DUMP, SCO_SOURCE);
        assert!(microphone.bluez_headset);
        assert!(!microphone.bluez.loopback);
        assert_eq!(microphone.form_factor, FormFactor::Headset);
        assert_eq!(
            microphone.native_rate(),
            Some(8_000.0),
            "CVSD: the narrow band, which the de-esser should know to stand aside for"
        );
        let sink = device_in(BLUEZ_HEADSET_HEAD_UNIT_WIREPLUMBER_04_DUMP, HEADSET_SINK);
        assert_eq!(sink.native_rate(), Some(8_000.0), "one SCO link, one codec");
    }

    #[test]
    fn a_headsets_codec_sets_its_native_rate_and_a_published_rate_outranks_it() {
        let parse = |pairs: &[(&str, &str)]| {
            DeviceInfo::from_props(7, &props_of(pairs)).expect("a named source is a device")
        };
        let base = [
            ("media.class", "Audio/Source"),
            ("node.name", SCO_SOURCE),
            ("api.bluez5.profile", "headset-head-unit"),
        ];
        for (codec, rate) in [
            ("cvsd", 8_000.0),
            ("msbc", 16_000.0),
            ("lc3_a127", 24_000.0),
            ("lc3_swb", 32_000.0),
        ] {
            let pairs: Vec<(&str, &str)> = base
                .iter()
                .copied()
                .chain([("api.bluez5.codec", codec)])
                .collect();
            assert_eq!(parse(&pairs).native_rate(), Some(rate), "{codec}");
        }
        assert_eq!(
            parse(&base).native_rate(),
            Some(16_000.0),
            "a headset profile that names no codec"
        );
        let published = parse(&[
            ("media.class", "Audio/Source"),
            ("node.name", SCO_SOURCE),
            ("api.bluez5.profile", "headset-head-unit"),
            ("api.bluez5.codec", "lc3_swb"),
            ("audio.rate", "16000"),
        ]);
        assert_eq!(
            published.native_rate(),
            Some(16_000.0),
            "the node's own word about its rate wins"
        );
        // A music codec on an A2DP node is no reason to call it narrow.
        let a2dp = parse(&[
            ("media.class", "Audio/Sink"),
            ("node.name", HEADSET_SINK),
            ("api.bluez5.profile", "a2dp-sink"),
            ("api.bluez5.codec", "sbc"),
        ]);
        assert_eq!(a2dp.native_rate(), None);
    }

    #[test]
    fn a_bluetooth_microphone_that_names_no_profile_is_a_headsets_and_a_sink_is_not() {
        let microphone = DeviceInfo::from_props(
            9,
            &props_of(&[
                ("media.class", "Audio/Source"),
                ("node.name", "bluez_input.66_77_88_99_AA_BB"),
                ("device.api", "bluez5"),
            ]),
        )
        .expect("a source");
        assert!(microphone.bluez_headset);
        assert_eq!(microphone.form_factor, FormFactor::Headset);
        assert_eq!(microphone.native_rate(), Some(16_000.0));

        let sink = DeviceInfo::from_props(
            10,
            &props_of(&[
                ("media.class", "Audio/Sink"),
                ("node.name", "bluez_output.66_77_88_99_AA_BB.1"),
                ("device.api", "bluez5"),
            ]),
        )
        .expect("a sink");
        assert!(
            !sink.bluez_headset,
            "a Bluetooth sink that says nothing of its profile is not taken for a call"
        );
        assert_eq!(sink.form_factor, FormFactor::Headphones);

        // The profile outranks everything: a phone streaming music into the computer is a
        // Bluetooth source, and no headset.
        let phone = DeviceInfo::from_props(
            11,
            &props_of(&[
                ("media.class", "Audio/Source"),
                ("node.name", "bluez_input.66_77_88_99_AA_BB.2"),
                ("device.api", "bluez5"),
                ("api.bluez5.profile", "a2dp-source"),
            ]),
        )
        .expect("a source");
        assert!(!phone.bluez_headset);
        assert_eq!(phone.form_factor, FormFactor::Headphones);
        assert_eq!(phone.native_rate(), None);
    }

    #[test]
    fn the_form_factor_reads_the_loopback_microphone_as_a_headset_on_its_properties_alone() {
        assert_eq!(
            FormFactor::from_props(&props_of(&[("bluez5.loopback", "true")])),
            FormFactor::Headset
        );
        assert_eq!(
            FormFactor::from_props(&props_of(&[
                ("bluez5.loopback", "false"),
                ("api.bluez5.profile", "headset-head-unit"),
            ])),
            FormFactor::Headset,
            "the SCO source WirePlumber marks `false` is a headset by its profile"
        );
        assert_eq!(
            FormFactor::from_props(&props_of(&[("device.api", "bluez5")])),
            FormFactor::Headphones,
            "direction-agnostic, a Bluetooth node that says nothing is headphones"
        );
        assert_eq!(
            FormFactor::from_props(&props_of(&[("bluez5.loopback", "false")])),
            FormFactor::Unknown,
            "`false` is not a loopback"
        );
    }

    #[test]
    fn a_microphone_known_only_by_its_registry_global_becomes_a_headset_by_its_card() {
        // What the registry announces for WirePlumber 0.5's microphone: nothing Bluetooth about it
        // but the card it names.
        let mut microphone = DeviceInfo::from_props(
            73,
            &props_of(&[
                ("media.class", "Audio/Source"),
                ("node.name", LOOPBACK_MICROPHONE),
                ("node.description", "Test Headset"),
                ("device.id", "60"),
                ("object.serial", "73"),
            ]),
        )
        .expect("a source");
        assert!(!microphone.bluez_headset);
        assert_eq!(microphone.form_factor, FormFactor::Microphone);
        assert_eq!(microphone.native_rate(), None);

        assert!(microphone.on_bluetooth_card(), "news the first time");
        assert!(microphone.bluez_headset);
        assert_eq!(microphone.form_factor, FormFactor::Headset);
        assert_eq!(microphone.native_rate(), Some(16_000.0));
        assert!(!microphone.on_bluetooth_card(), "and no news after");

        // A sink on the same card is Bluetooth, and headphones, until its info names a profile.
        let mut sink = DeviceInfo::from_props(
            70,
            &props_of(&[
                ("media.class", "Audio/Sink"),
                ("node.name", HEADSET_SINK),
                ("device.id", "60"),
            ]),
        )
        .expect("a sink");
        assert!(sink.on_bluetooth_card());
        assert!(!sink.bluez_headset);
        assert_eq!(sink.form_factor, FormFactor::Headphones);
    }

    #[test]
    fn a_nodes_info_tells_what_its_registry_global_could_not_and_keeps_what_its_card_said() {
        let registry_only = |name: &str| {
            let pairs = [
                ("media.class", "Audio/Source"),
                ("node.name", name),
                ("device.id", "60"),
            ];
            DeviceInfo::from_props(1, &props_of(&pairs)).expect("a source")
        };
        let info = |dump: &str, id: u32| {
            let props = objects_in(dump)
                .into_iter()
                .find(|(object, _, _)| *object == id)
                .map(|(_, _, props)| props)
                .expect("the node is in the fixture");
            let get = |key: &str| {
                props
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            };
            (BluezFacts::from_props(&get), FormFactor::from_props(&get))
        };

        // WirePlumber 0.5's microphone, learned from its info alone.
        let mut loopback = registry_only(LOOPBACK_MICROPHONE);
        let (facts, form_factor) = info(BLUEZ_HEADSET_HEAD_UNIT_DUMP, 73);
        assert!(loopback.learn_bluetooth(facts, form_factor));
        assert!(loopback.bluez_headset);
        assert_eq!(loopback.form_factor, FormFactor::Headset);
        assert_eq!(loopback.native_rate(), Some(16_000.0));
        assert!(
            !loopback.learn_bluetooth(facts, form_factor),
            "the same info again is no news"
        );

        // WirePlumber 0.4's, whose codec is only in its info.
        let mut sco = registry_only(SCO_SOURCE);
        let (facts, form_factor) = info(BLUEZ_HEADSET_HEAD_UNIT_WIREPLUMBER_04_DUMP, 71);
        assert!(sco.learn_bluetooth(facts, form_factor));
        assert_eq!(sco.native_rate(), Some(8_000.0));

        // What the card said survives an info that could not say it.
        let mut on_card = registry_only(LOOPBACK_MICROPHONE);
        assert!(on_card.on_bluetooth_card());
        let (facts, form_factor) = info(BLUEZ_HEADSET_HEAD_UNIT_DUMP, 73);
        on_card.learn_bluetooth(facts, form_factor);
        assert!(on_card.bluez.card);
        assert!(on_card.bluez.loopback);

        // And an info that names a profile outranks what the card suggested.
        let mut phone = registry_only("bluez_input.66_77_88_99_AA_BB.2");
        assert!(phone.on_bluetooth_card());
        assert!(
            phone.bluez_headset,
            "a Bluetooth microphone, as far as anyone knew"
        );
        let a2dp_source = [
            ("device.api", "bluez5"),
            ("api.bluez5.profile", "a2dp-source"),
        ];
        let get = props_of(&a2dp_source);
        assert!(phone.learn_bluetooth(BluezFacts::from_props(&get), FormFactor::from_props(&get)));
        assert!(!phone.bluez_headset);
        assert_eq!(phone.form_factor, FormFactor::Headphones);

        // An ALSA node's info leaves its icon to its registry global.
        let mut mic = source("alsa_input.usb-fifine", 2);
        mic.form_factor = FormFactor::Microphone;
        let alsa = [("device.api", "alsa"), ("device.icon-name", "audio-card")];
        let get = props_of(&alsa);
        assert!(!mic.learn_bluetooth(BluezFacts::from_props(&get), FormFactor::from_props(&get)));
        assert_eq!(mic.form_factor, FormFactor::Microphone);
    }

    #[test]
    fn a_headsets_sink_and_its_microphone_are_one_bluetooth_device_under_either_wireplumber() {
        let cards = cards_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        for (dump, microphone) in [
            (BLUEZ_HEADSET_HEAD_UNIT_DUMP, LOOPBACK_MICROPHONE),
            (BLUEZ_A2DP_WIREPLUMBER_05_DUMP, LOOPBACK_MICROPHONE),
            (BLUEZ_HEADSET_HEAD_UNIT_WIREPLUMBER_04_DUMP, SCO_SOURCE),
        ] {
            let sink = device_in(dump, HEADSET_SINK);
            let microphone = device_in(dump, microphone);
            assert!(sink.same_bluetooth_device(&microphone, &cards));
            assert!(
                microphone.same_bluetooth_device(&sink, &cards),
                "either way round"
            );
        }

        // By the card's address when the microphone names only its card and the sink only its
        // address.
        let sink = DeviceInfo {
            card_id: None,
            ..device_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, HEADSET_SINK)
        };
        let loopback = device_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, LOOPBACK_MICROPHONE);
        assert!(sink.same_bluetooth_device(&loopback, &cards));
        assert!(
            !sink.same_bluetooth_device(&loopback, &[]),
            "without the card, nothing ties an address to a card number"
        );
    }

    #[test]
    fn two_headsets_or_one_sound_card_are_not_one_bluetooth_device() {
        let cards = cards_in(BLUEZ_HEADSET_HEAD_UNIT_DUMP);
        let sink = device_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, HEADSET_SINK);
        let another = DeviceInfo {
            card_id: Some(61),
            ..device_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, LOOPBACK_MICROPHONE)
        };
        assert!(!sink.same_bluetooth_device(&another, &cards));

        // The laptop's speakers and microphone share a card, and nothing a lane could suffer from.
        let speakers = device_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, LAPTOP_SPEAKERS);
        let microphone = device_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP, LAPTOP_MICROPHONE);
        assert_eq!(speakers.card_id, microphone.card_id);
        assert!(!speakers.same_bluetooth_device(&microphone, &cards));
        // Nor is the headset one device with the laptop's microphone.
        assert!(!sink.same_bluetooth_device(&microphone, &cards));
    }

    #[test]
    fn a_card_says_it_is_bluetooth_in_its_registry_global() {
        let cards = cards_in(BLUEZ_A2DP_WIREPLUMBER_05_DUMP);
        assert_eq!(cards.len(), 1);
        assert!(cards[0].bluetooth);
        // What the registry announces of a card: its `device.api`, and no address.
        let announced = Card::from_props(60, &props_of(&[("device.api", "bluez5")]));
        assert!(announced.bluetooth);
        assert_eq!(announced.bluez_address, None);
        let alsa = Card::from_props(45, &props_of(&[("device.api", "alsa")]));
        assert!(!alsa.bluetooth);
    }
}
