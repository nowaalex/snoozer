#!/bin/sh
# Run the wake-latency benchmark with an auditable, recoverable cpuidle change.
#
# Stateful-write contract (SNOOZER-CPUIDLE-1):
# - Caller-controlled inputs are the four CPU IDs, benchmark path/arguments,
#   timeout, sysfs root, and optional write helper. Original disable values are
#   derived from sysfs immediately before the first write. The manifest version,
#   process ID, UID, and start time are runner-owned provenance.
# - The stable command identity is the canonical sysfs CPU tree plus its global
#   lock. A global dirty-owner record names the canonical private state directory
#   and authoritative manifest. A retry from any state directory never reapplies
#   while dirty: `--recover` restores exactly the recorded values first.
# - Recovery may rewrite only recorded `disable` files below the recorded sysfs
#   root. Unknown manifest versions, changed paths, bad ownership, and malformed
#   values fail closed without a write.
# - The manifest is durable before the marker is published. Normal exit, command
#   failure, timeout, HUP, INT, and TERM restore synchronously. SIGKILL cannot run
#   cleanup; the next operator uses `--recover`.
# - The benchmark has one outer timeout (default 900 seconds) and one five-second
#   TERM-to-KILL grace period. After KILL, a separate crash guardian must prove
#   the group empty before cpuidle restoration; inspection failure waits safely
#   with the active lock held instead of restoring under a possible survivor.
set -eu

MANIFEST_VERSION=SNOOZER_CPUIDLE_V2
GLOBAL_DIRTY_VERSION=SNOOZER_GLOBAL_DIRTY_V1
DEFAULT_TIMEOUT_SECONDS=900
KILL_GRACE_SECONDS=5
KILL_GRACE_POLLS=$((KILL_GRACE_SECONDS * 20))
KILL_GRACE_MILLISECONDS=$((KILL_GRACE_SECONDS * 1000))
ACTIVE_RECOVERY_WAIT_SECONDS=10
SUPERVISOR_STARTUP_POLLS=500

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
    command -v sudo >/dev/null 2>&1 \
        || die "sudo is required for the global real-sysfs recovery record"
    global_lock_file=/run/lock/snoozer-cpuidle.lock
    global_dirty_marker=/run/lock/snoozer-cpuidle.dirty
    global_marker_directory=/run/lock
    global_marker_privileged=1
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
    global_dirty_marker=$sysfs_root/.snoozer-cpuidle.dirty
    global_marker_directory=$sysfs_root
    global_marker_privileged=0
    reject_symlink "$global_lock_file" "custom-root cpuidle lock"
fi
exec 8>>"$global_lock_file"
reject_symlink "$global_lock_file" "global cpuidle lock"
flock -n 8 || die "another cpuidle runner is mutating this sysfs CPU tree"

boot_id=$(tr -d '[:space:]' </proc/sys/kernel/random/boot_id) \
    || die "cannot read the Linux boot identity"
case "$boot_id" in
    ''|*[!0-9a-fA-F-]*) die "Linux boot identity is malformed" ;;
esac

validate_global_dirty_file() {
    reject_symlink "$global_dirty_marker" "global dirty-owner record"
    [ -f "$global_dirty_marker" ] \
        || die "global dirty-owner record is not a regular file"
    marker_owner=$(stat -c '%u' "$global_dirty_marker") \
        || die "cannot inspect global dirty-owner record ownership"
    marker_mode=$(stat -c '%a' "$global_dirty_marker") \
        || die "cannot inspect global dirty-owner record mode"
    if [ "$global_marker_privileged" -eq 1 ]; then
        [ "$marker_owner" = 0 ] && [ "$marker_mode" = 600 ] \
            || die "global dirty-owner record must be root-owned with mode 600"
    else
        [ "$marker_owner" = "$current_uid" ] && [ "$marker_mode" = 600 ] \
            || die "global dirty-owner record must be owned by uid $current_uid with mode 600"
    fi
}

read_global_dirty_record() {
    validate_global_dirty_file
    if [ "$global_marker_privileged" -eq 1 ]; then
        sudo cat "$global_dirty_marker"
    else
        cat "$global_dirty_marker"
    fi
}

validate_global_dirty_record() {
    global_record=$1
    if ! printf '%s\n' "$global_record" | awk '
        NR == 1 && $0 !~ /^version=/ { exit 1 }
        NR == 2 && $0 !~ /^sysfs_root=/ { exit 1 }
        NR == 3 && $0 !~ /^state_root=/ { exit 1 }
        NR == 4 && $0 !~ /^manifest=/ { exit 1 }
        NR == 5 && $0 !~ /^uid=/ { exit 1 }
        NR == 6 && $0 !~ /^boot_id=/ { exit 1 }
        END { if (NR != 6) exit 1 }
    '; then
        die "global dirty-owner record is malformed"
    fi
    recorded_global_version=$(printf '%s\n' "$global_record" | sed -n '1s/^version=//p')
    recorded_global_sysfs=$(printf '%s\n' "$global_record" | sed -n '2s/^sysfs_root=//p')
    recorded_global_state=$(printf '%s\n' "$global_record" | sed -n '3s/^state_root=//p')
    recorded_global_manifest=$(printf '%s\n' "$global_record" | sed -n '4s/^manifest=//p')
    recorded_global_uid=$(printf '%s\n' "$global_record" | sed -n '5s/^uid=//p')
    recorded_global_boot=$(printf '%s\n' "$global_record" | sed -n '6s/^boot_id=//p')
    [ "$recorded_global_version" = "$GLOBAL_DIRTY_VERSION" ] \
        || die "unsupported global dirty-owner version: ${recorded_global_version:-missing}"
    [ "$recorded_global_sysfs" = "$sysfs_root" ] \
        || die "global dirty-owner sysfs root differs from the selected root"
    [ "$recorded_global_uid" = "$current_uid" ] \
        || die "global dirty run must be recovered as uid $recorded_global_uid"
    [ "$recorded_global_boot" = "$boot_id" ] \
        || die "global dirty-owner record belongs to a different Linux boot"
    case "$recorded_global_state$recorded_global_manifest" in
        ''|*'|'*|*"$newline"*) die "global dirty-owner paths are malformed" ;;
    esac
    [ "${recorded_global_state#/}" != "$recorded_global_state" ] \
        || die "global dirty-owner state directory is not absolute"
    [ "${recorded_global_manifest#/}" != "$recorded_global_manifest" ] \
        || die "global dirty-owner manifest is not absolute"
}

