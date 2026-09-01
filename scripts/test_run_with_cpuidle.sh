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
printf '%s\n' "$value" >"$target"
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
    : >"$SNOOZER_TEST_READY"
    while [ ! -e "$SNOOZER_TEST_RELEASE" ]; do sleep 0.01; done
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
    for manifest in "$state_root"/manifest.*; do
        [ ! -e "$manifest" ] || return 1
    done
}

# First apply enables exact POLL/C1, disables every other state, then restores.
: >"$write_log"
run_benchmark >/dev/null
assert_original
assert_clean
grep -q '|.*/state0/disable$' "$write_log"
grep -q '|.*/state4/disable$' "$write_log"
! grep -q '/cpu4/' "$write_log"
[ "$(tr -d '[:space:]' <"$sysfs_root/cpu4/cpuidle/state0/disable")" = 0 ]

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

# The outer timeout terminates the workload and still restores exact state.
ready=$test_root/timeout-ready
never_release=$test_root/timeout-never-release
set +e
SNOOZER_TEST_READY=$ready SNOOZER_TEST_RELEASE=$never_release runner_env "$runner" \
    --binary "$benchmark" --waiter-cpu 0 --victim-cpu 1 --producer-cpu 2 \
    --controller-cpu 3 --timeout-seconds 1 >/dev/null 2>&1
timeout_status=$?
set -e
[ "$timeout_status" -eq 124 ]
assert_original
assert_clean

# A dirty retry fails before writing; explicit recovery is idempotent restoration.
mkdir -p "$state_root"
chmod 700 "$state_root"
manifest=$state_root/manifest.recovery
{
    printf 'version=SNOOZER_CPUIDLE_V1\n'
    printf 'sysfs_root=%s\n' "$sysfs_root"
    printf 'pid=999999\nuid=%s\nstarted_epoch=0\n' "$(id -u)"
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
set +e
retry_output=$(run_benchmark 2>&1)
retry_status=$?
set -e
[ "$retry_status" -ne 0 ]
printf '%s\n' "$retry_output" | grep -q 'unfinished run detected'
[ ! -s "$write_log" ]
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
printf '%s\n' "$concurrent_output" | grep -q 'another cpuidle runner holds'
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

echo "cpuidle runner first-apply, retry, signal, restore-failure, and concurrency tests: PASS"
