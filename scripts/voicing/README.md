# Blind listening harness for the genre presets

Tracker item `s5-10`. The objective half of the question — "are these twelve
presets distinct from one another, and is each one internally coherent" — is
already answered by `crates/fxsound-dsp/tests/shipped_presets.rs`. This directory
answers the other half, the one no measurement reaches: **is the revoicing any
good, and is it better than doing nothing at all.**

Nothing here touches Rust. It is four shell scripts around
`crates/fxsound-dsp/examples/process_wav.rs`, which already renders a WAV through
the real `Engine` with a `.fac` preset and needs no new flags for this.

## The question it asks

Not "does Jazz sound like jazz" — that has no answer. It asks:

> Of three level-matched renders of the same forty-five seconds, which do you
> want to keep listening to? And if the untouched one is the best of the three,
> say so.

Thirty seconds to answer, and it yields the two facts that can be acted on: a
direction of preference, and a floor check that catches a preset which is
actively harmful.

Blind, because whoever revoiced these presets would otherwise be grading their
own intent. Level-matched, because otherwise the answer is just "the loudest one"
— measured on one identical input, dry sits at −26.3 LUFS and the presets land
between −24.4 and −18.9, a 7.4 LU spread, because Dynamic Boost is never
bypassed. Any unmatched comparison grades Dynamic Boost's auto-gain and nothing
else.

## The one thing not to change

**The dry anchor is the untouched file.** `render.sh` copies the excerpt; it does
not render it with the effects off. `DspParams` has a `power` flag, `process_wav`
does not expose it, and even at `power = true` with every knob at zero, Dynamic
Boost still imposes its −0.3 dBFS ceiling and its auto-gain. A "bypass" render is
therefore not dry, and swapping one in would quietly destroy the floor check this
harness exists to perform. If a true engine-bypass render is ever wanted for some
other purpose, adding `--power 0` to `process_wav` is a two-line change — but it
is not what belongs here.

## What you need

`ffmpeg`, `ffprobe`, `mpv`, `shuf`, and `cargo` for the renderer. `render.sh`
has cargo rebuild `process_wav` on every run — a no-op when nothing has moved —
and re-renders anything a newer build invalidates, because a render made by a
stale engine looks exactly like a good one. On Arch:

```sh
sudo pacman -S ffmpeg mpv
```

Everything the harness writes goes to `target/voicing/`, which is gitignored.
Nothing lands in the source tree.

## The half-hour run

**10 minutes, once — material.** Drop one track per genre into
`target/voicing/material/`, named after the preset: `Jazz.flac`, `Metal.mp3`,
`R&B.opus`. Any format ffmpeg reads. Your own records first — familiarity beats
licensing for a private judgement, and these renders never leave the machine.

The default excerpt is 45 seconds starting one minute in. Pick a busy passage,
not an intro; to move the window per genre, write
`target/voicing/material/excerpts.csv`:

```csv
genre,start,duration
Jazz,92,45
Metal,145,45
```

`./render.sh --list` says which genres have material and which do not. The twelve
are `70's`, `80's`, `Alternative Rock`, `Classic Rock`, `Classical`, `Jazz`,
`Metal`, `Modern Country`, `Modern Rock`, `Pop`, `R&B`, `Trap`. Judge only the
ones you have material for — a partial answer is a real answer.

**2 minutes — render and match.**

```sh
./render.sh          # cuts each excerpt, renders it dry / shipped / revoiced
./match-and-mux.sh   # level-matches the three and hides them in one file
```

`render.sh` pulls the pre-revoicing presets straight out of git (the parent of
`10b3d14`, "Fix, revoice and extend the shipped presets"); set `FXSV_OLD_REF` to
compare against a different commit, or put `.fac` files in
`target/voicing/presets-old/` to compare against something not in history.

`match-and-mux.sh` measures all three with `ffmpeg ebur128`, pulls everything down
to the quietest of them — attenuation only, never boost, so nothing can clip — and
muxes them as three FLAC tracks in one `.mkv`, in an order shuffled per genre. It
re-measures the muxed tracks afterwards and fails loudly if they did not land
within 0.3 LU of each other.

The DSP is the cheap part: about 0.1 s per 30 s per preset. The ffmpeg loudness
passes dominate.

**25–35 minutes — listen.**

```sh
./vote.sh
```

One genre at a time, looping until you decide:

| key | what it does |
| --- | --- |
| `a` `s` `d` | switch between tracks 1, 2 and 3 — instantly, same position, same level |
| `1` `2` `3` | keep this one; records the vote and moves to the next genre |
| `b` `h` `t` `p` | mark it boomy / harsh / thin / pumping |
| `u` | mark this judgement unsure |
| `z` | undo the last thing you recorded for this genre |
| `q` | skip this genre without voting |

About 2.5 minutes each, two keystrokes. You never touch a terminal mid-session,
and you can stop anywhere — `./vote.sh` again picks up where you left off.

**0 minutes — the count.** `vote.sh` unblinds and prints it when the session
ends; `./vote.sh --tally` reprints it any time.

### Smoke-testing without music

```sh
./render.sh --synth-material && ./match-and-mux.sh && ./vote.sh
```

