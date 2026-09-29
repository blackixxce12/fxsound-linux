//! The smooth handover of application streams: silence them for the moment a move takes, and give
//! them their volume back once they are where they were going.
//!
//! # Why
//!
//! WirePlumber moves a stream by unlinking it from one node and linking it to the next
//! (`linking/prepare-link.lua`, `linking/link-target.lua`), in the middle of a wave, with nothing
//! to smooth the cut: measured on PipeWire 1.6.9 and WirePlumber 0.5.17, every stream that follows
//! the default clicked at −18.8…−31.1 dBFS when FxSound's power went off or on and the default
//! moved with it. FxSound's own nodes fade what comes into them in (`lane_dsp`), so only the side a
//! stream leaves is heard, and FxSound cannot reach it — it is the application's audio, on the
//! application's node.
//!
//! What can reach it is the ramp PipeWire itself puts on a stream's master volume. The audio
//! converter in front of every `pw_stream` — `pipewire-pulse`'s streams included — takes, beside a
//! new `volume` in its `Props`, `volumeRampTime` and `volumeRampStepSamples`, and walks from the
//! old volume to the new one over that time, a few samples a step (`spa/plugins/audioconvert/
//! audioconvert.c`, `apply_props`, `generate_ramp_seq`; [`RAMP_STEP_SAMPLES`]). Only the master
//! `volume` moves; `channelVolumes`, the level a desktop's slider shows, stays. Faded to silence
//! over 20 ms, moved, and faded back once it has its new link, a stream's move measured
//! −50.5…−94.5 dBFS.
//!
//! # What this module is
//!
//! Plain data, as `app_streams` and `stranded` are: what the engine knows about each stream's
//! volume ([`Watched`]), the order a handover goes in ([`Handover`]), and the journal that lets a
//! later run put back a volume this one left at zero ([`Journal`]). The engine feeds it the
//! server's events and does what it says ([`Step`]); nothing here touches PipeWire, and the tests
//! drive it with made-up clocks.
//!
//! # The order of a handover
//!
//! 1. Every stream to be moved that is playing (`running`), has a master volume of its own
//!    (an audio converter) that is not locked (`channelmix.lock-volumes`) and is not silent
//!    already, has that volume noted — in the journal first — and is sent to 0 over [`RAMP_MS`].
//!    A stream standing still is left alone: the ramp is spent in its `process()`, so it would
//!    wait there for the next sound, and a stream that plays nothing clicks at nothing.
//! 2. The server's echo of each write — the stream's `Props`, now at 0 — starts its settling:
//!    the ramp itself, and then what the stream has buffered and a quantum ([`SETTLE`]).
//! 3. Once every stream has settled, the move is made ([`Step::Move`]) — whatever the caller
//!    wanted: a default written, a target set or deleted. The lanes' DSP is not held for it: the
//!    power switch reaches the chain at the press, through the app's snapshot, and dips there on
//!    its own (`fxsound_dsp::smooth::Dip`), before the streams that follow the default are faded
//!    (`docs/spec/12-audio-io.md`, step 4b).
//! 4. Each stream's next new link to become active — WirePlumber's, or the one `stranded` makes
//!    for a recorder WirePlumber failed to link — ends its move, and its volume is sent back over
//!    the same ramp: a player's at once, a recorder's [`RECORDER_LINKED`] later, once what its
//!    source plays has reached it. One that has none within [`LINK_WAIT`] gets its volume back all
//!    the same, and one whose move turned out to move nothing gets it back at once.
//! 5. The echo of the volume written back, and the ramp's own length after it ([`RAMP_IN_DONE`]),
//!    have the volume said once more, at once, where the ramp has taken it; the server reporting
//!    it ends the stream's handover, and takes its line out of the journal. Until it does, it is
//!    said again every [`CONFIRM_WAIT`], [`CONFIRM_TRIES`] times at most. A volume comes back over
//!    [`RAMP_IN_MS`], longer than the fade: what it comes back to may be a chain that has heard
//!    nothing for a while.
//!
//! Rules the measurements taught, on the same private graph:
//!
//! - One ramp at a time. The converter refuses a ramp while one is being applied ("volume ramp
//!   sequence is being applied try again") and then jumps to the new volume instead, so nothing is
//!   written back before the first write's echo, and the ramp's own length after it.
//! - A volume someone else writes meanwhile — WirePlumber lowering a stream for another's role, a
//!   mixer — is theirs: the handover lets that stream go and writes nothing back.
//! - An echo that never comes (an application that has stopped answering holds the request) does
//!   not hold the move up: after [`ECHO_WAIT`] the stream counts as settled, whatever it is.
//!
//! # A stream without the ramp
//!
//! The ramp arrived in PipeWire 0.3.68 ([`RAMPS_SINCE`]); Debian 12 ships 0.3.65. A volume written
//! to 0 without it would jump — a click of its own — so a stream without it is moved alone, at
//! once, as before 0.5.0 ([`Handover::begin`]). The ramp is the audio converter's, and the
//! converter runs in the process of the stream's own client, of that client's libpipewire: a
//! `pipewire-pulse` stream's is the server's, but an application from an older Flatpak runtime, or
//! with a libpipewire of its own, may play natively on a newer server with a converter that
//! ignores `volumeRampTime` and jumps. Its `PropInfo` says nothing of the ramp either way — 1.6.9's
//! converter lists no `volumeRamp*` there — but its client says its libpipewire's version
//! (`core.version`), and that decides, stream by stream ([`Watched::converter_ramps`]). A client
//! that says none is taken at the server's word, as every stream was before.
//!
//! # The journal
//!
//! A stream left at 0 is not only silent until FxSound puts its volume back. WirePlumber keeps a
//! stream's master volume in its state (`node/state-stream.lua`, `store_stream_props_hook`) and
//! gives it back to the application's next stream (`restore_stream_hook`): a `pipewire-pulse`
//! client left at 0 for 2.5 s was saved at `"volume":0.0` and, started again, played silence while
//! `pactl` showed 100 % — nothing a user could see or undo. So before a stream is faded its volume
//! goes into `$XDG_STATE_HOME/fxsound/handover.toml` under the key WirePlumber stores it under
//! ([`state_key`]), and it comes out again once the volume is back. A line still there when the
//! engine meets a stream of that application again — after FxSound was killed in the middle of a
//! handover, or the application closed during one — is put back if the stream is at 0
//! ([`Journal::repair`]). The line is dropped once the application plays at a volume of its own and
//! none of its streams can still be the one left at 0: every stream the engine has bound is known,
//! and none of the application's is silent or of a volume not in yet ([`Journal::outlived`]). A
//! handover fades only the streams that play, so a paused second tab of a browser met at its own
//! volume says nothing of the tab left at 0 — which the engine may meet a moment later. While a
//! handover still holds a stream of the application at 0 the line is that stream's: another stream
//! of the application met meanwhile is put back if it is at 0, and the line stays for the handover
//! to take out ([`Handover::holds_key`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The first PipeWire whose audio converter ramps its master volume: `volumeRamp*` are not in
/// `spa/include/spa/param/props.h` at 0.3.67 and are at 0.3.68.
pub(crate) const RAMPS_SINCE: (u32, u32, u32) = (0, 3, 68);

/// How long a fade takes, in milliseconds: `volumeRampTime`. With a step of one sample
/// (`volumeRampStepSamples`), a move with 20 ms on either side measured −70.4 dBFS where 10 ms
/// measured −64.4 and the move without them −18.8; longer only lengthens the gap in the sound.
pub(crate) const RAMP_MS: i32 = 20;

/// How many samples each step of a ramp holds: `volumeRampStepSamples`.
///
/// Not one. The converter takes a ramped write in two moves on the thread the write arrives on:
/// it applies the new volume at once, and only then builds the ramp — a `Props` object for every
/// step, in a pod it grows 4 KiB at a time — and hands it to the data thread
/// (`audioconvert.c` 1.6, `apply_props`, `generate_ramp_seq`). A cycle the data thread plays in
/// between plays at the new volume, and the ramp then starts from the old one: before a fade in,
/// a whole quantum at full level, then silence, then the ramp. Measured so in the click test: a
/// PulseAudio recorder given its volume back after the power switch played 512 frames at −17.7
/// dBFS, now and then. Without FxSound, on a private graph, a `parec` stream faded out and in over
/// 50 ms with `pw-cli`: at one sample a step — 2400 steps to build — 21 writes in 2400 did it; at
/// 8 samples 1 in 1600, at 4 and at 16 none in 800. Fewer steps make it rarer and do not end it:
/// the race is PipeWire's. Eight samples make a fade 120 steps and a fade in 300, each moving the
/// gain by 1/120 or 1/300 of the way: a staircase at 6 kHz whose residual lies under what the
/// click test measures of the moves (`graph_churn::clicks`).
pub(crate) const RAMP_STEP_SAMPLES: i32 = 8;

