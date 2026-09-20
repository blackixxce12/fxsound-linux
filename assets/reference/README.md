# Reference spectra

Six CSV files, read by `crates/fxsound-dsp/tests/genre_voicing.rs`, which asks whether a preset
named after a genre actually voices material toward that genre. Each file carries its own
provenance in `#` comments; this README holds what is common to all of them, the source tables in
full, and the things that are true of the set rather than of one file.

| file | what it is |
| --- | --- |
| `ltas-baseline.csv` | genre-neutral programme spectrum; the test synthesises its material to this |
| `ltas-classical.csv` | reference for the Classical preset |
| `ltas-jazz.csv` | reference for the Jazz preset |
| `ltas-metal.csv` | reference for the Metal preset |
| `ltas-pop.csv` | reference for the Pop preset |
| `ltas-trap.csv` | reference for the Trap preset |

Columns are `center_hz, level_db, baseline_db, genre_eq_db`, on the 29 third-octave centres from
31.5 Hz to 20 kHz. `level_db` is the curve the test compares against; the other two are its
decomposition, kept so that a hand-edit cannot quietly change what is being measured — a test
checks that `level_db == baseline_db + genre_eq_db` less its mean, and that `genre_eq_db` has the
2.00 dB RMS its header claims.

## Why these are built rather than measured

There is no per-genre long-term average spectrum available as numbers anywhere. Pestana and Reiss
publish theirs only as plots behind the AES paywall; iZotope's Tonal Balance curves and Har-Bal's
are proprietary and embedded in the plugins; the Million Song Dataset's twelve Echo Nest timbre
coefficients do not invert to a dB spectrum. `docs/preset-evaluation.md` records that survey.

What does exist, as numbers, are **two published per-genre equalizer tables** — consensus taste
curves rather than measurements, which is a weaker thing and is the thing there is. A genre EQ
preset says, in effect, "relative to generic programme material, this genre wants this much more
here and less there", so a genre's reference spectrum is approximated as *generic spectrum + that
genre's published EQ*. That is the whole derivation, and the per-file headers spell out each step.

## Source 1 — Android, used by every file here

`frameworks/av/media/libeffects/lvm/wrapper/Bundle/EffectBundle.h`, `EQNB_5BandSoftPresets`,
retrieved 2026-09-21 from `android.googlesource.com` branch `main`. Apache-2.0. Five peaking bands
at **60, 230, 910, 3600, 14000 Hz**, Q = 0.96, gains in dB:

| preset | 60 | 230 | 910 | 3.6 k | 14 k |
| --- | ---: | ---: | ---: | ---: | ---: |
| Normal | 3 | 0 | 0 | 0 | 3 |
| Classical | 5 | 3 | −2 | 4 | 4 |
| Dance | 6 | 0 | 2 | 4 | 1 |
| Flat | 0 | 0 | 0 | 0 | 0 |
| Folk | 3 | 0 | 0 | 2 | −1 |
| Heavy Metal | 4 | 1 | 9 | 3 | 0 |
| Hip Hop | 5 | 3 | 0 | 1 | 3 |
| Jazz | 4 | 2 | −2 | 2 | 5 |
| Pop | −1 | 2 | 5 | 1 | −2 |
| Rock | 5 | 3 | −1 | 3 | 5 |

The same header carries a second, stronger table, `EQNB_5BandNormalPresets`. The soft one is the
one `EqualizerSetPreset` installs, so the soft one is what is used here.

Mapping to this port's presets: Classical → Classical, Jazz → Jazz, **Heavy Metal → Metal**,
**Hip Hop → Trap**, Pop → Pop.

## Source 2 — Winamp/XMMS 1997, used only as a cross-check

The 1997 Winamp preset set that XMMS, Audacious, VLC and foobar2000 all carry. Ten bands at
**60, 170, 310, 600, 1000, 3000, 6000, 12000, 14000, 16000 Hz**. The three entries that overlap
this port's genre names:

| preset | 60 | 170 | 310 | 600 | 1 k | 3 k | 6 k | 12 k | 14 k | 16 k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Classical | 0 | 0 | 0 | 0 | 0 | 0 | −7.2 | −7.2 | −7.2 | −9.6 |
| Pop | −1.6 | 4.8 | 7.2 | 8.0 | 5.6 | 0 | −2.4 | −2.4 | −1.6 | −1.6 |
| Rock | 8.0 | 4.8 | −5.6 | −8.0 | −3.2 | 4.0 | 8.8 | 11.2 | 11.2 | 11.2 |

**The two sources disagree about Classical, and it matters.** Put through the same derivation and
normalised to the same 2.00 dB RMS, the Android-derived and Winamp-derived Classical curves are
**3.32 dB RMS apart** — Winamp cuts everything above 6 kHz hard, Android lifts the top. Under the
Winamp curve the Classical preset falls from first to fourth in its own column. The two Pop curves
are only **1.06 dB RMS apart**, and the Pop preset wins its column under either.

So of the five columns, **only Pop's result is robust to which published table you believe**. That
is recorded here rather than left for someone to rediscover.

## What has no reference, and why

Seven of the twelve genre presets have no column.

| preset | why not |
| --- | --- |
| 70's | an era, not a genre; no published curve anywhere |
| 80's | an era, not a genre; no published curve anywhere |
| Classic Rock | no published curve anywhere |
| Modern Country | no published curve anywhere |
| R&B | no published curve anywhere |
| Alternative Rock | both tables have exactly one "Rock" |
| Modern Rock | both tables have exactly one "Rock" |

The first five are the ones tracker item `s5-01` names: when those presets were revoiced they were
reasoned from spectral-analysis literature and the shipped house style, because no source says what
they should be. The last two are excluded for a different reason — there is a published "Rock"
curve, but this port ships three rock presets and deciding which one inherits it would be a choice
made by whoever wrote the test rather than by any source. All seven still *compete* in every
column; they simply have no column of their own.

## Regenerating

There is no generator script, deliberately: a script would be one more thing to keep true. Each
file's header states its derivation precisely enough to redo by hand from the tables above —
interpolate the published band gains onto the 29 third-octave centres linearly in dB against log
frequency, hold flat outside the outermost band, subtract the mean over 40 Hz–16 kHz, scale to
2.00 dB RMS over that band, add the baseline, and subtract the mean again. If you change the
method, change every file and the numbers in this README together; the test will catch a file that
no longer matches its own header, but nothing can catch a README that no longer matches the files.