`--synth-material` fills `material/` with twelve synthetic beds — pink noise, a
kick with a real transient so the leveller and Dynamic Boost engage, and a tone
per genre — plus a short `excerpts.csv` so the whole thing runs in seconds. It
proves the pipeline works. It is not a listening test; the beds are not music and
a preference between them means nothing.

## What comes out, and what to do with it

`target/voicing/votes.csv`:

```csv
preset,winner,flags,confidence,picked_track,order,voted_at
Jazz,new,h,sure,2,old|new|dry,2026-09-21T02:50:16+04:00
```

- **`winner = new`** — the revoicing is an improvement. Nothing to do.
- **`winner = old`** — the revoicing is a regression. Diff the two `.fac` files
  (`git diff 10b3d14^ 10b3d14 -- "assets/presets/BonusPresets/<name>.fac"`) and
  put back what it took away.
- **`winner = dry`** — the preset loses to doing nothing at all. That is the
  finding worth having. Pull the voicing back toward flat rather than tuning it
  sideways.
- **`flags` contains `p`** (pumping) — look at Dynamic Boost before anything else.
  The port has a known cliff there: stored positions 6 through 10 are all
  +11.60 dB, so a preset sitting above stored 70 is shouting at the same volume
  regardless of what its slider says. The fix is to move the stored value down
  into the 0..70 range where it still does something, not to nudge the slider.
- **`flags` contains `b` / `h` / `t`** (boomy / harsh / thin) — a direction for
  the EQ, not a diagnosis. Check it against
  `crates/fxsound-dsp/tests/shipped_presets.rs` first: if that test already flags
  the preset for an objective reason, fix the objective cause before revoicing by
  ear.
- **`confidence = unsure`** — the renders are still on disk, so that preset can go
  straight into an ABX of exactly that pair (Lacinato's shootout mode reads them
  as they are) without rebuilding anything.

The file is a deliverable, not scratch. Copy it to `docs/voicing-verdicts.csv` if
you want it in the repo, together with what you were listening on — the verdict is
about a voicing *and* a pair of speakers.

`target/voicing/key.csv` holds the answer key and the measured loudness of every
render. Do not read it before listening.

## Limits, stated rather than engineered around

One listener, one machine, twelve items, one trial each. That is not a
statistically strong result and it should not be written up as one. Its job is to
catch presets that are obviously wrong or obviously worse than no processing,
which one attentive listener does reliably.

It does not exercise the PipeWire path, output-device selection, or preset
switching in the running app. Keep the live A/B through the running app as an
unblinded second pass for those — it catches clicks on preset change and how a
voicing wears over an hour, which offline rendering structurally cannot.

The offline path is not an approximation of the live one, though: PipeWire runs at
48 kHz with `clock.quantum` 1024 and `process_wav` uses `BLOCK = 1024` at the
file's rate, so with 48 kHz material both run the same rate and the same block
size, Dynamic Boost's time-varying auto-gain included. That is why `render.sh`
forces 48 kHz 16-bit stereo, and not only because `process_wav` reads 16-bit PCM.

The engine reports 36 frames of latency — 0.75 ms, constant across every preset —
so the dry track leads the other two by that much. Only one track plays at a time,
so it is inaudible on a switch and the dry anchor is left bit-exact. Set
`FXSV_ALIGN_DRY=1` to pad it into sample alignment if you would rather.

## Files

| file | what it is |
| --- | --- |
| `common.sh` | paths, the genre list, loudness measurement — sourced by the rest |
| `render.sh` | cuts each excerpt and renders it dry / shipped / revoiced |
| `match-and-mux.sh` | level-matches the three and hides them in one shuffled file |
| `vote.sh` | runs the session, records the answers, unblinds and counts them |

Each takes `--help`, and `--genre <name>` to work on one preset at a time.

One implementation note, because it looks like a detour otherwise: `vote.sh` runs
mpv once per genre with a generated per-genre `input.conf`, rather than once over a
twelve-item playlist. mpv's `run` command does spawn the callback correctly, but
property expansion in its arguments resolves to `(unavailable)` there, so a
binding cannot tell the callback which file it is playing. Hard-coding the genre
into a conf the shell writes is the reliable way round it.

Everything the harness assumes can be moved from the environment:

| variable | default | what it changes |
| --- | --- | --- |
| `FXSV_WORK` | `target/voicing` | where everything is written |
| `FXSV_MATERIAL` | `$FXSV_WORK/material` | where the source tracks live |
| `FXSV_OLD_REF` | `10b3d14^` | the commit the "old" presets come from |
| `FXSV_NEW_PRESETS` | `assets/presets/BonusPresets` | the "new" presets |
| `FXSV_START` / `FXSV_DURATION` | `60` / `45` | default excerpt window, in seconds |
| `FXSV_ALIGN_DRY` | `0` | pad the dry track by the engine's 0.75 ms latency |
| `FXSV_PROCESS_WAV` | `target/release/examples/process_wav` | the renderer; setting it also skips the cargo rebuild |
| `FXSV_MPV_ARGS` | — | extra arguments for mpv during the session |
