#!/usr/bin/env bash
# Hold the output lane to another build of itself, bit for bit: every shipped preset, as the
# application plays it, on 10, 20 and 31 bands, over preset_drift's material, in blocks of 480,
# 512, 1024 and 2048 frames — from a cold start, and again with another preset, the band count
# and an effect switched while it plays, each compared once its glide is over
# (crates/fxsound-dsp/tests/windows_parity_bitexact.rs has the cases).
#
#   scripts/windows-parity-bitexact.sh v0.4.0             # A3: Off plays as 0.4.0
#   scripts/windows-parity-bitexact.sh --compat=windows --report 3596e99
#                                                         # A1: what differs at Interface and sound
#                                                         # from the engine before the 0.4.0
#                                                         # audit's fixes
#
# --compat=windows has the working tree play the Windows build's DSP, as «Like FxSound for
# Windows» = Interface and sound does (FXSOUND_BITEXACT_COMPAT); the default is linux, Off's.
#
# 3596e99 is the reference for "Interface and sound": its crates/fxsound-dsp and its preset
# parser are 0f05ba5's, the last engine before the audit's fixes, byte for byte, and its
# application is the one that played a preset on the user's band count as Windows does, by
# position — which 0f05ba5's own application did not do yet.
#
# The reference is checked out with scripts/reference-checkout.sh into --dir (default
# target/reference/<commit>) and built there, with this checkout's toolchain, in release; then
# this checkout's copy of the test runs the reference's copy and compares. It fails when a case
# differs by more than FXSOUND_BITEXACT_LIMIT_DB (−120 dBFS); with --report it prints what differs
# and passes. Every case's result goes to target/windows-parity-bitexact.tsv. The test file lists
# the variables that narrow the run.
#
# usage: scripts/windows-parity-bitexact.sh [--report] [--compat=linux|windows] [--dir DIRECTORY]
#        <commit>

set -euo pipefail

die() {
    echo "windows-parity-bitexact: $*" >&2
    exit 1
}

report=0
dir=""
ref=""
compat="${FXSOUND_BITEXACT_COMPAT:-linux}"
while [ $# -gt 0 ]; do
    case "$1" in
        --report) report=1 ;;
        --compat=linux | --compat=windows) compat="${1#--compat=}" ;;
        --compat=*) die "--compat takes linux or windows" ;;
        --dir)
            shift
            [ $# -gt 0 ] || die "--dir needs a directory"
            dir="$1"
            ;;
        -h | --help)
            sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        -*) die "unknown option $1" ;;
        *)
            [ -z "$ref" ] || die "one commit at a time"
            ref="$1"
            ;;
    esac
    shift
done
[ -n "$ref" ] || die "usage: scripts/windows-parity-bitexact.sh [--report] [--compat=linux|windows] [--dir DIRECTORY] <commit>"

repo_root="$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)"
[ -n "$dir" ] || dir="$repo_root/target/reference/$ref"
"$repo_root/scripts/reference-checkout.sh" "$ref" "$dir"
dir="$(cd "$dir" && pwd)"

# One toolchain for both builds: nothing promises that two versions of rustc round alike.
toolchain="$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$repo_root/rust-toolchain.toml")"
[ -n "$toolchain" ] || die "no channel in rust-toolchain.toml"
export RUSTUP_TOOLCHAIN="$toolchain"

# The test binary cargo builds in <tree> into <target directory>.
build() {
    local tree="$1" target="$2" executable
    executable="$(
        cd "$tree"
        if [ -n "${SCCACHE_BASEDIRS:-}" ]; then export SCCACHE_BASEDIRS="$tree"; fi
        CARGO_TARGET_DIR="$target" cargo test --release --locked -p fxsound-dsp \
            --test windows_parity_bitexact --no-run --message-format=json-render-diagnostics \
            | grep -o '"executable":"[^"]*windows_parity_bitexact[^"]*"' \
            | tail -n 1 | sed 's/^"executable":"\(.*\)"$/\1/'
    )" || die "could not build the harness in $tree"
    [ -x "$executable" ] || die "no harness binary built in $tree"
    echo "$executable"
}

echo "windows-parity-bitexact: building $ref in $dir" >&2
reference="$(build "$dir" "$dir/target")"
echo "windows-parity-bitexact: building the working tree" >&2
ours="$(build "$repo_root" "${CARGO_TARGET_DIR:-$repo_root/target}")"

if [ "$report" = 1 ]; then
    export FXSOUND_BITEXACT_EXPECT=report
fi
export FXSOUND_BITEXACT_COMPAT="$compat"
cd "$repo_root"
FXSOUND_BITEXACT_REFERENCE="$reference" "$ours" \
    every_shipped_preset_renders_as_the_reference_build_renders_it --exact --ignored --nocapture
