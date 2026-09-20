#!/usr/bin/env bash
# Step 3 of the blind listening harness: play each genre's three level-matched
# tracks in mpv, take one keystroke per judgement, and write the answers to a CSV
# that can be counted instead of remembered.
#
#   ./vote.sh              run the session over every genre that has a blind file
#   ./vote.sh --genre Jazz one genre (repeatable)
#   ./vote.sh --again      re-vote on genres that already have an answer
#   ./vote.sh --tally      rebuild votes.csv from the raw answers and print the count
#   ./vote.sh --reset      discard the raw answers and start the session over
#
# During playback:
#   a s d   switch instantly between tracks 1, 2 and 3 — same position, same level
#   1 2 3   keep this one; records the vote and moves to the next genre
#   b h t p mark the track you are on as boomy / harsh / thin / pumping
#   u       mark this judgement as unsure
#   z       undo the last thing you recorded for this genre
#   q       leave without voting on this genre

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/common.sh"

SELF="$VOICING_DIR/vote.sh"

# ------------------------------------------------------------- the run callback
# mpv's `run` command calls back into this script. It has no terminal, so any
# complaint goes to a log rather than into the void.
if [ "${1:-}" = "--record" ]; then
    shift
    genre="${1:-}"; kind="${2:-}"; value="${3:--}"
    log="$WORK/record-errors.log"
    {
        [ -f "$KEY_CSV" ] || { echo "$(date -Is) no key at $KEY_CSV" >> "$log"; exit 1; }
        order="$(awk -F, -v g="$genre" 'NR>1 && $1==g {print $2"|"$3"|"$4; exit}' "$KEY_CSV")"
        [ -n "$order" ] || { echo "$(date -Is) no key row for $genre" >> "$log"; exit 1; }
        fp="$(order_fingerprint "$order")"

        [ -f "$RAW_CSV" ] || printf 'recorded_at,preset,kind,value,fingerprint\n' > "$RAW_CSV"

        if [ "$kind" = undo ]; then
            # Drop this genre's most recent row, whatever it was.
            tmp="$(mktemp "$WORK/.raw.XXXXXX")"
            awk -F, -v g="$genre" '
                { rows[NR]=$0; if (NR>1 && $2==g) last=NR }
                END { for (i=1;i<=NR;i++) if (i!=last) print rows[i] }
            ' "$RAW_CSV" > "$tmp" && mv -- "$tmp" "$RAW_CSV"
            exit 0
        fi

        printf '%s,%s,%s,%s,%s\n' "$(date -Is)" "$genre" "$kind" "$value" "$fp" >> "$RAW_CSV"
    } 2>>"$log"
    exit 0
fi

# ------------------------------------------------------------------- unblinding
# Join the raw answers to the key. A row whose fingerprint no longer matches the
# key was cast against a different shuffle — report it, never resolve it.
unblind() {
    [ -f "$KEY_CSV" ] || die "no key at $KEY_CSV — run ./match-and-mux.sh first"
    if [ ! -f "$RAW_CSV" ]; then
        note "no answers recorded yet"
        return 1
    fi
    awk -F, -v out="$VOTES_CSV" '
        FNR==NR {
            if (FNR>1) { t[$1,1]=$2; t[$1,2]=$3; t[$1,3]=$4; ord[$1]=$2"|"$3"|"$4; fp[$1]=$9; known[$1]=1 }
            next
        }
        FNR==1 { next }
        {
            g=$2; k=$3; v=$4; f=$5
            if (!(g in known)) { unknown[g]=1; next }
            if (f != fp[g]) { stale[g]=1; next }
            # Rows are in the order they were recorded and the flags for a
            # judgement always precede its pick, so a pick closes the attempt and
            # clears the accumulator. Re-voting a genre with --again therefore
            # gets its own flags instead of inheriting the earlier attempt.
            if (k=="pick") {
                pick[g]=v; at[g]=$1
                flags[g]=acc[g]; unsure[g]=accu[g]
                acc[g]=""; accu[g]=0
            } else if (k=="flag") {
                if (v=="u") accu[g]=1
                else if (index(acc[g], v)==0) acc[g]=acc[g] v
            }
        }
        END {
            printf "preset,winner,flags,confidence,picked_track,order,voted_at\n" > out
            n=0
            for (g in known) {
                if (!(g in pick)) continue
                w = t[g, pick[g]+0]
                printf "%s,%s,%s,%s,%s,%s,%s\n", g, w, flags[g],
                       unsure[g] ? "unsure" : "sure", pick[g], ord[g], at[g] >> out
                n++
            }
            close(out)
            for (g in stale) printf "stale: %s was voted on against an older shuffle; re-vote it\n", g > "/dev/stderr"
            for (g in unknown) printf "orphan: %s has answers but no key row\n", g > "/dev/stderr"
            printf "%d\n", n
        }
    ' "$KEY_CSV" "$RAW_CSV" > "$WORK/.voted-count"
    # Sort the body so the file is stable between runs (awk iterates hashes).
    if [ -s "$VOTES_CSV" ]; then
        tmp="$(mktemp "$WORK/.votes.XXXXXX")"
        { head -n 1 "$VOTES_CSV"; tail -n +2 "$VOTES_CSV" | sort; } > "$tmp" && mv -- "$tmp" "$VOTES_CSV"
    fi
    return 0
}