global_record=
global_marker_candidate=
if [ -e "$global_dirty_marker" ]; then
    global_record=$(read_global_dirty_record) \
        || die "cannot read global dirty-owner record"
    validate_global_dirty_record "$global_record"
    if [ "$recover" -eq 0 ]; then
        echo "cpuidle runner: unfinished global run detected; no write was attempted" >&2
        echo "cpuidle runner: recover as uid $recorded_global_uid with SNOOZER_SYSFS_ROOT='$sysfs_root' $0 --recover" >&2
        echo "cpuidle runner: authoritative manifest: $recorded_global_manifest" >&2
        exit 2
    fi
    state_root=$recorded_global_state
fi

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
state_root=$(realpath "$state_root") || die "cannot canonicalize state directory"
[ "$state_root" != / ] || die "state directory must not be root"
if [ -n "$global_record" ]; then
    [ "$state_root" = "$recorded_global_state" ] \
        || die "global dirty-owner state directory is not canonical"
fi

lock_file=$state_root/runner.lock
dirty_marker=$state_root/dirty
reject_symlink "$lock_file" "lock file"
if [ -e "$lock_file" ]; then
    validate_private_file "$lock_file" "lock file"
fi
exec 9>>"$lock_file"
validate_private_file "$lock_file" "lock file"
flock -n 9 || die "another cpuidle runner holds $lock_file"

# This third, stable lock belongs to the crash guardian while a benchmark may
# still be alive. Unlike the mutation locks, it is intentionally inherited by
# the guardian and explicitly closed by the benchmark process group.
active_lock_file=$state_root/active-run.lock
reject_symlink "$active_lock_file" "active-run lock file"
if [ -e "$active_lock_file" ]; then
    validate_private_file "$active_lock_file" "active-run lock file"
fi
exec 7>>"$active_lock_file"
validate_private_file "$active_lock_file" "active-run lock file"

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
    canonical_state_directory=$(realpath "$state_directory") || return 1
    [ "$canonical_state_directory" = "$state_directory" ] || return 1
    reject_symlink "$state_directory" "cpuidle state directory"
    reject_symlink "$state_directory/name" "cpuidle state name"
    reject_symlink "$entry_path" "cpuidle disable file"
    [ -f "$state_directory/name" ] && [ -f "$entry_path" ] || return 1
    IFS= read -r current_name <"$state_directory/name" || return 1
    [ "$current_name" = "$entry_name" ]
}

validate_manifest_location() {
    inspected_manifest=$1
    manifest_directory=${inspected_manifest%/*}
    manifest_name=${inspected_manifest##*/}
    [ "$manifest_directory" = "$state_root" ] \
        || die "recovery manifest is not a direct child of the private state directory"
    case "$manifest_name" in
        manifest.*) ;;
        *) die "recovery manifest has an invalid name" ;;
    esac
    manifest_suffix=${manifest_name#manifest.}
    case "$manifest_suffix" in
        ''|*[!0-9A-Za-z]*) die "recovery manifest has an invalid name" ;;
    esac
}

