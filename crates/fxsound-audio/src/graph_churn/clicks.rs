//! Clicks: what a switch does to a steady tone that plays through FxSound while it happens.
//!
//! The 0.4.0 live check (E6a) found the power button clicking at −5 to −26 dBFS and voice presets
//! at −21, where nothing in the tests had looked: every test here asked where a stream ended up,
//! none what it sounded like on the way. This module is that measurement, kept.
//!
//! # The method
//!
//! A tone plays through the graph — 100 Hz at −12 dBFS into the speakers' lane, 300 Hz at
//! −18 dBFS into the microphone — and is recorded where an application or a device would hear it.
//! Each switch is made under the tone, and the recording is read afterwards:
//!
//! * **The tone is taken away** by a high-pass at 2 kHz: a windowed-sinc FIR of 511 taps under a
//!   Blackman window ([`high_pass_kernel`]). A pure tone far below the cut leaves next to nothing;
//!   a step, a gap or a jump in it — a click — is broadband and comes through.
//! * **The worst residual around each switch** is the loudest high-passed sample from 0.3 s
//!   before the switch to 0.6 s after it, in dBFS ([`EventResult::worst_dbfs`]).
//! * **The control** is the loudest high-passed sample of the steady state: everything more than
//!   0.5 s before and 0.8 s after every switch, and away from both ends ([`Analysis::floor_dbfs`]).
//!   A floor near the threshold means the instrument cannot tell a click, not that there is none.
//! * **The dip** is the longest stretch from 0.3 s before a switch to 1 s after it in which the
//!   tone itself, unfiltered, is more than 20 dB below its steady level, in 10 ms steps
//!   ([`EventResult::dip_ms`]): what a handover that fades instead of clicking costs.
//!
//! This is `~/Загрузки/fxsound-scratch/e6/bin/sig.py events` of 0.4.0, in Rust, with the same
//! numbers; the switch times come from how much each recorder had written when the switch was
//! made, rather than from a wall clock beside the recording.
//!
//! # The graph
//!
//! A [`PolicyGraph`]: a private PipeWire with WirePlumber 0.5 on it, because where a stream goes
//! when the power goes off is WirePlumber's decision, and the click of that move is part of what
//! is measured. The engine runs on it as the app drives it — the power switch is both lanes'
//! snapshots and both default claims ([`Bench::act`]); a preset is the snapshot the app would
//! publish for it; a route is the rules it would send. Tones are `pw-cat` playing a Sun AU file,
//! recorders `pw-record` writing a raw file: no `--raw`, which PipeWire 1.0's tools lack. A
//! scenario of PulseAudio applications plays and records with `pacat` instead ([`Scenario::pulse`]).
//!
//! A switch between devices is heard on both: the speakers' lane is recorded at both sinks'
//! monitors, and one player feeds both microphones the same wave, in step. The recordings are read
//! as one ([`joined`]), so a click on either device counts, and a dip is a time when neither had
//! the tone.
//!
//! # The scenarios
//!
//! One test each, the numbers of roadmap §7's list where it has them:
//!
//! 1. the power switch with streams pinned to FxSound's own nodes, and with streams that follow
//!    the default, which WirePlumber moves: `pw-cat`'s and `pw-record`'s, and PulseAudio
//!    applications' through the graph's own `pipewire-pulse`;
//! 2. an application's route on and off ([`UiToAudio::SetAppRoutes`]);
//! 3. FxSound picking another device for both lanes, as the device list, `--output`,
//!    `--next-output` and `--input` do ([`UiToAudio::SelectDevice`]);
//! 4. the desktop picking another default device (`wpctl set-default`) with FxSound on,
//! 5. and with FxSound off;
//!
//! 4 and 5 again with FxSound's hook in WirePlumber (`crate::wireplumber_hook`, D5), which fades
//! the moves WirePlumber makes itself;
//!
//! and the music equalizer and presets, the voice equalizer and presets, and «Like FxSound for
//! Windows» moved between Off and Interface and sound, on the lane and in an application's route
//! (roadmap 0.5.0 §1 A4). Every stream but the
//! pinned ones of the first test follows the default, as most applications' do. Both lanes but in
//! the preset tests, which play into the lane a preset belongs to. Tests 4 and 5 run as the engine
//! does until the app ranks its devices, following the system's default device: the mode in which
//! FxSound acts on a desktop's pick.
//!
//! # Repeats, xruns and the gate
//!
//! Each scenario runs on a fresh graph until three runs are clean, at most [`ATTEMPTS`] times.
//! A run is clean when no node of the graph had an xrun during it (the `ERR` column of `pw-top`,
//! which reads PipeWire's profiler): an xrun is a gap in the audio, which looks exactly like a
//! click, and on a busy machine or a shared CI runner it is the machine's and not the switch's.
//! The result is the median of the clean runs, switch by switch, and beside it the loudest of them:
//! a move WirePlumber makes varies by about 20 dB from run to run and from pass to pass, and a
//! median of three can land anywhere in that.
//!
//! FxSound's own rings, between the node applications play into and the stream to the device,
//! can come up short too, and the server cannot see that. What each lane reports of them
//! ([`AudioStatus`]) is printed with the table rather than discarded on, because a switch can do it
//! itself: the power coming back on under a stream that follows the default leaves the speakers'
//! ring 512 frames short every time: 11 ms of the 20–30 ms dip of that switch.
//!
//! [`FXSOUND_CLICK_GATE`](GATE_MODE) decides what a loud switch does:
//!
//! * `hard`, the default: a switch FxSound makes itself at [`HARD_GATE_DBFS`] or louder fails the
//!   test. This is the check an audio phase runs on a developer's machine before it is done.
//! * `report`, and the default where `CI=true`: nothing fails for loudness. The table is printed,
//!   a switch at [`SOFT_GATE_DBFS`] or louder is marked `WARNING`, and no clean run at all is
//!   reported rather than failed — a shared runner's xruns must not stop an audio phase.
//!
//! What fails in both modes is the instrument: a tone that is never heard, a WirePlumber that
//! went away. `hard` also fails a gated scenario whose floor is at [`FLOOR_LIMIT_DBFS`] or louder,
//! where a click could not be told from the steady state, and one with no clean run.
//!
//! A stream that follows the default is moved by WirePlumber when the power switch hands the
//! default back or takes it, when a route is made or taken away, and when a lane's nodes go and
//! come back on another device, and WirePlumber unlinks before it links, in the middle of the
//! wave. In 0.4.0 all of these are over the gate, so they were reported, not gated. The power
//! switch, the route and FxSound's own pick of a device are FxSound's own switches all the same,
//! held to the gate once 0.5.0 makes them quiet (roadmap §7 D1–D3): since D2 the power switch and
//! the route fade the streams they move (`crate::stream_handover`) and are gated, the gap they
//! leave held to [`HANDOVER_BUDGET`]; since D3 FxSound's own pick moves no stream at all, its
//! virtual nodes kept (`crate::engine`, "Another device, the same virtual node"), and is gated the
//! same way. The desktop's pick with FxSound on is two moves, read as two rows
//! ([`Scenario::claims`]): WirePlumber's, which roadmap §14 #16 accepted stays where plain Linux
//! has it unless WirePlumber itself is taught to fade (D5), and FxSound's claim of the default back
//! after it, gated. WirePlumber's move is not gated but held to the same picks with FxSound off,
//! plus [`BARE_MARGIN_DB`] (§7, test 4; [`against_bare_linux`]): its loudest run of all against
//! plain Linux's, each read above the tone ([`above_the_tone`]) rather than in dBFS, since
//! FxSound's chain plays the tone louder than plain Linux and every cut of it with it — a user
//! hears a click beside the sound it cuts. What the move leaves at the monitor of the speakers
//! FxSound played through, read on that monitor alone — the end of the sound it cut off in
//! FxSound's chain, faded since D4 — is gated ([`left_behind`]). With FxSound off the pick is
//! WirePlumber's alone, and reported: it is what the other is held to. With FxSound's hook in
//! WirePlumber (D5, an option of Settings ▸ Experimental) WirePlumber fades its own moves too, and
//! the same picks, with FxSound on and with it off, are gated, WirePlumber's move and FxSound's
//! claim alike ([`Scenario::hook`]).
//!
//! # Running it
//!
//! ```text
//! cargo test -p fxsound-audio --lib graph_churn::clicks -- --nocapture --test-threads=1
//! ```
//!
//! `--nocapture` shows the tables; one test at a time keeps the other tests' daemons from causing
//! the xruns that would discard runs. [`FXSOUND_CLICK_REPORT`](REPORT_FILE) names a file every
//! table is appended to as well. The thirteen measurements take about twenty minutes, and
//! their names share the prefix `under_a_steady_tone`, by which CI's Arch Linux leg leaves them out
//! of its test step and runs them in one of their own, in `report` mode, with the tables in the
//! job's summary (`.github/workflows/ci.yml`). They also run in a plain `cargo test --workspace`,
//! hard, alongside everything else; a machine too busy for them discards runs rather than fails,
//! until no run is left.
//!
//! # For the audio phases of 0.5.0
//!
//! Every audio phase runs this before it is done (roadmap §12 item 1): W1, W4, D1–D5, P4–P6, C4,
//! X0, G11. Its local check is the command above in `hard` mode, green; its evidence is the tables,
//! set beside the ones below, so that a switch that got louder without failing is seen too.
//!
//! * A phase that adds a switch or a parameter change adds it here: a [`Switch`] that makes it
//!   as the app does, and a [`Scenario`] — or a place in one — that makes it under the tone. The
//!   end state alone is not the test (roadmap §12 item 2).
//! * A phase that makes a reported switch quiet sets its scenario's `gated`: D2 did it for the
//!   power switch with following streams and for the route, D3 for FxSound's own pick of a device
//!   and for its claim after the desktop's, and each holds the dip to its budget with
//!   [`EventResult::dip_ms`] as well ([`Scenario::budget`]). D3 also held WirePlumber's move after
//!   the desktop's pick to plain Linux + 3 dB ([`against_bare_linux`]); D4 faded FxSound's part
//!   of it, the tail it cut off in FxSound's chain, and gates it at the monitor of the speakers
//!   FxSound played through, read on its own ([`left_behind`]); D5, whose hook in WirePlumber
//!   makes the rest fade, gates the desktop's picks with it in tests of their own.
//! * A phase that says a reported switch got quieter judges by the loudest run, of several passes,
//!   not by a shift of the median: what WirePlumber's move leaves spreads over about 20 dB (below).
//! * A phase that changes what a scenario sets up — a new node between the tone and the recorder,
//!   another default device — keeps the floor where it is: a floor that rises is the instrument
//!   going deaf, not the switches going quiet.
//!
//! # Where 0.4.0 stands
//!
//! Measured on 28.09.2026, PipeWire 1.6.9 and WirePlumber 0.5.17, on `release/0.5.0` at 7598d79,
//! whose audio path is 0.4.0's: one test at a time, each switch the median of three clean runs.
//! The worst residual in dBFS, the range over passes: two for the switches FxSound makes itself
//! that move no stream, three for the route, FxSound's and the desktop's picks, and nine for
//! the power switch with following streams (eight here, one a reviewer's). Four switches are given
//! in the order they are made; a dip where there was one.
//!
//! | Scenario | Speakers' lane | Microphone's lane |
//! |---|---|---|
//! | 1, power, pinned streams | off −89.7…−92.2, on −48.9…−50.9 | off −96.2…−99.6, on −87.0…−97.4 |
//! | 1, power, following streams (reported) | off −21.2…−26.2, on −18.8…−31.1, off −23.1…−31.6, on −18.8…−41.1; dip 20–40 ms | off −17.7…−21.1, on −21.1…−32.0, off −17.7…−21.1, on −24.9…−28.2; dip 0–10 ms |
//! | 2, route on, off (reported) | on −5.2…−5.3, off −8.8…−14.2; dip 30–50 ms off | on −17.6…−21.1, off −17.7…−21.1; dip 0–20 ms |
//! | 3, FxSound picks the other devices and back (reported) | −4.9…−10.3; dip 20–70 ms | −17.7…−21.1; dip 40–70 ms, 740 ms once |
//! | 4, the desktop picks them, FxSound on (reported) | −7.7…−15.6; dip 20–40 ms | −17.7…−25.0; dip 10–50 ms, 760–770 ms on the pick back in two passes |
//! | 5, the desktop picks them, FxSound off (reported) | −18.8…−23.1; dip 10–40 ms | −23.3…−28.2; dip 0–10 ms |
//! | music EQ off, on | −61.6…−62.0 | — |
//! | music presets | Bass Boost −51.8…−53.0, Gaming −65.8…−68.5, Volume Boost −74.2…−77.0, General −72.0…−73.7 | — |
//! | voice EQ off, on | — | −90.1…−97.6 |
//! | voice presets | — | Clean Voice −73.0, Studio −77.5, Noisy Room −70.8 (dip 10–20 ms) |
//!
//! Floors: −108.0 to −108.3 dBFS on the speakers' lane, −118.0 to −118.5 on the microphone's, and
//! −122.9 and −127.5 with FxSound off (5), whose tones do not go through the chains' gain. The
//! microphone's floor in 3 and 4 is −17.7 to −21.3 in one run of three or two: something clicks
//! again 0.8 to 1 s after a pick, past its window. Most likely the recorder was left linked to
//! nothing by the move and FxSound's rescue of stranded streams (`crate::stranded`) moved it again;
//! the dips of 740–770 ms would be the same stranding, seen inside a dip's second.
//!
//! The switches that move no stream are under the gate, the loudest the power coming back on with
//! pinned streams (−49) and Bass Boost (−52). Every move of a stream is over it. WirePlumber alone,
//! the desktop's pick with FxSound off (5), leaves −19 to −28. The power switch under following
//! streams leaves about as much; the route and FxSound's own pick of a device leave −5 to −14 on
//! the speakers' lane — the route going on at +0.16 to +0.25 s, when the stream is linked onto the
//! route, and a pick while it takes the lane's nodes down under the stream — and the desktop's pick
//! with FxSound on, which moves the streams twice, −8 to −16. These are the numbers D2 and D3 are
//! to bring under the gate, and roadmap §7 test 4's «bare Linux + 3 dB» is set beside 5.
//!
//! What a move leaves varies by about 20 dB: from run to run, and in the median of three from pass
//! to pass. The speakers' power coming back on under following streams had medians from −18.8 to
//! −41.1, and single runs of the power going off reached −11.7. So a phase that makes a move
//! quieter does not read an improvement from a median that moved within these ranges: it runs
//! several passes and judges by the loudest run of all of them, the `loudest run` of each table.
//!
//! FxSound's hook in WirePlumber, measured on 29.09.2026 at D5 in the same way: the desktop's picks
//! with FxSound on left, over two passes, −53.8…−61.2 dBFS for WirePlumber's move (the loudest run
//! −52.9) and −61.8…−63.8 for FxSound's claim on the speakers' lane, −58.6…−69.0 and −71.8…−76.6
//! on the microphone's, dips 100–160 ms; with FxSound off and ranking its devices, −59.8…−60.7
//! and −65.9…−66.7, dips 50–100 ms. The microphone's claim is not always under the gate: in one
//! run of a pass it jumped once in 24 switches, at −34.1 in the pass above and at −17.7 in a
//! later one (`t_other` to `t_mic2`), as loud as plain Linux — the converter's new volume played
//! before its ramp, below, which the hook does not end — and the scenario passes within
//! [`Budget::loud_runs`]. Without the hook WirePlumber's move is plain Linux's −19 to −28. A first
//! version of the hook gave the volume back 20 ms after the new link whatever the stream had left:
//! run beside the rest of the tests, FxSound's claim then met the hook's fade in on the same
//! stream, and the converter, refusing a second ramp, jumped: −18.8 to −31.1 in two runs of three.
//! A stream moved off FxSound's nodes is now held silent until FxSound has moved it back.
//!
//! «Like FxSound for Windows», measured on 29.09.2026 in the same way at W1d of 0.5.0, both scenarios
//! at −6 dB of master gain and +4 dB of balance: on the engine before W1d the lane's move to
//! Interface and sound left −20.0 dBFS and the move back −6.3; since W1d, which glides and
//! crossfades what the level changes, −64.7 and −53.3 on the lane and −64.0 and −55.7 in an
//! application's route, the loudest run −52.2, the floor −108.5.
//!
//! The smooth handover, measured on 29.09.2026 in the same way at D2 of 0.5.0, each switch the
//! median of three clean runs, and the loudest run beside it. With ramps of one sample a step, as
//! first made, over three passes: the power switch with following streams, on the speakers' lane
//! off −71.2…−91.3 and on −47.7…−51.5, a dip of 40–100 ms; on the microphone's lane off
//! −76.2…−84.2 and on −72.9…−76.9, a dip of 100–130 ms, the recorder's volume given back 50 ms
//! after its new link is active. The route on and off: on the speakers' lane on −46.9…−47.8 and
//! off −52.6…−54.4, a dip of 40–60 ms; on the microphone's lane −68.9…−81.4, a dip of 100–120 ms.
//! With eight samples a step (`crate::stream_handover::RAMP_STEP_SAMPLES`, below): the power
//! switch off −59.9…−60.8 and on −47.0…−50.5 on the speakers' lane, −65.9…−66.7 on the
//! microphone's; the route on −44.3…−46.9 and off −49.9…−52.7, −58.6…−59.1 on the microphone's
//! lane; the dips as before. The quiet ones are the steps' staircase now, 10–20 dB louder; the
//! loud ones hardly moved. Floors as before. Before the volume came back over 50 ms rather than
//! 20, the route going on left −40.6…−43.8: the route's chain, which has heard nothing yet, took
//! the onset at full level. Before the route's chain was switched off and let play its tail out
//! before its pair went, the route going off left −35.6…−37.8: its reverb's tail stopped in the
//! middle of its wave. The desktop's pick and FxSound's own pick are as before (for D3, below).
//!
//! The power switch with PulseAudio applications following the default, measured the same day in
//! the same way: on the speakers' lane off −53.8…−57.6 and on −45.9…−49.6, a dip of 60–90 ms; on
//! the microphone's lane −65.9…−66.3, a dip of 100–120 ms (with one sample a step, over nine
//! passes: off −66.9…−79.9 and on −47.7…−50.0; −76.2…−80.4). Not in every run, which the median
//! of three does not show. PipeWire's converter applies a ramped write's new volume at once and
//! only then hands its data thread the ramp, and a cycle played in between is heard at the new
//! volume: before a fade in, a whole quantum at full level, then silence, then the ramp. Counted
//! switch by switch over runs of four switches: the recorder played its tone so, at −17.7 to
//! −24.9 dBFS, at 1 switch in 96 with one sample a step and 2 in 160 with eight; the player at
//! −11.7 to −18.6, 3 in 96 with one and none in 160 with eight. It was put down, before it was
//! found, to the recorder's channels changing in its move from the microphone's one to FxSound's
//! two: it is not that — without FxSound, on a private graph, a `parec` stream faded out and in
//! over 50 ms by `pw-cli` did it 21 times in 2400 writes at one sample a step, and once in 1600 at
//! eight. It is PipeWire's, and FxSound's steps make it rarer and do not end it; so a handover's
//! scenario counts every run, lists each switch over the gate, and fails a pass with more than one
//! ([`Budget::loud_runs`]). With a player that asks for 20 ms rather than 85, the recorder jumped
//! at −17.7 to −28.6 on one run in three, 90 ms after the switch and before its own handover
//! began: the player's move changes the quantum of the graph the recorder is in, and a PulseAudio
//! recorder drops samples when the quantum falls — without FxSound too
//! (`PolicyGraph::pulse_player`'s latency).
//!
//! Two faults of the handover's own were mended with it. A second point of a ramp down, reported
//! by the converter, was taken for somebody else's volume: the stream let go, and its journal line
//! taken out, while the ramp went on to 0 (found in review, not seen in these runs). And, seen in
//! a run of sixteen switches 1.5 s apart, a converter that reported a point near the start of its
//! ramp back and then never echoed the volume said once more after it left the server showing that
//! point, 0.10, which the next power switch faded from and gave back: a player left 20 dB down for
//! good (`crate::stream_handover::CONFIRM_WAIT`).
//!
//! FxSound's own pick of a device and the desktop's pick with FxSound on, measured on 29.09.2026 in
//! the same way at D3 of 0.5.0, each switch the median of three clean runs and the loudest run
//! beside it, over three passes. FxSound's pick, both lanes' virtual nodes kept: −59.4 to −68.4
//! dBFS on the speakers' lane, the loudest run −59.1, and −69.0 to −75.6 on the microphone's, a
//! dip of 100–140 ms. Before the chain waited for its new stream's link to be active and settled,
//! it clicked at −20 to −30, 40 to 60 ms after the move: let out on the new stream's first block,
//! which the virtual node's link-group runs before anything links it, it faded in into nothing,
//! and let out at the link itself, it faded in over the first cycles on the new device's clock,
//! which left the ring a block short. The desktop's pick: FxSound's claim after it −59.9 to −60.8
//! and −65.9 to −66.7, a dip of 110–160 ms. Made before the applications were silent there,
//! FxSound's move onto the picked device clicked in them at −23 to −32: the stream it linked there
//! changed the quantum under them. WirePlumber's move −11.6 to −23.1 dBFS on the speakers' lane
//! and −17.7 to −24.9 on the microphone's, over plain Linux's −18.8 to −25.9 and −23.3 to −28.2 in
//! dBFS, since the chain plays the speakers' tone at −1.1 dBFS and the microphone's at −10.8, 10.9
//! and 7.2 dB above the bare ones. Above the tone, the loudest run of all: −10.5 dB against plain
//! Linux's −6.8 on the speakers' lane, −6.9 against −5.3 on the microphone's; the same in a fourth
//! pass made with [`against_bare_linux`], and −8.8 and −6.9 in a fifth, the gate's, with every
//! measurement at once. FxSound's chain makes it no louder, and that holds it there. Floors as
//! before.
//!
//! The tail de-click, measured on 29.09.2026 at D4 of 0.5.0 (`crate::engine`, "A sound cut off"):
//! WirePlumber's move after the desktop's pick with FxSound on, over twelve passes of the test
//! above, each speakers' monitor also read on its own. On the speakers FxSound had played through,
//! the move left −88.8 to −105.6 dBFS in 136 picks of 144, where it had left a step of the chain's
//! tone; in the other 8, six of them the third pick, the recording skipped a few milliseconds of
//! the fade downstream of FxSound, whose own blocks were played whole (traced), and jumped into it
//! at −8.4 to −16.3. On the picked speakers the application arrived unprocessed at −18.8 to −48.0,
//! as in plain Linux. The move's median, switch by switch, went from −11.6…−26.2 dBFS to
//! −18.8…−31.6 (plain Linux −18.8…−25.9); its loudest run above the tone to −17.7 dB in the five
//! passes with no skip, −7.3 to −15.1 in the others (−8.8 to −10.5 at D3), and −17.7 in a
//! thirteenth, the gate's, with every measurement at once. The microphone's lane, whose device
//! never goes silent, and FxSound's own switches, as at D3. The monitor of the speakers FxSound had
//! played through is read on its own in the test since, each pick in its own row, and gated
//! ([`left_behind`]): −89.2 to −97.1 dBFS in a fourteenth pass, the loudest run −89.2, and −89.0
//! to −97.0 in the gate's; with the fade taken out of the engine, 10 of a pass's 12 picks at −11.6
//! to −26.2, and it fails.
//!
//! E6a measured the same switches with the app's release build, where this runs the engine
//! unoptimised, and in another order. The switches FxSound makes itself agree within a few dB,
//! except Volume Boost and Noisy Room, which E6a had at −83.1 and −77.8: 6–9 dB louder here, all
//! far under the gate, the build and the switches around them the likely cause. WirePlumber's
//! moves differ by more, as they do from pass to pass: E6a had the microphone's first power off at
//! −38.3 (`~/Загрузки/fxsound-scratch/e6/evidence/02-clicks-final.txt`, `02-pinned-final.txt`).
//! Its desktop pick with FxSound off, between two sinks of its own environment, left −19.1 to
//! −26.2 (`02-wpmove.txt`), each device read on its own: 5 here.

