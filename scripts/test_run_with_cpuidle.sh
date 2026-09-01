#!/bin/sh
set -eu

test_root=$(mktemp -d "${TMPDIR:-/tmp}/snoozer-cpuidle-test.XXXXXX")
trap 'rm -rf "$test_root"' EXIT HUP INT TERM
sysfs_root=$test_root/sysfs
state_root=$test_root/state
write_helper=$test_root/write-helper
benchmark=$test_root/benchmark
runner=$(CDPATH= cd -- "$(dirname "$0")" && pwd)/run_with_cpuidle.sh
write_log=$test_root/writes
MAX_NORMAL_RUN_SECONDS=6
MIN_KILL_PATH_SECONDS=4
MAX_KILL_PATH_SECONDS=8

dash_shell=$(command -v dash 2>/dev/null || :)
dash_bin=
dash_runner=$runner
if [ -n "$dash_shell" ]; then
    dash_bin=$test_root/dash-bin
    mkdir "$dash_bin"
    ln -s "$dash_shell" "$dash_bin/sh"
    dash_runner=$test_root/dash-runner
    cat >"$dash_runner" <<'EOF'
#!/bin/sh
set -eu
PATH=$SNOOZER_TEST_DASH_BIN:$PATH
export PATH
exec "$SNOOZER_TEST_DASH_SHELL" "$SNOOZER_TEST_RUNNER" "$@"
EOF
    chmod +x "$dash_runner"
fi

monotonic_seconds() {
    awk '{ print int($1) }' /proc/uptime
}

for cpu in 0 1 2 3; do
    for specification in 0:POLL:1 1:C1:1 2:C1E:0 3:C2:0 4:C3:0; do
        state=${specification%%:*}
        remainder=${specification#*:}
        name=${remainder%%:*}
        value=${remainder##*:}
        directory=$sysfs_root/cpu$cpu/cpuidle/state$state
        mkdir -p "$directory"
        printf '%s\n' "$name" >"$directory/name"
        printf '%s\n' "$value" >"$directory/disable"
    done
done
# CPU 4 is deliberately outside the selected set and must never be touched.
mkdir -p "$sysfs_root/cpu4/cpuidle/state0"
printf 'C3\n' >"$sysfs_root/cpu4/cpuidle/state0/name"
printf '0\n' >"$sysfs_root/cpu4/cpuidle/state0/disable"

cat >"$write_helper" <<'EOF'
#!/bin/sh
set -eu
IFS= read -r value
target=$1
printf '%s|%s\n' "$value" "$target" >>"$SNOOZER_TEST_WRITE_LOG"
if [ "${SNOOZER_TEST_FAIL_VALUE:-}" = "$value" ] \
    && [ "${SNOOZER_TEST_FAIL_PATH_SUFFIX:-}" = "${target##*/cpuidle/}" ]; then
    exit 1
fi
if [ "${SNOOZER_TEST_MISMATCH_VALUE:-}" = "$value" ] \
    && [ "${SNOOZER_TEST_MISMATCH_PATH_SUFFIX:-}" = "${target##*/cpuidle/}" ]; then
    if [ "$value" = 1 ]; then unexpected=0; else unexpected=1; fi
    printf '%s\n' "$unexpected" >"$target"
    exit 0
fi
if [ "$value" = 1 ] && [ "${target##*/cpuidle/}" = state0/disable ]; then
    for pid_file in "${SNOOZER_TEST_RESTORE_BENCHMARK_PID_FILE:-}" \
        "${SNOOZER_TEST_RESTORE_DESCENDANT_PID_FILE:-}" \
        "${SNOOZER_TEST_RESTORE_ANCHOR_PID_FILE:-}" \
        "${SNOOZER_TEST_RESTORE_SUPERVISOR_PID_FILE:-}"; do
        [ -n "$pid_file" ] || continue
        [ -s "$pid_file" ] || exit 1
        inspected_pid=$(tr -d "[:space:]" <"$pid_file")
        case "$inspected_pid" in ""|*[!0-9]*) exit 1 ;; esac
        inspected_state=$(ps -o stat= -p "$inspected_pid" 2>/dev/null \
            | tr -d "[:space:]") || inspected_state=
        case "$inspected_state" in ""|Z*) ;; *) exit 1 ;; esac
    done
fi
printf '%s\n' "$value" >"$target"
if [ -n "${SNOOZER_TEST_FIRST_RESTORE_FILE:-}" ] \
    && [ ! -e "$SNOOZER_TEST_FIRST_RESTORE_FILE" ] \
    && [ "$value" = 1 ] && [ "${target##*/cpuidle/}" = state0/disable ]; then
    awk '{ printf "%.0f\n", $1 * 1000 }' /proc/uptime \
        >"$SNOOZER_TEST_FIRST_RESTORE_FILE"
fi
if [ "${SNOOZER_TEST_ADD_STATE_ON_RESTORE:-}" = 1 ] \
    && [ "$value" = 1 ] && [ "${target##*/cpuidle/}" = state0/disable ]; then
    added_state=${target%/state0/disable}/state5
    mkdir -p "$added_state"
    printf 'C4\n' >"$added_state/name"
    printf '0\n' >"$added_state/disable"
fi
EOF
chmod +x "$write_helper"

cat >"$benchmark" <<'EOF'
#!/bin/sh
set -eu
for cpu in 0 1 2 3; do
    [ "$(tr -d '[:space:]' <"$SNOOZER_SYSFS_ROOT/cpu$cpu/cpuidle/state0/disable")" = 0 ]
    [ "$(tr -d '[:space:]' <"$SNOOZER_SYSFS_ROOT/cpu$cpu/cpuidle/state1/disable")" = 0 ]
    [ "$(tr -d '[:space:]' <"$SNOOZER_SYSFS_ROOT/cpu$cpu/cpuidle/state2/disable")" = 1 ]
    [ "$(tr -d '[:space:]' <"$SNOOZER_SYSFS_ROOT/cpu$cpu/cpuidle/state3/disable")" = 1 ]
    [ "$(tr -d '[:space:]' <"$SNOOZER_SYSFS_ROOT/cpu$cpu/cpuidle/state4/disable")" = 1 ]
done
if [ -n "${SNOOZER_TEST_READY:-}" ]; then
    if [ -n "${SNOOZER_TEST_BENCHMARK_PID:-}" ]; then
        printf '%s\n' "$$" >"$SNOOZER_TEST_BENCHMARK_PID"
    fi
    : >"$SNOOZER_TEST_READY"
    if [ -n "${SNOOZER_TEST_DESCENDANT_PID:-}" ]; then
        sh -c '
            if [ "$2" = 1 ]; then trap "" TERM; fi
            printf "%s\n" "$$" >"$1"
            while :; do sleep 1; done
        ' sh "$SNOOZER_TEST_DESCENDANT_PID" \
            "${SNOOZER_TEST_DESCENDANT_IGNORE_TERM:-0}" &
    fi
    if [ "${SNOOZER_TEST_IGNORE_TERM:-}" = 1 ]; then
        trap '' TERM
    fi
    while [ ! -e "$SNOOZER_TEST_RELEASE" ]; do sleep 0.01; done
fi
if [ -n "${SNOOZER_TEST_BENCHMARK_EXIT_FILE:-}" ]; then
    awk '{ printf "%.0f\n", $1 * 1000 }' /proc/uptime \
        >"$SNOOZER_TEST_BENCHMARK_EXIT_FILE"
fi
EOF
chmod +x "$benchmark"

runner_env() {
    SNOOZER_SYSFS_ROOT=$sysfs_root \
    SNOOZER_STATE_DIR=$state_root \
    SNOOZER_WRITE_HELPER=$write_helper \
    SNOOZER_TEST_WRITE_LOG=$write_log \
    "$@"
}

run_benchmark() {
    runner_env "$runner" --binary "$benchmark" \
        --waiter-cpu 0 --victim-cpu 1 --producer-cpu 2 --controller-cpu 3 \
        --timeout-seconds 5
}

run_with_dash() {
    SNOOZER_TEST_DASH_BIN=$dash_bin \
    SNOOZER_TEST_DASH_SHELL=$dash_shell \
    SNOOZER_TEST_RUNNER=$runner \
    "$dash_runner" "$@"
}