manifest_from_marker() {
    reject_symlink "$dirty_marker" "dirty marker"
    validate_private_file "$dirty_marker" "dirty marker"
    manifest=$(tr -d '\r\n' <"$dirty_marker") || die "cannot read dirty marker"
    [ -n "$manifest" ] || die "dirty marker is empty"
    validate_manifest_location "$manifest"
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

validate_manifest_inventory() {
    inventory_manifest=$1
    inventory_cpus=$(sed -n '6s/^cpus=//p' "$inventory_manifest")
    inventory_paths=
    previous_ifs=$IFS
    IFS=,
    for inventory_cpu in $inventory_cpus; do
        found_inventory_state=0
        for state_directory in "$sysfs_root/cpu$inventory_cpu/cpuidle"/state*; do
            [ -d "$state_directory" ] || continue
            found_inventory_state=1
            inventory_state=${state_directory##*state}
            case "$inventory_state" in
                ''|*[!0-9]*) die "invalid current cpuidle state directory" ;;
            esac
            inventory_paths="${inventory_paths}${state_directory}/disable
"
        done
        [ "$found_inventory_state" -eq 1 ] \
            || die "selected CPU exposes no current cpuidle states during recovery"
    done
    IFS=$previous_ifs
    if ! awk -F '|' -v inventory="$inventory_paths" '
        BEGIN {
            count = split(inventory, path, "\n")
            for (inventory_index = 1; inventory_index <= count; inventory_index++) {
                if (path[inventory_index] != "") current[path[inventory_index]]++
            }
        }
        $1 == "state" { recorded[$2]++ }
        END {
            for (candidate in current) {
                if (current[candidate] != 1 || recorded[candidate] != 1) exit 1
            }
            for (candidate in recorded) {
                if (recorded[candidate] != 1 || current[candidate] != 1) exit 1
            }
        }
    ' "$inventory_manifest"; then
        die "recovery manifest does not exactly cover the current CPU state inventory"
    fi
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
    validate_manifest_inventory "$validated_manifest"
}

restore_manifest() {
    restore_manifest_path=$1
    restore_failed=0
    # Recheck the complete state inventory before restoration and again before
    # each write. A hotplugged/reindexed state must retain the recovery records
    # instead of allowing a partial restore to be mistaken for exact recovery.
    validate_manifest "$restore_manifest_path"
    while IFS='|' read -r kind path original desired name cpu state; do
        [ "$kind" = state ] || continue
        restore_path=$path
        restore_original=$original
        restore_desired=$desired
        restore_name=$name
        restore_cpu=$cpu
        restore_state=$state
        validate_manifest_inventory "$restore_manifest_path"
        if ! validate_manifest_entry "$restore_path" "$restore_original" "$restore_desired" \
            "$restore_name" "$restore_cpu" "$restore_state"; then
            echo "cpuidle runner: invalid recovery entry for CPU $restore_cpu state$restore_state" >&2
            restore_failed=1
            continue
        fi
        if ! write_and_verify "$restore_path" "$restore_original"; then
            echo "cpuidle runner: failed to restore CPU $restore_cpu state$restore_state ($restore_name) to $restore_original" >&2
            restore_failed=1
        fi
    done <"$restore_manifest_path"
    [ "$restore_failed" -eq 0 ]
}

remove_global_marker_candidate() {
    [ -n "$global_marker_candidate" ] || return 0
    if [ "$global_marker_privileged" -eq 1 ]; then
        sudo rm -f "$global_marker_candidate" || return 1
    else
        rm -f "$global_marker_candidate" || return 1
    fi
    global_marker_candidate=
}

publish_global_dirty_record() {
    published_manifest=$1
    validate_manifest_location "$published_manifest"
    [ ! -e "$global_dirty_marker" ] \
        || die "global dirty-owner record appeared while the lock was held"
    published_record="version=$GLOBAL_DIRTY_VERSION
sysfs_root=$sysfs_root
state_root=$state_root
manifest=$published_manifest
uid=$current_uid
boot_id=$boot_id"
    if [ "$global_marker_privileged" -eq 1 ]; then
        global_marker_candidate=$(sudo mktemp \
            "$global_marker_directory/snoozer-cpuidle.dirty.XXXXXX") || return 1
        printf '%s\n' "$published_record" | sudo tee "$global_marker_candidate" >/dev/null \
            || return 1
        sudo chown 0:0 "$global_marker_candidate" || return 1
        sudo chmod 0600 "$global_marker_candidate" || return 1
        sudo sync -f "$global_marker_candidate" || return 1
        sudo ln "$global_marker_candidate" "$global_dirty_marker" || return 1
    else
        global_marker_candidate=$(mktemp \
            "$global_marker_directory/.snoozer-cpuidle.dirty.XXXXXX") || return 1
        printf '%s\n' "$published_record" >"$global_marker_candidate" || return 1
        sync -f "$global_marker_candidate" || return 1
        ln "$global_marker_candidate" "$global_dirty_marker" || return 1
    fi
    case "${SNOOZER_TEST_SIGNAL_AFTER_GLOBAL_LINK:-}" in
        '') ;;
        TERM) kill -TERM "$$" ;;
        *) return 1 ;;
    esac
    # From the instant the atomic link succeeds, cleanup must treat the global
    # record as authoritative even if candidate removal, sync, or read-back
    # fails. Keeping the trusted in-memory record prevents a dangling global
    # pointer to a manifest that cleanup mistakenly deletes.
    global_record=$published_record
    validate_global_dirty_record "$global_record"
    if [ "${SNOOZER_TEST_FAIL_AFTER_GLOBAL_LINK:-}" = 1 ]; then
        return 1
    fi
    remove_global_marker_candidate || return 1
    if [ "$global_marker_privileged" -eq 1 ]; then
        sudo sync -f "$global_marker_directory" || return 1
    else
        sync -f "$global_marker_directory" || return 1
    fi
    readback_global_record=$(read_global_dirty_record) || return 1
    [ "$readback_global_record" = "$global_record" ] || return 1
}

clear_global_dirty_record() {
    [ -e "$global_dirty_marker" ] || die "global dirty-owner record disappeared"
    current_global_record=$(read_global_dirty_record) \
        || die "cannot read global dirty-owner record before removal"
    [ "$current_global_record" = "$global_record" ] \
        || die "global dirty-owner record changed; refusing removal"
    if [ "$global_marker_privileged" -eq 1 ]; then
        sudo rm "$global_dirty_marker" \
            || die "restored state but could not remove global dirty-owner record"
        sudo sync -f "$global_marker_directory" \
            || die "restored state but could not persist global marker removal"
    else
        rm "$global_dirty_marker" \
            || die "restored state but could not remove global dirty-owner record"
        sync -f "$global_marker_directory" \
            || die "restored state but could not persist global marker removal"
    fi
    global_record=
}

remove_stale_runtime_files() {
    for runtime_file in "$state_root"/supervisor-status.* \
        "$state_root"/supervisor-ready.* "$state_root"/supervisor-go.* \
        "$state_root"/guardian-ready.* "$state_root"/guardian-release.* \
        "$state_root"/guardian-group.* "$state_root"/guardian-proof-request.* \
        "$state_root"/guardian-proof-ready.*; do
        if [ -e "$runtime_file" ] || [ -L "$runtime_file" ]; then
            rm -f "$runtime_file" || die "cannot remove stale benchmark runtime file"
        fi
    done
}

