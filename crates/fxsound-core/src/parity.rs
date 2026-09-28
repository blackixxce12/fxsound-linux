//! «Как в Windows» / "Like FxSound for Windows": how close to the Windows build FxSound behaves.
//!
//! One setting, four cumulative levels (`docs/0.5.0-windows-parity.md`), of which 0.5.0 offers the
//! first three:
//!
//! | Level | Key | What it changes |
//! |---|---|---|
//! | Off | `off` | Nothing: FxSound for Linux as it is. |
//! | Interface | `interface` | What the Windows build *shows or operates* differently goes back to Windows. The sound does not change. |
//! | Interface and sound | `sound` | Plus the Windows DSP paths of the output lane, so a preset sounds as it does there. |
//! | Everything | `full` | Plus the features the Windows build does not have are hidden and stopped. |
//!
//! Everything arrives in 0.6.0 (W4): 0.5.0 keeps it in the enum, so that a later version's
//! `settings.toml` still reads, but refuses it on every path ([`FULL_NOT_YET`],
//! [`WindowsParity::offered`]). A file that says `full` keeps saying it — the level survives a
//! save, so going back up to 0.6.0 finds Everything again — and runs as Interface and sound
//! ([`WindowsParity::offered_or_below`]) wherever the level is used.
//!
//! Every level includes the ones before it, and none of them ever writes the user's other
//! settings: the level is a view over [`crate::Settings`], so going back to Off finds everything as
//! it was. What is never reverted at any level — the port's own fixes where Windows is wrong, the
//! protections against data loss, the click-free transitions, the Linux plumbing (tray, D-Bus,
//! `--status`, `--watch`) — is listed in the contract document.
//!
//! [`ParityClass`] is how every command, Settings tab and D-Bus call declares the level it belongs
//! to. Each classification is an exhaustive `match` with no `_` arm, so a new option, tab or
//! method does not compile until someone has decided which level changes it.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The level of «Как в Windows» / "Like FxSound for Windows".
///
/// Ordered: a level includes every level below it ([`WindowsParity::interface`],
/// [`WindowsParity::sound`], [`WindowsParity::full`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowsParity {
    /// FxSound for Linux as it is. The default.
    #[default]
    Off,
    /// The window and its controls as on Windows; the sound unchanged.
    Interface,
    /// Plus the Windows sound: presets sound as they do there.
    Sound,
    /// Plus only what Windows has: the port's own features are hidden and stopped.
    Full,
}

impl WindowsParity {
    /// Every level, in order, Everything included.
    pub const ALL: [Self; 4] = [Self::Off, Self::Interface, Self::Sound, Self::Full];

    /// The levels this version offers, in order: the slider's three positions. Everything is kept
    /// in the enum so that a `settings.toml` of a later version still reads, and arrives in 0.6.0
    /// (W4).
    pub const SLIDER_LEVELS: [Self; 3] = [Self::Off, Self::Interface, Self::Sound];

    /// Whether this version offers the level: every level but Everything, which arrives in 0.6.0.
    #[must_use]
    pub const fn offered(self) -> bool {
        !matches!(self, Self::Full)
    }

    /// The level this version runs for `self`: itself when it is offered, Interface and sound for
    /// Everything — what Everything includes, less what it hides.
    #[must_use]
    pub const fn offered_or_below(self) -> Self {
        if self.offered() { self } else { Self::Sound }
    }

