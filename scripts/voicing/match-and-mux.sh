#!/usr/bin/env bash
# Step 2 of the blind listening harness: level-match the three renders of each
# genre and mux them into one file as three switchable audio tracks, in an order
# shuffled per genre.
#
#   ./match-and-mux.sh              match and mux everything render.sh produced
#   ./match-and-mux.sh --genre Jazz one genre (repeatable)
#   ./match-and-mux.sh --reshuffle  draw fresh track orders (invalidates pending votes)
#   ./match-and-mux.sh --no-verify  skip the post-mux loudness check
#
# Why the matching matters more than it looks: measured on one identical input,
# dry sits at -26.3 LUFS and the presets land between -24.4 and -18.9, because
# Dynamic Boost is never bypassed. A sighted, unmatched comparison therefore grades
# Dynamic Boost's auto-gain and nothing else — the loudest preset wins every time.
#
# Matching is by attenuation only. Everything is pulled down to the quietest of the
# three (in practice the dry anchor), never pushed up, so nothing can clip.

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/common.sh"

RESHUFFLE=0
VERIFY=1
SELECTED=()

while [ $# -gt 0 ]; do
    case "$1" in
        --reshuffle) RESHUFFLE=1 ;;
        --no-verify) VERIFY=0 ;;
        --genre) shift; [ $# -gt 0 ] || die "--genre needs a name"; SELECTED+=("$1") ;;
        -h|--help) awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) die "unknown option $1" ;;
    esac
    shift
done