use super::policy::PolicyGraph;
use super::*;
use fxsound_core::messages::{AppRoute, DspEvent, DspParams, InputDspParams, RouteParams};
use fxsound_core::{AppKey, AudioStatus, DspCompat, Preset, WindowsParity, eq};
use fxsound_preset::input::InputPreset;
use std::fmt::Write as _;
use std::io::{Read as _, Seek as _};

/// The rate everything here runs at.
const RATE: usize = 48_000;

/// Where the tone is taken away: everything below it is the tone, everything above a click.
const HIGH_PASS_HZ: f64 = 2_000.0;

/// The high-pass's length: long enough to take a 100 Hz tone 100 dB down.
const TAPS: usize = 511;

/// A switch's window, before and after the moment it was made, in seconds.
const WINDOW: (f64, f64) = (0.3, 0.6);

/// What the steady state keeps clear of around every switch, in seconds.
const CLEAR: (f64, f64) = (0.5, 0.8);

/// What the steady state keeps clear of at the end of a recording, in seconds: a recorder that is
/// being stopped.
const TAIL: f64 = 1.0;

/// How far after a switch a dip is looked for, in seconds.
const DIP_AFTER: f64 = 1.0;

/// How much longer than a switch's window FxSound's own move after it is read
/// ([`Window::split`]): its handover may wait for another lane's before it begins
/// (`crate::engine::fades`, one handover at a time).
const CLAIM_EXTRA: f64 = 0.4;

/// How far below its steady level the tone has to be to count as dipped.
const DIP_DB: f64 = 20.0;

/// The level steps of the dip, and of the tone's steady level.
const LEVEL_STEP: usize = RATE / 100;

/// The gate: a switch FxSound makes itself must leave less than this.
const HARD_GATE_DBFS: f64 = -40.0;

/// The warning of the report mode.
const SOFT_GATE_DBFS: f64 = -30.0;

/// A floor at or above this cannot tell a click from the steady state.
const FLOOR_LIMIT_DBFS: f64 = -60.0;

/// How much louder than plain Linux WirePlumber's move after a desktop pick may be with FxSound on,
/// read above the tone ([`above_the_tone`]): roadmap 0.5.0 §7, test 4.
const BARE_MARGIN_DB: f64 = 3.0;

/// How long a tone plays through the chains before the steady state starts.
const SETTLE: Duration = Duration::from_secs(2);

/// Clean runs a result is the median of.
const REPEATS: usize = 3;

/// Runs a scenario gets to have [`REPEATS`] clean ones.
const ATTEMPTS: usize = 6;

/// The environment variable that picks [`Gate::Hard`] (`hard`) or [`Gate::Report`] (`report`).
const GATE_MODE: &str = "FXSOUND_CLICK_GATE";

/// The environment variable naming a file every table is appended to.
const REPORT_FILE: &str = "FXSOUND_CLICK_REPORT";

/// The tone of the speakers' lane: frequency and level.
const OUTPUT_TONE: (f64, f64) = (100.0, -12.0);

/// The tone fed into the microphone.
const INPUT_TONE: (f64, f64) = (300.0, -18.0);

/// How long FxSound may take to begin to take a default back after the desktop picked another
/// device ([`Bench::mark_claims`]): a supervisor tick for the lane's rules, the fade before its
/// stream on the device is replaced, and room for a slow machine.
const CLAIM_PATIENCE: Duration = Duration::from_millis(1500);

/// The loudest a tone is when it is not heard: −40 dBFS.
const HEARD: f32 = 0.01;

// ---------------------------------------------------------------------------------------------
// The analyser: pure arithmetic on a recording, E6a's `sig.py`.

/// A dB value, with silence at −240 rather than minus infinity.
fn dbfs(x: f64) -> f64 {
    20.0 * x.abs().max(1e-12).log10()
}

/// The high-pass that takes the tone away: a windowed-sinc low-pass at `hz` under a Blackman
/// window, subtracted from a unit impulse, as `sig.py` builds it with NumPy.
fn high_pass_kernel(hz: f64, taps: usize) -> Vec<f64> {
    let cut = 2.0 * hz / RATE as f64;
    let middle = (taps - 1) as f64 / 2.0;
    let span = (taps - 1) as f64;
    let mut kernel: Vec<f64> = (0..taps)
        .map(|n| {
            let t = n as f64 - middle;
            let x = std::f64::consts::PI * cut * t;
            let sinc = if t == 0.0 { 1.0 } else { x.sin() / x };
            let phase = 2.0 * std::f64::consts::PI * n as f64 / span;
            let blackman = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            -(sinc * cut * blackman)
        })
        .collect();
    kernel[(taps - 1) / 2] += 1.0;
    kernel
}

/// `signal` through `kernel`, aligned so that output sample `i` is centred on input sample `i`,
/// with silence beyond both ends: NumPy's `convolve(…, mode="same")` for a kernel of odd length.
///
/// By FFT, as one product of two spectra: the direct sum is 511 multiplications a sample, which
/// unoptimised test code takes most of a minute over for a run's recordings.
fn filtered(signal: &[f64], kernel: &[f64]) -> Vec<f64> {
    use realfft::RealFftPlanner;
    if signal.is_empty() {
        return Vec::new();
    }
    let size = (signal.len() + kernel.len() - 1).next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let forward = planner.plan_fft_forward(size);
    let inverse = planner.plan_fft_inverse(size);
    let spectrum = |values: &[f64]| {
        let mut padded = forward.make_input_vec();
        padded[..values.len()].copy_from_slice(values);
        let mut spectrum = forward.make_output_vec();
        forward
            .process(&mut padded, &mut spectrum)
            .expect("the lengths are the plan's");
        spectrum
    };
    let mut product = spectrum(signal);
    for (bin, k) in product.iter_mut().zip(spectrum(kernel)) {
        *bin *= k;
    }
    // Real spectra multiply to real ends; rounding must not make the inverse refuse them.
    for end in [0, product.len() - 1] {
        product[end].im = 0.0;
    }
    let mut full = inverse.make_output_vec();
    inverse
        .process(&mut product, &mut full)
        .expect("the lengths are the plan's");
    let offset = (kernel.len() - 1) / 2;
    full[offset..offset + signal.len()]
        .iter()
        .map(|value| value / size as f64)
        .collect()
}

/// A recording, one vector per channel.
type Channels = Vec<Vec<f64>>;

/// Split interleaved `f32` samples into channels.
fn deinterleave(samples: &[f32], channels: usize) -> Channels {
    (0..channels)
        .map(|channel| {
            samples
                .iter()
                .skip(channel)
                .step_by(channels)
                .map(|&sample| f64::from(sample))
                .collect()
        })
        .collect()
}

/// A lane's recordings as one: the first recorder's, with every other recorder's channels added
/// beside its own, moved onto the first one's frames. How far another recorder is from the first
/// is the median of how much more or less it had written at each of `frames` — the steady state's
/// start, then the switches — and the first one's are the frames of the whole. The analyser takes
/// the loudest channel sample by sample, so a tone that goes from one device to the other is heard
/// wherever it is, and a dip is a time when neither device had it. Two recorders are a quantum or
/// so apart this way: 20 ms, which a dip's 10 ms steps can see, and a switch's window cannot.
fn joined(recordings: Vec<(Channels, Vec<usize>)>) -> (Channels, Vec<usize>) {
    let mut recordings = recordings.into_iter();
    let (mut whole, first) = recordings.next().expect("a lane has a recorder");
    let frames = whole.first().map_or(0, Vec::len);
    for (channels, theirs) in recordings {
        let mut shifts: Vec<f64> = theirs
            .iter()
            .zip(&first)
            .map(|(theirs, ours)| *theirs as f64 - *ours as f64)
            .collect();
        let shift = median(&mut shifts).unwrap_or(0.0).round() as i64;
        for channel in channels {
            let moved = (0..frames)
                .map(|i| {
                    usize::try_from(i as i64 + shift)
                        .ok()
                        .and_then(|j| channel.get(j))
                        .copied()
                        .unwrap_or(0.0)
                })
                .collect();
            whole.push(moved);
        }
    }
    (whole, first)
}