    /// The spelling in `settings.toml`, on the command line, on D-Bus and in `--status`.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Interface => "interface",
            Self::Sound => "sound",
            Self::Full => "full",
        }
    }

    /// A level by its key, in any case and with the blanks around it ignored. `everything`, the
    /// slider's word for the last position, is taken for `full`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        Self::ALL
            .into_iter()
            .find(|level| text.eq_ignore_ascii_case(level.key()))
            .or_else(|| {
                text.eq_ignore_ascii_case("everything")
                    .then_some(Self::Full)
            })
    }

    /// The position's label under the slider: an English key for [`crate::i18n::tr`].
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Interface => "Interface",
            Self::Sound => "Interface and sound",
            Self::Full => "Everything",
        }
    }

    /// The one line under the slider that says what the position does: an English key for
    /// [`crate::i18n::tr`]. Neutral about what later versions add or take away, so that the
    /// translations do not have to be made again.
    #[must_use]
    pub const fn hint(self) -> &'static str {
        match self {
            Self::Off => "FxSound for Linux as it is.",
            Self::Interface => "Window and controls as on Windows. Same sound.",
            Self::Sound => "Plus the Windows sound: presets sound the same.",
            Self::Full => "Only what Windows has; the rest is hidden.",
        }
    }

    /// The position on the slider, `0..=2` for the levels offered; Everything's would be 3.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Off => 0,
            Self::Interface => 1,
            Self::Sound => 2,
            Self::Full => 3,
        }
    }

    /// The level at a slider position; the nearest one for a position past either end.
    #[must_use]
    pub fn from_index(index: usize) -> Self {
        Self::SLIDER_LEVELS[index.min(Self::SLIDER_LEVELS.len() - 1)]
    }

    /// Whether this is Off, the level `settings.toml` does not spell out.
    #[must_use]
    pub const fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }

    /// Whether the interface is the Windows one: Interface and every level above it.
    #[must_use]
    pub const fn interface(self) -> bool {
        self as u8 >= Self::Interface as u8
    }

    /// Whether the output lane runs the Windows DSP: Interface and sound, and Everything.
    #[must_use]
    pub const fn sound(self) -> bool {
        self as u8 >= Self::Sound as u8
    }

    /// Whether the port's own features are hidden and stopped: Everything only.
    #[must_use]
    pub const fn full(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether a thing of `class` is changed at this level: an interface thing from Interface
    /// on, a sound thing from Interface and sound on, a port-only thing at Everything, and a thing
    /// that is never reverted at no level at all.
    #[must_use]
    pub const fn changes(self, class: ParityClass) -> bool {
        match class {
            ParityClass::Interface => self.interface(),
            ParityClass::Sound => self.sound(),
            ParityClass::Full => self.full(),
            ParityClass::Never => false,
        }
    }

    /// The level a `settings.toml` value stands for. Never an error: a value that cannot be
    /// read costs this one key, never the file (see the `Deserialize` impl).
    ///
    /// A string is read by [`WindowsParity::parse`]. Everything (`full`), which this version
    /// does not offer, is kept as it was read, so that the next save writes it back and going
    /// back up a version loses nothing; it runs as Interface and sound
    /// ([`WindowsParity::offered_or_below`], applied where the level is used), so a file a later
    /// version wrote keeps the Windows interface and sound it asked for. `false` is
    /// Off. `true` is Interface and sound: the superseded plan for this mode had a single switch
    /// that meant the Windows interface and the Windows sound together, with nothing hidden, and
    /// a file edited by hand from that plan says `true`. Anything else is Off, with a warning in
    /// the log.
    #[must_use]
    pub fn from_toml(value: &toml::Value) -> Self {
        match value {
            toml::Value::String(text) => match Self::parse(text) {
                Some(level) => {
                    if !level.offered() {
                        log::info!(
                            "settings.toml: windows_parity = {text:?} arrives in a later version; \
                             running as sound and keeping {text:?} in the file"
                        );
                    }
                    level
                }
                None => {
                    log::warn!(
                        "settings.toml: windows_parity = {text:?} is not off, interface, sound \
                         or full; using off"
                    );
                    Self::Off
                }
            },
            toml::Value::Boolean(false) => Self::Off,
            toml::Value::Boolean(true) => Self::Sound,
            other => {
                log::warn!(
                    "settings.toml: windows_parity = {other} is not off, interface, sound or \
                     full; using off"
                );
                Self::Off
            }
        }
    }
}

impl std::fmt::Display for WindowsParity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

