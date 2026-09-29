//! Application streams the session manager failed to link to FxSound when FxSound took the default.
//!
//! When the power comes back on, FxSound takes both defaults again, and WirePlumber moves every
//! stream that follows the default device from the real device back onto FxSound's sink or
//! source. Now and then a recorder is moved onto nothing. WirePlumber 0.5.17 links a stream's ports
//! to its target's, and a stream that does not remix takes its target's channels: a recorder moved
//! from a mono microphone onto FxSound (Input), which is stereo, has its one port replaced with two
//! while the move is made. When WirePlumber creates the link before the replacement lands, the link
//! names the port that is about to go ("create pw link: … FL -> … MONO"), goes with it, and
//! WirePlumber gives up on the move ("link failed: 1 of 1 PipeWire links failed to activate"). The
//! recorder then records nothing until the default changes again — found by the 0.4.0 live check
//! on 3 to 6 of 9 quick power toggles with something playing to the default sink, on a private
//! graph with WirePlumber 0.5.17 and PipeWire 1.6.9. Moving the same recorder between two mono
//! microphones never did it: nothing about its ports has to change.
//!
//! WirePlumber tries such a link again at its next rescan of the graph, which only something else
//! causes. So FxSound looks after the streams it took over: while it holds a direction's default,
//! a stream that follows that default and has no link at all for [`STRANDED_AFTER`] is moved onto
//! FxSound's node by its `target.object` in the `default` metadata, and the key is deleted again at
//! once. Each change has WirePlumber rescan and link the stream to FxSound, where it belongs anyway;
//! the delete leaves it following the default as before, and leaves WirePlumber's own memory of the
//! application's target (`node/state-stream.lua`) empty, as it was for a stream that followed the
//! default. At most [`NUDGES`] times for each stream each time FxSound takes the default, so a
//! stream WirePlumber will not link for reasons of its own is not moved for ever.
//!
//! The same happens the other way, when the power goes off and FxSound hands the default back: a
//! recorder moved from FxSound's stereo source onto a mono microphone has its two ports replaced
//! with one, and WirePlumber 0.5.17 left one of four such recorders linked to nothing on about one
//! power off in fifteen, found by 0.5.0's test of how soon they record again
//! (`graph_churn::a_recorder_that_follows_the_default_source_records_fxsound_after_every_power_toggle`)
//! — and before 0.5.0's fades as well. So for [`AFTER_HAND_BACK`] after a hand-back, a stream that
//! follows the default and has no link for [`STRANDED_AFTER`] is moved onto the device the default
//! went to, the same way. After that the default is the desktop's, and its moves are
//! WirePlumber's own business.
//!
//! What this module keeps is plain data the main loop feeds from the registry: every link, and the
//! registry id and serial of FxSound's own sink and source.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use fxsound_core::DeviceDirection;

use crate::per_direction::PerDirection;

/// How long a stream that follows the default may have no link while FxSound holds it before it
/// is taken for stranded. A move WirePlumber makes unlinks the stream and links it again within a
/// few milliseconds — within 3 to 12, measured with WirePlumber 0.5.17 on a private graph; a
/// stranded one stays unlinked until the default changes again. A quarter of a second, and not
/// the half of 0.4.0: a recorder moved by the power switch is silent from its fade until it is
/// linked again (`crate::stream_handover`), and one WirePlumber stranded has to record again within
/// a second of the switch (roadmap 0.5.0 §7, test 1). The wait starts when the stream's last link
/// goes ([`Stranded::removed`]), and the supervisor's tick, every 200 ms, finds it due.
pub(crate) const STRANDED_AFTER: Duration = Duration::from_millis(250);

/// How many times a stranded stream is moved onto FxSound each time FxSound takes the default, or
/// onto the device each time FxSound hands it back.
pub(crate) const NUDGES: u8 = 2;

/// How long after FxSound handed a default back it still moves a stream the hand-back left linked
/// to nothing onto the device the default went to: the fade, the move, [`STRANDED_AFTER`], a
/// second nudge after as long again, and room for a slow server.
pub(crate) const AFTER_HAND_BACK: Duration = Duration::from_secs(3);

/// One link of the graph: which node it takes from and which it feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    output: u32,
    input: u32,
}

/// The links of the graph, FxSound's own sink and source, and which streams have gone unlinked
/// since when (module docs). Belongs to the session: emptied when it closes.
#[derive(Debug, Default)]
pub(crate) struct Stranded {
    links: HashMap<u32, Link>,
    /// FxSound's own node of each direction — the sink, the source — as `(id, object.serial)`.
    ours: PerDirection<Option<(u32, u64)>>,
    /// Each stream that follows a default FxSound holds and was last seen with no link: since when.
    since: HashMap<u32, Instant>,
    /// How often each stream has been moved onto FxSound since FxSound last took the default.
    nudged: HashMap<u32, u8>,
}

