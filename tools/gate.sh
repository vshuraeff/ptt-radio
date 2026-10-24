#!/bin/bash
# full acceptance gate: fmt, clippy, debug tests, release lib, the real-time scenario suite twice, aec_gate, lua checks.
# every step runs even after a failure; the exit status is non-zero when any step failed.
# usage: tools/gate.sh [log dir]
# env: PTT_REALTIME_LOCK  lock file shared by everything that measures real-time audio on this machine
#                         (default: ptt-realtime.lock in the repository's git common dir, shared by all worktrees)
#      PTT_REALTIME_WAIT  seconds to wait for that lock per real-time step (default 3600)
set -uo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
companion=$root/companion
target=${CARGO_TARGET_DIR:-$companion/target}
log=${1:-$target/gate/$(date +%Y%m%d-%H%M%S)}
lock=${PTT_REALTIME_LOCK:-$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)/ptt-realtime.lock}
wait=${PTT_REALTIME_WAIT:-3600}
for tool in cargo flock lua luac; do
  command -v "$tool" > /dev/null || { echo "gate: $tool not found" >&2; exit 2; }
done
mkdir -p "$log" || exit 2
summary=$log/summary.txt
: > "$summary"
failed=0
gate_start=$(date +%s)

note() { echo "$*" | tee -a "$summary"; }

# step <name> <command...>: runs the command from the companion dir, output to <log>/<name>.log
step() {
  local name=$1 start rc
  shift
  start=$(date +%s)
  (cd "$companion" && "$@") > "$log/$name.log" 2>&1
  rc=$?
  [ "$rc" -eq 0 ] || failed=1
  note "$(printf '%-28s rc=%-3s %5ss' "$name" "$rc" "$(($(date +%s) - start))")"
}

# realtime <name> <command...>: as step, holding the shared real-time lock; the lock wait is reported apart
realtime() {
  local name=$1 start
  shift
  start=$(date +%s)
  exec 9> "$lock" || { failed=1; note "$name: cannot open lock $lock"; return; }
  if ! flock -w "$wait" 9; then
    exec 9>&-
    failed=1
    note "$(printf '%-28s rc=%-3s lock wait over %ss' "$name" lock "$wait")"
    return
  fi
  note "$(printf '%-28s lock wait %ss' "$name" "$(($(date +%s) - start))")"
  step "$name" "$@"
  exec 9>&-
}

note "gate $(git -C "$root" rev-parse HEAD) logs $log lock $lock"
step fmt cargo fmt --check
step clippy cargo clippy --locked --all-targets --release -- -D warnings

step debug-lib cargo test --locked --lib --bins
for t in bridge_repr connectivity crypto_sessions game_probe multiparty_lab netlab receive_order relay_hub transport_contract; do
  step "debug-$t" cargo test --locked --test "$t" -- --test-threads 1
done

step release-lib cargo test --locked --release --lib
# two full runs of the real-time suite expose flaky cases; a failure in either fails the gate
realtime scenarios-1 cargo test --locked --release --test scenarios -- --nocapture --test-threads 1
realtime scenarios-2 cargo test --locked --release --test scenarios -- --nocapture --test-threads 1
realtime aec_gate cargo test --locked --release --test aec_gate -- --nocapture --test-threads 1

step luac-control luac -p ../mod/ptt-radio/control.lua
step luac-settings luac -p ../mod/ptt-radio/settings.lua
step lua-control-test lua ../mod/tests/control_test.lua ../mod/ptt-radio/control.lua

note "gate total $(($(date +%s) - gate_start))s, $([ "$failed" -eq 0 ] && echo passed || echo FAILED)"
exit "$failed"