impl Serialize for WindowsParity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.key())
    }
}

/// Never fails. A parse error in `settings.toml` moves the whole file aside and resets every
/// setting (`Settings::load_from`), so this key takes whatever TOML the file holds and reads
/// what it cannot understand as Off ([`WindowsParity::from_toml`]).
impl<'de> Deserialize<'de> for WindowsParity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        Ok(Self::from_toml(&value))
    }
}

/// The level at which something stops behaving as it does in FxSound for Linux.
///
/// Every command-line command, Settings tab and D-Bus call has one, given by an exhaustive
/// `match` so that nothing new can be added without deciding it (`docs/0.5.0-windows-parity.md`,
/// "Classification").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParityClass {
    /// A Windows feature the port shows or operates differently: set back to Windows from
    /// Interface on.
    Interface,
    /// A Windows DSP path the port computes differently: set back from Interface and sound on.
    Sound,
    /// A feature the Windows build does not have: hidden and stopped at Everything.
    Full,
    /// Kept as it is at every level: a fix of the port's where Windows is wrong, a protection
    /// against losing data, a click-free transition, or Linux plumbing.
    Never,
}

impl ParityClass {
    /// The lowest level that changes a thing of this class, or `None` for [`ParityClass::Never`].
    #[must_use]
    pub const fn level(self) -> Option<WindowsParity> {
        match self {
            Self::Interface => Some(WindowsParity::Interface),
            Self::Sound => Some(WindowsParity::Sound),
            Self::Full => Some(WindowsParity::Full),
            Self::Never => None,
        }
    }
}

/// A way the window looks or works in the Windows build that the port changed, and that
/// Interface sets back (`docs/0.5.0-windows-parity.md` §2, "Window look", and §2.1): one variant
/// per approved row, named at the place in the interface that follows it, so that a search for
/// the variant finds everything the row changes.
///
/// Not listed, because the approved table keeps them at every level: the values shown with the
/// power off (#42, a port error), Restore Defaults keeping the band count (R5, data loss) and an
/// effect slider showing a stored value between its positions (#14, which only keeps a touch from
/// changing a preset silently).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowsLook {
    /// Master Gain and Balance move by two decibels, as the Windows sliders do (0.4.0 audit #22).
    /// The microphone's Makeup Gain, which Windows does not have, keeps its one.
    LevelSteps,
    /// Volume Leveling's readout says `dB` after its 0 to 4 amount (#23).
    LevelingUnit,
    /// The Windows palette as it is (#24, #25): in the light theme a graph that is switched off
    /// turns its colours white, and the rules and a menu's edge are `Outline`.
    Palette,
    /// A long preset name is cut with its unsaved-changes mark, not before it (#26).
    ModifiedMarkCut,
    /// Only the tooltips the Windows build has (#27 and the tips the port added to Windows
    /// controls): none on the title bar's buttons, the card's flip button or the level sliders,
    /// and no line about a right-click reset or the solo. The port's own features, the microphone
    /// lane among them, keep theirs.
    WindowsTooltips,
    /// The three Windows tabs of Settings caption themselves in the normal font on one line, cut
    /// where the button ends, as on Windows (#29). The port's own tabs keep the fitted caption.
    NavCaptions,
    /// A slider's fill painted as `drawLinearSlider` paints it, from the thumb's x as a width
    /// (#41, `Fidelity::Faithful`).
    SliderFill,
    /// The five- and ten-band equalizer's end bands tune one way only, from the ladder's edge
    /// inwards (R6). A band a preset already puts past the edge stays there and can only move in.
    EndBandWheels,
    /// With the power off the preset list, the menu's preset items and the tray's preset
    /// submenus are not offered (R7). The command line and D-Bus still pick presets.
    PresetsNeedPower,
    /// The equalizer's curve is the straight line through the bands' gains, not the response
    /// that plays (R8).
    CurveThroughBands,
    /// A right-click does not reset an effect slider (R9): it presses it as the left button does,
    /// as a JUCE slider takes it. The level sliders and the bands keep their reset, which Windows
    /// has.
    EffectReset,
    /// The language switch runs in the Windows build's order (#28): the order only, never its
    /// wrong names.
    LanguageOrder,
    /// The tray cuts a playback device's name after 30 characters (#31; W3).
    TrayNames,
}