/// How long after a write's echo its ramp has surely been played out, and a second one would be
/// taken: [`RAMP_MS`] and a margin.
pub(crate) const RAMP_DONE: Duration = Duration::from_millis(30);

/// How long a volume given back takes, in milliseconds: longer than the fade, because what the
/// stream comes back to may be a chain that has heard nothing for a while. A stream moved onto an
/// application's own route — a chain that has never heard it — came back at −40.6 to −43.8 dBFS
/// over 20 ms, the levelling and the limiter taking its onset at full level, and at −47 over 50
/// ms; FxSound's own sink, at the power button, the same or better. At −20 dB within the first
/// 5 ms of it, it lengthens the gap in the sound by next to nothing.
pub(crate) const RAMP_IN_MS: i32 = 50;

/// How long after the echo of a volume given back its ramp has surely been played out: the stream
/// is held until then, so that the next handover's fade is not refused for it, and its volume is
/// then said once more where the ramp has taken it ([`RAMP_IN_MS`], and a margin for a quantum of
/// up to 2048 frames, over which a ramp is spent a cycle at a time).
pub(crate) const RAMP_IN_DONE: Duration = Duration::from_millis(100);

/// How long the volume said once more after a ramp back ([`RAMP_IN_DONE`]) may go without the
/// server reporting it before it is said again. The echo of that write can go missing: seen on
/// PipeWire 1.6.9, a converter that had reported a point 0.10 of the way up its ramp answered the
/// write of the volume it had come to with nothing, and the server kept showing 0.10 — what the
/// next handover then read as the stream's volume, faded from and gave back, leaving the
/// application 20 dB down for good. Its echo takes under a millisecond when it comes.
pub(crate) const CONFIRM_WAIT: Duration = Duration::from_millis(50);

/// How many times the volume is said once more after a ramp back before the handover lets the
/// stream go without hearing it ([`CONFIRM_WAIT`]).
pub(crate) const CONFIRM_TRIES: u8 = 4;

/// How long after its fade's echo a stream counts as silent where it is heard: the ramp, then
/// what the stream and the device behind it have buffered, and a quantum.
pub(crate) const SETTLE: Duration = Duration::from_millis(70);

/// How long a moved stream waits for its new link before its volume is put back regardless. A
/// recorder WirePlumber failed to link is linked by `stranded` once it has had no link for
/// `STRANDED_AFTER` (250 ms from when its last link went), on the supervisor's next tick (200 ms):
/// within half a second of its move, and a shorter wait than this one's would, on a slow server,
/// put its volume back before its link, and start its recording with a step.
pub(crate) const LINK_WAIT: Duration = Duration::from_secs(1);

/// How long a recorder's new link has to have been active before its volume comes back. A recorder
/// is handed what its source played in the cycle before: the first cycles after its link is active
/// bring it nothing yet, and a ramp written at once is spent on them — within one cycle, at the
/// default quantum of 1024 frames — and the sound then starts at full volume in the middle of a
/// wave. Measured so: a recorder moved by the power switch clicked at −21 to −34 dBFS with its
/// volume given back as its link became active. Two cycles at a quantum of 1024, and a margin. A
/// player's own sound is there from its first cycle, and it gets its volume back at once.
pub(crate) const RECORDER_LINKED: Duration = Duration::from_millis(50);

/// How long a write's echo may take before the write is taken as not made, or not to be heard
/// of: an application that no longer answers its server holds it.
pub(crate) const ECHO_WAIT: Duration = Duration::from_millis(300);

/// Below this a master volume is silent, and within it of another the two are the same:
/// WirePlumber compares volumes to four decimals (`numbersEqualRounded` in `state-stream.lua`).
const SAME: f32 = 1e-4;

/// The journal's file name, under `$XDG_STATE_HOME/fxsound`.
pub(crate) const JOURNAL_FILE: &str = "handover.toml";

/// The most lines the journal keeps. Each is an application met at 0 after a handover it never
/// came back from; the oldest go first.
const JOURNAL_LIMIT: usize = 64;

// ---------------------------------------------------------------------------------------------
// What the engine knows about a stream
// ---------------------------------------------------------------------------------------------

/// What the engine knows about one application stream's volume: from its properties, its state,
/// and its `Props`, which it subscribes to.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Watched {
    /// The key WirePlumber keeps the stream's volume under ([`state_key`]); `None` until its
    /// properties are in, or when it has nothing to be kept under.
    pub(crate) key: Option<String>,
    /// `object.serial`: which stream this is, in the log and the journal.
    pub(crate) serial: Option<u64>,
    /// It is playing: its node is `running`.
    pub(crate) running: bool,
    /// Its master volume, `Props`' `volume`; `None` for a stream with no audio converter, or whose
    /// `Props` have not arrived.
    pub(crate) level: Option<f32>,
    /// `channelmix.lock-volumes`: its converter ignores every volume written to it.
    pub(crate) locked: bool,
    /// Its properties are in: `key` and `serial` are what they will be. A stream is watched from
    /// the moment it is bound, before anything of it is known ([`Journal::outlived`]).
    pub(crate) known: bool,
    /// It records (`Stream/Input/Audio`): its volume comes back [`RECORDER_LINKED`] after its new
    /// link is active, not at once.
    pub(crate) records: bool,
    /// `client.id`: the connection the stream is on, whose libpipewire its audio converter is of
    /// (module docs, "A stream without the ramp").
    pub(crate) client: Option<u32>,
    /// Whether the stream's audio converter ramps a volume, as its client's libpipewire says;
    /// `None` when that is not known, and the server's word is taken ([`Handover::set_ramps`]).
    pub(crate) converter_ramps: Option<bool>,
    /// FxSound's hook in WirePlumber holds the stream silent for a move of WirePlumber's own
    /// (`crate::wireplumber_hook`, [`crate::wireplumber_hook::HELD_KEY`]): the volume it reports
    /// may be a point of the hook's fade, and is not the stream's to note, fade or give back.
    pub(crate) hook_held: bool,
}

impl Watched {
    /// Whether a handover fades this stream: it plays, it has a volume to ramp and may have it
    /// changed, it is not silent already, and FxSound's hook in WirePlumber is not holding it
    /// silent — as a stream silent already, the hook's is moved without a fade of FxSound's, and
    /// the hook gives it its volume back.
    #[must_use]
    pub(crate) fn fades(&self) -> bool {
        self.running
            && !self.locked
            && !self.hook_held
            && self.level.is_some_and(|level| level > SAME)
    }
}

/// The key WirePlumber 0.5 keeps a stream's volume and target under (`formKey` in
/// `node/state-stream.lua`): its `media.class` without `Stream/`, then `media.role:Notification`
/// for a notification, or the first of `application.id`, `application.name`, `media.name` and
/// `node.name` it has, with its value. `None` for a stream with none of them, or no class.
#[must_use]
pub(crate) fn state_key<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> Option<String> {
    let class = get("media.class")?;
    let class = class.strip_prefix("Stream/").unwrap_or(class);
    if get("media.role") == Some("Notification") {
        return Some(format!("{class}:media.role:Notification"));
    }
    [
        "application.id",
        "application.name",
        "media.name",
        "node.name",
    ]
    .into_iter()
    .find_map(|key| get(key).map(|value| format!("{class}:{key}:{value}")))
}

pub(crate) fn silent(level: f32) -> bool {
    level <= SAME
}

pub(crate) fn same(a: f32, b: f32) -> bool {
    (a - b).abs() <= SAME
}

/// Whether `level` lies strictly between silence and `saved`: a point a ramp between the two
/// passes through.
fn on_the_way(level: f32, saved: f32) -> bool {
    level > SAME && level < saved - SAME
}

// ---------------------------------------------------------------------------------------------
// The handover
// ---------------------------------------------------------------------------------------------

