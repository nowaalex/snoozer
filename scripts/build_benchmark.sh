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
for required in cargo git python3 rustc timeout; do
    command -v "$required" >/dev/null 2>&1 || {
        echo "required command is unavailable: $required" >&2
        exit 2
    }
done

repository=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
benchmark_commit=$(git -C "$repository" rev-parse --verify HEAD) || {
    echo "cannot determine the benchmark source commit" >&2
    exit 2
}
[ -n "$benchmark_commit" ] || {
    echo "the benchmark source commit is empty" >&2
    exit 2
}
if [ -n "$(git -C "$repository" status --porcelain --untracked-files=no)" ]; then
    benchmark_tracked_dirty=true
else
    benchmark_tracked_dirty=false
fi
export SNOOZER_BENCHMARK_COMMIT=$benchmark_commit
export SNOOZER_BENCHMARK_REPOSITORY=$repository
export SNOOZER_BENCHMARK_TRACKED_DIRTY=$benchmark_tracked_dirty
SNOOZER_BENCHMARK_RUSTC=$(rustc --version) || {
    echo "cannot determine the benchmark compiler version" >&2
    exit 2
}
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