impl WindowsLook {
    /// Every row, in the audit's order.
    pub const ALL: [Self; 13] = [
        Self::LevelSteps,
        Self::LevelingUnit,
        Self::Palette,
        Self::ModifiedMarkCut,
        Self::WindowsTooltips,
        Self::NavCaptions,
        Self::SliderFill,
        Self::EndBandWheels,
        Self::PresetsNeedPower,
        Self::CurveThroughBands,
        Self::EffectReset,
        Self::LanguageOrder,
        Self::TrayNames,
    ];

    /// The level that sets the row back to Windows. Exhaustive, so a new row cannot be added
    /// without deciding it.
    #[must_use]
    pub const fn parity_class(self) -> ParityClass {
        match self {
            Self::LevelSteps
            | Self::LevelingUnit
            | Self::Palette
            | Self::ModifiedMarkCut
            | Self::WindowsTooltips
            | Self::NavCaptions
            | Self::SliderFill
            | Self::EndBandWheels
            | Self::PresetsNeedPower
            | Self::CurveThroughBands
            | Self::EffectReset
            | Self::LanguageOrder
            | Self::TrayNames => ParityClass::Interface,
        }
    }

    /// The 0.4.0 audit item the row reverts, as the contract document names it.
    #[must_use]
    pub const fn audit_item(self) -> &'static str {
        match self {
            Self::LevelSteps => "#22",
            Self::LevelingUnit => "#23",
            Self::Palette => "#24, #25",
            Self::ModifiedMarkCut => "#26",
            Self::WindowsTooltips => "#27",
            Self::NavCaptions => "#29",
            Self::SliderFill => "#41",
            Self::EndBandWheels => "R6",
            Self::PresetsNeedPower => "R7",
            Self::CurveThroughBands => "R8",
            Self::EffectReset => "R9",
            Self::LanguageOrder => "#28",
            Self::TrayNames => "#31",
        }
    }
}

/// A feature a later version of FxSound adds, and the level at which it gives way to the Windows
/// build (`docs/0.5.0-windows-parity.md` §2.2). None of them is in 0.5.0: this is the hook each
/// one asks when it arrives, through [`WindowsParity::keeps`], so that the level is decided here,
/// once, in an exhaustive `match`, and not by each feature on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaterFeature {
    /// The Customization tab and the themes beyond FxSound's two (0.7.0): the Windows palette
    /// from Interface on; the tab is one line there, and hidden at Everything.
    Themes,
    /// Following the system's or the shell's palette (0.7.0): its watchers stop from Interface
    /// on.
    SystemPalette,
    /// The visualizer's new modes (0.7.0): Spectrum only from Interface on.
    VisualizerModes,
    /// The rainbow wordmark (0.7.0): the original wordmark, with its fade, from Interface on.
    RainbowWordmark,
    /// Frosted glass (0.7.0): opaque from Interface on.
    Glass,
    /// Importing foreign preset formats (0.7.0): up to Interface and sound, hidden at
    /// Everything.
    ForeignFormats,
    /// Global shortcuts through the portal (0.7.0): every level, since they bring back the
    /// Windows hotkeys.
    GlobalShortcuts,
    /// The MCP server and `--diag` (0.6.0): every level.
    Mcp,
    /// `fxsound cava` / `fxcava` (0.7.0): independent of the level.
    Cava,
}

impl LaterFeature {
    /// Every feature, in the order of the contract document.
    pub const ALL: [Self; 9] = [
        Self::Themes,
        Self::SystemPalette,
        Self::VisualizerModes,
        Self::RainbowWordmark,
        Self::Glass,
        Self::ForeignFormats,
        Self::GlobalShortcuts,
        Self::Mcp,
        Self::Cava,
    ];