/// What the engine is to do next for a handover.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// Write `level` to the stream's master volume, over `ramp_ms` milliseconds: [`RAMP_MS`] to
    /// silence, [`RAMP_IN_MS`] back, and 0 — at once — to say where a ramp back has ended.
    Volume { id: u32, level: f32, ramp_ms: i32 },
    /// Put the stream's volume in the journal before it is faded.
    Remember {
        key: String,
        serial: Option<u64>,
        level: f32,
    },
    /// Take an application's line out of the journal: its volume is back, or somebody else's.
    Forget { key: String },
    /// Make the move: every stream that fades is silent.
    Move,
    /// The handover is over; the next may begin.
    Finished,
}

/// Where one faded stream is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Its fade to 0 is written, and the echo has not come back.
    FadingOut { sent: Instant },
    /// The echo came: the ramp plays out until `ramped`, and what is buffered drains until
    /// `settled`. `last` is the lowest volume reported on the way down so far: a converter at a
    /// small quantum may report more than one point of the same ramp.
    Silent {
        ramped: Instant,
        settled: Instant,
        last: f32,
    },
    /// Silent where it is heard, and ready to be moved.
    Settled,
    /// The echo never came within [`ECHO_WAIT`]: whether it is silent is not known, and the move
    /// does not wait for it any longer.
    Unconfirmed,
    /// Moved: waiting for its new link, until `until`.
    Moved { until: Instant },
    /// A recorder whose new link is active: its volume comes back at `at` ([`RECORDER_LINKED`]).
    Linked { at: Instant },
    /// Its volume is written back, and the echo has not come.
    FadingIn { sent: Instant },
    /// The echo of its volume came: the ramp back plays out until `done` ([`RAMP_IN_DONE`]).
    Restoring { done: Instant },
    /// Its ramp back is over, and its volume has been said once more, at once, where the ramp has
    /// taken it: the stream is let go once the server reports that volume, and the volume is said
    /// again every [`CONFIRM_WAIT`] until it does — `tries` times at most ([`CONFIRM_TRIES`]).
    Confirming { sent: Instant, tries: u8 },
}

/// One stream a handover faded.
#[derive(Debug, Clone)]
struct Faded {
    id: u32,
    key: Option<String>,
    /// Its master volume before the fade: what it gets back.
    saved: f32,
    /// It records ([`Watched::records`]).
    records: bool,
    phase: Phase,
}

/// The handover in progress.
#[derive(Debug)]
struct Batch {
    streams: Vec<Faded>,
    /// The move has been made.
    moved: bool,
    /// The engine is closing: every stream gets its volume back as soon as it may, and the move
    /// is not made ([`Handover::restore_all`]).
    leaving: bool,
}

/// One handover at a time (module docs, "The order of a handover").
#[derive(Debug, Default)]
pub(crate) struct Handover {
    /// Whether the server ramps a volume ([`RAMPS_SINCE`]).
    ramps: bool,
    batch: Option<Batch>,
}

impl Handover {
    /// Tell the handover whether the server ramps a volume: learned from the server's version on
    /// each connection, and `false` until then. The word for a stream whose own converter is not
    /// known ([`Watched::converter_ramps`]).
    pub(crate) fn set_ramps(&mut self, ramps: bool) {
        self.ramps = ramps;
    }

    /// Whether a handover is in progress: the next has to wait for [`Step::Finished`].
    #[must_use]
    pub(crate) const fn busy(&self) -> bool {
        self.batch.is_some()
    }

    /// Whether the stream under `id` is one this handover faded and has not let go of.
    #[must_use]
    pub(crate) fn holds(&self, id: u32) -> bool {
        self.batch
            .as_ref()
            .is_some_and(|batch| batch.streams.iter().any(|stream| stream.id == id))
    }

    /// Whether this handover faded a stream of the application `key` and has not let go of it:
    /// that application's journal line is then the handover's, and only its own
    /// [`Step::Forget`] takes it out — not another stream of the same application met meanwhile.
    #[must_use]
    pub(crate) fn holds_key(&self, key: &str) -> bool {
        self.batch.as_ref().is_some_and(|batch| {
            batch
                .streams
                .iter()
                .any(|stream| stream.key.as_deref() == Some(key))
        })
    }

    /// Begin a handover of `streams`, each with what is known of it. The streams that fade are
    /// noted and sent to 0; with none — none plays, or none has a converter that ramps
    /// ([`Watched::converter_ramps`], and the server's word for a stream whose is not known) — the
    /// move is made at once and the handover is over in the same breath.
    ///
    /// Only when not [`Self::busy`]; the engine queues a handover asked for meanwhile.
    pub(crate) fn begin(&mut self, streams: &[(u32, &Watched)], now: Instant) -> Vec<Step> {
        debug_assert!(!self.busy(), "one handover at a time");
        let mut steps = Vec::new();
        let mut faded: Vec<Faded> = Vec::new();
        for &(id, watched) in streams {
            // A stream whose converter would jump to the volume written is moved as it is.
            if !watched.converter_ramps.unwrap_or(self.ramps) {
                continue;
            }
            let Some(level) = watched.level.filter(|_| watched.fades()) else {
                continue;
            };
            if faded.iter().any(|stream| stream.id == id) {
                continue;
            }
            if let Some(key) = &watched.key
                && !faded.iter().any(|stream| stream.key.as_ref() == Some(key))
            {
                steps.push(Step::Remember {
                    key: key.clone(),
                    serial: watched.serial,
                    level,
                });
            }
            steps.push(Step::Volume {
                id,
                level: 0.0,
                ramp_ms: RAMP_MS,
            });
            faded.push(Faded {
                id,
                key: watched.key.clone(),
                saved: level,
                records: watched.records,
                phase: Phase::FadingOut { sent: now },
            });
        }
        if faded.is_empty() {
            steps.extend([Step::Move, Step::Finished]);
            return steps;
        }
        self.batch = Some(Batch {
            streams: faded,
            moved: false,
            leaving: false,
        });
        steps
    }

    /// The stream under `id` reported its master volume, `level`: an echo of a write of ours, or
    /// somebody else's write.
    pub(crate) fn volume(&mut self, id: u32, level: Option<f32>, now: Instant) -> Vec<Step> {
        let mut steps = Vec::new();
        let Some(batch) = self.batch.as_mut() else {
            return steps;
        };
        let Some(index) = batch.streams.iter().position(|stream| stream.id == id) else {
            return steps;
        };
        let stream = &mut batch.streams[index];
        // A stream whose volume can no longer be read is left as it is: nothing is known to put
        // back, nor to write to.
        let Some(level) = level else {
            let gone = batch.streams.remove(index);
            steps.extend(forget_if_last(&batch.streams, gone.key));
            steps.extend(self.advance(now));
            return steps;
        };
        let saved = stream.saved;
        let ours = match stream.phase {
            // The echo — or the ramp under way: the converter reports the volume it has come to
            // when its `Props` are sent again in the middle of a ramp longer than a cycle, and
            // may never report the end of it.
            Phase::FadingOut { .. } | Phase::Unconfirmed
                if silent(level) || on_the_way(level, saved) =>
            {
                stream.phase = Phase::Silent {
                    ramped: now + RAMP_DONE,
                    settled: now + SETTLE,
                    last: level,
                };
                true
            }
            // What the stream said before our write reached it.
            Phase::FadingOut { .. } | Phase::Unconfirmed => same(level, saved),
            // A further point of the same ramp down: not above the last one reported. The ramp
            // began with the first report, so its end is not moved.
            Phase::Silent {
                ramped,
                settled,
                last,
            } if silent(level) || (on_the_way(level, saved) && level <= last + SAME) => {
                stream.phase = Phase::Silent {
                    ramped,
                    settled,
                    last: level,
                };
                true
            }
            Phase::Silent { .. } | Phase::Settled | Phase::Moved { .. } | Phase::Linked { .. } => {
                silent(level)
            }
            Phase::FadingIn { .. } if same(level, saved) || on_the_way(level, saved) => {
                stream.phase = Phase::Restoring {
                    done: now + RAMP_IN_DONE,
                };
                steps.extend(self.advance(now));
                return steps;
            }
            Phase::FadingIn { .. } => silent(level),
            Phase::Restoring { .. } => same(level, saved) || on_the_way(level, saved),
            // The server has the volume back: the stream's handover is over.
            Phase::Confirming { .. } if same(level, saved) => {
                let done = batch.streams.remove(index);
                steps.extend(forget_if_last(&batch.streams, done.key));
                steps.extend(self.advance(now));
                return steps;
            }
            // A point of the ramp back, reported late: the volume is said again when it is due.
            Phase::Confirming { .. } => on_the_way(level, saved),
        };
        if !ours {
            // Somebody else's volume: theirs to keep.
            let theirs = batch.streams.remove(index);
            steps.extend(forget_if_last(&batch.streams, theirs.key));
        }
        steps.extend(self.advance(now));
        steps
    }

