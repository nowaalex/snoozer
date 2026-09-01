#!/bin/sh
# Build the optimized harness and print its exact executable path.
set -eu

BUILD_TIMEOUT_SECONDS=${SNOOZER_BUILD_TIMEOUT_SECONDS:-300}
case "$BUILD_TIMEOUT_SECONDS" in
    ''|*[!0-9]*|0)
        echo "benchmark build timeout must be a positive integer" >&2
        exit 2
        ;;
esac
for required in awk cargo env git python3 realpath rustc timeout; do
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
build_log=$(mktemp "${TMPDIR:-/tmp}/snoozer-benchmark-build.XXXXXX")
trap 'rm -f "$build_log"' EXIT HUP INT TERM

if ! timeout --foreground --signal=TERM --kill-after=5s "$BUILD_TIMEOUT_SECONDS" \
    cargo bench --manifest-path "$repository/Cargo.toml" --bench wake_latency \
    --features benchmark-only --no-run --locked --message-format=json >"$build_log"; then
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
