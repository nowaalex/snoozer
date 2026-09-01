#!/bin/sh
# Run the wake-latency benchmark with an auditable, recoverable cpuidle change.
#
# Stateful-write contract (SNOOZER-CPUIDLE-1):
# - Caller-controlled inputs are the four CPU IDs, benchmark path/arguments,
#   timeout, sysfs root, and optional write helper. Original disable values are
#   derived from sysfs immediately before the first write. The manifest version,
#   process ID, UID, and start time are runner-owned provenance.
# - The stable command identity is the private state directory plus its exclusive
#   lock. A dirty marker names the only authoritative manifest. A retry never
#   reapplies while dirty: `--recover` restores exactly the recorded values first.
# - Recovery may rewrite only recorded `disable` files below the recorded sysfs
#   root. Unknown manifest versions, changed paths, bad ownership, and malformed
#   values fail closed without a write.
# - The manifest is durable before the marker is published. Normal exit, command
#   failure, timeout, HUP, INT, and TERM restore synchronously. SIGKILL cannot run
#   cleanup; the next operator uses `--recover`.
# - The benchmark has one outer timeout (default 900 seconds) and a five-second
#   TERM-to-KILL child grace period. There are no hidden intermediary timeouts.
set -eu

MANIFEST_VERSION=SNOOZER_CPUIDLE_V2
DEFAULT_TIMEOUT_SECONDS=900
KILL_GRACE_SECONDS=5

sysfs_root=${SNOOZER_SYSFS_ROOT:-/sys/devices/system/cpu}
command -v id >/dev/null 2>&1 || {
    echo "cpuidle runner: required command is unavailable: id" >&2
    exit 2
}
current_uid=$(id -u)
case "$current_uid" in ''|*[!0-9]*) echo "cpuidle runner: id returned an invalid UID" >&2; exit 2 ;; esac
state_root=${SNOOZER_STATE_DIR:-${XDG_RUNTIME_DIR:-/tmp}/snoozer-cpuidle-$current_uid}
write_helper=${SNOOZER_WRITE_HELPER:-}

usage() {
    usage_status=${1:-2}
    cat >&2 <<'EOF'
Usage:
  run_with_cpuidle.sh --binary PATH --waiter-cpu N --victim-cpu N \
    --producer-cpu N --controller-cpu N [--timeout-seconds N] [-- ARGS...]
  run_with_cpuidle.sh --recover

Only the selected CPUs are changed. POLL and exact C1 are enabled; C1E, C2,
C3, and every other state are disabled. Every original value is restored.
EOF
    exit "$usage_status"
}

die() {
    echo "cpuidle runner: $*" >&2
    exit 2
}

trim_trailing_slashes() {
    trimmed=$1
    while [ -n "$trimmed" ] && [ "${trimmed%/}" != "$trimmed" ]; do
        trimmed=${trimmed%/}
    done
    printf '%s\n' "$trimmed"
}

state_root=$(trim_trailing_slashes "$state_root")
sysfs_root=$(trim_trailing_slashes "$sysfs_root")
[ -n "$state_root" ] && [ "$state_root" != / ] || die "state directory must not be empty or root"
[ -n "$sysfs_root" ] && [ "$sysfs_root" != / ] || die "sysfs root must not be empty or root"
newline='
'
case "$state_root$sysfs_root" in
    *'|'*|*"$newline"*) die "state and sysfs paths must not contain a pipe or newline" ;;
esac

recover=0
binary=
waiter_cpu=
victim_cpu=
producer_cpu=
controller_cpu=
timeout_seconds=$DEFAULT_TIMEOUT_SECONDS

