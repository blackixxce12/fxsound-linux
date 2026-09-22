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
//! `default.audio.source`. The one deliberate difference is the mono guard: Windows refused mono
//! *playback* devices because of a driver bug (`sndDevices.h:39`), but a mono microphone is the
//! normal case and is accepted — the capture stream declares stereo and PipeWire's adapter
//! up-mixes.
//!
//! Everything here is a pure function over property dictionaries. Nothing in this module talks to
//! a PipeWire server, which is what lets the rules be tested against a captured `pw-dump` rather
//! than against the machine the tests happen to run on.

use fxsound_core::{AudioDevice, DeviceDirection};

use crate::{AudioError, OUR_NODE_NAMES};

/// `SND_DEVICES_MIN_NUM_CHANS` (`audiopassthru/include/sndDevices.h:190`).
///
/// A sink with fewer channels than this is refused rather than down-mixed, exactly as
/// `SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES` (`sndDevices.h:39`) makes Windows refuse it. A
/// *source* with fewer channels is up-mixed instead; see [`DeviceInfo::is_refused_mono`].
pub const MIN_CHANNELS: u32 = 2;

/// `SND_DEVICES_MAX_NUM_CHANS` (`sndDevices.h:191`).
pub const MAX_CHANNELS: u32 = 8;

/// `SND_DEVICES_MAX_SAMP_FREQ` (`sndDevices.h:189`).
pub const MAX_SAMPLE_RATE: u32 = 192_000;

