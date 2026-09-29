#!/usr/bin/env bash
# Rebuild the golden vectors the Windows DSP of «Like FxSound for Windows» is held to (A2): the
# C of FxSound LLC's Windows build, compiled here, prints what the unit tests compare against.
#
#   scripts/windows-vectors/build.sh <checkout of https://github.com/fxsound2/fxsound-app>
#
# parametric_below_20hz.c compiles dsp/ptutil/Filt/FiltCalcBiqd.cpp itself; ambience.c and
# gain_stage.c are statement-for-statement transcriptions (their headers say of what). All three
# are compiled as C with realtype = float, into a scratch directory, and run; nothing is written to
# the tree.
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 <fxsound-app checkout>" >&2; exit 2; }
app="$(cd "$1" && pwd)"
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d "${TMPDIR:-$HOME}/fxsound-windows-vectors.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# The Windows headers pull in <crtdbg.h>, <windows.h> and, off Windows, <android/log.h>; none of
# what the design uses comes from them.
mkdir -p "$work/stub/android"
touch "$work/stub/crtdbg.h" "$work/stub/windows.h" "$work/stub/android/log.h"

gcc -x c -O0 -w -D__ANDROID__ -I "$work/stub" -I "$app/dsp/ptutil/include" \
    -I "$app/dsp/ptutil/Filt" "$here/parametric_below_20hz.c" -o "$work/parametric" -lm
gcc -x c -O0 "$here/ambience.c" -o "$work/ambience" -lm
gcc -x c -O0 "$here/gain_stage.c" -o "$work/gain_stage" -lm

echo "// filtCalcParametric below 20 Hz, 48 kHz"
"$work/parametric"
echo "// Ambience, stored 0..127"
"$work/ambience"
echo "// The gain stage, -6 dB master gain, +10 dB balance, (channels, [(input, output) bits])"
"$work/gain_stage"