assert_original() {
    for cpu in 0 1 2 3; do
        [ "$(tr -d '[:space:]' <"$sysfs_root/cpu$cpu/cpuidle/state0/disable")" = 1 ]
        [ "$(tr -d '[:space:]' <"$sysfs_root/cpu$cpu/cpuidle/state1/disable")" = 1 ]
        [ "$(tr -d '[:space:]' <"$sysfs_root/cpu$cpu/cpuidle/state2/disable")" = 0 ]
        [ "$(tr -d '[:space:]' <"$sysfs_root/cpu$cpu/cpuidle/state3/disable")" = 0 ]
        [ "$(tr -d '[:space:]' <"$sysfs_root/cpu$cpu/cpuidle/state4/disable")" = 0 ]
    done
}

assert_clean() {
    [ ! -e "$state_root/dirty" ]
    [ ! -e "$sysfs_root/.snoozer-cpuidle.dirty" ]
    for candidate_manifest in "$state_root"/manifest.*; do
        [ ! -e "$candidate_manifest" ] || return 1
    done
    for supervisor_status in "$state_root"/supervisor-status.*; do
        [ ! -e "$supervisor_status" ] || return 1
    done
    for supervisor_handshake in "$state_root"/supervisor-ready.* \
        "$state_root"/supervisor-go.* "$state_root"/supervisor-anchor-ready.* \
        "$state_root"/supervisor-anchor-release.* "$state_root"/guardian-ready.* \
        "$state_root"/guardian-release.* "$state_root"/guardian-group.* \
        "$state_root"/guardian-group-candidate.* \
        "$state_root"/guardian-proof-request.* \
        "$state_root"/guardian-proof-ready.*; do
        [ ! -e "$supervisor_handshake" ] || return 1
    done
}

assert_process_not_live() {
    inspected_pid=$1
    inspected_state=$(ps -o stat= -p "$inspected_pid" 2>/dev/null \
        | tr -d '[:space:]') || inspected_state=
    case "$inspected_state" in
        ''|Z*) return 0 ;;
        *) return 1 ;;
    esac
}