while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h)
            usage 0
            ;;
        --recover)
            [ "$recover" -eq 0 ] || usage
            recover=1
            shift
            ;;
        --binary)
            [ "$#" -ge 2 ] && [ -z "$binary" ] || usage
            binary=$2
            shift 2
            ;;
        --waiter-cpu)
            [ "$#" -ge 2 ] && [ -z "$waiter_cpu" ] || usage
            waiter_cpu=$2
            shift 2
            ;;
        --victim-cpu)
            [ "$#" -ge 2 ] && [ -z "$victim_cpu" ] || usage
            victim_cpu=$2
            shift 2
            ;;
        --producer-cpu)
            [ "$#" -ge 2 ] && [ -z "$producer_cpu" ] || usage
            producer_cpu=$2
            shift 2
            ;;
        --controller-cpu)
            [ "$#" -ge 2 ] && [ -z "$controller_cpu" ] || usage
            controller_cpu=$2
            shift 2
            ;;
        --timeout-seconds)
            [ "$#" -ge 2 ] || usage
            timeout_seconds=$2
            shift 2
            ;;
        --)
            shift
            break
            ;;
        *) usage ;;
    esac
done

case "$timeout_seconds" in
    ''|*[!0-9]*|0) die "timeout must be a positive integer number of seconds" ;;
esac

for required in awk date flock id mktemp ps realpath setsid sleep stat sync timeout; do
    command -v "$required" >/dev/null 2>&1 || die "required command is unavailable: $required"
done
if [ -n "$write_helper" ]; then
    [ -x "$write_helper" ] || die "configured write helper is not executable: $write_helper"
else
    command -v sudo >/dev/null 2>&1 || die "sudo is required when no write helper is configured"
fi

sysfs_root=$(realpath "$sysfs_root") || die "cannot canonicalize sysfs root"
[ "$sysfs_root" != / ] || die "sysfs root must not be root"

reject_symlink() {
    candidate=$1
    label=$2
    [ ! -L "$candidate" ] || die "$label must not be a symbolic link: $candidate"
}

validate_private_file() {
    candidate=$1
    label=$2
    reject_symlink "$candidate" "$label"
    [ -f "$candidate" ] || die "$label is not a regular file: $candidate"
    owner=$(stat -c '%u' "$candidate") || die "cannot inspect owner of $candidate"
    mode=$(stat -c '%a' "$candidate") || die "cannot inspect mode of $candidate"
    [ "$owner" = "$current_uid" ] && [ "$mode" = 600 ] \
        || die "$label must be owned by uid $current_uid with mode 600: $candidate"
}

umask 077
if [ "$sysfs_root" = /sys/devices/system/cpu ]; then
    global_lock_file=/run/lock/snoozer-cpuidle.lock
    if [ ! -e "$global_lock_file" ]; then
        sudo touch "$global_lock_file" || die "cannot create global cpuidle lock"
        sudo chown 0:0 "$global_lock_file" || die "cannot secure global cpuidle lock ownership"
        sudo chmod 0666 "$global_lock_file" || die "cannot make the global cpuidle lock shareable"
    fi
    reject_symlink "$global_lock_file" "global cpuidle lock"
    [ -f "$global_lock_file" ] \
        && [ "$(stat -c '%u' "$global_lock_file")" = 0 ] \
        && [ "$(stat -c '%a' "$global_lock_file")" = 666 ] \
        || die "global cpuidle lock must be a root-owned regular file with mode 666"
else
    # Fake/custom sysfs roots use a root-local lock so tests and simulations do
    # not contend with the host, while distinct state directories still share it.
    global_lock_file=$sysfs_root/.snoozer-cpuidle.lock
    reject_symlink "$global_lock_file" "custom-root cpuidle lock"
fi
exec 8>>"$global_lock_file"
reject_symlink "$global_lock_file" "global cpuidle lock"
flock -n 8 || die "another cpuidle runner is mutating this sysfs CPU tree"

reject_symlink "$state_root" "state directory"
if [ -e "$state_root" ]; then
    [ -d "$state_root" ] || die "state path is not a directory: $state_root"
else
    mkdir "$state_root" || die "cannot create state directory: $state_root"
fi
reject_symlink "$state_root" "state directory"
[ "$(stat -c '%u' "$state_root")" = "$current_uid" ] \
    && [ "$(stat -c '%a' "$state_root")" = 700 ] \
    || die "state directory must be owned by uid $current_uid with mode 700: $state_root"