finish_recovery() {
    recovered_manifest=$1
    if [ -e "$dirty_marker" ]; then
        current_manifest=$(manifest_from_marker)
        [ "$current_manifest" = "$recovered_manifest" ] \
            || die "dirty marker changed during recovery"
        rm -f "$dirty_marker" || die "restored state but could not remove dirty marker"
        sync -f "$state_root" || die "restored state but could not persist marker removal"
    elif [ -z "$global_record" ]; then
        die "local dirty marker disappeared"
    fi
    if [ -n "$global_record" ]; then
        [ "$recorded_global_manifest" = "$recovered_manifest" ] \
            || die "global dirty-owner manifest changed during recovery"
        clear_global_dirty_record
    fi
    rm -f "$recovered_manifest" || die "restored state but could not remove manifest"
    remove_stale_runtime_files
}

reject_symlink "$dirty_marker" "dirty marker"
if [ "$recover" -eq 1 ]; then
    [ -z "$binary$waiter_cpu$victim_cpu$producer_cpu$controller_cpu" ] && [ "$#" -eq 0 ] || usage
    if [ -n "$global_record" ]; then
        recovery_manifest=$recorded_global_manifest
        validate_manifest_location "$recovery_manifest"
        validate_private_file "$recovery_manifest" "recovery manifest"
        if [ -e "$dirty_marker" ]; then
            local_manifest=$(manifest_from_marker)
            [ "$local_manifest" = "$recovery_manifest" ] \
                || die "local and global dirty-owner records disagree"
        fi
    else
        [ -e "$dirty_marker" ] || die "there is no dirty cpuidle run to recover"
        recovery_manifest=$(manifest_from_marker)
    fi
    validate_manifest "$recovery_manifest"
    flock -w "$ACTIVE_RECOVERY_WAIT_SECONDS" 7 \
        || die "active benchmark cleanup did not release $active_lock_file"
    if ! restore_manifest "$recovery_manifest"; then
        echo "cpuidle runner: RECOVERY FAILED; marker and manifest are retained" >&2
        exit 1
    fi
    finish_recovery "$recovery_manifest"
    echo "cpuidle runner: recovery complete"
    exit 0
fi

flock -n 7 || die "an earlier benchmark guardian still owns $active_lock_file"

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
child_group_verified=0
guardian_pid=
guardian_group_verified=0
guardian_ready_file=
guardian_release_file=
guardian_group_file=
guardian_proof_request_file=
guardian_proof_ready_file=
supervisor_status_file=
supervisor_ready_file=
supervisor_go_file=
supervisor_launching=0
signal_status=

cleanup() {
    original_status=$?
    trap - EXIT HUP INT TERM
    if [ -n "$child_pid" ]; then
        if [ "$child_group_verified" -eq 1 ]; then
            terminate_child "$child_pid" || {
                echo "cpuidle runner: could not prove the benchmark process group was drained; recovery files are retained" >&2
                exit 1
            }
        else
            # Before the go-ahead handshake the supervisor cannot launch the
            # benchmark and exits on its own after a fixed polling bound.
            wait "$child_pid" 2>/dev/null || true
        fi
        child_pid=
    fi
    if [ -n "$guardian_pid" ]; then
        if ! : >"$guardian_release_file"; then
            echo "cpuidle runner: could not release the benchmark crash guardian; recovery files are retained" >&2
            exit 1
        fi
        wait "$guardian_pid" 2>/dev/null || {
            echo "cpuidle runner: benchmark crash guardian failed; recovery files are retained" >&2
            exit 1
        }
        guardian_pid=
        guardian_group_verified=0
    fi
    rm -f "${supervisor_status_file:-}" "${supervisor_ready_file:-}" \
        "${supervisor_go_file:-}" "${guardian_ready_file:-}" \
        "${guardian_release_file:-}" "${guardian_group_file:-}" \
        "${guardian_proof_request_file:-}" "${guardian_proof_ready_file:-}" \
        2>/dev/null || true
    supervisor_status_file=
    supervisor_ready_file=
    supervisor_go_file=
    if [ -e "$global_dirty_marker" ] && [ -z "$global_record" ]; then
        # A handled signal may arrive after ln(2) published the marker but
        # before the next shell assignment. Re-adopt only the exact record this
        # invocation prepared; otherwise retain every recovery file fail-closed.
        [ -n "${published_record:-}" ] || {
            echo "cpuidle runner: global dirty-owner record appeared without an expected record; recovery files are retained" >&2
            exit 1
        }
        cleanup_global_record=$(read_global_dirty_record) || {
            echo "cpuidle runner: cannot adopt the published global dirty-owner record; recovery files are retained" >&2
            exit 1
        }
        [ "$cleanup_global_record" = "$published_record" ] || {
            echo "cpuidle runner: published global dirty-owner record changed; recovery files are retained" >&2
            exit 1
        }
        global_record=$cleanup_global_record
        validate_global_dirty_record "$global_record"
    fi
    if [ -n "$global_marker_candidate" ]; then
        remove_global_marker_candidate \
            || echo "cpuidle runner: could not remove abandoned global marker candidate" >&2
    fi
    if [ -e "$dirty_marker" ] || [ -n "$global_record" ]; then
        if [ -e "$dirty_marker" ]; then
            current_manifest=$(manifest_from_marker)
            if [ "$current_manifest" != "$manifest" ]; then
                echo "cpuidle runner: dirty marker changed; refusing automatic restore" >&2
                exit 1
            fi
        fi
        if [ -n "$global_record" ] \
            && [ "$recorded_global_manifest" != "$manifest" ]; then
            echo "cpuidle runner: global dirty-owner record changed; refusing automatic restore" >&2
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
    if [ "$supervisor_launching" -eq 1 ]; then
        return
    fi
    if [ -n "$child_pid" ]; then
        terminate_child "$child_pid" || {
            echo "cpuidle runner: could not prove the benchmark process group was drained; recovery files are retained" >&2
            exit 1
        }
        child_pid=
    fi
    exit "$signal_status"
}