impl Stranded {
    /// A link appeared: from a port of the node `output` to one of the node `input`.
    pub(crate) fn link_appeared(&mut self, id: u32, output: u32, input: u32) {
        self.links.insert(id, Link { output, input });
    }

    /// FxSound's sink (`Output`) or source (`Input`) appeared under `id`, with `serial`.
    pub(crate) fn own_node_appeared(&mut self, direction: DeviceDirection, id: u32, serial: u64) {
        *self.ours.get_mut(direction) = Some((id, serial));
    }

    /// Something left the registry: a link, a stream, one of FxSound's nodes, or none of them.
    /// Whether it was a link: the engine then looks for streams it left with none at once
    /// ([`Self::due`]), so that a stream's wait starts when it lost its last link, not at the
    /// supervisor's next tick.
    pub(crate) fn removed(&mut self, id: u32) -> bool {
        let link = self.links.remove(&id).is_some();
        self.since.remove(&id);
        self.nudged.remove(&id);
        for direction in DeviceDirection::ALL {
            let ours = self.ours.get_mut(direction);
            if (*ours).is_some_and(|(own, _)| own == id) {
                *ours = None;
            }
        }
        link
    }

    /// The `object.serial` of FxSound's node of `direction`, while it is in the graph.
    pub(crate) fn our_serial(&self, direction: DeviceDirection) -> Option<u64> {
        (*self.ours.get(direction)).map(|(_, serial)| serial)
    }

    /// FxSound has just taken a default, or handed it back: every stream may be moved where it went
    /// [`NUDGES`] times again.
    pub(crate) fn claimed(&mut self) {
        self.nudged.clear();
    }

    /// Whether the stream `id`, of `direction`, has a link: a player one from it, a recorder one
    /// into it.
    fn linked(&self, id: u32, direction: DeviceDirection) -> bool {
        self.links.values().any(|link| match direction {
            DeviceDirection::Output => link.output == id,
            DeviceDirection::Input => link.input == id,
        })
    }

    /// Whether the stream `id`, of `direction`, is linked to FxSound's own node of that direction:
    /// a player into FxSound's sink, a recorder out of FxSound's source.
    pub(crate) fn linked_to_ours(&self, id: u32, direction: DeviceDirection) -> bool {
        let Some((own, _)) = *self.ours.get(direction) else {
            return false;
        };
        self.links.values().any(|link| match direction {
            DeviceDirection::Output => link.output == id && link.input == own,
            DeviceDirection::Input => link.output == own && link.input == id,
        })
    }

    /// Which of `followers` — streams that follow a default FxSound holds, with the direction of
    /// each — are stranded as of `now`, and are to be moved onto FxSound's node now. Each one
    /// returned is counted, and waits [`STRANDED_AFTER`] again before it is returned again. A
    /// stream not among `followers` is forgotten: it went, took a target of its own, or its
    /// default is not FxSound's any more.
    pub(crate) fn due(
        &mut self,
        followers: &[(u32, DeviceDirection)],
        now: Instant,
    ) -> Vec<(u32, DeviceDirection)> {
        self.since
            .retain(|id, _| followers.iter().any(|(follower, _)| follower == id));
        let mut due = Vec::new();
        for &(id, direction) in followers {
            if self.linked(id, direction) {
                self.since.remove(&id);
                continue;
            }
            let since = *self.since.entry(id).or_insert(now);
            let nudged = self.nudged.entry(id).or_default();
            if now.saturating_duration_since(since) >= STRANDED_AFTER && *nudged < NUDGES {
                *nudged += 1;
                self.since.insert(id, now);
                due.push((id, direction));
            }
        }
        due
    }