need_cmd ffmpeg ffprobe shuf
[ ${#SELECTED[@]} -gt 0 ] || SELECTED=("${GENRES[@]}")
mkdir -p "$BLIND_DIR"

[ -f "$KEY_CSV" ] || printf 'preset,track1,track2,track3,lufs_dry,lufs_old,lufs_new,target_lufs,fingerprint\n' > "$KEY_CSV"

existing_order() {
    awk -F, -v g="$1" 'NR>1 && $1==g {print $2"|"$3"|"$4; exit}' "$KEY_CSV"
}

drop_key_row() {
    local tmp
    tmp="$(mktemp "$WORK/.key.XXXXXX")"
    awk -F, -v g="$1" 'NR==1 || $1!=g' "$KEY_CSV" > "$tmp" && mv -- "$tmp" "$KEY_CSV"
}

muxed=0
skipped=0

for g in "${SELECTED[@]}"; do
    dry="$RENDER_DIR/$g.dry.wav"
    old="$RENDER_DIR/$g.old.wav"
    new="$RENDER_DIR/$g.new.wav"
    if [ ! -s "$dry" ] || [ ! -s "$old" ] || [ ! -s "$new" ]; then
        skipped=$((skipped + 1))
        continue
    fi

    step "$g"

    l_dry="$(lufs "$dry")"
    l_old="$(lufs "$old")"
    l_new="$(lufs "$new")"
    target="$(awk -v a="$l_dry" -v b="$l_old" -v c="$l_new" \
        'BEGIN{m=a; if(b<m)m=b; if(c<m)m=c; printf "%.2f", m}')"
    note "  measured: dry $l_dry  old $l_old  new $l_new  LUFS -> matching all to $target"

    declare -A MEASURED=([dry]="$l_dry" [old]="$l_old" [new]="$l_new")
    declare -A ADJ=()
    for v in "${VARIANTS[@]}"; do
        # target - I, clamped at zero: attenuate only, so nothing can clip.
        ADJ[$v]="$(awk -v t="$target" -v i="${MEASURED[$v]}" \
            'BEGIN{d=t-i; if(d>0)d=0; printf "%.2f", d}')"
    done
    note "  gain:     dry ${ADJ[dry]} dB  old ${ADJ[old]} dB  new ${ADJ[new]} dB"

    # Track order. Reuse whatever the key already says for this genre, so that
    # re-rendering one preset does not silently re-blind a session in progress.
    order="$(existing_order "$g")"
    if [ -z "$order" ] || [ "$RESHUFFLE" -eq 1 ]; then
        order="$(shuf -e "${VARIANTS[@]}" | paste -sd'|' -)"
        [ "$RESHUFFLE" -eq 1 ] && note "  reshuffled — any vote already recorded for $g will no longer resolve"
    fi
    IFS='|' read -r p1 p2 p3 <<< "$order"
    positions=("$p1" "$p2" "$p3")

    declare -A INPUT_INDEX=([dry]=0 [old]=1 [new]=2)
    fc=""
    maps=()
    meta=()
    for i in 0 1 2; do
        v="${positions[$i]}"
        idx="${INPUT_INDEX[$v]}"
        adj="${ADJ[$v]}"
        chain=""
        # 0.75 ms of engine latency puts dry a hair ahead of the other two. It is
        # inaudible when only one track plays at a time, so by default dry passes
        # through untouched; FXSV_ALIGN_DRY=1 pads it into sample alignment.
        if [ "$v" = dry ] && [ "$ALIGN_DRY" = 1 ]; then
            ms="$(awk -v f="$ENGINE_LATENCY_FRAMES" 'BEGIN{printf "%.4f", f*1000/48000}')"
            chain="adelay=delays=$ms|$ms:all=1"
        fi
        if awk -v a="$adj" 'BEGIN{exit (a<-0.05)?0:1}'; then
            chain="${chain:+$chain,}volume=${adj}dB"
        fi
        if [ -n "$chain" ]; then
            fc="${fc}[${idx}:a]${chain}[p${i}];"
            maps+=(-map "[p$i]")
        else
            # No filter at all when no adjustment is needed, so the anchor reaches
            # the muxer bit-exact.
            maps+=(-map "${idx}:a:0")
        fi
        meta+=(-metadata:s:a:$i "title=$((i + 1))" -metadata:s:a:$i "language=und")
    done

    out="$BLIND_DIR/$g.mkv"
    ffmpeg_args=(-v error -y -i "$dry" -i "$old" -i "$new")
    [ -n "$fc" ] && ffmpeg_args+=(-filter_complex "${fc%;}")
    ffmpeg_args+=("${maps[@]}" -map_metadata -1 -map_chapters -1
        -c:a flac -sample_fmt s16 "${meta[@]}" "$out")
    ffmpeg "${ffmpeg_args[@]}" || die "mux failed for $g"

    streams="$(ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 "$out" | wc -l)"
    [ "$streams" -eq 3 ] || die "$out came out with $streams audio tracks, expected 3"

    if [ "$VERIFY" -eq 1 ]; then
        spread=""
        for i in 0 1 2; do
            tmp="$WORK/.verify.wav"
            ffmpeg -v error -y -i "$out" -map "0:a:$i" -c:a pcm_s16le "$tmp" || die "could not extract track $((i+1)) of $g"
            m="$(lufs "$tmp")"
            spread="${spread:+$spread }$m"
            awk -v m="$m" -v t="$target" 'BEGIN{d=m-t; if(d<0)d=-d; exit (d<=0.3)?0:1}' \
                || die "track $((i+1)) of $g measures $m LUFS, target was $target — matching did not take"
            rm -f "$tmp"
        done
        note "  muxed:    tracks 1/2/3 at $spread LUFS (target $target)"
    fi

    fp="$(order_fingerprint "$order")"
    drop_key_row "$g"
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$g" "$p1" "$p2" "$p3" "$l_dry" "$l_old" "$l_new" "$target" "$fp" >> "$KEY_CSV"
    note "  $out  (order hidden in $KEY_CSV)"
    muxed=$((muxed + 1))
done

step "muxed $muxed genre(s), skipped $skipped with no renders"
[ "$muxed" -gt 0 ] || die "nothing was muxed — run ./render.sh first"
note "the key is $KEY_CSV. Do not read it before listening."
note "next: ./vote.sh"
