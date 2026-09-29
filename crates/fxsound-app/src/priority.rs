//! The device priority list (U4): which real device each lane prefers.
//!
//! Upstream 1.2 keeps a ranked list of every output device it has ever seen (`DeviceConfig.cpp`)
//! and lets it choose the device (`FxController.cpp:1540-1612`). Here the list is
//! [`Settings::device_configs`], read per direction, and the choosing is the engine's
//! ([`UiToAudio::SetDevicePriority`]). What the controller does with it is three things:
//!
//! - keep it: every device a device list names is added to it the first time it is seen, at the
//!   bottom — or at the top with *Prioritize new output devices* ([`learn`]);
//! - hand it to the engine whenever it changes ([`ranking`]), or an empty list while *Follow the
//!   system's default device* is on;
//! - order what the window's combos, the tray and `--next-output`/`--next-input` offer by it
//!   ([`sort_by_rank`]), as upstream sorts its output list (`sortByDeviceConfigPriority`,
//!   `FxController.cpp:1714-1725`).
//!
//! [`UiToAudio::SetDevicePriority`]: fxsound_core::messages::UiToAudio::SetDevicePriority

use fxsound_core::settings::DeviceConfig;
use fxsound_core::{AudioDevice, DeviceDirection, Settings};

/// What one device list taught the priority list ([`learn`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Learned {
    /// `device_configs` changed — a device was added, or one it knew is described differently now
    /// — and the settings file has to be written.
    pub changed: bool,
    /// Per lane, outputs first: a device seen for the first time that went to the top of the list
    /// (*Prioritize new output devices*). The lane has to be moved to it by name, as upstream's
    /// `updateOutputs` moves to a newcomer that ranks above the current device
    /// (`FxController.cpp:1558-1584`): the engine ran its rules on the new graph before this list
    /// reached the controller, while the newcomer was not ranked at all, and a ranking that
    /// arrives afterwards moves nothing by itself.
    pub promoted: [Option<String>; 2],
}

/// The devices of `direction` the priority list holds, most preferred first.
pub(crate) fn ranked(
    settings: &Settings,
    direction: DeviceDirection,
) -> impl Iterator<Item = &DeviceConfig> {
    settings
        .device_configs
        .iter()
        .filter(move |config| config.direction == direction)
}

/// What the engine is told about `direction` ([`UiToAudio::SetDevicePriority`]): the list's
/// `node.name`s, most preferred first — or nothing at all while the user has asked FxSound to
/// follow the system's default device instead (upstream issue #629).
///
/// [`UiToAudio::SetDevicePriority`]: fxsound_core::messages::UiToAudio::SetDevicePriority
pub(crate) fn ranking(settings: &Settings, direction: DeviceDirection) -> Vec<String> {
    if settings.follow_system_default {
        return Vec::new();
    }
    ranked(settings, direction)
        .map(|config| config.device_id.clone())
        .collect()
}

/// Where `name` stands in `direction`'s list: its place, or after every listed device.
fn rank(settings: &Settings, direction: DeviceDirection, name: &str) -> usize {
    ranked(settings, direction)
        .position(|config| config.device_id == name)
        .unwrap_or(usize::MAX)
}

/// Put `devices` in the order the window lists them: outputs first, as the engine sends them, and
/// each direction by its priority list. The sort is stable, so devices the list does not know
/// keep the engine's order, after every one it does.
///
/// Whatever the *Follow the system's default device* switch says: it decides whether the list
/// picks the device, not whether it is the user's order of their devices.
pub(crate) fn sort_by_rank(devices: &mut [AudioDevice], settings: &Settings) {
    devices.sort_by_key(|device| {
        (
            lane(device.direction),
            rank(settings, device.direction, &device.name),
        )
    });
}