    /// The level from which the feature gives way to the Windows build.
    #[must_use]
    pub const fn parity_class(self) -> ParityClass {
        match self {
            Self::Themes
            | Self::SystemPalette
            | Self::VisualizerModes
            | Self::RainbowWordmark
            | Self::Glass => ParityClass::Interface,
            Self::ForeignFormats => ParityClass::Full,
            Self::GlobalShortcuts | Self::Mcp | Self::Cava => ParityClass::Never,
        }
    }
}

impl WindowsParity {
    /// Whether the window follows the Windows build in `look` at this level.
    #[must_use]
    pub const fn windows_look(self, look: WindowsLook) -> bool {
        self.changes(look.parity_class())
    }

    /// Whether a feature of a later version runs as it would with the mode off: `false` from the
    /// level at which it gives way to the Windows build ([`LaterFeature::parity_class`]).
    #[must_use]
    pub const fn keeps(self, feature: LaterFeature) -> bool {
        !self.changes(feature.parity_class())
    }
}

/// What the command line and D-Bus answer to Everything in 0.5.0, which does not offer it
/// ([`WindowsParity::offered`]). Plain English, as every refusal of theirs.
pub const FULL_NOT_YET: &str = "\"Like FxSound for Windows\" = Everything arrives in a later \
version of FxSound. This one offers off, interface and sound.";

