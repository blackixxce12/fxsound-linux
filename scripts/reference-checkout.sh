#!/usr/bin/env bash
# Check an older commit out beside this one, ready to be the other side of a two-build comparison:
# the bit-exactness harness of «Like FxSound for Windows»
# (crates/fxsound-dsp/tests/windows_parity_bitexact.rs, scripts/windows-parity-bitexact.sh), the
# drift measurement (crates/fxsound-dsp/tests/preset_drift.rs) and the "two engines" listening
# comparison (scripts/voicing/README.md).
#
# All three render a .fac the way the application plays it, through
# fxsound_dsp::preset::preset_params. A commit older than 0.5.0 has no such function: there the
# reading lived in the application (crates/fxsound-app/src/app.rs), where nothing outside it could
# call it. So when the checkout has no crates/fxsound-dsp/src/preset.rs, this writes one from that
# commit's own application — its music_controls, write_music_params, MusicLevels and ladder,
# copied as they are — and a preset_params that joins them the way 0.5.0's does. A commit older
# than the application's music_controls (0f05ba5 and before) played a preset on the preset's own
# band count, with the effects read through the sliders; its preset.rs says so and does that.
#
# A commit older than the Windows DSP of «Like FxSound for Windows» (0.5.0's W1b) has one DSP only,
# and no MusicLevels::with_windows_dsp to choose it with; the comparisons call it on both sides, so
# such a commit gets one that changes nothing. A commit older than the Windows reading of a preset
# (W1c) reads one way whichever DSP plays: it gets MusicLevels::ladder, slider_to_value and
# value_to_slider that are its own ladder and slider mappings.
#
# The measuring files are this checkout's, copied over: the harness, preset_drift.rs, their
# material modules and, with --with-process-wav, the process_wav example.
#
# Keep the directory off /tmp: two release builds of the workspace do not fit a tmpfs of a few
# gigabytes. The copy's target directory is inside it.
#
# usage: scripts/reference-checkout.sh [--with-process-wav] <commit> <directory>

set -euo pipefail

die() {
    echo "reference-checkout: $*" >&2
    exit 1
}

with_process_wav=0
args=()
for arg in "$@"; do
    case "$arg" in
        --with-process-wav) with_process_wav=1 ;;
        -h | --help)
            sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        -*) die "unknown option $arg" ;;
        *) args+=("$arg") ;;
    esac
done
[ "${#args[@]}" -eq 2 ] || die "usage: scripts/reference-checkout.sh [--with-process-wav] <commit> <directory>"
ref="${args[0]}"
dir="${args[1]}"

repo_root="$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)"
commit="$(git -C "$repo_root" rev-parse --verify --quiet "${ref}^{commit}")" \
    || die "no commit $ref"

if [ -e "$dir" ]; then
    [ -e "$dir/.git" ] || die "$dir exists and is not a checkout"
    here="$(git -C "$dir" rev-parse HEAD)"
    [ "$here" = "$commit" ] || die "$dir holds $here, not $ref ($commit)"
    # The files this script adds are its own; put them back as the commit has them, so that a
    # second run writes them afresh.
    git -C "$dir" checkout --quiet -- .
    git -C "$dir" clean --quiet -fd -- crates/fxsound-dsp
else
    mkdir -p "$(dirname "$dir")"
    git -C "$repo_root" worktree add --quiet --detach "$dir" "$commit"
fi
dir="$(cd "$dir" && pwd)"

tests="crates/fxsound-dsp/tests"
cp "$repo_root/$tests/windows_parity_bitexact.rs" "$repo_root/$tests/preset_drift.rs" "$dir/$tests/"
for module in genre_material voice_material output_material; do
    rm -rf "${dir:?}/$tests/$module"
    cp -r "$repo_root/$tests/$module" "$dir/$tests/"
done
if [ "$with_process_wav" = 1 ]; then
    cp "$repo_root/crates/fxsound-dsp/examples/process_wav.rs" "$dir/crates/fxsound-dsp/examples/"
fi

preset_rs="$dir/crates/fxsound-dsp/src/preset.rs"
one_dsp='
/// Written by scripts/reference-checkout.sh: this commit has one DSP, which it plays either way.
impl MusicLevels {
    #[must_use]
    pub const fn with_windows_dsp(self, _windows: bool) -> Self {
        self
    }
}
'
one_reading='
/// Written by scripts/reference-checkout.sh: this commit reads a preset one way, whichever DSP
/// plays it.
impl MusicLevels {
    #[must_use]
    pub fn ladder(self, count: usize) -> Vec<f32> {
        ladder(count)
    }

    #[must_use]
    pub fn slider_to_value(self, effect: Effect, slider: f32) -> f32 {
        scale::slider_to_value_for(effect, slider)
    }

    #[must_use]
    pub fn value_to_slider(self, effect: Effect, value: f32) -> f32 {
        scale::value_to_slider_for(effect, value)
    }
}
'
if [ -f "$preset_rs" ]; then
    if grep -q 'fn with_windows_dsp' "$preset_rs"; then
        echo "reference-checkout: $ref has fxsound_dsp::preset and the Windows DSP"
    else
        printf '%s' "$one_dsp" >> "$preset_rs"
        echo "reference-checkout: $ref has fxsound_dsp::preset and one DSP; added with_windows_dsp"
    fi
    if grep -q 'pub fn ladder(self' "$preset_rs"; then
        echo "reference-checkout: $ref reads a preset as its DSP does; nothing to add"
    else
        printf '%s' "$one_reading" >> "$preset_rs"
        echo "reference-checkout: $ref reads a preset one way; added the levels' reading"
    fi