assert_process_does_not_hold_runner_lock() {
    inspected_pid=$1
    for descriptor in "/proc/$inspected_pid/fd"/*; do
        [ -e "$descriptor" ] || continue
        descriptor_target=$(readlink "$descriptor") || continue
        case "$descriptor_target" in
            "$sysfs_root/.snoozer-cpuidle.lock"|"$state_root/runner.lock"|"$state_root/active-run.lock")
                return 1
                ;;
        esac
    done
}

assert_guardian_lock_contract() {
    inspected_pid=$1
    active_seen=0
    for descriptor in "/proc/$inspected_pid/fd"/*; do
        [ -e "$descriptor" ] || continue
        descriptor_target=$(readlink "$descriptor") || continue
        case "$descriptor_target" in
            "$sysfs_root/.snoozer-cpuidle.lock"|"$state_root/runner.lock") return 1 ;;
            "$state_root/active-run.lock") active_seen=1 ;;
        esac
    done
    [ "$active_seen" -eq 1 ]
}

# First apply enables exact POLL/C1, disables every other state, then restores.
: >"$write_log"
normal_benchmark_exit=$test_root/normal-benchmark-exit-ms
normal_first_restore=$test_root/normal-first-restore-ms
normal_output=$test_root/normal.out
normal_start_epoch=$(monotonic_seconds)
SNOOZER_TEST_BENCHMARK_EXIT_FILE=$normal_benchmark_exit \
    SNOOZER_TEST_FIRST_RESTORE_FILE=$normal_first_restore \
    run_benchmark >"$normal_output"
normal_elapsed_seconds=$(($(monotonic_seconds) - normal_start_epoch))
[ "$normal_elapsed_seconds" -le "$MAX_NORMAL_RUN_SECONDS" ]
grep -Fq 'Only POLL and exact C1 are enabled on the assigned CPUs. C1E and every other CPU idle state, including C2/C3 and deeper states, are disabled' \
    "$normal_output"
[ -s "$normal_benchmark_exit" ]
[ -s "$normal_first_restore" ]
normal_benchmark_exit_ms=$(tr -d '[:space:]' <"$normal_benchmark_exit")
normal_first_restore_ms=$(tr -d '[:space:]' <"$normal_first_restore")
normal_drain_milliseconds=$((normal_first_restore_ms - normal_benchmark_exit_ms))
[ "$normal_drain_milliseconds" -ge 0 ]
[ "$normal_drain_milliseconds" -lt 2000 ]
assert_original
assert_clean
grep -q '|.*/state0/disable$' "$write_log"
grep -q '|.*/state4/disable$' "$write_log"
! grep -q '/cpu4/' "$write_log"
[ "$(tr -d '[:space:]' <"$sysfs_root/cpu4/cpuidle/state0/disable")" = 0 ]

# Runner SIGKILL after the workload is drained but before guardian release
# cannot expose an unowned, reusable PGID. The verified supervisor and anchor
# still own the group, so the guardian KILLs that exact group, proves it empty,
# and releases recovery only afterward.
drained_crash_supervisor_pid_file=$test_root/drained-crash-supervisor-pid
drained_crash_anchor_pid_file=$test_root/drained-crash-anchor-pid
drained_crash_guardian_signal_file=$test_root/drained-crash-guardian-signal
set +e
SNOOZER_TEST_KILL_AFTER_BENCHMARK_DRAIN=1 \
    SNOOZER_TEST_SUPERVISOR_PID_FILE=$drained_crash_supervisor_pid_file \
    SNOOZER_TEST_ANCHOR_PID_FILE=$drained_crash_anchor_pid_file \
    SNOOZER_TEST_GUARDIAN_SIGNAL_FILE=$drained_crash_guardian_signal_file \
    run_benchmark >"$test_root/drained-crash.out" 2>&1
drained_crash_status=$?
set -e
[ "$drained_crash_status" -eq 137 ]
[ -s "$drained_crash_supervisor_pid_file" ]
[ -s "$drained_crash_anchor_pid_file" ]
drained_crash_supervisor_pid=$(tr -d '[:space:]' \
    <"$drained_crash_supervisor_pid_file")
drained_crash_anchor_pid=$(tr -d '[:space:]' \
    <"$drained_crash_anchor_pid_file")
SNOOZER_TEST_RESTORE_SUPERVISOR_PID_FILE=$drained_crash_supervisor_pid_file \
    SNOOZER_TEST_RESTORE_ANCHOR_PID_FILE=$drained_crash_anchor_pid_file \
    runner_env "$runner" --recover \
    >"$test_root/drained-crash-recovery.out" 2>&1
[ -s "$drained_crash_guardian_signal_file" ]
drained_crash_signaled_group=$(tr -d '[:space:]' \
    <"$drained_crash_guardian_signal_file")
[ "$drained_crash_signaled_group" = "$drained_crash_supervisor_pid" ]
assert_process_not_live "$drained_crash_supervisor_pid"
assert_process_not_live "$drained_crash_anchor_pid"
assert_original
assert_clean

# Normal benchmark completion still drains a TERM-resistant descendant within
# one bounded grace period before cpuidle restoration.
normal_descendant_ready=$test_root/normal-descendant-ready
normal_descendant_release=$test_root/normal-descendant-release
normal_descendant_pid_file=$test_root/normal-descendant-pid
: >"$normal_descendant_release"
normal_descendant_start_epoch=$(monotonic_seconds)
SNOOZER_TEST_READY=$normal_descendant_ready \
    SNOOZER_TEST_RELEASE=$normal_descendant_release \
    SNOOZER_TEST_DESCENDANT_PID=$normal_descendant_pid_file \
    SNOOZER_TEST_DESCENDANT_IGNORE_TERM=1 run_benchmark >/dev/null 2>&1
normal_descendant_elapsed=$(($(monotonic_seconds) - normal_descendant_start_epoch))
[ "$normal_descendant_elapsed" -ge "$MIN_KILL_PATH_SECONDS" ]
[ "$normal_descendant_elapsed" -le "$MAX_KILL_PATH_SECONDS" ]
[ -s "$normal_descendant_pid_file" ]
normal_descendant_pid=$(tr -d '[:space:]' <"$normal_descendant_pid_file")
! kill -0 "$normal_descendant_pid" 2>/dev/null
assert_original
assert_clean

# A successful helper call that leaves an unexpected value exercises the
# read-back verification path, then restores and clears both recovery records.
set +e
SNOOZER_TEST_MISMATCH_VALUE=1 SNOOZER_TEST_MISMATCH_PATH_SUFFIX=state2/disable \
    run_benchmark >"$test_root/apply-mismatch.out" 2>&1
apply_mismatch_status=$?
set -e
[ "$apply_mismatch_status" -ne 0 ]
grep -q 'failed to apply' "$test_root/apply-mismatch.out"
assert_original
assert_clean

# A symlink in the CPU ancestry cannot redirect a manifest write outside the
# canonical sysfs root.
mv "$sysfs_root/cpu0" "$test_root/escaped-cpu0"
ln -s "$test_root/escaped-cpu0" "$sysfs_root/cpu0"
: >"$write_log"
set +e
symlink_output=$(run_benchmark 2>&1)
symlink_status=$?
set -e
[ "$symlink_status" -ne 0 ]
printf '%s\n' "$symlink_output" | grep -q 'invalid state entry'
[ ! -s "$write_log" ]
assert_clean
rm "$sysfs_root/cpu0"
mv "$test_root/escaped-cpu0" "$sysfs_root/cpu0"
assert_original

# TERM after apply follows the same exact restore path.
: >"$write_log"
set +e
SNOOZER_TEST_SIGNAL_AFTER_APPLY=TERM runner_env "$runner" --binary "$benchmark" \
    --waiter-cpu 0 --victim-cpu 1 --producer-cpu 2 --controller-cpu 3 \
    --timeout-seconds 5 >/dev/null 2>&1
signal_status=$?
set -e
[ "$signal_status" -eq 143 ]
assert_original
assert_clean

# TERM in the exact hard-link-to-assignment window re-adopts the expected global
# record, restores exact state, and removes both durable ownership records.
set +e
SNOOZER_TEST_SIGNAL_AFTER_GLOBAL_LINK=TERM \
    run_benchmark >"$test_root/global-link-term.out" 2>&1
global_link_term_status=$?
set -e
[ "$global_link_term_status" -eq 143 ]
assert_original
assert_clean

# Supervisor setup failure occurs before the benchmark go-ahead, is reaped
# without signaling an unverified numeric PID, and restores exact state.
set +e
SNOOZER_TEST_SUPERVISOR_EXIT_BEFORE_READY=1 \
    run_benchmark >"$test_root/supervisor-before-ready.out" 2>&1
supervisor_before_ready_status=$?
set -e
[ "$supervisor_before_ready_status" -ne 0 ]
grep -q 'exited before its startup handshake' \
    "$test_root/supervisor-before-ready.out"
assert_original
assert_clean

# Anchor startup failure is visible before GO; no benchmark process is launched,
# the failed anchor is reaped, and the applied CPU-idle policy is restored.
anchor_startup_benchmark_ready=$test_root/anchor-startup-benchmark-ready
set +e
SNOOZER_TEST_ANCHOR_EXIT_BEFORE_READY=1 \
    SNOOZER_TEST_READY=$anchor_startup_benchmark_ready \
    run_benchmark >"$test_root/anchor-startup-failure.out" 2>&1
anchor_startup_status=$?
set -e
[ "$anchor_startup_status" -ne 0 ]
grep -q 'process-group anchor.*startup handshake' \
    "$test_root/anchor-startup-failure.out"
[ ! -e "$anchor_startup_benchmark_ready" ]
assert_original
assert_clean

# Exit after publishing READY is classified as terminal immediately; no
# benchmark go-ahead can be consumed and state is restored without startup wait.
guardian_exit_start=$(monotonic_seconds)
set +e
SNOOZER_TEST_GUARDIAN_EXIT_AFTER_READY=1 \
    run_benchmark >"$test_root/guardian-after-ready.out" 2>&1
guardian_exit_status=$?
set -e
[ "$guardian_exit_status" -ne 0 ]
[ "$(($(monotonic_seconds) - guardian_exit_start))" -le "$MAX_NORMAL_RUN_SECONDS" ]
grep -q 'guardian exited after its startup handshake' \
    "$test_root/guardian-after-ready.out"
assert_original
assert_clean

supervisor_exit_start=$(monotonic_seconds)
set +e
SNOOZER_TEST_SUPERVISOR_EXIT_AFTER_READY=1 \
    run_benchmark >"$test_root/supervisor-after-ready.out" 2>&1
supervisor_exit_status=$?
set -e
[ "$supervisor_exit_status" -ne 0 ]
[ "$(($(monotonic_seconds) - supervisor_exit_start))" -le "$MAX_NORMAL_RUN_SECONDS" ]
grep -Eq 'exited before acquiring a dedicated process group|process-group anchor failed verification before benchmark authorization' \
    "$test_root/supervisor-after-ready.out"
assert_original
assert_clean

# A SIGKILL after a deliberately partial candidate write but before the atomic
# ready-marker publication leaves the guardian group path absent. The guardian
# therefore sends no group signal, and the supervisor cannot launch the
# benchmark because GO was never published.
partial_group_benchmark_ready=$test_root/partial-group-benchmark-ready
partial_group_never_release=$test_root/partial-group-never-release
partial_group_supervisor_pid_file=$test_root/partial-group-supervisor-pid
partial_group_guardian_signal_file=$test_root/partial-group-guardian-signal
set +e
SNOOZER_TEST_KILL_DURING_GROUP_PUBLICATION=1 \
    SNOOZER_TEST_GUARDIAN_SIGNAL_FILE=$partial_group_guardian_signal_file \
    SNOOZER_TEST_SUPERVISOR_PID_FILE=$partial_group_supervisor_pid_file \
    SNOOZER_TEST_READY=$partial_group_benchmark_ready \
    SNOOZER_TEST_RELEASE=$partial_group_never_release \
    run_benchmark >"$test_root/partial-group-publication.out" 2>&1
partial_group_status=$?
set -e
[ "$partial_group_status" -eq 137 ]
[ -s "$partial_group_supervisor_pid_file" ]
[ ! -e "$partial_group_benchmark_ready" ]
[ ! -e "$partial_group_guardian_signal_file" ]
for published_group_file in "$state_root"/guardian-group.*; do
    [ ! -e "$published_group_file" ] || exit 1
done
runner_env "$runner" --recover \
    >"$test_root/partial-group-recovery.out" 2>&1
partial_group_supervisor_pid=$(tr -d '[:space:]' \
    <"$partial_group_supervisor_pid_file")
partial_group_attempt=0
while ! assert_process_not_live "$partial_group_supervisor_pid" \
    && [ "$partial_group_attempt" -lt 600 ]; do
    sleep 0.01
    partial_group_attempt=$((partial_group_attempt + 1))
done
assert_process_not_live "$partial_group_supervisor_pid"
[ ! -e "$partial_group_benchmark_ready" ]
[ ! -e "$partial_group_guardian_signal_file" ]
assert_original
assert_clean

# Once the complete PGID marker is published, an injected candidate-read fault
# is ambiguous rather than unpublished. Runner death before GO must leave the
# guardian active lock and the supervisor/anchor PGID owners intact beyond the
# old startup bound. Recovery performs no write until candidate reads recover.
post_link_ready=$test_root/post-link-ready
post_link_never_release=$test_root/post-link-never-release
post_link_metadata_block=$test_root/post-link-metadata-block
post_link_guardian_signal=$test_root/post-link-guardian-signal
post_link_supervisor_pid_file=$test_root/post-link-supervisor-pid
post_link_anchor_pid_file=$test_root/post-link-anchor-pid
post_link_benchmark_ready=$test_root/post-link-benchmark-ready
: >"$post_link_metadata_block"
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    SNOOZER_TEST_AFTER_GROUP_PUBLICATION_READY="$post_link_ready" \
    SNOOZER_TEST_AFTER_GROUP_PUBLICATION_RELEASE="$post_link_never_release" \
    SNOOZER_TEST_GROUP_METADATA_READ_BLOCK_FILE="$post_link_metadata_block" \
    SNOOZER_TEST_GUARDIAN_SIGNAL_FILE="$post_link_guardian_signal" \
    SNOOZER_TEST_SUPERVISOR_PID_FILE="$post_link_supervisor_pid_file" \
    SNOOZER_TEST_ANCHOR_PID_FILE="$post_link_anchor_pid_file" \
    SNOOZER_TEST_READY="$post_link_benchmark_ready" \
    SNOOZER_TEST_RELEASE="$post_link_never_release" \
    "$runner" --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 \
    --producer-cpu 2 --controller-cpu 3 --timeout-seconds 30 \
    >"$test_root/post-link-runner.out" 2>&1 &
post_link_runner_pid=$!
post_link_attempt=0
while [ ! -e "$post_link_ready" ] \
    || [ ! -s "$post_link_supervisor_pid_file" ] \
    || [ ! -s "$post_link_anchor_pid_file" ]; do
    [ "$post_link_attempt" -lt 500 ] \
        || { kill -KILL "$post_link_runner_pid" 2>/dev/null || true; exit 1; }
    sleep 0.01
    post_link_attempt=$((post_link_attempt + 1))
done
post_link_supervisor_pid=$(tr -d '[:space:]' \
    <"$post_link_supervisor_pid_file")
post_link_anchor_pid=$(tr -d '[:space:]' <"$post_link_anchor_pid_file")
post_link_group=$(ps -o pgid= -p "$post_link_anchor_pid" | tr -d '[:space:]')
[ "$post_link_group" = "$post_link_supervisor_pid" ]
kill -KILL "$post_link_runner_pid"
set +e
wait "$post_link_runner_pid" 2>/dev/null
post_link_runner_status=$?
set -e
[ "$post_link_runner_status" -eq 137 ]
sleep 6
! assert_process_not_live "$post_link_supervisor_pid"
! assert_process_not_live "$post_link_anchor_pid"
[ "$(ps -o pgid= -p "$post_link_anchor_pid" | tr -d '[:space:]')" \
    = "$post_link_group" ]
[ ! -e "$post_link_benchmark_ready" ]
[ ! -e "$post_link_guardian_signal" ]
post_link_guardian_pid=
for guardian_ready in "$state_root"/guardian-ready.*; do
    [ -s "$guardian_ready" ] || continue
    post_link_guardian_pid=$(tr -d '[:space:]' <"$guardian_ready")
done
[ -n "$post_link_guardian_pid" ]
assert_guardian_lock_contract "$post_link_guardian_pid"
post_link_writes_before_recovery=$(wc -l <"$write_log")
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    SNOOZER_TEST_RESTORE_ANCHOR_PID_FILE="$post_link_anchor_pid_file" \
    SNOOZER_TEST_RESTORE_SUPERVISOR_PID_FILE="$post_link_supervisor_pid_file" \
    "$runner" --recover >"$test_root/post-link-recovery.out" 2>&1 &
post_link_recovery_pid=$!
sleep 0.1
kill -0 "$post_link_recovery_pid"
[ "$(wc -l <"$write_log")" -eq "$post_link_writes_before_recovery" ]
rm "$post_link_metadata_block"
wait "$post_link_recovery_pid"
[ -e "$post_link_guardian_signal" ]
assert_process_not_live "$post_link_supervisor_pid"
assert_process_not_live "$post_link_anchor_pid"
[ ! -e "$post_link_benchmark_ready" ]
assert_original
assert_clean

# A published guardian release with an unreadable proof request is also
# fail-closed. The guardian retains the active lock and the stopped supervisor
# plus anchor retain the PGID after runner SIGKILL; recovery cannot write until
# the exact proof request becomes readable and valid again.
proof_read_block=$test_root/proof-read-block
proof_guardian_signal=$test_root/proof-guardian-signal
proof_supervisor_pid_file=$test_root/proof-supervisor-pid
proof_anchor_pid_file=$test_root/proof-anchor-pid
: >"$proof_read_block"
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    SNOOZER_TEST_PROOF_REQUEST_READ_BLOCK_FILE="$proof_read_block" \
    SNOOZER_TEST_GUARDIAN_SIGNAL_FILE="$proof_guardian_signal" \
    SNOOZER_TEST_SUPERVISOR_PID_FILE="$proof_supervisor_pid_file" \
    SNOOZER_TEST_ANCHOR_PID_FILE="$proof_anchor_pid_file" \
    "$runner" --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 \
    --producer-cpu 2 --controller-cpu 3 --timeout-seconds 30 \
    >"$test_root/proof-read-runner.out" 2>&1 &
proof_runner_pid=$!
proof_attempt=0
proof_request_path=
while :; do
    proof_request_published=0
    proof_release_published=0
    for proof_request in "$state_root"/guardian-proof-request.*; do
        if [ -s "$proof_request" ]; then
            proof_request_published=1
            proof_request_path=$proof_request
            break
        fi
    done
    for proof_release in "$state_root"/guardian-release.*; do
        if [ -e "$proof_release" ]; then proof_release_published=1; break; fi
    done
    [ "$proof_request_published" -eq 0 ] \
        || [ "$proof_release_published" -eq 0 ] || break
    [ "$proof_attempt" -lt 500 ] \
        || { kill -KILL "$proof_runner_pid" 2>/dev/null || true; exit 1; }
    sleep 0.01
    proof_attempt=$((proof_attempt + 1))
done
[ -s "$proof_supervisor_pid_file" ]
[ -s "$proof_anchor_pid_file" ]
proof_supervisor_pid=$(tr -d '[:space:]' <"$proof_supervisor_pid_file")
proof_anchor_pid=$(tr -d '[:space:]' <"$proof_anchor_pid_file")
proof_guardian_pid=
for guardian_ready in "$state_root"/guardian-ready.*; do
    [ -s "$guardian_ready" ] || continue
    proof_guardian_pid=$(tr -d '[:space:]' <"$guardian_ready")
done
[ -n "$proof_guardian_pid" ]
! assert_process_not_live "$proof_supervisor_pid"
! assert_process_not_live "$proof_anchor_pid"
assert_guardian_lock_contract "$proof_guardian_pid"
[ ! -e "$proof_guardian_signal" ]
# Move from an injected read failure to malformed content without ever making
# the request valid in between. Both states must retain the same ownership.
printf 'invalid-proof\n' >"$proof_request_path"
rm "$proof_read_block"
sleep 0.1
! assert_process_not_live "$proof_supervisor_pid"
! assert_process_not_live "$proof_anchor_pid"
assert_guardian_lock_contract "$proof_guardian_pid"
[ ! -e "$proof_guardian_signal" ]
kill -KILL "$proof_runner_pid"
set +e
wait "$proof_runner_pid" 2>/dev/null
proof_runner_status=$?
set -e
[ "$proof_runner_status" -eq 137 ]
sleep 0.1
! assert_process_not_live "$proof_supervisor_pid"
! assert_process_not_live "$proof_anchor_pid"
assert_guardian_lock_contract "$proof_guardian_pid"
[ ! -e "$proof_guardian_signal" ]
proof_writes_before_recovery=$(wc -l <"$write_log")
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    SNOOZER_TEST_RESTORE_SUPERVISOR_PID_FILE="$proof_supervisor_pid_file" \
    SNOOZER_TEST_RESTORE_ANCHOR_PID_FILE="$proof_anchor_pid_file" \
    "$runner" --recover >"$test_root/proof-read-recovery.out" 2>&1 &
proof_recovery_pid=$!
sleep 0.1
kill -0 "$proof_recovery_pid"
[ "$(wc -l <"$write_log")" -eq "$proof_writes_before_recovery" ]
printf '%s\n' "$proof_supervisor_pid" >"$proof_request_path"
wait "$proof_recovery_pid"
[ -s "$proof_guardian_signal" ]
[ "$(tr -d '[:space:]' <"$proof_guardian_signal")" \
    = "$proof_supervisor_pid" ]
assert_process_not_live "$proof_supervisor_pid"
assert_process_not_live "$proof_anchor_pid"
assert_original
assert_clean

# Checked go-ahead failure KILLs and proves the verified inner group empty
# before restoration instead of waiting for a zombie supervisor.
set +e
SNOOZER_TEST_FAIL_GO_PUBLICATION=1 \
    run_benchmark >"$test_root/go-publication-failure.out" 2>&1
go_publication_status=$?
set -e
[ "$go_publication_status" -ne 0 ]
grep -q 'cannot publish the benchmark go-ahead' \
    "$test_root/go-publication-failure.out"
assert_original
assert_clean

# Ubuntu uses dash for /bin/sh. Its kill builtin rejects `kill -KILL -- -PGID`
# but accepts the POSIX-style `kill -s KILL -- -PGID`. Force dash for both the
# runner and its nested shells when available so this runner-side regression
# does not depend on the developer host's /bin/sh selection.
if [ -n "$dash_shell" ]; then
    set +e
    SNOOZER_TEST_FAIL_GO_PUBLICATION=1 runner_env run_with_dash \
        --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 \
        --producer-cpu 2 --controller-cpu 3 --timeout-seconds 5 \
        >"$test_root/dash-group-signal.out" 2>&1
    dash_group_signal_status=$?
    set -e
    [ "$dash_group_signal_status" -ne 0 ]
    grep -q 'cannot publish the benchmark go-ahead' \
        "$test_root/dash-group-signal.out"
    assert_original
    assert_clean
fi

# A failed post-KILL inspection keeps the guardian and active lock alive. The
# pre-KILL gate first proves the supervisor is still stopped when the guardian
# owns teardown; after KILL, no restore write occurs until the inspection fault
# clears and the guardian can prove the group has no non-zombie members.
post_kill_block=$test_root/post-kill-inspection-block
post_kill_pre_block=$test_root/post-kill-pre-signal-block
post_kill_guardian_signal=$test_root/post-kill-guardian-signal
post_kill_supervisor_pid_file=$test_root/post-kill-supervisor-pid
: >"$post_kill_block"
: >"$post_kill_pre_block"
writes_before_post_kill=$(wc -l <"$write_log")
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    SNOOZER_TEST_GUARDIAN_PRE_GROUP_KILL_BLOCK_FILE="$post_kill_pre_block" \
    SNOOZER_TEST_POST_KILL_BLOCK_FILE="$post_kill_block" \
    SNOOZER_TEST_GUARDIAN_SIGNAL_FILE="$post_kill_guardian_signal" \
    SNOOZER_TEST_SUPERVISOR_PID_FILE="$post_kill_supervisor_pid_file" \
    "$runner" --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 \
    --producer-cpu 2 --controller-cpu 3 --timeout-seconds 5 \
    >"$test_root/post-kill-inspection.out" 2>&1 &
post_kill_runner_pid=$!
attempt=0
while :; do
    proof_published=0
    for proof_request in "$state_root"/guardian-proof-request.*; do
        if [ -s "$proof_request" ]; then proof_published=1; break; fi
    done
    [ "$proof_published" -eq 0 ] || break
    [ "$attempt" -lt 500 ] \
        || { kill -KILL "$post_kill_runner_pid" 2>/dev/null || true; exit 1; }
    sleep 0.01
    attempt=$((attempt + 1))
done
[ -s "$post_kill_supervisor_pid_file" ]
post_kill_supervisor_pid=$(tr -d '[:space:]' \
    <"$post_kill_supervisor_pid_file")
post_kill_supervisor_state=$(ps -o stat= -p "$post_kill_supervisor_pid" \
    2>/dev/null | tr -d '[:space:]') || post_kill_supervisor_state=
case "$post_kill_supervisor_state" in T*) ;; *) exit 1 ;; esac
post_kill_guardian_pid=
for guardian_ready in "$state_root"/guardian-ready.*; do
    [ -s "$guardian_ready" ] || continue
    post_kill_guardian_pid=$(tr -d '[:space:]' <"$guardian_ready")
