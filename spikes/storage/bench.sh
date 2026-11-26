#!/usr/bin/env bash
# THROWAWAY benchmark for the WartLog storage spike.
# Compares the spike against journalctl on your own user journal.
#
#   ./bench.sh            # user journal, 10x scale test
#   SCALE=1 ./bench.sh    # skip the scale test
#   COLD=1 ./bench.sh     # also run cold-cache timings (needs passwordless sudo)
#   KEEP=1 ./bench.sh     # keep the work directory afterwards
#
# Your logs are copied to $WORK (outside the repo) and deleted at the end unless KEEP=1.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/wl-spike-bench}"
SCALE="${SCALE:-10}"
RUNS="${RUNS:-5}"
# which journal to read; override for testing, e.g. JARGS="-D /path/to/journal/dir"
JARGS="${JARGS:---user}"
B="$HERE/target/release/wl-spike"
REPORT="$WORK/report.md"

command -v cargo >/dev/null || { echo "cargo not found: install Rust from https://rustup.rs" >&2; exit 1; }
command -v journalctl >/dev/null || { echo "journalctl not found" >&2; exit 1; }

rm -rf "$WORK"; mkdir -p "$WORK"; chmod 700 "$WORK"
cleanup() { [[ "${KEEP:-0}" == 1 ]] || rm -rf "$WORK/data" "$WORK/scaled" "$WORK/damaged" "$WORK/user.json"; }
trap cleanup EXIT

echo "== building spike (release)" >&2
(cd "$HERE" && cargo build --release -q)

now() { date +%s%N; }
secs() { awk "BEGIN { printf \"%.3f\", ($2 - $1) / 1e9 }"; }