/// What the command line and D-Bus answer when a move to Everything would take something live
/// away and nobody said to go ahead: they cannot ask, as the window does, so they refuse. Kept
/// for Everything, which arrives in 0.6.0 (W4); until then [`FULL_NOT_YET`] is the answer.
///
/// The confirmation's words, and how to say "go ahead" on each path.
pub const FULL_REFUSAL: &str = "\"Like FxSound for Windows\" = Everything would switch off \
FxSound's microphone and application presets now: programs recording through FxSound or playing \
through their own preset would move straight to the devices. Your settings are kept. Add --force \
to switch anyway (force = true over D-Bus).";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_levels_are_cumulative() {
        use WindowsParity::{Full, Interface, Off, Sound};
        assert!(!Off.interface() && !Off.sound() && !Off.full());
        assert!(Interface.interface() && !Interface.sound() && !Interface.full());
        assert!(Sound.interface() && Sound.sound() && !Sound.full());
        assert!(Full.interface() && Full.sound() && Full.full());
        assert!(Off < Interface && Interface < Sound && Sound < Full);
        assert_eq!(WindowsParity::default(), Off);
    }

    #[test]
    fn a_level_is_read_by_its_key_in_any_case_and_everything_is_full() {
        for level in WindowsParity::ALL {
            assert_eq!(WindowsParity::parse(level.key()), Some(level));
            assert_eq!(
                WindowsParity::parse(&format!(" {} ", level.key().to_uppercase())),
                Some(level)
            );
        }
        assert_eq!(
            WindowsParity::parse("Everything"),
            Some(WindowsParity::Full)
        );
        assert_eq!(WindowsParity::parse("windows"), None);
        assert_eq!(WindowsParity::parse(""), None);
    }

    #[test]
    fn each_class_changes_from_its_own_level_up_and_never_never() {
        for level in WindowsParity::ALL {
            for class in [
                ParityClass::Interface,
                ParityClass::Sound,
                ParityClass::Full,
                ParityClass::Never,
            ] {
                let expected = class.level().is_some_and(|from| level >= from);
                assert_eq!(level.changes(class), expected, "{level:?} {class:?}");
            }
        }
    }

    #[test]
    fn a_slider_position_and_an_offered_level_are_the_same_thing() {
        for level in WindowsParity::SLIDER_LEVELS {
            assert!(level.offered());
            assert_eq!(WindowsParity::from_index(level.index()), level);
        }
        assert_eq!(WindowsParity::from_index(9), WindowsParity::Sound);
    }

    #[test]
    fn everything_is_not_offered_in_this_version_and_runs_as_interface_and_sound() {
        assert!(!WindowsParity::Full.offered());
        assert!(!WindowsParity::SLIDER_LEVELS.contains(&WindowsParity::Full));
        assert_eq!(WindowsParity::Full.offered_or_below(), WindowsParity::Sound);
        for level in WindowsParity::SLIDER_LEVELS {
            assert_eq!(level.offered_or_below(), level);
        }
    }

    #[test]
    fn whatever_toml_the_file_holds_the_key_reads_as_a_level() {
        let read = |text: &str| -> WindowsParity {
            #[derive(Deserialize)]
            struct File {
                windows_parity: WindowsParity,
            }
            toml::from_str::<File>(text)
                .expect("never fails")
                .windows_parity
        };
        assert_eq!(read("windows_parity = \"Sound\""), WindowsParity::Sound);
        // Everything, which a later version writes, as it was read: it is kept for the next save
        // and runs as Interface and sound (`offered_or_below`, where the level is used).
        assert_eq!(read("windows_parity = \"everything\""), WindowsParity::Full);
        assert_eq!(read("windows_parity = \"full\""), WindowsParity::Full);
        assert_eq!(read("windows_parity = true"), WindowsParity::Sound);
        assert_eq!(read("windows_parity = false"), WindowsParity::Off);
        assert_eq!(read("windows_parity = \"windows\""), WindowsParity::Off);
        assert_eq!(read("windows_parity = 2"), WindowsParity::Off);
        assert_eq!(read("windows_parity = 1.5"), WindowsParity::Off);
        assert_eq!(read("windows_parity = [\"full\"]"), WindowsParity::Off);
        assert_eq!(
            read("[windows_parity]\nlevel = \"full\""),
            WindowsParity::Off
        );
        assert_eq!(read("windows_parity = 1979-05-27"), WindowsParity::Off);
    }

    #[test]
    fn every_row_of_the_window_look_follows_windows_from_interface_up_and_not_at_off() {
        for look in WindowsLook::ALL {
            assert_eq!(look.parity_class(), ParityClass::Interface, "{look:?}");
            assert!(!WindowsParity::Off.windows_look(look), "{look:?}");
            for level in [
                WindowsParity::Interface,
                WindowsParity::Sound,
                WindowsParity::Full,
            ] {
                assert!(level.windows_look(look), "{level:?} {look:?}");
            }
            assert!(look.audit_item().starts_with(['#', 'R']), "{look:?}");
        }
    }

    #[test]
    fn a_later_feature_gives_way_at_the_level_the_contract_names() {
        use LaterFeature::{
            Cava, ForeignFormats, Glass, GlobalShortcuts, Mcp, RainbowWordmark, SystemPalette,
            Themes, VisualizerModes,
        };
        for feature in LaterFeature::ALL {
            assert!(WindowsParity::Off.keeps(feature), "{feature:?}");
        }
        for feature in [
            Themes,
            SystemPalette,
            VisualizerModes,
            RainbowWordmark,
            Glass,
        ] {
            assert!(!WindowsParity::Interface.keeps(feature), "{feature:?}");
            assert!(!WindowsParity::Sound.keeps(feature), "{feature:?}");
        }
        assert!(WindowsParity::Sound.keeps(ForeignFormats));
        assert!(!WindowsParity::Full.keeps(ForeignFormats));
        for feature in [GlobalShortcuts, Mcp, Cava] {
            assert!(WindowsParity::Full.keeps(feature), "{feature:?}");
        }
    }

    #[test]
    fn every_position_has_a_label_and_a_hint_of_one_short_line() {
        for level in WindowsParity::ALL {
            assert!(!level.label().is_empty());
            // The rule the translators get: about fifty Latin characters at most.
            assert!(level.hint().chars().count() <= 50, "{:?}", level.hint());
        }
    }
}
