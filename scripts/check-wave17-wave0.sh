#!/usr/bin/env bash
# Reproducible Wave 17 / Wave 0 ratification gate.
#
# This script intentionally proves only host, compile, and proxy properties. It
# does not claim a physical-iPhone run, AirPods behavior, or device thermal
# evidence.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/check-wave17-wave0.sh <sampled|milestone|cancel-self-test|lock-self-test|quiescence-self-test>

  sampled    Run the complete Wave 0 gate with short maximum-capacity and
             eight-second neutral mobile host-proxy checks.
  milestone  Run every sampled gate, then add the ignored 1,800-second
             neutral mobile host-proxy soak.
  cancel-self-test
             Exercise the gate's one-PID cancellation and status contract
             against a TERM-resistant descendant. This is not evidence mode.
  lock-self-test
             Exercise live-owner exclusion, stale-lock recovery, and signal
             cleanup without entering any real gate stage. This is not
             evidence mode.
  quiescence-self-test
             Exercise host-load sample validation, streak reset, pass, and
             bounded-timeout behavior without entering a real gate stage.
             This is not evidence mode.

Sampled and milestone evidence runs are serialized across every worktree for
this user by:

  /tmp/fightbox-wave17-wave0-performance-${UID}.lock

The wrapper fails immediately when that lock has a live owner and reports its
PID. `/usr/bin/shlock` reclaims a stale lock atomically when the former owner is
gone. `cancel-self-test` does not acquire or inspect the evidence lock.

Immediately before each timing-sensitive stage, evidence mode requires three
consecutive decaying process-CPU readings at two-second intervals at or below
100 percent aggregate CPU, equivalent to no more than one fully occupied core.
It waits for at most two minutes with progress records, then rejects a
still-busy host as contaminated. This screens initial CPU contention; it does
not prove memory, storage-I/O, or thermal quiescence.

The milestone run takes at least 30 minutes. Launch it in the background and
preserve its output, PID, and final status outside the repository. For example:

  run_dir="$PWD/../evidence/wave17-wave0-$(date -u +%Y%m%dT%H%M%SZ)"
  mkdir -p "$run_dir"
  FIGHTBOX_WAVE17_STATUS_FILE="$run_dir/status" \
    nohup bash scripts/check-wave17-wave0.sh milestone \
    >"$run_dir/output.log" 2>&1 &
  echo "$!" >"$run_dir/pid"

When an agent exec harness reaps detached `nohup` children, launch the script
directly in a persistent PTY/session instead. Do not wrap it in `tee`: the
outer pipeline can mask an interrupt as success. Redirect the script itself,
and treat the status file as authoritative:

  exec env FIGHTBOX_WAVE17_STATUS_FILE="$run_dir/status" \
    bash scripts/check-wave17-wave0.sh milestone \
    >"$run_dir/output.log" 2>&1

Record the persistent session identifier and its process ID in the agent's
long-job registry so one interrupt reaches the gate rather than an outer pipe.

Follow progress with `tail -f "$run_dir/output.log"`. Stop that registered run
with:

  pid="$(cat "$run_dir/pid")"
  kill -TERM "$pid"

If a second evidence wrapper instead reports the host-wide lock owner, inspect
and stop that exact registered wrapper only when it is the run you intend to
cancel:

  lock_file="/tmp/fightbox-wave17-wave0-performance-${UID}.lock"
  owner_pid="$(cat "$lock_file")"
  kill -TERM "$owner_pid"

The wrapper tracks the active stage and recursively forwards termination to its
descendants before recording the result, so the registered wrapper PID is the
single cancellation handle. An empty status file means the process did not
reach its EXIT handler; otherwise it records the final numeric exit code.
Interrupt and terminate signals are recorded as 130 and 143 rather than as a
false successful run. If the bounded TERM/KILL cleanup cannot prove the stage
group empty, the wrapper records exit 70 and an explicit cleanup failure.

This gate does not produce physical-iPhone, AirPods, personalized-HRTF, jetsam,
or device-thermal proof. The long soak is explicitly a macOS host proxy.
USAGE
}

timestamp() {
  date -u '+%Y-%m-%dT%H:%M:%SZ'
}

log() {
  printf '[%s] %s\n' "$(timestamp)" "$*"
}

fail() {
  log "status=FAIL message=$*" >&2
  exit 1
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi

if [[ "$#" -ne 1 ]]; then
  usage >&2
  exit 64
fi

mode="$1"
case "$mode" in
  sampled)
    total_stages=21
    ;;
  milestone)
    total_stages=22
    ;;
  cancel-self-test)
    total_stages=1
    ;;
  lock-self-test)
    total_stages=0
    ;;
  quiescence-self-test)
    total_stages=0
    ;;
  *)
    usage >&2
    exit 64
    ;;
esac

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"
stage_index=0
revision=unknown
status_file="${FIGHTBOX_WAVE17_STATUS_FILE:-}"
status_file_active=0
active_stage_pid=""
active_stage_pgid=""
stage_launching=0
pending_signal_exit_code=""
performance_lock_file="/tmp/fightbox-wave17-wave0-performance-${UID}.lock"
performance_lock_owned=0
performance_lock_acquiring=0
lock_self_test_probe=0
lock_self_test_internal=0
lock_self_test_root=""
lock_self_test_child_pid=""
lock_self_test_stale_owner_pid=""