/// Add every device of `devices` the priority list does not know yet, as upstream's
/// `DeviceConfig::updateDeviceConfigs` does (`DeviceConfig.cpp:54-108`).
///
/// A newcomer goes to the bottom of its direction's list, or to the top with *Prioritize new
/// output devices* — several newcomers at once keep the order the list gave them. The first
/// time a direction is seen at all, its devices are the list, in the order upstream's
/// `initDeviceConfigs` gives them (`DeviceConfig.cpp:26-52`): the one in use first — the lane's
/// saved device, then the one it is attached to (`attached`, outputs first), then the session
/// default — and the rest as listed. Taken in any other order, the first list would rank some
/// device above the one playing, and the engine would move the lane to it.
///
/// An entry the list already has keeps its place and its preset, and takes the device's current
/// description and form factor, which the Settings pane shows.
pub(crate) fn learn(
    settings: &mut Settings,
    devices: &[AudioDevice],
    attached: [Option<&str>; 2],
) -> Learned {
    let mut learned = Learned::default();

    for config in &mut settings.device_configs {
        let Some(device) = devices
            .iter()
            .find(|d| d.direction == config.direction && d.name == config.device_id)
        else {
            continue;
        };
        if !device.description.is_empty() && config.device_name != device.description {
            config.device_name.clone_from(&device.description);
            learned.changed = true;
        }
        if !device.form_factor.is_empty() && config.device_form_factor != device.form_factor {
            config.device_form_factor.clone_from(&device.form_factor);
            learned.changed = true;
        }
    }

    for direction in DeviceDirection::ALL {
        let first_time = ranked(settings, direction).next().is_none();
        let mut newcomers: Vec<&AudioDevice> = Vec::new();
        for device in devices.iter().filter(|d| d.direction == direction) {
            // The same name listed twice is one device.
            if rank(settings, direction, &device.name) == usize::MAX
                && !newcomers.iter().any(|seen| seen.name == device.name)
            {
                newcomers.push(device);
            }
        }
        if newcomers.is_empty() {
            continue;
        }
        learned.changed = true;
        let entry = |device: &AudioDevice| DeviceConfig {
            device_id: device.name.clone(),
            direction,
            device_name: device.description.clone(),
            preset: String::new(),
            device_form_factor: device.form_factor.clone(),
            extra: toml::Table::new(),
        };

        if first_time {
            let saved = settings.device_name(direction).to_owned();
            let attached = attached[lane(direction)];
            let in_use = |device: &AudioDevice| {
                if device.name == saved {
                    0
                } else if attached == Some(device.name.as_str()) {
                    1
                } else if device.is_default {
                    2
                } else {
                    3
                }
            };
            newcomers.sort_by_key(|device| in_use(device));
            settings
                .device_configs
                .extend(newcomers.into_iter().map(entry));
        } else if settings.prioritize_new_output {
            learned.promoted[lane(direction)] = Some(newcomers[0].name.clone());
            settings
                .device_configs
                .splice(0..0, newcomers.into_iter().map(entry));
        } else {
            settings
                .device_configs
                .extend(newcomers.into_iter().map(entry));
        }
    }
    learned
}