done
[ -n "$post_kill_guardian_pid" ]
assert_guardian_lock_contract "$post_kill_guardian_pid"
rm "$post_kill_pre_block"
attempt=0
while [ ! -s "$post_kill_guardian_signal" ]; do
    [ "$attempt" -lt 500 ] \
        || { kill -KILL "$post_kill_runner_pid" 2>/dev/null || true; exit 1; }
    sleep 0.01
    attempt=$((attempt + 1))
done
[ "$(tr -d '[:space:]' <"$post_kill_guardian_signal")" \
    = "$post_kill_supervisor_pid" ]
assert_guardian_lock_contract "$post_kill_guardian_pid"
kill -0 "$post_kill_runner_pid"
[ -f "$state_root/dirty" ]
[ -f "$sysfs_root/.snoozer-cpuidle.dirty" ]
[ "$(tr -d '[:space:]' <"$sysfs_root/cpu0/cpuidle/state0/disable")" = 0 ]
# Only apply writes have happened; the blocked proof precedes every restore.
[ "$(wc -l <"$write_log")" -eq "$((writes_before_post_kill + 20))" ]
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    "$runner" --recover >"$test_root/post-kill-recovery.out" 2>&1 &
post_kill_recovery_pid=$!
sleep 0.1
kill -0 "$post_kill_recovery_pid"
[ "$(wc -l <"$write_log")" -eq "$((writes_before_post_kill + 20))" ]
rm "$post_kill_block"
set +e
wait "$post_kill_runner_pid"
post_kill_runner_status=$?
wait "$post_kill_recovery_pid"
post_kill_recovery_status=$?
set -e
[ "$post_kill_runner_status" -eq 0 ]
[ "$post_kill_recovery_status" -ne 0 ]
assert_process_not_live "$post_kill_supervisor_pid"
assert_original
assert_clean

