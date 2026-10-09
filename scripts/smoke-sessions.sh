#!/usr/bin/env bash
# End-to-end check: a real `app().run()` keeps sessions in horton. A
# session survives a clean restart and a `kill -9` (sync = true: a reply
# means the write is on disk). Needs `cargo build --example visits` first.
set -euo pipefail
cd "$(dirname "$0")/.."
bin=target/debug/examples/visits
port=3917
jar="$(mktemp)"
rm -f target/visits.horton

start() {
  AUTUMN_SERVER__PORT=$port "$bin" > target/smoke.log 2>&1 &
  pid=$!
  for _ in $(seq 1 60); do
    curl -s -o /dev/null "localhost:$port/actuator/health" && return 0
    sleep 0.5
  done
  echo "the app did not start"; cat target/smoke.log; exit 1
}
stop() {
  kill -INT "$pid"
  wait "$pid" || true
}
crash() {
  kill -9 "$pid"
  wait "$pid" || true
}
check() {
  local got
  got="$(curl -s -c "$jar" -b "$jar" "localhost:$port/me")"
  [ "$got" = "$1" ] || { echo "/me gave '$got', want '$1'"; cat target/smoke.log; exit 1; }
}

start
check 1
check 2
curl -s "localhost:$port/actuator/health" | grep -q '"horton":{"status":"UP"'
stop
start
check 3
stop
grep -q "shutdown: all phases completed cleanly" target/smoke.log
start
check 4
crash
start
check 5
stop
echo "sessions survive a restart and a kill -9"