/// The most a Bluetooth headset (HFP/HSP) profile carries: 16 kHz with mSBC, and CVSD is
/// narrower still at 8 kHz. Neither figure is ever published as `audio.rate` — the node
/// negotiates whatever the graph runs at and resamples inside bluez — so the profile name is the
/// only place the link's real bandwidth shows. [`DeviceInfo::native_rate`] reports it so that
/// the adaptive de-esser has something to adapt to (`docs/0.4.0-design.md` §6).
pub const BLUEZ_HEADSET_RATE: u32 = 16_000;

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

        // Bluetooth: the profile says whether the microphone is in play.
        if get("device.bus") == Some("bluetooth")
            || get("api.bluez5.profile").is_some()
            || get("api.bluez5.address").is_some()
        {
            return if is_bluez_headset_profile(get) {
                Self::Headset
            } else {
                Self::Headphones
            };
        }

        match get("device.api") {
            // `raop`/`roc`/`pulse-tunnel` sinks all announce themselves through device.api.
            Some("raop" | "roc" | "pulse-tunnel") => return Self::NetworkDevice,
            Some("bluez5") => return Self::Headphones,
            _ => {}
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

/// Whether a node's `api.bluez5.profile` is a headset profile (`headset-head-unit`,
/// `headset-audio-gateway`), the one Bluetooth profile with a microphone in play.
///
/// Shared by the form factor and by [`DeviceInfo::native_rate`], so the icon and the bandwidth
/// figure can never disagree about which nodes are headsets.
fn is_bluez_headset_profile<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> bool {
    get("api.bluez5.profile").is_some_and(|p| p.starts_with("headset") || p.starts_with("hfp"))
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

    /// Truncate or pad the layout so it describes exactly `channels` channels.
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
    pub object_serial: Option<u64>,
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
    /// Whether the node runs a Bluetooth headset (HFP/HSP) profile. Such a link carries
    /// [`BLUEZ_HEADSET_RATE`] at most and never says so in `audio.rate`; see
    /// [`DeviceInfo::native_rate`].
    pub bluez_headset: bool,
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
    /// device to attach to.
    ///
    /// Mirrors pass 1 of `sndDevices_GetAll.cpp:141-267`, including its fallbacks: a node with no
    /// `node.description` is labelled with its `node.name` rather than being dropped (the Windows
    /// code wrote `L"Unknown"`, `:260-262`), and a node that does not publish a channel count gets
    /// 0, which the mono guard in [`choose_device`] then treats the way Windows treated
    /// `deviceNumChannel = 0`.
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
        let bluez_headset = is_bluez_headset_profile(get);
        let positions = get("audio.position")
            .and_then(ChannelMap::parse)
            .filter(|map| map.len() == channels as usize)
            .unwrap_or_else(|| ChannelMap::default_for(channels));
        let form_factor = match (direction, FormFactor::from_props(get)) {
            // An ALSA source carries its card's `audio-card` icon, which reads as speakers; for a
            // capture device the honest default is a microphone.
            (DeviceDirection::Input, FormFactor::Speakers | FormFactor::Unknown) => {
                FormFactor::Microphone
            }
            (_, form_factor) => form_factor,
        };

        Some(Self {
            object_id,
            object_serial: get("object.serial").and_then(|s| s.parse::<u64>().ok()),
            name,
            description,
            nick,
            channels,
            rate,
            bluez_headset,
            positions,
            form_factor,
            direction,
        })
    }

    /// The rate the device really runs at, as far as its properties say: `audio.rate` when the
    /// node publishes one, [`BLUEZ_HEADSET_RATE`] for a Bluetooth headset profile, and `None`
    /// when the stream rate is all there is to know.
    ///
    /// The capture stream asks for 48 kHz whatever the microphone runs at, so the negotiated
    /// format cannot tell the voice chain how much bandwidth is in the signal — a resampled
    /// 16 kHz headset arrives at 48 kHz with nothing above 8 kHz, and a de-esser built for a
    /// 5500 Hz split would be working on silence. The audio crate hands this to
    /// `InputEngine::set_source_rate` when it builds the input lane's nodes, and the adaptive
    /// de-esser places its corner from it (`docs/0.4.0-design.md` §6).
    #[must_use]
    pub fn native_rate(&self) -> Option<f32> {
        match self.rate {
            Some(rate) => Some(rate as f32),
            None if self.bluez_headset => Some(BLUEZ_HEADSET_RATE as f32),
            None => None,
        }
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
    /// Only a *known* channel count below two counts. PipeWire's registry globals do not carry
    /// `audio.channels` — it lives in the node's info, which arrives only after binding to the
    /// node — so a device discovered through the registry reports `0` here. Treating that as mono
    /// refused every output on the system and left the engine with nothing to render to.
    #[must_use]
    pub const fn is_mono(&self) -> bool {
        self.channels != 0 && self.channels < MIN_CHANNELS
    }

    /// Whether the rules must refuse this device for being mono
    /// (`sndDevicesImplementDeviceRules.cpp:300`).
    ///
    /// Outputs only. `SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES` (`sndDevices.h:39`) worked around a
    /// *playback* driver bug; a mono microphone is what most microphones are, and the capture
    /// stream declares [`MIN_CHANNELS`] regardless so PipeWire's adapter up-mixes it
    /// (`docs/spec/12-audio-io.md` §28).
    #[must_use]
    pub const fn is_refused_mono(&self) -> bool {
        matches!(self.direction, DeviceDirection::Output) && self.is_mono()
    }

    /// `true` when the node never told us how many channels it has.
    #[must_use]
    pub const fn channels_unknown(&self) -> bool {
        self.channels == 0
    }

    /// The channel count FxSound will actually run, clamped to `2..=8`
    /// (`sndDevices.h:190-191`; `docs/spec/12-audio-io.md` §19.3 keeps the clamp). For a mono
    /// microphone this is the stereo pair PipeWire up-mixes it into.
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
/// 3. exactly one device → that one, *without* skipping the mono check (`:172-183`);
/// 4. an explicit user choice that still exists (`:188-193`);
/// 5. a device that appeared since the last enumeration, usable per the mono rule (`:197-231`);
/// 6. the session default, if it is not us (`:237-249`);
/// 7. otherwise the first of `most_recent_playback`, `most_recent_default`, `prior_default`,
///    `original_default` that is present, else the first device (`:257-284`).
///
/// Then the mono guard of `:290-327`, **outputs only**: a mono target is retried as
/// `most_recent_playback`, and if that is mono too the answer is
/// [`AudioError::AskUserSelectOutput`] when some other sink is usable and
/// [`AudioError::NoValidOutput`] when none is — `-58` and `-57` respectively. A mono microphone
/// passes straight through ([`DeviceInfo::is_refused_mono`]).
///
/// `previous_names` is the snapshot of real device names of this direction from the previous
/// enumeration, the equivalent of `pwszIDPreviousRealDevices` (`sndDevices.h:349`); pass an empty
/// slice on the first call — and after a direction switch — so rule 5 cannot fire.
///
/// # Errors
/// Returns the same four states the Windows rules could end in, so the GUI's existing error
/// surfaces port unchanged, plus the input-side twin of "no devices".
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

    // 3 — exactly one real device (`:172-183`).
    if target.is_none() && real.len() == 1 {
        target = Some(real[0]);
    }

    // 4 — an explicit user choice (`:188-193`).
    if target.is_none() {
        target = find(&real, &memory.user_selected);
    }

    // 5 — a device was just added (`:197-231`). Windows compared counts first; comparing the name
    // sets directly is the same test without the off-by-one at `:223` (see spec §15).
    if target.is_none() && !previous_names.is_empty() && real.len() > previous_names.len() {
        target = real
            .iter()
            .copied()
            .find(|s| !previous_names.iter().any(|p| p == &s.name) && !s.is_refused_mono());
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

    let mut chosen = target.ok_or(AudioError::DeviceNotPresent)?;

    // The mono guard (`:290-327`) — outputs only.
    if chosen.is_refused_mono() {
        match find(&real, &memory.most_recent_playback).filter(|s| !s.is_refused_mono()) {
            Some(fallback) => chosen = fallback,
            None => {
                let usable = real.iter().filter(|s| !s.is_refused_mono()).count();
                return Err(if usable >= 1 {
                    AudioError::AskUserSelectOutput
                } else {
                    AudioError::NoValidOutput
                });
            }
        }
    }

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

    /// Every object in the fixture.
    fn fixture_objects() -> Vec<FixtureObject> {
        let Json::Arr(objects) = Parser::new(PW_DUMP).value() else {
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

    /// The `default` metadata object's entries, as `(key, value)`, the way `pw-dump` prints them.
    fn fixture_default_metadata() -> Vec<(String, String)> {
        let Json::Arr(objects) = Parser::new(PW_DUMP).value() else {
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

    fn fixture_devices() -> Vec<DeviceInfo> {
        fixture_objects()
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
            name: name.to_owned(),
            description: name.to_owned(),
            nick: name.to_owned(),
            channels,
            rate: None,
            bluez_headset: false,
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

        // A sink whose channel count never reached us is not mono — it is unknown, and refusing it
        // is what made the engine report "no usable output" on a perfectly ordinary desktop.
        let unknown = DeviceInfo {
            channels: 0,
            ..alc.clone()
        };
        assert!(
            !unknown.is_mono(),
            "an unknown channel count must not be refused"
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
        assert!(
            real_mono.is_mono(),
            "a device that says it is mono is still refused"
        );
        assert!(real_mono.is_refused_mono());
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
        assert!(
            !mono.is_refused_mono(),
            "but the Windows mono bug was a playback bug; a capture device is not refused"
        );
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
    fn a_mono_microphone_is_accepted_where_a_mono_sink_would_be_refused() {
        let memory = SelectionMemory {
            most_recent_default: "mono-mic".into(),
            user_selected: "mono-mic".into(),
            ..SelectionMemory::default()
        };
        let devices = [source("mono-mic", 1), source("stereo-mic", 2)];
        let selection = choose_device(
            &devices,
            DeviceDirection::Input,
            "fxsound_source",
            Some("mono-mic"),
            &[],
            &memory,
        )
        .expect("a mono microphone is a perfectly good input");
        assert_eq!(selection.target, "mono-mic");

        // The same shape on the output side is the -58 state.
        let sinks = [sink("mono-mic", 1), sink("stereo-mic", 2)];
        assert_eq!(
            choose_output(&sinks, "fxsound_sink", Some("mono-mic"), &[], &memory),
            Err(AudioError::AskUserSelectOutput)
        );

        // Rule 5, too: a newly plugged mono microphone is taken, a newly plugged mono sink is not.
        let previous = vec!["stereo-mic".to_owned()];
        let fresh = SelectionMemory {
            most_recent_default: "stereo-mic".into(),
            most_recent_playback: "stereo-mic".into(),
            ..SelectionMemory::default()
        };
        let plugged = choose_device(
            &devices,
            DeviceDirection::Input,
            "fxsound_source",
            Some("fxsound_source"),
            &previous,
            &fresh,
        )
        .expect("a target");
        assert_eq!(plugged.target, "mono-mic");
        assert!(plugged.write_previous_default);
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
    fn a_newly_appeared_mono_sink_is_skipped() {
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
        assert_eq!(selection.target, "speakers");
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
    fn a_mono_target_with_a_stereo_alternative_asks_the_user_to_choose() {
        let sinks = [sink("mono-bt", 1), sink("speakers", 2)];
        let memory = SelectionMemory {
            most_recent_default: "mono-bt".into(),
            user_selected: "mono-bt".into(),
            ..SelectionMemory::default()
        };
        assert_eq!(
            choose_output(&sinks, "fxsound_sink", Some("mono-bt"), &[], &memory),
            Err(AudioError::AskUserSelectOutput),
            "this is the -58 SND_DEVICES_ASK_USER_SELECT_PLAYBACK_DEVICE state"
        );
    }

    #[test]
    fn a_mono_target_falls_back_to_the_last_stereo_device_we_used() {
        let sinks = [sink("mono-bt", 1), sink("speakers", 2)];
        let memory = SelectionMemory {
            most_recent_default: "mono-bt".into(),
            most_recent_playback: "speakers".into(),
            user_selected: "mono-bt".into(),
            ..SelectionMemory::default()
        };
        let selection =
            choose_output(&sinks, "fxsound_sink", Some("mono-bt"), &[], &memory).expect("a target");
        assert_eq!(selection.target, "speakers");
    }

    #[test]
    fn only_mono_sinks_present_is_the_no_valid_output_state() {
        // Both sinks must *say* they are mono. A channel count of 0 means "the node never told
        // us", which is the normal case for a registry global and is not a reason to refuse it.
        let sinks = [sink("mono-a", 1), sink("mono-b", 1)];
        let memory = SelectionMemory {
            most_recent_default: "mono-a".into(),
            ..SelectionMemory::default()
        };
        assert_eq!(
            choose_output(&sinks, "fxsound_sink", Some("mono-a"), &[], &memory),
            Err(AudioError::NoValidOutput),
            "this is the -57 SND_DEVICES_NO_VALID_PLAYBACK_DEVICE state"
        );
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
}
