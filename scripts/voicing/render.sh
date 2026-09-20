#!/usr/bin/env bash
# Step 1 of the blind listening harness: cut an excerpt per genre and render it
# three ways — untouched, through the shipped preset, through the revoiced one.
#
#   ./render.sh                 render every genre that has material
#   ./render.sh --genre Jazz    render one (repeatable)
#   ./render.sh --synth-material  fill material/ with synthetic beds and render them
#   ./render.sh --list          say what material is present and what is missing
#   ./render.sh --force         re-render even when the output is already current
#
# See README.md for the whole run.

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/common.sh"

FORCE=0
SYNTH=0
LIST_ONLY=0
SELECTED=()

while [ $# -gt 0 ]; do
    case "$1" in
        --force) FORCE=1 ;;
        --synth-material) SYNTH=1 ;;
        --list) LIST_ONLY=1 ;;
        --genre) shift; [ $# -gt 0 ] || die "--genre needs a name"; SELECTED+=("$1") ;;
        -h|--help) awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) die "unknown option $1" ;;
    esac
    shift
done

need_cmd ffmpeg
[ ${#SELECTED[@]} -gt 0 ] || SELECTED=("${GENRES[@]}")
for g in "${SELECTED[@]}"; do assert_csv_safe "$g"; done

mkdir -p "$MATERIAL_DIR" "$EXCERPT_DIR" "$RENDER_DIR" "$OLD_PRESET_DIR"

# ---------------------------------------------------------------- synthetic beds
# Not music, and not a listening test. This exists so the pipeline can be smoke
# tested end to end on a machine with no library and no network: broadband pink
# noise, a kick with a real transient so the leveller and Dynamic Boost actually
# engage, and a tone so the EQ has something tonal to move. Each genre gets a
# different seed and tone so the twelve files are not identical.
synthesise_material() {
    step "synthesising smoke-test material into $MATERIAL_DIR"
    local i=0 g dur=30
    for g in "${SELECTED[@]}"; do
        i=$((i + 1))
        local seed=$((i * 977)) tone=$((110 + i * 37))
        ffmpeg -v error -y \
            -f lavfi -i "anoisesrc=d=$dur:c=pink:r=48000:a=0.2:s=$seed" \
            -f lavfi -i "aevalsrc=exp(-9*mod(t\,0.5))*sin(2*PI*55*t):d=$dur:s=48000" \
            -f lavfi -i "sine=f=$tone:d=$dur:r=48000" \
            -filter_complex "[0:a][1:a][2:a]amix=inputs=3:normalize=0:weights=1 0.8 0.15,volume=0.55" \
            -ar 48000 -ac 2 -c:a pcm_s16le "$MATERIAL_DIR/$g.wav" \
            || die "could not synthesise material for $g"
        note "  $g.wav  (${dur}s, seed $seed, tone ${tone} Hz)"
    done
    # Short windows: a smoke test should finish in seconds, not minutes.
    {
        printf 'genre,start,duration\n'
        for g in "${SELECTED[@]}"; do printf '%s,1,12\n' "$g"; done
    } > "$MATERIAL_DIR/excerpts.csv"
    note "  excerpts.csv  (1 s in, 12 s long — delete it to go back to the ${EXCERPT_START}s/${EXCERPT_DUR}s default)"
}

[ "$SYNTH" -eq 1 ] && synthesise_material

# ------------------------------------------------------------------- stocktaking
if [ "$LIST_ONLY" -eq 1 ]; then
    printf '%-18s %-9s %s\n' GENRE STATUS SOURCE
    for g in "${SELECTED[@]}"; do
        if src="$(material_for "$g")"; then
            printf '%-18s %-9s %s\n' "$g" present "$(basename -- "$src")"
        else
            printf '%-18s %-9s %s\n' "$g" MISSING "-"
        fi
    done
    note ""
    note "material directory: $MATERIAL_DIR"
    exit 0
fi

# ------------------------------------------------------------------ old presets
# The comparison needs the presets as they were before the revoicing. Pull them
# out of git rather than asking anyone to keep a copy by hand; a .fac already
# sitting in $OLD_PRESET_DIR always wins, which is the escape hatch for comparing
# against something that is not in history.
step "collecting the pre-revoicing presets ($OLD_REF)"
git_ok=0
if command -v git >/dev/null 2>&1 && git -C "$REPO_ROOT" rev-parse --verify --quiet "$OLD_REF" >/dev/null 2>&1; then
    git_ok=1
fi
for g in "${SELECTED[@]}"; do
    [ -s "$OLD_PRESET_DIR/$g.fac" ] && continue
    if [ "$git_ok" -eq 1 ] \
        && git -C "$REPO_ROOT" show "$OLD_REF:assets/presets/BonusPresets/$g.fac" > "$OLD_PRESET_DIR/$g.fac" 2>/dev/null \
        && [ -s "$OLD_PRESET_DIR/$g.fac" ]; then
        note "  $g.fac  <- $OLD_REF"
    else
        rm -f "$OLD_PRESET_DIR/$g.fac"
        die "no pre-revoicing $g.fac. Either set FXSV_OLD_REF to a commit that has it, or put the file in $OLD_PRESET_DIR/ by hand."
    fi
done

# -------------------------------------------------------------------- the binary
# Let cargo decide whether the renderer is current, rather than only checking that
# a file is there. fxsound-dsp moves underneath this harness, and a stale
# process_wav renders the presets through an engine that no longer exists —
# silently, because the output is still a perfectly good WAV. cargo is a fast
# no-op when nothing has changed. An explicit FXSV_PROCESS_WAV is somebody else's
# binary and is taken as given.
if [ -z "${FXSV_PROCESS_WAV:-}" ] && command -v cargo >/dev/null 2>&1; then
    step "checking the offline renderer is current"
    ( cd "$REPO_ROOT" && cargo build --release -p fxsound-dsp --example process_wav ) \
        || die "could not build the process_wav example"
fi
[ -x "$PROCESS_WAV" ] \
    || die "no renderer at $PROCESS_WAV. Install cargo and re-run, or point FXSV_PROCESS_WAV at a build of the process_wav example."

# ------------------------------------------------------------------- the renders
newer_than() { [ -e "$1" ] && [ ! "$2" -nt "$1" ]; }

rendered=0
skipped=0
missing=()

for g in "${SELECTED[@]}"; do
    if ! src="$(material_for "$g")"; then
        missing+=("$g")
        continue
    fi

    excerpt="$EXCERPT_DIR/$g.wav"
    dry="$RENDER_DIR/$g.dry.wav"
    old="$RENDER_DIR/$g.old.wav"
    new="$RENDER_DIR/$g.new.wav"
    old_fac="$OLD_PRESET_DIR/$g.fac"
    new_fac="$NEW_PRESET_DIR/$g.fac"
    [ -s "$new_fac" ] || die "no revoiced preset at $new_fac"

    # The renderer counts as an input: a render made by an older process_wav is
    # stale the same way a render made from an older .fac is, and it is the one
    # kind of staleness nothing downstream can see.
    if [ "$FORCE" -eq 0 ] \
        && newer_than "$dry" "$src" \
        && newer_than "$old" "$old_fac" && newer_than "$old" "$PROCESS_WAV" \
        && newer_than "$new" "$new_fac" && newer_than "$new" "$PROCESS_WAV"; then
        skipped=$((skipped + 1))
        continue
    fi

    step "$g"
    IFS=, read -r ss dur <<< "$(excerpt_window "$g")"
    note "  excerpt: ${ss}s in, ${dur}s long, from $(basename -- "$src")"

    # 48 kHz 16-bit stereo is not a preference: process_wav reads 16-bit PCM only,
    # and 48 kHz is what the live PipeWire path runs at, so the offline render goes
    # through the same rate and the same 1024-frame block size as the real thing.
    ffmpeg -v error -y -ss "$ss" -t "$dur" -i "$src" \
        -ar 48000 -ac 2 -c:a pcm_s16le "$excerpt" \
        || die "could not cut the excerpt for $g"

    # Seeking past the end of a track is not an ffmpeg error: it writes a valid
    # WAV with no samples in it, and ffprobe then reports the duration as "N/A"
    # rather than as zero. Measure the payload instead, which is exact for PCM.
    got="$(awk -v b="$(stat -c %s -- "$excerpt")" \
        'BEGIN{printf "%.2f", (b-44)/(48000*2*2)}')"
    if awk -v d="$got" 'BEGIN{exit (d<1.0)?0:1}'; then
        die "the excerpt for $g is ${got}s long — is ${ss}s past the end of $(basename -- "$src")?"
    fi
    awk -v d="$got" -v w="$dur" 'BEGIN{exit (d < w-0.5)?0:1}' \
        && note "  note: only ${got}s of the requested ${dur}s were available"

    # THE anchor. A copy, not a render. Read the note beside VARIANTS in common.sh
    # before changing this line.
    cp -- "$excerpt" "$dry"

    "$PROCESS_WAV" "$excerpt" "$old" --preset "$old_fac" > "$RENDER_DIR/$g.old.log" 2>&1 \
        || { cat "$RENDER_DIR/$g.old.log" >&2; die "render failed: $g / old"; }
    "$PROCESS_WAV" "$excerpt" "$new" --preset "$new_fac" > "$RENDER_DIR/$g.new.log" 2>&1 \
        || { cat "$RENDER_DIR/$g.new.log" >&2; die "render failed: $g / new"; }

    for v in old new; do
        if grep -q 'exceeded full scale' "$RENDER_DIR/$g.$v.log"; then
            note "  note: $(grep 'exceeded full scale' "$RENDER_DIR/$g.$v.log") ($v)"
        fi
    done
    note "  dry/old/new written to $RENDER_DIR"
    rendered=$((rendered + 1))
done

step "rendered $rendered, up to date $skipped"
if [ ${#missing[@]} -gt 0 ]; then
    note "no material for: ${missing[*]}"
    note "drop one track per genre into $MATERIAL_DIR as <Genre>.<ext> (any format ffmpeg reads),"
    note "or run ./render.sh --synth-material to smoke-test the pipeline without music."
fi
[ "$rendered" -gt 0 ] || [ "$skipped" -gt 0 ] || die "nothing was rendered"
note "next: ./match-and-mux.sh"