# median wall time of RUNS runs of a pipeline that prints rows; sets T (seconds) and N (rows)
measure() {
  local times=() s e
  for ((i = 0; i < RUNS; i++)); do
    s=$(now); N=$( (eval "$1" || true) | wc -l); e=$(now)
    times+=("$(secs "$s" "$e")")
  done
  T=$(printf '%s\n' "${times[@]}" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
}

cold() {
  if [[ "${COLD:-0}" == 1 ]]; then sync; echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null; fi
}

{
echo "# WartLog storage spike: benchmark report"
echo
echo "- Host: $(uname -sr), $(nproc) CPUs, $(awk '/MemTotal/ {printf "%.0f GiB RAM", $2/1048576}' /proc/meminfo)"
echo "- journalctl: $(journalctl --version | head -1)"
echo "- Date: $(date -Iseconds)"
echo "- Timings are medians of $RUNS warm-cache runs unless marked. Both tools print JSON lines into \`wc -l\`."
echo "- Note: the spike scans blocks in parallel on all CPUs; journalctl is single-threaded."
echo
} > "$REPORT"

echo "== exporting user journal" >&2
s=$(now); journalctl $JARGS -o json --no-pager > "$WORK/user.json"; e=$(now)
LINES=$(wc -l < "$WORK/user.json"); JSON_BYTES=$(stat -c %s "$WORK/user.json")
DISK=$(journalctl $JARGS --disk-usage 2>/dev/null | grep -oE '[0-9.]+[KMGT]?B?' | head -1 || echo "?")

echo "== importing" >&2
IMPORT=$("$B" import --data "$WORK/data" --input "$WORK/user.json")
INFO=$("$B" info --data "$WORK/data")
"$B" pick --data "$WORK/data" > "$WORK/pick.sh"
# shellcheck disable=SC1091
source "$WORK/pick.sh"
STORE_BYTES=$(echo "$IMPORT" | grep -oE 'store_bytes=[0-9]+' | cut -d= -f2)

{
echo "## Data"
echo
echo "| | |"
echo "|---|---|"
echo "| Entries in user journal | $LINES |"
echo "| journalctl JSON export | $(numfmt --to=iec "$JSON_BYTES") in $(secs "$s" "$e") s |"
echo "| journald on-disk size (journalctl --disk-usage, includes archived files) | $DISK |"
echo "| WartLog store size | $(numfmt --to=iec "$STORE_BYTES") |"
echo
echo "Import: \`$IMPORT\`"
echo
echo "Store: \`$INFO\`"
echo
echo "## Test event picked from your journal"
echo
echo "- Rare term: \`$TERM\` (appears in $TERM_RECORDS entries). Example message: \`$EXAMPLE_MESSAGE\`"
echo "- Unit: \`$UNIT\` ($UNIT_RECORDS entries)"
echo "- Time window: $SINCE to $UNTIL (middle 10% of entries)"
echo
echo "## Queries (warm cache)"
echo
echo "| Query | journalctl | rows | WartLog | rows | speedup |"
echo "|---|---|---|---|---|---|"
} >> "$REPORT"

row() { # label, journalctl cmd, wl cmd
  measure "$2"; local jt=$T jn=$N
  measure "$3"; local wt=$T wn=$N
  local sp; sp=$(awk "BEGIN { if ($wt > 0) printf \"%.1fx\", $jt / $wt; else print \"-\" }")
  echo "| $1 | ${jt}s | $jn | ${wt}s | $wn | $sp |" >> "$REPORT"
}

J="journalctl $JARGS -q -o json --no-pager"
W="$B query --data $WORK/data"
echo "== running query comparisons" >&2
row "Rare term, whole journal (\`-g\` vs \`has\`)"        "$J -g '$TERM'"  "$W --has '$TERM'"
row "Rare term, whole journal (\`-g\` vs \`contains\`)"   "$J -g '$TERM'"  "$W --contains '$TERM'"
row "Rare term in time window"                            "$J -g '$TERM' --since $SINCE --until $UNTIL" "$W --has '$TERM' --since $SINCE --until $UNTIL"
row "Unit + time window (field match)"                    "$J _SYSTEMD_USER_UNIT='$UNIT' --since $SINCE --until $UNTIL" "$W --field _SYSTEMD_USER_UNIT='$UNIT' --since $SINCE --until $UNTIL"
row "Unit, whole journal (field match)"                   "$J _SYSTEMD_USER_UNIT='$UNIT'" "$W --field _SYSTEMD_USER_UNIT='$UNIT'"
row "Errors and worse (\`-p 3\`)"                         "$J -p 3" "$W --priority 3"
row "Everything in time window"                           "$J --since $SINCE --until $UNTIL" "$W --since $SINCE --until $UNTIL"

if [[ "${COLD:-0}" == 1 ]] && sudo -n true 2>/dev/null; then
  {
  echo
  echo "## Cold cache (single runs after dropping the page cache)"
  echo
  echo "| Query | journalctl | WartLog |"
  echo "|---|---|---|"
  } >> "$REPORT"
  for q in "-g '$TERM'|--has '$TERM'" "_SYSTEMD_USER_UNIT='$UNIT' --since $SINCE --until $UNTIL|--field _SYSTEMD_USER_UNIT='$UNIT' --since $SINCE --until $UNTIL"; do
    jq="${q%%|*}"; wq="${q##*|}"
    cold; s=$(now); eval "$J $jq" >/dev/null || true; e=$(now); jt=$(secs "$s" "$e")
    cold; s=$(now); eval "$W $wq" >/dev/null; e=$(now); wt=$(secs "$s" "$e")
    echo "| \`$jq\` | ${jt}s | ${wt}s |" >> "$REPORT"
  done
fi

echo "== index tests" >&2
{
echo
echo "## Indexes"
echo
echo "Execution modes for the rare-term query. \`index\` reads the .widx files; \`summaries\` ignores them and reads block summaries from the segments; \`full\` decodes every block with no pruning."
echo
echo '```'
for m in index summaries full; do "$B" query --data "$WORK/data" --mode $m --has "$TERM" --count --stats 2>&1 >/dev/null; done
for m in index summaries full; do "$B" query --data "$WORK/data" --mode $m --field _SYSTEMD_USER_UNIT="$UNIT" --since "$SINCE" --until "$UNTIL" --count --stats 2>&1 >/dev/null; done
echo '```'
echo
mkdir -p "$WORK/orig-idx"; cp "$WORK"/data/segments/*.widx "$WORK/orig-idx/"
echo "- Verify (rebuild in memory, compare to disk): \`$("$B" index verify --data "$WORK/data")\`"
echo "- Drop all indexes: \`$("$B" index drop --data "$WORK/data")\`"
echo "- Query with indexes gone (falls back to segment scan): \`$("$B" query --data "$WORK/data" --has "$TERM" --count --stats 2>&1 >/dev/null)\`"
echo "- Rebuild from segments: \`$("$B" index rebuild --data "$WORK/data")\`"
same=yes; for f in "$WORK"/orig-idx/*.widx; do cmp -s "$f" "$WORK/data/segments/$(basename "$f")" || same=no; done
echo "- Rebuilt indexes byte-identical to originals: **$same**"
echo
echo "## Damage test (on a copy)"
echo
rm -rf "$WORK/damaged"; cp -r "$WORK/data" "$WORK/damaged"
TOTAL=$("$B" query --data "$WORK/data" --mode full --count)
echo '```'
"$B" corrupt --data "$WORK/damaged" --hits 3 | sed "s|$WORK/||"
"$B" index rebuild --data "$WORK/damaged" 2>&1 | sed "s|$WORK/||"
echo "records before damage: $TOTAL"
echo "records readable after damage + rebuild: $("$B" query --data "$WORK/damaged" --count)"
echo '```'
} >> "$REPORT"

if [[ "$SCALE" -gt 1 ]]; then
  echo "== scale test (${SCALE}x)" >&2
  SIMPORT=$("$B" import --data "$WORK/scaled" --input "$WORK/user.json" --repeat "$SCALE")
  {
  echo
  echo "## Scale test: your journal replayed ${SCALE}x with shifted timestamps (WartLog only)"
  echo
  echo "Import: \`$SIMPORT\`"
  echo
  echo "| Query | WartLog | rows |"
  echo "|---|---|---|"
  } >> "$REPORT"
  WS="$B query --data $WORK/scaled"
  for q in "--has '$TERM'" "--contains '$TERM'" "--field _SYSTEMD_USER_UNIT='$UNIT' --since $SINCE --until $UNTIL" "--field _SYSTEMD_USER_UNIT='$UNIT'" "--priority 3"; do
    measure "$WS $q"; echo "| \`$q\` | ${T}s | $N |" >> "$REPORT"
  done
fi

echo >&2
cat "$REPORT"