/// What one switch did to one lane.
#[derive(Debug, Clone, Copy, PartialEq)]
struct EventResult {
    /// The loudest high-passed sample in the switch's window, in dBFS.
    worst_dbfs: f64,
    /// When it was, from the moment of the switch, in seconds.
    at_s: f64,
    /// The longest the tone was more than [`DIP_DB`] below its steady level, in ms.
    dip_ms: f64,
}

/// What one recording holds: its control, its tone, and every switch.
#[derive(Debug, Clone, PartialEq)]
struct Analysis {
    /// The loudest high-passed sample of the steady state, in dBFS.
    floor_dbfs: f64,
    /// When it was, from the start of the steady state, in seconds.
    floor_at_s: f64,
    /// The tone's steady level: the median 10 ms peak of the steady state, in dBFS.
    tone_dbfs: f64,
    events: Vec<EventResult>,
}

/// Where one row of a switch's table is read in a recording, in frames: the moment it is timed
/// from, the stretch its worst residual is looked for in, and the stretch its dip is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    at: usize,
    from: usize,
    to: usize,
    dip_to: usize,
}

impl Window {
    /// The window of a switch at frame `at`: [`WINDOW`] around it, the dip up to [`DIP_AFTER`]
    /// after it.
    fn around(at: usize) -> Self {
        Self {
            at,
            from: at.saturating_sub(seconds(WINDOW.0)),
            to: at + seconds(WINDOW.1),
            dip_to: at + seconds(DIP_AFTER),
        }
    }

    /// A switch at `at` that FxSound follows at `then` with a move of its own
    /// ([`Scenario::claims`]), read as two rows: what came before FxSound's move, from [`WINDOW`]
    /// before the switch — the session manager's move, which has no dip of its own to be held to —
    /// and FxSound's move from `then` on, for as long as a switch's window and a dip's.
    fn split(at: usize, then: usize) -> [Self; 2] {
        let from = at.saturating_sub(seconds(WINDOW.0));
        [
            Self {
                at,
                from,
                to: then.max(from),
                dip_to: from,
            },
            Self {
                at: then,
                from: then,
                to: then + seconds(WINDOW.1 + CLAIM_EXTRA),
                dip_to: then + seconds(DIP_AFTER + CLAIM_EXTRA),
            },
        ]
    }
}

/// Seconds as frames.
fn seconds(s: f64) -> usize {
    (s * RATE as f64) as usize
}

/// Read `recording` from frame `start` on, with a switch at each of `events` (frames). Nothing
/// before `start` counts: the recorder was running before the tone was.
fn analyse(recording: &Channels, start: usize, events: &[usize]) -> Analysis {
    let windows: Vec<Window> = events.iter().map(|&at| Window::around(at)).collect();
    analyse_windows(recording, start, &windows, events)
}

/// [`analyse`] with each row's own [`Window`], and the steady state kept clear of every frame of
/// `clear`: the switches, and every moment a row is timed from.
fn analyse_windows(
    recording: &Channels,
    start: usize,
    windows: &[Window],
    clear: &[usize],
) -> Analysis {
    let frames = recording.first().map_or(0, Vec::len);
    let kernel = high_pass_kernel(HIGH_PASS_HZ, TAPS);
    let mut residual = vec![0.0_f64; frames];
    let mut raw = vec![0.0_f64; frames];
    for channel in recording {
        for (i, sample) in filtered(channel, &kernel).into_iter().enumerate() {
            residual[i] = residual[i].max(sample.abs());
        }
        for (i, sample) in channel.iter().enumerate() {
            raw[i] = raw[i].max(sample.abs());
        }
    }

    // The steady state: past the start, clear of the end and of every switch.
    let mut steady = vec![false; frames];
    let end = frames.saturating_sub(seconds(TAIL));
    for flag in steady.iter_mut().take(end).skip(start) {
        *flag = true;
    }
    for &event in clear.iter().chain(windows.iter().map(|window| &window.at)) {
        let from = event.saturating_sub(seconds(CLEAR.0));
        let to = (event + seconds(CLEAR.1)).min(frames);
        for flag in steady.iter_mut().take(to).skip(from) {
            *flag = false;
        }
    }
    let (floor_at, floor) = residual
        .iter()
        .zip(&steady)
        .enumerate()
        .filter(|(_, (_, steady))| **steady)
        .fold((start, 0.0_f64), |(at, max), (i, (sample, _))| {
            if *sample > max {
                (i, *sample)
            } else {
                (at, max)
            }
        });

    // The tone's level, 10 ms at a time: the median peak of the steps wholly in the steady state.
    let steps: Vec<f64> = raw
        .chunks(LEVEL_STEP)
        .map(|step| step.iter().fold(0.0_f64, |max, sample| max.max(*sample)))
        .collect();
    let mut steady_steps: Vec<f64> = steps
        .iter()
        .enumerate()
        .filter(|(n, _)| {
            let from = n * LEVEL_STEP;
            steady
                .get(from..(from + LEVEL_STEP).min(frames))
                .is_some_and(|flags| flags.iter().all(|flag| *flag))
        })
        .map(|(_, peak)| *peak)
        .collect();
    let tone = median(&mut steady_steps).unwrap_or(0.0);
    let dipped = tone * 10_f64.powf(-DIP_DB / 20.0);

    let events = windows
        .iter()
        .map(|window| {
            let (event, from) = (window.at, window.from);
            let to = window.to.min(frames).max(from);
            let (at, worst) = residual
                .get(from..to)
                .unwrap_or_default()
                .iter()
                .enumerate()
                .fold((0, 0.0_f64), |(at, worst), (i, sample)| {
                    if *sample > worst {
                        (i, *sample)
                    } else {
                        (at, worst)
                    }
                });
            let first = from / LEVEL_STEP;
            let last = (window.dip_to / LEVEL_STEP).min(steps.len()).max(first);
            let longest = steps
                .get(first..last)
                .unwrap_or_default()
                .iter()
                .fold((0_usize, 0_usize), |(run, longest), peak| {
                    let run = if *peak < dipped { run + 1 } else { 0 };
                    (run, longest.max(run))
                })
                .1;
            EventResult {
                worst_dbfs: dbfs(worst),
                at_s: ((from + at) as f64 - event as f64) / RATE as f64,
                dip_ms: (longest * LEVEL_STEP * 1000 / RATE) as f64,
            }
        })
        .collect();
    Analysis {
        floor_dbfs: dbfs(floor),
        floor_at_s: floor_at.saturating_sub(start) as f64 / RATE as f64,
        tone_dbfs: dbfs(tone),
        events,
    }
}

/// How far above the tone's steady level row `row`'s worst residual is, in dB. A cut in the middle
/// of the wave is as loud as the wave it cuts, so this is what a click is worth whatever gain the
/// tone was played at: FxSound's chain plays the speakers' tone 11 dB above the bare one, and
/// every click on its way with it.
fn above_the_tone(analysis: &Analysis, row: usize) -> f64 {
    analysis.events[row].worst_dbfs - analysis.tone_dbfs
}

/// The median of `values`, sorted in place; the mean of the middle two of an even count. `None`
/// for none.
fn median(values: &mut [f64]) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(values[n / 2]),
        _ => Some(f64::midpoint(values[n / 2 - 1], values[n / 2])),
    }
}

/// Every node's error count in the last table `pw-top -b` printed, by id and name: the `ERR`
/// column, which counts a driver's xruns and a follower's missed cycles.
fn node_errors(listing: &str) -> Vec<(String, u64)> {
    let tables: Vec<&str> = listing.split("S   ID").collect();
    let Some(last) = tables.last() else {
        return Vec::new();
    };
    last.lines()
        .skip(1)
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let errors = words.get(8)?.parse::<u64>().ok()?;
            let name = words.last()?;
            Some((format!("{} {name}", words.get(1)?), errors))
        })
        .collect()
}

/// How many errors the graph's nodes had between two `pw-top` tables: every node's growth, and
/// the whole count of a node that is new.
fn errors_between(before: &[(String, u64)], after: &[(String, u64)]) -> u64 {
    after
        .iter()
        .map(|(node, errors)| {
            let earlier = before
                .iter()
                .find(|(other, _)| other == node)
                .map_or(0, |(_, errors)| *errors);
            errors.saturating_sub(earlier)
        })
        .sum()
}

// ---------------------------------------------------------------------------------------------
// The gate.

/// What a loud switch does ([`GATE_MODE`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gate {
    /// A gated switch at [`HARD_GATE_DBFS`] or louder fails the test.
    Hard,
    /// Nothing fails for loudness; a switch at [`SOFT_GATE_DBFS`] or louder is a warning.
    Report,
}

impl Gate {
    /// The mode the environment asks for: [`GATE_MODE`] when it is set, [`Gate::Report`] under
    /// CI, [`Gate::Hard`] otherwise.
    pub(super) fn from_env() -> Self {
        Self::of(
            std::env::var(GATE_MODE).ok().as_deref(),
            std::env::var("CI").ok().as_deref(),
        )
    }

    /// The mode for [`GATE_MODE`]'s value `mode` and `CI`'s value `ci`.
    fn of(mode: Option<&str>, ci: Option<&str>) -> Self {
        match (mode, ci) {
            (Some("report"), _) => Self::Report,
            (Some("hard"), _) => Self::Hard,
            (Some(other), _) => panic!("{GATE_MODE}={other}: it is `hard` or `report`"),
            (None, Some("true")) => Self::Report,
            (None, _) => Self::Hard,
        }
    }