    /// The stream under `id` has a new link, active: after the move, its move is over, and its
    /// volume goes back — a player's now, a recorder's [`RECORDER_LINKED`] from now. Before the
    /// move a link is not the one being waited for, and changes nothing.
    pub(crate) fn linked(&mut self, id: u32, now: Instant) -> Vec<Step> {
        let Some(stream) = self
            .batch
            .as_mut()
            .and_then(|batch| batch.streams.iter_mut().find(|stream| stream.id == id))
        else {
            return Vec::new();
        };
        if !matches!(stream.phase, Phase::Moved { .. }) {
            return Vec::new();
        }
        if stream.records {
            stream.phase = Phase::Linked {
                at: now + RECORDER_LINKED,
            };
            return Vec::new();
        }
        stream.phase = Phase::FadingIn { sent: now };
        vec![Step::Volume {
            id,
            level: stream.saved,
            ramp_ms: RAMP_IN_MS,
        }]
    }

    /// The stream under `id` has left the graph. Its line stays in the journal: WirePlumber may
    /// have kept the 0 it left at for the application's next stream.
    pub(crate) fn removed(&mut self, id: u32, now: Instant) -> Vec<Step> {
        let Some(batch) = self.batch.as_mut() else {
            return Vec::new();
        };
        batch.streams.retain(|stream| stream.id != id);
        self.advance(now)
    }

    /// The engine is closing, or its connection has, or the move moved nothing: every faded stream
    /// gets its volume back as soon as a ramp may be written to it, without waiting for a new
    /// link, and a move not made yet is not made — the caller makes it, or has no graph to make it
    /// in.
    pub(crate) fn restore_all(&mut self, now: Instant) -> Vec<Step> {
        if let Some(batch) = self.batch.as_mut() {
            batch.leaving = true;
        }
        self.advance(now)
    }

    /// Forget the handover without writing anything: the connection it was made on is gone, and
    /// the streams with it. Their lines stay in the journal, for the next connection to find.
    pub(crate) fn abandon(&mut self) {
        self.batch = None;
    }

    /// What time has made due: an echo waited for too long, a stream settled, a link waited for
    /// too long — and the move, once every stream is ready for it.
    pub(crate) fn tick(&mut self, now: Instant) -> Vec<Step> {
        self.advance(now)
    }

    /// When [`Self::tick`] has something to do next, if anything.
    #[must_use]
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let batch = self.batch.as_ref()?;
        batch
            .streams
            .iter()
            .filter_map(|stream| match stream.phase {
                Phase::FadingOut { sent } | Phase::FadingIn { sent } => Some(sent + ECHO_WAIT),
                Phase::Silent { ramped, .. } if batch.leaving => Some(ramped),
                Phase::Silent { settled, .. } => Some(settled),
                Phase::Moved { until } => Some(until),
                Phase::Linked { at } => Some(at),
                Phase::Restoring { done } => Some(done),
                Phase::Confirming { sent, .. } => Some(sent + CONFIRM_WAIT),
                Phase::Settled | Phase::Unconfirmed => None,
            })
            .min()
    }

    fn advance(&mut self, now: Instant) -> Vec<Step> {
        let mut steps = Vec::new();
        let Some(batch) = self.batch.as_mut() else {
            return steps;
        };
        let mut given_up = Vec::new();
        let mut unheard = Vec::new();
        for stream in &mut batch.streams {
            match stream.phase {
                // A volume back and its ramp played out. The volume is written once more, at
                // once, where the ramp has taken it: the converter may have last reported a
                // point on the way, and what the server shows would otherwise stay there — what
                // the next handover reads as the stream's volume, and what WirePlumber keeps for
                // the application's next start. On the way out too.
                Phase::Restoring { done } if now >= done => {
                    stream.phase = Phase::Confirming {
                        sent: now,
                        tries: 1,
                    };
                    steps.push(Step::Volume {
                        id: stream.id,
                        level: stream.saved,
                        ramp_ms: 0,
                    });
                }
                Phase::Confirming { sent, tries } if now >= sent + CONFIRM_WAIT => {
                    if tries < CONFIRM_TRIES {
                        stream.phase = Phase::Confirming {
                            sent: now,
                            tries: tries + 1,
                        };
                        steps.push(Step::Volume {
                            id: stream.id,
                            level: stream.saved,
                            ramp_ms: 0,
                        });
                    } else {
                        unheard.push(stream.id);
                    }
                }
                Phase::FadingOut { sent } if now >= sent + ECHO_WAIT => {
                    stream.phase = Phase::Unconfirmed;
                }
                Phase::FadingIn { sent } if now >= sent + ECHO_WAIT => given_up.push(stream.id),
                Phase::Silent { settled, .. } if now >= settled => stream.phase = Phase::Settled,
                Phase::Moved { until: due } | Phase::Linked { at: due }
                    if now >= due || batch.leaving =>
                {
                    stream.phase = Phase::FadingIn { sent: now };
                    steps.push(Step::Volume {
                        id: stream.id,
                        level: stream.saved,
                        ramp_ms: RAMP_IN_MS,
                    });
                }
                _ => {}
            }
            if batch.leaving {
                let may_write = match stream.phase {
                    Phase::Silent { ramped, .. } => now >= ramped,
                    Phase::Settled | Phase::Unconfirmed => true,
                    _ => false,
                };
                if may_write {
                    stream.phase = Phase::FadingIn { sent: now };
                    steps.push(Step::Volume {
                        id: stream.id,
                        level: stream.saved,
                        ramp_ms: RAMP_IN_MS,
                    });
                }
            }
        }
        // A volume written back whose echo never came: the stream keeps its journal line, for
        // the next time the engine meets it.
        batch
            .streams
            .retain(|stream| !given_up.contains(&stream.id));
        // A volume said again and again without the server reporting it: the stream is let go
        // all the same. The write was made, and what it plays at is the volume; the journal line
        // goes with it, as a line for a stream at a volume of its own would.
        for id in unheard {
            if let Some(index) = batch.streams.iter().position(|stream| stream.id == id) {
                let done = batch.streams.remove(index);
                steps.extend(forget_if_last(&batch.streams, done.key));
            }
        }

        if !batch.moved && !batch.leaving {
            let ready = batch
                .streams
                .iter()
                .all(|stream| matches!(stream.phase, Phase::Settled | Phase::Unconfirmed));
            if ready {
                batch.moved = true;
                for stream in &mut batch.streams {
                    stream.phase = Phase::Moved {
                        until: now + LINK_WAIT,
                    };
                }
                steps.push(Step::Move);
            }
        }
        if (batch.moved || batch.leaving) && batch.streams.is_empty() {
            self.batch = None;
            steps.push(Step::Finished);
        }
        steps
    }
}

/// [`Step::Forget`] for `key`, unless another stream still faded — another stream of the same
/// application — keeps it in the journal.
fn forget_if_last(streams: &[Faded], key: Option<String>) -> Option<Step> {
    let key = key?;
    (!streams
        .iter()
        .any(|stream| stream.key.as_ref() == Some(&key)))
    .then_some(Step::Forget { key })
}

// ---------------------------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------------------------

/// The streams a handover faded and has not given their volume back yet, by application (module
/// docs, "The journal"). Written before a fade and after each volume is back, so a run that is
/// killed in between leaves its lines for the next.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Journal {
    #[serde(default, rename = "stream")]
    entries: Vec<Entry>,
}

/// One application's line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    /// WirePlumber's key for the application's streams ([`state_key`]).
    key: String,
    /// The faded stream's `object.serial`, for the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serial: Option<u64>,
    /// Its master volume before the fade.
    volume: f32,
}

/// What to do about a stream the journal has a line for ([`Journal::repair`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Repair {
    /// The journal says nothing about it.
    Nothing,
    /// It is at 0: put this volume back.
    Restore(f32),
    /// It plays at a volume of its own: the line is out of date.
    Obsolete,
}