tally() {
    unblind || return 1
    local voted total
    voted="$(cat "$WORK/.voted-count" 2>/dev/null || echo 0)"
    total="$(($(wc -l < "$KEY_CSV") - 1))"
    rm -f "$WORK/.voted-count"

    step "verdicts: $voted of $total genres judged"
    printf '\n'
    # column -s, folds adjacent delimiters together, so an unflagged row would
    # shift a column left. Fill the blanks before printing; the CSV keeps them.
    awk -F, -v OFS=, '{for (i=1;i<=NF;i++) if ($i=="") $i="-"; print}' "$VOTES_CSV" \
        | { column -t -s, 2>/dev/null || cat; }
    printf '\n'

    awk -F, 'NR>1 {c[$2]++; if ($4=="unsure") u++; n=split($3,f,""); for(i=1;i<=n;i++) fl[f[i]]++}
        END {
            printf "  new wins: %d   old wins: %d   dry wins: %d   unsure: %d\n",
                   c["new"], c["old"], c["dry"], u+0
            if (length(fl)) {
                printf "  flags:"
                name["b"]="boomy"; name["h"]="harsh"; name["t"]="thin"; name["p"]="pumping"
                for (k in fl) printf " %s=%d", name[k], fl[k]
                printf "\n"
            }
        }' "$VOTES_CSV"

    local losses regressions pumping
    losses="$(awk -F, 'NR>1 && $2=="dry" {printf "%s ", $1}' "$VOTES_CSV")"
    regressions="$(awk -F, 'NR>1 && $2=="old" {printf "%s ", $1}' "$VOTES_CSV")"
    pumping="$(awk -F, 'NR>1 && index($3,"p") {printf "%s ", $1}' "$VOTES_CSV")"
    printf '\n'
    [ -n "$losses" ] && note "  beaten by doing nothing (pull the voicing back toward flat): $losses"
    [ -n "$regressions" ] && note "  the revoicing lost to the shipped preset (regression): $regressions"
    [ -n "$pumping" ] && note "  flagged as pumping — look at Dynamic Boost first: $pumping"
    note ""
    note "  $VOTES_CSV"
    return 0
}

# ------------------------------------------------------------------ the session
RESET=0
AGAIN=0
TALLY_ONLY=0
SELECTED=()

while [ $# -gt 0 ]; do
    case "$1" in
        --again) AGAIN=1 ;;
        --tally) TALLY_ONLY=1 ;;
        --reset) RESET=1 ;;
        --genre) shift; [ $# -gt 0 ] || die "--genre needs a name"; SELECTED+=("$1") ;;
        -h|--help) awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) die "unknown option $1" ;;
    esac
    shift
done

mkdir -p "$WORK" "$SESSION_DIR"

if [ "$RESET" -eq 1 ]; then
    printf 'discard every recorded answer in %s? [y/N] ' "$RAW_CSV"
    read -r reply
    case "$reply" in
        y|Y) rm -f "$RAW_CSV" "$VOTES_CSV"; note "cleared" ;;
        *) note "kept" ;;
    esac
    exit 0
fi

[ "$TALLY_ONLY" -eq 1 ] && { tally; exit $?; }

