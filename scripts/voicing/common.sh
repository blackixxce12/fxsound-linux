#!/usr/bin/env bash
# Shared settings and helpers for the blind listening harness (tracker item s5-10).
#
# Sourced by render.sh, match-and-mux.sh and vote.sh; it does nothing on its own.
# Everything here can be overridden from the environment, so a maintainer can point
# the harness at a different preset set or a different scratch directory without
# editing a script.

set -euo pipefail

VOICING_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$VOICING_DIR/../.." && pwd)"

# Everything the harness produces lands under target/, which is already gitignored.
# Nothing it writes belongs in the source tree.
WORK="${FXSV_WORK:-$REPO_ROOT/target/voicing}"
MATERIAL_DIR="${FXSV_MATERIAL:-$WORK/material}"
EXCERPT_DIR="$WORK/excerpt"
RENDER_DIR="$WORK/render"
OLD_PRESET_DIR="$WORK/presets-old"
BLIND_DIR="$WORK/blind"
SESSION_DIR="$WORK/session"

KEY_CSV="$WORK/key.csv"
RAW_CSV="$WORK/raw-votes.csv"
VOTES_CSV="$WORK/votes.csv"

# The revoiced presets as they stand in the working tree.
NEW_PRESET_DIR="${FXSV_NEW_PRESETS:-$REPO_ROOT/assets/presets/BonusPresets}"

# The presets as they were before "Fix, revoice and extend the shipped presets"
# (10b3d14) landed on 0.3.0-dev. render.sh pulls them out of git at this ref;
# override it, or drop .fac files into $OLD_PRESET_DIR by hand, to compare against
# something else.
OLD_REF="${FXSV_OLD_REF:-10b3d14^}"

PROCESS_WAV="${FXSV_PROCESS_WAV:-$REPO_ROOT/target/release/examples/process_wav}"

# Where in the source track the excerpt starts, and how long it runs. Pick a busy
# passage, not an intro. Per-genre overrides go in $MATERIAL_DIR/excerpts.csv as
# "genre,start,duration".
EXCERPT_START="${FXSV_START:-60}"
EXCERPT_DUR="${FXSV_DURATION:-45}"

# The twelve genre presets. Era presets ("70's", "80's") are in the list because
# they still get judged by ear; they are the two that no spectral reference can
# reach, which is a limitation of s5-09, not of this harness.
GENRES=(
    "70's"
    "80's"
    "Alternative Rock"
    "Classic Rock"
    "Classical"
    "Jazz"
    "Metal"
    "Modern Country"
    "Modern Rock"
    "Pop"
    "R&B"
    "Trap"
)

# The three things the listener compares. "dry" is the untouched excerpt — never a
# render with the power off. DspParams has a power flag, but process_wav does not
# expose it, and even at power=true with every knob at zero Dynamic Boost still
# imposes its -0.3 dBFS ceiling and its auto-gain. A "bypass" render is therefore
# not dry, and using one would quietly destroy the floor check this harness exists
# to perform. Copy the source file instead.
VARIANTS=(dry old new)

# Engine latency, measured at 36 frames (0.75 ms at 48 kHz) and constant across
# every preset tried, so "old" and "new" are aligned with each other and only "dry"
# leads them. 0.75 ms is inaudible when only one track plays at a time, so the
# default is to leave dry bit-exact. Set FXSV_ALIGN_DRY=1 to pad it into alignment.
ALIGN_DRY="${FXSV_ALIGN_DRY:-0}"
ENGINE_LATENCY_FRAMES=36

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '%s\n' "$*" >&2; }
step() { printf '\n== %s\n' "$*" >&2; }

need_cmd() {
    for c in "$@"; do
        command -v "$c" >/dev/null 2>&1 || die "$c is not installed; the harness needs it. On Arch: pacman -S ${c}"
    done
}

# Genre names are used as bare CSV fields, so assert the assumption rather than
# discovering it broken halfway through a listening session.
assert_csv_safe() {
    case "$1" in
        *,*|*'"'*) die "field contains a comma or a quote and would corrupt the CSV: $1" ;;
    esac
}

# Integrated loudness in LUFS, via ffmpeg's ebur128 filter (BS.1770-4).
# Prints e.g. "-26.3". Digital silence comes back as -70.0, not -inf, because -70
# is where ebur128's absolute gate sits — so the guard is a floor, not a test for
# the string "-inf". A silent excerpt is almost always an excerpt window that
# landed in a gap, and matching to it would make every track unlistenable.
lufs() {
    local file="$1" out value
    out="$(ffmpeg -nostats -hide_banner -i "$file" -filter:a ebur128 -f null - 2>&1)" \
        || die "ffmpeg could not measure $file"
    value="$(printf '%s\n' "$out" \
        | sed -n '/Integrated loudness/,/Threshold/s/^[[:space:]]*I:[[:space:]]*\(-\?[0-9.]*\|-inf\)[[:space:]]*LUFS/\1/p' \
        | tail -n 1)"
    [ -n "$value" ] || die "could not parse integrated loudness out of ffmpeg for $file"
    if [ "$value" = "-inf" ] || awk -v v="$value" 'BEGIN{exit (v<=-60)?0:1}'; then
        die "$file measures $value LUFS — silent or near-silent, so it cannot be loudness-matched. Move the excerpt window."
    fi
    printf '%s\n' "$value"
}

# Resolve the source file for a genre: $MATERIAL_DIR/<genre>.<anything>.
material_for() {
    local genre="$1" f
    for f in "$MATERIAL_DIR/$genre".*; do
        [ -e "$f" ] || continue
        case "$f" in
            *.csv) continue ;;
        esac
        printf '%s\n' "$f"
        return 0
    done
    return 1
}

# Per-genre excerpt window, from $MATERIAL_DIR/excerpts.csv if it has a row.
excerpt_window() {
    local genre="$1" line
    if [ -f "$MATERIAL_DIR/excerpts.csv" ]; then
        line="$(awk -F, -v g="$genre" 'NR>1 && $1==g {print $2","$3; exit}' "$MATERIAL_DIR/excerpts.csv")"
        if [ -n "$line" ]; then
            printf '%s\n' "$line"
            return 0
        fi
    fi
    printf '%s,%s\n' "$EXCERPT_START" "$EXCERPT_DUR"
}

# Short fingerprint of a key row's track order, so a vote recorded against one
# shuffle can never be unblinded against a different one.
order_fingerprint() {
    printf '%s' "$1" | cksum | awk '{printf "%08x\n", $1}'
}