process_group_has_live_descendants() {
    inspected_group=$1
    # The live supervisor owns the PGID until the runner releases it. Exclude
    # only that known leader; every non-zombie descendant must leave first.
    # Inspection failure is treated as live and therefore takes the KILL path.
    group_processes=$(ps -eo pid=,pgid=,stat= 2>/dev/null) || return 0
    printf '%s\n' "$group_processes" | awk -v expected_group="$inspected_group" '
        $2 == expected_group && $1 != expected_group && $3 !~ /^Z/ { found = 1 }
        END { exit !found }
    '
}

process_start_ticks() {
    inspected_pid=$1
    awk '
        NR == 1 {
            line = $0
            sub(/^.*\) /, "", line)
            fields = split(line, value, " ")
            if (fields < 20 || value[20] !~ /^[0-9]+$/) exit 1
            print value[20]
            found = 1
        }
        END { if (!found) exit 1 }
    ' "/proc/$inspected_pid/stat"
}

supervisor_is_stopped() {
    inspected_pid=$1
    supervisor_state=$(ps -o stat= -p "$inspected_pid" 2>/dev/null | tr -d '[:space:]') \
        || return 1
    case "$supervisor_state" in
        T*) return 0 ;;
        *) return 1 ;;
    esac
}

monotonic_milliseconds() {
    awk '
        NR == 1 && $1 ~ /^[0-9]+\.[0-9]+$/ {
            split($1, uptime, ".")
            fraction = substr(uptime[2] "000", 1, 3)
            printf "%.0f\n", (uptime[1] * 1000) + fraction
            found = 1
        }
        END { if (!found) exit 1 }
    ' /proc/uptime
}

termination_budget_remaining() {
    [ "$termination_attempt" -lt "$KILL_GRACE_POLLS" ] || return 1
    termination_now=$(monotonic_milliseconds) || return 1
    [ "$termination_now" -lt "$termination_deadline" ]
}

prove_killed_group_drained() {
    inspected_pid=$1
    [ -n "$guardian_pid" ] && [ "$guardian_group_verified" -eq 1 ] || return 1
    printf '%s\n' "$inspected_pid" >"$guardian_proof_request_file" || return 1
    while [ ! -s "$guardian_proof_ready_file" ]; do
        guardian_state=$(ps -o stat= -p "$guardian_pid" 2>/dev/null | tr -d '[:space:]') \
            || guardian_state=
        case "$guardian_state" in
            ''|Z*) return 1 ;;
        esac
        sleep 0.05
    done
    proved_group=$(tr -d '[:space:]' <"$guardian_proof_ready_file") || return 1
    [ "$proved_group" = "$inspected_pid" ]
}

signal_child_group() {
    child_signal=$1
    inspected_pid=$2
    [ "$child_group_verified" -eq 1 ] || return 1
    # The verified, unreaped supervisor remains the group leader until this
    # function reaps it. Therefore this negative PGID cannot be recycled.
    kill -s "$child_signal" -- "-$inspected_pid" 2>/dev/null
}

terminate_child() {
    inspected_pid=$1
    termination_mode=${2:-graceful}
    [ "$child_group_verified" -eq 1 ] || return 1
    child_group_killed=0
    if [ "$termination_mode" = immediate ]; then
        signal_child_group KILL "$inspected_pid" || return 1
        child_group_killed=1
    else
        signal_child_group TERM "$inspected_pid" || true
        termination_attempt=0
        termination_started=$(monotonic_milliseconds) || termination_started=
        if [ -z "$termination_started" ]; then
            signal_child_group KILL "$inspected_pid" || return 1
            child_group_killed=1
        else
            termination_deadline=$((termination_started + KILL_GRACE_MILLISECONDS))
            while process_group_has_live_descendants "$inspected_pid" \
                && termination_budget_remaining; do
                sleep 0.05
                termination_attempt=$((termination_attempt + 1))
            done
            if process_group_has_live_descendants "$inspected_pid"; then
                signal_child_group KILL "$inspected_pid" || return 1
                child_group_killed=1
            else
                # Do not send CONT before the supervisor reaches its final STOP;
                # otherwise a fast child exit could miss the wake and hang reap.
                while ! supervisor_is_stopped "$inspected_pid" \
                    && termination_budget_remaining; do
                    sleep 0.05
                    termination_attempt=$((termination_attempt + 1))
                done
                if supervisor_is_stopped "$inspected_pid"; then
                    kill -CONT "$inspected_pid" 2>/dev/null || return 1
                else
                    signal_child_group KILL "$inspected_pid" || return 1
                    child_group_killed=1
                fi
            fi
        fi
    fi
    # Keep the unreaped supervisor as the PGID owner until no non-zombie member
    # remains. Only then can reaping release the numeric process-group identity.
    if [ "$child_group_killed" -eq 1 ]; then
        prove_killed_group_drained "$inspected_pid" || return 1
    fi
    wait "$inspected_pid" 2>/dev/null || true
    child_group_verified=0
}

