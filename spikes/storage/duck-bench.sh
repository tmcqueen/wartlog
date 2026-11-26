#!/usr/bin/env bash
# THROWAWAY: WartLog spike vs DuckDB on Parquet tables.
# Reads FULL (never writes there). Stores go to WORK. Streams DuckDB NDJSON straight into wl-spike, so no JSON hits disk.
#   FULL=~/Projects/wartlog/full TABLES="complaints transactions ride_events" YEARS=2023 ./duck-bench.sh
# YEARS limits ride_events/transactions to files whose names contain one of these strings (empty = all files).
set -euo pipefail
cd "$(dirname "$0")"
FULL=${FULL:?set FULL to the parquet directory}
WORK=${WORK:-$HOME/.cache/wl-duck-bench}
RUNS=${RUNS:-5}
TABLES=${TABLES:-"complaints transactions ride_events"}
YEARS=${YEARS:-}
DUCKDB=${DUCKDB:-$(command -v duckdb || echo "$HOME/.local/bin/duckdb")}
WL=$PWD/target/release/wl-spike
cargo build --release -q
mkdir -p "$WORK"

duck() { "$DUCKDB" -no-agent -noheader -list -c "$1"; }
files_of() { # DuckDB list literal of the table's parquet files, filtered by YEARS
  local t=$1 list="" f
  while IFS= read -r f; do
    if [[ -n $YEARS && $t != complaints ]]; then
      local keep=0 y; for y in $YEARS; do [[ $(basename "$f") == *"$y"* ]] && keep=1; done
      [[ $keep == 1 ]] || continue
    fi
    list+="'$f',"
  done < <(find "$FULL" -name '*.parquet' -path "*$t*" | sort)
  [[ -n $list ]] || { echo "no parquet files for $t" >&2; exit 1; }
  echo "[${list%,}]"
}
median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
now() { date +%s.%N; }
measure() { # prints "median_seconds rows"
  local cmd=$1 t0 t1 rows ts=()
  for _ in $(seq "$RUNS"); do
    t0=$(now); rows=$( (eval "$cmd" || true) | grep -c '^{' || true); t1=$(now)
    ts+=("$(echo "$t1 - $t0" | bc)")
  done
  printf '%s %s\n' "$(printf '%s\n' "${ts[@]}" | median)" "$rows"
}
row() { # label duck_sql wl_args
  local d w
  d=$(measure "duck \"COPY ($2) TO '/dev/stdout' (FORMAT json)\"")
  w=$(measure "$WL query --data $STORE $3")
  printf '| %s | %s s | %s s | %s | %s |\n' "$1" "${d% *}" "${w% *}" "${d#* }" "${w#* }"
  [[ ${d#* } == "${w#* }" ]] || echo "| ROW COUNT MISMATCH above | | | | |"
}
size_of() { du -cb $(echo "$1" | tr -d "[]'" | tr , ' ') | tail -1 | cut -f1; }

for t in $TABLES; do
  F=$(files_of "$t")
  STORE=$WORK/$t
  case $t in
    ride_events) TIME=queue_entered_at; SRC=attraction_id; BODY="";;
    transactions) TIME=transacted_at; SRC=venue_id; BODY="";;
    complaints) TIME=received_at; SRC=channel; BODY=complaint_text;;
    *) echo "unknown table $t"; exit 1;;
  esac
  echo "## $t"
  if [[ ! -d $STORE || -n ${REIMPORT:-} ]]; then
    rm -rf "$STORE"
    t0=$(now)
    duck "COPY (SELECT * FROM read_parquet($F) ORDER BY $TIME) TO '/dev/stdout' (FORMAT json)" \
      | "$WL" import --data "$STORE" --journal "$t" --format ndjson --time-field "$TIME" --source-field "$SRC" \
          ${BODY:+--body-field $BODY} --per-source --block-age-s 1000000000 --term-index > "$WORK/$t.import.txt"
    t1=$(now)
    echo "export+import wall seconds: $(echo "$t1 - $t0" | bc)"
    cat "$WORK/$t.import.txt"
  fi
  "$WL" info --data "$STORE"
  echo "parquet bytes: $(size_of "$F")  wartlog bytes: $(du -sb "$STORE" | cut -f1)"
  echo
  echo "| Query | DuckDB | WartLog | DuckDB rows | WartLog rows |"
  echo "|---|---|---|---|---|"
  ep() { duck "SELECT epoch($1)::BIGINT"; }
  case $t in
    ride_events)
      G=$(duck "SELECT guest_id FROM read_parquet($F) WHERE guest_id IS NOT NULL USING SAMPLE 1 ROWS")
      read -r A DAY < <(duck "SELECT attraction_id, ride_date FROM read_parquet($F) USING SAMPLE 1 ROWS" | tr '|' ' ')
      S=$(ep "TIMESTAMP '$DAY 00:00:00'"); E=$((S + 86399))
      row "one guest's rides (guest_id=$G)" "SELECT * FROM read_parquet($F) WHERE guest_id=$G" "--field guest_id=$G"
      row "one attraction, one day" "SELECT * FROM read_parquet($F) WHERE attraction_id=$A AND $TIME BETWEEN TIMESTAMP '$DAY 00:00:00' AND TIMESTAMP '$DAY 23:59:59'" "--field attraction_id=$A --since @$S --until @$E"
      row "express lane, one day" "SELECT * FROM read_parquet($F) WHERE used_express_lane AND $TIME BETWEEN TIMESTAMP '$DAY 00:00:00' AND TIMESTAMP '$DAY 23:59:59'" "--field used_express_lane=true --since @$S --until @$E"
      row "one attraction, whole table" "SELECT * FROM read_parquet($F) WHERE attraction_id=$A" "--field attraction_id=$A"
      ;;
    transactions)
      H=$(duck "SELECT household_id FROM read_parquet($F) WHERE household_id IS NOT NULL USING SAMPLE 1 ROWS")
      read -r V DAY < <(duck "SELECT venue_id, transacted_at::DATE FROM read_parquet($F) WHERE venue_id IS NOT NULL USING SAMPLE 1 ROWS" | tr '|' ' ')
      P=$(duck "SELECT payment_method FROM read_parquet($F) WHERE payment_method IS NOT NULL GROUP BY 1 ORDER BY count(*) LIMIT 1")
      S=$(ep "TIMESTAMP '$DAY 00:00:00'"); E=$((S + 86399))
      row "one household (household_id=$H)" "SELECT * FROM read_parquet($F) WHERE household_id=$H" "--field household_id=$H"
      row "one venue, one day" "SELECT * FROM read_parquet($F) WHERE venue_id=$V AND $TIME BETWEEN TIMESTAMP '$DAY 00:00:00' AND TIMESTAMP '$DAY 23:59:59'" "--field venue_id=$V --since @$S --until @$E"
      row "rarest payment method ($P), one day" "SELECT * FROM read_parquet($F) WHERE payment_method='$P' AND $TIME BETWEEN TIMESTAMP '$DAY 00:00:00' AND TIMESTAMP '$DAY 23:59:59'" "--field 'payment_method=$P' --since @$S --until @$E"
      row "rarest payment method, whole table" "SELECT * FROM read_parquet($F) WHERE payment_method='$P'" "--field 'payment_method=$P'"
      ;;
    complaints)
      # a word that appears in only a few complaints
      W=$(duck "SELECT w FROM (SELECT DISTINCT complaint_id, unnest(regexp_split_to_array(lower(complaint_text), '[^a-z0-9]+')) AS w FROM read_parquet($F)) WHERE length(w) >= 6 GROUP BY w HAVING count(*) BETWEEN 2 AND 5 ORDER BY hash(w) LIMIT 1")
      C=$(duck "SELECT category FROM read_parquet($F) WHERE category IS NOT NULL USING SAMPLE 1 ROWS")
      row "rare word, whole word ($W)" "SELECT * FROM read_parquet($F) WHERE regexp_matches(lower(complaint_text), '(^|[^a-z0-9])$W([^a-z0-9]|\$)')" "--has $W"
      row "rare word, substring" "SELECT * FROM read_parquet($F) WHERE complaint_text ILIKE '%$W%'" "--contains $W"
      row "one category ($C)" "SELECT * FROM read_parquet($F) WHERE category='$C'" "--field 'category=$C'"
      ;;
  esac
  echo
done