else
    python3 - "$dir/crates/fxsound-app/src/app.rs" "$preset_rs" "$ref" <<'PYTHON'
import re
import sys

app_rs, preset_rs, ref = sys.argv[1:4]
source = open(app_rs, encoding="utf-8").read().split("\n")


def item(pattern):
    """A top-level item of app.rs, with the attributes above it, up to its closing brace."""
    for start, line in enumerate(source):
        if re.match(pattern, line):
            first = start
            while first > 0 and source[first - 1].startswith("#["):
                first -= 1
            end = start
            while source[end] != "}":
                end += 1
            return "\n".join(source[first : end + 1])
    return None


def public(text):
    if re.search(r"^struct ", text, flags=re.M):
        # A struct's fields; a function's parameters sit at the same indentation.
        text = re.sub(r"^    (\w+): ", r"    pub \1: ", text, flags=re.M)
    text = re.sub(r"^(const )?fn ", r"pub \1fn ", text, flags=re.M)
    text = re.sub(r"^struct ", "pub struct ", text, flags=re.M)
    text = re.sub(r"^    const fn ", "    pub const fn ", text, flags=re.M)
    text = re.sub(r"^    fn ", "    pub fn ", text, flags=re.M)
    return text.replace("fxsound_dsp::", "crate::")


names = [
    r"fn ladder\(",
    r"fn bands_of\(",
    r"struct MusicControls\b",
    r"fn music_controls\(",
    r"struct MusicLevels\b",
    r"impl MusicLevels\b",
    r"fn write_music_params\(",
]
items = [item(name) for name in names]

header = f"""//! The application's reading of a music preset at {ref}, where it lived in
//! `crates/fxsound-app/src/app.rs`, written here by `scripts/reference-checkout.sh` so that the
//! two-build comparisons render this commit's presets as its application played them. Not part of
//! {ref}.

#![allow(dead_code, unused_imports, clippy::all, clippy::pedantic)]

use fxsound_core::messages::DspParams;
use fxsound_core::{{Effect, EqBand, Preset, Settings, scale}};
"""

if all(items):
    body = "\n\n".join(public(text) for text in items)
    body += """

impl Default for MusicLevels {
    fn default() -> Self {
        Self::of(&Settings::default())
    }
}

pub fn preset_params(preset: &Preset, ladder: &[f32], levels: MusicLevels) -> DspParams {
    let MusicControls {
        effects,
        eq_on,
        eq_bands,
    } = music_controls(preset, ladder);
    let mut params = DspParams::default();
    write_music_params(&mut params, &effects, eq_on, &eq_bands, levels);
    params
}
"""
    note = "copied from its application"
else:
    missing = [name for name, text in zip(names, items) if text is None]
    if len(missing) != len(names):
        sys.exit(f"reference-checkout: {app_rs} has only part of the reading: {missing} missing")
    # Before 0.4.0's controller work the application put a preset's own bands in the window,
    # whatever the band count, and read its effects through the sliders
    # (`App::apply_preset` and `App::sync_params_from_state` of that time).
    body = """
pub fn ladder(count: usize) -> Vec<f32> {
    crate::eq::band_table(count).map_or_else(
        || {
            let mut eq = crate::GraphicEq::new();
            eq.set_num_bands(count);
            eq.center_frequencies().to_vec()
        },
        |(table, _, _)| table.to_vec(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MusicLevels {
    pub filter_q: f32,
    pub master_gain_db: f32,
    pub balance_db: f32,
    pub volume_leveling: f32,
}

impl MusicLevels {
    pub const fn of(settings: &Settings) -> Self {
        Self {
            filter_q: settings.filter_q,
            master_gain_db: settings.master_gain,
            balance_db: settings.balance,
            volume_leveling: settings.volume_leveling,
        }
    }
}

impl Default for MusicLevels {
    fn default() -> Self {
        Self::of(&Settings::default())
    }
}

/// The preset's own bands on any ladder: this commit's application played a preset on the
/// preset's band count.
pub fn preset_params(preset: &Preset, _ladder: &[f32], levels: MusicLevels) -> DspParams {
    let mut params = DspParams::default();
    for effect in Effect::ALL {
        let slider = scale::value_to_slider_for(effect, preset.effect(effect));
        params.set_effect(effect, scale::slider_to_value_for(effect, slider));
    }
    params.eq_on = preset.eq_on;
    params.set_bands(&preset.eq_bands);
    params.filter_q = levels.filter_q;
    params.master_gain_db = levels.master_gain_db;
    params.balance = levels.balance_db;
    params.volume_leveling_db = levels.volume_leveling;
    params
}
"""
    note = "its application's reading of that time: the preset's own band count"

open(preset_rs, "w", encoding="utf-8").write(header + "\n" + body.lstrip("\n"))
print(f"reference-checkout: wrote fxsound_dsp::preset for {ref}, {note}")
PYTHON
    printf '%s' "$one_dsp" >> "$preset_rs"
    printf '%s' "$one_reading" >> "$preset_rs"
    lib_rs="$dir/crates/fxsound-dsp/src/lib.rs"
    printf '\n/// Written by scripts/reference-checkout.sh; not part of this commit.\npub mod preset;\n' >> "$lib_rs"
fi

echo "reference-checkout: $ref ready in $dir"