verify_child_process_group() {
    inspected_pid=$1
    group_attempt=0
    while [ "$group_attempt" -lt 100 ]; do
        inspected_state=$(ps -o stat= -p "$inspected_pid" 2>/dev/null | tr -d '[:space:]') \
            || inspected_state=
        case "$inspected_state" in
            ''|Z*)
                wait "$inspected_pid" 2>/dev/null || true
                child_pid=
                supervisor_launching=0
                die "benchmark supervisor exited before acquiring a dedicated process group"
                ;;
        esac
        group=$(ps -o pgid= -p "$inspected_pid" 2>/dev/null | tr -d '[:space:]') || group=
        if [ "$group" = "$inspected_pid" ]; then
            child_group_verified=1
            return 0
        fi
        if ! kill -0 "$inspected_pid" 2>/dev/null; then
            wait "$inspected_pid" 2>/dev/null || true
            child_pid=
            supervisor_launching=0
            die "benchmark supervisor exited before acquiring a dedicated process group"
        fi
        sleep 0.01
        group_attempt=$((group_attempt + 1))
    done
    # No benchmark was allowed to start before this verification. The bounded
    # supervisor startup wait therefore owns the only process that must exit.
    wait "$inspected_pid" 2>/dev/null || true
    child_pid=
    supervisor_launching=0
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
        recorded_name=$(tr -d '\r\n' <"$state_dir/name") \
            || die "cannot read state name: $state_dir"
        name=$(printf '%s\n' "$recorded_name" | tr '[:lower:]' '[:upper:]') \
            || die "cannot normalize state name: $state_dir"
        original=$(tr -d '[:space:]' <"$state_dir/disable") \
            || die "cannot read disable value: $state_dir"
        case "$original" in 0|1) ;; *) die "invalid disable value '$original': $state_dir" ;; esac
        case "$name" in
            POLL) poll_count=$((poll_count + 1)); desired=0 ;;
            C1) c1_count=$((c1_count + 1)); desired=0 ;;
            *) desired=1 ;;
        esac
        printf 'state|%s|%s|%s|%s|%s|%s\n' \
            "$state_dir/disable" "$original" "$desired" "$recorded_name" "$cpu" "$state" >>"$manifest"
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
if ! publish_global_dirty_record "$manifest"; then
    echo "cpuidle runner: failed to publish the global dirty-owner record" >&2
    exit 1
fi
echo "cpuidle runner: recovery manifest $manifest"

while IFS='|' read -r kind path original desired name cpu state; do
    [ "$kind" = state ] || continue
    validate_manifest_inventory "$manifest"
    validate_manifest_entry "$path" "$original" "$desired" "$name" "$cpu" "$state" \
        || die "manifest validation failed before apply"
    if ! write_and_verify "$path" "$desired"; then
        echo "cpuidle runner: failed to apply CPU $cpu state$state ($name)=$desired" >&2
        exit 1
    fi
done <"$manifest"

case "${SNOOZER_TEST_SIGNAL_AFTER_APPLY:-}" in
    '') ;;
    TERM) kill -TERM "$$" ;;
    KILL) kill -KILL "$$" ;;
    *) die "unsupported test signal after apply" ;;
esac

echo "C2/C3 and every deeper CPU idle state are disabled on the assigned CPUs because their exit latency conflicts with the minimum-wake-latency objective. These results do not represent the default power-saving configuration."

supervisor_launching=1
runner_start_ticks=$(process_start_ticks "$$") \
    || die "cannot read runner process identity"
guardian_ready_file=$(mktemp "$state_root/guardian-ready.XXXXXX") \
    || die "cannot allocate guardian ready file"
guardian_release_file=$(mktemp "$state_root/guardian-release.XXXXXX") \
    || die "cannot allocate guardian release file"
guardian_group_file=$(mktemp "$state_root/guardian-group.XXXXXX") \
    || die "cannot allocate guardian group file"
guardian_proof_request_file=$(mktemp "$state_root/guardian-proof-request.XXXXXX") \
    || die "cannot allocate guardian proof request file"
guardian_proof_ready_file=$(mktemp "$state_root/guardian-proof-ready.XXXXXX") \
    || die "cannot allocate guardian proof result file"
rm -f "$guardian_ready_file" "$guardian_release_file" "$guardian_group_file" \
    "$guardian_proof_request_file" "$guardian_proof_ready_file" \
    || die "cannot initialize guardian handshake files"

# The guardian is deliberately outside the benchmark PGID. It alone inherits
# active-run.lock, while closing the mutation locks before publishing READY.
# If the runner is SIGKILLed, recovery waits on FD 7 until this guardian KILLs
# and proves the benchmark group empty.
setsid sh -c '
    ready_file=$1
    release_file=$2
    group_file=$3
    proof_request_file=$4
    proof_ready_file=$5
    runner_pid=$6
    runner_ticks=$7
    exec 8>&- 9>&- || exit 125
    trap "" HUP INT TERM

    process_ticks() {
        awk '\''
            NR == 1 {
                line = $0
                sub(/^.*\) /, "", line)
                fields = split(line, value, " ")
                if (fields < 20 || value[20] !~ /^[0-9]+$/) exit 1
                print value[20]
                found = 1
            }
            END { if (!found) exit 1 }
        '\'' "/proc/$1/stat"
    }
    runner_is_same_process() {
        observed_parent=$(ps -o ppid= -p "$$" 2>/dev/null | tr -d "[:space:]") \
            || return 1
        [ "$observed_parent" = "$runner_pid" ] || return 1
        observed_ticks=$(process_ticks "$runner_pid") || return 1
        [ "$observed_ticks" = "$runner_ticks" ]
    }
    group_scan() {
        inspected_group=$1
        if [ -n "${SNOOZER_TEST_POST_KILL_BLOCK_FILE:-}" ] \
            && [ -e "$SNOOZER_TEST_POST_KILL_BLOCK_FILE" ]; then
            return 2
        fi
        processes=$(ps -eo pgid=,stat= 2>/dev/null) || return 2
        printf "%s\n" "$processes" | awk -v expected_group="$inspected_group" '\''
            $1 == expected_group && $2 !~ /^Z/ { found = 1 }
            END { exit !found }
        '\''
    }
    prove_empty() {
        inspected_group=$1
        while :; do
            group_scan "$inspected_group"
            scan_status=$?
            case "$scan_status" in
                1) return 0 ;;
                0|2) sleep 0.05 ;;
                *) sleep 0.05 ;;
            esac
        done
    }
    read_group() {
        [ -s "$group_file" ] || return 1
        active_group=$(tr -d "[:space:]" <"$group_file") || return 1
        case "$active_group" in ""|*[!0-9]*) return 1 ;; esac
    }

    own_group=$(ps -o pgid= -p "$$" 2>/dev/null | tr -d "[:space:]") || exit 125
    [ "$own_group" = "$$" ] || exit 125
    runner_is_same_process || exit 125
    printf "%s\n" "$$" >"$ready_file" || exit 125
    [ "${SNOOZER_TEST_GUARDIAN_EXIT_AFTER_READY:-}" != 1 ] || exit 0

    while :; do
        [ ! -e "$release_file" ] || exit 0
        if ! runner_is_same_process; then
            if read_group; then
                kill -s KILL -- "-$active_group" 2>/dev/null || :
                prove_empty "$active_group"
            fi
            exit 0
        fi
        if [ -s "$proof_request_file" ]; then
            requested_group=$(tr -d "[:space:]" <"$proof_request_file") || exit 125
            read_group || exit 125
            [ "$requested_group" = "$active_group" ] || exit 125
            prove_empty "$active_group"
            printf "%s\n" "$active_group" >"$proof_ready_file" || exit 125
        fi
        sleep 0.05
    done