lock_file=$state_root/runner.lock
dirty_marker=$state_root/dirty
reject_symlink "$lock_file" "lock file"
if [ -e "$lock_file" ]; then
    validate_private_file "$lock_file" "lock file"
fi
exec 9>>"$lock_file"
validate_private_file "$lock_file" "lock file"
flock -n 9 || die "another cpuidle runner holds $lock_file"

write_and_verify() {
    target=$1
    expected=$2
    if [ -n "$write_helper" ]; then
        printf '%s\n' "$expected" | "$write_helper" "$target" >/dev/null \
            || return 1
    else
        printf '%s\n' "$expected" | sudo tee "$target" >/dev/null \
            || return 1
    fi
    actual=$(tr -d '[:space:]' <"$target") || return 1
    [ "$actual" = "$expected" ]
}

validate_manifest_entry() {
    entry_path=$1
    original=$2
    desired=$3
    entry_name=$4
    entry_cpu=$5
    entry_state=$6
    case "$original:$desired" in
        0:0|0:1|1:0|1:1) ;;
        *) return 1 ;;
    esac
    case "$entry_cpu:$entry_state" in
        *[!0-9:]*|:*|*:) return 1 ;;
    esac
    expected_path=$sysfs_root/cpu$entry_cpu/cpuidle/state$entry_state/disable
    [ "$entry_path" = "$expected_path" ] || return 1
    case "$entry_name" in ''|*'|'*|*"$newline"*) return 1 ;; esac
    state_directory=${entry_path%/disable}
    reject_symlink "$state_directory" "cpuidle state directory"
    reject_symlink "$state_directory/name" "cpuidle state name"
    reject_symlink "$entry_path" "cpuidle disable file"
    [ -f "$state_directory/name" ] && [ -f "$entry_path" ] || return 1
    current_name=$(tr '[:lower:]' '[:upper:]' <"$state_directory/name" | tr -d '\r\n') \
        || return 1
    [ "$current_name" = "$entry_name" ]
}

manifest_from_marker() {
    reject_symlink "$dirty_marker" "dirty marker"
    validate_private_file "$dirty_marker" "dirty marker"
    manifest=$(tr -d '\r\n' <"$dirty_marker") || die "cannot read dirty marker"
    [ -n "$manifest" ] || die "dirty marker is empty"
    case "$manifest" in
        "$state_root"/manifest.*) ;;
        *) die "dirty marker points outside the private state directory" ;;
    esac
    validate_private_file "$manifest" "recovery manifest"
    printf '%s\n' "$manifest"
}

check_manifest_header() {
    checked_manifest=$1
    version=$(sed -n '1s/^version=//p' "$checked_manifest")
    recorded_root=$(sed -n '2s/^sysfs_root=//p' "$checked_manifest")
    [ "$version" = "$MANIFEST_VERSION" ] || die "unsupported recovery manifest version: ${version:-missing}"
    [ "$recorded_root" = "$sysfs_root" ] || die "recovery sysfs root differs from the recorded root"
}