# Once the verified group identity is published, a deferred TERM is no longer
# latched: the ordinary bounded termination path runs immediately.
set +e
SNOOZER_TEST_SIGNAL_AFTER_GROUP_VERIFY=TERM \
    run_benchmark >"$test_root/post-group-term.out" 2>&1
post_group_term_status=$?
set -e
[ "$post_group_term_status" -eq 143 ]
assert_original
assert_clean

# A status-write failure leaves the supervisor stopped while the separate anchor
# preserves the PGID until the runner drains and reaps the group.
set +e
SNOOZER_TEST_SUPERVISOR_SKIP_STATUS=1 \
    run_benchmark >"$test_root/supervisor-status-failure.out" 2>&1
supervisor_status_failure=$?
set -e
[ "$supervisor_status_failure" -ne 0 ]
grep -q 'stopped without reporting benchmark status' \
    "$test_root/supervisor-status-failure.out"
assert_original
assert_clean

# A failure immediately after the global link is published still restores and
# removes the authoritative global record without dangling its manifest.
set +e
SNOOZER_TEST_FAIL_AFTER_GLOBAL_LINK=1 \
    run_benchmark >"$test_root/post-global-link-failure.out" 2>&1
post_link_status=$?
set -e
[ "$post_link_status" -ne 0 ]
grep -q 'failed to publish the global dirty-owner record' \
    "$test_root/post-global-link-failure.out"
assert_original
assert_clean

# A failed apply/read-back path restores from the durable records and clears
# both local and global dirty ownership only after exact restoration.
set +e
SNOOZER_TEST_FAIL_VALUE=1 SNOOZER_TEST_FAIL_PATH_SUFFIX=state2/disable \
    run_benchmark >"$test_root/apply-failure.out" 2>&1
apply_failure_status=$?
set -e
[ "$apply_failure_status" -ne 0 ]
grep -q 'failed to apply' "$test_root/apply-failure.out"
assert_original
assert_clean

# If the cpuidle inventory changes during restoration, the next write is
# refused and both recovery records remain authoritative for an explicit retry.
set +e
SNOOZER_TEST_ADD_STATE_ON_RESTORE=1 \
    run_benchmark >"$test_root/restore-inventory-change.out" 2>&1
restore_inventory_status=$?
set -e
[ "$restore_inventory_status" -ne 0 ]
grep -q 'does not exactly cover the current CPU state inventory' \
    "$test_root/restore-inventory-change.out"
[ -f "$state_root/dirty" ]
[ -f "$sysfs_root/.snoozer-cpuidle.dirty" ]
rm -rf "$sysfs_root/cpu0/cpuidle/state5"
runner_env "$runner" --recover >/dev/null
assert_original
assert_clean