' snoozer-benchmark-guardian "$guardian_ready_file" "$guardian_release_file" \
    "$guardian_group_file" "$guardian_proof_request_file" \
    "$guardian_proof_ready_file" "$$" "$runner_start_ticks" &
guardian_pid=$!

guardian_attempt=0
while [ ! -s "$guardian_ready_file" ] \
    && [ "$guardian_attempt" -lt "$SUPERVISOR_STARTUP_POLLS" ]; do
    guardian_state=$(ps -o stat= -p "$guardian_pid" 2>/dev/null | tr -d '[:space:]') \
        || guardian_state=
    case "$guardian_state" in
        ''|Z*)
            wait "$guardian_pid" 2>/dev/null || true
            guardian_pid=
            supervisor_launching=0
            die "benchmark crash guardian exited before its startup handshake"
            ;;
    esac
    sleep 0.01
    guardian_attempt=$((guardian_attempt + 1))
done
[ -s "$guardian_ready_file" ] \
    || die "benchmark crash guardian did not complete its startup handshake"
reported_guardian_pid=$(tr -d '[:space:]' <"$guardian_ready_file") \
    || die "cannot read benchmark crash guardian identity"
[ "$reported_guardian_pid" = "$guardian_pid" ] \
    || die "benchmark crash guardian startup identity did not match its child PID"
guardian_state=$(ps -o stat= -p "$guardian_pid" 2>/dev/null | tr -d '[:space:]') \
    || guardian_state=
case "$guardian_state" in
    ''|Z*)
        wait "$guardian_pid" 2>/dev/null || true
        guardian_pid=
        supervisor_launching=0
        die "benchmark crash guardian exited after its startup handshake"
        ;;
esac
guardian_group=$(ps -o pgid= -p "$guardian_pid" 2>/dev/null | tr -d '[:space:]') \
    || guardian_group=
[ "$guardian_group" = "$guardian_pid" ] \
    || die "benchmark crash guardian did not acquire a dedicated process group"
guardian_group_verified=1

# The supervisor remains the inner process-group leader after timeout exits and
# until the runner drains that group. It closes all runner locks before READY.
supervisor_status_file=$(mktemp "$state_root/supervisor-status.XXXXXX") \
    || die "cannot allocate supervisor status file"
supervisor_ready_file=$(mktemp "$state_root/supervisor-ready.XXXXXX") \
    || die "cannot allocate supervisor ready file"
supervisor_go_file=$(mktemp "$state_root/supervisor-go.XXXXXX") \
    || die "cannot allocate supervisor go file"
rm -f "$supervisor_ready_file" "$supervisor_go_file" \
    || die "cannot initialize supervisor handshake files"

set +e
setsid sh -c '
    ready_file=$1
    go_file=$2
    status_file=$3
    startup_polls=$4
    kill_grace=$5
    duration=$6
    shift 6
    exec 7>&- 8>&- 9>&- || exit 125
    trap "" TERM
    trap "exit 0" CONT
    [ "${SNOOZER_TEST_SUPERVISOR_EXIT_BEFORE_READY:-}" != 1 ] || exit 125
    own_group=$(ps -o pgid= -p "$$" 2>/dev/null | tr -d "[:space:]") || exit 125
    [ "$own_group" = "$$" ] || exit 125
    printf "%s\n" "$$" >"$ready_file" || exit 125
    [ "${SNOOZER_TEST_SUPERVISOR_EXIT_AFTER_READY:-}" != 1 ] || exit 125
    startup_attempt=0
    while [ ! -e "$go_file" ] && [ "$startup_attempt" -lt "$startup_polls" ]; do
        sleep 0.01
        startup_attempt=$((startup_attempt + 1))
    done
    [ -e "$go_file" ] || exit 125
    (
        trap - TERM
        exec timeout --foreground --signal=TERM --kill-after="${kill_grace}s" "$duration" "$@"
    )
    command_status=$?
    # Even if status publication fails, remain as the owned, stopped group
    # anchor. The runner can then drain the verified PGID without guessing.
    if [ "${SNOOZER_TEST_SUPERVISOR_SKIP_STATUS:-}" != 1 ]; then
        printf "%s\n" "$command_status" >"$status_file" || :
    fi
    kill -STOP "$$"
    exit 125