prepare_status_file() {
  if [[ -z "$status_file" ]]; then
    return
  fi
  case "$status_file" in
    /*) ;;
    *) fail "FIGHTBOX_WAVE17_STATUS_FILE must be an absolute path" ;;
  esac
  [[ -d "$(dirname "$status_file")" ]] \
    || fail "status-file parent does not exist: $(dirname "$status_file")"
  [[ ! -e "$status_file" && ! -L "$status_file" ]] \
    || fail "refusing to overwrite existing status path: $status_file"
  if ! (set -o noclobber; : >"$status_file") 2>/dev/null; then
    fail "could not create status file without clobbering: $status_file"
  fi
  status_file_active=1
}

read_performance_lock_owner() {
  local owner=""
  if [[ -r "$performance_lock_file" ]]; then
    IFS= read -r owner <"$performance_lock_file" || true
  fi
  case "$owner" in
    ''|*[!0-9]*) printf 'unknown\n' ;;
    *) printf '%s\n' "$owner" ;;
  esac
}

acquire_performance_lock() {
  [[ -x /usr/bin/shlock ]] \
    || fail "required command is unavailable: /usr/bin/shlock"
  local prior_owner
  local current_owner
  local stale_retry=0
  prior_owner="$(read_performance_lock_owner)"
  performance_lock_acquiring=1
  while ! /usr/bin/shlock -f "$performance_lock_file" -p "$$"; do
    current_owner="$(read_performance_lock_owner)"
    if [[ "$current_owner" != "unknown" ]] && kill -0 "$current_owner" 2>/dev/null; then
      performance_lock_acquiring=0
      fail "performance evidence lock is held: path=$performance_lock_file owner_pid=$current_owner; stop that registered wrapper with: kill -TERM $current_owner"
    fi
    if [[ "$stale_retry" -eq 0 && -e "$performance_lock_file" ]]; then
      stale_retry=1
      prior_owner="$current_owner"
      # shlock deliberately refuses to unlink a same-timestamp lock even after
      # its PID dies. Age that crash artifact past the one-second safety window,
      # then let shlock perform the only reclaim operation.
      log "performance_lock=STALE_WAIT path=$performance_lock_file prior_owner_pid=$prior_owner retry_after_s=1.1"
      sleep 1.1
      continue
    fi
    performance_lock_acquiring=0
    fail "could not acquire performance evidence lock through /usr/bin/shlock: path=$performance_lock_file owner_pid=$current_owner"
  done

  local acquire_window_probe_dir="${FIGHTBOX_WAVE17_LOCK_SELF_TEST_ACQUIRE_WINDOW_DIR:-}"
  if [[ -n "$acquire_window_probe_dir" ]]; then
    case "$mode" in
      sampled|milestone) ;;
      *) fail "the internal acquire-window probe requires sampled or milestone mode" ;;
    esac
    case "$acquire_window_probe_dir" in
      /*) ;;
      *) fail "FIGHTBOX_WAVE17_LOCK_SELF_TEST_ACQUIRE_WINDOW_DIR must be an absolute path" ;;
    esac
    [[ -d "$acquire_window_probe_dir" ]] \
      || fail "acquire-window probe directory does not exist: $acquire_window_probe_dir"
    [[ "$status_file_active" -eq 0 ]] \
      || fail "the internal acquire-window probe refuses an evidence status file"
    lock_self_test_internal=1
    printf '%s\n' "$$" >"$acquire_window_probe_dir/owner.pid"
    : >"$acquire_window_probe_dir/ready"
    log "mode=$mode evidence=false self_test=performance_lock_acquire_window status=READY probe_dir=$acquire_window_probe_dir"
    local attempt
    for ((attempt = 0; attempt < 600; attempt += 1)); do
      if [[ -e "$acquire_window_probe_dir/release" ]]; then
        fail "acquire-window probe was released instead of signaled"
      fi
      sleep 0.05
    done
    fail "acquire-window probe timed out after 30 seconds: $acquire_window_probe_dir"
  fi
  performance_lock_owned=1
  performance_lock_acquiring=0
  if [[ "$stale_retry" -eq 1 || "$prior_owner" != "unknown" ]]; then
    log "performance_lock=ACQUIRED path=$performance_lock_file owner_pid=$$ stale_reclaimed=true prior_owner_pid=$prior_owner"
  else
    log "performance_lock=ACQUIRED path=$performance_lock_file owner_pid=$$"
  fi
}

release_performance_lock() {
  if [[ "$performance_lock_owned" -ne 1 && "$performance_lock_acquiring" -ne 1 ]]; then
    return 0
  fi
  local was_owned="$performance_lock_owned"
  local current_owner
  current_owner="$(read_performance_lock_owner)"
  if [[ "$current_owner" != "$$" ]]; then
    performance_lock_owned=0
    performance_lock_acquiring=0
    if [[ "$was_owned" -eq 1 ]]; then
      log "performance_lock=NOT_RELEASED path=$performance_lock_file expected_owner_pid=$$ current_owner_pid=$current_owner" >&2
      return 70
    fi
    return 0
  fi
  if ! rm -f "$performance_lock_file"; then
    log "performance_lock=RELEASE_FAILED path=$performance_lock_file owner_pid=$$" >&2
    performance_lock_owned=0
    performance_lock_acquiring=0
    return 70
  fi
  performance_lock_owned=0
  performance_lock_acquiring=0
  log "performance_lock=RELEASED path=$performance_lock_file owner_pid=$$"
}

cleanup_lock_self_test_artifacts() {
  [[ -n "$lock_self_test_root" ]] || return 0
  local probe_name
  for probe_name in holder contender recovery acquire term; do
    rm -f \
      "$lock_self_test_root/$probe_name/owner.pid" \
      "$lock_self_test_root/$probe_name/ready" \
      "$lock_self_test_root/$probe_name/release"
    rmdir "$lock_self_test_root/$probe_name" 2>/dev/null || true
  done
  rm -f \
    "$lock_self_test_root/holder.log" \
    "$lock_self_test_root/contender.log" \
    "$lock_self_test_root/recovery.log" \
    "$lock_self_test_root/acquire.log" \
    "$lock_self_test_root/term.log"
  rmdir "$lock_self_test_root" 2>/dev/null || true
  lock_self_test_root=""
}

cleanup_lock_self_test_child() {
  local child_pid="$lock_self_test_child_pid"
  local cleanup_owner_pid=""
  local attempt
  if [[ -n "$child_pid" ]] && kill -0 "$child_pid" 2>/dev/null; then
    kill -TERM "$child_pid" 2>/dev/null || true
    for ((attempt = 0; attempt < 40; attempt += 1)); do
      if ! kill -0 "$child_pid" 2>/dev/null; then
        break
      fi
      sleep 0.05
    done
  fi
  if [[ -n "$child_pid" ]] && kill -0 "$child_pid" 2>/dev/null; then
    kill -KILL "$child_pid" 2>/dev/null || true
  fi
  if [[ -n "$child_pid" ]]; then
    wait "$child_pid" 2>/dev/null || true
    cleanup_owner_pid="$child_pid"
  fi
  lock_self_test_child_pid=""

  if [[ -n "$lock_self_test_stale_owner_pid" ]] \
    && [[ "$(read_performance_lock_owner)" == "$lock_self_test_stale_owner_pid" ]]; then
    cleanup_owner_pid="$lock_self_test_stale_owner_pid"
  fi
  if [[ -n "$cleanup_owner_pid" ]] \
    && [[ "$(read_performance_lock_owner)" == "$cleanup_owner_pid" ]]; then
    sleep 1.1
    if /usr/bin/shlock -f "$performance_lock_file" -p "$$"; then
      performance_lock_owned=1
      lock_self_test_stale_owner_pid=""
      log "performance_lock=ACQUIRED path=$performance_lock_file owner_pid=$$ stale_reclaimed=true prior_owner_pid=$cleanup_owner_pid cleanup=true"
    else
      log "performance_lock=SELF_TEST_CLEANUP_FAILED path=$performance_lock_file prior_owner_pid=$cleanup_owner_pid" >&2
      return 70
    fi
  fi
}

on_exit() {
  exit_code=$?
  trap '' INT TERM
  trap - EXIT
  if ! cleanup_lock_self_test_child && [[ "$exit_code" -eq 0 ]]; then
    exit_code=70
  fi
  cleanup_lock_self_test_artifacts
  if ! release_performance_lock && [[ "$exit_code" -eq 0 ]]; then
    exit_code=70
  fi
  if [[ "$status_file_active" -eq 1 ]]; then
    {
      printf 'exit_code=%s\n' "$exit_code"
      printf 'mode=%s\n' "$mode"
      printf 'revision=%s\n' "$revision"
      printf 'finished_at=%s\n' "$(timestamp)"
    } >"$status_file" || true
  fi
  if [[ "$lock_self_test_internal" -eq 1 && "$exit_code" -eq 0 ]]; then
    log "status=SELF_TEST_PASS mode=$mode evidence=false physical_device_proof=false"
  elif [[ "$lock_self_test_internal" -eq 1 ]]; then
    log "status=SELF_TEST_FAIL mode=$mode evidence=false exit_code=$exit_code physical_device_proof=false" >&2
  elif [[ "$exit_code" -eq 0 ]]; then
    log "status=PASS mode=$mode revision=$revision physical_device_proof=false"
  else
    log "status=FAIL mode=$mode revision=$revision exit_code=$exit_code physical_device_proof=false" >&2
  fi
  exit "$exit_code"
}

signal_stage_group() {
  local signal_name="$1"
  local stage_pid="$2"
  local stage_pgid="$3"
  if [[ -n "$stage_pgid" ]]; then
    kill "-$signal_name" -- "-$stage_pgid" 2>/dev/null || true
  else
    kill "-$signal_name" "$stage_pid" 2>/dev/null || true
  fi
}

stage_group_is_alive() {
  local stage_pid="$1"
  local stage_pgid="$2"
  if [[ -n "$stage_pgid" ]]; then
    kill -0 -- "-$stage_pgid" 2>/dev/null
  else
    kill -0 "$stage_pid" 2>/dev/null
  fi
}

wait_for_stage_group_exit() {
  local stage_pid="$1"
  local stage_pgid="$2"
  local attempts="$3"
  local attempt
  for ((attempt = 0; attempt < attempts; attempt += 1)); do
    if ! stage_group_is_alive "$stage_pid" "$stage_pgid"; then
      return 0
    fi
    sleep 0.05
  done
  ! stage_group_is_alive "$stage_pid" "$stage_pgid"
}

terminate_active_stage() {
  [[ -n "$active_stage_pid" ]] || return 0
  local stage_pid="$active_stage_pid"
  local stage_pgid="$active_stage_pgid"
  signal_stage_group TERM "$stage_pid" "$stage_pgid"
  if wait_for_stage_group_exit "$stage_pid" "$stage_pgid" 20; then
    wait "$stage_pid" 2>/dev/null || true
    active_stage_pid=""
    active_stage_pgid=""
    return 0
  fi
  signal_stage_group KILL "$stage_pid" "$stage_pgid"
  local cleanup_exit_code=0
  if ! wait_for_stage_group_exit "$stage_pid" "$stage_pgid" 40; then
    log "stage_process_group=$stage_pgid status=CLEANUP_FAILED signal=KILL exit_code=70" >&2
    cleanup_exit_code=70
  fi
  wait "$stage_pid" 2>/dev/null || true
  active_stage_pid=""
  active_stage_pgid=""
  return "$cleanup_exit_code"
}

on_signal() {
  local signal_exit_code="$1"
  if [[ "$stage_launching" -eq 1 ]]; then
    if [[ -z "$pending_signal_exit_code" ]]; then
      pending_signal_exit_code="$signal_exit_code"
    fi
    return
  fi
  trap '' INT TERM
  if terminate_active_stage; then
    exit "$signal_exit_code"
  else
    local cleanup_exit_code=$?
    exit "$cleanup_exit_code"
  fi
}

prepare_status_file
trap on_exit EXIT
trap 'on_signal 130' INT
trap 'on_signal 143' TERM

track_launched_stage() {
  local launched_pid="$1"
  active_stage_pid="$launched_pid"
  # Bash 3.2 job control gives this one-command background job a dedicated
  # process group led by $!, including the timestamping pipeline's subshell.
  active_stage_pgid="$launched_pid"
  stage_launching=0
  if [[ -n "$pending_signal_exit_code" ]]; then
    local signal_exit_code="$pending_signal_exit_code"
    pending_signal_exit_code=""
    on_signal "$signal_exit_code"
  fi
}

cleanup_completed_stage_group() {
  local stage_exit_code="$1"
  local stage_pid="$active_stage_pid"
  local stage_pgid="$active_stage_pgid"
  if stage_group_is_alive "$stage_pid" "$stage_pgid"; then
    log "stage_process_group=$stage_pgid status=ORPHANED cleanup=TERM_THEN_KILL" >&2
    signal_stage_group TERM "$stage_pid" "$stage_pgid"
    if ! wait_for_stage_group_exit "$stage_pid" "$stage_pgid" 20; then
      signal_stage_group KILL "$stage_pid" "$stage_pgid"
      if ! wait_for_stage_group_exit "$stage_pid" "$stage_pgid" 40; then
        log "stage_process_group=$stage_pgid status=CLEANUP_FAILED signal=KILL" >&2
        if [[ "$stage_exit_code" -eq 0 ]]; then
          stage_exit_code=70
        fi
      fi
    fi
    if [[ "$stage_exit_code" -eq 0 ]]; then
      stage_exit_code=70
    fi
  fi
  active_stage_pid=""
  active_stage_pgid=""
  return "$stage_exit_code"
}

run_stage() {
  label="$1"
  shift
  stage_index=$((stage_index + 1))
  started_at="$(date '+%s')"
  log "stage=$stage_index/$total_stages status=START name=$label"
  printf '[%s] stage=%s/%s status=COMMAND' \
    "$(timestamp)" "$stage_index" "$total_stages"
  for argument in "$@"; do
    printf ' %q' "$argument"
  done
  printf '\n'
  local launched_pid
  local stage_exit_code
  stage_launching=1
  set -m
  "$@" </dev/null &
  launched_pid=$!
  set +m
  track_launched_stage "$launched_pid"
  if wait "$active_stage_pid"; then
    stage_exit_code=0
  else
    stage_exit_code=$?
  fi
  if cleanup_completed_stage_group "$stage_exit_code"; then
    finished_at="$(date '+%s')"
    elapsed_seconds=$((finished_at - started_at))
    log "stage=$stage_index/$total_stages status=PASS elapsed_s=$elapsed_seconds name=$label"
  else
    exit_code=$?
    finished_at="$(date '+%s')"
    elapsed_seconds=$((finished_at - started_at))
    log "stage=$stage_index/$total_stages status=FAIL exit_code=$exit_code elapsed_s=$elapsed_seconds name=$label" >&2
    return "$exit_code"
  fi
}

run_stage_with_timestamped_output() {
  label="$1"
  shift
  stage_index=$((stage_index + 1))
  started_at="$(date '+%s')"
  log "stage=$stage_index/$total_stages status=START name=$label"
  printf '[%s] stage=%s/%s status=COMMAND' \
    "$(timestamp)" "$stage_index" "$total_stages"
  for argument in "$@"; do
    printf ' %q' "$argument"
  done
  printf '\n'
  local launched_pid
  local stage_exit_code
  stage_launching=1
  set -m
  (
    set -o pipefail
    "$@" 2>&1 | while IFS= read -r output_line || [[ -n "$output_line" ]]; do
      printf '[%s] stage=%s/%s status=OUTPUT %s\n' \
        "$(timestamp)" "$stage_index" "$total_stages" "$output_line"
    done
  ) </dev/null &
  launched_pid=$!
  set +m
  track_launched_stage "$launched_pid"
  if wait "$active_stage_pid"; then
    stage_exit_code=0
  else
    stage_exit_code=$?
  fi
  if cleanup_completed_stage_group "$stage_exit_code"; then
    finished_at="$(date '+%s')"
    elapsed_seconds=$((finished_at - started_at))
    log "stage=$stage_index/$total_stages status=PASS elapsed_s=$elapsed_seconds name=$label"
  else
    exit_code=$?
    finished_at="$(date '+%s')"
    elapsed_seconds=$((finished_at - started_at))
    log "stage=$stage_index/$total_stages status=FAIL exit_code=$exit_code elapsed_s=$elapsed_seconds name=$label" >&2
    return "$exit_code"
  fi
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command is unavailable: $1"
}

# Callback percentiles are only ratification evidence when unrelated host work
# is quiet immediately before each timing-sensitive stage. macOS `ps` reports
# a decaying CPU reading where 100 percent represents one fully occupied core.
# Every sample is parsed fail-closed before it can advance the three-sample
# admission streak.
sample_total_process_cpu_percent() {
  LC_ALL=C ps -A -o %cpu= \
    | awk '
        BEGIN { count = 0; invalid = 0; total = 0.0 }
        NF != 1 || $1 !~ /^[0-9]+([.][0-9]+)?$/ { invalid = 1; next }
        { count += 1; total += $1 }
        END {
          if (invalid || count == 0) {
            exit 65
          }
          printf "%.1f\n", total
        }
      '
}

advance_host_quiescence_streak() {
  local observed_percent="$1"
  local threshold_percent="$2"
  local prior_streak="$3"

  case "$observed_percent" in
    ''|*[!0-9.]*) return 65 ;;
  esac
  if ! awk \
    -v observed="$observed_percent" \
    'BEGIN {
      if (observed !~ /^[0-9]+([.][0-9]+)?$/) {
        exit 1
      }
    }'; then
    return 65
  fi

  if awk \
    -v observed="$observed_percent" \
    -v threshold="$threshold_percent" \
    'BEGIN { exit !(observed <= threshold) }'; then
    printf '%s\n' "$((prior_streak + 1))"
  else
    printf '0\n'
  fi
}

require_host_quiescence() {
  local sampler_function="${1:-sample_total_process_cpu_percent}"
  local maximum_attempts="${2:-60}"
  local sample_interval_seconds="${3:-2}"
  local threshold_percent=100.0
  local required_consecutive_samples=3
  local maximum_wait_seconds=120
  local consecutive_samples=0
  local attempt
  local total_process_cpu_percent
  local next_streak
  local load_averages
  local top_processes
  local started_at
  local deadline
  local now
  local remaining_seconds
  local sleep_seconds

  require_command awk
  require_command ps
  require_command sed
  require_command tr
  require_command sysctl
  command -v "$sampler_function" >/dev/null 2>&1 \
    || fail "host-quiescence sampler is unavailable: $sampler_function"

  started_at="$(date '+%s')"
  deadline=$((started_at + maximum_wait_seconds))
  for ((attempt = 1; attempt <= maximum_attempts; attempt += 1)); do
    if ! total_process_cpu_percent="$($sampler_function)"; then
      log "host_quiescence=INVALID_SAMPLE attempt=$attempt/$maximum_attempts sampler=$sampler_function" >&2
      return 65
    fi
    if ! next_streak="$(advance_host_quiescence_streak \
      "$total_process_cpu_percent" \
      "$threshold_percent" \
      "$consecutive_samples")"; then
      log "host_quiescence=INVALID_SAMPLE attempt=$attempt/$maximum_attempts value=$total_process_cpu_percent" >&2
      return 65
    fi
    consecutive_samples="$next_streak"
    load_averages="$(sysctl -n vm.loadavg)"
    now="$(date '+%s')"

    if [[ "$attempt" -eq 1 \
      || "$consecutive_samples" -gt 0 \
      || $((attempt % 5)) -eq 0 ]]; then
      log "host_quiescence=WAIT attempt=$attempt/$maximum_attempts total_process_cpu_percent=$total_process_cpu_percent threshold_percent=$threshold_percent consecutive=$consecutive_samples/$required_consecutive_samples load_averages=$load_averages nonclaims=memory_pressure,storage_io,thermal"
    fi

    if [[ "$consecutive_samples" -ge "$required_consecutive_samples" ]]; then
      log "host_quiescence=PASS total_process_cpu_percent=$total_process_cpu_percent threshold_percent=$threshold_percent consecutive=$consecutive_samples load_averages=$load_averages nonclaims=memory_pressure,storage_io,thermal"
      return 0
    fi

    if [[ "$attempt" -ge "$maximum_attempts" || "$now" -ge "$deadline" ]]; then
      break
    fi
    remaining_seconds=$((deadline - now))
    sleep_seconds="$sample_interval_seconds"
    if [[ "$sleep_seconds" -gt "$remaining_seconds" ]]; then
      sleep_seconds="$remaining_seconds"
    fi
    if [[ "$sleep_seconds" -gt 0 ]]; then
      sleep "$sleep_seconds"
    fi
  done

  now="$(date '+%s')"
  top_processes="$(
    LC_ALL=C ps -Ao pid=,%cpu=,ucomm= -r \
      | sed -n '1,5p' \
      | tr '\n' '|'
  )"
  log "host_quiescence=FAIL total_process_cpu_percent=$total_process_cpu_percent threshold_percent=$threshold_percent waited_s=$((now - started_at)) attempts=$attempt/$maximum_attempts top_processes=$top_processes nonclaims=memory_pressure,storage_io,thermal" >&2
  return 75
}

run_after_host_quiescence() {
  require_host_quiescence
  exec "$@"
}

host_quiescence_self_test_quiet_sampler() {
  printf '50.0\n'
}

host_quiescence_self_test_busy_sampler() {
  printf '101.0\n'
}

host_quiescence_self_test_blank_sampler() {
  :
}

run_host_quiescence_self_test() {
  local streak=0
  local sample
  local exit_code

  for sample in 50.0 75.0 100.0; do
    streak="$(advance_host_quiescence_streak "$sample" 100.0 "$streak")"
  done
  [[ "$streak" -eq 3 ]] \
    || fail "host-quiescence three-sample streak did not pass"

  streak=0
  for sample in 90.0 101.0 90.0 90.0 90.0; do
    streak="$(advance_host_quiescence_streak "$sample" 100.0 "$streak")"
  done
  [[ "$streak" -eq 3 ]] \
    || fail "host-quiescence over-threshold sample did not reset the streak"

  for sample in '' nan 10.0.0 '10 20'; do
    if advance_host_quiescence_streak "$sample" 100.0 0 >/dev/null 2>&1; then
      fail "host-quiescence malformed sample passed validation: $sample"
    fi
  done

  require_host_quiescence host_quiescence_self_test_quiet_sampler 3 0
  if require_host_quiescence host_quiescence_self_test_busy_sampler 60 0; then
    fail "host-quiescence busy sampler passed"
  else
    exit_code=$?
  fi
  [[ "$exit_code" -eq 75 ]] \
    || fail "host-quiescence busy sampler returned $exit_code instead of 75"
  if require_host_quiescence host_quiescence_self_test_blank_sampler 1 0; then
    fail "host-quiescence blank sampler passed"
  else
    exit_code=$?
  fi
  [[ "$exit_code" -eq 65 ]] \
    || fail "host-quiescence blank sampler returned $exit_code instead of 65"

  log "self_test=host_quiescence status=PASS real_stages=false"
}

require_stable_target() {
  target="$1"
  rustup target list --toolchain stable --installed | grep -qx "$target" \
    || fail "stable Rust target is not installed: $target"
}

ensure_fixture_link() {
  fixture_name="$1"
  relative_path="fixtures/assets/$fixture_name"
  source_path="$canonical_checkout/$relative_path"
  destination_path="$repo_root/$relative_path"

  git -C "$repo_root" check-ignore -q "$relative_path" \
    || fail "refusing to manage an unignored fixture path: $relative_path"
  [[ -d "$source_path" ]] \
    || fail "canonical ignored fixture directory is absent: $source_path"

  if [[ "$repo_root" == "$canonical_checkout" ]]; then
    [[ -d "$destination_path" ]] \
      || fail "canonical ignored fixture directory is absent: $destination_path"
    log "fixture=$fixture_name status=VALID canonical_directory=$destination_path"
    return
  fi

  if [[ -L "$destination_path" ]]; then
    [[ -d "$destination_path" ]] \
      || fail "fixture symlink is broken: $destination_path"
    source_real="$(cd "$source_path" && pwd -P)"
    destination_real="$(cd "$destination_path" && pwd -P)"
    [[ "$destination_real" == "$source_real" ]] \
      || fail "fixture symlink points outside the canonical checkout: $destination_path"
    log "fixture=$fixture_name status=VALID symlink=$destination_path target=$source_real"
    return
  fi

  [[ ! -e "$destination_path" ]] \
    || fail "refusing to replace non-symlink fixture content: $destination_path"
  ln -s "$source_path" "$destination_path"
  log "fixture=$fixture_name status=CREATED symlink=$destination_path target=$source_path"
}

require_clean_worktree() {
  worktree_status="$(git status --porcelain --untracked-files=normal)"
  if [[ -n "$worktree_status" ]]; then
    printf '%s\n' "$worktree_status" >&2
    return 1
  fi
}

require_revision_integrity() {
  current_revision="$(git rev-parse HEAD)"
  if [[ "$current_revision" != "$revision" ]]; then
    printf 'captured revision: %s\ncurrent revision:  %s\n' \
      "$revision" "$current_revision" >&2
    return 1
  fi
  require_clean_worktree
}

run_lock_self_test_probe_if_requested() {
  local probe_dir="${FIGHTBOX_WAVE17_LOCK_SELF_TEST_PROBE_DIR:-}"
  [[ -n "$probe_dir" ]] || return 0
  case "$mode" in
    sampled|milestone) ;;
    *) fail "the internal lock probe requires sampled or milestone mode" ;;
  esac
  case "$probe_dir" in
    /*) ;;
    *) fail "FIGHTBOX_WAVE17_LOCK_SELF_TEST_PROBE_DIR must be an absolute path" ;;
  esac
  [[ -d "$probe_dir" ]] \
    || fail "lock self-test probe directory does not exist: $probe_dir"
  [[ "$status_file_active" -eq 0 ]] \
    || fail "the internal lock probe refuses an evidence status file"

  lock_self_test_probe=1
  lock_self_test_internal=1
  printf '%s\n' "$$" >"$probe_dir/owner.pid"
  : >"$probe_dir/ready"
  log "mode=$mode evidence=false self_test=performance_lock_probe status=READY probe_dir=$probe_dir"
  local attempt
  for ((attempt = 0; attempt < 600; attempt += 1)); do
    if [[ -e "$probe_dir/release" ]]; then
      log "mode=$mode evidence=false self_test=performance_lock_probe status=RELEASED"
      return 0
    fi
    sleep 0.05
  done
  fail "lock self-test probe timed out after 30 seconds: $probe_dir"
}

wait_for_lock_self_test_probe() {
  local probe_dir="$1"
  local child_pid="$2"
  local attempt
  for ((attempt = 0; attempt < 100; attempt += 1)); do
    if [[ -e "$probe_dir/ready" ]]; then
      return 0
    fi
    if ! kill -0 "$child_pid" 2>/dev/null; then
      return 1
    fi
    sleep 0.05
  done
  return 1
}

run_performance_lock_self_test() {
  require_command bash
  require_command grep
  require_command mktemp
  [[ -x /usr/bin/shlock ]] \
    || fail "required command is unavailable: /usr/bin/shlock"

  lock_self_test_root="$(mktemp -d /tmp/fightbox-wave17-wave0-lock-self-test.XXXXXX)"
  mkdir \
    "$lock_self_test_root/holder" \
    "$lock_self_test_root/contender" \
    "$lock_self_test_root/recovery" \
    "$lock_self_test_root/acquire" \
    "$lock_self_test_root/term"

  log "self_test=performance_lock status=START real_stages=false"
  env FIGHTBOX_WAVE17_LOCK_SELF_TEST_PROBE_DIR="$lock_self_test_root/holder" \
    bash "$script_dir/check-wave17-wave0.sh" sampled \
    >"$lock_self_test_root/holder.log" 2>&1 &
  lock_self_test_child_pid=$!
  local holder_pid="$lock_self_test_child_pid"
  if ! wait_for_lock_self_test_probe "$lock_self_test_root/holder" "$holder_pid"; then
    sed -n '1,120p' "$lock_self_test_root/holder.log" >&2 || true
    fail "first sampled-mode lock probe did not acquire the evidence lock"
  fi
  [[ "$(read_performance_lock_owner)" == "$holder_pid" ]] \
    || fail "evidence lock did not name the live sampled-mode probe: expected=$holder_pid actual=$(read_performance_lock_owner)"

  local contender_exit_code
  if env FIGHTBOX_WAVE17_LOCK_SELF_TEST_PROBE_DIR="$lock_self_test_root/contender" \
    bash "$script_dir/check-wave17-wave0.sh" sampled \
    >"$lock_self_test_root/contender.log" 2>&1; then
    fail "second sampled-mode wrapper entered while live owner $holder_pid held the evidence lock"
  else
    contender_exit_code=$?
  fi
  [[ "$contender_exit_code" -eq 1 ]] \
    || fail "contending sampled-mode wrapper returned unexpected exit code: $contender_exit_code"
  [[ ! -e "$lock_self_test_root/contender/ready" ]] \
    || fail "contending sampled-mode wrapper reached its post-lock probe"
  grep -Fq "owner_pid=$holder_pid" "$lock_self_test_root/contender.log" \
    || fail "contending sampled-mode wrapper did not report live owner PID $holder_pid"
  if grep -Fq 'stage=1/' "$lock_self_test_root/contender.log"; then
    fail "contending sampled-mode wrapper entered a real stage"
  fi
  log "self_test=performance_lock check=live_owner_exclusion status=PASS owner_pid=$holder_pid"

  kill -KILL "$holder_pid"
  local killed_exit_code
  if wait "$holder_pid" 2>/dev/null; then
    fail "SIGKILLed sampled-mode lock probe returned success"
  else
    killed_exit_code=$?
  fi
  lock_self_test_child_pid=""
  [[ "$killed_exit_code" -eq 137 ]] \
    || fail "SIGKILLed sampled-mode lock probe returned unexpected exit code: $killed_exit_code"
  [[ "$(read_performance_lock_owner)" == "$holder_pid" ]] \
    || fail "SIGKILL did not leave the expected stale evidence lock for recovery"
  lock_self_test_stale_owner_pid="$holder_pid"

  : >"$lock_self_test_root/recovery/release"
  env FIGHTBOX_WAVE17_LOCK_SELF_TEST_PROBE_DIR="$lock_self_test_root/recovery" \
    bash "$script_dir/check-wave17-wave0.sh" sampled \
    >"$lock_self_test_root/recovery.log" 2>&1 &
  lock_self_test_child_pid=$!
  local recovery_pid="$lock_self_test_child_pid"
  local recovery_exit_code
  if wait "$recovery_pid"; then
    recovery_exit_code=0
  else
    recovery_exit_code=$?
  fi
  lock_self_test_child_pid=""
  if [[ "$recovery_exit_code" -ne 0 ]]; then
    sed -n '1,120p' "$lock_self_test_root/recovery.log" >&2 || true
  fi
  [[ "$recovery_exit_code" -eq 0 ]] \
    || fail "sampled-mode stale-lock recovery probe failed: exit_code=$recovery_exit_code"
  lock_self_test_stale_owner_pid=""
  [[ ! -e "$performance_lock_file" ]] \
    || fail "stale-lock recovery probe did not release its acquired evidence lock"
  grep -Fq "stale_reclaimed=true prior_owner_pid=$holder_pid" \
    "$lock_self_test_root/recovery.log" \
    || fail "sampled-mode recovery probe did not report shlock stale recovery"
  if grep -Fq 'stage=1/' "$lock_self_test_root/recovery.log"; then
    fail "stale-lock recovery probe entered a real stage"
  fi
  log "self_test=performance_lock check=shlock_stale_recovery status=PASS prior_owner_pid=$holder_pid recovery_pid=$recovery_pid"

  env FIGHTBOX_WAVE17_LOCK_SELF_TEST_ACQUIRE_WINDOW_DIR="$lock_self_test_root/acquire" \
    bash "$script_dir/check-wave17-wave0.sh" sampled \
    >"$lock_self_test_root/acquire.log" 2>&1 &
  lock_self_test_child_pid=$!
  local acquire_pid="$lock_self_test_child_pid"
  if ! wait_for_lock_self_test_probe "$lock_self_test_root/acquire" "$acquire_pid"; then
    sed -n '1,120p' "$lock_self_test_root/acquire.log" >&2 || true
    fail "acquire-window sampled-mode probe did not reach the post-shlock ownership window"
  fi
  [[ "$(read_performance_lock_owner)" == "$acquire_pid" ]] \
    || fail "acquire-window evidence lock did not name the sampled-mode probe"
  kill -TERM "$acquire_pid"
  local acquire_exit_code
  if wait "$acquire_pid"; then
    fail "TERM in the post-shlock ownership window returned success"
  else
    acquire_exit_code=$?
  fi
  lock_self_test_child_pid=""
  [[ "$acquire_exit_code" -eq 143 ]] \
    || fail "TERM in the post-shlock ownership window returned unexpected exit code: $acquire_exit_code"
  [[ ! -e "$performance_lock_file" ]] \
    || fail "post-shlock ownership-window signal left the evidence lock behind"
  grep -Fq "performance_lock=RELEASED path=$performance_lock_file owner_pid=$acquire_pid" \
    "$lock_self_test_root/acquire.log" \
    || fail "post-shlock ownership-window signal did not report evidence lock release"
  log "self_test=performance_lock check=post_shlock_signal_release status=PASS owner_pid=$acquire_pid"

  env FIGHTBOX_WAVE17_LOCK_SELF_TEST_PROBE_DIR="$lock_self_test_root/term" \
    bash "$script_dir/check-wave17-wave0.sh" sampled \
    >"$lock_self_test_root/term.log" 2>&1 &
  lock_self_test_child_pid=$!
  local term_pid="$lock_self_test_child_pid"
  if ! wait_for_lock_self_test_probe "$lock_self_test_root/term" "$term_pid"; then
    sed -n '1,120p' "$lock_self_test_root/term.log" >&2 || true
    fail "TERM cleanup sampled-mode probe did not acquire the evidence lock"
  fi
  kill -TERM "$term_pid"
  local term_exit_code
  if wait "$term_pid"; then
    fail "TERM cleanup sampled-mode probe returned success"
  else
    term_exit_code=$?
  fi
  lock_self_test_child_pid=""
  [[ "$term_exit_code" -eq 143 ]] \
    || fail "TERM cleanup sampled-mode probe returned unexpected exit code: $term_exit_code"
  [[ ! -e "$performance_lock_file" ]] \
    || fail "TERM signal path left the sampled-mode evidence lock behind"
  grep -Fq "performance_lock=RELEASED path=$performance_lock_file owner_pid=$term_pid" \
    "$lock_self_test_root/term.log" \
    || fail "TERM signal path did not report ownership-checked evidence lock release"
  log "self_test=performance_lock check=term_exit_release status=PASS owner_pid=$term_pid"

  cleanup_lock_self_test_artifacts
  log "self_test=performance_lock status=PASS real_stages=false"
}

if [[ "$mode" == "lock-self-test" ]]; then
  run_performance_lock_self_test
  exit 0
fi

if [[ "$mode" == "quiescence-self-test" ]]; then
  run_host_quiescence_self_test
  exit 0
fi

require_command git
require_command cargo
require_command rustup
require_command grep
require_command ps
require_command cc
require_command shasum
require_command xcrun

if [[ "$mode" == "cancel-self-test" ]]; then
  cancel_probe_dir="${FIGHTBOX_WAVE17_CANCEL_PROBE_DIR:-}"
  case "$cancel_probe_dir" in
    /*) ;;
    *) fail "FIGHTBOX_WAVE17_CANCEL_PROBE_DIR must be an absolute path" ;;
  esac
  [[ -d "$cancel_probe_dir" ]] \
    || fail "cancellation probe directory does not exist: $cancel_probe_dir"
  run_stage "TERM-resistant process-group cancellation probe" \
    bash -c '
      set -euo pipefail
      probe_dir="$1"
      bash -c '\''
        set -euo pipefail
        probe_dir="$1"
        trap "" INT TERM
        printf "%s\n" "$$" >"$probe_dir/child.pid"
        sleep 300 &
        grandchild_pid=$!
        printf "%s\n" "$grandchild_pid" >"$probe_dir/grandchild.pid"
        read -r process_group < <(ps -o pgid= -p "$$")
        printf "%s\n" "$process_group" >"$probe_dir/pgid"
        : >"$probe_dir/ready"
        wait "$grandchild_pid"
      '\'' bash "$probe_dir" &
      wait "$!"
    ' bash "$cancel_probe_dir"
  fail "cancellation self-test must be terminated by its driver"
fi

acquire_performance_lock
run_lock_self_test_probe_if_requested
if [[ "$lock_self_test_probe" -eq 1 ]]; then
  exit 0
fi

cd "$repo_root"
[[ "$(git rev-parse --is-inside-work-tree)" == "true" ]] \
  || fail "script is not running inside a Git worktree"
revision="$(git rev-parse HEAD)"
git_common_dir="$(git rev-parse --git-common-dir)"
case "$git_common_dir" in
  /*) ;;
  *) git_common_dir="$repo_root/$git_common_dir" ;;
esac
canonical_checkout="$(cd "$(dirname "$git_common_dir")" && pwd -P)"
steam_audio_sdk_dir="$canonical_checkout/.cache/steam-audio/steamaudio-4.8.1/steamaudio"
steam_audio_macos_lib_dir="$steam_audio_sdk_dir/lib/osx"

[[ -f "$steam_audio_sdk_dir/include/phonon.h" ]] \
  || fail "verified Steam Audio 4.8.1 header is absent: $steam_audio_sdk_dir/include/phonon.h"
[[ -f "$steam_audio_macos_lib_dir/libphonon.dylib" ]] \
  || fail "Steam Audio macOS library is absent: $steam_audio_macos_lib_dir/libphonon.dylib"
[[ -f "$steam_audio_sdk_dir/lib/ios/libphonon.a" ]] \
  || fail "Steam Audio iPhoneOS archive is absent: $steam_audio_sdk_dir/lib/ios/libphonon.a"

rustup run stable rustc --version >/dev/null
require_stable_target aarch64-apple-ios
require_stable_target aarch64-apple-ios-sim
xcrun --find swift >/dev/null
xcrun --find swiftc >/dev/null
xcrun --sdk iphoneos --show-sdk-path >/dev/null
xcrun --sdk iphonesimulator --show-sdk-path >/dev/null

ensure_fixture_link music
ensure_fixture_link squad

export STEAM_AUDIO_SDK_DIR="$steam_audio_sdk_dir"
if [[ -n "${DYLD_LIBRARY_PATH:-}" ]]; then
  export DYLD_LIBRARY_PATH="$steam_audio_macos_lib_dir:$DYLD_LIBRARY_PATH"
else
  export DYLD_LIBRARY_PATH="$steam_audio_macos_lib_dir"
fi

log "mode=$mode revision=$revision repo_root=$repo_root"
log "canonical_checkout=$canonical_checkout steam_audio_sdk_dir=$STEAM_AUDIO_SDK_DIR"
log "scope=host_and_compile_proof physical_iphone=false airpods=false personalized_hrtf=false device_thermal=false"

run_stage "clean nonignored worktree" require_clean_worktree
run_stage "rustfmt" cargo fmt --all -- --check
run_stage "working-tree whitespace diff" git diff --check
run_stage "index whitespace diff" git diff --cached --check

# Existing lint debt is allowed only for the three Wave 0 libraries. Tests are
# covered by the test gates below; this lint pass deliberately uses --lib so it
# does not mask unrelated CLI/UI debt. The allowances correspond to established
# constructor APIs and numeric loops, cfg-retained backend helpers, frozen Steam
# C acronym names, frozen unsafe C exports, and pre-existing local style debt.
clippy_allowances=(
  -A dead_code
  -A clippy::collapsible_if
  -A clippy::new_ret_no_self
  -A clippy::needless_range_loop
  -A clippy::too_many_arguments
  -A clippy::needless_return
  -A clippy::derivable_impls
  -A clippy::upper_case_acronyms
  -A clippy::field_reassign_with_default
  -A clippy::missing_safety_doc
)
run_stage "Wave 0 library clippy with audited existing allowances" \
  cargo clippy \
  -p fightbox-runtime \
  -p fightbox-steam-audio \
  -p fightbox-ffi \
  --lib \
  --features linked-sdk \
  -- \
  -D warnings \
  "${clippy_allowances[@]}"

run_stage "portable workspace" \
  cargo test --workspace --exclude fightbox-ffi
run_stage "complete linked workspace" \
  cargo test --workspace --features linked-sdk

run_stage "immutable one-source static hash" \
  cargo test -p fightbox-steam-audio --features linked-sdk \
  weight_zero_unbaked_direct_render_is_bit_identical_to_the_prechange_fingerprint -- \
  --nocapture --test-threads=1
run_stage "immutable eight-source final-mix hash" \
  cargo test -p fightbox-steam-audio --features linked-sdk \
  legacy_eight_point_source_final_mix_is_immutable -- \
  --nocapture --test-threads=1

run_stage "release host FFI archive" \
  cargo build -p fightbox-ffi --release
ffi_static_library="$repo_root/target/release/libfightbox_ffi.a"
[[ -f "$ffi_static_library" ]] \
  || fail "release FFI archive was not produced: $ffi_static_library"

run_stage "real frozen V1 C ABI" \
  bash "$repo_root/scripts/check-fightbox-ffi-v1-abi.sh" -- \
  "$ffi_static_library" \
  "-L$steam_audio_macos_lib_dir" \
  -lphonon \
  "-Wl,-rpath,$steam_audio_macos_lib_dir"
run_stage "real additive V2 C ABI" \
  bash "$repo_root/scripts/check-fightbox-ffi-v2-abi.sh" -- \
  "$ffi_static_library" \
  "-L$steam_audio_macos_lib_dir" \
  -lphonon \
  "-Wl,-rpath,$steam_audio_macos_lib_dir"

run_stage "Swift C-boundary compile proof" \
  bash "$repo_root/platforms/ios/test-swift-ffi-boundary.sh"
run_stage "arm64 iPhoneOS Rust archive compile" \
  env IPHONEOS_DEPLOYMENT_TARGET=15.0 \
  cargo +stable build --release --target aarch64-apple-ios -p fightbox-ffi
run_stage "arm64 iOS Simulator unavailable-backend compile" \
  env IPHONEOS_DEPLOYMENT_TARGET=15.0 \
  cargo +stable build --release --target aarch64-apple-ios-sim -p fightbox-ffi

run_stage "legacy eight-versus-sixteen short maximum gate" \
  run_after_host_quiescence \
  cargo test -p fightbox-steam-audio --release --features linked-sdk \
  wave0_eight_vs_sixteen_full_callback_and_footprint_short_soak -- \
  --ignored --nocapture --test-threads=1
run_stage "V2 16-feed Point desktop/mobile short maximum gate" \
  run_after_host_quiescence \
  cargo test -p fightbox-ffi --release \
  v2_sixteen_point_source_full_callback_short_soak -- \
  --ignored --nocapture --test-threads=1
run_stage "V2 48-feed desktop/mobile short maximum gate" \
  run_after_host_quiescence \
  cargo test -p fightbox-ffi --release \
  v2_sixteen_line_source_full_callback_short_soak -- \
  --ignored --nocapture --test-threads=1
run_stage "V2 sixteen-stereo-source residency gate" \
  run_after_host_quiescence \
  cargo test -p fightbox-ffi --release \
  v2_sixteen_stereo_images_bind_extra_history_and_thirty_two_feeds -- \
  --ignored --nocapture --test-threads=1

run_stage_with_timestamped_output "sampled eight-second neutral mobile host proxy" \
  run_after_host_quiescence \
  env FIGHTBOX_NEUTRAL_SOAK_SECONDS=8 \
  cargo test -p fightbox-steam-audio --release --features linked-sdk \
  neutral_mobile_sixteen_source_realtime_host_proxy_soak -- \
  --ignored --nocapture --test-threads=1

if [[ "$mode" == "milestone" ]]; then
  log "milestone_scope=host_sustained_load_proxy duration_s=1800 host_thermal_measurement=false device_thermal_proof=false physical_device_proof=false progress_interval_s=60"
  run_stage_with_timestamped_output "milestone 1,800-second neutral mobile host proxy" \
    run_after_host_quiescence \
    env FIGHTBOX_NEUTRAL_SOAK_SECONDS=1800 \
    cargo test -p fightbox-steam-audio --release --features linked-sdk \
    neutral_mobile_sixteen_source_realtime_host_proxy_soak -- \
    --ignored --nocapture --test-threads=1
fi

run_stage "frozen revision and final worktree quiescence" require_revision_integrity
[[ "$stage_index" -eq "$total_stages" ]] \
  || fail "stage accounting mismatch: completed=$stage_index expected=$total_stages"
