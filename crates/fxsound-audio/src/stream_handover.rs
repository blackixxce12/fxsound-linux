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
//! old volume to the new one over that time, sample by sample (`spa/plugins/audioconvert/
//! audioconvert.c`, `apply_props`, `generate_ramp_seq`). Only the master `volume` moves;
//! `channelVolumes`, the level a desktop's slider shows, stays. Faded to silence over 20 ms,
//! moved, and faded back once it has its new link, a stream's move measured −50.5…−94.5 dBFS.
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
//!    wanted: a default written, a target set, the DSP switched.
//! 4. Each stream's next new link — WirePlumber's, or the one `stranded` makes for a recorder
//!    WirePlumber failed to link — ends its move, and its volume is sent back over the same ramp.
//!    One that has none within [`LINK_WAIT`] gets its volume back all the same.
//! 5. The echo of the volume written back ends the stream's handover, and takes its line out of the
//!    journal.
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
//! # A server without the ramp
//!
//! The ramp arrived in PipeWire 0.3.68 ([`RAMPS_SINCE`]); Debian 12 ships 0.3.65. There a volume
//! written to 0 would jump — a click of its own — so on such a server a handover is the move alone,
//! at once, as before 0.5.0 ([`Handover::begin`]).
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

/// How long after a write's echo its ramp has surely been played out, and a second one would be
/// taken: [`RAMP_MS`] and a margin.
pub(crate) const RAMP_DONE: Duration = Duration::from_millis(30);

/// How long after its fade's echo a stream counts as silent where it is heard: the ramp, then
/// what the stream and the device behind it have buffered, and a quantum.
pub(crate) const SETTLE: Duration = Duration::from_millis(70);

/// How long a moved stream waits for its new link before its volume is put back regardless. A
/// recorder WirePlumber failed to link is linked by `stranded` once it has had no link for
/// `STRANDED_AFTER` (500 ms), on the supervisor's next tick (200 ms), so a shorter wait would put
/// its volume back before its link, and start its recording with a step.
pub(crate) const LINK_WAIT: Duration = Duration::from_secs(1);

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
}

impl Watched {
    /// Whether a handover fades this stream: it plays, it has a volume to ramp and may have it
    /// changed, and it is not silent already.
    #[must_use]
    pub(crate) fn fades(&self) -> bool {
        self.running && !self.locked && self.level.is_some_and(|level| level > SAME)
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

fn same(a: f32, b: f32) -> bool {
    (a - b).abs() <= SAME
}

// ---------------------------------------------------------------------------------------------
// The handover
// ---------------------------------------------------------------------------------------------

/// What the engine is to do next for a handover.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// Write `level` to the stream's master volume, over the ramp.
    Volume { id: u32, level: f32 },
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
    /// `settled`.
    Silent { ramped: Instant, settled: Instant },
    /// Silent where it is heard, and ready to be moved.
    Settled,
    /// The echo never came within [`ECHO_WAIT`]: whether it is silent is not known, and the move
    /// does not wait for it any longer.
    Unconfirmed,
    /// Moved: waiting for its new link, until `until`.
    Moved { until: Instant },
    /// Its volume is written back, and the echo has not come.
    FadingIn { sent: Instant },
}

/// One stream a handover faded.
#[derive(Debug, Clone)]
struct Faded {
    id: u32,
    key: Option<String>,
    /// Its master volume before the fade: what it gets back.
    saved: f32,
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
    /// each connection, and `false` until then.
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
    /// noted and sent to 0; with none — none plays, or the server has no ramp — the move is made
    /// at once and the handover is over in the same breath.
    ///
    /// Only when not [`Self::busy`]; the engine queues a handover asked for meanwhile.
    pub(crate) fn begin(&mut self, streams: &[(u32, &Watched)], now: Instant) -> Vec<Step> {
        debug_assert!(!self.busy(), "one handover at a time");
        let mut steps = Vec::new();
        let mut faded: Vec<Faded> = Vec::new();
        if self.ramps {
            for &(id, watched) in streams {
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
                steps.push(Step::Volume { id, level: 0.0 });
                faded.push(Faded {
                    id,
                    key: watched.key.clone(),
                    saved: level,
                    phase: Phase::FadingOut { sent: now },
                });
            }
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
        let ours = match stream.phase {
            Phase::FadingOut { .. } | Phase::Unconfirmed if silent(level) => {
                stream.phase = Phase::Silent {
                    ramped: now + RAMP_DONE,
                    settled: now + SETTLE,
                };
                true
            }
            // What the stream said before our write reached it.
            Phase::FadingOut { .. } | Phase::Unconfirmed => same(level, stream.saved),
            Phase::Silent { .. } | Phase::Settled | Phase::Moved { .. } => silent(level),
            Phase::FadingIn { .. } if same(level, stream.saved) => {
                let done = batch.streams.remove(index);
                steps.extend(forget_if_last(&batch.streams, done.key));
                steps.extend(self.advance(now));
                return steps;
            }
            Phase::FadingIn { .. } => silent(level),
        };
        if !ours {
            // Somebody else's volume: theirs to keep.
            let theirs = batch.streams.remove(index);
            steps.extend(forget_if_last(&batch.streams, theirs.key));
        }
        steps.extend(self.advance(now));
        steps
    }

    /// The stream under `id` has a new link: after the move, its move is over, and its volume goes
    /// back. Before the move a link is not the one being waited for, and changes nothing.
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
        stream.phase = Phase::FadingIn { sent: now };
        vec![Step::Volume {
            id,
            level: stream.saved,
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

    /// The engine is closing, or its connection has: every faded stream gets its volume back as
    /// soon as a ramp may be written to it, and the move is not made — the caller makes it, or
    /// has no graph to make it in.
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
        for stream in &mut batch.streams {
            match stream.phase {
                Phase::FadingOut { sent } if now >= sent + ECHO_WAIT => {
                    stream.phase = Phase::Unconfirmed;
                }
                Phase::FadingIn { sent } if now >= sent + ECHO_WAIT => given_up.push(stream.id),
                Phase::Silent { settled, .. } if now >= settled => stream.phase = Phase::Settled,
                Phase::Moved { until } if now >= until || batch.leaving => {
                    stream.phase = Phase::FadingIn { sent: now };
                    steps.push(Step::Volume {
                        id: stream.id,
                        level: stream.saved,
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
                    });
                }
            }
        }
        // A volume written back whose echo never came: the stream keeps its journal line, for
        // the next time the engine meets it.
        batch
            .streams
            .retain(|stream| !given_up.contains(&stream.id));

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
                Step::Volume { id, level } => Some((*id, *level)),
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
                Step::Volume { id: 7, level: 0.0 },
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
        let steps = handover.volume(7, Some(0.8), after(start, 95));
        assert_eq!(
            steps,
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
        let steps = handover.volume(2, Some(0.6), after(start, 86));
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
        assert!(
            handover.holds_key(browser),
            "the second stream is still faded"
        );
        handover.volume(2, Some(0.6), after(start, 86));
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
        let steps = handover.volume(7, Some(0.8), after(start, 45));
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