need_cmd mpv
[ -f "$KEY_CSV" ] || die "no key at $KEY_CSV — run ./render.sh and ./match-and-mux.sh first"
[ ${#SELECTED[@]} -gt 0 ] || mapfile -t SELECTED < <(awk -F, 'NR>1 {print $1}' "$KEY_CSV")
[ ${#SELECTED[@]} -gt 0 ] || die "the key has no genres in it"

# A pick only counts if it was cast against the shuffle the blind file still has.
# Reshuffling a genre therefore puts it back into the session rather than leaving
# it both un-votable and uncounted.
already_voted() {
    local order fp
    [ -f "$RAW_CSV" ] || return 1
    order="$(awk -F, -v g="$1" 'NR>1 && $1==g {print $2"|"$3"|"$4; exit}' "$KEY_CSV")"
    [ -n "$order" ] || return 1
    fp="$(order_fingerprint "$order")"
    awk -F, -v g="$1" -v f="$fp" \
        'NR>1 && $2==g && $3=="pick" && $5==f {found=1} END {exit found?0:1}' "$RAW_CSV"
}

write_conf() {
    local genre="$1" conf="$2"
    cat > "$conf" <<CONF
# Generated by vote.sh for "$genre". Regenerated every session; do not edit.
a set aid 1 ; show-text "track 1" 700
s set aid 2 ; show-text "track 2" 700
d set aid 3 ; show-text "track 3" 700
1 run "$SELF" "--record" "$genre" "pick" "1" ; show-text "kept track 1" 800 ; quit
2 run "$SELF" "--record" "$genre" "pick" "2" ; show-text "kept track 2" 800 ; quit
3 run "$SELF" "--record" "$genre" "pick" "3" ; show-text "kept track 3" 800 ; quit
b run "$SELF" "--record" "$genre" "flag" "b" ; show-text "boomy" 700
h run "$SELF" "--record" "$genre" "flag" "h" ; show-text "harsh" 700
t run "$SELF" "--record" "$genre" "flag" "t" ; show-text "thin" 700
p run "$SELF" "--record" "$genre" "flag" "p" ; show-text "pumping" 700
u run "$SELF" "--record" "$genre" "flag" "u" ; show-text "unsure" 700
z run "$SELF" "--record" "$genre" "undo" "-" ; show-text "undid the last mark" 800
CONF
}

# A moment of grace: mpv spawns the callback and then quits, so the row can land
# just after mpv is gone.
wait_for_vote() {
    local genre="$1" i
    for i in 1 2 3 4 5 6; do
        already_voted "$genre" && return 0
        sleep 0.5
    done
    return 1
}

cat <<'INTRO'

  Blind listening — the revoiced genre presets
  --------------------------------------------
  One file per genre. Three audio tracks: the untouched excerpt, the shipped
  preset and the revoiced one, all at the same integrated loudness, in an order
  you are not told. The same seconds loop until you decide.

    a s d   switch tracks instantly
    1 2 3   keep this one, and move on
    b h t p boomy / harsh / thin / pumping
    u       not sure about this one
    z       undo the last mark
    q       skip this genre

  The question is not "does Jazz sound like jazz". It is: of these three, which
  do you want to keep listening to — and if the untouched one is the best of the
  three, say so. That answer is the one that matters most.

INTRO

# Present the genres in a random order too, so fatigue does not always land on
# the same half of the list.
mapfile -t ORDERED < <(printf '%s\n' "${SELECTED[@]}" | shuf)

done_count=0
skipped=()
for g in "${ORDERED[@]}"; do
    blind="$BLIND_DIR/$g.mkv"
    if [ ! -s "$blind" ]; then
        note "no blind file for $g — skipping"
        continue
    fi
    if [ "$AGAIN" -eq 0 ] && already_voted "$g"; then
        note "$g: already judged (--again to redo it)"
        continue
    fi

    conf="$SESSION_DIR/$g.conf"
    write_conf "$g" "$conf"

    step "$g   — a/s/d to switch, 1/2/3 to keep one"
    # shellcheck disable=SC2086
    mpv --input-conf="$conf" --loop-file=inf --no-video --term-osd-bar \
        --term-status-msg='  track ${=aid}/3   ${time-pos}' \
        ${FXSV_MPV_ARGS:-} -- "$blind" || true

    if wait_for_vote "$g"; then
        done_count=$((done_count + 1))
    else
        skipped+=("$g")
        note "no answer recorded for $g"
    fi
done

step "judged $done_count genre(s) this session"
[ ${#skipped[@]} -gt 0 ] && note "no answer for: ${skipped[*]}  (run ./vote.sh again to pick them up)"

tally || note "nothing to tally yet"
