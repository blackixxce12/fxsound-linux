//! Whether a node can be heard: what its card says about the port behind it (U4,
//! `docs/0.4.0-upstream.md`).
//!
//! A node in the graph is not always a device anyone can hear. On a UCM card — a laptop's
//! SOF/HDA-DSP audio, most ARM boards — the profile holds every path the hardware has, so the sinks
//! for HDMI/DisplayPort 1 to 3 exist whether or not a monitor is plugged in, and so does the
//! headphones' sink, or the headset's microphone, with nothing in the jack. The card knows: each of
//! those paths is a *port*, and a port says `available = no` while its jack is empty. PipeWire
//! publishes ports as the card's routes — `SPA_PARAM_EnumRoute`, every port with the devices it can
//! serve, and `SPA_PARAM_Route`, the port each active device is on now — and the node names its
//! device on the card in `card.profile.device`.
//!
//! The rules must not pick such a node while something else can be heard: ranked first, it would
//! take the lane and play into an empty jack, and the old rules' last resort — "the first device
//! there is" — could do the same (`docs/0.4.0-upstream.md` U4; the review's correction to item 4;
//! upstream issues #104, #582 and #595). WirePlumber draws exactly this line for its own choices:
//! a node without an available route is neither made the default nor linked to
//! (`default-nodes/rescan.lua:110-114`, `linking/find-best-target.lua:72`). [`CardRoutes::available`]
//! is its verdict, `lutils.haveAvailableRoutes` (`lib/linking-utils.lua:356-410` in 0.5.17), step
//! for step, so FxSound and the session manager never disagree about which of the two a node is.
//!
//! Everything here runs on the main loop, from a card's `param` events; nothing is anywhere near
//! a process callback.

use libspa::param::ParamType;
use libspa::pod::deserialize::PodDeserializer;
use libspa::pod::{Pod, Value, ValueArray};
use libspa::utils::Id;

/// `SPA_PARAM_ROUTE_available`: whether the card knows of anything plugged into a port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Availability {
    /// The port has no jack to ask — a laptop's speakers, a digital microphone. Heard, as far as
    /// anyone can tell; WirePlumber treats it so, and so does this crate.
    #[default]
    Unknown,
    /// The jack is empty: headphones not plugged in, no monitor on the HDMI port.
    No,
    /// Something is plugged in.
    Yes,
}

impl Availability {
    /// From the raw `spa_param_availability` value. Anything this build does not know is
    /// [`Availability::Unknown`], which is the answer that never hides a device.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        match raw {
            libspa::sys::SPA_PARAM_AVAILABILITY_no => Self::No,
            libspa::sys::SPA_PARAM_AVAILABILITY_yes => Self::Yes,
            _ => Self::Unknown,
        }
    }
}

/// Which of a card's two route lists a route came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteList {
    /// `SPA_PARAM_Route`: the port each active device of the card is on now, one per device, each
    /// naming its device in [`Route::device`].
    Active,
    /// `SPA_PARAM_EnumRoute`: every port the card has, each with the devices it can serve in
    /// [`Route::devices`] and no single device of its own.
    All,
}

impl RouteList {
    /// The two params a card is subscribed to for them.
    pub const PARAMS: [ParamType; 2] = [ParamType::Route, ParamType::EnumRoute];

    /// The list a `param` event of type `param` belongs to, if it is one of the two.
    #[must_use]
    pub fn of(param: ParamType) -> Option<Self> {
        if param == ParamType::Route {
            Some(Self::Active)
        } else if param == ParamType::EnumRoute {
            Some(Self::All)
        } else {
            None
        }
    }
}

/// One route of a card: what [`CardRoutes::available`] needs of a `SPA_TYPE_OBJECT_ParamRoute`,
/// and nothing else. The name, the volumes and the rest of the object are the session manager's
/// business.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Route {
    /// `SPA_PARAM_ROUTE_index`: the port's index on the card.
    pub index: u32,
    /// `SPA_PARAM_ROUTE_device`: the device of the card this port is the active one of — a node's
    /// `card.profile.device`. Only a [`RouteList::Active`] route carries it.
    pub device: Option<u32>,
    /// `SPA_PARAM_ROUTE_devices`: every device of the card the port can serve.
    pub devices: Vec<u32>,
    /// `SPA_PARAM_ROUTE_available`.
    pub available: Availability,
}