impl Journal {
    /// The journal at `path`: empty when there is none, and when what is there cannot be read —
    /// said in the log, since a line lost is a volume a user may have to put back by hand.
    #[must_use]
    pub(crate) fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                log::warn!("could not read {}: {error}", path.display());
                return Self::default();
            }
        };
        match toml::from_str::<Self>(&text) {
            Ok(mut journal) => {
                journal
                    .entries
                    .retain(|entry| entry.volume.is_finite() && entry.volume >= 0.0);
                journal
            }
            Err(error) => {
                log::warn!(
                    "{} is not a journal FxSound can read: {error}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Write the journal to `path` — a whole new file put in place of the old, so a run killed
    /// while writing leaves the old one — or remove the file when there is nothing in it.
    ///
    /// # Errors
    /// Whatever creating the directory, writing or renaming the file failed with.
    pub(crate) fn save(&self, path: &Path) -> std::io::Result<()> {
        if self.entries.is_empty() {
            return match std::fs::remove_file(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        let partial = path.with_extension("toml.partial");
        std::fs::write(&partial, text)?;
        std::fs::rename(&partial, path)
    }

    /// Whether there is nothing in it.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Note `level` for the application `key` before its stream is faded. A line already there is
    /// kept: it is the volume from before an earlier handover that never came back, and the
    /// stream's volume now may be what that one left. Returns whether the journal changed.
    pub(crate) fn remember(&mut self, key: &str, serial: Option<u64>, level: f32) -> bool {
        if self.entries.iter().any(|entry| entry.key == key) {
            return false;
        }
        if self.entries.len() >= JOURNAL_LIMIT {
            self.entries.remove(0);
        }
        self.entries.push(Entry {
            key: key.to_owned(),
            serial,
            volume: level,
        });
        true
    }

    /// Take the application `key`'s line out. Returns whether there was one.
    pub(crate) fn forget(&mut self, key: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.key != key);
        self.entries.len() != before
    }

    /// What the journal says about a stream of the application `key` whose master volume is
    /// `level` ([`Repair`]). A stream whose volume is not known yet is left for when it is.
    #[must_use]
    pub(crate) fn repair(&self, key: &str, level: Option<f32>) -> Repair {
        let (Some(entry), Some(level)) =
            (self.entries.iter().find(|entry| entry.key == key), level)
        else {
            return Repair::Nothing;
        };
        if silent(level) {
            Repair::Restore(entry.volume)
        } else {
            Repair::Obsolete
        }
    }

    /// Whether the line for `key` has outlived the stream it was written for, given every stream
    /// the engine watches: a stream of the application plays at a volume of its own, and none can
    /// still be the one a handover left at 0 — every stream's properties are in, and none of the
    /// application's is silent or of a volume not known yet. A handover fades only the streams that
    /// play; a sibling heard at its own volume is no word on the one it faded, which a restarted
    /// engine may meet after it. `false` without a line.
    #[must_use]
    pub(crate) fn outlived<'a>(
        &self,
        key: &str,
        streams: impl IntoIterator<Item = &'a Watched> + Clone,
    ) -> bool {
        let of_key = |watched: &&Watched| watched.key.as_deref() == Some(key);
        let heard = |watched: &Watched| watched.level.is_some_and(|level| !silent(level));
        self.entries.iter().any(|entry| entry.key == key)
            && streams.clone().into_iter().filter(of_key).any(&heard)
            && streams
                .into_iter()
                .all(|watched| watched.known && (!of_key(&watched) || heard(watched)))
    }

    /// The applications the journal has a line for.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.key.as_str())
    }

    /// The serial the line for `key` was written for, for the log.
    #[must_use]
    pub(crate) fn serial(&self, key: &str) -> Option<u64> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .and_then(|entry| entry.serial)
    }
}

