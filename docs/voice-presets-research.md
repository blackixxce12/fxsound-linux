# Voice-preset research

Source material for the six factory input presets of 0.3.0 (tracker item `s3-05`). Gathered
2026-09-20 from public sources rather than from anyone's recollection: **116 sources, 71 of them
standards bodies, plugin source code or manufacturer documentation, and 290 attributed parameter
values**. The full machine-readable record, including every citation and every critique, is
`voice-presets-research.json` beside this file.

## Status

**These numbers are a reviewed draft, not settled values.** Each preset was designed against the
gathered evidence and then attacked by up to three independent reviewers — a tonal lens, a
dynamics lens and a fitness-for-purpose lens. They raised 59 quantitative objections, and five
reviews never ran. Apply the objections before shipping; *Warm Voice* has not been reviewed at all.

## What the evidence changed

- **The low end belongs to the high-pass, not to band 0.** Every preset below sets 62.5 Hz to
  exactly 0.0 dB. Cutting it as well as high-passing is the same job done twice, and on a male
  fundamental it is the difference between proximity control and a thin voice.
- **The moves are small.** Nothing exceeds ±2 dB. On a fixed ladder with derived Q, a large
  single-band move is a narrow artefact; a voice curve is built from broad, gentle ones.
- **The ceiling is −3 dBFS, not −1.** The output is re-encoded downstream — Opus for Discord, AAC
  for the streaming platforms — and a lossy encoder overshoots the sample peak it was given.
  ITU-R BS.1770-5 true-peak practice and the AES streaming recommendation both leave that room.

## Draft presets

| | HPF | Gate | Compressor | Makeup | De-esser | Ceiling |
|---|---|---|---|---|---|---|
| **Clean Voice** | 80 Hz | -45 dB, 2.0:1 | -18 dB, 3.0:1, 20/150 ms | +6.0 dB | 5500 Hz, -22 dB | -3.0 dBFS |
| **Streaming** | 100 Hz | -42 dB, 2.0:1 | -20 dB, 4.0:1, 15/150 ms | +12.0 dB | 5500 Hz, -25 dB | -3.0 dBFS |
| **Podcast** | 80 Hz | -45 dB, 2.0:1 | -20 dB, 3.0:1, 20/200 ms | +5.0 dB | 5500 Hz, -24 dB | -3.0 dBFS |
| **Discord** | 90 Hz | -45 dB, 2.0:1 | -18 dB, 3.0:1, 20/150 ms | +4.0 dB | 6000 Hz, -20 dB | -3.0 dBFS |
| **Warm Voice** | 75 Hz | -48 dB, 2.0:1 | -18 dB, 2.5:1, 25/200 ms | +5.0 dB | 6000 Hz, -18 dB | -3.0 dBFS |
| **Studio** | 75 Hz | off | -18 dB, 2.0:1, 25/200 ms | +1.5 dB | off | -3.0 dBFS |

Equalizer, on the fixed ten-band ladder (dB):

| | 62.5 | 115.734 | 214.311 | 396.85 | 734.867 | 1360.79 | 2519.84 | 4666.12 | 8640.48 | 16000 |
|---|---|---|---|---|---|---|---|---|---|---|
| **Clean Voice** | 0 | 0 | -1.0 | -1.5 | -0.5 | 0 | +1.5 | 0 | 0 | 0 |
| **Streaming** | 0 | 0 | -1.5 | -2.0 | -1.0 | 0 | +2.0 | +0.5 | +1.0 | -1.0 |
| **Podcast** | 0 | +1.5 | -1.0 | -2.0 | -1.0 | 0 | +2.0 | 0 | +1.0 | 0 |
| **Discord** | 0 | -2.0 | -1.0 | -2.0 | -1.0 | 0 | +2.0 | 0 | 0 | 0 |
| **Warm Voice** | 0 | +2.0 | +1.0 | -1.5 | -1.0 | 0 | -1.0 | -2.0 | -1.0 | -2.0 |
| **Studio** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |

## What the reviewers found

Two of these are not preset numbers at all — they are gaps in the parameter set that
`InputDspParams` has to close before any preset can be written down unambiguously:

1. **The compressor's detector is unspecified.** Peak and RMS detection of the same signal
   against the same threshold differ by 3–7 dB of gain reduction, so the identical preset means
   two different things. The detector, and its window, must be part of the parameter set.
2. **The gate has no range.** A downward expander with no cap on its attenuation pumps the noise
   floor in and out; every reference implementation surveyed (LSP, Calf, OBS) caps it. A
   `range_db` field is missing.

The rest are ordinary tuning disagreements, concentrated on the de-esser threshold (6 objections),
the makeup gain (6) and the high-pass corner (4).

## Reading the JSON

`research[]` holds each domain's sources, its attributed numbers and where they disagree.
`presets[]` holds each design with its per-value rationale and citations, and the critiques
against it. Nothing in it was taken from a forum post where a standard, a manufacturer or a
shipped implementation had an answer.