# An external TERM has its own TERM-to-KILL deadline even if the workload
# ignores TERM, and state restoration still completes.
ready=$test_root/signal-ready
never_release=$test_root/signal-never-release
benchmark_pid_file=$test_root/signal-benchmark-pid
descendant_pid_file=$test_root/signal-descendant-pid
signal_start_epoch=$(monotonic_seconds)
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    SNOOZER_TEST_READY="$ready" SNOOZER_TEST_RELEASE="$never_release" \
    SNOOZER_TEST_IGNORE_TERM=1 SNOOZER_TEST_BENCHMARK_PID="$benchmark_pid_file" \
    SNOOZER_TEST_DESCENDANT_PID="$descendant_pid_file" \
    SNOOZER_TEST_DESCENDANT_IGNORE_TERM=1 \
    "$runner" --binary "$benchmark" \
    --waiter-cpu 0 --victim-cpu 1 --producer-cpu 2 --controller-cpu 3 \
    --timeout-seconds 5 >"$test_root/signal.out" 2>&1 &
signal_runner_pid=$!
attempt=0
while [ ! -e "$ready" ] && [ "$attempt" -lt 300 ]; do
    sleep 0.01
    attempt=$((attempt + 1))
done
[ -e "$ready" ] || { kill "$signal_runner_pid" 2>/dev/null || true; exit 1; }
benchmark_pid=$(tr -d '[:space:]' <"$benchmark_pid_file")
attempt=0
while [ ! -e "$descendant_pid_file" ] && [ "$attempt" -lt 300 ]; do
    sleep 0.01
    attempt=$((attempt + 1))
done
[ -e "$descendant_pid_file" ] || { kill "$signal_runner_pid" 2>/dev/null || true; exit 1; }
descendant_pid=$(tr -d '[:space:]' <"$descendant_pid_file")
kill -TERM "$signal_runner_pid"
attempt=0
while kill -0 "$signal_runner_pid" 2>/dev/null && [ "$attempt" -lt 800 ]; do
    sleep 0.01
    attempt=$((attempt + 1))
done
if kill -0 "$signal_runner_pid" 2>/dev/null; then
    kill -KILL "$signal_runner_pid" 2>/dev/null || true
    exit 1
fi
set +e
wait "$signal_runner_pid"
signal_status=$?
set -e
signal_elapsed_seconds=$(($(monotonic_seconds) - signal_start_epoch))
[ "$signal_status" -eq 143 ]
[ "$signal_elapsed_seconds" -ge "$MIN_KILL_PATH_SECONDS" ]
[ "$signal_elapsed_seconds" -le "$MAX_KILL_PATH_SECONDS" ]
! kill -0 "$benchmark_pid" 2>/dev/null
! kill -0 "$descendant_pid" 2>/dev/null
assert_original
assert_clean