    /// The verdict on a switch that left `worst`, `gated` or only reported: the mark its line in
    /// the table gets, and whether it fails the test.
    fn judge(self, worst: f64, gated: bool) -> (&'static str, bool) {
        match (gated, self) {
            (false, _) => ("reported", false),
            (true, Self::Hard) if worst >= HARD_GATE_DBFS => ("LOUD, over the gate", true),
            (true, Self::Report) if worst >= SOFT_GATE_DBFS => {
                ("WARNING: over the soft threshold", false)
            }
            (true, Self::Report) if worst >= HARD_GATE_DBFS => ("over the gate", false),
            (true, _) => ("ok", false),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The bench: a graph, the engine on it, and what the app would send it.

/// A switch made under the tone.
#[derive(Debug, Clone)]
enum Switch {
    /// The power button: both lanes' snapshots, both default claims, and on the way back on the
    /// filter reset the app sends (`App::handle_one`, `UiAction::TogglePower`).
    Power(bool),
    /// A music preset picked, by its name: the snapshot the app publishes for it.
    Music(String, Box<DspParams>),
    /// A voice preset picked: its snapshot and its chain.
    Voice(String, Box<InputDspParams>, String),
    /// A preset of its own for the tone's player and recorder, or none again: the rules the app
    /// sends ([`UiToAudio::SetAppRoutes`]), with the lanes' own snapshots, so that the move onto
    /// the route and back is all that changes.
    Route(bool),
    /// FxSound's own pick of a device for both lanes, as the device list, `--output`,
    /// `--next-output` and `--input` make it ([`UiToAudio::SelectDevice`]): the other devices when
    /// `true`, the first ones again when `false`.
    Device(bool),
    /// The desktop's pick of a default device for both directions, as `wpctl set-default` makes it
    /// — the configured keys of the `default` metadata: the other devices when `true`, the first
    /// ones again when `false`.
    Desktop(bool),
    /// «Like FxSound for Windows» moved to a level: the music snapshot the app publishes at it
    /// (`App::set_windows_parity`), to the speakers' lane and to the applications' output routes
    /// alike, which play the Windows build's DSP from Interface and sound on.
    Parity(WindowsParity, Box<DspParams>),
}

impl Switch {
    fn label(&self) -> String {
        match self {
            Self::Power(true) => "power on".to_owned(),
            Self::Power(false) => "power off".to_owned(),
            Self::Music(name, _) => format!("music preset {name}"),
            Self::Voice(name, ..) => format!("voice preset {name}"),
            Self::Route(true) => "route on".to_owned(),
            Self::Route(false) => "route off".to_owned(),
            Self::Device(other) => format!("FxSound picks {}", lane_devices(*other).join(", ")),
            Self::Desktop(other) => format!("desktop picks {}", lane_devices(*other).join(", ")),
            Self::Parity(level, _) => format!("like Windows: {}", level.key()),
        }
    }
}

/// The devices of the graph ([`PolicyGraph`]) a lane is on, in [`DeviceDirection::ALL`]'s order:
/// the ones WirePlumber picks first, or the `other` ones.
fn lane_devices(other: bool) -> [&'static str; 2] {
    if other {
        ["t_other", "t_mic2"]
    } else {
        ["t_stereo", "t_mic"]
    }
}

/// Where the tone's streams are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Streams {
    /// On FxSound's own nodes, and kept there (`node.dont-move`): only FxSound acts on them.
    Pinned,
    /// Following the default, as most applications do: WirePlumber moves them when it changes.
    Following,
}

/// One scenario: its streams, the lanes it records, the switches, and whether they are gated.
struct Scenario {
    /// The graph's tag: short, for the socket path.
    tag: &'static str,
    title: &'static str,
    streams: Streams,
    lanes: &'static [DeviceDirection],
    switches: Vec<Switch>,
    /// Between two switches: long enough for whatever a switch moves to have moved.
    spacing: Duration,
    /// Whether the switches are held to the gate.
    gated: bool,
    /// Said with the table: why switches FxSound makes itself are only reported.
    note: &'static str,
    /// Whether FxSound's power is off before the tone starts, the defaults on the devices.
    off: bool,
    /// Whether the tone is heard on both devices of its direction: the speakers' lane recorded at
    /// both sinks' monitors, both microphones fed the same tone. For the switches that move a lane
    /// or a stream from one device to the other.
    both_devices: bool,
    /// Switches made before the tone starts, and not measured: the levels a scenario plays at,
    /// a route of their own for the tone's player and recorder ([`Switch::Route`]), which they
    /// then start on, so that nothing moves them during the run.
    before: Vec<Switch>,
    /// What a gated scenario holds its switches to beside the gate, where it holds them to more.
    budget: Option<Budget>,
    /// Whether the tone's player and the microphone's recorder are PulseAudio applications —
    /// `pacat` through the graph's own `pipewire-pulse` ([`PolicyGraph::start_pulse`]) — rather
    /// than `pw-cat` and `pw-record`. The recorders at the sinks' monitors stand for the devices,
    /// and stay `pw-record`.
    pulse: bool,
    /// Whether FxSound takes each lane's default back after each switch — the desktop's pick with
    /// FxSound on — so that each switch is two moves, WirePlumber's and then FxSound's, and is read
    /// as two rows ([`Window::split`]): the first from the switch to the moment FxSound begins its
    /// claim (its [`AudioToUi::RememberedDefault`]), reported; FxSound's from then on, held to the
    /// gate when the scenario is `gated`.
    claims: bool,
    /// Whether the graph's WirePlumber runs FxSound's hook ([`crate::wireplumber_hook`]), which
    /// fades the streams it moves itself: then WirePlumber's move after a desktop pick is held to
    /// the gate too ([`Self::rows`]).
    hook: bool,
    /// Whether FxSound ranks the graph's devices, the first ones of each direction above the
    /// others ([`UiToAudio::SetDevicePriority`]), as the app has it unless *Follow the system's
    /// default device* is ticked; otherwise the engine follows the system's default, as it does
    /// until the app ranks its devices.
    ranked: bool,
}

impl Scenario {
    /// The rows of each lane's table, in order, and whether each is held to the gate: one per
    /// switch, or two for a scenario whose switches FxSound follows with a claim
    /// ([`Self::claims`]).
    fn rows(&self) -> Vec<(String, bool)> {
        self.switches
            .iter()
            .flat_map(|switch| {
                let label = switch.label();
                if self.claims {
                    vec![
                        (format!("{label}: the move"), self.gated && self.hook),
                        (format!("{label}: FxSound's claim"), self.gated),
                    ]
                } else {
                    vec![(label, self.gated)]
                }
            })
            .collect()
    }
}

/// What a switch that moves streams, and fades them to do it, is held to beside the gate (roadmap
/// §7, test 1): the gap it leaves in the tone and the steady state around it, and how many of its
/// switches may go over the gate run by run.
#[derive(Debug, Clone, Copy)]
struct Budget {
    /// The longest a switch may leave the tone dipped ([`EventResult::dip_ms`]): the fade, the
    /// move, the new link and the fade back.
    dip: Duration,
    /// The loudest the steady state may be ([`Analysis::floor_dbfs`]): a handover that left a
    /// stream somewhere it should not be, or at a volume it should not have, shows here.
    floor_dbfs: f64,
    /// How many switches of all the clean runs of a pass may each go over the gate, the median
    /// aside, before the pass fails. Every one is listed in a warning. One: PipeWire's converter
    /// plays a quantum at the new volume before the ramp to it now and then, a race FxSound can
    /// make rarer and cannot end (`crate::stream_handover::RAMP_STEP_SAMPLES`) — measured at two
    /// recorder switches in 160; a handover that goes wrong on its own leaves more than one.
    loud_runs: usize,
}

/// The moves FxSound makes itself — the power switch's and a route's (D2), a device FxSound picks
/// and the default it takes back after the desktop's pick (D3): a gap of a tenth of a second or so
/// instead of the click, and a steady state as quiet as FxSound's own.
const HANDOVER_BUDGET: Budget = Budget {
    dip: Duration::from_millis(200),
    floor_dbfs: -90.0,
    loud_runs: 1,
};

/// A recorder of the bench: `pw-record` writing raw stereo `f32` into the graph's directory.
struct Tap {
    _child: Guarded,
    file: PathBuf,
}

impl Tap {
    const CHANNELS: usize = 2;

    /// How many frames it has written.
    fn frames(&self) -> usize {
        policy::size_of(&self.file) as usize / (4 * Self::CHANNELS)
    }

    /// Whether the last 0.2 s it wrote hold the tone, within the patience.
    fn hears_the_tone(&self) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if self.last_peak(RATE / 5) > HEARD {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// The loudest of the last `frames` frames written.
    fn last_peak(&self, frames: usize) -> f32 {
        let Ok(mut file) = std::fs::File::open(&self.file) else {
            return 0.0;
        };
        let length = file.metadata().map_or(0, |meta| meta.len());
        let bytes = (frames * Self::CHANNELS * 4) as u64;
        let from = length.saturating_sub(bytes) / 4 * 4;
        let mut tail = Vec::new();
        if file.seek(std::io::SeekFrom::Start(from)).is_err()
            || file.read_to_end(&mut tail).is_err()
        {
            return 0.0;
        }
        let (words, _) = tail.as_chunks::<4>();
        words
            .iter()
            .map(|word| f32::from_le_bytes(*word).abs())
            .fold(0.0, f32::max)
    }

    /// What it wrote up to frame `end`, by channel.
    fn read_until(&self, end: usize) -> Channels {
        let bytes = std::fs::read(&self.file).unwrap_or_default();
        let length = bytes.len().min(end.saturating_mul(4 * Self::CHANNELS));
        let (words, _) = bytes[..length].as_chunks::<4>();
        let samples: Vec<f32> = words.iter().map(|word| f32::from_le_bytes(*word)).collect();
        deinterleave(&samples, Self::CHANNELS)
    }
}

/// A private graph with WirePlumber, the engine on it with both lanes attached, and the
/// snapshots the app would have published.
struct Bench {
    graph: PolicyGraph,
    engine: EngineHandle,
    music: DspParams,
    voice: InputDspParams,
    /// The voice chain the input lane runs.
    chain: String,
    /// The last status each lane reported, in [`DeviceDirection::ALL`]'s order.
    status: [AudioStatus; 2],
    /// Whether the tone's player and recorder have routes of their own ([`Switch::Route`]).
    routed: bool,
}

impl Bench {
    /// `None` when the graph could not run here (a skip, said by [`PolicyGraph::start`]). With
    /// FxSound's `hook` in the graph's WirePlumber, or without it.
    fn start(tag: &str, hook: bool) -> Option<Self> {
        let graph = if hook {
            PolicyGraph::start_with_the_hook(tag)?
        } else {
            PolicyGraph::start(tag)?
        };
        let engine =
            AudioEngine::start_with_remote(Some(&graph.remote())).expect("the engine should start");
        let mut said = Transcript::default();
        engine.send(UiToAudio::SelectDevice {
            node_name: "t_mic".to_owned(),
            direction: DeviceDirection::Input,
        });
        assert!(said.attached(&engine, DeviceDirection::Input, Some("t_mic")));
        for direction in DeviceDirection::ALL {
            assert_eq!(
                graph.default_settles_on(direction, our_node_name(direction)),
                Some(Ok(())),
                "FxSound never became the default {}",
                direction.key()
            );
        }
        let mut bench = Self {
            graph,
            engine,
            music: music_params(&e6_music(true)),
            voice: e6_voice(true).to_params(),
            chain: e6_voice(true).chain,
            status: [AudioStatus::default(); 2],
            routed: false,
        };
        bench.engine.set_params(bench.music);
        bench.engine.set_input_params(bench.voice);
        bench
            .engine
            .send(UiToAudio::SetInputChain(bench.chain.clone()));
        Some(bench)
    }

    /// Make `switch` as the app makes it.
    fn act(&mut self, switch: &Switch) {
        match switch {
            Switch::Power(on) => {
                self.music.power = *on;
                self.voice.power = *on;
                self.engine.set_params(self.music);
                self.engine.set_input_params(self.voice);
                for direction in DeviceDirection::ALL {
                    self.engine.send(UiToAudio::SetAsDefault {
                        direction,
                        want: *on,
                    });
                }
                if *on {
                    for direction in DeviceDirection::ALL {
                        self.engine
                            .send_event(direction, DspEvent::ResetFilterState);
                    }
                }
            }
            Switch::Music(_, params) => {
                self.music = DspParams {
                    power: self.music.power,
                    ..**params
                };
                self.engine.set_params(self.music);
            }
            Switch::Voice(_, params, chain) => {
                self.voice = InputDspParams {
                    power: self.voice.power,
                    ..**params
                };
                self.chain.clone_from(chain);
                self.engine.set_input_params(self.voice);
                self.engine.send(UiToAudio::SetInputChain(chain.clone()));
            }
            Switch::Route(on) => {
                self.routed = *on;
                self.send_routes();
            }
            Switch::Parity(_, params) => {
                self.music = DspParams {
                    power: self.music.power,
                    ..**params
                };
                self.engine.set_params(self.music);
                // The routes share the lane's levels and follow its snapshot
                // (`App::refresh_app_routes`): their parameters are written in place.
                if self.routed {
                    self.send_routes();
                }
            }
            Switch::Device(other) => {
                for (direction, node_name) in
                    DeviceDirection::ALL.into_iter().zip(lane_devices(*other))
                {
                    self.engine.send(UiToAudio::SelectDevice {
                        node_name: node_name.to_owned(),
                        direction,
                    });
                }
            }
            Switch::Desktop(other) => {
                for (direction, node_name) in
                    DeviceDirection::ALL.into_iter().zip(lane_devices(*other))
                {
                    let written = self
                        .graph
                        .write_default(devices::configured_default_key(direction), node_name);
                    assert!(written.is_some(), "pw-metadata could not pick {node_name}");
                }
            }
        }
    }

    /// Whether the tone's player is linked into one of FxSound's route nodes, within the
    /// patience: what a scenario that gives it a route of its own is measuring through.
    fn player_plays_through_a_route(&self) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            let player = self.graph.node_id("t_player");
            let routes: Vec<u64> = self
                .graph
                .dump()
                .unwrap_or_default()
                .iter()
                .filter(|object| {
                    object["type"].as_str() == Some("PipeWire:Interface:Node")
                        && object["info"]["props"]["node.name"]
                            .as_str()
                            .is_some_and(|name| name.starts_with(crate::ROUTE_NODE_PREFIX))
                })
                .filter_map(|object| object["id"].as_u64())
                .collect();
            if let Some(player) = player
                && routes
                    .iter()
                    .any(|route| self.graph.linked(player, *route) == Some(true))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// The rules the app sends for the tone's player and recorder: a preset of their own with
    /// the lanes' own snapshots while [`Bench::routed`], none otherwise.
    fn send_routes(&mut self) {
        let rule = |name: &str, direction, params, chain: &str| AppRoute {
            direction,
            app: AppKey {
                name: name.to_owned(),
                ..AppKey::default()
            },
            preset: "Clicks".to_owned(),
            params,
            chain: chain.to_owned(),
        };
        let rules = if self.routed {
            vec![
                rule(
                    "t_player",
                    DeviceDirection::Output,
                    RouteParams::Output(self.music),
                    "",
                ),
                rule(
                    "t_recorder",
                    DeviceDirection::Input,
                    RouteParams::Input(self.voice),
                    &self.chain,
                ),
            ]
        } else {
            Vec::new()
        };
        self.engine.send(UiToAudio::SetAppRoutes(rules));
    }

    /// Write a tone into the graph's directory as a Sun AU file of 32-bit floats, for `pw-cat`,
    /// or as raw little-endian ones, for `pacat` (`raw`): `seconds` long, with 20 ms raised-cosine
    /// fades at both ends, the same on every channel.
    fn tone_file(
        &self,
        name: &str,
        (hz, level): (f64, f64),
        channels: usize,
        seconds: f64,
        raw: bool,
    ) -> PathBuf {
        const AU_FLOAT: u32 = 6;
        let frames = (seconds * RATE as f64) as usize;
        let fade = RATE / 50;
        let amplitude = 10_f64.powf(level / 20.0);
        let data = u32::try_from(frames * channels * 4).expect("a tone of minutes at most");
        let mut bytes = Vec::new();
        if !raw {
            bytes.extend_from_slice(b".snd");
            for word in [24, data, AU_FLOAT, RATE as u32, channels as u32] {
                bytes.extend_from_slice(&word.to_be_bytes());
            }
        }
        for n in 0..frames {
            let edge = n.min(frames - 1 - n);
            let gain = if edge < fade {
                0.5 - 0.5 * (std::f64::consts::PI * edge as f64 / fade as f64).cos()
            } else {
                1.0
            };
            let phase = 2.0 * std::f64::consts::PI * hz * n as f64 / RATE as f64;
            let sample = (amplitude * gain * phase.sin()) as f32;
            for _ in 0..channels {
                if raw {
                    bytes.extend_from_slice(&sample.to_le_bytes());
                } else {
                    bytes.extend_from_slice(&sample.to_be_bytes());
                }
            }
        }
        let file = self
            .graph
            .dir
            .join(format!("{name}.{}", if raw { "raw" } else { "au" }));
        std::fs::write(&file, bytes).expect("the graph's directory is ours");
        file
    }

    /// `pw-cat` playing `file`, as the application `name`, into `target` and kept there, or to
    /// wherever WirePlumber sends it. `extra` is added to its properties. `None` when it never
    /// appeared.
    fn play(
        &self,
        name: &str,
        file: &std::path::Path,
        target: Option<&str>,
        extra: &str,
    ) -> Option<Guarded> {
        let pin = if target.is_some() {
            " node.dont-move = true node.dont-reconnect = true"
        } else {
            ""
        };
        let mut player = support::command("pw-cat");
        player
            .arg("--remote")
            .arg(self.graph.socket())
            .args(["--playback", "--latency", "1024"])
            .args(target.into_iter().flat_map(|target| ["--target", target]))
            .arg(format!(
                "--properties={{ node.name = {name} application.name = {name} \
                 media.name = {name}{pin} {extra} }}"
            ))
            .arg(file)
            .env("XDG_RUNTIME_DIR", self.graph.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.graph.socket())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        let log = self.graph.stderr_log(&mut player, name);
        let mut child = support::spawn(player).ok()?;
        let deadline = Instant::now() + PATIENCE;
        while self.graph.node_id(name).is_none() {
            if Instant::now() >= deadline {
                println!("{name} never appeared: {}", child.account(Some(&log)));
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(child)
    }

    /// `pw-record` as `name`, kept on `target` — the monitor of a sink when `monitor` — or following
    /// the default source when there is no target: then `pacat` instead when `pulse`.
    fn record(&self, name: &str, target: Option<&str>, monitor: bool, pulse: bool) -> Option<Tap> {
        let Some(target) = target else {
            let (child, file) = if pulse {
                self.graph.pulse_recorder(name)?
            } else {
                self.graph.follow_default_recorder(name)?
            };
            return Some(Tap {
                _child: child,
                file,
            });
        };
        let file = self.graph.dir.join(format!("{name}.raw"));
        let capture = if monitor {
            " stream.capture.sink = true"
        } else {
            ""
        };
        let mut recorder = support::command_writing_at_most("pw-record", RECORDING_LIMIT);
        recorder
            .arg("--remote")
            .arg(self.graph.socket())
            .args(["--target", target, "--latency", "1024"])
            .args(["--format", "f32", "--rate", "48000", "--channels", "2"])
            .arg(format!(
                "--properties={{ node.name = {name} application.name = {name} media.name = {name} \
                 node.dont-move = true node.dont-reconnect = true{capture} }}"
            ))
            .arg(&file)
            .env("XDG_RUNTIME_DIR", self.graph.dir.join("run"))
            .env("PIPEWIRE_REMOTE", self.graph.socket())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        let log = self.graph.stderr_log(&mut recorder, name);
        let mut child = support::spawn(recorder).ok()?;
        let deadline = Instant::now() + PATIENCE;
        while self.graph.node_id(name).is_none() {
            if Instant::now() >= deadline {
                println!("{name} never appeared: {}", child.account(Some(&log)));
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(Tap {
            _child: child,
            file,
        })
    }

    /// Feed `microphones` `file`, a mono tone: a player WirePlumber leaves alone, linked by hand
    /// into each virtual microphone's input, since a session manager links no player to a source.
    /// One player into all of them, so that they carry the same wave, in step.
    fn feed_the_microphone(&self, file: &std::path::Path, microphones: &[&str]) -> Option<Guarded> {
        let feed = self.play("t_feed", file, None, "node.autoconnect = false")?;
        for microphone in microphones {
            let deadline = Instant::now() + PATIENCE;
            while !self.graph.link_nodes("t_feed", microphone) {
                if Instant::now() >= deadline {
                    println!("the tone could not be linked into {microphone}");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Some(feed)
    }

    /// By how many frames each lane's ring has come up short or overflowed, and how often it had
    /// to resync, as the lanes last reported it, in [`DeviceDirection::ALL`]'s order: FxSound's own
    /// gaps, which `pw-top` cannot see. Each lane reports once a second.
    fn ring_trouble(&mut self) -> [u64; 2] {
        while let Some(message) = self.engine.try_recv() {
            if let AudioToUi::Status { direction, status } = message {
                self.status[lane_of(direction)] = status;
            }
        }
        self.status
            .map(|status| status.underrun_frames + status.dropped_frames + status.resyncs)
    }

    /// Wait until FxSound has begun to take the default of every lane of `lanes` back after a
    /// switch — its [`AudioToUi::RememberedDefault`] for the lane, said as the claim begins — and
    /// note how much each lane's first recorder had written by then ([`Lane::claims`]).
    fn mark_claims(&mut self, lanes: &mut [Lane], title: &str) {
        let deadline = Instant::now() + CLAIM_PATIENCE;
        let mut waiting: Vec<DeviceDirection> = lanes.iter().map(|lane| lane.direction).collect();
        while !waiting.is_empty() && Instant::now() < deadline {
            while let Some(message) = self.engine.try_recv() {
                match message {
                    AudioToUi::Status { direction, status } => {
                        self.status[lane_of(direction)] = status;
                    }
                    AudioToUi::RememberedDefault { direction, .. } => {
                        waiting.retain(|waits| *waits != direction);
                        if let Some(lane) =
                            lanes.iter_mut().find(|lane| lane.direction == direction)
                            && let Some((tap, _)) = lane.taps.first()
                        {
                            lane.claims.push(tap.frames());
                        }
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            waiting.is_empty(),
            "{title}: FxSound never took the default back for {waiting:?}"
        );
    }

    /// Every node's error count now; `None` when `pw-top` gave no answer.
    fn node_errors(&self) -> Option<Vec<(String, u64)>> {
        self.graph
            .tool("pw-top", &["-b", "-n", "2"])
            .map(|listing| node_errors(&listing))
    }
}

/// E6a's music preset: every effect but one up, and a curve with 9 dB near the output tone, so a
/// change of it moves the tone. `eq_on` is the only difference between its two versions.
fn e6_music(eq_on: bool) -> Preset {
    let mut preset = fxsound_preset::parse(E6_MUSIC.as_bytes()).expect("E6a's preset reads");
    preset.eq_on = eq_on;
    preset
}

/// E6a's voice preset (`E6 Voice EQ On.toml`), likewise with 8 dB near the microphone's tone.
fn e6_voice(eq_on: bool) -> InputPreset {
    use fxsound_preset::input::{Compressor, DeEsser, Equalizer, Gate};
    let default = InputPreset::default();
    InputPreset {
        name: format!("E6 Voice EQ {}", if eq_on { "On" } else { "Off" }),
        rnnoise: false,
        denoise: None,
        highpass_hz: 80.0,
        highpass_order: 2,
        makeup_db: 5.0,
        ceiling_db: -3.0,
        gate: default.gate.map(|gate| Gate {
            threshold_db: -48.0,
            release_ms: 250.0,
            hold_ms: 300.0,
            ..gate
        }),
        compressor: default.compressor.map(|compressor| Compressor {
            threshold_db: -20.0,
            release_ms: 200.0,
            ..compressor
        }),
        deesser: default.deesser.map(|deesser| DeEsser {
            threshold_db: -24.0,
            ..deesser
        }),
        eq: Equalizer {
            centers_hz: vec![
                62.5, 115.734, 214.311, 396.85, 734.867, 1360.79, 2519.84, 4666.12, 8640.48,
                16000.0,
            ],
            gains_db: vec![0.0, 1.5, -1.0, 8.0, -1.0, 0.0, 2.0, 0.0, 0.0, 0.0],
            enabled: eq_on,
            ..default.eq
        },
        ..default
    }
}

/// The `.fac` of E6a's music preset.
const E6_MUSIC: &str = "CLASS1 : Effect Type\n9: Version\nE6 EQ On\n0: Double Params Flag\n\
1: Total number of elements\n60: Main 0\n50: Main 1\n0: Main 2\n0: Main 3\n70: Main 4\n35: Main 5\n\
0: Element Number\n   0: Param 0\n   0: Param 1\n   0: Param 2\n   0: Param 3\n   0: Param 4\n\
   0: Param 5\n   0: Param 6\n7: Number of Application Dependent Integers\n\
0: Number of Application Dependent Reals\n0: Number of Application Dependent Strings\n\
1: Integer[0]\n1: Integer[1]\n0: Integer[2]\n1: Integer[3]\n1: Integer[4]\n0: Integer[5]\n\
2: Integer[6]\n10: Number of EQ Bands\n1: On/Off Flag\nBand 1\n   62.5: CF\n   0: Boost/Cut\n\
Band 2\n   115.734: CF\n   9: Boost/Cut\nBand 3\n   250: CF\n   2: Boost/Cut\nBand 4\n\
   396.85: CF\n   0: Boost/Cut\nBand 5\n   734.867: CF\n   2: Boost/Cut\nBand 6\n\
   1360.79: CF\n   2: Boost/Cut\nBand 7\n   2519.84: CF\n   1: Boost/Cut\nBand 8\n   5350: CF\n\
   -1: Boost/Cut\nBand 9\n   8640.48: CF\n   0: Boost/Cut\nBand 10\n   13800: CF\n   2: Boost/Cut\n";

/// The snapshot the app publishes for a music preset on the default ladder with the default
/// levels: `fxsound_dsp::preset::preset_params`, the one path from a preset to its snapshot.
fn music_params(preset: &Preset) -> DspParams {
    fxsound_dsp::preset::preset_params(
        preset,
        &fxsound_dsp::preset::ladder(eq::DEFAULT_BANDS),
        fxsound_dsp::preset::MusicLevels::default(),
    )
}

/// «Like FxSound for Windows» moved to `level` with E6a's music preset playing: the snapshot the
/// app publishes for it there (`App::set_windows_parity`, [`fxsound_dsp::preset::MusicLevels`]),
/// at −6 dB of master gain and +4 dB of balance — the levels the W1c live check heard the switch
/// click at, −39.4 dBFS on the way to Interface and sound and −25.1 on the way back
/// (`docs/0.5.0-dsp-inventory.md`).
fn to_parity(level: WindowsParity) -> Switch {
    let levels = fxsound_dsp::preset::MusicLevels {
        master_gain_db: -6.0,
        balance_db: 4.0,
        ..fxsound_dsp::preset::MusicLevels::default()
    }
    .with_windows_dsp(DspCompat::for_level(level).windows());
    let params = fxsound_dsp::preset::preset_params(
        &e6_music(true),
        &levels.ladder(eq::DEFAULT_BANDS),
        levels,
    );
    Switch::Parity(level, Box::new(params))
}

/// A shipped preset's file.
fn shipped(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/presets")
        .join(relative)
}

/// A shipped music preset, as a switch to it.
fn to_music(file: &str) -> Switch {
    let preset = fxsound_preset::load(&shipped(&format!("Factsoft/{file}")))
        .expect("a shipped music preset reads");
    Switch::Music(preset.name.clone(), Box::new(music_params(&preset)))
}

/// A shipped voice preset, as a switch to it.
fn to_voice(name: &str) -> Switch {
    let preset = InputPreset::load(&shipped(&format!("Input/{name}.toml")))
        .expect("a shipped voice preset reads");
    voice_switch(&preset)
}

fn voice_switch(preset: &InputPreset) -> Switch {
    Switch::Voice(
        preset.name.clone(),
        Box::new(preset.to_params()),
        preset.chain.clone(),
    )
}

// ---------------------------------------------------------------------------------------------
// Running a scenario.

/// A lane's place in [`DeviceDirection::ALL`].
fn lane_of(direction: DeviceDirection) -> usize {
    usize::from(direction == DeviceDirection::Input)
}

/// A lane being recorded in a run: its recorders, each with how much it had written when the
/// steady state started and when each switch was made.
struct Lane {
    direction: DeviceDirection,
    taps: Vec<(Tap, Vec<usize>)>,
    /// How much the first recorder had written when FxSound began to take the lane's default back
    /// after each switch ([`Scenario::claims`]).
    claims: Vec<usize>,
}

/// One run of a scenario: each lane's recording read, in the scenario's order of lanes, and
/// whether the graph had xruns during it (`None`: not counted).
struct Run {
    lanes: Vec<Analysis>,
    xruns: Option<u64>,
    /// The frames each lane's ring came up short or overflowed by, and its resyncs, in
    /// [`DeviceDirection::ALL`]'s order.
    ring: [u64; 2],
    /// A scenario of the desktop's picks with FxSound on ([`Scenario::claims`]), heard on both
    /// speakers: each pick's WirePlumber's move read at the monitor of the speakers FxSound had
    /// played through before it alone, the loudest high-passed sample in dBFS
    /// ([`left_behind`]). Empty for any other scenario.
    left_behind: Vec<f64>,
}

/// Run `scenario` once on a fresh bench. `None` when it cannot run here.
fn run_once(scenario: &Scenario) -> Option<Run> {
    let mut bench = Bench::start(scenario.tag, scenario.hook)?;
    for tool in ["pw-cat", "pw-record", "pw-dump", "pw-link", "pw-metadata"] {
        if !installed(tool) {
            skip(&format!(
                "{tool} is not installed, so {} cannot run",
                scenario.title
            ));
            return None;
        }
    }
    if scenario.pulse && !bench.graph.start_pulse() {
        return None;
    }
    // Longer than the run by far: the tone must not end inside what is read.
    let length = 15.0 + scenario.spacing.as_secs_f64() * (scenario.switches.len() + 1) as f64;
    let pinned = scenario.streams == Streams::Pinned;
    if scenario.ranked {
        for (lane, direction) in DeviceDirection::ALL.into_iter().enumerate() {
            bench.engine.send(UiToAudio::SetDevicePriority {
                direction,
                names: [false, true]
                    .map(|other| lane_devices(other)[lane].to_owned())
                    .to_vec(),
                new_devices_first: false,
            });
        }
    }
    // Before any stream exists, so that a route is where the player and the recorder start.
    for switch in &scenario.before {
        bench.act(switch);
    }
    if scenario.off {
        bench.act(&Switch::Power(false));
        for (direction, device) in DeviceDirection::ALL.into_iter().zip(lane_devices(false)) {
            assert_eq!(
                bench.graph.default_settles_on(direction, device),
                Some(Ok(())),
                "the power off never handed the default {} back",
                direction.key()
            );
        }
    }
    // Each direction's devices: the first, and the other when the tone is heard on both.
    let [speakers, microphones] = [0, 1].map(|lane| {
        let mut devices = vec![lane_devices(false)[lane]];
        if scenario.both_devices {
            devices.push(lane_devices(true)[lane]);
        }
        devices
    });

    let mut streams = Vec::new();
    let mut lanes = Vec::new();
    for &direction in scenario.lanes {
        let taps = match direction {
            DeviceDirection::Output => {
                let tone = bench.tone_file("t_tone", OUTPUT_TONE, 2, length, scenario.pulse);
                let target = pinned.then_some(SINK_NODE_NAME);
                let player = if scenario.pulse {
                    bench.graph.pulse_player("t_player", Some(&tone))
                } else {
                    bench.play("t_player", &tone, target, "")
                };
                streams.push(player.expect("the tone plays"));
                let at = |sink: &str| {
                    bench.record(&format!("t_monitor_{sink}"), Some(sink), true, false)
                };
                speakers.iter().map(|sink| at(sink)).collect::<Vec<_>>()
            }
            DeviceDirection::Input => {
                let tone = bench.tone_file("t_voice", INPUT_TONE, 1, length, false);
                streams.push(
                    bench
                        .feed_the_microphone(&tone, &microphones)
                        .expect("the microphone is fed"),
                );
                vec![bench.record(
                    "t_recorder",
                    pinned.then_some(SOURCE_NODE_NAME),
                    false,
                    scenario.pulse,
                )]
            }
        };
        let taps = taps
            .into_iter()
            .map(|tap| (tap.expect("the recorder records"), Vec::new()))
            .collect();
        lanes.push(Lane {
            direction,
            taps,
            claims: Vec::new(),
        });
    }
    if scenario
        .before
        .iter()
        .any(|switch| matches!(switch, Switch::Route(true)))
        && scenario.lanes.contains(&DeviceDirection::Output)
    {
        assert!(
            bench.player_plays_through_a_route(),
            "{}: the tone's player never reached its route",
            scenario.title
        );
    }
    for lane in &lanes {
        assert!(
            lane.taps.iter().any(|(tap, _)| tap.hears_the_tone()),
            "{}: the {} lane's recorder never heard its tone",
            scenario.title,
            lane.direction.key()
        );
    }
    // The tone has only just begun: the music chain's leveller and boost take a second or two
    // to settle on it, and their gain moving under the tone leaves −55 dBFS over the high-pass.
    std::thread::sleep(SETTLE);
    let mark = |lanes: &mut Vec<Lane>| {
        for (tap, frames) in lanes.iter_mut().flat_map(|lane| &mut lane.taps) {
            frames.push(tap.frames());
        }
    };
    mark(&mut lanes);
    let errors_before = bench.node_errors();
    let ring_before = bench.ring_trouble();

    let begun = Instant::now();
    for (n, switch) in scenario.switches.iter().enumerate() {
        let due = begun + scenario.spacing * (n as u32 + 1);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        mark(&mut lanes);
        bench.act(switch);
        if scenario.claims {
            bench.mark_claims(&mut lanes, scenario.title);
        }
    }
    std::thread::sleep(scenario.spacing.max(Duration::from_millis(2_200)));
    let ends: Vec<Vec<usize>> = lanes
        .iter()
        .map(|lane| lane.taps.iter().map(|(tap, _)| tap.frames()).collect())
        .collect();

    let errors_after = bench.node_errors();
    let ring_after = bench.ring_trouble();
    let ring = [0, 1].map(|lane| ring_after[lane].saturating_sub(ring_before[lane]));
    let xruns = match (errors_before, errors_after) {
        (Some(before), Some(after)) => Some(errors_between(&before, &after)),
        _ => {
            unless_skipped(None::<()>, "pw-top", "whether the run had xruns");
            None
        }
    };
    assert!(
        bench.graph.session_manager_runs(),
        "the private WirePlumber went away"
    );
    drop(streams);
    let mut left_behind = Vec::new();
    let lanes = lanes
        .iter()
        .zip(ends)
        .map(|(lane, ends)| {
            let recordings = lane
                .taps
                .iter()
                .zip(ends)
                .map(|((tap, frames), end)| (tap.read_until(end), frames.clone()))
                .collect();
            let (recording, frames) = joined(recordings);
            if scenario.claims {
                let windows: Vec<Window> = frames[1..]
                    .iter()
                    .zip(&lane.claims)
                    .flat_map(|(&at, &then)| Window::split(at, then))
                    .collect();
                let clear: Vec<usize> = frames[1..].iter().chain(&lane.claims).copied().collect();
                if lane.direction == DeviceDirection::Output && scenario.both_devices {
                    left_behind = speakers_left_behind(scenario, &recording, &frames, &windows);
                }
                analyse_windows(&recording, frames[0], &windows, &clear)
            } else {
                analyse(&recording, frames[0], &frames[1..])
            }
        })
        .collect();
    bench.engine.shutdown();
    Some(Run {
        lanes,
        xruns,
        ring,
        left_behind,
    })
}

/// Each desktop pick's WirePlumber's move — the first of its two rows ([`Window::split`]) — read
/// at the monitor of the speakers FxSound had played through before it alone, out of a speakers'
/// lane `recording` [`joined`] from both monitors, each one's two channels in the order of
/// [`lane_devices`]: the loudest high-passed sample in dBFS, pick by pick. What is heard there is
/// the end of the sound FxSound's chain played, cut off by the move ([`left_behind`]).
fn speakers_left_behind(
    scenario: &Scenario,
    recording: &Channels,
    frames: &[usize],
    windows: &[Window],
) -> Vec<f64> {
    let speakers = [lane_devices(false)[0], lane_devices(true)[0]];
    let monitors: Vec<Analysis> = (0..speakers.len())
        .map(|monitor| {
            let channels = recording
                .iter()
                .skip(monitor * Tap::CHANNELS)
                .take(Tap::CHANNELS)
                .cloned()
                .collect();
            analyse_windows(&channels, frames[0], windows, &frames[1..])
        })
        .collect();
    scenario
        .switches
        .iter()
        .enumerate()
        .filter_map(|(n, switch)| {
            let Switch::Desktop(other) = switch else {
                return None;
            };
            // Before the pick of the `other` devices, FxSound played through the first ones.
            let before = lane_devices(!other)[0];
            let monitor = speakers.iter().position(|sink| *sink == before)?;
            Some(monitors.get(monitor)?.events.get(2 * n)?.worst_dbfs)
        })
        .collect()
}

/// A scenario's result: the median of its clean runs.
struct Report {
    text: String,
    failures: Vec<String>,
}

/// A scenario's runs: the clean ones, and the xruns of each one discarded.
struct Measured {
    clean: Vec<Run>,
    discarded: Vec<u64>,
}

/// Run `scenario` until [`REPEATS`] runs are clean or [`ATTEMPTS`] have been made. `None` when it
/// cannot run here.
fn measure(scenario: &Scenario) -> Option<Measured> {
    let mut clean = Vec::new();
    let mut discarded = Vec::new();
    for _ in 0..ATTEMPTS {
        if clean.len() == REPEATS {
            break;
        }
        let run = run_once(scenario)?;
        match run.xruns {
            Some(xruns) if xruns > 0 => discarded.push(xruns),
            _ => clean.push(run),
        }
    }
    Some(Measured { clean, discarded })
}

/// The table for `scenario` from its `clean` runs, with a line on the `discarded` ones, and every
/// switch the gate fails.
fn report(scenario: &Scenario, gate: Gate, clean: &[Run], discarded: &[u64]) -> Report {
    let mut text = String::new();
    let mut failures = Vec::new();
    let _ = writeln!(
        text,
        "# {}: the median of {} clean run(s); {} discarded for xruns {discarded:?}; gate {}",
        scenario.title,
        clean.len(),
        discarded.len(),
        match gate {
            Gate::Hard => "hard",
            Gate::Report => "report",
        }
    );
    if !scenario.note.is_empty() {
        let _ = writeln!(text, "{}", scenario.note);
    }
    if clean.is_empty() {
        let line = format!("{}: no run was free of xruns", scenario.title);
        let _ = writeln!(text, "WARNING: {line}");
        if gate == Gate::Hard {
            failures.push(line);
        }
        return Report { text, failures };
    }
    if clean.iter().any(|run| run.xruns.is_none()) {
        let _ = writeln!(text, "(xruns were not counted: pw-top gave no answer)");
    }
    // The runs of a handover's scenario whose switch went over the gate, whatever the median says.
    let mut over: Vec<String> = Vec::new();
    for (lane, direction) in scenario.lanes.iter().enumerate() {
        let (hz, level) = match direction {
            DeviceDirection::Output => OUTPUT_TONE,
            DeviceDirection::Input => INPUT_TONE,
        };
        let heard = match (direction, scenario.streams, scenario.both_devices) {
            (DeviceDirection::Output, Streams::Pinned, _) => {
                "played into fxsound_sink and kept there, recorded at the device's monitor"
            }
            (DeviceDirection::Output, Streams::Following, false) => {
                "played by an application that follows the default, recorded at the device's \
                 monitor"
            }
            (DeviceDirection::Output, Streams::Following, true) => {
                "played by an application that follows the default, recorded at both devices' \
                 monitors"
            }
            (DeviceDirection::Input, Streams::Pinned, _) => {
                "into the microphone, recorded from fxsound_source and kept there"
            }
            (DeviceDirection::Input, Streams::Following, false) => {
                "into the microphone, recorded by an application that follows the default source"
            }
            (DeviceDirection::Input, Streams::Following, true) => {
                "into both microphones, recorded by an application that follows the default source"
            }
        };
        let _ = writeln!(
            text,
            "## {}: {hz} Hz sine at {level} dBFS {heard}; high-passed at {HIGH_PASS_HZ} Hz",
            direction.key()
        );
        let pick = |f: &dyn Fn(&Analysis) -> f64| {
            let mut values: Vec<f64> = clean.iter().map(|run| f(&run.lanes[lane])).collect();
            median(&mut values).unwrap_or(f64::NAN)
        };
        let floor = pick(&|analysis| analysis.floor_dbfs);
        let tone = pick(&|analysis| analysis.tone_dbfs);
        let runs: Vec<String> = clean
            .iter()
            .map(|run| {
                let analysis = &run.lanes[lane];
                format!("{:.1} at {:.2} s", analysis.floor_dbfs, analysis.floor_at_s)
            })
            .collect();
        let _ = writeln!(
            text,
            "steady state: max {floor:.1} dBFS (runs: {}); the tone at {tone:.1} dBFS",
            runs.join(", ")
        );
        let ring: Vec<u64> = clean
            .iter()
            .map(|run| run.ring[lane_of(*direction)])
            .collect();
        if ring.iter().any(|frames| *frames > 0) {
            let _ = writeln!(
                text,
                "FxSound's ring came up short or overflowed by {ring:?} frames in the runs"
            );
        }
        if let Some(budget) = scenario.budget.filter(|_| scenario.gated)
            && floor > budget.floor_dbfs
        {
            let line = format!(
                "{} ({}): the steady state, {floor:.1} dBFS, is over {:.1} dBFS",
                scenario.title,
                direction.key(),
                budget.floor_dbfs
            );
            let _ = writeln!(text, "WARNING: {line}");
            if gate == Gate::Hard {
                failures.push(line);
            }
        }
        if floor >= FLOOR_LIMIT_DBFS && scenario.gated {
            let line = format!(
                "{} ({}): the floor, {floor:.1} dBFS, is too high to tell a click",
                scenario.title,
                direction.key()
            );
            let _ = writeln!(text, "WARNING: {line}");
            if gate == Gate::Hard {
                failures.push(line);
            }
        }
        for (n, (label, gated)) in scenario.rows().iter().enumerate() {
            let gated = *gated;
            let worst = pick(&|analysis| analysis.events[n].worst_dbfs);
            let at = pick(&|analysis| analysis.events[n].at_s);
            let dip = pick(&|analysis| analysis.events[n].dip_ms);
            let loudest = clean
                .iter()
                .map(|run| run.lanes[lane].events[n].worst_dbfs)
                .fold(f64::NEG_INFINITY, f64::max);
            let (mark, fails) = gate.judge(worst, gated);
            let _ = writeln!(
                text,
                "  {label:<32} worst {worst:6.1} dBFS (loudest run {loudest:6.1}) at {at:+.3} s  \
                 dip {dip:4.0} ms  {mark}"
            );
            if fails {
                failures.push(format!(
                    "{} ({}): {label} left {worst:.1} dBFS, over {HARD_GATE_DBFS} dBFS",
                    scenario.title,
                    direction.key(),
                ));
            }
            // A handover's switches are counted run by run too: the median hides a run in
            // twenty, and a run in twenty is a click a user hears (`Budget::loud_runs`).
            if scenario.budget.is_some() && gated {
                for (number, run) in clean.iter().enumerate() {
                    let value = run.lanes[lane].events[n].worst_dbfs;
                    if value >= HARD_GATE_DBFS {
                        over.push(format!(
                            "{} {label} in run {} at {value:.1} dBFS",
                            direction.key(),
                            number + 1
                        ));
                    }
                }
            }
            if let Some(budget) = scenario.budget.filter(|_| gated)
                && dip > budget.dip.as_secs_f64() * 1000.0
            {
                let line = format!(
                    "{} ({}): {label} left the tone dipped for {dip:.0} ms, over {} ms",
                    scenario.title,
                    direction.key(),
                    budget.dip.as_millis()
                );
                let _ = writeln!(text, "    WARNING: {line}");
                if gate == Gate::Hard {
                    failures.push(line);
                }
            }
        }
    }
    if let Some(budget) = scenario.budget.filter(|_| !over.is_empty()) {
        let switches = clean.len() * scenario.lanes.len() * scenario.switches.len();
        // (A scenario read in two rows a switch counts only FxSound's, the gated one.)
        let line = format!(
            "{}: {} of {switches} switches over {HARD_GATE_DBFS} dBFS, run by run: {}",
            scenario.title,
            over.len(),
            over.join("; ")
        );
        let _ = writeln!(text, "WARNING: {line}");
        if gate == Gate::Hard && over.len() > budget.loud_runs {
            failures.push(line);
        }
    }
    Report { text, failures }
}

/// Hold WirePlumber's move after each desktop pick with FxSound on — the ungated rows of a
/// scenario that `claims` ([`Scenario::rows`]) — to plain Linux + [`BARE_MARGIN_DB`], lane by lane
/// (roadmap 0.5.0 §7, test 4): the loudest of all the `clean` runs' moves against the loudest of
/// all the `bare` runs' switches of the same scenario with FxSound off, both read above the tone
/// ([`above_the_tone`]). The text for the table, and what the gate fails.
fn against_bare_linux(
    scenario: &Scenario,
    gate: Gate,
    clean: &[Run],
    bare: &Scenario,
    bare_clean: &[Run],
) -> Report {
    let mut text = String::new();
    let mut failures = Vec::new();
    let moves: Vec<usize> = scenario
        .rows()
        .iter()
        .enumerate()
        .filter(|(_, (_, gated))| !gated)
        .map(|(n, _)| n)
        .collect();
    let loudest = |runs: &[Run], lane: usize, rows: &[usize]| {
        runs.iter()
            .flat_map(|run| {
                rows.iter()
                    .map(|&row| above_the_tone(&run.lanes[lane], row))
            })
            .fold(None, |max: Option<f64>, value| {
                Some(max.map_or(value, |max| max.max(value)))
            })
    };
    for (lane, direction) in scenario.lanes.iter().enumerate() {
        let Some(bare_lane) = bare.lanes.iter().position(|other| other == direction) else {
            continue;
        };
        let bare_rows: Vec<usize> = (0..bare.rows().len()).collect();
        let (Some(on), Some(off)) = (
            loudest(clean, lane, &moves),
            loudest(bare_clean, bare_lane, &bare_rows),
        ) else {
            let line = format!(
                "{} ({}): no clean run to hold WirePlumber's move to plain Linux + \
                 {BARE_MARGIN_DB} dB",
                scenario.title,
                direction.key()
            );
            let _ = writeln!(text, "WARNING: {line}");
            if gate == Gate::Hard {
                failures.push(line);
            }
            continue;
        };
        let held = off + BARE_MARGIN_DB;
        let _ = writeln!(
            text,
            "## {}: WirePlumber's move, above the tone: loudest run {on:+.1} dB; plain Linux \
             ({}) {off:+.1} dB; held to {held:+.1} dB  {}",
            direction.key(),
            bare.title,
            if on <= held { "ok" } else { "LOUD" }
        );
        if on > held {
            let line = format!(
                "{} ({}): WirePlumber's move left {on:+.1} dB above the tone, over plain Linux's \
                 {off:+.1} + {BARE_MARGIN_DB} dB",
                scenario.title,
                direction.key()
            );
            let _ = writeln!(text, "WARNING: {line}");
            if gate == Gate::Hard {
                failures.push(line);
            }
        }
    }
    Report { text, failures }
}

/// How many of a pass's picks may leave [`HARD_GATE_DBFS`] or more at the speakers FxSound played
/// through, of all the clean runs, before [`left_behind`] fails it: a quarter. Now and then the
/// monitor's recorder skips a few milliseconds of the fade downstream of FxSound, whose own blocks
/// were played whole, and jumps into it — 8 picks in 144 over twelve passes at D4, six of them the
/// third, at −8.4 to −16.3 dBFS — which its recording cannot tell from a sound FxSound left cut
/// off. Without the fade every pick is one (roadmap 0.5.0 §7, D4).
const SKIPPED_FADES: f64 = 0.25;

/// Hold what WirePlumber's move after each desktop pick with FxSound on leaves on the speakers
/// FxSound had played through ([`Run::left_behind`]) to the gate, run by run: since D4 the end of
/// the sound the move cut off in FxSound's chain fades there (`crate::engine`, "A sound cut off"),
/// and what is left of the move is the application arriving on the picked speakers, unprocessed,
/// as in plain Linux ([`against_bare_linux`]). Every pick over the gate is listed; more than
/// [`SKIPPED_FADES`] of them fail the pass. The text for the table, and what the gate fails.
fn left_behind(scenario: &Scenario, gate: Gate, clean: &[Run]) -> Report {
    let mut text = String::new();
    let mut failures = Vec::new();
    let picks: Vec<(usize, usize, f64)> = clean
        .iter()
        .enumerate()
        .flat_map(|(run, measured)| {
            measured
                .left_behind
                .iter()
                .enumerate()
                .map(move |(pick, &worst)| (run, pick, worst))
        })
        .collect();
    let labels: Vec<String> = scenario.switches.iter().map(Switch::label).collect();
    if picks.is_empty() {
        let line = format!(
            "{}: no clean run read the speakers FxSound played through",
            scenario.title
        );
        let _ = writeln!(text, "WARNING: {line}");
        if gate == Gate::Hard {
            failures.push(line);
        }
        return Report { text, failures };
    }
    let _ = writeln!(
        text,
        "## output: WirePlumber's move at the monitor of the speakers FxSound played through"
    );
    for (n, label) in labels.iter().enumerate() {
        let mut values: Vec<f64> = picks
            .iter()
            .filter(|(_, pick, _)| *pick == n)
            .map(|(.., worst)| *worst)
            .collect();
        let loudest = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let worst = median(&mut values).unwrap_or(f64::NAN);
        let _ = writeln!(
            text,
            "  {label:<32} worst {worst:6.1} dBFS (loudest run {loudest:6.1})"
        );
    }
    let loud: Vec<String> = picks
        .iter()
        .filter(|(.., worst)| *worst >= HARD_GATE_DBFS)
        .map(|&(run, pick, worst)| {
            let label = labels.get(pick).map_or("?", String::as_str);
            format!("{label} in run {} at {worst:.1} dBFS", run + 1)
        })
        .collect();
    if loud.is_empty() {
        return Report { text, failures };
    }
    let line = format!(
        "{}: at the speakers FxSound played through, {} of {} picks over {HARD_GATE_DBFS} dBFS, \
         run by run: {}",
        scenario.title,
        loud.len(),
        picks.len(),
        loud.join("; ")
    );
    let _ = writeln!(text, "WARNING: {line}");
    if gate == Gate::Hard && loud.len() as f64 > picks.len() as f64 * SKIPPED_FADES {
        failures.push(line);
    }
    Report { text, failures }
}

/// Run `scenario`, print its table and add it to [`REPORT_FILE`], and fail the test on what the
/// gate fails.
fn check(scenario: &Scenario) {
    let gate = Gate::from_env();
    let Some(measured) = measure(scenario) else {
        return;
    };
    conclude(report(scenario, gate, &measured.clean, &measured.discarded));
}

/// Print `report`, add it to [`REPORT_FILE`], and fail the test on what the gate failed.
fn conclude(report: Report) {
    println!("{}", report.text);
    if let Some(path) = std::env::var_os(REPORT_FILE) {
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(format!("{}\n", report.text).as_bytes()));
        if let Err(error) = written {
            println!(
                "the report could not be added to {}: {error}",
                path.display()
            );
        }
    }
    assert!(
        report.failures.is_empty(),
        "{}\n{}",
        report.failures.join("\n"),
        report.text
    );
}

// ---------------------------------------------------------------------------------------------
// The scenarios.

/// With the streams on FxSound's own nodes, the power switch changes nothing but the processing:
/// the chains fade between processed and bypassed, and nothing is moved. Both lanes, off and on
/// twice. 0.4.0 fixed this in E6a: −5 dBFS before, −48 after.
#[test]
fn under_a_steady_tone_the_power_switch_does_not_click_in_streams_pinned_to_fxsound() {
    check(&Scenario {
        tag: "clk-pin",
        title: "power, streams pinned to FxSound",
        streams: Streams::Pinned,
        lanes: &DeviceDirection::ALL,
        switches: [false, true, false, true].map(Switch::Power).to_vec(),
        spacing: Duration::from_millis(2_500),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: Vec::new(),
        budget: None,
        pulse: false,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// With streams that follow the default, the power switch hands the defaults back or takes them,
/// and WirePlumber moves the streams — unlinking before it links, in the middle of the wave. Since
/// D2 FxSound fades each stream it is about to have moved to silence first, and gives it its volume
/// back once it is linked where it went (`crate::stream_handover`): gated, with the dip each move
/// leaves held to [`HANDOVER_BUDGET`] (roadmap §7, test 1).
#[test]
fn under_a_steady_tone_the_power_switch_moves_the_streams_that_follow_the_default_without_a_click()
{
    check(&Scenario {
        tag: "clk-follow",
        title: "power, streams that follow the default",
        streams: Streams::Following,
        lanes: &DeviceDirection::ALL,
        switches: [false, true, false, true].map(Switch::Power).to_vec(),
        spacing: Duration::from_secs(3),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: Vec::new(),
        budget: Some(HANDOVER_BUDGET),
        pulse: false,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// The same with PulseAudio applications (roadmap §7, test 1: «pw-cat and pulse»): the tone's
/// player and the microphone's recorder are `pacat`, through the graph's own `pipewire-pulse`, as
/// most applications on a desktop still are. Their streams are `pipewire-pulse`'s, each with the
/// audio converter the fade is made in; and it is a PulseAudio application that a fade left at 0
/// would leave silent for good, with WirePlumber giving the silence back to its next stream while
/// the desktop's mixer shows 100 % (`crate::stream_handover`, the journal).
#[test]
fn under_a_steady_tone_the_power_switch_moves_pulseaudio_applications_that_follow_the_default_without_a_click()
 {
    check(&Scenario {
        tag: "clk-pulse",
        title: "power, PulseAudio applications following the default",
        streams: Streams::Following,
        lanes: &DeviceDirection::ALL,
        switches: [false, true, false, true].map(Switch::Power).to_vec(),
        spacing: Duration::from_secs(3),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: Vec::new(),
        budget: Some(HANDOVER_BUDGET),
        pulse: true,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// The music equalizer off and on, then four shipped presets, under the speakers' tone of an
/// application that follows the default: FxSound's own switches, which glide over 20 ms.
#[test]
fn under_a_steady_tone_the_equalizer_and_the_music_presets_switch_without_a_click() {
    let eq = |on: bool| {
        Switch::Music(
            format!("E6 EQ {}", if on { "On" } else { "Off" }),
            Box::new(music_params(&e6_music(on))),
        )
    };
    check(&Scenario {
        tag: "clk-music",
        title: "music equalizer and presets",
        streams: Streams::Following,
        lanes: &[DeviceDirection::Output],
        switches: vec![
            eq(false),
            eq(true),
            to_music("8.fac"),
            to_music("5.fac"),
            to_music("4.fac"),
            to_music("1.fac"),
        ],
        spacing: Duration::from_secs(2),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: Vec::new(),
        budget: None,
        pulse: false,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// «Like FxSound for Windows» moved between Off and Interface and sound and back, twice, under
/// the speakers' tone of an application that follows the default (roadmap 0.5.0 §1 A4): the
/// output lane changes DSP under the stream, the gain stage moving from before the equalizer to
/// inside its block, and nothing moves the stream. FxSound's own switch, gated.
#[test]
fn under_a_steady_tone_moving_like_fxsound_for_windows_between_off_and_sound_does_not_click() {
    check(&Scenario {
        tag: "clk-parity",
        title: "like Windows, Off and Interface and sound",
        streams: Streams::Following,
        lanes: &[DeviceDirection::Output],
        switches: vec![
            to_parity(WindowsParity::Sound),
            to_parity(WindowsParity::Off),
            to_parity(WindowsParity::Sound),
            to_parity(WindowsParity::Off),
        ],
        spacing: Duration::from_secs(2),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: vec![to_parity(WindowsParity::Off)],
        budget: None,
        pulse: false,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// The same moves with the tone's player on a route of its own from before it starts: the
/// application's output route changes DSP with the lane (`App::refresh_app_routes`), its
/// parameters written in place, and nothing moves the stream. Gated.
#[test]
fn under_a_steady_tone_moving_like_fxsound_for_windows_in_an_application_route_does_not_click() {
    check(&Scenario {
        tag: "clk-parroute",
        title: "like Windows, Off and Interface and sound, in an application's route",
        streams: Streams::Following,
        lanes: &[DeviceDirection::Output],
        switches: vec![
            to_parity(WindowsParity::Sound),
            to_parity(WindowsParity::Off),
            to_parity(WindowsParity::Sound),
            to_parity(WindowsParity::Off),
        ],
        spacing: Duration::from_secs(2),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: vec![to_parity(WindowsParity::Off), Switch::Route(true)],
        budget: None,
        pulse: false,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// The voice equalizer off and on, then three shipped voice presets, under the microphone's tone
/// recorded by an application that follows the default source. 0.4.0 fixed these in E6a: −21 dBFS
/// before, −74 after.
#[test]
fn under_a_steady_tone_the_voice_equalizer_and_the_voice_presets_switch_without_a_click() {
    let eq = |on: bool| voice_switch(&e6_voice(on));
    check(&Scenario {
        tag: "clk-voice",
        title: "voice equalizer and presets",
        streams: Streams::Following,
        lanes: &[DeviceDirection::Input],
        switches: vec![
            eq(false),
            eq(true),
            to_voice("Clean Voice"),
            to_voice("Studio"),
            to_voice("Noisy Room"),
        ],
        spacing: Duration::from_secs(2),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: Vec::new(),
        budget: None,
        pulse: false,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// A preset of its own for the tone's player and recorder, then none, twice (roadmap §7, test 2):
/// the engine writes each stream's target, and WirePlumber moves it onto the route and back —
/// unlinking before it links, as for the power switch. Since D2 each stream is faded to silence
/// before its key is written, and the route it leaves goes only once it is off it: gated, with the
/// dip held to [`HANDOVER_BUDGET`].
#[test]
fn under_a_steady_tone_an_application_route_moves_the_streams_on_and_off_without_a_click() {
    check(&Scenario {
        tag: "clk-route",
        title: "application route on and off",
        streams: Streams::Following,
        lanes: &DeviceDirection::ALL,
        switches: [true, false, true, false].map(Switch::Route).to_vec(),
        spacing: Duration::from_secs(3),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: Vec::new(),
        budget: Some(HANDOVER_BUDGET),
        pulse: false,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// FxSound picks the other speakers and the other microphone, then the first ones again, twice
/// (roadmap §7, test 3), as the device list, `--output`, `--next-output` and `--input` do. 0.4.0
/// took both of a lane's nodes down and built them again on the new device, and meanwhile
/// WirePlumber moved the streams that follow the default onto a device of its own and back. Since
/// D3 the lane keeps its virtual node, nothing moves the streams, and the chain falls silent while
/// its stream on the device is replaced (`crate::engine`, "Another device, the same virtual
/// node"): gated, with the gap held to [`HANDOVER_BUDGET`]. The tone is heard on both devices of
/// each direction.
#[test]
fn under_a_steady_tone_fxsound_picks_another_device_without_a_click() {
    check(&Scenario {
        tag: "clk-device",
        title: "FxSound picks another device",
        streams: Streams::Following,
        lanes: &DeviceDirection::ALL,
        switches: [true, false, true, false].map(Switch::Device).to_vec(),
        spacing: Duration::from_secs(3),
        gated: true,
        note: "",
        off: false,
        both_devices: true,
        before: Vec::new(),
        budget: Some(HANDOVER_BUDGET),
        pulse: false,
        claims: false,
        hook: false,
        ranked: false,
    });
}

/// The desktop's pick of the other speakers and the other microphone as the defaults, then of the
/// first ones again, twice, with FxSound `on` (roadmap §7, test 4) or off (test 5). The tone is
/// heard on both devices of each direction.
fn desktop_picks(on: bool) -> Scenario {
    Scenario {
        tag: if on { "clk-deskon" } else { "clk-deskoff" },
        title: if on {
            "desktop picks the default device, FxSound on"
        } else {
            "desktop picks the default device, FxSound off"
        },
        streams: Streams::Following,
        lanes: &DeviceDirection::ALL,
        switches: [true, false, true, false].map(Switch::Desktop).to_vec(),
        spacing: Duration::from_secs(3),
        gated: on,
        note: if on {
            "Each pick is two moves: WirePlumber's, from the switch to FxSound's claim, held to \
             plain Linux + 3 dB above the tone (roadmap 0.5.0 §7 test 4, §14 #16), and FxSound's \
             claim of the default back, gated"
        } else {
            ""
        },
        off: !on,
        both_devices: true,
        before: Vec::new(),
        budget: on.then_some(HANDOVER_BUDGET),
        pulse: false,
        claims: on,
        hook: false,
        ranked: false,
    }
}

/// [`desktop_picks`] with FxSound's hook in the graph's WirePlumber (roadmap §7, D5; test 4 "with
/// the hook"): every move WirePlumber makes itself is faded, so each pick is held to the gate,
/// WirePlumber's move as well as FxSound's claim after it with FxSound `on`.
///
/// With FxSound off, FxSound ranks its devices ([`Scenario::ranked`]), as it does unless *Follow
/// the system's default device* is ticked. Following it, FxSound's lane — out of the sound, with
/// the power off — moves to the picked device too, about 0.2 s after the pick, and that move
/// clicks the stream the hook has just faded in there: −23 to −32 dBFS where the pick itself was
/// −60 (measured for D5, 3 runs × 4 picks; with the hook's fade in held back 300 ms, past it, the
/// click went). That is FxSound's own lane moving under another application's sound, not
/// WirePlumber's move, and not this scenario's.
fn desktop_picks_with_the_hook(on: bool) -> Scenario {
    Scenario {
        tag: if on { "clk-hookon" } else { "clk-hookoff" },
        title: if on {
            "desktop picks the default device, FxSound on, FxSound's hook in WirePlumber"
        } else {
            "desktop picks the default device, FxSound off, FxSound's hook in WirePlumber"
        },
        gated: true,
        note: if on {
            "Each pick is two moves: WirePlumber's, faded by FxSound's hook in WirePlumber, and \
             FxSound's claim of the default back; both gated (roadmap 0.5.0 §7 test 4, D5)"
        } else {
            "WirePlumber's move alone, faded by FxSound's hook in WirePlumber, with FxSound \
             ranking its devices; gated (roadmap 0.5.0 §7, D5)"
        },
        budget: Some(HANDOVER_BUDGET),
        hook: true,
        ranked: !on,
        ..desktop_picks(on)
    }
}

/// The desktop's picks with FxSound off, measured once for both the tests that read them: plain
/// Linux, which the picks with FxSound on are held to. `None` when they cannot run here.
fn bare_desktop_picks() -> Option<&'static Measured> {
    static MEASURED: std::sync::OnceLock<Option<Measured>> = std::sync::OnceLock::new();
    MEASURED
        .get_or_init(|| measure(&desktop_picks(false)))
        .as_ref()
}

/// The desktop picks the other speakers and the other microphone as the defaults, then the first
/// ones again, twice, with FxSound on (roadmap §7, test 4). WirePlumber moves the streams off
/// FxSound's nodes onto the picked devices at once, as it would without FxSound; FxSound, following
/// the system's default device as the engine does until the app ranks its devices, moves its lanes
/// there, keeping its virtual nodes, and takes the defaults back, fading the streams it moves.
/// Each switch is read as two rows ([`Scenario::claims`]): FxSound's claim, gated, with the gap
/// held to [`HANDOVER_BUDGET`]; and WirePlumber's move, which roadmap §14 #16 leaves where plain
/// Linux has it, held to the same picks with FxSound off + 3 dB ([`against_bare_linux`]): heard in
/// FxSound's chain, above the tone and not in dBFS, since the chain plays the tone louder than
/// plain Linux does. The tail it cuts off in FxSound's chain fades since D4 (`crate::engine`, "A
/// sound cut off"), held to the gate at the monitor of the speakers FxSound played through
/// ([`left_behind`]), and what is left is the application's arrival on the picked device.
#[test]
fn under_a_steady_tone_a_desktop_pick_with_fxsound_on_is_taken_back_without_a_click() {
    let gate = Gate::from_env();
    let scenario = desktop_picks(true);
    let Some(measured) = measure(&scenario) else {
        return;
    };
    let mut result = report(&scenario, gate, &measured.clean, &measured.discarded);
    let faded = left_behind(&scenario, gate, &measured.clean);
    result.text.push_str(&faded.text);
    result.failures.extend(faded.failures);
    if let Some(bare) = bare_desktop_picks() {
        let held = against_bare_linux(
            &scenario,
            gate,
            &measured.clean,
            &desktop_picks(false),
            &bare.clean,
        );
        result.text.push_str(&held.text);
        result.failures.extend(held.failures);
    }
    conclude(result);
}

/// The desktop's picks with FxSound on, with FxSound's hook in WirePlumber (roadmap §7, test 4
/// with the hook; D5): WirePlumber fades each stream it moves off FxSound's nodes onto the picked
/// device and back in once it is there, and FxSound takes the default back as without it. Both
/// moves of each pick are gated, and what the move leaves at the speakers FxSound played through is
/// held as without the hook ([`left_behind`]).
#[test]
fn under_a_steady_tone_a_desktop_pick_with_fxsound_on_and_its_hook_in_wireplumber_does_not_click() {
    let gate = Gate::from_env();
    let scenario = desktop_picks_with_the_hook(true);
    let Some(measured) = measure(&scenario) else {
        return;
    };
    let mut result = report(&scenario, gate, &measured.clean, &measured.discarded);
    let faded = left_behind(&scenario, gate, &measured.clean);
    result.text.push_str(&faded.text);
    result.failures.extend(faded.failures);
    conclude(result);
}

/// The desktop's picks with FxSound off, with FxSound's hook in WirePlumber and FxSound ranking its
/// devices: WirePlumber alone moves the streams, and the hook fades each move. Plain Linux's
/// click, which the option takes away with FxSound out of the sound too; gated.
#[test]
fn under_a_steady_tone_a_desktop_pick_with_fxsound_off_and_its_hook_in_wireplumber_does_not_click()
{
    check(&desktop_picks_with_the_hook(false));
}

/// The same picks with FxSound's power off (roadmap §7, test 5): FxSound is out of the sound, and
/// WirePlumber alone moves the streams from one device to the other — the bare Linux move every
/// other device switch is set beside. Reported.
#[test]
fn under_a_steady_tone_a_desktop_pick_of_the_default_device_with_fxsound_off_is_reported() {
    let Some(measured) = bare_desktop_picks() else {
        return;
    };
    conclude(report(
        &desktop_picks(false),
        Gate::from_env(),
        &measured.clean,
        &measured.discarded,
    ));
}

// ---------------------------------------------------------------------------------------------
// The analyser and the gate, without a graph.

/// A steady sine: `seconds` of `hz` at `level` dBFS.
fn sine(hz: f64, level: f64, seconds: f64) -> Vec<f64> {
    let amplitude = 10_f64.powf(level / 20.0);
    (0..(seconds * RATE as f64) as usize)
        .map(|n| amplitude * (2.0 * std::f64::consts::PI * hz * n as f64 / RATE as f64).sin())
        .collect()
}

#[test]
fn the_high_pass_takes_the_tones_away_and_lets_a_click_through() {
    let kernel = high_pass_kernel(HIGH_PASS_HZ, TAPS);
    for (hz, level) in [OUTPUT_TONE, INPUT_TONE] {
        let tone = sine(hz, level, 1.0);
        let residual = filtered(&tone, &kernel);
        let middle = &residual[RATE / 4..3 * RATE / 4];
        let worst = middle.iter().fold(0.0_f64, |max, x| max.max(x.abs()));
        assert!(dbfs(worst) < -100.0, "{hz} Hz left {}", dbfs(worst));
    }
    let mut click = vec![0.0; RATE / 10];
    click[RATE / 20] = 0.5;
    let residual = filtered(&click, &kernel);
    let worst = residual.iter().fold(0.0_f64, |max, x| max.max(x.abs()));
    assert!(dbfs(worst) > -10.0, "an impulse left only {}", dbfs(worst));
}

#[test]
fn a_filtered_signal_is_centred_on_its_input() {
    let kernel = [0.25, 0.5, 0.25];
    let signal = [0.0, 0.0, 1.0, 0.0, 0.0];
    let out = filtered(&signal, &kernel);
    assert_eq!(out.len(), signal.len());
    for (got, want) in out.iter().zip([0.0, 0.25, 0.5, 0.25, 0.0]) {
        assert!((got - want).abs() < 1e-12, "{out:?}");
    }
}

#[test]
fn a_step_in_the_tone_is_found_at_its_switch_and_the_steady_state_stays_below_it() {
    let mut tone = sine(OUTPUT_TONE.0, OUTPUT_TONE.1, 6.0);
    let switch = 3 * RATE;
    // The wave jumps by half its amplitude a few ms after the switch, and stays jumped.
    for sample in &mut tone[switch + RATE / 50..] {
        *sample += 0.1;
    }
    let analysis = analyse(&vec![tone.clone(), tone], RATE / 2, &[switch]);
    assert!(analysis.floor_dbfs < -100.0, "{analysis:?}");
    assert!(
        (analysis.tone_dbfs - OUTPUT_TONE.1).abs() < 3.0,
        "{analysis:?}"
    );
    let event = analysis.events[0];
    assert!(event.worst_dbfs > -40.0, "{analysis:?}");
    assert!((event.at_s - 0.02).abs() < 0.002, "{analysis:?}");
    assert!(event.dip_ms < 1.0, "{analysis:?}");
}

#[test]
fn a_switch_followed_by_fxsounds_own_move_is_read_as_the_click_before_it_and_the_gap_after() {
    let mut tone = sine(OUTPUT_TONE.0, OUTPUT_TONE.1, 6.0);
    let switch = 3 * RATE;
    // The session manager's move: a jump 20 ms after the switch. FxSound's, 150 ms after: 100 ms
    // of silence, faded out and in over 20 ms, which is no click.
    tone[switch + RATE / 50] += 0.2;
    let claim = switch + 3 * RATE / 20;
    let fade = RATE / 50;
    for n in 0..RATE / 10 + 2 * fade {
        let gain = if n < fade {
            1.0 - n as f64 / fade as f64
        } else if n >= RATE / 10 + fade {
            (n - RATE / 10 - fade) as f64 / fade as f64
        } else {
            0.0
        };
        tone[claim + n] *= gain;
    }
    let windows = Window::split(switch, claim);
    let analysis = analyse_windows(&vec![tone], RATE / 2, &windows, &[switch, claim]);
    let [the_move, fxsounds] = [analysis.events[0], analysis.events[1]];
    assert!(the_move.worst_dbfs > -40.0, "{analysis:?}");
    assert!((the_move.at_s - 0.02).abs() < 0.002, "{analysis:?}");
    assert!(
        the_move.dip_ms < 1.0,
        "the move's row has no dip to hold: {analysis:?}"
    );
    assert!(fxsounds.worst_dbfs < -60.0, "{analysis:?}");
    assert!((90.0..=120.0).contains(&fxsounds.dip_ms), "{analysis:?}");
    assert!(analysis.floor_dbfs < -100.0, "{analysis:?}");
}

#[test]
fn a_click_is_read_above_the_tone_so_the_gain_the_tone_was_played_at_cancels() {
    // The same cut at two levels, 11 dB apart as the speakers' tone with and without FxSound's
    // chain: the stream stopped at the crest of a wave for 20 ms, and back.
    let cut = |level: f64| {
        let mut tone = sine(OUTPUT_TONE.0, level, 6.0);
        let switch = 3 * RATE;
        let at = switch + RATE / 50 + RATE / 400;
        for sample in &mut tone[at..at + RATE / 50] {
            *sample = 0.0;
        }
        let analysis = analyse(&vec![tone], RATE / 2, &[switch]);
        (analysis.events[0].worst_dbfs, above_the_tone(&analysis, 0))
    };
    let (bare, bare_above) = cut(-12.0);
    let (louder, louder_above) = cut(-1.0);
    assert!(bare > -40.0, "the cut is a click: {bare}");
    assert!((louder - bare - 11.0).abs() < 0.5, "{bare} and {louder}");
    assert!(
        (louder_above - bare_above).abs() < 0.5,
        "{bare_above} and {louder_above}"
    );
}

#[test]
fn wireplumbers_move_with_fxsound_on_is_held_to_plain_linux_plus_3_db_above_the_tone() {
    let (on, bare) = (desktop_picks(true), desktop_picks(false));
    let analysis = |tone_dbfs: f64, worst: &[f64]| Analysis {
        floor_dbfs: -110.0,
        floor_at_s: 1.0,
        tone_dbfs,
        events: worst
            .iter()
            .map(|&worst_dbfs| EventResult {
                worst_dbfs,
                at_s: 0.04,
                dip_ms: 0.0,
            })
            .collect(),
    };
    let run = |tone: f64, worst: &[f64]| Run {
        lanes: vec![analysis(tone, worst), analysis(tone, worst)],
        xruns: Some(0),
        ring: [0, 0],
        left_behind: Vec::new(),
    };
    // Plain Linux: the tone at −12 dBFS, its loudest move at −19, 7 dB below it.
    let bare_runs = [
        run(-12.0, &[-19.0, -21.0, -20.0, -23.0]),
        run(-12.0, &[-22.0, -20.0, -25.0, -21.0]),
    ];
    // FxSound on: the tone at −1 dBFS, WirePlumber's moves (the even rows) 11 dB louder in dBFS
    // than plain Linux's and 2 dB above them over the tone; FxSound's claims (the odd rows) are
    // loud here only to show that they are the gate's, not this.
    let moves = [-6.0, -2.0, -9.0, -2.0, -8.0, -2.0, -10.0, -2.0];
    let quiet = [run(-1.0, &moves), run(-1.0, &moves)];
    let held = against_bare_linux(&on, Gate::Hard, &quiet, &bare, &bare_runs);
    assert!(held.failures.is_empty(), "{:?}", held.failures);
    assert!(
        held.text.contains(
            "## output: WirePlumber's move, above the tone: loudest run -5.0 dB; plain Linux \
             (desktop picks the default device, FxSound off) -7.0 dB; held to -4.0 dB  ok"
        ),
        "{}",
        held.text
    );

    // One run's move 3.5 dB above the tone: 3.5 dB over plain Linux + 3.
    let mut louder = moves;
    louder[4] = -4.5;
    let loud = [run(-1.0, &moves), run(-1.0, &louder)];
    let held = against_bare_linux(&on, Gate::Hard, &loud, &bare, &bare_runs);
    assert_eq!(held.failures.len(), 2, "both lanes: {:?}", held.failures);
    let reported = against_bare_linux(&on, Gate::Report, &loud, &bare, &bare_runs);
    assert!(reported.failures.is_empty());
    assert!(reported.text.contains("WARNING"), "{}", reported.text);

    // Nothing to hold it to is no pass.
    let held = against_bare_linux(&on, Gate::Hard, &quiet, &bare, &[]);
    assert_eq!(held.failures.len(), 2, "{:?}", held.failures);
}

#[test]
fn a_desktop_pick_that_leaves_the_speakers_fxsound_played_through_loud_in_more_than_a_quarter_of_runs_fails()
 {
    let scenario = desktop_picks(true);
    let run = |left_behind: [f64; 4]| Run {
        lanes: Vec::new(),
        xruns: Some(0),
        ring: [0, 0],
        left_behind: left_behind.to_vec(),
    };
    let faded = [-95.0, -101.2, -88.8, -99.0];
    let quiet = [run(faded), run(faded), run(faded)];
    let held = left_behind(&scenario, Gate::Hard, &quiet);
    assert!(held.failures.is_empty(), "{:?}", held.failures);
    assert!(
        held.text
            .contains("  desktop picks t_other, t_mic2    worst  -88.8 dBFS (loudest run  -88.8)"),
        "{}",
        held.text
    );
    assert!(!held.text.contains("WARNING"), "{}", held.text);

    // The recorder skipped into the fade at the third pick of one run: listed, not failed.
    let skipped = [run(faded), run([-95.0, -101.2, -12.4, -99.0]), run(faded)];
    let held = left_behind(&scenario, Gate::Hard, &skipped);
    assert!(held.failures.is_empty(), "{:?}", held.failures);
    assert!(
        held.text.contains(
            "at the speakers FxSound played through, 1 of 12 picks over -40 dBFS, run by run: \
             desktop picks t_other, t_mic2 in run 2 at -12.4 dBFS"
        ),
        "{}",
        held.text
    );

    // Four picks of twelve: more than a quarter. Every pick, as before D4: the tail cut off.
    let four = [
        run([-12.0, -101.2, -12.4, -99.0]),
        run([-95.0, -14.0, -88.8, -9.0]),
        run(faded),
    ];
    assert_eq!(left_behind(&scenario, Gate::Hard, &four).failures.len(), 1);
    let cut = [-11.6, -14.0, -12.4, -9.0];
    let unfaded = [run(cut), run(cut), run(cut)];
    assert_eq!(
        left_behind(&scenario, Gate::Hard, &unfaded).failures.len(),
        1
    );
    assert!(
        left_behind(&scenario, Gate::Report, &unfaded)
            .failures
            .is_empty()
    );

    // Nothing read is no pass.
    assert_eq!(left_behind(&scenario, Gate::Hard, &[]).failures.len(), 1);
    let unread = [run(faded)].map(|mut run| {
        run.left_behind.clear();
        run
    });
    assert_eq!(
        left_behind(&scenario, Gate::Hard, &unread).failures.len(),
        1
    );
}

#[test]
fn a_gap_in_the_tone_is_measured_as_a_dip() {
    let mut tone = sine(INPUT_TONE.0, INPUT_TONE.1, 6.0);
    let switch = 3 * RATE;
    // 100 ms of silence, faded in and out over 5 ms so that little of it is a click.
    let fade = RATE / 200;
    for n in 0..RATE / 10 + 2 * fade {
        let gain = if n < fade {
            1.0 - n as f64 / fade as f64
        } else if n >= RATE / 10 + fade {
            (n - RATE / 10 - fade) as f64 / fade as f64
        } else {
            0.0
        };
        tone[switch + n] *= gain;
    }
    let analysis = analyse(&vec![tone], RATE / 2, &[switch]);
    let dip = analysis.events[0].dip_ms;
    assert!((90.0..=120.0).contains(&dip), "{analysis:?}");
}

#[test]
fn a_tone_that_moves_from_one_recorder_to_another_is_heard_across_the_move() {
    let tone = sine(OUTPUT_TONE.0, OUTPUT_TONE.1, 6.0);
    let switch = 3 * RATE;
    // The first recorder hears the tone until the switch, the second from then on; the second
    // started 1000 frames earlier, so its counts are 1000 frames ahead, give or take a quantum.
    let ahead = 1_000;
    let mut before = tone.clone();
    before[switch..].fill(0.0);
    let mut after = vec![0.0; ahead];
    after.extend(
        tone.iter()
            .enumerate()
            .map(|(i, x)| if i < switch { 0.0 } else { *x }),
    );
    let (whole, frames) = joined(vec![
        (vec![before], vec![RATE / 2, switch]),
        (
            vec![after],
            vec![RATE / 2 + ahead - 512, switch + ahead + 512],
        ),
    ]);
    assert_eq!(frames, [RATE / 2, switch]);
    assert_eq!(whole.len(), 2);
    let analysis = analyse(&whole, frames[0], &frames[1..]);
    assert!(analysis.events[0].dip_ms < 1.0, "{analysis:?}");
    assert!(
        (analysis.tone_dbfs - OUTPUT_TONE.1).abs() < 3.0,
        "{analysis:?}"
    );
}

#[test]
fn a_switch_in_a_steady_tone_that_does_nothing_leaves_the_floor() {
    let tone = sine(OUTPUT_TONE.0, OUTPUT_TONE.1, 6.0);
    let analysis = analyse(&vec![tone], RATE / 2, &[3 * RATE]);
    assert!(analysis.events[0].worst_dbfs < -100.0, "{analysis:?}");
    assert!(analysis.events[0].dip_ms < 1.0, "{analysis:?}");
}

#[test]
fn the_median_is_the_middle_value_or_the_mean_of_the_middle_two() {
    assert_eq!(median(&mut [-50.0, -20.0, -80.0]), Some(-50.0));
    assert_eq!(median(&mut [-50.0, -20.0]), Some(-35.0));
    assert_eq!(median(&mut []), None);
}

#[test]
fn xruns_are_the_growth_of_every_nodes_error_count_in_pw_top_s_last_table() {
    let first = "S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME \n\
                 R   30   1024  48000  20.1us  10.0us  0.00  0.00    2    F32LE 2 48000 t_stereo\n\
                 R   45   1024  48000  12.0us   5.0us  0.00  0.00    0    F32LE 2 48000  + fxsound_sink\n";
    let second = "S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME \n\
                  S   30      0      0    ---     ---   ---   ---     0                  t_stereo\n\
                  S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME \n\
                  R   30   1024  48000  20.1us  10.0us  0.00  0.00    3    F32LE 2 48000 t_stereo\n\
                  R   45   1024  48000  12.0us   5.0us  0.00  0.00    0    F32LE 2 48000  + fxsound_sink\n\
                  R   51   1024  48000  12.0us   5.0us  0.00  0.00    1    F32LE 2 48000  + t_player\n";
    let before = node_errors(first);
    assert_eq!(
        before,
        vec![
            ("30 t_stereo".to_owned(), 2),
            ("45 fxsound_sink".to_owned(), 0)
        ]
    );
    assert_eq!(errors_between(&before, &node_errors(second)), 2);
    assert_eq!(errors_between(&before, &before), 0);
}

#[test]
fn the_gate_fails_only_a_gated_switch_and_only_when_it_is_hard() {
    assert_eq!(Gate::Hard.judge(-35.0, true), ("LOUD, over the gate", true));
    assert_eq!(Gate::Hard.judge(-45.0, true), ("ok", false));
    assert_eq!(Gate::Hard.judge(-10.0, false), ("reported", false));
    assert_eq!(
        Gate::Report.judge(-25.0, true),
        ("WARNING: over the soft threshold", false)
    );
    assert_eq!(Gate::Report.judge(-35.0, true), ("over the gate", false));
    assert_eq!(Gate::Report.judge(-45.0, true), ("ok", false));
}

#[test]
fn a_handovers_switch_over_the_gate_in_one_run_is_listed_and_in_two_fails_the_hard_gate() {
    let scenario = Scenario {
        tag: "clk-t",
        title: "handover",
        streams: Streams::Following,
        lanes: &[DeviceDirection::Input],
        switches: [false, true].map(Switch::Power).to_vec(),
        spacing: Duration::from_secs(3),
        gated: true,
        note: "",
        off: false,
        both_devices: false,
        before: Vec::new(),
        budget: Some(HANDOVER_BUDGET),
        pulse: true,
        claims: false,
        hook: false,
        ranked: false,
    };
    let run = |worst: [f64; 2]| Run {
        lanes: vec![Analysis {
            floor_dbfs: -120.0,
            floor_at_s: 1.0,
            tone_dbfs: -18.0,
            events: worst
                .map(|worst_dbfs| EventResult {
                    worst_dbfs,
                    at_s: 0.25,
                    dip_ms: 110.0,
                })
                .to_vec(),
        }],
        xruns: Some(0),
        ring: [0, 0],
        left_behind: Vec::new(),
    };
    let quiet = [-70.0, -70.0];

    let once = [run(quiet), run([-70.0, -17.7]), run(quiet)];
    let report_once = report(&scenario, Gate::Hard, &once, &[]);
    assert!(
        report_once.failures.is_empty(),
        "the median hides it, and one is allowed: {:?}",
        report_once.failures
    );
    assert!(
        report_once
            .text
            .contains("1 of 6 switches over -40 dBFS, run by run: input power on in run 2"),
        "but it is listed: {}",
        report_once.text
    );

    let twice = [run(quiet), run([-70.0, -17.7]), run([-25.0, -70.0])];
    assert_eq!(report(&scenario, Gate::Hard, &twice, &[]).failures.len(), 1);
    assert!(
        report(&scenario, Gate::Report, &twice, &[])
            .failures
            .is_empty()
    );
}

#[test]
fn the_gate_is_hard_unless_asked_to_report_or_run_under_ci() {
    assert_eq!(Gate::of(None, None), Gate::Hard);
    assert_eq!(Gate::of(None, Some("true")), Gate::Report);
    assert_eq!(Gate::of(Some("hard"), Some("true")), Gate::Hard);
    assert_eq!(Gate::of(Some("report"), None), Gate::Report);
}

#[test]
fn every_switch_of_the_scenarios_reads_its_preset() {
    let music = ["8.fac", "5.fac", "4.fac", "1.fac"].map(to_music);
    let names: Vec<String> = music.iter().map(Switch::label).collect();
    assert_eq!(
        names,
        [
            "music preset Bass Boost",
            "music preset Gaming",
            "music preset Volume Boost",
            "music preset General"
        ]
    );
    for name in ["Clean Voice", "Studio", "Noisy Room"] {
        assert_eq!(to_voice(name).label(), format!("voice preset {name}"));
    }
    let on = music_params(&e6_music(true));
    let off = music_params(&e6_music(false));
    assert!(on.eq_on && !off.eq_on);
    assert_eq!(on.band_boost_db[1], 9.0);
    assert!(e6_voice(true).to_params().eq_on && !e6_voice(false).to_params().eq_on);
}