validate_manifest() {
    validated_manifest=$1
    check_manifest_header "$validated_manifest"
    pid_value=$(sed -n '3s/^pid=//p' "$validated_manifest")
    uid_value=$(sed -n '4s/^uid=//p' "$validated_manifest")
    time_value=$(sed -n '5s/^started_epoch=//p' "$validated_manifest")
    recorded_cpus=$(sed -n '6s/^cpus=//p' "$validated_manifest")
    case "$pid_value:$uid_value:$time_value" in
        *[!0-9:]*|:*|*:) die "recovery manifest has invalid provenance" ;;
    esac
    [ -n "$pid_value" ] && [ -n "$uid_value" ] && [ -n "$time_value" ] \
        || die "recovery manifest has incomplete provenance"
    if ! printf '%s\n' "$recorded_cpus" | awk -F ',' '
        NF != 4 { exit 1 }
        {
            for (field_index = 1; field_index <= NF; field_index++) {
                if ($field_index !~ /^[0-9]+$/ || seen[$field_index]++) exit 1
            }
        }
    '; then
        die "recovery manifest must name four distinct numeric CPUs"
    fi
    if ! awk -F '|' '
        NR <= 6 { next }
        NF != 7 || $1 != "state" || seen[$2]++ { exit 1 }
        END { if (NR <= 6) exit 1 }
    ' "$validated_manifest"; then
        die "recovery manifest has malformed or duplicate state entries"
    fi
    while IFS='|' read -r kind path original desired name cpu state; do
        [ "$kind" = state ] || continue
        case ",$recorded_cpus," in
            *",$cpu,"*) ;;
            *) die "recovery manifest contains an unselected CPU" ;;
        esac
        validate_manifest_entry "$path" "$original" "$desired" "$name" "$cpu" "$state" \
            || die "recovery manifest has an invalid state entry"
    done <"$validated_manifest"
    previous_ifs=$IFS
    IFS=,
    for cpu in $recorded_cpus; do
        found_inventory_state=0
        for state_directory in "$sysfs_root/cpu$cpu/cpuidle"/state*; do
            [ -d "$state_directory" ] || continue
            found_inventory_state=1
            state=${state_directory##*state}
            case "$state" in ''|*[!0-9]*) die "invalid current cpuidle state directory" ;; esac
            inventory_path=$state_directory/disable
            if ! awk -F '|' -v expected="$inventory_path" '
                $1 == "state" && $2 == expected { count++ }
                END { exit count != 1 }
            ' "$validated_manifest"; then
                die "recovery manifest does not exactly cover the current CPU state inventory"
            fi
        done
        [ "$found_inventory_state" -eq 1 ] \
            || die "selected CPU exposes no current cpuidle states during recovery"
    done
    IFS=$previous_ifs
}

restore_manifest() {
    restore_manifest_path=$1
    restore_failed=0
    while IFS='|' read -r kind path original desired name cpu state; do
        [ "$kind" = state ] || continue
        if ! validate_manifest_entry "$path" "$original" "$desired" "$name" "$cpu" "$state"; then
            echo "cpuidle runner: invalid recovery entry for CPU $cpu state$state" >&2
            restore_failed=1
            continue
        fi
        if ! write_and_verify "$path" "$original"; then
            echo "cpuidle runner: failed to restore CPU $cpu state$state ($name) to $original" >&2
            restore_failed=1
        fi
    done <"$restore_manifest_path"
    [ "$restore_failed" -eq 0 ]
}

finish_recovery() {
    recovered_manifest=$1
    current_manifest=$(manifest_from_marker)
    [ "$current_manifest" = "$recovered_manifest" ] || die "dirty marker changed during recovery"
    rm -f "$dirty_marker" || die "restored state but could not remove dirty marker"
    sync -f "$state_root" || die "restored state but could not persist marker removal"
    rm -f "$recovered_manifest" || die "restored state but could not remove manifest"
}

reject_symlink "$dirty_marker" "dirty marker"
if [ "$recover" -eq 1 ]; then
    [ -z "$binary$waiter_cpu$victim_cpu$producer_cpu$controller_cpu" ] && [ "$#" -eq 0 ] || usage
    [ -e "$dirty_marker" ] || die "there is no dirty cpuidle run to recover"
    recovery_manifest=$(manifest_from_marker)
    validate_manifest "$recovery_manifest"
    if ! restore_manifest "$recovery_manifest"; then
        echo "cpuidle runner: RECOVERY FAILED; marker and manifest are retained" >&2
        exit 1
    fi
    finish_recovery "$recovery_manifest"
    echo "cpuidle runner: recovery complete"
    exit 0
fi

[ ! -e "$dirty_marker" ] || {
    stale_manifest=$(manifest_from_marker)
    echo "cpuidle runner: unfinished run detected; no write was attempted" >&2
    echo "cpuidle runner: recover with SNOOZER_STATE_DIR='$state_root' SNOOZER_SYSFS_ROOT='$sysfs_root' $0 --recover" >&2
    echo "cpuidle runner: authoritative manifest: $stale_manifest" >&2
    exit 2
}