impl Route {
    /// Read a route out of the pod a card's `param` event carries. `None` for a pod that is not a
    /// route object or carries no index — a route that cannot be read says nothing, and hides
    /// nothing.
    ///
    /// Four keys are looked up by id rather than the whole object deserialised: the object also
    /// carries the port's volumes, its description and a struct of free-form info, none of which
    /// is wanted here, and a key a later PipeWire adds must not be able to make a route unreadable.
    #[must_use]
    pub fn from_pod(pod: &Pod) -> Option<Self> {
        let object = pod.as_object().ok()?;
        if object.type_().as_raw() != libspa::sys::SPA_TYPE_OBJECT_ParamRoute {
            return None;
        }
        let prop = |key: u32| object.find_prop(Id(key)).map(|prop| prop.value());
        let int = |key: u32| {
            prop(key)
                .and_then(|value| value.get_int().ok())
                .and_then(|value| u32::try_from(value).ok())
        };
        let index = int(libspa::sys::SPA_PARAM_ROUTE_index)?;
        let device = int(libspa::sys::SPA_PARAM_ROUTE_device);
        let available = prop(libspa::sys::SPA_PARAM_ROUTE_available)
            .and_then(|value| value.get_id().ok())
            .map_or(Availability::Unknown, |id| Availability::from_raw(id.0));
        let devices = prop(libspa::sys::SPA_PARAM_ROUTE_devices)
            .and_then(|value| {
                // A pod's bytes stop at its size, and libspa's deserializer wants the padding to
                // eight that follows it in any buffer: a route of one device — the usual kind — has
                // a 20-byte array, which it reads as cut short.
                let mut padded = value.as_bytes().to_vec();
                padded.resize(padded.len().next_multiple_of(8), 0);
                PodDeserializer::deserialize_any_from(&padded)
                    .ok()
                    .map(|(_, value)| value)
            })
            .map(|value| match value {
                Value::ValueArray(ValueArray::Int(devices)) => devices
                    .into_iter()
                    .filter_map(|device| u32::try_from(device).ok())
                    .collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();
        Some(Self {
            index,
            device,
            devices,
            available,
        })
    }
}

/// One of a card's two route lists, as the server last sent it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Batch {
    routes: Vec<Route>,
    /// The place in the enumeration of the last route received — the `param` event's own index,
    /// not the route's.
    ///
    /// The server sends a list whole, in index order, on subscribing and again on every change to
    /// it (`impl-device.c`, `emit_params`), with nothing to mark where one sending ends. So an index
    /// that is not above the last one starts the list afresh. A list whose new sending starts
    /// above where the old one ended — a profile switch to devices all numbered higher — keeps the
    /// old routes beside the new ones; those name devices the new profile does not have, whose
    /// nodes are gone, and nothing asks about them until a later sending replaces them.
    last_index: Option<u32>,
}

impl Batch {
    fn learn(&mut self, index: u32, route: Route) {
        if self.last_index.is_some_and(|last| index <= last) {
            self.routes.clear();
        }
        self.last_index = Some(index);
        self.routes
            .retain(|known| (known.index, known.device) != (route.index, route.device));
        self.routes.push(route);
    }
}

/// What one card says about its ports: both of its route lists, kept per card for as long as the
/// card is in the graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardRoutes {
    active: Batch,
    all: Batch,
}

impl CardRoutes {
    /// Take one route from a card's `param` event. `index` is the event's own index — its place in
    /// the enumeration ([`Batch`] says why that matters).
    pub fn learn(&mut self, list: RouteList, index: u32, route: Route) {
        match list {
            RouteList::Active => self.active.learn(index, route),
            RouteList::All => self.all.learn(index, route),
        }
    }

    /// Whether a node on this card, on its device `profile_device`, can be heard.
    ///
    /// WirePlumber's verdict, in its order (`lib/linking-utils.lua:375-409`). The active route of
    /// that device decides when there is one: heard unless it says `available = no`. Without one,
    /// the ports that can serve the device decide: heard if any of them is not `no`. And a device no
    /// port serves at all — a Pro Audio profile's — is heard: there is nothing to say otherwise.
    #[must_use]
    pub fn available(&self, profile_device: u32) -> bool {
        if let Some(route) = self
            .active
            .routes
            .iter()
            .find(|route| route.device == Some(profile_device))
        {
            return route.available != Availability::No;
        }
        let mut serving = self
            .all
            .routes
            .iter()
            .filter(|route| route.devices.contains(&profile_device))
            .peekable();
        serving.peek().is_none() || serving.any(|route| route.available != Availability::No)
    }
}

