#!/usr/bin/env bash
# End-to-end HTTP + SSE performance probe for the Parable harness.
#
# Usage: tests/perf-http.sh <base-url> <token>
# Prints `PERF_HTTP ...` / `PERF_SSE ...` lines for the report.
set -uo pipefail

BASE="${1:-http://127.0.0.1:65113}"
TOKEN="${2:-smoketest123}"
AUTH="x-parable-token: $TOKEN"
OUT="$(mktemp)"

req() { # method path [body]
  if [ $# -ge 3 ]; then
    curl -s -o /dev/null -w '%{http_code} %{time_total}' -X "$1" -H "$AUTH" \
      -H 'Content-Type: application/json' --data-binary "$3" "$BASE$2"
  else
    curl -s -o /dev/null -w '%{http_code} %{time_total}' -X "$1" -H "$AUTH" "$BASE$2"
  fi
}

# ---- 1. single-request latency per endpoint -------------------------------
for ep in "/api/status" "/api/tools" "/api/models" "/api/sessions" "/api/agents" "/api/plugins" "/api/schedule" "/api/pending"; do
  # warm up
  req GET "$ep" >/dev/null 2>&1
  N=40
  t0=$(date +%s%N)
  for _ in $(seq 1 $N); do req GET "$ep" >/dev/null 2>&1; done
  t1=$(date +%s%N)
  name=$(echo "$ep" | tr -c 'a-zA-Z0-9' '_')
  awk -v n="$N" -v t="$((t1-t0))" -v e="$name" \
    'BEGIN{printf "PERF_HTTP %s\treq=%d\ttotal_ms=%.3f\tper_req_us=%.1f\n", e, n, t/1e6, t/1e3/n}'
done

# ---- 2. concurrency: N parallel requests ----------------------------------
for P in 1 8 32 64; do
  N=200
  t0=$(date +%s%N)
  for _ in $(seq 1 $N); do
    curl -s -o /dev/null -H "$AUTH" "$BASE/api/status" &
  done
  wait
  t1=$(date +%s%N)
  awk -v p="$P" -v n="$N" -v t="$((t1-t0))" \
    'BEGIN{printf "PERF_HTTP concurrent\tparallel=%d\treq=%d\ttotal_ms=%.3f\trps=%.0f\tper_req_us=%.1f\n", p, n, t/1e6, n/(t/1e9), t/1e3/n}'
done

# ---- 3. SSE: time to first event + total events ---------------------------
SSE_OUT="$(mktemp)"
( curl -s -N -m 6 -H "$AUTH" "$BASE/api/events" > "$SSE_OUT" ) &
SSE_PID=$!
sleep 1.2
kill $SSE_PID 2>/dev/null
wait $SSE_PID 2>/dev/null
lines=$(wc -l < "$SSE_OUT")
bytes=$(wc -c < "$SSE_OUT")
awk -v l="$lines" -v b="$bytes" \
  'BEGIN{printf "PERF_SSE idle_stream\tlines=%d\tbytes=%d\n", l, b}'
rm -f "$SSE_OUT"

# ---- 4. message search (the new endpoint) ---------------------------------
for q in "原神" "curl" "zzz_no_such_token"; do
  N=20
  t0=$(date +%s%N)
  for _ in $(seq 1 $N); do
    curl -s -o /dev/null -H "$AUTH" "$BASE/api/search/messages?q=$(printf %s "$q" | jq -sRr @uri 2>/dev/null || echo "$q")"
  done
  t1=$(date +%s%N)
  awk -v q="$q" -v n="$N" -v t="$((t1-t0))" \
    'BEGIN{printf "PERF_HTTP search\tq=%s\treq=%d\ttotal_ms=%.3f\tper_req_us=%.1f\n", q, n, t/1e6, t/1e3/n}'
done

rm -f "$OUT"