[ -n "$binary" ] && [ -x "$binary" ] || usage
for cpu in "$waiter_cpu" "$victim_cpu" "$producer_cpu" "$controller_cpu"; do
    case "$cpu" in ''|*[!0-9]*) usage ;; esac
    [ -d "$sysfs_root/cpu$cpu/cpuidle" ] || die "CPU $cpu has no cpuidle directory"
done
[ "$waiter_cpu" != "$victim_cpu" ] && [ "$waiter_cpu" != "$producer_cpu" ] \
    && [ "$waiter_cpu" != "$controller_cpu" ] && [ "$victim_cpu" != "$producer_cpu" ] \
    && [ "$victim_cpu" != "$controller_cpu" ] && [ "$producer_cpu" != "$controller_cpu" ] \
    || die "all four CPU roles must be distinct"

for forwarded in "$@"; do
    case "$forwarded" in
        --waiter-cpu|--waiter-cpu=*|--victim-cpu|--victim-cpu=*|--producer-cpu|--producer-cpu=*|--controller-cpu|--controller-cpu=*)
            die "CPU role arguments are owned by the runner and cannot be forwarded"
            ;;
    esac
done

manifest=$(mktemp "$state_root/manifest.XXXXXX")
cleanup_manifest=1
restored=0
child_pid=
signal_status=

cleanup() {
    original_status=$?
    trap - EXIT HUP INT TERM
    if [ -e "$dirty_marker" ]; then
        current_manifest=$(manifest_from_marker)
        if [ "$current_manifest" != "$manifest" ]; then
            echo "cpuidle runner: dirty marker changed; refusing automatic restore" >&2
            exit 1
        fi
        if ! restore_manifest "$manifest"; then
            echo "cpuidle runner: RESTORE FAILED; marker and manifest are retained" >&2
            exit 1
        fi
        restored=1
        finish_recovery "$manifest"
        cleanup_manifest=0
    fi
    if [ "$cleanup_manifest" -eq 1 ]; then
        rm -f "$manifest"
    fi
    if [ -n "$signal_status" ]; then
        exit "$signal_status"
    fi
    exit "$original_status"
}

handle_signal() {
    signal_status=$1
    if [ -n "$child_pid" ]; then
        terminate_child "$child_pid"
        child_pid=
    fi
    exit "$signal_status"
}

child_tree_alive() {
    inspected_pid=$1
    kill -0 "$inspected_pid" 2>/dev/null || kill -0 -- "-$inspected_pid" 2>/dev/null
}

signal_child_tree() {
    child_signal=$1
    inspected_pid=$2
    kill -"$child_signal" -- "-$inspected_pid" 2>/dev/null \
        || kill -"$child_signal" "$inspected_pid" 2>/dev/null \
        || true
}

terminate_child() {
    inspected_pid=$1
    signal_child_tree TERM "$inspected_pid"
    termination_deadline=$(($(date +%s) + KILL_GRACE_SECONDS))
    while child_tree_alive "$inspected_pid" \
        && [ "$(date +%s)" -lt "$termination_deadline" ]; do
        sleep 0.05
    done
    if child_tree_alive "$inspected_pid"; then
        signal_child_tree KILL "$inspected_pid"
    fi
    wait "$inspected_pid" 2>/dev/null || true
}

verify_child_process_group() {
    inspected_pid=$1
    group_attempt=0
    while [ "$group_attempt" -lt 100 ]; do
        group=$(ps -o pgid= -p "$inspected_pid" 2>/dev/null | tr -d '[:space:]') || group=
        [ "$group" = "$inspected_pid" ] && return 0
        if ! kill -0 "$inspected_pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.01
        group_attempt=$((group_attempt + 1))
    done
    signal_child_tree KILL "$inspected_pid"
    die "benchmark supervisor did not acquire a dedicated process group"
}

trap cleanup EXIT
trap 'handle_signal 129' HUP
trap 'handle_signal 130' INT
trap 'handle_signal 143' TERM