' snoozer-benchmark-supervisor "$supervisor_ready_file" "$supervisor_go_file" \
    "$supervisor_status_file" "$SUPERVISOR_STARTUP_POLLS" "$KILL_GRACE_SECONDS" \
    "$timeout_seconds" "$binary" \
    --waiter-cpu "$waiter_cpu" \
    --victim-cpu "$victim_cpu" \
    --producer-cpu "$producer_cpu" \
    --controller-cpu "$controller_cpu" \
    "$@" &
child_pid=$!

ready_attempt=0
while [ ! -s "$supervisor_ready_file" ] \
    && [ "$ready_attempt" -lt "$SUPERVISOR_STARTUP_POLLS" ]; do
    if ! kill -0 "$child_pid" 2>/dev/null; then
        wait "$child_pid" 2>/dev/null || true
        child_pid=
        supervisor_launching=0
        die "benchmark supervisor exited before its startup handshake"
    fi
    sleep 0.01
    ready_attempt=$((ready_attempt + 1))
done
if [ ! -s "$supervisor_ready_file" ]; then
    # The supervisor has not received its go-ahead and exits after the same
    # fixed startup bound, so waiting cannot outlive an active benchmark.
    wait "$child_pid" 2>/dev/null || true
    child_pid=
    supervisor_launching=0
    die "benchmark supervisor did not complete its startup handshake"
fi
reported_supervisor_pid=$(tr -d '[:space:]' <"$supervisor_ready_file")
[ "$reported_supervisor_pid" = "$child_pid" ] || {
    wait "$child_pid" 2>/dev/null || true
    child_pid=
    supervisor_launching=0
    die "benchmark supervisor startup identity did not match its owned child PID"
}
verify_child_process_group "$child_pid"
guardian_state=$(ps -o stat= -p "$guardian_pid" 2>/dev/null | tr -d '[:space:]') \
    || guardian_state=
guardian_group=$(ps -o pgid= -p "$guardian_pid" 2>/dev/null | tr -d '[:space:]') \
    || guardian_group=
guardian_invalid=0
[ -n "$guardian_state" ] && [ "$guardian_group" = "$guardian_pid" ] \
    || guardian_invalid=1
case "$guardian_state" in Z*) guardian_invalid=1 ;; esac
if [ "$guardian_invalid" -eq 1 ]; then
    signal_child_group KILL "$child_pid" || true
    wait "$child_pid" 2>/dev/null || true
    child_pid=
    child_group_verified=0
    wait "$guardian_pid" 2>/dev/null || true
    guardian_pid=
    guardian_group_verified=0
    supervisor_launching=0
    die "benchmark crash guardian exited before benchmark authorization"
fi
if [ "${SNOOZER_TEST_FAIL_GROUP_PUBLICATION:-}" = 1 ] \
    || ! printf '%s\n' "$child_pid" >"$guardian_group_file"; then
    signal_child_group KILL "$child_pid" || true
    wait "$child_pid" 2>/dev/null || true
    child_pid=
    child_group_verified=0
    supervisor_launching=0
    die "cannot publish the verified benchmark process-group identity"
fi
supervisor_launching=0
if [ -n "$signal_status" ]; then
    terminate_child "$child_pid" immediate || exit 1
    child_pid=
    exit "$signal_status"
fi
case "${SNOOZER_TEST_SIGNAL_AFTER_GROUP_VERIFY:-}" in
    '') ;;
    TERM) kill -TERM "$$" ;;
    *) die "unsupported test signal after group verification" ;;
esac
if [ "${SNOOZER_TEST_FAIL_GO_PUBLICATION:-}" = 1 ] \
    || ! : >"$supervisor_go_file"; then
    terminate_child "$child_pid" immediate || exit 1
    child_pid=
    die "cannot publish the benchmark go-ahead"
fi

while :; do
    supervisor_state=$(ps -o stat= -p "$child_pid" 2>/dev/null | tr -d '[:space:]') \
        || supervisor_state=
    case "$supervisor_state" in
        ''|Z*)
        wait "$child_pid" 2>/dev/null || true
        child_pid=
        child_group_verified=0
        die "benchmark supervisor exited without preserving its process-group anchor"
        ;;
    esac
    if [ -s "$supervisor_status_file" ]; then
        case "$supervisor_state" in
            T*) break ;;
        esac
    else
        case "$supervisor_state" in
            T*)
                terminate_child "$child_pid" immediate
                child_pid=
                die "benchmark supervisor stopped without reporting benchmark status"
                ;;
        esac
    fi
    sleep 0.01
done
command_status=$(tr -d '[:space:]' <"$supervisor_status_file")
case "$command_status" in
    ''|*[!0-9]*)
        terminate_child "$child_pid"
        child_pid=
        die "benchmark supervisor reported an invalid status"
        ;;
esac
if [ "$command_status" -gt 255 ]; then
    terminate_child "$child_pid"
    child_pid=
    die "benchmark supervisor reported an invalid status"
fi
# GNU timeout --foreground signals only the direct benchmark process. Drain the
# still-owned process group before restoring cpuidle so no benchmark descendant
# can outlive the measured run, including on the timeout path.
case "$command_status" in
    124|137) terminate_child "$child_pid" immediate ;;
    *) terminate_child "$child_pid" ;;
esac
child_pid=
rm -f "$supervisor_status_file" "$supervisor_ready_file" "$supervisor_go_file"
supervisor_status_file=
supervisor_ready_file=
supervisor_go_file=
set -e
exit "$command_status"