    /// The session ended, and the graph with it.
    pub(crate) fn forget_session(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IN: DeviceDirection = DeviceDirection::Input;
    const OUT: DeviceDirection = DeviceDirection::Output;

    #[test]
    fn a_follower_is_stranded_only_after_a_quarter_of_a_second_with_no_link_at_all() {
        let mut stranded = Stranded::default();
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        // A recorder linked from the source: nothing to do.
        stranded.link_appeared(100, 63, 78);
        assert!(stranded.due(&[(78, IN)], at(0)).is_empty());
        // Moved: its link goes, and the next one comes a moment later.
        assert!(stranded.removed(100));
        assert!(stranded.due(&[(78, IN)], at(10)).is_empty());
        stranded.link_appeared(101, 30, 78);
        assert!(stranded.due(&[(78, IN)], at(600)).is_empty());
        // Moved onto nothing: stranded once a quarter of a second has gone by.
        assert!(stranded.removed(101));
        assert!(stranded.due(&[(78, IN)], at(700)).is_empty());
        assert!(stranded.due(&[(78, IN)], at(900)).is_empty());
        assert_eq!(stranded.due(&[(78, IN)], at(950)), [(78, IN)]);
    }

    #[test]
    fn only_a_link_leaving_asks_for_a_look_at_once() {
        let mut stranded = Stranded::default();
        stranded.link_appeared(100, 63, 78);
        stranded.own_node_appeared(IN, 63, 1_055);
        assert!(!stranded.removed(78), "a stream is no link");
        assert!(!stranded.removed(63), "nor FxSound's own node");
        assert!(stranded.removed(100));
        assert!(!stranded.removed(100), "and a link goes once");
    }

    #[test]
    fn a_player_counts_its_links_out_and_a_recorder_its_links_in() {
        let mut stranded = Stranded::default();
        let start = Instant::now();
        let later = start + STRANDED_AFTER;
        // The player 77 feeds the sink 60; the recorder 78 only appears as a link's output here.
        stranded.link_appeared(1, 77, 60);
        stranded.link_appeared(2, 78, 90);
        let followers = [(77, OUT), (78, IN)];
        assert!(stranded.due(&followers, start).is_empty());
        assert_eq!(stranded.due(&followers, later), [(78, IN)]);
    }

    #[test]
    fn a_player_is_on_fxsound_linked_into_its_sink_and_a_recorder_linked_out_of_its_source() {
        let mut stranded = Stranded::default();
        stranded.own_node_appeared(OUT, 60, 1_050);
        stranded.own_node_appeared(IN, 63, 1_055);
        // The player 77 plays into FxSound's sink, the player 79 into a device; the recorder 78
        // records FxSound's source, the recorder 80 a microphone.
        stranded.link_appeared(1, 77, 60);
        stranded.link_appeared(2, 79, 90);
        stranded.link_appeared(3, 63, 78);
        stranded.link_appeared(4, 91, 80);
        assert!(stranded.linked_to_ours(77, OUT));
        assert!(!stranded.linked_to_ours(79, OUT));
        assert!(stranded.linked_to_ours(78, IN));
        assert!(!stranded.linked_to_ours(80, IN));
        // Each in its own direction only, and not once the link, or FxSound's node, has gone.
        assert!(!stranded.linked_to_ours(78, OUT));
        assert!(stranded.removed(1));
        assert!(!stranded.linked_to_ours(77, OUT));
        assert!(!stranded.removed(63));
        assert!(!stranded.linked_to_ours(78, IN));
    }

    #[test]
    fn a_stranded_stream_is_moved_twice_at_most_until_fxsound_takes_the_default_again() {
        let mut stranded = Stranded::default();
        let start = Instant::now();
        let step = STRANDED_AFTER;
        let followers = [(78, IN)];
        assert!(stranded.due(&followers, start).is_empty());
        assert_eq!(stranded.due(&followers, start + step), [(78, IN)]);
        // Waits again after each move.
        assert!(stranded.due(&followers, start + step + step / 2).is_empty());
        assert_eq!(stranded.due(&followers, start + step * 2), [(78, IN)]);
        assert!(stranded.due(&followers, start + step * 4).is_empty());
        assert!(stranded.due(&followers, start + step * 8).is_empty());
        stranded.claimed();
        assert_eq!(stranded.due(&followers, start + step * 9), [(78, IN)]);
    }

    #[test]
    fn a_stream_that_stops_following_starts_its_wait_over_when_it_follows_again() {
        let mut stranded = Stranded::default();
        let start = Instant::now();
        let step = STRANDED_AFTER;
        assert!(stranded.due(&[(78, IN)], start).is_empty());
        // The default is not FxSound's for a while: nobody follows it.
        assert!(stranded.due(&[], start + step).is_empty());
        assert!(stranded.due(&[(78, IN)], start + step * 3).is_empty());
        assert_eq!(stranded.due(&[(78, IN)], start + step * 4), [(78, IN)]);
    }

    #[test]
    fn fxsounds_own_nodes_are_known_by_serial_until_they_go() {
        let mut stranded = Stranded::default();
        stranded.own_node_appeared(IN, 63, 1_055);
        stranded.own_node_appeared(OUT, 60, 1_050);
        assert_eq!(stranded.our_serial(IN), Some(1_055));
        assert!(!stranded.removed(63));
        assert_eq!(stranded.our_serial(IN), None);
        assert_eq!(stranded.our_serial(OUT), Some(1_050));
        stranded.forget_session();
        assert_eq!(stranded.our_serial(OUT), None);
    }
}