# HUP and INT while the benchmark is active use the same bounded group cleanup.
for signal_specification in HUP:129 INT:130; do
    active_signal=${signal_specification%%:*}
    expected_status=${signal_specification##*:}
    ready=$test_root/active-$active_signal-ready
    never_release=$test_root/active-$active_signal-never-release
    benchmark_pid_file=$test_root/active-$active_signal-benchmark-pid
    env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
        SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
        SNOOZER_TEST_READY="$ready" SNOOZER_TEST_RELEASE="$never_release" \
        SNOOZER_TEST_BENCHMARK_PID="$benchmark_pid_file" \
        python3 -c 'import os, signal, sys; signal.signal(signal.SIGINT, signal.SIG_DFL); os.execv(sys.argv[1], sys.argv[1:])' \
        "$runner" --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 \
        --producer-cpu 2 --controller-cpu 3 --timeout-seconds 5 \
        >"$test_root/active-$active_signal.out" 2>&1 &
    active_runner_pid=$!
    attempt=0
    while [ ! -e "$ready" ] && [ "$attempt" -lt 300 ]; do
        sleep 0.01
        attempt=$((attempt + 1))
    done
    [ -e "$ready" ] || { kill "$active_runner_pid" 2>/dev/null || true; exit 1; }
    active_benchmark_pid=$(tr -d '[:space:]' <"$benchmark_pid_file")
    kill -"$active_signal" "$active_runner_pid"
    attempt=0
    while kill -0 "$active_runner_pid" 2>/dev/null && [ "$attempt" -lt 800 ]; do
        sleep 0.01
        attempt=$((attempt + 1))
    done
    if kill -0 "$active_runner_pid" 2>/dev/null; then
        kill -KILL "$active_runner_pid" 2>/dev/null || true
        exit 1
    fi
    set +e
    wait "$active_runner_pid"
    active_status=$?
    set -e
    [ "$active_status" -eq "$expected_status" ]
    ! kill -0 "$active_benchmark_pid" 2>/dev/null
    assert_original
    assert_clean
done

# Killing only the supervisor after GO cannot orphan a live benchmark group.
# The stable in-group anchor preserves the PGID until the runner KILLs it and
# the guardian proves every non-zombie member gone before the first restore.
supervisor_death_ready=$test_root/supervisor-death-ready
supervisor_death_never_release=$test_root/supervisor-death-never-release
supervisor_death_supervisor_pid_file=$test_root/supervisor-death-supervisor-pid
supervisor_death_anchor_pid_file=$test_root/supervisor-death-anchor-pid
supervisor_death_benchmark_pid_file=$test_root/supervisor-death-benchmark-pid
supervisor_death_descendant_pid_file=$test_root/supervisor-death-descendant-pid
supervisor_death_first_restore=$test_root/supervisor-death-first-restore
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    SNOOZER_TEST_READY="$supervisor_death_ready" \
    SNOOZER_TEST_RELEASE="$supervisor_death_never_release" \
    SNOOZER_TEST_IGNORE_TERM=1 SNOOZER_TEST_DESCENDANT_IGNORE_TERM=1 \
    SNOOZER_TEST_SUPERVISOR_PID_FILE="$supervisor_death_supervisor_pid_file" \
    SNOOZER_TEST_ANCHOR_PID_FILE="$supervisor_death_anchor_pid_file" \
    SNOOZER_TEST_BENCHMARK_PID="$supervisor_death_benchmark_pid_file" \
    SNOOZER_TEST_DESCENDANT_PID="$supervisor_death_descendant_pid_file" \
    SNOOZER_TEST_RESTORE_BENCHMARK_PID_FILE="$supervisor_death_benchmark_pid_file" \
    SNOOZER_TEST_RESTORE_DESCENDANT_PID_FILE="$supervisor_death_descendant_pid_file" \
    SNOOZER_TEST_RESTORE_ANCHOR_PID_FILE="$supervisor_death_anchor_pid_file" \
    SNOOZER_TEST_FIRST_RESTORE_FILE="$supervisor_death_first_restore" \
    "$runner" --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 \
    --producer-cpu 2 --controller-cpu 3 --timeout-seconds 30 \
    >"$test_root/supervisor-death.out" 2>&1 &
supervisor_death_runner_pid=$!
attempt=0
while [ ! -s "$supervisor_death_supervisor_pid_file" ] \
    || [ ! -s "$supervisor_death_anchor_pid_file" ] \
    || [ ! -s "$supervisor_death_benchmark_pid_file" ] \
    || [ ! -s "$supervisor_death_descendant_pid_file" ]; do
    if [ "$attempt" -ge 500 ]; then
        printf 'supervisor=%s anchor=%s benchmark=%s descendant=%s\n' \
            "$(test -s "$supervisor_death_supervisor_pid_file" && echo ready || echo missing)" \
            "$(test -s "$supervisor_death_anchor_pid_file" && echo ready || echo missing)" \
            "$(test -s "$supervisor_death_benchmark_pid_file" && echo ready || echo missing)" \
            "$(test -s "$supervisor_death_descendant_pid_file" && echo ready || echo missing)" >&2
        cat "$test_root/supervisor-death.out" >&2
        kill -KILL "$supervisor_death_runner_pid" 2>/dev/null || true
        exit 1
    fi
    sleep 0.01
    attempt=$((attempt + 1))
done
supervisor_death_supervisor_pid=$(tr -d '[:space:]' \
    <"$supervisor_death_supervisor_pid_file")
supervisor_death_anchor_pid=$(tr -d '[:space:]' \
    <"$supervisor_death_anchor_pid_file")
supervisor_death_benchmark_pid=$(tr -d '[:space:]' \
    <"$supervisor_death_benchmark_pid_file")
supervisor_death_descendant_pid=$(tr -d '[:space:]' \
    <"$supervisor_death_descendant_pid_file")
supervisor_death_group=$(ps -o pgid= -p "$supervisor_death_anchor_pid" \
    | tr -d '[:space:]')
[ "$supervisor_death_group" = "$supervisor_death_supervisor_pid" ]
assert_process_does_not_hold_runner_lock "$supervisor_death_anchor_pid"
kill -KILL "$supervisor_death_supervisor_pid"
attempt=0
while kill -0 "$supervisor_death_runner_pid" 2>/dev/null \
    && [ "$attempt" -lt 500 ]; do
    sleep 0.01
    attempt=$((attempt + 1))
done
if kill -0 "$supervisor_death_runner_pid" 2>/dev/null; then
    cat "$test_root/supervisor-death.out" >&2
    kill -KILL "$supervisor_death_runner_pid" 2>/dev/null || true
    exit 1
fi
set +e
wait "$supervisor_death_runner_pid"
supervisor_death_status=$?
set -e
[ "$supervisor_death_status" -ne 0 ]
if ! grep -q 'supervisor exited after go-ahead; its stable process group was killed and drained' \
    "$test_root/supervisor-death.out"; then
    cat "$test_root/supervisor-death.out" >&2
    exit 1
fi
[ -s "$supervisor_death_first_restore" ]
assert_process_not_live "$supervisor_death_benchmark_pid"
assert_process_not_live "$supervisor_death_descendant_pid"
assert_process_not_live "$supervisor_death_anchor_pid"
assert_original
assert_clean

# The outer timeout terminates the workload and still restores exact state.
ready=$test_root/timeout-ready
never_release=$test_root/timeout-never-release
timeout_benchmark_pid_file=$test_root/timeout-benchmark-pid
timeout_descendant_pid_file=$test_root/timeout-descendant-pid
timeout_start_epoch=$(monotonic_seconds)
set +e
SNOOZER_TEST_READY=$ready SNOOZER_TEST_RELEASE=$never_release \
    SNOOZER_TEST_IGNORE_TERM=1 SNOOZER_TEST_DESCENDANT_IGNORE_TERM=1 \
    SNOOZER_TEST_BENCHMARK_PID=$timeout_benchmark_pid_file \
    SNOOZER_TEST_DESCENDANT_PID=$timeout_descendant_pid_file runner_env "$runner" \
    --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 --producer-cpu 2 \
    --controller-cpu 3 --timeout-seconds 1 >/dev/null 2>&1
timeout_status=$?
set -e
[ "$timeout_status" -eq 137 ]
timeout_elapsed_seconds=$(($(monotonic_seconds) - timeout_start_epoch))
[ "$timeout_elapsed_seconds" -ge "$MIN_KILL_PATH_SECONDS" ]
[ "$timeout_elapsed_seconds" -le "$MAX_KILL_PATH_SECONDS" ]
[ -e "$timeout_benchmark_pid_file" ]
[ -e "$timeout_descendant_pid_file" ]
timeout_benchmark_pid=$(tr -d '[:space:]' <"$timeout_benchmark_pid_file")
timeout_descendant_pid=$(tr -d '[:space:]' <"$timeout_descendant_pid_file")
! kill -0 "$timeout_benchmark_pid" 2>/dev/null
! kill -0 "$timeout_descendant_pid" 2>/dev/null
assert_original
assert_clean

# SIGKILL while the benchmark is active leaves mutation ownership durable but
# not stuck in inherited FD 8/9. The guardian retains only active-run.lock,
# KILLs the inner group, proves it empty, then lets an alternate-dir recovery
# restore exact values without racing the old workload. When dash is available,
# this forces both the runner and guardian through dash's group-signal path.
crash_ready=$test_root/crash-ready
crash_never_release=$test_root/crash-never-release
crash_benchmark_pid_file=$test_root/crash-benchmark-pid
crash_descendant_pid_file=$test_root/crash-descendant-pid
crash_guardian_block=$test_root/crash-guardian-block
: >"$crash_guardian_block"
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$state_root" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    SNOOZER_TEST_DASH_BIN="$dash_bin" SNOOZER_TEST_DASH_SHELL="$dash_shell" \
    SNOOZER_TEST_RUNNER="$runner" \
    SNOOZER_TEST_READY="$crash_ready" SNOOZER_TEST_RELEASE="$crash_never_release" \
    SNOOZER_TEST_IGNORE_TERM=1 \
    SNOOZER_TEST_BENCHMARK_PID="$crash_benchmark_pid_file" \
    SNOOZER_TEST_DESCENDANT_PID="$crash_descendant_pid_file" \
    SNOOZER_TEST_DESCENDANT_IGNORE_TERM=1 \
    SNOOZER_TEST_POST_KILL_BLOCK_FILE="$crash_guardian_block" \
    "$dash_runner" --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 \
    --producer-cpu 2 --controller-cpu 3 --timeout-seconds 30 \
    >"$test_root/active-crash.out" 2>&1 &
crash_runner_pid=$!
attempt=0
while [ ! -s "$crash_benchmark_pid_file" ] \
    || [ ! -s "$crash_descendant_pid_file" ]; do
    [ "$attempt" -lt 500 ] \
        || { kill -KILL "$crash_runner_pid" 2>/dev/null || true; exit 1; }
    sleep 0.01
    attempt=$((attempt + 1))
done
crash_benchmark_pid=$(tr -d '[:space:]' <"$crash_benchmark_pid_file")
crash_descendant_pid=$(tr -d '[:space:]' <"$crash_descendant_pid_file")
for guardian_ready in "$state_root"/guardian-ready.*; do
    [ -s "$guardian_ready" ] || continue
    crash_guardian_pid=$(tr -d '[:space:]' <"$guardian_ready")
done
[ -n "${crash_guardian_pid:-}" ]
assert_guardian_lock_contract "$crash_guardian_pid"
assert_process_does_not_hold_runner_lock "$crash_benchmark_pid"
assert_process_does_not_hold_runner_lock "$crash_descendant_pid"
kill -KILL "$crash_runner_pid"
set +e
wait "$crash_runner_pid" 2>/dev/null
crash_runner_status=$?
set -e
[ "$crash_runner_status" -eq 137 ]

crash_recovery_state=$test_root/crash-recovery-state
crash_writes_before_recovery=$(wc -l <"$write_log")
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$crash_recovery_state" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    "$runner" --recover >"$test_root/active-crash-recovery.out" 2>&1 &
crash_recovery_pid=$!
sleep 0.1
if ! kill -0 "$crash_recovery_pid" 2>/dev/null; then
    set +e
    wait "$crash_recovery_pid"
    crash_recovery_early_status=$?
    set -e
    echo "active crash recovery exited early with status $crash_recovery_early_status" >&2
    cat "$test_root/active-crash-recovery.out" >&2
    exit 1
fi
[ "$(wc -l <"$write_log")" -eq "$crash_writes_before_recovery" ]
[ "$(tr -d '[:space:]' <"$sysfs_root/cpu0/cpuidle/state0/disable")" = 0 ]
rm "$crash_guardian_block"
wait "$crash_recovery_pid"
! kill -0 "$crash_benchmark_pid" 2>/dev/null
! kill -0 "$crash_descendant_pid" 2>/dev/null
assert_original
assert_clean

# SIGKILL leaves a global dirty-owner record. A retry using a different private
# state directory fails before writing, and --recover follows the global owner.
: >"$write_log"
set +e
SNOOZER_TEST_SIGNAL_AFTER_APPLY=KILL run_benchmark >"$test_root/kill.out" 2>&1
kill_status=$?
set -e
[ "$kill_status" -eq 137 ]
[ -f "$state_root/dirty" ]
[ -f "$sysfs_root/.snoozer-cpuidle.dirty" ]
killed_writes=$(wc -l <"$write_log")
killed_retry_state=$test_root/killed-retry-state
set +e
killed_retry_output=$(env SNOOZER_SYSFS_ROOT="$sysfs_root" \
    SNOOZER_STATE_DIR="$killed_retry_state" SNOOZER_WRITE_HELPER="$write_helper" \
    SNOOZER_TEST_WRITE_LOG="$write_log" "$runner" --binary "$benchmark" \
    --waiter-cpu 0 --victim-cpu 1 --producer-cpu 2 --controller-cpu 3 \
    --timeout-seconds 5 2>&1)
killed_retry_status=$?
set -e
[ "$killed_retry_status" -ne 0 ]
printf '%s\n' "$killed_retry_output" | grep -q 'unfinished global run detected'
[ ! -e "$killed_retry_state" ]
[ "$(wc -l <"$write_log")" -eq "$killed_writes" ]
env SNOOZER_SYSFS_ROOT="$sysfs_root" SNOOZER_STATE_DIR="$killed_retry_state" \
    SNOOZER_WRITE_HELPER="$write_helper" SNOOZER_TEST_WRITE_LOG="$write_log" \
    "$runner" --recover >/dev/null
assert_original
assert_clean

# A dirty retry fails before writing; explicit recovery is idempotent restoration.
mkdir -p "$state_root"
chmod 700 "$state_root"
manifest=$state_root/manifest.recovery
{
    printf 'version=SNOOZER_CPUIDLE_V2\n'
    printf 'sysfs_root=%s\n' "$sysfs_root"
    printf 'pid=999999\nuid=%s\nstarted_epoch=0\n' "$(id -u)"
    printf 'cpus=0,1,2,3\n'
    for cpu in 0 1 2 3; do
        for specification in 0:POLL:1:0 1:C1:1:0 2:C1E:0:1 3:C2:0:1 4:C3:0:1; do
            state=${specification%%:*}
            remainder=${specification#*:}
            name=${remainder%%:*}
            remainder=${remainder#*:}
            original=${remainder%%:*}
            desired=${remainder##*:}
            printf 'state|%s|%s|%s|%s|%s|%s\n' \
                "$sysfs_root/cpu$cpu/cpuidle/state$state/disable" \
                "$original" "$desired" "$name" "$cpu" "$state"
            printf '%s\n' "$desired" >"$sysfs_root/cpu$cpu/cpuidle/state$state/disable"
        done
    done
} >"$manifest"
chmod 600 "$manifest"
printf '%s\n' "$manifest" >"$state_root/dirty"
chmod 600 "$state_root/dirty"
: >"$write_log"

# A lexical traversal cannot pass as a direct-child recovery manifest, even if
# it starts with the canonical state-root prefix.
printf '%s\n' "$state_root/manifest.safe/../outside" >"$state_root/dirty"
set +e
traversal_output=$(runner_env "$runner" --recover 2>&1)
traversal_status=$?
set -e
[ "$traversal_status" -ne 0 ]
printf '%s\n' "$traversal_output" | grep -q 'not a direct child'
[ ! -s "$write_log" ]
printf '%s\n' "$manifest" >"$state_root/dirty"

# Recovery also rejects a symlinked CPU ancestor before its first restore write
# and retains the authoritative files for a later safe retry.
mv "$sysfs_root/cpu0" "$test_root/recovery-escaped-cpu0"
ln -s "$test_root/recovery-escaped-cpu0" "$sysfs_root/cpu0"
set +e
recovery_symlink_output=$(runner_env "$runner" --recover 2>&1)
recovery_symlink_status=$?
set -e
[ "$recovery_symlink_status" -ne 0 ]
printf '%s\n' "$recovery_symlink_output" | grep -q 'invalid state entry'
[ ! -s "$write_log" ]
[ -f "$state_root/dirty" ]
rm "$sysfs_root/cpu0"
mv "$test_root/recovery-escaped-cpu0" "$sysfs_root/cpu0"

set +e
retry_output=$(run_benchmark 2>&1)
retry_status=$?
set -e
[ "$retry_status" -ne 0 ]
printf '%s\n' "$retry_output" | grep -q 'unfinished run detected'
[ ! -s "$write_log" ]

# Recovery rechecks the recorded idle-state name before its first write, so a
# reused state index cannot restore a different C-state.
printf 'MWAIT\n' >"$sysfs_root/cpu0/cpuidle/state0/name"
set +e
name_output=$(runner_env "$runner" --recover 2>&1)
name_status=$?
set -e
[ "$name_status" -ne 0 ]
printf '%s\n' "$name_output" | grep -q 'invalid state entry'
[ ! -s "$write_log" ]
[ -f "$state_root/dirty" ]
printf 'POLL\n' >"$sysfs_root/cpu0/cpuidle/state0/name"

# A truncated manifest cannot restore a partial CPU state inventory.
cp "$manifest" "$test_root/complete-manifest"
sed '$d' "$test_root/complete-manifest" >"$manifest"
chmod 600 "$manifest"
set +e
inventory_output=$(runner_env "$runner" --recover 2>&1)
inventory_status=$?
set -e
[ "$inventory_status" -ne 0 ]
printf '%s\n' "$inventory_output" | grep -q 'does not exactly cover'
[ ! -s "$write_log" ]
cp "$test_root/complete-manifest" "$manifest"
chmod 600 "$manifest"
runner_env "$runner" --recover >/dev/null
assert_original
assert_clean

# The stable flock rejects concurrent mutation before a second write.
ready=$test_root/ready
release=$test_root/release
SNOOZER_TEST_READY=$ready SNOOZER_TEST_RELEASE=$release run_benchmark >"$test_root/first.out" 2>&1 &
first_pid=$!
attempt=0
while [ ! -e "$ready" ] && [ "$attempt" -lt 300 ]; do
    sleep 0.01
    attempt=$((attempt + 1))
done
[ -e "$ready" ] || { kill "$first_pid" 2>/dev/null || true; exit 1; }
set +e
concurrent_output=$(run_benchmark 2>&1)
concurrent_status=$?
set -e
[ "$concurrent_status" -ne 0 ]
printf '%s\n' "$concurrent_output" | grep -q 'another cpuidle runner is mutating'

# A different private state directory still contends on the sysfs-root lock.
second_state_root=$test_root/second-state
writes_before=$(wc -l <"$write_log")
set +e
global_output=$(env SNOOZER_SYSFS_ROOT="$sysfs_root" \
    SNOOZER_STATE_DIR="$second_state_root" SNOOZER_WRITE_HELPER="$write_helper" \
    SNOOZER_TEST_WRITE_LOG="$write_log" "$runner" --binary "$benchmark" \
    --waiter-cpu 0 --victim-cpu 1 --producer-cpu 2 --controller-cpu 3 \
    --timeout-seconds 5 2>&1)
global_status=$?
set -e
[ "$global_status" -ne 0 ]
printf '%s\n' "$global_output" | grep -q 'another cpuidle runner is mutating'
[ "$(wc -l <"$write_log")" -eq "$writes_before" ]
: >"$release"
wait "$first_pid"
assert_original
assert_clean

# Restore failure retains both authoritative recovery files for a later retry.
set +e
SNOOZER_TEST_FAIL_VALUE=1 SNOOZER_TEST_FAIL_PATH_SUFFIX=state0/disable \
    run_benchmark >"$test_root/restore-failure.out" 2>&1
restore_status=$?
set -e
[ "$restore_status" -ne 0 ]
[ -f "$state_root/dirty" ]
retained_manifest=$(tr -d '\r\n' <"$state_root/dirty")
[ -f "$retained_manifest" ]
grep -q 'RESTORE FAILED' "$test_root/restore-failure.out"
SNOOZER_TEST_FAIL_VALUE=__never__ SNOOZER_TEST_FAIL_PATH_SUFFIX=__never__ \
    runner_env "$runner" --recover >/dev/null
assert_original
assert_clean

# Unknown manifest versions fail closed without any sysfs write.
printf 'version=UNKNOWN\nsysfs_root=%s\n' "$sysfs_root" >"$manifest"
chmod 600 "$manifest"
printf '%s\n' "$manifest" >"$state_root/dirty"
chmod 600 "$state_root/dirty"
: >"$write_log"
set +e
version_output=$(runner_env "$runner" --recover 2>&1)
version_status=$?
set -e
[ "$version_status" -ne 0 ]
printf '%s\n' "$version_output" | grep -q 'unsupported recovery manifest version'
[ ! -s "$write_log" ]
rm -f "$state_root/dirty" "$manifest"

echo "cpuidle runner publication, process-group, symlink, recovery, and concurrency tests: PASS"