/// Where this user's journal is: [`JOURNAL_FILE`] in `fxsound/` under the state directory
/// ([`crate::volume::state_dir_in`]). `None` without one.
#[must_use]
pub(crate) fn journal_file() -> Option<PathBuf> {
    crate::volume::state_dir_in(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .map(|dir| dir.join("fxsound").join(JOURNAL_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(key: &str, level: f32) -> Watched {
        Watched {
            key: Some(key.to_owned()),
            serial: Some(40),
            running: true,
            level: Some(level),
            locked: false,
            known: true,
            records: false,
            client: None,
            converter_ramps: None,
            hook_held: false,
        }
    }

    fn ramping() -> Handover {
        let mut handover = Handover::default();
        handover.set_ramps(true);
        handover
    }

    fn after(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    fn volumes(steps: &[Step]) -> Vec<(u32, f32)> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Volume { id, level, .. } => Some((*id, *level)),
                _ => None,
            })
            .collect()
    }

    fn moves(steps: &[Step]) -> bool {
        steps.contains(&Step::Move)
    }

    /// Fade one stream out and have it echo and settle: the move is due at `+70 ms`.
    fn faded_one(handover: &mut Handover, start: Instant) {
        let player = playing("Output/Audio:application.name:Player", 0.8);
        let steps = handover.begin(&[(7, &player)], start);
        assert_eq!(volumes(&steps), [(7, 0.0)]);
        assert!(handover.volume(7, Some(0.0), after(start, 5)).is_empty());
    }

    #[test]
    fn a_server_without_the_ramp_moves_at_once_and_writes_no_volume() {
        let mut handover = Handover::default();
        let player = playing("Output/Audio:application.name:Player", 0.8);
        let steps = handover.begin(&[(7, &player)], Instant::now());
        assert_eq!(steps, [Step::Move, Step::Finished]);
        assert!(!handover.busy());
    }

    #[test]
    fn a_stream_standing_still_is_moved_without_a_fade() {
        let mut handover = ramping();
        let paused = Watched {
            running: false,
            ..playing("Output/Audio:application.name:Paused", 0.8)
        };
        let steps = handover.begin(&[(7, &paused)], Instant::now());
        assert_eq!(steps, [Step::Move, Step::Finished]);
    }

    #[test]
    fn a_stream_with_a_locked_or_no_volume_or_at_zero_is_moved_without_a_fade() {
        let mut handover = ramping();
        let locked = Watched {
            locked: true,
            ..playing("Output/Audio:application.name:A", 0.8)
        };
        let unknown = Watched {
            level: None,
            ..playing("Output/Audio:application.name:B", 0.8)
        };
        let quiet = playing("Output/Audio:application.name:C", 0.0);
        let steps = handover.begin(&[(1, &locked), (2, &unknown), (3, &quiet)], Instant::now());
        assert_eq!(steps, [Step::Move, Step::Finished]);
    }

    #[test]
    fn a_stream_whose_converter_has_no_ramp_is_moved_without_a_fade_on_a_server_that_has_one() {
        // An application with a libpipewire of its own older than 0.3.68, on a newer server: its
        // converter would jump to 0 and back, two clicks where the move alone makes one.
        let mut handover = ramping();
        let old = Watched {
            converter_ramps: Some(false),
            ..playing("Output/Audio:application.name:Old", 0.8)
        };
        let steps = handover.begin(&[(7, &old)], Instant::now());
        assert_eq!(steps, [Step::Move, Step::Finished]);

        // The converter is the client's, not the server's: one that ramps is faded whatever the
        // server's word, and one whose client is not known is taken at it.
        let mut handover = Handover::default();
        let new = Watched {
            converter_ramps: Some(true),
            ..playing("Output/Audio:application.name:New", 0.8)
        };
        let unknown = playing("Output/Audio:application.name:Unknown", 0.8);
        let steps = handover.begin(&[(7, &new), (8, &unknown)], Instant::now());
        assert_eq!(volumes(&steps), [(7, 0.0)]);
    }

    #[test]
    fn a_stream_the_hook_in_wireplumber_holds_is_moved_without_a_fade_and_its_level_not_noted() {
        // The hook faded it for the desktop's pick, and the last volume the converter reported is
        // a point on the way down: not the stream's own, to journal, fade from and give back.
        let mut handover = ramping();
        let held = Watched {
            hook_held: true,
            ..playing("Output/Audio:application.name:Player", 0.2)
        };
        assert!(!held.fades());
        let steps = handover.begin(&[(7, &held)], Instant::now());
        assert_eq!(steps, [Step::Move, Step::Finished]);
        assert!(!handover.busy());
    }

    #[test]
    fn a_volume_is_journalled_before_its_stream_is_faded() {
        let mut handover = ramping();
        let player = playing("Output/Audio:application.name:Player", 0.8);
        let steps = handover.begin(&[(7, &player)], Instant::now());
        assert_eq!(
            steps,
            [
                Step::Remember {
                    key: "Output/Audio:application.name:Player".to_owned(),
                    serial: Some(40),
                    level: 0.8,
                },
                Step::Volume {
                    id: 7,
                    level: 0.0,
                    ramp_ms: RAMP_MS
                },
            ]
        );
        assert!(handover.busy() && handover.holds(7));
    }

    #[test]
    fn the_move_waits_until_every_fade_has_echoed_and_settled() {
        let start = Instant::now();
        let mut handover = ramping();
        let a = playing("Output/Audio:application.name:A", 0.8);
        let b = playing("Output/Audio:application.name:B", 0.5);
        handover.begin(&[(1, &a), (2, &b)], start);

        assert!(!moves(&handover.volume(1, Some(0.0), after(start, 5))));
        assert!(
            !moves(&handover.tick(after(start, 100))),
            "the second stream's echo has not come"
        );
        assert!(!moves(&handover.volume(2, Some(0.0), after(start, 110))));
        assert_eq!(handover.next_deadline(), Some(after(start, 180)));
        assert!(!moves(&handover.tick(after(start, 179))));
        assert!(moves(&handover.tick(after(start, 180))));
        assert!(!moves(&handover.tick(after(start, 200))), "one move");
    }

    #[test]
    fn nothing_is_written_back_before_the_fade_has_echoed_and_ramped() {
        let start = Instant::now();
        let mut handover = ramping();
        let player = playing("Output/Audio:application.name:Player", 0.8);
        handover.begin(&[(7, &player)], start);

        // The engine closes before the echo: the volume waits for it.
        assert!(volumes(&handover.restore_all(after(start, 2))).is_empty());
        // What the stream said before the write is not the echo.
        assert!(volumes(&handover.volume(7, Some(0.8), after(start, 3))).is_empty());
        assert!(volumes(&handover.volume(7, Some(0.0), after(start, 4))).is_empty());
        assert!(
            volumes(&handover.tick(after(start, 33))).is_empty(),
            "the fade's ramp is still being played"
        );
        assert_eq!(volumes(&handover.tick(after(start, 34))), [(7, 0.8)]);
        // A link now is not the move's: no second write.
        assert!(handover.linked(7, after(start, 40)).is_empty());
    }

    #[test]
    fn a_new_link_after_the_move_puts_the_volume_back_at_once() {
        let start = Instant::now();
        let mut handover = ramping();
        faded_one(&mut handover, start);
        assert!(
            handover.linked(7, after(start, 20)).is_empty(),
            "a link before the move is not the one waited for"
        );
        assert!(moves(&handover.tick(after(start, 75))));
        assert_eq!(volumes(&handover.linked(7, after(start, 90))), [(7, 0.8)]);
        // The echo: the ramp back plays out before the stream is let go, so that the next
        // handover's fade of it is not refused.
        assert!(handover.volume(7, Some(0.8), after(start, 95)).is_empty());
        assert!(handover.holds(7));
        let done = after(start, 95) + RAMP_IN_DONE;
        assert_eq!(handover.next_deadline(), Some(done));
        assert_eq!(
            handover.tick(done),
            [Step::Volume {
                id: 7,
                level: 0.8,
                ramp_ms: 0
            }]
        );
        assert!(handover.holds(7), "until the server reports the volume");
        assert_eq!(
            handover.volume(7, Some(0.8), after(start, 196)),
            [
                Step::Forget {
                    key: "Output/Audio:application.name:Player".to_owned()
                },
                Step::Finished
            ]
        );
        assert!(!handover.busy());
    }

    #[test]
    fn a_volume_reported_on_the_way_of_a_ramp_is_the_ramp_and_not_somebody_elses() {
        // Measured: a converter asked to ramp longer than a cycle sends its `Props` again in the
        // middle of it, at the volume it has come to — 0.64 of the way back — and not at its end.
        let start = Instant::now();
        let mut handover = ramping();
        let player = playing("Output/Audio:application.name:Player", 0.8);
        handover.begin(&[(7, &player)], start);
        assert!(
            handover.volume(7, Some(0.5), after(start, 10)).is_empty(),
            "on the way down"
        );
        assert!(moves(&handover.tick(after(start, 80))));
        assert_eq!(volumes(&handover.linked(7, after(start, 90))), [(7, 0.8)]);
        assert!(handover.volume(7, Some(0.51), after(start, 110)).is_empty());
        assert!(handover.holds(7), "a point on the way up let the stream go");
        let steps = handover.tick(after(start, 110) + RAMP_IN_DONE);
        assert_eq!(
            volumes(&steps),
            [(7, 0.8)],
            "the volume is said again where the ramp ended"
        );
        let steps = handover.volume(7, Some(0.8), after(start, 211));
        assert!(steps.contains(&Step::Finished));
    }

    #[test]
    fn two_falling_points_of_the_same_ramp_down_keep_the_stream_held_and_its_volume_comes_back() {
        // At a small quantum the 20 ms fade spans several cycles, and the converter may report
        // more than one point of it on the way down.
        let start = Instant::now();
        let mut handover = ramping();
        let player = playing("Output/Audio:application.name:Player", 0.8);
        handover.begin(&[(7, &player)], start);
        assert!(handover.volume(7, Some(0.5), after(start, 5)).is_empty());
        assert!(
            handover.volume(7, Some(0.2), after(start, 10)).is_empty(),
            "a second point further down is the same ramp"
        );
        assert!(
            handover.holds(7),
            "the journal line is still the handover's"
        );
        assert!(handover.volume(7, Some(0.0), after(start, 15)).is_empty());
        assert!(moves(&handover.tick(after(start, 75))));
        assert_eq!(volumes(&handover.linked(7, after(start, 90))), [(7, 0.8)]);
        assert!(handover.volume(7, Some(0.8), after(start, 95)).is_empty());
        let steps = handover.tick(after(start, 95) + RAMP_IN_DONE);
        assert_eq!(volumes(&steps), [(7, 0.8)]);
        let steps = handover.volume(7, Some(0.8), after(start, 196));
        assert!(steps.contains(&Step::Finished));
    }

    #[test]
    fn a_volume_that_rises_after_the_ramp_down_began_is_somebody_elses() {
        let start = Instant::now();
        let mut handover = ramping();
        let player = playing("Output/Audio:application.name:Player", 0.8);
        handover.begin(&[(7, &player)], start);
        assert!(handover.volume(7, Some(0.2), after(start, 5)).is_empty());
        let steps = handover.volume(7, Some(0.6), after(start, 10));
        assert!(
            steps.contains(&Step::Forget {
                key: "Output/Audio:application.name:Player".to_owned()
            }),
            "a ramp down does not go up"
        );
        assert!(!handover.holds(7));
    }

    /// A handover's stream brought back to 0.8 and its ramp played out, at `+195 ms`: its volume
    /// has just been said once more, at once.
    fn brought_back(handover: &mut Handover, start: Instant) -> Instant {
        faded_one(handover, start);
        assert!(moves(&handover.tick(after(start, 75))));
        handover.linked(7, after(start, 90));
        // Measured: the converter reports a point near the start of its ramp back, and nothing at
        // its end.
        assert!(handover.volume(7, Some(0.08), after(start, 95)).is_empty());
        let done = after(start, 95) + RAMP_IN_DONE;
        assert_eq!(volumes(&handover.tick(done)), [(7, 0.8)]);
        done
    }

    #[test]
    fn the_volume_said_once_more_is_said_again_until_the_server_reports_it() {
        let start = Instant::now();
        let mut handover = ramping();
        let said = brought_back(&mut handover, start);
        assert_eq!(handover.next_deadline(), Some(said + CONFIRM_WAIT));
        assert!(volumes(&handover.tick(said + CONFIRM_WAIT - Duration::from_millis(1))).is_empty());
        let again = said + CONFIRM_WAIT;
        assert_eq!(
            handover.tick(again),
            [Step::Volume {
                id: 7,
                level: 0.8,
                ramp_ms: 0
            }],
            "the echo went missing: said again"
        );
        assert!(handover.holds(7));
        let steps = handover.volume(7, Some(0.8), again + Duration::from_millis(1));
        assert!(steps.contains(&Step::Finished));
        assert!(!handover.busy());
    }

    #[test]
    fn a_point_of_the_ramp_back_reported_late_is_not_somebody_elses() {
        let start = Instant::now();
        let mut handover = ramping();
        let said = brought_back(&mut handover, start);
        assert!(
            handover
                .volume(7, Some(0.5), said + Duration::from_millis(1))
                .is_empty()
        );
        assert!(handover.holds(7));
        assert_eq!(volumes(&handover.tick(said + CONFIRM_WAIT)), [(7, 0.8)]);
    }

    #[test]
    fn a_volume_somebody_else_writes_while_the_volume_is_said_once_more_is_theirs() {
        let start = Instant::now();
        let mut handover = ramping();
        let said = brought_back(&mut handover, start);
        let steps = handover.volume(7, Some(1.0), said + Duration::from_millis(1));
        assert_eq!(
            steps,
            [
                Step::Forget {
                    key: "Output/Audio:application.name:Player".to_owned()
                },
                Step::Finished
            ]
        );
        assert!(volumes(&handover.tick(said + ECHO_WAIT)).is_empty());
    }

    #[test]
    fn a_volume_the_server_never_reports_lets_the_stream_go_after_its_tries() {
        let start = Instant::now();
        let mut handover = ramping();
        let mut at = brought_back(&mut handover, start);
        for _ in 1..CONFIRM_TRIES {
            at += CONFIRM_WAIT;
            assert_eq!(volumes(&handover.tick(at)), [(7, 0.8)]);
        }
        at += CONFIRM_WAIT;
        assert_eq!(
            handover.tick(at),
            [
                Step::Forget {
                    key: "Output/Audio:application.name:Player".to_owned()
                },
                Step::Finished
            ]
        );
        assert!(!handover.busy());
    }

    #[test]
    fn a_volume_comes_back_over_a_longer_ramp_than_it_went() {
        let start = Instant::now();
        let mut handover = ramping();
        let player = playing("Output/Audio:application.name:Player", 0.8);
        let ramps = |steps: &[Step]| -> Vec<i32> {
            steps
                .iter()
                .filter_map(|step| match step {
                    Step::Volume { ramp_ms, .. } => Some(*ramp_ms),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(ramps(&handover.begin(&[(7, &player)], start)), [RAMP_MS]);
        handover.volume(7, Some(0.0), after(start, 5));
        assert!(moves(&handover.tick(after(start, 75))));
        assert_eq!(ramps(&handover.linked(7, after(start, 90))), [RAMP_IN_MS]);
        const {
            assert!(RAMP_IN_MS > RAMP_MS);
            assert!(RAMP_IN_DONE.as_millis() > RAMP_IN_MS.unsigned_abs() as u128);
        }
    }

    #[test]
    fn a_recorder_gets_its_volume_back_only_a_moment_after_its_new_link_is_active() {
        let start = Instant::now();
        let mut handover = ramping();
        let recorder = Watched {
            records: true,
            ..playing("Input/Audio:application.name:Recorder", 0.8)
        };
        assert_eq!(
            volumes(&handover.begin(&[(9, &recorder)], start)),
            [(9, 0.0)]
        );
        assert!(handover.volume(9, Some(0.0), after(start, 5)).is_empty());
        assert!(moves(&handover.tick(after(start, 75))));
        assert!(
            handover.linked(9, after(start, 90)).is_empty(),
            "the first cycles on the new link bring the recorder nothing yet"
        );
        let due = after(start, 90) + RECORDER_LINKED;
        assert_eq!(handover.next_deadline(), Some(due));
        assert!(volumes(&handover.tick(due - Duration::from_millis(1))).is_empty());
        assert_eq!(volumes(&handover.tick(due)), [(9, 0.8)]);
    }

    #[test]
    fn a_move_that_moved_nothing_gives_every_volume_back_at_once() {
        // The power switched back before its hand-back's turn: nothing is linked anew, and nothing
        // is to wait for one.
        let start = Instant::now();
        let mut handover = ramping();
        faded_one(&mut handover, start);
        assert!(moves(&handover.tick(after(start, 75))));
        let steps = handover.restore_all(after(start, 75));
        assert_eq!(volumes(&steps), [(7, 0.8)]);
        assert!(handover.volume(7, Some(0.8), after(start, 80)).is_empty());
        assert_eq!(
            volumes(&handover.tick(after(start, 80) + RAMP_IN_DONE)),
            [(7, 0.8)]
        );
        let steps = handover.volume(7, Some(0.8), after(start, 181));
        assert!(steps.contains(&Step::Finished));
        assert!(!handover.busy());
    }

    #[test]
    fn a_stream_with_no_new_link_gets_its_volume_back_when_the_wait_is_up() {
        let start = Instant::now();
        let mut handover = ramping();
        faded_one(&mut handover, start);
        assert!(moves(&handover.tick(after(start, 75))));
        let up = after(start, 75) + LINK_WAIT;
        assert_eq!(handover.next_deadline(), Some(up));
        assert!(volumes(&handover.tick(up - Duration::from_millis(1))).is_empty());
        assert_eq!(volumes(&handover.tick(up)), [(7, 0.8)]);
    }

    #[test]
    fn a_volume_somebody_else_writes_during_the_handover_is_not_overwritten() {
        let start = Instant::now();
        let mut handover = ramping();
        faded_one(&mut handover, start);
        assert!(moves(&handover.tick(after(start, 75))));
        // WirePlumber lowers the stream for another's role while it waits for its link.
        let steps = handover.volume(7, Some(0.3), after(start, 80));
        assert_eq!(
            steps,
            [
                Step::Forget {
                    key: "Output/Audio:application.name:Player".to_owned()
                },
                Step::Finished
            ]
        );
        assert!(handover.linked(7, after(start, 90)).is_empty());
        assert!(volumes(&handover.tick(after(start, 2000))).is_empty());
    }

    #[test]
    fn an_echo_that_never_comes_does_not_hold_the_move() {
        let start = Instant::now();
        let mut handover = ramping();
        let player = playing("Output/Audio:application.name:Hung", 0.8);
        handover.begin(&[(7, &player)], start);
        assert_eq!(handover.next_deadline(), Some(start + ECHO_WAIT));
        assert!(!moves(
            &handover.tick(start + ECHO_WAIT - Duration::from_millis(1))
        ));
        assert!(moves(&handover.tick(start + ECHO_WAIT)));
    }

    #[test]
    fn a_volume_written_back_that_never_echoes_keeps_its_journal_line() {
        let start = Instant::now();
        let mut handover = ramping();
        faded_one(&mut handover, start);
        handover.tick(after(start, 75));
        handover.linked(7, after(start, 90));
        let steps = handover.tick(after(start, 90) + ECHO_WAIT);
        assert_eq!(steps, [Step::Finished], "no Forget: the line stays");
    }

    #[test]
    fn a_stream_that_leaves_during_the_handover_keeps_its_journal_line() {
        let start = Instant::now();
        let mut handover = ramping();
        faded_one(&mut handover, start);
        let steps = handover.removed(7, after(start, 10));
        assert_eq!(steps, [Step::Move, Step::Finished]);
    }

    #[test]
    fn two_streams_of_one_application_keep_its_line_until_the_last_is_back() {
        let start = Instant::now();
        let mut handover = ramping();
        let first = playing("Output/Audio:application.name:Browser", 0.8);
        let second = playing("Output/Audio:application.name:Browser", 0.6);
        let steps = handover.begin(&[(1, &first), (2, &second)], start);
        assert_eq!(
            steps
                .iter()
                .filter(|step| matches!(step, Step::Remember { .. }))
                .count(),
            1
        );
        handover.volume(1, Some(0.0), after(start, 5));
        handover.volume(2, Some(0.0), after(start, 5));
        assert!(moves(&handover.tick(after(start, 75))));
        handover.linked(1, after(start, 80));
        handover.linked(2, after(start, 80));
        assert!(handover.volume(1, Some(0.8), after(start, 85)).is_empty());
        assert!(handover.volume(2, Some(0.6), after(start, 86)).is_empty());
        let steps = handover.tick(after(start, 85) + RAMP_IN_DONE);
        assert_eq!(
            volumes(&steps),
            [(1, 0.8)],
            "the first stream's volume is said where its ramp ended"
        );
        assert!(
            handover.volume(1, Some(0.8), after(start, 185)).is_empty(),
            "the second stream's ramp is still being played"
        );
        let steps = handover.tick(after(start, 86) + RAMP_IN_DONE);
        assert_eq!(volumes(&steps), [(2, 0.6)]);
        let steps = handover.volume(2, Some(0.6), after(start, 187));
        assert!(steps.contains(&Step::Forget {
            key: "Output/Audio:application.name:Browser".to_owned()
        }));
        assert!(steps.contains(&Step::Finished));
    }

    #[test]
    fn the_handover_holds_an_applications_key_until_its_last_faded_stream_is_let_go() {
        let start = Instant::now();
        let mut handover = ramping();
        let browser = "Output/Audio:application.name:Browser";
        assert!(!handover.holds_key(browser), "no handover, nothing held");
        let first = playing(browser, 0.8);
        let second = playing(browser, 0.6);
        handover.begin(&[(1, &first), (2, &second)], start);
        assert!(handover.holds_key(browser));
        assert!(!handover.holds_key("Output/Audio:application.name:Player"));

        handover.volume(1, Some(0.0), after(start, 5));
        handover.volume(2, Some(0.0), after(start, 5));
        assert!(moves(&handover.tick(after(start, 75))));
        handover.linked(1, after(start, 80));
        handover.linked(2, after(start, 80));
        handover.volume(1, Some(0.8), after(start, 85));
        handover.tick(after(start, 85) + RAMP_IN_DONE);
        handover.volume(1, Some(0.8), after(start, 185));
        assert!(
            handover.holds_key(browser),
            "the second stream is still faded"
        );
        handover.volume(2, Some(0.6), after(start, 86));
        handover.tick(after(start, 86) + RAMP_IN_DONE);
        handover.volume(2, Some(0.6), after(start, 187));
        assert!(!handover.holds_key(browser));
    }

    #[test]
    fn a_handover_the_engine_leaves_gives_every_volume_back_and_makes_no_move() {
        let start = Instant::now();
        let mut handover = ramping();
        faded_one(&mut handover, start);
        let steps = handover.restore_all(after(start, 40));
        assert_eq!(volumes(&steps), [(7, 0.8)]);
        assert!(!moves(&steps));
        assert!(handover.volume(7, Some(0.8), after(start, 45)).is_empty());
        // Said once more where the ramp ended — WirePlumber keeps what the server shows — and then
        // over, the move never made.
        let steps = handover.tick(after(start, 45) + RAMP_IN_DONE);
        assert_eq!(volumes(&steps), [(7, 0.8)]);
        let steps = handover.volume(7, Some(0.8), after(start, 146));
        assert!(steps.contains(&Step::Finished) && !moves(&steps));
    }

    #[test]
    fn the_key_is_the_one_wireplumber_keeps_a_stream_volume_under() {
        let key = |pairs: &[(&str, &str)]| {
            let pairs = pairs.to_vec();
            state_key(&move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| *value)
            })
        };
        assert_eq!(
            key(&[
                ("media.class", "Stream/Output/Audio"),
                ("application.name", "Firefox"),
                ("node.name", "Firefox"),
            ]),
            Some("Output/Audio:application.name:Firefox".to_owned())
        );
        assert_eq!(
            key(&[
                ("media.class", "Stream/Output/Audio"),
                ("application.id", "org.mozilla.firefox"),
                ("application.name", "Firefox"),
            ]),
            Some("Output/Audio:application.id:org.mozilla.firefox".to_owned())
        );
        assert_eq!(
            key(&[
                ("media.class", "Stream/Output/Audio"),
                ("media.role", "Notification"),
                ("application.name", "Mako"),
            ]),
            Some("Output/Audio:media.role:Notification".to_owned())
        );
        assert_eq!(
            key(&[("media.class", "Stream/Input/Audio"), ("node.name", "rec")]),
            Some("Input/Audio:node.name:rec".to_owned())
        );
        assert_eq!(key(&[("media.class", "Stream/Output/Audio")]), None);
        assert_eq!(key(&[("application.name", "Firefox")]), None);
    }

    #[test]
    fn the_journal_outlives_the_run_that_wrote_it() {
        let dir = fxsound_core::test_support::ScratchDir::new("handover-journal");
        let path = dir.path().join("fxsound").join(JOURNAL_FILE);
        let mut journal = Journal::default();
        assert!(journal.remember("Output/Audio:application.name:Player", Some(40), 0.8));
        assert!(
            !journal.remember("Output/Audio:application.name:Player", Some(41), 0.0),
            "the line from before the first fade is kept"
        );
        journal.save(&path).expect("the journal should be written");

        let read = Journal::load(&path);
        assert_eq!(read, journal);
        assert_eq!(
            read.serial("Output/Audio:application.name:Player"),
            Some(40)
        );
        assert_eq!(
            read.repair("Output/Audio:application.name:Player", Some(0.0)),
            Repair::Restore(0.8)
        );
        assert_eq!(
            read.repair("Output/Audio:application.name:Player", Some(0.5)),
            Repair::Obsolete
        );
        assert_eq!(
            read.repair("Output/Audio:application.name:Player", None),
            Repair::Nothing
        );
        assert_eq!(
            read.repair("Output/Audio:application.name:Other", Some(0.0)),
            Repair::Nothing
        );

        let mut emptied = read;
        assert!(emptied.forget("Output/Audio:application.name:Player"));
        emptied
            .save(&path)
            .expect("an empty journal removes its file");
        assert!(!path.exists());
        assert!(Journal::load(&path).is_empty());
    }

    #[test]
    fn a_line_outlives_its_stream_only_when_no_stream_of_the_application_can_still_be_at_zero() {
        let browser = "Output/Audio:application.name:Browser";
        let mut journal = Journal::default();
        journal.remember(browser, Some(40), 0.7);
        let paused_tab = Watched {
            running: false,
            serial: Some(41),
            ..playing(browser, 1.0)
        };
        let faded_tab = playing(browser, 0.0);
        let bound = Watched::default();
        let waiting_for_props = Watched {
            level: None,
            ..playing(browser, 0.0)
        };
        let other = playing("Output/Audio:application.name:Other", 0.0);

        assert!(
            !journal.outlived(browser, [&paused_tab, &faded_tab]),
            "a sibling at its own volume dropped the line of the tab still at 0"
        );
        assert!(
            !journal.outlived(browser, [&paused_tab, &bound]),
            "a sibling at its own volume dropped the line while a stream was bound but not known"
        );
        assert!(
            !journal.outlived(browser, [&paused_tab, &waiting_for_props]),
            "a sibling at its own volume dropped the line while a stream's volume was not in"
        );
        assert!(
            !journal.outlived(browser, [&faded_tab, &other]),
            "a line went with no stream of the application at a volume"
        );
        assert!(
            !journal.outlived(browser, [] as [&Watched; 0]),
            "a line went with no stream of the application at all"
        );
        assert!(
            journal.outlived(browser, [&paused_tab, &other]),
            "the application plays at its own volume and none of its streams is at 0"
        );
        assert!(
            journal.outlived(browser, [&playing(browser, 0.7), &paused_tab]),
            "the faded tab is back and its sibling plays at its own volume"
        );
        assert!(
            !journal.outlived("Output/Audio:application.name:Other", [&other]),
            "a line that is not there outlived something"
        );
    }

    #[test]
    fn a_journal_that_cannot_be_read_is_taken_for_an_empty_one() {
        let dir = fxsound_core::test_support::ScratchDir::new("handover-garbage");
        let path = dir.path().join(JOURNAL_FILE);
        std::fs::write(&path, "stream = 3\n").expect("write");
        assert!(Journal::load(&path).is_empty());
        std::fs::write(
            &path,
            "[[stream]]\nkey = \"Output/Audio:node.name:x\"\nvolume = nan\n",
        )
        .expect("write");
        assert!(
            Journal::load(&path).is_empty(),
            "a volume that is no volume"
        );
    }

    #[test]
    fn the_journal_keeps_its_newest_lines() {
        let mut journal = Journal::default();
        for index in 0..=JOURNAL_LIMIT {
            journal.remember(&format!("Output/Audio:node.name:{index}"), None, 1.0);
        }
        assert_eq!(journal.entries.len(), JOURNAL_LIMIT);
        assert_eq!(
            journal.repair("Output/Audio:node.name:0", Some(0.0)),
            Repair::Nothing
        );
        assert_eq!(
            journal.repair(
                &format!("Output/Audio:node.name:{JOURNAL_LIMIT}"),
                Some(0.0)
            ),
            Repair::Restore(1.0)
        );
    }
}
