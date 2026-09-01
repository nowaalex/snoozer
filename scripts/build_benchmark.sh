#!/bin/sh
# Build the optimized harness and print its exact executable path.
set -eu

BUILD_TIMEOUT_SECONDS=${SNOOZER_BUILD_TIMEOUT_SECONDS:-300}
BUILD_KILL_GRACE_POLLS=100
case "$BUILD_TIMEOUT_SECONDS" in
    ''|*[!0-9]*|0)
        echo "benchmark build timeout must be a positive integer" >&2
        exit 2
        ;;
esac
for required in awk cargo env git ps python3 realpath rustc setsid sleep timeout; do
    command -v "$required" >/dev/null 2>&1 || {
        echo "required command is unavailable: $required" >&2
        exit 2
    }
done

reject_override() {
    override_name=$1
    echo "benchmark build rejects the build-affecting environment override: $override_name" >&2
    exit 2
}

[ "${RUSTC+x}" != x ] || reject_override RUSTC
[ "${RUSTFLAGS+x}" != x ] || reject_override RUSTFLAGS
[ "${CARGO_ENCODED_RUSTFLAGS+x}" != x ] || reject_override CARGO_ENCODED_RUSTFLAGS
[ "${RUSTC_WRAPPER+x}" != x ] || reject_override RUSTC_WRAPPER
[ "${RUSTC_WORKSPACE_WRAPPER+x}" != x ] || reject_override RUSTC_WORKSPACE_WRAPPER
[ "${RUSTC_BOOTSTRAP+x}" != x ] || reject_override RUSTC_BOOTSTRAP
[ "${CARGO_INCREMENTAL+x}" != x ] || reject_override CARGO_INCREMENTAL
[ "${CARGO_BUILD_TARGET+x}" != x ] || reject_override CARGO_BUILD_TARGET
[ "${CARGO_BUILD_RUSTC+x}" != x ] || reject_override CARGO_BUILD_RUSTC
[ "${CARGO_BUILD_RUSTFLAGS+x}" != x ] || reject_override CARGO_BUILD_RUSTFLAGS
[ "${CARGO_BUILD_RUSTC_WRAPPER+x}" != x ] || reject_override CARGO_BUILD_RUSTC_WRAPPER
[ "${CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER+x}" != x ] \
    || reject_override CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER
dynamic_override=$(env | awk -F= '
    $1 ~ /^CARGO_TARGET_.*_(RUSTFLAGS|LINKER)$/ ||
    $1 ~ /^CARGO_PROFILE_(BENCH|RELEASE)_/ {
        print $1
        exit
    }
')
[ -z "$dynamic_override" ] || reject_override "$dynamic_override"

script_path=$(realpath "$0") || {
    echo "cannot resolve the physical benchmark build-helper path" >&2
    exit 2
}
repository=$(realpath "$(dirname "$script_path")/..") || {
    echo "cannot resolve the physical benchmark repository path" >&2
    exit 2
}
git_repository=$(git -C "$repository" rev-parse --show-toplevel 2>/dev/null) || {
    echo "benchmark build helper is not inside a Git work tree" >&2
    exit 2
}
git_repository=$(realpath "$git_repository") || {
    echo "cannot resolve the physical Git repository path" >&2
    exit 2
}
[ "$git_repository" = "$repository" ] || {
    echo "benchmark build helper must be located directly in the repository it builds" >&2
    exit 2
}
cd "$repository"

for repository_config in .cargo/config .cargo/config.toml; do
    if [ -e "$repository_config" ] \
        && ! git ls-files --error-unmatch "$repository_config" >/dev/null 2>&1; then
        echo "benchmark rejects an untracked or ignored repository Cargo config: $repository_config" >&2
        exit 2
    fi
done
[ -n "${HOME:-}" ] || {
    echo "HOME must be set so the benchmark can verify Cargo configuration provenance" >&2
    exit 2
}
default_cargo_home=$HOME/.cargo
cargo_home=${CARGO_HOME:-$default_cargo_home}
[ "$cargo_home" = "$default_cargo_home" ] || reject_override CARGO_HOME
for cargo_home_config in "$cargo_home/config" "$cargo_home/config.toml"; do
    [ ! -e "$cargo_home_config" ] || {
        echo "benchmark rejects Cargo configuration outside the tracked repository: $cargo_home_config" >&2
        exit 2
    }
done
ancestor=${repository%/*}
[ -n "$ancestor" ] || ancestor=/
while :; do
    for ancestor_config in "$ancestor/.cargo/config" "$ancestor/.cargo/config.toml"; do
        [ ! -e "$ancestor_config" ] || {
            echo "benchmark rejects Cargo configuration outside the tracked repository: $ancestor_config" >&2
            exit 2
        }
    done
    [ "$ancestor" != / ] || break
    ancestor=${ancestor%/*}
    [ -n "$ancestor" ] || ancestor=/
done

toolchain_channel=$(awk -F '"' '
    /^[[:space:]]*channel[[:space:]]*=/ { print $2; count++ }
    END { if (count != 1) exit 1 }
' "$repository/rust-toolchain.toml") || {
    echo "cannot determine the pinned repository Rust toolchain" >&2
    exit 2
}
[ "$toolchain_channel" = 1.98.0 ] || {
    echo "benchmark requires the repository Rust 1.98.0 toolchain" >&2
    exit 2
}
if [ "${RUSTUP_TOOLCHAIN+x}" = x ]; then
    case "$RUSTUP_TOOLCHAIN" in
        "$toolchain_channel"|"$toolchain_channel"-*) ;;
        *) reject_override RUSTUP_TOOLCHAIN ;;
    esac
    benchmark_rustup_toolchain=$RUSTUP_TOOLCHAIN
else
    benchmark_rustup_toolchain=repository-toolchain-file
fi
benchmark_commit=$(git -C "$repository" rev-parse --verify HEAD) || {
    echo "cannot determine the benchmark source commit" >&2
    exit 2
}
[ -n "$benchmark_commit" ] || {
    echo "the benchmark source commit is empty" >&2
    exit 2
}
benchmark_status=$(git -C "$repository" status --porcelain --untracked-files=all) || {
    echo "cannot verify that the benchmark working tree is clean" >&2
    exit 2
}
[ -z "$benchmark_status" ] || {
    echo "official benchmark builds require a clean working tree, including no untracked files" >&2
    exit 2
}
benchmark_dirty=false
export SNOOZER_BENCHMARK_COMMIT=$benchmark_commit
export SNOOZER_BENCHMARK_REPOSITORY=$repository
export SNOOZER_BENCHMARK_DIRTY=$benchmark_dirty
export SNOOZER_BENCHMARK_RUSTUP_TOOLCHAIN=$benchmark_rustup_toolchain
SNOOZER_BENCHMARK_RUSTC=$(rustc --version) || {
    echo "cannot determine the benchmark compiler version" >&2
    exit 2
}
case "$SNOOZER_BENCHMARK_RUSTC" in
    "rustc $toolchain_channel "*) ;;
    *)
        echo "benchmark compiler does not match pinned Rust $toolchain_channel: $SNOOZER_BENCHMARK_RUSTC" >&2
        exit 2
        ;;
esac
export SNOOZER_BENCHMARK_RUSTC
build_state=$(mktemp -d "${TMPDIR:-/tmp}/snoozer-benchmark-build.XXXXXX")
build_log=$build_state/cargo.jsonl
build_status_file=$build_state/status
build_release_file=$build_state/release
build_group=

group_has_other_live_members() {
    ps -eo pid=,pgid=,stat= | awk -v group="$build_group" '
        $2 == group && $1 != group && $3 !~ /^Z/ { found = 1 }
        END { exit !found }
    '
}

cleanup() {
    cleanup_status=$?
    trap - EXIT HUP INT TERM
    if [ -n "$build_group" ]; then
        # The anchor pins the PGID until every member is drained. Linux models
        # PID, PGID, and SID identities with the same refcounted struct pid:
        # https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/linux/pid.h
        kill -s TERM -- "-$build_group" 2>/dev/null || :
        cleanup_poll=0
        while group_has_other_live_members \
            && [ "$cleanup_poll" -lt "$BUILD_KILL_GRACE_POLLS" ]; do
            sleep 0.05
            cleanup_poll=$((cleanup_poll + 1))
        done
        if group_has_other_live_members; then
            kill -s KILL -- "-$build_group" 2>/dev/null || :
        else
            : >"$build_release_file"
        fi
        if wait "$build_group" 2>/dev/null; then
            :
        else
            :
        fi
    fi
    rm -rf "$build_state"
    exit "$cleanup_status"
}

handle_signal() {
    exit "$1"
}

trap cleanup EXIT
trap 'handle_signal 129' HUP
trap 'handle_signal 130' INT
trap 'handle_signal 143' TERM

# `timeout --foreground` remains in the anchored session so it cannot create a
# nested process group outside the helper's TERM/KILL ownership boundary.
setsid sh -c '
    status_file=$1
    release_file=$2
    shift 2
    trap ":" HUP INT TERM
    "$@" &
    worker_pid=$!
    if wait "$worker_pid"; then
        worker_status=0
    else
        worker_status=$?
    fi
    printf "%s\n" "$worker_status" >"$status_file"
    while [ ! -e "$release_file" ]; do
        sleep 0.05
    done
    exit "$worker_status"
' sh "$build_status_file" "$build_release_file" \
    timeout --foreground --signal=TERM --kill-after=5s "$BUILD_TIMEOUT_SECONDS" \
    cargo bench --manifest-path "$repository/Cargo.toml" --bench wake_latency \
    --features benchmark-only --no-run --locked --message-format=json >"$build_log" &
build_group=$!

while [ ! -s "$build_status_file" ]; do
    if ! kill -0 "$build_group" 2>/dev/null; then
        if wait "$build_group" 2>/dev/null; then
            :
        else
            :
        fi
        build_group=
        echo "optimized benchmark build supervisor exited before reporting status" >&2
        exit 1
    fi
    sleep 0.05
done
IFS= read -r build_status <"$build_status_file"
case "$build_status" in
    ''|*[!0-9]*)
        echo "optimized benchmark build supervisor reported an invalid status" >&2
        exit 1
        ;;
esac
if [ "$build_status" -ne 0 ]; then
    echo "optimized benchmark build failed or timed out" >&2
    exit 1
fi

python3 -c '
import json
import pathlib
import sys

paths = []
for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines():
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    target = message.get("target", {})
    executable = message.get("executable")
    if message.get("reason") == "compiler-artifact" and target.get("name") == "wake_latency" and "bench" in target.get("kind", []) and executable:
        paths.append(executable)
if len(paths) != 1:
    raise SystemExit(f"expected one wake_latency executable, found {len(paths)}")
print(paths[0])
' "$build_log"