started_epoch=$(date +%s)
{
    printf 'version=%s\n' "$MANIFEST_VERSION"
    printf 'sysfs_root=%s\n' "$sysfs_root"
    printf 'pid=%s\n' "$$"
    printf 'uid=%s\n' "$current_uid"
    printf 'started_epoch=%s\n' "$started_epoch"
    printf 'cpus=%s,%s,%s,%s\n' \
        "$waiter_cpu" "$victim_cpu" "$producer_cpu" "$controller_cpu"
} >"$manifest"

recorded_states=0
for cpu in "$waiter_cpu" "$victim_cpu" "$producer_cpu" "$controller_cpu"; do
    poll_count=0
    c1_count=0
    found_state=0
    for state_dir in "$sysfs_root/cpu$cpu/cpuidle"/state*; do
        [ -d "$state_dir" ] || continue
        found_state=1
        reject_symlink "$state_dir" "cpuidle state directory"
        state=${state_dir##*state}
        case "$state" in ''|*[!0-9]*) die "invalid cpuidle state directory: $state_dir" ;; esac
        name=$(tr '[:lower:]' '[:upper:]' <"$state_dir/name" | tr -d '\r\n') \
            || die "cannot read state name: $state_dir"
        original=$(tr -d '[:space:]' <"$state_dir/disable") \
            || die "cannot read disable value: $state_dir"
        case "$original" in 0|1) ;; *) die "invalid disable value '$original': $state_dir" ;; esac
        case "$name" in
            POLL) poll_count=$((poll_count + 1)); desired=0 ;;
            C1) c1_count=$((c1_count + 1)); desired=0 ;;
            *) desired=1 ;;
        esac
        printf 'state|%s|%s|%s|%s|%s|%s\n' \
            "$state_dir/disable" "$original" "$desired" "$name" "$cpu" "$state" >>"$manifest"
        recorded_states=$((recorded_states + 1))
    done
    [ "$found_state" -eq 1 ] || die "CPU $cpu exposes no cpuidle states"
    [ "$poll_count" -eq 1 ] && [ "$c1_count" -eq 1 ] \
        || die "CPU $cpu must expose exactly one POLL and one exact C1 state"
done
[ "$recorded_states" -gt 0 ] || die "no cpuidle states were recorded"
sync -f "$manifest" || die "cannot persist recovery manifest"
validate_manifest "$manifest"

marker_candidate=$(mktemp "$state_root/dirty.XXXXXX")
printf '%s\n' "$manifest" >"$marker_candidate"
sync -f "$marker_candidate" || die "cannot persist dirty marker candidate"
ln "$marker_candidate" "$dirty_marker" || die "cannot atomically publish dirty marker"
rm -f "$marker_candidate"
sync -f "$state_root" || die "cannot persist dirty marker"
echo "cpuidle runner: recovery manifest $manifest"

while IFS='|' read -r kind path original desired name cpu state; do
    [ "$kind" = state ] || continue
    validate_manifest_entry "$path" "$original" "$desired" "$name" "$cpu" "$state" \
        || die "manifest validation failed before apply"
    if ! write_and_verify "$path" "$desired"; then
        echo "cpuidle runner: failed to apply CPU $cpu state$state ($name)=$desired" >&2
        exit 1
    fi
done <"$manifest"

if [ "${SNOOZER_TEST_SIGNAL_AFTER_APPLY:-}" = TERM ]; then
    kill -TERM "$$"
fi

echo "C2/C3 and every deeper CPU idle state are disabled because their exit latency conflicts with the minimum-wake-latency objective. These results do not represent the default power-saving configuration."

set +e
setsid timeout --foreground --signal=TERM --kill-after="${KILL_GRACE_SECONDS}s" "$timeout_seconds" \
    "$binary" \
    --waiter-cpu "$waiter_cpu" \
    --victim-cpu "$victim_cpu" \
    --producer-cpu "$producer_cpu" \
    --controller-cpu "$controller_cpu" \
    "$@" &
child_pid=$!
verify_child_process_group "$child_pid"
wait "$child_pid"
command_status=$?
child_pid=
set -e
exit "$command_status"