/// A lane's slot in the per-lane arrays, outputs first.
const fn lane(direction: DeviceDirection) -> usize {
    match direction {
        DeviceDirection::Output => 0,
        DeviceDirection::Input => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;

    fn device(name: &str, direction: DeviceDirection) -> AudioDevice {
        AudioDevice {
            id: 0,
            name: name.to_owned(),
            description: name.to_uppercase(),
            is_default: false,
            direction,
            form_factor: String::new(),
        }
    }

    fn default_device(name: &str, direction: DeviceDirection) -> AudioDevice {
        AudioDevice {
            is_default: true,
            ..device(name, direction)
        }
    }

    fn names(settings: &Settings, direction: DeviceDirection) -> Vec<String> {
        ranked(settings, direction)
            .map(|config| config.device_id.clone())
            .collect()
    }

    fn known(pairs: &[(&str, DeviceDirection)]) -> Settings {
        let mut settings = Settings::default();
        settings.device_configs = pairs
            .iter()
            .map(|(name, direction)| DeviceConfig {
                device_id: (*name).to_owned(),
                direction: *direction,
                ..DeviceConfig::default()
            })
            .collect();
        settings
    }

    #[test]
    fn the_first_list_puts_the_device_in_use_first_and_the_rest_as_listed() {
        let mut settings = Settings::default();
        let devices = [
            device("hdmi", OUT),
            device("headphones", OUT),
            default_device("speakers", OUT),
            device("webcam", IN),
            default_device("laptop-mic", IN),
        ];
        let learned = learn(&mut settings, &devices, [None, None]);
        assert!(learned.changed);
        assert_eq!(
            learned.promoted,
            [None, None],
            "nothing is new to a first list"
        );
        assert_eq!(names(&settings, OUT), ["speakers", "hdmi", "headphones"]);
        assert_eq!(names(&settings, IN), ["laptop-mic", "webcam"]);
        // Nothing remembered with them.
        assert!(settings.device_configs.iter().all(|c| c.preset.is_empty()));
        assert_eq!(settings.device_configs[0].device_name, "SPEAKERS");
    }

    #[test]
    fn the_first_list_ranks_the_saved_device_then_the_attached_one_above_the_default() {
        let mut settings = Settings::default();
        settings.set_device_name(OUT, "headphones");
        let devices = [
            default_device("speakers", OUT),
            device("hdmi", OUT),
            device("headphones", OUT),
            device("usb", OUT),
        ];
        learn(&mut settings, &devices, [Some("hdmi"), None]);
        assert_eq!(
            names(&settings, OUT),
            ["headphones", "hdmi", "speakers", "usb"]
        );
    }

    #[test]
    fn a_new_device_goes_to_the_bottom_of_its_own_directions_list() {
        let mut settings = known(&[("speakers", OUT), ("mic", IN), ("hdmi", OUT)]);
        let devices = [
            device("hdmi", OUT),
            device("speakers", OUT),
            device("usb", OUT),
            device("mic", IN),
            device("headset", IN),
        ];
        let learned = learn(&mut settings, &devices, [None, None]);
        assert!(learned.changed);
        assert_eq!(learned.promoted, [None, None]);
        assert_eq!(names(&settings, OUT), ["speakers", "hdmi", "usb"]);
        assert_eq!(names(&settings, IN), ["mic", "headset"]);
    }

    #[test]
    fn with_new_devices_prioritised_they_go_to_the_top_in_list_order_and_are_promoted() {
        let mut settings = known(&[("speakers", OUT), ("mic", IN)]);
        settings.prioritize_new_output = true;
        let devices = [
            device("dock", OUT),
            device("speakers", OUT),
            device("usb", OUT),
            device("mic", IN),
            device("headset", IN),
        ];
        let learned = learn(&mut settings, &devices, [Some("speakers"), None]);
        assert_eq!(names(&settings, OUT), ["dock", "usb", "speakers"]);
        assert_eq!(names(&settings, IN), ["headset", "mic"]);
        assert_eq!(
            learned.promoted,
            [Some("dock".to_owned()), Some("headset".to_owned())]
        );
    }

    #[test]
    fn a_list_of_devices_it_knows_changes_nothing() {
        let mut settings = known(&[("speakers", OUT), ("hdmi", OUT), ("mic", IN)]);
        settings.device_configs[0].device_name = "SPEAKERS".to_owned();
        settings.device_configs[1].device_name = "HDMI".to_owned();
        settings.device_configs[2].device_name = "MIC".to_owned();
        let before = settings.clone();
        let devices = [
            device("hdmi", OUT),
            device("speakers", OUT),
            device("mic", IN),
        ];
        assert_eq!(
            learn(&mut settings, &devices, [None, None]),
            Learned::default()
        );
        assert_eq!(settings, before);
        // A device that is gone keeps its place: it may come back.
        assert_eq!(
            learn(&mut settings, &devices[1..], [None, None]),
            Learned::default()
        );
        assert_eq!(settings, before);
    }

    #[test]
    fn a_known_device_takes_its_new_description_and_keeps_its_place_and_preset() {
        let mut settings = known(&[("speakers", OUT), ("hdmi", OUT)]);
        settings.device_configs[1].preset = "Rock".to_owned();
        let mut hdmi = device("hdmi", OUT);
        hdmi.description = "LG TV".to_owned();
        hdmi.form_factor = "tv".to_owned();
        let learned = learn(
            &mut settings,
            &[hdmi, device("speakers", OUT)],
            [None, None],
        );
        assert!(learned.changed);
        assert_eq!(names(&settings, OUT), ["speakers", "hdmi"]);
        assert_eq!(settings.device_configs[1].device_name, "LG TV");
        assert_eq!(settings.device_configs[1].device_form_factor, "tv");
        assert_eq!(settings.device_configs[1].preset, "Rock");
    }

    #[test]
    fn the_same_name_listed_twice_is_one_entry() {
        let mut settings = Settings::default();
        learn(
            &mut settings,
            &[
                device("speakers", OUT),
                device("hdmi", OUT),
                device("speakers", OUT),
            ],
            [None, None],
        );
        assert_eq!(names(&settings, OUT), ["speakers", "hdmi"]);
    }

    #[test]
    fn a_name_in_both_directions_is_two_entries() {
        // A USB headset's sink and source can share a description, never a direction.
        let mut settings = known(&[("speakers", OUT)]);
        learn(
            &mut settings,
            &[device("usb", OUT), device("usb", IN)],
            [None, None],
        );
        assert_eq!(names(&settings, OUT), ["speakers", "usb"]);
        assert_eq!(names(&settings, IN), ["usb"]);
    }

    #[test]
    fn the_engine_is_given_the_list_or_nothing_while_the_system_decides() {
        let mut settings = known(&[("speakers", OUT), ("mic", IN), ("hdmi", OUT)]);
        assert_eq!(ranking(&settings, OUT), ["speakers", "hdmi"]);
        assert_eq!(ranking(&settings, IN), ["mic"]);
        settings.follow_system_default = true;
        assert!(ranking(&settings, OUT).is_empty());
        assert!(ranking(&settings, IN).is_empty());
    }

    #[test]
    fn devices_are_listed_outputs_first_each_by_its_rank_and_the_unknown_last() {
        let mut settings = known(&[
            ("hdmi", OUT),
            ("mic", IN),
            ("speakers", OUT),
            ("webcam", IN),
        ]);
        let mut devices = vec![
            device("mystery", OUT),
            device("speakers", OUT),
            device("hdmi", OUT),
            device("webcam", IN),
            device("stranger", IN),
            device("mic", IN),
            device("another", OUT),
        ];
        sort_by_rank(&mut devices, &settings);
        let order = |devices: &[AudioDevice]| -> Vec<String> {
            devices.iter().map(|d| d.name.clone()).collect()
        };
        let ranked_order = order(&devices);
        assert_eq!(
            ranked_order,
            [
                "hdmi", "speakers", "mystery", "another", "mic", "webcam", "stranger"
            ]
        );
        // Following the system changes who picks, not the order the user gave their devices.
        settings.follow_system_default = true;
        sort_by_rank(&mut devices, &settings);
        assert_eq!(order(&devices), ranked_order);
    }
}
