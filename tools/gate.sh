#!/bin/bash
# full acceptance gate: fmt, clippy, debug tests, release lib, the real-time scenario suite, aec_gate, lua checks.
# every step runs even after a failure; the exit status is non-zero when any step failed.
# a real-time step's failed tests get one rerun by exact name; a pass there is reported as flaky.
# usage: tools/gate.sh [--twice] [log dir]
#   --twice  runs the scenario suite a second time to expose flaky cases (optional)
# env: PTT_REALTIME_LOCK  lock file shared by everything that measures real-time audio on this machine
#                         (default: ptt-realtime.lock in the repository's git common dir, shared by all worktrees)
#      PTT_REALTIME_WAIT  seconds to wait for that lock per real-time step (default 3600)
set -uo pipefail
runs=1
if [ "${1:-}" = --twice ]; then
  runs=2
  shift
fi
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

# step <name> <command...>: runs the command from the companion dir (or $dir), output to <log>/<name>.log
step() {
  local name=$1 start rc
  shift
  start=$(date +%s)
  (cd "${dir:-$companion}" && "$@") > "$log/$name.log" 2>&1
  rc=$?
  [ "$rc" -eq 0 ] || failed=1
  note "$(printf '%-28s rc=%-3s %5ss' "$name" "$rc" "$(($(date +%s) - start))")"
  return "$rc"
}

# names from the last "failures:" block of a libtest log, one per line
failed_tests() {
  awk '/^failures:$/ { names = ""; collect = 1; next }
       /^test result:/ { collect = 0 }
       collect && /^    [^ ]+$/ { names = names $1 "\n" }
       END { printf "%s", names }' "$1"
}

# realtime <name> <command...>: as step, holding the shared real-time lock; the lock wait is reported apart.
# timing measurements on a shared machine flake under load, so the failed tests are rerun once by exact
# name: a pass on that rerun is reported as flaky and does not fail the gate.
realtime() {
  local name=$1 start prev=$failed test
  local -a tests=()
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
  if ! step "$name" "$@"; then
    # a read loop, not mapfile: the #!/bin/bash on macOS is bash 3.2
    while IFS= read -r test; do tests+=("$test"); done < <(failed_tests "$log/$name.log")
    if [ "${#tests[@]}" -gt 0 ] && step "$name-retry" "$@" --exact "${tests[@]}"; then
      failed=$prev
      note "$(printf '%-28s flaky: %s' "$name" "${tests[*]}")"
    fi
  fi
  exec 9>&-
}

note "gate $(git -C "$root" rev-parse HEAD) logs $log lock $lock scenario runs $runs"
step fmt cargo fmt --check
step clippy cargo clippy --locked --all-targets --release -- -D warnings

step debug-lib cargo test --locked --lib --bins
for t in bridge_repr connectivity crypto_sessions game_probe multiparty_lab netlab policy_contract policy_playout receive_order relay_hub transport_contract; do
  step "debug-$t" cargo test --locked --test "$t" -- --test-threads 1
done

step release-lib cargo test --locked --release --lib
# with --twice a second full run exposes flaky cases; a failure in either run fails the gate
for run in $(seq 1 "$runs"); do
  realtime "scenarios-$run" cargo test --locked --release --test scenarios -- --nocapture --test-threads 1
done
realtime aec_gate cargo test --locked --release --test aec_gate -- --nocapture --test-threads 1
# the policy revocation, retune and double-talk measurements of the real companion process
realtime policy_playout cargo test --locked --test policy_playout -- --ignored --nocapture --test-threads 1

# the mod's tests resolve its modules relative to ptt-radio/
dir=$root step luac-mod luac -p "$root"/mod/ptt-radio/*.lua "$root"/mod/tests/*.lua
for t in control channels admin settings_gui; do
  dir=$root step "lua-$t-test" lua "mod/tests/${t}_test.lua" mod/ptt-radio/control.lua
done

note "gate total $(($(date +%s) - gate_start))s, $([ "$failed" -eq 0 ] && echo passed || echo FAILED)"
exit "$failed"