/// Whether a node can be heard, given the routes of the card it names (`None`: a card that has
/// told nothing yet, or no card) and its `card.profile.device`.
///
/// A node that names no device on a card — a virtual sink, a Bluetooth node, one whose info has
/// not arrived yet — is heard, as WirePlumber has it (`linking-utils.lua:360-363`), and so is one
/// whose card has not been heard from (`:371-373`).
#[must_use]
pub fn node_available(card: Option<&CardRoutes>, profile_device: Option<u32>) -> bool {
    match (card, profile_device) {
        (Some(card), Some(device)) => card.available(device),
        _ => true,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use libspa::pod::serialize::PodSerializer;
    use libspa::pod::{Object, Property, PropertyFlags};

    /// A laptop on a UCM card — SOF over an HDA codec, `sof-hda-dsp` — with no monitor on any of its
    /// three HDMI/DisplayPort ports and nothing in its headset jack, under PipeWire 1.6 and
    /// WirePlumber 0.5.
    ///
    /// Composed rather than captured: no such laptop was at hand, the session graph is not ours to
    /// probe, and a private daemon cannot hold a card without the hardware behind it. Every value
    /// comes from something in that stack. The UCM devices, their comments, PCMs, priorities and
    /// jacks are alsa-ucm-conf 1.2.16's (`Intel/sof-hda-dsp/HiFi.conf`, `Hdmi.conf`,
    /// `codecs/hda/hdmi.conf`, `HDA/HiFi-analog.conf`, `HDA/HiFi-mic.conf`); one mapping per UCM
    /// device, with the speakers and the headphones sharing a PCM and so in two profiles, is ACP's
    /// since it took PulseAudio 17's UCM rework; the node names, nicks, descriptions and priorities
    /// are what WirePlumber 0.5.17's `monitors/alsa.lua:263-363` makes of them; and the route and
    /// profile objects have the keys and value types of ACP's `build_route` and `build_profile`, as
    /// `pw-dump` prints them. The card's devices are numbered apart from its ports, as they are on a
    /// real card, so nothing can match a node to its route by the port's number and pass.
    pub(crate) const UCM_LAPTOP: &str =
        include_str!("../tests/fixtures/pw-dump-ucm-laptop-hdmi.json");

    /// The laptop's speakers in that fixture: the sink the lane should be on.
    pub(crate) const SPEAKER: &str =
        "alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Speaker__sink";
    /// Its first HDMI/DisplayPort sink, with no monitor on the port.
    pub(crate) const HDMI1: &str =
        "alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__HDMI1__sink";
    pub(crate) const HDMI2: &str =
        "alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__HDMI2__sink";
    pub(crate) const HDMI3: &str =
        "alsa_output.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__HDMI3__sink";
    /// The digital microphone array: no jack, always heard.
    pub(crate) const DMIC: &str =
        "alsa_input.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Mic1__source";
    /// The headset jack's microphone, with nothing in the jack.
    pub(crate) const HEADSET_MIC: &str =
        "alsa_input.pci-0000_00_1f.3-platform-skl_hda_dsp_generic.HiFi__Mic2__source";

    /// A route object with the keys and value types PipeWire's ACP gives it
    /// (`spa/plugins/alsa/acp-device.c`, `build_route`), including the ones [`Route::from_pod`]
    /// has to step over: the name, the description, the priority, the free-form info struct, the
    /// profiles and the port's own volume object.
    pub(crate) fn route_pod(
        index: u32,
        device: Option<u32>,
        devices: &[u32],
        available: u32,
    ) -> Vec<u8> {
        use libspa::sys as spa;
        let property = |key: u32, value: Value| Property {
            key,
            flags: PropertyFlags::empty(),
            value,
        };
        let mut properties = vec![
            property(spa::SPA_PARAM_ROUTE_index, Value::Int(index as i32)),
            property(
                spa::SPA_PARAM_ROUTE_direction,
                Value::Id(Id(spa::SPA_DIRECTION_OUTPUT)),
            ),
            property(
                spa::SPA_PARAM_ROUTE_name,
                Value::String(format!("[Out] Port{index}")),
            ),
            property(
                spa::SPA_PARAM_ROUTE_description,
                Value::String("A port".to_owned()),
            ),
            property(spa::SPA_PARAM_ROUTE_priority, Value::Int(500)),
            property(spa::SPA_PARAM_ROUTE_available, Value::Id(Id(available))),
            property(
                spa::SPA_PARAM_ROUTE_info,
                Value::Struct(vec![
                    Value::Int(1),
                    Value::String("port.type".to_owned()),
                    Value::String("hdmi".to_owned()),
                ]),
            ),
            property(
                spa::SPA_PARAM_ROUTE_profiles,
                Value::ValueArray(ValueArray::Int(vec![1, 2])),
            ),
        ];
        if let Some(device) = device {
            properties.push(property(
                spa::SPA_PARAM_ROUTE_device,
                Value::Int(device as i32),
            ));
            properties.push(property(
                spa::SPA_PARAM_ROUTE_props,
                Value::Object(Object {
                    type_: spa::SPA_TYPE_OBJECT_Props,
                    id: spa::SPA_PARAM_Route,
                    properties: vec![
                        property(spa::SPA_PROP_mute, Value::Bool(false)),
                        property(
                            spa::SPA_PROP_channelVolumes,
                            Value::ValueArray(ValueArray::Float(vec![0.4, 0.4])),
                        ),
                        property(spa::SPA_PROP_latencyOffsetNsec, Value::Long(0)),
                    ],
                }),
            ));
        }
        properties.push(property(
            spa::SPA_PARAM_ROUTE_devices,
            Value::ValueArray(ValueArray::Int(
                devices.iter().map(|&device| device as i32).collect(),
            )),
        ));
        if device.is_some() {
            properties.push(property(spa::SPA_PARAM_ROUTE_profile, Value::Int(2)));
            properties.push(property(spa::SPA_PARAM_ROUTE_save, Value::Bool(false)));
        }
        let id = if device.is_some() {
            spa::SPA_PARAM_Route
        } else {
            spa::SPA_PARAM_EnumRoute
        };
        PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(Object {
                type_: spa::SPA_TYPE_OBJECT_ParamRoute,
                id,
                properties,
            }),
        )
        .expect("a route object serialises")
        .0
        .into_inner()
    }

    fn parsed(bytes: &[u8]) -> Option<Route> {
        Route::from_pod(Pod::from_bytes(bytes).expect("a whole pod"))
    }

    const NO: u32 = libspa::sys::SPA_PARAM_AVAILABILITY_no;
    const YES: u32 = libspa::sys::SPA_PARAM_AVAILABILITY_yes;
    const UNKNOWN: u32 = libspa::sys::SPA_PARAM_AVAILABILITY_unknown;

    fn route(index: u32, device: Option<u32>, devices: &[u32], available: Availability) -> Route {
        Route {
            index,
            device,
            devices: devices.to_vec(),
            available,
        }
    }

    #[test]
    fn an_active_route_is_read_out_of_the_pod_a_card_sends() {
        assert_eq!(
            parsed(&route_pod(3, Some(0), &[0], NO)),
            Some(route(3, Some(0), &[0], Availability::No))
        );
    }

    #[test]
    fn a_port_from_the_cards_whole_list_carries_its_devices_and_no_device_of_its_own() {
        assert_eq!(
            parsed(&route_pod(6, None, &[6, 7], UNKNOWN)),
            Some(route(6, None, &[6, 7], Availability::Unknown))
        );
    }

    #[test]
    fn each_availability_the_server_can_send_is_told_apart() {
        for (raw, want) in [
            (NO, Availability::No),
            (YES, Availability::Yes),
            (UNKNOWN, Availability::Unknown),
            (77, Availability::Unknown),
        ] {
            assert_eq!(
                parsed(&route_pod(0, Some(0), &[0], raw)).map(|route| route.available),
                Some(want),
                "raw availability {raw}"
            );
        }
    }

    #[test]
    fn a_pod_that_is_not_a_route_says_nothing() {
        let props = PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(Object {
                type_: libspa::sys::SPA_TYPE_OBJECT_Props,
                id: libspa::sys::SPA_PARAM_Props,
                properties: vec![Property {
                    key: libspa::sys::SPA_PROP_mute,
                    flags: PropertyFlags::empty(),
                    value: Value::Bool(true),
                }],
            }),
        )
        .expect("a props object serialises")
        .0
        .into_inner();
        assert_eq!(parsed(&props), None);
        let int = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Int(3))
            .expect("an int serialises")
            .0
            .into_inner();
        assert_eq!(parsed(&int), None);
    }

    #[test]
    fn only_the_two_route_params_are_route_lists() {
        assert_eq!(RouteList::of(ParamType::Route), Some(RouteList::Active));
        assert_eq!(RouteList::of(ParamType::EnumRoute), Some(RouteList::All));
        assert_eq!(RouteList::of(ParamType::Props), None);
        assert_eq!(RouteList::of(ParamType::Profile), None);
    }

    #[test]
    fn a_device_whose_active_port_is_unplugged_is_not_heard() {
        let mut card = CardRoutes::default();
        card.learn(
            RouteList::Active,
            0,
            route(0, Some(0), &[0], Availability::No),
        );
        card.learn(
            RouteList::Active,
            6,
            route(6, Some(6), &[6], Availability::Unknown),
        );
        assert!(!card.available(0), "HDMI with no monitor");
        assert!(card.available(6), "speakers, which have no jack to ask");
    }

    #[test]
    fn the_active_port_decides_over_the_other_ports_that_could_serve_the_device() {
        // `linking-utils.lua:377-386`: the device's own route answers first, whatever the rest of
        // the card's ports say.
        let mut card = CardRoutes::default();
        card.learn(RouteList::All, 0, route(0, None, &[4], Availability::Yes));
        card.learn(RouteList::All, 1, route(1, None, &[4], Availability::No));
        card.learn(
            RouteList::Active,
            4,
            route(1, Some(4), &[4], Availability::No),
        );
        assert!(!card.available(4));
    }

    #[test]
    fn without_an_active_port_any_port_that_can_serve_the_device_and_is_not_unplugged_will_do() {
        let mut card = CardRoutes::default();
        card.learn(RouteList::All, 0, route(0, None, &[4, 5], Availability::No));
        card.learn(
            RouteList::All,
            1,
            route(1, None, &[4], Availability::Unknown),
        );
        card.learn(RouteList::All, 2, route(2, None, &[5], Availability::No));
        assert!(card.available(4), "one of its two ports is not unplugged");
        assert!(!card.available(5), "both of its ports are");
    }

    #[test]
    fn a_device_no_port_serves_is_heard_as_a_pro_audio_profiles_is() {
        let mut card = CardRoutes::default();
        card.learn(RouteList::All, 0, route(0, None, &[0], Availability::No));
        assert!(card.available(9));
        assert!(
            CardRoutes::default().available(0),
            "a card that sent nothing"
        );
    }

    #[test]
    fn a_node_on_no_card_or_with_no_card_device_is_heard() {
        let mut card = CardRoutes::default();
        card.learn(
            RouteList::Active,
            0,
            route(0, Some(0), &[0], Availability::No),
        );
        assert!(!node_available(Some(&card), Some(0)));
        assert!(
            node_available(Some(&card), None),
            "no card.profile.device yet"
        );
        assert!(node_available(None, Some(0)), "a card not heard from");
        assert!(node_available(None, None), "a virtual sink");
    }

    #[test]
    fn a_list_sent_again_replaces_what_was_there() {
        let mut card = CardRoutes::default();
        card.learn(
            RouteList::Active,
            0,
            route(0, Some(0), &[0], Availability::No),
        );
        card.learn(
            RouteList::Active,
            6,
            route(6, Some(6), &[6], Availability::Unknown),
        );
        // A monitor plugged in: the server sends the whole list again, from the start.
        card.learn(
            RouteList::Active,
            0,
            route(0, Some(0), &[0], Availability::Yes),
        );
        assert!(card.available(0));
        assert_eq!(card.active.routes.len(), 1, "{:?}", card.active.routes);
        card.learn(
            RouteList::Active,
            6,
            route(6, Some(6), &[6], Availability::Unknown),
        );
        assert_eq!(card.active.routes.len(), 2);
    }

    #[test]
    fn the_same_route_sent_twice_in_a_row_is_kept_once() {
        let mut batch = Batch::default();
        batch.learn(3, route(3, Some(3), &[3], Availability::No));
        batch.learn(4, route(3, Some(3), &[3], Availability::Yes));
        assert_eq!(batch.routes, [route(3, Some(3), &[3], Availability::Yes)]);
    }

    #[test]
    fn the_two_lists_are_kept_apart() {
        let mut card = CardRoutes::default();
        card.learn(RouteList::All, 5, route(5, None, &[5], Availability::No));
        // A later active route at a lower place in its own enumeration starts only its own list.
        card.learn(
            RouteList::Active,
            0,
            route(0, Some(0), &[0], Availability::Yes),
        );
        assert!(
            !card.available(5),
            "the whole list still says port 5 is unplugged"
        );
        assert!(card.available(0));
    }

    // -----------------------------------------------------------------------------------------
    // The UCM laptop fixture, read the way the engine reads a graph: the nodes' properties as the
    // registry and their info give them, and the card's routes as pods built from what `pw-dump`
    // printed of them, with the keys ACP gives them.
    // -----------------------------------------------------------------------------------------

    fn dump() -> Vec<serde_json::Value> {
        serde_json::from_str(UCM_LAPTOP).expect("the fixture is a JSON array")
    }

    /// A `pw-dump` property value as PipeWire hands it to a client: a string.
    fn prop_text(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Bool(flag) => Some(flag.to_string()),
            serde_json::Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    }

    /// Every sink and source in the fixture, parsed as the engine parses a node.
    pub(crate) fn ucm_devices() -> Vec<crate::devices::DeviceInfo> {
        dump()
            .iter()
            .filter(|object| object["type"] == "PipeWire:Interface:Node")
            .filter_map(|object| {
                let id = u32::try_from(object["id"].as_u64()?).ok()?;
                let props: Vec<(String, String)> = object["info"]["props"]
                    .as_object()?
                    .iter()
                    .filter_map(|(key, value)| prop_text(value).map(|text| (key.clone(), text)))
                    .collect();
                crate::devices::DeviceInfo::from_props(id, &|key: &str| {
                    props
                        .iter()
                        .find(|(known, _)| known == key)
                        .map(|(_, value)| value.as_str())
                })
            })
            .collect()
    }

    fn availability_raw(text: &str) -> u32 {
        match text {
            "no" => NO,
            "yes" => YES,
            _ => UNKNOWN,
        }
    }

    fn ints(value: &serde_json::Value) -> Vec<u32> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_u64().and_then(|n| u32::try_from(n).ok()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One of the fixture card's route lists as the card's `param` events would deliver it: each
    /// route with its place in the enumeration — for ACP's active list the device's number, for
    /// its whole list the port's — and as a pod built from what `pw-dump` printed of it, with the
    /// keys ACP gives it, sent through [`Route::from_pod`].
    fn fixture_list(card: &serde_json::Value, list: RouteList) -> Vec<(u32, Route)> {
        let key = match list {
            RouteList::Active => "Route",
            RouteList::All => "EnumRoute",
        };
        card["info"]["params"][key]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|printed| {
                let index = printed["index"]
                    .as_u64()
                    .and_then(|index| u32::try_from(index).ok())
                    .expect("a route has an index");
                let device = printed["device"]
                    .as_u64()
                    .and_then(|device| u32::try_from(device).ok());
                let pod = route_pod(
                    index,
                    device,
                    &ints(&printed["devices"]),
                    availability_raw(printed["available"].as_str().unwrap_or("unknown")),
                );
                let route = parsed(&pod).expect("every route in the fixture is readable");
                (device.unwrap_or(index), route)
            })
            .collect()
    }

    /// Every card in the fixture with its routes, both lists delivered in the order the dump lists
    /// them — the order the server enumerates them in.
    pub(crate) fn ucm_card_routes() -> std::collections::HashMap<u32, CardRoutes> {
        let mut cards = std::collections::HashMap::new();
        for card in dump()
            .iter()
            .filter(|object| object["type"] == "PipeWire:Interface:Device")
        {
            let id = card["id"]
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .expect("a card has an id");
            let routes: &mut CardRoutes = cards.entry(id).or_default();
            for list in [RouteList::All, RouteList::Active] {
                for (place, route) in fixture_list(card, list) {
                    routes.learn(list, place, route);
                }
            }
        }
        cards
    }

    /// The fixture's devices with the availability the engine would give them.
    pub(crate) fn ucm_devices_as_heard() -> Vec<crate::devices::DeviceInfo> {
        let cards = ucm_card_routes();
        ucm_devices()
            .into_iter()
            .map(|mut device| {
                device.available = node_available(
                    device.card_id.and_then(|card| cards.get(&card)),
                    device.profile_device,
                );
                device
            })
            .collect()
    }

    #[test]
    fn the_ucm_fixture_holds_a_sink_for_every_hdmi_port_and_both_microphones() {
        let names: Vec<String> = ucm_devices().into_iter().map(|d| d.name).collect();
        for name in [SPEAKER, HDMI1, HDMI2, HDMI3, DMIC, HEADSET_MIC] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
        assert_eq!(names.len(), 6, "{names:?}");
    }

    #[test]
    fn every_node_on_the_ucm_card_names_its_card_and_its_device_on_it() {
        for device in ucm_devices() {
            assert_eq!(device.card_id, Some(46), "{}", device.name);
            assert!(device.profile_device.is_some(), "{}", device.name);
        }
    }

    #[test]
    fn the_hdmi_sinks_with_no_monitor_and_the_empty_headset_jack_are_not_heard() {
        let heard: Vec<(String, bool)> = ucm_devices_as_heard()
            .into_iter()
            .map(|device| (device.name, device.available))
            .collect();
        for (name, want) in [
            (SPEAKER, true),
            (HDMI1, false),
            (HDMI2, false),
            (HDMI3, false),
            (DMIC, true),
            (HEADSET_MIC, false),
        ] {
            assert!(
                heard.contains(&(name.to_owned(), want)),
                "{name} should be {}heard: {heard:?}",
                if want { "" } else { "un" }
            );
        }
    }

    #[test]
    fn with_only_the_cards_whole_list_the_unplugged_ports_still_silence_their_nodes() {
        // Before the active list has come — or on a card that sends none — the ports that can
        // serve each device decide, as in WirePlumber's second step. Every port here serves one
        // device, so each array is the single-element kind a padding slip once read as empty.
        let dump = dump();
        let fixture_card = dump
            .iter()
            .find(|object| object["type"] == "PipeWire:Interface:Device")
            .expect("the card");
        let mut card = CardRoutes::default();
        for (place, route) in fixture_list(fixture_card, RouteList::All) {
            assert_eq!(route.devices.len(), 1, "{route:?}");
            card.learn(RouteList::All, place, route);
        }
        for device in ucm_devices() {
            let heard = card.available(device.profile_device.expect("a card device"));
            let silent = [HDMI1, HDMI2, HDMI3, HEADSET_MIC].contains(&device.name.as_str());
            assert_eq!(heard, !silent, "{}", device.name);
        }
    }

    #[test]
    fn a_monitor_plugged_into_the_first_hdmi_port_makes_its_sink_heard() {
        let mut cards = ucm_card_routes();
        let card = cards.get_mut(&46).expect("the UCM card");
        let hdmi1 = ucm_devices()
            .into_iter()
            .find(|device| device.name == HDMI1)
            .expect("HDMI1 is in the fixture")
            .profile_device
            .expect("HDMI1 names its device on the card");
        assert!(!card.available(hdmi1));
        // The card sends its active list again, whole and from the start, with HDMI1's port now
        // plugged; everything else as it was.
        let dump = dump();
        let fixture_card = dump
            .iter()
            .find(|object| object["type"] == "PipeWire:Interface:Device")
            .expect("the card");
        for (place, mut route) in fixture_list(fixture_card, RouteList::Active) {
            if route.device == Some(hdmi1) {
                route.available = Availability::Yes;
            }
            card.learn(RouteList::Active, place, route);
        }
        assert!(card.available(hdmi1));
        let hdmi2 = ucm_devices()
            .into_iter()
            .find(|device| device.name == HDMI2)
            .and_then(|device| device.profile_device)
            .expect("HDMI2 names its device on the card");
        assert!(!card.available(hdmi2), "only the port with the monitor");
    }
}
